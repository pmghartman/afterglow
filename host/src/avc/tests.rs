//! The encoder against a real decoder: every frame FFmpeg decodes from the stream must
//! be the encoder's own reconstruction, byte for byte, or the two drift apart.

use super::*;

/// a deterministic pseudo-random sequence
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
    fn below(&mut self, n: usize) -> usize {
        self.next() as usize % n.max(1)
    }
}

/// a screen-like picture at the padded size: background, gradient, text-ish glyphs
struct Pic {
    w: usize,
    h: usize,
    buf: Vec<u8>,
}

impl Pic {
    fn new(pw: usize, ph: usize) -> Pic {
        let mut p = Pic { w: pw, h: ph, buf: vec![0; pw * ph * 3 / 2] };
        let (y, c) = p.buf.split_at_mut(pw * ph);
        for (i, v) in y.iter_mut().enumerate() {
            *v = 200 + ((i % pw) * 30 / pw) as u8;
        }
        c.fill(128);
        p
    }
    fn frame(&self) -> Frame<'_> {
        let n = self.w * self.h;
        Frame { y: &self.buf[..n], u: &self.buf[n..n + n / 4], v: &self.buf[n + n / 4..] }
    }
    /// glyph-like strokes in a box, a colour
    fn glyphs(&mut self, r: &mut Rng, x: usize, y: usize, w: usize, h: usize) {
        let (pw, ph) = (self.w, self.h);
        let shade = r.below(120) as u8;
        let (cu, cv) = (100 + r.below(56) as u8, 100 + r.below(56) as u8);
        for yy in y..(y + h).min(ph) {
            for xx in x..(x + w).min(pw) {
                if (xx * 7 + yy * 3 + r.below(5)) % 5 < 2 {
                    self.buf[yy * pw + xx] = shade + ((xx ^ yy) % 7) as u8;
                    let ci = pw * ph + (yy / 2) * (pw / 2) + xx / 2;
                    self.buf[ci] = cu;
                    self.buf[ci + pw * ph / 4] = cv;
                }
            }
        }
    }
    /// every pixel inverted: a full-screen change
    fn invert(&mut self) {
        let n = self.w * self.h;
        for v in &mut self.buf[..n] {
            *v = 255 - *v;
        }
    }
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse: f64 = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

/// the padded I420 picture cropped to w×h
fn crop(buf: &[u8], pw: usize, ph: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    for y in 0..h {
        out.extend_from_slice(&buf[y * pw..y * pw + w]);
    }
    for p in 0..2 {
        let base = pw * ph + p * pw * ph / 4;
        for y in 0..h / 2 {
            out.extend_from_slice(&buf[base + y * pw / 2..base + y * pw / 2 + w / 2]);
        }
    }
    out
}

fn ffmpeg() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// a run of frames encoded, decoded by FFmpeg, compared with the reconstruction
fn run(w: usize, h: usize, threads: usize, seed: u64) {
    if !ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let mut enc = Encoder::new(w, h, 60, threads);
    let (pw, ph) = enc.padded();
    let mut pic = Pic::new(pw, ph);
    // the padding outside w×h never changes: it's cropped away
    let mut rng = Rng(seed);
    let mut stream = Vec::new();
    let mut recons = Vec::new();
    let mut sources = Vec::new();
    let mut sizes = Vec::new();
    let mut qps = Vec::new();
    let steps = 24;
    for step in 0..steps {
        let mut o = Opts { idr: false, qp: 26, refine: None, always: false };
        match step {
            0 => {}
            // typing: a glyph or two
            1..=6 => {
                let (x, y) = (rng.below(w - 16), rng.below(h - 16));
                let gw = 8 + rng.below(10);
                pic.glyphs(&mut rng, x, y, gw, 12);
                enc.mark(x, y, 18, 12);
            }
            // a big redraw: most of the screen
            7 => {
                pic.glyphs(&mut rng, 0, 0, w, h);
                enc.mark_all();
                o.qp = 32;
            }
            // everything inverted, all slices
            8 => {
                pic.invert();
                enc.mark_all();
                o.qp = 30;
            }
            // refinement passes while still
            9..=12 => {
                o.refine = Some((16, 12_000));
            }
            // a keepalive with nothing changed
            13 => o.always = true,
            // damage over pixels that didn't change: nothing to send
            14 => enc.mark(0, 0, 64, 64),
            15 => o.idr = true,
            // low and high qps, small and medium changes
            _ => {
                let (x, y) = (rng.below(w / 2), rng.below(h / 2));
                let (bw, bh) = (16 + rng.below(w / 2), 16 + rng.below(h / 2));
                pic.glyphs(&mut rng, x, y, bw, bh);
                enc.mark(x, y, bw, bh);
                o.qp = [12, 18, 40, 51, 24, 28, 22, 34][step % 8];
            }
        }
        let d = enc.encode(&pic.frame(), &o);
        if step == 14 {
            assert!(!d.frame, "damage over unchanged pixels sent a frame");
            continue;
        }
        if (9..=12).contains(&step) && !d.frame {
            // refined already: nothing left to sharpen
            assert_eq!(enc.coarse(16), 0);
            continue;
        }
        assert!(d.frame, "step {step}: no frame");
        for n in enc.nals() {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(n);
        }
        sizes.push(d.bytes);
        qps.push(o.qp);
        recons.push(crop(enc.recon(), pw, ph, w, h));
        sources.push(crop(&pic.buf, pw, ph, w, h));
    }
    let dir = std::env::temp_dir().join(format!("afterglow-avc-{}-{w}x{h}-{threads}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (inp, out) = (dir.join("s.h264"), dir.join("d.yuv"));
    std::fs::write(&inp, &stream).unwrap();
    let r = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-err_detect", "explode", "-i"])
        .arg(&inp)
        .args(["-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-y"])
        .arg(&out)
        .output()
        .unwrap();
    let errs = String::from_utf8_lossy(&r.stderr);
    assert!(r.status.success() && errs.trim().is_empty(), "ffmpeg: {errs}");
    let dec = std::fs::read(&out).unwrap();
    let fsz = w * h * 3 / 2;
    let mut coarse = false;
    assert_eq!(dec.len(), fsz * recons.len(), "frames decoded");
    for (i, (rec, src)) in recons.iter().zip(&sources).enumerate() {
        let got = &dec[i * fsz..(i + 1) * fsz];
        if got != &rec[..] {
            let at = got.iter().zip(rec).position(|(a, b)| a != b).unwrap();
            let plane = if at < w * h {
                "Y"
            } else if at < w * h * 5 / 4 {
                "U"
            } else {
                "V"
            };
            let (px, py) = if at < w * h {
                (at % w, at / w)
            } else {
                let c = (at - w * h) % (w * h / 4);
                (c % (w / 2) * 2, c / (w / 2) * 2)
            };
            panic!(
                "frame {i}: decoded differs from the reconstruction at {plane} ({px}, {py}) near macroblock ({}, {})",
                px / 16,
                py / 16
            );
        }
        let q = psnr(&got[..w * h], &src[..w * h]);
        eprintln!("{w}x{h} frame {i}: {} bytes, Y psnr {q:.1} dB", sizes[i]);
        // what a qp of 40 and up leaves of noisy glyphs is coarse by design, and stays on
        // the picture until coded again
        coarse |= qps[i] >= 40;
        let floor = if coarse { 18.0 } else { 26.0 };
        assert!(q > floor, "frame {i} at qp {}: Y psnr {q:.1}", qps[i]);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn decoded_by_ffmpeg_exactly_as_reconstructed() {
    run(320, 192, 1, 1);
}

#[test]
fn sliced_across_threads_and_cropped() {
    run(500, 282, 3, 7);
}

#[test]
fn a_typed_glyph_is_a_small_frame_of_skips() {
    let (w, h) = (1920, 1080);
    let mut enc = Encoder::new(w, h, 120, 4);
    let (pw, ph) = enc.padded();
    let mut pic = Pic::new(pw, ph);
    let o = Opts { idr: false, qp: 24, refine: None, always: false };
    assert!(enc.encode(&pic.frame(), &o).key);
    let mut rng = Rng(3);
    pic.glyphs(&mut rng, 900, 500, 9, 14);
    enc.mark(900, 500, 9, 14);
    let t = std::time::Instant::now();
    let d = enc.encode(&pic.frame(), &o);
    let took = t.elapsed();
    assert!(d.frame && !d.key);
    assert!(d.coded >= 1 && d.coded <= 4, "{d:?}");
    assert!(d.bytes < 600, "{d:?}");
    eprintln!("a glyph at 1080p: {d:?} in {took:?}");
}

#[test]
fn the_sps_says_nothing_is_reordered() {
    let enc = Encoder::new(1280, 720, 120, 1);
    assert_eq!(crate::h264::reorder_frames(&enc.sps), Some(0));
    // the link's rewrite leaves it as it is
    assert!(crate::h264::low_delay_sps(&enc.sps).is_none());
    // the avcC record names Constrained Baseline
    let rec = crate::h264::avcc(&enc.sps, &enc.pps);
    assert_eq!(&rec[1..3], &[66, 0xc0]);
}

#[test]
#[ignore]
fn speed() {
    let threads: usize = std::env::var("AVC_THREADS").ok().and_then(|t| t.parse().ok()).unwrap_or(4);
    for (w, h) in [(1280, 720), (1920, 1080)] {
        let mut enc = Encoder::new(w, h, 120, threads);
        let (pw, ph) = enc.padded();
        let mut pic = Pic::new(pw, ph);
        let mut rng = Rng(9);
        // a terminal's worth of text: lines of glyphs, a third of the screen
        for line in 0..h / 3 / 16 {
            for col in 0..w / 9 / 2 {
                pic.glyphs(&mut rng, col * 9, line * 16, 7, 12);
            }
        }
        let o = Opts { idr: false, qp: 26, refine: None, always: false };
        enc.encode(&pic.frame(), &o);
        let n = 50;
        let (mut bytes, mut took) = (0, std::time::Duration::ZERO);
        for _ in 0..n {
            pic.invert();
            enc.mark_all();
            let t = std::time::Instant::now();
            bytes += enc.encode(&pic.frame(), &o).bytes;
            took += t.elapsed();
        }
        eprintln!(
            "{w}x{h} every pixel changed: {:.2} ms a frame, {} KiB",
            took.as_secs_f64() * 1000.0 / n as f64,
            bytes / n / 1024
        );
        let mut took = std::time::Duration::ZERO;
        for i in 0..n {
            pic.glyphs(&mut rng, 100 + i * 9, 300, 9, 14);
            enc.mark(100 + i * 9, 300, 9, 14);
            let t = std::time::Instant::now();
            enc.encode(&pic.frame(), &o);
            took += t.elapsed();
        }
        eprintln!("{w}x{h} a glyph: {:.1} us a frame", took.as_secs_f64() * 1e6 / n as f64);
    }
}
