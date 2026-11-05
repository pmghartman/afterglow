//! The displays: found by GDI name through DXGI, their modes set, one made primary.

use std::mem::size_of;

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
    QueryDisplayConfig,
};
use windows::Win32::Foundation::{LUID, RECT};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput};
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_SET_PRIMARY, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT,
    DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

use super::{from_wide, wide};
use crate::{info, warn};

pub struct Output {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    /// \\.\DISPLAYn
    pub name: String,
    pub rect: RECT,
    pub luid: LUID,
    pub adapter_name: String,
}

/// every display attached to the desktop, on every adapter
pub fn outputs() -> Vec<Output> {
    let mut out = Vec::new();
    // SAFETY: COM calls on objects this function owns
    unsafe {
        let Ok(f) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        let mut i = 0;
        while let Ok(a) = f.EnumAdapters1(i) {
            i += 1;
            let Ok(ad) = a.GetDesc1() else { continue };
            let mut j = 0;
            while let Ok(o) = a.EnumOutputs(j) {
                j += 1;
                let Ok(d) = o.GetDesc() else { continue };
                if !d.AttachedToDesktop.as_bool() {
                    continue;
                }
                out.push(Output {
                    adapter: a.clone(),
                    output: o,
                    name: from_wide(&d.DeviceName),
                    rect: d.DesktopCoordinates,
                    luid: ad.AdapterLuid,
                    adapter_name: from_wide(&ad.Description),
                });
            }
        }
    }
    out
}

/// the display by name; with none, the primary (the one at 0,0), else the first
pub fn find(name: Option<&str>) -> Option<Output> {
    let all = outputs();
    match name {
        Some(n) => all.into_iter().find(|o| o.name.eq_ignore_ascii_case(n)),
        None => {
            let (primary, rest): (Vec<_>, Vec<_>) = all.into_iter().partition(|o| o.rect.left == 0 && o.rect.top == 0);
            primary.into_iter().chain(rest).next()
        }
    }
}

/// an adapter's LUID by its name, as the config's adapter_name gives it
pub fn adapter_luid(name: &str) -> Option<LUID> {
    // SAFETY: COM calls on objects this function owns
    unsafe {
        let f = CreateDXGIFactory1::<IDXGIFactory1>().ok()?;
        let mut i = 0;
        while let Ok(a) = f.EnumAdapters1(i) {
            i += 1;
            if let Ok(d) = a.GetDesc1() {
                if from_wide(&d.Description) == name {
                    return Some(d.AdapterLuid);
                }
            }
        }
    }
    None
}

/// the GDI name of the source an adapter's target shows
pub fn gdi_name_of_target(adapter: LUID, target: u32) -> Option<String> {
    // SAFETY: buffers sized as GetDisplayConfigBufferSizes says
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
            .is_err()
        {
            return None;
        }
        let p = paths[..np as usize].iter().find(|p| {
            p.targetInfo.id == target
                && p.targetInfo.adapterId.LowPart == adapter.LowPart
                && p.targetInfo.adapterId.HighPart == adapter.HighPart
        })?;
        let mut sn = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        sn.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        sn.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        sn.header.adapterId = p.sourceInfo.adapterId;
        sn.header.id = p.sourceInfo.id;
        if DisplayConfigGetDeviceInfo(&mut sn.header) != 0 {
            return None;
        }
        Some(from_wide(&sn.viewGdiDeviceName))
    }
}

fn current_mode(name: &[u16]) -> Option<DEVMODEW> {
    let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    // SAFETY: name is NUL-terminated, dm sized
    unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool().then_some(dm) }
}

/// the display set to w×h at the nearest whole rate to millihz
pub fn set_mode(name: &str, w: u32, h: u32, millihz: u32) -> bool {
    let n = wide(name);
    let Some(mut dm) = current_mode(&n) else { return false };
    if dm.dmPelsWidth == w && dm.dmPelsHeight == h && dm.dmDisplayFrequency == (millihz + 500) / 1000 {
        return true;
    }
    let (hz, alt) = if millihz % 1000 >= 900 {
        (millihz / 1000 + 1, millihz / 1000)
    } else if millihz % 1000 > 0 {
        (millihz / 1000, millihz / 1000 + 1)
    } else {
        (millihz / 1000, (millihz / 1000).saturating_sub(1).max(1))
    };
    dm.dmPelsWidth = w;
    dm.dmPelsHeight = h;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    for f in [hz, alt] {
        dm.dmDisplayFrequency = f;
        // SAFETY: n is NUL-terminated, dm initialised
        let r = unsafe { ChangeDisplaySettingsExW(PCWSTR(n.as_ptr()), Some(&dm), None, CDS_UPDATEREGISTRY, None) };
        if r == DISP_CHANGE_SUCCESSFUL {
            info!("{name} set to {w}x{h}@{f}");
            return true;
        }
    }
    warn!("{name} wouldn't take {w}x{h}");
    false
}

/// the display made the primary one: moved to 0,0 and every other moved by as much
pub fn make_primary(name: &str) -> bool {
    let n = wide(name);
    let Some(target) = current_mode(&n) else { return false };
    // SAFETY: the union's position fields are the ones display modes use
    let (dx, dy) = unsafe {
        let p = target.Anonymous1.Anonymous2.dmPosition;
        (p.x, p.y)
    };
    if dx == 0 && dy == 0 {
        return true;
    }
    let mut i = 0;
    let mut ok = true;
    loop {
        let mut dd = DISPLAY_DEVICEW { cb: size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
        // SAFETY: dd sized
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            break;
        }
        i += 1;
        if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP).0 == 0 {
            continue;
        }
        let dn: Vec<u16> = dd.DeviceName.iter().copied().take_while(|&c| c != 0).chain([0]).collect();
        let Some(mut dm) = current_mode(&dn) else { continue };
        let is_target = from_wide(&dd.DeviceName).eq_ignore_ascii_case(name);
        // SAFETY: as above
        unsafe {
            let p = &mut dm.Anonymous1.Anonymous2.dmPosition;
            p.x -= dx;
            p.y -= dy;
        }
        dm.dmFields = DM_POSITION;
        let mut flags: CDS_TYPE = CDS_UPDATEREGISTRY | CDS_NORESET;
        if is_target {
            flags |= CDS_SET_PRIMARY;
        }
        // SAFETY: dn is NUL-terminated, dm initialised
        let r = unsafe { ChangeDisplaySettingsExW(PCWSTR(dn.as_ptr()), Some(&dm), None, flags, None) };
        ok &= r == DISP_CHANGE_SUCCESSFUL;
    }
    // SAFETY: applies what was staged above
    unsafe {
        ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
    }
    if ok {
        info!("{name} made the primary display");
    } else {
        warn!("{name} couldn't be made the primary display");
    }
    ok
}
