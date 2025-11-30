use std::io;
use std::time::Duration;

use mio::{Events, Poll, Token, Waker};

/// Reserved token used for waking the event loop.
pub const WAKER: Token = Token(usize::MAX);

/// MIO poll interval
const POLL_INTERVAL: Option<Duration> = Some(Duration::from_secs(1));

/// A resource readiness scheduler.
pub struct Scheduler {
    events: Events,
    has_waker: bool,
    listeners: Vec<Token>,
    carryover: hashbrown::HashMap<Token, Readiness>,
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
        poll.poll(&mut self.events, POLL_INTERVAL)?;

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
                token => panic!("unknown/unhandled token: {token:?} "),
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

    /// Iterates over ready connections. Takes a closure that provides the
    /// token and readiness for each connection. The readiness is mutable
    /// and its read and write fields must be set correctly upon being handled.
    /// If both are set to false, that readiness event is removed. Otherwise,
    /// it is carried over into the next round.
    pub fn connections<F: FnMut(Token, &mut Readiness) -> io::Result<()>>(
        &mut self,
        iteration: u64,
        mut f: F,
    ) -> io::Result<()> {
        let prev_carryover = !self.carryover.is_empty();

        for event in self
            .events
            .iter()
            .filter(|e| is_connection(self.n_listeners, e.token()))
        {
            let mut readiness = if prev_carryover    // - if carryover existed before we started
                && !self.carryover.is_empty()        // - if there is any carryover left now
                && let Some(carryover) = self.carryover.remove(&event.token())
            {
                carryover.update(event, iteration)
            } else {
                Readiness {
                    read: event.is_readable(),
                    write: event.is_writable(),
                    handled_in_iter: iteration,
                }
            };

            f(event.token(), &mut readiness)?;
            if readiness.has_more() {
                self.carryover.insert(event.token().into(), readiness);
            }
        }

        let mut result = Ok(());
        if !self.carryover.is_empty() {
            self.carryover.retain(|token, readiness| {
                if readiness.handled_in_iter != iteration {
                    if let Err(err) = f(*token, readiness) {
                        result = Err(err);
                    }
                    readiness.has_more()
                } else {
                    true
                }
            });
        }

        result
    }

    /// Based on whether there is carryover readiness, maybe rearm the waker in
    /// order to quickly move on to the next iteration.
    pub fn maybe_rearm(&mut self, waker: &Waker) -> io::Result<()> {
        if !self.carryover.is_empty() {
            waker.wake()
        } else {
            Ok(())
        }
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

pub struct Readiness {
    pub read: bool,
    pub write: bool,
    handled_in_iter: u64,
}

impl Readiness {
    #[inline(always)]
    fn update(mut self, event: &mio::event::Event, iteration: u64) -> Self {
        self.read = self.read || event.is_readable();
        self.write = self.write || event.is_writable();
        self.handled_in_iter = iteration;
        self
    }

    #[inline(always)]
    fn has_more(&self) -> bool {
        self.read || self.write
    }

    #[inline(always)]
    pub fn complete(&mut self) {
        self.read = false;
        self.write = false;
    }
}

#[cfg(test)]
mod test {
    use super::{WAKER, is_connection, is_listener};
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
