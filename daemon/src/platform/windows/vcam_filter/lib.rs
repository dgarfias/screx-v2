#![cfg(windows)]

// ---------------------------------------------------------------------------
// Screx virtual camera — DirectShow capture filter.
//
// This is a classic DirectShow video capture *source filter* (IBaseFilter +
// one output IPin), registered under the system's video-capture-sources
// category (CLSID_VideoInputDeviceCategory). The pin exposes a single MJPEG
// stream — the same compressed format the iPad camera sends — so the
// downstream graph (or browser) handles decompression, just like it does for
// a hardware MJPEG webcam.
//
// This path avoids Media Foundation's Frame Server / Sensor Group subsystem,
// which we found crashes deterministically on at least one real Windows 11
// system regardless of how our media source answered its queries.
//
// Windows' own long-standing compatibility bridge exposes any registered
// DirectShow capture filter to modern Media-Foundation-based consumers
// (browsers' getUserMedia, the Camera app, WinRT MediaCapture) automatically
// — so registering here is sufficient for both legacy and modern consumers.
//
// This DLL is loaded in-process by whatever application opens the device
// (there is no separate broker process the way Frame Server uses one for
// Media Foundation virtual cameras). It has no direct connection to the
// screx daemon process — the only channel between them is a named
// shared-memory frame buffer that the daemon writes into (see
// daemon/src/platform/windows/vcam.rs, which must agree on the exact layout
// below).
// ---------------------------------------------------------------------------

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{implement, ComObject, IUnknownImpl, Interface, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    BOOL, CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOTIMPL,
    E_POINTER, HANDLE, RECT, SIZE, S_FALSE, S_OK,
};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::DirectShow::{
    IAMStreamConfig, IAMStreamConfig_Impl, IBaseFilter, IBaseFilter_Impl, IEnumMediaTypes,
    IEnumMediaTypes_Impl, IEnumPins, IEnumPins_Impl, IFilterGraph, IFilterMapper2, IMediaFilter,
    IMediaFilter_Impl, IMediaSample, IMemAllocator, IMemInputPin, IPin, IPin_Impl, State_Paused,
    State_Running, State_Stopped, ALLOCATOR_PROPERTIES, AMPROPERTY_PIN_CATEGORY,
    E_PROP_SET_UNSUPPORTED, FILTER_INFO, FILTER_STATE, MERIT_NORMAL, PINDIR_OUTPUT, PIN_DIRECTION,
    PIN_INFO, REGFILTER2, REGFILTER2_0, REGFILTER2_0_0, REGFILTERPINS, REGPINTYPES,
    VFW_E_INVALIDMEDIATYPE, VFW_E_NOT_CONNECTED, VFW_E_NOT_FOUND, VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::KernelStreaming::{
    IKsPropertySet, IKsPropertySet_Impl, PINNAME_VIDEO_CAPTURE,
};
use windows::Win32::Media::MediaFoundation::{
    MFMediaType_Video as MEDIATYPE_VIDEO_GUID, AM_MEDIA_TYPE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemAlloc, CoTaskMemFree, IClassFactory_Impl, IPersist, IPersist_Impl,
    CLSCTX_INPROC_SERVER,
};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// windows-rs interface wrappers aren't `Send` by default (COM apartment
/// rules in general), but the specific interfaces we carry into our push
/// thread (`IMemInputPin`, `IMemAllocator`) are only ever touched through
/// synchronous, self-contained calls there.
struct SendComPtr<T>(T);
unsafe impl<T> Send for SendComPtr<T> {}
impl<T> SendComPtr<T> {
    /// A method call (rather than `.0` field projection) forces Rust's
    /// disjoint-closure-capture analysis to capture the whole wrapper (and
    /// thus use its `Send` impl) instead of reaching through to the inner
    /// non-Send field directly.
    fn into_inner(self) -> T {
        self.0
    }
}

/// Must match daemon/src/platform/windows/vcam.rs exactly.
const CLSID_SCREX_VCAM: GUID = GUID::from_u128(0x9b8f44ac_cd48_4a05_a396_32050774fb25);
const SHARED_MEM_NAME: &str = "Local\\ScrexVCamFrame";
/// [width u32][height u32][fps u32][pad u32][seq u64][data_len u32][pad u32]
const HEADER_SIZE: usize = 32;
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const DEFAULT_FPS: u32 = 30;
const PIN_NAME: &str = "Output";

// Well-known DirectShow GUIDs not bound as named constants in windows-rs
// (verified against the canonical strmiids definitions).
const CLSID_VIDEO_INPUT_DEVICE_CATEGORY: GUID =
    GUID::from_u128(0x860bb310_5d01_11d0_bd3b_00a0c911ce86);
const CLSID_FILTER_MAPPER2: GUID = GUID::from_u128(0xcda42200_bd88_11d0_bd4e_00a0c911ce86);
const CLSID_MEMORY_ALLOCATOR: GUID = GUID::from_u128(0x1e651cc0_b199_11d0_8212_00c04fc32c45);
const FORMAT_VIDEO_INFO: GUID = GUID::from_u128(0x05589f80_c356_11ce_bf01_00aa0055595a);
const MEDIASUBTYPE_MJPG: GUID = GUID::from_u128(0x47504A4D_0000_0010_8000_00AA00389B71);
/// For `IKsPropertySet`. `PINNAME_VIDEO_CAPTURE` (already bound in
/// windows-rs, imported above) happens to share this exact GUID value with
/// the classic `PIN_CATEGORY_CAPTURE` constant, so it's reused directly
/// instead of redefining an identical constant under a second name.
const AMPROPSETID_PIN: GUID = GUID::from_u128(0x9b00f101_1567_11d1_b3f1_00aa003761c5);

// ---------------------------------------------------------------------------
// Shared memory reader — daemon owns creation, we only ever open by name.
// ---------------------------------------------------------------------------

struct SharedFrameSource {
    _mapping: HANDLE,
    view: *const u8,
    view_len: usize,
    width: u32,
    height: u32,
    fps: u32,
}

unsafe impl Send for SharedFrameSource {}

impl SharedFrameSource {
    fn open() -> Option<Self> {
        unsafe {
            let name_w: Vec<u16> = SHARED_MEM_NAME.encode_utf16().chain(Some(0)).collect();
            let mapping = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name_w.as_ptr())).ok()?;
            let view: MEMORY_MAPPED_VIEW_ADDRESS = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                return None;
            }

            let ptr = view.Value as *const u8;
            let width = (ptr as *const u32).read_volatile();
            let height = (ptr.add(4) as *const u32).read_volatile();
            let fps = (ptr.add(8) as *const u32).read_volatile();

            Some(Self {
                _mapping: mapping,
                view: ptr,
                view_len: (width as usize) * (height as usize) * 2,
                width: if width > 0 { width } else { DEFAULT_WIDTH },
                height: if height > 0 { height } else { DEFAULT_HEIGHT },
                fps: if fps > 0 { fps } else { DEFAULT_FPS },
            })
        }
    }

    /// Returns whatever frame is currently sitting in the buffer. The payload
    /// is a raw MJPEG JPEG (variable length); `data_len` is stored in the
    /// shared-memory header.
    fn read_current(&self) -> Option<&[u8]> {
        unsafe {
            let data_len = (self.view.add(24) as *const u32).read_volatile() as usize;
            if data_len == 0 || data_len > self.view_len {
                return None;
            }
            Some(std::slice::from_raw_parts(
                self.view.add(HEADER_SIZE),
                data_len,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// AM_MEDIA_TYPE helpers — callers of EnumMediaTypes/GetFormat/GetStreamCaps
// own the returned pointer and free it with CoTaskMemFree, so we must
// allocate with CoTaskMemAlloc, not a Rust Box.
// ---------------------------------------------------------------------------

unsafe fn alloc_media_type(width: u32, height: u32, fps: u32) -> *mut AM_MEDIA_TYPE {
    let format_size = std::mem::size_of::<VIDEOINFOHEADER_NV12>();
    let pb_format = CoTaskMemAlloc(format_size) as *mut VIDEOINFOHEADER_NV12;
    std::ptr::write(
        pb_format,
        VIDEOINFOHEADER_NV12 {
            rc_source: RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            },
            rc_target: RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            },
            dw_bit_rate: (width * height * fps).saturating_mul(8),
            dw_bit_error_rate: 0,
            avg_time_per_frame: 10_000_000i64 / fps.max(1) as i64,
            bmi_header: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: height as i32,
                biPlanes: 1,
                biBitCount: 0,
                biCompression: MEDIASUBTYPE_MJPG.data1,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
        },
    );

    let mt = CoTaskMemAlloc(std::mem::size_of::<AM_MEDIA_TYPE>()) as *mut AM_MEDIA_TYPE;
    std::ptr::write(
        mt,
        AM_MEDIA_TYPE {
            majortype: MEDIATYPE_VIDEO_GUID,
            subtype: MEDIASUBTYPE_MJPG,
            bFixedSizeSamples: BOOL(0),
            bTemporalCompression: BOOL(0),
            lSampleSize: 0,
            formattype: FORMAT_VIDEO_INFO,
            pUnk: core::mem::ManuallyDrop::new(None),
            cbFormat: format_size as u32,
            pbFormat: pb_format as *mut u8,
        },
    );
    mt
}

/// Plain `VIDEOINFOHEADER` layout, defined locally because the windows-rs
/// version is gated behind unrelated feature combinations we don't otherwise
/// need; the memory layout must match `tagVIDEOINFOHEADER` exactly.
#[repr(C)]
struct VIDEOINFOHEADER_NV12 {
    rc_source: RECT,
    rc_target: RECT,
    dw_bit_rate: u32,
    dw_bit_error_rate: u32,
    avg_time_per_frame: i64,
    bmi_header: BITMAPINFOHEADER,
}

fn media_type_matches(mt: &AM_MEDIA_TYPE) -> bool {
    mt.majortype == MEDIATYPE_VIDEO_GUID && mt.subtype == MEDIASUBTYPE_MJPG
}

unsafe fn alloc_pwstr(s: &str) -> PWSTR {
    let w: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    let buf = CoTaskMemAlloc(w.len() * 2) as *mut u16;
    std::ptr::copy_nonoverlapping(w.as_ptr(), buf, w.len());
    PWSTR(buf)
}

// ---------------------------------------------------------------------------
// IEnumMediaTypes — we only ever offer one fixed NV12 format.
// ---------------------------------------------------------------------------

#[implement(IEnumMediaTypes)]
struct MediaTypeEnumerator {
    width: u32,
    height: u32,
    fps: u32,
    pos: Mutex<usize>,
}

impl IEnumMediaTypes_Impl for MediaTypeEnumerator_Impl {
    fn Next(
        &self,
        cmediatypes: u32,
        ppmediatypes: *mut *mut AM_MEDIA_TYPE,
        pcfetched: *mut u32,
    ) -> HRESULT {
        let mut pos = self.pos.lock().unwrap();
        let mut count = 0u32;
        unsafe {
            for i in 0..cmediatypes {
                if *pos >= 1 {
                    break;
                }
                *ppmediatypes.add(i as usize) = alloc_media_type(self.width, self.height, self.fps);
                *pos += 1;
                count += 1;
            }
            if !pcfetched.is_null() {
                *pcfetched = count;
            }
        }
        if count == cmediatypes {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, cmediatypes: u32) -> windows::core::Result<()> {
        let mut pos = self.pos.lock().unwrap();
        *pos = (*pos + cmediatypes as usize).min(1);
        Ok(())
    }

    fn Reset(&self) -> windows::core::Result<()> {
        *self.pos.lock().unwrap() = 0;
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumMediaTypes> {
        let pos = *self.pos.lock().unwrap();
        ComObject::new(MediaTypeEnumerator {
            width: self.width,
            height: self.height,
            fps: self.fps,
            pos: Mutex::new(pos),
        })
        .cast()
    }
}

// ---------------------------------------------------------------------------
// IEnumPins — a single output pin.
// ---------------------------------------------------------------------------

#[implement(IEnumPins)]
struct PinEnumerator {
    pins: Vec<IPin>,
    pos: Mutex<usize>,
}

impl IEnumPins_Impl for PinEnumerator_Impl {
    fn Next(&self, cpins: u32, pppins: *mut Option<IPin>, pcfetched: *mut u32) -> HRESULT {
        let mut pos = self.pos.lock().unwrap();
        let mut count = 0u32;
        unsafe {
            for i in 0..cpins {
                if *pos >= self.pins.len() {
                    break;
                }
                *pppins.add(i as usize) = Some(self.pins[*pos].clone());
                *pos += 1;
                count += 1;
            }
            if !pcfetched.is_null() {
                *pcfetched = count;
            }
        }
        if count == cpins {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, cpins: u32) -> windows::core::Result<()> {
        let mut pos = self.pos.lock().unwrap();
        *pos = (*pos + cpins as usize).min(self.pins.len());
        Ok(())
    }

    fn Reset(&self) -> windows::core::Result<()> {
        *self.pos.lock().unwrap() = 0;
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumPins> {
        let pos = *self.pos.lock().unwrap();
        ComObject::new(PinEnumerator {
            pins: self.pins.clone(),
            pos: Mutex::new(pos),
        })
        .cast()
    }
}

// ---------------------------------------------------------------------------
// CamPin — the single output pin. Frames are pushed (not pulled): once
// connected and running, a worker thread reads the latest frame out of
// shared memory and calls IMemInputPin::Receive on the downstream pin at
// the negotiated frame rate — the same push model every DirectShow capture
// source uses.
// ---------------------------------------------------------------------------

struct PinState {
    filter: Option<IBaseFilter>,
    connected_to: Option<IPin>,
    allocator: Option<IMemAllocator>,
    mem_input_pin: Option<IMemInputPin>,
    stop_flag: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[implement(IPin, IAMStreamConfig, IKsPropertySet)]
struct CamPin {
    state: Mutex<PinState>,
    width: u32,
    height: u32,
    fps: u32,
}

impl CamPin {
    fn set_filter(&self, filter: IBaseFilter) {
        self.state.lock().unwrap().filter = Some(filter);
    }

    fn is_connected(&self) -> bool {
        self.state.lock().unwrap().connected_to.is_some()
    }

    fn start_pushing(&self) {
        let (mem_input_pin, allocator, already_running) = {
            let state = self.state.lock().unwrap();
            if state.worker.is_some() {
                (None, None, true)
            } else {
                state.stop_flag.store(false, Ordering::SeqCst);
                (state.mem_input_pin.clone(), state.allocator.clone(), false)
            }
        };
        if already_running {
            return;
        }
        let (Some(mem_input_pin), Some(allocator)) = (mem_input_pin, allocator) else {
            return;
        };

        let stop_flag = Arc::clone(&self.state.lock().unwrap().stop_flag);
        let mem_input_pin = SendComPtr(mem_input_pin);
        let allocator = SendComPtr(allocator);
        let fps = self.fps.max(1);

        let handle = std::thread::Builder::new()
            .name("screx-vcam-push".into())
            .spawn(move || {
                let mem_input_pin = mem_input_pin.into_inner();
                let allocator = allocator.into_inner();
                let mut source = None;
                let frame_interval = Duration::from_millis(1000 / fps as u64);
                let frame_duration_100ns = 10_000_000i64 / fps as i64;
                let mut frame_count: i64 = 0;

                while !stop_flag.load(Ordering::Relaxed) {
                    let tick = Instant::now();
                    if source.is_none() {
                        source = SharedFrameSource::open();
                    }
                    if let Some(payload) = source.as_ref().and_then(|s| s.read_current()) {
                        unsafe {
                            match fill_sample(
                                &allocator,
                                Some(payload),
                                frame_count,
                                frame_duration_100ns,
                            ) {
                                Ok(sample) => {
                                    let _ = sample.SetDiscontinuity(frame_count == 0);
                                    let _ = sample.SetSyncPoint(true);
                                    let _ = mem_input_pin.Receive(&sample);
                                    frame_count += 1;
                                }
                                Err(_) => {}
                            }
                        }
                    }
                    let elapsed = tick.elapsed();
                    if elapsed < frame_interval {
                        std::thread::sleep(frame_interval - elapsed);
                    }
                }
            })
            .ok();

        self.state.lock().unwrap().worker = handle;
    }

    fn stop_pushing(&self) {
        let (stop_flag, worker) = {
            let mut state = self.state.lock().unwrap();
            (Arc::clone(&state.stop_flag), state.worker.take())
        };
        stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = worker {
            let _ = handle.join();
        }
    }
}

unsafe fn fill_sample(
    allocator: &IMemAllocator,
    payload: Option<&[u8]>,
    frame_count: i64,
    frame_duration_100ns: i64,
) -> windows::core::Result<IMediaSample> {
    let payload = payload.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
    let mut sample_opt: Option<IMediaSample> = None;
    allocator.GetBuffer(&mut sample_opt, None, None, 0)?;
    let sample = sample_opt.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;

    let ptr = sample.GetPointer()?;
    let capacity = sample.GetSize() as usize;
    let n = payload.len().min(capacity);
    std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr, n);
    sample.SetActualDataLength(n as i32)?;

    let start_t = frame_count * frame_duration_100ns;
    let end_t = start_t + frame_duration_100ns;
    sample.SetTime(Some(&start_t), Some(&end_t))?;
    sample.SetSyncPoint(true)?;
    sample.SetPreroll(frame_count == 0)?;

    Ok(sample)
}

impl IPin_Impl for CamPin_Impl {
    fn Connect(
        &self,
        preceivepin: Option<&IPin>,
        _pmt: *const AM_MEDIA_TYPE,
    ) -> windows::core::Result<()> {
        let receive_pin =
            preceivepin.ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))?;

        let self_pin: IPin = unsafe {
            let mut raw: *mut c_void = std::ptr::null_mut();
            self.QueryInterface(&IPin::IID, &mut raw).ok()?;
            IPin::from_raw(raw)
        };

        let offered = unsafe { alloc_media_type(self.width, self.height, self.fps) };
        let connect_result = unsafe { receive_pin.ReceiveConnection(&self_pin, offered) };
        unsafe { free_media_type(offered) };
        connect_result?;

        let mem_input_pin: IMemInputPin = receive_pin.cast()?;
        let requirements =
            unsafe { mem_input_pin.GetAllocatorRequirements() }.unwrap_or(ALLOCATOR_PROPERTIES {
                cBuffers: 0,
                cbBuffer: 0,
                cbAlign: 0,
                cbPrefix: 0,
            });
        let allocator = unsafe { mem_input_pin.GetAllocator() }.or_else(|_| unsafe {
            CoCreateInstance::<_, IMemAllocator>(
                &CLSID_MEMORY_ALLOCATOR,
                None,
                CLSCTX_INPROC_SERVER,
            )
        })?;
        // MJPEG payloads are variable length; reserve a generous upper bound.
        let max_payload = ((self.width * self.height * 2) as i32).max(2 * 1024 * 1024);
        let props = ALLOCATOR_PROPERTIES {
            cBuffers: if requirements.cBuffers > 0 {
                requirements.cBuffers
            } else {
                4
            },
            cbBuffer: max_payload.max(requirements.cbBuffer),
            cbAlign: if requirements.cbAlign > 0 {
                requirements.cbAlign
            } else {
                1
            },
            cbPrefix: requirements.cbPrefix,
        };
        unsafe {
            let _ = allocator.SetProperties(&props)?;
            mem_input_pin.NotifyAllocator(&allocator, false)?;
            allocator.Commit()?;
        }

        let mut state = self.state.lock().unwrap();
        state.connected_to = Some(receive_pin.clone());
        state.allocator = Some(allocator);
        state.mem_input_pin = Some(mem_input_pin);
        Ok(())
    }

    fn ReceiveConnection(
        &self,
        _pconnector: Option<&IPin>,
        _pmt: *const AM_MEDIA_TYPE,
    ) -> windows::core::Result<()> {
        // We are the output/source pin — connections are always initiated by
        // us via Connect(), never received.
        Err(windows::core::Error::from_hresult(E_NOTIMPL))
    }

    fn Disconnect(&self) -> windows::core::Result<()> {
        self.stop_pushing();
        let mut state = self.state.lock().unwrap();
        if let Some(allocator) = state.allocator.take() {
            unsafe {
                let _ = allocator.Decommit();
            }
        }
        state.mem_input_pin = None;
        state.connected_to = None;
        Ok(())
    }

    fn ConnectedTo(&self) -> windows::core::Result<IPin> {
        self.state
            .lock()
            .unwrap()
            .connected_to
            .clone()
            .ok_or_else(|| windows::core::Error::from_hresult(VFW_E_NOT_CONNECTED))
    }

    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> windows::core::Result<()> {
        if !self.is_connected() {
            return Err(windows::core::Error::from_hresult(VFW_E_NOT_CONNECTED));
        }
        unsafe {
            let mt = alloc_media_type(self.width, self.height, self.fps);
            std::ptr::copy_nonoverlapping(mt, pmt, 1);
            CoTaskMemFree(Some(mt as *const c_void));
        }
        Ok(())
    }

    fn QueryPinInfo(&self, pinfo: *mut PIN_INFO) -> windows::core::Result<()> {
        let filter = self.state.lock().unwrap().filter.clone();
        unsafe {
            let mut name_buf = [0u16; 128];
            let name_w: Vec<u16> = PIN_NAME.encode_utf16().collect();
            let n = name_w.len().min(127);
            name_buf[..n].copy_from_slice(&name_w[..n]);
            (*pinfo).achName = name_buf;
            (*pinfo).dir = PINDIR_OUTPUT;
            (*pinfo).pFilter = core::mem::ManuallyDrop::new(filter);
        }
        Ok(())
    }

    fn QueryDirection(&self) -> windows::core::Result<PIN_DIRECTION> {
        Ok(PINDIR_OUTPUT)
    }

    fn QueryId(&self) -> windows::core::Result<PWSTR> {
        Ok(unsafe { alloc_pwstr(PIN_NAME) })
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        if pmt.is_null() {
            return S_FALSE;
        }
        if media_type_matches(unsafe { &*pmt }) {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn EnumMediaTypes(&self) -> windows::core::Result<IEnumMediaTypes> {
        ComObject::new(MediaTypeEnumerator {
            width: self.width,
            height: self.height,
            fps: self.fps,
            pos: Mutex::new(0),
        })
        .cast()
    }

    fn QueryInternalConnections(
        &self,
        _appin: *mut Option<IPin>,
        _npin: *mut u32,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(E_NOTIMPL))
    }

    fn EndOfStream(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn BeginFlush(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn EndFlush(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn NewSegment(&self, _tstart: i64, _tstop: i64, _drate: f64) -> windows::core::Result<()> {
        Ok(())
    }
}

unsafe fn free_media_type(pmt: *mut AM_MEDIA_TYPE) {
    if pmt.is_null() {
        return;
    }
    let mt = &*pmt;
    if !mt.pbFormat.is_null() {
        CoTaskMemFree(Some(mt.pbFormat as *const c_void));
    }
    CoTaskMemFree(Some(pmt as *const c_void));
}

impl IAMStreamConfig_Impl for CamPin_Impl {
    fn SetFormat(&self, pmt: *const AM_MEDIA_TYPE) -> windows::core::Result<()> {
        if pmt.is_null() {
            return Ok(());
        }
        if media_type_matches(unsafe { &*pmt }) {
            Ok(())
        } else {
            Err(windows::core::Error::from_hresult(VFW_E_INVALIDMEDIATYPE))
        }
    }

    fn GetFormat(&self) -> windows::core::Result<*mut AM_MEDIA_TYPE> {
        Ok(unsafe { alloc_media_type(self.width, self.height, self.fps) })
    }

    fn GetNumberOfCapabilities(
        &self,
        picount: *mut i32,
        pisize: *mut i32,
    ) -> windows::core::Result<()> {
        unsafe {
            if !picount.is_null() {
                *picount = 1;
            }
            if !pisize.is_null() {
                *pisize = std::mem::size_of::<VIDEO_STREAM_CONFIG_CAPS>() as i32;
            }
        }
        Ok(())
    }

    fn GetStreamCaps(
        &self,
        iindex: i32,
        ppmt: *mut *mut AM_MEDIA_TYPE,
        pscc: *mut u8,
    ) -> windows::core::Result<()> {
        if iindex != 0 {
            return Err(windows::core::Error::from_hresult(E_INVALIDARG));
        }
        unsafe {
            *ppmt = alloc_media_type(self.width, self.height, self.fps);
            if !pscc.is_null() {
                let frame_interval = 10_000_000i64 / self.fps.max(1) as i64;
                let size = SIZE {
                    cx: self.width as i32,
                    cy: self.height as i32,
                };
                let caps = VIDEO_STREAM_CONFIG_CAPS {
                    guid: FORMAT_VIDEO_INFO,
                    VideoStandard: 0,
                    InputSize: size,
                    MinCroppingSize: size,
                    MaxCroppingSize: size,
                    CropGranularityX: 1,
                    CropGranularityY: 1,
                    CropAlignX: 0,
                    CropAlignY: 0,
                    MinOutputSize: size,
                    MaxOutputSize: size,
                    OutputGranularityX: 1,
                    OutputGranularityY: 1,
                    StretchTapsX: 0,
                    StretchTapsY: 0,
                    ShrinkTapsX: 0,
                    ShrinkTapsY: 0,
                    MinFrameInterval: frame_interval,
                    MaxFrameInterval: frame_interval,
                    MinBitsPerSecond: 0,
                    MaxBitsPerSecond: i32::MAX,
                };
                std::ptr::write(pscc as *mut VIDEO_STREAM_CONFIG_CAPS, caps);
            }
        }
        Ok(())
    }
}

/// Lets `ICaptureGraphBuilder2::RenderStream` (used internally by browsers'
/// getUserMedia capture pipeline, among others) identify this as the
/// capture pin. Without this, capture-graph builders that look up the pin by
/// category rather than just taking pin 0 fail to find one, which surfaces
/// to callers as a generic "could not start video source" activation error
/// even though plain DirectShow `Render()` (which doesn't care about
/// category) connects and streams just fine.
impl IKsPropertySet_Impl for CamPin_Impl {
    fn Set(
        &self,
        _guidpropset: *const GUID,
        _dwpropid: u32,
        _pinstancedata: *const c_void,
        _cbinstancedata: u32,
        _ppropdata: *const c_void,
        _cbpropdata: u32,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(E_PROP_SET_UNSUPPORTED))
    }

    fn Get(
        &self,
        guidpropset: *const GUID,
        dwpropid: u32,
        _pinstancedata: *const c_void,
        _cbinstancedata: u32,
        ppropdata: *mut c_void,
        cbpropdata: u32,
        pcbreturned: *mut u32,
    ) -> windows::core::Result<()> {
        unsafe {
            if !pcbreturned.is_null() {
                *pcbreturned = std::mem::size_of::<GUID>() as u32;
            }
            if guidpropset.is_null()
                || *guidpropset != AMPROPSETID_PIN
                || dwpropid != AMPROPERTY_PIN_CATEGORY.0 as u32
            {
                return Err(windows::core::Error::from_hresult(E_PROP_SET_UNSUPPORTED));
            }
            if ppropdata.is_null() || cbpropdata < std::mem::size_of::<GUID>() as u32 {
                return Err(windows::core::Error::from_hresult(E_PROP_SET_UNSUPPORTED));
            }
            std::ptr::write(ppropdata as *mut GUID, PINNAME_VIDEO_CAPTURE);
        }
        Ok(())
    }

    fn QuerySupported(
        &self,
        guidpropset: *const GUID,
        dwpropid: u32,
    ) -> windows::core::Result<u32> {
        unsafe {
            if !guidpropset.is_null()
                && *guidpropset == AMPROPSETID_PIN
                && dwpropid == AMPROPERTY_PIN_CATEGORY.0 as u32
            {
                Ok(1) // KSPROPERTY_SUPPORT_GET
            } else {
                Err(windows::core::Error::from_hresult(E_PROP_SET_UNSUPPORTED))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CamFilter — the filter object itself (IBaseFilter/IMediaFilter/IPersist).
// ---------------------------------------------------------------------------

struct FilterState {
    graph: Option<IFilterGraph>,
    filter_state: FILTER_STATE,
    clock: Option<IReferenceClock>,
}

#[implement(IPersist, IMediaFilter, IBaseFilter)]
struct CamFilter {
    state: Mutex<FilterState>,
    pin: ComObject<CamPin>,
    pin_iface: IPin,
}

impl CamFilter {
    fn create() -> windows::core::Result<ComObject<CamFilter>> {
        let (width, height, fps) = SharedFrameSource::open()
            .map(|s| (s.width, s.height, s.fps))
            .unwrap_or((DEFAULT_WIDTH, DEFAULT_HEIGHT, DEFAULT_FPS));

        let pin_obj = ComObject::new(CamPin {
            state: Mutex::new(PinState {
                filter: None,
                connected_to: None,
                allocator: None,
                mem_input_pin: None,
                stop_flag: Arc::new(AtomicBool::new(true)),
                worker: None,
            }),
            width,
            height,
            fps,
        });
        let pin_iface: IPin = pin_obj.cast()?;

        let filter_obj = ComObject::new(CamFilter {
            state: Mutex::new(FilterState {
                graph: None,
                filter_state: State_Stopped,
                clock: None,
            }),
            pin: pin_obj.clone(),
            pin_iface: pin_iface.clone(),
        });
        let filter_iface: IBaseFilter = filter_obj.cast()?;
        pin_obj.set_filter(filter_iface);

        Ok(filter_obj)
    }
}

impl IPersist_Impl for CamFilter_Impl {
    fn GetClassID(&self) -> windows::core::Result<GUID> {
        Ok(CLSID_SCREX_VCAM)
    }
}

impl IMediaFilter_Impl for CamFilter_Impl {
    fn Stop(&self) -> windows::core::Result<()> {
        self.pin.stop_pushing();
        self.state.lock().unwrap().filter_state = State_Stopped;
        Ok(())
    }

    fn Pause(&self) -> windows::core::Result<()> {
        if self.pin.is_connected() {
            self.pin.start_pushing();
        }
        self.state.lock().unwrap().filter_state = State_Paused;
        Ok(())
    }

    fn Run(&self, _tstart: i64) -> windows::core::Result<()> {
        if self.pin.is_connected() {
            self.pin.start_pushing();
        }
        self.state.lock().unwrap().filter_state = State_Running;
        Ok(())
    }

    fn GetState(&self, _dwmillisecstimeout: u32) -> windows::core::Result<FILTER_STATE> {
        Ok(self.state.lock().unwrap().filter_state)
    }

    fn SetSyncSource(&self, pclock: Option<&IReferenceClock>) -> windows::core::Result<()> {
        self.state.lock().unwrap().clock = pclock.cloned();
        Ok(())
    }

    fn GetSyncSource(&self) -> windows::core::Result<IReferenceClock> {
        self.state
            .lock()
            .unwrap()
            .clock
            .clone()
            .ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))
    }
}

impl IBaseFilter_Impl for CamFilter_Impl {
    fn EnumPins(&self) -> windows::core::Result<IEnumPins> {
        ComObject::new(PinEnumerator {
            pins: vec![self.pin_iface.clone()],
            pos: Mutex::new(0),
        })
        .cast()
    }

    fn FindPin(&self, id: &PCWSTR) -> windows::core::Result<IPin> {
        let requested = unsafe { id.to_string() }.unwrap_or_default();
        if requested == PIN_NAME {
            Ok(self.pin_iface.clone())
        } else {
            Err(windows::core::Error::from_hresult(VFW_E_NOT_FOUND))
        }
    }

    fn QueryFilterInfo(&self, pinfo: *mut FILTER_INFO) -> windows::core::Result<()> {
        let graph = self.state.lock().unwrap().graph.clone();
        unsafe {
            let mut name_buf = [0u16; 128];
            let name_w: Vec<u16> = "Screx Camera".encode_utf16().collect();
            let n = name_w.len().min(127);
            name_buf[..n].copy_from_slice(&name_w[..n]);
            (*pinfo).achName = name_buf;
            (*pinfo).pGraph = core::mem::ManuallyDrop::new(graph);
        }
        Ok(())
    }

    fn JoinFilterGraph(
        &self,
        pgraph: Option<&IFilterGraph>,
        _pname: &PCWSTR,
    ) -> windows::core::Result<()> {
        self.state.lock().unwrap().graph = pgraph.cloned();
        Ok(())
    }

    fn QueryVendorInfo(&self) -> windows::core::Result<PWSTR> {
        Err(windows::core::Error::from_hresult(E_NOTIMPL))
    }
}

// ---------------------------------------------------------------------------
// COM class factory + DLL entry points
// ---------------------------------------------------------------------------

#[implement(windows::Win32::System::Com::IClassFactory)]
struct ClassFactory;

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Option<&windows::core::IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if punkouter.is_some() {
            return Err(windows::core::Error::from_hresult(CLASS_E_NOAGGREGATION));
        }
        let filter = CamFilter::create()?;
        let filter_iface: IBaseFilter = filter.cast()?;
        unsafe { filter_iface.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, _flock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

#[no_mangle]
extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return E_POINTER;
        }
        *ppv = std::ptr::null_mut();
        if *rclsid != CLSID_SCREX_VCAM {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory_obj = ComObject::new(ClassFactory);
        let factory: Result<windows::Win32::System::Com::IClassFactory, _> = factory_obj.cast();
        match factory {
            Ok(f) => f.query(riid, ppv),
            Err(e) => e.code(),
        }
    }
}

#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

/// Registers this DLL's CLSID as an in-proc COM server AND as a device in
/// the system's video-capture-sources category, so it shows up as a normal
/// webcam to every capture API (DirectShow directly, and Media Foundation /
/// WinRT via Windows' own compatibility bridge). Must run elevated (writes
/// under HKLM, matching the daemon's other device-registration steps).
pub fn register(dll_path: &str, friendly_name: &str) -> windows::core::Result<()> {
    unsafe {
        let clsid_str = format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            CLSID_SCREX_VCAM.data1,
            CLSID_SCREX_VCAM.data2,
            CLSID_SCREX_VCAM.data3,
            CLSID_SCREX_VCAM.data4[0],
            CLSID_SCREX_VCAM.data4[1],
            CLSID_SCREX_VCAM.data4[2],
            CLSID_SCREX_VCAM.data4[3],
            CLSID_SCREX_VCAM.data4[4],
            CLSID_SCREX_VCAM.data4[5],
            CLSID_SCREX_VCAM.data4[6],
            CLSID_SCREX_VCAM.data4[7],
        );
        let clsid_key = format!("Software\\Classes\\CLSID\\{clsid_str}");

        // A stale per-user override at this same subkey under HKCU shadows
        // the HKLM registration below for COM activation (HKCU takes
        // precedence), which previously caused the camera to appear
        // registered but resolve to a dead/black source. Clear it first so
        // the fresh HKLM registration we're about to write is the one COM
        // actually sees. Ignore errors — it usually won't exist.
        let clsid_key_w: Vec<u16> = clsid_key.encode_utf16().chain(Some(0)).collect();
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(clsid_key_w.as_ptr()));

        write_reg_sz(HKEY_LOCAL_MACHINE, &clsid_key, "", friendly_name)?;
        let inproc_key = format!("{clsid_key}\\InprocServer32");
        write_reg_sz(HKEY_LOCAL_MACHINE, &inproc_key, "", dll_path)?;
        write_reg_sz(HKEY_LOCAL_MACHINE, &inproc_key, "ThreadingModel", "Both")?;

        let mapper: IFilterMapper2 =
            CoCreateInstance(&CLSID_FILTER_MAPPER2, None, CLSCTX_INPROC_SERVER)?;

        let media_type = REGPINTYPES {
            clsMajorType: &MEDIATYPE_VIDEO_GUID,
            clsMinorType: &MEDIASUBTYPE_MJPG,
        };
        let pin_name_w: Vec<u16> = PIN_NAME.encode_utf16().chain(Some(0)).collect();
        let pins = [REGFILTERPINS {
            strName: PWSTR(pin_name_w.as_ptr() as *mut u16),
            bRendered: BOOL(0),
            bOutput: BOOL(1),
            bZero: BOOL(0),
            bMany: BOOL(0),
            clsConnectsToFilter: std::ptr::null(),
            strConnectsToPin: PCWSTR::null(),
            nMediaTypes: 1,
            lpMediaType: &media_type,
        }];
        let regfilter2 = REGFILTER2 {
            dwVersion: 1,
            dwMerit: MERIT_NORMAL.0 as u32,
            Anonymous: REGFILTER2_0 {
                Anonymous1: REGFILTER2_0_0 {
                    cPins: 1,
                    rgPins: pins.as_ptr(),
                },
            },
        };

        let friendly_name_w: Vec<u16> = friendly_name.encode_utf16().chain(Some(0)).collect();
        mapper.RegisterFilter(
            &CLSID_SCREX_VCAM,
            PCWSTR(friendly_name_w.as_ptr()),
            None,
            &CLSID_VIDEO_INPUT_DEVICE_CATEGORY,
            PCWSTR::null(),
            &regfilter2,
        )?;

        Ok(())
    }
}

/// Removes this DLL's DirectShow capture-device registration and COM server
/// registration. Existing processes that already loaded the filter may keep
/// using it until their graph closes, but new apps will no longer see Screx
/// Camera after this returns.
pub fn unregister() -> windows::core::Result<()> {
    unsafe {
        if let Ok(mapper) =
            CoCreateInstance::<_, IFilterMapper2>(&CLSID_FILTER_MAPPER2, None, CLSCTX_INPROC_SERVER)
        {
            let _ = mapper.UnregisterFilter(
                &CLSID_VIDEO_INPUT_DEVICE_CATEGORY,
                PCWSTR::null(),
                &CLSID_SCREX_VCAM,
            );
        }

        let clsid_str = format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            CLSID_SCREX_VCAM.data1,
            CLSID_SCREX_VCAM.data2,
            CLSID_SCREX_VCAM.data3,
            CLSID_SCREX_VCAM.data4[0],
            CLSID_SCREX_VCAM.data4[1],
            CLSID_SCREX_VCAM.data4[2],
            CLSID_SCREX_VCAM.data4[3],
            CLSID_SCREX_VCAM.data4[4],
            CLSID_SCREX_VCAM.data4[5],
            CLSID_SCREX_VCAM.data4[6],
            CLSID_SCREX_VCAM.data4[7],
        );
        let clsid_key = format!("Software\\Classes\\CLSID\\{clsid_str}");
        let clsid_key_w: Vec<u16> = clsid_key.encode_utf16().chain(Some(0)).collect();
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(clsid_key_w.as_ptr()));
        // Also clear any stale per-user override, which would otherwise keep
        // shadowing HKLM for COM activation even after this unregister.
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(clsid_key_w.as_ptr()));
    }
    Ok(())
}

unsafe fn write_reg_sz(
    root: HKEY,
    subkey: &str,
    value_name: &str,
    value: &str,
) -> windows::core::Result<()> {
    let subkey_w: Vec<u16> = subkey.encode_utf16().chain(Some(0)).collect();
    let value_name_w: Vec<u16> = value_name.encode_utf16().chain(Some(0)).collect();
    let value_w: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();

    let mut hkey = std::mem::zeroed();
    RegCreateKeyExW(
        root,
        PCWSTR(subkey_w.as_ptr()),
        0,
        PCWSTR::null(),
        REG_OPTION_NON_VOLATILE,
        KEY_WRITE,
        None,
        &mut hkey,
        None,
    )
    .ok()?;
    let value_bytes: &[u8] =
        std::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2);
    let ret = RegSetValueExW(
        hkey,
        PCWSTR(value_name_w.as_ptr()),
        0,
        REG_SZ,
        Some(value_bytes),
    );
    let _ = RegCloseKey(hkey);
    ret.ok()
}
