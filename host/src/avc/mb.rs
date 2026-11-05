//! One slice's macroblocks: each either skipped (P_Skip, a copy of the reference),
//! coded as inter with a zero motion vector (the residual against the reference), or
//! coded Intra 16x16 (V, H, DC or plane prediction). The residual is transformed,
//! quantised and written as CAVLC, and reconstructed exactly as a decoder will, into
//! the reference the next frame predicts from.

use super::bits::Bits;
use super::tables::*;

/// what a macroblock is asked to be this frame
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Act {
    /// left as the reference has it
    #[default]
    Skip,
    /// changed (if its pixels differ from what was last coded there): coded at the frame's qp
    Dirty,
    /// unchanged, coded again at a finer qp: the residual is the earlier coding's error
    Refine,
}

/// row * 4 + column of the 4x4 luma block at each z-order index
const Z2R: [usize; 16] = [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];

/// a band of whole macroblock rows: one slice, its planes and per-macroblock state
pub struct Band<'a> {
    pub mbw: usize,
    pub rows: usize,
    /// the band's first macroblock row in the picture
    pub row0: usize,
    /// the reconstruction: the reference, overwritten in place as each macroblock is
    /// coded (zero motion only ever reads a macroblock's own pixels)
    pub ry: &'a mut [u8],
    pub ru: &'a mut [u8],
    pub rv: &'a mut [u8],
    /// the source as it was when last coded, to see what really changed
    pub ly: &'a mut [u8],
    pub lu: &'a mut [u8],
    pub lv: &'a mut [u8],
    pub sy: &'a [u8],
    pub su: &'a [u8],
    pub sv: &'a [u8],
    /// the macroblocks to code, by their index in the band, in order; every other one
    /// is skipped without being looked at
    pub todo: &'a [(u32, Act)],
    /// the bytes this slice may grow to refining: past them what's left to refine is
    /// skipped and stays coarse
    pub refine_bytes: usize,
    /// the qp each macroblock was last coded at
    pub q: &'a mut [u8],
    /// total_coeff of each 4x4 block (16 luma, raster, then 4 Cb, 4 Cr), good for the
    /// frame its stamp names: any other stamp reads as all zero, as a skip's are, so a
    /// skipped macroblock is never touched
    pub nnz: &'a mut [[u8; 24]],
    pub stamp: &'a mut [u32],
}

#[derive(Default, Clone, Copy)]
pub struct Count {
    pub coded: usize,
    pub refined: usize,
    pub intra: usize,
}

pub struct SliceHead {
    /// this frame's stamp for nnz
    pub tick: u32,
    pub idr: bool,
    pub frame_num: u16,
    pub idr_id: u32,
    pub qp: u8,
    pub refine_qp: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Inter,
    /// luma 0 V, 1 H, 2 DC, 3 plane; chroma 0 DC, 1 H, 2 V, 3 plane
    Intra(u8, u8),
}

/// a macroblock's pixels and coefficients while it's coded. Coefficients are kept by
/// position then block, every block side by side, so the transforms and quantisation
/// run on all of them at once as vectors
struct Mb {
    sy: [u8; 256],
    su: [u8; 64],
    sv: [u8; 64],
    py: [u8; 256],
    pu: [u8; 64],
    pv: [u8; 64],
    /// luma levels by raster coefficient, then raster block
    lv: [[i32; 16]; 16],
    /// Intra 16x16 luma DC levels, raster
    ldc: [i32; 16],
    cdc: [[i32; 4]; 2],
    /// chroma AC levels by raster coefficient, then block: Cb's four, Cr's four
    cl: [[i32; 8]; 16],
}

impl Default for Mb {
    fn default() -> Mb {
        Mb {
            sy: [0; 256],
            su: [0; 64],
            sv: [0; 64],
            py: [0; 256],
            pu: [0; 64],
            pv: [0; 64],
            lv: [[0; 16]; 16],
            ldc: [0; 16],
            cdc: [[0; 4]; 2],
            cl: [[0; 8]; 16],
        }
    }
}

/// the forward core transform of N 4x4 residuals at once, by raster position then block
#[inline(always)]
fn fdct_n<const N: usize>(r: &[[i32; N]; 16]) -> [[i32; N]; 16] {
    let mut t = [[0i32; N]; 16];
    for i in 0..4 {
        for b in 0..N {
            let (p0, p1, p2, p3) = (r[i * 4][b], r[i * 4 + 1][b], r[i * 4 + 2][b], r[i * 4 + 3][b]);
            let (s03, s12, d03, d12) = (p0 + p3, p1 + p2, p0 - p3, p1 - p2);
            t[i * 4][b] = s03 + s12;
            t[i * 4 + 1][b] = 2 * d03 + d12;
            t[i * 4 + 2][b] = s03 - s12;
            t[i * 4 + 3][b] = d03 - 2 * d12;
        }
    }
    let mut o = [[0i32; N]; 16];
    for j in 0..4 {
        for b in 0..N {
            let (p0, p1, p2, p3) = (t[j][b], t[4 + j][b], t[8 + j][b], t[12 + j][b]);
            let (s03, s12, d03, d12) = (p0 + p3, p1 + p2, p0 - p3, p1 - p2);
            o[j][b] = s03 + s12;
            o[4 + j][b] = 2 * d03 + d12;
            o[8 + j][b] = s03 - s12;
            o[12 + j][b] = d03 - 2 * d12;
        }
    }
    o
}

/// the inverse transform of N blocks at once, exactly as 8.5.12.2 has it: rows, then
/// columns, then (x + 32) >> 6
#[inline(always)]
fn idct_n<const N: usize>(d: &[[i32; N]; 16]) -> [[i32; N]; 16] {
    let mut f = [[0i32; N]; 16];
    for i in 0..4 {
        for b in 0..N {
            let (d0, d1, d2, d3) = (d[i * 4][b], d[i * 4 + 1][b], d[i * 4 + 2][b], d[i * 4 + 3][b]);
            let (e0, e1, e2, e3) = (d0 + d2, d0 - d2, (d1 >> 1) - d3, d1 + (d3 >> 1));
            f[i * 4][b] = e0 + e3;
            f[i * 4 + 1][b] = e1 + e2;
            f[i * 4 + 2][b] = e1 - e2;
            f[i * 4 + 3][b] = e0 - e3;
        }
    }
    let mut r = [[0i32; N]; 16];
    for j in 0..4 {
        for b in 0..N {
            let (f0, f1, f2, f3) = (f[j][b], f[4 + j][b], f[8 + j][b], f[12 + j][b]);
            let (g0, g1, g2, g3) = (f0 + f2, f0 - f2, (f1 >> 1) - f3, f1 + (f3 >> 1));
            r[j][b] = (g0 + g3 + 32) >> 6;
            r[4 + j][b] = (g1 + g2 + 32) >> 6;
            r[8 + j][b] = (g1 - g2 + 32) >> 6;
            r[12 + j][b] = (g0 - g3 + 32) >> 6;
        }
    }
    r
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn avx2() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

/// N blocks' coefficients from position `from` on quantised (dead zone f); below it
/// zero. Each block's count of non-zero levels (its total_coeff) back
#[inline(always)]
fn quant_n<const N: usize>(
    c: &[[i32; N]; 16],
    lv: &mut [[i32; N]; 16],
    from: usize,
    q: usize,
    f: i32,
    bits: u32,
) -> [u8; N] {
    #[cfg(target_arch = "x86_64")]
    if N % 8 == 0 && avx2() {
        let mut n = [0u8; N];
        // SAFETY: AVX2 checked; both arrays are 16 rows of N, a multiple of 8
        unsafe { quant_avx2(c.as_flattened(), lv.as_flattened_mut(), &mut n, from, q, f, bits) };
        return n;
    }
    for k in 0..16 {
        if k < from {
            lv[k] = [0; N];
            continue;
        }
        let mf = MF[q][CLASS[k]];
        for b in 0..N {
            let x = c[k][b];
            let a = ((x.abs() * mf + f) >> bits).min(LEVEL_MAX);
            lv[k][b] = if x < 0 { -a } else { a };
        }
    }
    counts(lv)
}

/// quant_n eight lanes at a time: |c| * mf + f, shifted, capped, the sign put back;
/// each lane's zeros counted as it goes
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn quant_avx2(c: &[i32], lv: &mut [i32], counts: &mut [u8], from: usize, q: usize, f: i32, bits: u32) {
    use std::arch::x86_64::*;
    let n = counts.len();
    let fv = _mm256_set1_epi32(f);
    let cap = _mm256_set1_epi32(LEVEL_MAX);
    let sh = _mm_cvtsi32_si128(bits as i32);
    let zero = _mm256_setzero_si256();
    let mut zeros = [zero; 2];
    for k in 0..16 {
        let mf = _mm256_set1_epi32(MF[q][CLASS[k]]);
        for (j, z) in zeros.iter_mut().enumerate().take(n / 8) {
            let at = k * n + j * 8;
            // SAFETY: at + 8 <= 16 * n
            unsafe {
                let r = if k < from {
                    zero
                } else {
                    let x = _mm256_loadu_si256(c.as_ptr().add(at) as *const __m256i);
                    let a = _mm256_add_epi32(_mm256_mullo_epi32(_mm256_abs_epi32(x), mf), fv);
                    _mm256_sign_epi32(_mm256_min_epi32(_mm256_sra_epi32(a, sh), cap), x)
                };
                _mm256_storeu_si256(lv.as_mut_ptr().add(at) as *mut __m256i, r);
                *z = _mm256_sub_epi32(*z, _mm256_cmpeq_epi32(r, zero));
            }
        }
    }
    let mut z = [0i32; 16];
    // SAFETY: two vectors into 16 lanes
    unsafe {
        _mm256_storeu_si256(z.as_mut_ptr() as *mut __m256i, zeros[0]);
        _mm256_storeu_si256(z.as_mut_ptr().add(8) as *mut __m256i, zeros[1]);
    }
    for b in 0..n {
        counts[b] = (16 - z[b]) as u8;
    }
}

/// N blocks' levels scaled back (8.5.12.1, flat matrices)
#[inline(always)]
fn dequant_n<const N: usize>(lv: &[[i32; N]; 16], q: usize, q6: u32) -> [[i32; N]; 16] {
    let mut d = [[0i32; N]; 16];
    #[cfg(target_arch = "x86_64")]
    if N % 8 == 0 && avx2() {
        // SAFETY: AVX2 checked; 16 rows of N, a multiple of 8
        unsafe { dequant_avx2(lv.as_flattened(), d.as_flattened_mut(), N, q, q6) };
        return d;
    }
    for k in 0..16 {
        let v = V[q][CLASS[k]];
        for b in 0..N {
            d[k][b] = (lv[k][b] * v) << q6;
        }
    }
    d
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dequant_avx2(lv: &[i32], d: &mut [i32], n: usize, q: usize, q6: u32) {
    use std::arch::x86_64::*;
    let sh = _mm_cvtsi32_si128(q6 as i32);
    for k in 0..16 {
        let v = _mm256_set1_epi32(V[q][CLASS[k]]);
        for j in (0..n).step_by(8) {
            // SAFETY: within 16 rows of n
            unsafe {
                let x = _mm256_loadu_si256(lv.as_ptr().add(k * n + j) as *const __m256i);
                let r = _mm256_sll_epi32(_mm256_mullo_epi32(x, v), sh);
                _mm256_storeu_si256(d.as_mut_ptr().add(k * n + j) as *mut __m256i, r);
            }
        }
    }
}

/// each block's count of non-zero levels: its total_coeff
#[inline(always)]
fn counts<const N: usize>(lv: &[[i32; N]; 16]) -> [u8; N] {
    let mut n = [0u8; N];
    for row in lv {
        for b in 0..N {
            n[b] += (row[b] != 0) as u8;
        }
    }
    n
}

/// H x H for the 4x4 Hadamard (the luma DC's, both ways)
fn hadamard4(x: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let (a, b, c, d) = (x[i * 4], x[i * 4 + 1], x[i * 4 + 2], x[i * 4 + 3]);
        t[i * 4] = a + b + c + d;
        t[i * 4 + 1] = a + b - c - d;
        t[i * 4 + 2] = a - b - c + d;
        t[i * 4 + 3] = a - b + c - d;
    }
    let mut o = [0i32; 16];
    for j in 0..4 {
        let (a, b, c, d) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        o[j] = a + b + c + d;
        o[4 + j] = a + b - c - d;
        o[8 + j] = a - b - c + d;
        o[12 + j] = a - b + c - d;
    }
    o
}

fn hadamard2(c: &[i32; 4]) -> [i32; 4] {
    [c[0] + c[1] + c[2] + c[3], c[0] - c[1] + c[2] - c[3], c[0] + c[1] - c[2] - c[3], c[0] - c[1] - c[2] + c[3]]
}

/// levels past this aren't written: Baseline's level_prefix stops at 15
const LEVEL_MAX: i32 = 2047;

#[inline(always)]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// the sum of absolute differences of two blocks of rows `w` wide (8 or 16)
#[inline(always)]
pub fn sad(a: &[u8], b: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if a.len() % 16 == 0 {
        use std::arch::x86_64::*;
        // SAFETY: SSE2 is x86-64's baseline; loads within both slices, 16 bytes each
        unsafe {
            let mut acc = _mm_setzero_si128();
            for i in (0..a.len().min(b.len())).step_by(16) {
                let x = _mm_loadu_si128(a.as_ptr().add(i) as *const __m128i);
                let y = _mm_loadu_si128(b.as_ptr().add(i) as *const __m128i);
                acc = _mm_add_epi64(acc, _mm_sad_epu8(x, y));
            }
            return (_mm_cvtsi128_si32(acc) + _mm_extract_epi16(acc, 4)) as u32;
        }
    }
    a.iter().zip(b).map(|(&x, &y)| (x as i32 - y as i32).unsigned_abs()).sum()
}

/// SAD against vertical prediction: every row the row above
fn sad_v<const N: usize>(src: &[u8], top: &[u8; N]) -> u32 {
    src.chunks_exact(N).map(|row| sad(row, top)).sum()
}

/// SAD against horizontal prediction: every row its left neighbour
fn sad_h<const N: usize>(src: &[u8], left: &[u8; N]) -> u32 {
    src.chunks_exact(N)
        .zip(left)
        .map(|(row, &l)| row.iter().map(|&v| (v as i32 - l as i32).unsigned_abs()).sum::<u32>())
        .sum()
}

/// what predicts a block from its neighbours: the row above and the column to the left
struct Edge<const N: usize> {
    top: Option<[u8; N]>,
    left: Option<[u8; N]>,
    corner: u8,
}

impl<const N: usize> Edge<N> {
    fn of(plane: &[u8], stride: usize, x: usize, y: usize, top: bool, left: bool) -> Edge<N> {
        let t = top.then(|| std::array::from_fn(|i| plane[(y - 1) * stride + x + i]));
        let l = left.then(|| std::array::from_fn(|i| plane[(y + i) * stride + x - 1]));
        let corner = if top && left { plane[(y - 1) * stride + x - 1] } else { 0 };
        Edge { top: t, left: l, corner }
    }

    /// p[i, -1] with p[-1, -1] at i = -1
    fn t(&self, i: isize) -> i32 {
        if i < 0 { self.corner as i32 } else { self.top.unwrap()[i as usize] as i32 }
    }
    fn l(&self, i: isize) -> i32 {
        if i < 0 { self.corner as i32 } else { self.left.unwrap()[i as usize] as i32 }
    }

    fn plane(&self, out: &mut [u8]) -> bool {
        if self.top.is_none() || self.left.is_none() {
            return false;
        }
        let half = (N / 2) as isize;
        let (mut hh, mut vv) = (0i32, 0i32);
        for k in 0..half {
            hh += (k as i32 + 1) * (self.t(half + k) - self.t(half - 2 - k));
            vv += (k as i32 + 1) * (self.l(half + k) - self.l(half - 2 - k));
        }
        let a = 16 * (self.l(N as isize - 1) + self.t(N as isize - 1));
        let (b, c) =
            if N == 16 { ((5 * hh + 32) >> 6, (5 * vv + 32) >> 6) } else { ((34 * hh + 32) >> 6, (34 * vv + 32) >> 6) };
        let mid = half as i32 - 1;
        for y in 0..N {
            for x in 0..N {
                out[y * N + x] = clip((a + b * (x as i32 - mid) + c * (y as i32 - mid) + 16) >> 5);
            }
        }
        true
    }

    fn vertical(&self, out: &mut [u8]) -> bool {
        let Some(t) = self.top else { return false };
        for row in out.chunks_exact_mut(N) {
            row.copy_from_slice(&t);
        }
        true
    }

    fn horizontal(&self, out: &mut [u8]) -> bool {
        let Some(l) = self.left else { return false };
        for (row, &v) in out.chunks_exact_mut(N).zip(&l) {
            row.fill(v);
        }
        true
    }
}

fn sum(a: &[u8]) -> i32 {
    a.iter().map(|&v| v as i32).sum()
}

/// Intra 16x16 DC
fn luma_dc(e: &Edge<16>, out: &mut [u8]) {
    let v = match (&e.top, &e.left) {
        (Some(t), Some(l)) => (sum(t) + sum(l) + 16) >> 5,
        (Some(t), None) => (sum(t) + 8) >> 4,
        (None, Some(l)) => (sum(l) + 8) >> 4,
        (None, None) => 128,
    };
    out.fill(v as u8);
}

/// chroma DC, each 4x4 block from its own part of the edges (8.3.4.1-3)
fn chroma_dc(e: &Edge<8>, out: &mut [u8]) {
    for by in 0..2 {
        for bx in 0..2 {
            let t = e.top.map(|t| sum(&t[bx * 4..bx * 4 + 4]));
            let l = e.left.map(|l| sum(&l[by * 4..by * 4 + 4]));
            let v = match (bx, by, t, l) {
                // the corner blocks: both when there are both
                (0, 0, Some(t), Some(l)) | (1, 1, Some(t), Some(l)) => (t + l + 4) >> 3,
                // top right: the top first
                (1, 0, Some(t), _) => (t + 2) >> 2,
                // bottom left: the left first
                (0, 1, _, Some(l)) => (l + 2) >> 2,
                (_, _, _, Some(l)) => (l + 2) >> 2,
                (_, _, Some(t), _) => (t + 2) >> 2,
                _ => 128,
            };
            for y in 0..4 {
                out[(by * 4 + y) * 8 + bx * 4..][..4].fill(v as u8);
            }
        }
    }
}

/// nC from the two neighbours' total_coeff, where they're in this slice
#[inline]
fn nc(a: Option<u8>, b: Option<u8>) -> i32 {
    match (a, b) {
        (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
        (Some(a), None) => a as i32,
        (None, Some(b)) => b as i32,
        (None, None) => 0,
    }
}

/// one residual block in CAVLC: its coefficients in scan order, nC (-1: chroma DC).
/// Its total_coeff back
fn residual(b: &mut Bits, coef: &[i32], nc: i32) -> u8 {
    let max = coef.len();
    let mut lv = [0i32; 16];
    let mut pos = [0usize; 16];
    let mut n = 0;
    for i in (0..max).rev() {
        if coef[i] != 0 {
            lv[n] = coef[i];
            pos[n] = i;
            n += 1;
        }
    }
    let mut t1 = 0;
    while t1 < n && t1 < 3 && lv[t1].abs() == 1 {
        t1 += 1;
    }
    let tok = n * 4 + t1;
    if nc < 0 {
        b.put(CHROMA_DC_TOKEN_LEN[tok] as u32, CHROMA_DC_TOKEN_BITS[tok] as u32);
    } else {
        let t = match nc {
            0..=1 => 0,
            2..=3 => 1,
            4..=7 => 2,
            _ => 3,
        };
        b.put(COEFF_TOKEN_LEN[t][tok] as u32, COEFF_TOKEN_BITS[t][tok] as u32);
    }
    if n == 0 {
        return 0;
    }
    for &l in &lv[..t1] {
        b.flag(l < 0);
    }
    let mut sl: u32 = if n > 10 && t1 < 3 { 1 } else { 0 };
    for (i, &l) in lv[..n].iter().enumerate().skip(t1) {
        let mut code = if l > 0 { 2 * l - 2 } else { -2 * l - 1 } as u32;
        if i == t1 && t1 < 3 {
            code -= 2;
        }
        if sl == 0 {
            if code < 14 {
                b.put(code + 1, 1);
            } else if code < 30 {
                b.put(15, 1);
                b.put(4, code - 14);
            } else {
                b.put(16, 1);
                b.put(12, code - 30);
            }
        } else if code < (15 << sl) {
            b.put((code >> sl) + 1, 1);
            b.put(sl, code & ((1 << sl) - 1));
        } else {
            b.put(16, 1);
            b.put(12, code - (15 << sl));
        }
        if sl == 0 {
            sl = 1;
        }
        if l.unsigned_abs() > (3 << (sl - 1)) && sl < 6 {
            sl += 1;
        }
    }
    if n < max {
        let tz = pos[0] + 1 - n;
        if nc < 0 {
            b.put(CHROMA_DC_ZEROS_LEN[n - 1][tz] as u32, CHROMA_DC_ZEROS_BITS[n - 1][tz] as u32);
        } else {
            b.put(TOTAL_ZEROS_LEN[n - 1][tz] as u32, TOTAL_ZEROS_BITS[n - 1][tz] as u32);
        }
        let mut left = tz;
        for i in 0..n - 1 {
            if left == 0 {
                break;
            }
            let run = pos[i] - pos[i + 1] - 1;
            let t = left.min(7) - 1;
            b.put(RUN_LEN[t][run] as u32, RUN_BITS[t][run] as u32);
            left -= run;
        }
    }
    n as u8
}

/// block `blk`'s levels in scan order, from coefficient `from` (0, or 1 for AC)
#[inline(always)]
fn scan<const N: usize>(lv: &[[i32; N]; 16], blk: usize, from: usize, out: &mut [i32; 16]) -> usize {
    for k in from..16 {
        out[k - from] = lv[ZIGZAG[k]][blk];
    }
    16 - from
}

/// a block with no coefficients: its coeff_token alone
#[inline(always)]
fn empty(b: &mut Bits, nc: i32) {
    let t = match nc {
        0..=1 => 0,
        2..=3 => 1,
        4..=7 => 2,
        _ => 3,
    };
    b.put(COEFF_TOKEN_LEN[t][0] as u32, COEFF_TOKEN_BITS[t][0] as u32);
}

impl Band<'_> {
    /// the slice: its header, its macroblocks, its trailing bits. Built twice, for AVX2
    /// and for any x86-64, and picked at run time: the side-by-side transforms and
    /// quantisation are 8 lanes wide with it
    pub fn slice(&mut self, h: &SliceHead, b: &mut Bits) -> Count {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: only where the CPU has AVX2
            return unsafe { self.slice_avx2(h, b) };
        }
        self.slice_any(h, b)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn slice_avx2(&mut self, h: &SliceHead, b: &mut Bits) -> Count {
        self.slice_any(h, b)
    }

    #[inline(always)]
    fn slice_any(&mut self, h: &SliceHead, b: &mut Bits) -> Count {
        b.clear();
        b.ue((self.row0 * self.mbw) as u32);
        b.ue(if h.idr { 7 } else { 5 });
        b.ue(0);
        b.put(16, h.frame_num as u32);
        if h.idr {
            b.ue(h.idr_id);
        } else {
            // num_ref_idx_active_override_flag, ref_pic_list_modification_flag_l0
            b.put(2, 0);
        }
        // dec_ref_pic_marking: IDR no_output_of_prior_pics and long_term_reference, or
        // P's sliding window
        b.put(if h.idr { 2 } else { 1 }, 0);
        b.se(h.qp as i32 - 26);
        // no deblocking: reconstruction is prediction plus residual, nothing more
        b.ue(1);
        let mut last_qp = h.qp;
        let mut skip = 0u32;
        let mut cnt = Count::default();
        let mut mb = Box::<Mb>::default();
        let mut next = 0usize;
        for &(i, act) in self.todo {
            let i = i as usize;
            skip += (i - next) as u32;
            next = i + 1;
            let (mx, my) = (i % self.mbw, i / self.mbw);
            self.load(&mut mb, mx, my);
            let coded = match act {
                Act::Skip => false,
                Act::Refine if b.buf.len() >= self.refine_bytes => false,
                Act::Dirty if !h.idr && !self.changed(&mb, mx, my) => false,
                Act::Dirty => {
                    let r = self.code(&mut mb, b, mx, my, h.idr, h.qp, h.tick, &mut last_qp, &mut skip);
                    cnt.coded += r.is_some() as usize;
                    cnt.intra += (r == Some(true)) as usize;
                    r.is_some()
                }
                Act::Refine => {
                    let r = self.code(&mut mb, b, mx, my, false, h.refine_qp, h.tick, &mut last_qp, &mut skip);
                    // even when nothing's left to send at that qp it's as good as it gets
                    self.q[i] = self.q[i].min(h.refine_qp);
                    cnt.refined += r.is_some() as usize;
                    r.is_some()
                }
            };
            if !coded {
                skip += 1;
            }
        }
        skip += (self.rows * self.mbw - next) as u32;
        if skip > 0 && !h.idr {
            b.ue(skip);
        }
        b.trailing();
        cnt
    }

    fn load(&self, mb: &mut Mb, mx: usize, my: usize) {
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        for y in 0..16 {
            mb.sy[y * 16..y * 16 + 16].copy_from_slice(&self.sy[(my * 16 + y) * ys + mx * 16..][..16]);
        }
        for y in 0..8 {
            mb.su[y * 8..y * 8 + 8].copy_from_slice(&self.su[(my * 8 + y) * cs + mx * 8..][..8]);
            mb.sv[y * 8..y * 8 + 8].copy_from_slice(&self.sv[(my * 8 + y) * cs + mx * 8..][..8]);
        }
    }

    /// whether the source differs from what was last coded here
    fn changed(&self, mb: &Mb, mx: usize, my: usize) -> bool {
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        (0..16).any(|y| mb.sy[y * 16..y * 16 + 16] != self.ly[(my * 16 + y) * ys + mx * 16..][..16])
            || (0..8).any(|y| {
                mb.su[y * 8..y * 8 + 8] != self.lu[(my * 8 + y) * cs + mx * 8..][..8]
                    || mb.sv[y * 8..y * 8 + 8] != self.lv[(my * 8 + y) * cs + mx * 8..][..8]
            })
    }

    /// the macroblock coded, or None where an inter coding left nothing to send (a
    /// skip then). Some(true): intra
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn code(
        &mut self,
        mb: &mut Mb,
        b: &mut Bits,
        mx: usize,
        my: usize,
        intra_only: bool,
        qp: u8,
        tick: u32,
        last_qp: &mut u8,
        skip: &mut u32,
    ) -> Option<bool> {
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        let (top, left) = (my > 0, mx > 0);
        // inter: the reference itself
        let mut inter_cost = u32::MAX;
        if !intra_only {
            for y in 0..16 {
                mb.py[y * 16..y * 16 + 16].copy_from_slice(&self.ry[(my * 16 + y) * ys + mx * 16..][..16]);
            }
            for y in 0..8 {
                mb.pu[y * 8..y * 8 + 8].copy_from_slice(&self.ru[(my * 8 + y) * cs + mx * 8..][..8]);
                mb.pv[y * 8..y * 8 + 8].copy_from_slice(&self.rv[(my * 8 + y) * cs + mx * 8..][..8]);
            }
            inter_cost = sad(&mb.sy, &mb.py) + sad(&mb.su, &mb.pu) + sad(&mb.sv, &mb.pv);
        }
        // intra, unless the reference is already close (a refine, a small change)
        let mut mode = Mode::Inter;
        if intra_only || inter_cost > 2 * 256 {
            let e = Edge::<16>::of(self.ry, ys, mx * 16, my * 16, top, left);
            let mut p = [0u8; 256];
            // DC first: a flat block is done with it; V and H scored off the edges; plane
            // made only when they're all poor
            luma_dc(&e, &mut p);
            let (mut best, mut lm) = (sad(&mb.sy, &p), 2u8);
            if best > 256 {
                if let Some(t) = &e.top {
                    let s = sad_v(&mb.sy, t);
                    if s < best {
                        (best, lm) = (s, 0);
                    }
                }
                if let Some(l) = &e.left {
                    let s = sad_h(&mb.sy, l);
                    if s < best {
                        (best, lm) = (s, 1);
                    }
                }
                if best > 1024 && e.plane(&mut p) {
                    let s = sad(&mb.sy, &p);
                    if s < best {
                        (best, lm) = (s, 3);
                    }
                }
            }
            let eu = Edge::<8>::of(self.ru, cs, mx * 8, my * 8, top, left);
            let ev = Edge::<8>::of(self.rv, cs, mx * 8, my * 8, top, left);
            let (mut pu, mut pv) = ([0u8; 64], [0u8; 64]);
            chroma_dc(&eu, &mut pu);
            chroma_dc(&ev, &mut pv);
            let (mut cbest, mut cm) = (sad(&mb.su, &pu) + sad(&mb.sv, &pv), 0u8);
            if cbest > 64 {
                if let (Some(tu), Some(tv)) = (&eu.left, &ev.left) {
                    let s = sad_h(&mb.su, tu) + sad_h(&mb.sv, tv);
                    if s < cbest {
                        (cbest, cm) = (s, 1);
                    }
                }
                if let (Some(tu), Some(tv)) = (&eu.top, &ev.top) {
                    let s = sad_v(&mb.su, tu) + sad_v(&mb.sv, tv);
                    if s < cbest {
                        (cbest, cm) = (s, 2);
                    }
                }
                if cbest > 256 && eu.plane(&mut pu) && ev.plane(&mut pv) {
                    let s = sad(&mb.su, &pu) + sad(&mb.sv, &pv);
                    if s < cbest {
                        (cbest, cm) = (s, 3);
                    }
                }
            }
            // intra costs more bits to say: it must win clearly
            if intra_only || best + cbest + 256 < inter_cost {
                mode = Mode::Intra(lm, cm);
                match lm {
                    0 => e.vertical(&mut mb.py),
                    1 => e.horizontal(&mut mb.py),
                    2 => {
                        luma_dc(&e, &mut mb.py);
                        true
                    }
                    _ => e.plane(&mut mb.py),
                };
                match cm {
                    0 => {
                        chroma_dc(&eu, &mut mb.pu);
                        chroma_dc(&ev, &mut mb.pv);
                    }
                    1 => {
                        eu.horizontal(&mut mb.pu);
                        ev.horizontal(&mut mb.pv);
                    }
                    2 => {
                        eu.vertical(&mut mb.pu);
                        ev.vertical(&mut mb.pv);
                    }
                    _ => {
                        eu.plane(&mut mb.pu);
                        ev.plane(&mut mb.pv);
                    }
                }
            }
        }
        let intra = mode != Mode::Inter;
        let (q6, q) = ((qp / 6) as u32, (qp % 6) as usize);
        let qbits = 15 + q6;
        let f = if intra { (1 << qbits) / 3 } else { (1 << qbits) / 6 };
        // luma: the residual of all 16 blocks side by side, transformed, quantised
        let mut r = [[0i32; 16]; 16];
        for by in 0..4 {
            for y in 0..4 {
                let row = (by * 4 + y) * 16;
                for bx in 0..4 {
                    for x in 0..4 {
                        r[y * 4 + x][by * 4 + bx] = mb.sy[row + bx * 4 + x] as i32 - mb.py[row + bx * 4 + x] as i32;
                    }
                }
            }
        }
        let c = fdct_n(&r);
        let ln = quant_n(&c, &mut mb.lv, intra as usize, q, f, qbits);
        let mut cbp_l = 0u32;
        if intra {
            let t = hadamard4(&c[0]);
            for (l, &t) in mb.ldc.iter_mut().zip(&t) {
                let x = t / 2;
                let a = ((x.abs() * MF[q][0] + 2 * f) >> (qbits + 1)).min(LEVEL_MAX);
                *l = if x < 0 { -a } else { a };
            }
            if ln.iter().any(|&n| n > 0) {
                cbp_l = 15;
            }
        } else {
            for (blk, &n) in ln.iter().enumerate() {
                if n > 0 {
                    cbp_l |= 1 << ((blk / 8) * 2 + (blk % 4) / 2);
                }
            }
        }
        // chroma: Cb's four blocks and Cr's side by side
        let qc = chroma_qp(qp);
        let (qc6, qcr) = ((qc / 6) as u32, (qc % 6) as usize);
        let cbits = 15 + qc6;
        let cf = if intra { (1 << cbits) / 3 } else { (1 << cbits) / 6 };
        let mut rc = [[0i32; 8]; 16];
        for p in 0..2 {
            let (src, pred) = if p == 0 { (&mb.su, &mb.pu) } else { (&mb.sv, &mb.pv) };
            for by in 0..2 {
                for y in 0..4 {
                    let row = (by * 4 + y) * 8;
                    for bx in 0..2 {
                        for x in 0..4 {
                            let at = row + bx * 4 + x;
                            rc[y * 4 + x][p * 4 + by * 2 + bx] = src[at] as i32 - pred[at] as i32;
                        }
                    }
                }
            }
        }
        let cc = fdct_n(&rc);
        let cn = quant_n(&cc, &mut mb.cl, 1, qcr, cf, cbits);
        let mut any_dc = false;
        for p in 0..2 {
            let w = hadamard2(&[cc[0][p * 4], cc[0][p * 4 + 1], cc[0][p * 4 + 2], cc[0][p * 4 + 3]]);
            for (l, &x) in mb.cdc[p].iter_mut().zip(&w) {
                let a = ((x.abs() * MF[qcr][0] + 2 * cf) >> (cbits + 1)).min(LEVEL_MAX);
                *l = if x < 0 { -a } else { a };
                any_dc |= a != 0;
            }
        }
        let any_ac = cn.iter().any(|&n| n > 0);
        let cbp_c = if any_ac { 2 } else { any_dc as u32 };
        if !intra && cbp_l == 0 && cbp_c == 0 {
            // nothing to send at this qp: what the reference has is the best it gets
            self.keep(mb, mx, my, qp);
            return None;
        }
        // total_coeff of each block, before any nC is asked
        let i = my * self.mbw + mx;
        let mut nz = [0u8; 24];
        nz[..16].copy_from_slice(&ln);
        nz[16..].copy_from_slice(&cn);
        self.nnz[i] = nz;
        self.stamp[i] = tick;
        // the syntax
        if !intra_only {
            b.ue(*skip);
        }
        *skip = 0;
        let mut dqp = qp as i32 - *last_qp as i32;
        if dqp > 25 {
            dqp -= 52;
        } else if dqp < -26 {
            dqp += 52;
        }
        let mut sc = [0i32; 16];
        match mode {
            Mode::Inter => {
                b.ue(0);
                // mvd: the predicted vector, which is always zero here
                b.put(2, 0b11);
                let cbp = cbp_l | (cbp_c << 4);
                b.ue(INTER_CBP_CODE[cbp as usize] as u32);
                b.se(dqp);
                *last_qp = qp;
                for i8 in 0..4 {
                    if cbp_l & (1 << i8) == 0 {
                        continue;
                    }
                    for i4 in 0..4 {
                        let blk = Z2R[i8 * 4 + i4];
                        let nc = self.luma_nc(&nz, mx, my, blk, tick);
                        if ln[blk] == 0 {
                            empty(b, nc);
                        } else {
                            let n = scan(&mb.lv, blk, 0, &mut sc);
                            residual(b, &sc[..n], nc);
                        }
                    }
                }
            }
            Mode::Intra(lm, cm) => {
                let base = if intra_only { 0 } else { 5 };
                b.ue(base + 1 + lm as u32 + 4 * cbp_c + if cbp_l != 0 { 12 } else { 0 });
                b.ue(cm as u32);
                b.se(dqp);
                *last_qp = qp;
                let mut dc = [0i32; 16];
                for k in 0..16 {
                    dc[k] = mb.ldc[ZIGZAG[k]];
                }
                let nc = self.luma_nc(&nz, mx, my, 0, tick);
                residual(b, &dc, nc);
                if cbp_l != 0 {
                    for &blk in &Z2R {
                        let nc = self.luma_nc(&nz, mx, my, blk, tick);
                        if ln[blk] == 0 {
                            empty(b, nc);
                        } else {
                            let n = scan(&mb.lv, blk, 1, &mut sc);
                            residual(b, &sc[..n], nc);
                        }
                    }
                }
            }
        }
        if cbp_c > 0 {
            for p in 0..2 {
                residual(b, &mb.cdc[p], -1);
            }
        }
        if cbp_c == 2 {
            for p in 0..2 {
                for blk in 0..4 {
                    let nc = self.chroma_nc(&nz, mx, my, p, blk, tick);
                    if cn[p * 4 + blk] == 0 {
                        empty(b, nc);
                    } else {
                        let n = scan(&mb.cl, p * 4 + blk, 1, &mut sc);
                        residual(b, &sc[..n], nc);
                    }
                }
            }
        }
        // the reconstruction, as the decoder makes it
        let mut d = dequant_n(&mb.lv, q, q6);
        if intra {
            d[0] = luma_dc_dequant(&mb.ldc, qp);
        }
        let res = idct_n(&d);
        let ys = self.mbw * 16;
        for by in 0..4 {
            for y in 0..4 {
                let row = (by * 4 + y) * 16;
                let out = &mut self.ry[(my * 16 + by * 4 + y) * ys + mx * 16..][..16];
                for bx in 0..4 {
                    for x in 0..4 {
                        let at = bx * 4 + x;
                        out[at] = clip(mb.py[row + at] as i32 + res[y * 4 + x][by * 4 + bx]);
                    }
                }
            }
        }
        let mut dc = dequant_n(&mb.cl, qcr, qc6);
        for p in 0..2 {
            let dcc = if cbp_c > 0 { chroma_dc_dequant(&mb.cdc[p], qc) } else { [0; 4] };
            dc[0][p * 4..p * 4 + 4].copy_from_slice(&dcc);
        }
        let resc = idct_n(&dc);
        let cs = self.mbw * 8;
        for p in 0..2 {
            let (pred, plane) = if p == 0 { (&mb.pu, &mut *self.ru) } else { (&mb.pv, &mut *self.rv) };
            for by in 0..2 {
                for y in 0..4 {
                    let row = (by * 4 + y) * 8;
                    let out = &mut plane[(my * 8 + by * 4 + y) * cs + mx * 8..][..8];
                    for bx in 0..2 {
                        for x in 0..4 {
                            let at = bx * 4 + x;
                            out[at] = clip(pred[row + at] as i32 + resc[y * 4 + x][p * 4 + by * 2 + bx]);
                        }
                    }
                }
            }
        }
        self.keep(mb, mx, my, qp);
        Some(intra)
    }

    /// the source kept as what's now coded here, at qp
    fn keep(&mut self, mb: &Mb, mx: usize, my: usize, qp: u8) {
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        for y in 0..16 {
            self.ly[(my * 16 + y) * ys + mx * 16..][..16].copy_from_slice(&mb.sy[y * 16..y * 16 + 16]);
        }
        for y in 0..8 {
            self.lu[(my * 8 + y) * cs + mx * 8..][..8].copy_from_slice(&mb.su[y * 8..y * 8 + 8]);
            self.lv[(my * 8 + y) * cs + mx * 8..][..8].copy_from_slice(&mb.sv[y * 8..y * 8 + 8]);
        }
        self.q[my * self.mbw + mx] = qp;
    }

    /// a neighbour's counts this frame: zeros unless it was coded in it
    #[inline(always)]
    fn nz(&self, i: usize, tick: u32, k: usize) -> u8 {
        if self.stamp[i] == tick { self.nnz[i][k] } else { 0 }
    }

    fn luma_nc(&self, cur: &[u8; 24], mx: usize, my: usize, blk: usize, tick: u32) -> i32 {
        let (bx, by) = (blk % 4, blk / 4);
        let i = my * self.mbw + mx;
        let a = if bx > 0 {
            Some(cur[blk - 1])
        } else if mx > 0 {
            Some(self.nz(i - 1, tick, by * 4 + 3))
        } else {
            None
        };
        let b = if by > 0 {
            Some(cur[blk - 4])
        } else if my > 0 {
            Some(self.nz(i - self.mbw, tick, 12 + bx))
        } else {
            None
        };
        nc(a, b)
    }

    fn chroma_nc(&self, cur: &[u8; 24], mx: usize, my: usize, p: usize, blk: usize, tick: u32) -> i32 {
        let (bx, by) = (blk % 2, blk / 2);
        let o = 16 + p * 4;
        let i = my * self.mbw + mx;
        let a = if bx > 0 {
            Some(cur[o + blk - 1])
        } else if mx > 0 {
            Some(self.nz(i - 1, tick, o + by * 2 + 1))
        } else {
            None
        };
        let b = if by > 0 {
            Some(cur[o + blk - 2])
        } else if my > 0 {
            Some(self.nz(i - self.mbw, tick, o + 2 + bx))
        } else {
            None
        };
        nc(a, b)
    }
}

/// Intra 16x16 luma DC levels to each block's DC coefficient (8.5.10)
fn luma_dc_dequant(c: &[i32; 16], qp: u8) -> [i32; 16] {
    if c.iter().all(|&v| v == 0) {
        return [0; 16];
    }
    let f = hadamard4(c);
    let q6 = (qp / 6) as u32;
    let ls = 16 * V[(qp % 6) as usize][0];
    let mut o = [0i32; 16];
    for k in 0..16 {
        o[k] = if qp >= 36 { (f[k] * ls) << (q6 - 6) } else { (f[k] * ls + (1 << (5 - q6))) >> (6 - q6) };
    }
    o
}

/// chroma DC levels to each block's DC coefficient (8.5.11.2)
fn chroma_dc_dequant(c: &[i32; 4], qc: u8) -> [i32; 4] {
    let f = hadamard2(c);
    let ls = 16 * V[(qc % 6) as usize][0];
    f.map(|v| ((v * ls) << (qc / 6)) >> 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transform_and_back_is_near_the_residual() {
        let r: [[i32; 2]; 16] = std::array::from_fn(|k| [(k as i32 * 37 % 61) - 30, (k as i32 * 11 % 29) - 14]);
        let c = fdct_n(&r);
        // quantised at qp 0 and dequantised: each residual comes back within a step
        let mut lv = [[0i32; 2]; 16];
        let _ = quant_n(&c, &mut lv, 0, 0, 1 << 14, 15);
        let back = idct_n(&dequant_n(&lv, 0, 0));
        for k in 0..16 {
            for b in 0..2 {
                assert!((back[k][b] - r[k][b]).abs() <= 1, "{k}/{b}: {} vs {}", back[k][b], r[k][b]);
            }
        }
    }
}
