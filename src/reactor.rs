use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use mio::net::TcpListener;
use mio::{Interest, Poll, Token, Waker};

use crate::connector::{self, Connector, IntoTarget};
use crate::{Command, Config, DisconnectReason, Event, Message, message_stream};

#[cfg(not(feature = "async"))]
use crossbeam_channel::{Receiver, Sender};

#[cfg(feature = "async")]
use async_channel::{Receiver, Sender};

mod connection;
mod scheduler;

/// Provides bidirectional communication with a reactor. If this is dropped the reactor stops.
pub struct Handle<M: Message, T: IntoTarget> {
    waker: Arc<Waker>,
    sender: Sender<SystemCommand<M, T>>,
    receiver: Receiver<Event<M, T>>,
}

impl<M: Message, T: IntoTarget> Handle<M, T> {
    /// Sends a command to a reactor associated with this handle. If this produces an IO error,
    /// it means the reactor is irrecoverable and should be discarded. This method never blocks so
    /// it is appropriate for use in async contexts.
    pub fn send(&self, command: Command<M, T>) -> io::Result<()> {
        #[cfg(not(feature = "async"))]
        let result = self.sender.send(SystemCommand::P2P(command));
        #[cfg(feature = "async")]
        let result = self.sender.try_send(SystemCommand::P2P(command));

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

    /// Shuts down the reactor and consumes the handle. No further commands can be sent afterward.
    pub fn shutdown(self) -> io::Result<()> {
        #[cfg(not(feature = "async"))]
        let result = self.sender.send(SystemCommand::Shutdown);
        #[cfg(feature = "async")]
        let result = self.sender.try_send(SystemCommand::Shutdown);

        match result {
            Ok(()) => self.waker.wake(),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel disconnected",
            )),
        }
    }

    /// Causes the reactor to panic. For testing only. No further commands can be sent afterward.
    #[cfg(debug_assertions)]
    pub fn panic(self) -> io::Result<()> {
        #[cfg(not(feature = "async"))]
        let result = self.sender.send(SystemCommand::Panic);
        #[cfg(feature = "async")]
        let result = self.sender.try_send(SystemCommand::Panic);

        match result {
            Ok(()) => self.waker.wake(),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel disconnected",
            )),
        }
    }
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
    receiver: Receiver<SystemCommand<M, T>>,
    connector: C,
    waker: Arc<Waker>,
    connect_tx: crossbeam_channel::Sender<ConnectResult<T>>,
    connect_rx: crossbeam_channel::Receiver<ConnectResult<T>>,
    listeners: Vec<(TcpListener, SocketAddr)>,
    connection_manager: connection::Manager<T>,
    last_maintenance: Instant,
    scheduler: scheduler::Scheduler,
    rx_buf: Vec<u8>,
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

impl<C, M, T> Reactor<C, M, T>
where
    C: Connector + Sync + Send + 'static,
    M: Message,
    T: IntoTarget,
{
    /// Creates a new reactor with a custom connector.
    pub fn with_connector(config: Config, connector: C) -> io::Result<(Self, Handle<M, T>)> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), scheduler::WAKER)?);

        let (cmd_sender, cmd_receiver) = channel(None);
        let (event_sender, event_receiver) = channel(config.receive_backpressure_control);

        let command_sender = Handle {
            sender: cmd_sender,
            receiver: event_receiver,
            waker: waker.clone(),
        };

        let (connect_tx, connect_rx) = crossbeam_channel::unbounded();

        let listeners: Vec<_> = config
            .bind_addr
            .iter()
            .enumerate()
            .map(|(offset, addr)| {
                let mut listener = TcpListener::bind(*addr)?;
                let token = usize::MAX - 1 - offset;

                poll.registry()
                    .register(&mut listener, Token(token), Interest::READABLE)?;

                log::info!("Server (token {token}): listen at {addr}");

                Ok((listener, *addr))
            })
            .collect::<std::io::Result<Vec<_>>>()?;

        let scheduler = scheduler::Scheduler::new(listeners.len());
        let mut read_buf = Vec::with_capacity(config.receive_buffer_size);

        #[allow(clippy::uninit_vec)]
        unsafe {
            // this is a receive buffer where we never care about the part that was not filled,
            // so having uninit memory is fine
            read_buf.set_len(config.receive_buffer_size);
        };

        let reactor = Self {
            poll,
            config,
            sender: EventSender(event_sender),
            receiver: cmd_receiver,
            connector,
            waker,
            connect_tx,
            connect_rx,
            listeners,
            connection_manager: connection::Manager::new(),
            last_maintenance: Instant::now(),
            scheduler,
            rx_buf: read_buf,
        };

        Ok((reactor, command_sender))
    }

    /// Runs the reactor in a newly spawned thread and returns a join handle to that thread.
    pub fn run(self) -> std::thread::JoinHandle<io::Result<()>> {
        std::thread::spawn(|| run_inner(self))
    }
}

fn run_inner<C, M, T>(
    Reactor {
        mut poll,
        config,
        sender,
        receiver,
        connector,
        waker,
        connect_tx,
        connect_rx,
        listeners,
        mut connection_manager,
        mut last_maintenance,
        mut scheduler,
        rx_buf: mut read_buf,
    }: Reactor<C, M, T>,
) -> io::Result<()>
where
    C: Connector + Sync + Send + 'static,
    M: Message,
    T: IntoTarget,
{
    let mut iteration: u64 = 0;
    loop {
        scheduler.update(&mut poll)?;

        let now = Instant::now();

        if scheduler.waker() {
            log::trace!("waker event");

            for cmd in std::iter::from_fn(|| receiver.try_recv().ok()) {
                log::trace!("command: {:?}", cmd);

                match cmd {
                    SystemCommand::P2P(Command::Connect(target)) => {
                        initiate_connect(&connector, target, &waker, &connect_tx);
                    }

                    SystemCommand::P2P(Command::Disconnect(peer)) => {
                        if connection_manager.disconnect(&peer, poll.registry(), now)? {
                            sender.send(Event::Disconnected {
                                peer,
                                reason: DisconnectReason::Requested,
                            });
                            log::debug!("disconnect: peer {peer} disconnected");
                        } else {
                            sender.send(Event::NoPeer(peer));
                            log::warn!("disconnect: peer {peer} not found");
                        }
                    }

                    SystemCommand::P2P(Command::Message(peer, message)) => {
                        match connection_manager.get_by_peer_id(&peer) {
                            Some(connection) => {
                                if !connection.queue_message(&message, poll.registry())? {
                                    sender.send(Event::SendBufferFull { peer, message });
                                    log::debug!("message: send buffer for peer {peer} is full");
                                }
                            }
                            None => {
                                sender.send(Event::NoPeer(peer));
                                log::warn!("message: peer {peer} not found");
                            }
                        }
                    }

                    SystemCommand::Shutdown => {
                        connection_manager.shutdown(now);
                        return Ok(());
                    }

                    #[cfg(debug_assertions)]
                    SystemCommand::Panic => panic!("panic command received"),
                }
            }

            // process outbound connection attempt results
            for connect in connect_rx.try_iter() {
                match connect.result {
                    Ok(stream) => {
                        // The connection could be in any state, still needs
                        // to undergo connectedness checks by expressing
                        // WRITE interest. Cannot assume anything here.
                        connection_manager.add_outbound(
                            poll.registry(),
                            stream,
                            config.stream_config,
                            now,
                            connect.target,
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
        }

        for token in scheduler.listeners() {
            let listener = usize::MAX - 1 - token.0;
            let (listener, interface) = &listeners[listener];
            log::trace!("listener: {} (interface {interface})", token.0);

            while connection_manager.has_slot(listeners.len()) {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let peer = connection_manager.add_inbound(
                            poll.registry(),
                            stream,
                            config.stream_config,
                        )?;
                        sender.send(Event::ConnectedFrom {
                            peer,
                            addr,
                            interface: *interface,
                        });
                        log::debug!("accepted connection from {addr}");
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => log::debug!("accept error: {}", err),
                }
            }
        }

        scheduler.connections(iteration, |token, readiness| {
            let (peer, connection) = match connection_manager.try_ready(&token, poll.registry())? {
                connection::Connectedness::New { peer, target } => {
                    assert!(!readiness.read && readiness.write);
                    sender.send(Event::ConnectedTo {
                        target,
                        result: Ok(peer),
                    });
                    readiness.write = false;
                    return Ok(());
                }
                connection::Connectedness::Ready { peer, connection } => (peer, connection),
                connection::Connectedness::NotReady => {
                    readiness.complete();
                    return Ok(());
                }
                connection::Connectedness::Errored { target, error } => {
                    sender.send(Event::ConnectedTo {
                        target,
                        result: Err(error),
                    });
                    readiness.complete();
                    return Ok(());
                }
            };

            if readiness.read {
                log::trace!("readable: peer {peer}");

                match connection.read(&mut read_buf, |message| {
                    log::debug!("read: peer {peer}: message={:?}", message);
                    sender.send(Event::Message { peer, message });
                }) {
                    Ok(maybe_more) => {
                        readiness.read = maybe_more;
                    }
                    Err(err) => {
                        let reason = match err {
                            message_stream::ReadError::MalformedMessage => {
                                log::info!("read: peer {peer}: codec violation");
                                DisconnectReason::CodecViolation
                            }
                            message_stream::ReadError::EndOfStream => {
                                log::debug!("peer {peer}: peer left");
                                DisconnectReason::Left
                            }
                            message_stream::ReadError::Error(err) => {
                                log::debug!("read: peer {peer}: IO error: {err}");
                                DisconnectReason::Error(err)
                            }
                        };

                        connection_manager.disconnect(&peer, poll.registry(), now)?;

                        sender.send(Event::Disconnected { peer, reason });
                        readiness.complete();

                        return Ok(());
                    }
                }
            }

            if readiness.write {
                // we are either just connected or just ready to write
                log::trace!("writeable: peer {peer}");

                if let Err(err) = connection.write(now, poll.registry(), token)? {
                    log::debug!("write: peer {peer}: IO error: {err}");

                    connection_manager.disconnect(&peer, poll.registry(), now)?;

                    sender.send(Event::Disconnected {
                        peer,
                        reason: DisconnectReason::Error(err),
                    });
                }
                readiness.write = false;
            }
            Ok(())
        })?;

        scheduler.maybe_rearm(&waker)?;

        if (now - last_maintenance).as_millis() > 900 {
            for dead in connection_manager.remove_dead(now, &config, poll.registry())? {
                match dead {
                    connection::Dead::OutboundTimeout(target) => {
                        sender.send(Event::ConnectedTo {
                            target,
                            result: Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "Connect attempt timed out",
                            )),
                        });
                    }
                    connection::Dead::WriteStale(peer) => {
                        sender.send(Event::Disconnected {
                            peer,
                            reason: DisconnectReason::WriteStale,
                        });
                    }
                }
            }
            last_maintenance = now;
        }

        connection_manager.maintenance(now);
        iteration = iteration.wrapping_add(1);
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

/// System commands. Not for external use.
#[derive(Debug)]
enum SystemCommand<M: Message, T: IntoTarget> {
    /// Various P2P commands.
    P2P(Command<M, T>),
    /// Close all connections and shut down the reactor.
    Shutdown,
    /// Causes the event loop to panic. Only available in debug mode for integration testing.
    #[cfg(debug_assertions)]
    Panic,
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

#[cfg(not(feature = "async"))]
fn channel<M>(
    cap: Option<NonZeroUsize>,
) -> (crossbeam_channel::Sender<M>, crossbeam_channel::Receiver<M>) {
    match cap {
        Some(cap) => crossbeam_channel::bounded(cap.into()),
        None => crossbeam_channel::unbounded(),
    }
}

#[cfg(feature = "async")]
fn channel<M>(cap: Option<NonZeroUsize>) -> (async_channel::Sender<M>, async_channel::Receiver<M>) {
    match cap {
        Some(cap) => async_channel::bounded(cap.into()),
        None => async_channel::unbounded(),
    }
}
