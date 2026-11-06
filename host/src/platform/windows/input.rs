//! The viewer's input as the console's: SendInput on whichever desktop has the input
//! (the user's, or the secure one for UAC and the lock screen), keys as US-layout scan
//! codes, text as UTF-16 units.

use std::mem::size_of;
use std::sync::Mutex;

use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DF_ALLOWOTHERACCOUNTHOOK, OpenDesktopW,
    OpenInputDesktop, SetThreadDesktop,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};
use windows::core::w;

use crate::input::Inject;

/// a US-layout virtual key's scan code (Apollo's table, from Sunshine's)
#[rustfmt::skip]
static VK_TO_SCAN: [u8; 256] = [
    0x00, 0x00, 0x00, 0x46, 0x00, 0x00, 0x00, 0x00, 0x0e, 0x0f, 0x00, 0x00, 0x4c, 0x1c, 0x00, 0x00,
    0x2a, 0x1d, 0x38, 0x00, 0x3a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x39, 0x49, 0x51, 0x4f, 0x47, 0x4b, 0x48, 0x4d, 0x50, 0x00, 0x00, 0x00, 0x54, 0x52, 0x53, 0x63,
    0x0b, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
    0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c, 0x5b, 0x5c, 0x5d, 0x00, 0x5f,
    0x52, 0x4f, 0x50, 0x51, 0x4b, 0x4c, 0x4d, 0x47, 0x48, 0x49, 0x37, 0x4e, 0x00, 0x4a, 0x53, 0x35,
    0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58, 0x64, 0x65, 0x66, 0x67,
    0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x76, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x45, 0x46, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x2a, 0x36, 0x1d, 0x1d, 0x38, 0x38, 0x6a, 0x69, 0x67, 0x68, 0x65, 0x66, 0x32, 0x20, 0x2e, 0x30,
    0x19, 0x10, 0x24, 0x22, 0x6c, 0x6d, 0x6b, 0x21, 0x00, 0x00, 0x27, 0x0d, 0x33, 0x0c, 0x34, 0x35,
    0x29, 0x73, 0x7e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1a, 0x2b, 0x1b, 0x28, 0x00,
    0x00, 0x00, 0x56, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x71, 0x5c, 0x7b, 0x00, 0x6f, 0x5a, 0x00,
    0x00, 0x5b, 0x00, 0x5f, 0x00, 0x5e, 0x00, 0x00, 0x00, 0x5d, 0x00, 0x62, 0x00, 0x00, 0x00, 0x00,];

/// the captured display, in virtual-desktop pixels: where absolute moves land
#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Default)]
pub struct WinInput {
    pub rect: Mutex<Rect>,
}

/// the calling thread moved to the desktop that has the input now (the user's, or the
/// secure one for UAC and the lock screen). False when it couldn't be: a thread that
/// owns a window or a hook stays where it is
pub fn sync_desktop() -> bool {
    // SAFETY: plain Win32 calls on handles this function owns
    unsafe {
        let Ok(d) = OpenInputDesktop(DF_ALLOWOTHERACCOUNTHOOK, false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) else {
            return false;
        };
        let ok = SetThreadDesktop(d).is_ok();
        let _ = CloseDesktop(d);
        ok
    }
}

/// the calling thread moved to the user's own desktop (Default), where the app's windows
/// are whatever has the input: the window follower's and a close's
pub fn user_desktop() -> Result<(), String> {
    // SAFETY: plain Win32 calls on handles this function owns
    unsafe {
        let d =
            OpenDesktopW(w!("Default"), DESKTOP_CONTROL_FLAGS(0), false, GENERIC_ALL.0).map_err(|e| e.to_string())?;
        let r = SetThreadDesktop(d).map_err(|e| e.to_string());
        let _ = CloseDesktop(d);
        r
    }
}

fn send(inputs: &[INPUT]) {
    // SAFETY: inputs are fully initialised INPUT structs
    unsafe {
        let n = SendInput(inputs, size_of::<INPUT>() as i32) as usize;
        if n < inputs.len() {
            // the input desktop changed under us (UAC, lock): follow it, and what didn't
            // go in once more (not what did, which would type it twice)
            sync_desktop();
            SendInput(&inputs[n..], size_of::<INPUT>() as i32);
        }
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn extended(vk: u8) -> bool {
    // https://learn.microsoft.com/windows/win32/inputdev/about-keyboard-input#extended-key-flag
    matches!(
        vk,
        0x5b | 0x5c | 0xa5 | 0xa3 | 0x2d | 0x2e | 0x24 | 0x23 | 0x21 | 0x22 | 0x26 | 0x28 | 0x25 | 0x27 | 0x6f | 0x5d
    )
}

impl WinInput {
    /// the displays woken, as a touch of the mouse wakes them: a move of nothing, so the
    /// pointer stays where it is and nothing is clicked or typed
    pub fn wake(&self) {
        send(&[mouse(MOUSEEVENTF_MOVE, 0, 0, 0)]);
    }
}

impl Inject for WinInput {
    fn mouse_abs(&self, x: f32, y: f32) {
        let r = *self.rect.lock().unwrap();
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        // SAFETY: GetSystemMetrics has no preconditions
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let px = r.left as f32 + x * (r.w - 1) as f32 - vx as f32;
        let py = r.top as f32 + y * (r.h - 1) as f32 - vy as f32;
        // 0..65535 across the whole virtual desktop
        let dx = (px * 65535.0 / (vw - 1) as f32).round() as i32;
        let dy = (py * 65535.0 / (vh - 1) as f32).round() as i32;
        send(&[mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, dx, dy, 0)]);
    }

    fn mouse_rel(&self, dx: i32, dy: i32) {
        send(&[mouse(MOUSEEVENTF_MOVE, dx, dy, 0)]);
    }

    fn button(&self, button: u8, down: bool) {
        let (flags, data) = match (button, down) {
            (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (1, false) => (MOUSEEVENTF_LEFTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (3, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (4, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1 as i32),
            (_, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2 as i32),
        };
        send(&[mouse(flags, 0, 0, data)]);
    }

    fn scroll(&self, amount: i32) {
        send(&[mouse(MOUSEEVENTF_WHEEL, 0, 0, amount)]);
    }

    fn hscroll(&self, amount: i32) {
        send(&[mouse(MOUSEEVENTF_HWHEEL, 0, 0, amount)]);
    }

    fn key(&self, vk: u8, down: bool) {
        let scan = VK_TO_SCAN[vk as usize] as u16;
        // as a scan code where there's one, for games that read those; else the key itself
        let (mut flags, wvk) = if scan != 0 { (KEYEVENTF_SCANCODE, 0) } else { (KEYBD_EVENT_FLAGS(0), vk as u16) };
        if extended(vk) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        send(&[keyboard(wvk, scan, flags)]);
    }

    fn text(&self, s: &str) {
        send(&typed(s));
    }
}

/// text as key events: each character pressed and let go before the next, a surrogate
/// pair's two units down together then up together. All of a string's characters down
/// first and then all up reads, to an app that looks at key state, as keys held at once
/// and repeating
fn typed(s: &str) -> Vec<INPUT> {
    let mut out = Vec::with_capacity(s.len() * 2);
    let mut units = [0u16; 2];
    for c in s.chars() {
        let us = c.encode_utf16(&mut units);
        out.extend(us.iter().map(|&u| keyboard(0, u, KEYEVENTF_UNICODE)));
        out.extend(us.iter().map(|&u| keyboard(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(i: &INPUT) -> (u16, bool) {
        // SAFETY: every INPUT typed() makes is a keyboard one
        let k = unsafe { i.Anonymous.ki };
        (k.wScan, k.dwFlags.contains(KEYEVENTF_KEYUP))
    }

    #[test]
    fn each_character_down_then_up_a_pair_together() {
        let got: Vec<_> = typed("ab😀").iter().map(said).collect();
        let (hi, lo) = (0xd83d, 0xde00);
        assert_eq!(
            got,
            [
                (b'a' as u16, false),
                (b'a' as u16, true),
                (b'b' as u16, false),
                (b'b' as u16, true),
                (hi, false),
                (lo, false),
                (hi, true),
                (lo, true)
            ]
        );
        assert!(typed("").is_empty());
    }
}
