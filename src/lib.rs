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

mod message_stream;
mod reactor;

pub mod connector;

use std::{io, net::SocketAddr, num::NonZeroUsize};

use crate::connector::Target;

pub use message_stream::StreamConfig;
pub use reactor::{Handle, run, run_with_connector};

#[cfg(feature = "socks")]
pub use reactor::run_with_socks5_proxy;

#[cfg(not(feature = "async"))]
pub use crossbeam_channel;

#[cfg(feature = "async")]
pub use async_channel;

/// Configuration parameters for the reactor.
#[derive(Debug, Clone)]
pub struct Config {
    /// The list of socket addresses where the reactor listens for inbound connections.
    pub bind_addr: Vec<std::net::SocketAddr>,

    /// Configuration parameters for individual peer connections. This allows the fine tuning of
    /// internal buffer sizes etc.
    pub stream_config: StreamConfig,

    /// The size of the shared receive buffer, i.e. the max number of bytes that can be read in one
    /// receive operation. Setting this too low can cause many reads to happen, whereas too high a
    /// figure will use up more memory and open up your application to DoS attacks. The default is
    /// 1 MB.
    ///
    /// This figure is capped by [`Message::MAX_SIZE`] since there is no need to ever take in more
    /// data in one read than the biggest message requires to decode.
    pub receive_buffer_size: usize,

    /// Whether the reactor should perform backpressure control on the receive side. Setting this
    /// to `Some(n)` means that the reactor will start blocking on sending events to the consumer
    /// when the receive channel of size `n` is full and events are not being read. Setting it to
    /// `None` means that the capacity of the event channel is unbounded and the reactor will send
    /// events to the consumer as fast as it can, regardless of whether those events are being read
    /// (at all). The default is no backpressure control (`None`).
    pub receive_channel_size: Option<NonZeroUsize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: Default::default(),
            stream_config: Default::default(),
            receive_buffer_size: 1024 * 1024,
            receive_channel_size: None,
        }
    }
}

/// A trait that network messages processed by the reactor must implement.
pub trait Message: std::fmt::Debug + Sized + Send + Sync + 'static {
    /// The size of the largest expected message. It is important to set this correctly because an
    /// incorrect value will interfere with inbound backpressure control and the ability to decode
    /// large messages. This is also crucial for DoS protection (resource exhaustion attacks).
    const MAX_SIZE: usize;

    /// Encodes a message into a writer. This is an in-memory sink that never panics so there is no
    /// need to handle the error path.
    ///
    /// Returns the number of encoded bytes.
    fn encode(&self, sink: &mut impl std::io::Write) -> usize;

    /// Provides access to the underlying read buffer. The buffer may contain any number of
    /// messages, including no messages at all or only a partial message. If there are enough bytes
    /// available to decode a message, the function must return an `Ok` with the decoded message and
    /// the number of bytes it consumed.
    ///
    /// If there is not enough data to decode a message (i.e. it is available only partially),
    /// `Err(DecodeError::NotEnoughData)` must be returned. That signals that decoding should be
    /// retried when more data comes in. If the message cannot be decoded at all, or exceeds size
    /// limits or otherwise represents junk data, `Err(DecodeError::MalformedMessage)` must be
    /// returned. Such peers are disconnected as protocol violators.
    fn decode(buffer: &[u8]) -> Result<(Self, usize), DecodeError>;

    /// If a message has a known size ahead of encoding, that value can be set here. This is useful
    /// for outbound backpressure control, so that a message is not preemptively encoded and placed
    /// into the send buffer only to be realized that the size of the send buffer will be exceeding
    /// its maximum. Getting this wrong can interfere with outbound backpressure control, so if the
    /// value is not certain, it is better not to override the method.
    fn size_hint(&self) -> Option<usize> {
        None
    }
}

/// Possible reasons why a message could not be decoded at a particular time.
#[derive(Debug)]
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
pub struct PeerId(u64);

impl PeerId {
    /// DANGER: allows the user to set peer ids directly. Normally these are assigned by the
    /// reactor and the consumer of the library should not be creating them manually. For
    /// development/testing/debugging purposes only. Use only if you really know what you
    /// are doing.
    pub fn set_raw(value: u64) -> Self {
        Self(value)
    }

    /// Returns the next id in sequence (self + 1).
    pub fn next(&self) -> Self {
        Self(self.0 + 1)
    }

    /// Returns the inner id of the peer id.
    pub fn inner(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Command variants for the reactor to process.
#[derive(Debug)]
pub enum Command<M: Message> {
    /// Connect to a remote host.
    Connect(Target),
    /// Disconnect from a peer.
    Disconnect(PeerId),
    /// Send a message to a peer.
    Message(PeerId, M),
}

impl<M: Message> Command<M> {
    /// Convenience function that converts a compatible argument into a connect [`Target`].
    /// Works on types such as:
    ///   - [`SocketAddr`](std::net::SocketAddr)
    ///   - [`SocketAddrV4`](std::net::SocketAddrV4)
    ///   - [`SocketAddrV6`](std::net::SocketAddrV6)
    ///   - [`(Ipv4Addr, u16)`](std::net::Ipv4Addr) -- (address, port)
    ///   - [`(Ipv6Addr, u16)`](std::net::Ipv4Addr) -- (address, port)
    ///   - [`(String, u16)`] -- (domain, port)
    ///   - [`(&str, u16)`] -- (domain, port)
    pub fn connect(target: impl Into<Target>) -> Self {
        Self::Connect(target.into())
    }
}

// Event variants produced by the reactor.
#[derive(Debug)]
pub enum Event<M: Message> {
    /// The reactor attempted to connect to a remote peer.
    ConnectedTo {
        /// The remote host that was connected to. This is in the same format it was specified.
        target: Target,
        /// The result of the connection attempt. A peer id is returned if successful.
        result: io::Result<PeerId>,
    },

    /// The reactor received a connection from a remote peer.
    ConnectedFrom {
        /// The peer associated with the event.
        peer: PeerId,
        /// The address of the remote peer.
        addr: SocketAddr,
        /// The address of the local interface that accepted the connection.
        interface: SocketAddr,
    },

    /// A peer disconnected.
    Disconnected {
        /// The peer associated with the event.
        peer: PeerId,
        /// The reason the peer left.
        reason: DisconnectReason,
    },

    /// A peer produced a message.
    Message {
        /// The peer associated with the event.
        peer: PeerId,
        /// The message received from the peer.
        message: M,
        /// The original wire size of the message before it was decoded.
        size: usize,
    },

    /// No peer exists with the specified id. Sent when an operation was specified using a peer id
    /// that is not present in the reactor.
    NoPeer(PeerId),

    /// The send buffer associated with the peer has less space available than the queued message.
    QueueRejected {
        /// The peer associated with the event.
        peer: PeerId,
        /// The message that could not be queued.
        message: M,
    },

    /// Some data has left the send buffer for a peer. This is only emitted if config enabled.
    Transmitted {
        /// The peer associated with the event.
        peer: PeerId,
        /// The number of bytes that can be queued without triggering a rejection.
        available: usize,
    },
}

/// Explains why a client connection was disconnected.
#[derive(Debug)]
pub enum DisconnectReason {
    /// The reactor was asked to perform a disconnect.
    Requested,
    /// The peer left and the end of stream was reached.
    Left,
    /// The peer violated the protocol in some way, usually by sending a malformed message.
    CodecViolation,
    /// The write side is stale, i.e. the peer is not reading the data we are sending.
    WriteStale,
    /// An IO error occurred.
    Error(io::Error),
}
