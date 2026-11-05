//! An RBSP written MSB first through a 64-bit accumulator, and its NAL unit with
//! emulation prevention bytes put in.

#[derive(Default)]
pub struct Bits {
    pub buf: Vec<u8>,
    acc: u64,
    /// bits held in acc, fewer than 8 between calls
    n: u32,
}

impl Bits {
    pub fn clear(&mut self) {
        self.buf.clear();
        self.acc = 0;
        self.n = 0;
    }

    /// the low `len` bits of v, 32 at most; written out 32 bits at a time
    #[inline]
    pub fn put(&mut self, len: u32, v: u32) {
        debug_assert!(len <= 32);
        if len == 0 {
            return;
        }
        self.acc = (self.acc << len) | (v as u64 & ((1u64 << len) - 1));
        self.n += len;
        if self.n >= 32 {
            self.n -= 32;
            self.buf.extend_from_slice(&((self.acc >> self.n) as u32).to_be_bytes());
        }
    }

    #[inline]
    pub fn flag(&mut self, b: bool) {
        self.put(1, b as u32);
    }

    /// unsigned Exp-Golomb
    #[inline]
    pub fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        if len <= 16 {
            self.put(2 * len - 1, x as u32);
        } else {
            self.put(len - 1, 0);
            self.put(len, x as u32);
        }
    }

    /// signed Exp-Golomb
    #[inline]
    pub fn se(&mut self, v: i32) {
        self.ue(if v > 0 { 2 * v as u32 - 1 } else { (-2 * v) as u32 });
    }

    /// rbsp_trailing_bits: the stop bit and zeros to the byte, everything written out
    pub fn trailing(&mut self) {
        self.put(1, 1);
        if self.n % 8 > 0 {
            self.put(8 - self.n % 8, 0);
        }
        while self.n >= 8 {
            self.n -= 8;
            self.buf.push((self.acc >> self.n) as u8);
        }
    }
}

/// a NAL unit: its header byte, then the RBSP with emulation prevention bytes put in
pub fn nal_into(header: u8, rbsp: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(rbsp.len() + rbsp.len() / 64 + 8);
    out.push(header);
    let mut zeros = 0u32;
    let mut i = 0;
    while i < rbsp.len() {
        // most of a slice has no zero bytes: copy up to the next one at once
        if zeros == 0 {
            let run = rbsp[i..].iter().position(|&b| b == 0).unwrap_or(rbsp.len() - i);
            out.extend_from_slice(&rbsp[i..i + run]);
            i += run;
            if i == rbsp.len() {
                break;
            }
        }
        let b = rbsp[i];
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_and_fixed_codes() {
        let mut b = Bits::default();
        b.ue(0); // 1
        b.ue(1); // 010
        b.ue(4); // 00101
        b.se(-1); // ue(2): 011
        b.put(3, 0b101);
        b.trailing();
        // 15 bits and the stop bit: two bytes, no padding
        assert_eq!(b.buf, [0b1010_0010, 0b1011_1011]);
        let mut b = Bits::default();
        b.ue(70000);
        b.trailing();
        // 70001 is 17 bits: 16 zeros, then it, then the stop bit, padded to 5 bytes
        assert_eq!(b.buf.len(), 5);
        assert_eq!(&b.buf[..2], &[0, 0]);
        assert_eq!(b.buf[2] >> 7, 1);
    }

    #[test]
    fn emulation_prevented() {
        let mut out = Vec::new();
        nal_into(0x65, &[1, 0, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 4, 0xff], &mut out);
        assert_eq!(out, [0x65, 1, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 4, 0xff]);
    }
}
