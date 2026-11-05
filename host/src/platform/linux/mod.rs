//! The Linux host, on X11. An app gets an X server of its own (Xvfb) at the viewer's
//! size, so it streams with no monitor, no desktop and no one signed in; or, without a
//! virtual display, the session's own display ($DISPLAY). Captured with MIT-SHM when
//! XDamage says something was drawn, the pointer drawn in from XFixes, encoded by
//! OpenH264; input injected with XTest. The app is a process group of its own.

mod capture;
mod convert;
mod encode;
mod wm;
mod xserver;

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::conf::Conf;
use crate::host::{Ctl, Host, Launch, LaunchError, Notes, Sink, VideoCfg};
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
    /// the screen's size, kept so a pointer move is one request and no round trip
    size: (u16, u16),
}

struct App {
    child: Child,
    pgid: i32,
    /// the app's own X server; dropped after the app
    xvfb: Option<Xvfb>,
    display: Display,
    dpi: u32,
    /// the last of what the app wrote, for the viewer when it ends
    log: Arc<Mutex<Vec<u8>>>,
    /// when its process group was first found gone, no window ever shown
    gone_at: Option<Instant>,
}

impl App {
    fn wm(&self) -> Option<&wm::Wm> {
        self.xvfb.as_ref().and_then(|x| x.wm.as_ref())
    }
}

pub struct Linux {
    xvfb_bin: String,
    session: Option<Display>,
    app: Mutex<Option<App>>,
    xi: Mutex<Option<Xi>>,
    spare: Spare,
    notes: Mutex<Option<Notes>>,
    /// how the last app ended
    ended: Mutex<serde_json::Value>,
}

/// the most of the app's output kept
const LOG_KEEP: usize = 16 << 10;

/// an Xvfb started ahead of the launch that takes it: half of an app's time to its first
/// frame was its X server coming up. Another is started as soon as one is taken, unless
/// this afterglow serves one app and goes (--stdio)
#[derive(Clone)]
struct Spare {
    bin: String,
    slot: Arc<Mutex<Option<Xvfb>>>,
    starting: Arc<std::sync::atomic::AtomicBool>,
    keep: bool,
}

impl Spare {
    fn refill(&self) {
        if self.starting.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let _ = thread::Builder::new().name("spare-xvfb".into()).spawn(move || {
            let x = Xvfb::start(&me.bin, xserver::MAX.0, xserver::MAX.1, 96);
            match x {
                Ok(x) => *me.slot.lock().unwrap() = Some(x),
                Err(e) => warn!("no spare X server: {e}"),
            }
            me.starting.store(false, Ordering::SeqCst);
        });
    }

    /// the spare, if one is up and still serving, set to w×h at dpi; one being started
    /// waited for (it's further along than a fresh one would be)
    fn take(&self, w: u16, h: u16, dpi: u32) -> Option<Xvfb> {
        let t0 = Instant::now();
        while self.starting.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(2));
        }
        let mut x = self.slot.lock().unwrap().take()?;
        if w > xserver::MAX.0 || h > xserver::MAX.1 || !x.alive() || x.fit(w, h, dpi).is_err() {
            return None;
        }
        Some(x)
    }
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
    let size = conn.get_geometry(root).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
    Ok(Xi { conn, root, keys, size: (size.width, size.height) })
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
            fb: None,
        });
        if !which(&xvfb_bin) {
            warn!("no {xvfb_bin}: apps get no display of their own");
        }
        if let Some(s) = &session {
            info!("the session's display: {}", s.name);
        }
        let spare = Spare { bin: xvfb_bin.clone(), slot: Arc::default(), starting: Arc::default(), keep: !conf.stdio };
        if which(&xvfb_bin) {
            spare.refill();
        }
        Ok(Arc::new(Linux {
            xvfb_bin,
            session,
            app: Mutex::new(None),
            xi: Mutex::new(None),
            spare,
            notes: Mutex::new(None),
            ended: Mutex::new(serde_json::json!({})),
        }))
    }

    /// the size the screen input lands on now is
    fn screen_is(&self, w: u16, h: u16) {
        if let Some(xi) = self.xi.lock().unwrap().as_mut() {
            xi.size = (w, h);
        }
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

impl Inject for Linux {
    fn mouse_abs(&self, x: f32, y: f32) {
        self.with_xi(|xi| {
            let (w, h) = xi.size;
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
        });
    }
}

/// what r says, its last LOG_KEEP bytes kept in log
fn keep_tail(mut r: Box<dyn std::io::Read + Send>, log: &Mutex<Vec<u8>>) {
    let mut b = [0u8; 4096];
    while let Ok(n) = r.read(&mut b) {
        if n == 0 {
            break;
        }
        let mut l = log.lock().unwrap();
        l.extend_from_slice(&b[..n]);
        let over = l.len().saturating_sub(LOG_KEEP);
        l.drain(..over);
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
        let mut argv: Vec<String> = if l.argv.is_empty() { vec![l.cmd.clone()] } else { l.argv.clone() };
        if argv[0].is_empty() || argv.iter().any(|a| a.contains('\0')) || argv[0].chars().any(char::is_control) {
            return Err(LaunchError::new("not an app this host can start"));
        }
        if !which(&argv[0]) {
            return Err(LaunchError::new(format!("{} isn't on this host's PATH", argv[0])));
        }
        self.terminate();
        *self.ended.lock().unwrap() = serde_json::json!({});
        let (xvfb, display) = if l.virtual_display && self.virtual_display() {
            let x = match self.spare.take(l.w as u16, l.h as u16, l.dpi) {
                Some(x) => x,
                None => Xvfb::start(&self.xvfb_bin, l.w as u16, l.h as u16, l.dpi)
                    .map_err(|e| LaunchError { code: Some(503), error: e })?,
            };
            if self.spare.keep {
                self.spare.refill();
            }
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
        // its own session bus: a single-instance app starts here rather than handing its
        // window to a copy already open on the host's own screen
        let dbus = l.dbus && which("dbus-run-session");
        if dbus {
            argv.splice(0..0, ["dbus-run-session".to_string(), "--".to_string()]);
        }
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
        // X11 for every toolkit, on this display, nothing of a Wayland session it may run in
        for k in ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DESKTOP_STARTUP_ID", "XDG_ACTIVATION_TOKEN"] {
            cmd.env_remove(k);
        }
        if dbus {
            cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
        }
        cmd.env("DISPLAY", &display.name)
            .env("XDG_SESSION_TYPE", "x11")
            .env("GDK_BACKEND", "x11")
            .env("QT_QPA_PLATFORM", "xcb")
            .env("SDL_VIDEODRIVER", "x11")
            .env("MOZ_ENABLE_WAYLAND", "0")
            .env("ELECTRON_OZONE_PLATFORM_HINT", "x11")
            .env("NO_AT_BRIDGE", "1")
            .env("_JAVA_AWT_WM_NONREPARENTING", "1");
        match &display.auth {
            Some(a) => {
                cmd.env("XAUTHORITY", a);
            }
            None => {
                cmd.env_remove("XAUTHORITY");
            }
        }
        if l.scale >= 1.5 {
            cmd.env("GDK_SCALE", format!("{}", l.scale.round() as u32))
                .env("QT_SCALE_FACTOR", format!("{:.2}", l.scale));
        }
        // a desktop entry's app starts at home, as a launcher starts it; a program by its path beside itself
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).filter(|h| h.is_dir());
        match (l.argv.is_empty(), home) {
            (false, Some(h)) => {
                cmd.current_dir(h);
            }
            _ => {
                if let Some(dir) = std::path::Path::new(&argv[0]).parent().filter(|p| p.is_dir()) {
                    cmd.current_dir(dir);
                }
            }
        }
        let mut child = cmd.spawn().map_err(|e| LaunchError::new(format!("{} didn't start: {e}", argv[0])))?;
        let log: Arc<Mutex<Vec<u8>>> = Arc::default();
        for r in [
            child.stdout.take().map(|o| Box::new(o) as Box<dyn std::io::Read + Send>),
            child.stderr.take().map(|e| Box::new(e) as _),
        ]
        .into_iter()
        .flatten()
        {
            let log = log.clone();
            let _ = thread::Builder::new().name("app-log".into()).spawn(move || keep_tail(r, &log));
        }
        let pgid = child.id() as i32;
        info!("started [{}] on {}, pid {pgid}", argv.join(" "), display.name);
        if let Some(w) = xvfb.as_ref().and_then(|x| x.wm.as_ref()) {
            w.set_notes(self.notes.lock().unwrap().clone());
        }
        *self.xi.lock().unwrap() = Some(xi);
        *self.app.lock().unwrap() = Some(App { child, pgid, xvfb, display, dpi: l.dpi, log, gone_at: None });
        Ok(())
    }

    fn running(&self) -> bool {
        let mut a = self.app.lock().unwrap();
        let Some(app) = a.as_mut() else { return false };
        let status = app.child.try_wait().ok().flatten();
        if group_alive(app.pgid) {
            return true;
        }
        let wins = app.wm().map(|w| (w.windows(), w.ever_had_a_window()));
        match wins {
            // its process gone, a window of it still up: one it handed off to runs it
            Some((n, _)) if n > 0 => return true,
            // never a window yet: a launcher that exits after starting the app gets a moment
            Some((_, false)) if app.gone_at.get_or_insert_with(Instant::now).elapsed() < Duration::from_secs(3) => {
                return true;
            }
            _ => {}
        }
        let log = app.log.lock().unwrap();
        let tail = String::from_utf8_lossy(&log[log.len().saturating_sub(4000)..]).into_owned();
        let code = status.map(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.code().unwrap_or_else(|| -s.signal().unwrap_or(0))
        });
        let mut v = serde_json::json!({"log": tail, "code": code});
        if wins == Some((0, false)) {
            v["error"] = "it exited before opening a window; an app open already (on the host's screen, or streamed) may have taken the window itself".into();
        }
        *self.ended.lock().unwrap() = v;
        false
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

    fn on_notes(&self, f: Notes) {
        *self.notes.lock().unwrap() = Some(f.clone());
        if let Some(w) = self.app.lock().unwrap().as_ref().and_then(App::wm) {
            w.set_notes(Some(f));
        }
    }

    fn features(&self) -> Vec<&'static str> {
        match self.app.lock().unwrap().as_ref().and_then(App::wm) {
            Some(_) => vec!["notes", "clipboard", "raise", "close"],
            None => Vec::new(),
        }
    }

    fn clip(&self, text: &str) {
        if let Some(w) = self.app.lock().unwrap().as_ref().and_then(App::wm) {
            w.send(wm::Cmd::Clip(text.to_string()));
        }
    }

    fn raise(&self, id: u32) {
        if let Some(w) = self.app.lock().unwrap().as_ref().and_then(App::wm) {
            w.send(wm::Cmd::Raise(id));
        }
    }

    fn close(&self) -> bool {
        match self.app.lock().unwrap().as_ref().and_then(App::wm) {
            Some(w) => {
                w.send(wm::Cmd::Close);
                true
            }
            None => false,
        }
    }

    fn exit_info(&self) -> serde_json::Value {
        self.ended.lock().unwrap().clone()
    }

    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: &mut dyn Sink) -> Result<(), String> {
        let (display, own, dpi) = {
            let a = self.app.lock().unwrap();
            let Some(app) = a.as_ref() else { return Err("no app is running".into()) };
            (app.display.clone(), app.xvfb.is_some(), app.dpi)
        };
        let mut grab = capture::Grab::new(&display)?;
        ctl.on_wake(grab.waker());
        // the app's own display follows the viewer's window: the app redraws at its size
        if own {
            let (conn, screen) = display.connect()?;
            let root = conn.setup().roots[screen].root;
            xserver::resize(&conn, root, v.w as u16, v.h as u16, dpi);
        }
        grab.geometry()?;
        self.screen_is(grab.w, grab.h);
        let (w, h) = (v.w as usize, v.h as usize);
        let mut enc = encode::H264::new(w, h, v.fps, v.kbps, threads_for(w, h))?;
        let mut yuv = convert::Yuv::new(w, h);
        let period = Duration::from_secs_f64(1.0 / v.fps as f64);
        let keepalive = Duration::from_secs(1);
        let t0 = Instant::now();
        let mut ch = capture::Changes::default();
        let full = |g: &capture::Grab| capture::Rect { x: 0, y: 0, w: g.w, h: g.h };
        let mut dirty = vec![full(&grab)];
        // the pointer: its shape sent as it changes, or drawn in where it is
        let mut img = grab.cursor_image();
        if !v.draw_cursor {
            if let Some(c) = &img {
                if !out.cursor(&capture::cursor_of(c)) {
                    return Ok(());
                }
            }
        }
        let mut at: Option<(i16, i16)> = None;
        let mut drawn: Option<capture::Rect> = None;
        let (mut sent, mut pending) = (false, true);
        let mut next_slot = Instant::now();
        let mut last = Instant::now();
        let mut stats = Stats::new();
        let mut scratch = Vec::new();
        while !ctl.stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            let mut until = if pending { next_slot } else { last + keepalive }.saturating_duration_since(now);
            if v.draw_cursor {
                // the pointer is looked for each slot: its moves draw nothing on the screen
                until = until.min(period);
            }
            ch.damage.clear();
            (ch.resized, ch.cursor, ch.woken) = (false, false, false);
            grab.wait(Some(until), &mut ch)?;
            if ch.resized {
                self.screen_is(grab.w, grab.h);
                yuv.black();
                dirty.clear();
                dirty.push(full(&grab));
            }
            dirty.append(&mut ch.damage);
            if ch.cursor {
                img = grab.cursor_image();
                if !v.draw_cursor {
                    if let Some(c) = &img {
                        if !out.cursor(&capture::cursor_of(c)) {
                            break;
                        }
                    }
                }
            }
            if v.draw_cursor {
                let now_at = grab.pointer();
                if now_at != at || ch.cursor {
                    at = now_at;
                    dirty.extend(drawn.take());
                    pending = true;
                }
            }
            pending |= !dirty.is_empty();
            let idr = ctl.idr.swap(false, Ordering::Relaxed) || !sent;
            let now = Instant::now();
            // a change in its slot, a keyframe asked, or once a second the last frame again
            if !((pending && now >= next_slot) || idr || now >= last + keepalive) {
                continue;
            }
            let t1 = Instant::now();
            let (gw, gh) = (grab.w, grab.h);
            let mut px_done = 0u64;
            if (gw as usize, gh as usize) == (w, h) {
                capture::coalesce(&mut dirty, gw, gh);
                grab.read(&dirty, |r, px, pitch| {
                    yuv.put(px, pitch, r.x as usize, r.y as usize, r.w as usize, r.h as usize);
                    px_done += r.w as u64 * r.h as u64;
                })?;
                if let (true, Some(c), Some(p)) = (v.draw_cursor, &img, at) {
                    if let Some(r) = capture::cursor_rect(c, p, gw, gh) {
                        grab.read(&[r], |r, px, pitch| {
                            scratch.clear();
                            scratch.extend_from_slice(px);
                            capture::draw_cursor(&mut scratch, pitch, r, c, p);
                            yuv.put(&scratch, pitch, r.x as usize, r.y as usize, r.w as usize, r.h as usize);
                        })?;
                        drawn = Some(r);
                    }
                }
            } else {
                // a display that isn't the video's size: all of it, scaled in
                let r = full(&grab);
                grab.read(&[r], |r, px, pitch| {
                    scratch.clear();
                    scratch.extend_from_slice(px);
                    if let (true, Some(c), Some(p)) = (v.draw_cursor, &img, at) {
                        capture::draw_cursor(&mut scratch, pitch, r, c, p);
                    }
                    convert::scaled(&scratch, pitch, r.w as usize, r.h as usize, &mut yuv);
                })?;
                px_done = gw as u64 * gh as u64;
            }
            dirty.clear();
            pending = false;
            last = now;
            next_slot = (next_slot + period).max(now);
            let t2 = Instant::now();
            let mut nals = Vec::new();
            let key = enc.encode(yuv.planes(), yuv.strides(), t0.elapsed().as_millis() as i64, idr, &mut nals)?;
            let t3 = Instant::now();
            if nals.is_empty() {
                continue;
            }
            sent = true;
            let bytes: usize = nals.iter().map(|n| n.len()).sum();
            if !out.frame(&nals, key) {
                break;
            }
            stats.frame(px_done, bytes, t2 - t1, t3 - t2, t3.elapsed());
        }
        stats.log();
        Ok(())
    }
}

/// encoder threads (one slice each) for a size: one for small pictures, where a
/// thread's hand-off costs more than it saves; two at 720p, three at 1080p and up (four
/// measured no faster than three, with the X server and the app wanting cores too)
fn threads_for(w: usize, h: usize) -> usize {
    let cores = thread::available_parallelism().map_or(1, |n| n.get());
    let want = if w * h >= 1920 * 1080 {
        3
    } else if w * h >= 1280 * 720 {
        2
    } else {
        1
    };
    want.min(cores.saturating_sub(1)).max(1)
}

/// where a video's time goes, logged every few seconds at debug
struct Stats {
    since: Instant,
    frames: u32,
    px: u64,
    bytes: u64,
    grab: Duration,
    enc: Duration,
    send: Duration,
    worst: Duration,
}

impl Stats {
    fn new() -> Stats {
        Stats {
            since: Instant::now(),
            frames: 0,
            px: 0,
            bytes: 0,
            grab: Duration::ZERO,
            enc: Duration::ZERO,
            send: Duration::ZERO,
            worst: Duration::ZERO,
        }
    }

    fn frame(&mut self, px: u64, bytes: usize, grab: Duration, enc: Duration, send: Duration) {
        self.frames += 1;
        self.px += px;
        self.bytes += bytes as u64;
        self.grab += grab;
        self.enc += enc;
        self.send += send;
        self.worst = self.worst.max(grab + enc + send);
        if self.since.elapsed() >= Duration::from_secs(5) {
            self.log();
        }
    }

    fn log(&mut self) {
        if self.frames > 0 {
            let n = self.frames as f64;
            let us = |d: Duration| d.as_secs_f64() * 1e6 / n;
            debug!(
                "video: {} frames, {:.0} px read a frame; a frame's read+convert {:.0} us, encode {:.0} us, send {:.0} us, worst {} us; {:.1} KiB a frame",
                self.frames,
                self.px as f64 / n,
                us(self.grab),
                us(self.enc),
                us(self.send),
                self.worst.as_micros(),
                self.bytes as f64 / n / 1024.0
            );
        }
        *self = Stats::new();
    }
}
