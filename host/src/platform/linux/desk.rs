//! The user's desktop on a Linux host for a driver (docs/protocol.md, "The desktop"):
//! their processes (afterglow runs as them, over ssh or as their own service) with
//! their windows on the app's display and the session's, one closed as its windows'
//! close buttons would (`_NET_CLOSE_WINDOW` to the window manager, or WM_DELETE_WINDOW
//! where none takes it; SIGTERM for a process with no window) or killed (SIGKILL), and
//! a file in their home deleted to the trash, as file managers do (the freedesktop.org
//! trash), or for good.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, MapState};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::xserver::Display;
use crate::desk::{self, End, Ended, Proc, Style, Windows};

/// a window on a display: its id, its client's pid, its title
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Win {
    pub id: u32,
    pub pid: Option<u32>,
    pub title: String,
}

fn atom(c: &RustConnection, n: &str) -> u32 {
    c.intern_atom(false, n.as_bytes()).ok().and_then(|r| r.reply().ok()).map_or(0, |r| r.atom)
}

fn prop32(c: &RustConnection, w: u32, p: u32, ty: impl Into<u32>, max: u32) -> Vec<u32> {
    c.get_property(false, w, p, ty.into(), 0, max)
        .ok()
        .and_then(|r| r.reply().ok())
        .and_then(|r| r.value32().map(|v| v.collect()))
        .unwrap_or_default()
}

fn title_of(c: &RustConnection, w: u32, net_name: u32, utf8: u32) -> String {
    let get = |p: u32, ty: u32| {
        c.get_property(false, w, p, ty, 0, 1024).ok().and_then(|r| r.reply().ok()).filter(|r| r.value_len > 0)
    };
    let s = match get(net_name, utf8) {
        Some(r) => String::from_utf8_lossy(&r.value).into_owned(),
        None => get(AtomEnum::WM_NAME.into(), AtomEnum::ANY.into())
            .map(|r| r.value.iter().map(|&b| b as char).collect())
            .unwrap_or_default(),
    };
    s.chars().take(300).collect()
}

/// the windows a display's window manager manages, topmost first: its _NET_CLIENT_LIST
/// (afterglow's own keeps it bottom to top), or with no EWMH window manager the root's
/// mapped children. Each one's pid from _NET_WM_PID, or the X server's own word for its
/// client (XRes) where the app set none
pub fn windows_on(d: &Display) -> Vec<Win> {
    let Ok((c, screen)) = d.connect() else { return Vec::new() };
    let root = c.setup().roots[screen].root;
    let (clients, stacking, wm_pid, net_name, utf8) = (
        atom(&c, "_NET_CLIENT_LIST"),
        atom(&c, "_NET_CLIENT_LIST_STACKING"),
        atom(&c, "_NET_WM_PID"),
        atom(&c, "_NET_WM_NAME"),
        atom(&c, "UTF8_STRING"),
    );
    let mut ids = prop32(&c, root, stacking, AtomEnum::WINDOW, 4096);
    if ids.is_empty() {
        ids = prop32(&c, root, clients, AtomEnum::WINDOW, 4096);
    }
    if ids.is_empty() {
        ids = c
            .query_tree(root)
            .ok()
            .and_then(|r| r.reply().ok())
            .map(|t| t.children)
            .unwrap_or_default()
            .into_iter()
            .filter(|&w| {
                c.get_window_attributes(w)
                    .ok()
                    .and_then(|r| r.reply().ok())
                    .is_some_and(|a| a.map_state == MapState::VIEWABLE && !a.override_redirect)
            })
            .collect();
    }
    let res = c.extension_information(x11rb::protocol::res::X11_EXTENSION_NAME).ok().flatten().is_some();
    ids.iter()
        .rev()
        .map(|&w| {
            let pid = prop32(&c, w, wm_pid, AtomEnum::CARDINAL, 1).first().copied().filter(|&p| p > 0).or_else(|| {
                if !res {
                    return None;
                }
                let spec = ClientIdSpec { client: w, mask: ClientIdMask::LOCAL_CLIENT_PID };
                let r = c.res_query_client_ids(&[spec]).ok()?.reply().ok()?;
                r.ids.first().and_then(|i| i.value.first().copied()).filter(|&p| p > 0)
            });
            Win { id: w, pid, title: title_of(&c, w, net_name, utf8) }
        })
        .collect()
}

/// windows on a display asked to close as their close buttons would: through the window
/// manager (_NET_CLOSE_WINDOW) where it takes that, else WM_DELETE_WINDOW to each that
/// takes it. How many were asked
pub fn close_on(d: &Display, wins: &[u32]) -> usize {
    let Ok((c, screen)) = d.connect() else { return 0 };
    let root = c.setup().roots[screen].root;
    let (supported, net_close, protocols, delete) = (
        atom(&c, "_NET_SUPPORTED"),
        atom(&c, "_NET_CLOSE_WINDOW"),
        atom(&c, "WM_PROTOCOLS"),
        atom(&c, "WM_DELETE_WINDOW"),
    );
    let by_wm = prop32(&c, root, supported, AtomEnum::ATOM, 1024).contains(&net_close);
    let mut n = 0;
    for &w in wins {
        let sent = if by_wm {
            // source 2: a pager, a tool acting for the user
            let e = ClientMessageEvent::new(32, w, net_close, [x11rb::CURRENT_TIME, 2, 0, 0, 0]);
            c.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, e).is_ok()
        } else if prop32(&c, w, protocols, AtomEnum::ATOM, 64).contains(&delete) {
            let e = ClientMessageEvent::new(32, w, protocols, [delete, x11rb::CURRENT_TIME, 0, 0, 0]);
            c.send_event(false, w, EventMask::NO_EVENT, e).is_ok()
        } else {
            false
        };
        n += sent as usize;
    }
    let _ = c.flush();
    let _ = c.sync();
    n
}

/// what /proc/<pid>/stat says of a process
#[derive(Debug, Clone)]
struct Stat {
    ppid: u32,
    comm: String,
    state: char,
    /// clock ticks after boot
    start: u64,
}

fn stat(pid: u32) -> Option<Stat> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // the name is in parentheses and may hold anything: the fields follow the last ')'
    let (open, close) = (s.find('(')?, s.rfind(')')?);
    let comm = s.get(open + 1..close)?.to_string();
    let f: Vec<&str> = s.get(close + 2..)?.split(' ').collect();
    Some(Stat {
        state: f.first()?.chars().next()?,
        ppid: f.get(1)?.parse().ok()?,
        start: f.get(19)?.parse().ok()?,
        comm,
    })
}

fn uid_of(pid: u32) -> Option<u32> {
    std::fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid())
}

fn me() -> u32 {
    // SAFETY: no arguments
    unsafe { libc::geteuid() }
}

/// ms since the Unix epoch of a start in clock ticks after boot
fn started_ms(ticks: u64) -> u64 {
    let btime = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse::<u64>().ok())))
        .unwrap_or(0);
    // SAFETY: a plain query
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    btime * 1000 + ticks * 1000 / hz
}

fn exe_of(pid: u32) -> String {
    std::fs::read_link(format!("/proc/{pid}/exe")).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// afterglow and what it runs under (ssh's shell, sshd): ending those cuts the link
fn own() -> Vec<u32> {
    let mut v = vec![std::process::id()];
    while let Some(s) = stat(*v.last().unwrap()) {
        if s.ppid <= 1 || v.contains(&s.ppid) {
            break;
        }
        v.push(s.ppid);
    }
    v
}

fn pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect())
        .unwrap_or_default()
}

/// the windows on these displays by their process, each process's in the order found
fn windows_by_pid(displays: &[Display]) -> (Vec<u32>, HashMap<u32, Windows>) {
    let mut order = Vec::new();
    let mut by: HashMap<u32, Windows> = HashMap::new();
    for d in displays {
        for w in windows_on(d) {
            let Some(pid) = w.pid else { continue };
            by.entry(pid)
                .or_insert_with(|| {
                    order.push(pid);
                    Vec::new()
                })
                .push((w.id as u64, w.title));
        }
    }
    (order, by)
}

/// the user's processes, those with windows on these displays first (topmost first)
pub fn processes(displays: &[Display], all: bool) -> Vec<Proc> {
    let uid = me();
    let (order, mut by) = windows_by_pid(displays);
    let windowed: HashSet<u32> = order.iter().copied().collect();
    let proc_of = |pid: u32, windows: Windows| -> Option<Proc> {
        let s = stat(pid).filter(|s| s.state != 'Z')?;
        (uid_of(pid)? == uid).then(|| Proc {
            pid,
            ppid: s.ppid,
            exe: exe_of(pid),
            name: s.comm,
            started: started_ms(s.start),
            windows,
        })
    };
    let mut list: Vec<Proc> =
        order.iter().filter_map(|&pid| proc_of(pid, by.remove(&pid).unwrap_or_default())).collect();
    if all {
        list.extend(pids().into_iter().filter(|p| !windowed.contains(p)).filter_map(|p| proc_of(p, Vec::new())));
    }
    desk::order(list, all)
}

/// gone, or another process by now
fn gone(pid: u32, start: u64) -> bool {
    stat(pid).is_none_or(|s| s.state == 'Z' || s.start != start)
}

fn wait_gone(pid: u32, start: u64, ms: u64) -> bool {
    let t0 = Instant::now();
    loop {
        if gone(pid, start) {
            return true;
        }
        if t0.elapsed() >= Duration::from_millis(ms) {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// one of the user's processes asked to close, or killed
pub fn end(displays: &[Display], e: &End) -> Result<Ended, String> {
    let pid = e.pid;
    let s = stat(pid).filter(|s| s.state != 'Z').ok_or_else(|| format!("there's no process {pid}"))?;
    if uid_of(pid) != Some(me()) {
        return Err(format!("pid {pid} isn't one of this user's processes"));
    }
    if e.started.is_some_and(|t| t != started_ms(s.start)) {
        return Err(format!("pid {pid} is another process now: list them again"));
    }
    let own = own();
    let exe = exe_of(pid);
    for name in [s.comm.as_str(), exe.rsplit('/').next().unwrap_or("")].into_iter().filter(|n| !n.is_empty()) {
        if let Some(why) = desk::refuse(name, pid, &own, e.shell) {
            return Err(why);
        }
    }
    let signal = |sig| {
        // SAFETY: a signal to one process, checked to be this user's
        if unsafe { libc::kill(pid as i32, sig) } != 0 {
            Err(format!("{} wasn't ended: {}", s.comm, std::io::Error::last_os_error()))
        } else {
            Ok(())
        }
    };
    if e.kill {
        signal(libc::SIGKILL)?;
        return Ok(Ended { how: "killed", gone: wait_gone(pid, s.start, e.wait_ms) });
    }
    let mut asked = 0;
    for d in displays {
        let wins: Vec<u32> = windows_on(d).into_iter().filter(|w| w.pid == Some(pid)).map(|w| w.id).collect();
        if !wins.is_empty() {
            asked += close_on(d, &wins);
        }
    }
    let how = if asked > 0 {
        "closed"
    } else {
        // no window to close: asked to end as a desktop's logout asks
        signal(libc::SIGTERM)?;
        "terminated"
    };
    Ok(Ended { how, gone: wait_gone(pid, s.start, e.wait_ms) })
}

/// a path for a .trashinfo's Path: percent-encoded but for what a URL path keeps
fn url_path(p: &str) -> String {
    let mut out = String::new();
    for &b in p.as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// now, local time, as a .trashinfo's DeletionDate wants it
fn local_now() -> String {
    // SAFETY: time and localtime_r fill what they're given
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// path (already checked) moved into the trash at `trash`, its .trashinfo beside, as
/// file managers do
fn to_trash(path: &Path, trash: &Path) -> Result<(), String> {
    let (files, info) = (trash.join("files"), trash.join("info"));
    for d in [trash, &files, &info] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(d)
            .map_err(|e| format!("no trash at {}: {e}", trash.display()))?;
    }
    let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "item".into());
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, x)) if !s.is_empty() => (s.to_string(), format!(".{x}")),
        _ => (base.clone(), String::new()),
    };
    for i in 1..1000 {
        let name = if i == 1 { base.clone() } else { format!("{stem}.{i}{ext}") };
        let ti = info.join(format!("{name}.trashinfo"));
        // the info file made first and alone: it claims the name
        let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(&ti) else { continue };
        if files.join(&name).symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&ti);
            continue;
        }
        let _ = write!(f, "[Trash Info]\nPath={}\nDeletionDate={}\n", url_path(&path.to_string_lossy()), local_now());
        drop(f);
        return match std::fs::rename(path, files.join(&name)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&ti);
                Err(if e.raw_os_error() == Some(libc::EXDEV) {
                    format!("{} is on another filesystem than the trash: ask for a permanent delete", path.display())
                } else {
                    format!("{} wasn't moved to the trash: {e}", path.display())
                })
            }
        };
    }
    Err("the trash has no free name for it".into())
}

/// where this user's trash is: $XDG_DATA_HOME/Trash, ~/.local/share/Trash by default
pub fn trash_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"))
        .join("Trash")
}

/// a file or folder within the user's home (or the configured roots) deleted: to the
/// trash unless permanent. True back when trashed
pub fn delete(path: &str, permanent: bool, roots: &[String], trash: &Path) -> Result<bool, String> {
    let p = Path::new(path);
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(format!("{path} isn't a plain absolute path"));
    }
    let meta = std::fs::symlink_metadata(p).map_err(|_| format!("{path} isn't there"))?;
    // the folder it's in resolved, the item itself not: a link is deleted, not what it names
    let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else { return Err(format!("{path} can't be deleted")) };
    let real = std::fs::canonicalize(dir).map_err(|_| format!("{path}'s folder can't be read"))?.join(name);
    let roots: Vec<String> = roots
        .iter()
        .map(|r| std::fs::canonicalize(r).map(|c| c.to_string_lossy().into_owned()).unwrap_or_else(|_| r.clone()))
        .collect();
    desk::may_delete(&real.to_string_lossy(), &roots, Style::Unix)?;
    if permanent {
        let r = if meta.is_dir() { std::fs::remove_dir_all(&real) } else { std::fs::remove_file(&real) };
        r.map_err(|e| format!("{path} wasn't deleted: {e}"))?;
        return Ok(false);
    }
    let trash = std::fs::canonicalize(trash).unwrap_or_else(|_| trash.to_path_buf());
    if real.starts_with(&trash) || trash.starts_with(&real) {
        return Err(format!("{path} is the trash, or in it: ask for a permanent delete"));
    }
    to_trash(&real, &trash)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::linux::xserver::{ONE_XVFB, Xvfb};
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{CreateWindowAux, PropMode, WindowClass};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("afterglow-desk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn this_users_processes_listed_one_closed_another_killed() {
        let mut quiet = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let mut stubborn = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; while :; do sleep 0.05; done"])
            .spawn()
            .unwrap();
        let (qp, sp) = (quiet.id(), stubborn.id());
        // trap set before the TERM is sent
        thread::sleep(Duration::from_millis(200));
        let all = processes(&[], true);
        let q = all.iter().find(|p| p.pid == qp).expect("the child listed");
        assert_eq!(q.name, "sleep");
        assert!(q.exe.ends_with("/sleep"), "{}", q.exe);
        assert_eq!(q.ppid, std::process::id());
        assert!(q.started > 1_600_000_000_000);
        assert!(all.iter().all(|p| uid_of(p.pid).is_none_or(|u| u == me())));
        // without all, only those with windows: none on no display
        assert!(processes(&[], false).is_empty());

        // afterglow itself, and pid 1 (not ours, or the session's)
        let me_too = End { pid: std::process::id(), wait_ms: 100, ..End::default() };
        assert!(end(&[], &me_too).unwrap_err().contains("afterglow"));
        assert!(end(&[], &End { pid: 1, wait_ms: 100, ..End::default() }).is_err());
        // a start time that isn't its own: another process by now, left alone
        let wrong = End { pid: qp, started: Some(q.started + 10_000), wait_ms: 100, ..End::default() };
        assert!(end(&[], &wrong).unwrap_err().contains("another process"));

        // no window: asked to end with SIGTERM, and it goes
        let d = end(&[], &End { pid: qp, started: Some(q.started), wait_ms: 3000, ..End::default() }).unwrap();
        assert_eq!(d, Ended { how: "terminated", gone: true });
        let _ = quiet.wait();
        // one that ignores SIGTERM stays; killed, it goes
        let d = end(&[], &End { pid: sp, wait_ms: 300, ..End::default() }).unwrap();
        assert_eq!(d, Ended { how: "terminated", gone: false });
        let d = end(&[], &End { pid: sp, kill: true, wait_ms: 3000, ..End::default() }).unwrap();
        assert_eq!(d, Ended { how: "killed", gone: true });
        let _ = stubborn.wait();
        assert!(end(&[], &End { pid: sp, wait_ms: 100, ..End::default() }).unwrap_err().contains("no process"));
    }

    #[test]
    fn deleted_to_the_trash_or_for_good_within_the_roots_only() {
        let home = tmp("home");
        let outside = tmp("outside");
        let trash = home.join(".local/share/Trash");
        let roots = vec![home.to_string_lossy().into_owned()];
        let desk = home.join("Desktop");
        std::fs::create_dir_all(desk.join("folder/inner")).unwrap();
        std::fs::write(desk.join("a b.txt"), "x").unwrap();
        std::fs::write(desk.join("folder/inner/f"), "y").unwrap();
        std::fs::write(outside.join("keep"), "z").unwrap();
        std::os::unix::fs::symlink(outside.join("keep"), desk.join("link")).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();

        // to the trash, with its info
        assert_eq!(delete(&s(&desk.join("a b.txt")), false, &roots, &trash), Ok(true));
        assert!(!desk.join("a b.txt").exists());
        assert!(trash.join("files/a b.txt").exists());
        let info = std::fs::read_to_string(trash.join("info/a b.txt.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/"), "{info}");
        assert!(info.contains("/Desktop/a%20b.txt\n"), "{info}");
        assert!(info.contains("DeletionDate=20"), "{info}");
        // the same name again: another name in the trash
        std::fs::write(desk.join("a b.txt"), "again").unwrap();
        assert_eq!(delete(&s(&desk.join("a b.txt")), false, &roots, &trash), Ok(true));
        assert_eq!(std::fs::read_to_string(trash.join("files/a b.2.txt")).unwrap(), "again");
        // a link: the link goes, what it names stays
        assert_eq!(delete(&s(&desk.join("link")), true, &roots, &trash), Ok(false));
        assert!(outside.join("keep").exists());
        // a folder for good
        assert_eq!(delete(&s(&desk.join("folder")), true, &roots, &trash), Ok(false));
        assert!(!desk.join("folder").exists());

        // refused: outside, the home itself, climbing out, not there, the trash itself
        assert!(delete(&s(&outside.join("keep")), true, &roots, &trash).unwrap_err().contains("outside"));
        assert!(delete(&s(&home), true, &roots, &trash).is_err());
        let climb = format!("{}/../{}/keep", s(&home), outside.file_name().unwrap().to_string_lossy());
        assert!(delete(&climb, true, &roots, &trash).is_err());
        assert!(delete(&s(&desk.join("nothing")), false, &roots, &trash).unwrap_err().contains("isn't there"));
        assert!(delete("relative/x", false, &roots, &trash).is_err());
        assert!(delete(&s(&trash.join("files/a b.2.txt")), false, &roots, &trash).unwrap_err().contains("trash"));
        assert!(outside.join("keep").exists());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn trash_paths_encoded() {
        assert_eq!(url_path("/home/me/a b%c.txt"), "/home/me/a%20b%25c.txt");
        assert_eq!(url_path("/é"), "/%C3%A9");
    }

    /// a window with its pid on afterglow's own display: listed by its process, closed
    /// through the window manager as its close button would
    #[test]
    fn windows_on_a_real_xvfb_listed_by_pid_and_closed_politely() {
        let _one = ONE_XVFB.lock().unwrap_or_else(|e| e.into_inner());
        if std::process::Command::new("Xvfb").arg("-help").output().is_err() {
            eprintln!("no Xvfb: skipped");
            return;
        }
        let x = Xvfb::start("Xvfb", 640, 480, 96).unwrap();
        let (c, screen) = x.display.connect().unwrap();
        let root = c.setup().roots[screen].root;
        let (wm_pid, protocols, delete_w) =
            (atom(&c, "_NET_WM_PID"), atom(&c, "WM_PROTOCOLS"), atom(&c, "WM_DELETE_WINDOW"));
        let mk = |title: &str, pid: Option<u32>| {
            let w = c.generate_id().unwrap();
            c.create_window(0, w, root, 0, 0, 200, 100, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new())
                .unwrap();
            c.change_property8(PropMode::REPLACE, w, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes()).unwrap();
            c.change_property32(PropMode::REPLACE, w, protocols, AtomEnum::ATOM, &[delete_w]).unwrap();
            if let Some(p) = pid {
                c.change_property32(PropMode::REPLACE, w, wm_pid, AtomEnum::CARDINAL, &[p]).unwrap();
            }
            c.map_window(w).unwrap();
            w
        };
        let me = std::process::id();
        let a = mk("first", Some(me));
        // no _NET_WM_PID: the server says whose it is
        let b = mk("second", None);
        c.flush().unwrap();
        let t0 = Instant::now();
        let wins = loop {
            let w = windows_on(&x.display);
            if w.len() == 2 || t0.elapsed() > Duration::from_secs(10) {
                break w;
            }
            thread::sleep(Duration::from_millis(20));
        };
        // topmost first
        assert_eq!(wins[0], Win { id: b, pid: Some(me), title: "second".into() });
        assert_eq!(wins[1], Win { id: a, pid: Some(me), title: "first".into() });
        let ps = processes(std::slice::from_ref(&x.display), false);
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].pid, me);
        assert_eq!(ps[0].windows, [(b as u64, "second".to_string()), (a as u64, "first".to_string())]);

        // closed through afterglow's window manager: the app asked, as by its close button
        assert_eq!(close_on(&x.display, &[a]), 1);
        let asked = loop {
            match c.wait_for_event().unwrap() {
                Event::ClientMessage(e) if e.type_ == protocols => break e,
                _ => {}
            }
        };
        assert_eq!((asked.window, asked.data.as_data32()[0]), (a, delete_w));
    }
}
