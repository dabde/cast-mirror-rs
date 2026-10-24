//! Exercise encrypted Cast RTP transport with deliberately non-decodable data.
//!
//! This is a packetization/load diagnostic; it will not render video. For a
//! visible interoperability test, run the `red_screen` example instead.
//!
//! Usage: `cargo run --example packet_transport_stress -- <receiver_address>`
//!
//! An IP address uses Cast's default port. To preserve an IPv6 scope ID, pass
//! a complete address such as `[fe80::1234%3]:8009`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chromiacast::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let receiver_address = parse_receiver_address(
        &std::env::args()
            .nth(1)
            .ok_or("usage: packet_transport_stress <receiver_address>")?,
    )?;

    let connection = CastConnection::connect_address(receiver_address).await?;
    let app = connection.launch(APP_MIRRORING).await?;
    println!("Launched app transport {}", app.transport_id());

    let offer = Offer::builder()
        .video(VideoStreamConfig {
            codec: VideoCodec::H264,
            max_bit_rate: 5_000_000,
            max_frame_rate: Framerate::new(30, 1)?,
            resolutions: vec![Resolution::new(1920, 1080)?],
            target_delay: Duration::from_millis(400),
        })
        .build()?;
    let answer = connection.exchange_offer(&offer, &app).await?;
    let bind_ip = if receiver_address.is_ipv6() {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    };
    let transport = UdpTransport::bind(SocketAddr::new(bind_ip, 0)).await?;
    let (session, mut events) =
        SenderSession::start_address(&offer, &answer, receiver_address, transport).await?;
    let video = session.video().ok_or("receiver rejected video")?;

    let event_task = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let SenderEvent::FatalError(error) = event {
                eprintln!("sender failed: {error}");
            }
        }
    });

    let frame_interval = Duration::from_millis(33);
    let timeline_origin = Instant::now();
    let send_result: Result<(), EnqueueError> = async {
        for frame_index in 0u32..300 {
            let dependency = if frame_index % 30 == 0 {
                FrameDependency::KeyFrame
            } else {
                FrameDependency::Delta
            };
            let size = if dependency == FrameDependency::KeyFrame {
                50_000
            } else {
                5_000
            };
            let media_timestamp = frame_interval * frame_index;
            video
                .send(
                    EncodedFrame::new(
                        dependency,
                        Bytes::from(vec![0; size]),
                        media_timestamp,
                        timeline_origin + media_timestamp,
                    )
                    .with_duration(frame_interval),
                )
                .await?;

            let target = timeline_origin + frame_interval * (frame_index + 1);
            if let Some(delay) = target.checked_duration_since(Instant::now()) {
                tokio::time::sleep(delay).await;
            }
        }
        Ok(())
    }
    .await;

    if let Ok(statistics) = session.statistics().await {
        println!("Final sender statistics: {statistics:#?}");
    }
    let sender_result = session.shutdown().await;
    let event_result = event_task.await;
    let stop_result = connection.stop(&app).await;
    let close_result = connection.close().await;

    sender_result?;
    send_result?;
    event_result?;
    stop_result?;
    close_result?;
    Ok(())
}

fn parse_receiver_address(value: &str) -> Result<SocketAddr, String> {
    if let Ok(address) = value.parse() {
        return Ok(address);
    }
    value
        .parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, CAST_PORT))
        .map_err(|error| format!("invalid receiver address {value:?}: {error}"))
}
