//! glow: afterglow straight to the viewer (docs/glow.md). The link stays the driver's
//! (launch, video, notes, the desktop); with glow the video's frames and the pointer's
//! shape go to the viewer itself over WebTransport (QUIC, UDP), and its input comes
//! back the same way. No relay copies a frame, and no lost packet holds up the ones
//! behind it as it would on TCP.
//!
//! A frame goes as one unit (its config before it when the parameter sets change, the
//! very messages the link would write) cut into datagrams with parity (fec.rs), so a
//! lost packet is usually rebuilt where it lands; what parity can't rebuild the viewer
//! asks for again at once, from the units kept here. QUIC's congestion control is BBR,
//! which keeps the path's queues short, and the encoder's bitrate follows what the path
//! takes (Event::Scale) rather than a keyframe being asked for when it's behind.
//!
//! The driver asks the link for a ticket (`direct`): the port, the SHA-256 of this
//! server's self-signed certificate for the viewer to pin (`serverCertificateHashes`,
//! so no CA and no name), and a one-time ticket that is the session's path. One viewer
//! at a time; a new one takes over. It runs on a tokio runtime of its own; the video
//! thread sends straight into QUIC, nothing handed between threads.

pub mod fec;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc};
use wtransport::endpoint::IncomingSession;
use wtransport::endpoint::endpoint_side::Server;
use wtransport::quinn;
use wtransport::{Connection, Endpoint, Identity, ServerConfig};

use crate::wire::{self, input as msg_in, output as msg_out};
use crate::{debug, info, warn};

/// glow's own version, in the welcome; bumped on any incompatible change
pub const GLOW: u32 = 1;

/// viewer to afterglow, datagrams
pub mod up {
    /// `[kind][flags][u16 seq][u16 idx]...`: pieces of a unit to send again; flags 1:
    /// all of it, the unit never seen
    pub const NACK: u8 = 0x10;
    /// `[kind]`: a keyframe, the viewer lost its place
    pub const KEY: u8 = 0x11;
    /// `[kind][0][u16 loss]`: the pieces lost lately, in thousandths
    pub const REPORT: u8 = 0x12;
    /// `[kind][0][u16 seq][input packet]`: the pointer moved; an older seq than one
    /// already applied is dropped. Also on the control stream, as a message of this type
    pub const MOVE: u8 = 0x20;
    /// `[kind][0][u32 n]`: answered by a PONG datagram
    pub const PING: u8 = 0x21;
}

/// `ready`-like JSON, the first message on the control stream
pub const WELCOME: u8 = 0x8b;

/// what QUIC may hold of datagrams not yet sent: big enough for a 4K keyframe; what's
/// held is watched (Viewer::queued) so it never gets to be dropped from
const SEND_BUF: usize = 8 << 20;
/// parity a moment after its data, so it isn't packed into the same packet
const PARITY_GAP: Duration = Duration::from_millis(1);
/// the newest unit's number said again this long after it, when nothing came after it
const SYNCS: [Duration; 2] = [Duration::from_millis(5), Duration::from_millis(30)];
/// units kept to be sent again
const KEEP_UNITS: usize = 256;
const KEEP_BYTES: usize = 32 << 20;
/// a unit older than this isn't sent again: the viewer will have asked for a keyframe
const RESEND_AGE: Duration = Duration::from_secs(1);
/// what may be sent again, bytes: a bucket this big, refilled this fast a second
const RESEND_BURST: f64 = (4 << 20) as f64;
const RESEND_RATE: f64 = (8 << 20) as f64;
/// past this much queued, deltas are dropped until a keyframe can go
const BEHIND_MS: f64 = 150.0;
/// a ticket's life, and how many can be out at once
const TICKET_LIFE: Duration = Duration::from_secs(60);
const TICKETS: usize = 8;
/// a certificate lives 13 days (WebTransport pins none longer than 14), replaced after 6
const CERT_DAYS: u64 = 13;
const CERT_RENEW: Duration = Duration::from_secs(6 * 86_400);

/// what the viewer did, for the link
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// an input packet, as the link's
    Input(Vec<u8>),
    /// a keyframe asked for
    Idr,
    /// the viewer's clipboard
    Clip(String),
    /// a viewer came (its address)
    Came(String),
    /// the viewer went, why
    Went(String),
    /// the bitrate the path takes now, in thousandths of the video's
    Scale(u32),
}

pub type Events = Arc<dyn Fn(Event) + Send + Sync>;

/// what a driver hands its viewer to come straight here
#[derive(Debug, Clone)]
pub struct Ticket {
    pub port: u16,
    /// the certificate's SHA-256, hex
    pub hash: String,
    pub ticket: String,
    pub expires_ms: u64,
}

struct Shared {
    tickets: Mutex<Vec<(String, Instant)>>,
    viewer: Mutex<Option<Arc<Viewer>>>,
    events: Events,
    ids: AtomicU64,
}

pub struct Glow {
    shared: Arc<Shared>,
    endpoint: Arc<Endpoint<Server>>,
    bind: SocketAddr,
    port: u16,
    cert: Mutex<(String, Instant)>,
    rt: tokio::runtime::Runtime,
}

fn identity() -> Result<Identity, String> {
    Identity::self_signed_builder()
        .subject_alt_names(["afterglow"])
        .from_now_utc()
        .validity_days(CERT_DAYS as u32)
        .build()
        .map_err(|e| format!("no certificate: {e}"))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn config(bind: SocketAddr, id: Identity) -> Result<ServerConfig, String> {
    let mut t = quinn::TransportConfig::default();
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t.datagram_send_buffer_size(SEND_BUF);
    t.datagram_receive_buffer_size(Some(1 << 20));
    t.initial_rtt(Duration::from_millis(30));
    Ok(ServerConfig::builder()
        .with_bind_address(bind)
        .with_custom_transport(id, t)
        .keep_alive_interval(Some(Duration::from_secs(2)))
        .max_idle_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?
        .build())
}

impl Glow {
    /// the server bound (port 0: any) and serving; events go to the link
    pub fn start(bind: SocketAddr, events: Events) -> Result<Arc<Glow>, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("glow")
            .enable_all()
            .build()
            .map_err(|e| format!("no runtime for glow: {e}"))?;
        let id = identity()?;
        let hash = hex(id.certificate_chain().as_slice()[0].hash().as_ref());
        let cfg = config(bind, id)?;
        let endpoint = rt
            .block_on(async { Endpoint::server(cfg) })
            .map_err(|e| format!("glow can't listen on udp {bind}: {e}"))?;
        let port = endpoint.local_addr().map_err(|e| e.to_string())?.port();
        let endpoint = Arc::new(endpoint);
        let shared =
            Arc::new(Shared { tickets: Mutex::default(), viewer: Mutex::default(), events, ids: AtomicU64::new(1) });
        let (ep, sh) = (endpoint.clone(), shared.clone());
        rt.spawn(async move {
            loop {
                let incoming = ep.accept().await;
                tokio::spawn(serve(sh.clone(), incoming));
            }
        });
        info!("glow on udp {}:{port}", bind.ip());
        Ok(Arc::new(Glow { shared, endpoint, bind, port, cert: Mutex::new((hash, Instant::now())), rt }))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// a one-time ticket for a viewer, the certificate renewed first when it's due
    pub fn ticket(&self) -> Result<Ticket, String> {
        let hash = {
            let mut c = self.cert.lock().unwrap();
            if c.1.elapsed() >= CERT_RENEW {
                let id = identity()?;
                let hash = hex(id.certificate_chain().as_slice()[0].hash().as_ref());
                let _g = self.rt.enter();
                self.endpoint.reload_config(config(self.bind, id)?, false).map_err(|e| e.to_string())?;
                *c = (hash, Instant::now());
                info!("glow's certificate renewed");
            }
            c.0.clone()
        };
        let mut b = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b)
            .map_err(|_| "no randomness for a ticket")?;
        let ticket = hex(&b);
        let mut t = self.shared.tickets.lock().unwrap();
        t.retain(|(_, at)| at.elapsed() < TICKET_LIFE);
        if t.len() >= TICKETS {
            t.remove(0);
        }
        t.push((ticket.clone(), Instant::now()));
        Ok(Ticket { port: self.port, hash, ticket, expires_ms: TICKET_LIFE.as_millis() as u64 })
    }

    /// the viewer frames go to now, if one is here
    pub fn viewer(&self) -> Option<Arc<Viewer>> {
        self.shared.viewer.lock().unwrap().clone().filter(|v| !v.gone.load(Ordering::Relaxed))
    }

    /// the viewer sent away (the driver it came through left)
    pub fn drop_viewer(&self, why: &str) {
        if let Some(v) = self.shared.viewer.lock().unwrap().take() {
            v.close(why);
        }
    }
}

impl Shared {
    fn take_ticket(&self, path: &str) -> bool {
        let want = path.trim_start_matches('/').split(['?', '/']).next().unwrap_or("");
        let mut t = self.tickets.lock().unwrap();
        t.retain(|(_, at)| at.elapsed() < TICKET_LIFE);
        match t.iter().position(|(k, _)| wire::same_token(k.as_bytes(), want.as_bytes())) {
            Some(i) => {
                t.remove(i);
                true
            }
            None => false,
        }
    }
}

/// a unit kept to be sent again
struct Kept {
    at: Instant,
    seq: u16,
    flags: u8,
    r: usize,
    plan: fec::Plan,
    unit: Arc<Vec<u8>>,
}

struct Tx {
    seq: u16,
    kept: VecDeque<Kept>,
    kept_bytes: usize,
    /// bytes that may be sent again now, and when that was worked out
    resend: (f64, Instant),
    parity: VecDeque<(Instant, Vec<u8>)>,
    /// when the newest unit went, and how many syncs have followed it
    last: Option<(Instant, u16)>,
    syncs: usize,
    need_key: bool,
    key_asked: Option<Instant>,
    /// bytes handed to QUIC, and what was queued, at the last look
    handed: u64,
    looked: (Instant, u64, usize),
    /// what QUIC drains, bytes a second, as seen while something was queued
    drain: f64,
    scale: f64,
    told: u32,
    down_at: Instant,
    calm_since: Instant,
    up_at: Instant,
}

/// the viewer connected now
pub struct Viewer {
    pub id: u64,
    pub addr: SocketAddr,
    conn: Connection,
    tx: Mutex<Tx>,
    ctl: mpsc::UnboundedSender<Vec<u8>>,
    wake: Notify,
    gone: AtomicBool,
    /// pieces lost lately, thousandths, as the viewer says
    loss: AtomicU32,
    events: Events,
}

fn newer(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

impl Viewer {
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    /// the bitrate the path takes, thousandths of the video's
    pub fn scale(&self) -> u32 {
        self.tx.lock().unwrap().told
    }

    fn close(&self, why: &str) {
        self.gone.store(true, Ordering::Relaxed);
        self.conn.close(0u32.into(), why.as_bytes());
    }

    /// what QUIC holds of datagrams not yet sent, bytes
    fn queued(&self) -> usize {
        SEND_BUF.saturating_sub(self.conn.quic_connection().datagram_send_buffer_space())
    }

    /// how long what's queued takes to drain, ms
    fn queued_ms(&self, tx: &Tx) -> f64 {
        let q = self.queued() as f64;
        if q == 0.0 {
            return 0.0;
        }
        let s = self.conn.quic_connection().stats();
        let rtt = self.conn.rtt().as_secs_f64().max(0.001);
        let est = (s.path.cwnd as f64 / rtt / 2.0).max(tx.drain).max(10_000.0);
        q / est * 1000.0
    }

    /// a message on the control stream: the pointer's shape
    pub fn control(&self, msg: Vec<u8>) {
        let _ = self.ctl.send(msg);
    }

    /// one unit to the viewer: its data pieces now, its parity a moment later. False
    /// when the viewer is gone. A delta is dropped while the path is too far behind,
    /// until a keyframe can go
    pub fn send(&self, unit: &[u8], key: bool) -> bool {
        if self.gone() {
            return false;
        }
        let Some(max) = self.conn.max_datagram_size() else {
            self.close("the viewer takes no datagrams");
            return false;
        };
        let max = max.saturating_sub(fec::HEAD).max(256);
        let mut tx = self.tx.lock().unwrap();
        if key {
            tx.need_key = false;
        } else if tx.need_key {
            return true;
        } else {
            let ms = self.queued_ms(&tx);
            if ms > BEHIND_MS {
                tx.need_key = true;
                debug!("glow: {ms:.0} ms queued, deltas dropped until a keyframe");
                return true;
            }
        }
        let plan = fec::Plan::new(unit.len(), max);
        let r = fec::parity_count(plan.k, self.loss.load(Ordering::Relaxed) as f64 / 1000.0);
        let seq = tx.seq;
        tx.seq = seq.wrapping_add(1);
        let flags = if key { fec::FLAG_KEY } else { 0 };
        for i in 0..plan.k {
            let d = fec::data(unit, &plan, r, seq, flags, i);
            match self.conn.send_datagram(&d) {
                Ok(()) => tx.handed += d.len() as u64,
                Err(wtransport::error::SendDatagramError::TooLarge) => {}
                Err(_) => {
                    self.gone.store(true, Ordering::Relaxed);
                    return false;
                }
            }
        }
        let due = Instant::now() + PARITY_GAP;
        for p in fec::parity(unit, &plan, r, seq, flags) {
            tx.parity.push_back((due, p));
        }
        tx.kept_bytes += unit.len();
        tx.kept.push_back(Kept { at: Instant::now(), seq, flags, r, plan, unit: Arc::new(unit.to_vec()) });
        while tx.kept.len() > KEEP_UNITS || (tx.kept_bytes > KEEP_BYTES && tx.kept.len() > 1) {
            let k = tx.kept.pop_front().unwrap();
            tx.kept_bytes -= k.unit.len();
        }
        tx.last = Some((Instant::now(), seq));
        tx.syncs = 0;
        drop(tx);
        self.wake.notify_one();
        true
    }

    fn datagram(&self, d: &[u8]) -> bool {
        self.conn.send_datagram(d).is_ok()
    }

    /// pieces asked for again, as long as the unit is recent and the bucket holds them
    fn resend(&self, seq: u16, all: bool, idx: &[u16]) {
        let (unit, plan, r, flags, list) = {
            let mut tx = self.tx.lock().unwrap();
            let Some(k) = tx.kept.iter().rev().find(|k| k.seq == seq && k.at.elapsed() < RESEND_AGE) else {
                return;
            };
            let (unit, plan, r, flags) = (k.unit.clone(), k.plan, k.r, k.flags | fec::FLAG_RESENT);
            let list: Vec<usize> = if all {
                (0..plan.k).collect()
            } else {
                idx.iter().map(|&i| i as usize).filter(|&i| i < plan.k).collect()
            };
            let (left, at) = tx.resend;
            let left = (left + at.elapsed().as_secs_f64() * RESEND_RATE).min(RESEND_BURST);
            let n = ((left / plan.p as f64) as usize).min(list.len());
            tx.resend = (left - (n * plan.p) as f64, Instant::now());
            (unit, plan, r, flags, list[..n].to_vec())
        };
        for i in list {
            if !self.datagram(&fec::data(&unit, &plan, r, seq, flags, i)) {
                return;
            }
        }
    }

    /// a keyframe asked for: deltas stop now, the encoder asked once the path has room
    fn want_key(&self) {
        let mut tx = self.tx.lock().unwrap();
        tx.need_key = true;
        self.ask_key(&mut tx);
    }

    fn ask_key(&self, tx: &mut Tx) {
        if tx.key_asked.is_none_or(|t| t.elapsed() >= Duration::from_millis(50)) && self.queued_ms(tx) < 30.0 {
            tx.key_asked = Some(Instant::now());
            (self.events)(Event::Idr);
        }
    }

    /// parity due, syncs due, the bitrate followed: when to look again
    fn tick(&self) -> Instant {
        let now = Instant::now();
        let mut tx = self.tx.lock().unwrap();
        while tx.parity.front().is_some_and(|(at, _)| *at <= now) {
            let (_, p) = tx.parity.pop_front().unwrap();
            tx.handed += p.len() as u64;
            if !self.datagram(&p) {
                return now + Duration::from_secs(1);
            }
        }
        if let Some((at, seq)) = tx.last {
            if tx.syncs < SYNCS.len() && now >= at + SYNCS[tx.syncs] {
                let mut d = vec![fec::SYNC, 0];
                d.extend_from_slice(&seq.to_le_bytes());
                self.datagram(&d);
                tx.syncs += 1;
            }
        }
        if now >= tx.looked.0 + Duration::from_millis(50) {
            self.follow(&mut tx, now);
        }
        if tx.need_key {
            self.ask_key(&mut tx);
        }
        let mut next = tx.looked.0 + Duration::from_millis(50);
        if let Some((at, _)) = tx.parity.front() {
            next = next.min(*at);
        }
        if let Some((at, _)) = tx.last {
            if tx.syncs < SYNCS.len() {
                next = next.min(at + SYNCS[tx.syncs]);
            }
        }
        if tx.need_key {
            next = next.min(now + Duration::from_millis(10));
        }
        next
    }

    /// the encoder's bitrate kept to what the path takes: down by a quarter when what's
    /// queued here or in the network grows, back up a tenth at a time once it's been calm
    fn follow(&self, tx: &mut Tx, now: Instant) {
        let q = self.queued();
        let (then, handed, q0) = tx.looked;
        let dt = now.duration_since(then).as_secs_f64().max(0.001);
        let drained = (tx.handed - handed) as f64 - (q as f64 - q0 as f64);
        if q0 > 0 && drained > 0.0 {
            let rate = drained / dt;
            tx.drain = if tx.drain == 0.0 { rate } else { tx.drain * 0.7 + rate * 0.3 };
        }
        tx.looked = (now, tx.handed, q);
        let qms = self.queued_ms(tx);
        let quic = self.conn.quic_connection();
        let (rtt, floor) = (quic.rtt(), quic.min_rtt());
        let swollen = rtt.saturating_sub(floor) > floor.max(Duration::from_millis(20));
        let busy = qms > 25.0 || swollen;
        let before = tx.scale;
        if busy {
            if now >= tx.down_at + rtt.max(Duration::from_millis(100)) {
                tx.scale = (tx.scale * 0.75).max(0.08);
                tx.down_at = now;
            }
            tx.calm_since = now;
        } else if tx.scale < 1.0
            && now >= tx.calm_since + Duration::from_secs(1)
            && now >= tx.up_at + Duration::from_millis(300)
        {
            tx.scale = (tx.scale * 1.1).min(1.0);
            tx.up_at = now;
        }
        let m = (tx.scale * 1000.0).round() as u32;
        if tx.scale != before && (m.abs_diff(tx.told) >= 20 || m == 1000) {
            tx.told = m;
            debug!(
                "glow: bitrate at {m}/1000 ({qms:.0} ms queued, rtt {} ms, floor {} ms)",
                rtt.as_millis(),
                floor.as_millis()
            );
            (self.events)(Event::Scale(m));
        }
    }
}

async fn serve(sh: Arc<Shared>, incoming: IncomingSession) {
    let Ok(req) = incoming.await else { return };
    let from = req.remote_address();
    if !sh.take_ticket(req.path()) {
        warn!("glow: a session from {from} without a ticket, refused");
        req.forbidden().await;
        return;
    }
    let Ok(conn) = req.accept().await else { return };
    let (ctl, mut ctl_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let now = Instant::now();
    let v = Arc::new(Viewer {
        id: sh.ids.fetch_add(1, Ordering::Relaxed),
        addr: from,
        conn: conn.clone(),
        tx: Mutex::new(Tx {
            seq: 0,
            kept: VecDeque::new(),
            kept_bytes: 0,
            resend: (RESEND_BURST, now),
            parity: VecDeque::new(),
            last: None,
            syncs: 0,
            need_key: false,
            key_asked: None,
            handed: 0,
            looked: (now, 0, 0),
            drain: 0.0,
            scale: 1.0,
            told: 1000,
            down_at: now,
            calm_since: now,
            up_at: now,
        }),
        ctl,
        wake: Notify::new(),
        gone: AtomicBool::new(false),
        loss: AtomicU32::new(0),
        events: sh.events.clone(),
    });
    let welcome = serde_json::json!({"glow": GLOW, "version": crate::link::VERSION});
    v.control(wire::message(WELCOME, welcome.to_string().as_bytes()));
    if let Some(old) = sh.viewer.lock().unwrap().replace(v.clone()) {
        old.close("another viewer took over");
        info!("glow: a new viewer took over");
    }
    info!("glow: a viewer from {from}");
    (sh.events)(Event::Came(from.to_string()));
    // parity, syncs and the bitrate, on their own clock
    let pv = v.clone();
    let pacer = tokio::spawn(async move {
        while !pv.gone() {
            let next = pv.tick();
            tokio::select! {
                _ = pv.wake.notified() => {}
                _ = tokio::time::sleep_until(next.into()) => {}
            }
        }
    });
    let why = run(&v, &conn, &mut ctl_rx).await;
    pacer.abort();
    v.gone.store(true, Ordering::Relaxed);
    let mine = {
        let mut slot = sh.viewer.lock().unwrap();
        let mine = slot.as_ref().is_some_and(|x| Arc::ptr_eq(x, &v));
        if mine {
            *slot = None;
        }
        mine
    };
    info!("glow: the viewer from {from} left: {why}");
    if mine {
        (sh.events)(Event::Went(why));
    }
}

/// the viewer's datagrams and control stream, until it goes; why it went
async fn run(v: &Arc<Viewer>, conn: &Connection, ctl_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> String {
    let mut moved: Option<u16> = None;
    let mut send: Option<wtransport::SendStream> = None;
    let mut recv: Option<wtransport::RecvStream> = None;
    let mut reader = wire::Reader::default();
    let mut buf = vec![0u8; 64 << 10];
    // control messages wait here until the viewer has opened the stream
    let mut held: VecDeque<Vec<u8>> = VecDeque::new();
    let on_move = |moved: &mut Option<u16>, b: &[u8]| {
        if b.len() < 2 {
            return;
        }
        let seq = u16::from_le_bytes([b[0], b[1]]);
        if moved.is_none_or(|m| newer(seq, m)) {
            *moved = Some(seq);
            (v.events)(Event::Input(b[2..].to_vec()));
        }
    };
    loop {
        tokio::select! {
            d = conn.receive_datagram() => {
                let Ok(d) = d else { return "closed".into() };
                let d: &[u8] = &d;
                match d.first().copied() {
                    Some(up::NACK) if d.len() >= 4 => {
                        let seq = u16::from_le_bytes([d[2], d[3]]);
                        let idx: Vec<u16> = d[4..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                        v.resend(seq, d[1] & 1 == 1, &idx);
                    }
                    Some(up::KEY) => v.want_key(),
                    Some(up::REPORT) if d.len() >= 4 => {
                        let l = u16::from_le_bytes([d[2], d[3]]).min(1000) as u32;
                        let old = v.loss.load(Ordering::Relaxed);
                        v.loss.store((old * 3 + l).div_ceil(4), Ordering::Relaxed);
                    }
                    Some(up::MOVE) if d.len() > 4 => on_move(&mut moved, &d[2..]),
                    Some(up::PING) if d.len() >= 6 => {
                        let mut p = vec![fec::PONG, 0];
                        p.extend_from_slice(&d[2..6]);
                        v.datagram(&p);
                    }
                    _ => {}
                }
            }
            s = conn.accept_bi(), if recv.is_none() => {
                let Ok((s, r)) = s else { return "closed".into() };
                let mut s = s;
                while let Some(m) = held.pop_front() {
                    if s.write_all(&m).await.is_err() {
                        return "its control stream failed".into();
                    }
                }
                send = Some(s);
                recv = Some(r);
            }
            n = async { recv.as_mut().unwrap().read(&mut buf).await }, if recv.is_some() => {
                let n = match n {
                    Ok(Some(n)) => n,
                    _ => return "its control stream ended".into(),
                };
                if !reader.feed(&buf[..n]) {
                    return "a message past the limit".into();
                }
                while let Some((ty, body)) = reader.next_message() {
                    match ty {
                        msg_in::INPUT => (v.events)(Event::Input(body)),
                        msg_in::IDR => v.want_key(),
                        msg_in::CLIP => (v.events)(Event::Clip(String::from_utf8_lossy(&body).into_owned())),
                        msg_in::PING => {
                            let _ = v.ctl.send(wire::message(msg_out::PONG, &body));
                        }
                        up::MOVE => on_move(&mut moved, &body),
                        _ => {}
                    }
                }
                if reader.broken() {
                    return "a message past the limit".into();
                }
            }
            m = ctl_rx.recv() => {
                let Some(m) = m else { return "closed".into() };
                match send.as_mut() {
                    Some(s) => {
                        if s.write_all(&m).await.is_err() {
                            return "its control stream failed".into();
                        }
                    }
                    None if held.len() < 256 => held.push_back(m),
                    None => {}
                }
            }
            e = conn.closed() => return e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_order_wraps() {
        assert!(newer(1, 0));
        assert!(newer(0, 65535));
        assert!(!newer(65535, 0));
        assert!(!newer(5, 5));
    }

    #[test]
    fn a_ticket_is_taken_once_and_only_whole() {
        let sh =
            Shared { tickets: Mutex::default(), viewer: Mutex::default(), events: Arc::new(|_| {}), ids: 1.into() };
        sh.tickets.lock().unwrap().push(("abcdef0123456789abcdef0123456789".into(), Instant::now()));
        assert!(!sh.take_ticket("/abcdef0123456789abcdef012345678"));
        assert!(!sh.take_ticket("/"));
        assert!(sh.take_ticket("/abcdef0123456789abcdef0123456789?x=1"));
        assert!(!sh.take_ticket("/abcdef0123456789abcdef0123456789"));
    }
}
