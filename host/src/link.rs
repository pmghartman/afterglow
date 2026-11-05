//! The link: one loopback TCP connection from a driver (docs/protocol.md). Each
//! encoded frame is reshaped once into the AVCC the viewer's decoder takes (wire.rs)
//! and written to the socket; input comes back on it as input packets (input.rs).
//!
//! One driver at a time: a new connection that says the token takes over and the old
//! one is closed. One app at a time, kept running between connections and videos so a
//! viewer that comes back (or a resize) resumes it.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::host::{Ctl, Host, Launch, VideoCfg};
use crate::input::Input;
use crate::wire::{self, input as msg_in, output as msg_out};
use crate::{error, info, warn};

pub const VERSION: &str = match option_env!("AFTERGLOW_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// one connection from a driver: writes from any thread, one at a time
struct Conn {
    sock: Mutex<TcpStream>,
    dead: AtomicBool,
}

impl Conn {
    fn send(&self, m: &[u8]) -> bool {
        if m.is_empty() {
            return true;
        }
        let mut s = self.sock.lock().unwrap();
        if self.dead.load(Ordering::Relaxed) {
            return false;
        }
        if s.write_all(m).is_err() {
            self.dead.store(true, Ordering::Relaxed);
            let _ = s.shutdown(Shutdown::Both);
        }
        !self.dead.load(Ordering::Relaxed)
    }

    fn json(&self, ty: u8, v: Value) -> bool {
        self.send(&wire::message(ty, v.to_string().as_bytes()))
    }

    fn close(&self) {
        let s = self.sock.lock().unwrap();
        self.dead.store(true, Ordering::Relaxed);
        let _ = s.shutdown(Shutdown::Both);
    }
}

/// one run of the video on its own thread
struct Run {
    ctl: Arc<Ctl>,
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
    token: String,
    input: Input,
    state: Mutex<State>,
    shutdown: Arc<AtomicBool>,
}

fn num(j: &Value, k: &str, d: i64, lo: i64, hi: i64) -> i64 {
    match j.get(k).and_then(Value::as_f64) {
        Some(v) => (v as i64).clamp(lo, hi),
        None => d,
    }
}

impl<H: Host> Link<H> {
    pub fn new(host: Arc<H>, token: String, shutdown: Arc<AtomicBool>) -> Arc<Link<H>> {
        let input = Input::new(host.clone());
        Arc::new(Link { host, token, input, state: Mutex::default(), shutdown })
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
        let watcher = thread::Builder::new().name("watch".into()).spawn(move || me.watch(addr)).expect("a thread");
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
            let Ok(w) = s.try_clone() else { continue };
            let c = Arc::new(Conn { sock: Mutex::new(w), dead: AtomicBool::new(false) });
            let me = self.clone();
            let _ = thread::Builder::new().name("conn".into()).spawn(move || me.serve(c, s));
        }
        let _ = watcher.join();
        let mut st = self.lock();
        self.stop_video(&mut st);
        if let Some(c) = st.current.take() {
            c.close();
        }
        if st.launched {
            self.host.terminate();
            st.launched = false;
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// the capture ending and the app quitting on their own, told to the driver
    fn watch(&self, addr: SocketAddr) {
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
            if let Some(c) = &st.current {
                c.json(msg_out::EXITED, json!({}));
            }
        }
        // the accept loop woken to see the shutdown
        let _ = TcpStream::connect_timeout(&addr, Duration::from_secs(1));
    }

    fn serve(&self, c: Arc<Conn>, mut s: TcpStream) {
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
                    if ty != msg_in::HELLO || !wire::same_token(&body, self.token.as_bytes()) {
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
            info!("the driver left");
        }
        c.close();
    }

    fn take_over(&self, c: &Arc<Conn>) {
        let mut st = self.lock();
        if let Some(old) = st.current.take() {
            self.stop_video(&mut st);
            old.close();
            info!("a new driver took over");
        }
        st.current = Some(c.clone());
        let running = st.launched && self.host.running();
        c.json(
            msg_out::READY,
            json!({
                "version": VERSION,
                "virtual_display": self.host.virtual_display(),
                "hostname": self.host.hostname(),
                "running": running,
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
                    r.ctl.idr.store(true, Ordering::Relaxed);
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
            msg_in::LAUNCH => self.launch(c, serde_json::from_slice(&body).unwrap_or(Value::Null)),
            msg_in::VIDEO => self.video(c, serde_json::from_slice(&body).unwrap_or(Value::Null)),
            _ => {}
        }
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
            c.json(msg_out::LAUNCHED, json!({"ok": true, "resumed": true}));
            return;
        }
        let l = Launch {
            cmd: j.get("cmd").and_then(Value::as_str).unwrap_or("").to_string(),
            name: j.get("name").and_then(Value::as_str).unwrap_or("phosphorus").to_string(),
            w: (num(&j, "w", 1280, 64, 7680) & !1) as u32,
            h: (num(&j, "h", 720, 64, 4320) & !1) as u32,
            fps: num(&j, "fps", 60, 10, 240) as u32,
            virtual_display: j.get("virtual_display").and_then(Value::as_bool).unwrap_or(true),
        };
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
                c.json(msg_out::LAUNCHED, json!({"ok": true, "resumed": false, "ms": ms}));
            }
            Err(e) => {
                warn!("launch of [{}] failed: {}", l.cmd, e.error);
                let mut r = json!({"ok": false, "error": e.error});
                if let Some(code) = e.code {
                    r["code"] = code.into();
                }
                c.json(msg_out::LAUNCHED, r);
            }
        }
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
        };
        let ctl = Arc::new(Ctl::default());
        let ended: Arc<Mutex<Option<String>>> = Arc::default();
        let (host, ctl2, ended2, conn) = (self.host.clone(), ctl.clone(), ended.clone(), c.clone());
        info!("video {vgen}: {}x{} at {} fps, {} kbps", v.w, v.h, v.fps, v.kbps);
        let thread = thread::Builder::new()
            .name("video".into())
            .spawn(move || {
                let mut shaper = wire::Shaper::default();
                let (w, h) = (v.w as u16, v.h as u16);
                let mut out = |b: &[u8], key: bool| {
                    let s = shaper.shape(b, key, vgen, w, h);
                    conn.send(&s.config) && conn.send(&s.frame)
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
        st.video = Some(Run { ctl, ended, vgen, thread: Some(thread) });
    }

    /// the running video stopped and joined; the app left running
    fn stop_video(&self, st: &mut State) {
        let Some(mut r) = st.video.take() else { return };
        r.ctl.stop.store(true, Ordering::Relaxed);
        if let Some(t) = r.thread.take() {
            let _ = t.join();
        }
        // no key left held down on the host
        self.input.reset();
    }
}
