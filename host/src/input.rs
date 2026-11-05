//! afterglow's input packets (docs/protocol.md), as the client's `InputPacker` writes them, into events for
//! the platform to inject; and the key state around them: a held key repeats, a stopped
//! video leaves nothing held down.
//!
//! A packet: u32 big-endian size of what follows it, u32 little-endian magic, the event.

use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// x, y within a reference size w, h (the video's, less one)
    MouseAbs {
        x: i16,
        y: i16,
        w: i16,
        h: i16,
    },
    MouseRel {
        dx: i16,
        dy: i16,
    },
    /// 1 left, 2 middle, 3 right, 4 and 5 the side buttons
    Button {
        button: u8,
        down: bool,
    },
    /// a notch is 120
    Scroll(i16),
    HScroll(i16),
    /// a US-layout Windows virtual key; mods: 1 shift, 2 ctrl, 4 alt, 8 meta
    Key {
        vk: u8,
        down: bool,
        mods: u8,
        flags: u8,
    },
    Text(String),
}

const KEY_DOWN: u32 = 0x03;
const KEY_UP: u32 = 0x04;
const MOUSE_ABS: u32 = 0x05;
const MOUSE_REL: u32 = 0x07;
const BUTTON_DOWN: u32 = 0x08;
const BUTTON_UP: u32 = 0x09;
const SCROLL: u32 = 0x0a;
const UTF8: u32 = 0x17;
const HSCROLL: u32 = 0x5500_0001;

pub fn parse(p: &[u8]) -> Option<Event> {
    if p.len() < 8 {
        return None;
    }
    let size = u32::from_be_bytes(p[0..4].try_into().ok()?) as usize;
    let p = p.get(..4 + size)?;
    let magic = u32::from_le_bytes(p[4..8].try_into().ok()?);
    let be16 = |at: usize| p.get(at..at + 2).map(|b| i16::from_be_bytes([b[0], b[1]]));
    Some(match magic {
        MOUSE_ABS => Event::MouseAbs { x: be16(8)?, y: be16(10)?, w: be16(14)?, h: be16(16)? },
        MOUSE_REL => Event::MouseRel { dx: be16(8)?, dy: be16(10)? },
        BUTTON_DOWN | BUTTON_UP => Event::Button { button: *p.get(8)?, down: magic == BUTTON_DOWN },
        SCROLL => Event::Scroll(be16(8)?),
        HSCROLL => Event::HScroll(be16(8)?),
        KEY_DOWN | KEY_UP => {
            let b = p.get(8..12)?;
            Event::Key { vk: b[1], down: magic == KEY_DOWN, mods: b[3], flags: b[0] }
        }
        UTF8 => Event::Text(String::from_utf8_lossy(&p[8..]).into_owned()),
        _ => return None,
    })
}

/// what a platform does with input: the console's mouse and keyboard
pub trait Inject: Send + Sync {
    /// a point on the captured display, each of x and y from 0 to 1
    fn mouse_abs(&self, x: f32, y: f32);
    fn mouse_rel(&self, dx: i32, dy: i32);
    fn button(&self, button: u8, down: bool);
    fn scroll(&self, amount: i32);
    fn hscroll(&self, amount: i32);
    fn key(&self, vk: u8, down: bool);
    fn text(&self, s: &str);
}

const REPEAT_DELAY: Duration = Duration::from_millis(500);
const REPEAT_PERIOD: Duration = Duration::from_micros(40_160);

const VK_SHIFT: u8 = 0x10;
const VK_CONTROL: u8 = 0x11;
const VK_MENU: u8 = 0x12;

fn modifier_bit(vk: u8) -> u8 {
    match vk {
        0x10 | 0xa0 | 0xa1 => 1,
        0x11 | 0xa2 | 0xa3 => 2,
        0x12 | 0xa4 | 0xa5 => 4,
        0x5b | 0x5c => 8,
        _ => 0,
    }
}

#[derive(Default)]
struct State {
    keys: HashSet<u8>,
    buttons: [bool; 6],
    /// the modifiers held by their own key events
    held: u8,
    /// the key that repeats, its modifiers to put around it, when it next does
    repeat: Option<(u8, u8, Instant)>,
    closed: bool,
}

/// one viewer's input to one platform: held keys and buttons tracked, a held key
/// repeated as a keyboard would, all of it let go on reset
pub struct Input {
    sink: Arc<dyn Inject>,
    state: Arc<(Mutex<State>, Condvar)>,
}

impl Input {
    pub fn new(sink: Arc<dyn Inject>) -> Input {
        let state: Arc<(Mutex<State>, Condvar)> = Arc::default();
        let (s2, sink2) = (state.clone(), sink.clone());
        thread::Builder::new().name("key-repeat".into()).spawn(move || repeat(&s2, &*sink2)).expect("a thread");
        Input { sink, state }
    }

    pub fn handle(&self, packet: &[u8]) {
        if let Some(e) = parse(packet) {
            self.event(e);
        }
    }

    pub fn event(&self, e: Event) {
        let (lock, cv) = &*self.state;
        let mut s = lock.lock().unwrap();
        match e {
            Event::MouseAbs { x, y, w, h } => {
                if w <= 0 || h <= 0 || x < 0 || y < 0 || x > w || y > h {
                    return;
                }
                self.sink.mouse_abs(x as f32 / w as f32, y as f32 / h as f32);
            }
            Event::MouseRel { dx, dy } => self.sink.mouse_rel(dx as i32, dy as i32),
            Event::Button { button, down } => {
                if !(1..=5).contains(&button) || s.buttons[button as usize] == down {
                    return;
                }
                s.buttons[button as usize] = down;
                self.sink.button(button, down);
            }
            Event::Scroll(a) => self.sink.scroll(a as i32),
            Event::HScroll(a) => self.sink.hscroll(a as i32),
            Event::Text(t) => self.sink.text(&t),
            Event::Key { vk, down, mods, .. } => {
                if vk == 0 {
                    return;
                }
                let bit = modifier_bit(vk);
                if down == s.keys.contains(&vk) {
                    // pressed again while down: the repeat's job; released while up: nothing to do
                    return;
                }
                if down {
                    s.keys.insert(vk);
                    s.held |= bit;
                } else {
                    s.keys.remove(&vk);
                    if bit != 0 && !s.keys.iter().any(|&k| modifier_bit(k) == bit) {
                        s.held &= !bit;
                    }
                }
                // the modifiers the packet says are down that no key event pressed
                let synth = if down && bit == 0 { mods & !s.held & 0x07 } else { 0 };
                press(&*self.sink, vk, down, synth);
                if down {
                    s.repeat = Some((vk, synth, Instant::now() + REPEAT_DELAY));
                    cv.notify_one();
                } else if s.repeat.is_some_and(|r| r.0 == vk) {
                    s.repeat = None;
                }
            }
        }
    }

    /// nothing left held down on the host
    pub fn reset(&self) {
        let mut s = self.state.0.lock().unwrap();
        s.repeat = None;
        for vk in std::mem::take(&mut s.keys) {
            self.sink.key(vk, false);
        }
        s.held = 0;
        for b in 1..s.buttons.len() {
            if std::mem::take(&mut s.buttons[b]) {
                self.sink.button(b as u8, false);
            }
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.reset();
        self.state.0.lock().unwrap().closed = true;
        self.state.1.notify_one();
    }
}

fn press(sink: &dyn Inject, vk: u8, down: bool, synth: u8) {
    let mods = [(1, VK_SHIFT), (2, VK_CONTROL), (4, VK_MENU)];
    for &(bit, m) in &mods {
        if synth & bit != 0 {
            sink.key(m, true);
        }
    }
    sink.key(vk, down);
    for &(bit, m) in mods.iter().rev() {
        if synth & bit != 0 {
            sink.key(m, false);
        }
    }
}

fn repeat(state: &(Mutex<State>, Condvar), sink: &dyn Inject) {
    let (lock, cv) = state;
    let mut s = lock.lock().unwrap();
    loop {
        if s.closed {
            return;
        }
        match s.repeat {
            None => s = cv.wait(s).unwrap(),
            Some((vk, synth, at)) => {
                let now = Instant::now();
                if now < at {
                    s = cv.wait_timeout(s, at - now).unwrap().0;
                    continue;
                }
                press(sink, vk, true, synth);
                s.repeat = Some((vk, synth, now + REPEAT_PERIOD));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // packets as src/afterglow/input.ts's pack writes them
    fn packet(magic: u32, body: &[u8]) -> Vec<u8> {
        let mut b = (4 + body.len() as u32).to_be_bytes().to_vec();
        b.extend_from_slice(&magic.to_le_bytes());
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn packets_parsed() {
        let abs = packet(MOUSE_ABS, &[0x02, 0x80, 0x01, 0x68, 0, 0, 0x04, 0xff, 0x02, 0xcf]);
        assert_eq!(parse(&abs), Some(Event::MouseAbs { x: 640, y: 360, w: 1279, h: 719 }));
        assert_eq!(parse(&packet(BUTTON_DOWN, &[3])), Some(Event::Button { button: 3, down: true }));
        assert_eq!(parse(&packet(SCROLL, &[0xff, 0x88, 0xff, 0x88, 0, 0])), Some(Event::Scroll(-120)));
        assert_eq!(parse(&packet(HSCROLL, &[0, 120])), Some(Event::HScroll(120)));
        assert_eq!(
            parse(&packet(KEY_DOWN, &[0, 0x41, 0x80, 0x01, 0, 0])),
            Some(Event::Key { vk: 0x41, down: true, mods: 1, flags: 0 })
        );
        assert_eq!(parse(&packet(UTF8, "é!".as_bytes())), Some(Event::Text("é!".into())));
        // cut short, or an unknown kind
        assert_eq!(parse(&abs[..12]), None);
        assert_eq!(parse(&packet(0x99, &[0; 4])), None);
        assert_eq!(parse(&packet(MOUSE_REL, &[0])), None);
    }

    #[derive(Default)]
    struct Rec(Mutex<Vec<String>>);
    impl Inject for Rec {
        fn mouse_abs(&self, x: f32, y: f32) {
            self.0.lock().unwrap().push(format!("abs {x:.2} {y:.2}"));
        }
        fn mouse_rel(&self, dx: i32, dy: i32) {
            self.0.lock().unwrap().push(format!("rel {dx} {dy}"));
        }
        fn button(&self, b: u8, down: bool) {
            self.0.lock().unwrap().push(format!("button {b} {down}"));
        }
        fn scroll(&self, a: i32) {
            self.0.lock().unwrap().push(format!("scroll {a}"));
        }
        fn hscroll(&self, a: i32) {
            self.0.lock().unwrap().push(format!("hscroll {a}"));
        }
        fn key(&self, vk: u8, down: bool) {
            self.0.lock().unwrap().push(format!("key {vk:#x} {down}"));
        }
        fn text(&self, s: &str) {
            self.0.lock().unwrap().push(format!("text {s}"));
        }
    }

    fn take(r: &Rec) -> Vec<String> {
        std::mem::take(&mut *r.0.lock().unwrap())
    }

    #[test]
    fn keys_held_repeated_synthesized_and_released_on_reset() {
        let rec = Arc::new(Rec::default());
        let input = Input::new(rec.clone());
        input.event(Event::MouseAbs { x: 640, y: 360, w: 1280, h: 720 });
        input.event(Event::Button { button: 1, down: true });
        input.event(Event::Button { button: 1, down: true });
        // shift said by the packet, never pressed: put around the key
        input.event(Event::Key { vk: 0x41, down: true, mods: 1, flags: 0 });
        input.event(Event::Key { vk: 0x41, down: true, mods: 1, flags: 0 });
        assert_eq!(take(&rec), ["abs 0.50 0.50", "button 1 true", "key 0x10 true", "key 0x41 true", "key 0x10 false"]);
        // held past the delay: it repeats
        thread::sleep(REPEAT_DELAY + REPEAT_PERIOD * 2 + Duration::from_millis(20));
        let got = take(&rec);
        assert!(got.len() >= 6, "{got:?}");
        assert_eq!(&got[..3], ["key 0x10 true", "key 0x41 true", "key 0x10 false"]);
        // a shift pressed by its own key: no synthesized one
        input.event(Event::Key { vk: 0xa0, down: true, mods: 1, flags: 0 });
        input.event(Event::Key { vk: 0x42, down: true, mods: 1, flags: 0 });
        // the rest of a repeat of 0x41 that was going out as the last take came can land
        // first (its three events aren't one push): only the new keys are this one's
        let mut got = take(&rec);
        got.retain(|e| !matches!(e.as_str(), "key 0x10 true" | "key 0x41 true" | "key 0x10 false"));
        assert_eq!(got, ["key 0xa0 true", "key 0x42 true"]);
        input.reset();
        let mut got = take(&rec);
        got.sort();
        assert_eq!(got, ["button 1 false", "key 0x41 false", "key 0x42 false", "key 0xa0 false"]);
        // released already: nothing
        input.event(Event::Key { vk: 0x41, down: false, mods: 0, flags: 0 });
        thread::sleep(REPEAT_DELAY + Duration::from_millis(50));
        assert!(take(&rec).is_empty());
    }
}
