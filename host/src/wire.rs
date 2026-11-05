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
    /// `[type][visible][u16 hot x][u16 hot y][u16 w][u16 h]` then a PNG
    pub const CURSOR: u8 = 0x92;
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

/// what one access unit became
pub struct Shaped<'a> {
    /// a config message to send first, empty when the parameter sets are as before
    pub config: &'a [u8],
    /// the frame message, empty when the access unit held no picture
    pub frame: &'a [u8],
}

/// H.264 access units in, the link's messages out: parameter sets as a config message
/// whenever they change (the SPS rewritten so nothing is held for reordering,
/// h264.rs), every other NAL unit of the frame in AVCC behind the viewer's header
/// room. Access unit delimiters and SEI are dropped. Its buffers are kept from frame to
/// frame: nothing allocated per frame once the first big one has been made
#[derive(Default)]
pub struct Shaper {
    /// the SPS as the encoder made it, and as it went out
    sps_in: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    config: Vec<u8>,
    frame: Vec<u8>,
}

impl Shaper {
    pub fn shape(&mut self, nals: &[&[u8]], key: bool, vgen: u16, w: u16, h: u16) -> Shaped<'_> {
        let (mut sps, mut pps): (&[u8], &[u8]) = (&[], &[]);
        let mut body = 0;
        for n in nals {
            match crate::h264::nal_type(n) {
                7 => sps = n,
                8 => pps = n,
                6 | 9 => {}
                _ => body += 4 + n.len(),
            }
        }
        self.config.clear();
        if !sps.is_empty() && !pps.is_empty() && (sps != self.sps_in || pps != self.pps) {
            self.sps_in = sps.to_vec();
            self.sps = crate::h264::low_delay_sps(sps).unwrap_or_else(|| sps.to_vec());
            self.pps = pps.to_vec();
            let rec = crate::h264::avcc(&self.sps, &self.pps);
            let c = &mut self.config;
            c.extend_from_slice(&((HEAD_LEN + rec.len()) as u32).to_le_bytes());
            c.push(output::CONFIG);
            c.push(0);
            c.extend_from_slice(&vgen.to_le_bytes());
            c.extend_from_slice(&w.to_le_bytes());
            c.extend_from_slice(&h.to_le_bytes());
            c.extend_from_slice(&rec);
        }
        self.frame.clear();
        if body > 0 {
            // [u32 len][type][key][u16 vgen][pad 4][AVCC]: the type byte starts the 8 bytes of room
            let f = &mut self.frame;
            f.reserve(4 + HEAD_LEN + body);
            f.extend_from_slice(&((HEAD_LEN + body) as u32).to_le_bytes());
            f.push(output::FRAME);
            f.push(key as u8);
            f.extend_from_slice(&vgen.to_le_bytes());
            f.extend_from_slice(&[0; 4]);
            for n in nals.iter().filter(|n| !matches!(crate::h264::nal_type(n), 6..=9)) {
                f.extend_from_slice(&(n.len() as u32).to_be_bytes());
                f.extend_from_slice(n);
            }
        }
        Shaped { config: &self.config, frame: &self.frame }
    }
}

/// the pointer's shape: `[type][u8 visible][u16 hot x][u16 hot y][u16 w][u16 h]` then a
/// PNG of it, none when hidden
pub fn cursor_message(c: &crate::host::Cursor) -> Vec<u8> {
    let png = if c.visible && c.w > 0 && c.h > 0 { crate::png::rgba(c.w, c.h, &c.rgba) } else { Vec::new() };
    let mut b = Vec::with_capacity(9 + png.len());
    b.push(c.visible as u8);
    for v in [c.hot_x, c.hot_y, c.w, c.h] {
        b.extend_from_slice(&(v.min(0xffff) as u16).to_le_bytes());
    }
    b.extend_from_slice(&png);
    message(output::CURSOR, &b)
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
    fn an_idr_parameter_sets_once_aud_dropped_slice_behind_the_room() {
        let s = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, 0, 0,
            1, 0x65, 0x88, 0x84,
        ];
        let nals = crate::h264::split(&s);
        let mut sh = Shaper::default();
        let r = sh.shape(&nals, true, 7, 1280, 720);
        let c = r.config.to_vec();
        assert_eq!(u32_at(&c), c.len() - 4);
        assert_eq!(c[4], output::CONFIG);
        assert_eq!(&c[6..12], &[7, 0, 0x00, 0x05, 0xd0, 0x02]);
        // avcC: version, profile, compat, level from the SPS
        assert_eq!(&c[12..16], &[1, 0x64, 0x00, 0x1f]);
        let f = r.frame;
        assert_eq!(f.len(), 4 + HEAD_LEN + 4 + 3);
        assert_eq!(u32_at(f), HEAD_LEN + 7);
        assert_eq!(&f[4..8], &[output::FRAME, 1, 7, 0]);
        assert_eq!(&f[12..19], &[0, 0, 0, 3, 0x65, 0x88, 0x84]);
        // the same parameter sets again: no config
        assert!(sh.shape(&nals, true, 7, 1280, 720).config.is_empty());
        // a fresh shaper sends them again
        assert!(!Shaper::default().shape(&nals, true, 7, 1280, 720).config.is_empty());
        // a P-frame: two slices, no key
        let p = crate::h264::split(&[0, 0, 1, 0x41, 1, 0, 0, 1, 0x41, 2, 3]);
        let rp = sh.shape(&p, false, 7, 1280, 720);
        assert!(rp.config.is_empty());
        assert_eq!(rp.frame[5], 0);
        assert_eq!(rp.frame.len(), 4 + HEAD_LEN + (4 + 2) + (4 + 3));
        // parameter sets alone: a config, no frame
        let ps = crate::h264::split(&s[6..23]);
        let mut fresh = Shaper::default();
        let only = fresh.shape(&ps, true, 1, 2, 2);
        assert!(!only.config.is_empty() && only.frame.is_empty());
    }

    #[test]
    fn a_cursor_with_its_shape_or_hidden() {
        let c = crate::host::Cursor { visible: true, w: 1, h: 1, hot_x: 3, hot_y: 4, rgba: vec![1, 2, 3, 4] };
        let m = cursor_message(&c);
        assert_eq!(m[4], output::CURSOR);
        assert_eq!(&m[5..14], &[1, 3, 0, 4, 0, 1, 0, 1, 0]);
        assert_eq!(&m[14..18], b"\x89PNG");
        let m = cursor_message(&crate::host::Cursor::default());
        assert_eq!(m.len(), 14);
        assert_eq!(m[5], 0);
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
