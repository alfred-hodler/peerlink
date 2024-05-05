use std::io::{self, Read, Write};
use std::time::Instant;

use crate::{DecodeError, Message};

/// Stream related configuration parameters.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// Defines the minimum size of the buffer used for message reassembly. Low values will cause
    /// more frequent reallocation while high values will reallocate less at the expense of more
    /// memory usage.
    pub rx_buf_min_size: usize,
    /// Defines the minimum size of the buffer used for outbound data. Low values will cause
    /// more frequent reallocation while high values will reallocate less at the expense of more
    /// memory usage.
    pub tx_buf_min_size: usize,
    /// Defines the maximum capacity of the send buffer. Once the send buffer is full,
    /// it is not possible to queue new messages for sending until some capacity is available.
    /// A send buffer becomes full when sending messages faster than the remote peer is reading.
    pub tx_buf_max_size: usize,
    /// The duration after which a peer is disconnected if it fails to read incoming data.
    pub stream_write_timeout: std::time::Duration,
    /// The duration after which a connect attempt is abandoned. Applies only to non-blocking
    /// connect attempts. Blocking ones performed in custom connectors ignore this value.
    pub stream_connect_timeout: std::time::Duration,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            rx_buf_min_size: 32 * 1024,
            tx_buf_min_size: 32 * 1024,
            tx_buf_max_size: 1024 * 1024,
            stream_write_timeout: std::time::Duration::from_secs(30),
            stream_connect_timeout: std::time::Duration::from_secs(5),
        }
    }
}

/// Wraps read and write parts of a peer connection and does message buffering and assembly
/// on the read side and message serialization and flushing on the write side.
#[derive(Debug)]
pub struct MessageStream<T: Read + Write> {
    /// Configuration parameters for the stream.
    config: StreamConfig,
    /// The read+write stream underlying the connection.
    stream: T,
    /// Buffer used for message reconstruction.
    rx_msg_buf: Vec<u8>,
    /// Buffer used for sending.
    tx_msg_buf: Vec<u8>,
    /// The list of queue points for outgoing messages.
    tx_queue_points: queue_points::Queue,
    /// Cached readyness.
    ready: bool,
    /// Last successful write time.
    last_write: Instant,
}

#[derive(Debug)]
pub enum ReadError {
    /// A malformed message was received.
    MalformedMessage,
    /// End of stream has been reached (closed stream).
    EndOfStream,
    /// The stream produced an I/O error.
    Error(io::Error),
}

impl<T: Read + Write> MessageStream<T> {
    pub fn new(stream: T, config: StreamConfig) -> Self {
        Self {
            stream,
            rx_msg_buf: Vec::new(),
            tx_msg_buf: Vec::with_capacity(config.tx_buf_min_size),
            tx_queue_points: Default::default(),
            ready: false,
            last_write: Instant::now(),
            config,
        }
    }

    /// Receives as many messages as possible, either until reads would start blocking, or until
    /// an error is encountered. Encountering an error means that the stream should be discarded.
    pub fn read<M: Message, F: Fn(M)>(
        &mut self,
        rx_buf: &mut [u8],
        on_msg: F,
    ) -> Result<(), ReadError> {
        'read: loop {
            match self.stream.read(rx_buf).map(|read| &rx_buf[..read]) {
                Ok(&[]) => break 'read Err(ReadError::EndOfStream),

                Ok(received) => {
                    if !self.rx_msg_buf.is_empty() {
                        self.rx_msg_buf.extend_from_slice(received);
                        'decode: loop {
                            if !self.rx_msg_buf.is_empty() {
                                match M::decode(&self.rx_msg_buf) {
                                    Ok((message, consumed)) => {
                                        self.rx_msg_buf.drain(..consumed);
                                        on_msg(message);
                                    }
                                    Err(DecodeError::NotEnoughData) => break 'decode,
                                    Err(DecodeError::MalformedMessage) => {
                                        break 'read Err(ReadError::MalformedMessage)
                                    }
                                }
                            } else {
                                break 'decode;
                            }
                        }
                    } else {
                        let mut next_from = 0;
                        'decode: loop {
                            let next = &received[next_from..];
                            if !next.is_empty() {
                                match M::decode(next) {
                                    Ok((message, consumed)) => {
                                        on_msg(message);
                                        next_from += consumed;
                                    }
                                    Err(DecodeError::NotEnoughData) => {
                                        if self.rx_msg_buf.capacity() == 0 {
                                            self.rx_msg_buf
                                                .reserve_exact(self.config.rx_buf_min_size);
                                        }
                                        self.rx_msg_buf.extend_from_slice(next);
                                        break 'decode;
                                    }
                                    Err(DecodeError::MalformedMessage) => {
                                        break 'read Err(ReadError::MalformedMessage);
                                    }
                                }
                            } else {
                                break 'decode;
                            }
                        }
                    }
                }

                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break 'read Ok(()),

                Err(err) => break 'read Err(ReadError::Error(err)),
            }
        }
    }

    /// Writes out as many bytes from the send buffer as possible, until blocking would start.
    pub fn write(&mut self, now: Instant) -> io::Result<()> {
        if !self.has_queued_data() {
            return Ok(());
        }

        loop {
            match self.attempt_write(now) {
                Ok(written) => {
                    let has_more = self.has_queued_data();
                    log::trace!("wrote out {written} bytes, has more: {}", has_more);

                    if !has_more {
                        break Ok(());
                    }
                }

                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    log::trace!("write would block");
                    break Ok(());
                }

                Err(err) => break Err(err),
            }
        }
    }
    /// Queues a message for sending. This method simply serializes the message and places it into
    /// the internal buffer. `write` must be called when the socket is writeable in order to flush.
    ///
    /// Returns `true` if the write buffer contains enough space to accept the message, or `false`
    /// if the buffer is full and the message cannot be queued at this time.
    ///
    /// Note: this will fail only if the buffer is full prior to even attempting to queue. A buffer
    /// that is close to full will not reject a message, even if queueing might exceed the
    /// configured limits.
    #[must_use]
    pub fn queue_message<M: Message>(&mut self, message: &M) -> bool {
        if self.tx_msg_buf.len() <= self.config.tx_buf_max_size {
            let encoded = message.encode(&mut self.tx_msg_buf);
            self.tx_queue_points.append(encoded);
            true
        } else {
            false
        }
    }

    /// Returns whether the stream is stale on the write side, i.e. the data is not leaving the
    /// send buffer in a timely manner.
    pub fn is_write_stale(&self, now: Instant) -> bool {
        match self.tx_queue_points.first() {
            Some(t) => {
                let timeout = self.config.stream_write_timeout;
                (now - t > timeout) && (now - self.last_write > timeout)
            }
            None => false,
        }
    }

    /// Resizes and shrinks the capacity of internal send and receive buffers by 1/3, until the
    /// floor is reached. This helps maintain memory usage at sane levels since keeping permanent
    /// large receive buffers (e.g. after receiving a large message) would eventually exhaust
    /// available memory on less powerful devices when managing many peers.
    pub fn shrink_buffers(&mut self) {
        fn shrink(v: &mut Vec<u8>, min: usize) {
            if v.capacity() > min {
                let shrink_to = (2 * v.capacity()) / 3;
                v.shrink_to(min.max(shrink_to));
            }
        }
        shrink(&mut self.rx_msg_buf, self.config.rx_buf_min_size);
        shrink(&mut self.tx_msg_buf, self.config.tx_buf_min_size);
        self.tx_queue_points.shrink();
    }

    /// Determines the interest set wanted by the connection.
    pub fn interest(&self) -> mio::Interest {
        if self.has_queued_data() {
            mio::Interest::READABLE | mio::Interest::WRITABLE
        } else {
            mio::Interest::READABLE
        }
    }

    /// Takes some bytes from the local send buffer and sends them. Removes successfully sent bytes
    /// from the buffer. Returns the number of bytes sent.
    fn attempt_write(&mut self, now: Instant) -> io::Result<usize> {
        let written = self.stream.write(&self.tx_msg_buf)?;
        self.tx_msg_buf.drain(..written);
        self.stream.flush()?;
        self.last_write = now;
        self.tx_queue_points.handle_write(written);
        Ok(written)
    }

    /// Returns whether the send buffer has more data to write.
    #[inline(always)]
    fn has_queued_data(&self) -> bool {
        !self.tx_msg_buf.is_empty()
    }
}

impl MessageStream<mio::net::TcpStream> {
    /// Returns `true` if the underlying stream is ready. Otherwise it tests readyness and
    /// caches the result.
    pub fn is_ready(&mut self) -> bool {
        if !self.ready {
            self.ready = self.stream.peer_addr().is_ok();
        }
        self.ready
    }
    /// Shuts down the underlying stream.
    pub fn shutdown(self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)
    }

    /// Returns the underlying stream as a Mio event source.
    pub fn as_source(&mut self) -> &mut impl mio::event::Source {
        &mut self.stream
    }
}

/// Provides a collection that tracks points in time where a message of certain size was queued.
/// This allows the consumer to track how long ago a message was attempted to be sent out and how
/// many bytes are yet to be sent.
mod queue_points {
    use std::time::Instant;

    /// A single queue point given a point in time and the remaining number of bytes.
    #[derive(Debug)]
    struct Point {
        time: Instant,
        left: usize,
    }

    /// A list of queue points, from oldest to newest.
    #[derive(Debug, Default)]
    pub struct Queue(Vec<Point>);

    impl Queue {
        /// Signals to the current queue point that a number of bytes were written out.
        pub fn handle_write(&mut self, n_written: usize) {
            let mut n_bytes_left = n_written;
            let mut n_pop = 0;

            for q in &mut self.0 {
                let q_written = n_bytes_left.min(q.left);
                n_bytes_left -= q_written;
                q.left -= q_written;

                if q.left == 0 {
                    n_pop += 1;
                }

                if n_bytes_left == 0 {
                    break;
                }
            }

            assert_eq!(n_bytes_left, 0);
            self.0.drain(..n_pop);
        }

        /// Appends a new queue point of a certain size.
        pub fn append(&mut self, size: usize) {
            self.0.push(Point {
                time: Instant::now(),
                left: size,
            })
        }

        /// Returns the creation instant of the first queue point, if any.
        pub fn first(&self) -> Option<Instant> {
            self.0.first().map(|p| p.time)
        }

        /// Shrinks the capacity of the queue by 1/3, floored at 8 or current size.
        pub fn shrink(&mut self) {
            self.0.shrink_to(8.max((2 * self.0.capacity()) / 3));
        }
    }

    #[cfg(test)]
    #[test]
    fn queue_behavior() {
        let mut queue = Queue::default();

        queue.append(10);
        queue.append(20);
        queue.append(30);

        assert_eq!(queue.0[0].left, 10);
        assert_eq!(queue.0[1].left, 20);
        assert_eq!(queue.0[2].left, 30);

        queue.handle_write(5);
        assert_eq!(queue.0[0].left, 5);
        assert_eq!(queue.0[1].left, 20);
        assert_eq!(queue.0[2].left, 30);

        queue.handle_write(5);
        assert_eq!(queue.0[0].left, 20);
        assert_eq!(queue.0[1].left, 30);

        queue.handle_write(25);
        assert_eq!(queue.0[0].left, 25);
        assert_eq!(queue.0.len(), 1);

        queue.handle_write(25);
        assert!(queue.first().is_none());
    }
}

#[cfg(test)]
mod test {
    use std::cell::RefCell;
    use std::io::Cursor;

    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct Ping(u64);

    impl Message for Ping {
        fn encode(&self, dest: &mut impl std::io::Write) -> usize {
            dest.write(&self.0.to_le_bytes()).unwrap()
        }

        fn decode(buffer: &[u8]) -> Result<(Self, usize), DecodeError> {
            if buffer.len() >= 8 {
                Ok((Ping(u64::from_le_bytes(buffer[..8].try_into().unwrap())), 8))
            } else {
                Err(DecodeError::NotEnoughData)
            }
        }
    }

    #[test]
    fn reassemble_message_whole_reads() {
        let mut buf = [0; 1024];
        let mut cursor = Cursor::new(Vec::new());

        Ping(0).encode(&mut cursor);
        Ping(1).encode(&mut cursor);
        cursor.set_position(0);

        let mut conn = MessageStream::new(&mut cursor, StreamConfig::default());

        let received: RefCell<Vec<Ping>> = Default::default();
        let err = conn.read(&mut buf, |message| {
            received.borrow_mut().push(message);
        });

        assert_eq!(received.borrow()[0], Ping(0));
        assert_eq!(received.borrow()[1], Ping(1));
        assert!(matches!(err, Err(ReadError::EndOfStream)));
        assert_eq!(conn.stream.position(), 16);
        assert!(conn.rx_msg_buf.is_empty());
    }

    #[test]
    fn reassemble_message_partial_reads() {
        let mut buf = [0; 1024];
        let mut cursor = Cursor::new(Vec::new());
        let mut conn = MessageStream::new(&mut cursor, StreamConfig::default());
        let mut serialized = Vec::new();
        Ping(u64::MAX - 1).encode(&mut serialized);
        Ping(u64::MAX).encode(&mut serialized);

        let received: RefCell<Vec<Ping>> = Default::default();

        conn.stream.get_mut().extend_from_slice(&serialized[..4]);
        let _ = conn.read(&mut buf, |message| {
            received.borrow_mut().push(message);
        });
        assert!(received.borrow().is_empty());
        assert_eq!(conn.rx_msg_buf.len(), 4);

        conn.stream.get_mut().extend_from_slice(&serialized[4..]);
        let _ = conn.read(&mut buf, |message| {
            received.borrow_mut().push(message);
        });
        assert_eq!(received.borrow()[0], Ping(u64::MAX - 1));
        assert_eq!(received.borrow()[1], Ping(u64::MAX));
    }

    #[test]
    fn send_message() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let mut connection = MessageStream::new(&mut wire, StreamConfig::default());

        assert!(connection.queue_message(&Ping(0)));
        assert!(connection.queue_message(&Ping(1)));
        assert!(connection.queue_message(&Ping(2)));

        let cloned_buffer = connection.tx_msg_buf.clone();
        connection.write(Instant::now()).unwrap();
        assert_eq!(wire.position(), 24);
        assert_eq!(wire.into_inner(), cloned_buffer);
    }

    #[test]
    fn send_message_buf_full() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let config = StreamConfig {
            tx_buf_min_size: 1,
            tx_buf_max_size: 7,
            ..Default::default()
        };
        let mut connection = MessageStream::new(&mut wire, config);

        assert!(connection.queue_message(&Ping(0)));
        assert!(!connection.queue_message(&Ping(1)));

        let buffer_len = connection.tx_msg_buf.len();
        let cloned_buffer = connection.tx_msg_buf.clone();
        connection.write(Instant::now()).unwrap();
        assert_eq!(wire.position(), buffer_len as u64);
        assert_eq!(wire.into_inner(), cloned_buffer);
    }
}
