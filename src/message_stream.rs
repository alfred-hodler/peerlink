use std::io::{self, Read, Write};

use bitcoin::consensus::{encode, Encodable};
use bitcoin::network::message::{self, RawNetworkMessage};

const MSG_LEN_OFFSET: usize = 16;

const MSG_PAYLOAD_OFFSET: usize = 24;

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
    /// The read+write stream underlying the connection.
    stream: T,
    /// Reusable read buffer for reading from the underlying reader.
    read_buf: Vec<u8>,
    /// Buffer used for message reconstruction.
    in_msg_buf: Vec<u8>,
    /// Buffer used for sending.
    out_msg_buf: Vec<u8>,
    /// Cached readyness.
    ready: bool,
}

impl<T: Read + Write + MaybeReady> MessageStream<T> {
    pub fn new(stream: T) -> Self {
        Self {
            stream,
            read_buf: vec![0; 1024 * 1024],
            in_msg_buf: Vec::with_capacity(8 * 1024),
            out_msg_buf: Vec::new(),
            ready: false,
        }
    }

    /// Reads some bytes from the underlying reader and places them into the internal buffer for
    /// future reassembly. Returns the number of bytes read.
    pub fn read(&mut self) -> io::Result<usize> {
        let read = self.stream.read(&mut self.read_buf)?;
        self.in_msg_buf.extend_from_slice(&self.read_buf[..read]);
        Ok(read)
    }

    /// Reassembles the next message from the internal buffer and returns it. Reassembly can fail
    /// for several reasons: the buffer contains only a partial message, the message is malformed etc.
    pub fn receive_message(&mut self) -> Result<RawNetworkMessage, DecodeError> {
        let msg_length = self
            .in_msg_buf
            .get(MSG_LEN_OFFSET..MSG_LEN_OFFSET + 4)
            .ok_or(DecodeError::NotEnoughData)?;

        let msg_length =
            encode::deserialize::<u32>(msg_length).expect("4 bytes -> u32 cannot fail") as usize;

        if msg_length > message::MAX_MSG_SIZE {
            return Err(DecodeError::ExceedsSizeLimit);
        }

        if self.in_msg_buf.len() >= msg_length + MSG_PAYLOAD_OFFSET {
            let decoded: Result<(RawNetworkMessage, usize), _> =
                encode::deserialize_partial(&self.in_msg_buf);

            match decoded {
                Ok((message, read)) => {
                    self.in_msg_buf.drain(..read);
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
    pub fn write(&mut self) -> io::Result<usize> {
        let written = self.stream.write(&self.out_msg_buf)?;
        self.out_msg_buf.drain(..written);
        self.stream.flush()?;
        Ok(written)
    }

    /// Sends a message. In reality this method simply serializes the message and places it into
    /// the internal buffer. `write` must be called when the socket is writeable in order to flush.
    pub fn send_message(&mut self, message: &RawNetworkMessage) {
        message
            .consensus_encode(&mut self.out_msg_buf)
            .expect("writing to Vec cannot fail");
    }

    /// Returns a mutable reference to the underlying stream.
    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.stream
    }

    /// Returns whether the send buffer has more data to write.
    #[inline(always)]
    pub fn has_queued_data(&self) -> bool {
        !self.out_msg_buf.is_empty()
    }

    /// Returns `true` if the underlying stream is ready. Otherwise it tests readyness and
    /// caches the result.
    pub fn is_ready(&mut self) -> bool {
        if !self.ready {
            self.ready = self.stream.is_ready();
        }

        self.ready
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

impl DecodeError {
    /// Determines if the error is the result of a peer violating the protocol.
    pub fn is_codec_violation(&self) -> bool {
        use DecodeError::*;
        match self {
            MalformedMessage | ExceedsSizeLimit => true,
            NotEnoughData => false,
        }
    }
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
        let mut cursor = Cursor::new(Vec::new());

        ping_msg(0).consensus_encode(&mut cursor).unwrap();
        ping_msg(1).consensus_encode(&mut cursor).unwrap();
        cursor.set_position(0);

        let mut conn = MessageStream::new(&mut cursor);
        let read = conn.read().unwrap();

        assert_eq!(read, 64);
        assert_eq!(conn.receive_message(), Ok(ping_msg(0)));
        assert_eq!(conn.receive_message(), Ok(ping_msg(1)));
        assert_eq!(conn.receive_message(), Err(DecodeError::NotEnoughData));
        assert_eq!(conn.stream.position(), 64);
        assert!(conn.in_msg_buf.is_empty());
    }

    #[test]
    fn reassemble_message_partial_reads() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let mut conn = MessageStream::new(&mut cursor);
        let serialized = encode::serialize(&ping_msg(0));

        let pos = conn.stream.position();
        conn.stream.write(&serialized[..10]).unwrap();
        conn.stream.set_position(pos);
        assert_eq!(10, conn.read().unwrap());
        assert_eq!(conn.receive_message(), Err(DecodeError::NotEnoughData));

        let pos = conn.stream.position();
        conn.stream.write(&serialized[10..20]).unwrap();
        conn.stream.set_position(pos);
        assert_eq!(10, conn.read().unwrap());
        assert_eq!(conn.receive_message(), Err(DecodeError::NotEnoughData));

        let pos = conn.stream.position();
        conn.stream.write(&serialized[20..]).unwrap();
        conn.stream.set_position(pos);
        assert_eq!(12, conn.read().unwrap());
        assert_eq!(conn.receive_message(), Ok(ping_msg(0)));
    }

    #[test]
    fn reassemble_message_excessive_size() {
        let mut msg = Vec::new();
        ping_msg(2).consensus_encode(&mut msg).unwrap();
        msg[16..20].copy_from_slice(&[255, 255, 255, 255]);

        let mut cursor = Cursor::new(msg);
        let mut conn = MessageStream::new(&mut cursor);
        let read = conn.read().unwrap();
        assert_eq!(read, 32);
        assert_eq!(conn.receive_message(), Err(DecodeError::ExceedsSizeLimit));
    }

    #[test]
    fn send_message() {
        let mut wire = Cursor::new(Vec::<u8>::new());
        let mut connection = MessageStream::new(&mut wire);

        connection.send_message(&ping_msg(0));
        connection.send_message(&ping_msg(1));
        connection.send_message(&ping_msg(2));

        let buffer_len = connection.out_msg_buf.len();
        let cloned_buffer = connection.out_msg_buf.clone();
        let written = connection.write().unwrap();
        assert_eq!(written, buffer_len);
        assert_eq!(wire.position(), 96);
        assert_eq!(wire.into_inner(), cloned_buffer);
    }
}
