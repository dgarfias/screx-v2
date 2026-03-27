//! Webcam capture adapter — captures frames from the local webcam,
//! compresses to MJPEG, chunks, and sends as CAM UDP packets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::Camera;

use crate::backend::UdpSender;

const CAM_MAGIC: &[u8] = b"CAM";
const CHUNK_SIZE: usize = 1300;

/// Handle to the running webcam capture.
pub struct WebcamCapture {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl WebcamCapture {
    /// Start webcam capture at the given resolution/fps.
    /// Frames are sent as CAM UDP packets via `udp`.
    pub fn start(udp: Arc<UdpSender>, width: u32, height: u32, fps: u32) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let thread = thread::spawn(move || {
            if let Err(e) = capture_loop(udp, width, height, fps, stop_flag) {
                eprintln!("[webcam] capture loop ended: {e:#}");
            }
        });

        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Stop the capture thread.
    pub fn stop_capture(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WebcamCapture {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

fn capture_loop(
    udp: Arc<UdpSender>,
    width: u32,
    height: u32,
    fps: u32,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
        CameraFormat::new(Resolution::new(width, height), FrameFormat::MJPEG, fps),
    ));

    let mut camera = Camera::new(CameraIndex::Index(0), requested).context("open webcam")?;

    camera.open_stream().context("start webcam stream")?;

    let mut frame_id: u32 = 0;
    let frame_interval = Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });

    while !stop.load(Ordering::Relaxed) {
        // Try to get a raw MJPEG frame directly.
        match camera.frame_raw() {
            Ok(raw_buf) => {
                let jpeg_data: &[u8] = &raw_buf;
                send_camera_frame(&udp, frame_id, jpeg_data);
                frame_id = frame_id.wrapping_add(1);
            }
            Err(e) => {
                eprintln!("[webcam] frame error: {e}");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        }

        // Simple rate limiting — sleep remaining frame time.
        thread::sleep(frame_interval);
    }

    camera.stop_stream().ok();
    Ok(())
}

/// Chunk and send a single MJPEG frame as CAM UDP packets.
/// Format: "CAM"(3) + frame_id(4 BE) + chunk_idx(2 BE) + total_chunks(2 BE) + chunk_data
fn send_camera_frame(udp: &UdpSender, frame_id: u32, jpeg_data: &[u8]) {
    let total_chunks = (jpeg_data.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;
    if total_chunks == 0 {
        return;
    }

    for (chunk_idx, chunk) in jpeg_data.chunks(CHUNK_SIZE).enumerate() {
        let mut packet = Vec::with_capacity(CAM_MAGIC.len() + 4 + 2 + 2 + chunk.len());
        packet.extend_from_slice(CAM_MAGIC);
        packet.extend_from_slice(&frame_id.to_be_bytes());
        packet.extend_from_slice(&(chunk_idx as u16).to_be_bytes());
        packet.extend_from_slice(&(total_chunks as u16).to_be_bytes());
        packet.extend_from_slice(chunk);

        if let Err(e) = udp.send_encrypted(&packet) {
            eprintln!("[webcam] send chunk {chunk_idx}/{total_chunks} failed: {e}");
            break;
        }
    }
}
