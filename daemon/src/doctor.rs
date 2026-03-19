use std::env;
use std::fs;
use std::net::IpAddr;
use std::process::Command;

use anyhow::{bail, Result};

pub fn run_doctor() -> Result<()> {
    println!("screx-daemon doctor checks");
    println!("-------------------------");

    let mut failures = 0_u32;
    check("ffmpeg binary", check_ffmpeg_binary(), &mut failures);
    check(
        "ffmpeg hevc_vaapi encoder",
        check_hevc_vaapi_encoder(),
        &mut failures,
    );
    check("VA-API render node", check_render_node(), &mut failures);
    check(
        "avahi-publish-service tool",
        check_avahi_publish_service(),
        &mut failures,
    );
    check("target IP env", check_target_ip_env(), &mut failures);

    if failures > 0 {
        println!("doctor result: FAILED ({failures} check(s) failed)");
        bail!("host is not MVP-ready yet");
    }

    println!("doctor result: OK");
    Ok(())
}

fn check(label: &str, result: Result<()>, failures: &mut u32) {
    match result {
        Ok(()) => println!("[ok]   {label}"),
        Err(err) => {
            *failures += 1;
            println!("[fail] {label}: {err}");
        }
    }
}

fn check_ffmpeg_binary() -> Result<()> {
    let out = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map_err(|err| anyhow::anyhow!("unable to execute ffmpeg: {err}"))?;
    if out.status.success() {
        Ok(())
    } else {
        bail!("ffmpeg returned non-zero status")
    }
}

fn check_hevc_vaapi_encoder() -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|err| anyhow::anyhow!("unable to list ffmpeg encoders: {err}"))?;
    if !out.status.success() {
        bail!("ffmpeg -encoders failed");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("hevc_vaapi") {
        Ok(())
    } else {
        bail!("hevc_vaapi encoder not found")
    }
}

fn check_render_node() -> Result<()> {
    let node = "/dev/dri/renderD128";
    let meta = fs::metadata(node).map_err(|err| anyhow::anyhow!("{node} missing: {err}"))?;
    if meta.permissions().readonly() {
        bail!("{node} is read-only for current user")
    }
    Ok(())
}

fn check_avahi_publish_service() -> Result<()> {
    let out = Command::new("avahi-publish-service")
        .arg("--help")
        .output()
        .map_err(|err| anyhow::anyhow!("unable to execute avahi-publish-service: {err}"))?;
    if out.status.success() || !out.stdout.is_empty() || !out.stderr.is_empty() {
        Ok(())
    } else {
        bail!("avahi-publish-service not functional")
    }
}

fn check_target_ip_env() -> Result<()> {
    let raw = env::var("SCREX_TARGET_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
    raw.parse::<IpAddr>()
        .map_err(|err| anyhow::anyhow!("SCREX_TARGET_IP invalid: {err}"))?;
    Ok(())
}
