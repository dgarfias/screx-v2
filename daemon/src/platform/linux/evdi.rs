use std::os::raw::{c_int, c_uint, c_void};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::capture::{CaptureFrame, DisplayBackend, DisplayMode};

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
    fn evdi_connect(handle: EvdiHandle, edid: *const u8, edid_length: c_uint, sku_area_limit: u32);
    fn evdi_disconnect(handle: EvdiHandle);
    fn evdi_register_buffer(handle: EvdiHandle, buffer: EvdiBuffer);
    fn evdi_unregister_buffer(handle: EvdiHandle, buffer_id: c_int);
    fn evdi_request_update(handle: EvdiHandle, buffer_id: c_int) -> bool;
    fn evdi_grab_pixels(handle: EvdiHandle, rects: *mut EvdiRect, num_rects: *mut c_int);
    fn evdi_handle_events(handle: EvdiHandle, evtctx: *mut EvdiEventContext);
    fn evdi_get_event_ready(handle: EvdiHandle) -> c_int;
}

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

pub struct EvdiDisplay {
    width: u32,
    height: u32,
    fps: u32,
    handle: Option<EvdiHandle>,
    pixel_buf: Option<Vec<u8>>,
}

unsafe impl Send for EvdiDisplay {}

impl EvdiDisplay {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        Ok(Self {
            width,
            height,
            fps,
            handle: None,
            pixel_buf: None,
        })
    }
}

impl DisplayBackend for EvdiDisplay {
    fn attach(&mut self, mode: DisplayMode) -> Result<()> {
        self.width = mode.width;
        self.height = mode.height;
        self.fps = mode.fps;
        Ok(())
    }

    fn run_capture_loop(
        &mut self,
        stop: &AtomicBool,
        force_refresh: &AtomicBool,
        on_frame: &mut dyn FnMut(CaptureFrame<'_>),
    ) -> Result<()> {
        let dev = find_or_create_device()?;
        let handle = unsafe { evdi_open(dev) };
        if handle.is_null() {
            anyhow::bail!("evdi_open({dev}) returned null");
        }
        println!("[capture] EVDI device {dev} opened");
        self.handle = Some(handle);

        let edid = generate_edid(self.width, self.height, self.fps.max(30));
        let area_limit = self.width * self.height;
        unsafe {
            evdi_connect(handle, edid.as_ptr(), edid.len() as c_uint, area_limit);
        }
        println!(
            "[capture] EVDI connected: {}x{}@{}Hz",
            self.width, self.height, self.fps
        );

        let stride = self.width as usize * 4;
        let buf_size = stride * self.height as usize;
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
            width: self.width as c_int,
            height: self.height as c_int,
            stride: stride as c_int,
            rects: rects.as_mut_ptr(),
            rect_count: rects.len() as c_int,
        };
        unsafe {
            evdi_register_buffer(handle, evdi_buf);
        }
        println!(
            "[capture] buffer registered: {}x{} stride={stride}",
            self.width, self.height
        );

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
            self.handle = None;
            anyhow::bail!("evdi_get_event_ready returned {evdi_fd}");
        }

        let frame_interval = Duration::from_micros(1_000_000 / self.fps.max(1) as u64);
        let poll_timeout_ms = frame_interval.as_millis().max(1).min(100) as i32;
        let mut stats_start = Instant::now();
        let mut stats_frames: u64 = 0;
        let start_time = Instant::now();

        println!("[capture] entering EVDI capture loop (poll timeout={poll_timeout_ms}ms)");
        println!("[capture] NOTE: enable the 'Screx Virtual' display in GNOME Settings > Displays");

        let mut pending_request = false;
        let mut no_update_count: u64 = 0;
        let mut has_first_frame = false;
        let mut refresh_retries: u32 = 0;

        while !stop.load(Ordering::Relaxed) {
            if force_refresh.swap(false, Ordering::Relaxed) {
                refresh_retries = 100;
                println!("[capture] force refresh requested, will retry for ~3s");
            }

            if refresh_retries > 0 {
                pending_request = false;
            }

            if !pending_request {
                let ready_now = unsafe { evdi_request_update(handle, 0) };
                if ready_now {
                    cb_state.update_ready = true;
                } else {
                    pending_request = true;
                }
            }

            if !cb_state.update_ready {
                let mut pollfd = libc::pollfd {
                    fd: evdi_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let timeout = if refresh_retries > 0 {
                    30
                } else {
                    poll_timeout_ms
                };
                let poll_ret = unsafe { libc::poll(&mut pollfd, 1, timeout) };

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

                let frame = CaptureFrame {
                    width: self.width,
                    height: self.height,
                    format: crate::capture::CapturePixelFormat::Bgra,
                    data: &pixel_buf,
                };

                on_frame(frame);
                stats_frames += 1;
                has_first_frame = true;

                if refresh_retries > 0 {
                    println!("[capture] force refresh: got real frame from compositor");
                    refresh_retries = 0;
                }
            } else {
                no_update_count += 1;
                if no_update_count > 60 {
                    pending_request = false;
                    no_update_count = 0;
                }

                if refresh_retries > 0 {
                    refresh_retries -= 1;
                    if refresh_retries == 0 {
                        if has_first_frame {
                            println!("[capture] force refresh: no new damage, sending last captured buffer");
                        } else {
                            println!("[capture] force refresh: no compositor damage yet, sending black bootstrap frame");
                        }
                        let frame = CaptureFrame {
                            width: self.width,
                            height: self.height,
                            format: crate::capture::CapturePixelFormat::Bgra,
                            data: &pixel_buf,
                        };
                        on_frame(frame);
                    }
                }
            }

            if stats_start.elapsed() >= Duration::from_secs(1) {
                let fps = stats_frames as f64 / stats_start.elapsed().as_secs_f64();
                let uptime = start_time.elapsed().as_secs();
                println!(
                    "[capture] fps={fps:.1} resolution={}x{} uptime={uptime}s",
                    self.width, self.height
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
        self.handle = None;
        println!("[capture] EVDI cleanup complete");
        Ok(())
    }

    fn detach(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe {
                evdi_disconnect(handle);
                evdi_close(handle);
            }
            println!("[capture] EVDI detached");
        }
    }
}

fn generate_edid(width: u32, height: u32, refresh_hz: u32) -> [u8; 128] {
    let mut edid = [0u8; 128];

    edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    edid[8] = 0x4E;
    edid[9] = 0x58;
    edid[10..16].copy_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00]);
    edid[16] = 0x01;
    edid[17] = 0x22;
    edid[18] = 0x01;
    edid[19] = 0x04;
    edid[20] = 0xA5;
    edid[21] = ((width as f64 * 0.02646) as u8).max(1);
    edid[22] = ((height as f64 * 0.02646) as u8).max(1);
    edid[23] = 120;
    edid[24] = 0x2E;
    edid[25..35].copy_from_slice(&[0xEE, 0x95, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    edid[35..38].copy_from_slice(&[0x00, 0x00, 0x00]);
    for i in (38..54).step_by(2) {
        edid[i] = 0x01;
        edid[i + 1] = 0x01;
    }

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
    d[17] = 0x1E;

    let name = b"Screx Virtual";
    let nd = &mut edid[72..90];
    nd[0..5].copy_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
    for (i, &ch) in name.iter().enumerate().take(13) {
        nd[5 + i] = ch;
    }
    if name.len() < 13 {
        nd[5 + name.len()] = 0x0A;
        for i in (5 + name.len() + 1)..18 {
            edid[i] = 0x20;
        }
    }

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

    let rd = &mut edid[108..126];
    rd[0..5].copy_from_slice(&[0x00, 0x00, 0x00, 0xFD, 0x00]);
    rd[5] = (refresh_hz.saturating_sub(1).max(1)) as u8;
    rd[6] = (refresh_hz.saturating_add(1)) as u8;
    rd[7] = 1;
    rd[8] = 255;
    rd[9] = ((pixel_clock_khz / 10000) + 1).min(255) as u8;
    rd[10] = 0x00;
    for i in 11..18 {
        rd[i] = 0x0A;
    }

    edid[126] = 0;
    let sum: u8 = edid[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    edid[127] = 0u8.wrapping_sub(sum);

    edid
}

fn find_available_device() -> Option<c_int> {
    for dev in 0..16 {
        let status = unsafe { evdi_check_device(dev) };
        if status == 0 {
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

    anyhow::bail!("modprobe evdi reported success but /sys/module/evdi is still missing")
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

    let _ = Command::new("udevadm")
        .args(["settle", "--timeout=10"])
        .status();

    for attempt in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(dev) = find_available_device() {
            println!("[capture] EVDI device created at card {dev} (attempt {attempt})");
            return Ok(dev);
        }
    }

    anyhow::bail!("evdi_add_device succeeded but no EVDI card became available after 10s")
}
