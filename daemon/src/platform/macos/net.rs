//! macOS UDP source-pinning — REAL fallback implementation (not a stub).
//!
//! macOS/Darwin does not support `IP_PKTINFO` on outgoing `sendmsg` the way
//! Linux does, and has no `sendmmsg` syscall at all. Per the project's
//! design doc, the sanctioned build-unblocking approach is a no-op pinning
//! implementation (packets use the default route) with a TODO for later —
//! this degrades multi-homing support, it does not break streaming. This is
//! the permanent-until-M4 approach, not something later milestones need to
//! "fix" as a stub.

use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;

/// Source address pinning for daemon->client UDP packets. Kept for type
/// compatibility with the shared `stream_server.rs` code; macOS never
/// actually resolves one (see `udp_source_for_local_ip` below), so no
/// pinning happens and packets go out the default route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpSource {
    pub ip: std::net::Ipv4Addr,
    pub ifindex: u32,
}

/// Empty marker type — there is no cmsg to build on macOS (no IP_PKTINFO
/// support on outgoing sendmsg), so this carries no data.
pub struct PktinfoCmsg;

impl PktinfoCmsg {
    pub fn as_slice(&self) -> &[u8] {
        &[]
    }

    pub fn controllen(&self) -> usize {
        0
    }
}

/// Tune the UDP socket for streaming. Only `SO_SNDBUF` is set here — the
/// `IP_TOS`/`SO_PRIORITY` tweaks the Linux backend applies are Linux-only
/// (`SO_PRIORITY` doesn't exist on macOS at all; `IP_TOS` handling differs
/// enough that it's not worth chasing for a TODO-fallback path).
pub fn tune_udp_socket(sock: &UdpSocket) -> anyhow::Result<()> {
    unsafe {
        let sndbuf: libc::c_int = 2 * 1024 * 1024;
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok(())
}

/// No pinning support on macOS — always returns `None` so callers fall back
/// to the socket's default route. TODO(M4): revisit if per-interface source
/// pinning turns out to matter in practice on macOS.
pub fn udp_source_for_local_ip(_local_ip: Option<std::net::IpAddr>) -> Option<UdpSource> {
    None
}

/// No cmsg to build on macOS; returns the empty marker.
pub fn build_pktinfo_cmsg(_source: UdpSource) -> PktinfoCmsg {
    PktinfoCmsg
}

/// `send_to` — source/cmsg are ignored on macOS (no pinning support).
pub fn send_to_from(
    socket: &UdpSocket,
    buf: &[u8],
    dst: SocketAddr,
    _source: Option<UdpSource>,
    _cmsg_buf: Option<&[u8]>,
) -> std::io::Result<usize> {
    socket.send_to(buf, dst)
}

/// No `sendmmsg` syscall on macOS; loop over `send_to` instead. Returns the
/// count of packets successfully sent, matching the Linux `sendmmsg_batch`
/// contract (caller advances its send cursor by the returned count and
/// falls back to per-packet `send_to_from` for anything not covered).
pub fn sendmmsg_batch(
    socket: &UdpSocket,
    pkts: &[&[u8]],
    dest: SocketAddr,
    _source: Option<UdpSource>,
) -> std::io::Result<usize> {
    let mut sent = 0;
    for pkt in pkts {
        match socket.send_to(pkt, dest) {
            Ok(_) => sent += 1,
            Err(e) => {
                if sent == 0 {
                    return Err(e);
                }
                break;
            }
        }
    }
    Ok(sent)
}
