use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitcoin::network::message::RawNetworkMessage;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Registry, Token, Waker};
use slab::Slab;

use crate::message_stream::{self, DecodeError, MessageStream};
use crate::{Config, PeerId};

/// Token used for waking the reactor event loop.
const WAKE_TOKEN: Token = Token(usize::MAX);

/// Command variants for the reactor to process.
#[derive(Debug)]
pub enum Command {
    /// Connect to a peer at some address.
    Connect(SocketAddr),
    /// Disconnect from a peer.
    Disconnect(PeerId),
    /// Send a message to a peer.
    Message(PeerId, RawNetworkMessage),
    /// Close all connections and shut down the reactor.
    Shutdown,
    /// Causes the event loop to panic. Only available in debug mode for integration testing.
    #[cfg(debug_assertions)]
    Panic,
}

// Event variants produced by the reactor.
#[derive(Debug)]
pub enum Event {
    /// The reactor attempted to connect to a remote peer.
    ConnectedTo(io::Result<(PeerId, SocketAddr)>),
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
        message: RawNetworkMessage,
    },
    /// No peer exists with the specified id. Sent when an operation was specified using a peer id
    /// that is not present in the reactor.
    NoPeer(PeerId),
    /// The send buffer associated with the peer is full. It means the peer is probably not
    /// reading data from the wire in a timely manner.
    SendBufferFull {
        /// The peer associated with the event.
        peer: PeerId,
        /// The message that could not be sent to the peer.
        message: RawNetworkMessage,
    },
}

/// Explains why a client connection was disconnected.
#[derive(Debug)]
pub enum DisconnectReason {
    /// The reactor was asked to perform a disconnect.
    Requested,
    /// The peer left and the end of stream was reached.
    Left,
    /// The peer violated the protocol in some way.
    BadPeer,
    /// The write side is stale, i.e. the peer is not reading the data we are sending.
    WriteStale,
    /// An IO error occurred.
    Error(io::Error),
}

/// Non-blocking network reactor. This always runs in its own thread and communicates with the
/// caller using a handle.
pub struct Reactor<C: Connector + Sync + Send + 'static> {
    poll: Poll,
    config: crate::Config,
    sender: crossbeam_channel::Sender<Event>,
    receiver: crossbeam_channel::Receiver<Command>,
    connector: C,
    _waker: Arc<Waker>,
}

impl Reactor<DefaultConnector> {
    /// Creates a new reactor with the default connector.
    pub fn new(config: Config) -> io::Result<(Self, Handle)> {
        Self::with_connector(config, DefaultConnector)
    }
}

#[cfg(feature = "socks")]
impl Reactor<Socks5Connector> {
    /// Creates a new reactor that connects through a socks5 proxy. Username and password are
    /// required if the proxy requires them.
    ///
    /// Only available under the `socks` feature.
    pub fn with_proxy(
        bind_addr: Vec<SocketAddr>,
        proxy: SocketAddr,
        username: Option<String>,
        password: Option<String>,
    ) -> io::Result<(Self, Handle)> {
        Self::with_connector(
            bind_addr,
            Socks5Connector {
                proxy,
                username,
                password,
            },
        )
    }
}

impl<C: Connector + Sync + Send + 'static> Reactor<C> {
    /// Creates a new reactor with a custom connector.
    pub fn with_connector(config: Config, connector: C) -> io::Result<(Self, Handle)> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN)?);
        let (cmd_sender, cmd_receiver) = crossbeam_channel::unbounded();
        let (event_sender, event_receiver) = crossbeam_channel::unbounded();

        let command_sender = Handle {
            sender: cmd_sender,
            receiver: event_receiver,
            waker: waker.clone(),
        };

        let reactor = Self {
            poll,
            config,
            sender: event_sender,
            receiver: cmd_receiver,
            connector,
            _waker: waker,
        };

        Ok((reactor, command_sender))
    }

    /// Runs the reactor in a newly spawned thread and returns a join handle to that thread.
    pub fn run(self) -> std::thread::JoinHandle<io::Result<()>> {
        std::thread::spawn(|| run(self))
    }
}

/// Used for bidirectional communication with a reactor.
pub struct Handle {
    waker: Arc<Waker>,
    sender: crossbeam_channel::Sender<Command>,
    receiver: crossbeam_channel::Receiver<Event>,
}

impl Handle {
    /// Sends a command to a reactor associated with this handle. If this produces an IO error,
    /// it means the reactor is irrecoverable and should be discarded.
    pub fn send(&self, command: Command) -> io::Result<()> {
        self.sender
            .send(command)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "channel disconnected"))?;

        self.waker.wake()
    }

    /// Blocks until the reactor associated with this handle produces a message. If an IO error is
    /// produced, it means the reactor is irrecoverable and should be discarded.
    pub fn receive(&self) -> io::Result<Event> {
        self.receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "channel disconnected"))
    }

    /// Attempts to receive a message from the reactor associated with this handle without blocking.
    /// If an IO error is produced, it means the reactor is irrecoverable and should be discarded.
    pub fn try_receive(&self) -> Option<io::Result<Event>> {
        match self.receiver.try_recv() {
            Ok(event) => Some(Ok(event)),
            Err(crossbeam_channel::TryRecvError::Empty) => None,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel disconnected",
            ))),
        }
    }

    // Attempts to receive a message from the reactor associated with this handle with a timeout.
    /// If an IO error is produced, it means the reactor is irrecoverable and should be discarded.
    pub fn receive_timeout(&self, duration: Duration) -> Option<io::Result<Event>> {
        match self.receiver.recv_timeout(duration) {
            Ok(event) => Some(Ok(event)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Some(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel disconnected",
            ))),
        }
    }
}

/// Runs the reactor in a loop until an error is produced or a shutdown command is received.
fn run<C: Connector + Sync + Send + 'static>(
    Reactor {
        mut poll,
        config,
        sender,
        receiver,
        mut connector,
        _waker,
    }: Reactor<C>,
) -> io::Result<()> {
    let listeners: Vec<_> = config
        .bind_addr
        .into_iter()
        .enumerate()
        .map(|(offset, addr)| {
            let mut server = TcpListener::bind(addr)?;
            let token = usize::MAX - 1 - offset;

            poll.registry()
                .register(&mut server, Token(token), Interest::READABLE)?;

            log::info!("Server (token {token}): listen at {addr}");

            Ok(server)
        })
        .collect::<std::io::Result<Vec<_>>>()?;

    let mut streams: Slab<MessageStream<TcpStream>> = Slab::with_capacity(16);
    let mut events = Events::with_capacity(1024);
    let mut read_buf = [0; 1024 * 1024];
    let mut last_maintenance = Instant::now();

    loop {
        poll.poll(&mut events, Some(Duration::from_secs(5)))?;

        let has_slot = has_slot(listeners.len(), streams.vacant_key());
        let now = Instant::now();

        'stream: for event in &events {
            match (event.token(), streams.get_mut(event.token().into())) {
                (WAKE_TOKEN, None) => {
                    log::trace!("waker event");

                    for cmd in receiver.try_iter() {
                        log::trace!("command: {:?}", cmd);

                        match cmd {
                            Command::Connect(addr) => {
                                let result = if has_slot {
                                    match connector.connect(addr) {
                                        Ok(stream) => {
                                            let peer = add_stream(
                                                poll.registry(),
                                                &mut streams,
                                                stream,
                                                config.stream_config.clone(),
                                            )?;

                                            log::info!("connected to peer {peer} at {addr}");

                                            Ok(peer)
                                        }
                                        Err(err) => Err(err),
                                    }
                                } else {
                                    Err(io::Error::new(
                                        io::ErrorKind::ConnectionRefused,
                                        "Too many connections are open",
                                    ))
                                };

                                let _ = sender.send(Event::ConnectedTo(result.map(|r| (r, addr))));
                            }

                            Command::Disconnect(peer) => match streams.try_remove(peer.value()) {
                                Some(mut stream) => {
                                    poll.registry().deregister(stream.inner_mut())?;

                                    let _ = write(&mut stream, now);
                                    let _ = stream.inner_mut().shutdown(std::net::Shutdown::Both);

                                    log::info!("peer {peer}: disconnected");

                                    let _ = sender.send(Event::Disconnected {
                                        peer,
                                        reason: DisconnectReason::Requested,
                                    });
                                }
                                None => {
                                    let _ = sender.send(Event::NoPeer(peer));
                                    log::warn!("disconnect: peer {} not found", peer.value());
                                }
                            },

                            Command::Message(peer, message) => {
                                match streams.get_mut(peer.value()) {
                                    Some(stream) => {
                                        if stream.queue_message(&message) {
                                            poll.registry().reregister(
                                                stream.inner_mut(),
                                                peer.into(),
                                                Interest::READABLE | Interest::WRITABLE,
                                            )?;
                                        } else {
                                            let _ = sender
                                                .send(Event::SendBufferFull { peer, message });
                                            log::warn!("send buffer for peer {peer} is full");
                                        }
                                    }

                                    None => {
                                        let _ = sender.send(Event::NoPeer(peer));
                                        log::warn!("message: peer {} not found", peer.value());
                                    }
                                }
                            }

                            Command::Shutdown => {
                                for (id, mut stream) in streams {
                                    let _ = write(&mut stream, now);
                                    let r = stream.inner_mut().shutdown(std::net::Shutdown::Both);
                                    log::debug!("shut down stream {}: {:?}", id, r);
                                }

                                return Ok(());
                            }

                            #[cfg(debug_assertions)]
                            Command::Panic => panic!("panic command received"),
                        }
                    }
                }

                (token, None) if is_listener(listeners.len(), token) && has_slot => {
                    let listener = usize::MAX - 1 - token.0;
                    let interface = listeners[listener].local_addr()?;
                    log::trace!("listener {} (interface {interface})", token.0);

                    loop {
                        match listeners[listener].accept() {
                            Ok((stream, addr)) => {
                                let peer = add_stream(
                                    poll.registry(),
                                    &mut streams,
                                    stream,
                                    config.stream_config.clone(),
                                )?;
                                log::info!("peer {peer}: accepted connection from {addr}");

                                let _ = sender.send(Event::ConnectedFrom {
                                    peer,
                                    addr,
                                    interface,
                                });
                            }
                            Err(err) if would_block(&err) => break,
                            Err(err) => log::warn!("accept error: {}", err),
                        }
                    }
                }

                (token, Some(stream)) => {
                    let peer = token.into();

                    if !stream.is_ready() {
                        log::trace!("peer: {peer}: stream not ready");
                        continue;
                    }

                    if event.is_readable() {
                        log::trace!("peer {peer}: readable");

                        'read: loop {
                            let read_result = stream.read(&mut read_buf);

                            'decode: loop {
                                match stream.receive_message() {
                                    Ok(message) => {
                                        log::debug!("peer {peer}: rx message: {}", message.cmd());

                                        let _ = sender.send(Event::Message { peer, message });
                                    }

                                    Err(
                                        DecodeError::ExceedsSizeLimit
                                        | DecodeError::MalformedMessage,
                                    ) => {
                                        log::info!("peer {peer}: codec violation");

                                        remove_stream(poll.registry(), &mut streams, peer)?;

                                        let _ = sender.send(Event::Disconnected {
                                            peer,
                                            reason: DisconnectReason::BadPeer,
                                        });

                                        break 'stream;
                                    }

                                    Err(DecodeError::NotEnoughData) => break 'decode,
                                }
                            }

                            match read_result {
                                Ok(0) => {
                                    log::debug!("peer {peer}: peer left");

                                    remove_stream(poll.registry(), &mut streams, peer)?;

                                    let _ = sender.send(Event::Disconnected {
                                        peer,
                                        reason: DisconnectReason::Left,
                                    });

                                    break 'stream;
                                }

                                Ok(_) => continue 'read,

                                Err(err) if would_block(&err) => break 'read,

                                Err(err) => {
                                    log::warn!("peer {peer}: IO error: {err}");

                                    remove_stream(poll.registry(), &mut streams, peer)?;

                                    let _ = sender.send(Event::Disconnected {
                                        peer,
                                        reason: DisconnectReason::Error(err),
                                    });

                                    break 'stream;
                                }
                            }
                        }
                    }

                    if event.is_writable() {
                        log::trace!("peer {peer}: writable");

                        match write(stream, now) {
                            Ok(()) => {
                                let interests = choose_interest(stream);
                                poll.registry()
                                    .reregister(stream.inner_mut(), token, interests)?;
                            }

                            Err(err) if would_block(&err) => {}

                            Err(err) => {
                                log::warn!("peer {peer}: IO error: {err}");

                                remove_stream(poll.registry(), &mut streams, peer)?;

                                let _ = sender.send(Event::Disconnected {
                                    peer,
                                    reason: DisconnectReason::Error(err),
                                });
                            }
                        }
                    }
                }

                (_token, stream) => {
                    log::warn!(
                        "spurious event: event={:?}, stream is_some={}",
                        event,
                        stream.is_some()
                    );
                }
            }
        }

        // stale stream removal
        streams.retain(|token, stream| {
            if stream.is_write_stale(now) {
                let peer = PeerId(token);
                log::info!("removing stale peer {peer}");

                poll.registry().deregister(stream.inner_mut()).unwrap();

                let _ = sender.send(Event::Disconnected {
                    peer,
                    reason: DisconnectReason::WriteStale,
                });

                false
            } else {
                true
            }
        });

        // periodic buffer resize
        if (now - last_maintenance).as_secs() > 30 {
            for (_, stream) in &mut streams {
                stream.resize_buffers();
            }

            last_maintenance = now;
        }
    }
}

/// Types implementing this trait can connect to a target address in a custom manner before
/// returning a `mio::net::TcpStream`. This can be used for proxying and other custom scenarios.
/// It is the responsibility of the caller to put the stream into nonblocking mode. Failing
/// to do so will block the reactor indefinitely and render it inoperable.
pub trait Connector {
    /// Connect to a target address and return a `mio` TCP stream.
    fn connect(&mut self, target: SocketAddr) -> io::Result<mio::net::TcpStream>;
}

/// Default `Connector` implementation for `mio` that just connects to a target address.
pub struct DefaultConnector;

impl Connector for DefaultConnector {
    fn connect(&mut self, target: SocketAddr) -> io::Result<mio::net::TcpStream> {
        TcpStream::connect(target)
    }
}

/// Connector that connects through a socks5 proxy.
///
/// The connector tries to put the socket in nonblocking mode and will retry that action several
/// times before giving up. This could lead to small amounts of blocking but usually works out the
/// first time without blocking.
#[cfg(feature = "socks")]
pub struct Socks5Connector {
    /// The socket address of the proxy.
    pub proxy: SocketAddr,
    /// Optional socks username.
    pub username: Option<String>,
    /// Optional socks password.
    pub password: Option<String>,
}

#[cfg(feature = "socks")]
impl Connector for Socks5Connector {
    fn connect(&mut self, target: SocketAddr) -> io::Result<mio::net::TcpStream> {
        let stream = match self.username.as_ref().zip(self.password.as_ref()) {
            Some((username, password)) => {
                socks::Socks5Stream::connect_with_password(self.proxy, target, username, password)?
            }
            None => socks::Socks5Stream::connect(self.proxy, target)?,
        }
        .into_inner();

        let try_cycle_duration = Duration::from_millis(1);
        let try_deadline = Duration::from_millis(5);

        let mut elapsed = Duration::ZERO;
        loop {
            match stream.set_nonblocking(true) {
                Ok(()) => break Ok(mio::net::TcpStream::from_std(stream)),

                Err(err) if would_block(&err) && elapsed < try_deadline => {
                    std::thread::sleep(try_cycle_duration);
                    elapsed += try_cycle_duration;
                }

                Err(err) => break Err(err),
            }
        }
    }
}

/// Causes a stream to write into its underlying stream.
fn write(stream: &mut MessageStream<TcpStream>, now: Instant) -> io::Result<()> {
    if !stream.has_queued_data() {
        return Ok(());
    }

    loop {
        match stream.write(now) {
            Ok(written) => {
                let has_more = stream.has_queued_data();
                log::trace!("wrote out {written} bytes, has more: {}", has_more);

                if !has_more {
                    break Ok(());
                }
            }

            Err(err) if would_block(&err) => {
                log::trace!("write would block");
                break Ok(());
            }

            Err(err) => break Err(err),
        }
    }
}

/// Registers a peer with the poll and adds him to the stream list.
fn add_stream(
    registry: &Registry,
    streams: &mut Slab<MessageStream<TcpStream>>,
    mut stream: TcpStream,
    stream_cfg: message_stream::StreamConfig,
) -> std::io::Result<PeerId> {
    let token = Token(streams.vacant_key());

    registry.register(&mut stream, token, Interest::READABLE)?;
    streams.insert(MessageStream::new(stream, stream_cfg));

    Ok(token.into())
}

/// Deregisters a peer from the poll and removes him from the stream list.
fn remove_stream(
    registry: &Registry,
    streams: &mut Slab<MessageStream<TcpStream>>,
    peer: PeerId,
) -> std::io::Result<()> {
    let mut stream = streams.remove(peer.value());

    registry.deregister(stream.inner_mut())?;

    Ok(())
}

/// Checks if the token is associated with the server (connection listener).
#[inline(always)]
fn is_listener(n_listeners: usize, token: Token) -> bool {
    token != WAKE_TOKEN && token.0 >= (usize::MAX - n_listeners)
}

/// Checks if it is possible to accept a new connection.
#[inline(always)]
fn has_slot(n_listeners: usize, next_key: usize) -> bool {
    next_key < usize::MAX - n_listeners
}

/// Checks if an IO error is of the "would block" variety.
#[inline(always)]
fn would_block(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::WouldBlock
}

/// Determines the interest set wanted by a stream.
#[inline(always)]
fn choose_interest(stream: &MessageStream<TcpStream>) -> Interest {
    match stream.has_queued_data() {
        true => Interest::READABLE | Interest::WRITABLE,
        false => Interest::READABLE,
    }
}

impl message_stream::MaybeReady for TcpStream {
    fn is_ready(&self) -> bool {
        self.peer_addr().is_ok()
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn is_listener() {
        use super::{is_listener, WAKE_TOKEN};
        use mio::Token;

        assert!(!is_listener(0, WAKE_TOKEN));
        assert!(!is_listener(0, Token(WAKE_TOKEN.0 - 1)));
        assert!(!is_listener(0, Token(usize::MIN)));

        assert!(!is_listener(1, WAKE_TOKEN));
        assert!(is_listener(1, Token(WAKE_TOKEN.0 - 1)));
        assert!(!is_listener(1, Token(WAKE_TOKEN.0 - 2)));

        assert!(!is_listener(3, WAKE_TOKEN));
        assert!(is_listener(3, Token(WAKE_TOKEN.0 - 1)));
        assert!(is_listener(3, Token(WAKE_TOKEN.0 - 2)));
        assert!(is_listener(3, Token(WAKE_TOKEN.0 - 3)));
        assert!(!is_listener(3, Token(WAKE_TOKEN.0 - 4)));
    }

    #[test]
    fn connection_slot_open() {}
}
