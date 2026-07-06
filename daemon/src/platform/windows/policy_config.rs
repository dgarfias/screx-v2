// ---------------------------------------------------------------------------
// IPolicyConfig — undocumented COM interface used to change the system's
// default audio endpoint (the same interface tools like SoundVolumeView /
// EarTrumpet use). Not shipped by the `windows` crate, so it's defined here
// using the same macros windows-core itself uses for other undocumented
// interfaces (e.g. IAgileReference in windows-core's own com_bindings.rs).
//
// Verified against Sunshine's (LizardByte) production PolicyConfig.h:
//   CLSID CPolicyConfigClient: 870af99c-171d-4f9e-af0d-e63df40c2bc9
//   IID   IPolicyConfig:       f8679f50-850a-41cf-9c72-430f290290c8
// Vtable order after IUnknown (all return HRESULT); only method 11
// (SetDefaultEndpoint) is ever called, but every method must still be
// declared in order so its offset in the vtable is correct.
// ---------------------------------------------------------------------------
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Media::Audio::{
    eRender, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PROPERTYKEY};

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

windows_core::imp::define_interface!(
    IPolicyConfig,
    IPolicyConfig_Vtbl,
    0xf8679f50_850a_41cf_9c72_430f290290c8
);
impl core::ops::Deref for IPolicyConfig {
    type Target = windows_core::IUnknown;
    fn deref(&self) -> &Self::Target {
        unsafe { core::mem::transmute(self) }
    }
}
windows_core::imp::interface_hierarchy!(IPolicyConfig, windows_core::IUnknown);

impl IPolicyConfig {
    unsafe fn set_default_endpoint(
        &self,
        device_id: PCWSTR,
        role: ERole,
    ) -> windows_core::Result<()> {
        (windows_core::Interface::vtable(self).SetDefaultEndpoint)(
            windows_core::Interface::as_raw(self),
            device_id,
            role,
        )
        .ok()
    }

    unsafe fn set_endpoint_visibility(
        &self,
        device_id: PCWSTR,
        visible: i32,
    ) -> windows_core::Result<()> {
        (windows_core::Interface::vtable(self).SetEndpointVisibility)(
            windows_core::Interface::as_raw(self),
            device_id,
            visible,
        )
        .ok()
    }
}

#[repr(C)]
#[allow(dead_code)] // only SetDefaultEndpoint is called; the rest exist to keep the vtable offset correct
pub struct IPolicyConfig_Vtbl {
    pub base__: windows_core::IUnknown_Vtbl,
    pub GetMixFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut *mut c_void) -> HRESULT,
    pub GetDeviceFormat:
        unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut *mut c_void) -> HRESULT,
    pub ResetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    pub SetDeviceFormat:
        unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void, *mut c_void) -> HRESULT,
    pub GetProcessingPeriod:
        unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut i64, *mut i64) -> HRESULT,
    pub SetProcessingPeriod: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut i64) -> HRESULT,
    pub GetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
    pub SetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
    pub GetPropertyValue:
        unsafe extern "system" fn(*mut c_void, PCWSTR, *const PROPERTYKEY, *mut c_void) -> HRESULT,
    pub SetPropertyValue: unsafe extern "system" fn(
        *mut c_void,
        PCWSTR,
        *const PROPERTYKEY,
        *const c_void,
    ) -> HRESULT,
    pub SetDefaultEndpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
    pub SetEndpointVisibility: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT,
}

/// Sets `device_id` as the default render endpoint for each of `roles`.
pub fn set_default_endpoint(device_id: &str, roles: &[ERole]) -> Result<()> {
    unsafe {
        let policy_config: IPolicyConfig =
            CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        let id_ws: Vec<u16> = device_id.encode_utf16().chain(Some(0)).collect();
        for &role in roles {
            policy_config.set_default_endpoint(PCWSTR(id_ws.as_ptr()), role)?;
        }
    }
    Ok(())
}

/// Show (true) or hide (false) an audio endpoint without uninstalling its driver.
pub fn set_endpoint_visibility(device_id: &str, visible: bool) -> Result<()> {
    unsafe {
        let policy_config: IPolicyConfig =
            CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        let id_ws: Vec<u16> = device_id.encode_utf16().chain(Some(0)).collect();
        policy_config.set_endpoint_visibility(PCWSTR(id_ws.as_ptr()), visible as i32)?;
    }
    Ok(())
}

/// The current default render endpoint's device id string for `role`.
pub fn current_default_endpoint_id(role: ERole) -> Result<String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, role)?;
        device_id_string(&device)
    }
}

pub(crate) unsafe fn device_id_string(device: &IMMDevice) -> Result<String> {
    let pwstr = device.GetId()?;
    let s = pwstr.to_string().unwrap_or_default();
    CoTaskMemFree(Some(pwstr.as_ptr() as *const c_void));
    Ok(s)
}

/// Custom property {b3f8fa53-0004-438e-9003-51a46e139bfc},6 — verified via
/// registry inspection (HKLM MMDevices\Audio\Render\<id>\Properties) to hold
/// the driver-supplied friendly name ("Steam Streaming Speakers"), unlike
/// PKEY_Device_FriendlyName (a45c254e-...,14) which was empty for this
/// endpoint, and PKEY_Device_DeviceDesc (a45c254e-...,2) which just holds the
/// generic jack label "Speakers". Not exported by the windows crate.
const PKEY_ENDPOINT_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xb3f8fa53_0004_438e_9003_51a46e139bfc),
    pid: 6,
};

unsafe fn endpoint_friendly_name(device: &IMMDevice) -> Option<String> {
    let store: IPropertyStore = device.OpenPropertyStore(STGM_READ).ok()?;
    let pv = store.GetValue(&PKEY_ENDPOINT_FRIENDLY_NAME).ok()?;
    let name = pv.to_string();
    (!name.is_empty()).then_some(name)
}

/// Enumerates endpoints on `data_flow` whose state matches `state_mask` and
/// returns the first whose friendly name contains `name` (case-insensitive).
pub fn find_endpoint_by_friendly_name_flow(
    data_flow: EDataFlow,
    name: &str,
    state_mask: DEVICE_STATE,
) -> Result<IMMDevice> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator.EnumAudioEndpoints(data_flow, state_mask)?;
        let count = collection.GetCount()?;
        let needle = name.to_ascii_lowercase();
        for i in 0..count {
            let device = collection.Item(i)?;
            if let Some(found) = endpoint_friendly_name(&device) {
                if found.to_ascii_lowercase().contains(&needle) {
                    return Ok(device);
                }
            }
        }
        bail!("no {data_flow:?} endpoint named like '{name}'");
    }
}

/// Same as `find_endpoint_by_friendly_name_flow` restricted to active render endpoints.
pub fn find_endpoint_by_friendly_name(name: &str) -> Result<IMMDevice> {
    find_endpoint_by_friendly_name_flow(eRender, name, DEVICE_STATE_ACTIVE)
}

/// Polls `find_endpoint_by_friendly_name` until it succeeds or `timeout`
/// elapses. A devnode being enabled doesn't mean WASAPI can see its endpoint
/// yet — same "no hot-plug interrupt for a software device" gap as the
/// virtual display driver's GDI adapter enumeration.
pub fn poll_for_endpoint(name: &str, timeout: Duration) -> Result<IMMDevice> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(d) = find_endpoint_by_friendly_name(name) {
            return Ok(d);
        }
        if Instant::now() >= deadline {
            bail!("'{name}' endpoint never became active");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
