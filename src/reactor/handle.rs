use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;

use mio::Waker;

use crate::reactor::SystemCommand;
use crate::{Command, Event, Message, Termination};

#[cfg(not(feature = "async"))]
use crossbeam_channel::{Receiver, Sender};

#[cfg(feature = "async")]
use async_channel::{Receiver, Sender};

/// Provides bidirectional communication with a reactor. If this is dropped the reactor stops.
pub struct Handle<M: Message> {
    waker: Arc<Waker>,
    sender: Sender<SystemCommand<M>>,
    receiver: Receiver<Event<M>>,
    join_handle: Option<std::thread::JoinHandle<io::Result<()>>>,
}

impl<M: Message> Handle<M> {
    /// Sends a command to a reactor associated with this handle. Never blocks.
    pub fn send(&self, command: Command<M>) -> Result<(), SendError> {
        self.send_inner(SystemCommand::P2P(command))
    }

    /// Blocks until the reactor associated with this handle produces a message. If an error is
    /// produced, the reactor is irrecoverable.
    pub fn recv_blocking(&self) -> Result<Event<M>, RecvError> {
        #[cfg(not(feature = "async"))]
        let result = self.receiver.recv();
        #[cfg(feature = "async")]
        let result = self.receiver.recv_blocking();

        result.map_err(|_| RecvError)
    }

    /// Attempts to immediately receive a message. If the inner option is `None`, it means the
    /// channel was empty. If there is a `RecvError`, the reactor is irrecoverable.
    pub fn try_recv(&self) -> Result<Option<Event<M>>, RecvError> {
        #[cfg(feature = "async")]
        use async_channel::TryRecvError;
        #[cfg(not(feature = "async"))]
        use crossbeam_channel::TryRecvError;

        match self.receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            #[cfg(feature = "async")]
            Err(TryRecvError::Closed) => Err(RecvError),
            #[cfg(not(feature = "async"))]
            Err(TryRecvError::Disconnected) => Err(RecvError),
        }
    }

    #[cfg(feature = "async")]
    pub async fn recv_async(&self) -> Result<Event<M>, RecvError> {
        self.receiver.recv().await.map_err(|_| RecvError)
    }

    #[cfg(not(feature = "async"))]
    pub async fn recv_timeout(
        &self,
        duration: std::time::Duration,
    ) -> Result<Option<Event<M>>, RecvError> {
        match self.receiver.recv_timeout(duration) {
            Ok(event) => Ok(Some(event)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(None),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(RecvError),
        }
    }

    /// Returns the number of events in the receive channel.
    pub fn event_count(&self) -> usize {
        self.receiver.len()
    }

    /// Shuts down the reactor and consumes the handle. No further commands can be sent afterward.
    /// Returns the join handle to the thread running the reactor, as well as whether the shutdown
    /// command was received. The latter erroring out means that the shutdown likely didn't go well.
    pub fn shutdown(
        mut self,
        termination: Termination,
    ) -> (JoinHandle<io::Result<()>>, Result<(), SendError>) {
        let send_result = self.send_inner(SystemCommand::Shutdown(termination));

        (
            self.join_handle.take().expect("always present"),
            send_result,
        )
    }

    /// Utility function for sending system commands to the reactor. Not part of the public API.
    fn send_inner(&self, command: SystemCommand<M>) -> Result<(), SendError> {
        #[cfg(not(feature = "async"))]
        let result = self.sender.try_send(command);
        #[cfg(feature = "async")]
        let result = self.sender.try_send(command);

        match result {
            Ok(()) => self.waker.wake().map_err(|e| {
                log::error!("handle: waker failure: {}", e);
                SendError::Dead
            }),
            Err(e) if e.is_full() => Err(SendError::Full),
            Err(e) => {
                log::error!("handle: send failure: {}", e);
                Err(SendError::Dead)
            }
        }
    }
}

impl<M: Message> Drop for Handle<M> {
    fn drop(&mut self) {
        #[cfg(not(feature = "async"))]
        let _ = self
            .sender
            .send(SystemCommand::Shutdown(Termination::Immediate));
        #[cfg(feature = "async")]
        let _ = self
            .sender
            .try_send(SystemCommand::Shutdown(Termination::Immediate));

        let _ = self.waker.wake();
    }
}

pub struct IdleHandle<M: Message> {
    pub waker: Arc<Waker>,
    pub sender: Sender<SystemCommand<M>>,
    pub receiver: Receiver<Event<M>>,
}

impl<M: Message> IdleHandle<M> {
    pub fn into_running(self, join_handle: std::thread::JoinHandle<io::Result<()>>) -> Handle<M> {
        Handle {
            waker: self.waker,
            sender: self.sender,
            receiver: self.receiver,
            join_handle: Some(join_handle),
        }
    }
}

#[derive(Debug)]
pub enum SendError {
    /// The channel to the reactor is currently full and cannot accept more messages.
    Full,
    /// The other side of the channel is disconnected (reactor is likely dead).
    Dead,
}

#[derive(Debug)]
pub struct RecvError;
