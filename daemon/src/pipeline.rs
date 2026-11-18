use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::capture::{self, DisplayId};
use crate::crypto::{ServerPacketSequence, SessionCipher};
use crate::display_manager::{DisplayManager, OutputSettings, Pipeline, TopologyRequest};
use crate::stream_server::{DisplayStreamControl, RequestedSettings, SharedState, UdpSender};

/// Keep the stream's target cadence even if the compositor supplies frames faster than requested.
/// Accumulated deadlines avoid turning 30 -> 24 fps into an accidental 15 fps.
struct FrameRateGate {
    interval: Duration,
    next: Option<Instant>,
}

impl FrameRateGate {
    fn new(fps: u32) -> Self {
        Self {
            interval: Duration::from_nanos(1_000_000_000 / u64::from(fps.clamp(1, 1_000_000_000))),
            next: None,
        }
    }

    fn admit(&mut self, now: Instant, force: bool) -> bool {
        if force || self.next.is_none() {
            self.next = Some(now + self.interval);
            return true;
        }
        let next = self.next.unwrap();
        if now < next {
            return false;
        }
        let steps = (now.duration_since(next).as_nanos() / self.interval.as_nanos())
            .min(u128::from(u32::MAX - 1)) as u32
            + 1;
        self.next = Some(
            next.checked_add(self.interval.saturating_mul(steps))
                .unwrap_or(now + self.interval),
        );
        true
    }
}

pub struct CaptureWorker {
    display: DisplayId,
    settings: OutputSettings,
    shared: Arc<SharedState>,
    control: Arc<DisplayStreamControl>,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    next_sequence: Arc<Mutex<Option<ServerPacketSequence>>>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureWorker {
    fn start(
        socket: UdpSocket,
        shared: Arc<SharedState>,
        session: u64,
        key: [u8; 32],
        display: DisplayId,
        settings: OutputSettings,
        config: &crate::AppConfig,
    ) -> Result<Self> {
        let control = Arc::new(DisplayStreamControl::new(settings.bitrate_bps));
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let next_sequence = Arc::new(Mutex::new(None));
        let mut sender = UdpSender::new(socket, display);
        sender.set_cipher(SessionCipher::new(&key));
        sender.set_source(*shared.udp_source.lock().unwrap());
        sender.set_session(settings.fps);
        control
            .generation
            .store(sender.generation(), Ordering::Release);
        control.force_idr.store(true, Ordering::Relaxed);
        control.force_refresh.store(true, Ordering::Relaxed);
        let mut worker = Self {
            display,
            settings,
            shared: Arc::clone(&shared),
            control: Arc::clone(&control),
            stop: Arc::clone(&stop),
            ready: Arc::clone(&ready),
            error: Arc::clone(&error),
            next_sequence: Arc::clone(&next_sequence),
            thread: None,
        };
        let enc_config = crate::encode::EncoderConfig {
            width: settings.width,
            height: settings.height,
            fps: settings.fps,
            bitrate_bps: settings.bitrate_bps,
            codec: settings.codec,
            backend: config.encoder_backend,
            gop: config.gop,
            intra_refresh: config.intra_refresh,
        };
        worker.thread = Some(
            thread::Builder::new()
                .name(format!("capture-{}", display as u8))
                .spawn(move || {
                    let result = (|| -> Result<()> {
                        let mut encoder = crate::encode::Encoder::new(enc_config)?;
                        let codec = encoder.codec().transport_id();
                        let mut backend =
                            capture::create_display_backend(&capture::CaptureConfig {
                                display_id: display,
                                monitor_name: settings.monitor_name,
                                physical_size_mm: settings.physical_size_mm,
                                width: settings.width,
                                height: settings.height,
                                fps: settings.fps,
                            })?;
                        backend.attach(capture::DisplayMode {
                            width: settings.width,
                            height: settings.height,
                            fps: settings.fps,
                        })?;
                        let mut dump_capture = dump_count("SCREX_DUMP_CAPTURE_FRAMES");
                        let mut dump_au = dump_count("SCREX_DUMP_SENT_AUS");
                        let mut frame_gate = FrameRateGate::new(settings.fps);
                        let mut consecutive_errors = 0;
                        let mut pipeline_error = None;
                        backend.run_capture_loop(&stop, &control.force_refresh, &mut |frame| {
                            if stop.load(Ordering::Relaxed)
                                || !shared.is_current_network_session(session)
                                || !shared.has_active_client.load(Ordering::Acquire)
                            {
                                stop.store(true, Ordering::Release);
                                return;
                            }
                            if let Some(sequence) = next_sequence.lock().unwrap().take() {
                                sender.replace_sequence(sequence);
                                control.force_idr.store(true, Ordering::Relaxed);
                            }
                            if !frame_gate
                                .admit(Instant::now(), control.force_idr.load(Ordering::Relaxed))
                            {
                                return;
                            }
                            if display == DisplayId::Internal && dump_capture > 0 {
                                crate::dump_capture_frame_ppm(dump_capture, &frame);
                                dump_capture -= 1;
                            }
                            let force_idr = control.force_idr.swap(false, Ordering::Relaxed);
                            let timestamp = shared.start_time.elapsed().as_millis() as u32;
                            let tuning = control.tuning();
                            if tuning.bitrate_bps != encoder.bitrate_bps() {
                                if let Err(e) = encoder.reconfigure_bitrate(tuning.bitrate_bps) {
                                    eprintln!(
                                        "[pipeline/{display:?}] encoder retune failed: {e:#}"
                                    );
                                }
                            }
                            let result = (|| -> Result<()> {
                                let units = encoder.encode_frame(&frame, force_idr)?;
                                let addr = *shared.client_addr.lock().unwrap();
                                if let Some(addr) = addr {
                                    if !shared.is_current_network_session(session) {
                                        return Ok(());
                                    }
                                    for unit in &units {
                                        if display == DisplayId::Internal && dump_au > 0 {
                                            crate::dump_sent_access_unit(
                                                dump_au,
                                                codec,
                                                unit.is_idr,
                                                &unit.annex_b,
                                            );
                                            dump_au -= 1;
                                        }
                                        sender.send_frame(unit, addr, timestamp, codec, tuning)?;
                                        if unit.is_idr {
                                            ready.store(true, Ordering::Release);
                                        }
                                    }
                                }
                                Ok(())
                            })();
                            if let Err(e) = result {
                                eprintln!("[pipeline/{display:?}] frame failed: {e:#}");
                                control.force_idr.store(true, Ordering::Relaxed);
                                control.force_refresh.store(true, Ordering::Relaxed);
                                consecutive_errors += 1;
                                if consecutive_errors >= 5 {
                                    pipeline_error = Some(e);
                                    stop.store(true, Ordering::Release);
                                }
                            } else {
                                consecutive_errors = 0;
                            }
                        })?;
                        backend.detach();
                        if let Some(error) = pipeline_error {
                            return Err(error);
                        }
                        Ok(())
                    })();
                    if let Err(e) = result {
                        *error.lock().unwrap() = Some(format!("{e:#}"));
                    }
                })
                .context("spawn display capture worker")?,
        );
        worker
            .shared
            .install_display_stream(display, Arc::clone(&worker.control));
        Ok(worker)
    }
}

impl Pipeline for CaptureWorker {
    fn settings(&self) -> OutputSettings {
        self.settings
    }
    fn generation(&self) -> u32 {
        self.control.generation.load(Ordering::Acquire)
    }
    fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    fn failure(&self) -> Option<String> {
        self.error.lock().unwrap().clone().or_else(|| {
            if self.thread.as_ref().is_some_and(|t| t.is_finished())
                && !self.stop.load(Ordering::Acquire)
            {
                Some("Capture worker stopped unexpectedly".into())
            } else {
                None
            }
        })
    }
    fn resume(&mut self) {
        let sequence = ServerPacketSequence::new();
        self.control
            .generation
            .store(sequence.generation, Ordering::Release);
        self.ready.store(false, Ordering::Release);
        *self.next_sequence.lock().unwrap() = Some(sequence);
        self.shared
            .install_display_stream(self.display, Arc::clone(&self.control));
        self.control.force_idr.store(true, Ordering::Relaxed);
        self.control.force_refresh.store(true, Ordering::Relaxed);
    }
    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                eprintln!("[pipeline/{:?}] capture worker panicked", self.display);
            }
        }
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.signal_stop();
        self.join();
    }
}

fn dump_count(name: &str) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn resolve(request: RequestedSettings, config: &crate::AppConfig) -> TopologyRequest {
    let common = OutputSettings {
        monitor_name: request.internal_display_name.unwrap_or(*b"Screx        "),
        physical_size_mm: request.internal_size_mm,
        width: request.width.unwrap_or(config.width),
        height: request.height.unwrap_or(config.height),
        fps: request.fps.unwrap_or(config.fps),
        codec: request.codec.unwrap_or(config.codec),
        bitrate_bps: request.bitrate_bps.unwrap_or(config.bitrate_bps),
    };
    let external = OutputSettings {
        monitor_name: *b"iPad External",
        // The iPad can't know the size of a display plugged into it.
        physical_size_mm: None,
        width: request.external_width.unwrap_or(config.width),
        height: request.external_height.unwrap_or(config.height),
        fps: request.external_fps.unwrap_or(common.fps),
        ..common
    };
    TopologyRequest {
        revision: request.revision,
        outputs: [
            (request.enabled_mask & 1 != 0).then_some(common),
            (request.enabled_mask & 2 != 0).then_some(external),
        ],
    }
}

/// One default-level line per monitor change; the per-step details are behind `-v`.
fn log_display_status(status: &crate::display_manager::DisplayStatus) {
    use crate::display_manager::DisplayPhase;
    let which = match status.display {
        DisplayId::Internal => "internal",
        DisplayId::External => "external",
    };
    match (status.phase, status.settings) {
        (DisplayPhase::Committed, Some(s)) => println!(
            "[display] {which} monitor \"{}\" {}x{}@{}",
            String::from_utf8_lossy(&s.monitor_name).trim_end(),
            s.width,
            s.height,
            s.fps
        ),
        (DisplayPhase::Removed, _) => crate::log_detail!("[display] {which} monitor off"),
        // Failures are reported once per request (one status per display carries the same text).
        (DisplayPhase::Failed, _) if status.display == DisplayId::Internal => {
            eprintln!("[display] switch failed: {}", status.detail)
        }
        _ => {}
    }
}

pub fn run(
    socket: UdpSocket,
    shared: Arc<SharedState>,
    stop: Arc<AtomicBool>,
    config: crate::AppConfig,
) -> Result<()> {
    let mut session = 0;
    let mut manager = DisplayManager::<CaptureWorker>::new();
    while !stop.load(Ordering::Acquire) {
        let current = shared.network_session_id.load(Ordering::Acquire);
        let connected = shared.has_active_client.load(Ordering::Acquire)
            && shared.is_current_network_session(current);
        if session != current || !connected {
            // Drop signals all workers before joining; none survive into a different session.
            manager = DisplayManager::new();
            session = if connected { current } else { 0 };
        }
        if connected {
            let request = {
                let mut pending = shared.pending_settings.0.lock().unwrap();
                if pending.as_ref().is_some_and(|(id, _)| *id <= session) {
                    pending.take()
                } else {
                    None
                }
            };
            if let Some((id, request)) = request.filter(|(id, _)| *id == session) {
                let key = *shared.session_key.lock().unwrap();
                if let Some(key) = key {
                    let outputs = resolve(request, &config);
                    if let Some(settings) = outputs.outputs[0] {
                        if let Ok(mut input) = shared.input_backend.lock() {
                            input.set_display_size(settings.physical_size_mm);
                        }
                    }
                    let events = manager.request(
                        outputs,
                        std::time::Instant::now(),
                        &mut |display, settings| {
                            CaptureWorker::start(
                                socket.try_clone()?,
                                Arc::clone(&shared),
                                id,
                                key,
                                display,
                                settings,
                                &config,
                            )
                        },
                    );
                    for status in events {
                        log_display_status(&status);
                        shared.queue_display_status(session, status);
                    }
                }
            }
            let acks = crate::stream_server::take_session_messages(
                &mut shared.presentation_acks.lock().unwrap(),
                session,
            );
            for ack in acks {
                manager.acknowledge(ack);
            }
            // Deferred (ordered) plugging and rollbacks also start workers from `tick`.
            let key = *shared.session_key.lock().unwrap();
            let mut spawn = |display, settings| -> Result<CaptureWorker> {
                let key = key.ok_or_else(|| anyhow::anyhow!("session key unavailable"))?;
                CaptureWorker::start(
                    socket.try_clone()?,
                    Arc::clone(&shared),
                    session,
                    key,
                    display,
                    settings,
                    &config,
                )
            };
            for status in manager.tick(std::time::Instant::now(), &mut spawn) {
                if status.display == DisplayId::Internal
                    && matches!(
                        status.phase,
                        crate::display_manager::DisplayPhase::Committed
                            | crate::display_manager::DisplayPhase::Failed
                    )
                {
                    if let Some(settings) = status.settings {
                        if let Ok(mut input) = shared.input_backend.lock() {
                            input.set_stream_size(settings.width, settings.height);
                        }
                    }
                }
                log_display_status(&status);
                shared.queue_display_status(session, status);
            }
        }
        // Also drives timeouts and observes worker failures when there is no incoming control traffic.
        let guard = shared.pending_settings.0.lock().unwrap();
        let _ = shared
            .pending_settings
            .1
            .wait_timeout(guard, Duration::from_millis(20))
            .unwrap();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn frame_gate_preserves_fractional_cadence_and_recovery() {
        let now = Instant::now();
        let mut gate = FrameRateGate::new(24);
        let frames = (0..60)
            .filter(|n| gate.admit(now + Duration::from_nanos(n * 1_000_000_000 / 30), false))
            .count();
        assert_eq!(frames, 48);
        let mut gate = FrameRateGate::new(30);
        assert!(gate.admit(now, false));
        assert!(!gate.admit(now + Duration::from_millis(1), false));
        assert!(
            gate.admit(now + Duration::from_millis(1), true),
            "Recovery keyframes must not be delayed by the FPS gate"
        );
        assert!(!gate.admit(now + Duration::from_millis(2), false));
        assert!(
            gate.admit(now + Duration::from_secs(3600), false),
            "Idle desktops must resume without a burst backlog"
        );
        assert!(!gate.admit(now + Duration::from_secs(3600), false));
    }

    #[test]
    fn independent_resolutions_and_fps_share_codec_and_bitrate() {
        let cli = crate::Cli::try_parse_from(["screx"]).unwrap();
        let config = crate::AppConfig::from_cli(&cli);
        let outputs = resolve(
            RequestedSettings {
                width: Some(1920),
                height: Some(1080),
                external_width: Some(3840),
                external_height: Some(2160),
                external_fps: Some(30),
                internal_display_name: None,
                internal_size_mm: None,
                audio_bitrate_bps: 128_000,
                fps: Some(60),
                codec: Some(crate::encode::VideoCodec::H265),
                bitrate_bps: Some(15_000_000),
                enabled_mask: 3,
                revision: 1,
            },
            &config,
        )
        .outputs;
        let (internal, external) = (outputs[0].unwrap(), outputs[1].unwrap());
        assert_eq!((internal.width, internal.height), (1920, 1080));
        assert_eq!((external.width, external.height), (3840, 2160));
        assert_eq!((internal.fps, external.fps), (60, 30));
        assert_eq!(
            (internal.codec, internal.bitrate_bps),
            (external.codec, external.bitrate_bps)
        );
    }

    #[test]
    fn omitted_external_fps_follows_internal_fps() {
        let config = crate::AppConfig::from_cli(&crate::Cli::try_parse_from(["screx"]).unwrap());
        let topology = resolve(
            RequestedSettings {
                fps: Some(30),
                enabled_mask: 3,
                revision: 1,
                ..Default::default()
            },
            &config,
        );
        assert_eq!(topology.outputs[0].unwrap().fps, 30);
        assert_eq!(topology.outputs[1].unwrap().fps, 30);
    }
}
