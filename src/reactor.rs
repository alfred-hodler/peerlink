use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use mio::net::TcpListener;
use mio::{Interest, Poll, Token, Waker};

use crate::connector::{self, Connector, Target};
use crate::reactor::handle::IdleHandle;
use crate::{Command, Config, DisconnectReason, Event, Message, message_stream};

#[cfg(not(feature = "async"))]
use crossbeam_channel::{Receiver, Sender};

#[cfg(feature = "async")]
use async_channel::{Receiver, Sender};

pub use handle::{Handle, RecvError, SendError};

mod connection;
mod handle;
mod scheduler;

/// Runs a new reactor using some config and the default connector.
pub fn run<M: Message>(config: Config) -> std::io::Result<Handle<M>> {
    let (reactor, handle) = Reactor::new(config)?;
    let join_handle = reactor.run();
    Ok(handle.into_running(join_handle))
}

/// Runs a new reactor using custom settings and a custom reactor.
pub fn run_with_connector<M: Message, C: connector::Connector>(
    config: Config,
    connector: C,
) -> std::io::Result<Handle<M>> {
    let (reactor, handle) = Reactor::with_connector(config, connector)?;
    let join_handle = reactor.run();
    Ok(handle.into_running(join_handle))
}

/// Runs a new peerlink instance that connects through a socks5 proxy. Credentials (username and
/// password) are required if the proxy requires them.
///
/// Only available with the `socks` feature.
#[cfg(feature = "socks")]
pub fn run_with_socks5_proxy<M: Message, C: connector::Connector>(
    config: Config,
    proxy: SocketAddr,
    credentials: Option<(String, String)>,
) -> std::io::Result<Handle<M>> {
    let connector = connector::Socks5Connector { proxy, credentials };
    let (reactor, handle) = Reactor::with_connector(config, connector)?;
    let join_handle = reactor.run();
    Ok(handle.into_running(join_handle))
}

/// Non-blocking network reactor. This always runs in its own thread and communicates with the
/// caller using [`Handle`].
struct Reactor<C, M>
where
    C: Connector,
    M: Message,
{
    poll: Poll,
    config: Config,
    sender: EventSender<M>,
    receiver: Receiver<SystemCommand<M>>,
    connector: C,
    waker: Arc<Waker>,
    connect_tx: crossbeam_channel::Sender<ConnectResult>,
    connect_rx: crossbeam_channel::Receiver<ConnectResult>,
    listeners: Vec<(TcpListener, SocketAddr)>,
    connection_manager: connection::Manager,
    scheduler: scheduler::Scheduler,
    rx_buf: Vec<u8>,
}

impl<M: Message> Reactor<connector::DefaultConnector, M> {
    /// Creates a new reactor with the default connector.
    fn new(config: Config) -> io::Result<(Self, IdleHandle<M>)> {
        Self::with_connector(config, connector::DefaultConnector)
    }
}

impl<C, M> Reactor<C, M>
where
    C: Connector + Sync + Send + 'static,
    M: Message,
{
    /// Creates a new reactor with a custom connector.
    fn with_connector(config: Config, connector: C) -> io::Result<(Self, IdleHandle<M>)> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), scheduler::WAKER)?);

        let (cmd_sender, cmd_receiver) = channel(None);
        let (event_sender, event_receiver) = channel(config.receive_channel_size);

        let command_sender = IdleHandle {
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
        let receive_buf_size = config.receive_buffer_size.min(M::MAX_SIZE);
        let mut read_buf = Vec::with_capacity(receive_buf_size);

        #[allow(clippy::uninit_vec)]
        unsafe {
            // this is a receive buffer where we never care about the part that was not filled,
            // so having uninit memory is fine
            read_buf.set_len(receive_buf_size);
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
            scheduler,
            rx_buf: read_buf,
        };

        Ok((reactor, command_sender))
    }

    /// Runs the reactor in a newly spawned thread and returns a join handle to that thread.
    fn run(self) -> std::thread::JoinHandle<io::Result<()>> {
        std::thread::spawn(|| run_inner(self))
    }
}

fn run_inner<C, M>(
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
        mut scheduler,
        rx_buf: mut read_buf,
    }: Reactor<C, M>,
) -> io::Result<()>
where
    C: Connector + Sync + Send + 'static,
    M: Message,
{
    let mut round: u64 = 0;
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
                                let (was_queued, bytes_queued) =
                                    connection.queue_message(&message, poll.registry())?;
                                if !was_queued {
                                    sender.send(Event::QueueRejected { peer, message });
                                    log::debug!("message: send buffer for peer {peer} is full");
                                } else if connection.config().outbound_telemetry {
                                    sender.send(Event::OutboundTelemetry {
                                        peer,
                                        available: connection.available::<M>(),
                                        delta: bytes_queued as isize,
                                    });
                                }
                            }
                            None => {
                                sender.send(Event::NoPeer(peer));
                                log::warn!("message: peer {peer} not found");
                            }
                        }
                    }

                    SystemCommand::Shutdown(termination) => {
                        let timeout = match termination {
                            Termination::Immediate => None,
                            Termination::TryFlush(duration) => Some(duration),
                        };
                        connection_manager.shutdown(timeout);
                        return Ok(());
                    }
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

        scheduler.connections(round, |token, is_readable, is_writable, is_standalone| {
            let (peer, connection) = match connection_manager.try_ready(&token, poll.registry())? {
                connection::Connectedness::Nonexistent => {
                    if is_standalone {
                        // standalone readiness that maps to no connection, probably expired
                        return Ok(scheduler::Carryover::none());
                    } else {
                        // this is a serious logic error, should never happen
                        unreachable!("try_ready: token -> connection mapping nonexistent");
                    }
                }
                connection::Connectedness::New { peer, target } => {
                    assert!(!is_readable && is_writable);
                    sender.send(Event::ConnectedTo {
                        target,
                        result: Ok(peer),
                    });
                    return Ok(scheduler::Carryover::none());
                }
                connection::Connectedness::Ready { peer, connection } => (peer, connection),
                connection::Connectedness::NotReady => {
                    return Ok(scheduler::Carryover::none());
                }
                connection::Connectedness::Errored { target, error } => {
                    sender.send(Event::ConnectedTo {
                        target,
                        result: Err(error),
                    });
                    return Ok(scheduler::Carryover::none());
                }
            };

            let mut read_carryover = false;
            let mut write_carryover = false;

            if is_readable {
                log::trace!("readable: peer {peer}");

                match connection.read(&mut read_buf, |message, size| {
                    log::debug!("read: peer {peer}: message={:?}", message);
                    sender.send(Event::Message {
                        peer,
                        message,
                        size,
                    });
                }) {
                    Ok(maybe_read_carryover) => {
                        read_carryover = maybe_read_carryover;
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

                        return Ok(scheduler::Carryover::none());
                    }
                }
            }

            if is_writable {
                // we are either just connected or just ready to write
                log::trace!("writeable: peer {peer}");

                match connection.write(now, poll.registry(), token)? {
                    Ok((carryover, written)) => {
                        write_carryover = carryover;
                        if config.stream_config.outbound_telemetry {
                            let available = connection.available::<M>();
                            sender.send(Event::OutboundTelemetry {
                                peer,
                                available,
                                delta: -(written as isize),
                            });
                        }
                    }
                    Err(err) => {
                        log::debug!("write: peer {peer}: IO error: {err}");
                        connection_manager.disconnect(&peer, poll.registry(), now)?;
                        sender.send(Event::Disconnected {
                            peer,
                            reason: DisconnectReason::Error(err),
                        });
                    }
                }
            }

            Ok(scheduler::Carryover {
                r: read_carryover,
                w: write_carryover,
            })
        })?;

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

        connection_manager.compact(now)?;

        round += 1;
    }
}

/// Convenience type that allows us to easily switch between send implementations depending on the
/// execution model and which feature is active.
struct EventSender<M: Message>(Sender<Event<M>>);

impl<M: Message> EventSender<M> {
    /// Sends an event to the handle.
    fn send(&self, event: Event<M>) {
        #[cfg(feature = "async")]
        let _ = self.0.send_blocking(event);
        #[cfg(not(feature = "async"))]
        let _ = self.0.send(event);
    }
}

/// System commands. Not for external use.
#[derive(Debug)]
enum SystemCommand<M: Message> {
    /// Various P2P commands.
    P2P(Command<M>),
    /// Close all connections and shut down the reactor.
    Shutdown(Termination),
}

/// Determines how the reactor should be shut down.
#[derive(Debug, Clone, Copy)]
pub enum Termination {
    /// Shuts down the reactor immediately and tries to flush whatever was in the outbound buffers
    /// without waiting further.
    Immediate,
    /// Shuts down the reactor but attempts to deliver all pending messages before closing.
    TryFlush(std::time::Duration),
}

/// Describes the result of a connect attempt against a remote host.
struct ConnectResult {
    target: Target,
    result: io::Result<mio::net::TcpStream>,
}

/// Initiates a connect procedure against a remote host using a `Connector` implementation.
fn initiate_connect<C: Connector>(
    connector: &C,
    target: Target,
    waker: &Arc<Waker>,
    sender: &crossbeam_channel::Sender<ConnectResult>,
) {
    #[inline]
    fn connect<C: Connector>(
        connector: &C,
        target: Target,
        waker: &Arc<Waker>,
        sender: &crossbeam_channel::Sender<ConnectResult>,
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
