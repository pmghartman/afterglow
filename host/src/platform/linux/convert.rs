//! BGRX to I420, BT.709 limited range, into a frame kept between frames: only what
//! changed is converted. Chroma is each 2×2 block's average. The kernel is plain code
//! the compiler vectorises, built twice, for AVX2 and for any x86-64, and picked once
//! at run time.

/// an I420 picture, kept and updated in place
pub struct Yuv {
    pub w: usize,
    pub h: usize,
    pub buf: Vec<u8>,
}

impl Yuv {
    pub fn new(w: usize, h: usize) -> Yuv {
        let mut y = Yuv { w, h, buf: vec![0; w * h * 3 / 2] };
        y.black();
        y
    }

    pub fn black(&mut self) {
        let n = self.w * self.h;
        self.buf[..n].fill(16);
        self.buf[n..].fill(128);
    }

    pub fn planes(&self) -> [&[u8]; 3] {
        let n = self.w * self.h;
        let (y, uv) = self.buf.split_at(n);
        let (u, v) = uv.split_at(n / 4);
        [y, u, v]
    }

    pub fn strides(&self) -> [usize; 3] {
        [self.w, self.w / 2, self.w / 2]
    }

    /// a rect of BGRX pixels (row pitch `pitch` bytes), w and h even, drawn at (x, y),
    /// x and y even. A big one is split by rows across cores
    pub fn put(&mut self, px: &[u8], pitch: usize, x: usize, y: usize, w: usize, h: usize) {
        debug_assert!(x % 2 == 0 && y % 2 == 0 && w % 2 == 0 && h % 2 == 0);
        let (w, h) = (w.min(self.w - x), h.min(self.h - y));
        if w == 0 || h == 0 {
            return;
        }
        let (fw, n) = (self.w, self.w * self.h);
        let cw = fw / 2;
        let (ys, uv) = self.buf.split_at_mut(n);
        let (us, vs) = uv.split_at_mut(n / 4);
        // this rect's rows of each plane, from its first
        let ys = &mut ys[y * fw..(y + h) * fw];
        let us = &mut us[(y / 2) * cw..(y + h) / 2 * cw];
        let vs = &mut vs[(y / 2) * cw..(y + h) / 2 * cw];
        let parts = if w * h >= 1 << 19 { cores().min(h / 64).max(1) } else { 1 };
        if parts == 1 {
            put_rows(px, pitch, x, w, h, ys, us, vs, fw);
            return;
        }
        // even row counts per part
        let per = (h / parts + 1) & !1;
        std::thread::scope(|sc| {
            let (mut px, mut ys, mut us, mut vs, mut left) = (px, ys, us, vs, h);
            while left > 0 {
                let rows = per.min(left);
                let (y0, y1) = std::mem::take(&mut ys).split_at_mut(rows * fw);
                let (u0, u1) = std::mem::take(&mut us).split_at_mut(rows / 2 * cw);
                let (v0, v1) = std::mem::take(&mut vs).split_at_mut(rows / 2 * cw);
                let p0 = px;
                if left > rows {
                    px = &px[rows * pitch..];
                }
                (ys, us, vs, left) = (y1, u1, v1, left - rows);
                if left == 0 {
                    put_rows(p0, pitch, x, w, rows, y0, u0, v0, fw);
                } else {
                    sc.spawn(move || put_rows(p0, pitch, x, w, rows, y0, u0, v0, fw));
                }
            }
        });
    }
}

fn cores() -> usize {
    use std::sync::OnceLock;
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()).min(4))
}

/// h rows (even) of a rect w wide at column x, into planes that start at its first row
#[allow(clippy::too_many_arguments)]
fn put_rows(
    px: &[u8],
    pitch: usize,
    x: usize,
    w: usize,
    h: usize,
    ys: &mut [u8],
    us: &mut [u8],
    vs: &mut [u8],
    fw: usize,
) {
    let cw = fw / 2;
    let f = kernel();
    for r in 0..h / 2 {
        let s0 = &px[2 * r * pitch..][..w * 4];
        let s1 = &px[(2 * r + 1) * pitch..][..w * 4];
        let (top, bottom) = ys[2 * r * fw..].split_at_mut(fw);
        let co = r * cw + x / 2;
        f(s0, s1, &mut top[x..x + w], &mut bottom[x..x + w], &mut us[co..co + w / 2], &mut vs[co..co + w / 2]);
    }
}

type Kernel = fn(&[u8], &[u8], &mut [u8], &mut [u8], &mut [u8], &mut [u8]);

fn kernel() -> Kernel {
    use std::sync::OnceLock;
    static K: OnceLock<Kernel> = OnceLock::new();
    *K.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: only picked where the CPU has AVX2
            return |a, b, c, d, e, f| unsafe { rows_avx2(a, b, c, d, e, f) };
        }
        rows_any
    })
}

#[inline(always)]
fn luma(b: u32, g: u32, r: u32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8
}

/// two rows of BGRX into two rows of Y and one of each chroma
#[inline(always)]
fn rows(s0: &[u8], s1: &[u8], y0: &mut [u8], y1: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    let n = u.len().min(v.len()).min(s0.len() / 8).min(s1.len() / 8).min(y0.len() / 2).min(y1.len() / 2);
    for i in 0..n {
        let a = &s0[i * 8..i * 8 + 8];
        let c = &s1[i * 8..i * 8 + 8];
        let (b0, g0, r0) = (a[0] as u32, a[1] as u32, a[2] as u32);
        let (b1, g1, r1) = (a[4] as u32, a[5] as u32, a[6] as u32);
        let (b2, g2, r2) = (c[0] as u32, c[1] as u32, c[2] as u32);
        let (b3, g3, r3) = (c[4] as u32, c[5] as u32, c[6] as u32);
        y0[i * 2] = luma(b0, g0, r0);
        y0[i * 2 + 1] = luma(b1, g1, r1);
        y1[i * 2] = luma(b2, g2, r2);
        y1[i * 2 + 1] = luma(b3, g3, r3);
        let (sb, sg, sr) = ((b0 + b1 + b2 + b3) as i32, (g0 + g1 + g2 + g3) as i32, (r0 + r1 + r2 + r3) as i32);
        u[i] = (((-26 * sr - 86 * sg + 112 * sb + 512) >> 10) + 128) as u8;
        v[i] = (((112 * sr - 102 * sg - 10 * sb + 512) >> 10) + 128) as u8;
    }
}

fn rows_any(s0: &[u8], s1: &[u8], y0: &mut [u8], y1: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    rows(s0, s1, y0, y1, u, v)
}

/// rows() 16 pixels at a time with AVX2: each pixel's weighted sum by madd (B·cb + G·cg,
/// R·cr + X·0) and hadd, the 2×2 chroma sums by adding the two rows first and hadding
/// twice; exactly rows()'s arithmetic, so the two agree to the bit
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn rows_avx2(s0: &[u8], s1: &[u8], y0: &mut [u8], y1: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    use std::arch::x86_64::*;
    let n = u.len().min(v.len()).min(s0.len() / 8).min(s1.len() / 8).min(y0.len() / 2).min(y1.len() / 2);
    let blocks = n / 8;
    // SAFETY: every load and store below is within the first blocks*16 pixels of each row
    unsafe {
        let z = _mm256_setzero_si256();
        let cy = _mm256_setr_epi16(16, 157, 47, 0, 16, 157, 47, 0, 16, 157, 47, 0, 16, 157, 47, 0);
        let cu = _mm256_setr_epi16(112, -86, -26, 0, 112, -86, -26, 0, 112, -86, -26, 0, 112, -86, -26, 0);
        let cv = _mm256_setr_epi16(-10, -102, 112, 0, -10, -102, 112, 0, -10, -102, 112, 0, -10, -102, 112, 0);
        let k128 = _mm256_set1_epi32(128);
        let k512 = _mm256_set1_epi32(512);
        let k16 = _mm256_set1_epi16(16);
        let c128 = _mm256_set1_epi32(128);
        let order = _mm256_setr_epi32(0, 1, 4, 5, 2, 3, 6, 7);
        // 8 pixels' Y as i32, in order
        let luma8 = |p: __m256i| {
            let a = _mm256_madd_epi16(_mm256_unpacklo_epi8(p, z), cy);
            let b = _mm256_madd_epi16(_mm256_unpackhi_epi8(p, z), cy);
            _mm256_srli_epi32(_mm256_add_epi32(_mm256_hadd_epi32(a, b), k128), 8)
        };
        // 16 pixels' Y stored as bytes
        let luma16 = |a: __m256i, b: __m256i, out: *mut u8| {
            let p = _mm256_permute4x64_epi64(_mm256_packs_epi32(luma8(a), luma8(b)), 0b11_01_10_00);
            let p = _mm256_add_epi16(p, k16);
            let bytes = _mm256_permute4x64_epi64(_mm256_packus_epi16(p, p), 0b00_00_10_00);
            _mm_storeu_si128(out as *mut __m128i, _mm256_castsi256_si128(bytes));
        };
        for i in 0..blocks {
            let p0 = s0.as_ptr().add(i * 64);
            let p1 = s1.as_ptr().add(i * 64);
            let a0 = _mm256_loadu_si256(p0 as *const __m256i);
            let a1 = _mm256_loadu_si256(p0.add(32) as *const __m256i);
            let b0 = _mm256_loadu_si256(p1 as *const __m256i);
            let b1 = _mm256_loadu_si256(p1.add(32) as *const __m256i);
            luma16(a0, a1, y0.as_mut_ptr().add(i * 16));
            luma16(b0, b1, y1.as_mut_ptr().add(i * 16));
            // the two rows summed, as u16, pixel by pixel
            let lo0 = _mm256_add_epi16(_mm256_unpacklo_epi8(a0, z), _mm256_unpacklo_epi8(b0, z));
            let hi0 = _mm256_add_epi16(_mm256_unpackhi_epi8(a0, z), _mm256_unpackhi_epi8(b0, z));
            let lo1 = _mm256_add_epi16(_mm256_unpacklo_epi8(a1, z), _mm256_unpacklo_epi8(b1, z));
            let hi1 = _mm256_add_epi16(_mm256_unpackhi_epi8(a1, z), _mm256_unpackhi_epi8(b1, z));
            let chroma = |c: __m256i| {
                let p0 = _mm256_hadd_epi32(_mm256_madd_epi16(lo0, c), _mm256_madd_epi16(hi0, c));
                let p1 = _mm256_hadd_epi32(_mm256_madd_epi16(lo1, c), _mm256_madd_epi16(hi1, c));
                // pixel pairs summed: blocks 0 1 4 5 | 2 3 6 7, put in order
                let s = _mm256_permutevar8x32_epi32(_mm256_hadd_epi32(p0, p1), order);
                _mm256_add_epi32(_mm256_srai_epi32(_mm256_add_epi32(s, k512), 10), c128)
            };
            let uv = _mm256_permute4x64_epi64(_mm256_packs_epi32(chroma(cu), chroma(cv)), 0b11_01_10_00);
            let uv = _mm256_packus_epi16(uv, z);
            _mm_storel_epi64(u.as_mut_ptr().add(i * 8) as *mut __m128i, _mm256_castsi256_si128(uv));
            _mm_storel_epi64(v.as_mut_ptr().add(i * 8) as *mut __m128i, _mm256_extracti128_si256(uv, 1));
        }
    }
    let (d, p) = (blocks * 8, blocks * 64);
    rows(&s0[p..], &s1[p..], &mut y0[d * 2..], &mut y1[d * 2..], &mut u[d..], &mut v[d..]);
}

/// the whole screen, sw×sh BGRX, into the frame scaled to fit (nearest neighbour), its
/// aspect kept with black around it: for a display that isn't the video's size
pub fn scaled(px: &[u8], pitch: usize, sw: usize, sh: usize, out: &mut Yuv) {
    let (w, h) = (out.w, out.h);
    let scale = (w as f64 / sw as f64).min(h as f64 / sh as f64);
    let fw = (((sw as f64 * scale) as usize).min(w)) & !1;
    let fh = (((sh as f64 * scale) as usize).min(h)) & !1;
    let (ox, oy) = (((w - fw) / 2) & !1, ((h - fh) / 2) & !1);
    out.black();
    if fw == 0 || fh == 0 {
        return;
    }
    let xs: Vec<usize> = (0..fw).map(|x| (x * sw / fw).min(sw - 1) * 4).collect();
    let mut r0 = vec![0u8; fw * 4];
    let mut r1 = vec![0u8; fw * 4];
    let n = w * h;
    let (ys, uv) = out.buf.split_at_mut(n);
    let (us, vs) = uv.split_at_mut(n / 4);
    let f = kernel();
    for r in 0..fh / 2 {
        for (dst, yy) in [(&mut r0, 2 * r), (&mut r1, 2 * r + 1)] {
            let src = &px[((yy * sh / fh).min(sh - 1)) * pitch..];
            for (d, &sx) in dst.chunks_exact_mut(4).zip(&xs) {
                d.copy_from_slice(&src[sx..sx + 4]);
            }
        }
        let ty = oy + 2 * r;
        let (top, bottom) = ys[ty * w..].split_at_mut(w);
        let co = (ty / 2) * (w / 2) + ox / 2;
        f(
            &r0,
            &r1,
            &mut top[ox..ox + fw],
            &mut bottom[ox..ox + fw],
            &mut us[co..co + fw / 2],
            &mut vs[co..co + fw / 2],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_and_black_to_limited_range_in_place() {
        let mut y = Yuv::new(4, 4);
        let white = [255u8; 2 * 2 * 4];
        y.put(&white, 8, 2, 2, 2, 2);
        let p = y.planes();
        assert_eq!(p[0], &[16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 235, 235, 16, 16, 235, 235]);
        assert_eq!(p[1], &[128, 128, 128, 128]);
        assert_eq!(p[2], &[128, 128, 128, 128]);
        // pure red: Y 63, U 102, V 240
        let red = [0u8, 0, 255, 0].repeat(4);
        y.put(&red, 8, 0, 0, 2, 2);
        let p = y.planes();
        assert_eq!(p[0][0], 63);
        assert_eq!((p[1][0], p[2][0]), (102, 240));
    }

    #[test]
    fn a_smaller_screen_letterboxed() {
        // 2x1 white over a 4x6 video: the picture 4x2 in the middle rows, black above and below
        let px = [255u8, 255, 255, 0, 255, 255, 255, 0];
        let mut out = Yuv::new(4, 6);
        scaled(&px, 8, 2, 1, &mut out);
        let y = out.planes()[0];
        assert_eq!(&y[..8], &[16; 8]);
        assert_eq!(&y[8..16], &[235; 8]);
        assert_eq!(&y[16..], &[16; 8]);
    }

    #[test]
    fn avx2_and_plain_agree_to_the_bit() {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            for w in [2usize, 16, 18, 34, 100, 1920] {
                let s0: Vec<u8> = (0..w * 4).map(|i| (i * 37 % 256) as u8).collect();
                let s1: Vec<u8> = (0..w * 4).map(|i| ((i * 91 % 256) ^ 0x5a) as u8).collect();
                let mut a = (vec![0u8; w], vec![0u8; w], vec![0u8; w / 2], vec![0u8; w / 2]);
                let mut b = a.clone();
                rows_any(&s0, &s1, &mut a.0, &mut a.1, &mut a.2, &mut a.3);
                // SAFETY: AVX2 checked above
                unsafe { rows_avx2(&s0, &s1, &mut b.0, &mut b.1, &mut b.2, &mut b.3) };
                assert_eq!(a, b, "{w} pixels");
            }
            // the extremes
            for px in [[0u8, 0, 0, 0], [255, 255, 255, 255], [255, 0, 0, 0], [0, 255, 0, 0], [0, 0, 255, 0]] {
                let s = px.repeat(16);
                let mut a = (vec![0u8; 16], vec![0u8; 16], vec![0u8; 8], vec![0u8; 8]);
                let mut b = a.clone();
                rows_any(&s, &s, &mut a.0, &mut a.1, &mut a.2, &mut a.3);
                // SAFETY: as above
                unsafe { rows_avx2(&s, &s, &mut b.0, &mut b.1, &mut b.2, &mut b.3) };
                assert_eq!(a, b, "{px:?}");
            }
        }
    }

    #[test]
    #[ignore]
    fn speed() {
        let (w, h) = (1920, 1080);
        let px: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let mut y = Yuv::new(w, h);
        let mut one = Yuv::new(w, h);
        // split across cores or not, the same picture
        y.put(&px, w * 4, 0, 0, w, h);
        {
            let (ys, uv) = one.buf.split_at_mut(w * h);
            let (us, vs) = uv.split_at_mut(w * h / 4);
            put_rows(&px, w * 4, 0, w, h, ys, us, vs, w);
        }
        assert!(y.buf == one.buf);
        let t = std::time::Instant::now();
        for _ in 0..100 {
            y.put(&px, w * 4, 0, 0, w, h);
        }
        eprintln!("1080p: {:.3} ms a frame", t.elapsed().as_secs_f64() * 10.0);
    }
}
