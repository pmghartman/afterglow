//! What the link asks of a platform: start and watch one app, capture and encode its
//! display, inject the viewer's input.

use std::sync::atomic::AtomicBool;

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
}

/// a running video's controls, raised by the link
#[derive(Default)]
pub struct Ctl {
    pub stop: AtomicBool,
    /// the next frame a keyframe
    pub idr: AtomicBool,
}

/// one encoded access unit, Annex B, and whether it's a keyframe. False back: stop
pub type Out<'a> = &'a mut dyn FnMut(&[u8], bool) -> bool;

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
    /// capture and encode until ctl.stop is raised or out says stop (Ok), or the
    /// capture fails (Err, why). Runs on its own thread; the first frame a keyframe
    fn video(&self, v: &VideoCfg, ctl: &Ctl, out: Out) -> Result<(), String>;
}
