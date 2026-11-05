//! An X screen captured: told by XDamage when anything was drawn, read with MIT-SHM
//! straight into memory shared with the server (GetImage over the socket where SHM
//! can't be had), the pointer from XFixes since the screen holds none.

use std::os::fd::AsRawFd;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{ConnectionExt as _, GetCursorImageReply};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

use super::xserver::Display;
use crate::debug;

struct Seg {
    id: u32,
    shmid: i32,
    ptr: *mut u8,
    size: usize,
}

pub struct Grab {
    conn: RustConnection,
    root: u32,
    seg: Option<Seg>,
    shm: bool,
    damage: Option<u32>,
    cursor: bool,
    pub w: u16,
    pub h: u16,
}

// SAFETY: the segment is only touched by the thread that owns the Grab
unsafe impl Send for Grab {}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Grab {
    pub fn new(d: &Display) -> Result<Grab, String> {
        let (conn, screen) = d.connect()?;
        let root = conn.setup().roots[screen].root;
        let shm = conn.extension_information(shm::X11_EXTENSION_NAME).ok().flatten().is_some()
            && conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some();
        let damage = if conn.damage_query_version(1, 1).ok().and_then(|c| c.reply().ok()).is_some() {
            let id = conn.generate_id().map_err(err)?;
            conn.damage_create(id, root, damage::ReportLevel::NON_EMPTY).map_err(err)?.check().map_err(err)?;
            Some(id)
        } else {
            None
        };
        let cursor = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
        let mut g = Grab { conn, root, seg: None, shm, damage, cursor, w: 0, h: 0 };
        g.geometry()?;
        debug!(
            "capturing {}: {}x{}, {}, {}",
            d.name,
            g.w,
            g.h,
            if g.shm { "MIT-SHM" } else { "GetImage" },
            if g.damage.is_some() { "XDamage" } else { "no XDamage, every frame read" }
        );
        Ok(g)
    }

    /// the screen's size now; the shared segment grown to it
    pub fn geometry(&mut self) -> Result<(u16, u16), String> {
        let g = self.conn.get_geometry(self.root).map_err(err)?.reply().map_err(err)?;
        (self.w, self.h) = (g.width, g.height);
        let need = g.width as usize * g.height as usize * 4;
        if self.shm && self.seg.as_ref().is_none_or(|s| s.size < need) {
            self.free_seg();
            self.seg = self.alloc_seg(need);
            if self.seg.is_none() {
                self.shm = false;
            }
        }
        Ok((g.width, g.height))
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
            Some(Seg { id, shmid, ptr: ptr as *mut u8, size })
        }
    }

    fn free_seg(&mut self) {
        if let Some(s) = self.seg.take() {
            let _ = self.conn.shm_detach(s.id);
            let _ = self.conn.flush();
            // SAFETY: attached by alloc_seg
            unsafe { libc::shmdt(s.ptr as *const libc::c_void) };
            let _ = s.shmid;
        }
    }

    /// whether anything was drawn within the timeout; the damage taken as seen
    pub fn wait(&mut self, timeout: Duration) -> Result<bool, String> {
        let Some(dmg) = self.damage else {
            // no XDamage: every look is a change
            std::thread::sleep(timeout);
            return Ok(true);
        };
        let mut damaged = false;
        let fd = self.conn.stream().as_raw_fd();
        let mut waited = false;
        loop {
            match self.conn.poll_for_event().map_err(err)? {
                Some(Event::DamageNotify(_)) => damaged = true,
                Some(_) => {}
                None if damaged || waited => break,
                None => {
                    self.conn.flush().map_err(err)?;
                    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                    // SAFETY: one pollfd
                    unsafe { libc::poll(&mut p, 1, timeout.as_millis().min(1000) as i32) };
                    waited = true;
                }
            }
        }
        if damaged {
            self.conn.damage_subtract(dmg, 0u32, 0u32).map_err(err)?;
        }
        Ok(damaged)
    }

    /// the whole screen as BGRX into buf
    pub fn grab(&mut self, buf: &mut Vec<u8>) -> Result<(), String> {
        let (w, h) = (self.w, self.h);
        let n = w as usize * h as usize * 4;
        if let Some(s) = &self.seg {
            let r = self
                .conn
                .shm_get_image(self.root, 0, 0, w, h, !0, ImageFormat::Z_PIXMAP.into(), s.id, 0)
                .map_err(err)?
                .reply()
                .map_err(|e| format!("the screen can't be read: {e}"))?;
            if r.depth < 24 {
                return Err(format!("a {}-bit screen; 24 or 32 wanted", r.depth));
            }
            buf.resize(n, 0);
            // SAFETY: the server wrote n bytes into the segment before replying
            unsafe { std::ptr::copy_nonoverlapping(s.ptr, buf.as_mut_ptr(), n) };
            return Ok(());
        }
        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, !0)
            .map_err(err)?
            .reply()
            .map_err(|e| format!("the screen can't be read: {e}"))?;
        if img.data.len() < n {
            return Err(format!("a {}-bit screen; 24 or 32 wanted", img.depth));
        }
        *buf = img.data;
        buf.truncate(n);
        Ok(())
    }

    pub fn cursor(&self) -> Option<GetCursorImageReply> {
        if !self.cursor {
            return None;
        }
        self.conn.xfixes_get_cursor_image().ok()?.reply().ok()
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

/// XFixes' cursor image (premultiplied ARGB) over the BGRX screen
pub fn draw_cursor(px: &mut [u8], sw: usize, sh: usize, c: &GetCursorImageReply) {
    let (cw, ch) = (c.width as i32, c.height as i32);
    let ox = c.x as i32 - c.xhot as i32;
    let oy = c.y as i32 - c.yhot as i32;
    for y in 0..ch {
        let ty = oy + y;
        if ty < 0 || ty >= sh as i32 {
            continue;
        }
        for x in 0..cw {
            let tx = ox + x;
            if tx < 0 || tx >= sw as i32 {
                continue;
            }
            let s = c.cursor_image[(y * cw + x) as usize];
            let a = s >> 24;
            if a == 0 {
                continue;
            }
            let at = (ty as usize * sw + tx as usize) * 4;
            for (i, shift) in [(0, 0), (1, 8), (2, 16)] {
                let src = (s >> shift) & 0xff;
                let dst = px[at + i] as u32;
                px[at + i] = (src + dst * (255 - a) / 255).min(255) as u8;
            }
        }
    }
}

/// BGRX at sw×sh into I420 at w×h (nearest-neighbour scaled, the aspect kept with
/// black around it), BT.709 limited range
pub fn to_i420(px: &[u8], sw: usize, sh: usize, out: &mut [u8], w: usize, h: usize) {
    let scale = (w as f64 / sw as f64).min(h as f64 / sh as f64);
    let (fw, fh) = (((sw as f64 * scale) as usize).min(w), ((sh as f64 * scale) as usize).min(h));
    let (ox, oy) = ((w - fw) / 2, (h - fh) / 2);
    let (ys, uv) = out.split_at_mut(w * h);
    let (us, vs) = uv.split_at_mut((w / 2) * (h / 2));
    ys.fill(16);
    us.fill(128);
    vs.fill(128);
    let xs: Vec<usize> = (0..fw).map(|x| (x * sw / fw.max(1)).min(sw - 1) * 4).collect();
    for y in 0..fh {
        let row = &px[((y * sh / fh.max(1)).min(sh - 1)) * sw * 4..];
        let (ty, chroma) = (oy + y, (oy + y) % 2 == 0);
        let yrow = &mut ys[ty * w + ox..ty * w + ox + fw];
        for (x, o) in yrow.iter_mut().enumerate() {
            let p = &row[xs[x]..xs[x] + 3];
            let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
            *o = ((47 * r + 157 * g + 16 * b + 128) >> 8) as u8 + 16;
            let tx = ox + x;
            if chroma && tx % 2 == 0 {
                let c = (ty / 2) * (w / 2) + tx / 2;
                us[c] = (((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                vs[c] = (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_and_black_to_limited_range_and_letterboxed() {
        // 2x1 white over a 4x4 video: the picture 4x2 in the middle rows, black above and below
        let px = [255u8, 255, 255, 0, 255, 255, 255, 0];
        let mut out = vec![0u8; 4 * 4 * 3 / 2];
        to_i420(&px, 2, 1, &mut out, 4, 4);
        let y = &out[..16];
        assert_eq!(&y[..4], &[16; 4]);
        assert_eq!(&y[4..12], &[235; 8]);
        assert_eq!(&y[12..], &[16; 4]);
        assert!(out[16..].iter().all(|&c| (127..=129).contains(&c)));
    }
}
