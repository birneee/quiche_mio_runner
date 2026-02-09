use crate::{Config, Runner, Socket};
use quiche_endpoint::test_utils::{default_client_config, default_server_config, key_pair};
use quiche_endpoint::{Endpoint, EndpointConfig, quiche};
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::thread;
use log::info;

trait App {
    fn handle(&mut self, quic_conn: &mut quiche::Connection);
    fn is_done(&self) -> bool;
}

struct ConsumeApp {
    buf: Box<[u8; 100_000]>,
    stream_data_received: usize,
    closed_conns: usize,
    expected_stream_data: usize,
}

impl ConsumeApp {
    fn new(expected_data: usize) -> Self {
        Self {
            buf: Box::new([0u8; _]),
            stream_data_received: 0,
            closed_conns: 0,
            expected_stream_data: expected_data,
        }
    }
}

impl App for ConsumeApp {
    fn handle(&mut self, quic_conn: &mut quiche::Connection) {
        for sid in quic_conn.readable() {
            'recv: loop {
                match quic_conn.stream_recv(sid, self.buf.as_mut()) {
                    Ok((n, fin)) => {
                        self.stream_data_received += n;
                        if fin {
                            quic_conn.close(false, 0, b"").unwrap();
                            self.closed_conns += 1;
                        }
                        info!("recv {} {}", quic_conn.trace_id(), self.stream_data_received)
                    }
                    Err(quiche::Error::Done) => {
                        break 'recv;
                    }
                    Err(e) => unimplemented!("{:?}", e),
                };
            }
        }
    }

    fn is_done(&self) -> bool {
        self.expected_stream_data == self.stream_data_received
    }
}

struct ProduceApp {
    buf: Box<[u8; 100_000]>,
    conn_stream_bytes_sent: HashMap<String, usize>,
    stream_id: u64,
    msg_size: usize,
    expected_num_conns: usize,
    open_conns: HashSet<String>,
}

impl ProduceApp {
    fn new(msg_size: usize, expected_num_conns: usize, stream_id: u64) -> Self {
        Self {
            buf: Box::new([0u8; _]),
            conn_stream_bytes_sent: HashMap::new(),
            stream_id,
            msg_size,
            expected_num_conns,
            open_conns: HashSet::new(),
        }
    }

    fn total_bytes_sent(&self) -> usize {
        self.conn_stream_bytes_sent.values().sum::<usize>()
    }
}

impl App for ProduceApp {
    fn handle(&mut self, quic_conn: &mut quiche::Connection) {
        if quic_conn.is_closed() {
            self.open_conns.remove(quic_conn.trace_id());
        } else {
            self.open_conns.insert(quic_conn.trace_id().to_string());
        }
        let conn_stream_bytes_sent = match self.conn_stream_bytes_sent.get_mut(quic_conn.trace_id()) {
            Some(v) => v,
            None => {
                self.conn_stream_bytes_sent.insert(quic_conn.trace_id().to_string(), 0);
                self.conn_stream_bytes_sent.get_mut(quic_conn.trace_id()).unwrap()
            }
        };
        'send: loop {
            let remaining = self.msg_size - *conn_stream_bytes_sent;
            let len = min(remaining, self.buf.len());
            let fin = remaining == len;
            if len == 0 {
                break 'send;
            }
            match quic_conn.stream_send(self.stream_id, &self.buf[..len], fin) {
                Ok(n) => {
                    *conn_stream_bytes_sent += n;
                    info!("send {} {}", quic_conn.trace_id(), *conn_stream_bytes_sent);
                }
                Err(quiche::Error::Done) => {
                    break 'send;
                }
                Err(e) => unimplemented!("{:?}", e),
            };
        }
    }

    fn is_done(&self) -> bool {
        self.total_bytes_sent() == self.msg_size * self.expected_num_conns && self.open_conns.is_empty()
    }
}

#[test]
fn single_runner_for_client_server() {
    let _ = env_logger::try_init();
    const MSG_SIZE: usize = 100_000_000;
    struct AppData {
        server_app: ConsumeApp,
        client_app: ProduceApp,
    }
    let (key, cert) = key_pair();
    let mut r = Runner::<(), AppData, ()>::new(
        {
            let mut c: Config<(), AppData, ()> = Config::default();
            c.post_handle_recvs = |r| {
                let e = &mut r.endpoint;
                for i in e.conn_index_iter() {
                    let (Some(c), app_data) = e.conn_with_app_data_mut(i) else {
                        continue;
                    };
                    let quic_conn = &mut c.conn;
                    if !quic_conn.is_established() {
                        continue;
                    };
                    if quic_conn.is_server() {
                        // server
                        app_data.server_app.handle(quic_conn);
                    } else {
                        // client
                        app_data.client_app.handle(quic_conn);
                    }
                }
                if e.app_data().client_app.is_done() && e.app_data().server_app.is_done() {
                    r.close()
                }
            };
            c
        },
        Endpoint::new(
            Some({
                let c = default_server_config(key, cert);
                c
            }),
            EndpointConfig::default(),
            AppData {
                client_app: ProduceApp::new(MSG_SIZE, 1, 0),
                server_app: ConsumeApp::new(MSG_SIZE),
            },
        ),
        None,
    );
    let socket = crate::Socket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr;
    r.register_socket(socket);
    r.endpoint.connect(
        None,
        addr,
        addr,
        &mut {
            let mut c = default_client_config(cert);
            c
        },
        (),
        None,
        None,
    );
    r.run();
    let a = r.endpoint.app_data();
    assert_eq!(a.client_app.total_bytes_sent(), MSG_SIZE);
    assert_eq!(a.server_app.stream_data_received, MSG_SIZE);
}

#[test]
fn stream_two_runners() {
    let _ = env_logger::try_init();
    const MSG_SIZE: usize = 100_000_000;
    let (key, cert) = key_pair();
    let mut sr = Runner::<(), ConsumeApp, ()>::new(
        {
            let mut c = Config::<(), ConsumeApp, ()>::default();
            c.post_handle_recvs = |r| {
                let e = &mut r.endpoint;
                for i in e.conn_index_iter() {
                    let (Some(c), app_data) = e.conn_with_app_data_mut(i) else {
                        continue;
                    };
                    let quic_conn = &mut c.conn;
                    if !quic_conn.is_established() {
                        continue;
                    };
                    app_data.handle(quic_conn);
                }
                if e.app_data().is_done() {
                    r.close();
                }
            };
            c
        },
        Endpoint::new(
            Some(default_server_config(key, cert)),
            EndpointConfig::default(),
            ConsumeApp::new(MSG_SIZE),
        ),
        None,
    );
    sr.register_socket(Socket::bind("127.0.0.1:0").unwrap());
    let mut cr = {
        let socket = Socket::bind("127.0.0.1:0").unwrap();
        let mut r = Runner::<(), ProduceApp, ()>::new(
            {
                let mut c = Config::<(), ProduceApp, ()>::default();
                c.post_handle_recvs = |r| {
                    let e = &mut r.endpoint;
                    for i in e.conn_index_iter() {
                        let (Some(c), app_data) = e.conn_with_app_data_mut(i) else {
                            continue;
                        };
                        let quic_conn = &mut c.conn;
                        if !quic_conn.is_established() {
                            continue;
                        };
                        app_data.handle(quic_conn);
                    }
                };
                c
            },
            {
                let mut e =
                    Endpoint::new(None, EndpointConfig::default(), ProduceApp::new(MSG_SIZE, 1, 0));
                e.connect(
                    None,
                    socket.local_addr,
                    sr.sockets.sockets[0].local_addr,
                    &mut default_client_config(cert),
                    (),
                    None,
                    None,
                );
                e
            },
            None,
        );
        r.register_socket(socket);
        r
    };

    let sh = thread::spawn(move || sr.run());
    let ch = thread::spawn(move || cr.run());
    ch.join().unwrap();
    sh.join().unwrap();
}

#[test]
fn parallel_upload_on_separate_runners() {
    let _ = env_logger::try_init();
    const NUM_CLIENTS: usize = 3;
    const MSG_SIZE: usize = 100_000_000;
    let (key, cert) = key_pair();
    let mut sr = Runner::<(), ConsumeApp, ()>::new(
        {
            let mut c = Config::<(), ConsumeApp, ()>::default();
            c.post_handle_recvs = |r| {
                let e = &mut r.endpoint;
                for i in e.conn_index_iter() {
                    let (Some(c), app_data) = e.conn_with_app_data_mut(i) else {
                        continue;
                    };
                    let quic_conn = &mut c.conn;
                    if !quic_conn.is_established() {
                        continue;
                    };
                    app_data.handle(quic_conn);
                }
                if e.app_data().is_done() {
                    r.close();
                }
            };
            c
        },
        Endpoint::new(
            Some(default_server_config(key, cert)),
            EndpointConfig::default(),
            ConsumeApp::new(MSG_SIZE*NUM_CLIENTS),
        ),
        None,
    );
    sr.register_socket(Socket::bind("127.0.0.1:0").unwrap());
    let crs: Vec<Runner<(), ProduceApp, ()>> = (0..NUM_CLIENTS)
        .map(|_| {
            let socket = Socket::bind("127.0.0.1:0").unwrap();
            let mut r = Runner::<(), ProduceApp, ()>::new(
                {
                    let mut c = Config::<(), ProduceApp, ()>::default();
                    c.post_handle_recvs = |r| {
                        let e = &mut r.endpoint;
                        for i in e.conn_index_iter() {
                            let (Some(c), app_data) = e.conn_with_app_data_mut(i) else {
                                continue;
                            };
                            let quic_conn = &mut c.conn;
                            if !quic_conn.is_established() {
                                continue;
                            };
                            app_data.handle(quic_conn);
                        }
                    };
                    c
                },
                {
                    let mut e =
                        Endpoint::new(None, EndpointConfig::default(), ProduceApp::new(MSG_SIZE, NUM_CLIENTS, 0));
                    e.connect(
                        None,
                        socket.local_addr,
                        sr.sockets.sockets[0].local_addr,
                        &mut default_client_config(cert),
                        (),
                        None,
                        None,
                    );
                    e
                },
                None,
            );
            r.register_socket(socket);
            r
        })
        .collect();

    let sh = thread::spawn(move || sr.run());
    let chs = crs
        .into_iter()
        .map(|mut cr| thread::spawn(move || cr.run()));

    for ch in chs {
        ch.join().unwrap();
    }
    sh.join().unwrap();
}

#[test]
fn parallel_download_on_separate_runners() {
    let _ = env_logger::try_init();
    const NUM_CLIENTS: usize = 3;
    const MSG_SIZE: usize = 100_000_000;
    let (key, cert) = key_pair();
    let mut sr = Runner::<(), ProduceApp, ()>::new(
        {
            let mut c = Config::<(), ProduceApp, ()>::default();
            c.post_handle_recvs = |r| {
                let e = &mut r.endpoint;
                for i in e.conn_index_iter() {
                    let (Some(c), app_data) = e.conn_with_app_data_mut(i) else {
                        continue;
                    };
                    let quic_conn = &mut c.conn;
                    if !quic_conn.is_established() {
                        continue;
                    };
                    app_data.handle(quic_conn);
                }
                if e.app_data().is_done() {
                    r.close();
                }
            };
            c
        },
        Endpoint::new(
            Some(default_server_config(key, cert)),
            EndpointConfig::default(),
            ProduceApp::new(MSG_SIZE, NUM_CLIENTS, 1),
        ),
        None,
    );
    sr.register_socket(Socket::bind("127.0.0.1:0").unwrap());
    let crs: Vec<Runner<(), ConsumeApp, ()>> = (0..NUM_CLIENTS)
        .map(|_| {
            let socket = Socket::bind("127.0.0.1:0").unwrap();
            let mut r = Runner::<(), ConsumeApp, ()>::new(
                {
                    let mut c = Config::<(), ConsumeApp, ()>::default();
                    c.post_handle_recvs = |r| {
                        let e = &mut r.endpoint;
                        for i in e.conn_index_iter() {
                            let (Some(c), app_data) = e.conn_with_app_data_mut(i) else {
                                continue;
                            };
                            let quic_conn = &mut c.conn;
                            if !quic_conn.is_established() {
                                continue;
                            };
                            app_data.handle(quic_conn);
                            if c.conn.is_closed() {
                                assert!(app_data.is_done());
                            }
                        }
                    };
                    c
                },
                {
                    let mut e =
                        Endpoint::new(None, EndpointConfig::default(), ConsumeApp::new(MSG_SIZE));
                    e.connect(
                        None,
                        socket.local_addr,
                        sr.sockets.sockets[0].local_addr,
                        &mut default_client_config(cert),
                        (),
                        None,
                        None,
                    );
                    e
                },
                None,
            );
            r.register_socket(socket);
            r
        })
        .collect();

    let sh = thread::spawn(move || sr.run());
    let chs = crs
        .into_iter()
        .map(|mut cr| thread::spawn(move || cr.run()));

    for ch in chs {
        ch.join().unwrap();
    }
    sh.join().unwrap();
}