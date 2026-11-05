//! A window manager for one app on its own Xvfb, inside afterglow: the app's main
//! windows fill the screen and have the keyboard, a dialog keeps its size, centred.
//! It tells the driver the main window's title and the app's windows as they change,
//! brings one to the front when asked, closes the app as its own close button would
//! (WM_DELETE_WINDOW), and carries the clipboard both ways: the viewer's text owned
//! here as CLIPBOARD and PRIMARY for the app to paste, what the app copies (XFixes
//! says when) read and sent on. Commands come from the link on a queue and a wake pipe.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde_json::json;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureNotifyEvent, ConfigureWindowAux,
    ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, PropMode, SelectionNotifyEvent, StackMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::xserver::{Display, MAX};
use crate::host::Notes;
use crate::{debug, warn};

pub enum Cmd {
    Raise(u32),
    Close,
    Clip(String),
}

/// what the link and the window manager's thread share
#[derive(Default)]
struct Shared {
    cmds: Mutex<VecDeque<Cmd>>,
    notes: Mutex<Option<Notes>>,
    /// the app's windows managed now, and whether it ever had one
    windows: AtomicUsize,
    seen: AtomicBool,
    stop: AtomicBool,
}

pub struct Wm {
    shared: Arc<Shared>,
    wake: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: a fresh non-blocking pipe, both ends owned from here
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) != 0 {
            return None;
        }
        Some((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

impl Wm {
    pub fn start(d: &Display) -> Result<Wm, String> {
        let (rx, tx) = pipe().ok_or("no pipe")?;
        let shared = Arc::new(Shared::default());
        let (d, sh) = (d.clone(), shared.clone());
        let thread = thread::Builder::new()
            .name("wm".into())
            .spawn(move || {
                if let Err(e) = run(&d, &sh, rx) {
                    warn!("the window manager on {} stopped: {e}", d.name);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Wm { shared, wake: tx, thread: Some(thread) })
    }

    fn poke(&self) {
        // SAFETY: one byte to our own pipe; a full pipe is already a wake
        unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr() as *const libc::c_void, 1) };
    }

    pub fn send(&self, c: Cmd) {
        self.shared.cmds.lock().unwrap().push_back(c);
        self.poke();
    }

    /// where the title, the windows and the clipboard go; the current ones told at once
    pub fn set_notes(&self, n: Option<Notes>) {
        *self.shared.notes.lock().unwrap() = n;
        self.poke();
    }

    pub fn windows(&self) -> usize {
        self.shared.windows.load(Ordering::Relaxed)
    }

    pub fn ever_had_a_window(&self) -> bool {
        self.shared.seen.load(Ordering::Relaxed)
    }
}

impl Drop for Wm {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.poke();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Atoms {
    protocols: u32,
    delete: u32,
    net_name: u32,
    utf8: u32,
    wtype: u32,
    dialogs: Vec<u32>,
    clipboard: u32,
    primary: u32,
    targets: u32,
    text: u32,
    plain: u32,
    compound: u32,
    ours: u32,
}

fn atoms(c: &RustConnection) -> Result<Atoms, String> {
    let names = [
        "WM_PROTOCOLS",
        "WM_DELETE_WINDOW",
        "_NET_WM_NAME",
        "UTF8_STRING",
        "_NET_WM_WINDOW_TYPE",
        "_NET_WM_WINDOW_TYPE_DIALOG",
        "_NET_WM_WINDOW_TYPE_UTILITY",
        "_NET_WM_WINDOW_TYPE_SPLASH",
        "_NET_WM_WINDOW_TYPE_TOOLBAR",
        "_NET_WM_WINDOW_TYPE_NOTIFICATION",
        "CLIPBOARD",
        "TARGETS",
        "TEXT",
        "text/plain;charset=utf-8",
        "COMPOUND_TEXT",
        "AFTERGLOW_CLIP",
    ];
    let cookies: Vec<_> = names.iter().map(|n| c.intern_atom(false, n.as_bytes())).collect();
    let mut a = Vec::with_capacity(names.len());
    for ck in cookies {
        a.push(ck.map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom);
    }
    Ok(Atoms {
        protocols: a[0],
        delete: a[1],
        net_name: a[2],
        utf8: a[3],
        wtype: a[4],
        dialogs: a[5..10].to_vec(),
        clipboard: a[10],
        primary: AtomEnum::PRIMARY.into(),
        targets: a[11],
        text: a[12],
        plain: a[13],
        compound: a[14],
        ours: a[15],
    })
}

/// the most a clipboard carries either way: past it, X would want INCR transfers
const CLIP_MAX: usize = 256 << 10;

struct State<'a> {
    c: &'a RustConnection,
    a: Atoms,
    root: u32,
    clipwin: u32,
    sw: u16,
    sh: u16,
    /// managed windows, bottom to top: (window, fills the screen)
    stack: Vec<(u32, bool)>,
    titles: HashMap<u32, String>,
    told_title: Option<String>,
    told_list: Option<serde_json::Value>,
    clip: Vec<u8>,
    sh_: &'a Shared,
}

impl State<'_> {
    fn note(&self, v: serde_json::Value) {
        if let Some(n) = self.sh_.notes.lock().unwrap().as_ref() {
            n(v);
        }
    }

    fn prop(&self, w: u32, p: u32, ty: u32, max: u32) -> Option<(u32, Vec<u8>)> {
        let r = self.c.get_property(false, w, p, ty, 0, max / 4).ok()?.reply().ok()?;
        (r.value_len > 0).then_some((r.type_, r.value))
    }

    fn title_of(&self, w: u32) -> String {
        let s = match self.prop(w, self.a.net_name, self.a.utf8, 4096) {
            Some((_, v)) => String::from_utf8_lossy(&v).into_owned(),
            None => match self.prop(w, AtomEnum::WM_NAME.into(), AtomEnum::ANY.into(), 4096) {
                Some((t, v)) if t == u32::from(AtomEnum::STRING) => v.iter().map(|&b| b as char).collect(),
                // COMPOUND_TEXT: its UTF-8 runs and Latin-1 between escapes, the escapes dropped
                Some((t, v)) if t == self.a.compound => {
                    let mut out = String::new();
                    let mut i = 0;
                    let mut utf8 = false;
                    let mut run = Vec::new();
                    let flush = |run: &mut Vec<u8>, utf8: bool, out: &mut String| {
                        if utf8 {
                            out.push_str(&String::from_utf8_lossy(run));
                        } else {
                            out.extend(run.iter().map(|&b| b as char));
                        }
                        run.clear();
                    };
                    while i < v.len() {
                        if v[i] == 0x1b && i + 2 < v.len() {
                            flush(&mut run, utf8, &mut out);
                            if v[i + 1] == b'%' {
                                utf8 = v[i + 2] == b'G';
                            }
                            i += 3;
                            continue;
                        }
                        run.push(v[i]);
                        i += 1;
                    }
                    flush(&mut run, utf8, &mut out);
                    out
                }
                Some((_, v)) => String::from_utf8_lossy(&v).into_owned(),
                None => String::new(),
            },
        };
        s.chars().take(300).collect()
    }

    fn is_dialog(&self, w: u32) -> bool {
        if self.prop(w, AtomEnum::WM_TRANSIENT_FOR.into(), AtomEnum::WINDOW.into(), 4).is_some() {
            return true;
        }
        let Some((_, v)) = self.prop(w, self.a.wtype, AtomEnum::ATOM.into(), 64) else { return false };
        v.chunks_exact(4).map(|b| u32::from_ne_bytes(b.try_into().unwrap())).any(|t| self.a.dialogs.contains(&t))
    }

    fn fill(&self, w: u32) {
        let _ = self.c.configure_window(
            w,
            &ConfigureWindowAux::new().x(0).y(0).width(self.sw as u32).height(self.sh as u32).border_width(0),
        );
        self.synthetic(w, 0, 0, self.sw, self.sh);
    }

    /// ICCCM 4.1.5: a client told where it is even when its request wasn't the answer
    fn synthetic(&self, w: u32, x: i16, y: i16, ww: u16, hh: u16) {
        let e = ConfigureNotifyEvent {
            response_type: x11rb::protocol::xproto::CONFIGURE_NOTIFY_EVENT,
            sequence: 0,
            event: w,
            window: w,
            above_sibling: x11rb::NONE,
            x,
            y,
            width: ww,
            height: hh,
            border_width: 0,
            override_redirect: false,
        };
        let _ = self.c.send_event(false, w, EventMask::STRUCTURE_NOTIFY, e);
    }

    fn focus(&self, w: u32) {
        let _ = self.c.configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        let _ = self.c.set_input_focus(InputFocus::POINTER_ROOT, w, x11rb::CURRENT_TIME);
    }

    /// the main window (the newest that fills the screen) and the list, told when changed
    fn report(&mut self) {
        self.sh_.windows.store(self.stack.len(), Ordering::Relaxed);
        if !self.stack.is_empty() {
            self.sh_.seen.store(true, Ordering::Relaxed);
        }
        let main = self.stack.iter().rev().find(|s| s.1).or(self.stack.last()).map_or(0, |s| s.0);
        let title = if main == 0 { String::new() } else { self.titles.get(&main).cloned().unwrap_or_default() };
        if self.told_title.as_ref() != Some(&title) {
            self.note(json!({"t": "title", "s": title}));
            self.told_title = Some(title);
        }
        let list: Vec<_> =
            self.stack.iter().map(|s| json!([s.0, self.titles.get(&s.0).cloned().unwrap_or_default()])).collect();
        let v = json!({"t": "windows", "list": list, "main": main});
        if self.told_list.as_ref() != Some(&v) {
            self.note(v.clone());
            self.told_list = Some(v);
        }
    }

    fn map(&mut self, w: u32) {
        let dialog = self.is_dialog(w);
        if dialog {
            if let Some(g) = self.c.get_geometry(w).ok().and_then(|c| c.reply().ok()) {
                let (ww, hh) = (g.width.min(self.sw), g.height.min(self.sh));
                let x = (self.sw as i32 - ww as i32) / 2;
                let y = (self.sh as i32 - hh as i32) / 2;
                let _ = self.c.configure_window(
                    w,
                    &ConfigureWindowAux::new().x(x.max(0)).y(y.max(0)).width(ww as u32).height(hh as u32),
                );
            }
        } else {
            self.fill(w);
        }
        let _ = self.c.change_window_attributes(
            w,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        );
        let _ = self.c.map_window(w);
        self.focus(w);
        self.stack.retain(|s| s.0 != w);
        self.stack.push((w, !dialog));
        let t = self.title_of(w);
        self.titles.insert(w, t);
        debug!("window {w:#x} {}", if dialog { "centred" } else { "fills the screen" });
        self.report();
    }

    fn unmanage(&mut self, w: u32) {
        if !self.stack.iter().any(|s| s.0 == w) {
            return;
        }
        self.stack.retain(|s| s.0 != w);
        self.titles.remove(&w);
        if let Some(&(top, _)) = self.stack.last() {
            let _ = self.c.set_input_focus(InputFocus::POINTER_ROOT, top, x11rb::CURRENT_TIME);
        }
        self.report();
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Raise(w) => {
                if let Some(i) = self.stack.iter().position(|s| s.0 == w) {
                    let s = self.stack.remove(i);
                    self.stack.push(s);
                    self.focus(w);
                    self.report();
                }
            }
            Cmd::Close => {
                // the topmost: a dialog in front is what the close button would close
                let Some(&(w, _)) = self.stack.last() else { return };
                let polite = self.prop(w, self.a.protocols, AtomEnum::ATOM.into(), 256).is_some_and(|(_, v)| {
                    v.chunks_exact(4).any(|b| u32::from_ne_bytes(b.try_into().unwrap()) == self.a.delete)
                });
                if polite {
                    let e =
                        ClientMessageEvent::new(32, w, self.a.protocols, [self.a.delete, x11rb::CURRENT_TIME, 0, 0, 0]);
                    let _ = self.c.send_event(false, w, EventMask::NO_EVENT, e);
                } else {
                    let _ = self.c.kill_client(w);
                }
            }
            Cmd::Clip(text) => {
                self.clip = text.into_bytes();
                self.clip.truncate(CLIP_MAX);
                for sel in [self.a.clipboard, self.a.primary] {
                    let _ = self.c.set_selection_owner(self.clipwin, sel, x11rb::CURRENT_TIME);
                }
            }
        }
    }

    /// another client asking for the text we own
    fn answer(&self, e: &x11rb::protocol::xproto::SelectionRequestEvent) {
        let prop = if e.property == x11rb::NONE { e.target } else { e.property };
        let string: u32 = AtomEnum::STRING.into();
        let ok = if e.target == self.a.targets {
            let list = [self.a.targets, self.a.utf8, string, self.a.text, self.a.plain];
            self.c.change_property32(PropMode::REPLACE, e.requestor, prop, AtomEnum::ATOM, &list).is_ok()
        } else if [self.a.utf8, string, self.a.text, self.a.plain].contains(&e.target) {
            let ty = if e.target == string { string } else { self.a.utf8 };
            self.c.change_property8(PropMode::REPLACE, e.requestor, prop, ty, &self.clip).is_ok()
        } else {
            false
        };
        let n = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { prop } else { x11rb::NONE },
        };
        let _ = self.c.send_event(false, e.requestor, EventMask::NO_EVENT, n);
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::MapRequest(e) => self.map(e.window),
            Event::ConfigureRequest(e) => {
                let full = self.stack.iter().find(|s| s.0 == e.window).map(|s| s.1);
                if full == Some(true) {
                    self.fill(e.window);
                } else {
                    let _ = self.c.configure_window(e.window, &ConfigureWindowAux::from_configure_request(&e));
                }
            }
            Event::UnmapNotify(e) if e.event == self.root => self.unmanage(e.window),
            Event::DestroyNotify(e) => self.unmanage(e.window),
            Event::ConfigureNotify(e) if e.window == self.root => {
                (self.sw, self.sh) = (e.width, e.height);
                for &(w, full) in &self.stack {
                    if full {
                        self.fill(w);
                    }
                }
            }
            Event::PropertyNotify(e)
                if (e.atom == self.a.net_name || e.atom == u32::from(AtomEnum::WM_NAME))
                    && self.stack.iter().any(|s| s.0 == e.window) =>
            {
                let t = self.title_of(e.window);
                self.titles.insert(e.window, t);
                self.report();
            }
            Event::SelectionRequest(e) if e.owner == self.clipwin => self.answer(&e),
            Event::XfixesSelectionNotify(e)
                if e.selection == self.a.clipboard && e.owner != self.clipwin && e.owner != x11rb::NONE =>
            {
                // the app copied: its text asked for, onto our window
                let _ = self.c.convert_selection(
                    self.clipwin,
                    self.a.clipboard,
                    self.a.utf8,
                    self.a.ours,
                    x11rb::CURRENT_TIME,
                );
            }
            Event::SelectionNotify(e) if e.requestor == self.clipwin && e.property != x11rb::NONE => {
                let r = self
                    .c
                    .get_property(true, self.clipwin, e.property, AtomEnum::ANY, 0, (CLIP_MAX / 4) as u32)
                    .ok()
                    .and_then(|c| c.reply().ok());
                if let Some(r) = r.filter(|r| !r.value.is_empty()) {
                    self.note(json!({"t": "clip", "s": String::from_utf8_lossy(&r.value)}));
                }
            }
            _ => {}
        }
    }
}

fn run(d: &Display, sh: &Shared, wake: OwnedFd) -> Result<(), String> {
    let (c, screen) = d.connect()?;
    let root = c.setup().roots[screen].root;
    let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY | EventMask::STRUCTURE_NOTIFY;
    c.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(mask))
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|_| format!("another window manager runs on {}", d.name))?;
    let a = atoms(&c)?;
    // a window of our own to own the clipboard with, never mapped
    let clipwin = c.generate_id().map_err(|e| e.to_string())?;
    c.create_window(0, clipwin, root, -10, -10, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new())
        .map_err(|e| e.to_string())?;
    if c.xfixes_query_version(4, 0).ok().and_then(|r| r.reply().ok()).is_some() {
        let _ = c.xfixes_select_selection_input(root, a.clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER);
    }
    let (sw, shh) = c.get_geometry(root).ok().and_then(|g| g.reply().ok()).map_or(MAX, |g| (g.width, g.height));
    let mut st = State {
        c: &c,
        a,
        root,
        clipwin,
        sw,
        sh: shh,
        stack: Vec::new(),
        titles: HashMap::new(),
        told_title: None,
        told_list: None,
        clip: Vec::new(),
        sh_: sh,
    };
    let _ = c.flush();
    let fd = c.stream().as_raw_fd();
    let mut had_notes = false;
    while !sh.stop.load(Ordering::Relaxed) {
        // notes newly wanted: everything told again
        let has = sh.notes.lock().unwrap().is_some();
        if has && !had_notes {
            st.told_title = None;
            st.told_list = None;
            st.report();
        }
        had_notes = has;
        let cmds: Vec<Cmd> = sh.cmds.lock().unwrap().drain(..).collect();
        for cmd in cmds {
            st.command(cmd);
        }
        while let Some(ev) = c.poll_for_event().map_err(|e| e.to_string())? {
            st.event(ev);
        }
        c.flush().map_err(|e| e.to_string())?;
        let mut p = [
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two pollfds
        unsafe { libc::poll(p.as_mut_ptr(), 2, 1000) };
        if p[1].revents != 0 {
            let mut b = [0u8; 64];
            // SAFETY: draining our own non-blocking pipe
            while unsafe { libc::read(wake.as_raw_fd(), b.as_mut_ptr() as *mut libc::c_void, b.len()) } > 0 {}
        }
    }
    let _ = c.sync();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::linux::xserver::Xvfb;
    use std::time::{Duration, Instant};
    use x11rb::protocol::xproto::{Atom, PropMode};

    fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let t0 = Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "timed out: {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn atom(c: &RustConnection, n: &str) -> Atom {
        c.intern_atom(false, n.as_bytes()).unwrap().reply().unwrap().atom
    }

    /// the app's side on its own connection: windows, a title changed, a dialog, raised,
    /// the clipboard both ways, closed as its close button would
    #[test]
    fn titles_windows_raise_clipboard_close_on_a_real_xvfb() {
        if std::process::Command::new("Xvfb").arg("-help").output().is_err() {
            eprintln!("no Xvfb: skipped");
            return;
        }
        let x = Xvfb::start("Xvfb", 640, 480, 96).unwrap();
        let wm = x.wm.as_ref().unwrap();
        let got: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let g = got.clone();
        wm.set_notes(Some(Arc::new(move |v| g.lock().unwrap().push(v))));
        let last = |t: &str| got.lock().unwrap().iter().rev().find(|v| v["t"] == t).cloned();

        let (c, screen) = x.display.connect().unwrap();
        let root = c.setup().roots[screen].root;
        let (utf8, net_name, protocols, delete) =
            (atom(&c, "UTF8_STRING"), atom(&c, "_NET_WM_NAME"), atom(&c, "WM_PROTOCOLS"), atom(&c, "WM_DELETE_WINDOW"));
        let win = |title: &str| {
            let w = c.generate_id().unwrap();
            c.create_window(
                0,
                w,
                root,
                0,
                0,
                200,
                100,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
            )
            .unwrap();
            c.change_property8(PropMode::REPLACE, w, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes()).unwrap();
            c.change_property32(PropMode::REPLACE, w, protocols, AtomEnum::ATOM, &[delete]).unwrap();
            w
        };
        let main = win("one");
        c.map_window(main).unwrap();
        c.flush().unwrap();
        until("the title", || last("title").filter(|v| v["s"] == "one"));
        assert_eq!(wm.windows(), 1);
        // it fills the screen
        let g =
            until("filled", || c.get_geometry(main).ok()?.reply().ok().filter(|g| g.width == 640 && g.height == 480));
        assert_eq!((g.x, g.y), (0, 0));

        // a new title, the UTF-8 one
        c.change_property8(PropMode::REPLACE, main, net_name, utf8, "über".as_bytes()).unwrap();
        c.flush().unwrap();
        until("the new title", || last("title").filter(|v| v["s"] == "über"));

        // a dialog: centred at its size, listed, the main window still the main one
        let dlg = win("save?");
        c.change_property32(PropMode::REPLACE, dlg, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, &[main]).unwrap();
        c.map_window(dlg).unwrap();
        c.flush().unwrap();
        let l = until("two windows", || last("windows").filter(|v| v["list"].as_array().is_some_and(|a| a.len() == 2)));
        assert_eq!(l["main"], main);
        assert_eq!(l["list"][1], json!([dlg, "save?"]));
        let g = until("centred", || c.get_geometry(dlg).ok()?.reply().ok().filter(|g| g.x == 220));
        assert_eq!((g.y, g.width, g.height), (190, 200, 100));
        assert_eq!(last("title").unwrap()["s"], "über");

        // the main window raised: last in the list
        wm.send(Cmd::Raise(main));
        until("raised", || last("windows").filter(|v| v["list"][1][0] == main));

        // the viewer's clipboard, pasted by the app: asked of whoever owns CLIPBOARD
        let clipboard = atom(&c, "CLIPBOARD");
        let prop = atom(&c, "TEST_PASTE");
        wm.send(Cmd::Clip("from the viewer".into()));
        until("owned", || {
            let o = c.get_selection_owner(clipboard).ok()?.reply().ok()?.owner;
            (o != x11rb::NONE).then_some(())
        });
        c.convert_selection(main, clipboard, utf8, prop, x11rb::CURRENT_TIME).unwrap();
        c.flush().unwrap();
        let pasted = until("the paste", || match c.poll_for_event().ok()?? {
            Event::SelectionNotify(e) if e.property == prop => {
                Some(c.get_property(true, main, prop, AtomEnum::ANY, 0, 1024).ok()?.reply().ok()?.value)
            }
            _ => None,
        });
        assert_eq!(pasted, b"from the viewer");

        // the app copies: its text comes to the viewer
        c.set_selection_owner(main, clipboard, x11rb::CURRENT_TIME).unwrap();
        c.flush().unwrap();
        until("asked for the app's text", || match c.poll_for_event().ok()?? {
            Event::SelectionRequest(e) => {
                c.change_property8(PropMode::REPLACE, e.requestor, e.property, utf8, b"from the app").ok()?;
                let n = SelectionNotifyEvent {
                    response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: e.time,
                    requestor: e.requestor,
                    selection: e.selection,
                    target: e.target,
                    property: e.property,
                };
                c.send_event(false, e.requestor, EventMask::NO_EVENT, n).ok()?;
                c.flush().ok()?;
                Some(())
            }
            _ => None,
        });
        until("the app's text", || last("clip").filter(|v| v["s"] == "from the app"));

        // closed politely: the topmost window asked to delete itself
        wm.send(Cmd::Close);
        let asked = until("asked to close", || match c.poll_for_event().ok()?? {
            Event::ClientMessage(e) if e.type_ == protocols => Some(e.window),
            _ => None,
        });
        assert_eq!(asked, main);
        c.destroy_window(dlg).unwrap();
        c.destroy_window(main).unwrap();
        c.flush().unwrap();
        until("no windows", || (wm.windows() == 0).then_some(()));
        assert!(wm.ever_had_a_window());
    }
}
