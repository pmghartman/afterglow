//! Icons by the shell's own calls, no process and no compile in the way. A cloud
//! placeholder is read by its attributes only, so nothing is ever hydrated.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::Win32::Graphics::Gdi::{
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HGDIOBJ,
    ReleaseDC,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, GetFileAttributesW};
use windows::Win32::Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::UI::Controls::IImageList;
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_PIDL, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, SHGetImageList,
    SHGetSpecialFolderLocation,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO, PrivateExtractIconsW};
use windows::core::{PCWSTR, PWSTR};

use super::{SIZE as SIZE_PX, Spec};
use crate::png;

/// the system image list is not safe to grow from two threads, so an index and its
/// icon are taken one thread at a time; decoding and encoding run alongside
static LIST: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// the shell wants a COM apartment on each thread that asks it
pub fn init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

pub fn extract(spec: &Spec) -> Option<Vec<u8>> {
    match spec {
        Spec::File(p) if cloud(p) => by_attrs(p),
        Spec::File(p) => by_path(p),
        Spec::Attr(p) => by_attrs(p),
        Spec::Bin => bin(),
        Spec::Uwp(id) => uwp(id),
        Spec::Loc(p, i) => loc(p, *i),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// offline 0x1000, recall on open 0x40000, recall on data access 0x400000
fn cloud(p: &str) -> bool {
    let w = wide(p);
    let a = unsafe { GetFileAttributesW(PCWSTR(w.as_ptr())) };
    a != u32::MAX && a & 0x44_1000 != 0
}

fn by_path(p: &str) -> Option<Vec<u8>> {
    let w = wide(p);
    let g = LIST.lock().unwrap_or_else(|e| e.into_inner());
    let mut fi = SHFILEINFOW::default();
    let r = unsafe {
        SHGetFileInfoW(PCWSTR(w.as_ptr()), Default::default(), Some(&mut fi), size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX)
    };
    let h = (r != 0).then(|| sys_icon(fi.iIcon)).flatten();
    drop(g);
    h.and_then(to_png)
}

fn by_attrs(p: &str) -> Option<Vec<u8>> {
    let w = wide(p);
    let g = LIST.lock().unwrap_or_else(|e| e.into_inner());
    let mut fi = SHFILEINFOW::default();
    let r = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut fi),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
        )
    };
    let h = (r != 0).then(|| sys_icon(fi.iIcon)).flatten();
    drop(g);
    h.and_then(to_png)
}

fn bin() -> Option<Vec<u8>> {
    let _g = LIST.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        // CSIDL_BITBUCKET
        let pidl = SHGetSpecialFolderLocation(None, 0x0a).ok()?;
        let mut fi = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            PCWSTR(pidl as *const u16),
            Default::default(),
            Some(&mut fi),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_PIDL,
        );
        CoTaskMemFree(Some(pidl as *const c_void));
        let h = (r != 0).then(|| sys_icon(fi.iIcon)).flatten();
        drop(_g);
        h.and_then(to_png)
    }
}

/// the 48px entry of the system image list, as an icon handle
fn sys_icon(i: i32) -> Option<HICON> {
    unsafe {
        // SHIL_EXTRALARGE, ILD_TRANSPARENT
        let il: IImageList = SHGetImageList(2).ok()?;
        il.GetIcon(i, 1).ok()
    }
}

fn loc(path: &str, index: i32) -> Option<Vec<u8>> {
    let w = wide(path);
    let mut file = [0u16; 260];
    if w.len() > file.len() {
        return None;
    }
    file[..w.len()].copy_from_slice(&w);
    let mut h = [HICON::default()];
    let mut id = [0u32];
    let n = unsafe { PrivateExtractIconsW(&file, index, SIZE_PX as i32, SIZE_PX as i32, Some(&mut h), Some(id.as_mut_ptr()), 0) };
    if n == 0 || n == u32::MAX || h[0].is_invalid() {
        return None;
    }
    to_png(h[0])
}

/// an icon's pixels as a PNG, its handle freed
fn to_png(h: HICON) -> Option<Vec<u8>> {
    let px = unsafe { rgba(h) };
    unsafe {
        let _ = DestroyIcon(h);
    }
    let (w, ht, px) = px?;
    Some(png::rgba_z(w, ht, &px))
}

unsafe fn rgba(h: HICON) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(h, &mut ii).ok()?;
        let out = pixels(&ii);
        let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
        out
    }
}

/// a color bitmap (and its mask, when the alpha is empty) as straight RGBA
unsafe fn pixels(ii: &ICONINFO) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let out = (|| {
            if ii.hbmColor.is_invalid() {
                return None;
            }
            let mut bm = BITMAP::default();
            if GetObjectW(HGDIOBJ(ii.hbmColor.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
                return None;
            }
            let (w, ht) = (bm.bmWidth, bm.bmHeight);
            if w <= 0 || ht <= 0 || w > 1024 || ht > 1024 {
                return None;
            }
            let dc = GetDC(None);
            let mut bi = BITMAPINFO::default();
            bi.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -ht,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            let n = (w * ht) as usize;
            let mut px = vec![0u8; n * 4];
            let got = GetDIBits(dc, ii.hbmColor, 0, ht as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
            // an icon with no alpha of its own is cut out by its mask
            if got != 0 && px.chunks_exact(4).all(|p| p[3] == 0) && !ii.hbmMask.is_invalid() {
                let mut mask = vec![0u8; n * 4];
                let mut mi = bi;
                if GetDIBits(dc, ii.hbmMask, 0, ht as u32, Some(mask.as_mut_ptr() as *mut c_void), &mut mi, DIB_RGB_COLORS) != 0 {
                    for (p, m) in px.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                        p[3] = if m[0] == 0 { 255 } else { 0 };
                    }
                }
            }
            ReleaseDC(None, dc);
            if got == 0 {
                return None;
            }
            for p in px.chunks_exact_mut(4) {
                p.swap(0, 2);
            }
            if px.chunks_exact(4).all(|p| p[3] == 0) {
                return None;
            }
            Some((w as u32, ht as u32, px))
        })();
        out
    }
}

/// a Store app's tile logo file, the 48px plated one first
fn uwp(aumid: &str) -> Option<Vec<u8>> {
    let (fam, app) = aumid.split_once('!')?;
    let dir = package_dir(fam)?;
    let manifest = std::fs::read_to_string(dir.join("AppxManifest.xml")).ok()?;
    let logo = manifest_logo(&manifest, app)?;
    let file = pick_logo(&dir.join(logo.replace('/', "\\")))?;
    std::fs::read(file).ok()
}

fn package_dir(family: &str) -> Option<PathBuf> {
    unsafe {
        let fam = wide(family);
        let (mut count, mut len) = (0u32, 0u32);
        let _ = GetPackagesByPackageFamily(PCWSTR(fam.as_ptr()), &mut count, None, &mut len, None);
        if count == 0 || len == 0 {
            return None;
        }
        let mut names = vec![PWSTR::null(); count as usize];
        let mut buf = vec![0u16; len as usize];
        if GetPackagesByPackageFamily(PCWSTR(fam.as_ptr()), &mut count, Some(names.as_mut_ptr()), &mut len, Some(PWSTR(buf.as_mut_ptr())))
            .0
            != 0
        {
            return None;
        }
        // the first install of the family that has a path
        for name in names.iter().take(count as usize) {
            let mut plen = 0u32;
            let _ = GetPackagePathByFullName(PCWSTR(name.0), &mut plen, None);
            if plen == 0 {
                continue;
            }
            let mut p = vec![0u16; plen as usize];
            if GetPackagePathByFullName(PCWSTR(name.0), &mut plen, Some(PWSTR(p.as_mut_ptr()))).0 == 0 {
                return Some(PathBuf::from(String::from_utf16_lossy(&p[..plen.saturating_sub(1) as usize])));
            }
        }
        None
    }
}

/// the value of name="..." in one tag's text
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        let rest = &tag[at + name.len()..];
        let before = tag[..at].chars().next_back();
        if before.is_some_and(|c| c.is_whitespace()) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
                let body = &rest[1..];
                return body.find(q).map(|e| &body[..e]);
            }
        }
        from = at + name.len();
    }
    None
}

/// Square44x44Logo of the VisualElements in the Application with this Id
fn manifest_logo(xml: &str, app: &str) -> Option<String> {
    let mut at = 0;
    while let Some(i) = xml[at..].find("<Application") {
        let s = at + i;
        let tag_end = s + xml[s..].find('>')?;
        let tag = &xml[s..tag_end];
        at = tag_end;
        // not <Applications>
        if !tag["<Application".len()..].starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        if !attr(tag, "Id").is_some_and(|v| v.eq_ignore_ascii_case(app)) {
            continue;
        }
        let end = xml[tag_end..].find("</Application>").map_or(xml.len(), |e| tag_end + e);
        let body = &xml[tag_end..end];
        let v = body.find("VisualElements")?;
        let vt = &body[v..];
        let vt = &vt[..vt.find('>')?];
        return attr(vt, "Square44x44Logo").map(String::from);
    }
    None
}

/// the best sized file beside the manifest's logo name
fn pick_logo(full: &Path) -> Option<PathBuf> {
    let dir = full.parent()?;
    let base = full.file_stem()?.to_string_lossy().to_lowercase();
    let ext = format!(".{}", full.extension()?.to_string_lossy().to_lowercase());
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            let l = n.to_lowercase();
            l.starts_with(&base) && l.ends_with(&ext)
        })
        .collect();
    names.sort();
    for want in ["targetsize-48_altform-unplated", "targetsize-48", "scale-200", "scale-100", ""] {
        if let Some(n) = names.iter().find(|n| n.to_lowercase().contains(want)) {
            return Some(dir.join(n));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<Package><Applications>
      <Application Id="App" Executable="a.exe"><uap:VisualElements DisplayName="x" Square44x44Logo="Assets\Sq44.png" /></Application>
      <Application Id='Other'><uap:VisualElements Square150x150Logo="b.png" Square44x44Logo='Assets/Other.png'/></Application>
    </Applications></Package>"#;

    #[test]
    fn manifest_logo_of_an_application() {
        assert_eq!(manifest_logo(XML, "App").as_deref(), Some("Assets\\Sq44.png"));
        assert_eq!(manifest_logo(XML, "other").as_deref(), Some("Assets/Other.png"));
        assert_eq!(manifest_logo(XML, "Missing"), None);
    }

    #[test]
    fn a_missing_file_is_a_miss() {
        assert!(extract(&Spec::Loc("C:\\no\\such\\file.dll".into(), 0)).is_none());
        assert!(extract(&Spec::Uwp("No.Such_zzz!App".into())).is_none());
        assert!(extract(&Spec::Uwp("nobang".into())).is_none());
    }

    #[test]
    fn a_shell_icon_has_pixels() {
        init();
        let p = extract(&Spec::Attr("x.txt".into())).expect("an icon for a text file");
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&p[16..24], &[0, 0, 0, 48, 0, 0, 0, 48]);
        assert!(extract(&Spec::Bin).is_some());
    }
}
