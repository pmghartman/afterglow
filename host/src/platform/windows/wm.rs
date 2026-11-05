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

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::JobObjects::IsProcessInJob;
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{ITaskbarList2, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SW_RESTORE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindowAsync, WM_CLOSE, WS_CAPTION, WS_CHILD,
    WS_EX_TOOLWINDOW, WS_THICKFRAME,
};
use windows::core::{BOOL, PWSTR};

use super::display;
use super::input::sync_desktop;
use crate::{info, warn};

/// what the follower knows of the app's windows, for the host to ask
#[derive(Default)]
pub struct Seen {
    /// the app's processes: its job's, and those whose windows were taken as its
    pids: Mutex<Vec<u32>>,
    /// windows taken as the app's whose processes aren't (an older copy's)
    taken: Mutex<Vec<isize>>,
    /// the app's windows up now, and whether it ever had one
    windows: AtomicUsize,
    ever: AtomicBool,
    /// windows of processes outside the job taken as the app's
    adopted: Mutex<Vec<isize>>,
}

/// the app's windows followed on their own thread until dropped
pub struct Follower {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    seen: Arc<Seen>,
    started: Instant,
}

impl Follower {
    /// the windows of everything in job kept to the display by this GDI name
    pub fn start(job: HANDLE, display: String) -> Follower {
        let stop = Arc::new(AtomicBool::new(false));
        // a handle isn't Send: its value crosses, the job outlives the follower
        let (s, job) = (stop.clone(), job.0 as usize);
        // SAFETY: fills the struct it's given
        let launched = unsafe { GetSystemTimeAsFileTime() };
        let seen = Arc::new(Seen::default());
        let out = seen.clone();
        let thread = thread::Builder::new()
            .name("windows".into())
            .spawn(move || follow(HANDLE(job as *mut c_void), &display, &s, ticks(launched), &out))
            .ok();
        Follower { stop, thread, seen, started: Instant::now() }
    }

    /// what it knows, for another thread to read
    pub fn watch(&self) -> Arc<Seen> {
        self.seen.clone()
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

impl Seen {
    /// the app's processes as last looked at
    pub fn pids(&self) -> Vec<u32> {
        self.pids.lock().unwrap().clone()
    }

    /// windows of the app's whose processes aren't its
    pub fn taken(&self) -> Vec<isize> {
        self.taken.lock().unwrap().clone()
    }
}

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

/// the shell's taskbar told to go under a filled window, by a thread of its own that
/// holds its object for the life of afterglow: making it talks to Explorer, which can
/// keep the caller waiting (from SYSTEM it did, for good), and placing the app's windows
/// mustn't wait on it
fn under_taskbar(h: HWND) {
    use std::sync::OnceLock;
    use std::sync::mpsc::{Sender, channel};
    static TX: OnceLock<Mutex<Sender<isize>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<isize>();
        let _ = thread::Builder::new().name("taskbar".into()).spawn(move || {
            sync_desktop();
            // SAFETY: COM for this thread alone, for as long as afterglow runs; the shell
            // object made, initialised and used on it
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let t: Option<ITaskbarList2> = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)
                    .ok()
                    .filter(|t: &ITaskbarList2| t.HrInit().is_ok());
                if t.is_none() {
                    warn!("the taskbar can't be told to go under the app's windows");
                }
                for h in rx {
                    if let Some(t) = &t {
                        let _ = t.MarkFullscreenWindow(HWND(h as *mut c_void), true);
                    }
                }
            }
        });
        Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(h.0 as isize);
}

fn follow(job: HANDLE, display: &str, stop: &AtomicBool, launched: u64, out: &Seen) {
    // onto the app's desktop: its windows are enumerated there
    sync_desktop();
    info!("following the app's windows onto {display}");
    let t0 = Instant::now();
    // windows already placed once, so the app has the keyboard as each comes
    let mut seen: HashSet<isize> = HashSet::new();
    // processes outside the job taken as the app's, and those looked at and not
    let mut adopted: HashSet<u32> = HashSet::new();
    let mut looked: HashMap<u32, bool> = HashMap::new();
    // the desktop's windows when the app started, and windows (not their processes)
    // taken as the app's since: one an app already running opened for the launch
    let before: HashSet<isize> = top_windows().into_iter().map(|w| w.0.0 as isize).collect();
    let mut taken: HashSet<isize> = HashSet::new();
    let (mut own_shown, mut said_none, mut said_lost) = (false, false, false);
    while !stop.load(Ordering::Relaxed) {
        match display::rect_of(display) {
            Some(mon) => {
                said_lost = false;
                let early = t0.elapsed() < ADOPT_FOR && !own_shown;
                let mut strangers = Vec::new();
                for (h, kind, pid) in top_windows() {
                    let hv = h.0 as isize;
                    // a new window of a process one of whose windows was taken: its dialog
                    let kin = !before.contains(&hv)
                        && taken.iter().any(|&t| {
                            let mut p = 0u32;
                            // SAFETY: a query on a handle that may have gone, which only fails
                            unsafe { GetWindowThreadProcessId(HWND(t as *mut c_void), Some(&mut p)) };
                            p == pid
                        });
                    let ours = if in_job(h, job) {
                        own_shown = true;
                        true
                    } else if adopted.contains(&pid) || taken.contains(&hv) || kin {
                        taken.insert(hv);
                        true
                    } else {
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
                            } else if !before.contains(&hv)
                                && process_of(pid).is_some_and(|(exe, _)| !SHELL.contains(&exe.as_str()))
                            {
                                // an older process, but a window new since the launch: the
                                // launch handed to a copy already running. That window
                                // alone, never the process's others
                                info!("window {hv:#x} (pid {pid}), new since the launch, taken as the app's");
                                taken.insert(hv);
                            }
                        }
                        if !adopted.contains(&pid) && !taken.contains(&hv) && !said_none {
                            strangers.push(pid);
                        }
                        adopted.contains(&pid) || taken.contains(&hv)
                    };
                    if ours {
                        let first = seen.insert(h.0 as isize);
                        if first && (adopted.contains(&pid) || taken.contains(&hv)) {
                            out.adopted.lock().unwrap().push(h.0 as isize);
                        }
                        place(h, kind, mon, first);
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
        out.windows.store(seen.len(), Ordering::Relaxed);
        let mut pids = super::process::pids_of(job);
        pids.extend(adopted.iter().copied());
        *out.pids.lock().unwrap() = pids;
        // SAFETY: IsWindow takes any value
        taken.retain(|&h| unsafe { IsWindow(Some(HWND(h as *mut c_void))) }.as_bool());
        *out.taken.lock().unwrap() = taken.iter().copied().collect();
        if !seen.is_empty() {
            out.ever.store(true, Ordering::Relaxed);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

unsafe extern "system" fn collect(h: HWND, l: LPARAM) -> BOOL {
    // SAFETY: l is the Vec app_windows passes, alive for the call
    let v = unsafe { &mut *(l.0 as *mut Vec<HWND>) };
    v.push(h);
    true.into()
}

/// the desktop's visible top-level windows that are someone's main window or dialog,
/// what would be done with each, and its process
fn top_windows() -> Vec<(HWND, Kind, u32)> {
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: collect only pushes to the Vec whose address it's given
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize));
    }
    let mut out = Vec::new();
    for h in all {
        // SAFETY: queries on a window handle; a window gone since answers false or fails
        unsafe {
            if !IsWindowVisible(h).as_bool() {
                continue;
            }
            let style = GetWindowLongW(h, GWL_STYLE) as u32;
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            // menus, tooltips and other tool windows follow their owner
            if style & WS_CHILD.0 != 0 || ex & WS_EX_TOOLWINDOW.0 != 0 {
                continue;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, 4).is_ok()
                && cloaked != 0
            {
                continue;
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
                continue;
            };
            out.push((h, kind, pid));
        }
    }
    out
}

fn in_job(h: HWND, job: HANDLE) -> bool {
    let mut pid = 0u32;
    // SAFETY: a process opened for a query and closed
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
        let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut r = BOOL(0);
        let ok = IsProcessInJob(p, Some(job), &mut r).is_ok() && r.as_bool();
        let _ = CloseHandle(p);
        ok
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

fn place(h: HWND, kind: Kind, mon: RECT, first: bool) {
    let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
    // SAFETY: window calls on a handle that may have gone, which only fail; async ones
    // never wait on a hung app
    unsafe {
        if kind == Kind::Fill && (IsZoomed(h).as_bool() || IsIconic(h).as_bool()) {
            // maximized stops at the taskbar, minimized can't be got back without one:
            // restored, and filled on the next pass
            let _ = ShowWindowAsync(h, SW_RESTORE);
            return;
        }
        let mut wr = RECT::default();
        if GetWindowRect(h, &mut wr).is_err() {
            return;
        }
        let fb = frame(h, wr);
        match kind {
            Kind::Fill => {
                if fb != mon {
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
                    under_taskbar(h);
                }
            }
            Kind::Centre => {
                let (cx, cy) = ((fb.left + fb.right) / 2, (fb.top + fb.bottom) / 2);
                if cx < mon.left || cx >= mon.right || cy < mon.top || cy >= mon.bottom {
                    let x = mon.left + ((mon.right - mon.left) - (wr.right - wr.left)) / 2;
                    let y = mon.top + ((mon.bottom - mon.top) - (wr.bottom - wr.top)) / 2;
                    let _ = SetWindowPos(h, None, x.max(mon.left), y.max(mon.top), 0, 0, flags | SWP_NOSIZE);
                    if first {
                        info!("window {:#x} centred on the display", h.0 as usize);
                    }
                }
            }
        }
        if first {
            // each new window gets the keyboard, as the Linux host's window manager gives it
            let _ = SetForegroundWindow(h);
        }
    }
}
