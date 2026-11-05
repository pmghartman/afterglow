//! The Windows host: apps started in the console's session, each on a SudoVDA display
//! of its own where the driver is installed, captured with DXGI, encoded by Media
//! Foundation, driven with SendInput.

mod capture;
mod display;
mod encode;
mod grab;
mod input;
mod process;
pub mod svc;
mod vdisplay;
mod wgc;
mod wm;

/// `afterglow --grab`: the app's windows captured as the signed-in user (grab.rs)
pub fn grab_main() -> i32 {
    grab::run()
}

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

struct Running {
    app: Option<process::App>,
    /// the virtual display's GDI name, while the app has one
    vdisplay: Option<String>,
    /// the app's windows kept to that display
    follower: Option<wm::Follower>,
}

pub struct Windows {
    conf: Conf,
    vda: Mutex<Option<vdisplay::Vda>>,
    run: Mutex<Running>,
    input: input::WinInput,
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
        let vda = vdisplay::Vda::open().map_err(|e| warn!("no virtual displays: {e}")).ok();
        Ok(Arc::new(Windows {
            conf: conf.clone(),
            vda: Mutex::new(vda),
            run: Mutex::new(Running { app: None, vdisplay: None, follower: None }),
            input: input::WinInput::default(),
        }))
    }

    /// the app's own windows captured (grab.rs) rather than its display, unless Media
    /// Foundation's encoder is asked for
    fn app_capture(&self) -> bool {
        !matches!(self.conf.encoder.as_str(), "hardware" | "mf" | "software")
    }

    fn vda_ok(&self) -> bool {
        let mut v = self.vda.lock().unwrap();
        if !v.as_ref().is_some_and(|d| d.ok()) {
            // the driver installed or restarted since: try it again
            *v = vdisplay::Vda::open().ok();
        }
        v.as_ref().is_some_and(|d| d.ok())
    }

    fn add_display(&self, l: &Launch) -> Option<String> {
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
        let millihz = l.fps * 1000;
        match vda.add(DISPLAY_GUID, l.w, l.h, millihz, "Phosphorus", DISPLAY_SERIAL) {
            Ok(name) => {
                info!("virtual display {name}: {}x{}@{}", l.w, l.h, l.fps);
                display::set_mode(&name, l.w, l.h, millihz);
                // the app's windows are captured on their own: the user's own monitor and
                // taskbar stay where they are
                if self.conf.primary && !self.app_capture() {
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

    fn remove_display(&self) {
        if let Some(v) = self.vda.lock().unwrap().as_ref() {
            if v.remove(DISPLAY_GUID) {
                info!("virtual display removed");
            }
        }
    }

    /// capture and encode one display until stopped or lost
    fn stream(&self, v: &VideoCfg, ctl: &Ctl, out: &mut dyn Sink, name: Option<&str>) -> Result<bool, String> {
        let Some(o) = display::find(name) else { return Err("no display to capture".into()) };
        *self.input.rect.lock().unwrap() = input::Rect {
            left: o.rect.left,
            top: o.rect.top,
            w: o.rect.right - o.rect.left,
            h: o.rect.bottom - o.rect.top,
        };
        let (dev, ctx) =
            capture::device_for(&o).map_err(|e| format!("no Direct3D device on {}: {e}", o.adapter_name))?;
        // afterglow's own encoder unless Media Foundation's is asked for, or the pointer is
        // drawn in (the GPU composes it)
        let own = !matches!(self.conf.encoder.as_str(), "hardware" | "mf" | "software") && !v.draw_cursor;
        let mut dup = match capture::Dup::new(&o, &dev, &ctx, v.draw_cursor, own) {
            Ok(d) => d,
            Err(capture::Lost::Again) => return Ok(true),
            Err(capture::Lost::Wait(why)) => {
                debug!("{why}");
                thread::sleep(Duration::from_millis(200));
                return Ok(true);
            }
            Err(capture::Lost::Fail(why)) => return Err(why),
        };
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
                    debug!("{why}");
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
    fn stream_own(&self, v: &VideoCfg, ctl: &Ctl, out: &mut dyn Sink, dup: &mut capture::Dup) -> Result<bool, String> {
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
                    debug!("{why}");
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
            last = now;
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
            if !d.frame {
                continue;
            }
            sent = true;
            stats.au(d.bytes);
            if !out.frame(&enc.nals(), d.key) {
                return Ok(false);
            }
            if change {
                changed_at = now;
            }
            if refine {
                next_refine = now + period;
            }
            coarse = enc.coarse(crate::avc::QP_FINE) > 0;
            stats.tick(0);
        }
        Ok(false)
    }
}

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
        let vname = if l.virtual_display { self.add_display(l) } else { None };
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
                *self.run.lock().unwrap() = Running { app: Some(app), vdisplay: vname, follower };
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

    fn running(&self) -> bool {
        let r = self.run.lock().unwrap();
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
        let (app, vd, follower) = {
            let mut r = self.run.lock().unwrap();
            (r.app.take(), r.vdisplay.take(), r.follower.take())
        };
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

    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: &mut dyn Sink) -> Result<(), String> {
        com_init();
        let _mmcss = Mmcss::capture();
        let name = self.run.lock().unwrap().vdisplay.clone();
        // the virtual display follows the viewer's window: the app redraws at its size
        if let Some(n) = &name {
            display::set_mode(n, v.w, v.h, v.fps * 1000);
        }
        // the app alone: its windows captured as the signed-in user, nothing else
        let seen = self.run.lock().unwrap().follower.as_ref().map(|f| f.watch());
        if let (true, false, Some(seen), Some(rect)) =
            (self.app_capture(), v.draw_cursor, seen, name.as_deref().and_then(display::rect_of))
        {
            match grab::Helper::start() {
                Ok(h) => match h.video(v, ctl, &mut *out, rect, || seen.pids()) {
                    Ok(()) => return Ok(()),
                    Err(e) => warn!("the app's windows couldn't be captured ({e}): its display is, instead"),
                },
                Err(e) => warn!("no capture process ({e}): the app's display is captured instead"),
            }
        }
        let mut fails = 0u32;
        // set up again after each loss (a mode change, a desktop switch) until stopped
        while !ctl.stop.load(Ordering::Relaxed) {
            match self.stream(v, ctl, &mut *out, name.as_deref()) {
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
