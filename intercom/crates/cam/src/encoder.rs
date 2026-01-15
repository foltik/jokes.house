//! RTSP → H.264 transcode → RTP packets

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::sync::mpsc;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocalWriter;

extern crate ffmpeg_next as ffmpeg;
use ffmpeg::codec::{self, Context as CodecContext};
use ffmpeg::format::{self, Pixel};
use ffmpeg::frame::Video as Frame;
use ffmpeg::software::scaling::{self, Flags};
use ffmpeg::{Dictionary, Packet, Rational};

/// Run encoder forever, restarting on errors.
pub async fn run(rtsp_url: &str, track: Arc<TrackLocalStaticRTP>, pli: Arc<AtomicBool>) {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);

    // RTP writer task
    let writer = tokio::spawn(async move {
        while let Some(pkt) = rx.recv().await {
            let _ = track.write(&pkt).await;
        }
    });

    // Encoder loop (blocking, restarts on failure)
    let url = rtsp_url.to_string();
    tokio::task::spawn_blocking(move || {
        loop {
            if let Err(e) = encode(&url, &tx, &pli) {
                eprintln!("Encoder: {e:#}");
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });

    let _ = writer.await;
}

fn encode(url: &str, tx: &mpsc::Sender<Vec<u8>>, pli: &AtomicBool) -> Result<()> {
    ffmpeg::init()?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);

    let mut opts = Dictionary::new();
    opts.set("rtsp_transport", "tcp");
    opts.set("timeout", "5000000");

    let mut input = format::input_with_dictionary(url, opts)?;
    let stream = input.streams().best(ffmpeg::media::Type::Video).context("no video")?;
    let stream_idx = stream.index();

    let mut decoder = CodecContext::from_parameters(stream.parameters())?.decoder().video()?;
    let (w, h, fmt) = (decoder.width(), decoder.height(), decoder.format());

    // Encoder setup
    let codec = codec::encoder::find_by_name("libx264").context("no libx264")?;
    let mut encoder = CodecContext::new_with_codec(codec).encoder().video()?;
    encoder.set_width(w);
    encoder.set_height(h);
    encoder.set_format(Pixel::YUV420P);
    encoder.set_frame_rate(Some(Rational::new(15, 1)));
    encoder.set_time_base(Rational::new(1, 15));
    encoder.set_bit_rate(400_000);
    encoder.set_max_bit_rate(500_000);

    let mut x264 = Dictionary::new();
    x264.set("preset", "veryfast");
    x264.set("tune", "zerolatency");
    x264.set("profile", "baseline");
    x264.set("x264-params", "keyint=150:min-keyint=150:scenecut=0:repeat-headers=1:bframes=0");
    x264.set("bufsize", "600k");
    let mut encoder = encoder.open_with(x264)?;

    let mut scaler = (fmt != Pixel::YUV420P).then(|| {
        scaling::Context::get(fmt, w, h, Pixel::YUV420P, w, h, Flags::FAST_BILINEAR)
    }).transpose()?;

    let mut rtp = RtpState::default();
    let mut decoded = Frame::empty();
    let mut scaled = Frame::empty();

    for (s, pkt) in input.packets() {
        if s.index() != stream_idx {
            continue;
        }

        decoder.send_packet(&pkt)?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            let frame = match &mut scaler {
                Some(s) => { s.run(&decoded, &mut scaled)?; &mut scaled }
                None => &mut decoded
            };

            // Force keyframe on PLI
            if pli.swap(false, Ordering::Relaxed) {
                frame.set_kind(ffmpeg::picture::Type::I);
            } else {
                frame.set_kind(ffmpeg::picture::Type::None);
            }

            encoder.send_frame(frame)?;

            let mut encoded = Packet::empty();
            while encoder.receive_packet(&mut encoded).is_ok() {
                for rtp_pkt in rtp.packetize(encoded.data().context("no data")?) {
                    if tx.blocking_send(rtp_pkt).is_err() {
                        bail!("channel closed");
                    }
                }
            }
            rtp.tick();
        }
    }
    Ok(())
}

/// RTP packetization state (RFC 6184).
#[derive(Default)]
struct RtpState {
    seq: u16,
    ts: u32,
}

impl RtpState {
    const SSRC: u32 = 0x12345678;
    const PT: u8 = 96;
    const MTU: usize = 1200;

    fn tick(&mut self) {
        self.ts = self.ts.wrapping_add(6000); // 90kHz / 15fps
    }

    fn packetize(&mut self, nal_data: &[u8]) -> Vec<Vec<u8>> {
        let mut pkts = Vec::new();
        let nals: Vec<_> = split_nals(nal_data);
        let nal_count = nals.len();

        for (nal_idx, nal) in nals.into_iter().enumerate() {
            let is_last_nal = nal_idx == nal_count - 1;

            if nal.len() <= Self::MTU - 12 {
                // Single NAL - marker only on last NAL
                pkts.push(self.rtp_packet(nal, is_last_nal));
            } else {
                // FU-A fragmentation
                let (indicator, nal_type) = (nal[0] & 0x60 | 28, nal[0] & 0x1F);
                let payload = &nal[1..];
                let max = Self::MTU - 14;

                for (i, chunk) in payload.chunks(max).enumerate() {
                    let first = i == 0;
                    let last_frag = (i + 1) * max >= payload.len();
                    let header = nal_type | if first { 0x80 } else { 0 } | if last_frag { 0x40 } else { 0 };

                    let marker = is_last_nal && last_frag;
                    let mut pkt = self.rtp_header(marker);
                    pkt.push(indicator);
                    pkt.push(header);
                    pkt.extend_from_slice(chunk);
                    pkts.push(pkt);
                    self.seq = self.seq.wrapping_add(1);
                }
            }
        }
        pkts
    }

    fn rtp_packet(&mut self, payload: &[u8], marker: bool) -> Vec<u8> {
        let mut pkt = self.rtp_header(marker);
        pkt.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        pkt
    }

    fn rtp_header(&self, marker: bool) -> Vec<u8> {
        let mut h = Vec::with_capacity(12);
        h.push(0x80);
        h.push(if marker { 0x80 | Self::PT } else { Self::PT });
        h.extend_from_slice(&self.seq.to_be_bytes());
        h.extend_from_slice(&self.ts.to_be_bytes());
        h.extend_from_slice(&Self::SSRC.to_be_bytes());
        h
    }
}

fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut start = 0;

    // Skip leading start code
    if data.starts_with(&[0, 0, 0, 1]) { start = 4; i = 4; }
    else if data.starts_with(&[0, 0, 1]) { start = 3; i = 3; }

    while i + 2 < data.len() {
        let code_len = if data[i..].starts_with(&[0, 0, 0, 1]) { 4 }
                       else if data[i..].starts_with(&[0, 0, 1]) { 3 }
                       else { 0 };
        if code_len > 0 {
            if i > start { nals.push(&data[start..i]); }
            start = i + code_len;
            i = start;
        } else {
            i += 1;
        }
    }
    if start < data.len() { nals.push(&data[start..]); }
    if nals.is_empty() && !data.is_empty() { nals.push(data); }
    nals
}
