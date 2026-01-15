//! Intercom camera server.
//!
//! RTSP camera → H.264 transcode → WebRTC

mod encoder;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::udp_mux::{UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

const CAMERA: &str = "rtsp://10.16.209.181/live";
const HTML: &[u8] = include_bytes!("index.html");

/// Shared state for the camera server.
struct Camera {
    track: Arc<TrackLocalStaticRTP>,
    pli: Arc<AtomicBool>,
    webrtc: webrtc::api::API,
    peers: Mutex<Vec<Arc<RTCPeerConnection>>>,
}

impl Camera {
    async fn new(public_ip: &str) -> Result<Self> {
        // Codec
        let mut media = MediaEngine::default();
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/H264".into(),
                    sdp_fmtp_line: "packetization-mode=1;profile-level-id=42e01f".into(),
                    ..Default::default()
                },
                payload_type: 96,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;

        // ICE over single UDP port
        let sock = UdpSocket::bind("0.0.0.0:50000").await?;
        let mux = UDPMuxDefault::new(UDPMuxParams::new(sock));

        let mut settings = SettingEngine::default();
        settings.set_lite(true);
        settings.set_udp_network(UDPNetwork::Muxed(mux));
        settings.set_network_types(vec![NetworkType::Udp4]);
        let ip: std::net::IpAddr = public_ip.parse()?;
        if ip.is_loopback() {
            settings.set_include_loopback_candidate(true);
        }
        settings.set_ip_filter(Box::new(move |candidate| candidate == ip));

        let registry = register_default_interceptors(Registry::new(), &mut media)?;
        let webrtc = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();

        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability { mime_type: "video/H264".into(), ..Default::default() },
            "video".into(),
            "cam".into(),
        ));

        Ok(Self {
            track,
            pli: Arc::new(AtomicBool::new(false)),
            webrtc,
            peers: Mutex::new(Vec::new()),
        })
    }

    async fn add_viewer(&self, offer: String) -> Result<String> {
        let pc = Arc::new(self.webrtc.new_peer_connection(Default::default()).await?);

        // Request keyframe when connected (+ backup after 100ms)
        let pli = self.pli.clone();
        pc.on_ice_connection_state_change(Box::new(move |s| {
            if s == RTCIceConnectionState::Connected {
                pli.store(true, Ordering::Relaxed);
                let pli2 = pli.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    pli2.store(true, Ordering::Relaxed);
                });
            }
            Box::pin(async {})
        }));

        pc.add_track(self.track.clone()).await?;
        pc.set_remote_description(RTCSessionDescription::offer(offer)?).await?;
        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer).await?;

        // Wait for ICE candidates
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = std::sync::Mutex::new(Some(tx));
        pc.on_ice_gathering_state_change(Box::new(move |s| {
            if s == webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState::Complete {
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }
            Box::pin(async {})
        }));
        let _ = rx.await;

        let sdp = pc.local_description().await.context("no local SDP")?.sdp;
        self.peers.lock().await.push(pc);
        Ok(sdp)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let public_ip = if std::env::var("LOCAL").is_ok() {
        "127.0.0.1".into()
    } else {
        tokio::net::lookup_host("jokes.house:443")
            .await?
            .next()
            .context("DNS failed")?
            .ip()
            .to_string()
    };
    eprintln!("Public IP: {public_ip}");

    let cam = Arc::new(Camera::new(&public_ip).await?);

    // Encoder writes RTP to track, checks PLI flag
    tokio::spawn(encoder::run(CAMERA, cam.track.clone(), cam.pli.clone()));

    let http = TcpListener::bind("0.0.0.0:8080").await?;
    eprintln!("Listening on :8080");

    loop {
        let (sock, _) = http.accept().await?;
        let cam = cam.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, &cam).await {
                eprintln!("Error: {e}");
            }
        });
    }
}

async fn handle(mut sock: TcpStream, cam: &Camera) -> Result<()> {
    let mut buf = [0u8; 8192];
    let n = sock.read(&mut buf).await?;
    let req = std::str::from_utf8(&buf[..n])?;
    let path = req.split_whitespace().nth(1).unwrap_or("/");

    match (req.starts_with("GET"), req.starts_with("POST"), path) {
        (true, _, "/cam") => respond(&mut sock, 200, "text/html", HTML).await,
        (_, true, "/cam") => {
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
            let offer: serde_json::Value = serde_json::from_str(body)?;
            let sdp = cam.add_viewer(offer["sdp"].as_str().unwrap_or("").into()).await?;
            let answer = serde_json::json!({"type": "answer", "sdp": sdp});
            respond(&mut sock, 200, "application/json", answer.to_string().as_bytes()).await
        }
        _ => respond(&mut sock, 404, "text/plain", b"not found").await,
    }
}

async fn respond(sock: &mut TcpStream, status: u16, ct: &str, body: &[u8]) -> Result<()> {
    let hdr = format!("HTTP/1.1 {status} OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n\r\n", body.len());
    sock.write_all(hdr.as_bytes()).await?;
    sock.write_all(body).await?;
    Ok(())
}
