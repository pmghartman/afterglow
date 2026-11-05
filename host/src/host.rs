//! What the link asks of a platform: start and watch one app, capture and encode its
//! display, inject the viewer's input.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::input::Inject;

#[derive(Debug, Clone)]
pub struct Launch {
    /// the app's path or shortcut
    pub cmd: String,
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    /// a display of its own at w×h, where the platform has one to give
    pub virtual_display: bool,
}

#[derive(Debug)]
pub struct LaunchError {
    /// 503: nothing to capture with
    pub code: Option<u32>,
    pub error: String,
}

impl LaunchError {
    pub fn new(error: impl Into<String>) -> LaunchError {
        LaunchError { code: None, error: error.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoCfg {
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    pub kbps: u32,
    /// asked for; H.264 4:4:4 isn't something every encoder makes, so it may be 4:2:0
    pub yuv444: bool,
    /// the pointer drawn into the frames. Otherwise its shape goes to the sink as it
    /// changes, for the viewer to draw where its own pointer is: no frame for a move,
    /// no latency on it
    pub draw_cursor: bool,
}

/// a running video's controls, raised by the link
#[derive(Default)]
pub struct Ctl {
    pub stop: AtomicBool,
    /// the next frame a keyframe
    pub idr: AtomicBool,
    /// what wakes the capture from its wait, set by the platform that waits
    wake: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Ctl {
    pub fn on_wake(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.wake.lock().unwrap() = Some(Box::new(f));
    }

    /// the capture woken to look at stop and idr now, not at its next timeout
    pub fn kick(&self) {
        if let Some(f) = self.wake.lock().unwrap().as_ref() {
            f();
        }
    }

    pub fn ask_idr(&self) {
        self.idr.store(true, Ordering::Relaxed);
        self.kick();
    }

    pub fn ask_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.kick();
    }
}

/// the pointer as the display shows it: straight-alpha RGBA rows, its hotspot
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    pub visible: bool,
    pub w: u32,
    pub h: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub rgba: Vec<u8>,
}

/// where a video goes
pub trait Sink {
    /// one access unit: its NAL units without start codes, whether it's a keyframe.
    /// False back: stop
    fn frame(&mut self, nals: &[&[u8]], key: bool) -> bool;
    /// the pointer's new shape, without draw_cursor
    fn cursor(&mut self, c: &Cursor) -> bool;
}

pub trait Host: Inject + Send + Sync + 'static {
    fn hostname(&self) -> String;
    /// whether an app can have a virtual display of its own here
    fn virtual_display(&self) -> bool;
    /// the app started, any app before it ended first
    fn launch(&self, l: &Launch) -> Result<(), LaunchError>;
    /// whether the launched app (or anything it started) still runs
    fn running(&self) -> bool;
    /// the app asked to close, then made to; its virtual display removed
    fn terminate(&self);
    /// capture and encode until ctl.stop is raised or the sink says stop (Ok), or the
    /// capture fails (Err, why). Runs on its own thread; the first frame a keyframe
    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: &mut dyn Sink) -> Result<(), String>;
}
