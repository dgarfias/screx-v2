// ---------------------------------------------------------------------------
// Windows virtual webcam backend.
//
// Uses a classic DirectShow capture filter (registered under the system's
// video-capture-sources category) rather than Media Foundation's
// MFCreateVirtualCamera/Frame Server — the same mechanism every UVC webcam
// driver and virtual-camera product (including OBS's own Windows virtual
// camera) has used since Windows XP. This sidesteps Frame Server's Sensor
// Group subsystem entirely, which was found to crash deterministically
// during real streaming regardless of how the media source answered its
// queries — a much newer and less battle-tested code path than classic
// DirectShow. The actual COM filter that Windows loads is built from this
// daemon package as screx_vcam.dll; this file registers that DLL's CLSID as a
// capture device and feeds it MJPEG frames through a named shared-memory
// buffer (the only channel to the other process — see vcam_filter/lib.rs for
// the reader side, which must agree on the exact header layout below).
//
// Like the Linux backend, Windows now uses MJPEG passthrough: the iPad sends
// JPEG frames, and we hand them straight to the virtual camera. The
// downstream DirectShow graph (and browser) handles decompression.
// ---------------------------------------------------------------------------

use std::ffi::c_void;

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

use crate::camera::CameraBackend;

const CAMERA_FRIENDLY_NAME: &str = "Screx Camera";
/// Must match vcam_filter/lib.rs exactly.
const SHARED_MEM_NAME: &str = "Local\\ScrexVCamFrame";
/// [width u32][height u32][fps u32][pad u32][seq u64][data_len u32][pad u32]
const HEADER_SIZE: usize = 32;

pub struct WindowsCamera {
    width: u32,
    height: u32,
    mapping: Option<HANDLE>,
    view: *mut u8,
    seq: u64,
    registered: bool,
}

// Raw pointers/handles here are only ever touched from the capture thread
// that owns this backend; CameraBackend is required to be Send purely so it
// can be boxed and moved into that thread once at construction.
unsafe impl Send for WindowsCamera {}

impl WindowsCamera {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            mapping: None,
            view: std::ptr::null_mut(),
            seq: 0,
            registered: false,
        }
    }

    fn create_shared_mem(&mut self, width: u32, height: u32) -> Result<()> {
        self.destroy_shared_mem();

        let max_payload = (width as usize) * (height as usize) * 2;
        let total_len = HEADER_SIZE + max_payload;

        unsafe {
            let name_w: Vec<u16> = SHARED_MEM_NAME.encode_utf16().chain(Some(0)).collect();
            let mapping = CreateFileMappingW(
                windows::Win32::Foundation::INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                total_len as u32,
                windows::core::PCWSTR(name_w.as_ptr()),
            )
            .context("CreateFileMappingW failed")?;

            let view: MEMORY_MAPPED_VIEW_ADDRESS =
                MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if view.Value.is_null() {
                let _ = CloseHandle(mapping);
                bail!("MapViewOfFile failed");
            }

            self.mapping = Some(mapping);
            self.view = view.Value as *mut u8;
        }
        Ok(())
    }

    fn destroy_shared_mem(&mut self) {
        unsafe {
            if !self.view.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view as *mut c_void,
                });
                self.view = std::ptr::null_mut();
            }
            if let Some(mapping) = self.mapping.take() {
                let _ = CloseHandle(mapping);
            }
        }
    }

    fn write_header(&mut self, width: u32, height: u32, fps: u32) {
        if self.view.is_null() {
            return;
        }
        unsafe {
            (self.view as *mut u32).write_volatile(width);
            (self.view.add(4) as *mut u32).write_volatile(height);
            (self.view.add(8) as *mut u32).write_volatile(fps);
            (self.view.add(16) as *mut u64).write_volatile(0);
            (self.view.add(24) as *mut u32).write_volatile(0);
        }
    }

    fn write_frame(&mut self, jpeg: &[u8]) -> Result<()> {
        if self.view.is_null() {
            return Ok(());
        }
        unsafe {
            let max_payload = (self.width as usize) * (self.height as usize) * 2;
            let n = jpeg.len().min(max_payload);
            let dst = std::slice::from_raw_parts_mut(self.view.add(HEADER_SIZE), max_payload);
            dst[..n].copy_from_slice(&jpeg[..n]);
            // data_len then seq last (release order) — readers only trust
            // data_len/frame bytes once they observe a new seq.
            (self.view.add(24) as *mut u32).write_volatile(n as u32);
            self.seq += 1;
            (self.view.add(16) as *mut u64).write_volatile(self.seq);
        }
        Ok(())
    }
}

pub fn cleanup_stale_registration() {
    if let Err(e) = screx_vcam::unregister() {
        eprintln!("[camera] failed to clean stale Screx Camera registration: {e}");
    }
}

impl CameraBackend for WindowsCamera {
    fn start(&mut self, w: u32, h: u32, fps: u32) -> Result<()> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        self.create_shared_mem(w, h)?;
        self.write_header(w, h, fps);
        self.width = w;
        self.height = h;

        if !self.registered {
            let dll_path = find_vcam_dll_path()
                .context("screx_vcam.dll not found; set SCREX_VCAM_DLL_PATH to its full path")?;
            println!("[camera] registering Screx Camera DLL: {}", dll_path.display());
            screx_vcam::register(&dll_path.to_string_lossy(), CAMERA_FRIENDLY_NAME)
                .context("failed to register Screx Camera capture device")?;
            self.registered = true;
        }

        println!("[camera] Screx Camera virtual webcam ready: {w}x{h}@{fps}");
        Ok(())
    }

    fn write_jpeg(&mut self, jpeg: &[u8]) -> Result<()> {
        if self.view.is_null() {
            return Ok(());
        }
        self.write_frame(jpeg)
    }

    fn stop(&mut self) {
        self.destroy_shared_mem();
        if self.registered {
            if let Err(e) = screx_vcam::unregister() {
                eprintln!("[camera] failed to unregister Screx Camera: {e}");
            } else {
                println!("[camera] Screx Camera unregistered");
            }
            self.registered = false;
        }
    }
}

fn find_vcam_dll_path() -> Option<std::path::PathBuf> {
    if let Ok(env) = std::env::var("SCREX_VCAM_DLL_PATH") {
        let p: std::path::PathBuf = env.into();
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("screx_vcam.dll");
            if p.exists() {
                return Some(p);
            }
        }
    }

    None
}
