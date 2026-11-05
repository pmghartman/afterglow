//! The link served on a real loopback socket against a stand-in platform: the token
//! first, launch, a video's config and frames shaped as the driver reads them, a
//! keyframe asked for, input handed on, ping, quit, the app exiting on its own, a new
//! driver taking over.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use afterglow::host::{Ctl, Host, Launch, LaunchError, Out, VideoCfg};
use afterglow::input::Inject;
use afterglow::link::Link;
use afterglow::wire::{self, input as i, output as o};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123";

#[derive(Default)]
struct Fake {
    running: AtomicBool,
    launches: Mutex<Vec<Launch>>,
    events: Mutex<Vec<String>>,
    idrs: Mutex<u32>,
}

impl Inject for Fake {
    fn mouse_abs(&self, x: f32, y: f32) {
        self.events.lock().unwrap().push(format!("abs {x:.2} {y:.2}"));
    }
    fn mouse_rel(&self, _: i32, _: i32) {}
    fn button(&self, b: u8, d: bool) {
        self.events.lock().unwrap().push(format!("button {b} {d}"));
    }
    fn scroll(&self, _: i32) {}
    fn hscroll(&self, _: i32) {}
    fn key(&self, vk: u8, d: bool) {
        self.events.lock().unwrap().push(format!("key {vk:#x} {d}"));
    }
    fn text(&self, _: &str) {}
}

const SPS: [u8; 8] = [0, 0, 0, 1, 0x67, 0x64, 0, 0x1f];
const PPS: [u8; 7] = [0, 0, 0, 1, 0x68, 0xee, 0x3c];

impl Host for Fake {
    fn hostname(&self) -> String {
        "box".into()
    }
    fn virtual_display(&self) -> bool {
        true
    }
    fn launch(&self, l: &Launch) -> Result<(), LaunchError> {
        if l.cmd == "bad" {
            return Err(LaunchError { code: Some(503), error: "no display or encoder to capture with".into() });
        }
        self.launches.lock().unwrap().push(l.clone());
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    fn terminate(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: Out) -> Result<(), String> {
        // a keyframe with its parameter sets, then P-frames until stopped
        let mut idr = true;
        let mut n = 0u8;
        while !ctl.stop.load(Ordering::SeqCst) {
            if ctl.idr.swap(false, Ordering::SeqCst) {
                *self.idrs.lock().unwrap() += 1;
                idr = true;
            }
            let au =
                if idr { [&SPS[..], &PPS, &[0, 0, 1, 0x65, n | 0x80]].concat() } else { vec![0, 0, 1, 0x41, n | 0x80] };
            if !out(&au, idr) {
                return Ok(());
            }
            idr = false;
            n = n.wrapping_add(1);
            if v.kbps == 999 {
                return Err("the display went away".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

struct Driver {
    s: TcpStream,
    r: wire::Reader,
}

impl Driver {
    fn open(port: u16) -> Driver {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Driver { s, r: wire::Reader::default() }
    }
    fn send(&mut self, ty: u8, body: &[u8]) {
        self.s.write_all(&wire::message(ty, body)).unwrap();
    }
    fn json(&mut self, ty: u8, v: Value) {
        self.send(ty, v.to_string().as_bytes());
    }
    /// the next message, or None when the link closed
    fn next(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            if let Some(m) = self.r.next_message() {
                return Some(m);
            }
            let mut b = [0u8; 65536];
            match self.s.read(&mut b) {
                Ok(0) | Err(_) => return None,
                Ok(n) => {
                    self.r.feed(&b[..n]);
                }
            }
        }
    }
    /// the next message of this type, others skipped
    fn wait(&mut self, ty: u8) -> Vec<u8> {
        loop {
            let (t, b) = self.next().expect("the link closed");
            if t == ty {
                return b;
            }
        }
    }
    fn wait_json(&mut self, ty: u8) -> Value {
        serde_json::from_slice(&self.wait(ty)).unwrap()
    }
}

fn serve() -> (Arc<Fake>, u16, Arc<AtomicBool>) {
    let fake = Arc::new(Fake::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let link = Link::new(fake.clone(), TOKEN.into(), shutdown.clone());
    thread::spawn(move || link.serve_on(l, addr));
    (fake, addr.port(), shutdown)
}

fn input_packet(magic: u32, body: &[u8]) -> Vec<u8> {
    let mut b = (4 + body.len() as u32).to_be_bytes().to_vec();
    b.extend_from_slice(&magic.to_le_bytes());
    b.extend_from_slice(body);
    b
}

#[test]
fn a_wrong_token_is_closed_unanswered() {
    let (_, port, _) = serve();
    let mut d = Driver::open(port);
    d.send(i::HELLO, b"0123456789abcdef0124");
    assert_eq!(d.next(), None);
    let mut d = Driver::open(port);
    d.send(i::PING, &[1, 2, 3, 4]);
    assert_eq!(d.next(), None);
}

#[test]
fn launch_video_keyframe_input_quit() {
    let (fake, port, shutdown) = serve();
    let mut d = Driver::open(port);
    d.send(i::HELLO, TOKEN.as_bytes());
    let ready = d.wait_json(o::READY);
    assert_eq!(ready["virtual_display"], true);
    assert_eq!(ready["hostname"], "box");
    assert_eq!(ready["running"], false);

    // a video before any app: ended at once
    d.json(i::VIDEO, json!({"gen": 3, "w": 640, "h": 480}));
    assert_eq!(d.wait_json(o::VIDEO_ENDED), json!({"gen": 3, "why": "no app is running"}));

    d.json(i::LAUNCH, json!({"cmd": "bad"}));
    assert_eq!(
        d.wait_json(o::LAUNCHED),
        json!({"ok": false, "code": 503, "error": "no display or encoder to capture with"})
    );
    d.json(i::LAUNCH, json!({"cmd": "C:\\App.lnk", "name": "App", "w": 1281, "h": 20, "fps": 1000, "fresh": true}));
    let l = d.wait_json(o::LAUNCHED);
    assert_eq!(l["ok"], true);
    assert_eq!(l["resumed"], false);
    {
        let ls = fake.launches.lock().unwrap();
        // sizes kept even and in range, the rate clamped, a virtual display by default
        assert_eq!((ls[0].w, ls[0].h, ls[0].fps, ls[0].virtual_display), (1280, 64, 240, true));
        assert_eq!(ls[0].name, "App");
    }
    // launched again, not fresh: resumed
    d.json(i::LAUNCH, json!({"cmd": "C:\\App.lnk"}));
    assert_eq!(d.wait_json(o::LAUNCHED)["resumed"], true);

    d.json(i::VIDEO, json!({"gen": 7, "w": 1280, "h": 720, "fps": 60, "kbps": 8000}));
    let cfg = d.wait(o::CONFIG);
    // [pad][u16 gen][u16 w][u16 h] avcC
    assert_eq!(&cfg[..7], &[0, 7, 0, 0x00, 0x05, 0xd0, 0x02]);
    assert_eq!(&cfg[7..11], &[1, 0x64, 0, 0x1f]);
    let f = d.wait(o::FRAME);
    // [key][u16 gen][pad 4] then the slice in AVCC
    assert_eq!(&f[..7], &[1, 7, 0, 0, 0, 0, 0]);
    assert_eq!(&f[7..12], &[0, 0, 0, 2, 0x65]);
    let p = d.wait(o::FRAME);
    assert_eq!(p[0], 0);

    // a keyframe asked: one comes, the parameter sets unchanged so no second config
    d.send(i::IDR, &[]);
    loop {
        let (t, b) = d.next().unwrap();
        assert_ne!(t, o::CONFIG);
        if t == o::FRAME && b[0] == 1 {
            break;
        }
    }
    assert_eq!(*fake.idrs.lock().unwrap(), 1);

    // input: a click at the middle, a key held, let go when the video stops
    d.send(i::INPUT, &input_packet(0x05, &[0x02, 0x80, 0x01, 0x68, 0, 0, 0x05, 0x00, 0x02, 0xd0]));
    d.send(i::INPUT, &input_packet(0x08, &[1]));
    d.send(i::INPUT, &input_packet(0x03, &[0, 0x41, 0x80, 0, 0, 0]));
    d.send(i::PING, &[9, 8, 7, 6]);
    assert_eq!(d.wait(o::PONG), [9, 8, 7, 6]);
    d.send(i::STOP_VIDEO, &[]);
    d.send(i::PING, &[1, 1, 1, 1]);
    d.wait(o::PONG);
    assert_eq!(
        *fake.events.lock().unwrap(),
        ["abs 0.50 0.50", "button 1 true", "key 0x41 true", "key 0x41 false", "button 1 false"]
    );

    d.send(i::QUIT, &[]);
    assert_eq!(d.wait_json(o::EXITED), json!({}));
    assert!(!fake.running());
    shutdown.store(true, Ordering::SeqCst);
}

#[test]
fn the_app_exiting_and_the_capture_ending_are_told() {
    let (fake, port, _) = serve();
    let mut d = Driver::open(port);
    d.send(i::HELLO, TOKEN.as_bytes());
    d.wait(o::READY);
    d.json(i::LAUNCH, json!({"cmd": "x"}));
    d.wait(o::LAUNCHED);
    // a capture that fails after one frame
    d.json(i::VIDEO, json!({"gen": 1, "kbps": 999}));
    d.wait(o::FRAME);
    assert_eq!(d.wait_json(o::VIDEO_ENDED), json!({"gen": 1, "why": "the display went away"}));
    fake.running.store(false, Ordering::SeqCst);
    assert_eq!(d.wait_json(o::EXITED), json!({}));
}

#[test]
fn a_new_driver_takes_over_and_finds_the_app_running() {
    let (_, port, _) = serve();
    let mut a = Driver::open(port);
    a.send(i::HELLO, TOKEN.as_bytes());
    a.wait(o::READY);
    a.json(i::LAUNCH, json!({"cmd": "x"}));
    a.wait(o::LAUNCHED);
    a.json(i::VIDEO, json!({"gen": 1}));
    a.wait(o::FRAME);
    let mut b = Driver::open(port);
    b.send(i::HELLO, TOKEN.as_bytes());
    assert_eq!(b.wait_json(o::READY)["running"], true);
    // the first one closed, its video stopped
    while a.next().is_some() {}
    b.json(i::VIDEO, json!({"gen": 2}));
    let f = b.wait(o::FRAME);
    assert_eq!(&f[1..3], &[2, 0]);
}
