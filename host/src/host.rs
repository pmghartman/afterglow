//! What the link asks of a platform: start and watch one app, capture and encode its
//! display, inject the viewer's input.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::desk::{End, Ended, Proc};
use crate::input::Inject;

/// the account to sign in at the console when no one is, kept out of every log
#[derive(Clone)]
pub struct Login {
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Login({})", self.user)
    }
}

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
    /// Linux: the program and its arguments, in place of cmd
    pub argv: Vec<String>,
    /// the display's dots per inch, and the viewer's pixel ratio (1.5 and up: the
    /// toolkits told to scale)
    pub dpi: u32,
    pub scale: f32,
    /// Linux: a session bus of the app's own, so a single-instance app starts here
    /// rather than handing its window to a copy on the host's own screen
    pub dbus: bool,
    /// Windows: signed in at the logon screen first when no one is signed in
    pub login: Option<Login>,
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

/// a running video carried on as a new one: its gen and bitrate, the rest the same
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retune {
    pub vgen: u16,
    pub kbps: u32,
}

/// a running video's controls, raised by the link
#[derive(Default)]
pub struct Ctl {
    pub stop: AtomicBool,
    /// the next frame a keyframe
    pub idr: AtomicBool,
    /// a new gen and bitrate for the video, taken at its next frame: the sink told the
    /// gen, the rate changed, a keyframe sent (Host::retunes)
    retune: Mutex<Option<Retune>>,
    /// what wakes the capture from its wait, set by the platform that waits
    wake: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Ctl {
    pub fn ask_retune(&self, r: Retune) {
        *self.retune.lock().unwrap() = Some(r);
        self.kick();
    }

    /// a retune asked since last taken
    pub fn take_retune(&self) -> Option<Retune> {
        self.retune.lock().unwrap().take()
    }

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
    /// the frames from here on are video `vgen`'s (a retune): its config goes ahead of
    /// the next keyframe
    fn regen(&mut self, _vgen: u16) {}
}

/// what the host tells the driver besides frames: `{t: "title", s}`, `{t: "windows",
/// list: [[id, title]], main}`, `{t: "clip", s}` (the host's clipboard)
pub type Notes = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

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
    /// whether a video running as `v` takes Ctl::ask_retune, so a new video differing
    /// only in its bitrate goes on in the same capture rather than starting another
    fn retunes(&self, _v: &VideoCfg) -> bool {
        false
    }

    /// where the host's notes go, set once by the link
    fn on_notes(&self, _f: Notes) {}
    /// what the launched app can be asked besides input: some of "clipboard", "raise",
    /// "close", "notes"
    fn features(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// the viewer's clipboard made the app's, so its own paste pastes it
    fn clip(&self, _text: &str) {}
    /// one of the app's windows (an id from a windows note) brought to the front
    fn raise(&self, _id: u32) {}
    /// the app asked to close as its own close button would; false where it can't be
    /// (the driver's close then ends it)
    fn close(&self) -> bool {
        false
    }
    /// everything kept ahead of the next launch let go (a spare display): afterglow is
    /// going
    fn release(&self) {}
    /// how the app ended, once it has: `{code, log, error}`, any of them
    fn exit_info(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    /// what this host can do on its desktop besides the app (docs/protocol.md, "The
    /// desktop"): some of "processes" (listed and ended), "attach", "delete"
    fn desk_features(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// the console user's processes: with windows, topmost first; with `all`, the rest
    /// too (crate::desk::order)
    fn processes(&self, _all: bool) -> Result<Vec<Proc>, String> {
        Err(NOT_HERE.into())
    }
    /// a process already running taken as the app, its windows followed as a launched
    /// app's are (onto a display of its own with l.virtual_display), nothing started.
    /// Any app before it is let go first. terminate() then lets it go and leaves it running
    fn attach(&self, _pid: u32, _started: Option<u64>, _shell: bool, _l: &Launch) -> Result<(), LaunchError> {
        Err(LaunchError::new(NOT_HERE))
    }
    /// one of the user's processes asked to close (its windows' close), or killed. Runs
    /// on a thread of its own: it may wait up to e.wait_ms
    fn end(&self, _e: &End) -> Result<Ended, String> {
        Err(NOT_HERE.into())
    }
    /// a file or folder within the user's folders deleted: to the Recycle Bin (Linux: the
    /// trash) unless permanent. True back when it was recycled. Runs on a thread of its own
    fn delete(&self, _path: &str, _permanent: bool) -> Result<bool, String> {
        Err(NOT_HERE.into())
    }
}

/// what a host says of a desktop action it doesn't have
pub const NOT_HERE: &str = "this host can't do that";
