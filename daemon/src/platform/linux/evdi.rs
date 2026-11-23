use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::raw::{c_int, c_uint, c_void};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::capture::{
    CaptureFrame, DisplayBackend, DisplayId, DisplayMode, DmaBufCursor, DmaBufFrame, DmaBufObject,
    DmaBufPlane,
};

// Allocation and opening must be serialized: checking a card does not reserve it.
// Keep an explicit lease until close, even if libevdi still reports it available.
static DEVICE_LEASES: Mutex<Vec<c_int>> = Mutex::new(Vec::new());

struct DeviceLease(c_int);

impl DeviceLease {
    fn open() -> Result<(Self, EvdiHandle)> {
        let mut leased = DEVICE_LEASES.lock().unwrap();
        let device = find_or_create_device(&leased)?;
        let handle = unsafe { evdi_open(device) };
        if handle.is_null() {
            anyhow::bail!("evdi_open({device}) returned null");
        }
        leased.push(device);
        Ok((Self(device), handle))
    }
}

impl Drop for DeviceLease {
    fn drop(&mut self) {
        DEVICE_LEASES
            .lock()
            .unwrap()
            .retain(|&device| device != self.0);
    }
}

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
}

extern "C" fn on_update_ready(_buf_id: c_int, user_data: *mut c_void) {
    let state = unsafe { &mut *(user_data as *mut CallbackState) };
    state.update_ready = true;
}

extern "C" fn on_mode_changed(mode: EvdiMode, _user_data: *mut c_void) {
    crate::log_detail!(
        "[capture] EVDI mode changed: {}x{}@{}Hz",
        mode.width,
        mode.height,
        mode.refresh_rate
    );
}

extern "C" fn on_dpms(mode: c_int, _user_data: *mut c_void) {
    let name = match mode {
        0 => "ON",
        1 => "STANDBY",
        2 => "SUSPEND",
        3 => "OFF",
        _ => "UNKNOWN",
    };
    crate::log_detail!("[capture] DPMS state changed: {name} ({mode})");
}

extern "C" fn on_crtc_state(state: c_int, _user_data: *mut c_void) {
    crate::log_detail!("[capture] CRTC state changed: {state}");
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ScrexDrmCursorPlane {
    plane_id: u32,
    prop_crtc_x: u32,
    prop_crtc_y: u32,
    prop_crtc_w: u32,
    prop_crtc_h: u32,
}

#[repr(C)]
struct ScrexDrmExport {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    num_objects: u32,
    num_planes: u32,
    fds: [i32; 4],
    object_sizes: [u64; 4],
    object_modifiers: [u64; 4],
    plane_obj_idx: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],

    cursor_fb_id: u32,
    cursor_x: i32,
    cursor_y: i32,
    cursor_width: u32,
    cursor_height: u32,
    cursor_crtc_w: u32,
    cursor_crtc_h: u32,
    cursor_pixel_format: u32,
    cursor_fd: i32,
    cursor_size: u64,
    cursor_modifier: u64,
    cursor_pitch: u32,
    cursor_offset: u32,
}

extern "C" {
    fn screx_drm_open_card(
        card_index: c_int,
        out_crtc_id: *mut u32,
        out_cursor: *mut ScrexDrmCursorPlane,
    ) -> c_int;
    fn screx_drm_export_fb(
        drm_fd: c_int,
        io_crtc_id: *mut u32,
        cursor_plane: *const ScrexDrmCursorPlane,
        out: *mut ScrexDrmExport,
    ) -> c_int;
    fn screx_evdi_ack_vblank(evdi_fd: c_int) -> c_int;
}

struct DrmCard {
    fd: OwnedFd,
    crtc_id: u32,
    cursor_plane: ScrexDrmCursorPlane,
}

impl DrmCard {
    fn open(card_index: c_int) -> Result<Self, i32> {
        let mut crtc_id = 0u32;
        let mut cursor_plane = ScrexDrmCursorPlane::default();
        let fd = unsafe { screx_drm_open_card(card_index, &mut crtc_id, &mut cursor_plane) };
        if fd < 0 {
            return Err(fd);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self {
            fd,
            crtc_id,
            cursor_plane,
        })
    }

    fn export_fb(&mut self) -> Result<(u32, u32, DmaBufFrame), i32> {
        let mut raw = ScrexDrmExport {
            fb_id: 0,
            width: 0,
            height: 0,
            pixel_format: 0,
            flags: 0,
            num_objects: 0,
            num_planes: 0,
            fds: [-1; 4],
            object_sizes: [0; 4],
            object_modifiers: [0; 4],
            plane_obj_idx: [0; 4],
            pitches: [0; 4],
            offsets: [0; 4],
            cursor_fb_id: 0,
            cursor_x: 0,
            cursor_y: 0,
            cursor_width: 0,
            cursor_height: 0,
            cursor_crtc_w: 0,
            cursor_crtc_h: 0,
            cursor_pixel_format: 0,
            cursor_fd: -1,
            cursor_size: 0,
            cursor_modifier: 0,
            cursor_pitch: 0,
            cursor_offset: 0,
        };
        let ret = unsafe {
            screx_drm_export_fb(
                self.fd.as_raw_fd(),
                &mut self.crtc_id,
                &self.cursor_plane,
                &mut raw,
            )
        };
        if ret < 0 {
            return Err(ret);
        }
        let num_objects = (raw.num_objects as usize).min(4);
        let num_planes = (raw.num_planes as usize).min(4);
        let mut objects = Vec::with_capacity(num_objects);
        for i in 0..num_objects {
            let fd = unsafe { OwnedFd::from_raw_fd(raw.fds[i]) };
            objects.push(DmaBufObject {
                fd,
                size: raw.object_sizes[i],
                modifier: raw.object_modifiers[i],
            });
        }
        let mut planes = Vec::with_capacity(num_planes);
        for p in 0..num_planes {
            planes.push(DmaBufPlane {
                object_index: raw.plane_obj_idx[p] as usize,
                offset: raw.offsets[p],
                pitch: raw.pitches[p],
            });
        }
        let cursor = if raw.cursor_fd >= 0 && raw.cursor_width > 0 && raw.cursor_height > 0 {
            Some(DmaBufCursor {
                fb_id: raw.cursor_fb_id,
                x: raw.cursor_x,
                y: raw.cursor_y,
                width: raw.cursor_width,
                height: raw.cursor_height,
                crtc_w: raw.cursor_crtc_w,
                crtc_h: raw.cursor_crtc_h,
                pixel_format: raw.cursor_pixel_format,
                fd: unsafe { OwnedFd::from_raw_fd(raw.cursor_fd) },
                size: raw.cursor_size,
                modifier: raw.cursor_modifier,
                pitch: raw.cursor_pitch,
                offset: raw.cursor_offset,
            })
        } else {
            None
        };
        Ok((
            raw.width,
            raw.height,
            DmaBufFrame {
                fb_id: raw.fb_id,
                pixel_format: raw.pixel_format,
                flags: raw.flags,
                objects,
                planes,
                cursor,
            },
        ))
    }
}

fn fourcc_label(fourcc: u32) -> String {
    let bytes = fourcc.to_le_bytes();
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

pub struct EvdiDisplay {
    display_id: DisplayId,
    monitor_name: [u8; 13],
    physical_size_mm: Option<(u16, u16)>,
    width: u32,
    height: u32,
    fps: u32,
    zero_copy: Arc<AtomicBool>,
    handle: Option<EvdiHandle>,
    device_lease: Option<DeviceLease>,
}

unsafe impl Send for EvdiDisplay {}

impl EvdiDisplay {
    pub fn new(
        display_id: DisplayId,
        monitor_name: [u8; 13],
        physical_size_mm: Option<(u16, u16)>,
        width: u32,
        height: u32,
        fps: u32,
        zero_copy: Arc<AtomicBool>,
    ) -> Result<Self> {
        Ok(Self {
            display_id,
            monitor_name,
            physical_size_mm,
            width,
            height,
            fps,
            zero_copy,
            handle: None,
            device_lease: None,
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
        anyhow::ensure!(self.handle.is_none(), "EVDI capture is already running");
        let (lease, handle) = DeviceLease::open()?;
        let dev = lease.0;
        crate::log_detail!("[capture] EVDI device {dev} opened");
        self.handle = Some(handle);
        self.device_lease = Some(lease);

        // Open the auxiliary DRM fd and drop DRM master BEFORE evdi_connect()
        // emits a hotplug event, so the compositor never races with open().
        let mut drm_card = if self.zero_copy.load(Ordering::Acquire) {
            match DrmCard::open(dev) {
                Ok(card) => {
                    crate::log_detail!(
                        "[capture] zero-copy DRM /dev/dri/card{dev} opened (crtc={}, cursor_plane={} props=x:{},y:{},w:{},h:{})",
                        card.crtc_id,
                        card.cursor_plane.plane_id,
                        card.cursor_plane.prop_crtc_x,
                        card.cursor_plane.prop_crtc_y,
                        card.cursor_plane.prop_crtc_w,
                        card.cursor_plane.prop_crtc_h,
                    );
                    Some(card)
                }
                Err(err) => {
                    eprintln!(
                        "[capture] zero-copy open /dev/dri/card{dev} failed (err={err}); falling back to CPU capture"
                    );
                    self.zero_copy.store(false, Ordering::Release);
                    None
                }
            }
        } else {
            None
        };

        let mut edid = generate_edid(
            self.display_id,
            self.width,
            self.height,
            self.fps.max(1),
            self.physical_size_mm.map(crate::capture::edid_size_cm),
        );
        set_monitor_name(&mut edid, &self.monitor_name);
        let area_limit = self.width * self.height;
        unsafe {
            evdi_connect(handle, edid.as_ptr(), edid.len() as c_uint, area_limit);
        }
        crate::log_detail!(
            "[capture] EVDI connected: {}x{}@{}Hz",
            self.width,
            self.height,
            self.fps
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
        crate::log_detail!(
            "[capture] buffer registered: {}x{} stride={stride}",
            self.width,
            self.height
        );

        let mut cb_state = CallbackState {
            update_ready: false,
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
                evdi_unregister_buffer(handle, 0);
                evdi_disconnect(handle);
                evdi_close(handle);
            }
            self.handle = None;
            self.device_lease = None;
            anyhow::bail!("evdi_get_event_ready returned {evdi_fd}");
        }

        let frame_interval = Duration::from_micros(1_000_000 / self.fps.max(1) as u64);
        let poll_timeout_ms = frame_interval.as_millis().max(1).min(100) as i32;
        let mut stats_start = Instant::now();
        let mut stats_frames: u64 = 0;
        let mut stats_zc_frames: u64 = 0;
        let mut stats_cap_us: u64 = 0;
        let start_time = Instant::now();

        let mut logged_zc_format: Option<(u32, u64, u32, bool)> = None;
        let mut logged_zc_export_err = false;
        let mut logged_zc_ack_err = false;

        crate::log_detail!(
            "[capture] entering EVDI capture loop (poll timeout={poll_timeout_ms}ms, zero_copy={})",
            drm_card.is_some()
        );
        crate::log_detail!(
            "[capture] monitor: {}",
            String::from_utf8_lossy(&self.monitor_name).trim_end()
        );

        let mut pending_request = false;
        let mut no_update_count: u64 = 0;
        let mut has_first_frame = false;
        let mut refresh_retries: u32 = 0;

        while !stop.load(Ordering::Relaxed) {
            if force_refresh.swap(false, Ordering::Relaxed) {
                refresh_retries = 100;
                crate::log_detail!("[capture] force refresh requested, will retry for ~3s");
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

                let mut sent_zero_copy = false;
                if self.zero_copy.load(Ordering::Acquire) {
                    if let Some(card) = drm_card.as_mut() {
                        let t_cap = Instant::now();
                        match card.export_fb() {
                            Ok((fb_w, fb_h, dmabuf))
                                if fb_w == self.width && fb_h == self.height =>
                            {
                                let export_us = t_cap.elapsed().as_micros() as u64;
                                let mod0 = dmabuf
                                    .objects
                                    .first()
                                    .map_or(0, |o| o.modifier);
                                let pitch0 = dmabuf.planes.first().map_or(0, |p| p.pitch);
                                let size0 = dmabuf.objects.first().map_or(0, |o| o.size);
                                let cursor_fb_id = dmabuf.cursor.as_ref().map_or(0, |c| c.fb_id);
                                let sig = (
                                    dmabuf.pixel_format,
                                    mod0,
                                    dmabuf.planes.len() as u32,
                                    cursor_fb_id != 0,
                                );
                                if logged_zc_format != Some(sig) {
                                    crate::log_detail!(
                                        "[capture] zero-copy DMA-BUF active: {fb_w}x{fb_h} fb={} fourcc={} (0x{:08x}) flags=0x{:x} mod=0x{:016x} planes={} objs={} pitch={} size={} cursor_fb={}",
                                        dmabuf.fb_id,
                                        fourcc_label(dmabuf.pixel_format),
                                        dmabuf.pixel_format,
                                        dmabuf.flags,
                                        mod0,
                                        dmabuf.planes.len(),
                                        dmabuf.objects.len(),
                                        pitch0,
                                        size0,
                                        cursor_fb_id,
                                    );
                                    logged_zc_format = Some(sig);
                                }
                                logged_zc_export_err = false;

                                let frame = CaptureFrame::dmabuf(fb_w, fb_h, &dmabuf);
                                on_frame(frame);

                                let t_ack = Instant::now();
                                let ack_ret = unsafe { screx_evdi_ack_vblank(evdi_fd) };
                                let ack_us = t_ack.elapsed().as_micros() as u64;
                                if ack_ret != 0 && ack_ret != -libc::EAGAIN && !logged_zc_ack_err {
                                    eprintln!(
                                        "[capture] zero-copy EVDI vblank ack returned {ack_ret}"
                                    );
                                    logged_zc_ack_err = true;
                                }

                                stats_cap_us += export_us + ack_us;
                                stats_frames += 1;
                                stats_zc_frames += 1;
                                has_first_frame = true;
                                sent_zero_copy = true;

                                if refresh_retries > 0 {
                                    crate::log_detail!(
                                        "[capture] force refresh: got real zero-copy frame from compositor"
                                    );
                                    refresh_retries = 0;
                                }
                            }
                            Ok((fb_w, fb_h, _)) => {
                                crate::log_detail!(
                                    "[capture] zero-copy FB size mismatch ({fb_w}x{fb_h} vs {}x{}); using CPU grab",
                                    self.width,
                                    self.height
                                );
                            }
                            Err(err) => {
                                let fatal = matches!(
                                    -err,
                                    libc::EACCES
                                        | libc::EPERM
                                        | libc::EOPNOTSUPP
                                        | libc::ENOSYS
                                );
                                if fatal {
                                    eprintln!(
                                        "[capture] zero-copy DRM export unsupported/denied (err={err}); disabling zero-copy"
                                    );
                                    drm_card = None;
                                    self.zero_copy.store(false, Ordering::Release);
                                } else if !logged_zc_export_err {
                                    crate::log_detail!(
                                        "[capture] zero-copy DRM export deferred (err={err}); using CPU grab"
                                    );
                                    logged_zc_export_err = true;
                                }
                            }
                        }
                    }
                } else if drm_card.is_some() {
                    // Pipeline disabled zero-copy (e.g. VA-API rejected the DMA-BUF).
                    drm_card = None;
                }

                if !sent_zero_copy {
                    let t_cap = Instant::now();
                    let mut num_rects: c_int = rects.len() as c_int;
                    unsafe {
                        evdi_grab_pixels(handle, rects.as_mut_ptr(), &mut num_rects);
                    }
                    let cap_us = t_cap.elapsed().as_micros() as u64;

                    if num_rects > 0 || !has_first_frame || refresh_retries > 0 {
                        let frame = CaptureFrame::bgra(self.width, self.height, &pixel_buf);

                        on_frame(frame);
                        stats_cap_us += cap_us;
                        stats_frames += 1;
                        has_first_frame = true;

                        if refresh_retries > 0 {
                            crate::log_detail!("[capture] force refresh: got real frame from compositor");
                            refresh_retries = 0;
                        }
                    }
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
                        let mut refreshed_zc = false;
                        if self.zero_copy.load(Ordering::Acquire) {
                            if let Some(card) = drm_card.as_mut() {
                                if let Ok((fb_w, fb_h, dmabuf)) = card.export_fb() {
                                    if fb_w == self.width && fb_h == self.height {
                                        crate::log_detail!(
                                            "[capture] force refresh: no new damage, re-exporting current DMA-BUF"
                                        );
                                        on_frame(CaptureFrame::dmabuf(fb_w, fb_h, &dmabuf));
                                        refreshed_zc = true;
                                    }
                                }
                            }
                        }
                        if !refreshed_zc {
                            if has_first_frame {
                                crate::log_detail!("[capture] force refresh: no new damage, sending last captured buffer");
                            } else {
                                crate::log_detail!("[capture] force refresh: no compositor damage yet, sending black bootstrap frame");
                            }
                            let frame = CaptureFrame::bgra(self.width, self.height, &pixel_buf);
                            on_frame(frame);
                        }
                    }
                }
            }

            if stats_start.elapsed() >= Duration::from_secs(1) {
                let elapsed = stats_start.elapsed().as_secs_f64();
                let fps = stats_frames as f64 / elapsed;
                let cap_ms = if stats_frames > 0 {
                    (stats_cap_us as f64 / stats_frames as f64) / 1000.0
                } else {
                    0.0
                };
                let mode = if stats_zc_frames > 0 || (stats_frames == 0 && drm_card.is_some()) {
                    "dmabuf"
                } else {
                    "cpu"
                };
                let uptime = start_time.elapsed().as_secs();
                crate::log_detail!(
                    "[capture] fps={fps:.1} mode={mode} cap_avg={cap_ms:.2}ms resolution={}x{} uptime={uptime}s",
                    self.width,
                    self.height
                );
                stats_start = Instant::now();
                stats_frames = 0;
                stats_zc_frames = 0;
                stats_cap_us = 0;
            }
        }

        crate::log_detail!("[capture] stopping EVDI capture");
        drop(drm_card);
        unsafe {
            evdi_unregister_buffer(handle, 0);
            evdi_disconnect(handle);
            evdi_close(handle);
        }
        self.handle = None;
        self.device_lease = None;
        crate::log_detail!("[capture] EVDI cleanup complete");
        Ok(())
    }

    fn detach(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe {
                evdi_disconnect(handle);
                evdi_close(handle);
            }
            crate::log_detail!("[capture] EVDI detached");
        }
        self.device_lease = None;
    }
}

impl Drop for EvdiDisplay {
    fn drop(&mut self) {
        self.detach();
    }
}

/// `size_cm` is the panel's real size. Without it (the external display) the EDID carries
/// the 16x10 cm "aspect ratio, size unknown" marker instead.
fn generate_edid(
    display_id: DisplayId,
    width: u32,
    height: u32,
    refresh_hz: u32,
    size_cm: Option<(u8, u8)>,
) -> [u8; 128] {
    let mut edid = [0u8; 128];

    edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    edid[8] = 0x4E;
    edid[9] = 0x58;
    edid[10..16].copy_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00]);
    if display_id == DisplayId::External {
        edid[10..16].copy_from_slice(&[0x02, 0x00, 0x02, 0x00, 0x00, 0x00]);
    }
    edid[16] = 0x01;
    edid[17] = 0x22;
    edid[18] = 0x01;
    edid[19] = 0x04;
    edid[20] = 0xA5;
    // A real size behaves like a real monitor: compositors derive DPI and scale from it and
    // match the touchscreen (same size) to this monitor. GNOME then names it vendor + diagonal
    // ("SRX 11\"") rather than by model. Without a size, EDID's 16x10 cm marker means "aspect
    // ratio encoded instead of size"; Mutter then names the output from the product descriptor
    // and stays at 100% scale.
    let (width_cm, height_cm) = size_cm.unwrap_or((16, 10));
    edid[21] = width_cm;
    edid[22] = height_cm;
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

    // The detailed timing's image size is in mm; keep it equal to the base block's cm.
    let (h_image, v_image) = match size_cm {
        Some((w, h)) => (u32::from(w) * 10, u32::from(h) * 10),
        None => (u32::from(width_cm), u32::from(height_cm)),
    };
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
    d[12] = (h_image & 0xFF) as u8;
    d[13] = (v_image & 0xFF) as u8;
    d[14] = (((h_image >> 8) & 0x0F) << 4 | ((v_image >> 8) & 0x0F)) as u8;
    d[15] = 0x00;
    d[16] = 0x00;
    d[17] = 0x1E;

    // Product/serial IDs stay stable; the session supplies the internal descriptor name.
    let name: &[u8] = match display_id {
        DisplayId::Internal => b"Screx",
        DisplayId::External => b"iPad External",
    };
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

    let serial = match display_id {
        DisplayId::Internal => b"001",
        DisplayId::External => b"002",
    };
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

fn set_monitor_name(edid: &mut [u8; 128], name: &[u8; 13]) {
    edid[77..90].copy_from_slice(name);
    let len = name.iter().rposition(|b| *b != b' ').map_or(0, |i| i + 1);
    if len < 13 {
        edid[77 + len] = b'\n';
    }
    edid[127] = 0u8.wrapping_sub(edid[..127].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)));
}

fn find_available_device(leased: &[c_int]) -> Option<c_int> {
    select_available_device(leased, |dev| unsafe { evdi_check_device(dev) } == 0)
}

fn select_available_device(
    leased: &[c_int],
    mut available: impl FnMut(c_int) -> bool,
) -> Option<c_int> {
    (0..16).find(|dev| !leased.contains(dev) && available(*dev))
}

fn ensure_evdi_module_loaded() -> Result<()> {
    if std::path::Path::new("/sys/module/evdi").exists() {
        return Ok(());
    }

    crate::log_detail!("[capture] evdi module not loaded; trying to load it via modprobe...");
    let status = Command::new("modprobe")
        .arg("evdi")
        .status()
        .context("failed to run modprobe evdi")?;
    if !status.success() {
        anyhow::bail!("modprobe evdi failed with status {status}");
    }

    for _ in 0..20 {
        if std::path::Path::new("/sys/module/evdi").exists() {
            crate::log_detail!("[capture] evdi module loaded");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    anyhow::bail!("modprobe evdi reported success but /sys/module/evdi is still missing")
}

fn find_or_create_device(leased: &[c_int]) -> Result<c_int> {
    ensure_evdi_module_loaded()?;

    if let Some(dev) = find_available_device(leased) {
        crate::log_detail!("[capture] found existing EVDI device at card {dev}");
        return Ok(dev);
    }

    crate::log_detail!("[capture] no EVDI device found, creating one...");
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
        if let Some(dev) = find_available_device(leased) {
            crate::log_detail!("[capture] EVDI device created at card {dev} (attempt {attempt})");
            return Ok(dev);
        }
    }

    anyhow::bail!("evdi_add_device succeeded but no EVDI card became available after 10s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leased_devices_are_not_reused_even_if_reported_available() {
        assert_eq!(select_available_device(&[0], |_| true), Some(1));
        assert_eq!(select_available_device(&[0, 1], |_| true), Some(2));
        assert_eq!(select_available_device(&[], |dev| dev == 4), Some(4));
        assert_eq!(select_available_device(&[4], |dev| dev == 4), None);
        assert_eq!(
            select_available_device(&(0..16).collect::<Vec<_>>(), |_| true),
            None
        );
    }

    #[test]
    fn model_names_preserve_stable_product_and_serial_identity() {
        let mut edid = generate_edid(DisplayId::Internal, 1920, 1080, 60, None);
        set_monitor_name(&mut edid, b"iPad A16 11\" ");
        assert_eq!(&edid[8..16], &[0x4e, 0x58, 1, 0, 1, 0, 0, 0]);
        assert_eq!(&edid[77..90], b"iPad A16 11\"\n");
        assert_eq!(&edid[95..108], b"001\n         ");
        assert_eq!(edid.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)), 0);
        assert_eq!(
            &edid[21..23],
            &[16, 10],
            "Without a known size, use EDID's aspect-as-size marker"
        );
    }

    #[test]
    fn physical_size_is_never_derived_from_stream_resolution() {
        for (width, height) in [(1280, 720), (2360, 1640), (3840, 2160)] {
            let edid = generate_edid(DisplayId::Internal, width, height, 60, None);
            assert_eq!(&edid[21..23], &[16, 10]);
        }
    }

    #[test]
    fn real_size_is_the_same_in_the_base_block_and_the_detailed_timing() {
        let size = crate::capture::edid_size_cm((235, 163));
        assert_eq!(size, (24, 16));
        let edid = generate_edid(DisplayId::Internal, 2360, 1640, 60, Some(size));
        assert_eq!(&edid[21..23], &[24, 16]);
        // Detailed timing image size: 240 x 160 mm, 12 bits each.
        let h_mm = u32::from(edid[66]) | (u32::from(edid[68] >> 4) << 8);
        let v_mm = u32::from(edid[67]) | (u32::from(edid[68] & 0x0F) << 8);
        assert_eq!((h_mm, v_mm), (240, 160));
        assert_eq!(edid.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)), 0);
    }

    #[test]
    fn displays_have_distinct_stable_identities_and_valid_checksums() {
        let internal = generate_edid(DisplayId::Internal, 1920, 1080, 60, None);
        let external = generate_edid(DisplayId::External, 1920, 1080, 60, None);
        assert_ne!(&internal[10..16], &external[10..16]);
        assert_ne!(&internal[72..108], &external[72..108]);
        for id in [DisplayId::Internal, DisplayId::External] {
            for (width, height) in [(1920, 1080), (2360, 1640), (3840, 2160)] {
                let edid = generate_edid(id, width, height, 60, None);
                assert_eq!(
                    edid.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
                    0
                );
                assert_eq!(&edid[8..16], &generate_edid(id, 1280, 720, 30, None)[8..16]);
                assert_eq!(&edid[72..108], &generate_edid(id, 1280, 720, 30, None)[72..108]);
                assert_eq!(u32::from(edid[56]) | (u32::from(edid[58] >> 4) << 8), width);
                assert_eq!(
                    u32::from(edid[59]) | (u32::from(edid[61] >> 4) << 8),
                    height
                );
            }
        }
    }

    #[test]
    fn short_monitor_name_does_not_corrupt_the_edid_header() {
        let edid = generate_edid(DisplayId::External, 1920, 1080, 60, None);
        assert_eq!(&edid[..8], &[0, 255, 255, 255, 255, 255, 255, 0]);
        assert_eq!(&edid[77..90], b"iPad External");
    }
}
