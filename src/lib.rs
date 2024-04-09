//! # Peer-to-peer networking reactor
//!
//! Peerlink is a low-level building block for P2P applications. It uses a nonblocking reactor to
//! accept inbound connections, make outbound connections, do message streaming and reassembly,
//! track peers and perform other low-level operations. It entirely abstracts away menial
//! networking plumbing such as managing TCP sockets and reading bytes off the wire. In other
//! words, it provides the consumer with a simple interface to talking with other nodes in a P2P
//! network.
//!
//! See the included example for usage.

pub mod connector;
mod message_stream;
pub mod reactor;

pub use message_stream::StreamConfig;
pub use mio::net::TcpStream;
pub use reactor::{Command, Event, Handle, Reactor};

#[cfg(not(feature = "async"))]
pub use crossbeam_channel;

#[cfg(feature = "async")]
pub use async_channel;

/// Configuration parameters for the reactor.
#[derive(Debug, Default)]
pub struct Config {
    /// The list of socket addresses where the reactor listens for inbound connections.
    pub bind_addr: Vec<std::net::SocketAddr>,
    /// Configuration parameters for individual peer connections. This allows the fine tuning of
    /// internal buffer sizes etc. Most consumers won't have to modify the default values.
    pub stream_config: StreamConfig,
}

/// A trait that network messages processed by the reactor must implement.
pub trait Message: std::fmt::Debug + Sized + Send + Sync + 'static {
    /// Encodes a message into a writer. This is an in-memory writer that never panics so there is
    /// no need to handle the error path.
    ///
    /// Returns the number of encoded bytes.
    fn encode(&self, dest: &mut impl std::io::Write) -> usize;

    /// Provides access to the underlying read buffer. The buffer may contain any number of
    /// messages, including no messages at all or only a partial message. If there are enough bytes
    /// available to decode a message, the function must return an `Ok` with the decoded message
    /// and the number of bytes it consumed.
    ///
    /// If there is not enough data to decode a message (i.e. it is available only partially),
    /// `Err(DecodeError::NotEnoughData)` must be returned. That signals that the read should be
    /// retried. If the message cannot be decoded at all, or exceeds size limits or otherwise
    /// represents junk data, `Err(DecodeError::MalformedMessage)` must be returned. Such peers are
    /// disconnected as protocol violators.
    fn decode(buffer: &[u8]) -> Result<(Self, usize), DecodeError>;
}

/// Possible reasons why a message could not be decoded at a particular time.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// There is not enough data available to reconstruct a message. This does not indicate an
    /// irrecoverable problem, it just means that not enough data has been taken of the wire yet
    /// and that the operation should be retried once more data comes in.
    NotEnoughData,
    /// The message is malformed in some way. Once this is encountered, the peer that sent it
    /// is disconnected.
    MalformedMessage,
}

/// Unique peer identifier. These are unique for the lifetime of the process and strictly
/// incrementing for each new connection. Even if the same peer (in terms of socket address)
/// connects multiple times, a new `PeerId` instance will be issued for each connection.
#[derive(Debug, Clone, Hash, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PeerId(pub u64);

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.0))
    }
}
