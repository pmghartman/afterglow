//! The app: started in the session signed in at the console, as its user when
//! afterglow runs as SYSTEM, in a job of its own so everything it starts is followed
//! and can be closed together.

use std::ffi::c_void;
use std::mem::size_of;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Security::{
    GetTokenInformation, ImpersonateLoggedOnUser, IsWellKnownSid, RevertToSelf, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinLocalSystemSid,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectBasicProcessIdList, QueryInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_ENUMERATE, DESKTOP_READOBJECTS, EnumDesktopWindows, OpenDesktopW,
};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOEXW, UpdateProcThreadAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, SendNotifyMessageW, WM_CLOSE,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use super::input::{WinInput, sync_desktop};
use super::wide;
use crate::host::Login;
use crate::input::Inject;
use crate::{info, warn};

const PROC_THREAD_ATTRIBUTE_JOB_LIST: usize = 0x2000d;

pub struct App {
    job: HANDLE,
}

// SAFETY: a job handle may be used from any thread
unsafe impl Send for App {}

/// the processes in a job
pub fn pids_of(job: HANDLE) -> Vec<u32> {
    // JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counts, then the ids as ULONG_PTRs
    let mut buf = vec![0usize; 2 + 1024];
    // SAFETY: buf is large enough for 1024 ids and is what this class of query fills
    unsafe {
        if QueryInformationJobObject(
            Some(job),
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * size_of::<usize>()) as u32,
            None,
        )
        .is_err()
        {
            return Vec::new();
        }
    }
    let n = (buf[0] >> 32) as usize; // NumberOfProcessIdsInList, the second u32
    buf[1..1 + n.min(1024)].iter().map(|&p| p as u32).collect()
}

/// a program started in the console's session on its desktop (as its user when this
/// is SYSTEM), its stdin and stdout pipes back: its process, what writes to its stdin,
/// what reads its stdout
pub fn spawn_piped(cmd: &str) -> Result<(HANDLE, std::fs::File, std::fs::File), String> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{STARTF_USESTDHANDLES, STARTUPINFOW};
    // SAFETY: every handle here is closed or handed to the Files returned
    unsafe {
        let token = if running_as_system() { Some(user_token()?) } else { None };
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let (mut in_r, mut in_w, mut out_r, mut out_w) =
            (HANDLE::default(), HANDLE::default(), HANDLE::default(), HANDLE::default());
        let close_all = |hs: &[HANDLE]| {
            for &h in hs {
                if !h.is_invalid() {
                    let _ = CloseHandle(h);
                }
            }
        };
        if let Err(e) = CreatePipe(&mut in_r, &mut in_w, Some(&sa), 0) {
            close_all(&[token.unwrap_or_default()]);
            return Err(e.to_string());
        }
        if let Err(e) = CreatePipe(&mut out_r, &mut out_w, Some(&sa), 0) {
            close_all(&[in_r, in_w, token.unwrap_or_default()]);
            return Err(e.to_string());
        }
        // our ends stay ours
        let _ = SetHandleInformation(in_w, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
        let _ = SetHandleInformation(out_r, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
        let mut desktop = wide("winsta0\\default");
        let si = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: in_r,
            hStdOutput: out_w,
            hStdError: HANDLE::default(),
            ..Default::default()
        };
        let mut cmdline = wide(cmd);
        let mut env: *mut c_void = std::ptr::null_mut();
        if let Some(t) = token {
            let _ = CreateEnvironmentBlock(&mut env, Some(t), false);
        }
        let flags = CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW;
        let mut pi = PROCESS_INFORMATION::default();
        let r = match token {
            Some(t) => CreateProcessAsUserW(
                Some(t),
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                (!env.is_null()).then_some(env as *const c_void),
                PCWSTR::null(),
                &si,
                &mut pi,
            ),
            None => CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            ),
        };
        if !env.is_null() {
            let _ = DestroyEnvironmentBlock(env);
        }
        // the child's ends are the child's now
        close_all(&[in_r, out_w, token.unwrap_or_default()]);
        if let Err(e) = r {
            close_all(&[in_w, out_r]);
            return Err(format!("{cmd} didn't start: {e}"));
        }
        let _ = CloseHandle(pi.hThread);
        Ok((pi.hProcess, std::fs::File::from_raw_handle(in_w.0 as _), std::fs::File::from_raw_handle(out_r.0 as _)))
    }
}

/// whether this process is SYSTEM: then the app is started as the console's user
pub fn running_as_system() -> bool {
    // SAFETY: token and buffer owned here, sized as GetTokenInformation asks
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut len = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), len, &mut len).is_ok();
        let _ = CloseHandle(token);
        ok && IsWellKnownSid((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, WinLocalSystemSid).as_bool()
    }
}

/// the console user's token, with their own elevation (a limited one for an admin under UAC)
fn user_token() -> Result<HANDLE, String> {
    // SAFETY: plain Win32 calls
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == u32::MAX {
            return Err("no session at the console".into());
        }
        let mut token = HANDLE::default();
        WTSQueryUserToken(session, &mut token).map_err(|e| format!("no one is signed in at the console: {e}"))?;
        Ok(token)
    }
}

/// no one signed in at the console: the logon screen woken, the password typed for the
/// account it offers and entered, then waited for. Once only, so a wrong one can't lock the account
pub fn sign_in(login: &Login) -> Result<(), String> {
    if !running_as_system() {
        return Ok(());
    }
    if let Ok(t) = user_token() {
        // SAFETY: a handle this function owns
        unsafe {
            let _ = CloseHandle(t);
        }
        return Ok(());
    }
    info!("no one is signed in: signing in at the logon screen");
    let input = WinInput::default();
    let tap = |vk: u8| {
        sync_desktop();
        input.key(vk, true);
        input.key(vk, false);
    };
    // the lock screen takes any key to show the password box
    tap(0x20);
    thread::sleep(Duration::from_millis(2000));
    sync_desktop();
    input.text(&login.password);
    thread::sleep(Duration::from_millis(300));
    tap(0x0d);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(60) {
        thread::sleep(Duration::from_millis(500));
        if let Ok(t) = user_token() {
            // SAFETY: a handle this function owns
            unsafe {
                let _ = CloseHandle(t);
            }
            // the shell and its desktop settle before the app starts on it
            thread::sleep(Duration::from_secs(3));
            info!("signed in after {} ms", t0.elapsed().as_millis());
            return Ok(());
        }
    }
    Err("the saved sign-in didn't get past the logon screen".into())
}

impl App {
    /// cmd started, cwd its working directory when given
    pub fn start(cmd: &str, cwd: Option<&str>) -> Result<App, String> {
        // SAFETY: every handle and buffer here is owned by this function or the App it returns
        unsafe {
            // the console user's token first: no one signed in, nothing made to clean up
            let token = if running_as_system() { Some(user_token()?) } else { None };
            let job = match CreateJobObjectW(None, PCWSTR::null()) {
                Ok(j) => j,
                Err(e) => {
                    if let Some(t) = token {
                        let _ = CloseHandle(t);
                    }
                    return Err(e.to_string());
                }
            };
            let fail = |e: windows::core::Error| {
                if let Some(t) = token {
                    let _ = CloseHandle(t);
                }
                let _ = CloseHandle(job);
                e.to_string()
            };
            let mut size = 0usize;
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
            let mut attr_buf = vec![0u8; size];
            let attrs = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut c_void);
            InitializeProcThreadAttributeList(Some(attrs), 1, None, &mut size).map_err(fail)?;
            let jobs = [job];
            if let Err(e) = UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST,
                Some(jobs.as_ptr() as *const c_void),
                size_of::<HANDLE>(),
                None,
                None,
            ) {
                DeleteProcThreadAttributeList(attrs);
                return Err(fail(e));
            }
            let mut desktop = wide("winsta0\\default");
            let mut si = STARTUPINFOEXW::default();
            si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            si.StartupInfo.lpDesktop = PWSTR(desktop.as_mut_ptr());
            si.lpAttributeList = attrs;
            let mut cmdline = wide(cmd);
            let cwd_w = cwd.map(wide);
            let cwd_p = cwd_w.as_ref().map_or(PCWSTR::null(), |c| PCWSTR(c.as_ptr()));
            let base = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW;
            let mut pi = PROCESS_INFORMATION::default();
            let mut r = Err(windows::core::Error::empty());
            let mut env: *mut c_void = std::ptr::null_mut();
            if let Some(t) = token {
                let _ = CreateEnvironmentBlock(&mut env, Some(t), false);
            }
            // out of afterglow's own job first; a job that allows no breakaway keeps it in
            for (attempt, flags) in [base | CREATE_BREAKAWAY_FROM_JOB, base].into_iter().enumerate() {
                r = if let Some(token) = token {
                    // as the user, so their mapped drives and profile resolve
                    let _ = ImpersonateLoggedOnUser(token);
                    let r = CreateProcessAsUserW(
                        Some(token),
                        PCWSTR::null(),
                        Some(PWSTR(cmdline.as_mut_ptr())),
                        None,
                        None,
                        false,
                        flags,
                        (!env.is_null()).then_some(env as *const c_void),
                        cwd_p,
                        &si.StartupInfo,
                        &mut pi,
                    );
                    let _ = RevertToSelf();
                    r
                } else {
                    CreateProcessW(
                        PCWSTR::null(),
                        Some(PWSTR(cmdline.as_mut_ptr())),
                        None,
                        None,
                        false,
                        flags,
                        None,
                        cwd_p,
                        &si.StartupInfo,
                        &mut pi,
                    )
                };
                if r.is_ok() {
                    if attempt > 0 {
                        // inside afterglow's job: under the service, the app ends when afterglow does
                        warn!("the app couldn't leave afterglow's job: it ends with afterglow");
                    }
                    break;
                }
            }
            if !env.is_null() {
                let _ = DestroyEnvironmentBlock(env);
            }
            if let Some(t) = token {
                let _ = CloseHandle(t);
            }
            DeleteProcThreadAttributeList(attrs);
            if let Err(e) = r {
                let _ = CloseHandle(job);
                return Err(format!("the app didn't start: {e}"));
            }
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            info!("started [{cmd}], pid {}", pi.dwProcessId);
            Ok(App { job })
        }
    }

    /// the job everything the app starts is in: open until terminate
    pub fn job(&self) -> HANDLE {
        self.job
    }

    /// whether anything in the app's job still runs
    pub fn running(&self) -> bool {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: info is the struct this class of query fills
        unsafe {
            QueryInformationJobObject(
                Some(self.job),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut c_void,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
            .is_ok()
                && info.ActiveProcesses > 0
        }
    }

    fn pids(&self) -> Vec<u32> {
        pids_of(self.job)
    }

    /// asked to close (WM_CLOSE to its windows), then made to after the wait
    pub fn terminate(self, wait: Duration) {
        let pids = self.pids();
        if !pids.is_empty() {
            unsafe extern "system" fn close(hwnd: HWND, l: LPARAM) -> BOOL {
                // SAFETY: l is the pid list passed below, alive for the EnumWindows call
                unsafe {
                    let pids = &*(l.0 as *const Vec<u32>);
                    let mut pid = 0;
                    GetWindowThreadProcessId(hwnd, Some(&mut pid));
                    if pids.contains(&pid) && IsWindowVisible(hwnd).as_bool() {
                        let _ = SendNotifyMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
                true.into()
            }
            // the app's windows are on the user's desktop, whichever has the input now
            let name = wide("Default");
            // SAFETY: the callback reads pids only during this call; the desktop is closed after
            unsafe {
                let l = LPARAM(&pids as *const Vec<u32> as isize);
                match OpenDesktopW(
                    PCWSTR(name.as_ptr()),
                    DESKTOP_CONTROL_FLAGS(0),
                    false,
                    DESKTOP_READOBJECTS.0 | DESKTOP_ENUMERATE.0,
                ) {
                    Ok(d) => {
                        let _ = EnumDesktopWindows(Some(d), Some(close), l);
                        let _ = CloseDesktop(d);
                    }
                    Err(_) => {
                        let _ = EnumWindows(Some(close), l);
                    }
                }
            }
            let t0 = Instant::now();
            while self.running() && t0.elapsed() < wait {
                thread::sleep(Duration::from_millis(100));
            }
            if self.running() {
                warn!("the app didn't close in {}s: ended", wait.as_secs());
            }
        }
        // SAFETY: the job is ours
        unsafe {
            let _ = TerminateJobObject(self.job, 1);
            let _ = CloseHandle(self.job);
        }
    }
}
