use libc::c_uint;
use nix::sys::socket::sockopt::UdpGroSegment;
use nix::sys::socket::ControlMessageOwned::UdpGroSegments;
use nix::sys::socket::{recvmsg, setsockopt, AddressFamily, MsgFlags, SockaddrLike, SockaddrStorage};
use std::io;
use std::io::IoSliceMut;
use std::mem::size_of;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::AsRawFd;


/// For Linux, try to detect GRO is available.
#[cfg(target_os = "linux")]
pub fn enable_gro(socket: &mio::net::UdpSocket) -> bool {
    // mio::net::UdpSocket doesn't implement AsFd (yet?).
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(socket.as_raw_fd()) };
    setsockopt(&fd, UdpGroSegment, &true).is_ok()
}

// Receive packet using recvmsg() with GRO
#[cfg(target_os = "linux")]
fn recv_from_gro(
    socket: &mio::net::UdpSocket,
    buf: &mut [u8],
    cmsg_buf: &mut Vec<u8>,
    flags: MsgFlags,
) -> io::Result<(usize, SocketAddr, u16)> {
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
            let addr = msg.address.map(|a| match a.family()? {
                AddressFamily::Inet => a.as_sockaddr_in().map(|a| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(a.ip()), a.port()))),
                AddressFamily::Inet6 => a.as_sockaddr_in6().map(|a| SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(a.ip()), a.port(), a.flowinfo(), a.scope_id()))),
                _ => unreachable!()
            }).flatten().unwrap();

            Ok((msg.bytes, addr, gro_size as u16))
        }
        Err(e) => Err(e.into())
    }
}


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

