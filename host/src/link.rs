//! The link: one loopback TCP connection from a driver (docs/protocol.md), or the
//! process's own stdin and stdout (`--stdio`, run over ssh as the viewer's login). Each
//! encoded frame is shaped once into the AVCC the viewer's decoder takes (wire.rs)
//! and written out; input comes back as input packets (input.rs).
//!
//! On TCP, one driver at a time: a new connection that says the token takes over and
//! the old one is closed. One app at a time, kept running between connections and
//! videos so a viewer that comes back (or a resize) resumes it. Over stdio there's one
//! driver, already let in by ssh: when it goes, the app and afterglow go too.

use std::io::{IoSlice, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::desk::End;
use crate::glow::{self, Glow};
use crate::host::{Ctl, Cursor, Host, Launch, Login, Sink, VideoCfg};
use crate::input::Input;
use crate::wire::{self, input as msg_in, output as msg_out};
use crate::{error, info, warn};

/// the link protocol's number (docs/protocol.md): bumped on any incompatible change
pub const PROTO: u32 = 1;

pub const VERSION: &str = match option_env!("AFTERGLOW_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// one connection from a driver: writes from any thread, one at a time
struct Conn {
    out: Mutex<Box<dyn Write + Send>>,
    dead: AtomicBool,
    /// what ends it for both sides: the socket shut down
    end: Box<dyn Fn() + Send + Sync>,
}

impl Conn {
    fn send(&self, m: &[u8]) -> bool {
        self.send2(m, &[])
    }

    /// two messages in one write: a config and the frame after it go out together
    fn send2(&self, a: &[u8], b: &[u8]) -> bool {
        if a.is_empty() && b.is_empty() {
            return true;
        }
        let mut s = self.out.lock().unwrap();
        if self.dead.load(Ordering::Relaxed) {
            return false;
        }
        if write_both(&mut **s, a, b).and_then(|_| s.flush()).is_err() {
            self.dead.store(true, Ordering::Relaxed);
            (self.end)();
        }
        !self.dead.load(Ordering::Relaxed)
    }

    fn json(&self, ty: u8, v: Value) -> bool {
        self.send(&wire::message(ty, v.to_string().as_bytes()))
    }

    fn close(&self) {
        let _s = self.out.lock().unwrap();
        self.dead.store(true, Ordering::Relaxed);
        (self.end)();
    }

    fn tcp(s: &TcpStream) -> Option<Conn> {
        let w = s.try_clone().ok()?;
        let e = s.try_clone().ok()?;
        Some(Conn {
            out: Mutex::new(Box::new(w)),
            dead: AtomicBool::new(false),
            end: Box::new(move || {
                let _ = e.shutdown(Shutdown::Both);
            }),
        })
    }
}

fn write_both(s: &mut dyn Write, mut a: &[u8], mut b: &[u8]) -> std::io::Result<()> {
    while !a.is_empty() || !b.is_empty() {
        let n = s.write_vectored(&[IoSlice::new(a), IoSlice::new(b)])?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        let k = n.min(a.len());
        a = &a[k..];
        b = &b[n - k..];
    }
    Ok(())
}

/// glow's server, once a driver has asked for it
type GlowSlot = Arc<Mutex<Option<Arc<Glow>>>>;

/// a video's frames and pointer shapes, shaped and written as they come: to the viewer
/// straight over glow while one is connected, otherwise over the link
struct Out {
    conn: Arc<Conn>,
    glow: GlowSlot,
    ctl: Arc<Ctl>,
    shaper: wire::Shaper,
    vgen: u16,
    w: u16,
    h: u16,
    /// where frames went last: 0 the link, else the glow viewer's id
    dest: u64,
    /// a new destination starts at a keyframe, its config before it
    need_key: bool,
    /// the pointer's shape last sent, for a new destination
    cursor: Option<Vec<u8>>,
    unit: Vec<u8>,
}

impl Out {
    fn viewer(&self) -> Option<Arc<glow::Viewer>> {
        self.glow.lock().unwrap().as_ref().and_then(|g| g.viewer())
    }
}

impl Sink for Out {
    fn frame(&mut self, nals: &[&[u8]], key: bool) -> bool {
        let v = self.viewer();
        let dest = v.as_ref().map_or(0, |v| v.id);
        if dest != self.dest {
            self.dest = dest;
            self.shaper = wire::Shaper::default();
            self.need_key = true;
            if let Some(c) = self.cursor.clone() {
                match &v {
                    Some(v) => v.control(c),
                    None => drop(self.conn.send(&c)),
                }
            }
        }
        if self.need_key && !key {
            self.ctl.ask_idr();
            return true;
        }
        self.need_key = false;
        let s = self.shaper.shape(nals, key, self.vgen, self.w, self.h);
        match v {
            Some(v) => {
                self.unit.clear();
                self.unit.extend_from_slice(s.config);
                self.unit.extend_from_slice(s.frame);
                // a viewer gone mid-frame: the next one goes over the link, from a keyframe
                if !self.unit.is_empty() {
                    v.send(&self.unit, key);
                }
                true
            }
            None => self.conn.send2(s.config, s.frame),
        }
    }

    fn cursor(&mut self, c: &Cursor) -> bool {
        let m = wire::cursor_message(c);
        self.cursor = Some(m.clone());
        match self.viewer() {
            Some(v) if v.id == self.dest => {
                v.control(m);
                true
            }
            _ => self.conn.send(&m),
        }
    }
}

/// one run of the video on its own thread
struct Run {
    ctl: Arc<Ctl>,
    /// the bitrate the driver asked for, kbps
    kbps: u32,
    /// why the capture stopped by itself, once it has
    ended: Arc<Mutex<Option<String>>>,
    vgen: u16,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct State {
    current: Option<Arc<Conn>>,
    video: Option<Run>,
    launched: bool,
}

pub struct Link<H: Host> {
    host: Arc<H>,
    /// what a driver says first; none over stdio, where ssh let it in
    token: Option<String>,
    input: Input,
    state: Mutex<State>,
    /// the driver the host's notes go to
    notes_to: Arc<Mutex<Option<Arc<Conn>>>>,
    shutdown: Arc<AtomicBool>,
    /// glow's server, started at the first driver's `direct`, where it binds
    glow: GlowSlot,
    direct_at: Mutex<SocketAddr>,
    me: Weak<Link<H>>,
}

fn num(j: &Value, k: &str, d: i64, lo: i64, hi: i64) -> i64 {
    match j.get(k).and_then(Value::as_f64) {
        Some(v) => (v as i64).clamp(lo, hi),
        None => d,
    }
}

impl<H: Host> Link<H> {
    pub fn new(host: Arc<H>, token: String, shutdown: Arc<AtomicBool>) -> Arc<Link<H>> {
        Self::make(host, Some(token), shutdown)
    }

    /// a link for one driver on stdin and stdout, let in by whatever started us
    pub fn stdio(host: Arc<H>, shutdown: Arc<AtomicBool>) -> Arc<Link<H>> {
        Self::make(host, None, shutdown)
    }

    fn make(host: Arc<H>, token: Option<String>, shutdown: Arc<AtomicBool>) -> Arc<Link<H>> {
        let input = Input::new(host.clone());
        let notes_to: Arc<Mutex<Option<Arc<Conn>>>> = Arc::default();
        let to = notes_to.clone();
        host.on_notes(Arc::new(move |v: Value| {
            if let Some(c) = to.lock().unwrap().clone() {
                c.json(msg_out::NOTE, v);
            }
        }));
        Arc::new_cyclic(|me| Link {
            host,
            token,
            input,
            state: Mutex::default(),
            notes_to,
            shutdown,
            glow: Arc::default(),
            direct_at: Mutex::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)),
            me: me.clone(),
        })
    }

    /// where glow listens once asked for: the address (all of them by default) and UDP
    /// port (0: the driver's `direct` names it, or any)
    pub fn direct_at(&self, at: SocketAddr) {
        *self.direct_at.lock().unwrap() = at;
    }

    /// glow's server, started now if it isn't yet
    fn glow(&self, port: u16) -> Result<Arc<Glow>, String> {
        let mut g = self.glow.lock().unwrap();
        if let Some(g) = g.as_ref() {
            return Ok(g.clone());
        }
        let mut at = *self.direct_at.lock().unwrap();
        if at.port() == 0 {
            at.set_port(port);
        }
        let me = self.me.clone();
        let started = Glow::start(
            at,
            Arc::new(move |e| {
                if let Some(l) = me.upgrade() {
                    l.glow_event(e);
                }
            }),
        )?;
        *g = Some(started.clone());
        Ok(started)
    }

    /// the glow viewer sent away: the driver it came through has gone
    fn drop_viewer(&self, why: &str) {
        if let Some(g) = self.glow.lock().unwrap().as_ref() {
            g.drop_viewer(why);
        }
    }

    /// what the glow viewer did
    fn glow_event(&self, e: glow::Event) {
        match e {
            glow::Event::Input(p) => {
                if self.lock().video.is_some() {
                    self.input.handle(&p);
                }
            }
            glow::Event::Idr => {
                if let Some(r) = &self.lock().video {
                    r.ctl.ask_idr();
                }
            }
            glow::Event::Clip(s) => {
                if self.lock().launched {
                    self.host.clip(&s);
                }
            }
            glow::Event::Scale(m) => {
                if let Some(r) = &self.lock().video {
                    let k = (r.kbps as u64 * m as u64 / 1000).max(1000) as u32;
                    r.ctl.kbps.store(if m >= 1000 { 0 } else { k }, Ordering::Relaxed);
                }
            }
            glow::Event::Came(from) => {
                // the running video starts again at a keyframe, over glow
                if let Some(r) = &self.lock().video {
                    r.ctl.ask_idr();
                }
                if let Some(c) = self.notes_to.lock().unwrap().clone() {
                    c.json(msg_out::NOTE, json!({"t": "direct", "on": true, "from": from}));
                }
            }
            glow::Event::Went(why) => {
                // nothing the viewer held stays held; the video goes on over the link
                self.input.reset();
                if let Some(r) = &self.lock().video {
                    r.ctl.ask_idr();
                }
                if let Some(c) = self.notes_to.lock().unwrap().clone() {
                    c.json(msg_out::NOTE, json!({"t": "direct", "on": false, "why": why}));
                }
            }
        }
    }

    /// a ticket for the driver's viewer to come straight here
    fn direct(&self, c: &Arc<Conn>, j: Value) {
        let port = num(&j, "port", 0, 0, 65535) as u16;
        let mut r = match self.glow(port).and_then(|g| g.ticket()) {
            Ok(t) => {
                info!("a glow ticket for udp port {}", t.port);
                json!({"ok": true, "port": t.port, "hash": t.hash, "ticket": t.ticket, "expires_ms": t.expires_ms})
            }
            Err(e) => {
                warn!("no glow: {e}");
                json!({"ok": false, "error": e})
            }
        };
        if let Some(id) = j.get("id") {
            r["id"] = id.clone();
        }
        c.json(msg_out::DIRECT, r);
    }

    /// serve the one driver on stdin and stdout until it goes (or a signal), then end
    /// the app
    pub fn serve_stdio(self: &Arc<Self>) {
        // SAFETY: fds 0 and 1 are the process's, read and written here alone; stdout
        // unbuffered as a File, so no frame is copied into a line buffer first
        #[cfg(unix)]
        let (input, output): (Box<dyn Read + Send>, Box<dyn Write + Send>) = unsafe {
            use std::os::fd::FromRawFd;
            (Box::new(std::fs::File::from_raw_fd(0)), Box::new(std::fs::File::from_raw_fd(1)))
        };
        // flushed after every message, so the line buffer holds nothing back
        #[cfg(not(unix))]
        let (input, output): (Box<dyn Read + Send>, Box<dyn Write + Send>) =
            (Box::new(std::io::stdin()), Box::new(std::io::stdout()));
        let c = Arc::new(Conn { out: Mutex::new(output), dead: AtomicBool::new(false), end: Box::new(|| {}) });
        let me = self.clone();
        let _ = thread::Builder::new().name("watch".into()).spawn(move || {
            me.watch(None);
            // a signal: everything put away and gone, whatever stdin is doing
            me.finish();
            std::process::exit(0);
        });
        self.serve(c, input);
        self.finish();
    }

    /// the video stopped, the driver let go, the app ended
    fn finish(&self) {
        self.drop_viewer("afterglow is going");
        let mut st = self.lock();
        self.stop_video(&mut st);
        if let Some(c) = st.current.take() {
            c.close();
        }
        *self.notes_to.lock().unwrap() = None;
        // ended whether it was still running or not: an app that exited leaves its display
        self.host.terminate();
        st.launched = false;
        self.host.release();
    }

    /// serve on 127.0.0.1:port until shutdown is raised
    pub fn run(self: &Arc<Self>, port: u16) -> Result<(), String> {
        let listener =
            TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("can't listen on 127.0.0.1:{port}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        info!("link on {addr}");
        self.serve_on(listener, addr);
        Ok(())
    }

    pub fn serve_on(self: &Arc<Self>, listener: TcpListener, addr: SocketAddr) {
        let me = self.clone();
        let watcher =
            thread::Builder::new().name("watch".into()).spawn(move || me.watch(Some(addr))).expect("a thread");
        for s in listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            let Ok(s) = s else {
                thread::sleep(Duration::from_millis(100));
                continue;
            };
            let _ = s.set_nodelay(true);
            // a stalled driver fails the write rather than holding the video forever
            let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
            let Some(c) = Conn::tcp(&s) else { continue };
            let c = Arc::new(c);
            let me = self.clone();
            let _ = thread::Builder::new().name("conn".into()).spawn(move || me.serve(c, s));
        }
        let _ = watcher.join();
        self.finish();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// the capture ending and the app quitting on their own, told to the driver
    fn watch(&self, wake: Option<SocketAddr>) {
        while !self.shutdown.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
            let mut st = self.lock();
            if let Some(r) = &st.video {
                let why = r.ended.lock().unwrap().take();
                if let (Some(why), Some(c)) = (why, &st.current) {
                    warn!("video {} ended: {why}", r.vgen);
                    c.json(msg_out::VIDEO_ENDED, json!({"gen": r.vgen, "why": why}));
                }
            }
            if !st.launched || self.host.running() {
                continue;
            }
            info!("the app exited");
            st.launched = false;
            self.stop_video(&mut st);
            // what's left of it (its display) put away now, how it ended kept to tell
            self.host.terminate();
            if let Some(c) = &st.current {
                c.json(msg_out::EXITED, self.host.exit_info());
            }
        }
        // the accept loop woken to see the shutdown
        if let Some(addr) = wake {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_secs(1));
        }
    }

    fn serve(&self, c: Arc<Conn>, mut s: impl Read) {
        let mut reader = wire::Reader::default();
        let mut hello = false;
        let mut buf = vec![0u8; 64 << 10];
        'read: loop {
            let n = match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !reader.feed(&buf[..n]) {
                break;
            }
            while let Some((ty, body)) = reader.next_message() {
                if !hello {
                    let ok = ty == msg_in::HELLO
                        && self.token.as_ref().is_none_or(|t| wire::same_token(&body, t.as_bytes()));
                    if !ok {
                        warn!("a connection without the token, closed");
                        break 'read;
                    }
                    hello = true;
                    self.take_over(&c);
                    continue;
                }
                self.handle(&c, ty, body);
            }
            if reader.broken() {
                warn!("a message past the limit, the link closed");
                break;
            }
        }
        let mut st = self.lock();
        if st.current.as_ref().is_some_and(|x| Arc::ptr_eq(x, &c)) {
            // nobody to send to: the video stopped, the app kept for whoever comes next
            self.stop_video(&mut st);
            st.current = None;
            *self.notes_to.lock().unwrap() = None;
            info!("the driver left");
            drop(st);
            self.drop_viewer("the driver left");
        }
        c.close();
    }

    fn take_over(&self, c: &Arc<Conn>) {
        let mut st = self.lock();
        if let Some(old) = st.current.take() {
            self.stop_video(&mut st);
            old.close();
            info!("a new driver took over");
            self.drop_viewer("a new driver took over");
        }
        st.current = Some(c.clone());
        *self.notes_to.lock().unwrap() = Some(c.clone());
        let running = st.launched && self.host.running();
        c.json(
            msg_out::READY,
            json!({
                "version": VERSION,
                "proto": PROTO,
                "virtual_display": self.host.virtual_display(),
                "hostname": self.host.hostname(),
                "running": running,
                "features": self.host.desk_features(),
                "direct": true,
            }),
        );
    }

    fn handle(&self, c: &Arc<Conn>, ty: u8, body: Vec<u8>) {
        match ty {
            msg_in::INPUT => {
                if self.lock().video.is_some() {
                    self.input.handle(&body);
                }
            }
            msg_in::IDR => {
                if let Some(r) = &self.lock().video {
                    r.ctl.ask_idr();
                }
            }
            msg_in::PING => {
                c.send(&wire::message(msg_out::PONG, &body));
            }
            msg_in::STOP_VIDEO => {
                let mut st = self.lock();
                self.stop_video(&mut st);
            }
            msg_in::QUIT => {
                let mut st = self.lock();
                self.stop_video(&mut st);
                if st.launched {
                    self.host.terminate();
                    st.launched = false;
                }
                c.json(msg_out::EXITED, json!({}));
            }
            msg_in::CLOSE => {
                // as the app's own close button: it may ask to save. Where that can't be
                // asked, ended as quit ends it
                if self.lock().launched && !self.host.close() {
                    self.handle(c, msg_in::QUIT, Vec::new());
                }
            }
            msg_in::CLIP => {
                if self.lock().launched {
                    self.host.clip(&String::from_utf8_lossy(&body));
                }
            }
            msg_in::RAISE => {
                if let (true, Ok(b)) = (self.lock().launched, <[u8; 4]>::try_from(&body[..])) {
                    self.host.raise(u32::from_le_bytes(b));
                }
            }
            msg_in::LAUNCH => self.launch(c, serde_json::from_slice(&body).unwrap_or(Value::Null)),
            msg_in::ATTACH => self.attach(c, serde_json::from_slice(&body).unwrap_or(Value::Null)),
            msg_in::PROCESSES | msg_in::END | msg_in::DELETE => {
                self.desk(c, ty, serde_json::from_slice(&body).unwrap_or(Value::Null))
            }
            msg_in::VIDEO => self.video(c, serde_json::from_slice(&body).unwrap_or(Value::Null)),
            msg_in::DIRECT => self.direct(c, serde_json::from_slice(&body).unwrap_or(Value::Null)),
            _ => {}
        }
    }

    /// what the launched app can be asked, then what the host's desktop can be
    fn features(&self) -> Vec<&'static str> {
        let mut f = self.host.features();
        f.extend(self.host.desk_features());
        f
    }

    /// the app started, or found running already
    fn launch(&self, c: &Arc<Conn>, j: Value) {
        if !j.is_object() {
            c.json(msg_out::LAUNCHED, json!({"ok": false, "error": "a launch that isn't json"}));
            return;
        }
        let mut st = self.lock();
        let fresh = j.get("fresh").and_then(Value::as_bool).unwrap_or(false);
        if st.launched && self.host.running() && !fresh {
            c.json(msg_out::LAUNCHED, json!({"ok": true, "resumed": true, "features": self.features()}));
            return;
        }
        let l = launch_of(&j);
        self.stop_video(&mut st);
        if st.launched {
            self.host.terminate();
            st.launched = false;
        }
        let t0 = Instant::now();
        match self.host.launch(&l) {
            Ok(()) => {
                st.launched = true;
                let ms = t0.elapsed().as_millis() as u64;
                info!("launched [{}] in {ms}ms", l.name);
                c.json(msg_out::LAUNCHED, json!({"ok": true, "resumed": false, "ms": ms, "features": self.features()}));
            }
            Err(e) => {
                warn!("launch of [{}] failed: {}", l.argv.first().unwrap_or(&l.cmd), e.error);
                let mut r = json!({"ok": false, "error": e.error});
                if let Some(code) = e.code {
                    r["code"] = code.into();
                }
                c.json(msg_out::LAUNCHED, r);
            }
        }
    }

    /// a process already running taken as the app: its windows followed as a launched
    /// app's are, nothing started. Answered as a launch is
    fn attach(&self, c: &Arc<Conn>, j: Value) {
        let Some(pid) = j.get("pid").and_then(Value::as_u64).filter(|&p| p > 0 && p <= u32::MAX as u64) else {
            c.json(msg_out::LAUNCHED, json!({"ok": false, "attached": true, "error": "an attach without a pid"}));
            return;
        };
        let pid = pid as u32;
        let started = j.get("started").and_then(Value::as_u64);
        let shell = j.get("shell").and_then(Value::as_bool).unwrap_or(false);
        let l = launch_of(&j);
        let mut st = self.lock();
        self.stop_video(&mut st);
        if st.launched {
            self.host.terminate();
            st.launched = false;
        }
        let t0 = Instant::now();
        match self.host.attach(pid, started, shell, &l) {
            Ok(()) => {
                st.launched = true;
                let ms = t0.elapsed().as_millis() as u64;
                info!("attached to pid {pid} [{}] in {ms}ms", l.name);
                c.json(
                    msg_out::LAUNCHED,
                    json!({"ok": true, "resumed": false, "attached": true, "ms": ms, "features": self.features()}),
                );
            }
            Err(e) => {
                warn!("attach to pid {pid} refused: {}", e.error);
                let mut r = json!({"ok": false, "attached": true, "error": e.error});
                if let Some(code) = e.code {
                    r["code"] = code.into();
                }
                c.json(msg_out::LAUNCHED, r);
            }
        }
    }

    /// the desktop's processes listed, one ended, a file deleted: each on a thread of its
    /// own, so input and video go on while it waits, answered to the driver that asked
    /// with the request's `id` echoed
    fn desk(&self, c: &Arc<Conn>, ty: u8, j: Value) {
        let host = self.host.clone();
        let c = c.clone();
        let _ = thread::Builder::new().name("desk".into()).spawn(move || {
            let id = j.get("id").cloned();
            let (out, mut r) = match ty {
                msg_in::PROCESSES => {
                    let all = j.get("all").and_then(Value::as_bool).unwrap_or(false);
                    let r = match host.processes(all) {
                        Ok(l) => {
                            info!("processes listed{}: {}", if all { ", all" } else { "" }, l.len());
                            json!({"ok": true, "list": l.iter().map(|p| p.json()).collect::<Vec<_>>()})
                        }
                        Err(e) => {
                            warn!("processes not listed: {e}");
                            json!({"ok": false, "error": e})
                        }
                    };
                    (msg_out::PROCESSES, r)
                }
                msg_in::END => {
                    let r = match End::of(&j) {
                        None => json!({"ok": false, "error": "an end without a pid"}),
                        Some(e) => {
                            let what = if e.kill { "kill" } else { "close" };
                            info!("asked to {what} pid {}{}", e.pid, if e.shell { ", the shell allowed" } else { "" });
                            match host.end(&e) {
                                Ok(d) => {
                                    info!(
                                        "pid {} {}{}",
                                        e.pid,
                                        d.how,
                                        if d.gone { ", gone" } else { ", still running" }
                                    );
                                    json!({"ok": true, "pid": e.pid, "how": d.how, "gone": d.gone})
                                }
                                Err(err) => {
                                    warn!("pid {} not ended: {err}", e.pid);
                                    json!({"ok": false, "pid": e.pid, "error": err})
                                }
                            }
                        }
                    };
                    (msg_out::ENDED, r)
                }
                _ => {
                    let path = j.get("path").and_then(Value::as_str).unwrap_or("").to_string();
                    let permanent = j.get("permanent").and_then(Value::as_bool).unwrap_or(false);
                    let r = if path.is_empty() {
                        json!({"ok": false, "path": path, "error": "a delete without a path"})
                    } else {
                        info!("asked to delete [{path}]{}", if permanent { " for good" } else { "" });
                        match host.delete(&path, permanent) {
                            Ok(recycled) => {
                                info!("[{path}] {}", if recycled { "recycled" } else { "deleted" });
                                json!({"ok": true, "path": path, "recycled": recycled})
                            }
                            Err(e) => {
                                warn!("[{path}] not deleted: {e}");
                                json!({"ok": false, "path": path, "error": e})
                            }
                        }
                    };
                    (msg_out::DELETED, r)
                }
            };
            if let Some(id) = id {
                r["id"] = id;
            }
            c.json(out, r);
        });
    }

    /// a fresh video at this size and rate, its first frame a keyframe
    fn video(&self, c: &Arc<Conn>, j: Value) {
        if !j.is_object() {
            return;
        }
        let mut st = self.lock();
        self.stop_video(&mut st);
        let vgen = num(&j, "gen", 0, 0, 65535) as u16;
        if !st.launched {
            c.json(msg_out::VIDEO_ENDED, json!({"gen": vgen, "why": "no app is running"}));
            return;
        }
        let v = VideoCfg {
            w: (num(&j, "w", 1280, 64, 7680) & !1) as u32,
            h: (num(&j, "h", 720, 64, 4320) & !1) as u32,
            fps: num(&j, "fps", 60, 10, 240) as u32,
            kbps: num(&j, "kbps", 20000, 500, 500_000) as u32,
            yuv444: j.get("yuv444").and_then(Value::as_bool).unwrap_or(false),
            draw_cursor: j.get("draw_cursor").and_then(Value::as_bool).unwrap_or(false),
        };
        let ctl = Arc::new(Ctl::default());
        // a glow viewer whose path takes less than this keeps its bitrate
        if let Some(m) = self.glow.lock().unwrap().as_ref().and_then(|g| g.viewer()).map(|v| v.scale()) {
            if m < 1000 {
                ctl.kbps.store((v.kbps as u64 * m as u64 / 1000).max(1000) as u32, Ordering::Relaxed);
            }
        }
        let ended: Arc<Mutex<Option<String>>> = Arc::default();
        let (host, ctl2, ended2, conn) = (self.host.clone(), ctl.clone(), ended.clone(), c.clone());
        let glow = self.glow.clone();
        info!(
            "video {vgen}: {}x{} at {} fps, {} kbps{}",
            v.w,
            v.h,
            v.fps,
            v.kbps,
            if v.draw_cursor { ", the pointer drawn in" } else { "" }
        );
        let thread = thread::Builder::new()
            .name("video".into())
            .spawn(move || {
                let mut out = Out {
                    conn: conn.clone(),
                    glow,
                    ctl: ctl2.clone(),
                    shaper: wire::Shaper::default(),
                    vgen,
                    w: v.w as u16,
                    h: v.h as u16,
                    dest: 0,
                    need_key: false,
                    cursor: None,
                    unit: Vec::new(),
                };
                let r = host.video(&v, &ctl2, &mut out);
                if ctl2.stop.load(Ordering::Relaxed) || conn.dead.load(Ordering::Relaxed) {
                    return;
                }
                let why = match r {
                    Ok(()) => "the capture ended".to_string(),
                    Err(e) => {
                        error!("video {vgen}: {e}");
                        e
                    }
                };
                *ended2.lock().unwrap() = Some(why);
            })
            .expect("a thread");
        st.video = Some(Run { ctl, kbps: v.kbps, ended, vgen, thread: Some(thread) });
    }

    /// the running video stopped and joined; the app left running
    fn stop_video(&self, st: &mut State) {
        let Some(mut r) = st.video.take() else { return };
        r.ctl.ask_stop();
        if let Some(t) = r.thread.take() {
            let _ = t.join();
        }
        // no key left held down on the host
        self.input.reset();
    }
}

/// a launch's (or an attach's) fields, each clamped to what a host can take
fn launch_of(j: &Value) -> Launch {
    Launch {
        cmd: j.get("cmd").and_then(Value::as_str).unwrap_or("").to_string(),
        name: j.get("name").and_then(Value::as_str).unwrap_or("phosphorus").to_string(),
        w: (num(j, "w", 1280, 64, 7680) & !1) as u32,
        h: (num(j, "h", 720, 64, 4320) & !1) as u32,
        fps: num(j, "fps", 60, 10, 240) as u32,
        virtual_display: j.get("virtual_display").and_then(Value::as_bool).unwrap_or(true),
        argv: j
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        dpi: num(j, "dpi", 96, 48, 480) as u32,
        scale: j.get("scale").and_then(Value::as_f64).unwrap_or(1.0).clamp(1.0, 4.0) as f32,
        dbus: j.get("dbus").and_then(Value::as_bool).unwrap_or(false),
        login: j.get("login").and_then(|l| {
            let pass = l.get("password").and_then(Value::as_str).filter(|p| !p.is_empty())?;
            let user = l.get("user").and_then(Value::as_str).unwrap_or("");
            Some(Login { user: user.to_string(), password: pass.to_string() })
        }),
    }
}
