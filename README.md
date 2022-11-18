# Peerlink

**What is Peerlink?** Peerlink is a low-level network client for the Bitcoin P2P network written in Rust. It uses a nonblocking reactor to accept inbound connections, make outbound connections, perform message streaming and reassembly, track peers and perform other low-level network operations.

**What is it not?** It is not a node. It contains no state machine whatsoever except for the networking reactor. While it is capable of handling an almost unlimited number of connections, it does not automatically perform handshakes or pingpongs. It is entirely up to the consumer of the API to make sure that required messages are being sent out and that incoming messages are handled in a desired manner.

**How is this meant to be used?** Mainly as a building block for a variety of applications that want to participate on the Bitcoin network. For instance, if writing a full node or a CBF/Neutrino client, Peerlink can handle and abstract away the networking part and provide the developer with simplified messaging capabilities and enable them to focus on the business logic of their application. A naive example might be that of connecting to the network just to broadcast a one-off transaction and then immediately disconnecting. If that was all that was needed, running a full node would be cumbersome, as would writing all the networking plumbing in-house.

**What is the state of the project?** The project is young but considered to be safe to use. The goal is to relatively quickly bring it to version `1.0`. Breaking API changes might take place until then, although that is not very likely.

**What about async?** Peerlink uses nonblocking IO on a separate thread to perform all network operations. It provides the consumer with a messaging handle that can be used both in sync and async contexts. Async contexts should wrap any blocking reads in whatever manner avoids blocking the event loop. There are no plans to add the `async/await` paradigm to the project since async can trivially wrap sync calls but the reverse does not hold true without introducing a runtime.

## Features

- First class support for **proxying** (Socks5, Tor...)
- **Efficient:** low latency operations based on nonblocking IO.
- **Safe:** written in Rust.

## Proxying

To enable socks5 proxying, build with the `socks` feature enabled.

## Usage

Please refer to the documentation or the supplied [examples](examples) and [integration tests](tests).

## Disclaimer

This project comes with no warranty whatsoever. Please refer to the license for details.
