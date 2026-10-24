//! H.264/HEVC hardware encoding via VideoToolbox, output as Annex B access units.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, CMFormatDescription, CMSampleBuffer,
    CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    CVImageBuffer, CVPixelBuffer, CVPixelBufferCreate,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VtCodec {
    H264,
    H264High,
    Hevc,
}

pub struct Encoded {
    /// Annex B; key frames start with the parameter sets (VPS/SPS/PPS).
    pub data: Vec<u8>,
    pub key_frame: bool,
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    // Boxed so the callback's refcon pointer stays valid.
    output: Box<Output>,
}

struct Output {
    hevc: bool,
    slot: Mutex<Option<Encoded>>,
}

// VTCompressionSession is thread-safe; the output slot is behind a mutex.
unsafe impl Send for Encoder {}

unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let output = unsafe { &*(refcon as *const Output) };
    // Null sample with status 0 means the encoder dropped the frame.
    if status != 0 || sample.is_null() {
        return;
    }
    if let Some(encoded) = unsafe { to_annex_b(&*sample, output.hevc) } {
        *output.slot.lock().unwrap() = Some(encoded);
    }
}

impl Encoder {
    pub fn new(codec: VtCodec, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let hevc = codec == VtCodec::Hevc;
        let output = Box::new(Output { hevc, slot: Mutex::new(None) });
        unsafe {
            let spec = dict(&[
                (kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, CFBoolean::new(true)),
                (kVTVideoEncoderSpecification_EnableLowLatencyRateControl, CFBoolean::new(true)),
            ]);
            let mut raw: *mut VTCompressionSession = ptr::null_mut();
            let status = VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                if hevc { kCMVideoCodecType_HEVC } else { kCMVideoCodecType_H264 },
                Some(spec.as_opaque()),
                None,
                None,
                Some(on_output),
                &*output as *const _ as *mut c_void,
                NonNull::from(&mut raw),
            );
            if status != 0 || raw.is_null() {
                bail!("VTCompressionSessionCreate failed: {status}");
            }
            let session = CFRetained::from_raw(NonNull::new_unchecked(raw));
            let encoder = Self { session, output };

            let profile = match codec {
                VtCodec::H264 => kVTProfileLevel_H264_Main_AutoLevel,
                VtCodec::H264High => kVTProfileLevel_H264_High_AutoLevel,
                VtCodec::Hevc => kVTProfileLevel_HEVC_Main_AutoLevel,
            };
            encoder.set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(true))?;
            encoder.set(kVTCompressionPropertyKey_ProfileLevel, profile)?;
            encoder.set(kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false))?;
            encoder.set(kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_i32(fps as i32))?;
            if !hevc {
                encoder.set(kVTCompressionPropertyKey_H264EntropyMode, kVTH264EntropyMode_CABAC)?;
            }
            // Key frames only on demand (session start, PLI); 10 s cap as a safety net.
            encoder.set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &CFNumber::new_i32(10))?;
            encoder.set_bitrate(bitrate)?;
            let status = encoder.session.prepare_to_encode_frames();
            if status != 0 {
                bail!("VTCompressionSessionPrepareToEncodeFrames failed: {status}");
            }
            Ok(encoder)
        }
    }

    /// Updates the target rate live; DataRateLimits caps any 1 s window at 1.5x.
    pub fn set_bitrate(&self, bitrate: u32) -> Result<()> {
        let cap_bytes = CFNumber::new_i64(bitrate as i64 * 3 / 2 / 8);
        let one_second = CFNumber::new_f64(1.0);
        let limits = CFArray::from_objects(&[&*cap_bytes, &*one_second]);
        self.set(unsafe { kVTCompressionPropertyKey_AverageBitRate }, &CFNumber::new_i32(bitrate as i32))?;
        self.set(unsafe { kVTCompressionPropertyKey_DataRateLimits }, &limits)
    }

    /// Never quantize a frame coarser than `qp` (0-51; lower = sharper, e.g. text). Raises bitrate as needed.
    pub fn set_max_qp(&self, qp: u32) -> Result<()> {
        self.set(unsafe { kVTCompressionPropertyKey_MaxAllowedFrameQP }, &CFNumber::new_i32(qp as i32))
    }

    fn set(&self, key: &CFString, value: &CFType) -> Result<()> {
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if status != 0 {
            bail!("VTSessionSetProperty({key}) failed: {status}");
        }
        Ok(())
    }

    /// Encodes one frame synchronously; `None` if the encoder dropped it.
    pub fn encode(&self, image: &CVImageBuffer, pts: Duration, force_key_frame: bool) -> Result<Option<Encoded>> {
        unsafe {
            let props = force_key_frame
                .then(|| dict(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, CFBoolean::new(true))]));
            let status = self.session.encode_frame(
                image,
                CMTime::new(pts.as_micros() as i64, 1_000_000),
                kCMTimeInvalid,
                props.as_ref().map(|p| p.as_opaque()),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if status != 0 {
                bail!("VTCompressionSessionEncodeFrame failed: {status}");
            }
            // With RealTime and no reordering this returns as soon as the frame is out.
            self.session.complete_frames(kCMTimeInvalid);
        }
        Ok(self.output.slot.lock().unwrap().take())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

unsafe fn dict(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFType, CFType>> {
    let keys: Vec<&CFType> = pairs.iter().map(|(k, _)| k.as_ref()).collect();
    let values: Vec<&CFType> = pairs.iter().map(|(_, v)| *v).collect();
    CFDictionary::<CFType, CFType>::from_slices(&keys, &values)
}

unsafe fn to_annex_b(sample: &CMSampleBuffer, hevc: bool) -> Option<Encoded> {
    let block = unsafe { sample.data_buffer()? };
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    if unsafe { block.copy_data_bytes(0, len, NonNull::new(avcc.as_mut_ptr().cast())?) } != 0 {
        return None;
    }
    let format = unsafe { sample.format_description()? };
    let (params, nal_len_size) = unsafe { parameter_sets(&format, hevc) };
    Some(avcc_to_annex_b(&avcc, nal_len_size, &params, hevc))
}

unsafe fn parameter_sets(format: &CMFormatDescription, hevc: bool) -> (Vec<Vec<u8>>, usize) {
    let get = if hevc {
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex
    } else {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex
    };
    let mut count = 0usize;
    let mut nal_len_size = 4i32;
    let mut sets = Vec::new();
    let mut index = 0;
    loop {
        let mut ptr: *const u8 = ptr::null();
        let mut size = 0usize;
        let status = unsafe { get(format, index, &mut ptr, &mut size, &mut count, &mut nal_len_size) };
        if status != 0 || ptr.is_null() {
            break;
        }
        sets.push(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec());
        index += 1;
        if index >= count {
            break;
        }
    }
    (sets, nal_len_size as usize)
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const H264_IDR: u8 = 5;
// HEVC IRAP picture types: BLA, IDR, CRA.
const HEVC_IRAP: std::ops::RangeInclusive<u8> = 16..=21;

/// Converts length-prefixed NAL units to start codes; prepends `params` on random-access frames.
pub fn avcc_to_annex_b(avcc: &[u8], nal_len_size: usize, params: &[Vec<u8>], hevc: bool) -> Encoded {
    let mut nals = Vec::new();
    let mut rest = avcc;
    while rest.len() > nal_len_size {
        let len = rest[..nal_len_size].iter().fold(0usize, |n, &b| n << 8 | b as usize);
        rest = &rest[nal_len_size..];
        if len == 0 || len > rest.len() {
            break;
        }
        nals.push(&rest[..len]);
        rest = &rest[len..];
    }
    let key_frame = nals.iter().any(|nal| {
        if hevc { HEVC_IRAP.contains(&(nal[0] >> 1 & 0x3f)) } else { nal[0] & 0x1f == H264_IDR }
    });
    let mut data = Vec::with_capacity(avcc.len() + 64);
    let leading = if key_frame { params } else { &[] };
    for nal in leading.iter().map(Vec::as_slice).chain(nals) {
        data.extend_from_slice(&START_CODE);
        data.extend_from_slice(nal);
    }
    Encoded { data, key_frame }
}

/// A solid-colour NV12 frame for the test pattern; colour is BT.601 video-range (Y, Cb, Cr).
pub fn solid_frame(width: u32, height: u32, (y, cb, cr): (u8, u8, u8)) -> Result<CFRetained<CVPixelBuffer>> {
    unsafe {
        let iosurface = dict(&[]);
        let attrs = dict(&[(kCVPixelBufferIOSurfacePropertiesKey, &iosurface)]);
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        let status = CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            Some(attrs.as_opaque()),
            NonNull::from(&mut raw),
        );
        if status != 0 || raw.is_null() {
            bail!("CVPixelBufferCreate failed: {status}");
        }
        let buffer = CFRetained::from_raw(NonNull::new_unchecked(raw));
        CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        for (plane, fill) in [(0, vec![y]), (1, vec![cb, cr])] {
            let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane) as *mut u8;
            let bytes = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane) * CVPixelBufferGetHeightOfPlane(&buffer, plane);
            let plane = std::slice::from_raw_parts_mut(base, bytes);
            for (i, b) in plane.iter_mut().enumerate() {
                *b = fill[i % fill.len()];
            }
        }
        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avcc(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&(n.len() as u32).to_be_bytes()[..], n].concat()).collect()
    }

    #[test]
    fn idr_gets_parameter_sets_and_start_codes() {
        let sps = vec![0x67, 1, 2];
        let pps = vec![0x68, 3];
        let out = avcc_to_annex_b(&avcc(&[&[0x65, 9, 9]]), 4, &[sps, pps], false);
        assert!(out.key_frame);
        assert_eq!(out.data, [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9]);
    }

    #[test]
    fn delta_frame_has_no_parameter_sets() {
        let out = avcc_to_annex_b(&avcc(&[&[0x41, 7], &[0x41, 8]]), 4, &[vec![0x67]], false);
        assert!(!out.key_frame);
        assert_eq!(out.data, [0, 0, 0, 1, 0x41, 7, 0, 0, 0, 1, 0x41, 8]);
    }

    #[test]
    fn truncated_input_stops_cleanly() {
        let out = avcc_to_annex_b(&[0, 0, 0, 9, 0x41, 1], 4, &[], false);
        assert!(out.data.is_empty());
    }

    #[test]
    fn hevc_idr_detected_from_nal_type() {
        // 0x26 = IDR_W_RADL (19) << 1; 0x02 = TRAIL_R (1) << 1.
        let vps = vec![0x40, 1];
        assert!(avcc_to_annex_b(&avcc(&[&[0x26, 1, 5]]), 4, &[vps.clone()], true).key_frame);
        let delta = avcc_to_annex_b(&avcc(&[&[0x02, 1, 5]]), 4, &[vps], true);
        assert!(!delta.key_frame && delta.data == [0, 0, 0, 1, 0x02, 1, 5]);
    }

    #[test]
    #[ignore = "needs VideoToolbox hardware"]
    fn hardware_hevc_starts_with_vps() {
        let enc = Encoder::new(VtCodec::Hevc, 640, 360, 30, 2_000_000).unwrap();
        let frame = solid_frame(640, 360, (81, 90, 240)).unwrap();
        let first = enc.encode(&frame, Duration::ZERO, true).unwrap().unwrap();
        assert!(first.key_frame && first.data[..4] == START_CODE && first.data[4] >> 1 & 0x3f == 32, "starts with VPS");
    }

    #[test]
    #[ignore = "needs VideoToolbox hardware"]
    fn hardware_encoder_emits_idr_then_delta() {
        let enc = Encoder::new(VtCodec::H264, 640, 360, 30, 2_000_000).unwrap();
        let frame = solid_frame(640, 360, (81, 90, 240)).unwrap();
        let first = enc.encode(&frame, Duration::ZERO, true).unwrap().unwrap();
        assert!(first.key_frame && first.data[..4] == START_CODE && first.data[4] & 0x1f == 7, "starts with SPS");
        let second = enc.encode(&frame, Duration::from_millis(33), false).unwrap().unwrap();
        assert!(!second.key_frame);
    }
}
