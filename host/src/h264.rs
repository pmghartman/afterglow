//! H.264 bitstream work done once on the host: Annex B split into NAL units, the avcC
//! record, and every SPS rewritten so a decoder shows each frame the moment it's
//! decoded.
//!
//! Why the rewrite: an SPS without VUI bitstream_restriction leaves a decoder to infer
//! max_num_reorder_frames = MaxDpbFrames (H.264 E.2.1), and decoders that follow it
//! (Chrome's D3D11, VA-API and V4L2 paths, VideoToolbox) hold up to 16 decoded
//! pictures back before output: a still screen's last change sits in the DPB until
//! enough later frames push it out. Sunshine and Moonlight patch the same thing. We
//! write bitstream_restriction with max_num_reorder_frames = 0 and
//! max_dec_frame_buffering = max_num_ref_frames, which is true of every stream we make
//! (no B-frames), keeping everything else in the SPS bit for bit.

/// one NAL unit's bytes in an Annex B stream, its start code left out, trailing zeros too
pub fn split(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    split_into(b, &mut out);
    out
}

/// as split, into a list kept between calls
pub fn split_into<'a>(b: &'a [u8], out: &mut Vec<&'a [u8]>) {
    out.clear();
    let n = b.len();
    let mut i = start_code(b, 0);
    while i < n {
        let start = i + 3;
        let next = start_code(b, start);
        let mut end = next;
        if next < n {
            // a 4-byte start code's leading zero is its own, not the NAL's before
            if end > start && b[end - 1] == 0 {
                end -= 1;
            }
        } else {
            while end > start && b[end - 1] == 0 {
                end -= 1;
            }
        }
        if end > start {
            out.push(&b[start..end]);
        }
        i = next;
    }
}

fn start_code(b: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 3 <= b.len() {
        // most bytes aren't zero: step by the third byte's value
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

/// a NAL unit with its start code or length prefix already gone: its type
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// an avcC record from one SPS and one PPS: what VideoDecoder takes as description
pub fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
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

/// the payload of a NAL unit with emulation prevention bytes taken out
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// emulation prevention bytes put back
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.b.get(self.at / 8)?;
            v = (v << 1) | ((byte >> (7 - self.at % 8)) & 1) as u32;
            self.at += 1;
        }
        Some(v)
    }
    fn flag(&mut self) -> Option<bool> {
        self.u(1).map(|v| v == 1)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.u(zeros)? as u64) as u32)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

#[derive(Default)]
struct Out {
    b: Vec<u8>,
    n: usize,
}

impl Out {
    fn u(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            if self.n % 8 == 0 {
                self.b.push(0);
            }
            if (v >> i) & 1 == 1 {
                *self.b.last_mut().unwrap() |= 0x80 >> (self.n % 8);
            }
            self.n += 1;
        }
    }
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        self.u(len - 1, 0);
        for i in (0..len).rev() {
            self.u(1, ((x >> i) & 1) as u32);
        }
    }
    /// bits [from, to) of b, as they are
    fn copy(&mut self, b: &[u8], from: usize, to: usize) {
        for i in from..to {
            self.u(1, ((b[i / 8] >> (7 - i % 8)) & 1) as u32);
        }
    }
}

fn scaling_list(r: &mut Bits, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn hrd(r: &mut Bits) -> Option<()> {
    let cpbs = r.ue()? + 1;
    if cpbs > 32 {
        return None;
    }
    r.u(8)?;
    for _ in 0..cpbs {
        r.ue()?;
        r.ue()?;
        r.flag()?;
    }
    r.u(20)?;
    Some(())
}

/// where an SPS's VUI is, as far as the rewrite needs
struct Walk {
    refs: u32,
    /// the bit of vui_parameters_present_flag
    vui_at: usize,
    /// the bit of bitstream_restriction_flag, with a VUI
    restriction_at: Option<usize>,
    /// a restriction there: max_num_reorder_frames, the bits it takes, where the
    /// restriction ends
    restriction: Option<(u32, usize, usize, usize)>,
}
fn walk(rbsp: &[u8]) -> Option<Walk> {
    let r = &mut Bits { b: rbsp, at: 8 };
    let profile = r.u(8)?;
    r.u(16)?;
    r.ue()?;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.flag()?;
        }
        r.ue()?;
        r.ue()?;
        r.flag()?;
        if r.flag()? {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.flag()? {
                    scaling_list(r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?;
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.flag()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()?.min(255) {
                r.se()?;
            }
        }
        _ => {}
    }
    let refs = r.ue()?;
    r.flag()?;
    r.ue()?;
    r.ue()?;
    if !r.flag()? {
        r.flag()?;
    }
    r.flag()?;
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let vui_at = r.at;
    if !r.flag()? {
        return Some(Walk { refs, vui_at, restriction_at: None, restriction: None });
    }
    if r.flag()? && r.u(8)? == 255 {
        r.u(32)?;
    }
    if r.flag()? {
        r.flag()?;
    }
    if r.flag()? {
        r.u(4)?;
        if r.flag()? {
            r.u(24)?;
        }
    }
    if r.flag()? {
        r.ue()?;
        r.ue()?;
    }
    if r.flag()? {
        r.u(32)?;
        r.u(32)?;
        r.flag()?;
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        hrd(r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.flag()?;
    }
    r.flag()?;
    let restriction_at = Some(r.at);
    let mut restriction = None;
    if r.flag()? {
        r.flag()?;
        for _ in 0..4 {
            r.ue()?;
        }
        let from = r.at;
        let reorder = r.ue()?;
        let to = r.at;
        r.ue()?;
        restriction = Some((reorder, from, to, r.at));
    }
    Some(Walk { refs, vui_at, restriction_at, restriction })
}

/// the SPS NAL unit rewritten to say no frame is held back for reordering; None when
/// it says so already or can't be read (then it's sent as it is)
pub fn low_delay_sps(nal: &[u8]) -> Option<Vec<u8>> {
    if nal_type(nal) != 7 {
        return None;
    }
    let rbsp = unescape(nal);
    let wk = walk(&rbsp)?;
    let mut w = Out::default();
    match (wk.restriction, wk.restriction_at) {
        (Some((0, ..)), _) => return None,
        // a restriction that says frames are reordered: that one number made 0
        (Some((_, from, to, end)), _) => {
            w.copy(&rbsp, 0, from);
            w.ue(0);
            w.copy(&rbsp, to, end);
        }
        (None, at) => {
            match at {
                Some(at) => w.copy(&rbsp, 0, at),
                None => {
                    // no VUI: one with nothing in it but the restriction. Aspect,
                    // overscan, signal type, chroma location, timing, NAL and VCL HRD,
                    // pic struct: absent
                    w.copy(&rbsp, 0, wk.vui_at);
                    w.u(1, 1);
                    w.u(8, 0);
                }
            }
            // bitstream_restriction: vectors may cross the picture's edges, no size
            // limits, the widest vector ranges, nothing reordered, a DPB of the
            // references alone
            w.u(1, 1);
            w.u(1, 1);
            w.ue(0);
            w.ue(0);
            w.ue(15);
            w.ue(15);
            w.ue(0);
            w.ue(wk.refs.max(1));
        }
    }
    // rbsp_trailing_bits
    w.u(1, 1);
    while w.n % 8 != 0 {
        w.u(1, 0);
    }
    Some(escape(&w.b))
}

/// max_num_reorder_frames as an SPS says it; None when it doesn't (or can't be read)
pub fn reorder_frames(nal: &[u8]) -> Option<u32> {
    if nal_type(nal) != 7 {
        return None;
    }
    walk(&unescape(nal))?.restriction.map(|r| r.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_units_three_and_four_byte_start_codes_trailing_padding() {
        let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0, 0];
        assert_eq!(split(&s), vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 9, 9]]);
        assert!(split(&[1, 2, 3]).is_empty());
        assert!(split(&[]).is_empty());
    }

    #[test]
    fn escaping_round_trips() {
        let raw = [0x67, 0, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0xff];
        let esc = escape(&raw);
        assert_eq!(esc, [0x67, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0xff]);
        assert_eq!(unescape(&esc), raw);
    }

    #[test]
    fn exp_golomb_written_and_read() {
        let mut w = Out::default();
        for v in [0, 1, 2, 3, 7, 15, 255, 65535] {
            w.ue(v);
        }
        w.u(3, 0b101);
        let r = &mut Bits { b: &w.b, at: 0 };
        for v in [0, 1, 2, 3, 7, 15, 255, 65535] {
            assert_eq!(r.ue(), Some(v));
        }
        assert_eq!(r.u(3), Some(0b101));
    }

    // x264's, High at 1280x720 with two B-frames: a restriction saying 2 are reordered
    const X264_HIGH: [u8; 26] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00,
        0x00, 0x03, 0x03, 0xc0, 0xf1, 0x83, 0x19, 0x60,
    ];
    // High 4:4:4 with the JVT scaling matrices: the long way through the walk
    const X264_444: [u8; 27] = [
        0x67, 0xf4, 0x00, 0x1e, 0x91, 0x9b, 0x28, 0x14, 0x05, 0xff, 0x13, 0x80, 0x88, 0x00, 0x00, 0x03, 0x00, 0x08,
        0x00, 0x00, 0x03, 0x01, 0xe0, 0x78, 0xb1, 0x6c, 0xb0,
    ];

    #[test]
    fn a_restriction_that_reorders_made_zero_the_rest_kept() {
        for sps in [&X264_HIGH[..], &X264_444] {
            assert_eq!(reorder_frames(sps), Some(2));
            let out = low_delay_sps(sps).expect("rewritten");
            assert_eq!(&out[..4], &sps[..4]);
            assert_eq!(reorder_frames(&out), Some(0));
            // done once: a rewritten one is left alone
            assert!(low_delay_sps(&out).is_none());
        }
        // not an SPS
        assert!(low_delay_sps(&[0x68, 0xce, 0x3c, 0x80]).is_none());
    }
}
