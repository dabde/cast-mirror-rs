//! Opus encoding of captured system audio into 10 ms packets on the shared media timeline.

use std::time::{Duration, Instant};

use anyhow::Result;
use opus::{Application, Bitrate, Channels};

use crate::capture::{AUDIO_CHANNELS, AUDIO_RATE};

pub const FRAME: Duration = Duration::from_millis(10);
const FRAME_SAMPLES: usize = (AUDIO_RATE as usize / 100) * AUDIO_CHANNELS;
// Jump the sample clock forward after a pause this long; bursty delivery stays continuous.
const MAX_GAP: Duration = Duration::from_millis(200);
// Ramps instead of hard edges: starting or stopping mid-waveform crackles on the receiver.
const FADE_IN: usize = AUDIO_RATE as usize / 20; // 50 ms, in sample frames
const FADE_OUT: usize = AUDIO_RATE as usize * 6 / 100; // 60 ms

pub struct Packet {
    pub data: Vec<u8>,
    pub media_timestamp: Duration,
}

pub struct OpusFramer {
    encoder: opus::Encoder,
    origin: Instant,
    pending: Vec<f32>,
    // Media time of the first sample in `pending`.
    pending_start: Option<Duration>,
    // Sample frames since the timeline was (re)anchored, for the fade-in.
    since_anchor: usize,
    // Sample frames into the fade-out, once it started.
    fading: Option<usize>,
    // Audio was dropped before the next chunk.
    realign: bool,
    // CAST_MIRROR_AUDIO_DUMP=file: raw f32 stereo of exactly what gets encoded, to look for clicks.
    dump: Option<std::fs::File>,
}

impl OpusFramer {
    pub fn new(origin: Instant, bitrate: u32) -> Result<Self> {
        let mut encoder = opus::Encoder::new(AUDIO_RATE, Channels::Stereo, Application::LowDelay)?;
        encoder.set_bitrate(Bitrate::Bits(bitrate as i32))?;
        Ok(Self {
            encoder,
            origin,
            pending: Vec::with_capacity(FRAME_SAMPLES * 4),
            pending_start: None,
            since_anchor: 0,
            fading: None,
            realign: false,
            dump: std::env::var_os("CAST_MIRROR_AUDIO_DUMP").and_then(|p| std::fs::File::create(p).ok()),
        })
    }

    /// Audio was dropped: if the timeline now lags the capture clock, the next chunk realigns it
    /// (with a fade-in). Otherwise it falls behind and the receiver skips everything as too late.
    pub fn gap(&mut self) {
        self.realign = true;
    }

    /// Ramps everything pushed from now on down to silence over 60 ms.
    pub fn fade_out(&mut self) {
        self.fading.get_or_insert(0);
    }

    fn apply_gain(&mut self, samples: &[f32]) -> Vec<f32> {
        samples
            .as_chunks::<AUDIO_CHANNELS>().0.iter()
            .flat_map(|frame| {
                let mut gain = (self.since_anchor as f32 / FADE_IN as f32).min(1.0);
                if let Some(n) = self.fading.as_mut() {
                    gain *= 1.0 - (*n as f32 / FADE_OUT as f32).min(1.0);
                    *n += 1;
                }
                self.since_anchor += 1;
                frame.iter().map(move |s| s * gain)
            })
            .collect()
    }

    /// `captured_at` is when the chunk's last sample was captured.
    pub fn push(&mut self, samples: &[f32], captured_at: Instant) -> Result<Vec<Packet>> {
        let chunk_len = Duration::from_secs_f64((samples.len() / AUDIO_CHANNELS) as f64 / AUDIO_RATE as f64);
        let wall_start = captured_at.saturating_duration_since(self.origin).saturating_sub(chunk_len);
        let queued = Duration::from_secs_f64((self.pending.len() / AUDIO_CHANNELS) as f64 / AUDIO_RATE as f64);
        let expected_start = self.pending_start.map(|s| s + queued);
        let paused = expected_start.is_some_and(|e| wall_start > e + MAX_GAP);
        // After dropped audio, but only if the timeline really lags: each realign restarts the fade-in.
        let lagging = std::mem::take(&mut self.realign) && expected_start.is_some_and(|e| wall_start > e + FRAME);
        if expected_start.is_none() || paused || lagging {
            // Every trigger moves forward: a receiver drops audio whose timestamps go back (crackling).
            self.pending.clear();
            self.pending_start = Some(wall_start);
            self.since_anchor = 0;
        }
        let samples = self.apply_gain(samples);
        if let Some(dump) = self.dump.as_mut() {
            use std::io::Write;
            let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            let _ = dump.write_all(&bytes);
        }
        self.pending.extend_from_slice(&samples);

        let mut packets = Vec::new();
        while self.pending.len() >= FRAME_SAMPLES {
            let data = self.encoder.encode_vec_float(&self.pending[..FRAME_SAMPLES], 1500)?;
            let start = self.pending_start.unwrap();
            packets.push(Packet { data, media_timestamp: start });
            self.pending.drain(..FRAME_SAMPLES);
            self.pending_start = Some(start + FRAME);
        }
        Ok(packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_10ms_apart_and_carry_over_remainder() {
        let origin = Instant::now();
        let mut framer = OpusFramer::new(origin, 128_000).unwrap();
        // 15 ms of audio ending 15 ms after origin -> one packet at t=0, 5 ms left over.
        let first = framer.push(&vec![0.0; 720 * 2], origin + Duration::from_millis(15)).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].media_timestamp, Duration::ZERO);
        let second = framer.push(&vec![0.0; 720 * 2], origin + Duration::from_millis(30)).unwrap();
        assert_eq!(second.iter().map(|p| p.media_timestamp).collect::<Vec<_>>(), [FRAME, FRAME * 2]);
    }

    #[test]
    fn large_gap_reanchors_timeline() {
        let origin = Instant::now();
        let mut framer = OpusFramer::new(origin, 128_000).unwrap();
        framer.push(&vec![0.0; 960], origin + Duration::from_millis(10)).unwrap();
        let later = framer.push(&vec![0.0; 960], origin + Duration::from_secs(2)).unwrap();
        assert_eq!(later[0].media_timestamp, Duration::from_millis(1990));
    }

    #[test]
    fn fades_in_at_start_and_out_on_request() {
        let origin = Instant::now();
        let mut framer = OpusFramer::new(origin, 128_000).unwrap();
        framer.push(&[1.0; 2], origin).unwrap();
        assert_eq!(framer.pending[..2], [0.0, 0.0], "first sample of the fade-in is silent");
        let full = framer.apply_gain(&vec![1.0; FADE_IN * 2]);
        assert_eq!(full[FADE_IN * 2 - 1], 1.0, "fade-in done after 50 ms");
        framer.fade_out();
        let out = framer.apply_gain(&vec![1.0; (FADE_OUT + 10) * 2]);
        // Interleaved stereo: sample frame i sits at index 2 * i.
        assert!(out[0] == 1.0 && out[2 * (FADE_OUT - 1)] < 0.01 && out[2 * (FADE_OUT + 5)] == 0.0, "60 ms ramp, then silence");
    }

    #[test]
    fn gap_realigns_forward_only() {
        let origin = Instant::now();
        let mut framer = OpusFramer::new(origin, 128_000).unwrap();
        framer.push(&vec![0.0; 960], origin + Duration::from_millis(10)).unwrap();
        // 120 ms went missing: below MAX_GAP, so only the reported gap realigns.
        framer.gap();
        let after = framer.push(&vec![0.0; 960], origin + Duration::from_millis(140)).unwrap();
        assert_eq!(after[0].media_timestamp, Duration::from_millis(130));
        // A gap while the capture clock reads behind the timeline must not move it back.
        framer.gap();
        let next = framer.push(&vec![0.0; 960], origin + Duration::from_millis(100)).unwrap();
        assert_eq!(next[0].media_timestamp, Duration::from_millis(140));
    }

    #[test]
    fn bursty_delivery_stays_continuous() {
        let origin = Instant::now();
        let mut framer = OpusFramer::new(origin, 128_000).unwrap();
        // Three 20 ms chunks that arrive almost at once, then one a bit late: timestamps must just count on.
        let mut stamps = Vec::new();
        for ms in [100, 101, 102, 190] {
            for p in framer.push(&vec![0.0; 1920], origin + Duration::from_millis(ms)).unwrap() {
                stamps.push(p.media_timestamp.as_millis());
            }
        }
        assert_eq!(stamps, [80, 90, 100, 110, 120, 130, 140, 150]);
    }
}
