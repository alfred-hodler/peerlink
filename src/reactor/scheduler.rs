use std::io;
use std::time::Duration;

use mio::{Events, Poll, Token};
use rustc_hash::FxHashMap;

/// Reserved token used for waking the event loop.
pub const WAKER: Token = Token(usize::MAX);

/// MIO poll interval.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A resource readiness scheduler.
pub struct Scheduler {
    events: Events,
    has_waker: bool,
    listeners: Vec<Token>,
    carryover: FxHashMap<Token, readiness::Readiness>,
    n_listeners: usize,
}

impl Scheduler {
    /// Creates a new scheduler.
    pub fn new(n_listeners: usize) -> Self {
        Self {
            events: Events::with_capacity(1024),
            has_waker: false,
            listeners: Vec::with_capacity(n_listeners),
            carryover: Default::default(),
            n_listeners,
        }
    }

    /// Updates internal resource readiness.
    pub fn update(&mut self, poll: &mut Poll) -> io::Result<()> {
        let interval = if !self.carryover.is_empty() {
            Duration::ZERO
        } else {
            POLL_INTERVAL
        };

        poll.poll(&mut self.events, Some(interval))?;

        for event in &self.events {
            match event.token() {
                WAKER => {
                    self.has_waker = true;
                }
                token if is_listener(self.n_listeners, token) => {
                    assert!(self.listeners.len() <= self.n_listeners);
                    self.listeners.push(token);
                }
                token if is_connection(self.n_listeners, token) => {}
                token => unreachable!("unknown/unhandled token: {token:?} "),
            }
        }

        Ok(())
    }

    /// Drains waker readiness.
    pub fn waker(&mut self) -> bool {
        let result = self.has_waker;
        self.has_waker = false;
        result
    }

    /// Drains listener readiness.
    pub fn listeners(&mut self) -> impl Iterator<Item = Token> {
        self.listeners.drain(..)
    }

    /// Iterates over ready connections. Takes a closure that provides the token, the read and write
    /// readiness respectively and whether the readiness is a pure carryover for each connection.
    pub fn connections<F: FnMut(Token, bool, bool, bool) -> io::Result<Carryover>>(
        &mut self,
        round: u64,
        mut f: F,
    ) -> io::Result<()> {
        use readiness::Readiness;
        use std::collections::hash_map::Entry;

        for event in self
            .events
            .iter()
            .filter(|e| is_connection(self.n_listeners, e.token()))
        {
            match self.carryover.entry(event.token()) {
                Entry::Occupied(mut entry) => {
                    let readiness = entry.get_mut();
                    readiness.update(round, Some(event));
                    let carryover = f(
                        event.token(),
                        readiness.drain_readable(),
                        readiness.drain_writable(),
                        false,
                    )?;

                    if !readiness.will_carryover(carryover) {
                        entry.remove();
                    }
                }
                Entry::Vacant(vacancy) => {
                    let mut readiness = Readiness::new(event, round);
                    let carryover = f(
                        event.token(),
                        readiness.drain_readable(),
                        readiness.drain_writable(),
                        false,
                    )?;

                    if readiness.will_carryover(carryover) {
                        vacancy.insert(readiness);
                    }
                }
            }
        }

        let mut result = Ok(());
        if !self.carryover.is_empty() {
            self.carryover.retain(|token, readiness| {
                if readiness.is_handled(round - 1) {
                    // carryover standalone readiness not associated with the current poll
                    // never used in this round
                    readiness.update(round, None);
                    match f(
                        *token,
                        readiness.drain_readable(),
                        readiness.drain_writable(),
                        true,
                    ) {
                        Ok(carryover) => readiness.will_carryover(carryover),
                        Err(err) => {
                            result = Err(err);
                            false
                        }
                    }
                } else if readiness.is_handled(round) {
                    // handled in the current poll already
                    true
                } else {
                    unreachable!(
                        "readiness mismatch: current round is {}, found {:?}",
                        round, readiness
                    )
                }
            });
        }

        result
    }
}

/// Checks if a token is associated with the server (connection listener).
#[inline(always)]
fn is_listener(n_listeners: usize, token: Token) -> bool {
    token.0 >= usize::MAX - n_listeners && token.0 < usize::MAX
}

/// Checks if a token is associated with the server (connection listener).
#[inline(always)]
fn is_connection(n_listeners: usize, token: Token) -> bool {
    token.0 < usize::MAX - n_listeners
}

/// Conveys read and write carryover.
#[derive(Debug, Default)]
pub struct Carryover {
    pub r: bool,
    pub w: bool,
}

impl Carryover {
    /// Creates a new, empty carryover.
    pub fn none() -> Self {
        Self { r: false, w: false }
    }
}

mod readiness {
    use super::Carryover;

    #[derive(Debug)]
    pub struct Readiness {
        ready: Carryover,
        used: Carryover,
        processed_in_round: u64,
    }

    impl Readiness {
        /// Creates a new readiness from a `mio` event and a round.
        pub fn new(event: &mio::event::Event, round: u64) -> Self {
            Self {
                ready: Carryover {
                    r: event.is_readable(),
                    w: event.is_writable(),
                },
                used: Carryover::default(),
                processed_in_round: round,
            }
        }

        /// Upates this readiness with a new round and a possible `mio` event, if any.
        pub fn update(&mut self, round: u64, event: Option<&mio::event::Event>) -> &mut Self {
            if self.processed_in_round != round {
                self.processed_in_round = round;
                self.used = Carryover::default();
            }

            if let Some(event) = event {
                self.ready.r |= event.is_readable() && !self.used.r;
                self.ready.w |= event.is_writable() && !self.used.w;
            }

            self
        }

        /// Drains and returns readable readiness, if any.
        pub fn drain_readable(&mut self) -> bool {
            if self.ready.r && !self.used.r {
                self.ready.r = false;
                self.used.r = true;
                true
            } else {
                false
            }
        }

        /// Drains and returns writeable readiness, if any.
        pub fn drain_writable(&mut self) -> bool {
            if self.ready.w && !self.used.w {
                self.ready.w = false;
                self.used.w = true;
                true
            } else {
                false
            }
        }

        /// Updates this readiness with a carryover and returns whether the readiness will carry
        /// over into the next round.
        pub fn will_carryover(&mut self, carryover: Carryover) -> bool {
            self.ready.r = carryover.r;
            self.ready.w = carryover.w;
            self.ready.r || self.ready.w
        }

        /// Returns whether the readiness was handled in a certain round.
        pub fn is_handled(&self, round: u64) -> bool {
            self.processed_in_round == round
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use mio::Token;

    #[test]
    fn is_listener_test() {
        assert!(!is_listener(0, WAKER));
        assert!(!is_listener(0, Token(WAKER.0 - 1)));
        assert!(!is_listener(0, Token(usize::MIN)));

        assert!(!is_listener(1, WAKER));
        assert!(is_listener(1, Token(WAKER.0 - 1)));
        assert!(!is_listener(1, Token(WAKER.0 - 2)));

        assert!(!is_listener(3, WAKER));
        assert!(is_listener(3, Token(WAKER.0 - 1)));
        assert!(is_listener(3, Token(WAKER.0 - 2)));
        assert!(is_listener(3, Token(WAKER.0 - 3)));
        assert!(!is_listener(3, Token(WAKER.0 - 4)));
    }

    #[test]
    fn is_connection_test() {
        assert!(!is_connection(0, WAKER));
        assert!(is_connection(0, Token(WAKER.0 - 1)));
        assert!(is_connection(0, Token(usize::MIN)));

        assert!(!is_connection(1, WAKER));
        assert!(!is_connection(1, Token(WAKER.0 - 1)));
        assert!(is_connection(1, Token(WAKER.0 - 2)));

        assert!(!is_connection(3, WAKER));
        assert!(!is_connection(3, Token(WAKER.0 - 1)));
        assert!(!is_connection(3, Token(WAKER.0 - 2)));
        assert!(!is_connection(3, Token(WAKER.0 - 3)));
        assert!(is_connection(3, Token(WAKER.0 - 4)));
    }
}
