//! afterglowsvc.exe "<afterglow.conf>": the Windows service. It runs as SYSTEM in
//! session 0, where nothing can be captured, and keeps afterglow.exe running as SYSTEM
//! in the session at the console instead: started again when it exits or the console
//! moves to another session, asked to stop (Ctrl-C) when the service stops.

use std::ffi::{OsString, c_void};
use std::mem::size_of;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TokenPrimary, TokenSessionId,
};
use windows::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_WRITE, FILE_SHARE_READ, GetTempPathW,
};
use windows::Win32::System::Console::{AttachConsole, CTRL_C_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessAsUserW,
    DETACHED_PROCESS, GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW, SetEvent, TerminateProcess, WaitForMultipleObjects,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    SessionChangeReason,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use super::wide;

const NAME: &str = "PhosphorusAfterglow";
const ERROR_SHUTDOWN_IN_PROGRESS: u32 = 1115;

define_windows_service!(ffi_main, service_main);

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--terminate" {
        std::process::exit(graceful(args[2].parse().unwrap_or(0)) as i32);
    }
    // services start in System32: afterglow.exe is one folder up from tools\
    if let Some(root) = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.parent()?.to_path_buf())) {
        let _ = std::env::set_current_dir(root);
    }
    if let Err(e) = service_dispatcher::start(NAME, ffi_main) {
        eprintln!("afterglowsvc runs as the {NAME} service: {e}");
        std::process::exit(1);
    }
}

/// Ctrl-C to afterglow's console: it stops its app and display and exits
fn graceful(pid: u32) -> u32 {
    // SAFETY: plain console calls
    unsafe {
        if AttachConsole(pid).is_err() {
            return 1;
        }
        let _ = SetConsoleCtrlHandler(None, true);
        if GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0).is_err() {
            return 2;
        }
    }
    0
}

struct Ev(HANDLE);
// SAFETY: event handles are signalled from any thread
unsafe impl Send for Ev {}
unsafe impl Sync for Ev {}

fn event(manual: bool) -> HANDLE {
    // SAFETY: an unnamed event
    unsafe { CreateEventW(None, manual, false, PCWSTR::null()).unwrap_or_default() }
}

/// SYSTEM's own token, moved to the console's session
fn token_for(session: u32) -> Option<HANDLE> {
    // SAFETY: tokens owned here or returned
    unsafe {
        let mut cur = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE, &mut cur).ok()?;
        let mut t = HANDLE::default();
        let r = DuplicateTokenEx(cur, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut t);
        let _ = CloseHandle(cur);
        r.ok()?;
        if SetTokenInformation(t, TokenSessionId, &session as *const u32 as *const c_void, 4).is_err() {
            let _ = CloseHandle(t);
            return None;
        }
        Some(t)
    }
}

fn kill_on_close_job() -> Option<HANDLE> {
    // SAFETY: a job owned by the caller
    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // afterglow dies with the service; the apps it starts break away and live on
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        Some(job)
    }
}

fn log_file() -> HANDLE {
    let mut buf = [0u16; 261];
    // SAFETY: buf sized
    let n = unsafe { GetTempPathW(Some(&mut buf)) } as usize;
    let path = format!("{}afterglow.log", super::from_wide(&buf[..n]));
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: true.into(),
        ..Default::default()
    };
    let p = wide(&path);
    // SAFETY: p NUL-terminated
    unsafe {
        CreateFileW(
            PCWSTR(p.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ,
            Some(&sa),
            CREATE_ALWAYS,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .unwrap_or_default()
    }
}

fn spawn(
    token: HANDLE,
    cmd: &str,
    log: HANDLE,
    flags: windows::Win32::System::Threading::PROCESS_CREATION_FLAGS,
) -> Option<PROCESS_INFORMATION> {
    let mut desktop = wide("winsta0\\default");
    let mut si = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let inherit = !log.is_invalid() && !log.0.is_null();
    if inherit {
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = log;
        si.hStdError = log;
    }
    let mut cl = wide(cmd);
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: buffers live across the call
    unsafe {
        CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cl.as_mut_ptr())),
            None,
            None,
            inherit,
            flags,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
        .ok()?;
    }
    Some(pi)
}

fn service_main(_args: Vec<OsString>) {
    let stop = Ev(event(true));
    let session_change = Ev(event(false));
    let (stop_h, sc_h) = (stop.0.0 as usize, session_change.0.0 as usize);
    let handler = move |c: ServiceControl| -> ServiceControlHandlerResult {
        match c {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::SessionChange(p) => {
                if p.reason == SessionChangeReason::ConsoleConnect {
                    // SAFETY: the event lives as long as the service
                    unsafe { SetEvent(HANDLE(sc_h as *mut c_void)).ok() };
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Stop | ServiceControl::Preshutdown => {
                // SAFETY: as above
                unsafe { SetEvent(HANDLE(stop_h as *mut c_void)).ok() };
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let Ok(status) = service_control_handler::register(NAME, handler) else { return };
    let set = |state, accept| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(30),
            process_id: None,
        });
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN | ServiceControlAccept::SESSION_CHANGE,
    );

    // the config afterglow.exe runs with, as the service's command line names it
    let conf = std::env::args().nth(1).filter(|c| !c.is_empty() && !c.contains('"'));
    let exe = std::env::current_dir().map(|d| d.join("afterglow.exe")).unwrap_or_else(|_| "afterglow.exe".into());
    let cmd = match &conf {
        Some(c) => format!("\"{}\" \"{c}\"", exe.display()),
        None => format!("\"{}\"", exe.display()),
    };
    let me = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let log = log_file();

    // SAFETY: handles created and closed in this loop
    unsafe {
        while WaitForSingleObject(stop.0, 3000) != WAIT_OBJECT_0 {
            let session = WTSGetActiveConsoleSessionId();
            if session == u32::MAX {
                continue;
            }
            let Some(token) = token_for(session) else { continue };
            let Some(job) = kill_on_close_job() else {
                let _ = CloseHandle(token);
                continue;
            };
            let Some(pi) = spawn(token, &cmd, log, CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED)
            else {
                let _ = CloseHandle(token);
                let _ = CloseHandle(job);
                continue;
            };
            let _ = AssignProcessToJobObject(job, pi.hProcess);
            ResumeThread(pi.hThread);
            loop {
                let w = WaitForMultipleObjects(&[stop.0, pi.hProcess, session_change.0], false, INFINITE);
                if w.0 == WAIT_OBJECT_0.0 + 2 {
                    if WTSGetActiveConsoleSessionId() == session {
                        continue;
                    }
                } else if w.0 == WAIT_OBJECT_0.0 + 1 {
                    let mut code = 0;
                    if GetExitCodeProcess(pi.hProcess, &mut code).is_ok() && code == ERROR_SHUTDOWN_IN_PROGRESS {
                        let _ = SetEvent(stop.0);
                    }
                    break;
                }
                // stopping, or the console moved: asked to exit, made to after 20s
                let asked = spawn(
                    token,
                    &format!("\"{me}\" --terminate {}", pi.dwProcessId),
                    HANDLE::default(),
                    DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT,
                )
                .map(|h| {
                    WaitForSingleObject(h.hProcess, 10_000);
                    let mut code = 1;
                    let _ = GetExitCodeProcess(h.hProcess, &mut code);
                    let _ = CloseHandle(h.hProcess);
                    let _ = CloseHandle(h.hThread);
                    code == 0
                })
                .unwrap_or(false);
                if !asked || WaitForSingleObject(pi.hProcess, 20_000) != WAIT_OBJECT_0 {
                    let _ = TerminateProcess(pi.hProcess, 1);
                }
                break;
            }
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(token);
            let _ = CloseHandle(job);
        }
        let _ = CloseHandle(log);
    }
    set(ServiceState::Stopped, ServiceControlAccept::empty());
}
