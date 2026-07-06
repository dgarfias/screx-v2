// ---------------------------------------------------------------------------
// Steam Streaming Speakers devnode lifecycle
//
// Same problem as the virtual display driver in display.rs: a root-enumerated
// hardware id has no bus to plug into, so something has to explicitly create
// a device instance for it (SetupDi pattern) before Windows will bind a
// driver to it. This mirrors display.rs's install_vdd_driver/find_vdd_devinst
// /enable_vdd/disable_vdd functions with the display-class specifics swapped
// for the audio driver's. Kept as a separate copy rather than a shared helper
// so this code can't regress the working VDD path.
//
// Unlike MikeTheTech's Virtual-Audio-Driver (KMDF, OV-tier Authenticode —
// fails to load without Test Mode with STATUS_INVALID_IMAGE_HASH, confirmed
// by repeated testing), Steam Streaming Speakers is EV-signed by Valve and
// loads fine on a normal, non-test-signing Windows install.
// ---------------------------------------------------------------------------

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Disable_DevNode, CM_Enable_DevNode, CM_Get_DevNode_Registry_PropertyW,
    CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW, CM_Locate_DevNodeW,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiSetDeviceRegistryPropertyW,
    UpdateDriverForPlugAndPlayDevicesW, CM_DRP_HARDWAREID, CM_LOCATE_DEVNODE_NORMAL,
    CM_LOCATE_DEVNODE_PHANTOM, CR_BUFFER_SMALL, CR_SUCCESS, DICD_GENERATE_ID, DIF_REGISTERDEVICE,
    GUID_DEVCLASS_MEDIA, INSTALLFLAG_FORCE, SPDRP_HARDWAREID, SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::BOOL;

pub const STEAM_SPK_HARDWARE_ID: &str = "Root\\SteamStreamingSpeakers";
pub const STEAM_SPK_FRIENDLY_NAME: &str = "Steam Streaming Speakers";

/// Install-if-absent + enable. Mirrors display.rs's pattern of "find the
/// devnode; if it's not there, install and retry enabling with backoff since
/// a freshly registered devnode can take a moment to become enumerable".
pub fn ensure_installed_and_enabled() -> Result<()> {
    match find_steam_spk_devinst() {
        Ok(devinst) => {
            let ret = unsafe { CM_Enable_DevNode(devinst, 0) };
            if ret != CR_SUCCESS {
                bail!("CM_Enable_DevNode failed ({ret:#?})");
            }
            Ok(())
        }
        Err(e) => {
            println!("[audio] Steam Streaming Speakers devnode not found ({e:#}); installing");
            install_steam_spk_driver()?;

            let mut last_err = None;
            for _ in 1..=10 {
                match find_steam_spk_devinst().and_then(|devinst| {
                    let ret = unsafe { CM_Enable_DevNode(devinst, 0) };
                    if ret != CR_SUCCESS {
                        bail!("CM_Enable_DevNode failed ({ret:#?})");
                    }
                    Ok(())
                }) {
                    Ok(()) => return Ok(()),
                    Err(e2) => {
                        last_err = Some(e2);
                        std::thread::sleep(Duration::from_millis(300));
                    }
                }
            }
            Err(last_err.unwrap())
        }
    }
}

pub fn disable_steam_spk() -> Result<()> {
    let devinst = find_steam_spk_devinst()?;
    let ret = unsafe { CM_Disable_DevNode(devinst, 0) };
    if ret != CR_SUCCESS {
        bail!("CM_Disable_DevNode failed ({ret:#?})");
    }
    Ok(())
}

fn find_steam_spk_devinst() -> Result<u32> {
    unsafe {
        let mut size = 0;
        let ret = CM_Get_Device_ID_List_SizeW(&mut size, PCWSTR::null(), 0);
        if ret != CR_SUCCESS {
            bail!("CM_Get_Device_ID_List_SizeW failed ({ret:#?})");
        }
        if size == 0 {
            bail!("no devices found");
        }

        let mut list: Vec<u16> = vec![0; size as usize];
        let ret = CM_Get_Device_ID_ListW(PCWSTR::null(), &mut list, 0);
        if ret != CR_SUCCESS {
            bail!("CM_Get_Device_ID_ListW failed ({ret:#?})");
        }

        for id in null_terminated_multi_sz(&list) {
            if id.is_empty() {
                continue;
            }
            let id_ws: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
            let mut devinst = 0;
            let mut ret = CM_Locate_DevNodeW(
                &mut devinst,
                PCWSTR(id_ws.as_ptr()),
                CM_LOCATE_DEVNODE_NORMAL,
            );
            if ret != CR_SUCCESS {
                ret = CM_Locate_DevNodeW(
                    &mut devinst,
                    PCWSTR(id_ws.as_ptr()),
                    CM_LOCATE_DEVNODE_PHANTOM,
                );
            }
            if ret != CR_SUCCESS {
                continue;
            }
            let hw_ids = devnode_hardware_ids(devinst).unwrap_or_default();
            if hw_ids
                .iter()
                .any(|h| h.eq_ignore_ascii_case(STEAM_SPK_HARDWARE_ID))
            {
                return Ok(devinst);
            }
        }

        bail!("Steam Streaming Speakers devnode not found (hardware id {STEAM_SPK_HARDWARE_ID})");
    }
}

unsafe fn devnode_hardware_ids(devinst: u32) -> Result<Vec<String>> {
    let mut buf_len = 0;
    let ret =
        CM_Get_DevNode_Registry_PropertyW(devinst, CM_DRP_HARDWAREID, None, None, &mut buf_len, 0);
    if ret != CR_SUCCESS && ret != CR_BUFFER_SMALL {
        bail!("CM_Get_DevNode_Registry_PropertyW size query failed ({ret:#?})");
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
        bail!("CM_Get_DevNode_Registry_PropertyW failed ({ret:#?})");
    }

    let wide: &[u16] = std::slice::from_raw_parts(buf.as_ptr() as *const u16, buf.len() / 2);
    Ok(null_terminated_multi_sz(wide))
}

fn null_terminated_multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// Creates a root-enumerated devnode for the Steam Streaming Speakers driver
/// and binds the driver to it. See display.rs's install_vdd_driver for the
/// full rationale on why SetupDi + DIF_REGISTERDEVICE is the right tool here
/// (not CM_Create_DevNodeW, which is for bus drivers, not root devices).
fn install_steam_spk_driver() -> Result<()> {
    let inf_path = find_steam_spk_inf_path().context(
        "SteamStreamingSpeakers.inf not found; set SCREX_STEAM_SPK_INF_PATH to its full path",
    )?;
    println!(
        "[audio] installing Steam Streaming Speakers driver from {}",
        inf_path.display()
    );

    unsafe {
        let dev_info = SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_MEDIA), None)
            .context("SetupDiCreateDeviceInfoList failed")?;

        let register = (|| -> Result<()> {
            let name_ws: Vec<u16> = "SteamStreamingSpeakers"
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut devinfo_data = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };

            SetupDiCreateDeviceInfoW(
                dev_info,
                PCWSTR(name_ws.as_ptr()),
                &GUID_DEVCLASS_MEDIA,
                PCWSTR::null(),
                None,
                DICD_GENERATE_ID,
                Some(&mut devinfo_data),
            )
            .context("SetupDiCreateDeviceInfoW failed")?;

            let mut hwid_bytes: Vec<u8> = STEAM_SPK_HARDWARE_ID
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
        if let Err(e) = register {
            println!("[audio] devnode registration: {e:#} (continuing to driver bind)");
        }

        let inf_ws: Vec<u16> = inf_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let hw_id_ws: Vec<u16> = STEAM_SPK_HARDWARE_ID
            .encode_utf16()
            .chain(Some(0))
            .collect();
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
            println!("[audio] driver install requires a reboot");
        }
        Ok(())
    }
}

fn find_steam_spk_inf_path() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("SCREX_STEAM_SPK_INF_PATH") {
        let p: PathBuf = env.into();
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("SteamStreamingSpeakers.inf");
            if p.exists() {
                return Some(p);
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(r"C:\Windows\INF") {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy().to_ascii_lowercase();
                if name.starts_with("oem") && name.ends_with(".inf") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if content
                            .to_ascii_lowercase()
                            .contains("root\\steamstreamingspeakers")
                        {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }

    None
}
