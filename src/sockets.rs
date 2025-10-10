use crate::socket::Socket;
use quiche_endpoint::quiche::SendInfo;
use slab::Slab;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;

#[derive(Default)]
pub struct MioSockets {
    pub sockets: Slab<Socket>,
    pub src_addr_to_key: HashMap<SocketAddr, usize>,
}

impl MioSockets {
    pub fn send(&self, buf: &[u8], send_info: &SendInfo, segment_size: usize) -> io::Result<usize> {
        let key = *self.src_addr_to_key.get(&send_info.from).unwrap();
        let socket = unsafe { self.sockets.get_unchecked(key) };
        socket.send(
            buf,
            send_info,
            segment_size,
        )
    }
}