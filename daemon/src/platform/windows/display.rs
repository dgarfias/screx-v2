use std::ffi::c_void;
use std::mem::{size_of, MaybeUninit};
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Disable_DevNode, CM_Enable_DevNode, CM_Get_DevNode_Registry_PropertyW,
    CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW, CM_Locate_DevNodeW,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiSetDeviceRegistryPropertyW,
    UpdateDriverForPlugAndPlayDevicesW, CM_DRP_HARDWAREID, CM_LOCATE_DEVNODE_NORMAL,
    CM_LOCATE_DEVNODE_PHANTOM, CR_BUFFER_SMALL, CR_SUCCESS, DICD_GENERATE_ID, DIF_REGISTERDEVICE,
    GUID_DEVCLASS_DISPLAY, INSTALLFLAG_FORCE, SPDRP_HARDWAREID, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, QueryDisplayConfig, SetDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS, SDC_APPLY, SDC_TOPOLOGY_EXTEND,
};
use windows::Win32::Foundation::{BOOL, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
    DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsExW,
    GetMonitorInfoW, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, EDS_RAWMODE, ENUM_DISPLAY_SETTINGS_MODE, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Registry::{
    RegCreateKeyExW, RegSetValueExW, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};

use crate::capture::{CaptureFrame, DisplayBackend, DisplayMode};

const VDD_SETTINGS_PATH: &str = "C:\\VirtualDisplayDriver\\vdd_settings.xml";
const VDD_SETTINGS_BACKUP: &str = "C:\\VirtualDisplayDriver\\vdd_settings.xml.screx-backup";

pub struct WindowsDisplay {
    width: u32,
    height: u32,
    fps: u32,
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    frame_data: Vec<u8>,
    output_width: u32,
    output_height: u32,
    output_left: i32,
    output_top: i32,
    have_frame: bool,
    gdi_device_name: Option<String>,
    cursor_visible: bool,
    cursor_pos: POINT,
    cursor_shape_info: Option<DXGI_OUTDUPL_POINTER_SHAPE_INFO>,
    cursor_shape: Vec<u8>,
    cursor_shape_err_logged: bool,
}

impl WindowsDisplay {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        Ok(Self {
            width,
            height,
            fps,
            device: None,
            context: None,
            duplication: None,
            staging: None,
            frame_data: Vec::new(),
            output_width: 0,
            output_height: 0,
            output_left: 0,
            output_top: 0,
            have_frame: false,
            gdi_device_name: None,
            cursor_visible: false,
            cursor_pos: POINT::default(),
            cursor_shape_info: None,
            cursor_shape: Vec::new(),
            cursor_shape_err_logged: false,
        })
    }
}

impl DisplayBackend for WindowsDisplay {
    fn attach(&mut self, mode: DisplayMode) -> Result<()> {
        self.width = mode.width;
        self.height = mode.height;
        self.fps = mode.fps;

        let enc_width = mode.width & !1;
        let enc_height = mode.height & !1;
        if enc_width == 0 || enc_height == 0 {
            bail!("invalid display mode {}x{}", mode.width, mode.height);
        }

        println!(
            "[display] VDD target mode: {}x{}@{} (encode {}x{})",
            mode.width, mode.height, mode.fps, enc_width, enc_height
        );

        write_vdd_settings(&mode).context("failed to write VDD settings")?;
        set_vdd_config_path().context("failed to set VDD config path registry key")?;

        // The driver only reads vdd_settings.xml once, at devnode start, so an
        // already-running instance won't see the file we just wrote. Force a
        // fresh read by cycling the devnode (disable+enable) if it exists, or
        // create it fresh if it doesn't — either way we end up with a driver
        // process that just started with the current target mode in hand.
        match find_vdd_devinst() {
            Ok(_) => {
                if let Err(e) = cycle_vdd_devnode() {
                    println!("[display] cycle_vdd_devnode failed: {e:#}");
                }
            }
            Err(e) => {
                println!("[display] VDD devnode not found ({e:#}); installing");
                if let Err(e2) = install_vdd_driver() {
                    println!("[display] install_vdd_driver failed: {e2:#}");
                } else {
                    // A freshly registered devnode can take a moment to
                    // become visible to CM_Get_Device_ID_ListW; retry
                    // briefly instead of failing on the first miss.
                    let mut last_err = None;
                    for _ in 1..=10 {
                        match enable_vdd() {
                            Ok(()) => {
                                last_err = None;
                                break;
                            }
                            Err(e3) => {
                                last_err = Some(e3);
                                std::thread::sleep(Duration::from_millis(300));
                            }
                        }
                    }
                    if let Some(e3) = last_err {
                        println!("[display] enable_vdd failed after install: {e3:#}");
                    }
                }
            }
        }

        // The devnode being enabled doesn't mean Windows has added it to the
        // desktop yet — there's no hot-plug interrupt for a software display.
        // Ask Windows to recompute the topology before waiting for it to show up.
        if find_vdd_adapter_name().is_some() {
            if let Err(e) = extend_desktop_topology() {
                println!("[display] extend_desktop_topology failed: {e:#}");
            }
        } else {
            println!("[display] no VDD adapter visible via EnumDisplayDevicesW yet");
        }

        // Accept whatever size it currently reports — it just restarted with
        // the settings file we wrote above, but set_vdd_mode() below is what
        // actually moves it to the target size. Requiring an exact size match
        // here would just spin until the timeout.
        let vdd = find_vdd_display_any(Duration::from_secs(15)).context(
            "virtual display did not appear; is Virtual Display Driver installed and enabled?",
        )?;
        self.gdi_device_name = Some(vdd.device_name.clone());
        println!("[display] virtual display: {}", vdd.device_name);

        set_vdd_mode(&vdd.device_name, enc_width, enc_height, mode.fps)
            .with_context(|| format!("failed to set mode on {}", vdd.device_name))?;

        // Refresh geometry after the mode change.
        let vdd = find_vdd_display(&mode, Duration::from_secs(10))
            .context("virtual display disappeared after mode set")?;
        self.output_width = (vdd.rect.right - vdd.rect.left) as u32;
        self.output_height = (vdd.rect.bottom - vdd.rect.top) as u32;
        self.output_left = vdd.rect.left;
        self.output_top = vdd.rect.top;
        if self.output_width == 0 || self.output_height == 0 {
            bail!("virtual display has zero-sized rectangle");
        }
        println!(
            "[display] virtual monitor rect: {}x{}+{}+{}",
            self.output_width, self.output_height, vdd.rect.left, vdd.rect.top
        );

        self.frame_data = vec![0u8; (self.output_width * self.output_height * 4) as usize];
        self.create_duplication()?;

        Ok(())
    }

    fn run_capture_loop(
        &mut self,
        stop: &AtomicBool,
        _force_refresh: &AtomicBool,
        on_frame: &mut dyn FnMut(CaptureFrame<'_>),
    ) -> Result<()> {
        // Cloned (cheap AddRef, not a deep copy) rather than borrowed from
        // `self` so the loop body below is free to take `&mut self` for
        // cursor handling without fighting the borrow checker. `mut` because
        // DXGI_ERROR_ACCESS_LOST recovery below swaps in freshly recreated
        // handles.
        let mut duplication = self.duplication.clone().context("display not attached")?;
        let mut context = self.context.clone().context("no D3D11 context")?;
        let mut staging = self.staging.clone().context("no staging texture")?;

        let frame_interval = Duration::from_millis(1000 / self.fps.max(1) as u64);
        let mut last_emit = Instant::now();
        // Counts consecutive times we've entered the ACCESS_LOST/INVALID_CALL
        // branch below without a single successful AcquireNextFrame in
        // between. A lightweight `create_duplication()` recreate can itself
        // "succeed" and then have the very next AcquireNextFrame immediately
        // fail again, over and over, if the desktop is still churning — this
        // tracks that so a long-running failure escalates to a harder reset
        // instead of spinning on the cheap path forever.
        let mut consecutive_topology_failures: u32 = 0;

        while !stop.load(Ordering::Relaxed) {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut desktop_resource = None;
            match unsafe { duplication.AcquireNextFrame(0, &mut frame_info, &mut desktop_resource) }
            {
                Ok(()) => {
                    consecutive_topology_failures = 0;
                    unsafe {
                        if let Some(resource) = desktop_resource {
                            let src: ID3D11Texture2D = resource
                                .cast()
                                .context("desktop resource is not a texture")?;
                            context.CopyResource(&staging, &src);

                            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                            context
                                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                                .context("Map(staging) failed")?;

                            let row_pitch = mapped.RowPitch as usize;
                            let height = self.output_height as usize;
                            let width = self.output_width as usize;
                            let expected_pitch = width * 4;

                            if row_pitch == expected_pitch {
                                ptr::copy_nonoverlapping(
                                    mapped.pData as *const u8,
                                    self.frame_data.as_mut_ptr(),
                                    height * row_pitch,
                                );
                            } else {
                                for y in 0..height {
                                    ptr::copy_nonoverlapping(
                                        (mapped.pData as *const u8).add(y * row_pitch),
                                        self.frame_data.as_mut_ptr().add(y * expected_pitch),
                                        expected_pitch,
                                    );
                                }
                            }

                            context.Unmap(&staging, 0);
                            self.have_frame = true;
                        }

                        // DXGI_OUTDUPL_FRAME_INFO.PointerPosition.Visible turned
                        // out to be unreliable in practice — it blipped false
                        // repeatedly during genuine, continuous real-mouse
                        // movement, causing the composited cursor to flicker.
                        // GetCursorInfo() is the plain Win32 "what is the
                        // current global system cursor state" query — the
                        // same ground truth Windows itself uses — and doesn't
                        // have that problem. Its position is in absolute
                        // desktop coordinates, so it does need the output
                        // offset subtracted (unlike DXGI's own position field,
                        // which is already output-relative).
                        let mut cursor_info: CURSORINFO = std::mem::zeroed();
                        cursor_info.cbSize = size_of::<CURSORINFO>() as u32;
                        let visible = GetCursorInfo(&mut cursor_info).is_ok()
                            && (cursor_info.flags.0 & CURSOR_SHOWING.0) != 0;

                        if visible {
                            let new_pos = POINT {
                                x: cursor_info.ptScreenPos.x - self.output_left,
                                y: cursor_info.ptScreenPos.y - self.output_top,
                            };
                            if !self.cursor_visible {
                                eprintln!(
                                    "[display] cursor became visible at ({}, {})",
                                    new_pos.x, new_pos.y
                                );
                            }
                            self.cursor_visible = true;
                            self.cursor_pos = new_pos;
                        } else if self.cursor_visible {
                            eprintln!("[display] cursor became hidden");
                            self.cursor_visible = false;
                        }

                        if frame_info.PointerShapeBufferSize > 0 {
                            match self.update_cursor_shape(
                                &duplication,
                                frame_info.PointerShapeBufferSize,
                            ) {
                                Ok(()) => {}
                                Err(e) => {
                                    if !self.cursor_shape_err_logged {
                                        eprintln!("[display] failed to fetch cursor shape: {e:#}");
                                        self.cursor_shape_err_logged = true;
                                    }
                                }
                            }
                        }

                        if self.have_frame && self.cursor_visible {
                            self.composite_cursor();
                        }

                        let _ = duplication.ReleaseFrame();
                    }
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                Err(e)
                    if e.code() == DXGI_ERROR_ACCESS_LOST
                        || e.code() == DXGI_ERROR_INVALID_CALL =>
                {
                    // Fires whenever the display topology changes — not just
                    // on disconnect. Switching Win+P mode (e.g. Extend <->
                    // Second-screen-only) invalidates the duplication
                    // interface even if the VDD output itself is untouched.
                    // Empirically Windows reports this as either
                    // ACCESS_LOST or, sometimes, INVALID_CALL for the same
                    // kind of topology change — treat both as recoverable.
                    // Recreate it in place instead of tearing down the whole
                    // capture/client session; retry with backoff until the
                    // output is enumerable again (e.g. the user switches
                    // back), honoring `stop` so shutdown isn't blocked.
                    println!("[display] {e:#} (display topology changed?); recreating duplication");
                    self.duplication = None;
                    self.staging = None;
                    self.context = None;
                    self.device = None;

                    consecutive_topology_failures += 1;
                    // A bare topology recompute (create_duplication()'s own
                    // extend-and-retry) can keep "succeeding" and then
                    // immediately failing again on the very next frame,
                    // indefinitely, when a Win+P mode switch (e.g. Extend ->
                    // Second-screen-only, which can drop the VDD from the
                    // topology entirely) leaves the IddCx driver's swap chain
                    // wedged. Escalate every so often to a full driver
                    // restart, the same thing attach() does at session
                    // start, which actually clears it.
                    if consecutive_topology_failures % 12 == 0 {
                        println!(
                            "[display] {consecutive_topology_failures} consecutive topology failures; hard-resetting VDD driver"
                        );
                        if let Err(e) = self.hard_reset_vdd() {
                            println!("[display] hard reset failed: {e:#}");
                        }
                    }

                    let mut last_err_logged = Instant::now() - Duration::from_secs(60);
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        match self.create_duplication() {
                            Ok(()) => {
                                println!("[display] duplication recreated after ACCESS_LOST");
                                break;
                            }
                            Err(e2) => {
                                if last_err_logged.elapsed() >= Duration::from_secs(5) {
                                    println!("[display] still recreating duplication: {e2:#}");
                                    last_err_logged = Instant::now();
                                }
                                std::thread::sleep(Duration::from_millis(250));
                            }
                        }
                    }
                    duplication = self.duplication.clone().context("display not attached")?;
                    context = self.context.clone().context("no D3D11 context")?;
                    staging = self.staging.clone().context("no staging texture")?;

                    // The topology can keep settling for a bit even after
                    // create_duplication() itself succeeds — observed this
                    // spin recreate-succeeds/immediately-fails-again several
                    // dozen times in a row with no delay otherwise. A short
                    // pause here just paces those retries; it doesn't change
                    // whether recovery eventually happens.
                    std::thread::sleep(Duration::from_millis(150));
                }
                Err(e) => bail!("AcquireNextFrame failed: {e:#}"),
            }

            if self.have_frame && last_emit.elapsed() >= frame_interval {
                on_frame(CaptureFrame {
                    width: self.output_width,
                    height: self.output_height,
                    data: &self.frame_data[..(self.output_width * self.output_height * 4) as usize],
                });
                last_emit = Instant::now();
            }

            std::thread::sleep(Duration::from_millis(1));
        }

        Ok(())
    }

    fn detach(&mut self) {
        self.duplication = None;
        self.staging = None;
        self.context = None;
        self.device = None;
        self.have_frame = false;
        self.gdi_device_name = None;
        self.cursor_visible = false;
        self.cursor_shape_info = None;
        self.cursor_shape.clear();
        self.cursor_shape_err_logged = false;

        if let Err(e) = disable_vdd() {
            eprintln!("[display] failed to disable VDD: {e:#}");
        }
    }

    fn output_rect(&self) -> Option<(i32, i32, u32, u32)> {
        Some((
            self.output_left,
            self.output_top,
            self.output_width,
            self.output_height,
        ))
    }
}

// ---------------------------------------------------------------------------
// Cursor compositing
//
// DXGI Desktop Duplication deliberately excludes the cursor from the desktop
// image on every Windows display (real or virtual) — it hands the real
// cursor bitmap and position back separately via GetFramePointerShape() so
// callers can composite it themselves. This is the same technique every
// screen-mirroring tool uses (OBS, Sunshine, RDP, etc.); it is not a
// synthetic/approximated cursor, it's the actual current OS cursor image.
// ---------------------------------------------------------------------------

impl WindowsDisplay {
    /// Full recovery for when repeated lightweight `create_duplication()`
    /// retries don't converge: restarts the VDD driver process (disable +
    /// enable, forcing a fresh IddCx swap chain) and re-establishes its mode
    /// and geometry from scratch, mirroring most of what `attach()` does at
    /// session start. Some Win+P topology transitions (e.g. forcing
    /// "Second screen only" away from the VDD and back to Extend) leave the
    /// driver's swap chain in a state a bare topology recompute can't
    /// unstick — only a fresh driver process reliably clears it.
    fn hard_reset_vdd(&mut self) -> Result<()> {
        println!("[display] hard reset: cycling VDD devnode");
        cycle_vdd_devnode().context("cycle_vdd_devnode failed")?;

        if let Err(e) = extend_desktop_topology() {
            println!("[display] extend_desktop_topology failed during hard reset: {e:#}");
        }

        let enc_width = self.width & !1;
        let enc_height = self.height & !1;

        let vdd = find_vdd_display_any(Duration::from_secs(15))
            .context("virtual display did not reappear after devnode cycle")?;
        self.gdi_device_name = Some(vdd.device_name.clone());

        if let Err(e) = set_vdd_mode(&vdd.device_name, enc_width, enc_height, self.fps) {
            println!("[display] set_vdd_mode failed during hard reset: {e:#}");
        }

        let mode = DisplayMode {
            width: self.width,
            height: self.height,
            fps: self.fps,
        };
        let vdd = find_vdd_display(&mode, Duration::from_secs(10))
            .context("virtual display disappeared again right after hard reset")?;
        self.output_width = (vdd.rect.right - vdd.rect.left) as u32;
        self.output_height = (vdd.rect.bottom - vdd.rect.top) as u32;
        self.output_left = vdd.rect.left;
        self.output_top = vdd.rect.top;
        if self.output_width == 0 || self.output_height == 0 {
            bail!("virtual display has zero-sized rectangle after hard reset");
        }
        self.frame_data = vec![0u8; (self.output_width * self.output_height * 4) as usize];
        println!(
            "[display] hard reset complete: {}x{}+{}+{}",
            self.output_width, self.output_height, self.output_left, self.output_top
        );
        Ok(())
    }

    /// (Re)creates the D3D11 device/duplication/staging texture for the
    /// currently cached `gdi_device_name`. Split out of `attach()` so
    /// `run_capture_loop` can call it again after `DXGI_ERROR_ACCESS_LOST` —
    /// which fires whenever the display topology changes (e.g. the user
    /// switches Win+P mode between Extend/Second-screen-only), not just on
    /// disconnect — without tearing down the whole capture/client session.
    fn create_duplication(&mut self) -> Result<()> {
        // Re-resolve the VDD's current GDI adapter name every time rather
        // than trusting the cached one: a topology change can renumber
        // \\.\DISPLAYn, and EnumDisplayDevicesW (unlike DXGI/QueryDisplayConfig)
        // reports adapters even when they're not currently part of the
        // active desktop, so this also tells us whether the VDD has been
        // dropped from the topology entirely (e.g. the user picked a Win+P
        // mode that only includes their physical monitors).
        let device_name = find_vdd_adapter_name()
            .or_else(|| self.gdi_device_name.clone())
            .context("VDD adapter not found at all; is it still enabled?")?;
        self.gdi_device_name = Some(device_name.clone());

        unsafe {
            // A topology change can move (or resize) where this output sits
            // on the virtual desktop — refresh the cached rect so cursor
            // compositing doesn't keep using a stale offset until the next
            // full attach()/reconnect.
            if let Some(rect) = monitor_rect_for_device(&device_name) {
                let new_width = (rect.right - rect.left) as u32;
                let new_height = (rect.bottom - rect.top) as u32;
                if new_width > 0
                    && new_height > 0
                    && (rect.left != self.output_left
                        || rect.top != self.output_top
                        || new_width != self.output_width
                        || new_height != self.output_height)
                {
                    println!(
                        "[display] output rect changed: {}x{}+{}+{} -> {}x{}+{}+{}",
                        self.output_width,
                        self.output_height,
                        self.output_left,
                        self.output_top,
                        new_width,
                        new_height,
                        rect.left,
                        rect.top
                    );
                    self.output_left = rect.left;
                    self.output_top = rect.top;
                    self.output_width = new_width;
                    self.output_height = new_height;
                    self.frame_data = vec![0u8; (new_width * new_height * 4) as usize];
                }
            }

            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().context("CreateDXGIFactory1 failed")?;

            // The adapter can exist at the GDI level (EnumDisplayDevicesW
            // found it above) while still being absent from DXGI's output
            // list, because Win+P topology changes can drop it from the
            // active desktop without removing the devnode. That's a
            // different failure mode than the VDD being gone outright, and
            // recovers the same way attach() originally brought it up: ask
            // Windows to recompute an extended topology and look again,
            // rather than just bailing out to the outer retry loop and
            // hoping the next iteration happens to be luckier.
            let (adapter, output) = match find_output_by_device_name(&factory, &device_name) {
                Ok(found) => found,
                Err(e) => {
                    println!(
                        "[display] {e:#}; VDD adapter exists but isn't in the active desktop topology, re-extending"
                    );
                    if let Err(e2) = extend_desktop_topology() {
                        println!("[display] extend_desktop_topology retry failed: {e2:#}");
                    }
                    std::thread::sleep(Duration::from_millis(300));
                    find_output_by_device_name(&factory, &device_name)
                        .with_context(|| format!("no DXGI output for {device_name}"))?
                }
            };

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice failed")?;

            let device = device.context("D3D11 device is null")?;
            let context = context.context("D3D11 context is null")?;

            let output1: IDXGIOutput1 = output.cast().context("IDXGIOutput1 cast failed")?;
            let duplication = output1
                .DuplicateOutput(&device)
                .context("DuplicateOutput failed")?;

            let dup_desc: DXGI_OUTDUPL_DESC = duplication.GetDesc();
            let tex_width = dup_desc.ModeDesc.Width;
            let tex_height = dup_desc.ModeDesc.Height;

            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: tex_width,
                Height: tex_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };

            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .context("CreateTexture2D(staging) failed")?;
            let staging = staging.context("CreateTexture2D returned null")?;

            self.device = Some(device);
            self.context = Some(context);
            self.duplication = Some(duplication);
            self.staging = Some(staging);
            self.have_frame = false;
        }
        Ok(())
    }

    unsafe fn update_cursor_shape(
        &mut self,
        duplication: &IDXGIOutputDuplication,
        buffer_size: u32,
    ) -> Result<()> {
        self.cursor_shape.resize(buffer_size as usize, 0);
        let mut required = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        duplication.GetFramePointerShape(
            buffer_size,
            self.cursor_shape.as_mut_ptr() as *mut c_void,
            &mut required,
            &mut info,
        )?;
        self.cursor_shape_info = Some(info);
        Ok(())
    }

    fn composite_cursor(&mut self) {
        let Some(info) = self.cursor_shape_info else {
            return;
        };
        if info.Width == 0 || info.Height == 0 {
            return;
        }

        let origin_x = self.cursor_pos.x - info.HotSpot.x;
        let origin_y = self.cursor_pos.y - info.HotSpot.y;
        let frame_w = self.output_width as i32;
        let frame_h = self.output_height as i32;

        if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
            self.composite_color_cursor(origin_x, origin_y, info, frame_w, frame_h);
        } else if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
            self.composite_masked_color_cursor(origin_x, origin_y, info, frame_w, frame_h);
        } else if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
            self.composite_monochrome_cursor(origin_x, origin_y, info, frame_w, frame_h);
        }
    }

    /// Standard 32bpp BGRA cursor with a real alpha channel — alpha-blend directly.
    fn composite_color_cursor(
        &mut self,
        origin_x: i32,
        origin_y: i32,
        info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        frame_w: i32,
        frame_h: i32,
    ) {
        let pitch = info.Pitch as usize;
        let shape_len = self.cursor_shape.len();
        for row in 0..info.Height as i32 {
            let fy = origin_y + row;
            if fy < 0 || fy >= frame_h {
                continue;
            }
            for col in 0..info.Width as i32 {
                let fx = origin_x + col;
                if fx < 0 || fx >= frame_w {
                    continue;
                }
                let src_off = row as usize * pitch + col as usize * 4;
                if src_off + 4 > shape_len {
                    continue;
                }
                let b = self.cursor_shape[src_off];
                let g = self.cursor_shape[src_off + 1];
                let r = self.cursor_shape[src_off + 2];
                let a = self.cursor_shape[src_off + 3];
                if a == 0 {
                    continue;
                }
                let dst_off = (fy as usize * frame_w as usize + fx as usize) * 4;
                if dst_off + 4 > self.frame_data.len() {
                    continue;
                }
                if a == 255 {
                    self.frame_data[dst_off] = b;
                    self.frame_data[dst_off + 1] = g;
                    self.frame_data[dst_off + 2] = r;
                    self.frame_data[dst_off + 3] = 255;
                } else {
                    let a32 = a as u32;
                    let inv = 255 - a32;
                    self.frame_data[dst_off] =
                        ((b as u32 * a32 + self.frame_data[dst_off] as u32 * inv) / 255) as u8;
                    self.frame_data[dst_off + 1] =
                        ((g as u32 * a32 + self.frame_data[dst_off + 1] as u32 * inv) / 255) as u8;
                    self.frame_data[dst_off + 2] =
                        ((r as u32 * a32 + self.frame_data[dst_off + 2] as u32 * inv) / 255) as u8;
                }
            }
        }
    }

    /// 32bpp BGRA where alpha is either 0x00 (direct color replace) or 0xFF
    /// (XOR with the screen) — the format Windows uses for a handful of
    /// legacy invert-style cursors.
    fn composite_masked_color_cursor(
        &mut self,
        origin_x: i32,
        origin_y: i32,
        info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        frame_w: i32,
        frame_h: i32,
    ) {
        let pitch = info.Pitch as usize;
        let shape_len = self.cursor_shape.len();
        for row in 0..info.Height as i32 {
            let fy = origin_y + row;
            if fy < 0 || fy >= frame_h {
                continue;
            }
            for col in 0..info.Width as i32 {
                let fx = origin_x + col;
                if fx < 0 || fx >= frame_w {
                    continue;
                }
                let src_off = row as usize * pitch + col as usize * 4;
                if src_off + 4 > shape_len {
                    continue;
                }
                let b = self.cursor_shape[src_off];
                let g = self.cursor_shape[src_off + 1];
                let r = self.cursor_shape[src_off + 2];
                let a = self.cursor_shape[src_off + 3];
                let dst_off = (fy as usize * frame_w as usize + fx as usize) * 4;
                if dst_off + 4 > self.frame_data.len() {
                    continue;
                }
                if a == 0xFF {
                    self.frame_data[dst_off] ^= b;
                    self.frame_data[dst_off + 1] ^= g;
                    self.frame_data[dst_off + 2] ^= r;
                } else {
                    self.frame_data[dst_off] = b;
                    self.frame_data[dst_off + 1] = g;
                    self.frame_data[dst_off + 2] = r;
                    self.frame_data[dst_off + 3] = 255;
                }
            }
        }
    }

    /// Classic 1bpp AND+XOR mask cursor. The buffer stacks an AND mask on top
    /// of an XOR mask of the same size (so info.Height is 2x the apparent
    /// cursor height), each row bit-packed and padded to `Pitch` bytes.
    fn composite_monochrome_cursor(
        &mut self,
        origin_x: i32,
        origin_y: i32,
        info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        frame_w: i32,
        frame_h: i32,
    ) {
        let pitch = info.Pitch as usize;
        let cursor_h = (info.Height / 2) as i32;
        let shape_len = self.cursor_shape.len();
        for row in 0..cursor_h {
            let fy = origin_y + row;
            if fy < 0 || fy >= frame_h {
                continue;
            }
            for col in 0..info.Width as i32 {
                let fx = origin_x + col;
                if fx < 0 || fx >= frame_w {
                    continue;
                }
                let byte_col = (col / 8) as usize;
                let bit = 7 - (col % 8) as u32;
                let and_byte_off = row as usize * pitch + byte_col;
                let xor_byte_off = (row as usize + cursor_h as usize) * pitch + byte_col;
                if xor_byte_off >= shape_len {
                    continue;
                }
                let and_bit = (self.cursor_shape[and_byte_off] >> bit) & 1;
                let xor_bit = (self.cursor_shape[xor_byte_off] >> bit) & 1;
                let dst_off = (fy as usize * frame_w as usize + fx as usize) * 4;
                if dst_off + 4 > self.frame_data.len() {
                    continue;
                }
                match (and_bit, xor_bit) {
                    (0, 0) => {
                        self.frame_data[dst_off] = 0;
                        self.frame_data[dst_off + 1] = 0;
                        self.frame_data[dst_off + 2] = 0;
                        self.frame_data[dst_off + 3] = 255;
                    }
                    (0, 1) => {
                        self.frame_data[dst_off] = 255;
                        self.frame_data[dst_off + 1] = 255;
                        self.frame_data[dst_off + 2] = 255;
                        self.frame_data[dst_off + 3] = 255;
                    }
                    (1, 0) => {
                        // AND=1, XOR=0: transparent — leave the screen pixel as-is.
                    }
                    _ => {
                        // AND=1, XOR=1: invert the screen pixel.
                        self.frame_data[dst_off] = !self.frame_data[dst_off];
                        self.frame_data[dst_off + 1] = !self.frame_data[dst_off + 1];
                        self.frame_data[dst_off + 2] = !self.frame_data[dst_off + 2];
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VDD settings file
// ---------------------------------------------------------------------------

fn write_vdd_settings(mode: &DisplayMode) -> Result<()> {
    let path = std::path::Path::new(VDD_SETTINGS_PATH);
    let parent = path.parent().context("vdd_settings path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;

    if path.exists() && !std::path::Path::new(VDD_SETTINGS_BACKUP).exists() {
        std::fs::copy(path, VDD_SETTINGS_BACKUP)
            .with_context(|| format!("failed to back up {}", VDD_SETTINGS_PATH))?;
    }

    let xml = format!(
        "<?xml version='1.0' encoding='utf-8'?>\n\
        <vdd_settings>\n\
        \t<monitors><count>1</count></monitors>\n\
        \t<gpu><friendlyname>default</friendlyname></gpu>\n\
        \t<global>\n\
        \t\t<g_refresh_rate>{fps}</g_refresh_rate>\n\
        \t</global>\n\
        \t<resolutions>\n\
        \t\t<resolution>\n\
        \t\t\t<width>{w}</width>\n\
        \t\t\t<height>{h}</height>\n\
        \t\t\t<refresh_rate>{fps}</refresh_rate>\n\
        \t\t</resolution>\n\
        \t</resolutions>\n\
        \t<logging>\n\
        \t\t<SendLogsThroughPipe>true</SendLogsThroughPipe>\n\
        \t\t<logging>false</logging>\n\
        \t\t<debuglogging>false</debuglogging>\n\
        \t</logging>\n\
        \t<colour>\n\
        \t\t<SDR10bit>false</SDR10bit>\n\
        \t\t<HDRPlus>false</HDRPlus>\n\
        \t\t<ColourFormat>RGB</ColourFormat>\n\
        \t</colour>\n\
        \t<cursor>\n\
        \t\t<HardwareCursor>true</HardwareCursor>\n\
        \t\t<CursorMaxX>128</CursorMaxX>\n\
        \t\t<CursorMaxY>128</CursorMaxY>\n\
        \t\t<AlphaCursorSupport>true</AlphaCursorSupport>\n\
        \t\t<XorCursorSupportLevel>2</XorCursorSupportLevel>\n\
        \t</cursor>\n\
        \t<edid>\n\
        \t\t<CustomEdid>false</CustomEdid>\n\
        \t\t<PreventSpoof>false</PreventSpoof>\n\
        \t\t<EdidCeaOverride>false</EdidCeaOverride>\n\
        \t</edid>\n\
        </vdd_settings>\n",
        w = mode.width,
        h = mode.height,
        fps = mode.fps
    );

    std::fs::write(path, xml).with_context(|| format!("failed to write {}", VDD_SETTINGS_PATH))?;
    Ok(())
}

fn set_vdd_config_path() -> Result<()> {
    unsafe {
        let subkey: Vec<u16> = "SOFTWARE\\MikeTheTech\\VirtualDisplayDriver"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let value_name: Vec<u16> = "VDDPATH".encode_utf16().chain(Some(0)).collect();
        let value_data: Vec<u16> = "C:\\VirtualDisplayDriver"
            .encode_utf16()
            .chain(Some(0))
            .collect();

        let mut hkey = std::mem::zeroed();
        let ret = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if ret.0 != 0 {
            bail!("RegCreateKeyExW failed ({:#?})", ret);
        }

        let value_bytes: &[u8] =
            std::slice::from_raw_parts(value_data.as_ptr() as *const u8, value_data.len() * 2);
        let ret = RegSetValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            0,
            REG_SZ,
            Some(value_bytes),
        );
        if ret.0 != 0 {
            bail!("RegSetValueExW failed ({:#?})", ret);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Devnode enable/disable
// ---------------------------------------------------------------------------

fn find_vdd_devinst() -> Result<u32> {
    unsafe {
        let mut size = 0;
        let ret = CM_Get_Device_ID_List_SizeW(
            &mut size,
            PCWSTR::null(),
            0, // CM_GETIDLIST_FILTER_NONE
        );
        if ret != CR_SUCCESS {
            bail!("CM_Get_Device_ID_List_SizeW failed ({:#?})", ret);
        }
        if size == 0 {
            bail!("no devices found");
        }

        let mut list: Vec<u16> = vec![0; size as usize];
        let ret = CM_Get_Device_ID_ListW(
            PCWSTR::null(),
            &mut list,
            0, // CM_GETIDLIST_FILTER_NONE
        );
        if ret != CR_SUCCESS {
            bail!("CM_Get_Device_ID_ListW failed ({:#?})", ret);
        }

        let ids: Vec<String> = null_terminated_multi_sz(&list);
        eprintln!(
            "[display] CM_Get_Device_ID_ListW: size={size} chars, parsed {} device ids",
            ids.len()
        );
        let mut candidates = Vec::new();
        let mut locate_failures = 0u32;
        for id in ids {
            if id.is_empty() {
                continue;
            }
            let id_ws: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
            let mut devinst = 0;
            let ret = CM_Locate_DevNodeW(
                &mut devinst,
                PCWSTR(id_ws.as_ptr()),
                CM_LOCATE_DEVNODE_NORMAL,
            );
            let ret = if ret != CR_SUCCESS {
                CM_Locate_DevNodeW(
                    &mut devinst,
                    PCWSTR(id_ws.as_ptr()),
                    CM_LOCATE_DEVNODE_PHANTOM,
                )
            } else {
                ret
            };
            if ret != CR_SUCCESS {
                locate_failures += 1;
                continue;
            }
            let hw_ids = devnode_hardware_ids(devinst).unwrap_or_default();
            let desc = devnode_description(devinst).unwrap_or_default();
            let class_guid = devnode_class_guid(devinst).unwrap_or_default();
            let is_display = class_guid.eq_ignore_ascii_case(DISPLAY_CLASS_GUID);
            for hw in &hw_ids {
                if is_vdd_hardware_id(hw) || is_vdd_description(&desc) {
                    return Ok(devinst);
                }
            }
            // Print display-class devices and anything that smells like VDD.
            if is_display || is_vdd_description(&desc) || is_vdd_hardware_id_any(&hw_ids) {
                candidates.push((id.clone(), class_guid, hw_ids.clone(), desc.clone()));
            }
        }

        eprintln!("[display] display-class and VDD-like devices found:");
        for (id, class_guid, hw_ids, desc) in &candidates {
            eprintln!("  instance={id}");
            eprintln!("    class={class_guid} desc={desc}");
            for hw in hw_ids {
                eprintln!("    hwid={hw}");
            }
        }
        if candidates.is_empty() {
            eprintln!("[display] no candidate display devices found at all (CM_Locate_DevNodeW failed for {locate_failures} ids)");
        }
        bail!("Virtual Display Driver devnode not found; expected hardware id or description containing VDD/VirtualDisplayDriver/Virtual Display Driver");
    }
}

fn is_vdd_hardware_id_any(hw_ids: &[String]) -> bool {
    hw_ids.iter().any(|h| is_vdd_hardware_id(h))
}

fn is_vdd_description(desc: &str) -> bool {
    let lower = desc.to_ascii_lowercase();
    lower.contains("virtual display driver")
        || lower.contains("virtualdisplaydriver")
        || lower.contains("mttvdd")
        || lower.contains("indirect display")
}

fn is_vdd_hardware_id(hw: &str) -> bool {
    let lower = hw.to_ascii_lowercase();
    lower.contains("mttvdd")
        || lower.contains("virtualdisplaydriver")
        || lower.contains("virtual display driver")
        || lower.contains("vdd")
}

unsafe fn devnode_hardware_ids(devinst: u32) -> Result<Vec<String>> {
    let mut buf_len = 0;
    let ret =
        CM_Get_DevNode_Registry_PropertyW(devinst, CM_DRP_HARDWAREID, None, None, &mut buf_len, 0);
    if ret != CR_SUCCESS && ret != CR_BUFFER_SMALL {
        bail!(
            "CM_Get_DevNode_Registry_PropertyW size query failed ({:#?})",
            ret
        );
    }
    if buf_len == 0 {
        return Ok(Vec::new());
    }

    let mut buf: Vec<u8> = vec![0; buf_len as usize];
    let ret = CM_Get_DevNode_Registry_PropertyW(
        devinst,
        CM_DRP_HARDWAREID,
        None,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut buf_len,
        0,
    );
    if ret != CR_SUCCESS {
        bail!("CM_Get_DevNode_Registry_PropertyW failed ({:#?})", ret);
    }

    let wide: &[u16] = std::slice::from_raw_parts(buf.as_ptr() as *const u16, buf.len() / 2);
    Ok(null_terminated_multi_sz(wide))
}

const CM_REGISTRY_DEVICE_DESC: u32 = 1; // SPDRP_DEVICEDESC
const CM_DRP_CLASSGUID: u32 = 9;
const DISPLAY_CLASS_GUID: &str = "{4d36e968-e325-11ce-bfc1-08002be10318}";

unsafe fn devnode_description(devinst: u32) -> Result<String> {
    let mut buf_len = 0;
    let ret = CM_Get_DevNode_Registry_PropertyW(
        devinst,
        CM_REGISTRY_DEVICE_DESC,
        None,
        None,
        &mut buf_len,
        0,
    );
    if ret != CR_SUCCESS && ret != CR_BUFFER_SMALL {
        bail!(
            "CM_Get_DevNode_Registry_PropertyW desc size query failed ({:#?})",
            ret
        );
    }
    if buf_len == 0 {
        return Ok(String::new());
    }

    let mut buf: Vec<u8> = vec![0; buf_len as usize];
    let ret = CM_Get_DevNode_Registry_PropertyW(
        devinst,
        CM_REGISTRY_DEVICE_DESC,
        None,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut buf_len,
        0,
    );
    if ret != CR_SUCCESS {
        bail!("CM_Get_DevNode_Registry_PropertyW desc failed ({:#?})", ret);
    }

    let wide: &[u16] = std::slice::from_raw_parts(buf.as_ptr() as *const u16, buf.len() / 2);
    let ids = null_terminated_multi_sz(wide);
    Ok(ids.into_iter().next().unwrap_or_default())
}

unsafe fn devnode_class_guid(devinst: u32) -> Result<String> {
    let mut buf_len = 0;
    let ret =
        CM_Get_DevNode_Registry_PropertyW(devinst, CM_DRP_CLASSGUID, None, None, &mut buf_len, 0);
    if ret != CR_SUCCESS && ret != CR_BUFFER_SMALL {
        bail!(
            "CM_Get_DevNode_Registry_PropertyW classguid size query failed ({:#?})",
            ret
        );
    }
    if buf_len == 0 {
        return Ok(String::new());
    }

    let mut buf: Vec<u8> = vec![0; buf_len as usize];
    let ret = CM_Get_DevNode_Registry_PropertyW(
        devinst,
        CM_DRP_CLASSGUID,
        None,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut buf_len,
        0,
    );
    if ret != CR_SUCCESS {
        bail!(
            "CM_Get_DevNode_Registry_PropertyW classguid failed ({:#?})",
            ret
        );
    }

    let wide: &[u16] = std::slice::from_raw_parts(buf.as_ptr() as *const u16, buf.len() / 2);
    let ids = null_terminated_multi_sz(wide);
    Ok(ids.into_iter().next().unwrap_or_default())
}

fn enable_vdd() -> Result<()> {
    unsafe {
        let devinst = find_vdd_devinst()?;
        let ret = CM_Enable_DevNode(devinst, 0);
        if ret != CR_SUCCESS {
            bail!("CM_Enable_DevNode failed ({:#?})", ret);
        }
        Ok(())
    }
}

/// Disables then re-enables the devnode, forcing its driver process to exit
/// and restart — which is the only way to make it pick up the vdd_settings.xml
/// we just wrote, since it only reads that file once at devnode start.
fn cycle_vdd_devnode() -> Result<()> {
    unsafe {
        let devinst = find_vdd_devinst()?;
        let ret = CM_Disable_DevNode(devinst, 0);
        if ret != CR_SUCCESS {
            bail!("CM_Disable_DevNode failed ({:#?})", ret);
        }
        std::thread::sleep(Duration::from_millis(500));
        let ret = CM_Enable_DevNode(devinst, 0);
        if ret != CR_SUCCESS {
            bail!("CM_Enable_DevNode failed ({:#?})", ret);
        }
        Ok(())
    }
}

const VDD_HARDWARE_ID: &str = "Root\\MttVDD";

/// Creates a root-enumerated devnode for the VDD and binds the driver to it.
///
/// The driver package being present in the driver store (e.g. after
/// `pnputil /add-driver`) does not create a device instance for a
/// root-enumerated ("Root\...") hardware ID; something has to explicitly
/// register one. This mirrors what `devcon.exe install` does under the hood:
/// SetupDiCreateDeviceInfo + SetupDiSetDeviceRegistryProperty(HARDWAREID) +
/// SetupDiCallClassInstaller(DIF_REGISTERDEVICE), then bind the driver with
/// UpdateDriverForPlugAndPlayDevicesW. (CM_Create_DevNodeW is the wrong tool
/// here; it is meant for bus drivers enumerating child devices, not user-mode
/// root-device registration, and fails with CR_INVALID_DEVINST.)
fn install_vdd_driver() -> Result<()> {
    let inf_path = find_vdd_inf_path()
        .context("MttVDD.inf not found; set SCREX_VDD_INF_PATH to its full path")?;
    println!(
        "[display] installing VDD driver from {}",
        inf_path.display()
    );

    unsafe {
        let dev_info = SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_DISPLAY), None)
            .context("SetupDiCreateDeviceInfoList failed")?;

        let register = (|| -> Result<()> {
            let name_ws: Vec<u16> = "MttVDD".encode_utf16().chain(Some(0)).collect();
            let mut devinfo_data = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };

            SetupDiCreateDeviceInfoW(
                dev_info,
                PCWSTR(name_ws.as_ptr()),
                &GUID_DEVCLASS_DISPLAY,
                PCWSTR::null(),
                None,
                DICD_GENERATE_ID,
                Some(&mut devinfo_data),
            )
            .context("SetupDiCreateDeviceInfoW failed")?;

            // REG_MULTI_SZ: hardware id followed by two NULs.
            let mut hwid_bytes: Vec<u8> = VDD_HARDWARE_ID
                .encode_utf16()
                .flat_map(|c| c.to_le_bytes())
                .collect();
            hwid_bytes.extend_from_slice(&[0u8, 0, 0, 0]);

            SetupDiSetDeviceRegistryPropertyW(
                dev_info,
                &mut devinfo_data,
                SPDRP_HARDWAREID,
                Some(&hwid_bytes),
            )
            .context("SetupDiSetDeviceRegistryPropertyW failed")?;

            SetupDiCallClassInstaller(DIF_REGISTERDEVICE, dev_info, Some(&devinfo_data))
                .context("SetupDiCallClassInstaller(DIF_REGISTERDEVICE) failed")?;

            Ok(())
        })();

        let _ = SetupDiDestroyDeviceInfoList(dev_info);
        // Ignore "already such devnode" so re-running after a partial failure still proceeds to bind the driver.
        if let Err(e) = register {
            println!("[display] devnode registration: {e:#} (continuing to driver bind)");
        }

        let inf_ws: Vec<u16> = inf_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let hw_id_ws: Vec<u16> = VDD_HARDWARE_ID.encode_utf16().chain(Some(0)).collect();
        let mut reboot = BOOL(0);
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            PCWSTR(hw_id_ws.as_ptr()),
            PCWSTR(inf_ws.as_ptr()),
            INSTALLFLAG_FORCE,
            Some(&mut reboot),
        )
        .context("UpdateDriverForPlugAndPlayDevicesW failed")?;

        if reboot.as_bool() {
            println!("[display] driver install requires a reboot");
        }
        Ok(())
    }
}

fn find_vdd_inf_path() -> Option<std::path::PathBuf> {
    if let Ok(env) = std::env::var("SCREX_VDD_INF_PATH") {
        let p: std::path::PathBuf = env.into();
        if p.exists() {
            return Some(p);
        }
    }

    // Check the directory containing screx.exe.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("MttVDD.inf");
            if p.exists() {
                return Some(p);
            }
        }
    }

    let candidates = [
        r"C:\VirtualDisplayDriver\MttVDD.inf",
        r"C:\Program Files\VirtualDisplayDriver\MttVDD.inf",
        r"C:\Program Files (x86)\VirtualDisplayDriver\MttVDD.inf",
    ];
    for c in &candidates {
        let p = std::path::Path::new(c);
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }

    // Search the Windows driver store for a published INF that knows Root\MttVDD.
    if let Ok(entries) = std::fs::read_dir(r"C:\Windows\INF") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy().to_ascii_lowercase();
                if name.starts_with("oem") && name.ends_with(".inf") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if content.to_ascii_lowercase().contains("root\\mttvdd") {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }

    None
}

fn disable_vdd() -> Result<()> {
    unsafe {
        let devinst = find_vdd_devinst()?;
        let ret = CM_Disable_DevNode(devinst, 0);
        if ret != CR_SUCCESS {
            bail!("CM_Disable_DevNode failed ({:#?})", ret);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Display discovery / mode-set
// ---------------------------------------------------------------------------

struct VddDisplay {
    device_name: String,
    rect: RECT,
}

fn find_vdd_display(mode: &DisplayMode, timeout: Duration) -> Result<VddDisplay> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(vdd) = try_find_vdd_display(mode, true) {
            return Ok(vdd);
        }
        if Instant::now() >= deadline {
            dump_all_display_devices();
            bail!("timed out waiting for virtual display");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Diagnostic dump of every GDI display device Windows currently reports,
/// active or not. Used when we give up waiting for the VDD to show up, so a
/// failure comes with full data instead of requiring another round trip.
fn dump_all_display_devices() {
    unsafe {
        eprintln!("[display] EnumDisplayDevicesW dump (all adapters/monitors):");
        let mut idx = 0;
        let mut any = false;
        loop {
            let mut dd: DISPLAY_DEVICEW = std::mem::zeroed();
            dd.cb = size_of::<DISPLAY_DEVICEW>() as u32;
            if !EnumDisplayDevicesW(PCWSTR::null(), idx, &mut dd, EDS_RAWMODE.0).as_bool() {
                break;
            }
            any = true;
            let device_name = wide_to_string(&dd.DeviceName);
            let device_string = wide_to_string(&dd.DeviceString);
            eprintln!(
                "  [{idx}] name={device_name} string={device_string} state=0x{:08X}",
                dd.StateFlags
            );
            idx += 1;
        }
        if !any {
            eprintln!("  (EnumDisplayDevicesW returned no adapters at all)");
        }
    }
}

fn find_vdd_display_any(timeout: Duration) -> Result<VddDisplay> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(vdd) = try_find_vdd_display(
            &DisplayMode {
                width: 0,
                height: 0,
                fps: 0,
            },
            false,
        ) {
            return Ok(vdd);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for virtual display");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn try_find_vdd_display(mode: &DisplayMode, require_size: bool) -> Option<VddDisplay> {
    unsafe {
        let mut idx = 0;
        loop {
            let mut dd: DISPLAY_DEVICEW = std::mem::zeroed();
            dd.cb = size_of::<DISPLAY_DEVICEW>() as u32;
            if !EnumDisplayDevicesW(PCWSTR::null(), idx, &mut dd, EDS_RAWMODE.0).as_bool() {
                break;
            }
            idx += 1;

            let device_name = wide_to_string(&dd.DeviceName);
            let device_string = wide_to_string(&dd.DeviceString);
            if !is_vdd_device(&device_string, &device_name) {
                continue;
            }

            if let Some(rect) = monitor_rect_for_device(&device_name) {
                let w = (rect.right - rect.left) as u32;
                let h = (rect.bottom - rect.top) as u32;
                if !require_size || matches_size(w, h, mode.width, mode.height) {
                    return Some(VddDisplay { device_name, rect });
                }
            }
        }
        None
    }
}

fn is_vdd_device(device_string: &str, device_name: &str) -> bool {
    let s = device_string.to_ascii_lowercase();
    s.contains("virtual display driver")
        || s.contains("mttvdd")
        || s.contains("virtualdisplaydriver")
        || device_name.to_ascii_lowercase().contains("vdd")
}

/// Finds the VDD's GDI adapter name even if Windows hasn't attached it to the
/// desktop yet (EnumDisplayDevicesW reports adapters regardless of whether
/// they're part of the active desktop; QueryDisplayConfig/EnumDisplayMonitors
/// only report active ones).
fn find_vdd_adapter_name() -> Option<String> {
    unsafe {
        let mut idx = 0;
        loop {
            let mut dd: DISPLAY_DEVICEW = std::mem::zeroed();
            dd.cb = size_of::<DISPLAY_DEVICEW>() as u32;
            if !EnumDisplayDevicesW(PCWSTR::null(), idx, &mut dd, EDS_RAWMODE.0).as_bool() {
                return None;
            }
            idx += 1;
            let device_name = wide_to_string(&dd.DeviceName);
            let device_string = wide_to_string(&dd.DeviceString);
            if is_vdd_device(&device_string, &device_name) {
                return Some(device_name);
            }
        }
    }
}

/// A freshly created IddCx devnode doesn't automatically become part of the
/// Windows desktop the way a hot-plugged physical monitor does — there's no
/// hot-plug interrupt to trigger it. This asks Windows to recompute the
/// display topology to extend across all currently available displays and
/// apply it immediately — the same thing Windows does internally for
/// Win+P -> Extend / Settings -> "Detect".  (The legacy ChangeDisplaySettingsEx
/// dance was tried first and consistently rejected with DISP_CHANGE_FAILED;
/// SetDisplayConfig with an auto-generated topology is the modern, documented
/// equivalent and doesn't require hand-building a path/mode array.)
fn extend_desktop_topology() -> Result<()> {
    unsafe {
        let ret = SetDisplayConfig(None, None, SDC_TOPOLOGY_EXTEND | SDC_APPLY);
        if ret != 0 {
            bail!("SetDisplayConfig(TOPOLOGY_EXTEND) returned {ret}");
        }
        Ok(())
    }
}

fn matches_size(w: u32, h: u32, target_w: u32, target_h: u32) -> bool {
    let dw = if w > target_w {
        w - target_w
    } else {
        target_w - w
    };
    let dh = if h > target_h {
        h - target_h
    } else {
        target_h - h
    };
    dw <= 2 && dh <= 2
}

unsafe fn monitor_rect_for_device(device_name: &str) -> Option<RECT> {
    // Prefer QueryDisplayConfig for precise source mode / position.
    let mut path_count = 0;
    let mut mode_count = 0;
    let _ = QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut path_count,
        ptr::null_mut(),
        &mut mode_count,
        ptr::null_mut(),
        None,
    );
    if path_count > 0 && mode_count > 0 {
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
            vec![MaybeUninit::zeroed().assume_init(); path_count as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            vec![MaybeUninit::zeroed().assume_init(); mode_count as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .0 == 0
        {
            for path in &paths[..path_count as usize] {
                let mut target_name: DISPLAYCONFIG_TARGET_DEVICE_NAME = std::mem::zeroed();
                target_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
                target_name.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
                target_name.header.adapterId = path.targetInfo.adapterId;
                target_name.header.id = path.targetInfo.id;
                if DisplayConfigGetDeviceInfo(&mut target_name.header) == 0 {
                    let name = wide_to_string(&target_name.monitorFriendlyDeviceName);
                    if is_vdd_device(&name, device_name) {
                        let source_mode_idx = path.sourceInfo.Anonymous.modeInfoIdx;
                        if source_mode_idx != u32::MAX {
                            if let Some(mode) = modes.iter().find(|m| m.id == source_mode_idx) {
                                let info = &mode.Anonymous.sourceMode;
                                return Some(RECT {
                                    left: info.position.x,
                                    top: info.position.y,
                                    right: info.position.x + info.width as i32,
                                    bottom: info.position.y + info.height as i32,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: enumerate HMONITORs and match by MONITORINFOEXW::szDevice.
    let mut data = EnumMonitorData {
        target: device_name.to_string(),
        rect: None,
    };
    let _ = EnumDisplayMonitors(
        None,
        None,
        Some(monitor_enum_proc),
        LPARAM(&mut data as *mut _ as isize),
    );
    data.rect
}

struct EnumMonitorData {
    target: String,
    rect: Option<RECT>,
}

extern "system" fn monitor_enum_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _lprc_monitor: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    unsafe {
        let data = &mut *(lparam.0 as *mut EnumMonitorData);
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            let name = wide_to_string(&info.szDevice);
            if name.eq_ignore_ascii_case(&data.target) {
                data.rect = Some(info.monitorInfo.rcMonitor);
                return BOOL(0); // stop enumerating
            }
        }
        TRUE
    }
}

/// Finds a mode the adapter actually advertises for the given size, preferring
/// an exact refresh-rate match and otherwise the closest one. ChangeDisplaySettingsExW
/// rejects hand-built DEVMODEs that don't correspond to a mode the driver listed
/// (DISP_CHANGE_BADMODE), so this is required, not just a nicety.
unsafe fn find_advertised_mode(
    device_name: &str,
    width: u32,
    height: u32,
    fps: u32,
) -> Option<DEVMODEW> {
    let name_ws: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();
    let mut best: Option<DEVMODEW> = None;
    let mut best_fps_diff = u32::MAX;
    let mut mode_idx = 0u32;
    loop {
        let mut candidate: DEVMODEW = std::mem::zeroed();
        candidate.dmSize = size_of::<DEVMODEW>() as u16;
        if !EnumDisplaySettingsExW(
            PCWSTR(name_ws.as_ptr()),
            ENUM_DISPLAY_SETTINGS_MODE(mode_idx),
            &mut candidate,
            windows::Win32::Graphics::Gdi::ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
        .as_bool()
        {
            break;
        }
        mode_idx += 1;

        if candidate.dmPelsWidth != width || candidate.dmPelsHeight != height {
            continue;
        }
        let fps_diff = candidate.dmDisplayFrequency.abs_diff(fps);
        if fps_diff < best_fps_diff {
            best_fps_diff = fps_diff;
            best = Some(candidate);
            if fps_diff == 0 {
                break;
            }
        }
    }
    best
}

fn set_vdd_mode(device_name: &str, width: u32, height: u32, fps: u32) -> Result<()> {
    unsafe {
        let name_ws: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();

        let mut devmode = find_advertised_mode(device_name, width, height, fps).unwrap_or_else(|| {
            eprintln!("[display] no advertised mode matched {width}x{height}@{fps}; advertised modes are:");
            let mut idx = 0u32;
            loop {
                let mut m: DEVMODEW = std::mem::zeroed();
                m.dmSize = size_of::<DEVMODEW>() as u16;
                if !EnumDisplaySettingsExW(
                    PCWSTR(name_ws.as_ptr()),
                    ENUM_DISPLAY_SETTINGS_MODE(idx),
                    &mut m,
                    windows::Win32::Graphics::Gdi::ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
                .as_bool()
                {
                    break;
                }
                eprintln!(
                    "  [{idx}] {}x{}@{} bpp={}",
                    m.dmPelsWidth, m.dmPelsHeight, m.dmDisplayFrequency, m.dmBitsPerPel
                );
                idx += 1;
            }
            let mut dm: DEVMODEW = std::mem::zeroed();
            dm.dmSize = size_of::<DEVMODEW>() as u16;
            dm.dmFields = windows::Win32::Graphics::Gdi::DEVMODE_FIELD_FLAGS(
                (DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY).0 as u32,
            );
            dm.dmPelsWidth = width;
            dm.dmPelsHeight = height;
            dm.dmDisplayFrequency = fps;
            dm
        });
        devmode.dmFields = windows::Win32::Graphics::Gdi::DEVMODE_FIELD_FLAGS(
            (DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY).0 as u32,
        );
        eprintln!(
            "[display] submitting devmode: {}x{}@{} bpp={} fields=0x{:X}",
            devmode.dmPelsWidth,
            devmode.dmPelsHeight,
            devmode.dmDisplayFrequency,
            devmode.dmBitsPerPel,
            devmode.dmFields.0
        );

        let ret = ChangeDisplaySettingsExW(
            PCWSTR(name_ws.as_ptr()),
            Some(&devmode),
            None,
            CDS_UPDATEREGISTRY,
            None,
        );
        if ret.0 != 0 {
            bail!("ChangeDisplaySettingsExW returned {}", ret.0);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DXGI output lookup
// ---------------------------------------------------------------------------

unsafe fn find_output_by_device_name(
    factory: &IDXGIFactory1,
    device_name: &str,
) -> Result<(IDXGIAdapter, IDXGIOutput)> {
    let mut adapter_idx = 0;
    loop {
        let adapter = match factory.EnumAdapters(adapter_idx) {
            Ok(a) => a,
            Err(_) => bail!("no DXGI adapter with matching output"),
        };

        let mut output_idx = 0;
        loop {
            match adapter.EnumOutputs(output_idx) {
                Ok(output) => {
                    let desc: DXGI_OUTPUT_DESC = output.GetDesc()?;
                    let output_name = wide_to_string(&desc.DeviceName);
                    if output_name.eq_ignore_ascii_case(device_name) {
                        return Ok((adapter, output));
                    }
                }
                Err(_) => break,
            }
            output_idx += 1;
        }
        adapter_idx += 1;
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn null_terminated_multi_sz(buf: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &c) in buf.iter().enumerate() {
        if c == 0 {
            if i == start {
                break;
            }
            let s = String::from_utf16_lossy(&buf[start..i]);
            out.push(s);
            start = i + 1;
        }
    }
    out
}

fn wide_to_string(arr: &[u16]) -> String {
    let len = arr.iter().position(|&c| c == 0).unwrap_or(arr.len());
    String::from_utf16_lossy(&arr[..len])
}
