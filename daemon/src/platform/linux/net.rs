use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::ptr;

const PKTINFO_CMSG_SPACE: usize = 64;

/// Source address pinning for daemon→client UDP packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpSource {
    pub ip: std::net::Ipv4Addr,
    pub ifindex: u32,
}

/// Aligned storage for an IP_PKTINFO cmsg buffer.
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

/// Tune the UDP socket for streaming.
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

        let low_delay_tos: libc::c_int = 0x88;
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_TOS,
            &low_delay_tos as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );

        let priority: libc::c_int = 6;
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PRIORITY,
            &priority as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok(())
}

/// Send a batch of packets, optionally pinning the source IPv4 address/interface.
pub fn send_packets(
    sock: &UdpSocket,
    pkts: &[&[u8]],
    dest: SocketAddr,
    from_local_ip: Option<std::net::IpAddr>,
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

/// Find the interface index owning a local IPv4 address (0 if unknown).
pub fn ifindex_for_ipv4(ip: std::net::Ipv4Addr) -> u32 {
    unsafe {
        let mut ifap: *mut libc::ifaddrs = ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return 0;
        }
        let mut found = 0u32;
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family == libc::AF_INET as libc::sa_family_t
            {
                let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                let addr = std::net::Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
                if addr == ip {
                    found = libc::if_nametoindex(ifa.ifa_name);
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        found
    }
}

/// Resolve a session's UDP source pinning from the TCP handshake local IP.
pub fn udp_source_for_local_ip(local_ip: Option<std::net::IpAddr>) -> Option<UdpSource> {
    match local_ip {
        Some(std::net::IpAddr::V4(ip)) if !ip.is_unspecified() => Some(UdpSource {
            ip,
            ifindex: ifindex_for_ipv4(ip),
        }),
        _ => None,
    }
}

/// Build an IP_PKTINFO cmsg buffer that pins the IPv4 source address/interface.
pub fn build_pktinfo_cmsg(source: UdpSource) -> PktinfoCmsg {
    unsafe {
        let data_len = std::mem::size_of::<libc::in_pktinfo>() as libc::c_uint;
        let space = libc::CMSG_SPACE(data_len) as usize;
        assert!(space <= PKTINFO_CMSG_SPACE, "PKTINFO_CMSG_SPACE too small");
        let mut cmsg = PktinfoCmsg::default();
        let cmsg_hdr = cmsg.buf.as_mut_ptr() as *mut libc::cmsghdr;
        (*cmsg_hdr).cmsg_len = libc::CMSG_LEN(data_len) as _;
        (*cmsg_hdr).cmsg_level = libc::IPPROTO_IP;
        (*cmsg_hdr).cmsg_type = libc::IP_PKTINFO;
        let pi = libc::CMSG_DATA(cmsg_hdr) as *mut libc::in_pktinfo;
        std::ptr::write_unaligned(
            pi,
            libc::in_pktinfo {
                ipi_ifindex: source.ifindex as libc::c_int,
                ipi_spec_dst: libc::in_addr {
                    s_addr: u32::from(source.ip).to_be(),
                },
                ipi_addr: libc::in_addr { s_addr: 0 },
            },
        );
        cmsg.controllen = space;
        cmsg
    }
}

/// `send_to` with an optional pinned IPv4 source address (IP_PKTINFO).
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

    let (cmsg_ptr, controllen, _built) = match cmsg_buf {
        Some(b) if !b.is_empty() => (b.as_ptr() as *mut libc::c_void, b.len(), None),
        _ => {
            let built = build_pktinfo_cmsg(source);
            let ptr = built.buf.as_ptr() as *mut libc::c_void;
            let len = built.controllen();
            (ptr, len, Some(built))
        }
    };

    let (mut addr_storage, addr_len) = socket_addr_to_raw(dst);
    let mut iov = libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let msg = libc::msghdr {
        msg_name: (&mut addr_storage as *mut libc::sockaddr_storage).cast(),
        msg_namelen: addr_len,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: cmsg_ptr,
        msg_controllen: controllen as _,
        msg_flags: 0,
    };
    let ret = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, 0) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

/// Send a batch of packets using `sendmmsg` when available.
pub fn sendmmsg_batch(
    socket: &UdpSocket,
    pkts: &[&[u8]],
    dest: SocketAddr,
    source: Option<UdpSource>,
) -> std::io::Result<usize> {
    let batch_len = pkts.len();
    if batch_len == 0 {
        return Ok(0);
    }
    let (mut addr_storage, addr_len) = socket_addr_to_raw(dest);
    let mut iovecs = Vec::with_capacity(batch_len);
    let mut msgs = Vec::with_capacity(batch_len);

    let cmsg_buf = source.map(build_pktinfo_cmsg);
    let (cmsg_ptr, cmsg_len) = match (source, dest) {
        (Some(_), SocketAddr::V4(_)) => match cmsg_buf.as_ref() {
            Some(buf) => (
                buf.as_slice().as_ptr() as *mut libc::c_void,
                buf.controllen() as libc::size_t,
            ),
            None => (ptr::null_mut(), 0),
        },
        _ => (ptr::null_mut(), 0),
    };

    for pkt in pkts {
        iovecs.push(libc::iovec {
            iov_base: pkt.as_ptr() as *mut libc::c_void,
            iov_len: pkt.len(),
        });
    }

    for i in 0..batch_len {
        msgs.push(libc::mmsghdr {
            msg_hdr: libc::msghdr {
                msg_name: (&mut addr_storage as *mut libc::sockaddr_storage).cast(),
                msg_namelen: addr_len,
                msg_iov: &mut iovecs[i] as *mut libc::iovec,
                msg_iovlen: 1,
                msg_control: cmsg_ptr,
                msg_controllen: cmsg_len,
                msg_flags: 0,
            },
            msg_len: 0,
        });
    }

    let ret = unsafe { libc::sendmmsg(socket.as_raw_fd(), msgs.as_mut_ptr(), batch_len as u32, 0) };

    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

pub fn socket_addr_to_raw(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    match addr {
        SocketAddr::V4(addr) => {
            let sockaddr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: addr.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_be_bytes(addr.ip().octets()).to_be(),
                },
                sin_zero: [0; 8],
            };
            let mut storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
            unsafe {
                ptr::write(
                    (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>(),
                    sockaddr,
                );
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(addr) => {
            let sockaddr = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: addr.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: addr.ip().octets(),
                },
                sin6_scope_id: addr.scope_id(),
            };
            let mut storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
            unsafe {
                ptr::write(
                    (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>(),
                    sockaddr,
                );
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}
