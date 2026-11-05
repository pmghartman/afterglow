//! OpenH264 driven straight through its C API: every parameter set for latency, and
//! each access unit's NAL units handed on as slices of the encoder's own buffer, no
//! copy between the encoder and the link's frame message.
//!
//! Camera real-time mode, not screen content: measured on xterm at 1080p it encodes a
//! frame in 40% of the time (screen content's scroll and texture analysis runs over
//! every macroblock every frame), its keyframes are as sharp, and its P-frames go on
//! refining text where screen content skips them (44 dB against 40 after ten frames).
//! Low complexity, one reference, no scene-change IDRs (a keyframe is only ever asked
//! for), no frame skipping, one slice per thread so a big change encodes across cores.

use std::os::raw::{c_int, c_void};
use std::ptr::null_mut;

use openh264_sys2::{
    CAMERA_VIDEO_REAL_TIME, CONSTANT_ID, DEBLOCKING_IDC_0, ENCODER_OPTION_DATAFORMAT, ENCODER_OPTION_TRACE_LEVEL,
    ISVCEncoder, LOW_COMPLEXITY, RC_BITRATE_MODE, SEncParamExt, SFrameBSInfo, SM_FIXEDSLCNUM_SLICE, SM_SINGLE_SLICE,
    SSourcePicture, WELS_LOG_QUIET, source::APILoader, videoFormatI420, videoFrameTypeI, videoFrameTypeIDR,
    videoFrameTypeSkip,
};

pub struct H264 {
    enc: *mut ISVCEncoder,
    info: Box<SFrameBSInfo>,
    w: usize,
    h: usize,
}

// SAFETY: the encoder is used by the one thread that owns this
unsafe impl Send for H264 {}

macro_rules! call {
    ($e:expr, $f:ident $(, $a:expr)*) => {
        ((**$e).$f.expect(stringify!($f)))($e $(, $a)*)
    };
}

impl H264 {
    pub fn new(w: usize, h: usize, fps: u32, kbps: u32, threads: usize) -> Result<H264, String> {
        if w > 3840 || h > 2160 || w % 2 != 0 || h % 2 != 0 {
            return Err(format!("OpenH264 can't encode {w}x{h}"));
        }
        let mut enc: *mut ISVCEncoder = null_mut();
        // SAFETY: the encoder is created, configured and only then used; freed in Drop
        unsafe {
            if APILoader::WelsCreateSVCEncoder(&mut enc) != 0 || enc.is_null() {
                return Err("OpenH264 wouldn't make an encoder".into());
            }
            let mut p = SEncParamExt::default();
            call!(enc, GetDefaultParams, &mut p);
            let bps = kbps.saturating_mul(1000).min(i32::MAX as u32) as c_int;
            p.iUsageType = CAMERA_VIDEO_REAL_TIME;
            p.iPicWidth = w as c_int;
            p.iPicHeight = h as c_int;
            p.iTargetBitrate = bps;
            p.iMaxBitrate = bps.saturating_mul(2);
            p.iRCMode = RC_BITRATE_MODE;
            p.fMaxFrameRate = fps as f32;
            p.iTemporalLayerNum = 1;
            p.iSpatialLayerNum = 1;
            p.iComplexityMode = LOW_COMPLEXITY;
            p.uiIntraPeriod = 0;
            p.iNumRefFrame = 1;
            p.eSpsPpsIdStrategy = CONSTANT_ID;
            p.bPrefixNalAddingCtrl = false;
            p.bEnableSSEI = false;
            p.bEnableFrameSkip = false;
            p.bEnableSceneChangeDetect = false;
            p.bEnableBackgroundDetection = true;
            p.bEnableAdaptiveQuant = false;
            p.bEnableDenoise = false;
            p.bEnableLongTermReference = false;
            p.bEnableFrameCroppingFlag = true;
            p.iLoopFilterDisableIdc = DEBLOCKING_IDC_0 as c_int;
            p.iMultipleThreadIdc = threads.max(1) as u16;
            p.bUseLoadBalancing = false;
            p.iEntropyCodingModeFlag = 0;
            p.iPaddingFlag = 0;
            let l = &mut p.sSpatialLayers[0];
            l.iVideoWidth = w as c_int;
            l.iVideoHeight = h as c_int;
            l.fFrameRate = fps as f32;
            l.iSpatialBitrate = bps;
            l.iMaxSpatialBitrate = bps.saturating_mul(2);
            if threads > 1 {
                l.sSliceArgument.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
                l.sSliceArgument.uiSliceNum = threads as u32;
            } else {
                l.sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
                l.sSliceArgument.uiSliceNum = 1;
            }
            // BT.709, limited range: what capture converts to
            l.bVideoSignalTypePresent = true;
            l.uiVideoFormat = 5;
            l.bFullRange = false;
            l.bColorDescriptionPresent = true;
            l.uiColorPrimaries = 1;
            l.uiTransferCharacteristics = 1;
            l.uiColorMatrix = 1;
            let r = call!(enc, InitializeExt, &p);
            if r != 0 {
                APILoader::WelsDestroySVCEncoder(enc);
                return Err(format!("OpenH264 refused {w}x{h} at {fps} fps, {kbps} kbps ({r})"));
            }
            let mut quiet: c_int = WELS_LOG_QUIET as c_int;
            call!(enc, SetOption, ENCODER_OPTION_TRACE_LEVEL, &mut quiet as *mut c_int as *mut c_void);
            let mut fmt: c_int = videoFormatI420 as c_int;
            call!(enc, SetOption, ENCODER_OPTION_DATAFORMAT, &mut fmt as *mut c_int as *mut c_void);
        }
        Ok(H264 { enc, info: Box::default(), w, h })
    }

    /// one I420 picture (planes and their strides) encoded; ms its time. Its NAL units
    /// (start codes gone) go into nals, borrowed from the encoder until the next call;
    /// whether it's a keyframe back
    pub fn encode<'a>(
        &'a mut self,
        planes: [&[u8]; 3],
        strides: [usize; 3],
        ms: i64,
        idr: bool,
        nals: &mut Vec<&'a [u8]>,
    ) -> Result<bool, String> {
        let mut pic = SSourcePicture {
            iColorFormat: videoFormatI420 as c_int,
            iPicWidth: self.w as c_int,
            iPicHeight: self.h as c_int,
            uiTimeStamp: ms,
            ..Default::default()
        };
        for i in 0..3 {
            pic.iStride[i] = strides[i] as c_int;
            pic.pData[i] = planes[i].as_ptr() as *mut u8;
        }
        // SAFETY: the picture's planes outlive the call; the info is the encoder's to fill
        let ft = unsafe {
            if idr {
                call!(self.enc, ForceIntraFrame, true);
            }
            let r = call!(self.enc, EncodeFrame, &pic, &mut *self.info);
            if r != 0 {
                return Err(format!("the encoder failed ({r})"));
            }
            self.info.eFrameType
        };
        nals.clear();
        if ft == videoFrameTypeSkip {
            return Ok(false);
        }
        let info: &'a SFrameBSInfo = &self.info;
        for layer in &info.sLayerInfo[..info.iLayerNum.max(0) as usize] {
            let mut at = 0usize;
            for i in 0..layer.iNalCount.max(0) as usize {
                // SAFETY: OpenH264 lays out iNalCount NAL units back to back in pBsBuf
                let n = unsafe {
                    let len = *layer.pNalLengthInByte.add(i) as usize;
                    let s = std::slice::from_raw_parts(layer.pBsBuf.add(at), len);
                    at += len;
                    s
                };
                let body = if n.starts_with(&[0, 0, 0, 1]) {
                    &n[4..]
                } else if n.starts_with(&[0, 0, 1]) {
                    &n[3..]
                } else {
                    n
                };
                if !body.is_empty() {
                    nals.push(body);
                }
            }
        }
        Ok(ft == videoFrameTypeIDR || ft == videoFrameTypeI)
    }
}

impl Drop for H264 {
    fn drop(&mut self) {
        // SAFETY: made in new, not used after
        unsafe {
            call!(self.enc, Uninitialize);
            APILoader::WelsDestroySVCEncoder(self.enc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyframe_then_a_small_change_its_sps_made_low_delay() {
        let (w, h) = (320usize, 192usize);
        let mut yuv = vec![128u8; w * h * 3 / 2];
        yuv[..w * h].fill(200);
        let mut enc = H264::new(w, h, 60, 2000, 2).unwrap();
        {
            let (y, uv) = yuv.split_at(w * h);
            let (u, v) = uv.split_at(w * h / 4);
            let mut nals = Vec::new();
            assert!(enc.encode([y, u, v], [w, w / 2, w / 2], 0, true, &mut nals).unwrap());
            let types: Vec<u8> = nals.iter().map(|n| crate::h264::nal_type(n)).collect();
            assert!(types.contains(&7) && types.contains(&8) && types.contains(&5), "{types:?}");
            let sps = nals.iter().find(|n| crate::h264::nal_type(n) == 7).unwrap();
            // what the shaper sends says nothing is reordered, OpenH264's own or rewritten
            let low = crate::h264::low_delay_sps(sps).unwrap_or_else(|| sps.to_vec());
            assert_eq!(crate::h264::reorder_frames(&low), Some(0));
        }
        yuv[w * 10 + 10] = 20;
        let (y, uv) = yuv.split_at(w * h);
        let (u, v) = uv.split_at(w * h / 4);
        let mut nals = Vec::new();
        assert!(!enc.encode([y, u, v], [w, w / 2, w / 2], 16, false, &mut nals).unwrap());
        assert!(!nals.is_empty());
    }
}
