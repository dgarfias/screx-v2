use std::path::PathBuf;

/// Return the directory where Screx stores its persistent configuration.
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("screx")
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".config").join("screx")
        } else {
            PathBuf::from("/tmp/screx")
        }
    }
    #[cfg(target_os = "windows")]
    {
        dirs::config_dir()
            .map(|p| p.join("screx"))
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows\Temp\screx"))
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("screx")
        } else {
            PathBuf::from("/tmp/screx")
        }
    }
}

/// Return the local host name.
pub fn hostname() -> Option<String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc != 0 {
            return None;
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let hostname = String::from_utf8_lossy(&buf[..len]).trim().to_string();
        if hostname.is_empty() {
            None
        } else {
            Some(hostname)
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::SystemInformation::{
            ComputerNameDnsHostname, GetComputerNameExW,
        };
        let mut buf = [0u16; 256];
        let mut size = buf.len() as u32;
        unsafe {
            if GetComputerNameExW(
                ComputerNameDnsHostname,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut size,
            )
            .is_ok()
            {
                let name = String::from_utf16_lossy(&buf[..size as usize]);
                let name = name.trim();
                if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                }
            } else {
                None
            }
        }
    }
}
