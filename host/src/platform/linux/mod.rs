//! The Linux host, on X11. An app gets an X server of its own (Xvfb) at the viewer's
//! size, so it streams with no monitor, no desktop and no one signed in; or, without a
//! virtual display, the session's own display ($DISPLAY). Captured with MIT-SHM when
//! XDamage says something was drawn, the pointer drawn in from XFixes, encoded by
//! OpenH264; input injected with XTest. The app is a process group of its own.

mod capture;
mod xserver;

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
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::conf::Conf;
use crate::host::{Ctl, Host, Launch, LaunchError, Out, VideoCfg};
use crate::input::Inject;
use crate::{debug, info, warn};
use xserver::{Display, Xvfb};

struct Keys {
    /// keysym to (keycode, shifted)
    map: HashMap<u32, (u8, bool)>,
    /// a keycode with nothing on it, borrowed for a character the layout lacks
    spare: Option<u8>,
    shift: u8,
}

/// the connection input goes through, to the app's display
struct Xi {
    conn: RustConnection,
    root: u32,
    keys: Keys,
}

struct App {
    child: Child,
    pgid: i32,
    /// the app's own X server; dropped after the app
    xvfb: Option<Xvfb>,
    display: Display,
}

pub struct Linux {
    xvfb_bin: String,
    session: Option<Display>,
    app: Mutex<Option<App>>,
    xi: Mutex<Option<Xi>>,
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

fn xi_for(d: &Display) -> Result<Xi, String> {
    let (conn, screen) = d.connect()?;
    let root = conn.setup().roots[screen].root;
    conn.xtest_get_version(2, 2).map_err(|e| e.to_string())?.reply().map_err(|_| format!("{} has no XTEST", d.name))?;
    let keys = keys_of(&conn)?;
    Ok(Xi { conn, root, keys })
}

fn which(bin: &str) -> bool {
    if bin.contains('/') {
        return std::path::Path::new(bin).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

impl Linux {
    pub fn open(conf: &Conf) -> Result<Arc<Linux>, String> {
        let xvfb_bin = if conf.xvfb.is_empty() { "Xvfb".to_string() } else { conf.xvfb.clone() };
        let session = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()).map(|name| Display {
            name,
            cookie: None,
            auth: None,
        });
        if !which(&xvfb_bin) {
            warn!("no {xvfb_bin}: apps get no display of their own");
        }
        if let Some(s) = &session {
            info!("the session's display: {}", s.name);
        }
        Ok(Arc::new(Linux { xvfb_bin, session, app: Mutex::new(None), xi: Mutex::new(None) }))
    }

    fn with_xi(&self, f: impl FnOnce(&mut Xi)) {
        if let Some(xi) = self.xi.lock().unwrap().as_mut() {
            f(xi);
            let _ = xi.conn.flush();
        }
    }
}

fn fake(xi: &Xi, ty: u8, detail: u8, x: i16, y: i16) {
    let _ = xi.conn.xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, xi.root, x, y, 0);
}

fn keycode(xi: &mut Xi, keysym: u32) -> Option<(u8, bool)> {
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

fn tap(xi: &mut Xi, keysym: u32) {
    let Some((code, shifted)) = keycode(xi, keysym) else { return };
    let shift = xi.keys.shift;
    if shifted {
        fake(xi, xproto::KEY_PRESS_EVENT, shift, 0, 0);
    }
    fake(xi, xproto::KEY_PRESS_EVENT, code, 0, 0);
    fake(xi, xproto::KEY_RELEASE_EVENT, code, 0, 0);
    if shifted {
        fake(xi, xproto::KEY_RELEASE_EVENT, shift, 0, 0);
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

fn screen_of(xi: &Xi) -> (u16, u16) {
    xi.conn.get_geometry(xi.root).ok().and_then(|c| c.reply().ok()).map_or((1, 1), |g| (g.width, g.height))
}

impl Inject for Linux {
    fn mouse_abs(&self, x: f32, y: f32) {
        self.with_xi(|xi| {
            let (w, h) = screen_of(xi);
            let px = (x * (w.max(2) - 1) as f32).round() as i16;
            let py = (y * (h.max(2) - 1) as f32).round() as i16;
            fake(xi, xproto::MOTION_NOTIFY_EVENT, 0, px, py);
        });
    }

    fn mouse_rel(&self, dx: i32, dy: i32) {
        self.with_xi(|xi| fake(xi, xproto::MOTION_NOTIFY_EVENT, 1, dx as i16, dy as i16));
    }

    fn button(&self, button: u8, down: bool) {
        let b = match button {
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 8,
            _ => 9,
        };
        let ty = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.with_xi(|xi| fake(xi, ty, b, 0, 0));
    }

    fn scroll(&self, amount: i32) {
        // X wheels click: up is 4, down is 5
        let (b, n) = if amount > 0 { (4, amount / 120) } else { (5, -amount / 120) };
        self.with_xi(|xi| {
            for _ in 0..n.max(1) {
                fake(xi, xproto::BUTTON_PRESS_EVENT, b, 0, 0);
                fake(xi, xproto::BUTTON_RELEASE_EVENT, b, 0, 0);
            }
        });
    }

    fn hscroll(&self, amount: i32) {
        let (b, n) = if amount > 0 { (7, amount / 120) } else { (6, -amount / 120) };
        self.with_xi(|xi| {
            for _ in 0..n.max(1) {
                fake(xi, xproto::BUTTON_PRESS_EVENT, b, 0, 0);
                fake(xi, xproto::BUTTON_RELEASE_EVENT, b, 0, 0);
            }
        });
    }

    fn key(&self, vk: u8, down: bool) {
        let Some(sym) = keysym_of(vk) else { return };
        self.with_xi(|xi| {
            let Some((code, _)) = keycode(xi, sym) else { return };
            let ty = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
            fake(xi, ty, code, 0, 0);
        });
    }

    fn text(&self, s: &str) {
        self.with_xi(|xi| {
            for c in s.chars() {
                let cp = c as u32;
                let sym = match cp {
                    0x0a | 0x0d => 0xff0d,
                    0x09 => 0xff09,
                    0x20..=0x7e | 0xa0..=0xff => cp,
                    _ => 0x0100_0000 | cp,
                };
                tap(xi, sym);
            }
            let _ = xi.conn.sync();
        });
    }
}

fn group_alive(pgid: i32) -> bool {
    // SAFETY: a signal 0 probe of a process group we made
    unsafe { libc::kill(-pgid, 0) == 0 }
}

impl Host for Linux {
    fn hostname(&self) -> String {
        std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
    }

    fn virtual_display(&self) -> bool {
        which(&self.xvfb_bin)
    }

    fn launch(&self, l: &Launch) -> Result<(), LaunchError> {
        if l.cmd.is_empty() || l.cmd.chars().any(char::is_control) {
            return Err(LaunchError::new("not an app this host can start"));
        }
        self.terminate();
        let (xvfb, display) = if l.virtual_display && self.virtual_display() {
            let x = Xvfb::start(&self.xvfb_bin, l.w as u16, l.h as u16)
                .map_err(|e| LaunchError { code: Some(503), error: e })?;
            let d = x.display.clone();
            (Some(x), d)
        } else {
            match &self.session {
                Some(s) => (None, s.clone()),
                None => {
                    return Err(LaunchError {
                        code: Some(503),
                        error: "no display or encoder to capture with: no Xvfb and no $DISPLAY".into(),
                    });
                }
            }
        };
        let xi = xi_for(&display).map_err(|e| LaunchError { code: Some(503), error: e })?;
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(&l.cmd);
        cmd.env("DISPLAY", &display.name).stdin(Stdio::null()).process_group(0);
        if let Some(a) = &display.auth {
            cmd.env("XAUTHORITY", a);
        }
        if let Some(dir) = std::path::Path::new(&l.cmd).parent().filter(|p| p.is_dir()) {
            cmd.current_dir(dir);
        }
        let child = cmd.spawn().map_err(|e| LaunchError::new(format!("the app didn't start: {e}")))?;
        let pgid = child.id() as i32;
        info!("started [{}] on {}, pid {pgid}", l.cmd, display.name);
        *self.xi.lock().unwrap() = Some(xi);
        *self.app.lock().unwrap() = Some(App { child, pgid, xvfb, display });
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
        *self.xi.lock().unwrap() = None;
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
        if group_alive(app.pgid) {
            warn!("the app didn't close in 5s: killed");
        }
        unsafe { libc::kill(-app.pgid, libc::SIGKILL) };
        let _ = app.child.wait();
        // its X server last, once nothing draws on it
        drop(app.xvfb.take());
    }

    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: Out) -> Result<(), String> {
        let (display, own) = {
            let a = self.app.lock().unwrap();
            let Some(app) = a.as_ref() else { return Err("no app is running".into()) };
            (app.display.clone(), app.xvfb.is_some())
        };
        let mut grab = capture::Grab::new(&display)?;
        // the app's own display follows the viewer's window: the app redraws at its size
        if own {
            let (conn, screen) = display.connect()?;
            let root = conn.setup().roots[screen].root;
            xserver::resize(&conn, root, v.w as u16, v.h as u16);
            grab.geometry()?;
        }
        let threads = thread::available_parallelism().map_or(1, |n| n.get()).min(4) as u16;
        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(v.kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(v.fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .num_threads(threads)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .vui(VuiConfig::bt709());
        let mut enc =
            Encoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| format!("no encoder: {e}"))?;
        let (w, h) = (v.w as usize, v.h as usize);
        let mut yuv = vec![0u8; w * h * 3 / 2];
        let period = Duration::from_secs_f64(1.0 / v.fps as f64);
        // the screen as last read, and with the pointer drawn
        let (mut screen, mut frame) = (Vec::new(), Vec::new());
        let mut cursor_at = None;
        let (mut sent, mut screen_dirty, mut pending) = (false, true, true);
        let mut next_slot = Instant::now();
        let mut last = Instant::now();
        let mut frames = 0u64;
        while !ctl.stop.load(Ordering::Relaxed) {
            let wait = if pending { next_slot.saturating_duration_since(Instant::now()) } else { period };
            if grab.wait(wait.max(Duration::from_millis(1)))? {
                screen_dirty = true;
                pending = true;
            }
            let cursor = grab.cursor();
            let at = cursor.as_ref().map(|c| (c.x, c.y, c.cursor_serial));
            if at != cursor_at {
                cursor_at = at;
                pending = true;
            }
            let idr = ctl.idr.swap(false, Ordering::Relaxed) || !sent;
            let now = Instant::now();
            // a change in its slot, a keyframe asked, or once a second the last frame again
            if !((pending && now >= next_slot) || idr || last.elapsed() >= Duration::from_secs(1)) {
                continue;
            }
            if screen_dirty {
                grab.geometry()?;
                grab.grab(&mut screen)?;
                screen_dirty = false;
            }
            let (sw, sh) = (grab.w as usize, grab.h as usize);
            frame.clear();
            frame.extend_from_slice(&screen);
            if let Some(c) = &cursor {
                capture::draw_cursor(&mut frame, sw, sh, c);
            }
            capture::to_i420(&frame, sw, sh, &mut yuv, w, h);
            pending = false;
            last = now;
            next_slot = (next_slot + period).max(now);
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
            sent = true;
            frames += 1;
            if !out(&au, key) {
                break;
            }
        }
        debug!("video: {frames} frames encoded");
        Ok(())
    }
}
