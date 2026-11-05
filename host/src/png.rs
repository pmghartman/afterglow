//! A pointer's shape as a PNG, what a browser takes for a CSS cursor: RGBA, deflate's
//! stored blocks (a cursor is a few KiB; compressing it would cost more than sending it)

fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, e) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let at = out.len();
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let crc = crc32(&out[at..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// w×h straight-alpha RGBA, rows top to bottom, as a PNG
pub fn rgba(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
    let row = w as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * h as usize);
    for r in px.chunks_exact(row).take(h as usize) {
        raw.push(0);
        raw.extend_from_slice(r);
    }
    let mut z = vec![0x78, 0x01];
    let blocks = raw.chunks(65535);
    let n = blocks.len().max(1);
    if raw.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    for (i, b) in blocks.enumerate() {
        z.push((i + 1 == n) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_as_zlib_and_png_have_them() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn a_png_laid_out() {
        let p = rgba(2, 2, &[255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 9, 9, 9, 9]);
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&p[12..16], b"IHDR");
        assert_eq!(&p[16..24], &[0, 0, 0, 2, 0, 0, 0, 2]);
        assert_eq!(&p[p.len() - 12..], &[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]);
    }
}
