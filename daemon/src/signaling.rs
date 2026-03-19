use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::CorsLayer;

use crate::webrtc_sender::WebRtcSender;

pub fn build_router(sender: Arc<WebRtcSender>) -> Router {
    Router::new()
        .route("/", get(serve_receiver_page))
        .route("/api/offer", post(handle_offer))
        .layer(CorsLayer::permissive())
        .with_state(sender)
}

async fn serve_receiver_page() -> Html<&'static str> {
    Html(RECEIVER_HTML)
}

async fn handle_offer(
    State(sender): State<Arc<WebRtcSender>>,
    body: String,
) -> Result<String, (StatusCode, String)> {
    println!("[signaling] received SDP offer ({} bytes)", body.len());
    sender
        .handle_offer(body)
        .await
        .map(|answer| {
            println!("[signaling] sending SDP answer ({} bytes)", answer.len());
            answer
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("signaling error: {e}")))
}

const RECEIVER_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no">
<title>Screx</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
html,body{width:100%;height:100%;overflow:hidden;background:#000}
video{width:100%;height:100%;object-fit:contain;display:block}
#status{position:fixed;top:12px;left:12px;color:#0f0;font:13px/1.4 monospace;
  background:rgba(0,0,0,.7);padding:6px 10px;border-radius:6px;z-index:10;
  pointer-events:none;transition:opacity .3s}
#status.hidden{opacity:0}
</style>
</head>
<body>
<div id="status">Connecting...</div>
<video id="v" autoplay playsinline muted></video>
<script>
const status = document.getElementById('status');
const video = document.getElementById('v');
let hideTimer;

function log(msg) {
  status.textContent = msg;
  status.classList.remove('hidden');
  clearTimeout(hideTimer);
  hideTimer = setTimeout(() => status.classList.add('hidden'), 4000);
}

async function start() {
  const pc = new RTCPeerConnection({
    iceServers: [{ urls: 'stun:stun.l.google.com:19302' }]
  });

  pc.ontrack = (ev) => {
    log('Track received, playing...');
    video.srcObject = ev.streams[0] || new MediaStream([ev.track]);
    video.play().catch(() => {});
  };

  pc.oniceconnectionstatechange = () => log('ICE: ' + pc.iceConnectionState);
  pc.onconnectionstatechange = () => {
    log('Connection: ' + pc.connectionState);
    if (pc.connectionState === 'failed') setTimeout(start, 2000);
  };

  pc.addTransceiver('video', { direction: 'recvonly' });

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);

  await new Promise(r => {
    if (pc.iceGatheringState === 'complete') return r();
    pc.onicegatheringstatechange = () => {
      if (pc.iceGatheringState === 'complete') r();
    };
  });

  log('Sending offer...');
  const resp = await fetch('/api/offer', {
    method: 'POST',
    body: pc.localDescription.sdp
  });
  if (!resp.ok) { log('Signaling failed: ' + resp.status); return; }
  const answerSdp = await resp.text();
  await pc.setRemoteDescription({ type: 'answer', sdp: answerSdp });
  log('Connected!');
}

start().catch(e => log('Error: ' + e.message));

document.addEventListener('click', () => {
  status.classList.toggle('hidden');
});
</script>
</body>
</html>"##;
