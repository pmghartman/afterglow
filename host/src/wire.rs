//! The link's framing and the H.264 reshaping done once here, so the driver copies
//! each frame once. No platform code: tested on its own.
//!
//! A message: u32 little-endian length of what follows, one type byte, the body.
//! Frames and parameter sets go out shaped for the viewer: 8 bytes in front that the
//! driver overwrites with its own header (the type byte is the first of them), then
//! AVCC, length-prefixed NAL units, which is what WebCodecs takes.

/// driver to afterglow
pub mod input {
    pub const HELLO: u8 = 0x01;
    pub const LAUNCH: u8 = 0x02;
    pub const VIDEO: u8 = 0x03;
    pub const STOP_VIDEO: u8 = 0x04;
    pub const IDR: u8 = 0x05;
    pub const INPUT: u8 = 0x06;
    pub const QUIT: u8 = 0x07;
    pub const PING: u8 = 0x08;
}

/// afterglow to driver
pub mod output {
    pub const READY: u8 = 0x81;
    pub const LAUNCHED: u8 = 0x82;
    pub const EXITED: u8 = 0x83;
    pub const VIDEO_ENDED: u8 = 0x84;
    pub const PONG: u8 = 0x88;
    /// `[type][pad][u16 vgen][u16 w][u16 h]` then avcC
    pub const CONFIG: u8 = 0x90;
    /// `[type][key][u16 vgen][pad 4]` then AVCC
    pub const FRAME: u8 = 0x91;
}

/// room in front of a frame's AVCC: the viewer's own header length
pub const HEAD_LEN: usize = 8;
/// the largest message either side accepts: a 4K IDR at a high rate fits many times
pub const MAX_LEN: u32 = 64 << 20;

/// one whole message: length, type, body
pub fn message(ty: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.extend_from_slice(&(1 + body.len() as u32).to_le_bytes());
    out.push(ty);
    out.extend_from_slice(body);
    out
}

/// messages out of a byte stream as it arrives, whatever the chunking
#[derive(Default)]
pub struct Reader {
    buf: Vec<u8>,
    at: usize,
    bad: bool,
}

impl Reader {
    /// false: the stream is broken
    pub fn feed(&mut self, b: &[u8]) -> bool {
        self.buf.extend_from_slice(b);
        !self.bad
    }

    pub fn next_message(&mut self) -> Option<(u8, Vec<u8>)> {
        let have = self.buf.len() - self.at;
        if self.bad || have < 4 {
            self.compact();
            return None;
        }
        let len = u32::from_le_bytes(self.buf[self.at..self.at + 4].try_into().unwrap());
        if !(1..=MAX_LEN).contains(&len) {
            self.bad = true;
            return None;
        }
        let len = len as usize;
        if have < 4 + len {
            self.compact();
            return None;
        }
        let ty = self.buf[self.at + 4];
        let body = self.buf[self.at + 5..self.at + 4 + len].to_vec();
        self.at += 4 + len;
        Some((ty, body))
    }

    pub fn broken(&self) -> bool {
        self.bad
    }

    fn compact(&mut self) {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
    }
}

/// one NAL unit in an Annex B stream: where its bytes start (after the start code)
/// and end, its type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nal {
    pub start: usize,
    pub end: usize,
    pub ty: u8,
}

fn start_code(b: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 3 <= b.len() {
        // most bytes aren't zero: skip by the third byte's value
        if b[i + 2] > 1 {
            i += 3;
        } else if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            return i;
        } else {
            i += 1;
        }
    }
    b.len()
}

pub fn nals_of(b: &[u8]) -> Vec<Nal> {
    let n = b.len();
    let mut out = Vec::new();
    let mut i = start_code(b, 0);
    while i < n {
        let start = i + 3;
        let next = start_code(b, start);
        let mut end = next;
        // a 4-byte start code's leading zero belongs to it, not the NAL before
        if next < n && end > start && b[end - 1] == 0 {
            end -= 1;
        }
        // trailing zero padding isn't the NAL's
        if next >= n {
            while end > start && b[end - 1] == 0 {
                end -= 1;
            }
        }
        if end > start {
            out.push(Nal { start, end, ty: b[start] & 0x1f });
        }
        i = next;
    }
    out
}

/// an avcC record from one SPS and one PPS: what VideoDecoder takes as description
pub fn avcc_of(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + sps.len() + pps.len());
    let at = |i: usize| sps.get(i).copied().unwrap_or(0);
    out.extend_from_slice(&[1, at(1), at(2), at(3), 0xff, 0xe1]);
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

/// what one access unit becomes
#[derive(Default)]
pub struct Shaped {
    /// a config message to send first, empty when the parameter sets are as before
    pub config: Vec<u8>,
    /// the frame message, empty when the access unit held no picture
    pub frame: Vec<u8>,
}

/// H.264 access units in, the link's messages out: parameter sets as a config message
/// whenever they change, every other NAL unit of the frame in AVCC behind the viewer's
/// header room. Access unit delimiters are dropped, nothing else
#[derive(Default)]
pub struct Shaper {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl Shaper {
    pub fn shape(&mut self, b: &[u8], key: bool, vgen: u16, w: u16, h: u16) -> Shaped {
        let mut r = Shaped::default();
        let list = nals_of(b);
        let mut body = 0;
        let (mut sps, mut pps): (&[u8], &[u8]) = (&[], &[]);
        for u in &list {
            let v = &b[u.start..u.end];
            match u.ty {
                7 => sps = v,
                8 => pps = v,
                9 => {}
                _ => body += 4 + v.len(),
            }
        }
        if !sps.is_empty() && !pps.is_empty() && (sps != self.sps || pps != self.pps) {
            self.sps = sps.to_vec();
            self.pps = pps.to_vec();
            let rec = avcc_of(sps, pps);
            let mut cfg = vec![0u8; HEAD_LEN - 1];
            cfg[1..3].copy_from_slice(&vgen.to_le_bytes());
            cfg[3..5].copy_from_slice(&w.to_le_bytes());
            cfg[5..7].copy_from_slice(&h.to_le_bytes());
            cfg.extend_from_slice(&rec);
            r.config = message(output::CONFIG, &cfg);
        }
        if body == 0 {
            return r;
        }
        // [u32 len][type][key][u16 vgen][pad 4][AVCC]: the type byte starts the 8 bytes of room
        let mut f = Vec::with_capacity(4 + HEAD_LEN + body);
        f.extend_from_slice(&((HEAD_LEN + body) as u32).to_le_bytes());
        f.push(output::FRAME);
        f.push(key as u8);
        f.extend_from_slice(&vgen.to_le_bytes());
        f.extend_from_slice(&[0; 4]);
        for u in list.iter().filter(|u| !matches!(u.ty, 7..=9)) {
            f.extend_from_slice(&((u.end - u.start) as u32).to_be_bytes());
            f.extend_from_slice(&b[u.start..u.end]);
        }
        r.frame = f;
        r
    }
}

/// the token compared in constant time: a wrong guess learns nothing from timing
pub fn same_token(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// an app's command line as cmd.exe starts it, waited on so the stream follows the app
/// until it quits: a path the host's own desk listed (a .lnk, .exe, .url), never
/// anything with a quote or a control character in it
pub fn launcher_cmd(target: &str) -> Option<String> {
    if target.is_empty() || target.len() > 4096 || target.contains('"') || target.bytes().any(|c| c < 0x20) {
        return None;
    }
    Some(format!("cmd.exe /d /c start \"\" /wait \"{target}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8]) -> usize {
        u32::from_le_bytes(b[..4].try_into().unwrap()) as usize
    }

    #[test]
    fn messages_out_of_a_stream_however_cut() {
        let a = message(output::READY, b"{\"v\":1}");
        let b = message(input::IDR, b"");
        let s = [a, b].concat();
        let mut r = Reader::default();
        for c in &s {
            assert!(r.feed(std::slice::from_ref(c)));
        }
        assert_eq!(r.next_message(), Some((output::READY, b"{\"v\":1}".to_vec())));
        assert_eq!(r.next_message(), Some((input::IDR, vec![])));
        assert_eq!(r.next_message(), None);
    }

    #[test]
    fn a_length_past_the_limit_breaks_the_stream() {
        let mut r = Reader::default();
        r.feed(&[0xff, 0xff, 0xff, 0xff, 1]);
        assert_eq!(r.next_message(), None);
        assert!(r.broken());
        let mut r = Reader::default();
        r.feed(&[0, 0, 0, 0, 1]);
        assert_eq!(r.next_message(), None);
        assert!(r.broken());
    }

    #[test]
    fn nal_units_three_and_four_byte_start_codes_trailing_padding() {
        let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0, 0];
        let l = nals_of(&s);
        assert_eq!(
            l,
            vec![Nal { start: 4, end: 7, ty: 7 }, Nal { start: 10, end: 12, ty: 8 }, Nal { start: 16, end: 19, ty: 5 },]
        );
        assert!(nals_of(&[1, 2, 3]).is_empty());
        assert!(nals_of(&[]).is_empty());
    }

    #[test]
    fn an_idr_parameter_sets_once_aud_dropped_slice_behind_the_room() {
        let s = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, 0, 0,
            1, 0x65, 0x88, 0x84,
        ];
        let mut sh = Shaper::default();
        let r = sh.shape(&s, true, 7, 1280, 720);
        let c = &r.config;
        assert_eq!(u32_at(c), c.len() - 4);
        assert_eq!(c[4], output::CONFIG);
        assert_eq!(&c[6..12], &[7, 0, 0x00, 0x05, 0xd0, 0x02]);
        // avcC: version, profile, compat, level from the SPS
        assert_eq!(&c[12..16], &[1, 0x64, 0x00, 0x1f]);
        let f = &r.frame;
        assert_eq!(f.len(), 4 + HEAD_LEN + 4 + 3);
        assert_eq!(u32_at(f), HEAD_LEN + 7);
        assert_eq!(&f[4..8], &[output::FRAME, 1, 7, 0]);
        assert_eq!(&f[12..19], &[0, 0, 0, 3, 0x65, 0x88, 0x84]);
        // the same parameter sets again: no config
        assert!(sh.shape(&s, true, 7, 1280, 720).config.is_empty());
        // a fresh shaper sends them again
        assert!(!Shaper::default().shape(&s, true, 7, 1280, 720).config.is_empty());
        // a P-frame: two slices, no key
        let p = [0, 0, 1, 0x41, 1, 0, 0, 1, 0x41, 2, 3];
        let rp = sh.shape(&p, false, 7, 1280, 720);
        assert!(rp.config.is_empty());
        assert_eq!(rp.frame[5], 0);
        assert_eq!(rp.frame.len(), 4 + HEAD_LEN + (4 + 2) + (4 + 3));
        // parameter sets alone: a config, no frame
        let ps = &s[6..23];
        let only = Shaper::default().shape(ps, true, 1, 2, 2);
        assert!(!only.config.is_empty() && only.frame.is_empty());
    }

    #[test]
    fn tokens_and_launch_lines() {
        assert!(same_token(b"abc", b"abc"));
        assert!(!same_token(b"abc", b"abd"));
        assert!(!same_token(b"", b""));
        assert!(!same_token(b"abc", b"abcd"));
        assert_eq!(launcher_cmd("C:\\x\\App.lnk").unwrap(), "cmd.exe /d /c start \"\" /wait \"C:\\x\\App.lnk\"");
        assert!(launcher_cmd("a\" & calc").is_none());
        assert!(launcher_cmd("a\nb").is_none());
        assert!(launcher_cmd("").is_none());
    }
}
