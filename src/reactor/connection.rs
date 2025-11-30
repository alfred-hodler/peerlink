use std::io;
use std::time::{Duration, Instant};

use mio::net::TcpStream;
use mio::{Interest, Registry, Token};
use slab::Slab;

use crate::connector::IntoTarget;
use crate::message_stream::{MessageStream, ReadError};
use crate::{Config, Message, PeerId, StreamConfig};

/// The direction of a connection.
#[derive(Debug, Clone)]
enum Direction<T: IntoTarget> {
    /// The connection is from a remote peer to us.
    /// Inbound connections are always ready to use.
    Inbound { peer: PeerId },
    /// The connection is from us to a remote peer.
    /// The state can vary.
    Outbound { target: T, state: State },
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
pub struct Connection<T: IntoTarget> {
    stream: MessageStream<TcpStream>,
    direction: Direction<T>,
    token: Token,
}

impl<T: IntoTarget> Connection<T> {
    // TODO document
    pub fn read<M: Message, F: Fn(M)>(
        &mut self,
        rx_buf: &mut [u8],
        on_msg: F,
    ) -> Result<bool, ReadError> {
        self.stream.read(rx_buf, on_msg)
    }

    // TODO document
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

    // TODO document
    pub fn write(
        &mut self,
        now: Instant,
        registry: &Registry,
        token: Token,
    ) -> io::Result<io::Result<()>> {
        match self.stream.write(now) {
            Ok(true) => Ok(Ok(())),
            Ok(false) => {
                registry.reregister(self.stream.as_source(), token, Interest::READABLE)?;
                Ok(Ok(()))
            }
            Err(err) => Ok(Err(err)),
        }
    }
}

/// The state of a connection for outside observation.
pub enum Connectedness<'a, T: IntoTarget> {
    NotReady,
    Ready {
        peer: PeerId,
        connection: &'a mut Connection<T>,
    },
    New {
        peer: PeerId,
        target: T,
    },
    Errored {
        target: T,
        error: io::Error,
    },
}

/// The reason a connection is dead, for outside observation.
pub enum Dead<T: IntoTarget> {
    OutboundTimeout(T),
    WriteStale(PeerId),
}

/// Handles and tracks connections.
pub struct Manager<T: IntoTarget> {
    connections: Slab<Connection<T>>,
    token_map: hashbrown::HashMap<PeerId, Token>,
    next_peer_id: PeerId,
    dead: Vec<Dead<T>>,
    last_buffer_resize: Instant,
}

impl<T: IntoTarget> Manager<T> {
    /// Creates a new connection manager.
    pub fn new() -> Self {
        Self {
            connections: Slab::with_capacity(16),
            token_map: hashbrown::HashMap::with_capacity(16),
            next_peer_id: PeerId(0),
            dead: Vec::with_capacity(8),
            last_buffer_resize: Instant::now(),
        }
    }

    /// Returns a connection by peer id, if such a peer id is known.
    pub fn get_by_peer_id(&mut self, peer: &PeerId) -> Option<&mut Connection<T>> {
        self.token_map.get_mut(peer).map(|token| {
            self.connections
                .get_mut(token.0)
                .expect("get: token -> connection must exist")
        })
    }

    pub fn try_ready<'a>(
        &'a mut self,
        token: &Token,
        registry: &Registry,
    ) -> io::Result<Connectedness<'a, T>> {
        let connect_err = {
            let connection = self
                .connections
                .get(token.0)
                .expect("try_ready: token -> connection must exist");

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

    pub fn add_outbound(
        &mut self,
        registry: &Registry,
        mut stream: TcpStream,
        stream_cfg: StreamConfig,
        start: Instant,
        target: T,
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

    pub fn maintenance(&mut self, now: Instant) {
        if (now - self.last_buffer_resize).as_secs() > 30 {
            for (_, connection) in &mut self.connections {
                connection.stream.shrink_buffers();
            }
            self.last_buffer_resize = now;
        }
    }

    /// Removes dead connections and returns an iterator of removal reasons.
    pub fn remove_dead(
        &mut self,
        now: Instant,
        config: &Config,
        registry: &Registry,
    ) -> io::Result<impl Iterator<Item = Dead<T>>> {
        self.dead.clear();
        self.dead.shrink_to(8);
        let mut deregister_error = Ok(());

        self.connections.retain(|_, connection| {
            let retain = match &connection.direction {
                Direction::Outbound {
                    target,
                    state: State::Connecting { start },
                } if (now - *start) > config.stream_config.stream_connect_timeout => {
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
