use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{bail, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

#[derive(Debug, Clone)]
pub struct DiscoveredReceiver {
    pub name: String,
    pub addr: SocketAddr,
    pub width: u32,
    pub height: u32,
}

pub struct DiscoveryHandle {
    daemon: ServiceDaemon,
}

impl DiscoveryHandle {
    pub fn shutdown(self) {
        let _ = self.daemon.shutdown();
    }
}

pub struct AdvertisementHandle {
    daemon: ServiceDaemon,
    fullname: String,
}

impl AdvertisementHandle {
    pub fn shutdown(self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

fn get_hostname() -> String {
    let mut buf = [0u8; 256];
    unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..len]).to_string();
            if name.ends_with(".local.") {
                name
            } else {
                format!("{name}.local.")
            }
        } else {
            "screx-daemon.local.".to_string()
        }
    }
}

pub fn start_sender_advertisement(
    service_type: &str,
    instance_name: &str,
    port: u16,
) -> Result<AdvertisementHandle> {
    let mdns = ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns-sd init failed: {e}"))?;

    let qualified = if service_type.ends_with(".local.") {
        service_type.to_string()
    } else {
        format!("{service_type}.local.")
    };

    let host_name = get_hostname();
    let info = ServiceInfo::new(&qualified, instance_name, &host_name, "", port, None)
        .map_err(|e| anyhow::anyhow!("failed to build mdns service info: {e}"))?;
    let fullname = info.get_fullname().to_string();

    mdns.register(info)
        .map_err(|e| anyhow::anyhow!("failed to register mdns service: {e}"))?;

    println!(
        "[discovery] advertising sender '{}' on {} (port {}) hostname={}",
        instance_name, qualified, port, host_name
    );

    Ok(AdvertisementHandle { daemon: mdns, fullname })
}

pub fn browse_for_receiver(
    service_type: &str,
    timeout: Duration,
) -> Result<(DiscoveredReceiver, DiscoveryHandle)> {
    let mdns = ServiceDaemon::new().map_err(|e| anyhow::anyhow!("mdns-sd init failed: {e}"))?;

    let qualified = if service_type.ends_with(".local.") {
        service_type.to_string()
    } else {
        format!("{service_type}.local.")
    };
    let receiver = mdns
        .browse(&qualified)
        .map_err(|e| anyhow::anyhow!("mdns browse failed: {e}"))?;

    println!("[discovery] browsing for {qualified} (timeout {}s)...", timeout.as_secs());

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            bail!("no screx receiver found via mDNS within {}s", timeout.as_secs());
        }

        let event = match receiver.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(ev) => ev,
            Err(_) => continue,
        };

        if let ServiceEvent::ServiceResolved(info) = event {
            let ip = info
                .get_addresses()
                .iter()
                .find(|a| matches!(a, mdns_sd::ScopedIp::V4(_)))
                .or_else(|| info.get_addresses().iter().next());

            let ip = match ip {
                Some(scoped) => scoped.to_ip_addr(),
                None => continue,
            };

            let port = info.get_port();
            let props = info.get_properties();

            let width: u32 = props
                .get_property_val_str("width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1920);
            let height: u32 = props
                .get_property_val_str("height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1080);

            let name = info.get_fullname().to_string();
            let addr = SocketAddr::new(ip, port);

            println!(
                "[discovery] found receiver '{}' at {} (screen {}x{})",
                name, addr, width, height
            );

            return Ok((
                DiscoveredReceiver {
                    name,
                    addr,
                    width: width & !1,
                    height: height & !1,
                },
                DiscoveryHandle { daemon: mdns },
            ));
        }
    }
}
