# Peerlink

[![Documentation](https://img.shields.io/docsrs/peerlink)](https://docs.rs/peerlink/latest/peerlink/)
[![Crates.io](https://img.shields.io/crates/v/peerlink.svg)](https://crates.io/crates/peerlink)
[![License](https://img.shields.io/crates/l/peerlink.svg)](https://github.com/alfred-hodler/peerlink/blob/master/LICENSE)
[![Test Status](https://github.com/alfred-hodler/peerlink/actions/workflows/rust.yml/badge.svg)](https://github.com/alfred-hodler/peerlink/actions)

**What is Peerlink?** Peerlink is a low-level building block for P2P applications. It uses a nonblocking reactor to accept inbound connections, make outbound connections, do message streaming and reassembly, track peers and perform other low-level operations. It entirely abstracts away menial networking plumbing such as managing TCP sockets and reading bytes off the wire. In other words, it provides the consumer with a simple interface to talking with other nodes in a P2P network.

**How is this meant to be used?** Mainly as a building block for a variety of applications that communicate in a P2P fashion. For instance, if writing a Bitcoin node or BitTorrent client implementation, Peerlink can handle and abstract away the networking aspect and provide the developer with simplified messaging capabilities and enable them to focus on the business logic of their application. There is a single trait called `Message` that needs to be implemented on whatever message type you plan on sending and receiving and Peerlink does the heavy lifting.

**How is this different from competitors?** The philosophy of this crate is extreme simplicity. Rather than make assumptions about NAT traversal, peer discovery, encryption and other application-level aspects of decentralized networking, Peerlink provides the developer with a simple API to connecting to other peers, handling incoming peers and sending and receiving arbitrary messages.

## Features

- Simple usage (see [examples](examples)).
- First class support for **proxying** (Socks5, Tor...)
- **Efficient:** low latency operations based on nonblocking IO.
- **Safe:** written in Rust.

## Proxying

To enable socks5 proxying, build with the `socks` feature enabled.

## Usage

Please refer to the documentation or the supplied [examples](examples) and [integration tests](tests).

## Disclaimer

This project comes with no warranty whatsoever. Please refer to the license for details.
