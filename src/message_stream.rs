use std::io::{self, Read, Write};
use std::num::NonZeroU8;
use std::time::Instant;

use crate::{DecodeError, Message};

/// Stream related configuration parameters.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// Defines the minimum size of the per-connection receive buffer used for message reassembly.
    /// Low values will cause more frequent reallocation while high values will reallocate less at
    /// the expense of more memory usage. A buffer never shrinks to less than this value.
    pub rx_buf_min_size: usize,

    /// Defines the maximum size of the per-connection receive buffer used for message reassembly.
    /// This defaults to [`Message::MAX_SIZE`]. This number is very important in DoS mitigation
    /// because it prevents a malicious sender from filling up a connection's receive buffer with
    /// endless junk data.
    pub rx_buf_max_size: MaxMessageSizeMultiple,

    /// Defines the minimum size of the per-connection send buffer. Low values will cause more
    /// frequent reallocation while high values will reallocate less at the expense of more memory
    /// usage. A buffer never shrinks to less than this value.
    pub tx_buf_min_size: usize,

    /// Defines the maximum size of the per-connection send buffer. Once the send buffer is full, it
    /// is not possible to queue new messages for sending until some capacity is available. A send
    /// buffer becomes full when sending messages faster than the remote peer is reading. This value
    /// is important for outbound backpressure control. Defaults to 2 x [`Message::MAX_SIZE`].
    pub tx_buf_max_size: MaxMessageSizeMultiple,

    /// The duration after which a peer is disconnected if it fails to read our data.
    pub tx_timeout: std::time::Duration,

    /// The duration after which a connect attempt is abandoned. Applies only to non-blocking
    /// connect attempts. Blocking ones performed in custom connectors ignore this value.
    pub connect_timeout: std::time::Duration,

    /// Whether an event should be emitted every time data leaves the send buffer. This event
    /// contains information on how much data can be queued without rejection in that moment. Useful
    /// for fine-grained backpressure control.
    /// The event emitted is [`Event::Transmitted`](super::Event::Transmitted).
    pub notify_on_transmit: bool,
}

/// A deferred size expressed as a multiple of the protocol's maximum message size
/// ([`Message::MAX_SIZE`]).
///
/// This type does not store a byte length. Instead, it stores a factor that is multiplied by the
/// maximum message size defined by the protocol to obtain a concrete size at evaluation time.
#[derive(Debug, Clone, Copy)]
pub struct MaxMessageSizeMultiple(pub NonZeroU8);

impl MaxMessageSizeMultiple {
    pub fn compute<M: Message>(&self) -> usize {
        (self.0.get() as usize) * M::MAX_SIZE
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            rx_buf_min_size: 4 * 1024,
            rx_buf_max_size: MaxMessageSizeMultiple(NonZeroU8::new(1).unwrap()),
            tx_buf_min_size: 4 * 1024,
            tx_buf_max_size: MaxMessageSizeMultiple(NonZeroU8::new(2).unwrap()),
            tx_timeout: std::time::Duration::from_secs(30),
            connect_timeout: std::time::Duration::from_secs(5),
            notify_on_transmit: false,
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

// Some notes:
// The read strategy is as follows: every stream gets a fair chance to read. The most a stream can
// read in one round is the size of shared receive buffer, or the amount of space remaining in the
// receive buffer for incomplete messages, whichever is lower. This way we ensure that we never get
// overwhelmed by an aggressive sender.

impl<T: Read + Write> MessageStream<T> {
    /// Creates a new [`Stream`] instance.
    pub fn new(stream: T, config: StreamConfig) -> Self {
        Self {
            stream,
            rx_msg_buf: Vec::new(),
            tx_msg_buf: Vec::new(),
            tx_queue_points: Default::default(),
            ready: false,
            last_write: Instant::now(),
            config,
        }
    }

    /// Reads data from a stream and then attempts to decode messages from the data.
    /// Decoded messages are passed into the provided closure.
    /// Encountering an error means that the stream must be discarded.
    /// Attempts to read from a connection and decode messages in a fair manner.
    ///
    /// Returns whether there is more work available.
    #[must_use]
    pub fn read<M: Message, F: Fn(M, usize)>(
        &mut self,
        rx_buf: &mut [u8],
        on_msg: F,
    ) -> Result<bool, ReadError> {
        let preexisting = !self.rx_msg_buf.is_empty();

        // limit ourselves to reading some fixed number of bytes
        let max_buf_size = self.config.rx_buf_max_size.compute::<M>();
        let limit = (max_buf_size - self.rx_msg_buf.len()).min(rx_buf.len());

        let (total_read, read_result) = {
            let buffer = &mut rx_buf[..limit];
            let mut total_read: usize = 0;

            let result = loop {
                match self.stream.read(&mut buffer[total_read..]) {
                    // there is maybe more to read but our buffer was already full
                    Ok(0) if buffer.len() == 0 => break Ok(true),
                    // buffer was not full and we simply reached the end of stream
                    Ok(0) => break Err(ReadError::EndOfStream),
                    // regular nonzero read
                    Ok(read @ 1..) => {
                        total_read += read;
                        if total_read == buffer.len() {
                            // exceeded, maybe there is more but we need to move on
                            break Ok(true);
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break Ok(false),
                    Err(err) => break Err(ReadError::Error(err)),
                }
            };

            (total_read, result)
        };

        // for now we always decode as much as we can so we don't care about this result
        let _decode_has_more = if !preexisting {
            let (consumed, result) = decode_from_buffer(&mut rx_buf[..total_read], on_msg)?;
            if consumed < total_read {
                self.rx_msg_buf
                    .extend_from_slice(&rx_buf[consumed..total_read]);
            }
            result
        } else {
            self.rx_msg_buf.extend_from_slice(&rx_buf[..total_read]);
            let (consumed, result) = decode_from_buffer(&mut &mut self.rx_msg_buf[..], on_msg)?;
            self.rx_msg_buf.drain(..consumed);
            result
        };

        read_result
    }

    /// Writes out as many bytes from the send buffer as possible, until blocking would start or
    /// some write budget is exceeded.
    ///
    /// Returns the status of the write.
    #[must_use]
    pub fn write(&mut self, now: Instant) -> io::Result<WriteResult> {
        if !self.has_queued_data() {
            return Ok(WriteResult::Done);
        }

        let mut write_budget: usize = 128 * 1024;
        let mut syscall_budget: u8 = 8;

        loop {
            match self.try_write(now) {
                Ok(written) => {
                    let has_more = self.has_queued_data();
                    log::trace!("wrote out {written} bytes, has more: {}", has_more);

                    if !has_more {
                        break Ok(WriteResult::Done);
                    } else {
                        write_budget = write_budget.saturating_sub(written);
                        syscall_budget -= 1;
                        if write_budget == 0 || syscall_budget == 0 {
                            break Ok(WriteResult::BudgetExceeded);
                        }
                    }
                }

                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    log::trace!("write would block");
                    break Ok(WriteResult::WouldBlock);
                }

                Err(err) => break Err(err),
            }
        }
    }

    /// Queues a message for sending. This method simply encodes the message and places it into
    /// the internal buffer. [`write`] must be called when the socket is writeable in order to flush.
    ///
    /// Returns `true` if the write buffer contains enough space to accept the message, or `false`
    /// if the buffer is full and the message cannot be queued at this time.
    ///
    /// Note that if a message has a [`Message::size_hint`] implementation, the size hint is used to
    /// determine whether the message will be accepted into the send buffer. If a message does
    /// not have a size hint, two scenarios exist:
    ///   - The buffer is not full -- the message is encoded and placed into the buffer even if
    ///        that will exceed its maximum size and push it past the configured limits.
    ///   - The buffer is full -- the message will not be encoded and queued.
    #[must_use]
    pub fn queue_message<M: Message>(&mut self, message: &M) -> bool {
        let size_hint = message.size_hint().unwrap_or_default();
        if size_hint + self.tx_msg_buf.len() >= self.config.tx_buf_max_size.compute::<M>() {
            false
        } else {
            let encoded = message.encode(&mut self.tx_msg_buf);
            self.tx_queue_points.append(encoded);
            true
        }
    }

    /// Returns how many bytes can be queued immediately with respect to transmit buffer size limit.
    pub fn available<M: Message>(&self) -> usize {
        let max_size = self.config.tx_buf_max_size.compute::<M>();
        max_size - self.tx_msg_buf.len()
    }

    /// Returns whether the stream is stale on the write side, i.e. the data is not leaving the
    /// send buffer in a timely manner.
    pub fn is_write_stale(&self, now: Instant) -> bool {
        self.tx_queue_points.first().is_some_and(|t| {
            let timeout = self.config.tx_timeout;
            (now - t > self.config.tx_timeout) && (now - self.last_write > timeout)
        })
    }

    /// Resizes and shrinks the capacity of internal send and receive buffers by 1/4, until the
    /// floor is reached. This helps maintain memory usage at sane levels since keeping permanent
    /// large receive buffers (e.g. after receiving a large message) would eventually exhaust
    /// available memory on less powerful devices when managing many peers.
    pub fn shrink_buffers(&mut self) {
        fn shrink(v: &mut Vec<u8>, min: usize) {
            if v.capacity() > min {
                let shrink_to = 3 * (v.capacity() / 4);
                v.shrink_to(min.max(shrink_to));
            }
        }
        shrink(&mut self.rx_msg_buf, self.config.rx_buf_min_size);
        shrink(&mut self.tx_msg_buf, self.config.tx_buf_min_size);
        self.tx_queue_points.shrink();
    }

    /// Takes some bytes from the local send buffer and sends them. Removes successfully sent bytes
    /// from the buffer. Returns the number of bytes sent.
    fn try_write(&mut self, now: Instant) -> io::Result<usize> {
        let written = self.stream.write(&self.tx_msg_buf)?;
        self.tx_msg_buf.drain(..written);
        self.stream.flush()?;
        self.last_write = now;
        self.tx_queue_points.mark_write(written);
        Ok(written)
    }

    /// Returns whether the send buffer has more data to write.
    #[inline(always)]
    pub fn has_queued_data(&self) -> bool {
        !self.tx_msg_buf.is_empty()
    }
}

pub enum WriteResult {
    /// The write job is done and no more data remains queued.
    Done,
    /// More data remains queued but the write would block.
    WouldBlock,
    /// More data remains queued but the write budget has been exceeded.
    BudgetExceeded,
}

fn decode_from_buffer<M: Message, F: Fn(M, usize)>(
    buffer: &mut [u8],
    on_msg: F,
) -> Result<(usize, bool), ReadError> {
    let mut cursor: usize = 0;
    loop {
        match M::decode(&buffer[cursor..]) {
            Ok((message, consumed)) => {
                cursor += consumed;
                on_msg(message, consumed);
            }
            Err(DecodeError::NotEnoughData) => {
                break Ok((cursor, false)); // not ready in the next round as far as we know
            }
            Err(DecodeError::MalformedMessage) => {
                break Err(ReadError::MalformedMessage);
            }
        }
    }
}

impl MessageStream<mio::net::TcpStream> {
    /// Returns `true` if the underlying stream is ready. Otherwise it tests readiness and
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

    pub fn take_error(&self) -> Option<io::Error> {
        self.stream.take_error().ok().flatten()
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
    use std::collections::VecDeque;
    use std::time::Instant;

    /// A single queue point given a point in time and the remaining number of bytes.
    #[derive(Debug)]
    struct Point {
        time: Instant,
        left: usize,
    }

    /// A list of queue points, from oldest to newest.
    #[derive(Debug, Default)]
    pub struct Queue(VecDeque<Point>);

    impl Queue {
        /// Handles a write event.
        pub fn mark_write(&mut self, n_written: usize) {
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
            self.0.push_back(Point {
                time: Instant::now(),
                left: size,
            })
        }

        /// Returns the creation instant of the first queue point, if any.
        pub fn first(&self) -> Option<Instant> {
            self.0.front().map(|p| p.time)
        }

        /// Shrinks the capacity of the queue by 1/4, floored at 8.
        pub fn shrink(&mut self) {
            if self.0.capacity() > 8 {
                self.0.shrink_to(8.max(3 * (self.0.capacity() / 4)));
            }
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

        queue.mark_write(5);
        assert_eq!(queue.0[0].left, 5);
        assert_eq!(queue.0[1].left, 20);
        assert_eq!(queue.0[2].left, 30);

        queue.mark_write(5);
        assert_eq!(queue.0[0].left, 20);
        assert_eq!(queue.0[1].left, 30);

        queue.mark_write(25);
        assert_eq!(queue.0[0].left, 25);
        assert_eq!(queue.0.len(), 1);

        queue.mark_write(25);
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
        const MAX_SIZE: usize = 8;

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
        conn.read(&mut buf, |message, size| {
            assert_eq!(size, 8);
            received.borrow_mut().push(message);
        })
        .unwrap();

        assert_eq!(received.borrow()[0], Ping(0));

        conn.read(&mut buf, |message, size| {
            assert_eq!(size, 8);
            received.borrow_mut().push(message);
        })
        .unwrap();
        assert_eq!(received.borrow()[1], Ping(1));

        let err = conn.read(&mut buf, |message, size| {
            assert_eq!(size, 8);
            received.borrow_mut().push(message);
        });
        assert!(matches!(err, Err(ReadError::EndOfStream)));
        assert_eq!(conn.stream.position(), 16);
        assert!(conn.rx_msg_buf.is_empty());
    }

    #[test]
    fn reassemble_message_partial_reads() {
        let mut buf = [0; 8];
        let mut cursor = Cursor::new(Vec::new());
        let mut conn = MessageStream::new(&mut cursor, StreamConfig::default());
        let mut serialized = Vec::new();
        Ping(u64::MAX - 1).encode(&mut serialized);
        Ping(u64::MAX).encode(&mut serialized);

        let received: RefCell<Vec<Ping>> = Default::default();

        conn.stream.get_mut().extend_from_slice(&serialized[..4]);
        let _ = conn.read(&mut buf, |message, size| {
            assert_eq!(size, 8);
            received.borrow_mut().push(message);
        });
        assert!(received.borrow().is_empty());
        assert_eq!(conn.rx_msg_buf.len(), 4);

        conn.stream.get_mut().extend_from_slice(&serialized[4..]);
        let _ = conn.read(&mut buf, |message, size| {
            assert_eq!(size, 8);
            received.borrow_mut().push(message);
        });
        assert_eq!(received.borrow()[0], Ping(u64::MAX - 1));

        let _ = conn.read(&mut buf, |message, size| {
            assert_eq!(size, 8);
            received.borrow_mut().push(message);
        });
        assert_eq!(received.borrow()[1], Ping(u64::MAX));
    }

    #[test]
    fn send_message() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let mut connection = MessageStream::new(
            &mut wire,
            StreamConfig {
                tx_buf_max_size: MaxMessageSizeMultiple(3.try_into().unwrap()),
                ..Default::default()
            },
        );

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
            tx_buf_max_size: MaxMessageSizeMultiple(1.try_into().unwrap()),
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

    #[test]
    fn outbound_buffer_availability() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let config = StreamConfig {
            tx_buf_min_size: 1,
            tx_buf_max_size: MaxMessageSizeMultiple(1.try_into().unwrap()),
            ..Default::default()
        };
        let mut connection = MessageStream::new(&mut wire, config);

        assert!(connection.queue_message(&Ping(0)));
        assert!(!connection.queue_message(&Ping(1)));

        assert_eq!(connection.available::<Ping>(), 0);
        connection.write(Instant::now()).unwrap();
        assert_eq!(connection.available::<Ping>(), 8);
    }
}
