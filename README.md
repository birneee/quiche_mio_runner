# Quiche mio Runner

> ⚠️ Not production ready

> This project is not affiliated with [quiche](https://github.com/cloudflare/quiche) or [mio](https://github.com/tokio-rs/mio)

**Quiche mio runner** provides an event-driven I/O runtime for [quiche_endpoint](https://github.com/birneee/quiche_endpoint),  
built using the [mio](https://github.com/tokio-rs/mio) crate.

It handles socket events and timeout scheduling automatically, allowing `quiche_endpoint` to focus purely on QUIC state management.

## Features
- Batched send and receive
- GSO and GRO
- I/O using epoll

## Build
```shell
cargo build
```
