use std::collections::VecDeque;

pub const HEADER_LEN: usize = 13;
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Regular,
    Handshake,
    KeepAlive,
}

impl FrameType {
    fn to_byte(self) -> u8 {
        match self {
            FrameType::Regular => 1,
            FrameType::Handshake => 2,
            FrameType::KeepAlive => 9,
        }
    }

    fn from_byte(byte: u8) -> Option<FrameType> {
        match byte {
            1 => Some(FrameType::Regular),
            2 => Some(FrameType::Handshake),
            9 => Some(FrameType::KeepAlive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: FrameType,
    pub id: u32,
    pub ack: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: FrameType, id: u32, ack: u32, payload: impl Into<Vec<u8>>) -> Frame {
        Frame {
            kind,
            id,
            ack,
            payload: payload.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(HEADER_LEN + self.payload.len());
        buffer.push(self.kind.to_byte());
        buffer.extend_from_slice(&self.id.to_be_bytes());
        buffer.extend_from_slice(&self.ack.to_be_bytes());
        buffer.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        buffer.extend_from_slice(&self.payload);
        buffer
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("unknown relay frame type: {0}")]
    UnknownType(u8),
    #[error("relay frame payload of {0} bytes exceeds the {MAX_MESSAGE_SIZE}-byte limit")]
    Oversized(usize),
}

#[derive(Default)]
pub struct FrameDecoder {
    buffer: VecDeque<u8>,
}

impl FrameDecoder {
    pub fn new() -> FrameDecoder {
        FrameDecoder::default()
    }

    pub fn feed(&mut self, chunk: impl AsRef<[u8]>) {
        self.buffer.extend(chunk.as_ref().iter().copied());
    }

    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.buffer.len() < HEADER_LEN {
            return Ok(None);
        }
        let mut header = [0u8; HEADER_LEN];
        for (slot, byte) in header.iter_mut().zip(self.buffer.iter()) {
            *slot = *byte;
        }
        let length = u32::from_be_bytes([header[9], header[10], header[11], header[12]]) as usize;
        if length > MAX_MESSAGE_SIZE {
            return Err(FrameError::Oversized(length));
        }
        let kind = FrameType::from_byte(header[0]).ok_or(FrameError::UnknownType(header[0]))?;
        if self.buffer.len() < HEADER_LEN + length {
            return Ok(None);
        }
        self.buffer.drain(..HEADER_LEN);
        let payload = self.buffer.drain(..length).collect::<Vec<_>>();
        Ok(Some(Frame {
            kind,
            id: u32::from_be_bytes([header[1], header[2], header[3], header[4]]),
            ack: u32::from_be_bytes([header[5], header[6], header[7], header[8]]),
            payload,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_regular_frame() {
        let frame = Frame::new(FrameType::Regular, 7, 3, b"hello".to_vec());
        let mut decoder = FrameDecoder::new();
        decoder.feed(frame.encode());
        let decoded = decoder.next_frame().unwrap().unwrap();
        assert_eq!(decoded.kind, FrameType::Regular);
        assert_eq!(decoded.id, 7);
        assert_eq!(decoded.ack, 3);
        assert_eq!(decoded.payload, b"hello");
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn waits_for_a_full_frame_across_chunks() {
        let frame = Frame::new(FrameType::KeepAlive, 1, 1, Vec::new());
        let bytes = frame.encode();
        let (head, tail) = bytes.split_at(6);
        let mut decoder = FrameDecoder::new();
        decoder.feed(head);
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.feed(tail);
        assert_eq!(
            decoder.next_frame().unwrap().unwrap().kind,
            FrameType::KeepAlive
        );
    }

    #[test]
    fn decodes_two_frames_from_one_buffer() {
        let mut bytes = Frame::new(FrameType::Regular, 1, 0, b"a".to_vec()).encode();
        bytes.extend(Frame::new(FrameType::Handshake, 2, 1, b"bb".to_vec()).encode());
        let mut decoder = FrameDecoder::new();
        decoder.feed(bytes);
        assert_eq!(decoder.next_frame().unwrap().unwrap().id, 1);
        let second = decoder.next_frame().unwrap().unwrap();
        assert_eq!(second.kind, FrameType::Handshake);
        assert_eq!(second.payload, b"bb");
    }

    #[test]
    fn rejects_an_unknown_type() {
        let mut decoder = FrameDecoder::new();
        let mut bytes = vec![7u8];
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        decoder.feed(bytes);
        assert!(matches!(
            decoder.next_frame(),
            Err(FrameError::UnknownType(7))
        ));
    }
}
