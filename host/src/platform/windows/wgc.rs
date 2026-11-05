//! One window captured with Windows.Graphics.Capture, as OBS's window capture and
//! Discord's app streaming do: the window's own pixels whatever covers it, no desktop,
//! no taskbar, no other app, no pointer and no border. Each frame says what changed in
//! it (its dirty regions, Windows 11 24H2 and later), and only that is copied out of the
//! frame's GPU texture into a staging texture that mirrors the window, where the CPU
//! reads it. A frame comes only when the window changes.

use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureDirtyRegionMode, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::SetEvent;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{Interface, Result};

/// a rectangle in pixels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = (self.right().min(o.right()), self.bottom().min(o.bottom()));
        Rect { x: x0, y: y0, w: (x1 - x0).max(0), h: (y1 - y0).max(0) }
    }
    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, ..*self }
    }
}

/// the WinRT device capture wants, over a D3D11 device
pub fn winrt_device(dev: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = dev.cast()?;
    // SAFETY: a device the caller owns, wrapped
    let i = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    i.cast()
}

pub struct Window {
    pub hwnd: isize,
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// the pool's buffers' size
    size: SizeInt32,
    /// the window as last captured, where the CPU reads it, and its size
    stage: Option<ID3D11Texture2D>,
    pub w: i32,
    pub h: i32,
    /// all of it to be copied with the next frame
    whole: bool,
    /// frames say what changed in them
    pub dirty_regions: bool,
}

// SAFETY: the free-threaded pool and its session are agile; the textures are used by
// the one thread that owns this
unsafe impl Send for Window {}

impl Window {
    /// capture of hwnd started; each frame it makes sets `wake`
    pub fn new(hwnd: isize, wdev: &IDirect3DDevice, wake: HANDLE) -> Result<Window> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: a window handle, which capture checks
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(hwnd as *mut _))? };
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            wdev,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            SizeInt32 { Width: size.Width.max(1), Height: size.Height.max(1) },
        )?;
        let ev = wake.0 as usize;
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            // SAFETY: an event the capture loop owns for as long as any pool lives
            unsafe {
                let _ = SetEvent(HANDLE(ev as *mut _));
            }
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        // the viewer draws its own pointer; no yellow border around the window
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        let dirty_regions = session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportOnly).is_ok();
        // as often as it changes, not the display's refresh at most
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000 });
        session.StartCapture()?;
        Ok(Window { hwnd, _item: item, pool, session, size, stage: None, w: 0, h: 0, whole: true, dirty_regions })
    }

    /// the frames come since last asked: what changed in them copied into the mirror,
    /// and said in `changed` (the window's own coordinates). Whether any came
    pub fn take(
        &mut self,
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        wdev: &IDirect3DDevice,
        changed: &mut Vec<Rect>,
    ) -> Result<bool> {
        let mut got = false;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            got = true;
            let content = f.ContentSize()?;
            if content.Width != self.size.Width || content.Height != self.size.Height {
                // resized: buffers at its new size from the next frame, all of it copied
                self.size = SizeInt32 { Width: content.Width.max(1), Height: content.Height.max(1) };
                self.pool.Recreate(wdev, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, self.size)?;
                self.whole = true;
            }
            let access: IDirect3DDxgiInterfaceAccess = f.Surface()?.cast()?;
            // SAFETY: the frame's texture, used before the frame is closed
            let tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: desc written
            unsafe { tex.GetDesc(&mut desc) };
            let (w, h) = (content.Width.min(desc.Width as i32), content.Height.min(desc.Height as i32));
            if w <= 0 || h <= 0 {
                continue;
            }
            if self.stage.is_none() || (w, h) != (self.w, self.h) {
                self.stage = Some(staging(dev, w as u32, h as u32)?);
                (self.w, self.h) = (w, h);
                self.whole = true;
            }
            let stage = self.stage.as_ref().unwrap();
            let all = Rect { x: 0, y: 0, w, h };
            let mut rects = Vec::new();
            if !self.whole && self.dirty_regions {
                if let Ok(v) = f.DirtyRegions() {
                    for r in v {
                        rects.push(Rect { x: r.X, y: r.Y, w: r.Width, h: r.Height }.intersect(&all));
                    }
                }
            } else {
                rects.push(all);
            }
            self.whole = false;
            for r in rects.into_iter().filter(|r| !r.is_empty()) {
                let bx = D3D11_BOX {
                    left: r.x as u32,
                    top: r.y as u32,
                    front: 0,
                    right: r.right() as u32,
                    bottom: r.bottom() as u32,
                    back: 1,
                };
                // SAFETY: a box within both textures
                unsafe { ctx.CopySubresourceRegion(stage, 0, r.x as u32, r.y as u32, 0, &tex, 0, Some(&bx)) };
                changed.push(r);
            }
            let _ = f.Close();
        }
        Ok(got)
    }

    /// the mirror mapped for reading: its BGRA rows, pitch bytes apart
    pub fn read<T>(&self, ctx: &ID3D11DeviceContext, f: impl FnOnce(&[u8], usize) -> T) -> Option<T> {
        let stage = self.stage.as_ref()?;
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: mapped between Map and Unmap, rows by its pitch
        unsafe {
            ctx.Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
            let pitch = m.RowPitch as usize;
            let r = f(std::slice::from_raw_parts(m.pData as *const u8, pitch * self.h as usize), pitch);
            ctx.Unmap(stage, 0);
            Some(r)
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn staging(dev: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut t = None;
    // SAFETY: desc initialised, t written on success
    unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t))? };
    Ok(t.unwrap())
}
