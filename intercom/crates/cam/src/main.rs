//! Intercom remote camera access server.
//!
//! FFmpeg transcodes the intercom's RTSP stream into a local RTP stream
//! suitable for broadcasting. To avoid burning the CPU, we spawn it when the
//! first viewer connects, and kill it when the last viewer disconnects.
//!
//! WebRTC wraps the local RTP stream. This internally creates one SRTP stream
//! per connected browser, each with their own unique encryption keys.

mod ffmpeg;
mod webrtc;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::webrtc::WebRTC;
use crate::webrtc::traits::*;

/// URL of the IP camera.
const CAMERA_STREAM: &str = "rtsp://bastion/live";
/// URL of the transcoded RTP stream produced by FFmpeg.
const FFMPEG_STREAM: &str = "127.0.0.1:5004";

const LISTEN_ADDR: &str = "0.0.0.0:8080";
const HTML: &[u8] = include_bytes!("index.html");

#[tokio::main]
async fn main() -> Result<()> {
    let webrtc = Arc::new(WebRTC::new());

    // Constantly pump the FFmpeg RTP stream into the WebRTC track.
    tokio::spawn({
        let track = Arc::clone(&webrtc.track);
        async move {
            let sock = UdpSocket::bind(FFMPEG_STREAM).await.unwrap();
            let mut buf = [0u8; 2048];
            loop {
                let (n, _addr) = sock.recv_from(&mut buf).await.unwrap();
                let _ = track.write(&buf[..n]).await;
            }
        }
    });

    let sock = TcpListener::bind(LISTEN_ADDR).await?;
    eprintln!("Listening at {LISTEN_ADDR}");

    loop {
        let (mut sock, addr) = sock.accept().await?;

        let webrtc = Arc::clone(&webrtc);
        tokio::spawn(async move {
            if let Err(e) = serve(&mut sock, addr, webrtc).await {
                eprintln!("Error serving {addr}: {e}");
            }
        });
    }
}

async fn serve(sock: &mut TcpStream, addr: SocketAddr, webrtc: Arc<WebRTC>) -> Result<()> {
    // Size the read buffer large enough that all of our known requests will fit.
    let mut buf = [0u8; 16_384];
    let n = sock.read(&mut buf).await?;
    // Silently ignore failures here, which are probably just botnets sending us wordpress exploits.
    let Ok(req) = std::str::from_utf8(&buf[..n]) else {
        return Ok(());
    };
    let Some((resource, _)) = req.split_once(" HTTP") else {
        return Ok(());
    };
    eprintln!("{addr}: {resource}");

    match resource {
        // Send the main HTML page with the <video>
        "GET /cam" => {
            respond(sock, 200, "text/html", HTML).await?;
        }
        // Handle browsers requesting to start the WebRTC stream
        "POST /cam" => {
            #[derive(serde::Deserialize)]
            struct Offer {
                sdp: String,
            }
            #[derive(serde::Serialize)]
            struct Answer<'a> {
                #[serde(rename = "type")]
                kind: &'static str,
                sdp: &'a str,
            }

            let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
            let Offer { sdp: offer_sdp } = serde_json::from_str(body)?;

            let answer_sdp = webrtc.connect(offer_sdp).await?;
            let body = serde_json::to_vec(&Answer { kind: "answer", sdp: &answer_sdp })?;

            respond(sock, 200, "application/json", &body).await?;
        }
        _ => respond(sock, 404, "text/plain", b"404").await?,
    }
    Ok(())
}

async fn respond(sock: &mut TcpStream, code: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let len = body.len();
    let header = format!(
        "HTTP/1.1 {code} OK\r\n\
         Content-Length: {len}\r\n\
         Content-Type: {content_type}\r\n\r\n",
    );
    sock.write_all(header.as_bytes()).await?;
    sock.write_all(body).await
}
