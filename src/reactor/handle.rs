use std::io;
use std::sync::Arc;

use mio::Waker;

use crate::reactor::SystemCommand;
use crate::{Command, Event, Message};

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
    /// Sends a command to a reactor associated with this handle. If this produces an IO error,
    /// it means the reactor is irrecoverable and should be discarded. This method never blocks so
    /// it is appropriate for use in async contexts.
    pub fn send(&self, command: Command<M>) -> io::Result<()> {
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
    pub fn receive_blocking(&self) -> io::Result<Event<M>> {
        #[cfg(not(feature = "async"))]
        let result = self.receiver.recv();
        #[cfg(feature = "async")]
        let result = self.receiver.recv_blocking();

        result.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "channel disconnected"))
    }

    /// Exposes the receive portion of the handle. The receiver could be of a blocking or async
    /// variety, depending on which feature is active.
    pub fn receiver(&self) -> &Receiver<Event<M>> {
        &self.receiver
    }

    /// Shuts down the reactor and consumes the handle. No further commands can be sent afterward.
    pub fn shutdown(mut self) -> io::Result<std::thread::JoinHandle<io::Result<()>>> {
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
        }?;

        Ok(self.join_handle.take().expect("exists at this point"))
    }
}

impl<M: Message> Drop for Handle<M> {
    fn drop(&mut self) {
        #[cfg(not(feature = "async"))]
        let _ = self.sender.send(SystemCommand::Shutdown);
        #[cfg(feature = "async")]
        let _ = self.sender.try_send(SystemCommand::Shutdown);

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
