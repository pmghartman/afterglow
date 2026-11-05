//! An app's own X server: Xvfb started at the largest size a viewer gets, locked with
//! a cookie only afterglow and the app hold, set to the viewer's size with RandR, and
//! run by a window manager of a few lines that keeps the app's windows filling it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use super::wm::Wm;
use crate::{info, warn};

/// where an X server is and how to get in
#[derive(Clone)]
pub struct Display {
    /// `:N`
    pub name: String,
    /// MIT-MAGIC-COOKIE-1 for an Xvfb of ours; none for a session's own display
    pub cookie: Option<Vec<u8>>,
    /// the file holding it, for the app
    pub auth: Option<PathBuf>,
    /// our Xvfb's framebuffer as a file in memory (XWD, -fbdir): read in place
    pub fb: Option<PathBuf>,
}

impl Display {
    pub fn connect(&self) -> Result<(RustConnection, usize), String> {
        let Some(cookie) = &self.cookie else {
            return RustConnection::connect(Some(&self.name)).map_err(|e| format!("no X server at {}: {e}", self.name));
        };
        let n = self.name.trim_start_matches(':');
        let sock = UnixStream::connect(format!("/tmp/.X11-unix/X{n}")).map_err(|e| format!("{}: {e}", self.name))?;
        let (stream, _) = DefaultStream::from_unix_stream(sock).map_err(|e| e.to_string())?;
        let c =
            RustConnection::connect_to_stream_with_auth_info(stream, 0, b"MIT-MAGIC-COOKIE-1".to_vec(), cookie.clone())
                .map_err(|e| format!("{} refused us: {e}", self.name))?;
        Ok((c, 0))
    }
}

/// an Xauthority entry: family, address, display number, auth name, data, each
/// length-prefixed (u16 big-endian). Family wild with no address: any host
fn xauth_entry(number: &str, cookie: &[u8]) -> Vec<u8> {
    let mut b = 0xffffu16.to_be_bytes().to_vec();
    for f in [&b""[..], number.as_bytes(), b"MIT-MAGIC-COOKIE-1", cookie] {
        b.extend_from_slice(&(f.len() as u16).to_be_bytes());
        b.extend_from_slice(f);
    }
    b
}

fn random(n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| format!("no randomness: {e}"))?;
    Ok(b)
}

pub struct Xvfb {
    child: Child,
    pub display: Display,
    /// the window manager inside afterglow that runs its windows
    pub wm: Option<Wm>,
}

/// the framebuffer Xvfb gets: what a viewer's size can grow to without a new server
pub const MAX: (u16, u16) = (3840, 2160);

impl Xvfb {
    pub fn start(bin: &str, w: u16, h: u16, dpi: u32) -> Result<Xvfb, String> {
        let cookie = random(16)?;
        let tag = random(6)?.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let auth = std::env::temp_dir().join(format!("afterglow-{tag}.xauth"));
        let write_auth = |number: &str| -> Result<(), String> {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&auth)
                .map_err(|e| format!("{}: {e}", auth.display()))?;
            f.write_all(&xauth_entry(number, &cookie)).map_err(|e| e.to_string())
        };
        // the server reads the cookie at start, any display; the app's copy names its display after
        write_auth("")?;
        let mut fds = [0i32; 2];
        // SAFETY: a pipe whose write end Xvfb alone inherits to say its display number:
        // close-on-exec, so no other child started meanwhile (the app, another Xvfb) holds it
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err("no pipe".into());
        }
        // SAFETY: fds[0] is ours alone from here
        let mut rd = unsafe { File::from_raw_fd(fds[0]) };
        let size = (MAX.0.max(w), MAX.1.max(h));
        // the framebuffer in RAM, in a directory only we can read: capture reads it there
        let fbdir = fb_dir(&tag);
        let mut cmd = Command::new(bin);
        cmd.args(["-displayfd", &fds[1].to_string()])
            .args(["-screen", "0", &format!("{}x{}x24", size.0, size.1)])
            .args(["-nolisten", "tcp", "-noreset", "-dpi", &dpi.to_string(), "-auth"])
            .arg(&auth);
        if let Some(d) = &fbdir {
            cmd.arg("-fbdir").arg(d);
        }
        let wfd = fds[1];
        // SAFETY: in the child between fork and exec, one fcntl on a descriptor it has: the
        // write end kept across the exec for Xvfb
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::fcntl(wfd, libc::F_SETFD, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{bin} didn't start: {e}"));
        // SAFETY: our copy of the write end, closed so the read sees Xvfb's alone
        unsafe { libc::close(fds[1]) };
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                if let Some(d) = &fbdir {
                    let _ = std::fs::remove_dir_all(d);
                }
                return Err(e);
            }
        };
        let number = match read_line(&mut rd, Duration::from_secs(10)) {
            Some(n) if n.parse::<u32>().is_ok() => n,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&auth);
                if let Some(d) = &fbdir {
                    let _ = std::fs::remove_dir_all(d);
                }
                return Err(format!("{bin} didn't say which display it serves"));
            }
        };
        write_auth(&number)?;
        let fb = fbdir.map(|d| d.join("Xvfb_screen0")).filter(|f| f.is_file());
        let display = Display { name: format!(":{number}"), cookie: Some(cookie), auth: Some(auth), fb };
        let mut x = Xvfb { child, display, wm: None };
        x.fit(w, h, dpi)?;
        x.wm = Some(Wm::start(&x.display)?);
        info!("Xvfb on {} at {w}x{h}", x.display.name);
        Ok(x)
    }

    /// its screen set to w×h at dpi, for an app taking up a spare one
    pub fn fit(&self, w: u16, h: u16, dpi: u32) -> Result<(), String> {
        let (conn, screen) = self.display.connect()?;
        resize(&conn, conn.setup().roots[screen].root, w, h, dpi);
        Ok(())
    }

    /// whether it's still serving
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        // its window manager first, while the server's still there to say goodbye to
        drop(self.wm.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(a) = &self.display.auth {
            let _ = std::fs::remove_file(a);
        }
        if let Some(d) = self.display.fb.as_ref().and_then(|f| f.parent()) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// a fresh private directory on a RAM filesystem for an Xvfb's framebuffer; none where
/// there's no /dev/shm (then capture reads the screen over MIT-SHM)
fn fb_dir(tag: &str) -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let base = PathBuf::from("/dev/shm");
    if !base.is_dir() {
        return None;
    }
    let d = base.join(format!("afterglow-{tag}"));
    std::fs::DirBuilder::new().mode(0o700).create(&d).ok()?;
    Some(d)
}

fn read_line(f: &mut File, timeout: Duration) -> Option<String> {
    let t0 = Instant::now();
    let mut got = Vec::new();
    while t0.elapsed() < timeout {
        let mut p = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd
        if unsafe { libc::poll(&mut p, 1, 100) } <= 0 {
            continue;
        }
        let mut b = [0u8; 16];
        match f.read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&b[..n]),
        }
        if got.contains(&b'\n') {
            break;
        }
    }
    let s = String::from_utf8_lossy(&got).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// the screen set to w×h: a mode of that size (made if new), the CRTC moved to it, its
/// size in millimetres what says dpi
pub fn resize(conn: &RustConnection, root: u32, w: u16, h: u16, dpi: u32) -> bool {
    let go = || -> Result<(), Box<dyn std::error::Error>> {
        let g = conn.get_geometry(root)?.reply()?;
        if (g.width, g.height) == (w, h) {
            // the size as it is; the dpi said again in case it's new
            let mm = |px: u16| (px as u32 * 254 / (dpi.max(1) * 10)).max(1);
            let _ = conn.randr_set_screen_size(root, w, h, mm(w), mm(h));
            return Ok(());
        }
        conn.randr_query_version(1, 3)?.reply()?;
        let res = conn.randr_get_screen_resources(root)?.reply()?;
        let (Some(&out), Some(&crtc)) = (res.outputs.first(), res.crtcs.first()) else {
            return Err("no RandR output".into());
        };
        let mode = match res.modes.iter().find(|m| m.width == w && m.height == h) {
            Some(m) => m.id,
            None => {
                let name = format!("{w}x{h}");
                let info = randr::ModeInfo {
                    width: w,
                    height: h,
                    dot_clock: w as u32 * h as u32 * 60,
                    htotal: w,
                    vtotal: h,
                    name_len: name.len() as u16,
                    ..Default::default()
                };
                let m = conn.randr_create_mode(root, info, name.as_bytes())?.reply()?.mode;
                conn.randr_add_output_mode(out, m)?.check()?;
                m
            }
        };
        // the CRTC off, the screen resized, the CRTC on at the new mode
        conn.randr_set_crtc_config(crtc, 0, 0, 0, 0, 0, randr::Rotation::ROTATE0, &[])?.reply()?;
        let mm = |px: u16| (px as u32 * 254 / (dpi.max(1) * 10)).max(1);
        conn.randr_set_screen_size(root, w, h, mm(w), mm(h))?.check()?;
        conn.randr_set_crtc_config(crtc, 0, 0, 0, 0, mode, randr::Rotation::ROTATE0, &[out])?.reply()?;
        Ok(())
    };
    match go() {
        Ok(()) => true,
        Err(e) => {
            warn!("the display can't be {w}x{h}: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_xauthority_entry() {
        let e = xauth_entry("12", &[0xab; 16]);
        assert_eq!(&e[..4], &[0xff, 0xff, 0, 0]);
        assert_eq!(&e[4..8], &[0, 2, b'1', b'2']);
        assert_eq!(&e[8..10], &[0, 18]);
        assert_eq!(&e[10..28], b"MIT-MAGIC-COOKIE-1");
        assert_eq!(&e[28..30], &[0, 16]);
        assert_eq!(e.len(), 30 + 16);
    }
}
