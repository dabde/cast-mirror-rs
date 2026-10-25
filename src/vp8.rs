//! Software VP8 fallback (libvpx) for receivers that do not take H.264.

use std::mem::MaybeUninit;
use std::ptr;
use std::time::Duration;

use anyhow::{bail, Result};
use vpx_sys as vpx;
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};

use crate::video::Encoded;

pub struct Vp8Encoder {
    ctx: vpx::vpx_codec_ctx_t,
    cfg: vpx::vpx_codec_enc_cfg_t,
    i420: Vec<u8>,
    width: usize,
    height: usize,
    frame_us: u64,
}

// The libvpx context is only touched through &mut self.
unsafe impl Send for Vp8Encoder {}

impl Vp8Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            let iface = vpx::vpx_codec_vp8_cx();
            let mut cfg = MaybeUninit::<vpx::vpx_codec_enc_cfg_t>::zeroed().assume_init();
            if vpx::vpx_codec_enc_config_default(iface, &mut cfg, 0) != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                bail!("vpx_codec_enc_config_default failed");
            }
            cfg.g_w = width;
            cfg.g_h = height;
            cfg.g_timebase = vpx::vpx_rational { num: 1, den: 1_000_000 };
            cfg.g_threads = 4;
            cfg.g_lag_in_frames = 0;
            cfg.g_error_resilient = vpx::VPX_ERROR_RESILIENT_DEFAULT;
            cfg.rc_end_usage = vpx::vpx_rc_mode::VPX_CBR;
            cfg.rc_target_bitrate = bitrate / 1000;
            // Key frames on demand only (start, PLI).
            cfg.kf_max_dist = 10_000;
            let mut ctx = MaybeUninit::<vpx::vpx_codec_ctx_t>::zeroed().assume_init();
            let err = vpx::vpx_codec_enc_init_ver(&mut ctx, iface, &cfg, 0, vpx::VPX_ENCODER_ABI_VERSION as i32);
            if err != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                bail!("vpx_codec_enc_init failed: {err:?}");
            }
            // Speed over quality for real time: libvpx's 6..16 range for VP8.
            vpx::vpx_codec_control_(&mut ctx, vpx::vp8e_enc_control_id::VP8E_SET_CPUUSED as i32, 12i32);
            let (w, h) = (width as usize, height as usize);
            Ok(Self { ctx, cfg, i420: vec![0; w * h * 3 / 2], width: w, height: h, frame_us: 1_000_000 / u64::from(fps.max(1)) })
        }
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        self.cfg.rc_target_bitrate = bitrate / 1000;
        if unsafe { vpx::vpx_codec_enc_config_set(&mut self.ctx, &self.cfg) } != vpx::vpx_codec_err_t::VPX_CODEC_OK {
            bail!("vpx_codec_enc_config_set failed");
        }
        Ok(())
    }

    /// `image` must be NV12 (what ScreenCaptureKit and the test pattern produce).
    pub fn encode(&mut self, image: &CVImageBuffer, pts: Duration, force_key_frame: bool) -> Result<Option<Encoded>> {
        unsafe {
            CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly);
            let plane = |i| {
                let stride = CVPixelBufferGetBytesPerRowOfPlane(image, i);
                (CVPixelBufferGetBaseAddressOfPlane(image, i) as *const u8, stride)
            };
            nv12_to_i420(plane(0), plane(1), self.width, self.height, &mut self.i420);
            CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly);

            let mut img = MaybeUninit::<vpx::vpx_image_t>::zeroed().assume_init();
            vpx::vpx_img_wrap(
                &mut img,
                vpx::vpx_img_fmt::VPX_IMG_FMT_I420,
                self.width as u32,
                self.height as u32,
                1,
                self.i420.as_mut_ptr(),
            );
            let flags = if force_key_frame { vpx::VPX_EFLAG_FORCE_KF as i64 } else { 0 };
            let err = vpx::vpx_codec_encode(
                &mut self.ctx,
                &img,
                pts.as_micros() as i64,
                self.frame_us,
                flags,
                vpx::VPX_DL_REALTIME as _,
            );
            if err != vpx::vpx_codec_err_t::VPX_CODEC_OK {
                bail!("vpx_codec_encode failed: {err:?}");
            }
            let mut iter = ptr::null();
            let mut out = None;
            loop {
                let pkt = vpx::vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind == vpx::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                    let frame = (*pkt).data.frame;
                    out = Some(Encoded {
                        data: std::slice::from_raw_parts(frame.buf as *const u8, frame.sz).to_vec(),
                        key_frame: frame.flags & vpx::VPX_FRAME_IS_KEY != 0,
                    });
                }
            }
            Ok(out)
        }
    }
}

impl Drop for Vp8Encoder {
    fn drop(&mut self) {
        unsafe { vpx::vpx_codec_destroy(&mut self.ctx) };
    }
}

/// Copies NV12 planes (with row padding) into a packed I420 buffer.
unsafe fn nv12_to_i420(
    (y, y_stride): (*const u8, usize),
    (uv, uv_stride): (*const u8, usize),
    width: usize,
    height: usize,
    out: &mut [u8],
) {
    let (luma, chroma) = out.split_at_mut(width * height);
    let (u_out, v_out) = chroma.split_at_mut(width * height / 4);
    for row in 0..height {
        let src = unsafe { std::slice::from_raw_parts(y.add(row * y_stride), width) };
        luma[row * width..][..width].copy_from_slice(src);
    }
    let (cw, ch) = (width / 2, height / 2);
    for row in 0..ch {
        let src = unsafe { std::slice::from_raw_parts(uv.add(row * uv_stride), cw * 2) };
        for (i, pair) in src.as_chunks::<2>().0.iter().enumerate() {
            u_out[row * cw + i] = pair[0];
            v_out[row * cw + i] = pair[1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::solid_frame;

    #[test]
    fn deinterleaves_padded_nv12() {
        // 4x2 frame, strides padded to 6: Y rows then one interleaved UV row.
        let y = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        let uv = [10, 20, 11, 21, 0, 0];
        let mut out = [0u8; 12];
        unsafe { nv12_to_i420((y.as_ptr(), 6), (uv.as_ptr(), 6), 4, 2, &mut out) };
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 20, 21]);
    }

    #[test]
    fn encodes_key_then_delta() {
        let mut enc = Vp8Encoder::new(320, 180, 30, 1_000_000).unwrap();
        let frame = solid_frame(320, 180, (81, 90, 240)).unwrap();
        let first = enc.encode(&frame, Duration::ZERO, true).unwrap().unwrap();
        // VP8 key frame: bit 0 of the first byte is 0, start code 9d 01 2a at offset 3.
        assert!(first.key_frame && first.data[0] & 1 == 0 && first.data[3..6] == [0x9d, 0x01, 0x2a]);
        let second = enc.encode(&frame, Duration::from_millis(33), false).unwrap().unwrap();
        assert!(!second.key_frame && second.data[0] & 1 == 1);
    }
}
