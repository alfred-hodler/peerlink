use peerlink::DecodeError;

#[derive(Debug)]
pub enum Message {
    Ping(u64),
    Pong(u64),
    Data(Vec<u8>),
}

impl Message {
    fn prefix(&self) -> &[u8; 4] {
        match self {
            Message::Ping(_) => b"ping",
            Message::Pong(_) => b"pong",
            Message::Data(_) => b"data",
        }
    }
}

impl peerlink::Message for Message {
    const MAX_SIZE: usize = 1024 * 1024 * 10 + 8;

    fn encode(&self, dest: &mut impl bytes::BufMut) {
        dest.put_slice(self.prefix());

        match self {
            Message::Ping(p) | Message::Pong(p) => {
                dest.put_u64_le(*p);
            }
            Message::Data(data) => {
                dest.put_u32_le(data.len() as u32);
                dest.put_slice(&data);
            }
        }
    }

    fn decode(buffer: &mut impl bytes::Buf) -> Result<Self, DecodeError> {
        let mut prefix = [0_u8; 4];
        buffer
            .try_copy_to_slice(&mut prefix)
            .map_err(|_| peerlink::DecodeError::Partial)?;

        match &prefix {
            p @ b"ping" | p @ b"pong" => match buffer.try_get_u64_le().ok() {
                Some(value) => {
                    if p == b"ping" {
                        Ok(Message::Ping(value))
                    } else {
                        Ok(Message::Pong(value))
                    }
                }
                None => Err(DecodeError::Partial),
            },
            b"data" => match buffer.try_get_u32_le().ok() {
                Some(length) => {
                    let length = length as usize;
                    if buffer.remaining() >= length {
                        let mut data = vec![0_u8; length];
                        buffer.copy_to_slice(&mut data);
                        Ok(Message::Data(data.to_owned()))
                    } else {
                        Err(DecodeError::Partial)
                    }
                }
                None => Err(DecodeError::Partial),
            },
            _ => Err(DecodeError::Malformed),
        }
    }

    fn wire_size(&self) -> usize {
        match self {
            Message::Ping(_) => 12,
            Message::Pong(_) => 12,
            Message::Data(items) => 4 + 4 + items.len(),
        }
    }
}
