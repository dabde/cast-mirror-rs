use bytes::Bytes;
use openh264::encoder::{Encoder, FrameType};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chromiacast::*;

const IMAGE: &str = "docker.io/rgerganov/shanocast:latest";
const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const SHANOCAST_PORT: u16 = 8010;

struct Harness {
    xvfb: Child,
    container_name: String,
    display: String,
}

impl Harness {
    fn start() -> Self {
        let display_num = 50 + (std::process::id() % 50) as u16;
        let display = format!(":{display_num}");
        let container_name = format!("chromiacast-test-{display_num}");

        let xvfb = Command::new("Xvfb")
            .args([&display, "-screen", "0", "1920x1080x24", "-ac"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start Xvfb (is xorg-x11-server-Xvfb installed?)");

        std::thread::sleep(Duration::from_secs(1));

        Command::new("podman")
            .args(["rm", "-f", &container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();

        let status = Command::new("podman")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &container_name,
                "--network",
                "host",
                "-e",
                &format!("DISPLAY={display}"),
                "-e",
                "SDL_VIDEODRIVER=x11",
                "-e",
                "SDL_AUDIODRIVER=dummy",
                "-v",
                "/tmp/.X11-unix:/tmp/.X11-unix:rw",
                IMAGE,
                "-x",
                "-v",
                "lo",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("failed to start shanocast container (is podman installed?)");

        assert!(status.success(), "podman run failed — is the image pulled?");

        wait_for_port(SHANOCAST_PORT, Duration::from_secs(10));

        Harness {
            xvfb,
            container_name,
            display,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "-f", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let _ = self.xvfb.kill();
        let _ = self.xvfb.wait();
    }
}

fn wait_for_port(port: u16, timeout: Duration) {
    let start = Instant::now();
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    while start.elapsed() < timeout {
        if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("port {port} not ready after {timeout:?}");
}

fn encode_solid_frame(encoder: &mut Encoder, r: u8, g: u8, b: u8) -> (Vec<u8>, FrameDependency) {
    let rgb_data: Vec<u8> = [r, g, b].repeat(WIDTH * HEIGHT);
    let source = RgbSliceU8::new(&rgb_data, (WIDTH, HEIGHT));
    let yuv = YUVBuffer::from_rgb8_source(source);
    encoder.force_intra_frame();
    let bitstream = encoder.encode(&yuv).expect("encode failed");
    let dependency = match bitstream.frame_type() {
        FrameType::IDR | FrameType::I => FrameDependency::KeyFrame,
        FrameType::P => FrameDependency::Delta,
        other => panic!("unexpected OpenH264 frame type: {other:?}"),
    };
    (bitstream.to_vec(), dependency)
}

fn capture_screenshot(display: &str, path: &str) {
    let output = Command::new("import")
        .env("DISPLAY", display)
        .args(["-window", "root", path])
        .output()
        .expect("failed to run import (is ImageMagick installed?)");

    assert!(
        output.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn sample_center_color(path: &str) -> (u8, u8, u8) {
    let output = Command::new("convert")
        .args([
            path,
            "-crop",
            "100x100+910+490",
            "-resize",
            "1x1!",
            "-format",
            "%[fx:int(r*255)],%[fx:int(g*255)],%[fx:int(b*255)]",
            "info:",
        ])
        .output()
        .expect("failed to run convert (is ImageMagick installed?)");

    let text = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<u8> = text
        .trim()
        .split(',')
        .map(|s| s.parse().expect("bad pixel value"))
        .collect();

    (parts[0], parts[1], parts[2])
}

#[tokio::test]
#[ignore]
async fn rendered_pixels_match_input() {
    let harness = Harness::start();

    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), SHANOCAST_PORT);
    let conn = CastConnection::connect_address_unverified_for_testing(address)
        .await
        .unwrap();
    let app = conn.launch(APP_MIRRORING).await.unwrap();

    let offer = Offer::builder()
        .video(VideoStreamConfig {
            codec: VideoCodec::H264,
            max_bit_rate: 5_000_000,
            max_frame_rate: Framerate::new(30, 1).unwrap(),
            resolutions: vec![Resolution::new(WIDTH as u32, HEIGHT as u32).unwrap()],
            target_delay: Duration::from_millis(400),
        })
        .build()
        .unwrap();

    let answer = conn.exchange_offer(&offer, &app).await.unwrap();

    let transport = UdpTransport::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        .await
        .unwrap();
    let (session, mut events) =
        SenderSession::start(&offer, &answer, IpAddr::V4(Ipv4Addr::LOCALHOST), transport)
            .await
            .unwrap();

    let event_handle = tokio::spawn(async move {
        let mut ack_count = 0u32;
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        {
            if let SenderEvent::FrameAcked { .. } = event {
                ack_count += 1;
            }
        }
        ack_count
    });

    let video = session.video().expect("no video stream");
    let mut encoder = Encoder::new().expect("failed to create encoder");

    let start = Instant::now();
    for i in 0u32..30 {
        let (h264_data, dependency) = encode_solid_frame(&mut encoder, 255, 0, 0);
        let media_timestamp = Duration::from_millis(33) * i;

        video
            .send(
                EncodedFrame::new(
                    dependency,
                    Bytes::from(h264_data),
                    media_timestamp,
                    start + media_timestamp,
                )
                .with_duration(Duration::from_millis(33)),
            )
            .await
            .unwrap();

        let target = start + Duration::from_millis(33) * i;
        if let Some(delay) = target.checked_duration_since(Instant::now()) {
            tokio::time::sleep(delay).await;
        }
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    let screenshot_path = format!("/tmp/chromiacast-pixel-verify-{}.png", std::process::id());
    capture_screenshot(&harness.display, &screenshot_path);

    let (r, g, b) = sample_center_color(&screenshot_path);
    let _ = std::fs::remove_file(&screenshot_path);

    session.shutdown().await.unwrap();
    let acks = event_handle.await.unwrap();
    conn.close().await.unwrap();

    assert!(acks > 0, "receiver did not ACK any frames");
    assert!(
        r > 200 && g < 50 && b < 50,
        "expected red, got R={r} G={g} B={b} (YUV rounding tolerance: R>200, G<50, B<50)",
    );
}
