use std::sync::Arc;

use anyhow::Result;
use tokio::net::UdpSocket;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{API, APIBuilder};
use webrtc::ice::udp_mux::{UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

type Sdp = String;

pub mod traits {
    pub use webrtc::track::track_local::TrackLocalWriter;
}

pub struct WebRTC {
    api: API,
    pub track: Arc<TrackLocalStaticRTP>,
}

impl WebRTC {
    pub async fn new(public_ip: String) -> Result<Self> {
        // unwrap(): this crap won't fail, I checked...
        let mut me = MediaEngine::default();
        //me.register_default_codecs().unwrap();
        me.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/H264".into(),
                    sdp_fmtp_line: "packetization-mode=1;profile-level-id=42e01f".into(),
                    ..Default::default()
                },
                payload_type: 96,
                stats_id: "video".into(),
            },
            webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video,
        ).unwrap();
        let reg = register_default_interceptors(Registry::default(), &mut me).unwrap();

        let udp_socket = UdpSocket::bind("0.0.0.0:50000").await?;
        let udp_mux = UDPMuxDefault::new(UDPMuxParams::new(udp_socket));

        let mut se = SettingEngine::default();
        se.set_udp_network(UDPNetwork::Muxed(udp_mux));
        se.set_nat_1to1_ips(vec![public_ip], RTCIceCandidateType::Host);

        let api = APIBuilder::new()
            .with_media_engine(me).
            with_interceptor_registry(reg)
            .with_setting_engine(se)
            .build();

        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability { mime_type: "video/H264".into(), ..Default::default() },
            "video".into(),
            "video".into(),
        ));

        Ok(Self { api, track })
    }

    pub async fn connect(&self, sdp: Sdp) -> Result<Sdp> {
        // Speak the required incantations.
        let pc = self.api.new_peer_connection(Default::default()).await?;

        let track = Arc::clone(&self.track);
        pc.add_track(track).await?;
        pc.set_remote_description(RTCSessionDescription::offer(sdp)?.into()).await?;
        let ans = pc.create_answer(None).await?;
        pc.set_local_description(ans.clone()).await?;

        // Wait for ICE gathering
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = std::sync::Mutex::new(Some(tx));
        pc.on_ice_gathering_state_change(Box::new(move |state| {
            if state == RTCIceGathererState::Complete {
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }
            Box::pin(async {})
        }));
        let _ = rx.await;

        let sdp = pc.local_description().await.unwrap().sdp;
        Ok(sdp)
    }
}
