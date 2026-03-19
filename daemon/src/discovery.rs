use std::process::Stdio;

use anyhow::Result;
use tokio::process::{Child, Command};

#[derive(Debug)]
pub struct DiscoveryHandle {
    child: Child,
}

impl DiscoveryHandle {
    pub async fn stop(&mut self) {
        if let Some(id) = self.child.id() {
            println!("[discovery] stopping avahi advertisement process pid={id}");
        }
        if let Err(err) = self.child.kill().await {
            eprintln!("[discovery] failed to stop avahi process: {err}");
        }
    }
}

pub async fn start_avahi_advertisement(
    service_name: &str,
    service_type: &str,
    stream_port: u16,
    control_port: u16,
) -> Result<Option<DiscoveryHandle>> {
    let mut cmd = Command::new("avahi-publish-service");
    cmd.arg(service_name)
        .arg(service_type)
        .arg(stream_port.to_string())
        .arg(format!("control={control_port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    match cmd.spawn() {
        Ok(child) => {
            println!(
                "[discovery] advertising service name='{service_name}' type='{service_type}' port={stream_port} txt_control_port={control_port}"
            );
            Ok(Some(DiscoveryHandle { child }))
        }
        Err(err) => {
            // Keep daemon runnable in environments without avahi tooling.
            eprintln!(
                "[discovery] avahi-publish-service unavailable ({err}); running without mDNS advertisement"
            );
            Ok(None)
        }
    }
}
