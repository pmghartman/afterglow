//! The X11 backend: an X server's screen (Xvfb in CI) captured with GetImage, the
//! pointer drawn in from XFixes, encoded by OpenH264; input injected with XTest; the
//! app a process group of its own. What the link is run against off Windows.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::conf::Conf;
use crate::host::{Ctl, Host, Launch, LaunchError, Out, VideoCfg};
use crate::input::Inject;
use crate::{debug, info};

struct App {
    child: Child,
    pgid: i32,
}

struct Keys {
    /// keysym to (keycode, shifted)
    map: HashMap<u32, (u8, bool)>,
    /// a keycode with nothing on it, borrowed for a character the layout lacks
    spare: Option<u8>,
    shift: u8,
}

struct Xi {
    conn: RustConnection,
    root: u32,
    w: u16,
    h: u16,
    keys: Keys,
}

pub struct X11 {
    display: String,
    xi: Mutex<Xi>,
    app: Mutex<Option<App>>,
}

fn connect(display: &str) -> Result<(RustConnection, usize), String> {
    RustConnection::connect(Some(display)).map_err(|e| format!("no X server at {display}: {e}"))
}

fn keys_of(conn: &RustConnection) -> Result<Keys, String> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let r =
        conn.get_keyboard_mapping(min, max - min + 1).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
    let per = r.keysyms_per_keycode as usize;
    let mut map = HashMap::new();
    let mut spare = None;
    for (i, syms) in r.keysyms.chunks(per).enumerate() {
        let code = min + i as u8;
        if syms.iter().all(|&s| s == 0) {
            spare = Some(code);
            continue;
        }
        for (idx, &s) in syms.iter().take(2).enumerate() {
            if s != 0 {
                map.entry(s).or_insert((code, idx == 1));
            }
        }
    }
    let shift = map.get(&0xffe1).map(|k| k.0).unwrap_or(50);
    Ok(Keys { map, spare, shift })
}

impl X11 {
    pub fn open(_conf: &Conf) -> Result<Arc<X11>, String> {
        let display = std::env::var("DISPLAY").map_err(|_| "DISPLAY isn't set: no X server to capture".to_string())?;
        let (conn, screen) = connect(&display)?;
        let s = &conn.setup().roots[screen];
        let (root, w, h) = (s.root, s.width_in_pixels, s.height_in_pixels);
        conn.xtest_get_version(2, 2)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|_| "the X server has no XTEST".to_string())?;
        let keys = keys_of(&conn)?;
        info!("X server {display}: {w}x{h}");
        Ok(Arc::new(X11 { display, xi: Mutex::new(Xi { conn, root, w, h, keys }), app: Mutex::new(None) }))
    }

    fn fake(&self, xi: &Xi, ty: u8, detail: u8, x: i16, y: i16) {
        let _ = xi.conn.xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, xi.root, x, y, 0);
    }

    fn keycode(&self, xi: &mut Xi, keysym: u32) -> Option<(u8, bool)> {
        if let Some(&k) = xi.keys.map.get(&keysym) {
            return Some(k);
        }
        // the spare keycode given this keysym, as xdotool does
        let code = xi.keys.spare?;
        let _ = xi.conn.change_keyboard_mapping(1, code, 1, &[keysym]);
        let _ = xi.conn.sync();
        thread::sleep(Duration::from_millis(5));
        xi.keys.map.retain(|_, v| v.0 != code);
        xi.keys.map.insert(keysym, (code, false));
        Some((code, false))
    }

    fn tap(&self, xi: &mut Xi, keysym: u32) {
        let Some((code, shifted)) = self.keycode(xi, keysym) else { return };
        let shift = xi.keys.shift;
        if shifted {
            self.fake(xi, xproto::KEY_PRESS_EVENT, shift, 0, 0);
        }
        self.fake(xi, xproto::KEY_PRESS_EVENT, code, 0, 0);
        self.fake(xi, xproto::KEY_RELEASE_EVENT, code, 0, 0);
        if shifted {
            self.fake(xi, xproto::KEY_RELEASE_EVENT, shift, 0, 0);
        }
    }
}

/// a US-layout Windows virtual key as an X keysym
fn keysym_of(vk: u8) -> Option<u32> {
    Some(match vk {
        0x30..=0x39 => vk as u32,
        0x41..=0x5a => vk as u32 + 0x20,
        0x60..=0x69 => 0xffb0 + (vk - 0x60) as u32,
        0x70..=0x87 => 0xffbe + (vk - 0x70) as u32,
        0x08 => 0xff08,
        0x09 => 0xff09,
        0x0d => 0xff0d,
        0x13 => 0xff13,
        0x14 => 0xffe5,
        0x1b => 0xff1b,
        0x20 => 0x20,
        0x21 => 0xff55,
        0x22 => 0xff56,
        0x23 => 0xff57,
        0x24 => 0xff50,
        0x25 => 0xff51,
        0x26 => 0xff52,
        0x27 => 0xff53,
        0x28 => 0xff54,
        0x2c => 0xff61,
        0x2d => 0xff63,
        0x2e => 0xffff,
        0x5b => 0xffeb,
        0x5c => 0xffec,
        0x5d => 0xff67,
        0x6a => 0xffaa,
        0x6b => 0xffab,
        0x6d => 0xffad,
        0x6e => 0xffae,
        0x6f => 0xffaf,
        0x90 => 0xff7f,
        0x91 => 0xff14,
        0x10 | 0xa0 => 0xffe1,
        0xa1 => 0xffe2,
        0x11 | 0xa2 => 0xffe3,
        0xa3 => 0xffe4,
        0x12 | 0xa4 => 0xffe9,
        0xa5 => 0xffea,
        0xba => 0x3b,
        0xbb => 0x3d,
        0xbc => 0x2c,
        0xbd => 0x2d,
        0xbe => 0x2e,
        0xbf => 0x2f,
        0xc0 => 0x60,
        0xdb => 0x5b,
        0xdc => 0x5c,
        0xdd => 0x5d,
        0xde => 0x27,
        0xe2 => 0x3c,
        _ => return None,
    })
}

impl Inject for X11 {
    fn mouse_abs(&self, x: f32, y: f32) {
        let xi = self.xi.lock().unwrap();
        let px = (x * (xi.w - 1) as f32).round() as i16;
        let py = (y * (xi.h - 1) as f32).round() as i16;
        self.fake(&xi, xproto::MOTION_NOTIFY_EVENT, 0, px, py);
        let _ = xi.conn.flush();
    }

    fn mouse_rel(&self, dx: i32, dy: i32) {
        let xi = self.xi.lock().unwrap();
        self.fake(&xi, xproto::MOTION_NOTIFY_EVENT, 1, dx as i16, dy as i16);
        let _ = xi.conn.flush();
    }

    fn button(&self, button: u8, down: bool) {
        let b = match button {
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 8,
            _ => 9,
        };
        let xi = self.xi.lock().unwrap();
        let ty = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(&xi, ty, b, 0, 0);
        let _ = xi.conn.flush();
    }

    fn scroll(&self, amount: i32) {
        // X wheels click: up is 4, down is 5
        let (b, n) = if amount > 0 { (4, amount / 120) } else { (5, -amount / 120) };
        let xi = self.xi.lock().unwrap();
        for _ in 0..n.max(1) {
            self.fake(&xi, xproto::BUTTON_PRESS_EVENT, b, 0, 0);
            self.fake(&xi, xproto::BUTTON_RELEASE_EVENT, b, 0, 0);
        }
        let _ = xi.conn.flush();
    }

    fn hscroll(&self, amount: i32) {
        let (b, n) = if amount > 0 { (7, amount / 120) } else { (6, -amount / 120) };
        let xi = self.xi.lock().unwrap();
        for _ in 0..n.max(1) {
            self.fake(&xi, xproto::BUTTON_PRESS_EVENT, b, 0, 0);
            self.fake(&xi, xproto::BUTTON_RELEASE_EVENT, b, 0, 0);
        }
        let _ = xi.conn.flush();
    }

    fn key(&self, vk: u8, down: bool) {
        let Some(sym) = keysym_of(vk) else { return };
        let mut xi = self.xi.lock().unwrap();
        let Some((code, _)) = self.keycode(&mut xi, sym) else { return };
        let ty = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(&xi, ty, code, 0, 0);
        let _ = xi.conn.flush();
    }

    fn text(&self, s: &str) {
        let mut xi = self.xi.lock().unwrap();
        for c in s.chars() {
            let cp = c as u32;
            let sym = match cp {
                0x0a | 0x0d => 0xff0d,
                0x09 => 0xff09,
                0x20..=0x7e | 0xa0..=0xff => cp,
                _ => 0x0100_0000 | cp,
            };
            self.tap(&mut xi, sym);
        }
        let _ = xi.conn.sync();
    }
}

fn group_alive(pgid: i32) -> bool {
    // SAFETY: a signal 0 probe of a process group we made
    unsafe { libc::kill(-pgid, 0) == 0 }
}

impl Host for X11 {
    fn hostname(&self) -> String {
        std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
    }

    fn virtual_display(&self) -> bool {
        false
    }

    fn launch(&self, l: &Launch) -> Result<(), LaunchError> {
        if l.cmd.is_empty() || l.cmd.chars().any(char::is_control) {
            return Err(LaunchError::new("not an app this host can start"));
        }
        use std::os::unix::process::CommandExt;
        let child = Command::new(&l.cmd)
            .env("DISPLAY", &self.display)
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| LaunchError::new(format!("the app didn't start: {e}")))?;
        let pgid = child.id() as i32;
        *self.app.lock().unwrap() = Some(App { child, pgid });
        Ok(())
    }

    fn running(&self) -> bool {
        let mut a = self.app.lock().unwrap();
        let Some(app) = a.as_mut() else { return false };
        let _ = app.child.try_wait();
        group_alive(app.pgid)
    }

    fn terminate(&self) {
        let Some(mut app) = self.app.lock().unwrap().take() else { return };
        // SAFETY: signals to the process group the app was started in
        unsafe { libc::kill(-app.pgid, libc::SIGTERM) };
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) {
            let _ = app.child.try_wait();
            if !group_alive(app.pgid) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(-app.pgid, libc::SIGKILL) };
        let _ = app.child.wait();
    }

    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: Out) -> Result<(), String> {
        let (conn, screen) = connect(&self.display)?;
        let root = conn.setup().roots[screen].root;
        let has_cursor = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(v.kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(v.fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .vui(VuiConfig::bt709());
        let mut enc =
            Encoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| format!("no encoder: {e}"))?;
        let (w, h) = (v.w as usize, v.h as usize);
        let mut yuv = vec![0u8; w * h * 3 / 2];
        let period = Duration::from_secs_f64(1.0 / v.fps as f64);
        let mut prev: Vec<u8> = Vec::new();
        let mut first = true;
        let mut next = Instant::now();
        let mut frames = 0u64;
        while !ctl.stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                thread::sleep(next - now);
            }
            next += period;
            if next < Instant::now() {
                next = Instant::now() + period;
            }
            let geo = conn.get_geometry(root).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
            let (sw, sh) = (geo.width, geo.height);
            let img = conn
                .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, sw, sh, !0)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| format!("the screen can't be read: {e}"))?;
            let mut px = img.data;
            if px.len() < sw as usize * sh as usize * 4 {
                return Err(format!("a {}-bit screen; 24 or 32 wanted", img.depth));
            }
            if has_cursor {
                if let Ok(c) = conn.xfixes_get_cursor_image().map_err(|_| ()).and_then(|c| c.reply().map_err(|_| ())) {
                    draw_cursor(&mut px, sw as usize, sh as usize, &c);
                }
            }
            let idr = ctl.idr.swap(false, Ordering::Relaxed) || first;
            if !idr && px == prev {
                continue;
            }
            to_i420(&px, sw as usize, sh as usize, &mut yuv, w, h);
            prev = px;
            if idr {
                enc.force_intra_frame();
            }
            let (ys, uv) = yuv.split_at(w * h);
            let (us, vs) = uv.split_at(w * h / 4);
            let src = YUVSlices::new((ys, us, vs), (w, h), (w, w / 2, w / 2));
            let bits = enc.encode(&src).map_err(|e| format!("the encoder failed: {e}"))?;
            let key = matches!(bits.frame_type(), FrameType::IDR | FrameType::I);
            let au = bits.to_vec();
            if au.is_empty() {
                continue;
            }
            first = false;
            frames += 1;
            if !out(&au, key) {
                break;
            }
        }
        debug!("video: {frames} frames encoded");
        Ok(())
    }
}

/// XFixes' cursor image (premultiplied ARGB) over the BGRX screen
fn draw_cursor(px: &mut [u8], sw: usize, sh: usize, c: &x11rb::protocol::xfixes::GetCursorImageReply) {
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

/// BGRX at sw×sh scaled (nearest) into I420 at w×h, BT.709 limited range
fn to_i420(px: &[u8], sw: usize, sh: usize, buf: &mut [u8], w: usize, h: usize) {
    let xs: Vec<usize> = (0..w).map(|x| x * sw / w).collect();
    let (ys, rest) = buf.split_at_mut(w * h);
    let (us, vs) = rest.split_at_mut((w / 2) * (h / 2));
    for y in 0..h {
        let row = &px[(y * sh / h) * sw * 4..];
        for x in 0..w {
            let p = &row[xs[x] * 4..];
            let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
            ys[y * w + x] = ((47 * r + 157 * g + 16 * b + 128) / 256 + 16) as u8;
            if y % 2 == 0 && x % 2 == 0 {
                let c = (y / 2) * (w / 2) + x / 2;
                us[c] = ((-26 * r - 87 * g + 112 * b + 128) / 256 + 128).clamp(0, 255) as u8;
                vs[c] = ((112 * r - 102 * g - 10 * b + 128) / 256 + 128).clamp(0, 255) as u8;
            }
        }
    }
}
