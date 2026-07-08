use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole,
    IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE,
    DEVICE_STATEMASK_ALL, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::audio::{AudioBackend, MicSink, BYTES_PER_CHUNK, CHANNELS, SAMPLE_RATE};
use crate::platform::windows::{audio_driver, policy_config};

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
const SUBTYPE_IEEE_FLOAT: windows::core::GUID = windows::core::GUID::from_values(
    0x00000003,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71],
);

const REFTIMES_PER_MILLISEC: i64 = 10_000;

pub struct WasapiBackend;

impl WasapiBackend {
    pub fn new() -> Self {
        Self
    }
}

pub fn cleanup_stale_audio_endpoints() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    for (flow, names, label) in [
        (eRender, VB_CABLE_RENDER_NAMES, "VB-CABLE playback"),
        (eCapture, VB_CABLE_CAPTURE_NAMES, "VB-CABLE capture"),
    ] {
        if let Ok(device) = find_vbcable_endpoint(flow, names, DEVICE_STATE(DEVICE_STATEMASK_ALL)) {
            if let Ok(id) = unsafe { policy_config::device_id_string(&device) } {
                if let Err(e) = policy_config::set_endpoint_visibility(&id, true) {
                    eprintln!("[mic] failed to restore {label} endpoint visibility: {e:#}");
                }
            }
        }
    }
}

/// The default render endpoint (per role) that was active before we made the
/// Steam Streaming Speakers device the default, so it can be restored on
/// disable. `create_virtual_sink()`/`remove_virtual_sink()` construct a fresh
/// `WasapiBackend` on every call (see audio.rs), so this can't live on
/// `self` — it has to be a process-wide static instead.
struct SavedDefaults {
    console: Option<String>,
    multimedia: Option<String>,
    communications: Option<String>,
}

static PREVIOUS_DEFAULT: Mutex<Option<SavedDefaults>> = Mutex::new(None);

impl AudioBackend for WasapiBackend {
    fn create_sink(&mut self) -> Result<()> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        if let Err(e) = audio_driver::ensure_installed_and_enabled() {
            eprintln!(
                "[audio] Steam Streaming Speakers unavailable ({e:#}); \
                 speaker passthrough will use whatever is currently the default output"
            );
            return Ok(());
        }

        let device = policy_config::poll_for_endpoint(
            audio_driver::STEAM_SPK_FRIENDLY_NAME,
            Duration::from_secs(5),
        )
        .context("Steam Streaming Speakers endpoint did not become active")?;
        let device_id = unsafe {
            let pwstr = device.GetId().context("IMMDevice::GetId failed")?;
            let s = pwstr.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.as_ptr() as *const _));
            s
        };

        {
            let mut saved = PREVIOUS_DEFAULT.lock().unwrap();
            if saved.is_none() {
                *saved = Some(SavedDefaults {
                    console: policy_config::current_default_endpoint_id(eConsole).ok(),
                    multimedia: policy_config::current_default_endpoint_id(eMultimedia).ok(),
                    communications: policy_config::current_default_endpoint_id(eCommunications)
                        .ok(),
                });
            }
        }

        policy_config::set_default_endpoint(&device_id, &[eConsole, eMultimedia, eCommunications])
            .context("failed to make Steam Streaming Speakers the default output")?;
        println!("[audio] Steam Streaming Speakers is now the default output");
        Ok(())
    }

    fn destroy_sink(&mut self) {
        if let Some(saved) = PREVIOUS_DEFAULT.lock().unwrap().take() {
            let restores: [(ERole, Option<String>); 3] = [
                (eConsole, saved.console),
                (eMultimedia, saved.multimedia),
                (eCommunications, saved.communications),
            ];
            for (role, id) in restores {
                if let Some(id) = id {
                    if let Err(e) = policy_config::set_default_endpoint(&id, &[role]) {
                        eprintln!("[audio] failed to restore previous default endpoint for role {role:?}: {e:#}");
                    }
                }
            }
        }

        // Do not disable the devnode here. The iPad intentionally sends quick
        // SPKR off/on resets around audio-route changes; cycling a Windows
        // audio devnode during that reset races WASAPI and can leave speaker
        // passthrough broken. Restoring the previous default output is enough
        // for "off" semantics; the installed signed endpoint can remain enabled.
    }

    fn run_loopback(&mut self, stop: &AtomicBool, on_chunk: &mut dyn FnMut(&[u8])) -> Result<()> {
        unsafe { run_wasapi_loopback(stop, on_chunk) }
    }

    fn create_mic(&mut self) -> Result<Box<dyn MicSink>> {
        unsafe { create_vbcable_mic() }
    }

    fn destroy_mic(&mut self) {}
}

/// Cheap capability probe: true if the Steam Streaming Speakers endpoint is
/// currently enumerable. Mirrors the lookup `run_wasapi_loopback` performs
/// before falling back to the plain default output device — does not open
/// the endpoint, just checks it exists.
pub fn probe_speaker_available() -> bool {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    policy_config::find_endpoint_by_friendly_name(audio_driver::STEAM_SPK_FRIENDLY_NAME).is_ok()
}

unsafe fn run_wasapi_loopback(stop: &AtomicBool, on_chunk: &mut dyn FnMut(&[u8])) -> Result<()> {
    // COM may already be initialized on this thread; ignore that error.
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .context("CoCreateInstance(MMDeviceEnumerator) failed")?;

    // Target the Steam Streaming Speakers endpoint by name rather than
    // "whatever is default" so loopback capture keeps working even if
    // something else changes the system default mid-session. Falls back to
    // the plain default endpoint if the driver isn't available.
    let device: IMMDevice = match policy_config::find_endpoint_by_friendly_name(
        audio_driver::STEAM_SPK_FRIENDLY_NAME,
    ) {
        Ok(d) => d,
        Err(e) => {
            println!("[audio] Steam Streaming Speakers not found ({e:#}); loopback-capturing current default instead");
            enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("GetDefaultAudioEndpoint failed")?
        }
    };

    let client: IAudioClient = device
        .Activate::<IAudioClient>(CLSCTX_ALL, None)
        .context("Activate(IAudioClient) failed")?;

    let mix_format_ptr = client.GetMixFormat().context("GetMixFormat failed")?;
    let (input_channels, input_rate, bits_per_sample, format_tag) =
        read_wave_format(mix_format_ptr);
    let is_float = is_float_format(mix_format_ptr);

    println!(
        "[audio] WASAPI mix format: {} channels, {} Hz, {} bits, tag=0x{:04x}",
        input_channels, input_rate, bits_per_sample, format_tag
    );

    // We only support 48 kHz for now; resampling is left as a future improvement.
    if input_rate != SAMPLE_RATE {
        bail!("WASAPI mix format sample rate {input_rate} is not {SAMPLE_RATE}; resampling not implemented");
    }
    if input_channels != CHANNELS {
        bail!("WASAPI mix format channels {input_channels} is not {CHANNELS}; channel conversion not implemented");
    }

    let buffer_duration = REFTIMES_PER_MILLISEC * 10;
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            buffer_duration,
            0,
            mix_format_ptr,
            None,
        )
        .context("IAudioClient::Initialize failed")?;

    let capture_client: IAudioCaptureClient = client
        .GetService()
        .context("GetService(IAudioCaptureClient) failed")?;

    client.Start().context("IAudioClient::Start failed")?;

    let mut accum = Vec::<u8>::with_capacity(BYTES_PER_CHUNK * 2);

    while !stop.load(Ordering::Relaxed) {
        let packet_length = match capture_client.GetNextPacketSize() {
            Ok(n) => n,
            Err(e) => {
                let _ = client.Stop();
                bail!("GetNextPacketSize failed: {e}");
            }
        };

        if packet_length == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }

        let mut data_ptr = ptr::null_mut();
        let mut flags = 0u32;
        let mut frames_available = 0u32;
        if let Err(e) =
            capture_client.GetBuffer(&mut data_ptr, &mut frames_available, &mut flags, None, None)
        {
            let _ = client.Stop();
            bail!("GetBuffer failed: {e}");
        }

        if !data_ptr.is_null() && frames_available > 0 {
            let byte_count = (frames_available as usize)
                * (input_channels as usize)
                * ((bits_per_sample as usize) / 8);

            if is_float {
                let samples = frames_available as usize * input_channels as usize;
                let floats = std::slice::from_raw_parts(data_ptr as *const f32, samples);
                for &sample in floats {
                    let s16 = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    accum.extend_from_slice(&s16.to_le_bytes());
                }
            } else {
                let input_slice = std::slice::from_raw_parts(data_ptr as *const u8, byte_count);
                accum.extend_from_slice(input_slice);
            }

            let _ = capture_client.ReleaseBuffer(frames_available);

            while accum.len() >= BYTES_PER_CHUNK {
                on_chunk(&accum[..BYTES_PER_CHUNK]);
                accum.drain(..BYTES_PER_CHUNK);
            }
        } else {
            let _ = capture_client.ReleaseBuffer(0);
        }
    }

    let _ = client.Stop();
    // mix_format_ptr was allocated by GetMixFormat; Windows docs say caller must
    // CoTaskMemFree it. The windows-rs crate does not expose CoTaskMemFree, so
    // we leak a small allocation on shutdown.

    Ok(())
}

unsafe fn read_wave_format(ptr: *const WAVEFORMATEX) -> (u16, u32, u16, u16) {
    (
        std::ptr::addr_of!((*ptr).nChannels).read_unaligned(),
        std::ptr::addr_of!((*ptr).nSamplesPerSec).read_unaligned(),
        std::ptr::addr_of!((*ptr).wBitsPerSample).read_unaligned(),
        std::ptr::addr_of!((*ptr).wFormatTag).read_unaligned(),
    )
}

unsafe fn is_float_format(fmt_ptr: *const WAVEFORMATEX) -> bool {
    let tag = std::ptr::addr_of!((*fmt_ptr).wFormatTag).read_unaligned();
    if tag == WAVE_FORMAT_IEEE_FLOAT {
        return true;
    }
    if tag == WAVE_FORMAT_EXTENSIBLE {
        let sub_format = std::ptr::addr_of!((*(fmt_ptr as *const WAVEFORMATEXTENSIBLE)).SubFormat)
            .read_unaligned();
        return sub_format == SUBTYPE_IEEE_FLOAT;
    }
    false
}

// ---------------------------------------------------------------------------
// VB-Audio VB-CABLE virtual microphone
// ---------------------------------------------------------------------------

const VB_CABLE_RENDER_NAMES: &[&str] = &[
    "CABLE Input",
    "CABLE In",
    "Speakers (VB-Audio Virtual Cable)",
    "VB-Audio Virtual Cable",
];
const VB_CABLE_CAPTURE_NAMES: &[&str] = &["CABLE Output", "VB-Audio Virtual Cable"];

struct VbCableMicSink {
    sender: Option<mpsc::SyncSender<Vec<i16>>>,
    stop: Arc<AtomicBool>,
    writes: u64,
    thread: Option<std::thread::JoinHandle<()>>,
}

enum RenderStartResult {
    Started,
    Failed(String),
}

impl Drop for VbCableMicSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        drop(self.sender.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl MicSink for VbCableMicSink {
    fn write_pcm(&mut self, s16_mono_48k: &[i16]) {
        if let Some(ref sender) = self.sender {
            self.writes = self.writes.wrapping_add(1);
            // Drop packets if the renderer can't keep up; better a short glitch
            // than an unbounded backlog.
            if sender.try_send(s16_mono_48k.to_vec()).is_err() && self.writes % 50 == 0 {
                eprintln!("[mic] render queue full; dropping PCM packet");
            }
        }
    }
}

/// Cheap capability probe: true if a VB-CABLE capture endpoint is currently
/// enumerable. Mirrors the lookup `create_vbcable_mic` performs before it
/// actually activates the endpoint — does not open the device.
pub fn probe_mic_available() -> bool {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    find_vbcable_endpoint(eCapture, VB_CABLE_CAPTURE_NAMES, DEVICE_STATE_ACTIVE).is_ok()
}

unsafe fn create_vbcable_mic() -> Result<Box<dyn MicSink>> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    // 1. Locate the recording side of the cable so users can select it.
    let _capture_device =
        find_vbcable_endpoint(eCapture, VB_CABLE_CAPTURE_NAMES, DEVICE_STATE_ACTIVE)
            .context("VB-CABLE capture endpoint not found; install VB-Audio VB-CABLE first")?;

    // 2. Locate the playback side that we feed with PCM.
    let render_device = find_vbcable_endpoint(eRender, VB_CABLE_RENDER_NAMES, DEVICE_STATE_ACTIVE)
        .context("VB-CABLE render endpoint not found")?;
    let render_device_id = policy_config::device_id_string(&render_device)
        .context("failed to read VB-CABLE render device id")?;

    // 3. Spawn the render thread.
    let (sender, receiver) = mpsc::sync_channel(200);
    let (start_tx, start_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let render_thread_device_id = render_device_id.clone();
    let thread = std::thread::spawn(move || {
        if let Err(e) =
            vbcable_render_thread(receiver, thread_stop, render_thread_device_id, start_tx)
        {
            eprintln!("[mic] render thread exited: {e:#}");
        }
    });

    match start_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(RenderStartResult::Started) => {}
        Ok(RenderStartResult::Failed(e)) => {
            stop.store(true, Ordering::SeqCst);
            let _ = thread.join();
            bail!("VB-CABLE render thread failed to start: {e}");
        }
        Err(e) => {
            stop.store(true, Ordering::SeqCst);
            let _ = thread.join();
            bail!("VB-CABLE render thread did not start: {e}");
        }
    }

    Ok(Box::new(VbCableMicSink {
        sender: Some(sender),
        stop,
        writes: 0,
        thread: Some(thread),
    }))
}

fn find_vbcable_endpoint(
    flow: EDataFlow,
    names: &[&str],
    state_mask: DEVICE_STATE,
) -> Result<IMMDevice> {
    let mut last_err = None;
    for name in names {
        match policy_config::find_endpoint_by_friendly_name_flow(flow, name, state_mask) {
            Ok(device) => return Ok(device),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no VB-CABLE endpoint names configured")))
}

struct RenderFormat {
    channels: u16,
    rate: u32,
    bits: u16,
    is_float: bool,
    bytes_per_sample: u16,
}

unsafe fn vbcable_render_thread(
    receiver: mpsc::Receiver<Vec<i16>>,
    stop: Arc<AtomicBool>,
    device_id: String,
    start_tx: mpsc::Sender<RenderStartResult>,
) -> Result<()> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .context("CoCreateInstance(MMDeviceEnumerator) failed")?;
    let device_id_w: Vec<u16> = device_id.encode_utf16().chain(Some(0)).collect();
    let device = enumerator
        .GetDevice(windows::core::PCWSTR(device_id_w.as_ptr()))
        .context("GetDevice for VB-CABLE playback endpoint failed")?;

    let client: IAudioClient = match device.Activate::<IAudioClient>(CLSCTX_ALL, None) {
        Ok(client) => client,
        Err(e) => {
            let _ = start_tx.send(RenderStartResult::Failed(format!(
                "Activate(IAudioClient) on VB-CABLE playback endpoint failed: {e}"
            )));
            return Err(e.into());
        }
    };

    // Try our preferred format first: 48 kHz, stereo, 16-bit PCM.
    let preferred_format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: 2,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 2 * 2,
        nBlockAlign: 2 * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };

    let buffer_duration = REFTIMES_PER_MILLISEC * 10;
    let init_ok = client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            0,
            buffer_duration,
            0,
            &preferred_format,
            None,
        )
        .is_ok();

    let format = if init_ok {
        println!("[mic] VB-CABLE initialized with preferred 48kHz stereo s16");
        RenderFormat {
            channels: 2,
            rate: SAMPLE_RATE,
            bits: 16,
            is_float: false,
            bytes_per_sample: 2,
        }
    } else {
        println!("[mic] preferred format rejected, falling back to mix format");
        let mix_format_ptr = client.GetMixFormat().context("GetMixFormat failed")?;
        let fmt = read_render_format(mix_format_ptr);
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                buffer_duration,
                0,
                mix_format_ptr,
                None,
            )
            .context("Initialize with mix format failed")?;
        if fmt.rate != SAMPLE_RATE {
            bail!(
                "VB-CABLE mix format sample rate {} is not {}; resampling not implemented",
                fmt.rate,
                SAMPLE_RATE
            );
        }
        println!(
            "[mic] VB-CABLE mix format: {} channels, {} Hz, {} bits, float={}",
            fmt.channels, fmt.rate, fmt.bits, fmt.is_float
        );
        fmt
    };

    let buffer_size = client.GetBufferSize().context("GetBufferSize failed")?;
    let render_client: IAudioRenderClient = client
        .GetService()
        .context("GetService(IAudioRenderClient) failed")?;

    if let Err(e) = client.Start() {
        let _ = start_tx.send(RenderStartResult::Failed(format!(
            "IAudioClient::Start failed: {e}"
        )));
        return Err(e.into());
    }

    let _ = start_tx.send(RenderStartResult::Started);

    let mut queue: VecDeque<i16> = VecDeque::with_capacity(SAMPLE_RATE as usize);

    while !stop.load(Ordering::Relaxed) {
        // Drain any newly decoded packets into the sample queue.
        while let Ok(packet) = receiver.try_recv() {
            queue.extend(packet);
        }

        let padding = client
            .GetCurrentPadding()
            .context("GetCurrentPadding failed")?;
        let frames_available = buffer_size.saturating_sub(padding);
        if frames_available == 0 {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }

        let data_ptr = render_client
            .GetBuffer(frames_available)
            .context("GetBuffer failed")?;

        write_interleaved_s16(data_ptr, frames_available, &format, &mut queue);

        render_client
            .ReleaseBuffer(frames_available, 0)
            .context("ReleaseBuffer failed")?;

        std::thread::sleep(Duration::from_millis(1));
    }

    let _ = client.Stop();
    Ok(())
}

unsafe fn read_render_format(ptr: *const WAVEFORMATEX) -> RenderFormat {
    let channels = std::ptr::addr_of!((*ptr).nChannels).read_unaligned();
    let rate = std::ptr::addr_of!((*ptr).nSamplesPerSec).read_unaligned();
    let bits = std::ptr::addr_of!((*ptr).wBitsPerSample).read_unaligned();
    let is_float = is_float_format(ptr);
    let bytes_per_sample = if is_float { 4 } else { (bits / 8).max(2) } as u16;
    RenderFormat {
        channels,
        rate,
        bits,
        is_float,
        bytes_per_sample,
    }
}

unsafe fn write_interleaved_s16(
    data: *mut u8,
    frames: u32,
    format: &RenderFormat,
    queue: &mut VecDeque<i16>,
) {
    let channels = format.channels as usize;
    let bytes_per_sample = format.bytes_per_sample as usize;
    let frame_bytes = channels * bytes_per_sample;
    let total_bytes = frames as usize * frame_bytes;
    let buf = std::slice::from_raw_parts_mut(data, total_bytes);

    for frame in 0..frames as usize {
        let mono = queue.pop_front().unwrap_or(0i16);
        for ch in 0..channels {
            // Duplicate mono to left/right; zero-fill any extra channels.
            let val = if ch < 2 { mono } else { 0i16 };
            let offset = frame * frame_bytes + ch * bytes_per_sample;
            if format.is_float {
                let f = (val as f32 / 32768.0).clamp(-1.0, 1.0);
                buf[offset..offset + 4].copy_from_slice(&f.to_le_bytes());
            } else {
                buf[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
            }
        }
    }
}
