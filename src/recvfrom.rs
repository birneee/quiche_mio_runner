use nix::sys::socket::MsgFlags;
use std::io;
use std::net::SocketAddr;

/// For Linux, try to detect GRO is available.
#[cfg(target_os = "linux")]
pub fn enable_gro(socket: &mio::net::UdpSocket) -> bool {
    use nix::sys::socket::setsockopt;
    use nix::sys::socket::sockopt::UdpGroSegment;
    use std::os::fd::AsRawFd;

    // mio::net::UdpSocket doesn't implement AsFd (yet?).
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(socket.as_raw_fd()) };
    setsockopt(&fd, UdpGroSegment, &true).is_ok()
}

/// For non-Linux, GRO is not available.
#[cfg(not(target_os = "linux"))]
pub fn enable_gro(_socket: &mio::net::UdpSocket) -> bool {
    false
}

// Receive packet using recvmsg() with GRO
#[cfg(target_os = "linux")]
fn recv_from_gro(
    socket: &mio::net::UdpSocket,
    buf: &mut [u8],
    cmsg_buf: &mut Vec<u8>,
    flags: MsgFlags,
) -> io::Result<(usize, SocketAddr, u16)> {
    use libc::c_uint;
    use nix::sys::socket::ControlMessageOwned::UdpGroSegments;
    use nix::sys::socket::{recvmsg, AddressFamily, SockaddrLike, SockaddrStorage};
    use std::io::IoSliceMut;
    use std::mem::size_of;
    use std::net::{SocketAddrV4, SocketAddrV6};
    use std::os::fd::AsRawFd;

    unsafe { debug_assert!(cmsg_buf.capacity() >= libc::CMSG_SPACE(size_of::<u32>() as c_uint) as usize); }

    let mut iov = [IoSliceMut::new(buf)];
    let sockfd = socket.as_raw_fd();

    match recvmsg::<SockaddrStorage>(
        sockfd,
        &mut iov,
        Some(cmsg_buf),
        flags,
    ) {
        Ok(msg) => {
            let mut gro_size = 0;
            for cmsg in msg.cmsgs()? {
                match cmsg {
                    UdpGroSegments(s) => gro_size = s,
                    _ => panic!("unexpected control message")
                }
            }
            let addr = msg.address.and_then(|a| match a.family()? {
                AddressFamily::Inet => a.as_sockaddr_in().map(|a| SocketAddr::V4(SocketAddrV4::new(a.ip(), a.port()))),
                AddressFamily::Inet6 => a.as_sockaddr_in6().map(|a| SocketAddr::V6(SocketAddrV6::new(a.ip(), a.port(), a.flowinfo(), a.scope_id()))),
                _ => unreachable!()
            }).unwrap();

            Ok((msg.bytes, addr, gro_size as u16))
        }
        Err(e) => Err(e.into())
    }
}


#[cfg(target_os = "linux")]
pub fn recv_from(
    socket: &mio::net::UdpSocket,
    buf: &mut [u8],
    cmsg_buf: &mut Vec<u8>,
    flags: MsgFlags,
    enable_gro: bool,
) -> io::Result<(usize, SocketAddr, u16)> {
    if enable_gro {
        recv_from_gro(socket, buf, cmsg_buf, flags)
    } else {
        socket.recv_from(buf).map(|(size, addr)| (size, addr, size as u16))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn recv_from(
    socket: &mio::net::UdpSocket,
    buf: &mut [u8],
    _cmsg_buf: &mut Vec<u8>,
    _flags: MsgFlags,
    _enable_gro: bool,
) -> io::Result<(usize, SocketAddr, u16)> {
    socket.recv_from(buf).map(|(size, addr)| (size, addr, size as u16))
}

