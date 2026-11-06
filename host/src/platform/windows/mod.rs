//! The Windows host: apps started in the console's session, each on a SudoVDA display
//! of its own where the driver is installed, captured with DXGI, encoded by Media
//! Foundation, driven with SendInput.

mod capture;
mod desk;
mod display;
mod encode;
mod input;
mod process;
pub mod svc;
mod vdisplay;
mod wm;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler,
};
use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::core::{BOOL, GUID, PWSTR};

use crate::conf::Conf;
use crate::host::{Ctl, Host, Launch, LaunchError, Sink, VideoCfg};
use crate::input::Inject;
use crate::{debug, info, warn};

/// the display afterglow's apps get: one id, so Windows keeps its settings
const DISPLAY_GUID: GUID = GUID::from_u128(0x5d1b0f7a_3c55_4e8b_9d2e_af7e0c6b1a01);
const DISPLAY_SERIAL: &str = "5D1B0F7A-3C55";

/// `afterglow --delete`: a delete done as whoever runs it (docs/protocol.md, "The desktop")
pub fn delete_main() -> i32 {
    desk::delete_main()
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub(crate) fn from_wide(w: &[u16]) -> String {
    String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())])
}

/// COM on this thread, as Media Foundation's encoder activation needs; already there is fine
fn com_init() {
    // SAFETY: balanced by the thread's end; S_FALSE (already initialised) is ignored
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

static SHUTDOWN: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub fn on_shutdown(flag: Arc<AtomicBool>) {
    let _ = SHUTDOWN.set(flag);
    unsafe extern "system" fn handler(ev: u32) -> BOOL {
        if let Some(f) = SHUTDOWN.get() {
            f.store(true, Ordering::SeqCst);
        }
        // the process ends when this returns for these: time for the app and display to be put away
        if ev == CTRL_CLOSE_EVENT || ev == CTRL_LOGOFF_EVENT || ev == CTRL_SHUTDOWN_EVENT {
            thread::sleep(Duration::from_secs(5));
        }
        matches!(ev, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT)
            .into()
    }
    // SAFETY: the handler only stores to an atomic and sleeps
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
}

/// a process attached to: not started here, so never ended here when let go
struct Attached {
    pid: u32,
    /// opened to wait on
    process: windows::Win32::Foundation::HANDLE,
}

// SAFETY: a process handle may be waited on and closed from any thread
unsafe impl Send for Attached {}

impl Drop for Attached {
    fn drop(&mut self) {
        // SAFETY: the handle this owns
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.process);
        }
    }
}

struct Running {
    app: Option<process::App>,
    /// or a process already running, taken as the app
    attached: Option<Attached>,
    /// the virtual display's GDI name, while the app has one
    vdisplay: Option<String>,
    /// the app's windows kept to that display
    follower: Option<wm::Follower>,
}

pub struct Windows {
    conf: Conf,
    vda: Mutex<Option<vdisplay::Vda>>,
    /// when the driver was last looked for, so a machine without it isn't asked at
    /// every connection
    vda_tried: Mutex<Instant>,
    run: Mutex<Running>,
    input: input::WinInput,
    /// the capture is waiting on the secure desktop (Windows::waiting)
    wait: AtomicBool,
}

impl Windows {
    pub fn open(conf: &Conf) -> Result<Arc<Windows>, String> {
        com_init();
        // SAFETY: process-wide settings made once at start
        unsafe {
            // DXGI duplication wants per-monitor DPI awareness
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            timeBeginPeriod(1);
            // without it (a Windows Server lacking the feature) the link still serves; videos fail
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                warn!("Media Foundation won't start, so nothing can be encoded: {e}");
            }
        }
        if process::running_as_system() {
            info!("running as SYSTEM: apps start as the console's user");
        }
        let vda = vdisplay::Vda::open(true).map_err(|e| warn!("no virtual displays: {e}")).ok();
        Ok(Arc::new(Windows {
            conf: conf.clone(),
            vda: Mutex::new(vda),
            vda_tried: Mutex::new(Instant::now()),
            run: Mutex::new(Running { app: None, attached: None, vdisplay: None, follower: None }),
            input: input::WinInput::default(),
            wait: AtomicBool::new(false),
        }))
    }

    fn vda_ok(&self) -> bool {
        let mut v = self.vda.lock().unwrap();
        if !v.as_ref().is_some_and(|d| d.ok()) {
            // the driver installed or restarted since: tried again, now and then
            let mut tried = self.vda_tried.lock().unwrap();
            if tried.elapsed() >= VDA_RETRY {
                *tried = Instant::now();
                *v = match vdisplay::Vda::open(false) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        debug!("no virtual displays still: {e}");
                        None
                    }
                };
            }
        }
        v.as_ref().is_some_and(|d| d.ok())
    }

    /// a virtual display added at w×h, set to that mode (and made primary, asked): its
    /// GDI name
    fn add_display(&self, w: u32, h: u32, fps: u32) -> Option<String> {
        if !self.vda_ok() {
            return None;
        }
        let v = self.vda.lock().unwrap();
        let vda = v.as_ref()?;
        if !self.conf.adapter.is_empty() {
            match display::adapter_luid(&self.conf.adapter) {
                Some(luid) if vda.set_render_adapter(luid) => {}
                _ => warn!("the virtual display can't render on {}", self.conf.adapter),
            }
        }
        let millihz = fps * 1000;
        match vda.add(DISPLAY_GUID, w, h, millihz, "Phosphorus", DISPLAY_SERIAL) {
            Ok(name) => {
                info!("virtual display {name}: {w}x{h}@{fps}");
                // Windows puts back the mode it last kept for this display first, and
                // can turn a change down while it does: asked again for a moment
                let t0 = Instant::now();
                loop {
                    let last = t0.elapsed() >= MODE_SETTLE;
                    if display::set_mode(&name, w, h, millihz, last) || last {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                if self.conf.primary {
                    display::make_primary(&name);
                }
                Some(name)
            }
            Err(e) => {
                warn!("{e}");
                // added or not, it's removed with the app
                vda.remove(DISPLAY_GUID);
                None
            }
        }
    }

    /// the app's display set to the video's size. SudoVDA offers the size a display
    /// was added at and a few standard ones, so for any other it's added again at this
    /// one: the app's windows follow it there. Its name, which may be new, back
    fn fit_display(&self, name: &str, w: u32, h: u32, fps: u32) -> Option<String> {
        if display::set_mode(name, w, h, fps * 1000, false) {
            return Some(name.to_string());
        }
        info!("{name} has no {w}x{h} mode: added again at that size");
        self.remove_display();
        let new = self.add_display(w, h, fps);
        let mut r = self.run.lock().unwrap();
        r.vdisplay = new.clone();
        if let (Some(f), Some(n)) = (&r.follower, &new) {
            f.set_display(n);
        }
        new
    }

    fn remove_display(&self) {
        if let Some(v) = self.vda.lock().unwrap().as_ref() {
            if v.remove(DISPLAY_GUID) {
                info!("virtual display removed");
            }
        }
    }

    /// the capture waiting on what it can't capture (the secure desktop without the
    /// right to it), said once as it starts and once as it ends
    fn waiting(&self, why: Option<&str>) {
        let was = self.wait.swap(why.is_some(), Ordering::Relaxed);
        match why {
            Some(w) if !was => info!("the capture waits: {w}"),
            None if was => info!("the capture goes on"),
            _ => {}
        }
    }

    /// afterglow's own encoder unless Media Foundation's is asked for, or the pointer is
    /// drawn in (the GPU composes it)
    fn own_encoder(&self, v: &VideoCfg) -> bool {
        !matches!(self.conf.encoder.as_str(), "hardware" | "mf" | "software") && !v.draw_cursor
    }

    /// capture and encode one display until stopped or lost
    fn stream(&self, v: &mut VideoCfg, ctl: &Ctl, out: &mut dyn Sink, name: Option<&str>) -> Result<bool, String> {
        // onto the desktop with the input (the secure one for UAC and the lock screen)
        // before anything is made on this thread: one that owns a window can't move
        if !input::sync_desktop() {
            debug!("the capture couldn't move to the desktop with the input");
        }
        let Some(o) = display::find(name) else { return Err("no display to capture".into()) };
        *self.input.rect.lock().unwrap() = input::Rect {
            left: o.rect.left,
            top: o.rect.top,
            w: o.rect.right - o.rect.left,
            h: o.rect.bottom - o.rect.top,
        };
        let (dev, ctx) =
            capture::device_for(&o).map_err(|e| format!("no Direct3D device on {}: {e}", o.adapter_name))?;
        let own = self.own_encoder(v);
        let mut dup = match capture::Dup::new(&o, &dev, &ctx, v.draw_cursor, own) {
            Ok(d) => d,
            Err(capture::Lost::Again) => return Ok(true),
            Err(capture::Lost::Wait(why)) => {
                self.waiting(Some(&why));
                thread::sleep(Duration::from_millis(200));
                return Ok(true);
            }
            Err(capture::Lost::Fail(why)) => return Err(why),
        };
        self.waiting(None);
        if own {
            info!("capturing {} ({}x{}) on {}, afterglow's own encoder", o.name, dup.w, dup.h, o.adapter_name);
            return self.stream_own(v, ctl, out, &mut dup);
        }
        let force_sw = self.conf.encoder == "software";
        let mut enc = encode::Encoder::new(&dev, &ctx, &dup.comp, dup.w, dup.h, v, o.luid, force_sw)?;
        info!("capturing {} ({}x{}) on {}", o.name, dup.w, dup.h, o.adapter_name);
        let period = Duration::from_secs_f64(1.0 / v.fps as f64);
        // a frame given to the encoder yet: until then every capture is the keyframe's
        let mut sent = false;
        // a change captured and not yet encoded: a display faster than the video waits its turn
        let mut pending = false;
        let mut next_slot = Instant::now();
        let mut last = Instant::now();
        // after a change, the same frame given again (in its slots) while the encoder
        // holds frames back, so the change comes out now rather than with the next one
        let mut tail = 0u32;
        let mut stats = Stats::new();
        while !ctl.stop.load(Ordering::Relaxed) {
            // what the encoder finished since, sent as soon as it's out
            for (au, key) in enc.poll()? {
                stats.au(au.len());
                if !out.frame(&crate::h264::split(&au), key) {
                    return Ok(false);
                }
            }
            stats.tick(enc.held());
            let now = Instant::now();
            let flush = tail > 0 && enc.held() > 0;
            let wait = if pending || flush { next_slot.saturating_duration_since(now) } else { period };
            // short: the wait holds the device the encoder shares
            match dup.next(wait.as_millis().clamp(1, 4) as u32) {
                Ok(capture::Got::Frame) => {
                    pending = true;
                    stats.captured += 1;
                }
                Ok(capture::Got::Timeout) => {}
                Err(capture::Lost::Again) => {
                    // a mode change or a desktop switch: everything set up again, a pause the viewer sees
                    info!("the capture was lost; set up again");
                    return Ok(true);
                }
                Err(capture::Lost::Wait(why)) => {
                    self.waiting(Some(&why));
                    thread::sleep(Duration::from_millis(100));
                    return Ok(true);
                }
                Err(capture::Lost::Fail(why)) => return Err(why),
            }
            // the pointer's shape to the viewer as it changes, not drawn in
            if !v.draw_cursor {
                if let Some(c) = dup.cursor() {
                    if !out.cursor(&c) {
                        return Ok(false);
                    }
                }
            }
            if !sent && !pending {
                // no picture captured yet
                continue;
            }
            let idr = ctl.idr.swap(false, Ordering::Relaxed) || !sent;
            let now = Instant::now();
            // a change in its slot, a keyframe asked, or once a second the last frame again to keep the stream alive
            let flush = tail > 0 && enc.held() > 0;
            let due = ((pending || flush) && now >= next_slot) || idr || last.elapsed() >= Duration::from_secs(1);
            if !due {
                continue;
            }
            tail = if pending || idr { FLUSH_FRAMES } else { tail.saturating_sub(1) };
            pending = false;
            last = now;
            next_slot = (next_slot + period).max(now);
            let t0 = Instant::now();
            let made = enc.encode(idr)?;
            stats.encoded(t0.elapsed());
            sent = true;
            for (au, key) in made {
                stats.au(au.len());
                if !out.frame(&crate::h264::split(&au), key) {
                    return Ok(false);
                }
            }
        }
        Ok(false)
    }
}

impl Windows {
    /// the display encoded by afterglow's own encoder (crate::avc): at the video's size
    /// only the rectangles duplication says changed are read back, converted and coded
    /// (at another size, all of it scaled in); what's coarse is refined once the
    /// screen is still. As the Linux host's
    fn stream_own(
        &self,
        v: &mut VideoCfg,
        ctl: &Ctl,
        out: &mut dyn Sink,
        dup: &mut capture::Dup,
    ) -> Result<bool, String> {
        let (w, h) = (v.w as usize, v.h as usize);
        let cores = thread::available_parallelism().map_or(1, |n| n.get());
        let mut enc = crate::avc::Encoder::new(w, h, v.fps, cores);
        let mut rate = crate::avc::Rate::new(v.kbps, v.fps);
        let (pw, ph) = enc.padded();
        let mut yuv = crate::convert::Yuv::padded(w, h, pw, ph);
        let period = Duration::from_secs_f64(1.0 / v.fps as f64);
        let keepalive = Duration::from_secs(1);
        let (mut sent, mut pending) = (false, false);
        let mut next_slot = Instant::now();
        let mut last = Instant::now();
        let mut changed_at = Instant::now();
        let mut next_refine = Instant::now();
        let mut coarse = false;
        let mut stats = Stats::new();
        let started = Instant::now();
        while !ctl.stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            let refine_at = (changed_at + REFINE_AFTER).max(next_slot).max(next_refine);
            let until = if pending {
                next_slot
            } else if coarse {
                refine_at.min(last + keepalive)
            } else {
                last + keepalive
            };
            // short enough that a keyframe or a stop asked is seen soon
            let wait = until.saturating_duration_since(now).min(Duration::from_millis(8));
            match dup.next(wait.as_millis() as u32) {
                Ok(capture::Got::Frame) => {
                    pending = true;
                    stats.captured += 1;
                }
                Ok(capture::Got::Timeout) => {}
                Err(capture::Lost::Again) => {
                    info!("the capture was lost; set up again");
                    return Ok(true);
                }
                Err(capture::Lost::Wait(why)) => {
                    self.waiting(Some(&why));
                    thread::sleep(Duration::from_millis(100));
                    return Ok(true);
                }
                Err(capture::Lost::Fail(why)) => return Err(why),
            }
            if let Some(c) = dup.cursor() {
                if !out.cursor(&c) {
                    return Ok(false);
                }
            }
            if !sent && !pending && started.elapsed() < FIRST_FRAME {
                // an idle display presents nothing until something draws: past a moment, a
                // black keyframe, so the viewer has the config and a picture
                continue;
            }
            // a new gen and bitrate: the same capture goes on, a keyframe under the new gen
            if let Some(r) = ctl.take_retune() {
                out.regen(r.vgen);
                v.kbps = r.kbps;
                rate.retarget(r.kbps, v.fps);
                ctl.idr.store(true, Ordering::Relaxed);
            }
            let idr = ctl.idr.swap(false, Ordering::Relaxed) || !sent;
            let now = Instant::now();
            let change = (pending && now >= next_slot) || idr;
            let alive = now >= last + keepalive;
            let refine = !change && !pending && coarse && now >= refine_at;
            if !(change || alive || refine) {
                continue;
            }
            if change {
                let lost = |e| match e {
                    capture::Lost::Fail(why) | capture::Lost::Wait(why) => why,
                    capture::Lost::Again => "the capture was lost".to_string(),
                };
                if (dup.w, dup.h) == (v.w, v.h) {
                    dup.read(|r, px, pitch| {
                        yuv.put(px, pitch, r.x as usize, r.y as usize, r.w as usize, r.h as usize);
                        enc.mark(r.x as usize, r.y as usize, r.w as usize, r.h as usize);
                    })
                    .map_err(lost)?;
                } else {
                    let (sw, sh) = (dup.w as usize, dup.h as usize);
                    dup.read_whole(|px, pitch| crate::convert::scaled(px, pitch, sw, sh, &mut yuv)).map_err(lost)?;
                    enc.mark_all();
                }
                pending = false;
                next_slot = (next_slot + period).max(now);
            }
            let [y, u, vv] = yuv.planes();
            let frame = crate::avc::Frame { y, u, v: vv };
            let (opts, act) = if refine {
                let fine =
                    if enc.coarse(crate::avc::QP_LEGIBLE) > 0 { crate::avc::QP_READ } else { crate::avc::QP_FINE };
                (
                    crate::avc::Opts { idr: false, qp: fine, refine: Some((fine, rate.refine_bytes())), always: false },
                    0.0,
                )
            } else {
                let (mbs, act) = enc.activity(&frame, idr);
                (crate::avc::Opts { idr, qp: rate.qp(mbs, act), refine: None, always: alive && !change }, act)
            };
            let t0 = Instant::now();
            let d = enc.encode(&frame, &opts);
            stats.encoded(t0.elapsed());
            rate.seen(&d, opts.qp, act);
            // whether refinement has more to do, frame or not: a refinement that found
            // nothing to send leaves nothing coarse, and isn't tried again at once
            coarse = enc.coarse(crate::avc::QP_FINE) > 0;
            if refine {
                next_refine = now + period;
            }
            if !d.frame {
                continue;
            }
            // the keepalive counts from the last frame that went out
            last = now;
            sent = true;
            stats.au(d.bytes);
            if !out.frame(&enc.nals(), d.key) {
                return Ok(false);
            }
            if change {
                changed_at = now;
            }
            stats.tick(0);
        }
        Ok(false)
    }
}

/// how often SudoVDA is looked for again while it isn't there
const VDA_RETRY: Duration = Duration::from_secs(10);

/// how long a display just added is asked again for its mode, while Windows is putting
/// back the one it kept for it
const MODE_SETTLE: Duration = Duration::from_millis(1000);

/// how long the screen is still before what was coded coarsely is refined
const REFINE_AFTER: Duration = Duration::from_millis(60);

/// how long an app whose job emptied before any window of it showed is still waited for
const LAUNCHER_GRACE: Duration = Duration::from_secs(10);

/// how long a video waits for duplication's first frame before a black keyframe goes out
const FIRST_FRAME: Duration = Duration::from_millis(250);

/// the calling thread registered with the multimedia scheduler (MMCSS) as capture
/// work, for as long as this lives: the scheduler runs it ahead of ordinary threads so
/// a busy machine doesn't delay a frame by a scheduling quantum
struct Mmcss(windows::Win32::Foundation::HANDLE);

impl Mmcss {
    fn capture() -> Option<Mmcss> {
        let mut idx = 0u32;
        // SAFETY: registers this thread; reverted in Drop on the same thread
        unsafe {
            windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(windows::core::w!("Capture"), &mut idx)
                .ok()
                .map(Mmcss)
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: the handle AvSetMmThreadCharacteristicsW gave this thread
        unsafe {
            let _ = windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

/// frames given again after a change at most, while the encoder holds some back
const FLUSH_FRAMES: u32 = 8;

/// what a video did, logged every few seconds: where its time goes when it's slow
struct Stats {
    since: Instant,
    captured: u32,
    encodes: u32,
    encode_time: Duration,
    encode_max: Duration,
    aus: u32,
    bytes: u64,
    held_max: i64,
}

impl Stats {
    fn new() -> Stats {
        Stats {
            since: Instant::now(),
            captured: 0,
            encodes: 0,
            encode_time: Duration::ZERO,
            encode_max: Duration::ZERO,
            aus: 0,
            bytes: 0,
            held_max: 0,
        }
    }

    fn encoded(&mut self, d: Duration) {
        self.encodes += 1;
        self.encode_time += d;
        self.encode_max = self.encode_max.max(d);
    }

    fn au(&mut self, n: usize) {
        self.aus += 1;
        self.bytes += n as u64;
    }

    fn tick(&mut self, held: i64) {
        self.held_max = self.held_max.max(held);
        let s = self.since.elapsed();
        if s < Duration::from_secs(5) {
            return;
        }
        let secs = s.as_secs_f64();
        let avg = if self.encodes > 0 { self.encode_time.as_secs_f64() * 1000.0 / self.encodes as f64 } else { 0.0 };
        info!(
            "video: {:.0} changes/s, {:.0} encodes/s ({avg:.1} ms, max {} ms), {:.0} frames/s out at {:.0} kbps, up to {} held",
            self.captured as f64 / secs,
            self.encodes as f64 / secs,
            self.encode_max.as_millis(),
            self.aus as f64 / secs,
            self.bytes as f64 * 8.0 / 1000.0 / secs,
            self.held_max,
        );
        *self = Stats::new();
    }
}

impl Inject for Windows {
    fn mouse_abs(&self, x: f32, y: f32) {
        self.input.mouse_abs(x, y)
    }
    fn mouse_rel(&self, dx: i32, dy: i32) {
        self.input.mouse_rel(dx, dy)
    }
    fn button(&self, b: u8, down: bool) {
        self.input.button(b, down)
    }
    fn scroll(&self, a: i32) {
        self.input.scroll(a)
    }
    fn hscroll(&self, a: i32) {
        self.input.hscroll(a)
    }
    fn key(&self, vk: u8, down: bool) {
        self.input.key(vk, down)
    }
    fn text(&self, s: &str) {
        self.input.text(s)
    }
}

impl Host for Windows {
    fn hostname(&self) -> String {
        let mut buf = [0u16; 256];
        let mut n = buf.len() as u32;
        // SAFETY: buf sized as n says
        if unsafe { GetComputerNameExW(ComputerNameDnsHostname, Some(PWSTR(buf.as_mut_ptr())), &mut n) }.is_ok() {
            from_wide(&buf[..n as usize])
        } else {
            String::new()
        }
    }

    fn virtual_display(&self) -> bool {
        self.vda_ok()
    }

    fn launch(&self, l: &Launch) -> Result<(), LaunchError> {
        let Some(cmd) = crate::wire::launcher_cmd(&l.cmd) else {
            return Err(LaunchError::new("not an app this host can start"));
        };
        if let Some(login) = &l.login {
            process::sign_in(login).map_err(LaunchError::new)?;
        }
        self.terminate();
        let vname = if l.virtual_display { self.add_display(l.w, l.h, l.fps) } else { None };
        if display::find(vname.as_deref()).is_none() {
            if vname.is_some() {
                self.remove_display();
            }
            return Err(LaunchError { code: Some(503), error: "no display or encoder to capture with".into() });
        }
        let cwd = Path::new(&l.cmd).parent().filter(|p| p.is_dir()).and_then(|p| p.to_str()).map(str::to_string);
        match process::App::start(&cmd, cwd.as_deref()) {
            Ok(app) => {
                // on a display of its own, the app alone: its windows fill it, the desktop unseen
                let follower = vname.clone().map(|n| wm::Follower::start(app.job(), n));
                *self.run.lock().unwrap() = Running { app: Some(app), attached: None, vdisplay: vname, follower };
                Ok(())
            }
            Err(e) => {
                if vname.is_some() {
                    self.remove_display();
                }
                Err(LaunchError::new(e))
            }
        }
    }

    fn retunes(&self, v: &VideoCfg) -> bool {
        self.own_encoder(v)
    }

    fn running(&self) -> bool {
        let r = self.run.lock().unwrap();
        if let Some(a) = &r.attached {
            // while it runs and, on a display of its own, while a window of it is up there
            return desk::alive(a.process)
                && r.follower.as_ref().is_none_or(|f| f.windows() > 0 || (!f.ever() && f.age() < LAUNCHER_GRACE));
        }
        let Some(app) = r.app.as_ref() else { return false };
        if app.running() {
            return true;
        }
        // its job empty: a window of it up still runs it (an app already running took
        // the launch, a broker started the real one), and a launcher that's gone has a
        // moment to show one
        r.follower.as_ref().is_some_and(|f| f.windows() > 0 || (!f.ever() && f.age() < LAUNCHER_GRACE))
    }

    fn terminate(&self) {
        let (app, attached, vd, follower) = {
            let mut r = self.run.lock().unwrap();
            (r.app.take(), r.attached.take(), r.vdisplay.take(), r.follower.take())
        };
        if let Some(a) = attached {
            // not started here: let go, its windows put back, still running
            drop(follower);
            info!("let go of pid {}: it runs on", a.pid);
            if vd.is_some() {
                self.remove_display();
            }
            return;
        }
        if let Some(f) = &follower {
            // windows from outside the job asked to close, as the job's are
            if f.close_adopted() > 0 {
                let t0 = Instant::now();
                while f.adopted_left() > 0 && t0.elapsed() < Duration::from_secs(5) {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        // stopped before the job it reads is closed
        drop(follower);
        if let Some(a) = app {
            a.terminate(Duration::from_secs(5));
        }
        if vd.is_some() {
            self.remove_display();
        }
    }

    fn features(&self) -> Vec<&'static str> {
        // an app attached to is closed by its windows; a launched one is ended with its job
        if self.run.lock().unwrap().attached.is_some() { vec!["close"] } else { Vec::new() }
    }

    fn close(&self) -> bool {
        let pid = self.run.lock().unwrap().attached.as_ref().map(|a| a.pid);
        pid.is_some_and(|p| desk::close_windows(p) > 0)
    }

    fn desk_features(&self) -> Vec<&'static str> {
        vec!["processes", "attach", "delete"]
    }

    fn processes(&self, all: bool) -> Result<Vec<crate::desk::Proc>, String> {
        desk::processes(all)
    }

    fn attach(&self, pid: u32, started: Option<u64>, shell: bool, l: &Launch) -> Result<(), LaunchError> {
        let process = desk::take(pid, started, shell).map_err(LaunchError::new)?;
        let a = Attached { pid, process };
        self.terminate();
        let vname = if l.virtual_display { self.add_display(l.w, l.h, l.fps) } else { None };
        if display::find(vname.as_deref()).is_none() {
            if vname.is_some() {
                self.remove_display();
            }
            return Err(LaunchError { code: Some(503), error: "no display or encoder to capture with".into() });
        }
        // on a display of its own its windows are moved there and fill it, as a launched app's
        let follower = vname.clone().map(|n| wm::Follower::attach(pid, n));
        *self.run.lock().unwrap() = Running { app: None, attached: Some(a), vdisplay: vname, follower };
        Ok(())
    }

    fn end(&self, e: &crate::desk::End) -> Result<crate::desk::Ended, String> {
        desk::end(e)
    }

    fn delete(&self, path: &str, permanent: bool) -> Result<bool, String> {
        desk::delete(path, permanent, &self.conf.delete_roots)
    }

    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: &mut dyn Sink) -> Result<(), String> {
        com_init();
        let _mmcss = Mmcss::capture();
        // its bitrate as retuned since, kept across a capture set up again
        let mut v = *v;
        let mut name = self.run.lock().unwrap().vdisplay.clone();
        // the virtual display follows the viewer's window: the app redraws at its size
        if let Some(n) = name.clone() {
            name = self.fit_display(&n, v.w, v.h, v.fps);
        }
        let mut fails = 0u32;
        // set up again after each loss (a mode change, a desktop switch) until stopped
        while !ctl.stop.load(Ordering::Relaxed) {
            match self.stream(&mut v, ctl, &mut *out, name.as_deref()) {
                Ok(false) => return Ok(()),
                Ok(true) => fails = 0,
                Err(e) => {
                    fails += 1;
                    // a display coming up after a mode change takes a moment; past that it's gone
                    if fails > 20 {
                        return Err(e);
                    }
                    debug!("{e}: again");
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
        Ok(())
    }
}
