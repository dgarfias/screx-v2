use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

#[cfg(not(feature = "real-capture"))]
use std::sync::atomic::Ordering;
#[cfg(not(feature = "real-capture"))]
use std::time::Duration;

use anyhow::Result;

#[derive(Debug, Clone, Copy)]
pub enum PixelFormat {
    Bgra8888,
}

#[derive(Debug)]
pub struct CaptureFrame<'a> {
    pub frame_index: u64,
    pub timestamp_90k: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: &'a [u8],
    pub captured_at: Instant,
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

// ---------------------------------------------------------------------------
// EVDI capture (real-capture feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "real-capture")]
mod evdi {
    use std::os::raw::{c_int, c_uint, c_void};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};

    use super::{CaptureConfig, CaptureFrame, PixelFormat};

    // -- FFI declarations --------------------------------------------------

    type EvdiHandle = *mut c_void;

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct EvdiRect {
        x1: c_int,
        y1: c_int,
        x2: c_int,
        y2: c_int,
    }

    #[repr(C)]
    struct EvdiBuffer {
        id: c_int,
        buffer: *mut c_void,
        width: c_int,
        height: c_int,
        stride: c_int,
        rects: *mut EvdiRect,
        rect_count: c_int,
    }

    type DpmsHandler = extern "C" fn(c_int, *mut c_void);
    type ModeChangedHandler = extern "C" fn(EvdiMode, *mut c_void);
    type UpdateReadyHandler = extern "C" fn(c_int, *mut c_void);
    type CrtcStateHandler = extern "C" fn(c_int, *mut c_void);
    type CursorSetHandler = extern "C" fn(*const c_void, *mut c_void);
    type CursorMoveHandler = extern "C" fn(*const c_void, *mut c_void);
    type DdcciDataHandler = extern "C" fn(*const c_void, *mut c_void);

    #[repr(C)]
    struct EvdiEventContext {
        dpms_handler: Option<DpmsHandler>,
        mode_changed_handler: Option<ModeChangedHandler>,
        update_ready_handler: Option<UpdateReadyHandler>,
        crtc_state_handler: Option<CrtcStateHandler>,
        cursor_set_handler: Option<CursorSetHandler>,
        cursor_move_handler: Option<CursorMoveHandler>,
        ddcci_data_handler: Option<DdcciDataHandler>,
        user_data: *mut c_void,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct EvdiMode {
        width: c_int,
        height: c_int,
        refresh_rate: c_int,
        bits_per_pixel: c_int,
        pixel_format: c_uint,
    }

    #[link(name = "evdi")]
    extern "C" {
        fn evdi_check_device(device: c_int) -> c_int;
        fn evdi_add_device() -> c_int;
        fn evdi_open(device: c_int) -> EvdiHandle;
        fn evdi_close(handle: EvdiHandle);
        fn evdi_connect(
            handle: EvdiHandle,
            edid: *const u8,
            edid_length: c_uint,
            sku_area_limit: u32,
        );
        fn evdi_disconnect(handle: EvdiHandle);
        fn evdi_register_buffer(handle: EvdiHandle, buffer: EvdiBuffer);
        fn evdi_unregister_buffer(handle: EvdiHandle, buffer_id: c_int);
        fn evdi_request_update(handle: EvdiHandle, buffer_id: c_int) -> bool;
        fn evdi_grab_pixels(
            handle: EvdiHandle,
            rects: *mut EvdiRect,
            num_rects: *mut c_int,
        );
        fn evdi_handle_events(handle: EvdiHandle, evtctx: *mut EvdiEventContext);
        fn evdi_get_event_ready(handle: EvdiHandle) -> c_int;
    }

    // -- Callback ----------------------------------------------------------

    struct CallbackState {
        update_ready: bool,
        mode: Option<EvdiMode>,
    }

    extern "C" fn on_update_ready(_buf_id: c_int, user_data: *mut c_void) {
        let state = unsafe { &mut *(user_data as *mut CallbackState) };
        state.update_ready = true;
    }

    extern "C" fn on_mode_changed(mode: EvdiMode, user_data: *mut c_void) {
        let state = unsafe { &mut *(user_data as *mut CallbackState) };
        println!(
            "[capture] EVDI mode changed: {}x{}@{}Hz",
            mode.width, mode.height, mode.refresh_rate
        );
        state.mode = Some(mode);
    }

    extern "C" fn on_dpms(mode: c_int, _user_data: *mut c_void) {
        let name = match mode {
            0 => "ON",
            1 => "STANDBY",
            2 => "SUSPEND",
            3 => "OFF",
            _ => "UNKNOWN",
        };
        println!("[capture] DPMS state changed: {name} ({mode})");
    }
    extern "C" fn on_crtc_state(state: c_int, _user_data: *mut c_void) {
        println!("[capture] CRTC state changed: {state}");
    }

    // -- EDID generation ---------------------------------------------------

    fn generate_edid(width: u32, height: u32, refresh_hz: u32) -> [u8; 128] {
        let mut edid = [0u8; 128];

        // Header
        edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);

        // Manufacturer "SRX" (S=19, R=18, X=24)
        // Packed: 0b0_10011_10010_11000 = 0x4E58
        edid[8] = 0x4E;
        edid[9] = 0x58;

        // Product code + serial
        edid[10..16].copy_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00]);

        // Week 1, Year 2024 (offset from 1990 = 34)
        edid[16] = 0x01;
        edid[17] = 0x22;

        // EDID version 1.4
        edid[18] = 0x01;
        edid[19] = 0x04;

        // Digital input, 8-bit color, DisplayPort
        edid[20] = 0xA5;

        // Physical size in cm (~96 DPI)
        edid[21] = ((width as f64 * 0.02646) as u8).max(1);
        edid[22] = ((height as f64 * 0.02646) as u8).max(1);

        // Gamma 2.2
        edid[23] = 120;

        // Features: sRGB, preferred timing, continuous frequency
        edid[24] = 0x2E;

        // Chromaticity (sRGB standard values)
        edid[25..35].copy_from_slice(&[
            0xEE, 0x95, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54,
        ]);

        // No established timings
        edid[35..38].copy_from_slice(&[0x00, 0x00, 0x00]);

        // Standard timings: none
        for i in (38..54).step_by(2) {
            edid[i] = 0x01;
            edid[i + 1] = 0x01;
        }

        // Detailed timing descriptor for our resolution (CVT-RB style)
        let h_blank: u32 = 80;
        let h_sync_offset: u32 = 8;
        let h_sync_width: u32 = 32;
        let v_blank: u32 = 47;
        let v_sync_offset: u32 = 3;
        let v_sync_width: u32 = 8;

        let h_total = width + h_blank;
        let v_total = height + v_blank;
        let pixel_clock_khz = (h_total as u64 * v_total as u64 * refresh_hz as u64) / 1000;
        let pixel_clock_10khz = (pixel_clock_khz / 10) as u16;

        let h_image = edid[21];
        let v_image = edid[22];
        let d = &mut edid[54..72];
        d[0] = (pixel_clock_10khz & 0xFF) as u8;
        d[1] = (pixel_clock_10khz >> 8) as u8;
        d[2] = (width & 0xFF) as u8;
        d[3] = (h_blank & 0xFF) as u8;
        d[4] = (((width >> 8) & 0x0F) << 4 | ((h_blank >> 8) & 0x0F)) as u8;
        d[5] = (height & 0xFF) as u8;
        d[6] = (v_blank & 0xFF) as u8;
        d[7] = (((height >> 8) & 0x0F) << 4 | ((v_blank >> 8) & 0x0F)) as u8;
        d[8] = (h_sync_offset & 0xFF) as u8;
        d[9] = (h_sync_width & 0xFF) as u8;
        d[10] = (((v_sync_offset & 0x0F) << 4) | (v_sync_width & 0x0F)) as u8;
        d[11] = 0x00;
        d[12] = h_image;
        d[13] = v_image;
        d[14] = 0x00;
        d[15] = 0x00;
        d[16] = 0x00;
        d[17] = 0x1E; // non-interlaced, digital separate, +H +V sync

        // Descriptor 2: Monitor name
        let name = b"Screx Virtual";
        let nd = &mut edid[72..90];
        nd[0..5].copy_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
        for (i, &ch) in name.iter().enumerate().take(13) {
            nd[5 + i] = ch;
        }
        if name.len() < 13 {
            nd[5 + name.len()] = 0x0A;
            for i in (5 + name.len() + 1)..18 {
                nd[i] = 0x20;
            }
        }

        // Descriptor 3: Monitor serial
        let serial = b"001";
        let sd = &mut edid[90..108];
        sd[0..5].copy_from_slice(&[0x00, 0x00, 0x00, 0xFF, 0x00]);
        for (i, &ch) in serial.iter().enumerate().take(13) {
            sd[5 + i] = ch;
        }
        sd[5 + serial.len()] = 0x0A;
        for i in (5 + serial.len() + 1)..18 {
            sd[i] = 0x20;
        }

        // Descriptor 4: Range limits
        let rd = &mut edid[108..126];
        rd[0..5].copy_from_slice(&[0x00, 0x00, 0x00, 0xFD, 0x00]);
        rd[5] = (refresh_hz.saturating_sub(1).max(1)) as u8; // min V rate
        rd[6] = (refresh_hz.saturating_add(1)) as u8; // max V rate
        rd[7] = 1;  // min H rate kHz
        rd[8] = 255; // max H rate kHz
        rd[9] = ((pixel_clock_khz / 10000) + 1).min(255) as u8; // max pixel clock / 10 MHz
        rd[10] = 0x00; // default GTF
        for i in 11..18 {
            rd[i] = 0x0A;
        }

        // Extension count
        edid[126] = 0;

        // Checksum
        let sum: u8 = edid[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        edid[127] = 0u8.wrapping_sub(sum);

        edid
    }

    // -- Device management -------------------------------------------------

    fn find_available_device() -> Option<c_int> {
        for dev in 0..16 {
            let status = unsafe { evdi_check_device(dev) };
            if status == 0 {
                // AVAILABLE
                return Some(dev);
            }
        }
        None
    }

    fn ensure_evdi_module_loaded() -> Result<()> {
        if std::path::Path::new("/sys/module/evdi").exists() {
            return Ok(());
        }

        println!("[capture] evdi module not loaded; trying to load it via modprobe...");
        let status = Command::new("modprobe")
            .arg("evdi")
            .status()
            .context("failed to run modprobe evdi")?;
        if !status.success() {
            anyhow::bail!("modprobe evdi failed with status {status}");
        }

        for _ in 0..20 {
            if std::path::Path::new("/sys/module/evdi").exists() {
                println!("[capture] evdi module loaded");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        anyhow::bail!("modprobe evdi reported success but /sys/module/evdi is still missing");
    }

    fn find_or_create_device() -> Result<c_int> {
        ensure_evdi_module_loaded()?;

        if let Some(dev) = find_available_device() {
            println!("[capture] found existing EVDI device at card {dev}");
            return Ok(dev);
        }

        println!("[capture] no EVDI device found, creating one...");
        let ret = unsafe { evdi_add_device() };
        if ret < 0 {
            anyhow::bail!(
                "evdi_add_device() failed ({}). Is the evdi kernel module loaded? Try: sudo modprobe evdi",
                ret
            );
        }

        // Best effort: wait for udev to settle newly created DRM node.
        let _ = Command::new("udevadm")
            .args(["settle", "--timeout=10"])
            .status();

        // Wait for udev to create the device node, then rescan
        for attempt in 0..40 {
            std::thread::sleep(Duration::from_millis(250));
            if let Some(dev) = find_available_device() {
                println!("[capture] EVDI device created at card {dev} (attempt {attempt})");
                return Ok(dev);
            }
        }

        anyhow::bail!(
            "evdi_add_device succeeded but no EVDI card became available after 10s"
        );
    }

    // -- Public capture entry point ----------------------------------------

    pub(super) fn run_capture(
        config: &CaptureConfig,
        stop: &Arc<AtomicBool>,
        on_frame: &mut impl FnMut(CaptureFrame<'_>),
    ) -> Result<()> {
        let dev = find_or_create_device()?;
        let handle = unsafe { evdi_open(dev) };
        if handle.is_null() {
            anyhow::bail!("evdi_open({dev}) returned null");
        }
        println!("[capture] EVDI device {dev} opened");

        // Generate EDID and connect
        let edid = generate_edid(config.width, config.height, config.fps.max(30));
        let area_limit = config.width * config.height;
        unsafe {
            evdi_connect(handle, edid.as_ptr(), edid.len() as c_uint, area_limit);
        }
        println!(
            "[capture] EVDI connected: {}x{}@{}Hz",
            config.width, config.height, config.fps
        );

        // Allocate pixel buffer (XRGB8888 = 4 bytes per pixel)
        let stride = config.width as usize * 4;
        let buf_size = stride * config.height as usize;
        let mut pixel_buf = vec![0u8; buf_size];
        let mut rects = vec![
            EvdiRect {
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 0,
            };
            16
        ];

        let evdi_buf = EvdiBuffer {
            id: 0,
            buffer: pixel_buf.as_mut_ptr() as *mut c_void,
            width: config.width as c_int,
            height: config.height as c_int,
            stride: stride as c_int,
            rects: rects.as_mut_ptr(),
            rect_count: rects.len() as c_int,
        };
        unsafe {
            evdi_register_buffer(handle, evdi_buf);
        }
        println!("[capture] buffer registered: {}x{} stride={stride}", config.width, config.height);

        // Set up event callbacks
        let mut cb_state = CallbackState {
            update_ready: false,
            mode: None,
        };

        let mut evtctx = EvdiEventContext {
            dpms_handler: Some(on_dpms),
            mode_changed_handler: Some(on_mode_changed),
            update_ready_handler: Some(on_update_ready),
            crtc_state_handler: Some(on_crtc_state),
            cursor_set_handler: None,
            cursor_move_handler: None,
            ddcci_data_handler: None,
            user_data: &mut cb_state as *mut CallbackState as *mut c_void,
        };

        let evdi_fd = unsafe { evdi_get_event_ready(handle) };
        if evdi_fd < 0 {
            unsafe {
                evdi_disconnect(handle);
                evdi_close(handle);
            }
            anyhow::bail!("evdi_get_event_ready returned {evdi_fd}");
        }

        let frame_interval = Duration::from_micros(1_000_000 / config.fps.max(1) as u64);
        let poll_timeout_ms = frame_interval.as_millis().max(1).min(100) as i32;
        let mut frame_index: u64 = 0;
        let mut stats_start = Instant::now();
        let mut stats_frames: u64 = 0;
        let start_time = Instant::now();

        println!("[capture] entering EVDI capture loop (poll timeout={poll_timeout_ms}ms)");
        println!("[capture] NOTE: enable the 'Screx Virtual' display in GNOME Settings > Displays");

        let mut pending_request = false;
        let mut no_update_count: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            // Request an update if we don't have one pending
            if !pending_request {
                let ready_now = unsafe { evdi_request_update(handle, 0) };
                if ready_now {
                    cb_state.update_ready = true;
                } else {
                    pending_request = true;
                }
            }

            // Poll the EVDI fd for events
            if !cb_state.update_ready {
                let mut pollfd = libc::pollfd {
                    fd: evdi_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let poll_ret = unsafe { libc::poll(&mut pollfd, 1, poll_timeout_ms) };

                if poll_ret > 0 && (pollfd.revents & libc::POLLIN != 0) {
                    unsafe {
                        evdi_handle_events(handle, &mut evtctx);
                    }
                }
            }

            if cb_state.update_ready {
                cb_state.update_ready = false;
                pending_request = false;
                no_update_count = 0;

                let mut num_rects: c_int = rects.len() as c_int;
                unsafe {
                    evdi_grab_pixels(handle, rects.as_mut_ptr(), &mut num_rects);
                }

                let timestamp_90k = ((frame_index * 90_000) / config.fps.max(1) as u64) as u32;
                let frame = CaptureFrame {
                    frame_index,
                    timestamp_90k,
                    width: config.width,
                    height: config.height,
                    format: PixelFormat::Bgra8888,
                    data: &pixel_buf,
                    captured_at: Instant::now(),
                };

                on_frame(frame);
                frame_index += 1;
                stats_frames += 1;
            } else {
                no_update_count += 1;
                // If we've been waiting a long time, re-request
                if no_update_count > 60 {
                    pending_request = false;
                    no_update_count = 0;
                }
            }

            // Stats
            if stats_start.elapsed() >= Duration::from_secs(1) {
                let fps = stats_frames as f64 / stats_start.elapsed().as_secs_f64();
                let uptime = start_time.elapsed().as_secs();
                println!(
                    "[capture] fps={fps:.1} resolution={}x{} uptime={uptime}s",
                    config.width, config.height
                );
                stats_start = Instant::now();
                stats_frames = 0;
            }
        }

        println!("[capture] stopping EVDI capture");
        unsafe {
            evdi_unregister_buffer(handle, 0);
            evdi_disconnect(handle);
            evdi_close(handle);
        }
        println!("[capture] EVDI cleanup complete");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Synthetic capture (fallback when real-capture is unavailable)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "real-capture"))]
fn run_synthetic_capture(
    config: &CaptureConfig,
    stop: &Arc<AtomicBool>,
    on_frame: &mut impl FnMut(CaptureFrame<'_>),
) -> Result<()> {
    let frame_interval = Duration::from_micros(1_000_000 / config.fps.max(1) as u64);
    let mut frame_index = 0_u64;
    let mut stats_start = Instant::now();
    let mut stats_frames = 0_u64;
    let start = Instant::now();
    let pixel_count = (config.width as usize) * (config.height as usize);

    println!(
        "[capture] using synthetic source: {}x{}@{}fps",
        config.width, config.height, config.fps
    );

    while !stop.load(Ordering::Relaxed) {
        let mut data = vec![0u8; pixel_count * 4];
        let t = (frame_index % 255) as u8;
        for px in data.chunks_exact_mut(4).step_by(97) {
            px[0] = t;
            px[1] = t.wrapping_add(80);
            px[2] = t.wrapping_add(160);
            px[3] = 255;
        }

        let timestamp_90k = ((frame_index * 90_000) / config.fps.max(1) as u64) as u32;
        on_frame(CaptureFrame {
            frame_index,
            timestamp_90k,
            width: config.width,
            height: config.height,
            format: PixelFormat::Bgra8888,
            data: &data,
            captured_at: Instant::now(),
        });

        frame_index += 1;
        stats_frames += 1;
        if stats_start.elapsed() >= Duration::from_secs(1) {
            let fps = stats_frames as f64 / stats_start.elapsed().as_secs_f64();
            println!(
                "[capture] fps={fps:.1} resolution={}x{} uptime={}s (synthetic)",
                config.width, config.height, start.elapsed().as_secs()
            );
            stats_start = Instant::now();
            stats_frames = 0;
        }

        std::thread::sleep(frame_interval);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run_capture_loop(
    config: CaptureConfig,
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(CaptureFrame<'_>),
) -> Result<()> {
    #[cfg(feature = "real-capture")]
    {
        // In production builds we require real EVDI capture. If EVDI fails,
        // fail the capture loop instead of silently switching to synthetic.
        return evdi::run_capture(&config, &stop, &mut on_frame);
    }

    #[cfg(not(feature = "real-capture"))]
    {
        println!("[capture] built without real-capture feature, using synthetic source");
        return run_synthetic_capture(&config, &stop, &mut on_frame);
    }
}
