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
    fn encode(&self, dest: &mut impl std::io::Write) -> usize {
        let mut written = 0;

        let _ = dest.write_all(self.prefix());
        written += self.prefix().len();

        match self {
            Message::Ping(p) | Message::Pong(p) => {
                let _ = dest.write_all(&p.to_le_bytes());
                written += 8;
            }
            Message::Data(data) => {
                let _ = dest.write_all(&(data.len() as u32).to_le_bytes());
                written += 4;
                let _ = dest.write_all(data);
                written += data.len();
            }
        }

        written
    }

    fn decode(buffer: &[u8]) -> Result<(Self, usize), DecodeError> {
        match buffer.get(0..4) {
            Some(p @ b"ping" | p @ b"pong") => match buffer.get(4..12) {
                Some(value) => {
                    let value = u64::from_le_bytes(value.try_into().unwrap());
                    if p == b"ping" {
                        Ok((Message::Ping(value), 12))
                    } else {
                        Ok((Message::Pong(value), 12))
                    }
                }
                None => Err(DecodeError::NotEnoughData),
            },
            Some(b"data") => match buffer.get(4..8) {
                Some(length) => {
                    let length = u32::from_le_bytes(length.try_into().unwrap()) as usize;
                    match buffer.get(8..8 + length) {
                        Some(data) => Ok((Message::Data(data.to_owned()), length + 8)),
                        None => Err(DecodeError::NotEnoughData),
                    }
                }
                None => Err(DecodeError::NotEnoughData),
            },
            None => Err(DecodeError::NotEnoughData),
            _ => Err(DecodeError::MalformedMessage),
        }
    }
}
