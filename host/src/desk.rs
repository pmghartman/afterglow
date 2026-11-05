//! The host's desktop as a driver may see and act on it (docs/protocol.md, "The desktop"):
//! the console user's processes and their windows, a process closed or killed, a file
//! the desktop shows deleted. The rules are here, apart from any platform, so they're
//! tested on their own: which processes are never ended, which paths may be deleted.
//! The platforms do the work, as the user (Windows: their token; Linux: afterglow runs
//! as them), so the rules are a guard rail and the user's own rights the wall.

use serde_json::{Value, json};

/// a process's visible top-level windows, topmost first: (id, title)
pub type Windows = Vec<(u64, String)>;

/// one of the user's processes, and its windows on the desktop (or the app's display)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// its executable's full path; empty when it can't be read
    pub exe: String,
    /// its executable's file name (Linux: its command name when the path can't be read)
    pub name: String,
    /// when it started, milliseconds since the Unix epoch: with the pid, which process it is
    pub started: u64,
    pub windows: Windows,
}

impl Proc {
    pub fn json(&self) -> Value {
        let windows: Vec<Value> = self.windows.iter().map(|(id, t)| json!([id, t])).collect();
        json!({
            "pid": self.pid,
            "ppid": self.ppid,
            "exe": self.exe,
            "name": self.name,
            "started": self.started,
            "windows": windows,
        })
    }
}

/// a list as it goes out: processes with windows first, in the order their windows were
/// found (topmost first), then the rest by name; without `all`, only those with windows
pub fn order(mut list: Vec<Proc>, all: bool) -> Vec<Proc> {
    if !all {
        list.retain(|p| !p.windows.is_empty());
    }
    // stable: windowed ones keep the platform's order
    list.sort_by(|a, b| match (a.windows.is_empty(), b.windows.is_empty()) {
        (false, false) => std::cmp::Ordering::Equal,
        (x, y) if x != y => x.cmp(&y),
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.pid.cmp(&b.pid)),
    });
    list
}

/// what a driver asks to end
#[derive(Debug, Clone, Default, PartialEq)]
pub struct End {
    pub pid: u32,
    /// the start time a process list gave: a process by this pid that started at
    /// another time isn't the one meant, and is left alone
    pub started: Option<u64>,
    /// killed at once, rather than asked to close
    pub kill: bool,
    /// the desktop shell (explorer.exe, gnome-shell) may be ended too
    pub shell: bool,
    /// how long to wait for it to go, in ms
    pub wait_ms: u64,
}

impl End {
    pub fn of(j: &Value) -> Option<End> {
        let pid = j.get("pid").and_then(Value::as_u64).filter(|&p| p > 0 && p <= u32::MAX as u64)? as u32;
        let kill = j.get("kill").and_then(Value::as_bool).unwrap_or(false);
        let def = if kill { 2000 } else { 5000 };
        Some(End {
            pid,
            started: j.get("started").and_then(Value::as_u64),
            kill,
            shell: j.get("shell").and_then(Value::as_bool).unwrap_or(false),
            wait_ms: j.get("wait_ms").and_then(Value::as_u64).unwrap_or(def).min(30_000),
        })
    }
}

/// how an end went
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    /// "closed": its windows asked to close; "terminated": asked to end (Linux SIGTERM,
    /// it had no window to close); "killed": ended at once
    pub how: &'static str,
    /// whether it had gone by the end of the wait
    pub gone: bool,
}

/// processes that keep the session up: never ended, never attached
const CRITICAL: &[&str] = &[
    // Windows
    "csrss",
    "winlogon",
    "lsass",
    "lsaiso",
    "dwm",
    "smss",
    "services",
    "wininit",
    "fontdrvhost",
    "logonui",
    "sihost",
    "svchost",
    "userinit",
    "afterglow",
    "afterglowsvc",
    // Linux
    "init",
    "systemd",
    "sshd",
    "sshd-session",
    "sshd-auth",
    "dbus-daemon",
    "dbus-broker",
    "dbus-run-session",
    "xorg",
    "xwayland",
    "xvfb",
    "gnome-session-binary",
    "gnome-session",
    "ksmserver",
    "xfce4-session",
    "lxsession",
    "gdm",
    "sddm",
    "lightdm",
];

/// the desktop shell: ended or attached only when asked for by name (`shell`)
const SHELL: &[&str] = &[
    "explorer",
    "shellexperiencehost",
    "startmenuexperiencehost",
    "gnome-shell",
    "plasmashell",
    "kwin_x11",
    "kwin_wayland",
    "mutter",
    "xfwm4",
    "xfce4-panel",
    "xfdesktop",
    "cinnamon",
    "marco",
    "mate-panel",
    "openbox",
];

/// a process's name as the lists name it: lower case, without `.exe`, without a path
fn bare(name: &str) -> String {
    let n = name.rsplit(['\\', '/']).next().unwrap_or(name).to_lowercase();
    n.strip_suffix(".exe").map(str::to_string).unwrap_or(n)
}

/// why this process can't be ended or attached, None when it can: one of the session's
/// own (csrss, winlogon, lsass, dwm, sshd, the X server), afterglow itself (`own`: its
/// pid and, on Linux, those it runs under, so the link isn't cut by its own hand), or
/// the desktop shell unless `shell`
pub fn refuse(name: &str, pid: u32, own: &[u32], shell: bool) -> Option<String> {
    let b = bare(name);
    if own.contains(&pid) {
        return Some("that's afterglow itself, or what it runs under".into());
    }
    if pid <= 4 || CRITICAL.contains(&b.as_str()) {
        return Some(format!("{name} keeps the session running: it's never ended here"));
    }
    if SHELL.contains(&b.as_str()) && !shell {
        return Some(format!("{name} is the desktop shell: ask with shell to end it"));
    }
    None
}

/// how paths are spelled
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Unix,
    /// case-insensitive, `\` or `/`, a drive or a UNC share in front, `\\?\` dropped
    Windows,
}

/// an absolute path's root and its parts, as compared; None for a relative path or one
/// with `.`, `..`, a NUL or a control character, or (Windows) a `:` past the drive
fn parts(p: &str, style: Style) -> Option<(String, Vec<String>)> {
    if p.is_empty() || p.chars().any(char::is_control) {
        return None;
    }
    let (root, rest) = match style {
        Style::Unix => (String::from("/"), p.strip_prefix('/')?),
        Style::Windows => {
            let p = p.replace('/', "\\");
            let p = if let Some(r) = p.strip_prefix("\\\\?\\UNC\\") {
                format!("\\\\{r}")
            } else if let Some(r) = p.strip_prefix("\\\\?\\") {
                r.to_string()
            } else {
                p
            };
            if let Some(unc) = p.strip_prefix("\\\\") {
                // \\server\share is the root
                let mut it = unc.splitn(3, '\\');
                let (server, share) = (it.next()?, it.next()?);
                if server.is_empty() || share.is_empty() || server == "." || server == "?" {
                    return None;
                }
                let rest = it.next().unwrap_or("").to_string();
                return finish(format!("\\\\{server}\\{share}").to_lowercase(), &rest, style);
            }
            let b = p.as_bytes();
            if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
                return None;
            }
            return finish(p[..2].to_lowercase(), &p[3..], style);
        }
    };
    finish(root, rest, style)
}

fn finish(root: String, rest: &str, style: Style) -> Option<(String, Vec<String>)> {
    let mut out = Vec::new();
    for c in rest.split(if style == Style::Windows { '\\' } else { '/' }) {
        if c.is_empty() {
            continue;
        }
        if c == "." || c == ".." || (style == Style::Windows && c.contains(':')) {
            return None;
        }
        out.push(if style == Style::Windows { c.to_lowercase() } else { c.to_string() });
    }
    Some((root, out))
}

/// path is root, or under it
fn under(path: &(String, Vec<String>), root: &(String, Vec<String>)) -> bool {
    path.0 == root.0 && path.1.len() >= root.1.len() && path.1[..root.1.len()] == root.1[..]
}

/// whether path may be deleted: absolute, plain (no `.` or `..`), under one of roots and
/// none of them itself (a profile, a desktop folder, a configured root). Both are
/// expected canonical already (links resolved), as the platform makes them
pub fn may_delete(path: &str, roots: &[String], style: Style) -> Result<(), String> {
    let p = parts(path, style).ok_or_else(|| format!("{path} isn't a plain absolute path"))?;
    let roots: Vec<_> = roots.iter().filter_map(|r| parts(r, style)).collect();
    if roots.iter().any(|r| r == &p || under(r, &p)) {
        return Err(format!("{path} is one of the folders deletes are kept within, or holds one"));
    }
    if !roots.iter().any(|r| under(&p, r)) {
        return Err(format!("{path} is outside the user's folders: only what's in them may be deleted here"));
    }
    Ok(())
}

/// extra roots deletes may reach, from the config's `delete_roots` (or
/// AFTERGLOW_DELETE_ROOTS): a list as PATH is, `;` between on Windows, `:` on Linux
pub fn roots_of(list: &str) -> Vec<String> {
    std::env::split_paths(list).filter(|p| p.is_absolute()).map(|p| p.to_string_lossy().into_owned()).collect()
}

/// a FILETIME (100 ns ticks since 1601) as ms since the Unix epoch
pub fn filetime_ms(ticks: u64) -> u64 {
    ticks.saturating_sub(116_444_736_000_000_000) / 10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, name: &str, windows: &[(u64, &str)]) -> Proc {
        Proc {
            pid,
            name: name.into(),
            windows: windows.iter().map(|(i, t)| (*i, t.to_string())).collect(),
            ..Proc::default()
        }
    }

    #[test]
    fn windowed_first_in_their_order_then_by_name() {
        let l = vec![p(9, "zsh", &[]), p(3, "Code.exe", &[(7, "x")]), p(4, "bash", &[]), p(2, "app", &[(1, "y")])];
        let all: Vec<u32> = order(l.clone(), true).iter().map(|p| p.pid).collect();
        assert_eq!(all, [3, 2, 4, 9]);
        let shown: Vec<u32> = order(l, false).iter().map(|p| p.pid).collect();
        assert_eq!(shown, [3, 2]);
        let j = p(3, "Code.exe", &[(7, "x")]).json();
        assert_eq!(j["windows"], json!([[7, "x"]]));
        assert_eq!(j["name"], "Code.exe");
    }

    #[test]
    fn the_sessions_own_processes_afterglow_and_the_shell_refused() {
        for n in ["csrss.exe", "WINLOGON.EXE", "lsass.exe", "dwm.exe", "C:\\Windows\\System32\\svchost.exe", "Xorg"] {
            assert!(refuse(n, 1000, &[], false).is_some(), "{n}");
            assert!(refuse(n, 1000, &[], true).is_some(), "{n} with shell");
        }
        assert!(refuse("afterglow.exe", 1000, &[], true).is_some());
        assert!(refuse("whatever", 77, &[77], true).unwrap().contains("afterglow"));
        assert!(refuse("explorer.exe", 1000, &[], false).unwrap().contains("shell"));
        assert!(refuse("explorer.exe", 1000, &[], true).is_none());
        assert!(refuse("gnome-shell", 1000, &[], false).is_some());
        assert!(refuse("notepad.exe", 1000, &[], false).is_none());
        assert!(refuse("xterm", 1000, &[], false).is_none());
        // the idle and system processes
        assert!(refuse("System", 4, &[], true).is_some());
    }

    #[test]
    fn an_end_read_with_its_defaults() {
        assert_eq!(End::of(&json!({"pid": 0})), None);
        assert_eq!(End::of(&json!({"pid": -3})), None);
        assert_eq!(End::of(&json!({})), None);
        let e = End::of(&json!({"pid": 42})).unwrap();
        assert_eq!((e.kill, e.shell, e.wait_ms, e.started), (false, false, 5000, None));
        let e = End::of(&json!({"pid": 42, "kill": true, "shell": true, "started": 5, "wait_ms": 99_999})).unwrap();
        assert_eq!((e.kill, e.shell, e.wait_ms, e.started), (true, true, 30_000, Some(5)));
        assert_eq!(End::of(&json!({"pid": 42, "kill": true})).unwrap().wait_ms, 2000);
    }

    #[test]
    fn deletes_kept_within_the_users_folders_on_linux() {
        let roots = vec!["/home/me".to_string(), "/srv/share".to_string()];
        let u = Style::Unix;
        assert!(may_delete("/home/me/Desktop/a.txt", &roots, u).is_ok());
        assert!(may_delete("/home/me/x", &roots, u).is_ok());
        assert!(may_delete("/srv/share/y/z", &roots, u).is_ok());
        // a root itself, or what holds one
        assert!(may_delete("/home/me", &roots, u).is_err());
        assert!(may_delete("/home/me/", &roots, u).is_err());
        assert!(may_delete("/home", &roots, u).is_err());
        assert!(may_delete("/", &roots, u).is_err());
        // outside, a sibling sharing the prefix, climbing out, relative
        assert!(may_delete("/etc/passwd", &roots, u).is_err());
        assert!(may_delete("/home/mel/a", &roots, u).is_err());
        assert!(may_delete("/home/me/../other/a", &roots, u).is_err());
        assert!(may_delete("/home/me/./a", &roots, u).is_err());
        assert!(may_delete("home/me/a", &roots, u).is_err());
        assert!(may_delete("/home/me/a\nb", &roots, u).is_err());
        assert!(may_delete("", &roots, u).is_err());
        // case matters on Linux
        assert!(may_delete("/HOME/me/a", &roots, u).is_err());
    }

    #[test]
    fn deletes_kept_within_the_users_folders_on_windows() {
        let roots = vec![
            "C:\\Users\\Me".to_string(),
            "\\\\?\\D:\\Desktop".to_string(),
            "C:\\Users\\Public\\Desktop".to_string(),
            "\\\\nas\\home\\me".to_string(),
        ];
        let w = Style::Windows;
        assert!(may_delete("C:\\Users\\Me\\Desktop\\App.lnk", &roots, w).is_ok());
        assert!(may_delete("c:/users/me/desktop/app.lnk", &roots, w).is_ok());
        assert!(may_delete("\\\\?\\C:\\Users\\Me\\Documents\\x.docx", &roots, w).is_ok());
        assert!(may_delete("D:\\Desktop\\notes.txt", &roots, w).is_ok());
        assert!(may_delete("C:\\Users\\Public\\Desktop\\Shared.lnk", &roots, w).is_ok());
        assert!(may_delete("\\\\NAS\\Home\\me\\f", &roots, w).is_ok());
        assert!(may_delete("\\\\?\\UNC\\nas\\home\\me\\f", &roots, w).is_ok());
        // the roots themselves and what holds them
        assert!(may_delete("C:\\Users\\Me", &roots, w).is_err());
        assert!(may_delete("D:\\Desktop\\", &roots, w).is_err());
        assert!(may_delete("C:\\Users", &roots, w).is_err());
        assert!(may_delete("C:\\", &roots, w).is_err());
        // outside
        assert!(may_delete("C:\\Windows\\System32\\drivers\\etc\\hosts", &roots, w).is_err());
        assert!(may_delete("C:\\Users\\Public\\Documents\\x", &roots, w).is_err());
        assert!(may_delete("C:\\Users\\Mel\\x", &roots, w).is_err());
        assert!(may_delete("E:\\Users\\Me\\x", &roots, w).is_err());
        assert!(may_delete("\\\\nas\\other\\me\\f", &roots, w).is_err());
        // climbing out, a stream, relative, a device
        assert!(may_delete("C:\\Users\\Me\\..\\Other\\x", &roots, w).is_err());
        assert!(may_delete("C:\\Users\\Me\\x.txt:hidden", &roots, w).is_err());
        assert!(may_delete("Users\\Me\\x", &roots, w).is_err());
        assert!(may_delete("C:Users\\Me\\x", &roots, w).is_err());
        assert!(may_delete("\\\\.\\C:\\Users\\Me\\x", &roots, w).is_err());
    }

    #[test]
    fn start_times_from_filetimes() {
        // 2024-01-01T00:00:00Z
        assert_eq!(filetime_ms(133_485_408_000_000_000), 1_704_067_200_000);
        assert_eq!(filetime_ms(0), 0);
    }
}
