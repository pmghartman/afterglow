//! The app's windows kept to its virtual display, as the Linux host's window manager
//! keeps them to its Xvfb: a main window fills the display, over the taskbar, and a
//! dialog or a fixed-size window is centred on it, so the video shows the app and not
//! the desktop around it. Windows are looked for every 100ms; nothing is hooked.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::JobObjects::IsProcessInJob;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Shell::{ITaskbarList2, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, IsZoomed, SW_RESTORE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOSIZE,
    SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindowAsync, WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW,
    WS_THICKFRAME,
};
use windows::core::BOOL;

use super::display;
use super::input::sync_desktop;
use crate::debug;

/// the app's windows followed on their own thread until dropped
pub struct Follower {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Follower {
    /// the windows of everything in job kept to the display by this GDI name
    pub fn start(job: HANDLE, display: String) -> Follower {
        let stop = Arc::new(AtomicBool::new(false));
        // a handle isn't Send: its value crosses, the job outlives the follower
        let (s, job) = (stop.clone(), job.0 as usize);
        let thread = thread::Builder::new()
            .name("windows".into())
            .spawn(move || follow(HANDLE(job as *mut c_void), &display, &s))
            .ok();
        Follower { stop, thread }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// what's done with a window
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// a main window: fills the display
    Fill,
    /// a dialog, a fixed-size window, a splash: centred when it's off the display
    Centre,
}

fn follow(job: HANDLE, display: &str, stop: &AtomicBool) {
    // onto the app's desktop before COM: a thread with a window of its own can't change desktops
    sync_desktop();
    // SAFETY: balanced below; the taskbar's object lives in an apartment
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    // SAFETY: a shell object created and initialised on this thread
    let taskbar: Option<ITaskbarList2> = unsafe {
        CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok().filter(|t: &ITaskbarList2| t.HrInit().is_ok())
    };
    // windows already placed once, so the app has the keyboard as each comes
    let mut seen: HashSet<isize> = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        if let Some(mon) = display::rect_of(display) {
            for (h, kind) in app_windows(job) {
                let first = seen.insert(h.0 as isize);
                place(h, kind, mon, first, taskbar.as_ref());
            }
        }
        // SAFETY: IsWindow takes any value
        seen.retain(|&h| unsafe { IsWindow(Some(HWND(h as *mut c_void))) }.as_bool());
        thread::sleep(Duration::from_millis(100));
    }
    drop(taskbar);
    if com {
        // SAFETY: the CoInitializeEx above
        unsafe { CoUninitialize() };
    }
}

unsafe extern "system" fn collect(h: HWND, l: LPARAM) -> BOOL {
    // SAFETY: l is the Vec app_windows passes, alive for the call
    let v = unsafe { &mut *(l.0 as *mut Vec<HWND>) };
    v.push(h);
    true.into()
}

/// the app's visible top-level windows, and what's done with each
fn app_windows(job: HANDLE) -> Vec<(HWND, Kind)> {
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: collect only pushes to the Vec whose address it's given
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize));
    }
    let mut out = Vec::new();
    for h in all {
        // SAFETY: queries on a window handle; a window gone since answers false or fails
        unsafe {
            if !IsWindowVisible(h).as_bool() {
                continue;
            }
            let style = GetWindowLongW(h, GWL_STYLE) as u32;
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            // menus, tooltips and other tool windows follow their owner
            if style & WS_CHILD.0 != 0 || ex & WS_EX_TOOLWINDOW.0 != 0 {
                continue;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4).is_ok()
                && cloaked != 0
            {
                continue;
            }
            if !in_job(h, job) {
                continue;
            }
            let owned = GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid());
            let captioned = style & WS_CAPTION.0 == WS_CAPTION.0;
            let kind = if !owned && style & WS_THICKFRAME.0 != 0 {
                Kind::Fill
            } else if captioned || !owned {
                Kind::Centre
            } else {
                // an owned popup without a caption (a dropdown, a flyout): placed by the app
                continue;
            };
            out.push((h, kind));
        }
    }
    out
}

fn in_job(h: HWND, job: HANDLE) -> bool {
    let mut pid = 0u32;
    // SAFETY: a process opened for a query and closed
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
        let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut r = BOOL(0);
        let ok = IsProcessInJob(p, Some(job), &mut r).is_ok() && r.as_bool();
        let _ = CloseHandle(p);
        ok
    }
}

/// the window's visible bounds, without the resize borders Windows draws invisibly around it
fn frame(h: HWND, wr: RECT) -> RECT {
    let mut fb = RECT::default();
    let p = &mut fb as *mut RECT as *mut c_void;
    // SAFETY: p is fb, sized as the attribute's RECT
    let ok = unsafe { DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, p, size_of::<RECT>() as u32) }.is_ok();
    if ok { fb } else { wr }
}

fn place(h: HWND, kind: Kind, mon: RECT, first: bool, taskbar: Option<&ITaskbarList2>) {
    let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
    // SAFETY: window calls on a handle that may have gone, which only fail; async ones
    // never wait on a hung app
    unsafe {
        if kind == Kind::Fill && (IsZoomed(h).as_bool() || IsIconic(h).as_bool()) {
            // maximized stops at the taskbar, minimized can't be got back without one:
            // restored, and filled on the next pass
            let _ = ShowWindowAsync(h, SW_RESTORE);
            return;
        }
        let mut wr = RECT::default();
        if GetWindowRect(h, &mut wr).is_err() {
            return;
        }
        let fb = frame(h, wr);
        match kind {
            Kind::Fill => {
                if fb != mon {
                    // the visible frame over the whole display, the invisible borders past its edges
                    let (l, t) = (fb.left - wr.left, fb.top - wr.top);
                    let (r, b) = (wr.right - fb.right, wr.bottom - fb.bottom);
                    let (w, ht) = (mon.right - mon.left + l + r, mon.bottom - mon.top + t + b);
                    let _ = SetWindowPos(h, None, mon.left - l, mon.top - t, w, ht, flags);
                    if first {
                        debug!("window {:#x} fills the display", h.0 as usize);
                    }
                }
                if first {
                    // the taskbar goes under it while it's in front
                    if let Some(t) = taskbar {
                        let _ = t.MarkFullscreenWindow(h, true);
                    }
                }
            }
            Kind::Centre => {
                let (cx, cy) = ((fb.left + fb.right) / 2, (fb.top + fb.bottom) / 2);
                if cx < mon.left || cx >= mon.right || cy < mon.top || cy >= mon.bottom {
                    let x = mon.left + ((mon.right - mon.left) - (wr.right - wr.left)) / 2;
                    let y = mon.top + ((mon.bottom - mon.top) - (wr.bottom - wr.top)) / 2;
                    let _ = SetWindowPos(h, None, x.max(mon.left), y.max(mon.top), 0, 0, flags | SWP_NOSIZE);
                    if first {
                        debug!("window {:#x} centred on the display", h.0 as usize);
                    }
                }
            }
        }
        if first {
            // each new window gets the keyboard, as the Linux host's window manager gives it
            let _ = SetForegroundWindow(h);
        }
    }
}
