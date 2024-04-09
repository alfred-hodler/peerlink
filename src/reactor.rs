use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use intmap::IntMap;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Registry, Token, Waker};
use slab::Slab;

use crate::connector::{self, Connector, IntoTarget};
use crate::message_stream::{self, MessageStream};
use crate::{Config, DecodeError, Message, PeerId};

#[cfg(not(feature = "async"))]
use crossbeam_channel::{Receiver, Sender};

#[cfg(feature = "async")]
use async_channel::{Receiver, Sender};

/// Token used for waking the reactor event loop.
const WAKE_TOKEN: Token = Token(usize::MAX);

/// Command variants for the reactor to process.
#[derive(Debug)]
pub enum Command<M: Message, T: IntoTarget> {
    /// Connect to a remote host.
    Connect(T),
    /// Disconnect from a peer.
    Disconnect(PeerId),
    /// Send a message to a peer.
    Message(PeerId, M),
    /// Close all connections and shut down the reactor.
    Shutdown,
    /// Causes the event loop to panic. Only available in debug mode for integration testing.
    #[cfg(debug_assertions)]
    Panic,
}

// Event variants produced by the reactor.
#[derive(Debug)]
pub enum Event<M: Message, T: IntoTarget> {
    /// The reactor attempted to connect to a remote peer.
    ConnectedTo {
        /// The remote host that was connected to. This is in the same format it was specified.
        target: T,
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
        message: M,
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

/// Non-blocking network reactor. This always runs in its own thread and communicates with the
/// caller using [`Handle`].
pub struct Reactor<C, M, T>
where
    C: Connector,
    M: Message,
    T: IntoTarget,
{
    poll: Poll,
    config: Config,
    sender: EventSender<M, T>,
    receiver: Receiver<Command<M, T>>,
    connector: C,
    waker: Arc<Waker>,
    connect_tx: crossbeam_channel::Sender<ConnectResult<T>>,
    connect_rx: crossbeam_channel::Receiver<ConnectResult<T>>,
}

impl<M: Message, T: IntoTarget> Reactor<connector::DefaultConnector, M, T> {
    /// Creates a new reactor with the default connector.
    pub fn new(config: Config) -> io::Result<(Self, Handle<M, T>)> {
        Self::with_connector(config, connector::DefaultConnector)
    }
}

#[cfg(feature = "socks")]
impl<M: Message, T: IntoTarget> Reactor<connector::Socks5Connector, M, T> {
    /// Creates a new reactor that connects through a socks5 proxy. Credentials (username and
    /// password) are required if the proxy requires them.
    ///
    /// Only available with the `socks` feature.
    pub fn with_proxy(
        config: Config,
        proxy: SocketAddr,
        credentials: Option<(String, String)>,
    ) -> io::Result<(Self, Handle<M, T>)> {
        Self::with_connector(config, connector::Socks5Connector { proxy, credentials })
    }
}

/// Convenience type that allows us to easily switch between send implementations depending on the
/// execution model and which feature is active.
struct EventSender<M: Message, T: IntoTarget>(Sender<Event<M, T>>);

impl<M: Message, T: IntoTarget> EventSender<M, T> {
    /// Sends an event to the handle.
    fn send(&self, event: Event<M, T>) {
        #[cfg(feature = "async")]
        let _ = self.0.send_blocking(event);
        #[cfg(not(feature = "async"))]
        let _ = self.0.send(event);
    }
}

impl<C, M, T> Reactor<C, M, T>
where
    C: Connector + Sync + Send + 'static,
    M: Message,
    T: IntoTarget,
{
    /// Creates a new reactor with a custom connector.
    pub fn with_connector(config: Config, connector: C) -> io::Result<(Self, Handle<M, T>)> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE_TOKEN)?);

        let (cmd_sender, cmd_receiver) = channel();
        let (event_sender, event_receiver) = channel();

        let command_sender = Handle {
            sender: cmd_sender,
            receiver: event_receiver,
            waker: waker.clone(),
        };

        let (connect_tx, connect_rx) = crossbeam_channel::unbounded();

        let reactor = Self {
            poll,
            config,
            sender: EventSender(event_sender),
            receiver: cmd_receiver,
            connector,
            waker,
            connect_tx,
            connect_rx,
        };

        Ok((reactor, command_sender))
    }

    /// Runs the reactor in a newly spawned thread and returns a join handle to that thread.
    pub fn run(self) -> std::thread::JoinHandle<io::Result<()>> {
        std::thread::spawn(|| run(self))
    }
}

/// Provides bidirectional communication with a reactor. If this is dropped the reactor stops.
pub struct Handle<M: Message, T: IntoTarget> {
    waker: Arc<Waker>,
    sender: Sender<Command<M, T>>,
    receiver: Receiver<Event<M, T>>,
}

impl<M: Message, T: IntoTarget> Handle<M, T> {
    /// Sends a command to a reactor associated with this handle. If this produces an IO error,
    /// it means the reactor is irrecoverable and should be discarded. This method never blocks so
    /// it is appropriate for use in async contexts.
    pub fn send(&self, command: Command<M, T>) -> io::Result<()> {
        #[cfg(not(feature = "async"))]
        let result = self.sender.send(command);
        #[cfg(feature = "async")]
        let result = self.sender.try_send(command);

        match result {
            Ok(()) => self.waker.wake(),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel disconnected",
            )),
        }
    }

    /// Blocks until the reactor associated with this handle produces a message. If an IO error is
    /// produced, the reactor is irrecoverable and should be discarded. While this method is
    /// available in async contexts, it **should not** be used there. Use `receiver()` to get a
    /// raw handle on the receiver for use in async scenarios or where extra API surfaces are required.
    pub fn receive_blocking(&self) -> io::Result<Event<M, T>> {
        #[cfg(not(feature = "async"))]
        let result = self.receiver.recv();
        #[cfg(feature = "async")]
        let result = self.receiver.recv_blocking();

        result.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "channel disconnected"))
    }

    /// Exposes the receive portion of the handle. The receiver could be of a blocking or async
    /// variety, depending on which feature is active.
    pub fn receiver(&self) -> &Receiver<Event<M, T>> {
        &self.receiver
    }
}

/// The direction of a peer connection.
#[derive(Debug)]
enum Direction<T: IntoTarget> {
    /// The connection is from a remote peer to the reactor.
    Inbound {
        interface: SocketAddr,
        addr: SocketAddr,
    },
    /// The connection is from the reactor to a remote peer.
    Outbound { target: T },
}

/// The state of a peer connection.
enum State {
    /// The connection is still being established.
    Connecting { start: Instant },
    /// The connection is established and ready.
    Connected { id: PeerId },
}

/// Contains a TCP connection along with its metadata.
struct Connection<T: IntoTarget> {
    stream: MessageStream<TcpStream>,
    direction: Direction<T>,
    state: State,
}

/// Runs the reactor in a loop until an error is produced or the shutdown command is received.
fn run<C, M, T>(
    Reactor {
        mut poll,
        config,
        sender,
        receiver,
        connector,
        waker,
        connect_tx,
        connect_rx,
    }: Reactor<C, M, T>,
) -> io::Result<()>
where
    C: Connector + Sync + Send + 'static,
    M: Message,
    T: IntoTarget,
{
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

    let mut connections: Slab<Connection<T>> = Slab::with_capacity(16);
    let mut events = Events::with_capacity(1024);
    let mut read_buf = [0; 1024 * 1024];
    let mut last_maintenance = Instant::now();
    let mut token_map: IntMap<Token> = IntMap::new();
    let mut next_peer_id: u64 = 0;

    loop {
        poll.poll(&mut events, Some(Duration::from_secs(1)))?;

        let now = Instant::now();

        'events: for event in &events {
            match (event.token(), connections.get_mut(event.token().into())) {
                (WAKE_TOKEN, None) => {
                    log::trace!("waker event");

                    for connect in connect_rx.try_iter() {
                        match connect.result {
                            Ok(stream) => {
                                add_stream(
                                    poll.registry(),
                                    &mut connections,
                                    Direction::Outbound {
                                        target: connect.target,
                                    },
                                    stream,
                                    config.stream_config,
                                )?;
                            }

                            Err(err) => {
                                sender.send(Event::ConnectedTo {
                                    target: connect.target,
                                    result: Err(err),
                                });
                            }
                        }
                    }

                    for cmd in std::iter::from_fn(|| receiver.try_recv().ok()) {
                        log::trace!("command: {:?}", cmd);

                        match cmd {
                            Command::Connect(target) => {
                                initiate_connect(&connector, target, &waker, &connect_tx);
                            }

                            Command::Disconnect(peer) => {
                                if token_map.contains_key(peer.0) {
                                    let mut connection = remove_stream(
                                        poll.registry(),
                                        &mut connections,
                                        &mut token_map,
                                        peer,
                                    )?;

                                    let _ = connection.stream.write(now);
                                    let _ = connection.stream.shutdown();

                                    log::debug!("disconnect: peer {peer} disconnected");

                                    sender.send(Event::Disconnected {
                                        peer,
                                        reason: DisconnectReason::Requested,
                                    });
                                } else {
                                    sender.send(Event::NoPeer(peer));
                                    log::warn!("disconnect: peer {peer} not found");
                                }
                            }

                            Command::Message(peer, message) => match token_map.get(peer.0) {
                                Some(token) => {
                                    let connection =
                                        connections.get_mut(token.0).expect("must exist here");

                                    if connection.stream.queue_message(&message) {
                                        poll.registry().reregister(
                                            connection.stream.as_source(),
                                            *token,
                                            Interest::READABLE | Interest::WRITABLE,
                                        )?;
                                    } else {
                                        sender.send(Event::SendBufferFull { peer, message });
                                        log::debug!("message: send buffer for peer {peer} is full");
                                    }
                                }

                                None => {
                                    sender.send(Event::NoPeer(peer));
                                    log::warn!("message: peer {peer} not found");
                                }
                            },

                            Command::Shutdown => {
                                for (id, mut connection) in connections {
                                    let _ = connection.stream.write(now);
                                    let r = connection.stream.shutdown();

                                    log::debug!("shutdown: stream {}: {:?}", id, r);
                                }

                                return Ok(());
                            }

                            #[cfg(debug_assertions)]
                            Command::Panic => panic!("panic command received"),
                        }
                    }
                }

                (token, None) if is_listener(listeners.len(), token) => {
                    let listener = usize::MAX - 1 - token.0;
                    let interface = listeners[listener].local_addr()?;
                    log::trace!("listener: {} (interface {interface})", token.0);

                    while has_slot(connections.len(), connections.vacant_key()) {
                        match listeners[listener].accept() {
                            Ok((stream, addr)) => {
                                add_stream(
                                    poll.registry(),
                                    &mut connections,
                                    Direction::Inbound { interface, addr },
                                    stream,
                                    config.stream_config,
                                )?;
                                log::debug!("accepted connection from {addr}");
                            }
                            Err(err) if would_block(&err) => break,
                            Err(err) => log::debug!("accept error: {}", err),
                        }
                    }
                }

                (token, Some(connection)) => {
                    let peer = match connection.state {
                        State::Connecting { .. } => {
                            if !connection.stream.is_ready() {
                                log::debug!("stream not ready: {:?}", connection.direction);

                                continue 'events;
                            } else {
                                log::debug!("stream ready: {:?}", connection.direction);

                                let peer = PeerId(next_peer_id);
                                let prev_mapping = token_map.insert(peer.0, token);
                                assert!(prev_mapping.is_none());
                                next_peer_id += 1;

                                connection.state = State::Connected { id: peer };

                                let event = match &connection.direction {
                                    Direction::Inbound { interface, addr } => {
                                        Event::ConnectedFrom {
                                            peer,
                                            addr: *addr,
                                            interface: *interface,
                                        }
                                    }
                                    Direction::Outbound { target } => Event::ConnectedTo {
                                        target: target.clone(),
                                        result: Ok(peer),
                                    },
                                };

                                poll.registry().reregister(
                                    connection.stream.as_source(),
                                    token,
                                    Interest::READABLE,
                                )?;

                                sender.send(event);
                            }

                            continue;
                        }
                        State::Connected { id } => id,
                    };

                    if event.is_readable() {
                        log::trace!("readable: peer {peer}");

                        'read: loop {
                            let read_result = connection.stream.read(&mut read_buf);

                            'decode: loop {
                                match connection.stream.receive_message::<M>() {
                                    Ok(message) => {
                                        log::debug!("read: peer {peer}: message={:?}", message);

                                        sender.send(Event::Message { peer, message });
                                    }

                                    Err(DecodeError::MalformedMessage) => {
                                        log::info!("read: peer {peer}: codec violation");

                                        remove_stream(
                                            poll.registry(),
                                            &mut connections,
                                            &mut token_map,
                                            peer,
                                        )?;

                                        sender.send(Event::Disconnected {
                                            peer,
                                            reason: DisconnectReason::CodecViolation,
                                        });

                                        continue 'events;
                                    }

                                    Err(DecodeError::NotEnoughData) => break 'decode,
                                }
                            }

                            match read_result {
                                Ok(0) => {
                                    log::debug!("peer {peer}: peer left");

                                    remove_stream(
                                        poll.registry(),
                                        &mut connections,
                                        &mut token_map,
                                        peer,
                                    )?;

                                    sender.send(Event::Disconnected {
                                        peer,
                                        reason: DisconnectReason::Left,
                                    });

                                    continue 'events;
                                }

                                Ok(_) => continue 'read,

                                Err(err) if would_block(&err) => break 'read,

                                Err(err) => {
                                    log::debug!("peer {peer}: IO error: {err}");

                                    remove_stream(
                                        poll.registry(),
                                        &mut connections,
                                        &mut token_map,
                                        peer,
                                    )?;

                                    sender.send(Event::Disconnected {
                                        peer,
                                        reason: DisconnectReason::Error(err),
                                    });

                                    continue 'events;
                                }
                            }
                        }
                    }

                    if event.is_writable() {
                        log::trace!("writeable: peer {peer}");

                        match connection.stream.write(now) {
                            Ok(()) => {
                                let interest = connection.stream.interest();
                                poll.registry().reregister(
                                    connection.stream.as_source(),
                                    token,
                                    interest,
                                )?;
                            }

                            Err(err) if would_block(&err) => {}

                            Err(err) => {
                                log::debug!("write: peer {peer}: IO error: {err}");

                                remove_stream(
                                    poll.registry(),
                                    &mut connections,
                                    &mut token_map,
                                    peer,
                                )?;

                                sender.send(Event::Disconnected {
                                    peer,
                                    reason: DisconnectReason::Error(err),
                                });
                            }
                        }
                    }
                }

                (_token, connection) => {
                    log::debug!(
                        "spurious event: event={:?}, connection is_some={}",
                        event,
                        connection.is_some()
                    );
                }
            }
        }

        // if we get an error during deregistration, the reactor must terminate
        let mut deregister_error = None;

        // dead stream removal
        connections.retain(|_, connection| {
            let retain = match connection.state {
                State::Connecting { start }
                    if (now - start) > config.stream_config.stream_connect_timeout =>
                {
                    log::debug!("connect timeout: {:?}", &connection.direction);

                    if let Direction::Outbound { target } = &connection.direction {
                        sender.send(Event::ConnectedTo {
                            target: target.clone(),
                            result: Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "Connect attempt timed out",
                            )),
                        });
                    }

                    false
                }
                State::Connected { id } if connection.stream.is_write_stale(now) => {
                    sender.send(Event::Disconnected {
                        peer: id,
                        reason: DisconnectReason::WriteStale,
                    });

                    false
                }
                _ => true,
            };

            if !retain {
                if let Err(err) = poll.registry().deregister(connection.stream.as_source()) {
                    let _ = deregister_error.insert(err);
                }

                if let State::Connected { id } = connection.state {
                    token_map.remove(id.0);
                }
            }

            retain
        });

        if let Some(err) = deregister_error {
            return Err(err);
        }

        // periodic buffer resize
        if (now - last_maintenance).as_secs() > 30 {
            for (_, connection) in &mut connections {
                connection.stream.shrink_buffers();
            }

            last_maintenance = now;
        }
    }
}

/// Registers a peer with the poll and adds it to the connection list.
fn add_stream<T: IntoTarget>(
    registry: &Registry,
    connections: &mut Slab<Connection<T>>,
    direction: Direction<T>,
    mut stream: TcpStream,
    stream_cfg: message_stream::StreamConfig,
) -> std::io::Result<()> {
    let token = Token(connections.vacant_key());

    registry.register(&mut stream, token, Interest::WRITABLE)?;

    connections.insert(Connection {
        stream: MessageStream::new(stream, stream_cfg),
        direction,
        state: State::Connecting {
            start: Instant::now(),
        },
    });

    Ok(())
}

/// Deregisters a peer from the poll and removes it from the connection list.
fn remove_stream<T: IntoTarget>(
    registry: &Registry,
    connections: &mut Slab<Connection<T>>,
    token_map: &mut IntMap<Token>,
    peer: PeerId,
) -> std::io::Result<Connection<T>> {
    let token = token_map.remove(peer.0).expect("must exist here");

    let mut connection = connections.remove(token.0);

    registry.deregister(connection.stream.as_source())?;

    Ok(connection)
}

/// Describes the result of a connect attempt against a remote host.
struct ConnectResult<T: IntoTarget> {
    target: T,
    result: io::Result<mio::net::TcpStream>,
}

/// Initiates a connect procedure against a remote host using a `Connector` implementation.
fn initiate_connect<C: Connector, T: IntoTarget>(
    connector: &C,
    target: T,
    waker: &Arc<Waker>,
    sender: &crossbeam_channel::Sender<ConnectResult<T>>,
) {
    #[inline]
    fn connect<C: Connector, T: IntoTarget>(
        connector: &C,
        target: T,
        waker: &Arc<Waker>,
        sender: &crossbeam_channel::Sender<ConnectResult<T>>,
    ) {
        let start = Instant::now();
        let result = connector.connect(&target);
        let elapsed = Instant::now() - start;
        log::debug!(
            "connector: target={:?} elapsed={}ms",
            target,
            elapsed.as_millis()
        );
        let _ = sender.send(ConnectResult { target, result });
        let _ = waker.wake();
    }

    if C::CONNECT_IN_BACKGROUND {
        let waker = waker.clone();
        let sender = sender.clone();
        let connector = connector.clone();

        std::thread::spawn(move || connect(&connector, target, &waker, &sender));
    } else {
        connect(connector, target, waker, sender)
    }
}

/// Checks if a token is associated with the server (connection listener).
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

#[cfg(not(feature = "async"))]
fn channel<M>() -> (crossbeam_channel::Sender<M>, crossbeam_channel::Receiver<M>) {
    crossbeam_channel::unbounded()
}

#[cfg(feature = "async")]
fn channel<M>() -> (async_channel::Sender<M>, async_channel::Receiver<M>) {
    async_channel::unbounded()
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
}
