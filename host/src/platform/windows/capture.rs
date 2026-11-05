//! A display captured with DXGI desktop duplication, on the input desktop so UAC and
//! the lock screen are captured too, the pointer drawn in: duplication hands it apart.

use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
use windows::core::{Interface, Result};

use super::display::Output;
use super::input::sync_desktop;

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
    /// the desktop as last captured, without the pointer
    desk: ID3D11Texture2D,
    /// the desktop with the pointer drawn: what's encoded
    pub comp: ID3D11Texture2D,
    ptr: Pointer,
    has_image: bool,
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
    pub fn new(o: &Output, dev: &ID3D11Device, ctx: &ID3D11DeviceContext) -> std::result::Result<Dup, Lost> {
        sync_desktop();
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
            let desk = mk().map_err(|e| Lost::Fail(e.to_string()))?;
            let comp = mk().map_err(|e| Lost::Fail(e.to_string()))?;
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
                    self.ctx.CopyResource(&self.desk, &t);
                    self.has_image = true;
                    changed = true;
                }
            }
            if info.LastMouseUpdateTime != 0 {
                let p = info.PointerPosition;
                self.ptr.visible = p.Visible.as_bool();
                self.ptr.x = p.Position.x;
                self.ptr.y = p.Position.y;
                changed = true;
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
                }
            }
            let _ = self.dup.ReleaseFrame();
            if !changed || !self.has_image {
                return Ok(Got::Timeout);
            }
        }
        self.compose().map_err(|e| Lost::Fail(format!("the pointer couldn't be drawn: {e}")))?;
        Ok(Got::Frame)
    }

    /// comp: the desktop and the pointer over it
    fn compose(&mut self) -> Result<()> {
        // SAFETY: COM calls on owned textures
        unsafe { self.ctx.CopyResource(&self.comp, &self.desk) };
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
        let stage = texture(
            &self.dev,
            bw,
            bh,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_FLAG(0),
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ | D3D11_CPU_ACCESS_WRITE,
        )?;
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
