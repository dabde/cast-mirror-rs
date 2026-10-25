//! One mirroring session per connection; reconnects until stopped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use chromiacast::{
    AudioCodec, AudioStreamConfig, CastApp, CastConnection, ControlEvent, EncodedFrame, EnqueueError, FrameDependency, Framerate, Offer,
    Resolution, SenderEvent, SenderSession, StreamHandle, StreamStatistics, UdpTransport, VideoCodec,
    VideoStreamConfig,
};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::audio::OpusFramer;
use crate::capture::{self, Capture, LatestFrame, AUDIO_CHANNELS, AUDIO_RATE};
use crate::video::{solid_frame, Encoded, Encoder, VtCodec};
use crate::{connect_and_launch, resolve, Codec, MirrorArgs};

const AUDIO_BITRATE: u32 = 128_000;
const MIN_VIDEO_BITRATE: u32 = 500_000;
// The receiver must see something at least this often, even on a static screen.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);
// A reconnected session lost again this soon means another sender (e.g. another laptop) is fighting over the receiver.
const LOSS_WINDOW: Duration = Duration::from_secs(30);
// Longest we skip frames while the receiver looks behind.
const STALL: Duration = Duration::from_millis(250);
const MIN_AUDIO_DELAY: Duration = Duration::from_millis(100);
// Audio older than this is a backlog from before the session started; the receiver would skip it anyway.
const STALE_AUDIO: Duration = Duration::from_millis(80);

#[derive(Clone, Copy, PartialEq)]
pub enum Source {
    Screen,
    Pattern,
}

enum End {
    Stopped,
    Lost(anyhow::Error),
    // The receiver dropped our app session: another sender launched, or someone stopped it on the TV.
    TakenOver,
}

/// How a caller stops a session and hears what it is doing.
#[derive(Clone)]
pub struct Control {
    pub stop: watch::Receiver<bool>,
    pub status: Arc<dyn Fn(String) + Send + Sync>,
    /// Live measurements, once a second.
    pub stats: Arc<dyn Fn(String) + Send + Sync>,
}

impl Control {
    /// Stops on Ctrl-C; status goes to the log only.
    pub fn ctrl_c() -> Self {
        let (tx, stop) = watch::channel(false);
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(true);
        });
        Self { stop, status: Arc::new(|_| {}), stats: Arc::new(|_| {}) }
    }

    async fn stopped(&self) {
        let _ = self.stop.clone().wait_for(|s| *s).await;
    }
}

pub async fn run(args: MirrorArgs, source: Source, control: Control) -> Result<()> {
    if source == Source::Screen {
        capture::ensure_permission()?;
        if let Some(query) = &args.window {
            capture::check_window(query)?;
        }
    }
    // A second monitor instead of a mirror: 1920x1080 at 1x, crisp at TV size. It lives as long as this run.
    let mut args = args;
    let _extended = if args.extend {
        let display = crate::virtual_display::VirtualDisplay::create("Cast Mirror", 1920, 1080, false)?;
        args.display = capture::display_index(display.id)?;
        Some(display)
    } else {
        None
    };

    let addr = resolve(args.device.as_deref()).await?;
    #[cfg(feature = "test-receiver")]
    let unverified = args.unverified;
    #[cfg(not(feature = "test-receiver"))]
    let unverified = false;
    let mut reconnected = false;
    loop {
        (control.status)(format!("Connecting to {addr}"));
        let launched = tokio::select! {
            launched = connect_and_launch(addr, unverified) => launched,
            _ = control.stopped() => return Ok(()),
        };
        let e = match launched {
            // Receiver unreachable: keep retrying, that is not a loss.
            Err(e) => e,
            Ok((connection, app)) => {
                let began = Instant::now();
                let end = session(&args, source, addr, &control, &connection, &app).await.unwrap_or_else(End::Lost);
                let lasted = began.elapsed();
                // After a loss the receiver may already run another sender's session, and STOP would end that one.
                // Bounded: a half-dead receiver must not keep Stop or Quit waiting.
                if matches!(end, End::Stopped) {
                    let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, connection.stop(&app)).await;
                }
                let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, connection.close()).await;
                match end {
                    End::Stopped => return Ok(()),
                    End::TakenOver => bail!("receiver taken over by another sender, Start to take it back"),
                    // A reconnect that fails again quickly means fighting, not a glitch.
                    End::Lost(e) if reconnected && lasted < LOSS_WINDOW => {
                        warn!("session ended again: {e:#}; not reconnecting");
                        bail!("receiver lost twice in {}s ({e}); another sender casting? Start to take it back", LOSS_WINDOW.as_secs());
                    }
                    End::Lost(e) => {
                        reconnected = true;
                        e
                    }
                }
            }
        };
        warn!("{e:#}; reconnecting in {RECONNECT_DELAY:?}");
        (control.status)(format!("Reconnecting: {e}"));
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = control.stopped() => return Ok(()),
        }
    }
}

/// Resolves once the receiver no longer runs our app session.
async fn app_gone(mut events: tokio::sync::broadcast::Receiver<ControlEvent>, ours: String) {
    use tokio::sync::broadcast::error::RecvError;
    if ours.is_empty() {
        // Nothing to compare against: every status would look like a takeover.
        return std::future::pending().await;
    }
    loop {
        match events.recv().await {
            Ok(ControlEvent::ApplicationStopped(app)) if app.session_id() == ours => return,
            Ok(ControlEvent::ReceiverStatus(status)) if !status.applications().iter().any(|a| a.session_id() == ours) => {
                return
            }
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            // Connection gone: the media side notices that on its own.
            Err(RecvError::Closed) => std::future::pending().await,
        }
    }
}

async fn session(
    args: &MirrorArgs,
    source: Source,
    addr: std::net::SocketAddr,
    control: &Control,
    connection: &CastConnection,
    app: &CastApp,
) -> Result<End> {
    let origin = Instant::now();
    let latest = Arc::new(LatestFrame::default());
    let (audio_tx, audio_rx) = mpsc::channel(64);
    let audio_tx = (!args.no_audio).then_some(audio_tx);

    let mut fps = args.fps;
    let mut capture = match source {
        Source::Screen => {
            let bounds = capture::box_16_9(args.width);
            Some(Capture::start(args.display, args.window.as_deref(), bounds, fps, latest.clone(), audio_tx.clone())?)
        }
        Source::Pattern => None,
    };
    let (mut width, mut height) =
        capture.as_ref().map_or((args.width & !1, (args.width * 9 / 16) & !1), |c| (c.width, c.height));

    let mut taken_over = std::pin::pin!(app_gone(connection.subscribe(), app.session_id().to_owned()));
    let delay = Duration::from_millis(args.delay);
    let mut builder = Offer::builder().android_rtp_hack(!args.spec_payload_types);
    if audio_tx.is_some() {
        builder = builder.audio(AudioStreamConfig {
            codec: AudioCodec::Opus,
            bit_rate: AUDIO_BITRATE,
            sample_rate: AUDIO_RATE,
            channels: AUDIO_CHANNELS as u8,
            // ScreenCaptureKit hands audio over 10-50 ms after capture; below ~100 ms the receiver skips it.
            target_delay: delay.max(MIN_AUDIO_DELAY),
        });
    }
    // chromiacast indexes audio streams before video streams.
    let first_video_index = usize::from(audio_tx.is_some());
    for codec in &args.codecs {
        builder = builder.video(VideoStreamConfig {
            codec: match codec {
                Codec::H264 | Codec::H264High => VideoCodec::H264,
                Codec::Hevc => VideoCodec::Hevc,
                Codec::Vp8 => VideoCodec::Vp8,
            },
            max_bit_rate: args.bitrate,
            max_frame_rate: Framerate::new(fps, 1)?,
            resolutions: vec![Resolution::new(width, height)?],
            target_delay: delay,
        });
    }
    let offer = builder.build()?;
    let answer = connection.exchange_offer(&offer, app).await.context("OFFER/ANSWER")?;
    info!(?answer, "receiver ANSWER");
    let codec = answer
        .send_indexes
        .iter()
        .find_map(|&i| i.checked_sub(first_video_index).and_then(|v| args.codecs.get(v)))
        .copied()
        .ok_or_else(|| anyhow!("receiver accepted none of the offered video codecs {:?}", args.codecs))?;

    let mut max_bitrate = args.bitrate;
    if let Some(v) = answer.constraints.as_ref().and_then(|c| c.video.as_ref()) {
        max_bitrate = v.max_bit_rate.map_or(max_bitrate, |m| max_bitrate.min(m));
        if let Some(dims) = v.max_dimensions {
            if let Some(rate) = dims.frame_rate {
                fps = fps.min((rate.numerator() / rate.denominator().max(1)).max(1));
            }
            if width > dims.width || height > dims.height {
                let scale = (dims.width as f64 / width as f64).min(dims.height as f64 / height as f64);
                width = (width as f64 * scale) as u32 & !1;
                height = (height as f64 * scale) as u32 & !1;
                info!("receiver caps video at {}x{}; capturing {width}x{height}", dims.width, dims.height);
                if source == Source::Screen {
                    drop(capture.take());
                    let bounds = capture::box_16_9(width);
                    let c = Capture::start(args.display, args.window.as_deref(), bounds, fps, latest.clone(), audio_tx.clone())?;
                    // Capture fits its own 16:9 box; encode exactly what it delivers.
                    (width, height) = (c.width, c.height);
                    capture = Some(c);
                }
            }
        }
    }

    let transport = UdpTransport::bind("0.0.0.0:0".parse()?).await?;
    let (sender, events) = SenderSession::start_address(&offer, &answer, addr, transport).await?;
    let video = sender.video().ok_or_else(|| anyhow!("receiver rejected the video stream"))?;
    let need_key = Arc::new(AtomicBool::new(true));
    let mut lost = tokio::spawn(watch_events(events, need_key.clone()));
    let fade = Arc::new(AtomicBool::new(false));
    let audio_task = sender.audio().map(|audio| tokio::spawn(send_audio(audio, audio_rx, origin, fade.clone())));
    info!("mirroring {codec:?} {width}x{height}@{fps} to {addr}, start bitrate {max_bitrate}");

    let mut encoder = VideoEncoder::new(codec, width, height, fps, max_bitrate)?;
    encoder.set_max_qp(args.max_qp);
    let mut pipeline = Pipeline {
        encoder,
        video,
        origin,
        need_key,
        rate: RateControl::new(max_bitrate),
        stats: Stats::default(),
        last_pts: None,
        last_sent: Instant::now(),
        behind_since: None,
    };
    let pattern = match source {
        Source::Pattern => [(81, 90, 240), (145, 54, 34), (41, 240, 110), (235, 128, 128)]
            .map(|c| solid_frame(width, height, c))
            .into_iter()
            .collect::<Result<Vec<_>>>()?,
        Source::Screen => Vec::new(),
    };
    let mut last: Option<capture::Frame> = None;
    let mut ticker = tokio::time::interval(Duration::from_secs(1) / fps);
    let mut report = tokio::time::interval(Duration::from_secs(1));

    (control.status)(format!("Casting {codec:?} {width}x{height}@{fps}"));
    // One future for the whole session: re-creating it per iteration can miss a stop sent mid-frame.
    let stopped = control.stopped();
    tokio::pin!(stopped);
    let end = match async {
        loop {
            tokio::select! {
                _ = &mut stopped => break Ok(End::Stopped),
                _ = &mut taken_over => break Ok(End::TakenOver),
                result = &mut lost => break Ok(End::Lost(result.unwrap_or_else(|e| anyhow!(e)))),
                _ = report.tick() => {
                    let stats = sender.statistics().await?;
                    if let Some(v) = stats.video.as_ref() {
                        if let Some(bitrate) = pipeline.rate.update(v) {
                            pipeline.encoder.set_bitrate(bitrate)?;
                        }
                        if args.stats {
                            pipeline.stats.print(v, pipeline.rate.bitrate);
                        }
                        if let Some(rtt) = v.current_rtt {
                            (control.stats)(format!("Cast RTT {} ms", rtt.as_millis()));
                        }
                    }
                }
                _ = ticker.tick(), if source == Source::Pattern => {
                    let colour = &pattern[(origin.elapsed().as_secs() % 4) as usize];
                    pipeline.send(colour, Instant::now(), &sender).await?;
                }
                _ = latest.ready.notified(), if source == Source::Screen => {
                    if let Some(frame) = latest.take_sized(width, height) {
                        pipeline.send(frame.image(), frame.captured_at, &sender).await?;
                        last = Some(frame);
                    }
                }
                _ = tokio::time::sleep_until((pipeline.last_sent + REFRESH_INTERVAL).into()), if source == Source::Screen && last.is_some() => {
                    let frame = last.clone().unwrap();
                    pipeline.send(frame.image(), Instant::now(), &sender).await?;
                }
            }
        }
    }
    .await
    {
        Ok(end) => end,
        // An error mid-session still needs the teardown below, then a reconnect.
        Err(e) => End::Lost(e),
    };

    if audio_task.is_some() {
        // Keep capturing a moment so the last real audio ramps down instead of being cut.
        fade.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(capture);
    // Closing the channel lets the audio task finish with silence before the receiver is told to stop.
    drop(audio_tx);
    if let Some(task) = audio_task {
        let _ = tokio::time::timeout(Duration::from_millis(300), task).await;
        // STOP tears the receiver down at once: let the buffered audio and the silence play out first.
        tokio::time::sleep(delay.max(MIN_AUDIO_DELAY) + Duration::from_millis(150)).await;
    }
    let _ = sender.shutdown().await;
    Ok(end)
}

pub(crate) enum VideoEncoder {
    Vt(Encoder),
    #[cfg(feature = "vp8")]
    Vp8(Box<crate::vp8::Vp8Encoder>),
}

impl VideoEncoder {
    pub(crate) fn new(codec: Codec, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let vt = |c| Ok(Self::Vt(Encoder::new(c, width, height, fps, bitrate)?));
        match codec {
            Codec::H264 => vt(VtCodec::H264),
            Codec::H264High => vt(VtCodec::H264High),
            Codec::Hevc => vt(VtCodec::Hevc),
            #[cfg(feature = "vp8")]
            Codec::Vp8 => Ok(Self::Vp8(Box::new(crate::vp8::Vp8Encoder::new(width, height, bitrate)?))),
            #[cfg(not(feature = "vp8"))]
            Codec::Vp8 => Err(anyhow!("VP8 needs a build with --features vp8")),
        }
    }

    pub(crate) fn encode(&mut self, image: &objc2_core_video::CVImageBuffer, pts: Duration, key: bool) -> Result<Option<Encoded>> {
        match self {
            Self::Vt(e) => e.encode(image, pts, key),
            #[cfg(feature = "vp8")]
            Self::Vp8(e) => e.encode(image, pts, key),
        }
    }

    /// Only VideoToolbox has a QP cap; a rejection is logged, not fatal.
    pub(crate) fn set_max_qp(&mut self, qp: Option<u32>) {
        if let (Some(qp), Self::Vt(e)) = (qp, &*self) {
            match e.set_max_qp(qp) {
                Ok(()) => info!("max QP {qp}"),
                Err(err) => warn!("max QP not supported: {err}"),
            }
        }
    }

    pub(crate) fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        match self {
            Self::Vt(e) => e.set_bitrate(bitrate),
            #[cfg(feature = "vp8")]
            Self::Vp8(e) => e.set_bitrate(bitrate),
        }
    }
}

struct Pipeline {
    encoder: VideoEncoder,
    video: StreamHandle,
    origin: Instant,
    need_key: Arc<AtomicBool>,
    rate: RateControl,
    stats: Stats,
    last_pts: Option<Duration>,
    last_sent: Instant,
    behind_since: Option<Instant>,
}

impl Pipeline {
    async fn send(
        &mut self,
        image: &objc2_core_video::CVImageBuffer,
        captured_at: Instant,
        sender: &SenderSession,
    ) -> Result<()> {
        // Never queue: skip the frame while the network is behind.
        let stats = sender.statistics().await?;
        if let Some(v) = stats.video.as_ref() {
            let behind = v.in_flight_frames > 0 && v.in_flight_media_duration >= v.max_acceptable_in_flight_duration;
            if behind && v.frames_acked > 0 {
                // Skip briefly, but not forever: a receiver that discarded late frames waits for new ones
                // while we wait for its ACKs. After STALL, a key frame breaks that deadlock.
                let since = *self.behind_since.get_or_insert_with(Instant::now);
                if since.elapsed() < STALL {
                    self.stats.dropped += 1;
                    self.last_sent = Instant::now(); // Also keeps the refresh timer from firing in a loop.
                    return Ok(());
                }
                warn!("receiver stalled for {STALL:?}, forcing a key frame");
                self.need_key.store(true, Ordering::Release);
            }
            self.behind_since = None;
        }
        let mut pts = captured_at.saturating_duration_since(self.origin);
        if let Some(prev) = self.last_pts.filter(|p| pts <= *p) {
            pts = prev + Duration::from_millis(1);
        }
        // Also when the frame gets dropped below, or the refresh timer fires again at once and spins.
        self.last_sent = Instant::now();
        let force_key = self.need_key.swap(false, Ordering::AcqRel);
        let encoded = tokio::task::block_in_place(|| self.encoder.encode(image, pts, force_key))?;
        let Some(encoded) = encoded else {
            self.need_key.fetch_or(force_key, Ordering::AcqRel);
            return Ok(());
        };
        let dependency = if encoded.key_frame { FrameDependency::KeyFrame } else { FrameDependency::Delta };
        self.stats.frames += 1;
        self.stats.bytes += encoded.data.len() as u64;
        let frame = EncodedFrame::new(dependency, Bytes::from(encoded.data), pts, self.origin + pts);
        match self.video.send(frame).await {
            Ok(_) => self.last_pts = Some(pts),
            // The receiver will not get the reference chain; restart it with an IDR.
            Err(EnqueueError::ReachedIdSpanLimit) => self.need_key.store(true, Ordering::Release),
            Err(e) => {
                warn!("video enqueue: {e}");
                self.need_key.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
}

async fn watch_events(mut events: mpsc::Receiver<SenderEvent>, need_key: Arc<AtomicBool>) -> anyhow::Error {
    while let Some(event) = events.recv().await {
        match event {
            SenderEvent::NeedsKeyFrame { .. } | SenderEvent::PictureLoss { .. } => {
                need_key.store(true, Ordering::Release);
            }
            SenderEvent::ReceiverTimedOut => return anyhow!("receiver stopped acknowledging"),
            SenderEvent::FatalError(e) => return anyhow!("sender failed: {e}"),
            SenderEvent::MalformedPacket { description } => warn!("malformed RTCP: {description}"),
            _ => {}
        }
    }
    anyhow!("sender event stream closed")
}

async fn send_audio(audio: StreamHandle, mut rx: mpsc::Receiver<capture::AudioChunk>, origin: Instant, fade: Arc<AtomicBool>) {
    let mut framer = match OpusFramer::new(origin, AUDIO_BITRATE) {
        Ok(f) => f,
        Err(e) => return warn!("opus init: {e}"),
    };
    // How late each frame leaves relative to its timestamp; frames later than the playout delay get skipped.
    let (mut late_min, mut late_max, mut late_at) = (i64::MAX, i64::MIN, Instant::now());
    loop {
        let Some(chunk) = rx.recv().await else {
            // End of session: 100 ms of silence, so the receiver doesn't cut off mid-waveform (a pop).
            let silence = vec![0.0; AUDIO_RATE as usize / 10 * AUDIO_CHANNELS];
            if let Ok(packets) = framer.push(&silence, Instant::now()) {
                for p in packets {
                    let ts = p.media_timestamp;
                    let frame = EncodedFrame::new(FrameDependency::KeyFrame, Bytes::from(p.data), ts, origin + ts);
                    let _ = audio.send(frame.with_duration(crate::audio::FRAME)).await;
                }
            }
            return;
        };
        if chunk.captured_at.elapsed() > STALE_AUDIO {
            tracing::debug!("stale audio chunk dropped ({:?} old)", chunk.captured_at.elapsed());
            framer.gap();
            continue;
        }
        if chunk.after_gap {
            framer.gap();
        }
        if fade.load(Ordering::Acquire) {
            framer.fade_out();
        }
        let packets = match framer.push(&chunk.samples, chunk.captured_at) {
            Ok(p) => p,
            Err(e) => return warn!("opus encode: {e}"),
        };
        for p in packets {
            let ts = p.media_timestamp;
            let late = Instant::now().saturating_duration_since(origin).as_millis() as i64 - ts.as_millis() as i64;
            (late_min, late_max) = (late_min.min(late), late_max.max(late));
            if late_at.elapsed() >= Duration::from_secs(1) {
                tracing::debug!("audio sent {late_min}..{late_max} ms after its timestamp");
                (late_min, late_max, late_at) = (i64::MAX, i64::MIN, Instant::now());
            }
            let frame = EncodedFrame::new(FrameDependency::KeyFrame, Bytes::from(p.data), ts, origin + ts)
                .with_duration(crate::audio::FRAME);
            if let Err(e) = audio.send(frame).await {
                warn!("audio enqueue: {e}");
            }
        }
    }
}

/// AIMD on receiver loss and send backlog.
// ponytail: loss/backlog AIMD, not openscreen's BandwidthEstimator; that needs per-burst byte counts from inside chromiacast's pacer.
struct RateControl {
    bitrate: u32,
    max: u32,
    retransmitted: u64,
    sent: u64,
}

impl RateControl {
    fn new(max: u32) -> Self {
        Self { bitrate: max, max, retransmitted: 0, sent: 0 }
    }

    /// Returns the new bitrate when it changed.
    fn update(&mut self, s: &StreamStatistics) -> Option<u32> {
        let retx = s.packets_retransmitted.saturating_sub(self.retransmitted);
        let sent = s.packets_sent.saturating_sub(self.sent).max(1);
        (self.retransmitted, self.sent) = (s.packets_retransmitted, s.packets_sent);
        // fraction_lost is RTCP's 8-bit fixed point: 13/256 = 5 %.
        let lossy = s.fraction_lost.unwrap_or(0) >= 13 || retx * 20 > sent;
        let backlog = s.in_flight_media_duration * 2 > s.max_acceptable_in_flight_duration;
        let next = if lossy || backlog {
            (self.bitrate as f64 * 0.8) as u32
        } else {
            self.bitrate + self.max / 20
        }
        .clamp(MIN_VIDEO_BITRATE.min(self.max), self.max);
        (next != self.bitrate).then(|| {
            self.bitrate = next;
            next
        })
    }
}

#[derive(Default)]
struct Stats {
    frames: u64,
    bytes: u64,
    dropped: u64,
    retransmitted: u64,
}

impl Stats {
    fn print(&mut self, v: &StreamStatistics, target: u32) {
        let rtt = v.current_rtt.map_or("-".into(), |r| format!("{} ms", r.as_millis()));
        eprintln!(
            "fps {:>3}  sent {:>5} kbit/s  target {:>5} kbit/s  rtt {rtt:>6}  retx {:>4}  dropped {:>3}  in-flight {:>2} ({} ms)",
            self.frames,
            self.bytes * 8 / 1000,
            target / 1000,
            v.packets_retransmitted - self.retransmitted,
            self.dropped,
            v.in_flight_frames,
            v.in_flight_media_duration.as_millis(),
        );
        self.retransmitted = v.packets_retransmitted;
        (self.frames, self.bytes, self.dropped) = (0, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(sent: u64, retx: u64, lost: u8, in_flight_ms: u64) -> StreamStatistics {
        let mut s = StreamStatistics::default();
        s.packets_sent = sent;
        s.packets_retransmitted = retx;
        s.fraction_lost = Some(lost);
        s.in_flight_media_duration = Duration::from_millis(in_flight_ms);
        s.max_acceptable_in_flight_duration = Duration::from_millis(100);
        s
    }

    #[test]
    fn rate_backs_off_on_loss_and_recovers() {
        let mut rc = RateControl::new(6_000_000);
        assert_eq!(rc.update(&stats(1000, 0, 0, 0)), None, "already at max");
        assert_eq!(rc.update(&stats(2000, 30, 0, 0)), None, "3% retransmits is normal Wi-Fi");
        assert_eq!(rc.update(&stats(3000, 60, 6, 0)), None, "2% receiver loss too");
        assert_eq!(rc.update(&stats(4000, 130, 0, 0)), Some(4_800_000), "7% retransmits");
        assert_eq!(rc.update(&stats(5000, 130, 30, 0)), Some(3_840_000), "12% receiver loss");
        assert_eq!(rc.update(&stats(6000, 130, 0, 80)), Some(3_072_000), "backlog");
        assert_eq!(rc.update(&stats(7000, 130, 0, 0)), Some(3_372_000), "additive increase");
    }

    #[test]
    fn rate_never_drops_below_floor() {
        let mut rc = RateControl::new(1_000_000);
        for i in 1..20 {
            rc.update(&stats(i * 100, i * 50, 100, 0));
        }
        assert_eq!(rc.bitrate, MIN_VIDEO_BITRATE);
    }
}
