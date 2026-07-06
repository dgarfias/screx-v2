use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket};

use windows::Win32::Networking::WinSock::{
    WSASendMsg, ADDRESS_FAMILY, AF_INET, CMSGHDR, IN_ADDR, IN_ADDR_0, IN_PKTINFO, IPPROTO_IP,
    IP_PKTINFO, SOCKADDR, SOCKADDR_IN, SOCKET, WSABUF, WSAMSG,
};

/// Total buffer size for the IP_PKTINFO ancillary data: WSA_CMSG_ALIGN(sizeof(CMSGHDR))
/// + WSA_CMSG_ALIGN(sizeof(IN_PKTINFO)) = 16 + 8 = 24 on x64; rounded up with headroom.
const PKTINFO_CMSG_SPACE: usize = 32;

/// Source address pinning for daemon→client UDP packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpSource {
    pub ip: std::net::Ipv4Addr,
    pub ifindex: u32,
}

/// Aligned storage for a WSA IP_PKTINFO cmsg buffer.
#[repr(C, align(8))]
pub struct PktinfoCmsg {
    buf: [u8; PKTINFO_CMSG_SPACE],
    controllen: usize,
}

impl PktinfoCmsg {
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.controllen]
    }

    pub fn controllen(&self) -> usize {
        self.controllen
    }
}

impl Default for PktinfoCmsg {
    fn default() -> Self {
        Self {
            buf: [0u8; PKTINFO_CMSG_SPACE],
            controllen: 0,
        }
    }
}

pub fn tune_udp_socket(sock: &UdpSocket) -> anyhow::Result<()> {
    use socket2::Socket;
    let s = unsafe { Socket::from_raw_socket(sock.as_raw_socket()) };
    s.set_send_buffer_size(2 * 1024 * 1024)?;
    // Release the raw socket without closing it; the std::net::UdpSocket still owns it.
    let _ = s.into_raw_socket();
    Ok(())
}

/// Send a batch of packets, optionally pinning the source IPv4 address/interface —
/// mirrors platform/linux/net.rs's `send_packets` (IP_PKTINFO via `sendmsg`), just
/// using Windows' `WSASendMsg` instead of POSIX `sendmsg`.
pub fn send_packets(
    sock: &UdpSocket,
    pkts: &[&[u8]],
    dest: SocketAddr,
    from_local_ip: Option<IpAddr>,
) -> anyhow::Result<()> {
    let source = udp_source_for_local_ip(from_local_ip);
    let cmsg_buf = source.map(build_pktinfo_cmsg);
    for pkt in pkts {
        send_to_from(
            sock,
            pkt,
            dest,
            source,
            cmsg_buf.as_ref().map(|b| b.as_slice()),
        )?;
    }
    Ok(())
}

/// Resolve a session's UDP source pinning from the TCP handshake local IP —
/// same intent as the Linux version: "send from the address we received on."
/// `ifindex` isn't required by Windows' IP_PKTINFO send path (only `ipi_addr`
/// matters for overriding the outgoing source address), so it's left 0.
pub fn udp_source_for_local_ip(local_ip: Option<IpAddr>) -> Option<UdpSource> {
    match local_ip {
        Some(IpAddr::V4(ip)) if !ip.is_unspecified() => Some(UdpSource { ip, ifindex: 0 }),
        _ => None,
    }
}

const fn wsa_cmsg_align(len: usize) -> usize {
    const ALIGN: usize = std::mem::size_of::<usize>();
    (len + ALIGN - 1) & !(ALIGN - 1)
}

/// Build a WSA IP_PKTINFO cmsg buffer that pins the IPv4 source address.
/// Windows' `IN_PKTINFO` (unlike Linux's `in_pktinfo`) has only `ipi_addr` +
/// `ipi_ifindex` — no separate `ipi_spec_dst` — and `ipi_addr` is exactly the
/// field Windows honors as the overridden source address for an outgoing
/// packet on a socket bound to `INADDR_ANY`.
pub fn build_pktinfo_cmsg(source: UdpSource) -> PktinfoCmsg {
    unsafe {
        let hdr_len = wsa_cmsg_align(std::mem::size_of::<CMSGHDR>());
        let data_len = std::mem::size_of::<IN_PKTINFO>();
        let space = hdr_len + wsa_cmsg_align(data_len);
        assert!(space <= PKTINFO_CMSG_SPACE, "PKTINFO_CMSG_SPACE too small");

        let mut cmsg = PktinfoCmsg::default();
        let hdr_ptr = cmsg.buf.as_mut_ptr() as *mut CMSGHDR;
        std::ptr::write_unaligned(
            hdr_ptr,
            CMSGHDR {
                cmsg_len: hdr_len + data_len,
                cmsg_level: IPPROTO_IP.0,
                cmsg_type: IP_PKTINFO,
            },
        );

        let data_ptr = cmsg.buf.as_mut_ptr().add(hdr_len) as *mut IN_PKTINFO;
        std::ptr::write_unaligned(
            data_ptr,
            IN_PKTINFO {
                ipi_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from(source.ip).to_be(),
                    },
                },
                ipi_ifindex: source.ifindex,
            },
        );

        cmsg.controllen = space;
        cmsg
    }
}

/// `send_to` with an optional pinned IPv4 source address (IP_PKTINFO via `WSASendMsg`).
pub fn send_to_from(
    socket: &UdpSocket,
    buf: &[u8],
    dst: SocketAddr,
    source: Option<UdpSource>,
    cmsg_buf: Option<&[u8]>,
) -> std::io::Result<usize> {
    let source = match (source, dst) {
        (Some(s), SocketAddr::V4(_)) => s,
        _ => return socket.send_to(buf, dst),
    };
    let dst_v4 = match dst {
        SocketAddr::V4(v4) => v4,
        SocketAddr::V6(_) => return socket.send_to(buf, dst),
    };

    // Deferred-init: only build a fresh cmsg buffer if the caller didn't
    // already hand us one. `cmsg_slice` borrows `built` (when used) rather
    // than a raw pointer captured before a move, so the borrow checker
    // guarantees it stays valid for the rest of this function.
    let built;
    let cmsg_slice: &[u8] = match cmsg_buf {
        Some(b) if !b.is_empty() => b,
        _ => {
            built = build_pktinfo_cmsg(source);
            built.as_slice()
        }
    };
    let mut sockaddr = SOCKADDR_IN {
        sin_family: ADDRESS_FAMILY(AF_INET.0),
        sin_port: dst_v4.port().to_be(),
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from(*dst_v4.ip()).to_be(),
            },
        },
        sin_zero: [0; 8],
    };

    let mut wsabuf = WSABUF {
        len: buf.len() as u32,
        buf: windows::core::PSTR(buf.as_ptr() as *mut u8),
    };

    let control = WSABUF {
        len: cmsg_slice.len() as u32,
        buf: windows::core::PSTR(cmsg_slice.as_ptr() as *mut u8),
    };

    let msg = WSAMSG {
        name: &mut sockaddr as *mut SOCKADDR_IN as *mut SOCKADDR,
        namelen: std::mem::size_of::<SOCKADDR_IN>() as i32,
        lpBuffers: &mut wsabuf,
        dwBufferCount: 1,
        Control: control,
        dwFlags: 0,
    };

    let win_socket = SOCKET(socket.as_raw_socket() as usize);
    let mut sent: u32 = 0;
    let ret = unsafe { WSASendMsg(win_socket, &msg, 0, Some(&mut sent), None, None) };
    if ret != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(sent as usize)
    }
}

/// No Windows equivalent of `sendmmsg` (batched datagram send); loop instead.
pub fn sendmmsg_batch(
    socket: &UdpSocket,
    pkts: &[&[u8]],
    dest: SocketAddr,
    source: Option<UdpSource>,
) -> std::io::Result<usize> {
    let cmsg_buf = source.map(build_pktinfo_cmsg);
    let mut sent = 0;
    for pkt in pkts {
        send_to_from(
            socket,
            pkt,
            dest,
            source,
            cmsg_buf.as_ref().map(|b| b.as_slice()),
        )?;
        sent += 1;
    }
    Ok(sent)
}
