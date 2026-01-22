use std::io;
use std::time::{Duration, Instant};

use mio::net::TcpStream;
use mio::{Interest, Registry, Token};
use slab::Slab;

use crate::connector::Target;
use crate::message_stream::{MessageStream, ReadError, WriteResult};
use crate::{Config, Message, PeerId, StreamConfig};

/// The direction of a connection.
#[derive(Debug, Clone)]
enum Direction {
    /// The connection is from a remote peer to us. Inbound connections are always ready to use.
    Inbound { peer: PeerId },
    /// The connection is from us to a remote peer. The state can vary.
    Outbound { target: Target, state: State },
}

/// The internal state of an outbound connection.
#[derive(Debug, Clone)]
enum State {
    /// The connection is still being established.
    Connecting { start: Instant },
    /// The connection is established and ready.
    Connected { peer: PeerId },
}

/// Contains a TCP connection along with its metadata.
pub struct Connection {
    stream: MessageStream<TcpStream>,
    direction: Direction,
    token: Token,
}

impl Connection {
    /// Reads data from a connection and then attempts to decode messages from the data.
    /// Decoded messages are passed into the provided closure, which takes a message and its
    /// original wire size. Encountering an error means that the connection must be discarded.
    /// Attempts to read from a connection and decode messages in a fair manner.
    ///
    /// Returns a boolean denoting whether there is more work available.
    pub fn read<M: Message, F: Fn(M, usize)>(
        &mut self,
        rx_buf: &mut [u8],
        on_msg: F,
    ) -> Result<bool, ReadError> {
        self.stream.read(rx_buf, on_msg)
    }

    /// Queues a message for sending. This method simply encodes the message and places it into
    /// the internal buffer. [`write`] must be called when the socket is writeable in order to flush.
    ///
    /// Returns `true` if the write buffer contains enough space to accept the message, or `false`
    /// if the buffer is full and the message cannot be queued at this time.
    pub fn queue_message<M: Message>(
        &mut self,
        message: &M,
        registry: &Registry,
    ) -> io::Result<bool> {
        if self.stream.queue_message(message) {
            registry.reregister(
                self.stream.as_source(),
                self.token,
                Interest::READABLE | Interest::WRITABLE,
            )?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns how many bytes can be queued immediately with respect to transmit buffer size limit.
    pub fn available<M: Message>(&self) -> usize {
        self.stream.available::<M>()
    }

    /// Writes out as many bytes from the send buffer as possible, until blocking would start.
    ///
    /// Returns whether more write work is available immediately (without blocking). Encountering an
    /// error here means the connection must be discarded.
    pub fn write(
        &mut self,
        now: Instant,
        registry: &Registry,
        token: Token,
    ) -> io::Result<io::Result<bool>> {
        match self.stream.write(now) {
            Ok(WriteResult::Done) => {
                registry.reregister(self.stream.as_source(), token, Interest::READABLE)?;
                Ok(Ok(false))
            }
            Ok(WriteResult::WouldBlock) => Ok(Ok(false)),
            Ok(WriteResult::BudgetExceeded) => Ok(Ok(true)),
            Err(err) => Ok(Err(err)),
        }
    }
}

/// The state of a connection for outside observation.
pub enum Connectedness<'a> {
    Nonexistent,
    NotReady,
    Ready {
        peer: PeerId,
        connection: &'a mut Connection,
    },
    New {
        peer: PeerId,
        target: Target,
    },
    Errored {
        target: Target,
        error: io::Error,
    },
}

/// The reason a connection is dead, for outside observation.
pub enum Dead {
    OutboundTimeout(Target),
    WriteStale(PeerId),
}

/// Handles and tracks connections.
pub struct Manager {
    connections: Slab<Connection>,
    token_map: hashbrown::HashMap<PeerId, Token>,
    next_peer_id: PeerId,
    dead: Vec<Dead>,
    last_buffer_resize: Instant,
    last_cleanup: Instant,
}

impl Manager {
    /// Creates a new connection manager.
    pub fn new() -> Self {
        Self {
            connections: Slab::with_capacity(16),
            token_map: hashbrown::HashMap::with_capacity(16),
            next_peer_id: PeerId(0),
            dead: Vec::with_capacity(8),
            last_buffer_resize: Instant::now(),
            last_cleanup: Instant::now(),
        }
    }

    /// Returns a connection by peer id, if such a peer id is known.
    pub fn get_by_peer_id(&mut self, peer: &PeerId) -> Option<&mut Connection> {
        self.token_map.get_mut(peer).map(|token| {
            self.connections
                .get_mut(token.0)
                .expect("get: token -> connection must exist")
        })
    }

    /// Gets the connectedness status of a connection. For new connections, it attempts to put them
    /// in a usable state and register them with the registry.
    pub fn try_ready<'a>(
        &'a mut self,
        token: &Token,
        registry: &Registry,
    ) -> io::Result<Connectedness<'a>> {
        let connect_err = {
            let Some(connection) = self.connections.get(token.0) else {
                return Ok(Connectedness::Nonexistent);
            };

            match &connection.direction {
                Direction::Outbound {
                    target,
                    state: State::Connecting { .. },
                } => connection
                    .stream
                    .take_error()
                    .map(|err| (err, target.clone())),
                _ => None,
            }
        };

        if let Some((err, target)) = connect_err {
            let mut connection = self.connections.remove(token.0);
            let _ = registry.deregister(connection.stream.as_source());
            Ok(Connectedness::Errored { target, error: err })
        } else {
            let connection = self
                .connections
                .get_mut(token.0)
                .expect("try_ready: token -> connection must exist");

            match &connection.direction {
                Direction::Outbound {
                    target,
                    state: State::Connecting { .. },
                } => {
                    if !connection.stream.is_ready() {
                        log::debug!("stream not ready: {:?}", connection.direction);
                        Ok(Connectedness::NotReady)
                    } else {
                        log::debug!("stream ready: {:?}", connection.direction);

                        let peer = self.next_peer_id;
                        let prev_mapping = self.token_map.insert(peer, *token);
                        assert!(prev_mapping.is_none());
                        self.next_peer_id = self.next_peer_id.next();
                        let target = target.clone();

                        connection.direction = Direction::Outbound {
                            target: target.clone(),
                            state: State::Connected { peer },
                        };

                        registry.reregister(
                            connection.stream.as_source(),
                            *token,
                            Interest::READABLE,
                        )?;

                        Ok(Connectedness::New { peer, target })
                    }
                }
                Direction::Outbound {
                    state: State::Connected { peer },
                    ..
                }
                | Direction::Inbound { peer, .. } => Ok(Connectedness::Ready {
                    peer: *peer,
                    connection,
                }),
            }
        }
    }

    /// Adds an outbound connection to the manager.
    pub fn add_outbound(
        &mut self,
        registry: &Registry,
        mut stream: TcpStream,
        stream_cfg: StreamConfig,
        start: Instant,
        target: Target,
    ) -> std::io::Result<()> {
        let vacancy = self.connections.vacant_entry();
        let token = Token(vacancy.key());

        registry.register(&mut stream, Token(vacancy.key()), Interest::WRITABLE)?;

        vacancy.insert(Connection {
            stream: MessageStream::new(stream, stream_cfg),
            direction: Direction::Outbound {
                target,
                state: State::Connecting { start },
            },
            token,
        });

        Ok(())
    }

    /// Adds an inbound connection to the manager.
    pub fn add_inbound(
        &mut self,
        registry: &Registry,
        mut stream: TcpStream,
        stream_cfg: StreamConfig,
    ) -> std::io::Result<PeerId> {
        let vacancy = self.connections.vacant_entry();
        let token = Token(vacancy.key());

        registry.register(&mut stream, token, Interest::READABLE)?;

        let peer = self.next_peer_id;
        let prev_mapping = self.token_map.insert(peer, token);
        assert!(prev_mapping.is_none());
        self.next_peer_id = self.next_peer_id.next();

        vacancy.insert(Connection {
            stream: MessageStream::new(stream, stream_cfg),
            direction: Direction::Inbound { peer },
            token,
        });

        Ok(peer)
    }

    /// Disconnects a peer. Returns a boolean denoting whether the peer existed.
    pub fn disconnect(
        &mut self,
        peer: &PeerId,
        registry: &Registry,
        now: Instant,
    ) -> io::Result<bool> {
        match self.token_map.remove(peer) {
            Some(token) => {
                let mut connection = self
                    .connections
                    .try_remove(token.0)
                    .expect("disconnect: token -> connection must exist");
                registry.deregister(connection.stream.as_source())?;
                let _ = connection.stream.write(now);
                let _ = connection.stream.shutdown();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Performs internal housekeeping if necessary.
    pub fn compact<F: FnMut(Token, Token)>(
        &mut self,
        now: Instant,
        registry: &Registry,
        mut rekey: F,
    ) -> io::Result<()> {
        if (now - self.last_buffer_resize).as_secs() > 30 {
            for (_, connection) in &mut self.connections {
                connection.stream.shrink_buffers();
            }
            self.last_buffer_resize = now;
        }

        let mut result = Ok(());
        let capacity = self.connections.capacity();
        if capacity > 32 && capacity >= self.connections.len() * 2 {
            self.connections.compact(|c, from, to| {
                let (_, entry) = self
                    .token_map
                    .iter_mut()
                    .find(|(_, token)| token.0 == from)
                    .unwrap();
                *entry = Token(to);

                c.token = Token(to);

                rekey(Token(from), Token(to));

                let interest = if c.stream.has_queued_data() {
                    Interest::READABLE | Interest::WRITABLE
                } else {
                    Interest::READABLE
                };

                if let Err(err) = registry.reregister(c.stream.as_source(), Token(to), interest) {
                    result = Err(err);
                    false
                } else {
                    true
                }
            });
        }

        result
    }

    /// Removes dead connections and returns an iterator of removal reasons.
    pub fn remove_dead(
        &mut self,
        now: Instant,
        config: &Config,
        registry: &Registry,
    ) -> io::Result<impl Iterator<Item = Dead>> {
        if (now - self.last_cleanup).as_millis() >= 1000 {
            self.last_cleanup = now;

            self.dead.clear();
            self.dead.shrink_to(8);
            let mut deregister_error = Ok(());

            self.connections.retain(|_, connection| {
                let retain = match &connection.direction {
                    Direction::Outbound {
                        target,
                        state: State::Connecting { start },
                    } if (now - *start) > config.stream_config.connect_timeout => {
                        log::debug!("connect timeout: {:?}", &connection.direction);
                        self.dead.push(Dead::OutboundTimeout(target.clone()));
                        false
                    }
                    Direction::Inbound { peer, .. }
                    | Direction::Outbound {
                        state: State::Connected { peer },
                        ..
                    } if connection.stream.is_write_stale(now) => {
                        self.dead.push(Dead::WriteStale(*peer));
                        self.token_map.remove(peer);
                        false
                    }
                    _ => true,
                };

                if !retain {
                    if let Err(err) = registry.deregister(connection.stream.as_source()) {
                        deregister_error = Err(err);
                    }
                }

                retain
            });

            deregister_error.map(|_| self.dead.drain(..))
        } else {
            self.dead.clear();
            Ok(self.dead.drain(..))
        }
    }

    /// Shuts down every connection.
    pub fn shutdown(self, now: Instant) {
        for (token, mut connection) in self.connections {
            let _ = connection.stream.write(now);
            if connection
                .stream
                .is_write_stale(now + Duration::from_secs(3600))
            {
                log::debug!("shutdown: connection had unsent data");
            }
            let shutdown_result = connection.stream.shutdown();

            log::debug!("shutdown: stream {}: {:?}", token, shutdown_result);
        }
    }

    /// Checks if it is possible to accept a new connection.
    #[inline(always)]
    pub fn has_slot(&self, n_listeners: usize) -> bool {
        self.connections.vacant_key() < usize::MAX - n_listeners
    }
}
