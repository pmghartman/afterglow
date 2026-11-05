//! An X screen captured, as little of it as changed. XDamage reports each new damaged
//! rectangle as an event (no round trip to learn what changed); only those rectangles
//! are read, with MIT-SHM straight into memory shared with the server, all of them
//! asked for at once and waited on together. The screen's size comes as ConfigureNotify
//! on the root and the pointer's shape as XFixes CursorNotify, so a still screen costs
//! nothing: the capture sleeps on the X connection's socket and a wake pipe the link
//! pokes for a keyframe or a stop.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _, GetCursorImageReply};
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat};
use x11rb::rust_connection::RustConnection;

use super::xserver::Display;
use crate::debug;
use crate::host::Cursor;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    fn area(&self) -> u32 {
        self.w as u32 * self.h as u32
    }
    fn union(&self, o: &Rect) -> Rect {
        let (x0, y0) = (self.x.min(o.x), self.y.min(o.y));
        let x1 = (self.x as u32 + self.w as u32).max(o.x as u32 + o.w as u32);
        let y1 = (self.y as u32 + self.h as u32).max(o.y as u32 + o.h as u32);
        Rect { x: x0, y: y0, w: (x1 - x0 as u32) as u16, h: (y1 - y0 as u32) as u16 }
    }
    /// grown to even edges (each 2×2 chroma block whole) and kept within w×h
    pub fn even(&self, w: u16, h: u16) -> Option<Rect> {
        let x0 = self.x.min(w) & !1;
        let y0 = self.y.min(h) & !1;
        let x1 = ((self.x as u32 + self.w as u32).min(w as u32) as u16 + 1) & !1;
        let y1 = ((self.y as u32 + self.h as u32).min(h as u32) as u16 + 1) & !1;
        let (x1, y1) = (x1.min(w & !1), y1.min(h & !1));
        (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }
}

/// the damage worth reading: a few rectangles, merged where merging costs little, or
/// the whole screen when most of it changed
pub fn coalesce(rects: &mut Vec<Rect>, w: u16, h: u16) {
    let full = Rect { x: 0, y: 0, w, h };
    rects.retain_mut(|r| match r.even(w, h) {
        Some(e) => {
            *r = e;
            true
        }
        None => false,
    });
    if rects.len() > 32 {
        let u = rects.iter().skip(1).fold(rects[0], |a, b| a.union(b));
        rects.clear();
        rects.push(u);
    }
    // merge any two whose union wastes less than a 64x64 block
    let mut i = 0;
    while i < rects.len() {
        let mut merged = false;
        for j in i + 1..rects.len() {
            let u = rects[i].union(&rects[j]);
            if u.area() <= rects[i].area() + rects[j].area() + 4096 {
                rects[i] = u;
                rects.swap_remove(j);
                merged = true;
                break;
            }
        }
        if !merged {
            i += 1;
        }
    }
    let total: u32 = rects.iter().map(Rect::area).sum();
    if total * 10 > full.area() * 6 {
        rects.clear();
        rects.push(full.even(w, h).unwrap_or(full));
    }
}

struct Seg {
    id: u32,
    ptr: *mut u8,
    size: usize,
}

/// an Xvfb's framebuffer mapped read-only: its pixels read where the server draws them,
/// no request, no reply, no copy
struct Fb {
    map: *const u8,
    len: usize,
    /// where the pixels start, a row's bytes, the framebuffer's own size
    at: usize,
    pitch: usize,
    w: u16,
    h: u16,
}

impl Fb {
    fn open(path: &std::path::Path) -> Option<Fb> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let mut head = [0u8; 100];
        f.read_exact(&mut head).ok()?;
        let at = |i: usize| u32::from_be_bytes(head[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
        // XWD: header size, ..., width, height, ..., bits per pixel, bytes per line, ..., colours
        let (hsize, w, h, bpp, pitch, ncolors) = (at(0), at(4), at(5), at(11), at(12), at(19));
        let len = f.metadata().ok()?.len() as usize;
        let start = hsize + ncolors * 12;
        if bpp != 32 || pitch < w * 4 || start + pitch * h > len {
            return None;
        }
        // SAFETY: a read-only shared map of the whole file, unmapped in Drop
        let map = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            return None;
        }
        Some(Fb { map: map as *const u8, len, at: start, pitch, w: w as u16, h: h as u16 })
    }

    /// the pixels from (x, y) on, rows `pitch` apart
    fn from(&self, r: &Rect) -> &[u8] {
        let off = self.at + r.y as usize * self.pitch + r.x as usize * 4;
        let n = (r.h as usize - 1) * self.pitch + r.w as usize * 4;
        // SAFETY: r lies within the framebuffer, checked by the caller against w and h
        unsafe { std::slice::from_raw_parts(self.map.add(off), n) }
    }
}

impl Drop for Fb {
    fn drop(&mut self) {
        // SAFETY: mapped in open
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
    }
}

/// what happened while waiting
#[derive(Default)]
pub struct Changes {
    pub damage: Vec<Rect>,
    pub resized: bool,
    pub cursor: bool,
    pub woken: bool,
}

pub struct Grab {
    conn: RustConnection,
    root: u32,
    fb: Option<Fb>,
    seg: Option<Seg>,
    shm: bool,
    damage: Option<u32>,
    xfixes: bool,
    wake_rx: OwnedFd,
    wake_tx: Arc<OwnedFd>,
    pub w: u16,
    pub h: u16,
    /// GetImage's bytes when there's no SHM
    fallback: Vec<u8>,
}

// SAFETY: the segment is only touched by the thread that owns the Grab
unsafe impl Send for Grab {}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0i32; 2];
    // SAFETY: a fresh non-blocking pipe, both ends owned from here
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) != 0 {
            return Err("no pipe".into());
        }
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

impl Grab {
    pub fn new(d: &Display) -> Result<Grab, String> {
        let (conn, screen) = d.connect()?;
        let root = conn.setup().roots[screen].root;
        let shm = conn.extension_information(shm::X11_EXTENSION_NAME).ok().flatten().is_some()
            && conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some();
        let damage = if conn.damage_query_version(1, 1).ok().and_then(|c| c.reply().ok()).is_some() {
            let id = conn.generate_id().map_err(err)?;
            conn.damage_create(id, root, damage::ReportLevel::DELTA_RECTANGLES).map_err(err)?.check().map_err(err)?;
            Some(id)
        } else {
            None
        };
        let xfixes = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
        if xfixes {
            conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR).map_err(err)?;
        }
        // the root's own size changes, as RandR makes them
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))
            .map_err(err)?;
        let (wake_rx, wake_tx) = pipe()?;
        let fb = d.fb.as_deref().and_then(Fb::open);
        if fb.is_some() && xfixes {
            // Xvfb draws its pointer into the framebuffer; ours is drawn from its shape
            conn.xfixes_hide_cursor(root).map_err(err)?;
        }
        let mut g = Grab {
            conn,
            root,
            fb,
            seg: None,
            shm,
            damage,
            xfixes,
            wake_rx,
            wake_tx: Arc::new(wake_tx),
            w: 0,
            h: 0,
            fallback: Vec::new(),
        };
        g.geometry()?;
        debug!(
            "capturing {}: {}x{}, {}, {}",
            d.name,
            g.w,
            g.h,
            if g.fb.is_some() {
                "its framebuffer read in place"
            } else if g.shm {
                "MIT-SHM"
            } else {
                "GetImage"
            },
            if g.damage.is_some() { "XDamage rectangles" } else { "no XDamage, every frame read" }
        );
        Ok(g)
    }

    /// what wakes wait() from another thread
    pub fn waker(&self) -> impl Fn() + Send + Sync + 'static {
        let tx = self.wake_tx.clone();
        move || {
            // SAFETY: one byte to a pipe we own; a full pipe is already a wake
            unsafe { libc::write(tx.as_raw_fd(), [1u8].as_ptr() as *const libc::c_void, 1) };
        }
    }

    /// the screen's size asked of the server; the shared segment grown to it
    pub fn geometry(&mut self) -> Result<(u16, u16), String> {
        let g = self.conn.get_geometry(self.root).map_err(err)?.reply().map_err(err)?;
        self.sized(g.width, g.height);
        Ok((g.width, g.height))
    }

    fn sized(&mut self, w: u16, h: u16) {
        (self.w, self.h) = (w, h);
        let need = w as usize * h as usize * 4;
        if self.fb.is_none() && self.shm && self.seg.as_ref().is_none_or(|s| s.size < need) {
            self.free_seg();
            self.seg = self.alloc_seg(need);
            if self.seg.is_none() {
                self.shm = false;
            }
        }
    }

    fn alloc_seg(&self, size: usize) -> Option<Seg> {
        // SAFETY: a private segment, attached here and by the server, marked for removal
        // once both have it so nothing outlives the two of us
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                return None;
            }
            let ptr = libc::shmat(shmid, std::ptr::null(), 0);
            if ptr as isize == -1 {
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                return None;
            }
            let id = self.conn.generate_id().ok()?;
            let ok = self.conn.shm_attach(id, shmid as u32, false).ok().and_then(|c| c.check().ok()).is_some();
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            if !ok {
                libc::shmdt(ptr);
                return None;
            }
            Some(Seg { id, ptr: ptr as *mut u8, size })
        }
    }

    fn free_seg(&mut self) {
        if let Some(s) = self.seg.take() {
            let _ = self.conn.shm_detach(s.id);
            let _ = self.conn.flush();
            // SAFETY: attached by alloc_seg
            unsafe { libc::shmdt(s.ptr as *const libc::c_void) };
        }
    }

    fn take(&mut self, e: Event, ch: &mut Changes) {
        match e {
            Event::DamageNotify(d) => {
                let a = d.area;
                if a.width > 0 && a.height > 0 {
                    ch.damage.push(Rect { x: a.x.max(0) as u16, y: a.y.max(0) as u16, w: a.width, h: a.height });
                }
            }
            Event::ConfigureNotify(c) if c.window == self.root && (c.width, c.height) != (self.w, self.h) => {
                self.sized(c.width, c.height);
                ch.resized = true;
            }
            Event::XfixesCursorNotify(_) => ch.cursor = true,
            _ => {}
        }
    }

    /// what happened, waiting up to timeout for something to (None: no limit). The
    /// damage seen is subtracted so the next drawing is reported again
    pub fn wait(&mut self, timeout: Option<Duration>, ch: &mut Changes) -> Result<(), String> {
        let Some(dmg) = self.damage else {
            // no XDamage: every look is a change, the whole screen
            std::thread::sleep(timeout.unwrap_or(Duration::from_millis(16)).min(Duration::from_millis(16)));
            ch.damage.push(Rect { x: 0, y: 0, w: self.w, h: self.h });
            return Ok(());
        };
        let before = ch.damage.len();
        let mut waited = false;
        loop {
            while let Some(e) = self.conn.poll_for_event().map_err(err)? {
                self.take(e, ch);
            }
            let got = ch.damage.len() > before || ch.resized || ch.cursor || ch.woken;
            if got || waited || timeout == Some(Duration::ZERO) {
                break;
            }
            self.conn.flush().map_err(err)?;
            let mut p = [
                libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.wake_rx.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            let ms = timeout.map_or(-1, |t| t.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32);
            // SAFETY: two pollfds
            unsafe { libc::poll(p.as_mut_ptr(), 2, ms) };
            if p[1].revents != 0 {
                let mut b = [0u8; 64];
                // SAFETY: draining our own non-blocking pipe
                while unsafe { libc::read(self.wake_rx.as_raw_fd(), b.as_mut_ptr() as *mut libc::c_void, b.len()) } > 0
                {
                }
                ch.woken = true;
            }
            waited = true;
        }
        if ch.damage.len() > before {
            self.conn.damage_subtract(dmg, 0u32, 0u32).map_err(err)?;
            if self.fb.is_some() {
                // read in place, the framebuffer isn't ordered after the subtract the way
                // a GetImage request is: without this round trip, drawing the server did
                // after our read and before the subtract would be wiped from the damage
                // and never reported
                use x11rb::wrapper::ConnectionExt as _;
                self.conn.sync().map_err(err)?;
            }
        }
        Ok(())
    }

    /// the rectangles read, all asked for at once; each one's pixels and row pitch to
    /// the callback, in order
    pub fn read(&mut self, rects: &[Rect], mut each: impl FnMut(&Rect, &[u8], usize)) -> Result<(), String> {
        if let Some(fb) = &self.fb {
            // the damage subtracted and synced in wait(): everything drawn before is
            // here, everything after will be reported
            for r in rects {
                if r.w == 0 || r.h == 0 || r.x + r.w > fb.w.min(self.w) || r.y + r.h > fb.h.min(self.h) {
                    continue;
                }
                each(r, fb.from(r), fb.pitch);
            }
            return Ok(());
        }
        if let Some(s) = &self.seg {
            let mut cookies = Vec::with_capacity(rects.len());
            let mut at = 0usize;
            let mut parts = Vec::with_capacity(rects.len());
            for r in rects {
                let n = r.area() as usize * 4;
                if at + n > s.size {
                    // more than the segment: the rest on the next pass
                    break;
                }
                let c = self
                    .conn
                    .shm_get_image(
                        self.root,
                        r.x as i16,
                        r.y as i16,
                        r.w,
                        r.h,
                        !0,
                        ImageFormat::Z_PIXMAP.into(),
                        s.id,
                        at as u32,
                    )
                    .map_err(err)?;
                cookies.push(c);
                parts.push((*r, at));
                at += n;
            }
            let mut ok = Vec::with_capacity(parts.len());
            for c in cookies {
                match c.reply() {
                    Ok(r) if r.depth >= 24 => ok.push(true),
                    Ok(r) => return Err(format!("a {}-bit screen; 24 or 32 wanted", r.depth)),
                    // a rect the screen shrank away from under us
                    Err(_) => ok.push(false),
                }
            }
            for ((r, at), good) in parts.iter().zip(ok) {
                if good {
                    // SAFETY: the server wrote r's pixels at `at` before replying
                    let px = unsafe { std::slice::from_raw_parts(s.ptr.add(*at), r.area() as usize * 4) };
                    each(r, px, r.w as usize * 4);
                }
            }
            return Ok(());
        }
        for r in rects {
            let img = self
                .conn
                .get_image(ImageFormat::Z_PIXMAP, self.root, r.x as i16, r.y as i16, r.w, r.h, !0)
                .map_err(err)?
                .reply()
                .map_err(|e| format!("the screen can't be read: {e}"))?;
            if img.data.len() < r.area() as usize * 4 {
                return Err(format!("a {}-bit screen; 24 or 32 wanted", img.depth));
            }
            self.fallback = img.data;
            each(r, &self.fallback, r.w as usize * 4);
        }
        Ok(())
    }

    /// the pointer's shape and where it is, from XFixes
    pub fn cursor_image(&self) -> Option<GetCursorImageReply> {
        if !self.xfixes {
            return None;
        }
        self.conn.xfixes_get_cursor_image().ok()?.reply().ok()
    }

    /// the pointer's position on the screen, asked
    pub fn pointer(&self) -> Option<(i16, i16)> {
        self.conn.query_pointer(self.root).ok()?.reply().ok().map(|p| (p.root_x, p.root_y))
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        self.free_seg();
        if let Some(d) = self.damage {
            let _ = self.conn.damage_destroy(d);
            let _ = self.conn.flush();
        }
    }
}

/// XFixes' premultiplied ARGB as straight-alpha RGBA, as a browser's cursor wants it
pub fn cursor_of(c: &GetCursorImageReply) -> Cursor {
    let mut rgba = Vec::with_capacity(c.cursor_image.len() * 4);
    for &p in &c.cursor_image {
        let a = p >> 24;
        let un = |v: u32| (v * 255 + a / 2).checked_div(a).map_or(0, |x| x.min(255) as u8);
        rgba.extend_from_slice(&[un((p >> 16) & 0xff), un((p >> 8) & 0xff), un(p & 0xff), a as u8]);
    }
    Cursor { visible: true, w: c.width as u32, h: c.height as u32, hot_x: c.xhot as u32, hot_y: c.yhot as u32, rgba }
}

/// where the pointer's image lands on the screen, even-aligned: what's redrawn for it
pub fn cursor_rect(c: &GetCursorImageReply, at: (i16, i16), w: u16, h: u16) -> Option<Rect> {
    let x = at.0 as i32 - c.xhot as i32;
    let y = at.1 as i32 - c.yhot as i32;
    let (x0, y0) = (x.max(0), y.max(0));
    let (x1, y1) = ((x + c.width as i32).min(w as i32), (y + c.height as i32).min(h as i32));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Rect { x: x0 as u16, y: y0 as u16, w: (x1 - x0) as u16, h: (y1 - y0) as u16 }.even(w, h)
}

/// XFixes' cursor image (premultiplied ARGB) over BGRX pixels of the rect `r` (pitch
/// bytes a row), the pointer at `at`
pub fn draw_cursor(px: &mut [u8], pitch: usize, r: &Rect, c: &GetCursorImageReply, at: (i16, i16)) {
    let ox = at.0 as i32 - c.xhot as i32 - r.x as i32;
    let oy = at.1 as i32 - c.yhot as i32 - r.y as i32;
    let (cw, ch) = (c.width as i32, c.height as i32);
    for y in 0..r.h as i32 {
        let cy = y - oy;
        if cy < 0 || cy >= ch {
            continue;
        }
        for x in 0..r.w as i32 {
            let cx = x - ox;
            if cx < 0 || cx >= cw {
                continue;
            }
            let s = c.cursor_image[(cy * cw + cx) as usize];
            let a = s >> 24;
            if a == 0 {
                continue;
            }
            let at = y as usize * pitch + x as usize * 4;
            for (i, shift) in [(0, 0), (1, 8), (2, 16)] {
                let src = (s >> shift) & 0xff;
                let dst = px[at + i] as u32;
                px[at + i] = (src + dst * (255 - a) / 255).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_evened_merged_or_made_the_whole_screen() {
        let mut r = vec![Rect { x: 3, y: 5, w: 4, h: 3 }];
        coalesce(&mut r, 100, 100);
        assert_eq!(r, [Rect { x: 2, y: 4, w: 6, h: 4 }]);
        // two glyphs side by side: one rect
        let mut r = vec![Rect { x: 10, y: 10, w: 8, h: 16 }, Rect { x: 18, y: 10, w: 8, h: 16 }];
        coalesce(&mut r, 1000, 1000);
        assert_eq!(r, [Rect { x: 10, y: 10, w: 16, h: 16 }]);
        // far apart: kept apart
        let mut r = vec![Rect { x: 0, y: 0, w: 10, h: 10 }, Rect { x: 900, y: 900, w: 10, h: 10 }];
        coalesce(&mut r, 1000, 1000);
        assert_eq!(r.len(), 2);
        // most of it: the whole screen
        let mut r = vec![Rect { x: 0, y: 0, w: 90, h: 90 }];
        coalesce(&mut r, 100, 100);
        assert_eq!(r, [Rect { x: 0, y: 0, w: 100, h: 100 }]);
        // off the screen: gone
        let mut r = vec![Rect { x: 200, y: 0, w: 10, h: 10 }];
        coalesce(&mut r, 100, 100);
        assert!(r.is_empty());
    }
}
