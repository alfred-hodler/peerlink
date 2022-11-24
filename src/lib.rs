//! Peerlink is a low-level network client for Bitcoin. It is designed to abstract away and hide
//! networking logic and allow the consumer to focus on communicating with other peers on the
//! network. Message streaming, buffering and basic peer management is handled by the library.
//!
//! The way to use this crate is to create a `Reactor` instance, acquire its messaging handle
//! and let the reactor run on its own thread. The API consumer then communicates with the reactor
//! using a `Handle` instance.
//!
//! The following example covers connecting to a Bitcoin node running on localhost, sending
//! a message and then shutting down the reactor.
//!
//! ```no_run
//! # use bitcoin::network::message::RawNetworkMessage;
//! # fn main() -> std::io::Result<()> {
//! // passing an empty vector means we aren't listening for connections on any interfaces
//! let (reactor, handle) = peerlink::Reactor::new(vec![])?;
//!
//! // start the reactor (spawns its own thread)
//! let reactor_join_handle = reactor.run();
//!
//! // issue a command to connect to our peer
//! handle.send(peerlink::Command::Connect("127.0.0.1:8333".parse().unwrap()))?;
//!
//! // expect a `ConnectedTo` event if everything went well
//! let peer = match handle.receive()? {
//!     peerlink::Event::ConnectedTo(Ok((peer, peer_addr))) => {
//!         println!("Connected to peer {} at address {}", peer, peer_addr);
//!         peer
//!     }
//!     _ => panic!("not interested in other messages yet"),
//! };
//!
//! // send a message to the peer and perform other operations...
//! handle.send(peerlink::Command::Message(peer, RawNetworkMessage {magic: todo!(), payload: todo!()}))?;
//!
//! // When done, shut down the reactor and join with its thread
//! handle.send(peerlink::Command::Shutdown)?;
//! let _ = reactor_join_handle.join();
//!
//! # Ok(())
//! # }
//! ```

mod message_stream;
mod peer;
pub mod reactor;

pub use bitcoin;
pub use mio::net::TcpStream;
pub use peer::PeerId;
pub use reactor::{Command, Connector, Event, Reactor};
