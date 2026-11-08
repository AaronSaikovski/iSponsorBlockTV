//! BrowserChannel framing counts UTF-16 string units, not UTF-8 bytes.
//! Protocol reference: Closure Library channelrequest.js at b312823ec5f84239ff1db7526f4a75cba0420a33.
use bytes::{Buf, BytesMut};
use thiserror::Error;

use super::events::{Envelope, decode_batch};

pub const MAX_FRAME_UNITS: usize = 1024 * 1024;
pub const MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("invalid Lounge frame length header")]
    Header,
    #[error("Lounge frame exceeds its size limit")]
    TooLarge,
    #[error("invalid Lounge UTF-8 payload")]
    Utf8,
    #[error("Lounge frame ends inside a UTF-16 scalar pair")]
    ScalarBoundary,
    #[error("invalid Lounge JSON event batch")]
    Batch,
    #[error("truncated Lounge frame")]
    Truncated,
    #[error("Lounge decoder already failed")]
    Failed,
}

/// Incremental, linear-time decoder. A framing failure is terminal: callers must
/// replace the session, never attempt to resynchronize on untrusted bytes.
#[derive(Default)]
pub struct FrameDecoder {
    buffer: BytesMut,
    target: Option<usize>,
    scanned: usize,
    units: usize,
    failed: bool,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        if self.failed {
            return Err(FrameError::Failed);
        }
        if bytes.len() > MAX_BUFFER_BYTES - self.buffer.len() {
            self.failed = true;
            return Err(FrameError::TooLarge);
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    pub fn next_batch(&mut self) -> Result<Option<Vec<Envelope>>, FrameError> {
        if self.failed {
            return Err(FrameError::Failed);
        }
        let result = self.decode_next();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn decode_next(&mut self) -> Result<Option<Vec<Envelope>>, FrameError> {
        if self.target.is_none() {
            let mut length = 0usize;
            let mut end = None;
            for (index, byte) in self.buffer.iter().copied().enumerate() {
                if byte == b'\n' {
                    if index == 0 || length == 0 {
                        return Err(FrameError::Header);
                    }
                    end = Some(index + 1);
                    break;
                }
                if index >= 10 || !byte.is_ascii_digit() {
                    return Err(FrameError::Header);
                }
                length = length
                    .checked_mul(10)
                    .and_then(|n| n.checked_add((byte - b'0') as usize))
                    .ok_or(FrameError::TooLarge)?;
                if length > MAX_FRAME_UNITS {
                    return Err(FrameError::TooLarge);
                }
            }
            let Some(end) = end else {
                return Ok(None);
            };
            self.buffer.advance(end);
            self.target = Some(length);
        }
        let target = self.target.expect("header parsed");
        while self.units < target {
            let Some(&lead) = self.buffer.get(self.scanned) else {
                return Ok(None);
            };
            let width = match lead {
                0x00..=0x7f => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return Err(FrameError::Utf8),
            };
            let available = self.buffer.len() - self.scanned;
            if available < width {
                // Reject an already-invalid prefix now, retain genuinely partial scalars.
                if std::str::from_utf8(&self.buffer[self.scanned..])
                    .is_err_and(|e| e.error_len().is_some())
                {
                    return Err(FrameError::Utf8);
                }
                return Ok(None);
            }
            let scalar = std::str::from_utf8(&self.buffer[self.scanned..self.scanned + width])
                .map_err(|_| FrameError::Utf8)?;
            self.units += scalar.chars().next().expect("nonempty scalar").len_utf16();
            if self.units > target {
                return Err(FrameError::ScalarBoundary);
            }
            self.scanned += width;
        }
        let batch = decode_batch(&self.buffer[..self.scanned])?;
        self.buffer.advance(self.scanned);
        self.target = None;
        self.scanned = 0;
        self.units = 0;
        Ok(Some(batch))
    }

    /// Call only after draining complete batches. Clean EOF is a frame boundary.
    pub fn finish(&mut self) -> Result<(), FrameError> {
        if self.failed {
            return Err(FrameError::Failed);
        }
        if self.target.is_some() || !self.buffer.is_empty() {
            self.failed = true;
            return Err(FrameError::Truncated);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(payload: &str) -> Vec<u8> {
        format!("{}\n{payload}", payload.encode_utf16().count()).into_bytes()
    }

    #[test]
    fn every_byte_split_preserves_unicode_and_whitespace() {
        let payload = "[ [1,[\"nowPlaying\",{\"videoId\":\"é🦀\"}]] ]\n";
        let bytes = framed(payload);
        for split in 0..=bytes.len() {
            let mut decoder = FrameDecoder::new();
            decoder.push(&bytes[..split]).unwrap();
            let first = decoder.next_batch().unwrap();
            decoder.push(&bytes[split..]).unwrap();
            let batch = first.or_else(|| decoder.next_batch().unwrap()).unwrap();
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0].aid, 1);
            decoder.finish().unwrap();
        }
    }

    #[test]
    fn coalesced_frames_and_leading_zero_header() {
        let mut decoder = FrameDecoder::new();
        decoder.push(b"0002\n[]3\n[]\n").unwrap();
        assert!(decoder.next_batch().unwrap().unwrap().is_empty());
        assert!(decoder.next_batch().unwrap().unwrap().is_empty());
        assert!(decoder.next_batch().unwrap().is_none());
        decoder.finish().unwrap();
    }

    #[test]
    fn malformed_frames_are_terminal() {
        for bytes in [
            b"0\n".as_slice(),
            b"+2\n[]",
            b"2\r\n[]",
            b" 2\n[]",
            b"\n",
            b"00000000002\n[]",
            b"1048577\n",
            b"9999999999\n",
            b"1\n\xff",
            b"1\n\xf0\x9f\xa6\x80",
            b"2\n{}",
            b"3\n[0]",
            b"2\nxx",
        ] {
            let mut decoder = FrameDecoder::new();
            decoder.push(bytes).unwrap();
            assert!(decoder.next_batch().is_err(), "{bytes:?}");
            assert_eq!(decoder.next_batch().unwrap_err(), FrameError::Failed);
        }
    }

    #[test]
    fn eof_and_payload_newline_are_exact() {
        for bytes in [b"2".as_slice(), b"2\n[", b"1\n\xc3"] {
            let mut decoder = FrameDecoder::new();
            decoder.push(bytes).unwrap();
            assert!(decoder.next_batch().unwrap().is_none());
            assert_eq!(decoder.finish().unwrap_err(), FrameError::Truncated);
        }
        let mut decoder = FrameDecoder::new();
        decoder.push(b"2\n[]\n").unwrap();
        decoder.next_batch().unwrap().unwrap();
        assert_eq!(decoder.next_batch().unwrap_err(), FrameError::Header);
    }

    #[test]
    fn validates_whole_envelope_batch_before_exposing_events() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&framed("[[1,[\"noop\"]],[2,[]]]")).unwrap();
        assert_eq!(decoder.next_batch().unwrap_err(), FrameError::Batch);
    }

    #[test]
    fn bounds_unconsumed_bytes() {
        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.push(&vec![b' '; MAX_BUFFER_BYTES + 1]),
            Err(FrameError::TooLarge)
        );
    }
}
