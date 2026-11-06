//! SudoVDA, the virtual display driver Apollo ships: a display per app at the viewer's
//! size, so a machine with no monitor streams. Its IOCTLs as Apollo's
//! third-party/sudovda/sudovda-ioctl.h defines them, protocol 0.2.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, LUID};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{GUID, PCWSTR};

use super::display;
use crate::{info, warn};

const INTERFACE: GUID = GUID::from_u128(0xe5bcc234_1e0c_418a_a0d4_ef8b7501414d);

const fn ioctl(func: u32) -> u32 {
    // CTL_CODE(FILE_DEVICE_UNKNOWN, func, METHOD_BUFFERED, FILE_ANY_ACCESS)
    (0x22 << 16) | (func << 2)
}
const ADD: u32 = ioctl(0x800);
const REMOVE: u32 = ioctl(0x801);
const SET_RENDER_ADAPTER: u32 = ioctl(0x802);
const GET_WATCHDOG: u32 = ioctl(0x803);
const PING: u32 = ioctl(0x888);
const GET_PROTOCOL_VERSION: u32 = ioctl(0x8ff);

#[repr(C)]
struct AddParams {
    width: u32,
    height: u32,
    /// millihertz
    refresh: u32,
    guid: GUID,
    name: [u8; 14],
    serial: [u8; 14],
}

#[repr(C)]
#[derive(Default)]
struct AddOut {
    adapter: LUID,
    target: u32,
}

/// the driver's handle, closed once both the Vda and its watchdog's pinger let go
struct Device(HANDLE);

// SAFETY: the device handle is used with synchronous DeviceIoControl from any thread
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: ours, and no one else's call is in flight: they hold this
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct Vda {
    dev: Arc<Device>,
    failed: Arc<AtomicBool>,
}

fn call(h: HANDLE, code: u32, input: &[u8], out: &mut [u8]) -> bool {
    let mut n = 0;
    // SAFETY: the buffers are valid for their lengths for the call's duration
    unsafe {
        DeviceIoControl(
            h,
            code,
            (!input.is_empty()).then_some(input.as_ptr() as *const c_void),
            input.len() as u32,
            (!out.is_empty()).then_some(out.as_mut_ptr() as *mut c_void),
            out.len() as u32,
            Some(&mut n),
            None,
        )
        .is_ok()
    }
}

fn bytes<T>(v: &T) -> &[u8] {
    // SAFETY: a plain repr(C) value read as its bytes
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, size_of::<T>()) }
}

fn bytes_mut<T>(v: &mut T) -> &mut [u8] {
    // SAFETY: a plain repr(C) value written as its bytes
    unsafe { std::slice::from_raw_parts_mut(v as *mut T as *mut u8, size_of::<T>()) }
}

fn device_path() -> Option<Vec<u16>> {
    // SAFETY: the device info set is destroyed before return; the detail buffer is sized as asked
    unsafe {
        let set =
            SetupDiGetClassDevsW(Some(&INTERFACE), PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE).ok()?;
        let mut data =
            SP_DEVICE_INTERFACE_DATA { cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
        let mut path = None;
        if SetupDiEnumDeviceInterfaces(set, None, &INTERFACE, 0, &mut data).is_ok() {
            let mut need = 0;
            let _ = SetupDiGetDeviceInterfaceDetailW(set, &data, None, 0, Some(&mut need), None);
            let mut buf = vec![0u64; (need as usize).div_ceil(8).max(1)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &data, Some(detail), need, None, None).is_ok() {
                let p = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
                let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                let mut v = std::slice::from_raw_parts(p, len).to_vec();
                v.push(0);
                path = Some(v);
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        path
    }
}

impl Vda {
    /// the driver opened, its protocol checked, its watchdog fed. `patient`: its device
    /// waited for a moment, as it may still be coming up when afterglow starts with Windows
    pub fn open(patient: bool) -> Result<Vda, String> {
        let mut wait = 20;
        let path = loop {
            if let Some(p) = device_path() {
                break p;
            }
            if !patient || wait > 320 {
                return Err("SudoVDA isn't installed".into());
            }
            thread::sleep(Duration::from_millis(wait));
            wait *= 2;
        };
        // SAFETY: path is NUL-terminated
        let h = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| format!("SudoVDA won't open: {e}"))?;
        let dev = Arc::new(Device(h));
        let mut v = [0u8; 4];
        if !call(h, GET_PROTOCOL_VERSION, &[], &mut v) || v[0] != 0 || v[1] < 2 {
            return Err(format!("SudoVDA speaks protocol {}.{}.{}, 0.2 wanted", v[0], v[1], v[2]));
        }
        let failed = Arc::new(AtomicBool::new(false));
        let mut wd = [0u32; 2];
        let wd_bytes = bytes_mut(&mut wd);
        if call(h, GET_WATCHDOG, &[], wd_bytes) && wd[0] > 0 {
            // pinged three times a timeout, or the driver removes our displays
            let every = Duration::from_millis(wd[0] as u64 * 1000 / 3);
            let (d, f) = (dev.clone(), failed.clone());
            thread::Builder::new()
                .name("sudovda-ping".into())
                .spawn(move || {
                    let mut fails = 0;
                    while !f.load(Ordering::Relaxed) {
                        if call(d.0, PING, &[], &mut []) {
                            fails = 0;
                        } else {
                            fails += 1;
                            if fails > 3 {
                                warn!("SudoVDA stopped answering");
                                f.store(true, Ordering::Relaxed);
                                return;
                            }
                        }
                        thread::sleep(every);
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        info!("SudoVDA protocol {}.{}.{}, watchdog {}s", v[0], v[1], v[2], wd[0]);
        Ok(Vda { dev, failed })
    }

    pub fn ok(&self) -> bool {
        !self.failed.load(Ordering::Relaxed)
    }

    pub fn set_render_adapter(&self, luid: LUID) -> bool {
        call(self.dev.0, SET_RENDER_ADAPTER, bytes(&luid), &mut [])
    }

    /// a display added, its GDI name (\\.\DISPLAYn) once Windows has given it one
    pub fn add(&self, guid: GUID, w: u32, h: u32, millihz: u32, name: &str, serial: &str) -> Result<String, String> {
        let mut p = AddParams { width: w, height: h, refresh: millihz, guid, name: [0; 14], serial: [0; 14] };
        for (d, s) in p.name.iter_mut().zip(name.bytes().take(13)) {
            *d = s;
        }
        for (d, s) in p.serial.iter_mut().zip(serial.bytes().take(13)) {
            *d = s;
        }
        let mut out = AddOut::default();
        if !call(self.dev.0, ADD, bytes(&p), bytes_mut(&mut out)) {
            return Err(format!("SudoVDA didn't add a display: {}", windows::core::Error::from_thread()));
        }
        let mut wait = 20;
        loop {
            if let Some(n) = display::gdi_name_of_target(out.adapter, out.target) {
                return Ok(n);
            }
            if wait > 640 {
                return Err("the virtual display got no name in time".into());
            }
            thread::sleep(Duration::from_millis(wait));
            wait *= 2;
        }
    }

    pub fn remove(&self, guid: GUID) -> bool {
        call(self.dev.0, REMOVE, bytes(&guid), &mut [])
    }
}

impl Drop for Vda {
    fn drop(&mut self) {
        // the pinger stops at its next turn and lets go of the device: closed then
        self.failed.store(true, Ordering::Relaxed);
    }
}
