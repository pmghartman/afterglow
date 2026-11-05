//! The app alone, as OBS's window capture and Discord's app streaming capture it: each
//! of its windows captured with Windows.Graphics.Capture, laid together as they are on
//! its display, coded by crate::avc. No desktop, taskbar, other app or lock screen can
//! reach the stream: only the app's own windows are ever read.
//!
//! Window capture is the signed-in user's, so it runs in `afterglow --grab`, started as
//! the console's user by afterglow (which runs as SYSTEM) and spoken to over its stdin
//! and stdout in the link's own framing (wire.rs):
//!
//! to it: START `{w, h, fps, kbps, display: [left, top, right, bottom]}`, PIDS
//! `[pid, ...]` (the app's processes), HWNDS `[hwnd, ...]` (windows of the app's whose
//! processes aren't: what an older copy opened for the launch), IDR, STOP (or its stdin
//! closing);
//! from it: AU `[key]` then AVCC, CURSOR `[visible][u16 hot x][u16 hot y][u16 w][u16 h]`
//! then straight RGBA, LOG text, ENDED why.
//!
//! It finds the app's windows itself, every few milliseconds, so a menu or a dialog is
//! in the stream as it opens. Only what changed is read: each window's dirty regions,
//! copied out of its frames on the GPU, put together on the CPU and coded as damage.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND, LPARAM, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP,
    HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetEvent, SetThreadPriority, THREAD_PRIORITY_HIGHEST, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, EnumWindows, GW_OWNER, GetCursorInfo, GetIconInfo, GetWindow, GetWindowRect,
    GetWindowThreadProcessId, HICON, ICONINFO, IsIconic, IsWindowVisible,
};
use windows::core::BOOL;

use super::wgc::{self, Rect};
use crate::host::{Ctl, Cursor, Sink, VideoCfg};
use crate::wire;

pub const START: u8 = 0x01;
pub const PIDS: u8 = 0x02;
pub const IDR: u8 = 0x03;
pub const STOP: u8 = 0x04;
pub const HWNDS: u8 = 0x05;
pub const AU: u8 = 0x10;
pub const CURSOR: u8 = 0x11;
pub const LOG: u8 = 0x12;
pub const ENDED: u8 = 0x13;

/// what the helper writes: whole messages, one at a time
struct Out(Mutex<std::io::Stdout>);

impl Out {
    fn send(&self, ty: u8, body: &[u8]) -> bool {
        let mut o = self.0.lock().unwrap();
        o.write_all(&wire::message(ty, body)).and_then(|_| o.flush()).is_ok()
    }
    fn log(&self, s: &str) {
        self.send(LOG, s.as_bytes());
    }
}

/// how long the screen is still before what was coded coarsely is refined
const REFINE_AFTER: Duration = Duration::from_millis(60);
/// how often the app's windows are looked for
const FIND_EVERY: Duration = Duration::from_millis(15);

/// `afterglow --grab`: until told to stop or its stdin closes
pub fn run() -> i32 {
    // SAFETY: process-wide settings made once at start; COM for this thread
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    let out = Arc::new(Out(Mutex::new(std::io::stdout())));
    // SAFETY: an auto-reset event, set by capture and by the reader, waited on below
    let wake = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(e) => {
            out.send(ENDED, format!("no event: {e}").as_bytes());
            return 1;
        }
    };
    let (tx, rx) = channel::<(u8, Vec<u8>)>();
    let w = wake.0 as usize;
    let _ = thread::Builder::new().name("grab-in".into()).spawn(move || {
        let mut r = wire::Reader::default();
        let mut buf = vec![0u8; 64 << 10];
        let mut stdin = std::io::stdin();
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            r.feed(&buf[..n]);
            while let Some(m) = r.next_message() {
                let _ = tx.send(m);
                // SAFETY: the event outlives this thread's use of it (the process ends with the loop)
                unsafe {
                    let _ = SetEvent(HANDLE(w as *mut c_void));
                }
            }
        }
        let _ = tx.send((STOP, Vec::new()));
        // SAFETY: as above
        unsafe {
            let _ = SetEvent(HANDLE(w as *mut c_void));
        }
    });
    let code = match grab(&out, &rx, wake) {
        Ok(()) => 0,
        Err(e) => {
            out.send(ENDED, e.as_bytes());
            1
        }
    };
    // SAFETY: ours
    unsafe {
        let _ = CloseHandle(wake);
    }
    code
}

fn device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let (mut dev, mut ctx) = (None, None);
    // SAFETY: out-params written by D3D11CreateDevice
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )
        .map_err(|e| format!("no Direct3D device: {e}"))?;
    }
    Ok((dev.unwrap(), ctx.unwrap()))
}

/// a window of the app's, captured, and where it is in the video
struct Placed {
    win: wgc::Window,
    at: Rect,
}

unsafe extern "system" fn collect(h: HWND, l: LPARAM) -> BOOL {
    // SAFETY: l is the Vec find() passes, alive for the call
    let v = unsafe { &mut *(l.0 as *mut Vec<HWND>) };
    v.push(h);
    true.into()
}

/// the app's visible top-level windows, bottom to top: main windows, dialogs, menus,
/// tooltips, whatever its processes show, and the windows taken as its and what they own
fn find(pids: &HashSet<u32>, taken: &HashSet<isize>) -> Vec<isize> {
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: collect only pushes to the Vec whose address it's given
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize));
    }
    let mut out = Vec::new();
    for h in all.into_iter().rev() {
        // SAFETY: queries on a window handle; a window gone since answers false or fails
        unsafe {
            if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
                continue;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            let owner = GetWindow(h, GW_OWNER).map_or(0, |o| o.0 as isize);
            if !pids.contains(&pid) && !taken.contains(&(h.0 as isize)) && !taken.contains(&owner) {
                continue;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4).is_ok()
                && cloaked != 0
            {
                continue;
            }
        }
        out.push(h.0 as isize);
    }
    out
}

/// where a window's visible frame is on the desktop
fn frame_of(hwnd: isize) -> Option<RECT> {
    let h = HWND(hwnd as *mut c_void);
    let mut r = RECT::default();
    // SAFETY: r is a RECT, sized as the attribute's
    unsafe {
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut c_void, 16).is_ok() {
            return Some(r);
        }
        GetWindowRect(h, &mut r).ok()?;
    }
    Some(r)
}

/// rectangles grown to even edges, kept within w×h; most of the screen: all of it
fn tidy(rects: &mut Vec<Rect>, w: i32, h: i32) {
    let all = Rect { x: 0, y: 0, w, h };
    let mut out: Vec<Rect> = Vec::with_capacity(rects.len());
    for r in rects.drain(..) {
        let r = r.intersect(&all);
        if r.is_empty() {
            continue;
        }
        let (x0, y0) = (r.x & !1, r.y & !1);
        let (x1, y1) = (((r.right() + 1) & !1).min(w & !1), ((r.bottom() + 1) & !1).min(h & !1));
        if x1 > x0 && y1 > y0 {
            out.push(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 });
        }
    }
    let area: i64 = out.iter().map(|r| r.w as i64 * r.h as i64).sum();
    if out.len() > 48 || area * 10 > w as i64 * h as i64 * 6 {
        out.clear();
        out.push(Rect { x: 0, y: 0, w: w & !1, h: h & !1 });
    }
    *rects = out;
}

fn grab(out: &Out, rx: &Receiver<(u8, Vec<u8>)>, wake: HANDLE) -> Result<(), String> {
    // the video asked for
    let start = loop {
        match rx.recv() {
            Ok((START, b)) => break serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string())?,
            Ok((STOP, _)) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
    };
    let num = |k: &str, d: i64| start.get(k).and_then(Value::as_i64).unwrap_or(d);
    let (w, h) = ((num("w", 1280) as i32) & !1, (num("h", 720) as i32) & !1);
    let (fps, kbps) = (num("fps", 60).clamp(10, 240) as u32, num("kbps", 20000).clamp(500, 500_000) as u32);
    let disp: Vec<i64> = start
        .get("display")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_i64).collect());
    let origin = (disp.first().copied().unwrap_or(0) as i32, disp.get(1).copied().unwrap_or(0) as i32);
    let (dev, ctx) = device()?;
    let wdev = wgc::winrt_device(&dev).map_err(|e| format!("window capture isn't available here: {e}"))?;
    let cores = thread::available_parallelism().map_or(1, |n| n.get());
    let mut enc = crate::avc::Encoder::new(w as usize, h as usize, fps, cores);
    let mut rate = crate::avc::Rate::new(kbps, fps);
    let (pw, ph) = enc.padded();
    let mut yuv = crate::convert::Yuv::padded(w as usize, h as usize, pw, ph);
    out.log(&format!("capturing the app's windows into {w}x{h} at {fps} fps, {kbps} kbps"));
    // while the app is streamed the session is in use: no idle display-off, sleep or the
    // lock that follows them (a remote viewer's input isn't the console's). Lifted when
    // this process ends
    // SAFETY: a flag on this thread
    unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };

    let mut pids: HashSet<u32> = HashSet::new();
    let mut taken: HashSet<isize> = HashSet::new();
    let mut placed: Vec<Placed> = Vec::new();
    // windows that couldn't be captured, not asked again
    let mut refused: HashSet<isize> = HashSet::new();
    let mut said_dirty = false;
    let mut dirty: Vec<Rect> = vec![Rect { x: 0, y: 0, w, h }];
    let mut changed: Vec<Rect> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let keepalive = Duration::from_secs(1);
    let (mut sent, mut idr_asked) = (false, true);
    let mut next_slot = Instant::now();
    let mut last = Instant::now();
    let mut changed_at = Instant::now();
    let mut next_refine = Instant::now();
    let mut found_at = Instant::now() - FIND_EVERY;
    let mut coarse = false;
    let mut cursor_was: (isize, bool) = (-1, false);
    let mut frames = 0u64;
    let mut enc_time = Duration::ZERO;
    let mut since = Instant::now();
    loop {
        // what the host asked
        loop {
            match rx.try_recv() {
                Ok((STOP, _)) => return Ok(()),
                Ok((IDR, _)) => idr_asked = true,
                Ok((PIDS, b)) => {
                    let list: Vec<u32> = serde_json::from_slice(&b).unwrap_or_default();
                    pids = list.into_iter().collect();
                    found_at = Instant::now() - FIND_EVERY;
                }
                Ok((HWNDS, b)) => {
                    let list: Vec<isize> = serde_json::from_slice(&b).unwrap_or_default();
                    taken = list.into_iter().collect();
                    found_at = Instant::now() - FIND_EVERY;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        // the app's windows: new ones captured, gone ones dropped, in their order
        if found_at.elapsed() >= FIND_EVERY {
            found_at = Instant::now();
            let now_list = find(&pids, &taken);
            let mut keep: HashMap<isize, Placed> = placed.drain(..).map(|p| (p.win.hwnd, p)).collect();
            for hwnd in &now_list {
                if let Some(p) = keep.remove(hwnd) {
                    placed.push(p);
                } else if !refused.contains(hwnd) {
                    match wgc::Window::new(*hwnd, &wdev, wake) {
                        Ok(win) => {
                            if !win.dirty_regions && !said_dirty {
                                said_dirty = true;
                                out.log("frames don't say what changed in them here: each is read whole");
                            }
                            placed.push(Placed { win, at: Rect::default() });
                        }
                        Err(e) => {
                            refused.insert(*hwnd);
                            out.log(&format!("window {hwnd:#x} can't be captured: {e}"));
                        }
                    }
                }
            }
            // what was under a window gone shows again
            for (_, p) in keep {
                dirty.push(p.at);
            }
        }
        // where each window is now; frames come since
        for p in &mut placed {
            if let Some(r) = frame_of(p.win.hwnd) {
                let (cw, chh) = if p.win.w > 0 { (p.win.w, p.win.h) } else { (r.right - r.left, r.bottom - r.top) };
                let at = Rect { x: r.left - origin.0, y: r.top - origin.1, w: cw, h: chh };
                if at != p.at {
                    dirty.push(p.at);
                    dirty.push(at);
                    p.at = at;
                }
            }
            changed.clear();
            match p.win.take(&dev, &ctx, &wdev, &mut changed) {
                Ok(_) => {
                    for c in &changed {
                        dirty.push(c.offset(p.at.x, p.at.y));
                    }
                }
                Err(e) => out.log(&format!("window {:#x}: {e}", p.win.hwnd)),
            }
        }
        // the pointer's shape, as it changes
        if let Some((id, visible, shape)) = cursor(cursor_was) {
            cursor_was = (id, visible);
            if !out.send(CURSOR, &shape) {
                return Ok(());
            }
        }
        let now = Instant::now();
        let pending = !dirty.is_empty();
        let idr = std::mem::take(&mut idr_asked) || !sent;
        let change = (pending && now >= next_slot) || idr;
        let alive = now >= last + keepalive;
        let refine_at = (changed_at + REFINE_AFTER).max(next_slot).max(next_refine);
        let refine = !change && !pending && coarse && now >= refine_at;
        if change || alive || refine {
            if change {
                if idr {
                    dirty.push(Rect { x: 0, y: 0, w, h });
                }
                tidy(&mut dirty, w, h);
                for d in &dirty {
                    compose(d, &placed, &ctx, &mut scratch);
                    yuv.put(&scratch, d.w as usize * 4, d.x as usize, d.y as usize, d.w as usize, d.h as usize);
                    enc.mark(d.x as usize, d.y as usize, d.w as usize, d.h as usize);
                }
                dirty.clear();
                next_slot = (next_slot + period).max(now);
            }
            last = now;
            let [y, u, v] = yuv.planes();
            let frame = crate::avc::Frame { y, u, v };
            let (opts, act) = if refine {
                let fine =
                    if enc.coarse(crate::avc::QP_LEGIBLE) > 0 { crate::avc::QP_READ } else { crate::avc::QP_FINE };
                (
                    crate::avc::Opts { idr: false, qp: fine, refine: Some((fine, rate.refine_bytes())), always: false },
                    0.0,
                )
            } else {
                let (mbs, act) = enc.activity(&frame, idr);
                (crate::avc::Opts { idr, qp: rate.qp(mbs, act), refine: None, always: alive && !change }, act)
            };
            let t0 = Instant::now();
            let d = enc.encode(&frame, &opts);
            enc_time += t0.elapsed();
            rate.seen(&d, opts.qp, act);
            if d.frame {
                let mut body = Vec::with_capacity(1 + d.bytes + 4 * 8);
                body.push(d.key as u8);
                for n in enc.nals() {
                    body.extend_from_slice(&(n.len() as u32).to_be_bytes());
                    body.extend_from_slice(n);
                }
                if !out.send(AU, &body) {
                    return Ok(());
                }
                sent = true;
                frames += 1;
                if change {
                    changed_at = now;
                }
                if refine {
                    next_refine = now + period;
                }
                coarse = enc.coarse(crate::avc::QP_FINE) > 0;
            }
        }
        if since.elapsed() >= Duration::from_secs(5) && frames > 0 {
            out.log(&format!(
                "{} windows, {:.0} frames/s, encode {:.0} us a frame",
                placed.len(),
                frames as f64 / since.elapsed().as_secs_f64(),
                enc_time.as_secs_f64() * 1e6 / frames as f64
            ));
            (frames, enc_time, since) = (0, Duration::ZERO, Instant::now());
        }
        // asleep until a window changes, the host asks, or it's time
        let now = Instant::now();
        let until = if !dirty.is_empty() {
            next_slot
        } else if coarse {
            refine_at.min(last + keepalive)
        } else {
            last + keepalive
        };
        let ms = until.saturating_duration_since(now).min(FIND_EVERY).as_millis() as u32;
        // SAFETY: an event this function was given
        unsafe { WaitForSingleObject(wake, if ms == 0 && dirty.is_empty() { 1 } else { ms }) };
    }
}

/// a rectangle of the video as the app's windows show it: black where none is, each
/// window over those below it
fn compose(d: &Rect, placed: &[Placed], ctx: &ID3D11DeviceContext, scratch: &mut Vec<u8>) {
    let (dw, dh) = (d.w as usize, d.h as usize);
    scratch.clear();
    scratch.resize(dw * dh * 4, 0);
    for p in placed {
        let i = d.intersect(&p.at);
        if i.is_empty() {
            continue;
        }
        p.win.read(ctx, |px, pitch| {
            let (sx, sy) = ((i.x - p.at.x) as usize, (i.y - p.at.y) as usize);
            let (dx, dy) = ((i.x - d.x) as usize, (i.y - d.y) as usize);
            let n = i.w as usize * 4;
            for row in 0..i.h as usize {
                let s = (sy + row) * pitch + sx * 4;
                let t = (dy + row) * dw * 4 + dx * 4;
                if s + n <= px.len() {
                    scratch[t..t + n].copy_from_slice(&px[s..s + n]);
                }
            }
        });
    }
}

/// the pointer's shape when it changed since `was` (its handle, whether it showed):
/// the message body to send
fn cursor(was: (isize, bool)) -> Option<(isize, bool, Vec<u8>)> {
    let mut ci = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
    // SAFETY: ci sized as asked
    unsafe { GetCursorInfo(&mut ci).ok()? };
    let visible = ci.flags.0 & CURSOR_SHOWING.0 != 0 && !ci.hCursor.is_invalid();
    let id = ci.hCursor.0 as isize;
    if (id, visible) == was {
        return None;
    }
    let c = if visible { shape(HICON(ci.hCursor.0)).unwrap_or_default() } else { Cursor::default() };
    let mut b = Vec::with_capacity(9 + c.rgba.len());
    b.push(c.visible as u8);
    for v in [c.hot_x, c.hot_y, c.w, c.h] {
        b.extend_from_slice(&(v.min(0xffff) as u16).to_le_bytes());
    }
    b.extend_from_slice(&c.rgba);
    Some((id, visible, b))
}

/// a bitmap's pixels as 32-bit BGRA rows, top down
fn bits(dc: windows::Win32::Graphics::Gdi::HDC, bmp: HBITMAP) -> Option<(usize, usize, Vec<u8>)> {
    let mut bm = BITMAP::default();
    // SAFETY: bm sized as passed; the DIB buffer sized for w×h×4
    unsafe {
        if GetObjectW(HGDIOBJ(bmp.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut c_void)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth.max(0) as usize, bm.bmHeight.max(0) as usize);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; w * h * 4];
        if GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS) == 0 {
            return None;
        }
        Some((w, h, buf))
    }
}

/// a cursor's shape as straight-alpha RGBA. What inverts the screen (a monochrome XOR)
/// can't be said in RGBA: drawn black, so an I-beam stays visible over light pages
fn shape(icon: HICON) -> Option<Cursor> {
    let mut ii = ICONINFO::default();
    // SAFETY: the icon's bitmaps are this function's to delete; the screen's DC released
    unsafe {
        GetIconInfo(icon, &mut ii).ok()?;
        let dc = GetDC(None);
        let color = (!ii.hbmColor.is_invalid()).then(|| bits(dc, ii.hbmColor)).flatten();
        let mask = (!ii.hbmMask.is_invalid()).then(|| bits(dc, ii.hbmMask)).flatten();
        ReleaseDC(None, dc);
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
        let (hx, hy) = (ii.xHotspot, ii.yHotspot);
        let mut rgba;
        let (w, h);
        if let Some((cw, ch, c)) = color {
            (w, h) = (cw, ch);
            rgba = vec![0u8; w * h * 4];
            let has_alpha = c.chunks_exact(4).any(|p| p[3] != 0);
            for i in 0..w * h {
                let p = &c[i * 4..i * 4 + 4];
                let a = if has_alpha {
                    p[3]
                } else {
                    // the AND mask: set where the screen shows through
                    let m = mask.as_ref().map_or(0, |(_, _, m)| m.get(i * 4).copied().unwrap_or(0));
                    if m == 0 { 255 } else { 0 }
                };
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[p[2], p[1], p[0], a]);
            }
        } else {
            let (mw, mh, m) = mask?;
            (w, h) = (mw, mh / 2);
            rgba = vec![0u8; w * h * 4];
            for i in 0..w * h {
                let and = m[i * 4] != 0;
                let xor = m[(i + w * h) * 4] != 0;
                let px: [u8; 4] = match (and, xor) {
                    (false, false) => [0, 0, 0, 255],
                    (false, true) => [255, 255, 255, 255],
                    (true, false) => [0, 0, 0, 0],
                    (true, true) => [0, 0, 0, 255],
                };
                rgba[i * 4..i * 4 + 4].copy_from_slice(&px);
            }
        }
        Some(Cursor { visible: true, w: w as u32, h: h as u32, hot_x: hx, hot_y: hy, rgba })
    }
}

/// afterglow's side: the helper started (as the console's user when afterglow is
/// SYSTEM), told the video, the app's processes and keyframes, its frames passed on
pub struct Helper {
    process: HANDLE,
    to: Mutex<std::fs::File>,
    from: Receiver<(u8, Vec<u8>)>,
}

// SAFETY: the process handle is only waited on and closed
unsafe impl Send for Helper {}

impl Helper {
    pub fn start() -> Result<Helper, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let cmd = format!("\"{}\" --grab", exe.display());
        let (process, to, from) = super::process::spawn_piped(&cmd)?;
        let (tx, rx): (Sender<(u8, Vec<u8>)>, _) = channel();
        let _ = thread::Builder::new().name("grab-out".into()).spawn(move || {
            let mut from = from;
            let mut r = wire::Reader::default();
            let mut buf = vec![0u8; 256 << 10];
            loop {
                let n = match from.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if !r.feed(&buf[..n]) {
                    break;
                }
                while let Some(m) = r.next_message() {
                    if tx.send(m).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Helper { process, to: Mutex::new(to), from: rx })
    }

    fn send(&self, ty: u8, body: &[u8]) -> bool {
        self.to.lock().unwrap().write_all(&wire::message(ty, body)).is_ok()
    }

    /// the video run through the helper until stopped (Ok), or it fails (Err)
    pub fn video(
        &self,
        v: &VideoCfg,
        ctl: &Ctl,
        out: &mut dyn Sink,
        display: RECT,
        pids: impl Fn() -> Vec<u32>,
        taken: impl Fn() -> Vec<isize>,
    ) -> Result<(), String> {
        let start = json!({
            "w": v.w, "h": v.h, "fps": v.fps, "kbps": v.kbps,
            "display": [display.left, display.top, display.right, display.bottom],
        });
        if !self.send(START, start.to_string().as_bytes()) {
            return Err("the capture process didn't take the video".into());
        }
        let mut told: Vec<u32> = Vec::new();
        let mut told_w: Vec<isize> = Vec::new();
        let mut asked = Instant::now() - Duration::from_secs(1);
        while !ctl.stop.load(std::sync::atomic::Ordering::Relaxed) {
            if asked.elapsed() >= Duration::from_millis(50) {
                asked = Instant::now();
                let mut p = pids();
                p.sort_unstable();
                if p != told {
                    self.send(PIDS, serde_json::to_string(&p).unwrap_or_default().as_bytes());
                    told = p;
                }
                let mut t = taken();
                t.sort_unstable();
                if t != told_w {
                    self.send(HWNDS, serde_json::to_string(&t).unwrap_or_default().as_bytes());
                    told_w = t;
                }
            }
            if ctl.idr.swap(false, std::sync::atomic::Ordering::Relaxed) {
                self.send(IDR, &[]);
            }
            match self.from.recv_timeout(Duration::from_millis(10)) {
                Ok((AU, b)) if !b.is_empty() => {
                    let mut nals: Vec<&[u8]> = Vec::with_capacity(8);
                    let mut i = 1;
                    while i + 4 <= b.len() {
                        let n = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
                        if i + 4 + n > b.len() {
                            break;
                        }
                        nals.push(&b[i + 4..i + 4 + n]);
                        i += 4 + n;
                    }
                    if !out.frame(&nals, b[0] == 1) {
                        return Ok(());
                    }
                }
                Ok((CURSOR, b)) if b.len() >= 9 => {
                    let u = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as u32;
                    let c = Cursor {
                        visible: b[0] == 1,
                        hot_x: u(1),
                        hot_y: u(3),
                        w: u(5),
                        h: u(7),
                        rgba: b[9..].to_vec(),
                    };
                    if !out.cursor(&c) {
                        return Ok(());
                    }
                }
                Ok((LOG, b)) => crate::info!("capture: {}", String::from_utf8_lossy(&b)),
                Ok((ENDED, b)) => return Err(String::from_utf8_lossy(&b).into_owned()),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("the capture process ended".into()),
            }
        }
        Ok(())
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.send(STOP, &[]);
        // SAFETY: a process handle this owns: waited on briefly, then ended, then closed
        unsafe {
            if WaitForSingleObject(self.process, 1000) != WAIT_OBJECT_0 {
                let _ = windows::Win32::System::Threading::TerminateProcess(self.process, 1);
            }
            let _ = CloseHandle(self.process);
        }
    }
}
