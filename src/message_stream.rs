use std::io::{self, Read, Write};
use std::time::Instant;

use bitcoin::consensus::{encode, Encodable};
use bitcoin::network::message::{self, RawNetworkMessage};

const MSG_LEN_OFFSET: usize = 16;

const MSG_PAYLOAD_OFFSET: usize = 24;

/// Stream related configuration parameters.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Defines the minimum size of the buffer used for message reassembly. Low values will cause
    /// more frequent reallocation while high values will reallocate less at the expense of more
    /// memory usage.
    pub rx_buf_min_size: usize,
    /// Defines the lower and upper size bounds for the send buffer. Once the send buffer is full,
    /// it is not possible to queue new messages for sending until some capacity is available.
    pub tx_buf_limits: std::ops::Range<usize>,
    /// The duration after which a peer is disconnected if it fails to read incoming data.
    pub stream_write_timeout: std::time::Duration,
    /// The duration after which a connection attempt is abandoned.
    pub stream_connect_timeout: std::time::Duration,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            rx_buf_min_size: 128 * 1024,
            tx_buf_limits: (128 * 1024)..message::MAX_MSG_SIZE,
            stream_write_timeout: std::time::Duration::from_secs(30),
            stream_connect_timeout: std::time::Duration::from_secs(5),
        }
    }
}

/// This trait allows callers to check if the item is in a usable state (connectedness etc).
/// Useful with streams that do not guarantee to be immediately usable.
pub trait MaybeReady {
    /// Checks whether the item is ready and usable.
    fn is_ready(&self) -> bool;
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
    /// Cached readyness.
    ready: bool,
    /// Last successful write time.
    last_write: Instant,
}

impl<T: Read + Write + MaybeReady> MessageStream<T> {
    pub fn new(stream: T, config: StreamConfig) -> Self {
        Self {
            stream,
            rx_msg_buf: Vec::with_capacity(config.rx_buf_min_size),
            tx_msg_buf: Vec::with_capacity(config.tx_buf_limits.end),
            ready: false,
            last_write: Instant::now(),
            config,
        }
    }

    /// Reads some bytes from the underlying reader and places them into the internal buffer for
    /// future reassembly. Returns the number of bytes read.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.stream.read(buf)?;
        self.rx_msg_buf.extend_from_slice(&buf[..read]);
        Ok(read)
    }

    /// Reassembles the next message from the internal buffer and returns it. Reassembly can fail
    /// for several reasons: the buffer contains only a partial message, the message is malformed etc.
    pub fn receive_message(&mut self) -> Result<RawNetworkMessage, DecodeError> {
        let msg_length = self
            .rx_msg_buf
            .get(MSG_LEN_OFFSET..MSG_LEN_OFFSET + 4)
            .ok_or(DecodeError::NotEnoughData)?;

        let msg_length =
            encode::deserialize::<u32>(msg_length).expect("4 bytes -> u32 cannot fail") as usize;

        if msg_length > message::MAX_MSG_SIZE {
            return Err(DecodeError::ExceedsSizeLimit);
        }

        if self.rx_msg_buf.len() >= msg_length + MSG_PAYLOAD_OFFSET {
            let decoded: Result<(RawNetworkMessage, usize), _> =
                encode::deserialize_partial(&self.rx_msg_buf);

            match decoded {
                Ok((message, read)) => {
                    self.rx_msg_buf.drain(..read);
                    Ok(message)
                }
                Err(err) => {
                    log::debug!("malformed message: {:?}", err);
                    Err(DecodeError::MalformedMessage)
                }
            }
        } else {
            Err(DecodeError::NotEnoughData)
        }
    }

    /// Takes some bytes from the local send buffer and sends them. Removes successfully sent bytes
    /// from the buffer. Returns the number of bytes sent.
    pub fn write(&mut self, now: Instant) -> io::Result<usize> {
        let written = self.stream.write(&self.tx_msg_buf)?;
        self.tx_msg_buf.drain(..written);
        self.stream.flush()?;
        self.last_write = now;
        Ok(written)
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
    pub fn queue_message(&mut self, message: &RawNetworkMessage) -> bool {
        if self.tx_msg_buf.len() <= self.config.tx_buf_limits.end {
            message
                .consensus_encode(&mut self.tx_msg_buf)
                .expect("writing to Vec cannot fail");

            true
        } else {
            false
        }
    }

    /// Returns a mutable reference to the underlying stream.
    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.stream
    }

    /// Returns whether the send buffer has more data to write.
    #[inline(always)]
    pub fn has_queued_data(&self) -> bool {
        !self.tx_msg_buf.is_empty()
    }

    /// Returns whether the stream is stale on the write side, i.e. the data is not leaving the
    /// send buffer in a timely manner.
    pub fn is_write_stale(&self, now: Instant) -> bool {
        self.has_queued_data() && (now - self.last_write) >= self.config.stream_write_timeout
    }

    /// Returns `true` if the underlying stream is ready. Otherwise it tests readyness and
    /// caches the result.
    pub fn is_ready(&mut self) -> bool {
        if !self.ready {
            self.ready = self.stream.is_ready();
        }

        self.ready
    }

    /// Resizes and truncates the capacity of internal send and receive buffers to their size or
    /// some set minimum, whichever is greater. This helps maintain memory usage at sane levels
    /// since keeping permanent large receive buffers (e.g. after receiving a block message) would
    /// eventually exhaust available memory on less powerful devices when managing many peers.
    pub fn resize_buffers(&mut self) {
        self.rx_msg_buf
            .truncate(self.config.rx_buf_min_size.max(self.rx_msg_buf.len()));
        self.tx_msg_buf
            .truncate(self.config.tx_buf_limits.start.max(self.tx_msg_buf.len()));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// There is not enough data available to reconstruct a message. This does not indicate an
    /// irrecoverable problem, it just means that not enough data has been taken of the wire yet
    /// and that the operation should be retried later.
    NotEnoughData,
    /// The message has bad length or checksum and cannot be decoded.
    MalformedMessage,
    /// Exceeds maximum message size limit. This points to a bad or malicious client.
    ExceedsSizeLimit,
}

#[cfg(test)]
mod test {
    use bitcoin::consensus::Encodable;
    use bitcoin::network::message::NetworkMessage;

    use std::io::Cursor;

    use super::*;

    impl<T> MaybeReady for &mut Cursor<T> {
        fn is_ready(&self) -> bool {
            true
        }
    }

    fn ping_msg(value: u64) -> RawNetworkMessage {
        RawNetworkMessage {
            magic: 0,
            payload: NetworkMessage::Ping(value),
        }
    }

    #[test]
    fn reassemble_message_whole_reads() {
        let mut buf = [0; 1024];
        let mut cursor = Cursor::new(Vec::new());

        ping_msg(0).consensus_encode(&mut cursor).unwrap();
        ping_msg(1).consensus_encode(&mut cursor).unwrap();
        cursor.set_position(0);

        let mut conn = MessageStream::new(&mut cursor, StreamConfig::default());
        let read = conn.read(&mut buf).unwrap();

        assert_eq!(read, 64);
        assert_eq!(conn.receive_message(), Ok(ping_msg(0)));
        assert_eq!(conn.receive_message(), Ok(ping_msg(1)));
        assert_eq!(conn.receive_message(), Err(DecodeError::NotEnoughData));
        assert_eq!(conn.stream.position(), 64);
        assert!(conn.rx_msg_buf.is_empty());
    }

    #[test]
    fn reassemble_message_partial_reads() {
        let mut buf = [0; 1024];
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let mut conn = MessageStream::new(&mut cursor, StreamConfig::default());
        let serialized = encode::serialize(&ping_msg(0));

        let pos = conn.stream.position();
        conn.stream.write(&serialized[..10]).unwrap();
        conn.stream.set_position(pos);
        assert_eq!(10, conn.read(&mut buf).unwrap());
        assert_eq!(conn.receive_message(), Err(DecodeError::NotEnoughData));

        let pos = conn.stream.position();
        conn.stream.write(&serialized[10..20]).unwrap();
        conn.stream.set_position(pos);
        assert_eq!(10, conn.read(&mut buf).unwrap());
        assert_eq!(conn.receive_message(), Err(DecodeError::NotEnoughData));

        let pos = conn.stream.position();
        conn.stream.write(&serialized[20..]).unwrap();
        conn.stream.set_position(pos);
        assert_eq!(12, conn.read(&mut buf).unwrap());
        assert_eq!(conn.receive_message(), Ok(ping_msg(0)));
    }

    #[test]
    fn reassemble_message_excessive_size() {
        let mut buf = [0; 1024];
        let mut msg = Vec::new();
        ping_msg(2).consensus_encode(&mut msg).unwrap();
        msg[16..20].copy_from_slice(&[255, 255, 255, 255]);

        let mut cursor = Cursor::new(msg);
        let mut conn = MessageStream::new(&mut cursor, StreamConfig::default());
        let read = conn.read(&mut buf).unwrap();
        assert_eq!(read, 32);
        assert_eq!(conn.receive_message(), Err(DecodeError::ExceedsSizeLimit));
    }

    #[test]
    fn send_message() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let mut connection = MessageStream::new(&mut wire, StreamConfig::default());

        assert!(connection.queue_message(&ping_msg(0)));
        assert!(connection.queue_message(&ping_msg(1)));
        assert!(connection.queue_message(&ping_msg(2)));

        let buffer_len = connection.tx_msg_buf.len();
        let cloned_buffer = connection.tx_msg_buf.clone();
        let written = connection.write(Instant::now()).unwrap();
        assert_eq!(written, buffer_len);
        assert_eq!(wire.position(), 96);
        assert_eq!(wire.into_inner(), cloned_buffer);
    }

    #[test]
    fn send_message_buf_full() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let config = StreamConfig {
            tx_buf_limits: 1..16,
            ..Default::default()
        };
        let mut connection = MessageStream::new(&mut wire, config);

        assert!(connection.queue_message(&ping_msg(0)));
        assert!(!connection.queue_message(&ping_msg(1)));

        let buffer_len = connection.tx_msg_buf.len();
        let cloned_buffer = connection.tx_msg_buf.clone();
        let written = connection.write(Instant::now()).unwrap();
        assert_eq!(written, buffer_len);
        assert_eq!(wire.position(), 32);
        assert_eq!(wire.into_inner(), cloned_buffer);
    }
}
