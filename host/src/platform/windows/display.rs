//! The displays: found by GDI name through DXGI, their modes set, one made primary.

use std::mem::size_of;

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_SAVE_TO_DATABASE,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG, SetDisplayConfig,
};
use windows::Win32::Foundation::{LUID, RECT};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput};
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_SET_PRIMARY, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT,
    DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplayDevicesW,
    EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

use super::{from_wide, wide};
use crate::{debug, info, warn};

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

/// the active display configuration, as CCD has it: its paths and their modes
fn config() -> Option<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    // SAFETY: buffers sized as GetDisplayConfigBufferSizes says, cut to what was filled
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).ok().ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
            .ok()
            .ok()?;
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        Some((paths, modes))
    }
}

/// the index in modes of the source mode of the display by this GDI name
fn source_mode(paths: &[DISPLAYCONFIG_PATH_INFO], modes: &[DISPLAYCONFIG_MODE_INFO], name: &str) -> Option<usize> {
    for p in paths {
        let mut sn = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        sn.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        sn.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        sn.header.adapterId = p.sourceInfo.adapterId;
        sn.header.id = p.sourceInfo.id;
        // SAFETY: the header is the struct's own, sized; the union's index is what a
        // path without virtual mode awareness holds
        unsafe {
            if DisplayConfigGetDeviceInfo(&mut sn.header) != 0
                || !from_wide(&sn.viewGdiDeviceName).eq_ignore_ascii_case(name)
            {
                continue;
            }
            let i = p.sourceInfo.Anonymous.modeInfoIdx as usize;
            if modes.get(i).is_some_and(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE) {
                return Some(i);
            }
        }
    }
    None
}

/// where a display sits on the desktop, by its GDI name: as CCD has it, else DXGI, else
/// GDI (whose view of a display just added can lag, and then said nothing)
pub fn rect_of(name: &str) -> Option<RECT> {
    if let Some((paths, modes)) = config() {
        if let Some(i) = source_mode(&paths, &modes, name) {
            // SAFETY: a source mode, as its type says
            let m = unsafe { modes[i].Anonymous.sourceMode };
            let (x, y) = (m.position.x, m.position.y);
            return Some(RECT { left: x, top: y, right: x + m.width as i32, bottom: y + m.height as i32 });
        }
    }
    if let Some(o) = find(Some(name)) {
        return Some(o.rect);
    }
    let dm = current_mode(&wide(name))?;
    // SAFETY: the union's position fields are the ones display modes use
    let p = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
    Some(RECT { left: p.x, top: p.y, right: p.x + dm.dmPelsWidth as i32, bottom: p.y + dm.dmPelsHeight as i32 })
}

/// the display set to w×h at the nearest whole rate to millihz, or at any rate it has
/// for that size. `say`: a refusal warned of (else left to the caller)
pub fn set_mode(name: &str, w: u32, h: u32, millihz: u32, say: bool) -> bool {
    let n = wide(name);
    let Some(mut dm) = current_mode(&n) else {
        if say {
            warn!("{name}'s mode can't be read: left as it is");
        }
        return false;
    };
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
    let mut last = DISP_CHANGE_SUCCESSFUL;
    // the rate asked, the one beside it, then the size at whatever rate it comes in
    for f in [Some(hz), Some(alt), None] {
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        if let Some(f) = f {
            dm.dmFields |= DM_DISPLAYFREQUENCY;
            dm.dmDisplayFrequency = f;
        }
        // SAFETY: n is NUL-terminated, dm initialised
        last = unsafe { ChangeDisplaySettingsExW(PCWSTR(n.as_ptr()), Some(&dm), None, CDS_UPDATEREGISTRY, None) };
        if last == DISP_CHANGE_SUCCESSFUL {
            let at = current_mode(&n).map_or(0, |m| m.dmDisplayFrequency);
            info!("{name} set to {w}x{h}@{at}");
            return true;
        }
    }
    let sizes = sizes_of(&n);
    let has = sizes.contains(&(w, h));
    debug!(
        "{name} turned down {w}x{h} ({}): {} sizes offered{}",
        last.0,
        sizes.len(),
        if has { ", that one among them" } else { "" }
    );
    if say {
        warn!("{name} wouldn't take {w}x{h}");
    }
    false
}

/// the sizes a display offers, each once
fn sizes_of(name: &[u16]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
        // SAFETY: name is NUL-terminated, dm sized
        if !unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm) }.as_bool() {
            break;
        }
        let s = (dm.dmPelsWidth, dm.dmPelsHeight);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// the display made the primary one: moved to 0,0 and every other moved by as much
pub fn make_primary(name: &str) -> bool {
    match primary_ccd(name) {
        Ok(true) => {
            info!("{name} made the primary display");
            return true;
        }
        Ok(false) => {}
        Err(e) => warn!("{name} couldn't be made the primary display through CCD ({e}); trying GDI"),
    }
    let n = wide(name);
    let Some(target) = current_mode(&n) else {
        warn!("{name} couldn't be made the primary display: GDI can't read its mode");
        return false;
    };
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

/// the display made primary through CCD, as Windows' own display settings do: every
/// source moved by as much as puts this one at 0,0. Ok(false): CCD doesn't know it
fn primary_ccd(name: &str) -> Result<bool, String> {
    let Some((paths, mut modes)) = config() else { return Ok(false) };
    let Some(i) = source_mode(&paths, &modes, name) else { return Ok(false) };
    // SAFETY: source modes, as their type says
    unsafe {
        let p = modes[i].Anonymous.sourceMode.position;
        if p.x == 0 && p.y == 0 {
            return Ok(true);
        }
        for m in modes.iter_mut().filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE) {
            m.Anonymous.sourceMode.position.x -= p.x;
            m.Anonymous.sourceMode.position.y -= p.y;
        }
        let r = SetDisplayConfig(
            Some(&paths),
            Some(&modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES,
        );
        if r == 0 { Ok(true) } else { Err(format!("error {r}")) }
    }
}
