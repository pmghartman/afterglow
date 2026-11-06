//! The link served on a real loopback socket against a stand-in platform: the token
//! first, launch, a video's config and frames shaped as the driver reads them, a
//! keyframe asked for, input handed on, ping, quit, the app exiting on its own, a new
//! driver taking over, a new bitrate taken by the running video; the desktop's processes listed, one attached to, ended, a file
//! deleted, none of it before the token.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use afterglow::desk::{End, Ended, Proc};
use afterglow::host::{Ctl, Cursor, Host, Launch, LaunchError, Retune, Sink, VideoCfg};
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
    videos: Mutex<Vec<VideoCfg>>,
    /// retunes the running video took
    retuned: Mutex<Vec<Retune>>,
    /// what the desktop was asked: "processes all", "attach 7", "end 7 kill", "delete /x"
    desk: Mutex<Vec<String>>,
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
    fn desk_features(&self) -> Vec<&'static str> {
        vec!["processes", "attach", "delete"]
    }
    fn processes(&self, all: bool) -> Result<Vec<Proc>, String> {
        self.desk.lock().unwrap().push(format!("processes{}", if all { " all" } else { "" }));
        let p = Proc {
            pid: 7,
            ppid: 1,
            exe: "C:\\x\\notepad.exe".into(),
            name: "notepad.exe".into(),
            started: 1_700_000_000_000,
            windows: vec![(0x1234, "notes - Notepad".into())],
        };
        let mut l = vec![p];
        if all {
            l.push(Proc { pid: 8, name: "bg.exe".into(), ..Proc::default() });
        }
        Ok(l)
    }
    fn attach(&self, pid: u32, started: Option<u64>, shell: bool, l: &Launch) -> Result<(), LaunchError> {
        self.desk.lock().unwrap().push(format!("attach {pid} {started:?} {shell} {}x{}", l.w, l.h));
        if pid != 7 {
            return Err(LaunchError::new("there's no process 9"));
        }
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn end(&self, e: &End) -> Result<Ended, String> {
        self.desk.lock().unwrap().push(format!("end {}{}", e.pid, if e.kill { " kill" } else { "" }));
        if e.pid == 4 {
            return Err("System keeps the session running: it's never ended here".into());
        }
        // a slow close: the link serves on meanwhile
        thread::sleep(Duration::from_millis(300));
        Ok(Ended { how: if e.kill { "killed" } else { "closed" }, gone: true })
    }
    fn delete(&self, path: &str, permanent: bool) -> Result<bool, String> {
        self.desk.lock().unwrap().push(format!("delete {path}{}", if permanent { " for good" } else { "" }));
        if path.starts_with("C:\\Windows") {
            return Err(format!("{path} is outside the user's folders"));
        }
        Ok(!permanent)
    }
    fn retunes(&self, v: &VideoCfg) -> bool {
        !v.draw_cursor
    }
    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: &mut dyn Sink) -> Result<(), String> {
        self.videos.lock().unwrap().push(*v);
        // the pointer's shape first, unless it's drawn in
        if !v.draw_cursor {
            let c = Cursor { visible: true, w: 1, h: 1, hot_x: 0, hot_y: 0, rgba: vec![0, 0, 0, 255] };
            if !out.cursor(&c) {
                return Ok(());
            }
        }
        // a keyframe with its parameter sets, then P-frames until stopped
        let mut idr = true;
        let mut n = 0u8;
        while !ctl.stop.load(Ordering::SeqCst) {
            if let Some(r) = ctl.take_retune() {
                self.retuned.lock().unwrap().push(r);
                out.regen(r.vgen);
                idr = true;
            }
            if ctl.idr.swap(false, Ordering::SeqCst) {
                *self.idrs.lock().unwrap() += 1;
                idr = true;
            }
            let au =
                if idr { [&SPS[..], &PPS, &[0, 0, 1, 0x65, n | 0x80]].concat() } else { vec![0, 0, 1, 0x41, n | 0x80] };
            if !out.frame(&afterglow::h264::split(&au), idr) {
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
fn the_desktop_untouched_before_the_token() {
    let (fake, port, _) = serve();
    for (ty, body) in [
        (i::PROCESSES, json!({"all": true})),
        (i::END, json!({"pid": 7, "kill": true})),
        (i::DELETE, json!({"path": "C:\\Users\\me\\Desktop\\x.txt"})),
        (i::ATTACH, json!({"pid": 7})),
    ] {
        let mut d = Driver::open(port);
        d.json(ty, body.clone());
        assert_eq!(d.next(), None);
        // a wrong token first, then the ask: closed at the token
        let mut d = Driver::open(port);
        d.send(i::HELLO, b"0123456789abcdef0124");
        d.json(ty, body);
        assert_eq!(d.next(), None);
    }
    thread::sleep(Duration::from_millis(100));
    assert!(fake.desk.lock().unwrap().is_empty());
}

#[test]
fn processes_listed_one_attached_one_ended_a_file_deleted() {
    let (fake, port, _) = serve();
    let mut d = Driver::open(port);
    d.send(i::HELLO, TOKEN.as_bytes());
    // what the host's desktop can do, said before any app
    assert_eq!(d.wait_json(o::READY)["features"], json!(["processes", "attach", "delete"]));

    d.json(i::PROCESSES, json!({"id": 1}));
    let l = d.wait_json(o::PROCESSES);
    assert_eq!(l["ok"], true);
    assert_eq!(l["id"], 1);
    assert_eq!(
        l["list"],
        json!([{"pid": 7, "ppid": 1, "exe": "C:\\x\\notepad.exe", "name": "notepad.exe", "started": 1_700_000_000_000u64,
            "windows": [[0x1234, "notes - Notepad"]]}])
    );
    d.json(i::PROCESSES, json!({"all": true, "id": "b"}));
    let l = d.wait_json(o::PROCESSES);
    assert_eq!((l["id"].as_str(), l["list"].as_array().unwrap().len()), (Some("b"), 2));

    // ended: a slow close answered later, a ping meanwhile not held up behind it
    d.json(i::END, json!({"pid": 7, "id": 3}));
    d.send(i::PING, &[5, 5, 5, 5]);
    assert_eq!(d.next().unwrap(), (o::PONG, vec![5, 5, 5, 5]));
    assert_eq!(d.wait_json(o::ENDED), json!({"ok": true, "id": 3, "pid": 7, "how": "closed", "gone": true}));
    d.json(i::END, json!({"pid": 7, "kill": true}));
    assert_eq!(d.wait_json(o::ENDED)["how"], "killed");
    d.json(i::END, json!({"pid": 4, "id": 4}));
    let e = d.wait_json(o::ENDED);
    assert_eq!((e["ok"].as_bool(), e["pid"].as_u64(), e["id"].as_u64()), (Some(false), Some(4), Some(4)));
    assert!(e["error"].as_str().unwrap().contains("never ended"));
    d.json(i::END, json!({"kill": true}));
    assert_eq!(d.wait_json(o::ENDED), json!({"ok": false, "error": "an end without a pid"}));

    // deleted: to the bin unless asked otherwise
    d.json(i::DELETE, json!({"path": "C:\\Users\\me\\Desktop\\x.txt", "id": 5}));
    assert_eq!(
        d.wait_json(o::DELETED),
        json!({"ok": true, "id": 5, "path": "C:\\Users\\me\\Desktop\\x.txt", "recycled": true})
    );
    d.json(i::DELETE, json!({"path": "C:\\Users\\me\\y", "permanent": true}));
    assert_eq!(d.wait_json(o::DELETED)["recycled"], false);
    d.json(i::DELETE, json!({"path": "C:\\Windows\\win.ini"}));
    let r = d.wait_json(o::DELETED);
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("outside"));
    d.json(i::DELETE, json!({}));
    assert_eq!(d.wait_json(o::DELETED)["error"], "a delete without a path");

    // attached: answered as a launch, the video then runs as for a launched app
    d.json(i::ATTACH, json!({"pid": 9}));
    let a = d.wait_json(o::LAUNCHED);
    assert_eq!((a["ok"].as_bool(), a["attached"].as_bool()), (Some(false), Some(true)));
    assert_eq!(a["error"], "there's no process 9");
    d.json(i::ATTACH, json!({"pid": 7, "started": 1_700_000_000_000u64, "w": 1024, "h": 768}));
    let a = d.wait_json(o::LAUNCHED);
    assert_eq!((a["ok"].as_bool(), a["attached"].as_bool()), (Some(true), Some(true)));
    assert_eq!(a["features"], json!(["processes", "attach", "delete"]));
    d.json(i::VIDEO, json!({"gen": 2}));
    assert_eq!(&d.wait(o::FRAME)[1..3], &[2, 0]);
    d.send(i::QUIT, &[]);
    d.wait(o::EXITED);
    assert!(!fake.running());
    assert_eq!(
        *fake.desk.lock().unwrap(),
        [
            "processes",
            "processes all",
            "end 7",
            "end 7 kill",
            "end 4",
            "delete C:\\Users\\me\\Desktop\\x.txt",
            "delete C:\\Users\\me\\y for good",
            "delete C:\\Windows\\win.ini",
            "attach 9 None false 1280x720",
            "attach 7 Some(1700000000000) false 1024x768",
        ]
    );
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
    // the pointer's shape, a PNG behind its hotspot and size
    let c = d.wait(o::CURSOR);
    assert_eq!(&c[..9], &[1, 0, 0, 0, 0, 1, 0, 1, 0]);
    assert_eq!(&c[9..13], b"\x89PNG");
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

    // the pointer drawn in, asked: no shape sent
    d.json(i::VIDEO, json!({"gen": 8, "draw_cursor": true}));
    loop {
        let (t, b) = d.next().unwrap();
        assert_ne!(t, o::CURSOR);
        if t == o::FRAME && b[1..3] == [8, 0] {
            break;
        }
    }
    let vs = fake.videos.lock().unwrap().clone();
    assert_eq!((vs[0].draw_cursor, vs[1].draw_cursor), (false, true));

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

#[test]
fn a_new_bitrate_retunes_the_running_video() {
    let (fake, port, shutdown) = serve();
    let mut d = Driver::open(port);
    d.send(i::HELLO, TOKEN.as_bytes());
    d.wait(o::READY);
    d.json(i::LAUNCH, json!({"cmd": "C:\\App.lnk", "fresh": true}));
    assert_eq!(d.wait_json(o::LAUNCHED)["ok"], true);
    d.json(i::VIDEO, json!({"gen": 1, "w": 1280, "h": 720, "fps": 60, "kbps": 8000}));
    assert_eq!(&d.wait(o::CONFIG)[1..3], &[1, 0]);
    d.wait(o::FRAME);

    // only the bitrate differs: the same capture, its config and a keyframe under gen 2
    d.json(i::VIDEO, json!({"gen": 2, "w": 1280, "h": 720, "fps": 60, "kbps": 4000}));
    let cfg = d.wait(o::CONFIG);
    assert_eq!(&cfg[1..7], &[2, 0, 0x00, 0x05, 0xd0, 0x02]);
    let f = d.wait(o::FRAME);
    assert_eq!(&f[..3], &[1, 2, 0]);
    // and every frame after it is gen 2's
    let p = d.wait(o::FRAME);
    assert_eq!(&p[..3], &[0, 2, 0]);
    assert_eq!(fake.videos.lock().unwrap().len(), 1);
    assert_eq!(*fake.retuned.lock().unwrap(), [Retune { vgen: 2, kbps: 4000 }]);

    // as AfterglowSource changes the bitrate: the video stopped, then the next asked.
    // Stopped, nothing more of it comes; the next is the same capture, retuned
    d.send(i::STOP_VIDEO, &[]);
    d.send(i::PING, &[2, 2, 2, 2]);
    loop {
        let (t, _) = d.next().unwrap();
        if t == o::PONG {
            break;
        }
        assert_ne!(t, o::CONFIG);
    }
    thread::sleep(Duration::from_millis(200));
    d.send(i::PING, &[3, 3, 3, 3]);
    let (t, _) = d.next().unwrap();
    assert_eq!(t, o::PONG, "nothing of a stopped video goes out");
    d.json(i::VIDEO, json!({"gen": 9, "w": 1280, "h": 720, "fps": 60, "kbps": 6000}));
    // its pointer shape again, its config and keyframe under gen 9
    assert_eq!(d.wait(o::CURSOR)[0], 1);
    assert_eq!(&d.wait(o::CONFIG)[1..3], &[9, 0]);
    assert_eq!(&d.wait(o::FRAME)[..3], &[1, 9, 0]);
    assert_eq!(fake.videos.lock().unwrap().len(), 1);
    assert_eq!(fake.retuned.lock().unwrap().last(), Some(&Retune { vgen: 9, kbps: 6000 }));

    // stopped and nothing after: the capture let go once the moment's past
    d.send(i::STOP_VIDEO, &[]);
    thread::sleep(Duration::from_millis(2000));
    d.json(i::VIDEO, json!({"gen": 10, "w": 1280, "h": 720, "fps": 60, "kbps": 6000}));
    assert_eq!(&d.wait(o::CONFIG)[1..3], &[10, 0]);
    assert_eq!(fake.videos.lock().unwrap().len(), 2);
    assert_eq!(fake.retuned.lock().unwrap().len(), 2);

    // another size: a video of its own
    d.json(i::VIDEO, json!({"gen": 3, "w": 800, "h": 600, "fps": 60, "kbps": 4000}));
    assert_eq!(&d.wait(o::CONFIG)[1..3], &[3, 0]);
    let vs = fake.videos.lock().unwrap().clone();
    assert_eq!(vs.len(), 3);
    assert_eq!((vs[2].w, vs[2].h, vs[2].kbps), (800, 600, 4000));

    // one the host doesn't retune (the pointer drawn in) is restarted, then a new rate
    // for it is too
    d.json(i::VIDEO, json!({"gen": 4, "w": 800, "h": 600, "kbps": 4000, "draw_cursor": true}));
    assert_eq!(&d.wait(o::CONFIG)[1..3], &[4, 0]);
    d.json(i::VIDEO, json!({"gen": 5, "w": 800, "h": 600, "kbps": 9000, "draw_cursor": true}));
    assert_eq!(&d.wait(o::CONFIG)[1..3], &[5, 0]);
    assert_eq!(fake.videos.lock().unwrap().len(), 5);
    assert_eq!(fake.retuned.lock().unwrap().len(), 2);
    shutdown.store(true, Ordering::SeqCst);
}
