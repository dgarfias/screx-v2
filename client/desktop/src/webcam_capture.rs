//! Webcam capture adapter — captures frames from the local webcam,
//! compresses to JPEG, chunks, and sends as CAM UDP packets.
//!
//! On macOS the AVFoundation backend cannot deliver MJPEG directly,
//! so we request YUYV, decode to RGB via nokhwa, then JPEG-encode
//! in software before sending.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::Camera;

use crate::backend::UdpSender;

const CAM_MAGIC: &[u8] = b"CAM";
const CHUNK_SIZE: usize = 1300;
/// JPEG quality (1-100). 80 is a good balance of quality vs size.
const JPEG_QUALITY: u8 = 80;

/// Request camera permission on macOS via AVFoundation.
/// Blocks until the user grants or denies.
/// Returns `true` if permission was granted.
#[cfg(target_os = "macos")]
fn ensure_camera_permission() -> bool {
    use std::sync::{Condvar, Mutex};

    // If already authorized, skip the dialog.
    if nokhwa::nokhwa_check() {
        return true;
    }

    let pair = Arc::new((Mutex::new(None::<bool>), Condvar::new()));
    let pair2 = Arc::clone(&pair);

    nokhwa::nokhwa_initialize(move |granted| {
        let (lock, cvar) = &*pair2;
        let mut result = lock.lock().unwrap();
        *result = Some(granted);
        cvar.notify_one();
    });

    let (lock, cvar) = &*pair;
    let mut result = lock.lock().unwrap();
    // Wait up to 60 seconds for the user to respond to the dialog.
    while result.is_none() {
        let (guard, timeout) = cvar.wait_timeout(result, Duration::from_secs(60)).unwrap();
        result = guard;
        if timeout.timed_out() {
            return false;
        }
    }
    result.unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn ensure_camera_permission() -> bool {
    true
}

/// Encode an RGB image buffer to JPEG bytes.
fn encode_jpeg(width: u32, height: u32, rgb_data: &[u8]) -> Result<Vec<u8>> {
    use image::codecs::jpeg::JpegEncoder;
    use image::{ExtendedColorType, ImageEncoder};

    let mut jpeg_buf = Cursor::new(Vec::with_capacity(width as usize * height as usize));
    let encoder = JpegEncoder::new_with_quality(&mut jpeg_buf, JPEG_QUALITY);
    encoder
        .write_image(rgb_data, width, height, ExtendedColorType::Rgb8)
        .context("JPEG encode failed")?;
    Ok(jpeg_buf.into_inner())
}

/// Handle to the running webcam capture.
pub struct WebcamCapture {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl WebcamCapture {
    /// Start webcam capture at the given resolution/fps.
    /// Frames are sent as CAM UDP packets via `udp`.
    ///
    /// Blocks until camera permission is confirmed and the device is
    /// successfully opened, so errors propagate to the caller.
    pub fn start(udp: Arc<UdpSender>, width: u32, height: u32, fps: u32) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        // Use a channel so the spawned thread can report back whether
        // camera init succeeded before we return to the caller.
        let (init_tx, init_rx) = mpsc::channel::<Result<()>>();

        let thread = thread::spawn(move || {
            // Permission check + camera open happen here (on the capture thread)
            // because nokhwa::Camera may not be Send.
            let init_result = (|| -> Result<Camera> {
                if !ensure_camera_permission() {
                    bail!("Camera permission was denied by the user");
                }

                // Request YUYV — AVFoundation can deliver this natively.
                // On Linux/Windows with V4L2/MSMF, MJPEG would also work,
                // but YUYV is universally safe across all backends.
                let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
                    CameraFormat::new(Resolution::new(width, height), FrameFormat::YUYV, fps),
                ));

                let mut camera =
                    Camera::new(CameraIndex::Index(0), requested).context("open webcam")?;

                eprintln!(
                    "[webcam] opened camera: {}x{} @ {} fps, format={:?}",
                    camera.camera_format().resolution().width(),
                    camera.camera_format().resolution().height(),
                    camera.camera_format().frame_rate(),
                    camera.camera_format().format(),
                );

                camera.open_stream().context("start webcam stream")?;
                Ok(camera)
            })();

            match init_result {
                Ok(camera) => {
                    // Signal success to the caller.
                    let _ = init_tx.send(Ok(()));
                    // Run the capture loop (blocks until stop flag).
                    let frame_interval =
                        Duration::from_millis(if fps > 0 { 1000 / fps as u64 } else { 33 });
                    capture_loop(camera, udp, frame_interval, stop_flag);
                }
                Err(e) => {
                    // Signal failure to the caller.
                    let _ = init_tx.send(Err(e));
                }
            }
        });

        // Wait for the capture thread to finish initialization.
        // Timeout after 90 seconds (covers the macOS permission dialog wait).
        match init_rx.recv_timeout(Duration::from_secs(90)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                // Init failed — join the thread (it already exited).
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // Timed out waiting for init.
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                bail!("Webcam initialization timed out")
            }
        }
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
    mut camera: Camera,
    udp: Arc<UdpSender>,
    frame_interval: Duration,
    stop: Arc<AtomicBool>,
) {
    let mut frame_id: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        // Get a frame from the camera as a Buffer (raw bytes + format metadata).
        match camera.frame() {
            Ok(buffer) => {
                // Decode the raw camera data (YUYV/NV12/whatever) to an RGB ImageBuffer.
                match buffer.decode_image::<RgbFormat>() {
                    Ok(rgb_image) => {
                        let w = rgb_image.width();
                        let h = rgb_image.height();
                        let rgb_data = rgb_image.into_raw();

                        // Encode the RGB pixels to JPEG.
                        match encode_jpeg(w, h, &rgb_data) {
                            Ok(jpeg_data) => {
                                send_camera_frame(&udp, frame_id, &jpeg_data);
                                frame_id = frame_id.wrapping_add(1);
                            }
                            Err(e) => {
                                eprintln!("[webcam] JPEG encode error: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[webcam] frame decode error: {e}");
                    }
                }
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
}

/// Chunk and send a single JPEG frame as CAM UDP packets.
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
