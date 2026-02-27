use std::io::{self, Read, Write};
use std::num::NonZeroU8;
use std::time::{Duration, Instant};

use bytes::Buf;

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

    /// The duration after which a connect attempt is abandoned. Applies only to non-blocking
    /// connect attempts. Blocking ones performed in custom connectors ignore this value.
    pub connect_timeout: std::time::Duration,

    /// Whether an event should be emitted every time data enters and leaves the send buffer for the
    /// peer. Useful for fine grained flow control and backpressure control. The event emitted is
    /// [`Event::OutboundTelemetry`](super::Event::OutboundTelemetry).
    pub outbound_telemetry: bool,
}

/// A deferred size expressed as a multiple of the protocol's maximum message size
/// ([`Message::MAX_SIZE`]).
///
/// This type does not store a byte length. Instead, it stores a factor that is multiplied by the
/// maximum message size defined by the protocol to obtain a concrete size at evaluation time.
#[derive(Debug, Clone, Copy)]
pub struct MaxMessageSizeMultiple(pub NonZeroU8);

impl MaxMessageSizeMultiple {
    pub const ONE: Self = Self(NonZeroU8::new(1).unwrap());
    pub const TWO: Self = Self(NonZeroU8::new(2).unwrap());
    pub const THREE: Self = Self(NonZeroU8::new(3).unwrap());
    pub const FOUR: Self = Self(NonZeroU8::new(4).unwrap());
}

impl MaxMessageSizeMultiple {
    pub fn compute<M: Message>(&self) -> usize {
        (self.0.get() as usize) * M::MAX_SIZE
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            rx_buf_min_size: 4 * 1024,
            rx_buf_max_size: MaxMessageSizeMultiple::ONE,
            tx_buf_min_size: 4 * 1024,
            tx_buf_max_size: MaxMessageSizeMultiple::TWO,
            connect_timeout: std::time::Duration::from_secs(5),
            outbound_telemetry: false,
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
    rx_msg_buf: shiftbuf::ShiftBuf,
    /// Buffer used for sending.
    tx_msg_buf: shiftbuf::ShiftBuf,
    /// The list of queue points for outgoing messages.
    tx_queue_points: queue_points::Queue,
    /// Cached readyness.
    ready: bool,
    /// Last successful write time.
    last_write: Instant,
    /// The time the current message frame started coming in.
    frame_time: Instant,
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
        let now = Instant::now();
        Self {
            stream,
            rx_msg_buf: Default::default(),
            tx_msg_buf: Default::default(),
            tx_queue_points: Default::default(),
            ready: false,
            last_write: now,
            frame_time: now,
            config,
        }
    }

    /// Reads data from a stream and then attempts to decode messages from the data.
    /// Decoded messages are passed into the provided closure.
    /// Encountering an error means that the stream must be discarded.
    /// Attempts to read from a connection and decode messages in a fair manner.
    ///
    /// Returns whether there is more work available.
    pub fn read<M: Message, F: Fn(M, usize, Duration)>(
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
                    Ok(0) if buffer.is_empty() => break Ok(true),
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
        let now = Instant::now();
        if !preexisting {
            let consumed = decode_from_buffer(&mut &rx_buf[..total_read], on_msg, None)?;
            if consumed < total_read {
                self.rx_msg_buf.put(&rx_buf[consumed..total_read]);
                self.frame_time = now;
            }
        } else {
            let mut buf =
                spanned_view::SpannedView::new(self.rx_msg_buf.data(), &rx_buf[..total_read]);
            let consumed = decode_from_buffer(&mut buf, on_msg, Some(self.frame_time))?;
            self.frame_time = now;
            let a_consumed = consumed.min(self.rx_msg_buf.len());
            let b_consumed = consumed - a_consumed;
            let b_remainder = total_read - b_consumed;
            self.rx_msg_buf.advance(a_consumed);
            if self.rx_msg_buf.is_empty() {
                self.rx_msg_buf.clear();
            }
            if b_remainder > 0 {
                self.rx_msg_buf.put(&rx_buf[b_consumed..total_read]);
            }
        };

        read_result
    }

    /// Writes out as many bytes from the send buffer as possible, until blocking would start or
    /// some write budget is exceeded.
    ///
    /// Returns the status of the write and how many bytes were written out.
    pub fn write(&mut self, now: Instant) -> io::Result<(WriteResult, usize)> {
        if !self.has_queued_data() {
            return Ok((WriteResult::Done, 0));
        }

        let mut write_budget: usize = 128 * 1024;
        let mut syscall_budget: u8 = 8;
        let mut total_written = 0;

        loop {
            match self.try_write(now) {
                Ok(written) => {
                    total_written += written;
                    let has_more = self.has_queued_data();
                    log::trace!("wrote out {written} bytes, has more: {}", has_more);

                    if !has_more {
                        self.tx_msg_buf.clear();
                        break Ok((WriteResult::Done, total_written));
                    } else {
                        write_budget = write_budget.saturating_sub(written);
                        syscall_budget -= 1;
                        if write_budget == 0 || syscall_budget == 0 {
                            break Ok((WriteResult::BudgetExceeded, total_written));
                        }
                    }
                }

                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    log::trace!("write would block");
                    break Ok((WriteResult::WouldBlock, total_written));
                }

                Err(err) => break Err(err),
            }
        }
    }

    /// Queues a message for sending. This method simply encodes the message and places it into
    /// the internal buffer. [`write`] must be called when the socket is writeable in order to flush.
    ///
    /// Returns `true` if the write buffer contains enough space to accept the message, or `false`
    /// if the buffer is full and the message cannot be queued at this time. The second field
    /// returned denotes how many bytes exactly were queued.
    #[must_use]
    pub fn queue_message<M: Message>(&mut self, message: &M) -> (bool, usize) {
        let size = message.wire_size();
        let available = self.available::<M>();
        if available < size {
            (false, 0)
        } else {
            let pre_encode_len = self.tx_msg_buf.len();
            message.encode(&mut self.tx_msg_buf);
            let encoded = self.tx_msg_buf.len() - pre_encode_len;
            if encoded != size {
                if cfg!(debug_assertions) {
                    panic!(
                        "encoded size ({}) was not equal to predicted size ({})",
                        encoded, size
                    );
                } else {
                    log::warn!(
                        "encoded size ({}) was not equal to predicted size ({})",
                        encoded,
                        size
                    );
                }
            }
            self.tx_queue_points.append(encoded);
            (true, encoded)
        }
    }

    /// Returns how many bytes can be queued immediately with respect to transmit buffer size limit.
    #[inline(always)]
    fn available<M: Message>(&self) -> usize {
        self.config.tx_buf_max_size.compute::<M>() - self.tx_msg_buf.len()
    }

    /// Resizes and shrinks the capacity of internal send and receive buffers by 1/4, until the
    /// floor is reached. This helps maintain memory usage at sane levels since keeping permanent
    /// large receive buffers (e.g. after receiving a large message) would eventually exhaust
    /// available memory on less powerful devices when managing many peers.
    pub fn shrink_buffers(&mut self) {
        self.rx_msg_buf.shrink(self.config.rx_buf_min_size);
        self.tx_msg_buf.shrink(self.config.tx_buf_min_size);
        self.tx_queue_points.shrink();
    }

    /// Takes some bytes from the local send buffer and sends them.
    /// Returns the number of bytes sent.
    fn try_write(&mut self, now: Instant) -> io::Result<usize> {
        let written = self.stream.write(self.tx_msg_buf.data())?;
        self.tx_msg_buf.advance(written);
        self.stream.flush()?;
        self.last_write = now;
        self.tx_queue_points.mark_write(written);
        Ok(written)
    }

    /// Exposes the config.
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Returns whether the outbound buffer has more data queued.
    #[inline(always)]
    pub fn has_queued_data(&self) -> bool {
        !self.tx_msg_buf.is_empty()
    }

    /// Returns various metrics on the outbound buffer.
    #[inline(always)]
    pub fn outbound_metrics<M: Message>(&self) -> OutboundMetrics {
        OutboundMetrics {
            available: self.available::<M>(),
            queued: self.tx_msg_buf.len(),
            oldest: self.tx_queue_points.first(),
        }
    }
}

pub struct OutboundMetrics {
    /// How many bytes can be queued immediately with respect to tx buffer size limit.
    pub available: usize,
    /// The number of currently queued bytes.
    pub queued: usize,
    /// The time the oldest queueud byte, if any.
    pub oldest: Option<Instant>,
}

pub enum WriteResult {
    /// The write job is done and no more data remains queued.
    Done,
    /// More data remains queued but the write would block.
    WouldBlock,
    /// More data remains queued but the write budget has been exceeded.
    BudgetExceeded,
}

fn decode_from_buffer<M: Message, F: Fn(M, usize, Duration)>(
    buffer: &mut impl bytes::Buf,
    on_msg: F,
    first_frame: Option<Instant>,
) -> Result<usize, ReadError> {
    let mut is_first_frame = true;
    let mut total_consumed: usize = 0;
    loop {
        let pre = buffer.remaining();
        match M::decode(buffer) {
            Ok(message) => {
                let duration = if is_first_frame && let Some(first_frame) = first_frame {
                    is_first_frame = false;
                    first_frame.elapsed()
                } else {
                    Duration::ZERO
                };
                let consumed = pre - buffer.remaining();
                total_consumed += consumed;
                on_msg(message, consumed, duration);
            }
            Err(DecodeError::Partial) => {
                if pre >= M::MAX_SIZE {
                    break Err(ReadError::MalformedMessage);
                } else {
                    break Ok(total_consumed);
                }
            }
            Err(DecodeError::Malformed) => {
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

mod shiftbuf {
    /// A buffer implementation that enables multiple, individual reads to happen without having
    /// to clear the read data from the head. This can be done lazily when opportune instead.
    #[derive(Debug, Default)]
    pub struct ShiftBuf {
        buf: Vec<u8>,
        cursor: usize,
    }

    impl ShiftBuf {
        /// Returns the view of data that has not been read yet.
        pub fn data(&self) -> &[u8] {
            &self.buf[self.cursor..]
        }

        /// Returns the length of the data that has not been read yet.
        #[inline(always)]
        pub fn len(&self) -> usize {
            self.buf.len() - self.cursor
        }

        /// Returns the number of unread elements in the buffer.
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// Advances the read cursor forward, consuming bytes.
        pub fn advance(&mut self, n: usize) {
            self.cursor += n;
            assert!(self.cursor <= self.buf.len());
        }

        /// Clears the entire buffer and resets the read cursor.
        pub fn clear(&mut self) {
            self.buf.clear();
            self.cursor = 0;
        }

        /// Moves the yet unread data forward, discarding any previously read data.
        /// This operation only has an effect if the "junk" (already consumed) data takes up more
        /// than one third of space in the buffer.
        #[inline(always)]
        pub fn smart_shift(&mut self) {
            if self.cursor > 0 && !self.buf.is_empty() && self.cursor >= self.buf.len() / 3 {
                self.shift();
            }
        }

        /// Shrinks the buffer by one quarter, with respect to some floor.
        pub fn shrink(&mut self, min: usize) {
            if self.cursor > 0 {
                self.shift();
            }

            if self.buf.capacity() > min {
                let shrink_to = 3 * (self.buf.capacity() / 4);
                self.buf.shrink_to(min.max(shrink_to));
            }
        }

        pub fn put(&mut self, data: &[u8]) {
            self.buf.extend_from_slice(data);
        }

        /// Moves the yet unread data forward, discarding any previously read data.
        fn shift(&mut self) {
            self.buf.copy_within(self.cursor.., 0);
            self.buf.truncate(self.buf.len() - self.cursor);
            self.cursor = 0;
        }
    }

    unsafe impl bytes::BufMut for ShiftBuf {
        #[inline]
        fn remaining_mut(&self) -> usize {
            isize::MAX as usize - self.len()
        }

        #[inline]
        unsafe fn advance_mut(&mut self, cnt: usize) {
            let new_len = self.buf.len() + cnt;
            assert!(new_len <= self.buf.capacity());
            unsafe { self.buf.set_len(new_len) };
        }

        #[inline]
        fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
            self.smart_shift();
            if self.buf.len() == self.buf.capacity() {
                self.buf.reserve(64);
            }

            let cap = self.buf.capacity();
            let len = self.buf.len();

            unsafe {
                let ptr = self.buf.as_mut_ptr().add(len);
                bytes::buf::UninitSlice::from_raw_parts_mut(ptr, cap - len)
            }
        }
    }

    #[cfg(test)]
    #[test]
    fn buffer_test() {
        use bytes::BufMut;
        let mut buffer = ShiftBuf::default();

        assert_eq!(buffer.len(), 0);

        buffer.put_slice(b"test 123");
        assert_eq!(buffer.len(), 8);
        assert_eq!(buffer.data(), b"test 123");

        buffer.advance(5);
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.data(), b"123");
        assert_eq!(buffer.buf, b"test 123");

        buffer.shift();
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.data(), b"123");
        assert_eq!(buffer.buf, b"123");

        buffer.shift(); // this one is a no-op
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.data(), b"123");
        assert_eq!(buffer.buf, b"123");

        buffer.advance(3);
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.data(), b"");
        assert_eq!(buffer.buf, b"123");

        buffer.shift();
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.data(), b"");
        assert_eq!(buffer.buf, b"");

        assert_eq!(buffer.buf.capacity(), 64);
        buffer.shrink(4);
        assert!(buffer.buf.capacity() >= 4 && buffer.buf.capacity() < 64);
    }
}

/// Contains a data structure providing a contiguous view over two disjointed slices.
mod spanned_view {
    /// A lightweight wrapper that logically concatenates two byte slices into a single readable
    /// stream without allocation.
    pub struct SpannedView<'a> {
        a: &'a [u8],
        b: &'a [u8],
        cursor: usize,
    }

    impl<'a> SpannedView<'a> {
        pub fn new(a: &'a [u8], b: &'a [u8]) -> Self {
            Self { a, b, cursor: 0 }
        }
    }

    impl<'a> bytes::Buf for SpannedView<'a> {
        fn remaining(&self) -> usize {
            (self.a.len() + self.b.len()) - self.cursor
        }

        fn chunk(&self) -> &[u8] {
            if self.cursor < self.a.len() {
                &self.a[self.cursor..]
            } else {
                let b_cursor = self.cursor - self.a.len();
                &self.b[b_cursor..]
            }
        }

        fn advance(&mut self, cnt: usize) {
            self.cursor += cnt;
        }
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

        fn encode(&self, dest: &mut impl bytes::BufMut) {
            dest.put_u64_le(self.0);
        }

        fn decode(buffer: &mut impl bytes::Buf) -> Result<Self, DecodeError> {
            match buffer.try_get_u64_le() {
                Ok(v) => Ok(Ping(v)),
                Err(_) => Err(DecodeError::Partial),
            }
        }

        fn wire_size(&self) -> usize {
            8
        }
    }

    #[test]
    fn reassemble_message_whole_reads() {
        let mut buf = [0; 1024];
        let mut cursor = Vec::new();

        Ping(0).encode(&mut cursor);
        Ping(1).encode(&mut cursor);

        let mut conn = MessageStream::new(Cursor::new(cursor), StreamConfig::default());

        let received: RefCell<Vec<Ping>> = Default::default();
        conn.read(&mut buf, |message, size, time| {
            assert_eq!(size, 8);
            assert_eq!(time, Duration::ZERO);
            received.borrow_mut().push(message);
        })
        .unwrap();

        assert_eq!(received.borrow()[0], Ping(0));

        conn.read(&mut buf, |message, size, time| {
            assert_eq!(size, 8);
            assert_eq!(time, Duration::ZERO);
            received.borrow_mut().push(message);
        })
        .unwrap();
        assert_eq!(received.borrow()[1], Ping(1));

        let err = conn.read(&mut buf, |message, size, time| {
            assert_eq!(size, 8);
            assert_eq!(time, Duration::ZERO);
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
        let _ = conn.read(&mut buf, |message, size, time| {
            assert_eq!(size, 8);
            assert_eq!(time, Duration::ZERO);
            received.borrow_mut().push(message);
        });
        assert!(received.borrow().is_empty());
        assert_eq!(conn.rx_msg_buf.len(), 4);

        conn.stream.get_mut().extend_from_slice(&serialized[4..]);
        let _ = conn.read(&mut buf, |message, size, time| {
            assert_eq!(size, 8);
            assert!(time != Duration::ZERO);
            received.borrow_mut().push(message);
        });
        assert_eq!(received.borrow()[0], Ping(u64::MAX - 1));

        let _ = conn.read(&mut buf, |message, size, time| {
            assert_eq!(size, 8);
            assert_eq!(time, Duration::ZERO);
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
                tx_buf_max_size: MaxMessageSizeMultiple::THREE,
                ..Default::default()
            },
        );

        assert_eq!(connection.queue_message(&Ping(0)), (true, 8));
        assert_eq!(connection.queue_message(&Ping(1)), (true, 8));
        assert_eq!(connection.queue_message(&Ping(2)), (true, 8));

        let cloned_buffer = connection.tx_msg_buf.data().to_vec();
        connection.write(Instant::now()).unwrap();
        assert_eq!(wire.position(), 24);
        assert_eq!(wire.into_inner(), cloned_buffer);
    }

    #[test]
    fn send_message_buf_full() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let config = StreamConfig {
            tx_buf_min_size: 1,
            tx_buf_max_size: MaxMessageSizeMultiple::ONE,
            ..Default::default()
        };
        let mut connection = MessageStream::new(&mut wire, config);

        assert_eq!(connection.queue_message(&Ping(0)), (true, 8));
        assert_eq!(connection.queue_message(&Ping(1)), (false, 0));

        let buffer_len = connection.tx_msg_buf.len();
        let cloned_buffer = connection.tx_msg_buf.data().to_vec();
        connection.write(Instant::now()).unwrap();
        assert_eq!(wire.position(), buffer_len as u64);
        assert_eq!(wire.into_inner(), cloned_buffer);
    }

    #[test]
    fn outbound_buffer_availability() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let config = StreamConfig {
            tx_buf_min_size: 1,
            tx_buf_max_size: MaxMessageSizeMultiple::ONE,
            ..Default::default()
        };
        let mut connection = MessageStream::new(&mut wire, config);

        assert_eq!(connection.queue_message(&Ping(0)), (true, 8));
        assert_eq!(connection.queue_message(&Ping(1)), (false, 0));

        assert_eq!(connection.available::<Ping>(), 0);
        connection.write(Instant::now()).unwrap();
        assert_eq!(connection.available::<Ping>(), 8);
    }
}
