//! afterglow's own H.264 encoder: what a frame costs to encode is what changed in it,
//! not how big it is.
//!
//! A screen mostly stands still. A general encoder still searches motion and decides
//! modes for every macroblock of every frame: OpenH264 took 3.7 ms at 1080p for a
//! frame where one glyph changed. Here the capture says which macroblocks it drew in
//! (XDamage, DXGI's dirty rects), those whose pixels really differ from what was last
//! coded are coded, and every other one is a P_Skip in a skip run, a few bits for any
//! number of them. A typed glyph is a few macroblocks: tens of microseconds at any
//! resolution.
//!
//! Each coded macroblock is either inter with a zero motion vector (the residual
//! against the reference: right for a change on what was there, and for refinement) or
//! Intra 16x16 (V, H, DC or plane), whichever is closer. No motion search: every
//! vector in the stream is zero, so every predicted vector is too. Constrained
//! Baseline, CAVLC, no deblocking (the reconstruction is prediction plus residual),
//! one reference, POC type 2: a decoder outputs each frame as it's decoded, and the
//! SPS says so (bitstream_restriction, max_num_reorder_frames = 0).
//!
//! Its reconstruction is kept bit for bit as a decoder makes it (tested against
//! FFmpeg's), so it can go on predicting from it forever with no drift. And because a
//! frame is cheap, quality comes in two steps: a change is coded at the qp the rate
//! allows, then once the screen is still, what was coded coarsely is coded again at a
//! fine qp, the residual being only the first coding's error. Text is on screen in
//! microseconds and sharp a frame or two later.
//!
//! A big change is split into slices of whole macroblock rows, balanced by the work in
//! them, encoded on as many threads.

mod bits;
mod mb;
mod tables;

use bits::{Bits, nal_into};
use mb::{Act, Band, Count, SliceHead};

/// a picture in I420 at the encoder's padded size: Y then U then V, rows of
/// `mbw * 16` and `mbw * 8` bytes
pub struct Frame<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Done {
    /// macroblocks coded because they changed, and again to sharpen them
    pub coded: usize,
    pub refined: usize,
    pub intra: usize,
    pub key: bool,
    /// whether there's a frame: none when nothing changed and none was asked for
    pub frame: bool,
    pub bytes: usize,
    pub slices: usize,
}

pub struct Opts {
    pub idr: bool,
    pub qp: u8,
    /// what's coded coarser than this is coded again at it, in this many bytes at most:
    /// whatever doesn't fit stays coarse for the next refinement
    pub refine: Option<(u8, usize)>,
    /// a frame even when nothing changed (a keepalive)
    pub always: bool,
}

pub struct Encoder {
    /// the picture's size, and in macroblocks
    pub w: usize,
    pub h: usize,
    pub mbw: usize,
    pub mbh: usize,
    rec: Vec<u8>,
    last: Vec<u8>,
    q: Vec<u8>,
    nnz: Vec<[u8; 24]>,
    stamp: Vec<u32>,
    tick: u32,
    todo: Vec<(u32, Act)>,
    dirty: Vec<bool>,
    frame_num: u16,
    idr_id: u32,
    threads: usize,
    sps: Vec<u8>,
    pps: Vec<u8>,
    rbsp: Vec<Bits>,
    nals: Vec<Vec<u8>>,
    n_nals: usize,
    started: bool,
}

/// a slice thread run ahead of the app's own: on a machine the app keeps busy (a
/// build, a game) a slice waiting its turn holds the whole frame back. On Windows the
/// video thread is MMCSS work and these threads don't inherit it: seen waiting up to a
/// second behind Visual Studio
fn urgent() {
    #[cfg(windows)]
    // SAFETY: this thread's own priority
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

/// the least H.264 level for this size and rate (A.3.1's MaxFS and MaxMBPS), up to 5.2
fn level(mbs: usize, fps: u32) -> u8 {
    let rate = mbs as u64 * fps as u64;
    for (lvl, fs, mbps) in [
        (30u8, 1620usize, 40500u64),
        (31, 3600, 108000),
        (32, 5120, 216000),
        (40, 8192, 245760),
        (42, 8704, 522240),
        (50, 22080, 589824),
        (51, 36864, 983040),
    ] {
        if mbs <= fs && rate <= mbps {
            return lvl;
        }
    }
    52
}

impl Encoder {
    /// w and h even, the picture's own size: it's coded in whole macroblocks and
    /// cropped back
    pub fn new(w: usize, h: usize, fps: u32, threads: usize) -> Encoder {
        let (mbw, mbh) = (w.div_ceil(16), h.div_ceil(16));
        let n = mbw * mbh;
        let size = n * 384;
        let mut e = Encoder {
            w,
            h,
            mbw,
            mbh,
            rec: vec![0; size],
            last: vec![0; size],
            q: vec![255; n],
            nnz: vec![[0; 24]; n],
            stamp: vec![0; n],
            tick: 0,
            todo: Vec::new(),
            dirty: vec![true; n],
            frame_num: 0,
            idr_id: 0,
            // more than 8 slices' threads buy nothing at these sizes and only crowd the app
            threads: threads.clamp(1, 8),
            sps: Vec::new(),
            pps: Vec::new(),
            rbsp: Vec::new(),
            nals: Vec::new(),
            n_nals: 0,
            started: false,
        };
        e.parameter_sets(fps);
        e
    }

    /// the padded picture's size in pixels: what a Frame's planes hold
    pub fn padded(&self) -> (usize, usize) {
        (self.mbw * 16, self.mbh * 16)
    }

    fn parameter_sets(&mut self, fps: u32) {
        let mut b = Bits::default();
        // Constrained Baseline: profile 66, constraint_set0 and 1
        b.put(8, 66);
        b.put(8, 0b1100_0000);
        b.put(8, level(self.mbw * self.mbh, fps) as u32);
        b.ue(0);
        // log2_max_frame_num 16, POC type 2, one reference, no gaps
        b.ue(12);
        b.ue(2);
        b.ue(1);
        b.flag(false);
        b.ue(self.mbw as u32 - 1);
        b.ue(self.mbh as u32 - 1);
        // frame_mbs_only, direct_8x8_inference
        b.put(2, 0b11);
        let (cx, cy) = ((self.mbw * 16 - self.w) / 2, (self.mbh * 16 - self.h) / 2);
        b.flag(cx > 0 || cy > 0);
        if cx > 0 || cy > 0 {
            b.ue(0);
            b.ue(cx as u32);
            b.ue(0);
            b.ue(cy as u32);
        }
        // VUI: BT.709 limited range, what capture converts to; no timing, no HRD
        b.flag(true);
        b.put(2, 0);
        b.flag(true);
        b.put(3, 5);
        b.flag(false);
        b.flag(true);
        b.put(24, 0x01_01_01);
        b.put(5, 0);
        // bitstream_restriction: nothing reordered, a DPB of the one reference
        b.flag(true);
        b.flag(true);
        b.ue(0);
        b.ue(0);
        b.ue(16);
        b.ue(16);
        b.ue(0);
        b.ue(1);
        b.trailing();
        nal_into(0x67, &b.buf, &mut self.sps);
        let mut b = Bits::default();
        b.ue(0);
        b.ue(0);
        // CAVLC, no field POC, one slice group, one reference each way
        b.put(2, 0);
        b.ue(0);
        b.ue(0);
        b.ue(0);
        // no weighted prediction
        b.put(3, 0);
        b.se(0);
        b.se(0);
        b.se(0);
        // deblocking_filter_control_present, no constrained intra, no redundant pictures
        b.put(3, 0b100);
        b.trailing();
        nal_into(0x68, &b.buf, &mut self.pps);
    }

    /// pixels (x, y, w, h) of the picture drawn in since the last frame
    pub fn mark(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if w == 0 || h == 0 {
            return;
        }
        let (x0, y0) = ((x / 16).min(self.mbw), (y / 16).min(self.mbh));
        let (x1, y1) = ((x + w).div_ceil(16).min(self.mbw), (y + h).div_ceil(16).min(self.mbh));
        for my in y0..y1 {
            self.dirty[my * self.mbw + x0..my * self.mbw + x1].fill(true);
        }
    }

    pub fn mark_all(&mut self) {
        self.dirty.fill(true);
    }

    /// macroblocks coded coarser than qp: what refinement has left to do
    pub fn coarse(&self, qp: u8) -> usize {
        self.q.iter().filter(|&&q| q > qp).count()
    }

    /// what the next frame has to say, as a cheap look tells it before coding: the
    /// macroblocks marked (all of them for a keyframe), and their activity, the sum
    /// over them of their luma's SAD to the reference or to its own mean, whichever is
    /// less (what inter or intra DC would leave to code). A big frame is sampled
    pub fn activity(&self, src: &Frame, idr: bool) -> (usize, f64) {
        let n = self.mbw * self.mbh;
        let ys = self.mbw * 16;
        let marked: Vec<usize> = if idr { (0..n).collect() } else { (0..n).filter(|&i| self.dirty[i]).collect() };
        let step = (marked.len() / 1024).max(1);
        let mut e = 0u64;
        for &i in marked.iter().step_by(step) {
            let (mx, my) = (i % self.mbw, i / self.mbw);
            let at = my * 16 * ys + mx * 16;
            let mut s = [0u8; 256];
            for y in 0..16 {
                s[y * 16..y * 16 + 16].copy_from_slice(&src.y[at + y * ys..at + y * ys + 16]);
            }
            let mean = (s.iter().map(|&v| v as u32).sum::<u32>() / 256) as u8;
            let flat = mb::sad(&s, &[mean; 256]);
            let ref_sad = if idr {
                u32::MAX
            } else {
                (0..16).map(|y| mb::sad(&s[y * 16..y * 16 + 16], &self.rec[at + y * ys..at + y * ys + 16])).sum()
            };
            e += flat.min(ref_sad) as u64;
        }
        (marked.len(), e as f64 * step as f64)
    }

    /// a frame: an IDR when asked (or the first), otherwise the marked macroblocks that
    /// changed and any refinement. Done.frame false: nothing to send, no state moved
    pub fn encode(&mut self, src: &Frame, o: &Opts) -> Done {
        let n = self.mbw * self.mbh;
        let idr = o.idr || !self.started;
        // what to code, in order: the marked macroblocks, and refinement's share of the
        // coarse ones. Nothing else is looked at
        self.todo.clear();
        if idr {
            self.todo.extend((0..n as u32).map(|i| (i, Act::Dirty)));
        } else {
            let mut refine = Vec::new();
            if let Some((rq, _)) = o.refine {
                refine.extend((0..n).filter(|&i| !self.dirty[i] && self.q[i] > rq).map(|i| i as u32));
            }
            let mut r = refine.into_iter().peekable();
            for (i, _) in self.dirty.iter().enumerate().filter(|(_, d)| **d) {
                while let Some(j) = r.next_if(|&j| (j as usize) < i) {
                    self.todo.push((j, Act::Refine));
                }
                self.todo.push((i as u32, Act::Dirty));
            }
            self.todo.extend(r.map(|j| (j, Act::Refine)));
        }
        let total = self.todo.len();
        let refines = self.todo.iter().filter(|t| t.1 == Act::Refine).count();
        if total == 0 && !o.always {
            return Done::default();
        }
        // bands of whole rows, about three a thread for a big frame so one heavy with text
        // doesn't keep the rest waiting; each a slice, taken by whichever thread is free
        let threads = (total / 192).clamp(1, self.threads);
        let want = if threads > 1 { threads * 3 } else { 1 };
        let per = total.div_ceil(want).max(1);
        let mut cuts = vec![0usize];
        let mut at = 0usize;
        while at < total {
            at += per;
            if at >= total {
                break;
            }
            let row = self.todo[at].0 as usize / self.mbw;
            if row > *cuts.last().unwrap() {
                cuts.push(row);
            }
            // past the rest of that row
            while at < total && self.todo[at].0 as usize / self.mbw == row {
                at += 1;
            }
        }
        cuts.push(self.mbh);
        cuts.dedup();
        let nb = cuts.len() - 1;
        if self.rbsp.len() < nb {
            self.rbsp.resize_with(nb, Bits::default);
        }
        self.tick = self.tick.wrapping_add(1);
        let head = SliceHead {
            tick: self.tick,
            idr,
            frame_num: if idr { 0 } else { self.frame_num },
            idr_id: self.idr_id,
            qp: o.qp.clamp(10, 51),
            refine_qp: o.refine.map_or(o.qp, |r| r.0).clamp(10, 51),
        };
        let (mbw, ys, cs) = (self.mbw, self.mbw * 16, self.mbw * 8);
        let luma = ys * self.mbh * 16;
        let chroma = cs * self.mbh * 8;
        let counts = {
            let (ry, rc) = self.rec.split_at_mut(luma);
            let (ru, rv) = rc.split_at_mut(chroma);
            let (ly, lc) = self.last.split_at_mut(luma);
            let (lu, lv) = lc.split_at_mut(chroma);
            let mut planes = (ry, ru, rv, ly, lu, lv, &mut self.q[..], &mut self.nnz[..], &mut self.stamp[..]);
            let mut jobs = Vec::with_capacity(nb);
            let mut todo = &mut self.todo[..];
            for (bi, rbsp) in self.rbsp.iter_mut().enumerate().take(nb) {
                let (r0, r1) = (cuts[bi], cuts[bi + 1]);
                let rows = r1 - r0;
                let (ry, ru, rv, ly, lu, lv, q, nnz, stamp) = planes;
                let (a, ry) = ry.split_at_mut(rows * 16 * ys);
                let (bu, ru) = ru.split_at_mut(rows * 8 * cs);
                let (bv, rv) = rv.split_at_mut(rows * 8 * cs);
                let (c, ly) = ly.split_at_mut(rows * 16 * ys);
                let (cu, lu) = lu.split_at_mut(rows * 8 * cs);
                let (cv, lv) = lv.split_at_mut(rows * 8 * cs);
                let (qq, q) = q.split_at_mut(rows * mbw);
                let (nn, nnz) = nnz.split_at_mut(rows * mbw);
                let (st, stamp) = stamp.split_at_mut(rows * mbw);
                planes = (ry, ru, rv, ly, lu, lv, q, nnz, stamp);
                // this band's share of the list, its indices made the band's own
                let k = todo.iter().position(|t| t.0 as usize >= r1 * mbw).unwrap_or(todo.len());
                let (mine, rest) = std::mem::take(&mut todo).split_at_mut(k);
                todo = rest;
                for t in mine.iter_mut() {
                    t.0 -= (r0 * mbw) as u32;
                }
                let refining = mine.iter().filter(|t| t.1 == Act::Refine).count();
                let refine_bytes = o.refine.map_or(usize::MAX, |(_, most)| {
                    if refines == 0 { most } else { (most as u128 * refining as u128 / refines as u128) as usize }
                });
                let band = Band {
                    mbw,
                    rows,
                    row0: r0,
                    ry: a,
                    ru: bu,
                    rv: bv,
                    ly: c,
                    lu: cu,
                    lv: cv,
                    sy: &src.y[r0 * 16 * ys..r1 * 16 * ys],
                    su: &src.u[r0 * 8 * cs..r1 * 8 * cs],
                    sv: &src.v[r0 * 8 * cs..r1 * 8 * cs],
                    todo: mine,
                    refine_bytes,
                    q: qq,
                    nnz: nn,
                    stamp: st,
                };
                jobs.push(std::sync::Mutex::new(Some((band, rbsp))));
            }
            let head = &head;
            let out: Vec<std::sync::Mutex<Count>> = (0..nb).map(|_| Default::default()).collect();
            let next = std::sync::atomic::AtomicUsize::new(0);
            let work = || {
                loop {
                    let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(job) = jobs.get(j) else { break };
                    let (mut band, rbsp) = job.lock().unwrap().take().expect("a band taken once");
                    *out[j].lock().unwrap() = band.slice(head, rbsp);
                }
            };
            if threads == 1 {
                work();
            } else {
                std::thread::scope(|s| {
                    for _ in 1..threads {
                        s.spawn(|| {
                            urgent();
                            work()
                        });
                    }
                    work();
                });
            }
            out.into_iter().map(|c| c.into_inner().unwrap()).collect::<Vec<_>>()
        };
        let mut d = Done { key: idr, frame: true, slices: nb, ..Done::default() };
        for c in &counts {
            d.coded += c.coded;
            d.refined += c.refined;
            d.intra += c.intra;
        }
        self.dirty.fill(false);
        if !idr && d.coded + d.refined == 0 && !o.always {
            // drawn over with what was there: every macroblock a skip, no frame needed,
            // and nothing a decoder holds has moved
            return Done::default();
        }
        // the NAL units: parameter sets with a keyframe, then the slices
        self.n_nals = 0;
        let header = if idr { 0x65 } else { 0x61 };
        let want = nb + if idr { 2 } else { 0 };
        if self.nals.len() < want {
            self.nals.resize_with(want, Vec::new);
        }
        if idr {
            self.nals[0].clone_from(&self.sps);
            self.nals[1].clone_from(&self.pps);
            self.n_nals = 2;
        }
        for rbsp in &self.rbsp[..nb] {
            nal_into(header, &rbsp.buf, &mut self.nals[self.n_nals]);
            self.n_nals += 1;
        }
        d.bytes = self.nals[..self.n_nals].iter().map(Vec::len).sum();
        self.dirty.fill(false);
        self.started = true;
        if idr {
            self.idr_id = (self.idr_id + 1) % 65536;
            self.frame_num = 1;
        } else {
            self.frame_num = self.frame_num.wrapping_add(1);
        }
        d
    }

    /// the last frame's NAL units, start codes left out
    pub fn nals(&self) -> Vec<&[u8]> {
        self.nals[..self.n_nals].iter().map(Vec::as_slice).collect()
    }

    /// the decoder's picture as it stands: Y, U, V at the padded size
    #[cfg(test)]
    pub fn recon(&self) -> &[u8] {
        &self.rec
    }
}

/// the finest a change is coded at first, and what refinement brings everything to:
/// what's coarser than QP_LEGIBLE first to QP_READ (cheap, and text reads), then all of
/// it to QP_FINE
pub const QP_FIRST: u8 = 22;
pub const QP_FINE: u8 = 14;
pub const QP_LEGIBLE: u8 = 32;
pub const QP_READ: u8 = 26;

/// the qp a frame is coded at, from what it has to say: a frame that would take much
/// more than its share of the bitrate on the wire is coarser, since its size is latency
/// too (the viewer sees nothing until all of it has arrived). Refinement then sharpens
/// it in frames that each keep to a frame's share
pub struct Rate {
    /// a frame's share of the bitrate, in bytes
    share: f64,
    /// bytes a unit of activity costs at qp 26, as seen lately
    k: f64,
}

/// what a coded macroblock costs whatever its content: its type, its pattern, its qp
const MB_BYTES: f64 = 1.5;

impl Rate {
    pub fn new(kbps: u32, fps: u32) -> Rate {
        Rate { share: kbps as f64 * 125.0 / fps.max(1) as f64, k: 0.02 }
    }

    /// a change over `mbs` macroblocks of this activity: the finest qp whose frame
    /// comes to two shares at most
    pub fn qp(&self, mbs: usize, activity: f64) -> u8 {
        let room = (self.share * 2.0 - MB_BYTES * mbs as f64).max(self.share * 0.25);
        let at26 = (activity * self.k).max(1.0);
        let qp = 26.0 + 6.0 * (at26 / room).log2();
        qp.ceil().clamp(QP_FIRST as f64, 51.0) as u8
    }

    /// a new bitrate, what's been learnt of the content kept
    pub fn retarget(&mut self, kbps: u32, fps: u32) {
        self.share = kbps as f64 * 125.0 / fps.max(1) as f64;
    }

    /// the bytes one refinement frame may take: a share
    pub fn refine_bytes(&self) -> usize {
        self.share as usize
    }

    /// what a frame came to, for the next ones
    pub fn seen(&mut self, d: &Done, qp: u8, activity: f64) {
        // learnt only from frames whose size is mostly their content, not each
        // macroblock's fixed cost; quicker to think frames bigger than smaller, since
        // one too big is late and one too small is only refined sooner
        let mix = |old: f64, new: f64| if new > old { 0.4 * old + 0.6 * new } else { 0.8 * old + 0.2 * new };
        if d.coded >= 4 && d.refined == 0 && activity > 256.0 {
            let content = d.bytes as f64 - MB_BYTES * d.coded as f64;
            if content > 0.25 * d.bytes as f64 {
                let k = content / activity * 2f64.powf((qp as f64 - 26.0) / 6.0);
                self.k = mix(self.k, k).clamp(1e-3, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests;
