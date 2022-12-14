//! # Peerlink
//!
//! Peerlink is a low-level network client for Bitcoin. It is designed to abstract away and hide
//! networking logic and allow the consumer to focus on communicating with other peers on the
//! network. Message streaming, buffering and basic peer management is handled by the library.
//!
//! The way to use this crate is to create a `Reactor` instance, acquire its messaging handle
//! and let the reactor run on its own thread. The API consumer then communicates with the reactor
//! using a `Handle` instance.
//!
//! # User Agent Handling
//!
//! While Peerlink does not enforce any particular user agent string format when sending out
//! `version` messages, please use the `user_agent` function in the crate root to create a well
//! formatted BIP0014 UA string. Doing so ensures that Peerlink is identifying itself properly to
//! the network.
//!
//! # Example
//!
//! The following example covers connecting to a Bitcoin node running on localhost, sending
//! a message and then shutting down the reactor.
//!
//! ```no_run
//! # use bitcoin::network::message::RawNetworkMessage;
//! # fn main() -> std::io::Result<()> {
//! let config = peerlink::Config {
//!     // empty vec means we aren't listening for inbound connections
//!     bind_addr: vec![],
//!     ..Default::default()
//! };
//!
//! let (reactor, handle) = peerlink::Reactor::new(config)?;
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
pub use message_stream::StreamConfig;
pub use mio::net::TcpStream;
pub use peer::PeerId;
pub use reactor::{Command, Connector, Event, Reactor};

/// Configuration parameters for the reactor.
#[derive(Debug, Default)]
pub struct Config {
    /// The list of socket addresses where the reactor listens for inbound connections.
    pub bind_addr: Vec<std::net::SocketAddr>,
    /// Configuration parameters for individual peer connections. This allows the fine tuning of
    /// internal buffer sizes etc. Most consumers won't have to modify this.
    pub stream_config: StreamConfig,
}

/// Creates a well formed BIP0014 user agent string that should be used with `version` messages.
/// For instance, if `app_name` is `supernode` and `app_version` is `0.6.5` and the version of
/// Peerlink being used is `0.4.0`, the resulting string will be `/peerlink:0.4.0/supernode:0.6.5/`.
pub fn user_agent(app_name: &str, app_version: &str) -> String {
    format!(
        "/peerlink:{}/{app_name}:{app_version}/",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod test {
    #[test]
    fn ua_format() {
        let ua = super::user_agent("supernode", "0.6.5");
        assert_eq!(
            ua,
            format!("/peerlink:{}/supernode:0.6.5/", env!("CARGO_PKG_VERSION"))
        );
    }
}
