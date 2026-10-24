//! Screen and system-audio capture via ScreenCaptureKit.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVImageBuffer, CVPixelBuffer};
use screencapturekit::cm::{CMSampleBuffer, CMSampleBufferExt, CMSampleBufferSCExt, CMTime, SCFrameStatus};
use screencapturekit::prelude::*;
use tokio::sync::{mpsc, Notify};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[derive(Clone)]
pub struct Frame {
    image: CFRetained<CVPixelBuffer>,
    pub captured_at: Instant,
}

// CVPixelBuffers are thread-safe reference-counted objects.
unsafe impl Send for Frame {}

impl Frame {
    /// Takes ownership of the sample's image buffer.
    fn from_sample(sample: &CMSampleBuffer) -> Option<Self> {
        // The bridge returns the image buffer +1 retained; adopt it, or every call pins a
        // capture surface and SCK stops after queueDepth frames.
        let raw = NonNull::new(sample.image_buffer_ptr() as *mut CVPixelBuffer)?;
        Some(Self { image: unsafe { CFRetained::from_raw(raw) }, captured_at: Instant::now() })
    }

    pub fn image(&self) -> &CVImageBuffer {
        &self.image
    }
}

/// Only the newest frame is kept: a slow consumer skips frames instead of queueing them.
#[derive(Default)]
pub struct LatestFrame {
    slot: Mutex<Option<Frame>>,
    pub ready: Notify,
}

impl LatestFrame {
    pub fn take(&self) -> Option<Frame> {
        self.slot.lock().unwrap().take()
    }
}

/// 10 ms of interleaved stereo f32 at 48 kHz per message is typical, but sizes vary.
pub struct AudioChunk {
    pub samples: Vec<f32>,
    pub captured_at: Instant,
}

pub struct Capture {
    stream: SCStream,
    pub width: u32,
    pub height: u32,
}

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: usize = 2;

pub fn ensure_permission() -> Result<()> {
    if unsafe { CGPreflightScreenCaptureAccess() } {
        return Ok(());
    }
    unsafe { CGRequestScreenCaptureAccess() };
    bail!(
        "Screen Recording permission missing. Allow this terminal under System Settings → \
         Privacy & Security → Screen & System Audio Recording, then restart it."
    )
}

impl Capture {
    /// Captures `display` scaled to `width` (height follows the display aspect, both even).
    pub fn start(
        display: usize,
        width: u32,
        fps: u32,
        latest: Arc<LatestFrame>,
        audio: Option<mpsc::Sender<AudioChunk>>,
    ) -> Result<Self> {
        ensure_permission()?;
        let content = SCShareableContent::get().context("listing displays")?;
        let displays = content.displays();
        let target = displays
            .get(display)
            .with_context(|| format!("display {display} not found ({} available)", displays.len()))?;
        let (width, height) = fit_16_9(width.min(target.width() * 2), target.width(), target.height());

        let filter = SCContentFilter::create().with_display(target).with_excluding_windows(&[]).build();
        let mut config = SCStreamConfiguration::default();
        config
            .set_width(width)
            .set_height(height)
            // NV12 so VideoToolbox takes the IOSurface without conversion.
            .set_pixel_format(PixelFormat::YCbCr_420v)
            .set_minimum_frame_interval(&CMTime::new(1, fps as i32))
            .set_shows_cursor(true)
            .set_queue_depth(3);
        if audio.is_some() {
            config
                .set_captures_audio(true)
                .set_sample_rate(AUDIO_RATE as i32)
                .set_channel_count(AUDIO_CHANNELS as i32)
                .set_excludes_current_process_audio(true);
        }

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            move |sample: CMSampleBuffer, _| {
                // Idle frames carry no image. frame_status() reads None on macOS 27, so only reject explicit non-Complete.
                if sample.frame_status().is_some_and(|s| s != SCFrameStatus::Complete) {
                    return;
                }
                let Some(frame) = Frame::from_sample(&sample) else { return };
                *latest.slot.lock().unwrap() = Some(frame);
                latest.ready.notify_one();
            },
            SCStreamOutputType::Screen,
        );
        if let Some(tx) = audio {
            stream.add_output_handler(
                move |sample: CMSampleBuffer, _| {
                    if let Some(samples) = interleave(&sample) {
                        // Dropping audio beats building latency when the sender stalls.
                        if tx.try_send(AudioChunk { samples, captured_at: Instant::now() }).is_err() {
                            tracing::debug!("audio chunk dropped: sender queue full");
                        }
                    }
                },
                SCStreamOutputType::Audio,
            );
        }
        stream.start_capture().context("starting screen capture")?;
        Ok(Self { stream, width, height })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}

/// Scales the display into a `max_width` x 16:9 box with even sides. Receivers that send no
/// constraints (Chromecast) still show a black screen above 1080 lines, e.g. 1920x1240 from a 16:10 Mac.
fn fit_16_9(max_width: u32, display_w: u32, display_h: u32) -> (u32, u32) {
    let max_height = max_width * 9 / 16;
    let scale = (max_width as f64 / display_w as f64).min(max_height as f64 / display_h as f64);
    ((display_w as f64 * scale) as u32 & !1, (display_h as f64 * scale) as u32 & !1)
}

/// SCK delivers float32, usually one buffer per channel; returns interleaved stereo.
fn interleave(sample: &CMSampleBuffer) -> Option<Vec<f32>> {
    let list = sample.audio_buffer_list()?;
    let planes: Vec<Vec<f32>> = list
        .iter()
        .map(|b| b.data().as_chunks::<4>().0.iter().map(|c| f32::from_ne_bytes(*c)).collect())
        .collect();
    match planes.as_slice() {
        [left, right, ..] => Some(left.iter().zip(right).flat_map(|(l, r)| [*l, *r]).collect()),
        [both] if list.get(0)?.number_channels == 2 => Some(both.clone()),
        [mono] => Some(mono.iter().flat_map(|s| [*s, *s]).collect()),
        [] => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_into_16_9_box() {
        assert_eq!(fit_16_9(1920, 1512, 982), (1662, 1080), "16:10-ish MacBook");
        assert_eq!(fit_16_9(1920, 1920, 1080), (1920, 1080));
        assert_eq!(fit_16_9(1280, 3440, 1440), (1280, 534), "ultrawide is width-bound");
    }
    use crate::video::Encoder;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs Screen Recording permission and a display"]
    async fn captures_encodes_and_delivers_audio() {
        let latest = Arc::new(LatestFrame::default());
        let (tx, mut rx) = mpsc::channel(64);
        let cap = Capture::start(0, 1280, 30, latest.clone(), Some(tx)).unwrap();
        let enc = Encoder::new(crate::video::VtCodec::H264, cap.width, cap.height, 30, 4_000_000).unwrap();
        tokio::time::timeout(Duration::from_secs(3), latest.ready.notified()).await.expect("no video frame");
        let frame = latest.take().unwrap();
        let out = enc.encode(frame.image(), Duration::ZERO, true).unwrap().unwrap();
        assert!(out.key_frame && out.data.len() > 100);
        let chunk = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.expect("no audio").unwrap();
        assert!(!chunk.samples.is_empty() && chunk.samples.len() % 2 == 0);
        eprintln!("{}x{} key frame {} bytes, audio chunk {} samples", cap.width, cap.height, out.data.len(), chunk.samples.len());
    }
}

#[cfg(test)]
mod leak_probe {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs Screen Recording permission and a changing screen"]
    async fn frames_keep_coming_when_released() {
        let latest = Arc::new(LatestFrame::default());
        let _cap = Capture::start(0, 1280, 30, latest.clone(), None).unwrap();
        let mut n = 0;
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if tokio::time::timeout(Duration::from_millis(200), latest.ready.notified()).await.is_ok() {
                drop(latest.take());
                n += 1;
            }
        }
        assert!(n > 20, "only {n} frames in 3 s: are sample buffers leaking?");
    }
}
