use std::cmp::min;
use std::io::Write;
use std::thread;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use log::info;
use pprof::criterion::{Output, PProfProfiler};
use quiche_endpoint::{quiche, Endpoint, EndpointConfig};
use quiche_endpoint::test_utils::{default_client_config, default_server_config, key_pair};
use quiche_mio_runner::{Config, Runner, Socket};

criterion_main!(benches);

criterion_group!(
    name = benches;
    config = Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)));
    targets = bench_throughput
);

fn throughput(msg_size: usize, disable_gso: bool, disable_gro: bool) {
    let (mut close_pipe_tx, mut close_pipe_rx) = mio::unix::pipe::new().unwrap();
    let (key, cert) = key_pair();
    // server
    let mut sr = {
        struct AppData {
            msg_size: usize,
            stream_data_received: usize,
            buf: [u8; 100_000],
        }
        let mut r = Runner::<(), AppData, ()>::new(
            {
                let mut c = Config::<(), AppData, ()>::default();
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
                        for sid in quic_conn.readable() {
                            'recv: loop {
                                match quic_conn.stream_recv(sid, app_data.buf.as_mut()) {
                                    Ok((n, fin)) => {
                                        app_data.stream_data_received += n;
                                        if fin {
                                            quic_conn.close(false, 0, b"").unwrap();
                                        }
                                        info!("recv {} {}", quic_conn.trace_id(), app_data.stream_data_received)
                                    }
                                    Err(quiche::Error::Done) => {
                                        break 'recv;
                                    }
                                    Err(e) => unimplemented!("{:?}", e),
                                };
                            }
                        }
                        if app_data.msg_size == app_data.stream_data_received {
                            r.close();
                            return
                        }
                    }
                };
                c
            },
            Endpoint::new(
                Some(default_server_config(key, cert)),
                EndpointConfig::default(),
                AppData {
                    msg_size,
                    stream_data_received: 0,
                    buf: [0u8; _],
                },
            ),
            None,
        );
        r.register_socket(Socket::bind("127.0.0.1:0".parse().unwrap(), disable_gro, false, disable_gso).unwrap());
        r
    };
    //client
    let mut cr = {
        struct AppData {
            msg_size: usize,
            stream_data_sent: usize,
            buf: [u8; 100_000],
        }
        let socket = Socket::bind("127.0.0.1:0".parse().unwrap(), disable_gro, false, disable_gso).unwrap();
        let mut r = Runner::<(), AppData, ()>::new(
            {
                let mut c = Config::<(), AppData, ()>::default();
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
                        'send: loop {
                            let remaining = app_data.msg_size - app_data.stream_data_sent;
                            let len = min(remaining, app_data.buf.len());
                            let fin = remaining == len;
                            if len == 0 {
                                break 'send;
                            }
                            match quic_conn.stream_send(0, &app_data.buf[..len], fin) {
                                Ok(n) => {
                                    app_data.stream_data_sent += n;
                                    info!("send {} {}", quic_conn.trace_id(), app_data.stream_data_sent);
                                }
                                Err(quiche::Error::Done) => {
                                    break 'send;
                                }
                                Err(e) => unimplemented!("{:?}", e),
                            };
                        }
                    }
                };
                c
            },
            {
                let mut e =
                    Endpoint::new(None, EndpointConfig::default(), AppData {
                        msg_size,
                        stream_data_sent: 0,
                        buf: [0u8; _],
                    });
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
            Some(&mut close_pipe_rx),
        );
        r.register_socket(socket);
        r
    };

    let sh = thread::spawn(move || sr.run());
    let ch = thread::spawn(move || cr.run());
    sh.join().unwrap(); // everything is received
    _ = close_pipe_tx.write(&[0]).unwrap();
    ch.join().unwrap();
}

fn bench_throughput(c: &mut Criterion) {
    let mut g = c.benchmark_group("throughput");
    const TARGET: usize = 1E9 as usize;
    g.throughput(Throughput::Bytes(TARGET as u64));
    g.sample_size(10);
    g.bench_function("1GB", |b| {
        b.iter(|| {
            throughput(1E9 as usize, true, true)
        });
    });
    g.bench_function("1GB-gro-gso", |b| {
        b.iter(|| {
            throughput(1E9 as usize, false, false)
        });
    });
}
