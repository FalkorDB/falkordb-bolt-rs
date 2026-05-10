// Bolt chunked transfer encoding — framing layer for PackStream messages.
//
// Every Bolt message on the wire is wrapped in one or more chunks:
//   Chunk     = [u16 BE size][size bytes of payload]
//   EndMarker = [0x00, 0x00]
//   Message   = Chunk+ EndMarker
//
// This module provides only the read-side de-chunker. The encode side will be
// fused into the writer (issue #10/#11) so framing happens during PackStream
// serialization with no intermediate buffer copy.

use bytes::BytesMut;

/// Default maximum message size the decoder will accept (16 MiB).
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during chunk decoding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    /// The accumulated message exceeded the configured maximum size.
    /// The decoder is now defunct and the connection must be closed.
    #[error("message size {size} exceeds maximum allowed size {max}")]
    MessageTooLarge { size: usize, max: usize },
    /// The decoder is in a defunct state after a previous fatal error.
    /// The connection must be closed and a new decoder created.
    #[error("decoder is defunct after a previous error")]
    Defunct,
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Threshold above which the internal buffer is shrunk after emitting a
/// message, to avoid retaining large allocations across many connections.
const BUFFER_SHRINK_THRESHOLD: usize = 256 * 1024; // 256 KiB

/// Internal state of the chunk decoder.
#[derive(Debug, Clone, Copy)]
enum DecoderState {
    /// Waiting for the first byte of the 2-byte chunk size header.
    ReadingHeader,
    /// Have the high byte of the header, waiting for the low byte.
    ReadingHeaderByte2 { first_byte: u8 },
    /// Reading payload bytes; `remaining` bytes still expected.
    ReadingPayload { remaining: u16 },
    /// A fatal error occurred; the decoder is unusable and the connection
    /// must be closed.
    Defunct,
}

/// Accumulates Bolt chunks from the wire and reassembles complete messages.
///
/// TCP can deliver data at arbitrary byte boundaries, so the decoder
/// maintains internal state across multiple [`feed`](Self::feed) calls.
/// Each completed message (delimited by a `0x0000` zero-chunk) is returned
/// as a contiguous `BytesMut` buffer suitable for zero-copy parsing by
/// `PackStreamReader`.
///
/// The contiguous-buffer guarantee is load-bearing: a single PackStream
/// value can straddle chunk boundaries, so the parser must see one
/// continuous payload to support `&'a str` / `&'a [u8]` zero-copy reads.
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
    /// exceeds the configured maximum size. The decoder becomes defunct
    /// after this error — the connection must be closed.
    ///
    /// Returns [`ChunkError::Defunct`] if the decoder was already in a
    /// defunct state from a previous error.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<BytesMut>, ChunkError> {
        if matches!(self.state, DecoderState::Defunct) {
            return Err(ChunkError::Defunct);
        }

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
                        self.state = DecoderState::ReadingHeaderByte2 {
                            first_byte: data[pos],
                        };
                        pos += 1;
                    }
                }
                DecoderState::ReadingHeaderByte2 { first_byte } => {
                    let size = u16::from_be_bytes([first_byte, data[pos]]);
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
                DecoderState::Defunct => return Err(ChunkError::Defunct),
            }
        }

        Ok(messages)
    }

    /// Process a completed chunk header.
    fn handle_header(&mut self, size: u16, messages: &mut Vec<BytesMut>) -> Result<(), ChunkError> {
        if size == 0 {
            // Zero-chunk = end of message. Emit the accumulated buffer.
            messages.push(self.buffer.split());

            // If the previous message grew the buffer beyond the shrink
            // threshold, release the allocation so we don't retain a
            // high-watermark across many small subsequent messages.
            if self.buffer.capacity() > BUFFER_SHRINK_THRESHOLD {
                self.buffer = BytesMut::new();
            }

            self.state = DecoderState::ReadingHeader;
        } else {
            // Check that adding this chunk won't exceed the limit.
            let new_size = self.buffer.len() + size as usize;
            if new_size > self.max_message_size {
                // Transition to Defunct — the remaining payload bytes in the
                // stream would be misinterpreted as headers if we tried to
                // continue. The connection must be closed.
                self.buffer.clear();
                self.state = DecoderState::Defunct;
                return Err(ChunkError::MessageTooLarge {
                    size: new_size,
                    max: self.max_message_size,
                });
            }
            // Reserve space for the incoming chunk to reduce reallocations
            // when large payloads arrive across many small feed() calls.
            self.buffer.reserve(size as usize);
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
        let mut data = vec![0x00, 0x14]; // header: 20
        data.extend_from_slice(&[0x42; 20]);
        data.extend_from_slice(&[0x00, 0x00]);
        let result = dec.feed(&data);
        assert_eq!(
            result,
            Err(ChunkError::MessageTooLarge { size: 20, max: 10 })
        );
    }

    #[test]
    fn decode_defunct_after_too_large() {
        let mut dec = ChunkDecoder::with_max_message_size(10);
        // Trigger MessageTooLarge.
        let result = dec.feed(&[0x00, 0x14]);
        assert!(matches!(result, Err(ChunkError::MessageTooLarge { .. })));
        // Any subsequent feed returns Defunct.
        let result = dec.feed(&[0x00, 0x01, 0x42, 0x00, 0x00]);
        assert_eq!(result, Err(ChunkError::Defunct));
    }
}
