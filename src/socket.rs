use crate::recvfrom;
use crate::recvfrom::recv_from;
use crate::sendto::{detect_gso, send_to};
use libc::{ioctl, TIOCOUTQ};
use mio::net::UdpSocket;
use nix::cmsg_space;
use nix::sys::socket::sockopt::UdpGsoSegment;
use nix::sys::socket::{setsockopt, MsgFlags};
use quiche_endpoint::quiche;
use std::io;
use std::net::SocketAddr;
use std::os::fd::RawFd;

pub struct Socket {
    pub inner: UdpSocket,
    /// faster than calling inner.local_addr()
    pub local_addr: SocketAddr,
    pub cmsg_buf: Vec<u8>,
    pub flags: MsgFlags,
    pub enable_gro: bool,
    pub enable_pacing: bool,
    pub enable_gso: bool,
}

impl Socket {
    pub fn bind(addr: SocketAddr, disable_gro: bool, disable_pacing: bool, disable_gso: bool) -> io::Result<Self> {
        let inner = mio::net::UdpSocket::bind(addr)?;
        let local_addr = inner.local_addr()?;

        let enable_gro = !disable_gro && recvfrom::enable_gro(&inner);
        let enable_pacing = !disable_pacing && set_txtime_sockopt(&inner).is_ok();
        let enable_gso = !disable_gso && detect_gso(&inner, 9000);

        Ok(Self {
            inner,
            local_addr,
            cmsg_buf: cmsg_space!([u32; 1]),
            flags: MsgFlags::empty(),
            enable_gro,
            enable_pacing,
            enable_gso,
        })
    }

    pub fn recv(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, u16)> {
        recv_from(&self.inner, buf, &mut self.cmsg_buf, self.flags, self.enable_gro)
    }

    pub fn send(&self, buf: &[u8], send_info: &quiche::SendInfo, segment_size: usize) -> io::Result<usize> {
        send_to(&self.inner, buf, send_info, segment_size, self.enable_pacing, self.enable_gso)
    }
}

/// Set SO_TXTIME socket option.
///
/// This socket option is set to send to kernel the outgoing UDP
/// packet transmission time in the sendmsg syscall.
///
/// Note that this socket option is set only on linux platforms.
#[cfg(target_os = "linux")]
fn set_txtime_sockopt(sock: &mio::net::UdpSocket) -> io::Result<()> {
    use nix::sys::socket::setsockopt;
    use nix::sys::socket::sockopt::TxTime;
    use std::os::unix::io::AsRawFd;

    let config = nix::libc::sock_txtime {
        clockid: libc::CLOCK_MONOTONIC,
        flags: 0,
    };

    // mio::net::UdpSocket doesn't implement AsFd (yet?).
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(sock.as_raw_fd()) };

    setsockopt(&fd, TxTime, &config)?;

    Ok(())
}

pub fn send_buffer_queued(fd: RawFd) -> io::Result<usize> {
    let mut availabe: i32 = 0;
    match unsafe { ioctl(fd, TIOCOUTQ, &mut availabe) } {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(availabe as usize)
    }
}

pub fn gso_supported() -> bool {
    let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    setsockopt(&socket, UdpGsoSegment, &1500).is_ok()
}
