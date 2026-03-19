use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch, Notify};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use crate::encode::EncodedAccessUnit;

pub struct WebRtcSender {
    pub pc: Arc<RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    connected: Arc<Notify>,
}

impl WebRtcSender {
    pub async fn new() -> Result<Self> {
        let mut media_engine = MediaEngine::default();

        let h264_feedback = vec![
            RTCPFeedback { typ: "goog-remb".to_owned(), parameter: String::new() },
            RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
            RTCPFeedback { typ: "nack".to_owned(), parameter: String::new() },
            RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
        ];

        for (profile, pt) in [
            ("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f", 102u8),
            ("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f", 127),
            ("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f", 125),
            ("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f", 108),
            ("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032", 123),
        ] {
            media_engine.register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: MIME_TYPE_H264.to_owned(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line: profile.to_owned(),
                        rtcp_feedback: h264_feedback.clone(),
                    },
                    payload_type: pt,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )?;
        }

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let pc = Arc::new(api.new_peer_connection(config).await?);

        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f".to_owned(),
                rtcp_feedback: vec![],
            },
            "video".to_owned(),
            "screx-stream".to_owned(),
        ));

        pc.add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        let connected = Arc::new(Notify::new());
        let connected_clone = connected.clone();

        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            println!("[webrtc] connection state: {state}");
            if state == RTCPeerConnectionState::Connected {
                connected_clone.notify_waiters();
            }
            Box::pin(async {})
        }));

        println!("[webrtc] peer connection created, waiting for signaling");

        Ok(Self {
            pc,
            video_track,
            connected,
        })
    }

    pub async fn handle_offer(&self, offer_sdp: String) -> Result<String> {
        let offer = RTCSessionDescription::offer(offer_sdp)?;
        self.pc.set_remote_description(offer).await?;

        let answer = self.pc.create_answer(None).await?;

        let mut gather_complete = self.pc.gathering_complete_promise().await;
        self.pc.set_local_description(answer).await?;
        let _ = gather_complete.recv().await;

        let local_desc = self
            .pc
            .local_description()
            .await
            .context("no local description after answer")?;

        Ok(local_desc.sdp)
    }

    pub async fn wait_connected(&self) {
        self.connected.notified().await;
    }

    pub async fn send_sample(&self, au: &EncodedAccessUnit) -> Result<()> {
        self.video_track
            .write_sample(&Sample {
                data: au.annex_b.clone().into(),
                duration: Duration::from_millis(16),
                ..Default::default()
            })
            .await?;
        Ok(())
    }
}

pub async fn run_webrtc_feed_loop(
    sender: Arc<WebRtcSender>,
    mut au_rx: mpsc::Receiver<EncodedAccessUnit>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    println!("[webrtc] waiting for peer to connect...");
    sender.wait_connected().await;
    println!("[webrtc] peer connected, streaming video");

    let mut frames_in_window = 0_u64;
    let mut bytes_in_window = 0_u64;
    let mut window_start = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    println!("[webrtc] stop signal received");
                    break;
                }
            }
            maybe_au = au_rx.recv() => {
                let Some(au) = maybe_au else {
                    println!("[webrtc] upstream channel closed");
                    break;
                };

                bytes_in_window += au.annex_b.len() as u64;
                frames_in_window += 1;

                if let Err(e) = sender.send_sample(&au).await {
                    eprintln!("[webrtc] write_sample error: {e}");
                }

                if window_start.elapsed() >= Duration::from_secs(1) {
                    let elapsed = window_start.elapsed().as_secs_f64();
                    let fps = frames_in_window as f64 / elapsed;
                    let mbps = (bytes_in_window as f64 * 8.0 / elapsed) / 1_000_000.0;
                    println!("[webrtc] fps={fps:.1} stream_mbps={mbps:.2}");
                    frames_in_window = 0;
                    bytes_in_window = 0;
                    window_start = tokio::time::Instant::now();
                }
            }
        }
    }

    let _ = sender.pc.close().await;
    Ok(())
}
