//! A unit (one access unit's messages, as the link would write them) cut into
//! datagrams, with parity that lets a viewer rebuild what was lost without asking: no
//! round trip for a lost packet unless more were lost than the parity covers.
//!
//! Every datagram is a 14-byte head, then a piece:
//!
//! ```text
//! u8 kind | u8 flags | u16 seq | u16 idx | u16 k | u16 r | u32 len    (little-endian)
//! ```
//!
//! `k` data pieces of `p = ceil(len / k)` bytes each (the last shorter), then `r`
//! parity pieces of `p` bytes: parity `j` is the XOR of every data piece `i` with
//! `i % r == j`, the last one zero-padded. So any run of up to `r` lost data pieces is
//! rebuilt, and in general any loss that leaves at most one piece missing in each
//! class with its parity there. A unit of one piece has one parity piece: itself again.

/// a data piece
pub const DATA: u8 = 0x01;
/// a parity piece
pub const PARITY: u8 = 0x02;
/// `[kind][0][u16 seq]`: the newest unit sent, so a unit lost whole is noticed
pub const SYNC: u8 = 0x04;
/// `[kind][0][u32 n]`: a viewer's ping answered
pub const PONG: u8 = 0x05;

/// the unit starts with a keyframe: a viewer waiting for one starts here
pub const FLAG_KEY: u8 = 1;
/// sent again, asked for
pub const FLAG_RESENT: u8 = 2;

pub const HEAD: usize = 14;

/// how a unit of `len` bytes is cut for pieces of at most `max` bytes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub len: usize,
    pub k: usize,
    pub p: usize,
}

impl Plan {
    pub fn new(len: usize, max: usize) -> Plan {
        let k = len.div_ceil(max.max(1)).max(1);
        Plan { len, k, p: len.div_ceil(k).max(1) }
    }

    /// data piece i's bytes within the unit
    pub fn range(&self, i: usize) -> std::ops::Range<usize> {
        let a = (i * self.p).min(self.len);
        a..((i + 1) * self.p).min(self.len)
    }
}

/// how many parity pieces a unit of k pieces gets, at this loss (a fraction of
/// pieces, as the viewer last told it): one for a small unit, about one in sixteen for a
/// big one with no loss seen, more as loss rises; never more than half of k
pub fn parity_count(k: usize, loss: f64) -> usize {
    if k <= 4 {
        return 1;
    }
    let f = 1.0 / 16.0 + 3.0 * loss.clamp(0.0, 0.2);
    ((k as f64 * f).ceil() as usize).clamp(1, (k / 2).max(1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub kind: u8,
    pub flags: u8,
    pub seq: u16,
    pub idx: u16,
    pub k: u16,
    pub r: u16,
    pub len: u32,
}

impl Head {
    pub fn parse(d: &[u8]) -> Option<Head> {
        if d.len() < HEAD || !matches!(d[0], DATA | PARITY) {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
        let h = Head {
            kind: d[0],
            flags: d[1],
            seq: u16_at(2),
            idx: u16_at(4),
            k: u16_at(6),
            r: u16_at(8),
            len: u32::from_le_bytes(d[10..14].try_into().ok()?),
        };
        let ok = h.k > 0
            && h.r > 0
            && (h.idx < if h.kind == DATA { h.k } else { h.r })
            && h.len as usize >= h.k as usize
            && (h.len as usize).div_ceil(h.k as usize) <= 65_535;
        ok.then_some(h)
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.kind);
        out.push(self.flags);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.idx.to_le_bytes());
        out.extend_from_slice(&self.k.to_le_bytes());
        out.extend_from_slice(&self.r.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
    }

    pub fn plan(&self) -> Plan {
        let len = self.len as usize;
        let k = self.k as usize;
        Plan { len, k, p: len.div_ceil(k).max(1) }
    }
}

/// data piece i of a unit, head and all
pub fn data(unit: &[u8], plan: &Plan, r: usize, seq: u16, flags: u8, i: usize) -> Vec<u8> {
    let b = &unit[plan.range(i)];
    let mut out = Vec::with_capacity(HEAD + b.len());
    head(DATA, flags, seq, i, plan, r).write(&mut out);
    out.extend_from_slice(b);
    out
}

/// every parity piece of a unit, heads and all
pub fn parity(unit: &[u8], plan: &Plan, r: usize, seq: u16, flags: u8) -> Vec<Vec<u8>> {
    (0..r)
        .map(|j| {
            let mut out = Vec::with_capacity(HEAD + plan.p);
            head(PARITY, flags, seq, j, plan, r).write(&mut out);
            out.resize(HEAD + plan.p, 0);
            let acc = &mut out[HEAD..];
            for i in (j..plan.k).step_by(r) {
                xor(acc, &unit[plan.range(i)]);
            }
            out
        })
        .collect()
}

fn head(kind: u8, flags: u8, seq: u16, idx: usize, plan: &Plan, r: usize) -> Head {
    Head { kind, flags, seq, idx: idx as u16, k: plan.k as u16, r: r as u16, len: plan.len as u32 }
}

/// acc ^= b, b no longer than acc; eight bytes at a time
pub fn xor(acc: &mut [u8], b: &[u8]) {
    let n = b.len() & !7;
    for (a, b) in acc[..n].chunks_exact_mut(8).zip(b[..n].chunks_exact(8)) {
        let v = u64::from_ne_bytes(a.try_into().unwrap()) ^ u64::from_ne_bytes(b.try_into().unwrap());
        a.copy_from_slice(&v.to_ne_bytes());
    }
    for (a, b) in acc[n..b.len()].iter_mut().zip(&b[n..]) {
        *a ^= b;
    }
}

/// one unit put back together from what arrives: data pieces kept in place, parity
/// used to rebuild a class's one missing piece
pub struct Assembly {
    pub flags: u8,
    pub plan: Plan,
    pub r: usize,
    buf: Vec<u8>,
    have: Vec<bool>,
    parity: Vec<Option<Vec<u8>>>,
    missing: usize,
    /// pieces that arrived, data and parity, sent again or not
    pub pieces: usize,
}

impl Assembly {
    pub fn new(h: &Head) -> Assembly {
        let plan = h.plan();
        Assembly {
            flags: h.flags,
            plan,
            r: h.r as usize,
            buf: vec![0; plan.len],
            have: vec![false; plan.k],
            parity: vec![None; h.r as usize],
            missing: plan.k,
            pieces: 0,
        }
    }

    /// a piece of this unit; true once the unit is whole
    pub fn put(&mut self, h: &Head, piece: &[u8]) -> bool {
        if h.plan() != self.plan || h.r as usize != self.r {
            return self.done();
        }
        self.pieces += 1;
        let i = h.idx as usize;
        if h.kind == DATA {
            let r = self.plan.range(i);
            if !self.have[i] && piece.len() == r.len() {
                self.buf[r].copy_from_slice(piece);
                self.have[i] = true;
                self.missing -= 1;
            }
        } else if self.parity[i].is_none() && piece.len() == self.plan.p {
            self.parity[i] = Some(piece.to_vec());
        }
        if !self.done() {
            self.rebuild();
        }
        self.done()
    }

    pub fn done(&self) -> bool {
        self.missing == 0
    }

    /// each class missing one piece, with its parity here, rebuilt
    fn rebuild(&mut self) {
        for j in 0..self.r {
            let Some(par) = &self.parity[j] else { continue };
            let mut gone = None;
            let mut n = 0;
            for i in (j..self.plan.k).step_by(self.r) {
                if !self.have[i] {
                    gone = Some(i);
                    n += 1;
                }
            }
            let (1, Some(g)) = (n, gone) else { continue };
            let mut acc = par.clone();
            for i in (j..self.plan.k).step_by(self.r) {
                if i != g {
                    xor(&mut acc, &self.buf[self.plan.range(i)]);
                }
            }
            let r = self.plan.range(g);
            let n = r.len();
            self.buf[r].copy_from_slice(&acc[..n]);
            self.have[g] = true;
            self.missing -= 1;
        }
    }

    /// the data pieces that parity can't rebuild as things stand: what to ask for
    pub fn wanted(&self) -> Vec<u16> {
        let mut out = Vec::new();
        for j in 0..self.r {
            let gone: Vec<usize> = (j..self.plan.k).step_by(self.r).filter(|&i| !self.have[i]).collect();
            let skip = usize::from(self.parity[j].is_some() && !gone.is_empty());
            out.extend(gone.into_iter().skip(skip).map(|i| i as u16));
        }
        out.sort_unstable();
        out
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    fn cut(u: &[u8], max: usize, r: usize) -> Vec<Vec<u8>> {
        let plan = Plan::new(u.len(), max);
        let mut out: Vec<Vec<u8>> = (0..plan.k).map(|i| data(u, &plan, r, 9, FLAG_KEY, i)).collect();
        out.extend(parity(u, &plan, r, 9, FLAG_KEY));
        out
    }

    fn assemble(ds: &[Vec<u8>], lose: impl Fn(usize) -> bool) -> Assembly {
        let first = Head::parse(&ds[0]).unwrap();
        let mut a = Assembly::new(&first);
        for (n, d) in ds.iter().enumerate() {
            if !lose(n) {
                let h = Head::parse(d).unwrap();
                a.put(&h, &d[HEAD..]);
            }
        }
        a
    }

    #[test]
    fn plans_cut_evenly() {
        assert_eq!(Plan::new(1, 1200), Plan { len: 1, k: 1, p: 1 });
        assert_eq!(Plan::new(1200, 1200), Plan { len: 1200, k: 1, p: 1200 });
        // 1201 bytes: two pieces of 601 and 600, not 1200 and 1
        let p = Plan::new(1201, 1200);
        assert_eq!((p.k, p.p), (2, 601));
        assert_eq!(p.range(1), 601..1201);
        let p = Plan::new(150_000, 1186);
        assert!(p.p <= 1186 && p.range(p.k - 1).end == 150_000 && !p.range(p.k - 1).is_empty());
    }

    #[test]
    fn parity_grows_with_size_and_loss() {
        assert_eq!(parity_count(1, 0.0), 1);
        assert_eq!(parity_count(4, 0.3), 1);
        assert_eq!(parity_count(32, 0.0), 2);
        assert_eq!(parity_count(128, 0.0), 8);
        assert_eq!(parity_count(128, 0.05), 28);
        assert_eq!(parity_count(10, 0.2), 5);
    }

    #[test]
    fn heads_round_trip_and_nonsense_is_refused() {
        let d = cut(&unit(3000), 1000, 2);
        let h = Head::parse(&d[1]).unwrap();
        assert_eq!(h, Head { kind: DATA, flags: FLAG_KEY, seq: 9, idx: 1, k: 3, r: 2, len: 3000 });
        assert_eq!(Head::parse(&d[4]).unwrap().kind, PARITY);
        assert!(Head::parse(&d[0][..13]).is_none());
        let mut bad = d[0].clone();
        bad[4] = 3; // idx past k
        assert!(Head::parse(&bad).is_none());
        let mut bad = d[0].clone();
        bad[0] = SYNC;
        assert!(Head::parse(&bad).is_none());
    }

    #[test]
    fn whole_without_loss() {
        let u = unit(5000);
        let a = assemble(&cut(&u, 1186, 2), |_| false);
        assert!(a.done());
        assert_eq!(a.bytes(), &u[..]);
    }

    #[test]
    fn one_piece_lost_comes_back_from_its_copy() {
        let u = unit(300);
        let ds = cut(&u, 1186, 1);
        assert_eq!(ds.len(), 2);
        let a = assemble(&ds, |n| n == 0);
        assert!(a.done());
        assert_eq!(a.bytes(), &u[..]);
    }

    #[test]
    fn a_burst_as_long_as_the_parity_is_rebuilt() {
        let u = unit(40_000);
        let plan = Plan::new(u.len(), 1186);
        let r = 4;
        let ds = cut(&u, 1186, r);
        // four in a row from the middle, and the last data piece with them
        for start in [0, 7, plan.k - 4] {
            let a = assemble(&ds, |n| (start..start + r).contains(&n));
            assert!(a.done(), "a burst at {start}");
            assert_eq!(a.bytes(), &u[..]);
        }
    }

    #[test]
    fn too_much_lost_asks_for_just_what_parity_cant_rebuild() {
        let u = unit(10_000);
        let ds = cut(&u, 1000, 2);
        // pieces 0, 2 and 4 are all class 0; parity covers one of them
        let a = assemble(&ds, |n| [0, 2, 4].contains(&n));
        assert!(!a.done());
        assert_eq!(a.wanted(), vec![2, 4]);
        // and with that class's parity lost too, all three
        let a = assemble(&ds, |n| [0, 2, 4, 10].contains(&n));
        assert_eq!(a.wanted(), vec![0, 2, 4]);
    }

    #[test]
    fn asked_pieces_finish_it() {
        let u = unit(10_000);
        let ds = cut(&u, 1000, 2);
        let mut a = assemble(&ds, |n| [0, 2, 4].contains(&n));
        for i in a.wanted() {
            let d = &ds[i as usize];
            a.put(&Head::parse(d).unwrap(), &d[HEAD..]);
        }
        assert!(a.done());
        assert_eq!(a.bytes(), &u[..]);
    }

    #[test]
    fn xor_any_length() {
        let mut a = vec![0xffu8; 19];
        xor(&mut a, &[0x0f; 13]);
        assert_eq!(&a[..13], &[0xf0; 13]);
        assert_eq!(&a[13..], &[0xff; 6]);
    }
}
