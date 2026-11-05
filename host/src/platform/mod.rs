//! Each platform's host: Windows (DXGI capture, Media Foundation H.264, SendInput,
//! SudoVDA), and Linux (an Xvfb per app or the session's X display, MIT-SHM and
//! XDamage capture, OpenH264, XTest).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows::Windows as Native;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::Linux as Native;

/// this platform's host, set up from the config
pub fn open(conf: &crate::conf::Conf) -> Result<Arc<Native>, String> {
    Native::open(conf)
}

/// `afterglow --grab`: the capture process Windows' host starts as the signed-in user
pub fn grab() -> i32 {
    #[cfg(windows)]
    return windows::grab_main();
    #[cfg(not(windows))]
    {
        eprintln!("afterglow: --grab is Windows' alone");
        2
    }
}

/// shutdown raised on Ctrl-C, a console close or SIGTERM
pub fn on_shutdown(flag: Arc<AtomicBool>) {
    #[cfg(windows)]
    windows::on_shutdown(flag);
    #[cfg(unix)]
    unix_signals(flag);
}

#[cfg(unix)]
fn unix_signals(flag: Arc<AtomicBool>) {
    use std::sync::OnceLock;
    use std::sync::atomic::Ordering;
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    let _ = FLAG.set(flag);
    extern "C" fn raise(_: libc::c_int) {
        if let Some(f) = FLAG.get() {
            f.store(true, Ordering::SeqCst);
        }
    }
    // SAFETY: the handler only stores to an atomic
    unsafe {
        libc::signal(libc::SIGINT, raise as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGTERM, raise as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
}
