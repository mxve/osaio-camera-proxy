use bytes::{BufMut, Bytes, BytesMut};

const START_CODE: [u8; 4] = [0, 0, 0, 1];

// rfc 7798 hevc rtp depacketizer into annex-b nal units
#[derive(Default)]
pub struct Depacketizer {
    fragment: Option<BytesMut>,
}

impl Depacketizer {
    pub fn push(&mut self, payload: &[u8]) -> Vec<Bytes> {
        if payload.len() < 2 {
            return Vec::new();
        }
        match (payload[0] >> 1) & 0x3f {
            48 => self.aggregation(payload),
            49 => self.fragment(payload).into_iter().collect(),
            _ => vec![with_start_code(payload)],
        }
    }

    fn aggregation(&mut self, payload: &[u8]) -> Vec<Bytes> {
        let mut out = Vec::new();
        let mut i = 2;
        while i + 2 <= payload.len() {
            let size = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
            i += 2;
            if i + size > payload.len() {
                break;
            }
            out.push(with_start_code(&payload[i..i + size]));
            i += size;
        }
        out
    }

    fn fragment(&mut self, payload: &[u8]) -> Option<Bytes> {
        if payload.len() < 3 {
            return None;
        }
        let header = payload[2];
        let (start, end) = (header & 0x80 != 0, header & 0x40 != 0);
        let body = &payload[3..];

        if start {
            let mut buf = BytesMut::new();
            buf.put_u8((payload[0] & 0x81) | ((header & 0x3f) << 1));
            buf.put_u8(payload[1]);
            buf.put_slice(body);
            self.fragment = Some(buf);
        } else if let Some(buf) = &mut self.fragment {
            buf.put_slice(body);
        } else {
            return None;
        }

        if !end {
            return None;
        }
        let buf = self.fragment.take()?;
        Some(with_start_code(&buf))
    }
}

fn with_start_code(nalu: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(4 + nalu.len());
    buf.put_slice(&START_CODE);
    buf.put_slice(nalu);
    buf.freeze()
}
