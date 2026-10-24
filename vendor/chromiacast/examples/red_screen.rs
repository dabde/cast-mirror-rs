//! Cast a decodable solid-red H.264 stream to the first discovered receiver.
//!
//! Run with `cargo run --features discovery --example red_screen` and press
//! Ctrl-C to stop the receiver application cleanly.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use openh264::encoder::{Encoder, FrameType};
use openh264::formats::{RgbSliceU8, YUVBuffer};

use chromiacast::*;

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const KEY_FRAME_INTERVAL: u64 = 150;
const STARTUP_KEY_FRAME_INTERVAL: u64 = 15;

fn startup_key_frame_due(frame_index: u64, frames_acked: u64) -> bool {
    frames_acked == 0 && frame_index % STARTUP_KEY_FRAME_INTERVAL == 0
}

fn should_throttle(under_pressure: bool, frame_index: u64, frames_acked: u64) -> bool {
    under_pressure && !startup_key_frame_due(frame_index, frames_acked)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }

    println!("Scanning for Cast receivers for five seconds...");
    let devices = discover(Duration::from_secs(5)).await?;
    if devices.is_empty() {
        return Err("no Cast receivers found".into());
    }
    for (index, device) in devices.iter().enumerate() {
        println!(
            "  [{index}] {} ({}) at {}",
            device.name(),
            device.model(),
            device.preferred_endpoint().address()
        );
    }

    let device = &devices[0];
    println!("Connecting securely to {}...", device.name());
    let receiver_endpoint = device.preferred_endpoint().address();
    let connection = CastConnection::connect_address(receiver_endpoint).await?;
    let identity = connection
        .device_identity()
        .expect("production connection must be authenticated");
    println!(
        "Receiver authenticated (audio-only={}, revocation-data={})",
        identity.is_audio_only(),
        identity.revocation_checked()
    );

    if connection.get_app_availability(APP_MIRRORING).await? != AppAvailability::Available {
        return Err("receiver does not offer the mirroring application".into());
    }
    let app = connection.launch(APP_MIRRORING).await?;

    let offer = Offer::builder()
        .video(VideoStreamConfig {
            codec: VideoCodec::H264,
            max_bit_rate: 5_000_000,
            max_frame_rate: Framerate::new(30, 1)?,
            resolutions: vec![Resolution::new(WIDTH as u32, HEIGHT as u32)?],
            target_delay: Duration::from_millis(400),
        })
        .build()?;
    let answer = connection.exchange_offer(&offer, &app).await?;
    println!(
        "Negotiated video stream on receiver UDP port {} (receiver SSRC {:?}).",
        answer.udp_port, answer.sync_sources
    );

    let bind_ip = if receiver_endpoint.is_ipv6() {
        "::".parse::<IpAddr>()?
    } else {
        "0.0.0.0".parse::<IpAddr>()?
    };
    let transport = UdpTransport::bind(SocketAddr::new(bind_ip, 0)).await?;
    let (session, mut events) =
        SenderSession::start_address(&offer, &answer, receiver_endpoint, transport).await?;
    let video = session
        .video()
        .ok_or("receiver rejected the video stream")?;

    let needs_key_frame = Arc::new(AtomicBool::new(true));
    let event_key_frame = Arc::clone(&needs_key_frame);
    let event_task = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                SenderEvent::NeedsKeyFrame { .. } => {
                    event_key_frame.store(true, Ordering::Release);
                    eprintln!("receiver requested a key frame");
                }
                SenderEvent::PictureLoss { .. } => eprintln!("receiver reported picture loss"),
                SenderEvent::ReceiverTimedOut => eprintln!("receiver acknowledgements timed out"),
                SenderEvent::FatalError(error) => eprintln!("stream failed: {error}"),
                SenderEvent::MalformedPacket { description } => {
                    eprintln!("ignored malformed RTCP: {description}");
                }
                _ => {}
            }
        }
    });

    let rgb_data: Vec<u8> = [255, 0, 0].repeat(WIDTH * HEIGHT);
    let source = RgbSliceU8::new(&rgb_data, (WIDTH, HEIGHT));
    let yuv = YUVBuffer::from_rgb8_source(source);
    let mut encoder = Encoder::new()?;
    let timeline_origin = Instant::now();
    let mut frame_index = 0u64;
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    println!("Streaming a red screen; press Ctrl-C to stop.");
    let stream_result: Result<(), Box<dyn std::error::Error>> = async {
        loop {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result?;
                    break Ok(());
                }
                _ = ticker.tick() => {
                    let statistics = session.statistics().await?;
                    let video_statistics = statistics
                        .video
                        .as_ref()
                        .ok_or("video statistics disappeared")?;
                    let under_pressure = video_statistics.in_flight_frames != 0
                        && video_statistics.in_flight_media_duration
                            >= video_statistics.max_acceptable_in_flight_duration;
                    let startup_key_frame =
                        startup_key_frame_due(frame_index, video_statistics.frames_acked);
                    if should_throttle(
                        under_pressure,
                        frame_index,
                        video_statistics.frames_acked,
                    ) {
                        // Preserve real-time media timestamps without feeding
                        // another frame into the encoder until the receiver
                        // has made room. Because no frame was encoded, the
                        // next delta frame still references the last frame the
                        // receiver actually received.
                        frame_index += 1;
                        continue;
                    }
                    if startup_key_frame && under_pressure {
                        eprintln!("receiver media path is not ready; retrying with a key frame");
                    }
                    let key_frame_requested = needs_key_frame.swap(false, Ordering::AcqRel);
                    if startup_key_frame
                        || key_frame_requested
                        || frame_index % KEY_FRAME_INTERVAL == 0
                    {
                        encoder.force_intra_frame();
                    }
                    let bitstream = encoder.encode(&yuv)?;
                    let dependency = match bitstream.frame_type() {
                        FrameType::IDR | FrameType::I => FrameDependency::KeyFrame,
                        FrameType::P => FrameDependency::Delta,
                        FrameType::Skip => {
                            frame_index += 1;
                            continue;
                        }
                        frame_type => {
                            return Err(
                                format!("unsupported OpenH264 frame type: {frame_type:?}").into()
                            );
                        }
                    };
                    let media_timestamp = FRAME_INTERVAL * u32::try_from(frame_index)?;
                    let enqueue_result = video.send(
                        EncodedFrame::new(
                            dependency,
                            Bytes::from(bitstream.to_vec()),
                            media_timestamp,
                            timeline_origin + media_timestamp,
                        )
                        .with_duration(FRAME_INTERVAL),
                    ).await;
                    match enqueue_result {
                        Ok(_) => {}
                        Err(EnqueueError::ReachedIdSpanLimit) => {
                            // The encoder has advanced past a frame the
                            // receiver will not see. Re-enter with an IDR once
                            // feedback opens the hard frame-ID window.
                            needs_key_frame.store(true, Ordering::Release);
                        }
                        Err(error) => return Err(error.into()),
                    }
                    frame_index += 1;
                }
            }
        }
    }
    .await;

    let sender_result = session.shutdown().await;
    let event_result = event_task.await;
    let stop_result = connection.stop(&app).await;
    let close_result = connection.close().await;

    sender_result?;
    stream_result?;
    event_result?;
    stop_result?;
    close_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_key_frames_repeat_until_receiver_acknowledges() {
        assert!(startup_key_frame_due(0, 0));
        assert!(!startup_key_frame_due(1, 0));
        assert!(startup_key_frame_due(STARTUP_KEY_FRAME_INTERVAL, 0));
        assert!(startup_key_frame_due(STARTUP_KEY_FRAME_INTERVAL * 2, 0));
        assert!(!startup_key_frame_due(STARTUP_KEY_FRAME_INTERVAL * 2, 1));

        assert!(!should_throttle(true, STARTUP_KEY_FRAME_INTERVAL, 0));
        assert!(should_throttle(true, 1, 0));
        assert!(should_throttle(true, STARTUP_KEY_FRAME_INTERVAL, 1));
    }
}
