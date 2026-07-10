//! macOS audio backend — system-audio (speaker) capture via ScreenCaptureKit.
//!
//! Unlike Linux (PulseAudio null-sink reroute: the host goes silent while a
//! client is connected) macOS has no built-in mechanism to *redirect* system
//! audio into a virtual capture device. Instead we use ScreenCaptureKit's
//! audio tap (`SCStreamConfiguration.capturesAudio`, macOS 13+): it captures
//! a copy of whatever the system is playing while audio keeps playing
//! normally through the Mac's own speakers. That's the accepted v1
//! semantics per the design doc — there is no "virtual sink" to create or
//! destroy, so `create_sink`/`destroy_sink` are cheap (a permission
//! preflight and a no-op, respectively).
//!
//! A dedicated, audio-only `SCStream` is used (a separate `SCContentFilter`
//! + `SCStreamConfiguration` from the display capture path, which uses
//! `CGDisplayStream` and shares nothing with ScreenCaptureKit) so that
//! display attach/detach and audio start/stop stay fully independent, per
//! the design doc. We never register a `.screen` stream output — only
//! `.audio` — so no video frames are delivered or decoded on our side.
//!
//! `run_loopback` mirrors the Linux backend's shape (blocking loop, slices
//! captured audio into fixed 10ms chunks, calls `on_chunk` per chunk, honors
//! `stop`) but the transport is entirely different: ScreenCaptureKit hands
//! us `CMSampleBuffer`s (wrapping an `AudioBufferList` of Float32 PCM,
//! interleaved or planar depending on the OS/hardware — checked at runtime
//! and logged once) on a private dispatch queue instead of a pipe from a
//! subprocess. We convert to interleaved stereo s16le and accumulate into a
//! ring buffer (`Mutex<VecDeque<u8>>` + `Condvar`), emitting exactly
//! `BYTES_PER_CHUNK`-byte chunks to `on_chunk` from the capture thread.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::ffi::NSInteger;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, AudioBuffer, AudioBufferList,
};
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::audio::{AudioBackend, MicSink, BYTES_PER_CHUNK, CHANNELS, SAMPLE_RATE};

pub struct MacAudioBackend;

impl MacAudioBackend {
    pub fn new() -> Self {
        Self
    }
}

impl AudioBackend for MacAudioBackend {
    fn create_sink(&mut self) -> Result<()> {
        // Nothing to create — see module doc. Just confirm the permission
        // ScreenCaptureKit audio capture rides on (Screen Recording) is
        // actually granted, so failures show up here with an actionable
        // message instead of as a cryptic SCStream error later.
        if !CGPreflightScreenCaptureAccess() {
            bail!(
                "Screen Recording permission not granted — grant it to this binary in \
                 System Settings → Privacy & Security → Screen Recording, then restart the daemon"
            );
        }
        Ok(())
    }

    fn destroy_sink(&mut self) {}

    fn run_loopback(&mut self, stop: &AtomicBool, on_chunk: &mut dyn FnMut(&[u8])) -> Result<()> {
        if !CGPreflightScreenCaptureAccess() {
            bail!(
                "Screen Recording permission not granted — grant it to this binary in \
                 System Settings → Privacy & Security → Screen Recording, then restart the daemon"
            );
        }

        let display = fetch_first_display()?;

        // Audio is system-wide once `capturesAudio` is set, but SCStream
        // still requires *some* content filter — any display works as the
        // anchor. We never add a `.screen` stream output, so its window
        // exclusion list (empty) and video settings below are irrelevant to
        // what actually gets delivered to us.
        let excluded = NSArray::<SCWindow>::from_retained_slice(&[]);
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            )
        };

        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setCapturesAudio(true);
            config.setSampleRate(SAMPLE_RATE as NSInteger);
            config.setChannelCount(CHANNELS as NSInteger);
            // Minimize video-side overhead: tiny frame size, ~1fps. We never
            // register a `.screen` stream output so no frames reach us
            // regardless, but SCStream still budgets resources per these.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            config.setQueueDepth(8);
        }

        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };

        let capture_state = Arc::new(AudioCaptureState::new());
        let output_obj = AudioStreamOutput::new(Arc::clone(&capture_state));
        let output_proto = ProtocolObject::from_ref(&*output_obj);

        let sample_queue = DispatchQueue::new("com.screx.daemon.audio.sample", None);

        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    output_proto,
                    SCStreamOutputType::Audio,
                    Some(&sample_queue),
                )
                .map_err(|e| anyhow!("addStreamOutput (audio) failed: {}", nserror_string(&e)))?;
        }

        wait_for_completion(|block| unsafe {
            stream.startCaptureWithCompletionHandler(Some(block));
        })
        .context("SCStream startCapture (audio) failed")?;

        println!("[audio] ScreenCaptureKit audio capture started");

        let mut emit_buf: Vec<Vec<u8>> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let guard = capture_state.buf.lock().unwrap();
            let (mut guard, _timeout) = capture_state
                .cvar
                .wait_timeout_while(guard, Duration::from_millis(200), |b| {
                    b.len() < BYTES_PER_CHUNK && !stop.load(Ordering::Relaxed)
                })
                .unwrap();

            if stop.load(Ordering::Relaxed) {
                break;
            }

            emit_buf.clear();
            while guard.len() >= BYTES_PER_CHUNK {
                emit_buf.push(guard.drain(..BYTES_PER_CHUNK).collect());
            }
            drop(guard);

            for chunk in &emit_buf {
                on_chunk(chunk);
            }
        }

        let stop_result = wait_for_completion(|block| unsafe {
            stream.stopCaptureWithCompletionHandler(Some(block));
        });
        if let Err(e) = stop_result {
            eprintln!("[audio] SCStream stopCapture (audio) error: {e:#}");
        }
        println!("[audio] ScreenCaptureKit audio capture stopped");

        Ok(())
    }

    fn create_mic(&mut self) -> Result<Box<dyn MicSink>> {
        bail!("macOS mic forwarding not yet implemented")
    }

    fn destroy_mic(&mut self) {}
}

// ---------------------------------------------------------------------------
// Shareable-content lookup (async SCK API, bridged to a blocking call)
// ---------------------------------------------------------------------------

/// Thin wrapper allowing a `Retained<T>` (an ObjC object handle) to cross
/// from the completion-handler thread (ScreenCaptureKit invokes these on an
/// internal dispatch queue, not necessarily our calling thread) to the
/// capture thread that's blocked waiting for it. We only ever touch the
/// wrapped object from one thread at a time (constructed on the handler's
/// thread, then handed off exactly once via the Mutex/Condvar rendezvous
/// below), and ObjC retain/release are atomic regardless of the wrapped
/// class's own thread-safety, so the transfer itself is sound even though
/// `Retained<T>` isn't (blanket) `Send` — this is the same reasoning
/// `MacDisplay` relies on for its own `unsafe impl Send`.
struct SendRetained<T>(Retained<T>);
unsafe impl<T> Send for SendRetained<T> {}

fn nserror_string(e: &NSError) -> String {
    format!("{}", unsafe { e.localizedDescription() })
}

/// Fetch `SCShareableContent` (async, completion-handler API) and return its
/// first display, blocking the calling thread until the handler fires.
fn fetch_first_display() -> Result<Retained<SCDisplay>> {
    let pair: Arc<(Mutex<Option<Result<SendRetained<SCDisplay>, String>>>, Condvar)> =
        Arc::new((Mutex::new(None), Condvar::new()));
    let cb_pair = Arc::clone(&pair);

    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result: Result<SendRetained<SCDisplay>, String> = (|| {
                if content.is_null() {
                    let msg = if error.is_null() {
                        "no shareable content and no error".to_string()
                    } else {
                        nserror_string(unsafe { &*error })
                    };
                    return Err(msg);
                }
                // SAFETY: non-null per the completion-handler contract;
                // retain it so it outlives this callback.
                let content = unsafe { Retained::retain(content) }
                    .ok_or_else(|| "SCShareableContent retain failed".to_string())?;
                let displays = unsafe { content.displays() };
                let first = displays
                    .firstObject()
                    .ok_or_else(|| "no displays available to anchor an audio-only SCStream (is this a headless Mac?)".to_string())?;
                Ok(SendRetained(first))
            })();

            let (lock, cvar) = &*cb_pair;
            *lock.lock().unwrap() = Some(result);
            cvar.notify_all();
        },
    );

    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
    }

    let (lock, cvar) = &*pair;
    let mut guard = lock.lock().unwrap();
    while guard.is_none() {
        guard = cvar.wait(guard).unwrap();
    }
    guard
        .take()
        .unwrap()
        .map(|s| s.0)
        .map_err(|e| anyhow!("SCShareableContent fetch failed: {e}"))
}

/// Block the calling thread on an SCK `...CompletionHandler:` call (start
/// capture / stop capture), which reports success via a `NULL` `NSError*`.
fn wait_for_completion(register: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let pair: Arc<(Mutex<Option<Option<String>>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
    let cb_pair = Arc::clone(&pair);

    let block = RcBlock::new(move |error: *mut NSError| {
        let msg = if error.is_null() {
            None
        } else {
            Some(nserror_string(unsafe { &*error }))
        };
        let (lock, cvar) = &*cb_pair;
        *lock.lock().unwrap() = Some(msg);
        cvar.notify_all();
    });

    register(&block);

    let (lock, cvar) = &*pair;
    let mut guard = lock.lock().unwrap();
    while guard.is_none() {
        guard = cvar.wait(guard).unwrap();
    }
    match guard.take().unwrap() {
        None => Ok(()),
        Some(msg) => Err(anyhow!(msg)),
    }
}

// ---------------------------------------------------------------------------
// Sample buffer -> interleaved stereo s16le conversion
// ---------------------------------------------------------------------------

struct AudioCaptureState {
    buf: Mutex<VecDeque<u8>>,
    cvar: Condvar,
    format_logged: AtomicBool,
    unsupported_format_logged: AtomicBool,
}

impl AudioCaptureState {
    fn new() -> Self {
        Self {
            buf: Mutex::new(VecDeque::new()),
            cvar: Condvar::new(),
            format_logged: AtomicBool::new(false),
            unsupported_format_logged: AtomicBool::new(false),
        }
    }

    fn push_stereo_samples(&self, samples: &[i16]) {
        let mut buf = self.buf.lock().unwrap();
        for s in samples {
            buf.extend(s.to_le_bytes());
        }
        drop(buf);
        self.cvar.notify_all();
    }
}

#[inline]
fn f32_to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// Convert one ScreenCaptureKit audio `CMSampleBuffer` to interleaved stereo
/// s16le and push it into `state`'s ring buffer.
fn handle_sample_buffer(state: &AudioCaptureState, sample_buffer: &objc2_core_media::CMSampleBuffer) {
    let num_frames = unsafe { sample_buffer.num_samples() } as usize;
    if num_frames == 0 {
        return;
    }

    let format_desc = match unsafe { sample_buffer.format_description() } {
        Some(fd) => fd,
        None => return,
    };
    let asbd_ptr = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&format_desc) };
    if asbd_ptr.is_null() {
        return;
    }
    // SAFETY: non-null per the API contract above, valid for as long as
    // `format_desc` is alive (it is, for the remainder of this function).
    let asbd = unsafe { *asbd_ptr };

    let is_float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
    let is_noninterleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;

    if !state.format_logged.swap(true, Ordering::SeqCst) {
        println!(
            "[audio] SCK format detected: sampleRate={} channels={} bitsPerChannel={} \
             flags=0x{:x} (float={is_float} nonInterleaved={is_noninterleaved})",
            asbd.mSampleRate, asbd.mChannelsPerFrame, asbd.mBitsPerChannel, asbd.mFormatFlags,
        );
    }

    if !is_float {
        if !state.unsupported_format_logged.swap(true, Ordering::SeqCst) {
            eprintln!(
                "[audio] unsupported SCK audio format (not Float32, flags=0x{:x}) — dropping buffers",
                asbd.mFormatFlags
            );
        }
        return;
    }

    // Query the AudioBufferList size, then allocate an 8-byte-aligned
    // buffer of that size (AudioBufferList's trailing `mBuffers` field is a
    // C "flexible array member" idiom — `Vec<u64>` guarantees the alignment
    // AudioBuffer's pointer field needs, sized generously in units of 8
    // bytes) and fetch the real list into it.
    let mut needed_size: usize = 0;
    let status = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            &mut needed_size,
            std::ptr::null_mut(),
            0,
            None,
            None,
            0,
            std::ptr::null_mut(),
        )
    };
    if status != 0 || needed_size == 0 {
        return;
    }

    let mut storage: Vec<u64> = vec![0u64; needed_size.div_ceil(8)];
    let buffer_list_ptr = storage.as_mut_ptr() as *mut AudioBufferList;
    let mut block_buffer: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = unsafe {
        sample_buffer.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            buffer_list_ptr,
            needed_size,
            None,
            None,
            0,
            &mut block_buffer,
        )
    };
    if status != 0 {
        return;
    }
    // Own the +1 retained block buffer for the duration of this function —
    // it backs the sample data that `abl`'s `mData` pointers point into.
    let _owned_block_buffer =
        NonNull::new(block_buffer).map(|nn| unsafe { objc2_core_foundation::CFRetained::from_raw(nn) });

    // SAFETY: `buffer_list_ptr` points into `storage`, which we just
    // populated via the call above and which is still alive.
    let abl: &AudioBufferList = unsafe { &*buffer_list_ptr };
    let num_buffers = abl.mNumberBuffers as usize;
    if num_buffers == 0 {
        return;
    }
    // SAFETY: `storage` was sized to hold `mNumberBuffers` `AudioBuffer`
    // entries (that's exactly what `needed_size` from the first call
    // accounts for), so reading `num_buffers` entries from the trailing
    // array is in-bounds even though the Rust type only declares one.
    let buffers: &[AudioBuffer] =
        unsafe { std::slice::from_raw_parts(abl.mBuffers.as_ptr(), num_buffers) };

    let mut stereo = vec![0i16; num_frames * 2];

    if num_buffers == 1 {
        // Interleaved: one buffer holding all channels per frame.
        let buf0 = &buffers[0];
        let ch = (buf0.mNumberChannels as usize).max(1);
        let available = (buf0.mDataByteSize as usize / 4 / ch).min(num_frames);
        // SAFETY: `mData` is valid for `mDataByteSize` bytes per the
        // AudioBufferList contract; we only read `available` frames.
        let data =
            unsafe { std::slice::from_raw_parts(buf0.mData as *const f32, available * ch) };
        for i in 0..available {
            if ch >= 2 {
                stereo[i * 2] = f32_to_i16(data[i * ch]);
                stereo[i * 2 + 1] = f32_to_i16(data[i * ch + 1]);
            } else {
                let s = f32_to_i16(data[i]);
                stereo[i * 2] = s;
                stereo[i * 2 + 1] = s;
            }
        }
    } else {
        // Planar: one buffer per channel (typically mono each).
        let mut chans: Vec<&[f32]> = Vec::with_capacity(num_buffers);
        for b in buffers {
            let n = (b.mDataByteSize as usize / 4).min(num_frames);
            // SAFETY: same contract as above, per-buffer.
            chans.push(unsafe { std::slice::from_raw_parts(b.mData as *const f32, n) });
        }
        for i in 0..num_frames {
            let l = chans.first().and_then(|c| c.get(i)).copied().unwrap_or(0.0);
            let r = chans.get(1).and_then(|c| c.get(i)).copied().unwrap_or(l);
            stereo[i * 2] = f32_to_i16(l);
            stereo[i * 2 + 1] = f32_to_i16(r);
        }
    }

    state.push_stereo_samples(&stereo);
}

// ---------------------------------------------------------------------------
// SCStreamOutput delegate object
// ---------------------------------------------------------------------------

pub struct AudioOutputIvars {
    state: Arc<AudioCaptureState>,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `AudioStreamOutput` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = AudioOutputIvars]
    struct AudioStreamOutput;

    unsafe impl NSObjectProtocol for AudioStreamOutput {}

    unsafe impl SCStreamOutput for AudioStreamOutput {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &objc2_core_media::CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Audio {
                return;
            }
            let state = Arc::clone(&self.ivars().state);
            // This is invoked by the ObjC runtime on a GCD queue; a Rust
            // panic unwinding through that frame is UB, so contain it.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                handle_sample_buffer(&state, sample_buffer);
            }));
        }
    }
);

impl AudioStreamOutput {
    fn new(state: Arc<AudioCaptureState>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AudioOutputIvars { state });
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Manual smoke test exercising the real ScreenCaptureKit audio path:
    /// starts loopback capture, plays audible system sound so captured
    /// samples are provably non-silent, counts/validates chunks for ~3s,
    /// then stops. Ignored by default since it requires Screen Recording
    /// permission granted to the test binary and makes noise.
    ///
    /// Run manually with:
    ///   cargo test --release -- --ignored macos_audio_smoke --nocapture
    #[test]
    #[ignore]
    fn macos_audio_smoke() {
        let mut backend = MacAudioBackend::new();
        backend
            .create_sink()
            .expect("create_sink failed (Screen Recording permission?)");

        // Make some noise so we can tell captured samples aren't just
        // silence. Fire once immediately and again mid-capture.
        let _ = std::process::Command::new("afplay")
            .arg("/System/Library/Sounds/Ping.aiff")
            .spawn();
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(800));
            let _ = std::process::Command::new("say")
                .arg("testing screx audio capture")
                .status();
        });

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            stop_thread.store(true, Ordering::Relaxed);
        });

        let mut chunk_count = 0usize;
        let mut bad_size_count = 0usize;
        let mut any_nonzero = false;
        let mut max_peak = 0i16;

        let start = Instant::now();
        backend
            .run_loopback(&stop, &mut |chunk: &[u8]| {
                chunk_count += 1;
                if chunk.len() != BYTES_PER_CHUNK {
                    bad_size_count += 1;
                    return;
                }
                let mut peak = 0i16;
                let mut sum_sq: f64 = 0.0;
                for s in chunk.chunks_exact(2) {
                    let v = i16::from_le_bytes([s[0], s[1]]);
                    if v != 0 {
                        any_nonzero = true;
                    }
                    peak = peak.max(v.abs());
                    sum_sq += (v as f64) * (v as f64);
                }
                max_peak = max_peak.max(peak);
                if chunk_count % 50 == 0 {
                    let rms = (sum_sq / (chunk.len() as f64 / 2.0)).sqrt();
                    println!("chunk {chunk_count}: peak={peak} rms={rms:.1}");
                }
            })
            .expect("run_loopback failed");

        println!(
            "captured {chunk_count} chunks in {:?} (bad_size={bad_size_count}, any_nonzero={any_nonzero}, max_peak={max_peak})",
            start.elapsed()
        );
        assert!(chunk_count > 0, "no audio chunks captured");
        assert_eq!(bad_size_count, 0, "some chunks had the wrong byte length");
        assert!(any_nonzero, "all captured samples were silent");
    }
}
