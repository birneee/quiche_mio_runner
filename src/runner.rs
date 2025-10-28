use crate::config::Config;
use crate::socket::Socket;
use crate::sockets::MioSockets;
use log::{debug, error, trace};
use mio::unix::pipe::Receiver;
use quiche_endpoint as endpoint;
use quiche_endpoint::MAX_UDP_PAYLOAD;
use quiche_endpoint::{quiche, Endpoint};
use slab::Slab;
use std::cmp::min;
use std::io;
use std::time::Duration;

/// Runner handles socket IO and the run loop for an `Endpoint`, which multiplexes client and server QUIC connections.
/// uses `mio` for IO.
pub struct Runner<TConnAppData, TAppData, TExternalEventValue> {
    pub config: Config<TConnAppData, TAppData, TExternalEventValue>,
    pub buf: [u8; MAX_UDP_PAYLOAD],
    pub sockets: MioSockets,
    pub mio_events: mio::Events,
    pub endpoint: Endpoint<TConnAppData, TAppData>,
    pub registry: Registry<TExternalEventValue>,
    app_timeout: Option<Duration>,
}

impl<'a, TConnAppData, TAppData, TExternalEventValue> Runner<TConnAppData, TAppData, TExternalEventValue> {

    /// construct new runner
    /// at least one socket should be registered with `Self::register_socket`
    pub fn new(config: Config<TConnAppData, TAppData, TExternalEventValue>,endpoint: Endpoint<TConnAppData, TAppData>, close_pipe_rx: Option<&mut Receiver>) -> Self {
        let poll = mio::Poll::new().unwrap();
        let mut events = Slab::new();
        if let Some(close_pipe_rx) = close_pipe_rx {
            let token = events.insert(Event::Close);
            poll.registry().register(close_pipe_rx, mio::Token(token), mio::Interest::READABLE).unwrap();
        }
        Self {
            config,
            buf: [0; MAX_UDP_PAYLOAD],
            sockets: Default::default(),
            mio_events: mio::Events::with_capacity(1024),
            endpoint,
            registry: Registry {
                events,
                poll,
            },
            app_timeout: None,
        }
    }

    /// register socket for receiving and sending
    pub fn register_socket(&mut self, socket: Socket) {
        let socket_token = self.sockets.insert(socket);
        let event_token = self.registry.events.insert(Event::Socket(socket_token));
        self.registry.poll.registry()
            .register(
                &mut unsafe { self.sockets.sockets.get_unchecked_mut(socket_token) }.inner,
                mio::Token(event_token),
                mio::Interest::READABLE,
            )
            .unwrap();
    }


    /// run protocol logic,
    /// this function return when all connections are closed `Self::server` is `None`.
    /// if `Self::server` is `Some` the function will never return.
    pub fn run(&mut self) {
        'run: loop {
            let timeout = match (self.endpoint.has_pending_sends(), self.endpoint.timeout(), self.app_timeout.take()) {
                (true, _, _) => Some(Duration::from_secs(0)),
                (false, Some(d), None) => Some(d),
                (false, None, Some(d)) => Some(d),
                (false, Some(quic_timeout), Some(app_timeout)) => Some(min(quic_timeout, app_timeout)),
                (false, None, None) => None,
            };

            trace!("poll with timeout {:?}", timeout);
            let mut poll_res = self.registry.poll.poll(&mut self.mio_events, timeout);
            while let Err(e) = poll_res.as_ref() {
                if e.kind() == io::ErrorKind::Interrupted {
                    trace!("mio poll() call failed, retrying: {:?}", e);
                    poll_res = self.registry.poll.poll(&mut self.mio_events, timeout);
                } else {
                    panic!("mio poll() call failed fatally: {:?}", e);
                }
            }

            (self.config.pre_handle_recvs)(self);

            if self.mio_events.is_empty() && !self.endpoint.has_pending_sends() {
                self.endpoint.on_timeout();
            } else {
                for mio_event in &self.mio_events {
                    let event = self.registry.events.get(mio_event.token().into()).unwrap();
                    let r = Self::handle_event(
                        mio_event,
                        event,
                        &mut self.sockets.sockets,
                        self.buf.as_mut(),
                        &mut self.endpoint,
                        self.config.on_external_event,
                    );
                    match r {
                        Err(endpoint::Error::CloseByUser) => {
                            break 'run
                        },
                        Err(e) => { panic!("{:?}", e)},
                        Ok(()) => {},
                    }
                }
            }

            (self.config.post_handle_recvs)(self);

            // send as long as packets are available
            loop {
                let ok = match self.endpoint.send_packets_out(&mut self.buf) {
                    Ok(v) => v,
                    Err(quiche_endpoint::Error::Quiche(quiche::Error::Done)) => break,
                    Err(e) => panic!("unexpected error: {:?}", e),
                };
                match self.sockets.send(&self.buf[..ok.total], &ok.send_info, ok.segment_size) {
                    Ok(_) => {}
                    Err(e) => error!("error sending UDP datagram: {:?}", e),
                }
            }

            self.endpoint.collect_garbage();

            if !self.endpoint.is_server() && self.endpoint.num_conns() == 0 {
                break; // stop because all client connections are closed
            }
        }
    }

    pub fn handle_event(
        mio_event: &mio::event::Event,
        event: &Event<TExternalEventValue>,
        sockets: &mut Slab<Socket>,
        buf: &mut [u8],
        endpoint: &mut Endpoint<TConnAppData, TAppData>,
        on_external_event: Option<fn(&mut Endpoint<TConnAppData, TAppData>, &TExternalEventValue)>,
    ) -> endpoint::Result<()>  {
        match event {
            Event::Close => {
                Err(endpoint::Error::CloseByUser)
            }
            Event::Socket(_) => {
                Self::handle_readable_event(mio_event, event, sockets, buf, endpoint)
            }
            Event::External(v) => {
                if let Some(on_external_event) = on_external_event {
                    on_external_event(endpoint, v);
                }
                Ok(())
            }
        }
    }

    pub fn handle_readable_event(
        mio_event: &mio::event::Event,
        event: &Event<TExternalEventValue>,
        sockets: &mut Slab<Socket>,
        buf: &mut [u8],
        endpoint: &mut Endpoint<TConnAppData, TAppData>,
    ) -> endpoint::Result<()> {
        debug_assert!(mio_event.is_readable());
        let socket = if let Event::Socket(socket_token) = event {
            &mut sockets[*socket_token]
        } else {
            unreachable!()
        };
        let local_addr = socket.local_addr;
        'read: loop {
            let (len, from, segment_size) = match socket.recv(buf) {
                Ok(v) => v,

                Err(e) => {
                    // There are no more UDP packets to read on this socket.
                    // Process subsequent events.
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        trace!("{}: recv() would block", local_addr);
                        break 'read;
                    }

                    return Err(endpoint::Error::IO(e));
                }
            };

            let segment_size = if segment_size == 0 {
                len
            } else {
                segment_size as usize
            };

            trace!("{}: got {} bytes of {} byte segments", local_addr, len, segment_size);

            let info = quiche::RecvInfo {
                to: local_addr,
                from,
            };

            // process GRO segments
            // if disabled just process the one
            for segment in buf[..len].chunks_mut(segment_size) {
                match endpoint.recv(segment, info) {
                    Ok(_) => {} // everything ok
                    Err(endpoint::Error::InvalidHeader(e)) => {
                        error!("Parsing packet header failed: {:?}", e);
                        continue;
                    }
                    Err(endpoint::Error::UnknownConnID) => {
                        debug!("Received unknown connection id packet");
                        continue;
                    }
                    Err(endpoint::Error::IO(e)) => {
                        if e.kind() == io::ErrorKind::WouldBlock {
                            trace!("send() would block");
                            break;
                        }

                        panic!("send() failed: {:?}", e);
                    }
                    Err(endpoint::Error::InvalidAddrToken) => {
                        continue
                    }
                    Err(endpoint::Error::InvalidConnID) => {
                        continue
                    }
                    Err(endpoint::Error::QuicheRecvFailed(e)) => {
                        error!("{}: quiche recv failed: {:?}", local_addr, e);
                        continue
                    }
                    e => {
                        panic!("unexpected error: {:?}", e)
                    }
                }
            }
        }
        Ok(())
    }

    pub fn registry(&mut self) -> &mut Registry<TExternalEventValue> {
        &mut self.registry
    }

    /// Set a timeout for the application logic.
    /// Must be set again after every iteration, ideally in the `post_handle_recvs` callback.
    pub fn set_app_timeout(&mut self, timeout: Duration) {
        self.app_timeout = Some(timeout);
    }
}

pub struct Registry<TExternalEventValue> {
    events: Slab<Event<TExternalEventValue>>,
    poll: mio::Poll,
}

impl <TExternalEventValue> Registry<TExternalEventValue> {
    pub fn register_external<S>(
        &mut self,
        source: &mut S,
        interest: mio::Interest,
        value: TExternalEventValue
    )
    where S: mio::event::Source + ?Sized
    {
        let event_token = self.events.insert(Event::External(value));
        self.poll.registry().register(
            source,
            mio::Token(event_token),
            interest,
        ).unwrap();
    }
}

pub enum Event<T> {
    Close,
    Socket(usize),
    External(T)
}
