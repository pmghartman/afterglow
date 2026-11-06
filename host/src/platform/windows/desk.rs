//! The console user's desktop for a driver (docs/protocol.md, "The desktop"): their
//! processes and windows listed, one closed or killed, a file deleted to their Recycle
//! Bin. Under the service afterglow is SYSTEM, so everything here is done with the
//! console user's rights: their processes picked out by their token's SID and their
//! session, opened to be ended while impersonating them (what they couldn't end, this
//! can't), and files deleted by a copy of afterglow started as them (`afterglow
//! --delete`), so the Recycle Bin is theirs and only what they could delete can be.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::mem::size_of;
use std::path::{Component, Path};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Security::{
    GetLengthSid, GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_ACCESS_RIGHTS,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING,
    FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FOLDERID_Desktop, FOLDERID_PublicDesktop, FileOperation,
    GetUserProfileDirectoryW, IFileOperation, IShellItem, KNOWN_FOLDER_FLAG, SHCreateItemFromParsingName,
    SHGetKnownFolderPath, SHQUERYRBINFO, SHQueryRecycleBinW,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
use windows::core::{PCWSTR, PWSTR};

use super::input::user_desktop;
use super::{from_wide, process, wide, wm};
use crate::desk::{self, End, Ended, Proc, Style, Windows};
use crate::warn;

/// the console user: their SID and session, a token of theirs to query (and, when
/// afterglow is SYSTEM, to act as)
struct User {
    sid: Vec<u8>,
    session: u32,
    token: HANDLE,
    /// afterglow is SYSTEM: the token is the console user's, put on to act
    impersonate: bool,
}

impl Drop for User {
    fn drop(&mut self) {
        // SAFETY: the token this owns
        unsafe {
            let _ = CloseHandle(self.token);
        }
    }
}

fn user() -> Result<User, String> {
    // SAFETY: tokens owned by the User made, or closed here
    unsafe {
        let (token, session, impersonate) = if process::running_as_system() {
            (process::user_token()?, WTSGetActiveConsoleSessionId(), true)
        } else {
            let mut t = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t).map_err(|e| e.to_string())?;
            let mut s = 0u32;
            let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut s);
            (t, s, false)
        };
        let mut u = User { sid: Vec::new(), session, token, impersonate };
        u.sid = sid_of(token).ok_or("the console user's account can't be read")?;
        Ok(u)
    }
}

/// a token's user SID, as bytes
fn sid_of(token: HANDLE) -> Option<Vec<u8>> {
    // SAFETY: the buffer is sized as GetTokenInformation asks; the SID read within it
    unsafe {
        let mut len = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), len, &mut len).ok()?;
        let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
        let n = GetLengthSid(sid) as usize;
        Some(std::slice::from_raw_parts(sid.0 as *const u8, n).to_vec())
    }
}

impl User {
    /// whether pid is one of this user's processes, in their session
    fn owns(&self, pid: u32) -> bool {
        // SAFETY: a process and its token opened for queries and closed
        unsafe {
            let mut s = u32::MAX;
            if ProcessIdToSessionId(pid, &mut s).is_err() || s != self.session {
                return false;
            }
            let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
            let mut t = HANDLE::default();
            let ok = OpenProcessToken(p, TOKEN_QUERY, &mut t).is_ok();
            let _ = CloseHandle(p);
            if !ok {
                return false;
            }
            let sid = sid_of(t);
            let _ = CloseHandle(t);
            sid.as_deref() == Some(&self.sid[..])
        }
    }

    /// a process opened as the user would open it: their rights decide what's allowed
    fn open(&self, access: PROCESS_ACCESS_RIGHTS, pid: u32) -> Result<HANDLE, String> {
        // SAFETY: impersonation on this thread alone, reverted before returning
        unsafe {
            if self.impersonate {
                ImpersonateLoggedOnUser(self.token).map_err(|e| format!("can't act as the console user: {e}"))?;
            }
            let r = OpenProcess(access, false, pid);
            if self.impersonate {
                let _ = RevertToSelf();
            }
            r.map_err(|e| format!("the console user can't do that to pid {pid}: {e}"))
        }
    }
}

/// a process as the snapshot has it
struct Entry {
    pid: u32,
    ppid: u32,
    name: String,
}

fn snapshot() -> Vec<Entry> {
    let mut out = Vec::new();
    // SAFETY: the snapshot is closed after; the entry is sized as its dwSize says
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            out.push(Entry { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, name: from_wide(&e.szExeFile) });
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// a process's executable's full path and when it started (ms since the Unix epoch)
fn image(pid: u32) -> (String, u64) {
    // SAFETY: a process opened for queries and closed; buffers sized as passed
    unsafe {
        let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return (String::new(), 0) };
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let started = if GetProcessTimes(p, &mut c, &mut e, &mut k, &mut u).is_ok() {
            desk::filetime_ms(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
        } else {
            0
        };
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let path = if QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).is_ok() {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            String::new()
        };
        let _ = CloseHandle(p);
        (path, started)
    }
}

fn proc_of(e: &Entry, windows: Windows) -> Proc {
    let (exe, started) = image(e.pid);
    Proc { pid: e.pid, ppid: e.ppid, exe, name: e.name.clone(), started, windows }
}

/// the console user's processes, those with windows first (topmost first)
pub fn processes(all: bool) -> Result<Vec<Proc>, String> {
    let u = user()?;
    let snap = snapshot();
    let by_pid: HashMap<u32, &Entry> = snap.iter().map(|e| (e.pid, e)).collect();
    let mut order: Vec<u32> = Vec::new();
    let mut wins: HashMap<u32, Windows> = HashMap::new();
    for (h, pid, title) in wm::listed() {
        wins.entry(pid)
            .or_insert_with(|| {
                order.push(pid);
                Vec::new()
            })
            .push((h.0 as usize as u64, title));
    }
    let windowed: std::collections::HashSet<u32> = order.iter().copied().collect();
    let mut list = Vec::new();
    for pid in order {
        if let (Some(e), true) = (by_pid.get(&pid), u.owns(pid)) {
            list.push(proc_of(e, wins.remove(&pid).unwrap_or_default()));
        }
    }
    if all {
        for e in snap.iter().filter(|e| !windowed.contains(&e.pid)) {
            if u.owns(e.pid) {
                list.push(proc_of(e, Vec::new()));
            }
        }
    }
    Ok(desk::order(list, all))
}

/// a process a driver named, checked: the console user's, the one meant (its start time),
/// not one never ended or attached. Its name back
fn check(u: &User, pid: u32, started: Option<u64>, shell: bool) -> Result<String, String> {
    let snap = snapshot();
    let e = snap.iter().find(|e| e.pid == pid).ok_or_else(|| format!("there's no process {pid}"))?;
    if !u.owns(pid) {
        return Err(format!("pid {pid} isn't one of the console user's processes"));
    }
    if let Some(s) = started {
        if image(pid).1 != s {
            return Err(format!("pid {pid} is another process now: list them again"));
        }
    }
    // SAFETY: no arguments
    let me = unsafe { GetCurrentProcessId() };
    let parent = snap.iter().find(|e| e.pid == me).map_or(0, |e| e.ppid);
    if let Some(why) = desk::refuse(&e.name, pid, &[me, parent], shell) {
        return Err(why);
    }
    Ok(e.name.clone())
}

/// waited for the process to end, up to ms; whether it has
fn wait(h: HANDLE, ms: u64) -> bool {
    // SAFETY: a process handle opened with SYNCHRONIZE
    unsafe { WaitForSingleObject(h, ms as u32) == WAIT_OBJECT_0 }
}

/// WM_CLOSE to pid's windows on the user's desktop, as their close buttons would; how many
pub fn close_windows(pid: u32) -> usize {
    // the windows' desktop (the user's, whatever has the input), so the messages reach them
    let _ = user_desktop();
    let mut n = 0;
    for (h, p, _) in wm::listed() {
        // SAFETY: a message posted to a window that may have gone, which only fails
        if p == pid && unsafe { PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok() {
            n += 1;
        }
    }
    n
}

/// one of the console user's processes asked to close, or killed
pub fn end(e: &End) -> Result<Ended, String> {
    let u = user()?;
    let name = check(&u, e.pid, e.started, e.shell)?;
    if e.kill {
        let h = u.open(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, e.pid)?;
        // SAFETY: a handle opened for this, closed after
        unsafe {
            let r = TerminateProcess(h, 1);
            let gone = r.is_ok() && wait(h, e.wait_ms);
            let _ = CloseHandle(h);
            r.map_err(|err| format!("{name} wasn't ended: {err}"))?;
            return Ok(Ended { how: "killed", gone });
        }
    }
    // to wait on it: as the user, as the close is theirs
    let h = u.open(PROCESS_SYNCHRONIZE, e.pid).ok();
    let asked = close_windows(e.pid);
    let gone = asked > 0 && h.is_some_and(|h| wait(h, e.wait_ms));
    if let Some(h) = h {
        // SAFETY: the handle opened above
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    if asked == 0 {
        return Err(format!("{name} has no window to close: ask to kill it"));
    }
    Ok(Ended { how: "closed", gone })
}

/// a process to attach to, checked as an end is: one of the user's with a window up, not
/// the session's own or afterglow, nor the shell unless asked. A handle to wait on back
pub fn take(pid: u32, started: Option<u64>, shell: bool) -> Result<HANDLE, String> {
    let u = user()?;
    let name = check(&u, pid, started, shell)?;
    if !wm::listed().iter().any(|w| w.1 == pid) {
        return Err(format!("{name} has no window to show"));
    }
    u.open(PROCESS_SYNCHRONIZE, pid)
}

/// whether a process (opened with SYNCHRONIZE) still runs
pub fn alive(h: HANDLE) -> bool {
    !wait(h, 0)
}

/// a known folder of the user's (their token) or everyone's (none)
fn known(id: &windows::core::GUID, token: Option<HANDLE>) -> Option<String> {
    // SAFETY: the string the shell allocated is read and freed
    unsafe {
        let p = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), token).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// a path as Windows resolves it (links, junctions, short names), without `\\?\`
fn canonical(p: &Path) -> Option<String> {
    let c = std::fs::canonicalize(p).ok()?.to_string_lossy().into_owned();
    Some(match c.strip_prefix("\\\\?\\UNC\\") {
        Some(r) => format!("\\\\{r}"),
        None => c.strip_prefix("\\\\?\\").map(str::to_string).unwrap_or(c),
    })
}

/// where deletes may reach: the user's profile, their desktop (wherever it's redirected),
/// the public desktop, and the config's delete_roots
fn roots(u: &User, extra: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    // SAFETY: buf sized as n says
    if unsafe { GetUserProfileDirectoryW(u.token, Some(PWSTR(buf.as_mut_ptr())), &mut n) }.is_ok() {
        out.push(from_wide(&buf));
    }
    out.extend(known(&FOLDERID_Desktop, Some(u.token)));
    out.extend(known(&FOLDERID_PublicDesktop, None));
    out.extend(extra.iter().cloned());
    out.into_iter().map(|r| canonical(Path::new(&r)).unwrap_or(r)).collect()
}

/// a file or folder within the user's folders deleted, to their Recycle Bin unless
/// permanent: true back when recycled
pub fn delete(path: &str, permanent: bool, extra: &[String]) -> Result<bool, String> {
    let u = user()?;
    let p = Path::new(path);
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(format!("{path} isn't a plain absolute path"));
    }
    std::fs::symlink_metadata(p).map_err(|_| format!("{path} isn't there"))?;
    // the folder it's in resolved, the item itself not: a link is deleted, not what it names
    let (dir, name) = (p.parent(), p.file_name());
    let (Some(dir), Some(name)) = (dir, name) else { return Err(format!("{path} can't be deleted")) };
    let dir = canonical(dir).ok_or_else(|| format!("{path}'s folder can't be read"))?;
    let real = format!("{}\\{}", dir.trim_end_matches('\\'), name.to_string_lossy());
    desk::may_delete(&real, &roots(&u, extra), Style::Windows)?;
    if !u.impersonate {
        // afterglow is the user: here, on a thread of its own for the shell's apartment
        return std::thread::spawn(move || delete_here(&real, permanent))
            .join()
            .unwrap_or_else(|_| Err("the delete failed".into()));
    }
    drop(u);
    by_helper(&real, permanent)
}

/// the delete done by `afterglow --delete` started as the console user
fn by_helper(path: &str, permanent: bool) -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let (process, mut to, mut from) = process::spawn_piped(&format!("\"{}\" --delete", exe.display()))?;
    let ask = serde_json::json!({"path": path, "permanent": permanent}).to_string();
    let _ = to.write_all(ask.as_bytes());
    drop(to);
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new().name("delete".into()).spawn(move || {
        let mut s = String::new();
        let _ = from.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    // long enough for a big folder going to the bin, or the user answering the shell's
    // question about one too big for it
    let got = rx.recv_timeout(Duration::from_secs(120));
    // SAFETY: the helper's process handle, ours to end and close
    unsafe {
        if got.is_err() {
            let _ = TerminateProcess(process, 1);
        }
        let _ = CloseHandle(process);
    }
    let got = got.map_err(|_| "the delete didn't finish in 2 minutes".to_string())?;
    let j: serde_json::Value =
        serde_json::from_str(got.trim()).map_err(|_| "the delete helper said nothing".to_string())?;
    if j["ok"] == true {
        Ok(j["recycled"] == true)
    } else {
        Err(j["error"].as_str().unwrap_or("the delete failed").to_string())
    }
}

/// `afterglow --delete`: `{path, permanent}` on stdin, done as whoever runs it, `{ok,
/// recycled}` or `{ok: false, error}` on stdout. What afterglow starts as the console user
pub fn delete_main() -> i32 {
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    let r = match serde_json::from_str::<serde_json::Value>(s.trim()) {
        Ok(j) => {
            let path = j["path"].as_str().unwrap_or("").to_string();
            let permanent = j["permanent"].as_bool().unwrap_or(false);
            match delete_here(&path, permanent) {
                Ok(recycled) => serde_json::json!({"ok": true, "recycled": recycled}),
                Err(e) => serde_json::json!({"ok": false, "error": e}),
            }
        }
        Err(e) => serde_json::json!({"ok": false, "error": format!("not a delete: {e}")}),
    };
    println!("{r}");
    0
}

/// the drive or share a path is on, as SHQueryRecycleBinW takes it
fn volume_of(path: &str) -> String {
    if let Some(unc) = path.strip_prefix("\\\\") {
        let mut it = unc.splitn(3, '\\');
        let (a, b) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        return format!("\\\\{a}\\{b}\\");
    }
    path.get(..3).unwrap_or(path).to_string()
}

/// the shell's own delete (IFileOperation), as Explorer's: to the Recycle Bin unless
/// permanent; never a dialog but the shell's warning before something too big for the
/// bin would be gone for good. On a thread with no apartment yet
fn delete_here(path: &str, permanent: bool) -> Result<bool, String> {
    if path.is_empty() {
        return Err("no path".into());
    }
    // SAFETY: COM for this thread, balanced below; the shell objects live within it
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let r = (|| -> Result<bool, String> {
            if !permanent {
                // a drive with no bin would have it gone for good: not without asking
                let vol = wide(&volume_of(path));
                let mut info = SHQUERYRBINFO { cbSize: size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
                SHQueryRecycleBinW(PCWSTR(vol.as_ptr()), &mut info)
                    .map_err(|_| "there's no Recycle Bin where it is: ask for a permanent delete".to_string())?;
            }
            let op: IFileOperation =
                CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(|e| format!("no shell delete: {e}"))?;
            let mut flags = FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_NOCONFIRMMKDIR | FOFX_EARLYFAILURE;
            if !permanent {
                flags = flags | FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOF_WANTNUKEWARNING;
            }
            op.SetOperationFlags(flags).map_err(|e| e.to_string())?;
            let w = wide(path);
            let item: IShellItem =
                SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).map_err(|e| format!("{path}: {e}"))?;
            op.DeleteItem(&item, None).map_err(|e| e.to_string())?;
            op.PerformOperations().map_err(|e| format!("{path} wasn't deleted: {e}"))?;
            if op.GetAnyOperationsAborted().is_ok_and(|a| a.as_bool()) {
                return Err(format!("{path} wasn't deleted: the delete was cancelled"));
            }
            Ok(!permanent)
        })();
        if com {
            CoUninitialize();
        }
        if let Err(e) = &r {
            warn!("{e}");
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::volume_of;

    #[test]
    fn the_volume_a_path_is_on() {
        assert_eq!(volume_of("C:\\Users\\me\\x"), "C:\\");
        assert_eq!(volume_of("\\\\nas\\home\\me\\x"), "\\\\nas\\home\\");
    }
}
