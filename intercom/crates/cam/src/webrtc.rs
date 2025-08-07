use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::{API, APIBuilder};
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

use crate::ffmpeg::FFmpeg;

type Sdp = String;

pub mod traits {
    pub use webrtc::track::track_local::TrackLocalWriter;
}

pub struct WebRTC {
    api: API,
    pub track: Arc<TrackLocalStaticRTP>,

    ffmpeg: Arc<FFmpeg>,
}

impl WebRTC {
    pub fn new() -> Self {
        // unwrap(): this crap won't fail, I checked...
        let mut me = MediaEngine::default();
        me.register_default_codecs().unwrap();
        let reg = register_default_interceptors(Registry::default(), &mut me).unwrap();

        let api = APIBuilder::new().with_media_engine(me).with_interceptor_registry(reg).build();
        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability { mime_type: "video/H264".into(), ..Default::default() },
            "video".into(),
            "video".into(),
        ));

        let ffmpeg = Arc::new(FFmpeg::default());

        Self { api, track, ffmpeg }
    }

    pub async fn connect(&self, sdp: Sdp) -> Result<Sdp> {
        // Speak the required incantations.
        let pc = self.api.new_peer_connection(Default::default()).await?;
        let track = Arc::clone(&self.track);
        pc.add_track(track).await?;
        pc.set_remote_description(RTCSessionDescription::offer(sdp)?.into()).await?;
        let ans = pc.create_answer(None).await?;
        pc.set_local_description(ans.clone()).await?;

        // Increment/decrement FFmpeg viewers along with the connection lifetime.
        self.ffmpeg.inc_viewers();
        let ffmpeg = Arc::clone(&self.ffmpeg);
        let closed = Arc::new(AtomicBool::new(false));
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let ffmpeg = Arc::clone(&ffmpeg);
            let closed = Arc::clone(&closed);
            Box::pin(async move {
                use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::*;
                if matches!(state, Disconnected | Failed | Closed) {
                    if closed.fetch_or(true, Ordering::Relaxed) == false {
                        ffmpeg.dec_viewers();
                    }
                }
            })
        }));

        Ok(ans.sdp)
    }
}
