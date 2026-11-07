//! glow end to end on loopback: a driver on the link asks for a ticket, a WebTransport
//! client comes with it (the certificate pinned by the hash, as a browser does), and
//! the video's units arrive whole and in order though a quarter of the datagrams are
//! dropped on the way in: parity rebuilds most, what it can't is asked for again.
//! Input comes back as datagrams and on the control stream; a ticket works once; the
//! viewer gone, the video goes back to the link from a keyframe.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use afterglow::glow::fec::{self, Assembly, Head};
use afterglow::glow::{WELCOME, up};
use afterglow::host::{Ctl, Cursor, Host, Launch, LaunchError, Sink, VideoCfg};
use afterglow::input::Inject;
use afterglow::link::Link;
use afterglow::wire::{self, input as i, output as o};
use serde_json::{Value, json};
use wtransport::tls::Sha256Digest;
use wtransport::{ClientConfig, Connection, Endpoint};

const TOKEN: &str = "0123456789abcdef0123";

#[derive(Default)]
struct Fake {
    running: AtomicBool,
    events: Mutex<Vec<String>>,
    idrs: Mutex<u32>,
    kbps: Mutex<Vec<u32>>,
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

/// frame n's slice: its type, n, then bytes that are never 0 (no start code in them)
fn slice(key: bool, n: u8) -> Vec<u8> {
    let len = if key { 20_000 } else { 3_000 };
    let mut s = vec![if key { 0x65 } else { 0x41 }, n | 0x80];
    s.extend((0..len).map(|i| ((i * 31 + n as usize) % 255 + 1) as u8));
    s
}

impl Host for Fake {
    fn hostname(&self) -> String {
        "box".into()
    }
    fn virtual_display(&self) -> bool {
        true
    }
    fn launch(&self, _: &Launch) -> Result<(), LaunchError> {
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    fn terminate(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: &mut dyn Sink) -> Result<(), String> {
        let c = Cursor { visible: true, w: 1, h: 1, hot_x: 0, hot_y: 0, rgba: vec![0, 0, 0, 255] };
        if !out.cursor(&c) {
            return Ok(());
        }
        let mut idr = true;
        let mut n = 0u8;
        while !ctl.stop.load(Ordering::SeqCst) {
            if ctl.idr.swap(false, Ordering::SeqCst) {
                *self.idrs.lock().unwrap() += 1;
                idr = true;
            }
            let k = ctl.kbps.load(Ordering::SeqCst);
            if k != 0 {
                self.kbps.lock().unwrap().push(k.min(v.kbps));
            }
            let mut au = if idr { [&SPS[..], &PPS].concat() } else { Vec::new() };
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend(slice(idr, n));
            if !out.frame(&afterglow::h264::split(&au), idr) {
                return Ok(());
            }
            idr = false;
            n = n.wrapping_add(1);
            thread::sleep(Duration::from_millis(8));
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
    /// the next note of type t
    fn note(&mut self, t: &str) -> Value {
        loop {
            let n = self.wait_json(o::NOTE);
            if n["t"] == t {
                return n;
            }
        }
    }
}

fn serve() -> (Arc<Fake>, u16) {
    let fake = Arc::new(Fake::default());
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let link = Link::new(fake.clone(), TOKEN.into(), Arc::new(AtomicBool::new(false)));
    link.direct_at("127.0.0.1:0".parse().unwrap());
    thread::spawn(move || link.serve_on(l, addr));
    (fake, addr.port())
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

async fn connect(port: u16, hash: &str, ticket: &str) -> Result<Connection, String> {
    let digest = Sha256Digest::new(unhex(hash).try_into().unwrap());
    let cfg = ClientConfig::builder()
        .with_bind_address("127.0.0.1:0".parse().unwrap())
        .with_server_certificate_hashes([digest])
        .build();
    let ep = Endpoint::client(cfg).map_err(|e| e.to_string())?;
    ep.connect(format!("https://127.0.0.1:{port}/{ticket}")).await.map_err(|e| e.to_string())
}

/// a viewer's receiving end as a browser's would be: pieces put together, what parity
/// can't rebuild asked for again (each unit at most every 20 ms, a window of 64 at
/// most), units handed on in order from the first keyframe
struct Rx {
    units: HashMap<u16, Assembly>,
    asked: HashMap<u16, (Instant, u32)>,
    next: Option<u16>,
    newest: u16,
    /// every nth fresh datagram dropped on the way in
    drop_every: usize,
    n: usize,
    dropped: usize,
    nacks: usize,
}

impl Rx {
    fn new(drop_every: usize) -> Rx {
        Rx {
            units: HashMap::new(),
            asked: HashMap::new(),
            next: None,
            newest: 0,
            drop_every,
            n: 0,
            dropped: 0,
            nacks: 0,
        }
    }

    /// what's missing from next up to newest (and newest itself when `incl`), asked for
    fn ask(&mut self, conn: &Connection, incl: bool) {
        let Some(next) = self.next else { return };
        let span = self.newest.wrapping_sub(next).min(64) + incl as u16;
        for s in (0..span).map(|i| next.wrapping_add(i)) {
            let (idx, all) = match self.units.get(&s) {
                Some(a) if a.done() => continue,
                Some(a) => (a.wanted(), false),
                None => (Vec::new(), true),
            };
            if !all && idx.is_empty() {
                continue;
            }
            if self.asked.get(&s).is_some_and(|(t, _)| t.elapsed() < Duration::from_millis(20)) {
                continue;
            }
            let n = self.asked.get(&s).map_or(0, |a| a.1) + 1;
            assert!(n < 10, "unit {s} asked for {n} times");
            self.asked.insert(s, (Instant::now(), n));
            let mut d = vec![up::NACK, all as u8];
            d.extend_from_slice(&s.to_le_bytes());
            for i in idx {
                d.extend_from_slice(&i.to_le_bytes());
            }
            self.nacks += 1;
            conn.send_datagram(d).unwrap();
        }
    }

    async fn unit(&mut self, conn: &Connection) -> Vec<u8> {
        loop {
            if let Some(n) = self.next {
                if self.units.get(&n).is_some_and(|a| a.done()) {
                    self.next = Some(n.wrapping_add(1));
                    self.asked.remove(&n);
                    return self.units.remove(&n).unwrap().into_bytes();
                }
            }
            let d = match tokio::time::timeout(Duration::from_millis(20), conn.receive_datagram()).await {
                Ok(d) => d.unwrap(),
                Err(_) => {
                    self.ask(conn, true);
                    continue;
                }
            };
            let d: &[u8] = &d;
            if d[0] == fec::SYNC {
                let seq = u16::from_le_bytes([d[2], d[3]]);
                if self.next.is_some_and(|n| seq.wrapping_sub(n) < 0x8000) {
                    self.newest = seq;
                    self.ask(conn, true);
                }
                continue;
            }
            let Some(h) = Head::parse(d) else { continue };
            if h.flags & fec::FLAG_RESENT == 0 {
                self.n += 1;
                if self.n % self.drop_every == 0 {
                    self.dropped += 1;
                    continue;
                }
            }
            if self.next.is_none() {
                if h.flags & fec::FLAG_KEY == 0 {
                    continue;
                }
                self.next = Some(h.seq);
                self.newest = h.seq;
            }
            let next = self.next.unwrap();
            if h.seq.wrapping_sub(next) >= 0x8000 {
                continue; // already handed on
            }
            if h.seq.wrapping_sub(self.newest) < 0x8000 {
                self.newest = h.seq;
            }
            self.units.entry(h.seq).or_insert_with(|| Assembly::new(&h)).put(&h, &d[fec::HEAD..]);
            // a later unit, or this one's last parity, says what's missing before it
            let last_parity = h.kind == fec::PARITY && h.idx + 1 == h.r;
            if h.seq != next || last_parity {
                self.ask(conn, last_parity);
            }
        }
    }
}

fn messages(unit: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut r = wire::Reader::default();
    r.feed(unit);
    std::iter::from_fn(|| r.next_message()).collect()
}

fn input_packet(magic: u32, body: &[u8]) -> Vec<u8> {
    let mut b = (4 + body.len() as u32).to_be_bytes().to_vec();
    b.extend_from_slice(&magic.to_le_bytes());
    b.extend_from_slice(body);
    b
}

/// the link's messages up to a pong, none of them a frame
fn no_frames_on_the_link(d: &mut Driver) {
    d.send(i::PING, &[4, 3, 2, 1]);
    loop {
        let (t, _) = d.next().unwrap();
        assert!(t != o::FRAME && t != o::CONFIG, "a frame came over the link");
        if t == o::PONG {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn straight_to_the_viewer_through_loss() {
    let (fake, port) = serve();
    let mut d = Driver::open(port);
    d.send(i::HELLO, TOKEN.as_bytes());
    assert_eq!(d.wait_json(o::READY)["direct"], true);
    d.json(i::LAUNCH, json!({"cmd": "C:\\App.lnk"}));
    assert_eq!(d.wait_json(o::LAUNCHED)["ok"], true);

    d.json(i::DIRECT, json!({"id": 4}));
    let t = d.wait_json(o::DIRECT);
    assert_eq!((t["ok"].clone(), t["id"].clone()), (json!(true), json!(4)));
    let (gport, hash, ticket) =
        (t["port"].as_u64().unwrap() as u16, t["hash"].as_str().unwrap(), t["ticket"].as_str().unwrap());
    assert_eq!((hash.len(), ticket.len()), (64, 32));

    // a made-up ticket is refused
    assert!(connect(gport, hash, "00000000000000000000000000000000").await.is_err());

    let conn = connect(gport, hash, ticket).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
    // the stream is opened by what's first on it: a ping, answered on it
    send.write_all(&wire::message(i::PING, &[1, 2, 3, 4])).await.unwrap();
    assert_eq!(d.note("direct")["on"], true);

    d.json(i::VIDEO, json!({"gen": 5, "w": 1280, "h": 720, "fps": 120, "kbps": 20000}));
    // the control stream: the welcome first
    let mut cr = wire::Reader::default();
    let mut ctl = Vec::new();
    while ctl.len() < 3 {
        let mut b = [0u8; 4096];
        let n = recv.read(&mut b).await.unwrap().unwrap();
        cr.feed(&b[..n]);
        ctl.extend(std::iter::from_fn(|| cr.next_message()));
    }
    assert_eq!(ctl[0].0, WELCOME);
    assert_eq!(serde_json::from_slice::<Value>(&ctl[0].1).unwrap()["glow"], 1);
    // the pong and the shape, whichever was first
    assert!(ctl.iter().any(|(t, b)| *t == o::PONG && b[..] == [1, 2, 3, 4]));
    assert!(ctl.iter().any(|(t, _)| *t == o::CURSOR));

    // the units: a config and keyframe, then deltas, each whole, in order, a quarter of
    // what came dropped
    let mut rx = Rx::new(4);
    let first = messages(&rx.unit(&conn).await);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].0, o::CONFIG);
    assert_eq!(&first[0].1[..7], &[0, 5, 0, 0x00, 0x05, 0xd0, 0x02]);
    assert_eq!(first[1].0, o::FRAME);
    let f = &first[1].1;
    assert_eq!(&f[..3], &[1, 5, 0]);
    let n0 = f[12] & 0x7f;
    assert_eq!(&f[11..], &slice(true, n0)[..]);
    for k in 1..60u8 {
        let m = messages(&rx.unit(&conn).await);
        assert_eq!(m.len(), 1);
        let f = &m[0].1;
        assert_eq!(f[0], 0, "a delta");
        let n = f[12] & 0x7f;
        assert_eq!(n, (n0 + k) & 0x7f, "in order");
        assert_eq!(&f[11..], &slice(false, n)[..], "whole");
    }
    assert!(rx.dropped > 40 && rx.nacks > 0, "dropped {}, asked {}", rx.dropped, rx.nacks);
    no_frames_on_the_link(&mut d);

    // input: a move as a datagram, an older one dropped, a click on the control stream
    let mv = |seq: u16, x: u16| {
        let mut b = vec![up::MOVE, 0];
        b.extend_from_slice(&seq.to_le_bytes());
        let mut body = x.to_be_bytes().to_vec();
        body.extend_from_slice(&[0x01, 0x68, 0, 0, 0x05, 0x00, 0x02, 0xd0]);
        b.extend(input_packet(0x05, &body));
        b
    };
    conn.send_datagram(mv(7, 0x0280)).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    conn.send_datagram(mv(6, 0)).unwrap();
    send.write_all(&wire::message(i::INPUT, &input_packet(0x08, &[1]))).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while fake.events.lock().unwrap().len() < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*fake.events.lock().unwrap(), ["abs 0.50 0.50", "button 1 true"]);

    // a ticket works once
    assert!(connect(gport, hash, ticket).await.is_err());

    // the viewer gone: the link's again, from its config and a keyframe
    conn.close(0u32.into(), b"bye");
    assert_eq!(d.note("direct")["on"], false);
    let cfg = d.wait(o::CONFIG);
    assert_eq!(&cfg[..3], &[0, 5, 0]);
    let f = d.wait(o::FRAME);
    assert_eq!(f[0], 1);
    // the button the viewer held let go
    assert_eq!(fake.events.lock().unwrap().last().unwrap(), "button 1 false");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_driver_leaving_sends_the_viewer_away() {
    let (_, port) = serve();
    let mut d = Driver::open(port);
    d.send(i::HELLO, TOKEN.as_bytes());
    d.wait_json(o::READY);
    d.json(i::DIRECT, json!({}));
    let t = d.wait_json(o::DIRECT);
    let conn = connect(t["port"].as_u64().unwrap() as u16, t["hash"].as_str().unwrap(), t["ticket"].as_str().unwrap())
        .await
        .unwrap();
    d.note("direct");
    drop(d);
    tokio::time::timeout(Duration::from_secs(5), conn.closed()).await.expect("the viewer was sent away");
}
