use crate::config::Config;
use crate::socket::Socket;
use crate::sockets::MioSockets;
use log::{debug, error, trace};
use quiche_endpoint as endpoint;
use quiche_endpoint::MAX_UDP_PAYLOAD;
use quiche_endpoint::{quiche, Endpoint};
use slab::Slab;
use std::cmp::min;
use std::io;
use std::net::SocketAddr;
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
    /// if the application marked the runner as closed
    marked_closed: bool,
}

impl<TConnAppData, TAppData, TExternalEventValue> Runner<TConnAppData, TAppData, TExternalEventValue> {

    /// construct new runner
    /// at least one socket should be registered with `Self::register_socket`
    pub fn new(config: Config<TConnAppData, TAppData, TExternalEventValue>, endpoint: Endpoint<TConnAppData, TAppData>) -> Self {
        let poll = mio::Poll::new().unwrap();
        let events = Slab::new();
        Self {
            config,
            buf: [0; _],
            sockets: Default::default(),
            mio_events: mio::Events::with_capacity(1024),
            endpoint,
            registry: Registry {
                events,
                poll,
                owned_sources: Vec::new(),
            },
            app_timeout: None,
            marked_closed: false,
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
            self.endpoint.send_all(&mut self.buf, |out, send_info, segment_size| {
                match self.sockets.send(out, send_info, segment_size) {
                    Ok(written) => {
                        assert_eq!(written, out.len());
                        Ok(())
                    }
                    Err(e) => {
                        error!("error sending UDP datagram: {:?}", e);
                        Err(e)
                    },
                }
            });

            self.endpoint.collect_garbage(self.config.on_close);

            if !self.endpoint.is_server() && self.endpoint.num_conns() == 0 {
                break 'run; // stop because all client connections are closed
            }
            if self.marked_closed {
                break 'run; // application closed runner
            }
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn handle_event(
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
                Self::handle_readable_event(event, sockets, buf, endpoint)
            }
            Event::External(v) => {
                if let Some(on_external_event) = on_external_event {
                    on_external_event(endpoint, v);
                }
                Ok(())
            }
        }
    }

    // returns WouldBlock if nothing to receive
    fn recv_single<T1, T2>(socket: &mut Socket, buf: &mut [u8], local_addr: SocketAddr, endpoint: &mut Endpoint<T1, T2>) -> io::Result<()> {
        let (len, from, segment_size) = match socket.recv(buf) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // There are no more UDP packets to read on this socket.
                // Process subsequent events.
                return Err(e)
            },
            Err(e) => {
                unimplemented!("{:?}", e);
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
        'segment: for segment in buf[..len].chunks_mut(segment_size) {
            match endpoint.recv(segment, info) {
                Ok(_) => {} // everything ok
                Err(endpoint::Error::InvalidHeader(e)) => {
                    error!("Parsing packet header failed: {:?}", e);
                    continue 'segment;
                }
                Err(endpoint::Error::UnknownConnID) => {
                    debug!("Received unknown connection id packet");
                    continue 'segment;
                }
                Err(endpoint::Error::InvalidAddrToken) => {
                    continue 'segment
                }
                Err(endpoint::Error::InvalidConnID) => {
                    continue 'segment
                }
                Err(endpoint::Error::QuicheRecvFailed(e)) => {
                    error!("{}: quiche recv failed: {:?}", local_addr, e);
                    continue 'segment
                }
                e => {
                    panic!("unexpected error: {:?}", e)
                }
            }
        }
        Ok(())
    }

    pub fn handle_readable_event(
        event: &Event<TExternalEventValue>,
        sockets: &mut Slab<Socket>,
        buf: &mut [u8],
        endpoint: &mut Endpoint<TConnAppData, TAppData>,
    ) -> endpoint::Result<()> {
        let socket = if let Event::Socket(socket_token) = event {
            &mut sockets[*socket_token]
        } else {
            unreachable!()
        };
        let local_addr = socket.local_addr;
        'read: loop {
            match Self::recv_single(
                socket,
                buf,
                local_addr,
                endpoint,
            ) {
                Ok(_) => {},
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    trace!("{}: recv() would block", local_addr);
                    break 'read;
                },
                Err(e) => unimplemented!("{:?}", e)
            }
        }
        Ok(())
    }

    /// Returns the registry for registering mio events.
    pub fn registry(&mut self) -> &mut Registry<TExternalEventValue> {
        &mut self.registry
    }

    /// Set a timeout for the application logic.
    /// Must be set again after every iteration, ideally in the `post_handle_recvs` callback.
    pub fn set_app_timeout(&mut self, timeout: Duration) {
        self.app_timeout = Some(timeout);
    }

    /// Mark the runner as closed.
    /// The runner will stop at the next opportunity.
    /// This does not cause QUIC connections to send CONNECTION_CLOSE.
    pub fn close(&mut self) {
        self.marked_closed = true;
    }
}

pub struct Registry<TExternalEventValue> {
    events: Slab<Event<TExternalEventValue>>,
    poll: mio::Poll,
    owned_sources: Vec<Box<dyn mio::event::Source + Send>>,
}

impl <TExternalEventValue> Registry<TExternalEventValue> {
    /// Register a close signal source. When the source becomes readable, the runner stops.
    pub fn register_close<S>(&mut self, source: &mut S)
    where S: mio::event::Source + ?Sized
    {
        let token = self.events.insert(Event::Close);
        self.poll.registry().register(source, mio::Token(token), mio::Interest::READABLE).unwrap();
    }

    /// Same as [`Self::register_close`], but takes ownership of the source.
    pub fn register_close_owned<S>(&mut self, mut source: S)
    where S: mio::event::Source + Send + 'static
    {
        self.register_close(&mut source);
        self.owned_sources.push(Box::new(source));
    }

    /// Register an external event source. When the source becomes readable, `Config::on_external_event` is called with the associated `value`.
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

    /// Same as [`Self::register_external`], but takes ownership of the source.
    pub fn register_external_owned<S>(
        &mut self,
        mut source: S,
        interest: mio::Interest,
        value: TExternalEventValue
    )
    where S: mio::event::Source + Send + 'static
    {
        self.register_external(&mut source, interest, value);
        self.owned_sources.push(Box::new(source));
    }
}

pub enum Event<T> {
    Close,
    Socket(usize),
    External(T)
}
