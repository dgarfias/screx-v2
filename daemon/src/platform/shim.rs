use std::path::PathBuf;

/// Return the directory where Screx stores its persistent configuration.
pub fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("screx")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("screx")
    } else {
        PathBuf::from("/tmp/screx")
    }
}

/// Return the local host name.
pub fn hostname() -> Option<String> {
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
