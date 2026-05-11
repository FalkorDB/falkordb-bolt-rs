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

use bytes::{Buf, Bytes, BytesMut};

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
/// as a contiguous `Bytes` buffer suitable for zero-copy parsing by
/// `PackStreamReader`.
///
/// The contiguous-buffer guarantee is load-bearing: a single PackStream
/// value can straddle chunk boundaries, so the parser must see one
/// continuous payload to support `&'a str` / `&'a [u8]` zero-copy reads.
///
/// # Zero-copy fast path
///
/// When a complete single-chunk message arrives in a single `feed` call —
/// the common case for small Bolt messages on a healthy TCP link — the
/// decoder returns a zero-copy `Bytes::slice` of the input. Multi-chunk
/// messages and TCP-fragmented messages fall back to the accumulating
/// slow path.
pub struct ChunkDecoder {
    /// Accumulation buffer used by the slow path. Empty when the fast path
    /// is eligible.
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
    pub fn feed(&mut self, mut data: Bytes) -> Result<Vec<Bytes>, ChunkError> {
        if matches!(self.state, DecoderState::Defunct) {
            return Err(ChunkError::Defunct);
        }

        let mut messages = Vec::new();

        while !data.is_empty() {
            // Fast path: at message boundary, with no accumulated state, and
            // the start of `data` contains a complete single-chunk message.
            // Slice the payload directly out of `data` — no copy.
            if matches!(self.state, DecoderState::ReadingHeader)
                && self.buffer.is_empty()
                && data.len() >= 2
            {
                let size = u16::from_be_bytes([data[0], data[1]]) as usize;
                if size == 0 {
                    // Empty message — just a zero-chunk terminator.
                    messages.push(Bytes::new());
                    data.advance(2);
                    continue;
                }
                if size <= self.max_message_size
                    && data.len() >= 2 + size + 2
                    && data[2 + size] == 0
                    && data[2 + size + 1] == 0
                {
                    // Single-chunk message fully present: zero-copy slice.
                    messages.push(data.slice(2..2 + size));
                    data.advance(2 + size + 2);
                    continue;
                }
                // Otherwise: size > max (slow path will reject), or message
                // is multi-chunk, or message is fragmented. Fall through.
            }

            // Slow path: walk the state machine across the available bytes,
            // accumulating payload into self.buffer.
            match self.state {
                DecoderState::ReadingHeader => {
                    if data.len() >= 2 {
                        let size = u16::from_be_bytes([data[0], data[1]]);
                        data.advance(2);
                        self.handle_header(size, &mut messages)?;
                    } else {
                        // Only one byte — stash it for next feed.
                        self.state = DecoderState::ReadingHeaderByte2 {
                            first_byte: data[0],
                        };
                        data.advance(1);
                    }
                }
                DecoderState::ReadingHeaderByte2 { first_byte } => {
                    let size = u16::from_be_bytes([first_byte, data[0]]);
                    data.advance(1);
                    self.handle_header(size, &mut messages)?;
                }
                DecoderState::ReadingPayload { remaining } => {
                    let to_copy = std::cmp::min(remaining as usize, data.len());
                    self.buffer.extend_from_slice(&data[..to_copy]);
                    data.advance(to_copy);

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

    /// Process a completed chunk header on the slow path.
    fn handle_header(&mut self, size: u16, messages: &mut Vec<Bytes>) -> Result<(), ChunkError> {
        if size == 0 {
            // Zero-chunk = end of message. Emit the accumulated buffer.
            messages.push(self.buffer.split().freeze());

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

    /// True iff `slice`'s storage lies within `original`'s allocation range.
    /// Used to verify fast-path zero-copy slicing.
    fn ptr_within(slice: &Bytes, original_ptr: *const u8, original_len: usize) -> bool {
        let start = slice.as_ptr() as usize;
        let base = original_ptr as usize;
        start >= base && start < base + original_len
    }

    // ---- Basic decoder behavior (covers both fast and slow paths). ----

    #[test]
    fn decode_empty_message() {
        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(Bytes::from_static(&[0x00, 0x00])).unwrap();
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
        let msgs = dec.feed(Bytes::from(data)).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &payload);
    }

    #[test]
    fn decode_multi_chunk() {
        let mut dec = ChunkDecoder::new();
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x03, 0xAA, 0xBB, 0xCC]);
        data.extend_from_slice(&[0x00, 0x02, 0xDD, 0xEE]);
        data.extend_from_slice(&[0x00, 0x00]); // terminator

        let msgs = dec.feed(Bytes::from(data)).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
    }

    #[test]
    fn decode_partial_header() {
        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(Bytes::from_static(&[0x00])).unwrap();
        assert!(msgs.is_empty());

        let mut rest = vec![0x03, 0x01, 0x02, 0x03];
        rest.extend_from_slice(&[0x00, 0x00]);
        let msgs = dec.feed(Bytes::from(rest)).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn decode_partial_payload() {
        let mut dec = ChunkDecoder::new();
        let msgs = dec
            .feed(Bytes::from_static(&[0x00, 0x05, 0x01, 0x02]))
            .unwrap();
        assert!(msgs.is_empty());
        let msgs = dec
            .feed(Bytes::from_static(&[0x03, 0x04, 0x05, 0x00, 0x00]))
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0x01, 0x02, 0x03, 0x04, 0x05]);
    }

    #[test]
    fn decode_byte_at_a_time() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut wire = vec![0x00, 0x04];
        wire.extend_from_slice(&payload);
        wire.extend_from_slice(&[0x00, 0x00]);

        let mut dec = ChunkDecoder::new();
        let mut all_msgs = Vec::new();
        for &b in &wire {
            let msgs = dec.feed(Bytes::copy_from_slice(&[b])).unwrap();
            all_msgs.extend(msgs);
        }
        assert_eq!(all_msgs.len(), 1);
        assert_eq!(&all_msgs[0][..], &payload);
    }

    #[test]
    fn decode_multiple_messages_one_feed() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x02, 0x01, 0x02, 0x00, 0x00]);
        data.extend_from_slice(&[0x00, 0x01, 0x03, 0x00, 0x00]);

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(Bytes::from(data)).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(&msgs[0][..], &[0x01, 0x02]);
        assert_eq!(&msgs[1][..], &[0x03]);
    }

    #[test]
    fn decode_no_complete_message() {
        let mut dec = ChunkDecoder::new();
        let msgs = dec
            .feed(Bytes::from_static(&[0x00, 0x05, 0x01, 0x02]))
            .unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn decode_message_too_large() {
        let mut dec = ChunkDecoder::with_max_message_size(10);
        let mut data = vec![0x00, 0x14]; // header: 20
        data.extend_from_slice(&[0x42; 20]);
        data.extend_from_slice(&[0x00, 0x00]);
        let result = dec.feed(Bytes::from(data));
        assert_eq!(
            result,
            Err(ChunkError::MessageTooLarge { size: 20, max: 10 })
        );
    }

    #[test]
    fn decode_defunct_after_too_large() {
        let mut dec = ChunkDecoder::with_max_message_size(10);
        let result = dec.feed(Bytes::from_static(&[0x00, 0x14]));
        assert!(matches!(result, Err(ChunkError::MessageTooLarge { .. })));
        let result = dec.feed(Bytes::from_static(&[0x00, 0x01, 0x42, 0x00, 0x00]));
        assert_eq!(result, Err(ChunkError::Defunct));
    }

    // ---- Fast-path zero-copy verification. ----

    #[test]
    fn decode_fast_path_zero_copy() {
        // A complete single-chunk message in one feed must be returned as a
        // zero-copy slice of the input — no allocation in the decoder.
        let payload = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let mut wire = vec![0x00, 0x05];
        wire.extend_from_slice(&payload);
        wire.extend_from_slice(&[0x00, 0x00]);
        let input = Bytes::from(wire);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(input).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &payload);
        assert!(
            ptr_within(&msgs[0], input_ptr, input_len),
            "fast path should return a slice of the input, not a copy"
        );
    }

    #[test]
    fn decode_fast_path_multiple_messages() {
        // Two single-chunk messages in one feed — both should be zero-copy.
        let mut wire = Vec::new();
        wire.extend_from_slice(&[0x00, 0x02, 0x01, 0x02, 0x00, 0x00]);
        wire.extend_from_slice(&[0x00, 0x03, 0x0A, 0x0B, 0x0C, 0x00, 0x00]);
        let input = Bytes::from(wire);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(input).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(&msgs[0][..], &[0x01, 0x02]);
        assert_eq!(&msgs[1][..], &[0x0A, 0x0B, 0x0C]);
        assert!(ptr_within(&msgs[0], input_ptr, input_len));
        assert!(ptr_within(&msgs[1], input_ptr, input_len));
    }

    #[test]
    fn decode_fast_path_falls_back_on_multi_chunk() {
        // Multi-chunk message must still decode correctly via the slow path.
        // The output buffer is freshly allocated, not a slice of the input.
        let mut wire = Vec::new();
        wire.extend_from_slice(&[0x00, 0x03, 0xAA, 0xBB, 0xCC]);
        wire.extend_from_slice(&[0x00, 0x02, 0xDD, 0xEE]);
        wire.extend_from_slice(&[0x00, 0x00]);
        let input = Bytes::from(wire);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(input).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
        assert!(
            !ptr_within(&msgs[0], input_ptr, input_len),
            "multi-chunk path must reassemble into a fresh buffer, not slice the input"
        );
    }

    #[test]
    fn decode_mixed_fast_slow() {
        // First message is single-chunk (fast path). Second starts but
        // doesn't complete in this feed (multi-chunk straddling feeds).
        let mut wire = Vec::new();
        wire.extend_from_slice(&[0x00, 0x02, 0x01, 0x02, 0x00, 0x00]); // msg 1: [0x01, 0x02]
        wire.extend_from_slice(&[0x00, 0x03, 0x0A, 0x0B, 0x0C]); // msg 2: first chunk, no terminator
        let input = Bytes::from(wire);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(input).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0x01, 0x02]);
        assert!(
            ptr_within(&msgs[0], input_ptr, input_len),
            "first message should still be zero-copy even with partial follow-up"
        );

        // Finish message 2.
        let msgs = dec
            .feed(Bytes::from_static(&[0x00, 0x02, 0x0D, 0x0E, 0x00, 0x00]))
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(&msgs[0][..], &[0x0A, 0x0B, 0x0C, 0x0D, 0x0E]);
    }
}
