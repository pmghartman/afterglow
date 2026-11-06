//! A display captured with DXGI desktop duplication, on the input desktop so UAC and
//! the lock screen are captured too, the pointer drawn in: duplication hands it apart.
//!
//! For afterglow's own encoder (crate::avc) the capture is read back to memory, and
//! only what changed: each frame's dirty and move rectangles, as duplication reports
//! them, are copied on the GPU into a staging texture that mirrors the desktop, then
//! read out of it at once with one map. A typed glyph is a few hundred bytes across
//! the bus, not the whole desktop.

use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
use windows::core::{Interface, Result};

use super::display::Output;
use crate::host::Cursor;

pub fn device_for(o: &Output) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut dev = None;
    let mut ctx = None;
    // SAFETY: out-params written by D3D11CreateDevice
    unsafe {
        // the full set first; a software adapter (a runner's Basic Render Driver) can refuse
        // video support or level 11.1, so those are dropped in turn
        let tries: [(D3D11_CREATE_DEVICE_FLAG, &[D3D_FEATURE_LEVEL]); 3] = [
            (
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                &[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0],
            ),
            (D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, &[D3D_FEATURE_LEVEL_11_0]),
            (D3D11_CREATE_DEVICE_BGRA_SUPPORT, &[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1]),
        ];
        let mut last = None;
        for (flags, levels) in tries {
            match D3D11CreateDevice(
                &o.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            ) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        if let Some(e) = last {
            return Err(e);
        }
        let dev: ID3D11Device = dev.unwrap();
        // Media Foundation's threads share this device
        if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        // the GPU's highest priority for our copies, conversion and encode, so a game
        // that fills the GPU doesn't queue the capture behind its own frames (SYSTEM
        // has the privilege it takes)
        if let Ok(d) = dev.cast::<IDXGIDevice>() {
            let _ = d.SetGPUThreadPriority(7);
        }
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        Ok((dev, ctx.unwrap()))
    }
}

pub fn texture(
    dev: &ID3D11Device,
    w: u32,
    h: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    bind: D3D11_BIND_FLAG,
    usage: D3D11_USAGE,
    cpu: D3D11_CPU_ACCESS_FLAG,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: cpu.0 as u32,
        MiscFlags: 0,
    };
    let mut t = None;
    // SAFETY: desc initialised, t written on success
    unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t))? };
    Ok(t.unwrap())
}

#[derive(Default)]
struct Pointer {
    visible: bool,
    x: i32,
    y: i32,
    shape: Vec<u8>,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
}

pub enum Got {
    /// the picture or the pointer changed: comp holds the frame
    Frame,
    Timeout,
}

pub struct Dup {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    pub w: u32,
    pub h: u32,
    /// the desktop as last captured, without the pointer, when it's drawn in
    desk: Option<ID3D11Texture2D>,
    /// what's encoded: the desktop, with the pointer drawn when it's drawn in
    pub comp: ID3D11Texture2D,
    ptr: Pointer,
    has_image: bool,
    /// the pointer drawn into comp. Otherwise its shape is the viewer's, and a pointer
    /// move costs no copy, no compose and no frame
    draw: bool,
    /// the shape or visibility changed since cursor() last said
    shape_dirty: bool,
    /// the desktop mirrored in memory the CPU reads, kept up to date by each frame's
    /// changed rectangles; and those rectangles since read() last took them
    stage: Option<ID3D11Texture2D>,
    rects: Vec<Rect>,
    /// the whole desktop is to be copied into the stage at the next frame
    whole: bool,
    meta: Vec<u8>,
    /// where the pointer is drawn in, kept from frame to frame at its size
    ptr_stage: Option<(ID3D11Texture2D, u32, u32)>,
}

/// a rectangle of the desktop, in pixels
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// clipped to w×h and grown to even edges (each 2×2 chroma block whole)
    fn even(self, w: u32, h: u32) -> Option<Rect> {
        let (x0, y0) = (self.x.min(w) & !1, self.y.min(h) & !1);
        let x1 = ((self.x + self.w).min(w) + 1).min(w & !1) & !1;
        let y1 = ((self.y + self.h).min(h) + 1).min(h & !1) & !1;
        (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }
}

/// why a capture must be set up again
pub enum Lost {
    /// the mode changed, the desktop switched, the device went: set up again now
    Again,
    /// the secure desktop and no right to it: wait, then again, for as long as it lasts
    Wait(String),
    /// the display can't be captured: counted, given up on when it keeps failing
    Fail(String),
}

impl Dup {
    /// `cpu`: the changed rectangles read back to memory (for crate::avc) rather than
    /// the frame kept on the GPU
    pub fn new(
        o: &Output,
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        draw: bool,
        cpu: bool,
    ) -> std::result::Result<Dup, Lost> {
        // SAFETY: COM calls on owned objects
        unsafe {
            let dup = match o.output.cast::<IDXGIOutput5>() {
                Ok(o5) => o5.DuplicateOutput1(dev, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]),
                Err(_) => o.output.cast::<IDXGIOutput1>().and_then(|o1| o1.DuplicateOutput(dev)),
            }
            .or_else(|e| {
                // an older driver turning down DuplicateOutput1: the plain one
                if e.code() == DXGI_ERROR_UNSUPPORTED {
                    o.output.cast::<IDXGIOutput1>().and_then(|o1| o1.DuplicateOutput(dev))
                } else {
                    Err(e)
                }
            })
            .map_err(|e| {
                if e.code() == E_ACCESSDENIED {
                    Lost::Wait("the secure desktop can't be captured".into())
                } else {
                    Lost::Fail(format!("the display can't be duplicated: {e}"))
                }
            })?;
            let d = dup.GetDesc();
            let (w, h) = (d.ModeDesc.Width, d.ModeDesc.Height);
            let bind = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
            let mk =
                || texture(dev, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, bind, D3D11_USAGE_DEFAULT, D3D11_CPU_ACCESS_FLAG(0));
            let desk = if draw { Some(mk().map_err(|e| Lost::Fail(e.to_string()))?) } else { None };
            let comp = mk().map_err(|e| Lost::Fail(e.to_string()))?;
            let stage = if cpu {
                let t = texture(
                    dev,
                    w,
                    h,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_BIND_FLAG(0),
                    D3D11_USAGE_STAGING,
                    D3D11_CPU_ACCESS_READ,
                )
                .map_err(|e| Lost::Fail(e.to_string()))?;
                Some(t)
            } else {
                None
            };
            Ok(Dup {
                dev: dev.clone(),
                ctx: ctx.clone(),
                dup,
                w,
                h,
                desk,
                comp,
                ptr: Pointer::default(),
                has_image: false,
                draw,
                shape_dirty: true,
                stage,
                rects: Vec::new(),
                whole: true,
                meta: Vec::new(),
                ptr_stage: None,
            })
        }
    }

    /// the next change within timeout_ms
    pub fn next(&mut self, timeout_ms: u32) -> std::result::Result<Got, Lost> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res = None;
        // SAFETY: COM calls on owned objects; every acquired frame is released
        unsafe {
            match self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Got::Timeout),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Err(Lost::Again),
                Err(e) => return Err(Lost::Fail(format!("capture failed: {e}"))),
            }
            let mut changed = false;
            if info.LastPresentTime != 0 {
                if let Some(t) = res.as_ref().and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                    match self.stage.clone() {
                        Some(stage) => self.mirror(&stage, &t, info.TotalMetadataBufferSize),
                        None => self.ctx.CopyResource(self.desk.as_ref().unwrap_or(&self.comp), &t),
                    }
                    self.has_image = true;
                    changed = true;
                }
            }
            if info.LastMouseUpdateTime != 0 {
                let p = info.PointerPosition;
                let visible = p.Visible.as_bool();
                self.shape_dirty |= visible != self.ptr.visible;
                self.ptr.visible = visible;
                self.ptr.x = p.Position.x;
                self.ptr.y = p.Position.y;
                changed |= self.draw;
            }
            if info.PointerShapeBufferSize > 0 {
                let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
                let mut need = 0;
                let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                if self
                    .dup
                    .GetFramePointerShape(buf.len() as u32, buf.as_mut_ptr() as *mut _, &mut need, &mut si)
                    .is_ok()
                {
                    self.ptr.shape = buf;
                    self.ptr.info = si;
                    self.shape_dirty = true;
                    changed |= self.draw;
                }
            }
            let _ = self.dup.ReleaseFrame();
            if !changed || !self.has_image {
                return Ok(Got::Timeout);
            }
        }
        if self.draw {
            self.compose().map_err(|e| Lost::Fail(format!("the pointer couldn't be drawn: {e}")))?;
        }
        Ok(Got::Frame)
    }

    /// what changed in the acquired frame `t` copied into the stage, and remembered for
    /// read(): duplication's dirty rectangles and its moves' destinations; all of it the
    /// first time, or when it says nothing
    fn mirror(&mut self, stage: &ID3D11Texture2D, t: &ID3D11Texture2D, meta: u32) {
        let (w, h) = (self.w, self.h);
        let mut got = Vec::new();
        if !self.whole && meta > 0 {
            self.meta.resize(meta as usize, 0);
            let mut need = 0u32;
            // SAFETY: the buffer is meta bytes, as duplication asked for; read as its structs
            unsafe {
                if self
                    .dup
                    .GetFrameMoveRects(meta, self.meta.as_mut_ptr() as *mut DXGI_OUTDUPL_MOVE_RECT, &mut need)
                    .is_ok()
                {
                    let n = need as usize / size_of::<DXGI_OUTDUPL_MOVE_RECT>();
                    let mv = std::slice::from_raw_parts(self.meta.as_ptr() as *const DXGI_OUTDUPL_MOVE_RECT, n);
                    got.extend(mv.iter().map(|m| m.DestinationRect));
                } else {
                    self.whole = true;
                }
                let mut need = 0u32;
                if self.dup.GetFrameDirtyRects(meta, self.meta.as_mut_ptr() as *mut RECT, &mut need).is_ok() {
                    let n = need as usize / size_of::<RECT>();
                    got.extend_from_slice(std::slice::from_raw_parts(self.meta.as_ptr() as *const RECT, n));
                } else {
                    self.whole = true;
                }
            }
        }
        if self.whole || (meta == 0 && got.is_empty()) {
            // SAFETY: both textures are the desktop's size and format
            unsafe { self.ctx.CopyResource(stage, t) };
            self.rects.clear();
            self.rects.push(Rect { x: 0, y: 0, w, h });
            self.whole = false;
            return;
        }
        for r in got {
            let (x0, y0) = (r.left.max(0) as u32, r.top.max(0) as u32);
            let (x1, y1) = ((r.right.max(0) as u32).min(w), (r.bottom.max(0) as u32).min(h));
            let Some(r) = (Rect { x: x0, y: y0, w: x1.saturating_sub(x0), h: y1.saturating_sub(y0) }).even(w, h) else {
                continue;
            };
            let bx = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + r.w, bottom: r.y + r.h, back: 1 };
            // SAFETY: a box within both textures
            unsafe { self.ctx.CopySubresourceRegion(stage, 0, r.x, r.y, 0, t, 0, Some(&bx)) };
            self.rects.push(r);
        }
    }

    /// the rectangles changed since the last read, each with its pixels (BGRA rows
    /// `pitch` bytes apart, even-aligned): one map of the stage, which waits for the
    /// GPU's copies into it. Nothing when nothing changed
    pub fn read(&mut self, mut each: impl FnMut(&Rect, &[u8], usize)) -> std::result::Result<(), Lost> {
        let Some(stage) = &self.stage else { return Ok(()) };
        if self.rects.is_empty() {
            return Ok(());
        }
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the stage is mapped for reading between Map and Unmap, rows by its pitch
        unsafe {
            self.ctx
                .Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut m))
                .map_err(|e| Lost::Fail(format!("the capture couldn't be read: {e}")))?;
            let pitch = m.RowPitch as usize;
            let all = std::slice::from_raw_parts(m.pData as *const u8, pitch * self.h as usize);
            for r in self.rects.drain(..) {
                let at = r.y as usize * pitch + r.x as usize * 4;
                let n = (r.h as usize - 1) * pitch + r.w as usize * 4;
                each(&r, &all[at..at + n], pitch);
            }
            self.ctx.Unmap(stage, 0);
        }
        Ok(())
    }

    /// the whole desktop as it stands (BGRA rows `pitch` bytes apart), after any change:
    /// for a video of another size, scaled from all of it
    pub fn read_whole(&mut self, each: impl FnOnce(&[u8], usize)) -> std::result::Result<(), Lost> {
        let Some(stage) = &self.stage else { return Ok(()) };
        if self.rects.is_empty() {
            return Ok(());
        }
        self.rects.clear();
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: as read()
        unsafe {
            self.ctx
                .Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut m))
                .map_err(|e| Lost::Fail(format!("the capture couldn't be read: {e}")))?;
            let pitch = m.RowPitch as usize;
            each(std::slice::from_raw_parts(m.pData as *const u8, pitch * self.h as usize), pitch);
            self.ctx.Unmap(stage, 0);
        }
        Ok(())
    }

    /// the pointer's shape, when it or whether it shows changed since last asked
    pub fn cursor(&mut self) -> Option<Cursor> {
        if !std::mem::take(&mut self.shape_dirty) {
            return None;
        }
        let p = &self.ptr;
        if !p.visible || p.shape.is_empty() {
            return Some(Cursor::default());
        }
        Some(cursor_of(&p.shape, &p.info))
    }

    /// comp: the desktop and the pointer over it
    fn compose(&mut self) -> Result<()> {
        let Some(desk) = &self.desk else { return Ok(()) };
        // SAFETY: COM calls on owned textures
        unsafe { self.ctx.CopyResource(&self.comp, desk) };
        let p = &self.ptr;
        if !p.visible || p.shape.is_empty() {
            return Ok(());
        }
        let si = p.info;
        let mono = si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
        let (cw, ch) = (si.Width as i32, if mono { si.Height as i32 / 2 } else { si.Height as i32 });
        // the pointer's box, clipped to the display
        let (x0, y0) = (p.x.max(0), p.y.max(0));
        let (x1, y1) = ((p.x + cw).min(self.w as i32), (p.y + ch).min(self.h as i32));
        if x1 <= x0 || y1 <= y0 {
            return Ok(());
        }
        let (bw, bh) = ((x1 - x0) as u32, (y1 - y0) as u32);
        let stage = match &self.ptr_stage {
            Some((t, w, h)) if (*w, *h) == (bw, bh) => t.clone(),
            _ => {
                let t = texture(
                    &self.dev,
                    bw,
                    bh,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_BIND_FLAG(0),
                    D3D11_USAGE_STAGING,
                    D3D11_CPU_ACCESS_READ | D3D11_CPU_ACCESS_WRITE,
                )?;
                self.ptr_stage = Some((t.clone(), bw, bh));
                t
            }
        };
        let p = &self.ptr;
        let bx = D3D11_BOX { left: x0 as u32, top: y0 as u32, front: 0, right: x1 as u32, bottom: y1 as u32, back: 1 };
        // SAFETY: the staging texture is mapped only between Map and Unmap, rows by its pitch
        unsafe {
            self.ctx.CopySubresourceRegion(&stage, 0, 0, 0, 0, &self.comp, 0, Some(&bx));
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&stage, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut m))?;
            let base = m.pData as *mut u8;
            let pitch = si.Pitch as usize;
            for y in 0..bh as i32 {
                let row = std::slice::from_raw_parts_mut(base.add(y as usize * m.RowPitch as usize), bw as usize * 4);
                let sy = (y0 + y - p.y) as usize;
                for x in 0..bw as i32 {
                    let sx = (x0 + x - p.x) as usize;
                    let d = &mut row[x as usize * 4..x as usize * 4 + 4];
                    blend(&p.shape, pitch, si.Type, ch as usize, sx, sy, d);
                }
            }
            self.ctx.Unmap(&stage, 0);
            self.ctx.CopySubresourceRegion(&self.comp, 0, x0 as u32, y0 as u32, 0, &stage, 0, None);
        }
        Ok(())
    }
}

/// one pointer pixel over one desktop pixel (BGRA)
fn blend(shape: &[u8], pitch: usize, ty: u32, h: usize, x: usize, y: usize, d: &mut [u8]) {
    if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        // an AND mask over an XOR mask, a bit a pixel
        let bit = 0x80 >> (x % 8);
        let and = shape[y * pitch + x / 8] & bit != 0;
        let xor = shape[(y + h) * pitch + x / 8] & bit != 0;
        for c in &mut d[..3] {
            *c = match (and, xor) {
                (false, false) => 0,
                (false, true) => 255,
                (true, false) => *c,
                (true, true) => !*c,
            };
        }
        return;
    }
    let s = &shape[y * pitch + x * 4..y * pitch + x * 4 + 4];
    if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
        // alpha 0: the color; alpha 0xff: XORed with the screen
        for i in 0..3 {
            d[i] = if s[3] == 0 { s[i] } else { d[i] ^ s[i] };
        }
        return;
    }
    let a = s[3] as u32;
    for i in 0..3 {
        d[i] = ((s[i] as u32 * a + d[i] as u32 * (255 - a)) / 255) as u8;
    }
}

/// DXGI's pointer shape as straight-alpha RGBA for a browser's cursor. What inverts the
/// screen under it (a monochrome XOR, a masked colour's XOR pixels) can't be said in
/// RGBA: it's drawn black, so an I-beam stays visible on the light pages it's mostly over
pub fn cursor_of(shape: &[u8], si: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> Cursor {
    let mono = si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
    let masked = si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
    let (w, h) = (si.Width as usize, if mono { si.Height as usize / 2 } else { si.Height as usize });
    let pitch = si.Pitch as usize;
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let d = &mut rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
            if mono {
                let bit = 0x80 >> (x % 8);
                let (Some(&a), Some(&b)) = (shape.get(y * pitch + x / 8), shape.get((y + h) * pitch + x / 8)) else {
                    continue;
                };
                d.copy_from_slice(match (a & bit != 0, b & bit != 0) {
                    (false, false) => &[0, 0, 0, 255],
                    (false, true) => &[255, 255, 255, 255],
                    (true, false) => &[0, 0, 0, 0],
                    (true, true) => &[0, 0, 0, 255],
                });
                continue;
            }
            let Some(s) = shape.get(y * pitch + x * 4..y * pitch + x * 4 + 4) else { continue };
            if masked {
                // alpha 0: the colour as it is; 0xff: XORed with the screen, nothing where it's 0
                let xor = s[3] != 0;
                let none = s[..3] == [0, 0, 0];
                d.copy_from_slice(&match (xor, none) {
                    (false, _) => [s[2], s[1], s[0], 255],
                    (true, true) => [0, 0, 0, 0],
                    (true, false) => [0, 0, 0, 255],
                });
                continue;
            }
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
    }
    Cursor {
        visible: true,
        w: w as u32,
        h: h as u32,
        hot_x: si.HotSpot.x.max(0) as u32,
        hot_y: si.HotSpot.y.max(0) as u32,
        rgba,
    }
}
