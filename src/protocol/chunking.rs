// Bolt chunked transfer encoding — framing layer for PackStream messages.
//
// Every Bolt message on the wire is wrapped in one or more chunks:
//   Chunk     = [u16 BE size][size bytes of payload]
//   EndMarker = [0x00, 0x00]
//   Message   = Chunk+ EndMarker

use bytes::{BufMut, BytesMut};

/// Default maximum message size the decoder will accept (16 MiB).
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during chunk decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkError {
    /// The accumulated message exceeded the configured maximum size.
    MessageTooLarge { size: usize, max: usize },
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::MessageTooLarge { size, max } => {
                write!(f, "message size {size} exceeds maximum allowed size {max}")
            }
        }
    }
}

impl std::error::Error for ChunkError {}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Encode a complete message into Bolt chunked transfer format.
///
/// Splits `message` into chunks of at most `max_chunk_size` bytes, each
/// prefixed with a 16-bit big-endian length header, followed by a zero
/// terminator (`0x0000`).
///
/// # Panics
///
/// Panics if `max_chunk_size` is 0.
pub fn chunk_message(message: &[u8], max_chunk_size: u16) -> BytesMut {
    assert!(max_chunk_size > 0, "max_chunk_size must be > 0");

    let max = max_chunk_size as usize;
    let num_full = message.len() / max;
    let remainder = message.len() % max;

    // Pre-calculate exact output size: headers + payloads + terminator.
    let capacity = num_full * (2 + max) + if remainder > 0 { 2 + remainder } else { 0 } + 2; // zero terminator

    let mut buf = BytesMut::with_capacity(capacity);

    let mut offset = 0;
    while offset < message.len() {
        let end = std::cmp::min(offset + max, message.len());
        let chunk_len = end - offset;
        buf.put_u16(chunk_len as u16);
        buf.extend_from_slice(&message[offset..end]);
        offset = end;
    }

    // Zero terminator — end of message.
    buf.put_u16(0);
    buf
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Internal state of the chunk decoder.
#[derive(Debug, Clone, Copy)]
enum DecoderState {
    /// Waiting for the first byte of the 2-byte chunk size header.
    ReadingHeader,
    /// Have the high byte of the header, waiting for the low byte.
    ReadingHeaderByte2(u8),
    /// Reading payload bytes; `remaining` bytes still expected.
    ReadingPayload { remaining: u16 },
}

/// Accumulates Bolt chunks from the wire and reassembles complete messages.
///
/// TCP can deliver data at arbitrary byte boundaries, so the decoder
/// maintains internal state across multiple [`feed`](Self::feed) calls.
/// Each completed message (delimited by a `0x0000` zero-chunk) is returned
/// as a contiguous `BytesMut` buffer suitable for zero-copy parsing by
/// `PackStreamReader`.
pub struct ChunkDecoder {
    /// Accumulation buffer for the current message being assembled.
    buffer: BytesMut,
    /// Parser state machine.
    state: DecoderState,
    /// Maximum allowed message size in bytes.
    max_message_size: usize,
}

impl ChunkDecoder {
    /// Create a new decoder with the default max message size (16 MiB).
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::new(),
            state: DecoderState::ReadingHeader,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        }
    }

    /// Create a new decoder with a custom maximum message size.
    pub fn with_max_message_size(max: usize) -> Self {
        Self {
            buffer: BytesMut::new(),
            state: DecoderState::ReadingHeader,
            max_message_size: max,
        }
    }

    /// Feed raw bytes from the wire. Returns any complete de-chunked messages.
    ///
    /// A single `feed` call may return zero, one, or multiple messages
    /// depending on how much data is provided.
    ///
    /// # Errors
    ///
    /// Returns [`ChunkError::MessageTooLarge`] if the accumulated message
    /// exceeds the configured maximum size.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<BytesMut>, ChunkError> {
        let mut messages = Vec::new();
        let mut pos = 0;

        while pos < data.len() {
            match self.state {
                DecoderState::ReadingHeader => {
                    if pos + 1 < data.len() {
                        // Have both header bytes available.
                        let size = u16::from_be_bytes([data[pos], data[pos + 1]]);
                        pos += 2;
                        self.handle_header(size, &mut messages)?;
                    } else {
                        // Only one byte available — stash it.
                        self.state = DecoderState::ReadingHeaderByte2(data[pos]);
                        pos += 1;
                    }
                }
                DecoderState::ReadingHeaderByte2(hi) => {
                    let size = u16::from_be_bytes([hi, data[pos]]);
                    pos += 1;
                    self.handle_header(size, &mut messages)?;
                }
                DecoderState::ReadingPayload { remaining } => {
                    let available = data.len() - pos;
                    let to_copy = std::cmp::min(remaining as usize, available);
                    self.buffer.extend_from_slice(&data[pos..pos + to_copy]);
                    pos += to_copy;

                    let left = remaining - to_copy as u16;
                    if left == 0 {
                        self.state = DecoderState::ReadingHeader;
                    } else {
                        self.state = DecoderState::ReadingPayload { remaining: left };
                    }
                }
            }
        }

        Ok(messages)
    }

    /// Process a completed chunk header.
    fn handle_header(&mut self, size: u16, messages: &mut Vec<BytesMut>) -> Result<(), ChunkError> {
        if size == 0 {
            // Zero-chunk = end of message. Emit the accumulated buffer.
            messages.push(self.buffer.split());
            self.state = DecoderState::ReadingHeader;
        } else {
            // Check that adding this chunk won't exceed the limit.
            let new_size = self.buffer.len() + size as usize;
            if new_size > self.max_message_size {
                // Reset state so the decoder is usable after the error.
                self.buffer.clear();
                self.state = DecoderState::ReadingHeader;
                return Err(ChunkError::MessageTooLarge {
                    size: new_size,
                    max: self.max_message_size,
                });
            }
            self.state = DecoderState::ReadingPayload { remaining: size };
        }
        Ok(())
    }
}

impl Default for ChunkDecoder {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Encoder tests ----

    #[test]
    fn chunk_empty_message() {
        let out = chunk_message(&[], 65535);
        assert_eq!(&out[..], &[0x00, 0x00]);
    }

    #[test]
    fn chunk_single_small() {
        let msg = [1, 2, 3, 4, 5];
        let out = chunk_message(&msg, 65535);
        // header [0x00, 0x05] + payload + terminator [0x00, 0x00]
        let mut expected = vec![0x00, 0x05];
        expected.extend_from_slice(&msg);
        expected.extend_from_slice(&[0x00, 0x00]);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn chunk_exactly_max_size() {
        let msg = vec![0xAB; 65535];
        let out = chunk_message(&msg, 65535);
        assert_eq!(out.len(), 2 + 65535 + 2); // header + payload + terminator
        assert_eq!(u16::from_be_bytes([out[0], out[1]]), 65535);
        assert_eq!(&out[2..2 + 65535], &msg[..]);
        assert_eq!(&out[2 + 65535..], &[0x00, 0x00]);
    }

    #[test]
    fn chunk_one_byte_over_max() {
        let msg = vec![0xCD; 65536];
        let out = chunk_message(&msg, 65535);
        // First chunk: 65535 bytes
        assert_eq!(u16::from_be_bytes([out[0], out[1]]), 65535);
        // Second chunk: 1 byte
        let second_header_start = 2 + 65535;
        assert_eq!(
            u16::from_be_bytes([out[second_header_start], out[second_header_start + 1]]),
            1
        );
        // Terminator
        let term_start = second_header_start + 2 + 1;
        assert_eq!(&out[term_start..], &[0x00, 0x00]);
    }

    #[test]
    fn chunk_large_multi_chunk() {
        let msg = vec![0x42; 70_000];
        let out = chunk_message(&msg, 65535);
        // Chunk 1: 65535 bytes
        assert_eq!(u16::from_be_bytes([out[0], out[1]]), 65535);
        // Chunk 2: 4465 bytes
        let c2 = 2 + 65535;
        assert_eq!(u16::from_be_bytes([out[c2], out[c2 + 1]]), 4465);
        // Verify total length
        assert_eq!(out.len(), 2 + 65535 + 2 + 4465 + 2);
    }

    #[test]
    fn chunk_exact_multiple_of_max() {
        let max: u16 = 100;
        let msg = vec![0x11; 200];
        let out = chunk_message(&msg, max);
        // Two full chunks + terminator
        assert_eq!(out.len(), (2 + 100) + (2 + 100) + 2);
        assert_eq!(u16::from_be_bytes([out[0], out[1]]), 100);
        assert_eq!(u16::from_be_bytes([out[102], out[103]]), 100);
        assert_eq!(&out[204..], &[0x00, 0x00]);
    }

    #[test]
    fn chunk_small_max_size() {
        let msg = vec![0x22; 25];
        let out = chunk_message(&msg, 10);
        // 3 chunks: 10 + 10 + 5
        assert_eq!(out.len(), (2 + 10) + (2 + 10) + (2 + 5) + 2);
        assert_eq!(u16::from_be_bytes([out[0], out[1]]), 10);
        assert_eq!(u16::from_be_bytes([out[12], out[13]]), 10);
        assert_eq!(u16::from_be_bytes([out[24], out[25]]), 5);
        assert_eq!(&out[31..], &[0x00, 0x00]);
    }

    #[test]
    fn chunk_max_size_one() {
        let msg = [0xAA, 0xBB, 0xCC];
        let out = chunk_message(&msg, 1);
        // 3 single-byte chunks + terminator
        assert_eq!(out.len(), 3 * (2 + 1) + 2);
        assert_eq!(&out[..3], &[0x00, 0x01, 0xAA]);
        assert_eq!(&out[3..6], &[0x00, 0x01, 0xBB]);
        assert_eq!(&out[6..9], &[0x00, 0x01, 0xCC]);
        assert_eq!(&out[9..], &[0x00, 0x00]);
    }

    #[test]
    #[should_panic(expected = "max_chunk_size must be > 0")]
    fn chunk_max_size_zero_panics() {
        chunk_message(&[1, 2, 3], 0);
    }

    // ---- Decoder tests ----

    #[test]
    fn decode_empty_message() {
        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(&[0x00, 0x00]).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_empty());
    }

    #[test]
    fn decode_single_chunk() {
        let mut dec = ChunkDecoder::new();
        let payload = [1, 2, 3, 4, 5];
        let mut data = vec![0x00, 0x05]; // header: 5
        data.extend_from_slice(&payload);
        data.extend_from_slice(&[0x00, 0x00]); // terminator
        let msgs = dec.feed(&data).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &payload);
    }

    #[test]
    fn decode_multi_chunk() {
        let mut dec = ChunkDecoder::new();
        let mut data = Vec::new();
        // Chunk 1: 3 bytes
        data.extend_from_slice(&[0x00, 0x03, 0xAA, 0xBB, 0xCC]);
        // Chunk 2: 2 bytes
        data.extend_from_slice(&[0x00, 0x02, 0xDD, 0xEE]);
        // Terminator
        data.extend_from_slice(&[0x00, 0x00]);

        let msgs = dec.feed(&data).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
    }

    #[test]
    fn decode_partial_header() {
        let mut dec = ChunkDecoder::new();
        // Feed only the high byte of the header.
        let msgs = dec.feed(&[0x00]).unwrap();
        assert!(msgs.is_empty());
        // Feed the low byte + payload + terminator.
        let mut rest = vec![0x03, 0x01, 0x02, 0x03];
        rest.extend_from_slice(&[0x00, 0x00]);
        let msgs = dec.feed(&rest).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn decode_partial_payload() {
        let mut dec = ChunkDecoder::new();
        // Feed header + first 2 bytes of a 5-byte payload.
        let msgs = dec.feed(&[0x00, 0x05, 0x01, 0x02]).unwrap();
        assert!(msgs.is_empty());
        // Feed remaining 3 bytes + terminator.
        let msgs = dec.feed(&[0x03, 0x04, 0x05, 0x00, 0x00]).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0x01, 0x02, 0x03, 0x04, 0x05]);
    }

    #[test]
    fn decode_byte_at_a_time() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut wire = vec![0x00, 0x04]; // header: 4
        wire.extend_from_slice(&payload);
        wire.extend_from_slice(&[0x00, 0x00]); // terminator

        let mut dec = ChunkDecoder::new();
        let mut all_msgs = Vec::new();
        for &b in &wire {
            let msgs = dec.feed(&[b]).unwrap();
            all_msgs.extend(msgs);
        }
        assert_eq!(all_msgs.len(), 1);
        assert_eq!(&all_msgs[0][..], &payload);
    }

    #[test]
    fn decode_multiple_messages_one_feed() {
        let mut data = Vec::new();
        // Message 1: [0x01, 0x02]
        data.extend_from_slice(&[0x00, 0x02, 0x01, 0x02, 0x00, 0x00]);
        // Message 2: [0x03]
        data.extend_from_slice(&[0x00, 0x01, 0x03, 0x00, 0x00]);

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(&data).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(&msgs[0][..], &[0x01, 0x02]);
        assert_eq!(&msgs[1][..], &[0x03]);
    }

    #[test]
    fn decode_no_complete_message() {
        let mut dec = ChunkDecoder::new();
        // Just a header and partial payload — no terminator.
        let msgs = dec.feed(&[0x00, 0x05, 0x01, 0x02]).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn decode_message_too_large() {
        let mut dec = ChunkDecoder::with_max_message_size(10);
        // Try to send a chunk of 20 bytes.
        let mut data = vec![0x00, 20]; // header: 20
        data.extend_from_slice(&[0x42; 20]);
        data.extend_from_slice(&[0x00, 0x00]);
        let result = dec.feed(&data);
        assert_eq!(
            result,
            Err(ChunkError::MessageTooLarge { size: 20, max: 10 })
        );
    }

    // ---- Round-trip tests ----

    #[test]
    fn round_trip_small() {
        let msg = b"hello bolt";
        let chunked = chunk_message(msg, 65535);
        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(&chunked).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], msg);
    }

    #[test]
    fn round_trip_large() {
        let msg = vec![0x42; 70_000];
        let chunked = chunk_message(&msg, 65535);
        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(&chunked).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &msg[..]);
    }

    #[test]
    fn round_trip_empty() {
        let chunked = chunk_message(&[], 65535);
        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(&chunked).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_empty());
    }

    #[test]
    fn round_trip_various_max_sizes() {
        let msg = vec![0xAB; 1000];
        for max_chunk in [1, 7, 100, 999, 1000, 1001, 65535] {
            let chunked = chunk_message(&msg, max_chunk);
            let mut dec = ChunkDecoder::new();
            let msgs = dec.feed(&chunked).unwrap();
            assert_eq!(msgs.len(), 1, "failed for max_chunk_size={max_chunk}");
            assert_eq!(
                &msgs[0][..],
                &msg[..],
                "payload mismatch for max_chunk_size={max_chunk}"
            );
        }
    }
}
