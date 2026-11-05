//! Frames to H.264: the GPU's video processor turns the BGRA desktop into NV12 at the
//! video's size (letterboxed, BT.709 limited), then Media Foundation's encoder for that
//! GPU (NVENC, AMF and Quick Sync each register one) makes the access units in low
//! latency mode; Microsoft's software encoder when there's no hardware one.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_NOTIMPL, LUID, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_NV12,
    DXGI_RATIONAL,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, Result};

use super::capture::texture;
use crate::host::VideoCfg;
use crate::{debug, info, warn};

/// BGRA at the capture's size in, NV12 at the video's size out
struct Convert {
    vctx: ID3D11VideoContext,
    vp: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    output: ID3D11VideoProcessorOutputView,
}

impl Convert {
    #[allow(clippy::too_many_arguments)]
    fn new(
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        sw: u32,
        sh: u32,
        dst: &ID3D11Texture2D,
        w: u32,
        h: u32,
        fps: u32,
    ) -> Result<Convert> {
        // SAFETY: COM calls on owned objects, descriptors initialised
        unsafe {
            let vdev: ID3D11VideoDevice = dev.cast()?;
            let vctx: ID3D11VideoContext = ctx.cast()?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: sw,
                InputHeight: sh,
                OutputFrameRate: rate,
                OutputWidth: w,
                OutputHeight: h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = vdev.CreateVideoProcessor(&en, 0)?;
            let mut input = None;
            let iv = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            vdev.CreateVideoProcessorInputView(src, &en, &iv, Some(&mut input))?;
            let mut output = None;
            let ov = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            vdev.CreateVideoProcessorOutputView(dst, &en, &ov, Some(&mut output))?;
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // the picture fitted inside the video, its aspect kept, black around it
            let scale = (w as f64 / sw as f64).min(h as f64 / sh as f64);
            let (fw, fh) = (((sw as f64 * scale) as i32) & !1, ((sh as f64 * scale) as i32) & !1);
            let (ox, oy) = ((w as i32 - fw) / 2, (h as i32 - fh) / 2);
            vctx.VideoProcessorSetStreamSourceRect(
                &vp,
                0,
                true,
                Some(&RECT { left: 0, top: 0, right: sw as i32, bottom: sh as i32 }),
            );
            vctx.VideoProcessorSetStreamDestRect(
                &vp,
                0,
                true,
                Some(&RECT { left: ox, top: oy, right: ox + fw, bottom: oy + fh }),
            );
            vctx.VideoProcessorSetOutputTargetRect(
                &vp,
                true,
                Some(&RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }),
            );
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    YCbCr: D3D11_VIDEO_COLOR_YCbCrA { Y: 0.0625, Cb: 0.5, Cr: 0.5, A: 1.0 },
                },
            };
            vctx.VideoProcessorSetOutputBackgroundColor(&vp, true, &black);
            Ok(Convert { vctx, vp, input: input.unwrap(), output: output.unwrap() })
        }
    }

    fn run(&self) -> Result<()> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        let streams = [stream];
        // SAFETY: the views belong to this processor's device
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.vp, &self.output, 0, &streams) };
        let [mut s] = streams;
        // SAFETY: the clone taken above, released once
        unsafe { ManuallyDrop::drop(&mut s.pInputSurface) };
        r
    }
}

fn type_info(sub: GUID) -> MFT_REGISTER_TYPE_INFO {
    MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: sub }
}

/// the encoders Media Foundation lists, the first activated that will
fn find(hardware: bool, luid: LUID) -> Option<(IMFTransform, String)> {
    let (input, output) = (type_info(MFVideoFormat_NV12), type_info(MFVideoFormat_H264));
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: the activate array is freed after its entries are taken
    unsafe {
        let r = if hardware {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1).ok()?;
            let attrs = attrs?;
            let bytes = std::slice::from_raw_parts(&luid as *const LUID as *const u8, size_of::<LUID>());
            attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, bytes).ok()?;
            MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &attrs,
                &mut list,
                &mut n,
            )
        } else {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut list,
                &mut n,
            )
        };
        if r.is_err() || list.is_null() {
            return None;
        }
        let acts: Vec<Option<IMFActivate>> = (0..n as usize).map(|i| (*list.add(i)).take()).collect();
        CoTaskMemFree(Some(list as *const c_void));
        for a in acts.into_iter().flatten() {
            let name = {
                let mut p = windows::core::PWSTR::null();
                let mut len = 0;
                if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.0 as *const c_void));
                    s
                } else {
                    "an encoder".to_string()
                }
            };
            match a.ActivateObject::<IMFTransform>() {
                Ok(t) => return Some((t, name)),
                Err(e) => debug!("{name} wouldn't start: {e}"),
            }
        }
        None
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn video_type(sub: GUID, v: &VideoCfg) -> Result<IMFMediaType> {
    // SAFETY: attribute setters on an owned media type
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &sub)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(v.w, v.h))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(v.fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        if sub == MFVideoFormat_H264 {
            t.SetUINT32(&MF_MT_AVG_BITRATE, v.kbps.saturating_mul(1000))?;
        }
        Ok(t)
    }
}

pub struct Encoder {
    ctx: ID3D11DeviceContext,
    convert: Convert,
    nv12: ID3D11Texture2D,
    /// a software encoder's input: the NV12 frame read back
    staging: Option<ID3D11Texture2D>,
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    /// async: inputs the encoder has asked for and not yet been given
    credits: u32,
    provides: bool,
    out_size: u32,
    seq: Vec<u8>,
    n: i64,
    period: i64,
    w: u32,
    h: u32,
    /// MF needs this alive while the encoder uses the device
    _mgr: Option<IMFDXGIDeviceManager>,
}

impl Encoder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        sw: u32,
        sh: u32,
        v: &VideoCfg,
        luid: LUID,
        software: bool,
    ) -> std::result::Result<Encoder, String> {
        let e = |e: windows::core::Error| e.to_string();
        let nv12 = texture(
            dev,
            v.w,
            v.h,
            DXGI_FORMAT_NV12,
            D3D11_BIND_RENDER_TARGET,
            D3D11_USAGE_DEFAULT,
            D3D11_CPU_ACCESS_FLAG(0),
        )
        .map_err(e)?;
        let convert = Convert::new(dev, ctx, src, sw, sh, &nv12, v.w, v.h, v.fps)
            .map_err(|x| format!("the video processor: {x}"))?;
        let mut tried = Vec::new();
        for hw in [true, false] {
            if hw && software {
                continue;
            }
            let Some((mft, name)) = find(hw, luid) else { continue };
            match Self::setup(dev, &mft, v, hw) {
                Ok((events, codec, mgr, staging_needed)) => {
                    let staging = if staging_needed {
                        Some(
                            texture(
                                dev,
                                v.w,
                                v.h,
                                DXGI_FORMAT_NV12,
                                D3D11_BIND_FLAG(0),
                                D3D11_USAGE_STAGING,
                                D3D11_CPU_ACCESS_READ,
                            )
                            .map_err(e)?,
                        )
                    } else {
                        None
                    };
                    // SAFETY: stream info of a configured encoder
                    let info = unsafe { mft.GetOutputStreamInfo(0) }.map_err(e)?;
                    let provides = info.dwFlags
                        & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
                        != 0;
                    let mut enc = Encoder {
                        ctx: ctx.clone(),
                        convert,
                        nv12,
                        staging,
                        mft,
                        events,
                        codec,
                        credits: 0,
                        provides,
                        out_size: info.cbSize.max(v.w * v.h * 3 / 2),
                        seq: Vec::new(),
                        n: 0,
                        period: 10_000_000 / v.fps as i64,
                        w: v.w,
                        h: v.h,
                        _mgr: mgr,
                    };
                    enc.seq = enc.sequence_header();
                    info!("encoding with {name}{}", if hw { "" } else { " (software)" });
                    return Ok(enc);
                }
                Err(x) => {
                    warn!("{name} couldn't be set up: {x}");
                    tried.push(name);
                }
            }
        }
        Err(if tried.is_empty() {
            "no H.264 encoder".into()
        } else {
            format!("no H.264 encoder would start: {}", tried.join(", "))
        })
    }

    #[allow(clippy::type_complexity)]
    fn setup(
        dev: &ID3D11Device,
        mft: &IMFTransform,
        v: &VideoCfg,
        hw: bool,
    ) -> Result<(Option<IMFMediaEventGenerator>, Option<ICodecAPI>, Option<IMFDXGIDeviceManager>, bool)> {
        // SAFETY: COM calls on an encoder this function configures
        unsafe {
            let attrs = mft.GetAttributes()?;
            let mut events = None;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                events = Some(mft.cast::<IMFMediaEventGenerator>()?);
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let mut mgr = None;
            let d3d = hw && attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0;
            if d3d {
                let mut token = 0;
                let mut m = None;
                MFCreateDXGIDeviceManager(&mut token, &mut m)?;
                let m = m.unwrap();
                m.ResetDevice(dev, token)?;
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)?;
                mgr = Some(m);
            }
            let codec = mft.cast::<ICodecAPI>().ok();
            if let Some(c) = &codec {
                let set = |k: &GUID, val: VARIANT| {
                    if let Err(e) = c.SetValue(k, &val) {
                        debug!("encoder setting {k:?} refused: {e}");
                    }
                };
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
                set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
                set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(v.kbps.saturating_mul(1000)));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                // keyframes when asked: a long GOP between them
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(v.fps.saturating_mul(60)));
                set(&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(0u32));
            }
            let out = video_type(MFVideoFormat_H264, v)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            if mft.SetOutputType(0, &out, 0).is_err() {
                // not every encoder makes High
                out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
                mft.SetOutputType(0, &out, 0)?;
            }
            if mft.SetInputType(0, &video_type(MFVideoFormat_NV12, v)?, 0).is_err() {
                // an encoder that wants its own NV12 type: its offer, at our size and rate
                let mut i = 0;
                loop {
                    let t = mft.GetInputAvailableType(0, i)?;
                    i += 1;
                    if t.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12 {
                        continue;
                    }
                    t.SetUINT64(&MF_MT_FRAME_SIZE, pack(v.w, v.h))?;
                    t.SetUINT64(&MF_MT_FRAME_RATE, pack(v.fps, 1))?;
                    mft.SetInputType(0, &t, 0)?;
                    break;
                }
            }
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok((events, codec, mgr, !d3d))
        }
    }

    /// SPS and PPS as the output type carries them, Annex B
    fn sequence_header(&self) -> Vec<u8> {
        // SAFETY: a blob read into a buffer of its size
        unsafe {
            let Ok(t) = self.mft.GetOutputCurrentType(0) else { return Vec::new() };
            let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return Vec::new() };
            let mut b = vec![0u8; n as usize];
            if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut b, None).is_err() {
                return Vec::new();
            }
            b
        }
    }

    fn input_sample(&mut self) -> Result<IMFSample> {
        // SAFETY: COM calls; the staging texture is mapped only while copied from
        unsafe {
            self.convert.run()?;
            let buf = match &self.staging {
                None => MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &self.nv12, 0, false)?,
                Some(st) => {
                    self.ctx.CopyResource(st, &self.nv12);
                    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                    self.ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
                    let (w, h) = (self.w as usize, self.h as usize);
                    let size = w * h * 3 / 2;
                    let mb = MFCreateMemoryBuffer(size as u32)?;
                    let mut p = std::ptr::null_mut();
                    mb.Lock(&mut p, None, None)?;
                    let src = m.pData as *const u8;
                    let pitch = m.RowPitch as usize;
                    // Y rows, then the interleaved UV rows below them
                    for r in 0..h + h / 2 {
                        std::ptr::copy_nonoverlapping(src.add(r * pitch), p.add(r * w), w);
                    }
                    mb.Unlock()?;
                    mb.SetCurrentLength(size as u32)?;
                    self.ctx.Unmap(st, 0);
                    mb
                }
            };
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(self.n * self.period)?;
            s.SetSampleDuration(self.period)?;
            self.n += 1;
            Ok(s)
        }
    }

    /// one output pulled, if the encoder has one
    fn pull(&mut self, out: &mut Vec<(Vec<u8>, bool)>) -> Result<bool> {
        // SAFETY: the output buffer struct's sample and events are released after the call
        unsafe {
            let sample = if self.provides {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                Some(s)
            };
            let mut b = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let r = self.mft.ProcessOutput(0, &mut b, &mut status);
            let [mut b] = b;
            ManuallyDrop::drop(&mut b.pEvents);
            let sample = ManuallyDrop::take(&mut b.pSample);
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // the encoder settled its output: take the type it offers
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    self.seq = self.sequence_header();
                    return Ok(true);
                }
                Err(e) => return Err(e),
            }
            let Some(s) = sample else { return Ok(true) };
            let key = s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buf = s.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0;
            buf.Lock(&mut p, None, Some(&mut len))?;
            let mut au = std::slice::from_raw_parts(p, len as usize).to_vec();
            buf.Unlock()?;
            if key && !crate::wire::nals_of(&au).iter().any(|n| n.ty == 7) {
                if self.seq.is_empty() {
                    self.seq = self.sequence_header();
                }
                // a keyframe without its parameter sets: the type's put in front
                au.splice(0..0, self.seq.iter().copied());
            }
            out.push((au, key));
            Ok(true)
        }
    }

    /// async encoders: what they asked for, and their outputs, as their events say
    fn drain_events(&mut self, out: &mut Vec<(Vec<u8>, bool)>) -> Result<()> {
        let Some(ev) = self.events.clone() else { return Ok(()) };
        loop {
            // SAFETY: a non-blocking event read
            let e = match unsafe { ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e),
            };
            // SAFETY: as above
            match unsafe { e.GetType()? } as i32 {
                t if t == METransformNeedInput.0 => self.credits += 1,
                t if t == METransformHaveOutput.0 => {
                    self.pull(out)?;
                }
                _ => {}
            }
        }
    }

    /// the composed frame encoded: whatever access units came out
    pub fn encode(&mut self, idr: bool) -> std::result::Result<Vec<(Vec<u8>, bool)>, String> {
        self.encode_inner(idr).map_err(|e| format!("the encoder failed: {e}"))
    }

    fn encode_inner(&mut self, idr: bool) -> Result<Vec<(Vec<u8>, bool)>> {
        let mut out = Vec::new();
        if idr {
            if let Some(c) = &self.codec {
                // SAFETY: a plain setting
                let _ = unsafe { c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
            }
        }
        let sample = self.input_sample()?;
        if self.events.is_none() {
            // SAFETY: a synchronous encoder given one frame, then emptied
            unsafe { self.mft.ProcessInput(0, &sample, 0)? };
            while self.pull(&mut out)? {}
            return Ok(out);
        }
        // asynchronous: given the frame when it asks, its output waited for briefly
        let t0 = Instant::now();
        while self.credits == 0 {
            self.drain_events(&mut out)?;
            if self.credits == 0 {
                if t0.elapsed() > Duration::from_secs(2) {
                    return Err(windows::core::Error::new(E_NOTIMPL, "the encoder stopped asking for frames"));
                }
                std::thread::sleep(Duration::from_micros(500));
            }
        }
        self.credits -= 1;
        // SAFETY: an input the encoder asked for
        unsafe { self.mft.ProcessInput(0, &sample, 0)? };
        let t1 = Instant::now();
        let before = out.len();
        while out.len() == before && t1.elapsed() < Duration::from_millis(100) {
            self.drain_events(&mut out)?;
            if out.len() == before {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the stream ended on an encoder this owns
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(ev) = &self.events {
                if let Ok(s) = ev.cast::<IMFShutdown>() {
                    let _ = s.Shutdown();
                }
            }
        }
    }
}
