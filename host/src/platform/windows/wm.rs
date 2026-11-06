//! The app's windows kept to its virtual display, as the Linux host's window manager
//! keeps them to its Xvfb: a main window fills the display, over the taskbar, and a
//! dialog or a fixed-size window is centred on it, so the video shows the app and not
//! the desktop around it. Windows are looked for every 100ms; nothing is hooked.
//!
//! The app's windows are those of its job's processes. An app can show its window from
//! outside the job, though: one already running takes the launch and opens it, a broker
//! starts the real process. So until a window of the job's own has shown, for the first
//! seconds after the launch, a window of a process started since then is taken as the
//! app's too; and what was seen (or wasn't) is logged, so a launch that shows nothing
//! on its display says why.
//!
//! The follower works on the user's own desktop (Default) whatever has the input: a
//! launch while the screen is locked, or a UAC prompt up, still finds the app's windows.
//!
//! An app attached to (a process already running, docs/protocol.md "The desktop") is
//! followed by its pid instead, and when it's let go its windows are put back where
//! they were before it was attached.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_ENUMERATE, DESKTOP_READOBJECTS, EnumDesktopWindows, OpenDesktopW,
};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{ITaskbarList2, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetWindow, GetWindowLongW, GetWindowPlacement, GetWindowRect,
    GetWindowThreadProcessId, InternalGetWindowText, IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW,
    SW_RESTORE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPlacement,
    SetWindowPos, ShowWindowAsync, WINDOWPLACEMENT, WM_CLOSE, WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW, WS_THICKFRAME,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use super::display;
use super::input::user_desktop;
use crate::{debug, info, warn};

/// what the follower knows of the app's windows, for the host to ask
#[derive(Default)]
struct Seen {
    /// the app's windows up now, and whether it ever had one
    windows: AtomicUsize,
    ever: AtomicBool,
    /// windows of processes outside the job taken as the app's
    adopted: Mutex<Vec<isize>>,
}

/// whose windows are the app's
enum Owner {
    /// everything in a job (a handle's value: the job outlives the follower)
    Job(usize),
    /// a process attached to, not started here
    Pid(u32),
}

/// the app's windows followed on their own thread until dropped
pub struct Follower {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    seen: Arc<Seen>,
    started: Instant,
    /// the display's GDI name, which changes when it's added again at another size
    display: Arc<Mutex<String>>,
}

impl Follower {
    /// the windows of everything in job kept to the display by this GDI name; `before`
    /// the desktop's windows from before the launch (snapshot())
    pub fn start(job: HANDLE, display: String, before: HashSet<isize>) -> Follower {
        // a handle isn't Send: its value crosses, the job outlives the follower
        Self::spawn(Owner::Job(job.0 as usize), display, before)
    }

    /// the windows of a process attached to kept to the display; put back where they
    /// were when the follower is dropped
    pub fn attach(pid: u32, display: String) -> Follower {
        Self::spawn(Owner::Pid(pid), display, HashSet::new())
    }

    fn spawn(owner: Owner, display: String, before: HashSet<isize>) -> Follower {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        // SAFETY: fills the struct it's given
        let launched = unsafe { GetSystemTimeAsFileTime() };
        let seen = Arc::new(Seen::default());
        let out = seen.clone();
        let display = Arc::new(Mutex::new(display));
        let d = display.clone();
        let thread = thread::Builder::new()
            .name("windows".into())
            .spawn(move || follow(&owner, &d, &s, ticks(launched), &before, &out))
            .ok();
        Follower { stop, thread, seen, started: Instant::now(), display }
    }

    /// the windows kept to this display from now on (the app's, added again)
    pub fn set_display(&self, name: &str) {
        *self.display.lock().unwrap() = name.to_string();
    }

    /// the app's windows up now
    pub fn windows(&self) -> usize {
        self.seen.windows.load(Ordering::Relaxed)
    }

    /// whether a window of the app's has ever shown
    pub fn ever(&self) -> bool {
        self.seen.ever.load(Ordering::Relaxed)
    }

    /// since the launch
    pub fn age(&self) -> Duration {
        self.started.elapsed()
    }

    /// the windows taken from outside the job asked to close, as their close button
    /// would: their processes weren't started by afterglow, so they're never ended. How
    /// many were asked
    pub fn close_adopted(&self) -> usize {
        let hs = self.seen.adopted.lock().unwrap().clone();
        let mut n = 0;
        for h in hs {
            let h = HWND(h as *mut c_void);
            // SAFETY: a message posted to a window that may have gone, which only fails
            unsafe {
                if IsWindow(Some(h)).as_bool() && PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() {
                    n += 1;
                }
            }
        }
        n
    }

    /// adopted windows still up
    pub fn adopted_left(&self) -> usize {
        let hs = self.seen.adopted.lock().unwrap();
        // SAFETY: IsWindow takes any value
        hs.iter().filter(|&&h| unsafe { IsWindow(Some(HWND(h as *mut c_void))) }.as_bool()).count()
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// what's done with a window
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// a main window: fills the display
    Fill,
    /// a dialog, a fixed-size window, a splash: centred when it's off the display
    Centre,
}

/// how long after the launch a window from outside the job can be taken as the app's,
/// while none of the job's own has shown; and when to say that none has
const ADOPT_FOR: Duration = Duration::from_secs(15);
const NONE_YET: Duration = Duration::from_secs(3);

/// processes of the shell, never taken for the app
const SHELL: [&str; 10] = [
    "explorer.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "searchhost.exe",
    "searchapp.exe",
    "textinputhost.exe",
    "lockapp.exe",
    "applicationframehost.exe",
    "afterglow.exe",
    "conhost.exe",
];

fn ticks(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// a process's executable's file name, lower case, and when it started (FILETIME ticks)
fn process_of(pid: u32) -> Option<(String, u64)> {
    // SAFETY: a process opened for queries and closed; buffers sized as passed
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let times = GetProcessTimes(p, &mut c, &mut e, &mut k, &mut u).is_ok();
        let mut buf = [0u16; 512];
        let mut n = buf.len() as u32;
        let named = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).is_ok();
        let _ = CloseHandle(p);
        let path = if named { String::from_utf16_lossy(&buf[..n as usize]) } else { String::new() };
        let file = path.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase();
        times.then_some((file, ticks(c)))
    }
}

/// how often a window is asked to its place in a row, the display and the window as they
/// were each time: past that it keeps where it is (a minimum size bigger than the
/// display, a window that places itself) until either changes
const ASKS: u8 = 3;

fn follow(
    owner: &Owner,
    shared: &Mutex<String>,
    stop: &AtomicBool,
    launched: u64,
    before: &HashSet<isize>,
    out: &Seen,
) {
    // onto the app's desktop before COM: a thread with a window of its own can't change desktops
    if let Err(e) = user_desktop() {
        warn!("the window follower can't reach the user's desktop: {e}");
    }
    // SAFETY: balanced below; the taskbar's object lives in an apartment
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    // SAFETY: a shell object created and initialised on this thread
    let taskbar: Option<ITaskbarList2> = unsafe {
        CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok().filter(|t: &ITaskbarList2| t.HrInit().is_ok())
    };
    let t0 = Instant::now();
    // windows already placed once, so the app has the keyboard as each comes
    let mut seen: HashSet<isize> = HashSet::new();
    // processes outside the job taken as the app's, and those looked at and not
    let mut adopted: HashSet<u32> = HashSet::new();
    let mut looked: HashMap<u32, bool> = HashMap::new();
    let (mut own_shown, mut said_none, mut said_lost) = (false, false, false);
    // an attached app's windows as they were before they were first placed, and those
    // told to the taskbar as fullscreen: both undone when it's let go
    let mut placed: Vec<(isize, WINDOWPLACEMENT)> = Vec::new();
    let mut marked: Vec<isize> = Vec::new();
    let mut asked: HashMap<isize, Asked> = HashMap::new();
    let mut display = String::new();
    // processes already running whose window new since the launch (not in `before`) was
    // taken as the app's: an app that's single-instance (Notepad, a browser) opens the
    // launch's window in its older process, and only that window and its new ones after
    // are the app's
    let mut took: HashSet<u32> = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        let now = shared.lock().unwrap().clone();
        if now != display {
            if !display.is_empty() {
                info!("the app's windows follow its display to {now}");
            }
            display = now;
            said_lost = false;
            asked.clear();
        }
        match display::rect_of(&display) {
            Some(mon) => {
                said_lost = false;
                let early = t0.elapsed() < ADOPT_FOR && !own_shown;
                let mut strangers = Vec::new();
                // the job's processes, once a pass rather than a process opened a window
                let in_job: HashSet<u32> = match owner {
                    Owner::Job(job) => super::process::job_pids(HANDLE(*job as *mut c_void)).into_iter().collect(),
                    Owner::Pid(_) => HashSet::new(),
                };
                for (h, kind, pid) in top_windows() {
                    let ours = match owner {
                        Owner::Pid(p) => {
                            own_shown |= *p == pid;
                            *p == pid
                        }
                        Owner::Job(_) if in_job.contains(&pid) => {
                            own_shown = true;
                            true
                        }
                        Owner::Job(_) if adopted.contains(&pid) => true,
                        Owner::Job(_) if took.contains(&pid) && !before.contains(&(h.0 as isize)) => true,
                        Owner::Job(_) => {
                            let fresh = !before.contains(&(h.0 as isize));
                            if early && fresh && !looked.get(&pid).copied().unwrap_or(false) {
                                // a window new since the launch, of a process older than it
                                if let Some((exe, _)) = process_of(pid).filter(|(e, _)| !SHELL.contains(&e.as_str())) {
                                    info!(
                                        "a window of {exe} (pid {pid}), running already, new since the launch: taken as the app's"
                                    );
                                    took.insert(pid);
                                }
                            }
                            if early {
                                // started since the launch, and not the shell's: the app's
                                let take = *looked.entry(pid).or_insert_with(|| {
                                    process_of(pid).is_some_and(|(exe, started)| {
                                        let take = started + 20_000_000 >= launched && !SHELL.contains(&exe.as_str());
                                        if take {
                                            info!(
                                                "windows of {exe} (pid {pid}), started since the launch, taken as the app's"
                                            );
                                        }
                                        take
                                    })
                                });
                                if take {
                                    adopted.insert(pid);
                                }
                            }
                            let mine = adopted.contains(&pid) || (fresh && took.contains(&pid));
                            if !mine && !said_none {
                                strangers.push(pid);
                            }
                            mine
                        }
                    };
                    if ours {
                        let first = seen.insert(h.0 as isize);
                        // from outside the job: closed by its window when the app is, never ended
                        if first && matches!(owner, Owner::Job(_)) && !in_job.contains(&pid) {
                            out.adopted.lock().unwrap().push(h.0 as isize);
                        }
                        if first && matches!(owner, Owner::Pid(_)) {
                            let mut wp =
                                WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                            // SAFETY: wp sized as its length says
                            if unsafe { GetWindowPlacement(h, &mut wp) }.is_ok() {
                                placed.push((h.0 as isize, wp));
                            }
                        }
                        let a = asked.entry(h.0 as isize).or_default();
                        if place(h, kind, mon, first, taskbar.as_ref(), a) {
                            marked.push(h.0 as isize);
                        }
                    }
                }
                if seen.is_empty() && !said_none && t0.elapsed() >= NONE_YET {
                    said_none = true;
                    let mut others: Vec<String> = strangers
                        .iter()
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .filter_map(|&p| process_of(p).map(|(exe, _)| format!("{exe} ({p})")))
                        .collect();
                    others.sort();
                    others.truncate(12);
                    warn!(
                        "no window of the app's after {}s; windows on the desktop not its: {}",
                        NONE_YET.as_secs(),
                        if others.is_empty() { "none".into() } else { others.join(", ") }
                    );
                }
            }
            None if !said_lost => {
                said_lost = true;
                warn!("{display} isn't found among the displays: the app's windows are left where they are");
            }
            None => {}
        }
        // SAFETY: IsWindow takes any value
        seen.retain(|&h| unsafe { IsWindow(Some(HWND(h as *mut c_void))) }.as_bool());
        asked.retain(|h, _| seen.contains(h));
        out.windows.store(seen.len(), Ordering::Relaxed);
        if !seen.is_empty() {
            out.ever.store(true, Ordering::Relaxed);
        }
        thread::sleep(Duration::from_millis(100));
    }
    for h in marked {
        let h = HWND(h as *mut c_void);
        // SAFETY: a window that may have gone, which only fails
        if let (Some(t), true) = (taskbar.as_ref(), unsafe { IsWindow(Some(h)) }.as_bool()) {
            let _ = unsafe { t.MarkFullscreenWindow(h, false) };
        }
    }
    // an attached app's windows back where they were: it runs on, on the console's display
    for (h, wp) in placed {
        let h = HWND(h as *mut c_void);
        // SAFETY: as above; the placement is the one this window had
        unsafe {
            if IsWindow(Some(h)).as_bool() {
                let _ = SetWindowPlacement(h, &wp);
            }
        }
    }
    drop(taskbar);
    if com {
        // SAFETY: the CoInitializeEx above
        unsafe { CoUninitialize() };
    }
}

unsafe extern "system" fn collect(h: HWND, l: LPARAM) -> BOOL {
    // SAFETY: l is the Vec app_windows passes, alive for the call
    let v = unsafe { &mut *(l.0 as *mut Vec<HWND>) };
    v.push(h);
    true.into()
}

/// every top-level window on the user's desktop (Default) as it stands, shown or not: what
/// was there before a launch
pub fn snapshot() -> HashSet<isize> {
    let mut all: Vec<HWND> = Vec::new();
    let name = super::wide("Default");
    // SAFETY: collect only pushes to the Vec whose address it's given; the desktop is
    // closed after
    unsafe {
        let l = LPARAM(&mut all as *mut Vec<HWND> as isize);
        match OpenDesktopW(
            PCWSTR(name.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_READOBJECTS.0 | DESKTOP_ENUMERATE.0,
        ) {
            Ok(d) => {
                let _ = EnumDesktopWindows(Some(d), Some(collect), l);
                let _ = CloseDesktop(d);
            }
            Err(_) => {
                let _ = EnumWindows(Some(collect), l);
            }
        }
    }
    all.into_iter().map(|h| h.0 as isize).collect()
}

/// every top-level window on the thread's desktop, shown or not
fn all_windows() -> Vec<HWND> {
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: collect only pushes to the Vec whose address it's given
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize));
    }
    all
}

/// the desktop's visible top-level windows that are someone's main window or dialog,
/// what would be done with each, and its process
fn top_windows() -> Vec<(HWND, Kind, u32)> {
    all_windows().into_iter().filter_map(|h| shown(h).map(|(k, pid)| (h, k, pid))).collect()
}

/// a window someone would call one of an app's: visible, top-level and of some size,
/// not a tool window, not cloaked, a main window or a dialog (not a caption-less popup an app places
/// itself); what would be done with it, and its process
fn shown(h: HWND) -> Option<(Kind, u32)> {
    // SAFETY: queries on a window handle; a window gone since answers false or fails
    unsafe {
        if !IsWindowVisible(h).as_bool() {
            return None;
        }
        let style = GetWindowLongW(h, GWL_STYLE) as u32;
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        // menus, tooltips and other tool windows follow their owner
        if style & WS_CHILD.0 != 0 || ex & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4).is_ok() && cloaked != 0 {
            return None;
        }
        // "visible" and of no size: a toolkit's helper (wxWidgets makes many), not a window
        let mut r = RECT::default();
        if GetWindowRect(h, &mut r).is_err() || r.right - r.left < 2 || r.bottom - r.top < 2 {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let owned = GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid());
        let captioned = style & WS_CAPTION.0 == WS_CAPTION.0;
        let kind = if !owned && style & WS_THICKFRAME.0 != 0 {
            Kind::Fill
        } else if captioned || !owned {
            Kind::Centre
        } else {
            // an owned popup without a caption (a dropdown, a flyout): placed by the app
            return None;
        };
        Some((kind, pid))
    }
}

/// the windows on the user's desktop (Default, whichever has the input now) as the
/// follower sees them, topmost first: each with its process and title. Only reads:
/// no message is sent, so a hung window can't hold it
pub fn listed() -> Vec<(HWND, u32, String)> {
    let mut all: Vec<HWND> = Vec::new();
    let name = super::wide("Default");
    // SAFETY: collect only pushes to the Vec whose address it's given; the desktop is
    // closed after
    unsafe {
        let l = LPARAM(&mut all as *mut Vec<HWND> as isize);
        match OpenDesktopW(
            PCWSTR(name.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_READOBJECTS.0 | DESKTOP_ENUMERATE.0,
        ) {
            Ok(d) => {
                let _ = EnumDesktopWindows(Some(d), Some(collect), l);
                let _ = CloseDesktop(d);
            }
            Err(_) => {
                let _ = EnumWindows(Some(collect), l);
            }
        }
    }
    let mut out = Vec::new();
    for h in all {
        let Some((_, pid)) = shown(h) else { continue };
        let mut buf = [0u16; 512];
        // SAFETY: buf as long as passed; InternalGetWindowText reads the window's text
        // without asking its thread
        let n = unsafe { InternalGetWindowText(h, &mut buf) }.max(0) as usize;
        let title: String = String::from_utf16_lossy(&buf[..n.min(buf.len())]).chars().take(300).collect();
        out.push((h, pid, title));
    }
    out
}

/// a window's asks to be placed, in a row: the display and the window's rect at the last
#[derive(Default)]
struct Asked {
    mon: RECT,
    wr: RECT,
    n: u8,
}

impl Asked {
    /// whether to ask again: not past ASKS while nothing has changed since the last
    fn again(&mut self, mon: RECT, wr: RECT, h: HWND) -> bool {
        if self.mon == mon && self.wr == wr {
            if self.n >= ASKS {
                return false;
            }
            self.n += 1;
            if self.n == ASKS {
                debug!("window {:#x} keeps its own place: left there", h.0 as usize);
            }
        } else {
            *self = Asked { mon, wr, n: 1 };
        }
        true
    }
}

/// the window's visible bounds, without the resize borders Windows draws invisibly around it
fn frame(h: HWND, wr: RECT) -> RECT {
    let mut fb = RECT::default();
    let p = &mut fb as *mut RECT as *mut c_void;
    // SAFETY: p is fb, sized as the attribute's RECT
    let ok = unsafe { DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, p, size_of::<RECT>() as u32) }.is_ok();
    if ok { fb } else { wr }
}

/// the window placed on the display; true when it was told to the taskbar as fullscreen
fn place(h: HWND, kind: Kind, mon: RECT, first: bool, taskbar: Option<&ITaskbarList2>, asked: &mut Asked) -> bool {
    let mut marked = false;
    let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
    // SAFETY: window calls on a handle that may have gone, which only fail; async ones
    // never wait on a hung app
    unsafe {
        if kind == Kind::Fill && (IsZoomed(h).as_bool() || IsIconic(h).as_bool()) {
            // maximized stops at the taskbar, minimized can't be got back without one:
            // restored, and filled on the next pass
            let _ = ShowWindowAsync(h, SW_RESTORE);
            return false;
        }
        let mut wr = RECT::default();
        if GetWindowRect(h, &mut wr).is_err() {
            return false;
        }
        let fb = frame(h, wr);
        match kind {
            Kind::Fill => {
                if fb != mon && asked.again(mon, wr, h) {
                    // the visible frame over the whole display, the invisible borders past its edges
                    let (l, t) = (fb.left - wr.left, fb.top - wr.top);
                    let (r, b) = (wr.right - fb.right, wr.bottom - fb.bottom);
                    let (w, ht) = (mon.right - mon.left + l + r, mon.bottom - mon.top + t + b);
                    let _ = SetWindowPos(h, None, mon.left - l, mon.top - t, w, ht, flags);
                    if first {
                        info!("window {:#x} fills the display", h.0 as usize);
                    }
                }
                if first {
                    // the taskbar goes under it while it's in front
                    if let Some(t) = taskbar {
                        marked = t.MarkFullscreenWindow(h, true).is_ok();
                    }
                }
            }
            Kind::Centre => {
                let (cx, cy) = ((fb.left + fb.right) / 2, (fb.top + fb.bottom) / 2);
                let off = cx < mon.left || cx >= mon.right || cy < mon.top || cy >= mon.bottom;
                if off && asked.again(mon, wr, h) {
                    let x = mon.left + ((mon.right - mon.left) - (wr.right - wr.left)) / 2;
                    let y = mon.top + ((mon.bottom - mon.top) - (wr.bottom - wr.top)) / 2;
                    let _ = SetWindowPos(h, None, x.max(mon.left), y.max(mon.top), 0, 0, flags | SWP_NOSIZE);
                    if first {
                        debug!("window {:#x} centred on the display", h.0 as usize);
                    }
                }
            }
        }
        if first {
            // each new window gets the keyboard, as the Linux host's window manager gives it
            let _ = SetForegroundWindow(h);
        }
    }
    marked
}
