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
//
// # Streaming architecture
//
// The decoder accumulates *references* to incoming `Bytes` (not their
// contents). Each emitted message is a [`MessageBuf`] — a multi-segment
// `bytes::Buf` view over the payload chunks that comprise the message. No
// payload byte is ever copied between buffers; segments are reference-counted
// slices of the original TCP read buffers.
//
// `PackStreamReader<MessageBuf>` walks the segments via the `Buf` trait;
// variable-length values that fit within a single segment are zero-copy
// slices; values that straddle segment boundaries trigger a single
// `copy_to_bytes` allocation for just that value (not the whole message).

use std::collections::VecDeque;

use bytes::{Buf, Bytes};

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
// MessageBuf — multi-segment Buf over a message's payload chunks
// ---------------------------------------------------------------------------

/// A complete Bolt message's payload, represented as one or more
/// reference-counted [`Bytes`] segments.
///
/// Implements [`bytes::Buf`] so the segments can be walked by
/// `PackStreamReader` without materializing them into a contiguous
/// allocation. Each segment is a zero-copy slice of an original TCP read
/// buffer.
pub struct MessageBuf {
    /// Payload chunks in order. The front element is the current segment.
    /// Empty when the buffer is fully consumed. `VecDeque` gives O(1)
    /// front-removal as segments are drained.
    chunks: VecDeque<Bytes>,
    /// Total bytes remaining across all chunks. Maintained explicitly so
    /// `Buf::remaining` is O(1).
    remaining: usize,
}

impl MessageBuf {
    /// Construct from a list of payload segments.
    ///
    /// Empty segments in the input are filtered out — the internal
    /// invariant is that `chunks` never contains an empty `Bytes`. This
    /// keeps `chunk()` non-empty whenever `remaining() > 0` without
    /// needing to re-trim after every pop in `advance`/`copy_to_bytes`.
    pub fn from_chunks(chunks: Vec<Bytes>) -> Self {
        let mut filtered = VecDeque::with_capacity(chunks.len());
        let mut remaining = 0usize;
        for c in chunks {
            if !c.is_empty() {
                remaining = remaining.saturating_add(c.len());
                filtered.push_back(c);
            }
        }
        Self {
            chunks: filtered,
            remaining,
        }
    }

    /// Number of underlying segments (not bytes). Used by tests to verify
    /// the decoder produced a single-segment or multi-segment buffer.
    #[cfg(test)]
    pub(crate) fn segment_count(&self) -> usize {
        self.chunks.len()
    }

    /// Borrow the underlying segments. Test-only.
    #[cfg(test)]
    pub(crate) fn segments(&self) -> &VecDeque<Bytes> {
        &self.chunks
    }
}

impl Buf for MessageBuf {
    fn remaining(&self) -> usize {
        self.remaining
    }

    fn chunk(&self) -> &[u8] {
        self.chunks.front().map(|b| b.as_ref()).unwrap_or(&[])
    }

    fn advance(&mut self, mut cnt: usize) {
        assert!(
            cnt <= self.remaining,
            "advance past end of MessageBuf ({cnt} > {})",
            self.remaining
        );
        self.remaining -= cnt;
        while cnt > 0 {
            let first = &mut self.chunks[0];
            let take = std::cmp::min(first.len(), cnt);
            first.advance(take);
            cnt -= take;
            if first.is_empty() {
                self.chunks.pop_front();
            }
        }
    }

    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        if len == 0 {
            return Bytes::new();
        }
        assert!(
            len <= self.remaining,
            "copy_to_bytes past end ({len} > {})",
            self.remaining
        );
        if self.chunks[0].len() >= len {
            // Zero-copy: split out from the current segment.
            let out = self.chunks[0].split_to(len);
            self.remaining -= len;
            if self.chunks[0].is_empty() {
                self.chunks.pop_front();
            }
            out
        } else {
            // Value straddles segment boundary — one allocation for just
            // this value, not for the whole message.
            let mut acc = bytes::BytesMut::with_capacity(len);
            let mut taken = 0;
            while taken < len {
                let first = &mut self.chunks[0];
                let take = std::cmp::min(first.len(), len - taken);
                acc.extend_from_slice(&first[..take]);
                first.advance(take);
                if first.is_empty() {
                    self.chunks.pop_front();
                }
                taken += take;
            }
            self.remaining -= len;
            acc.freeze()
        }
    }
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
    ReadingHeaderByte2 { first_byte: u8 },
    /// Reading payload bytes; `remaining` bytes still expected.
    ReadingPayload { remaining: u16 },
    /// A fatal error occurred; the decoder is unusable and the connection
    /// must be closed.
    Defunct,
}

/// Accumulates Bolt chunks from the wire and emits complete messages as
/// multi-segment [`MessageBuf`]s.
///
/// TCP can deliver data at arbitrary byte boundaries; the decoder
/// maintains state across multiple [`feed`](Self::feed) calls. **No
/// payload bytes are ever copied** — each emitted segment is a refcounted
/// slice of an input `Bytes`.
pub struct ChunkDecoder {
    /// Payload segments accumulated for the message currently being assembled.
    pending: Vec<Bytes>,
    /// Running total of bytes in `pending`, used for `max_message_size` checks.
    pending_size: usize,
    /// Parser state machine.
    state: DecoderState,
    /// Maximum allowed message size in bytes.
    max_message_size: usize,
}

impl ChunkDecoder {
    /// Create a new decoder with the default max message size (16 MiB).
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            pending_size: 0,
            state: DecoderState::ReadingHeader,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        }
    }

    /// Create a new decoder with a custom maximum message size.
    pub fn with_max_message_size(max: usize) -> Self {
        Self {
            pending: Vec::new(),
            pending_size: 0,
            state: DecoderState::ReadingHeader,
            max_message_size: max,
        }
    }

    /// Feed raw bytes from the wire. Returns any complete de-chunked messages.
    ///
    /// A single `feed` call may return zero, one, or multiple messages
    /// depending on how much data is provided. Returned [`MessageBuf`]s
    /// hold references into `data` (and possibly into prior feed inputs);
    /// the underlying allocations stay alive via reference counting.
    ///
    /// # Errors
    ///
    /// Returns [`ChunkError::MessageTooLarge`] if the accumulated message
    /// exceeds the configured maximum size. The decoder becomes defunct
    /// after this error — the connection must be closed.
    ///
    /// Returns [`ChunkError::Defunct`] if the decoder was already in a
    /// defunct state from a previous error.
    pub fn feed(&mut self, mut data: Bytes) -> Result<Vec<MessageBuf>, ChunkError> {
        if matches!(self.state, DecoderState::Defunct) {
            return Err(ChunkError::Defunct);
        }

        let mut messages = Vec::new();

        while !data.is_empty() {
            match self.state {
                DecoderState::ReadingHeader => {
                    if data.len() >= 2 {
                        let size = u16::from_be_bytes([data[0], data[1]]);
                        data.advance(2);
                        self.handle_header(size, &mut messages)?;
                    } else {
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
                    let to_take = std::cmp::min(remaining as usize, data.len());
                    // Zero-copy: split a slice off the front of the input.
                    let segment = data.split_to(to_take);
                    self.pending.push(segment);
                    // `handle_header` already verified `pending_size + size`
                    // fits under `max_message_size`; `saturating_add` is
                    // belt-and-suspenders for pathological `max_message_size`
                    // configurations near `usize::MAX`.
                    self.pending_size = self.pending_size.saturating_add(to_take);

                    let left = remaining - to_take as u16;
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
    fn handle_header(
        &mut self,
        size: u16,
        messages: &mut Vec<MessageBuf>,
    ) -> Result<(), ChunkError> {
        if size == 0 {
            // Zero-chunk = end of message. Emit a MessageBuf over the
            // accumulated segments.
            let segments = std::mem::take(&mut self.pending);
            self.pending_size = 0;
            messages.push(MessageBuf::from_chunks(segments));
            self.state = DecoderState::ReadingHeader;
        } else {
            // Overflow-safe size check on untrusted input. `checked_add`
            // catches the case where `max_message_size` is configured near
            // `usize::MAX` and `pending_size + size` would wrap.
            let new_size = self.pending_size.checked_add(size as usize);
            if new_size.is_none_or(|s| s > self.max_message_size) {
                // Defunct — the remaining payload bytes would be misinterpreted
                // as headers if we tried to continue. Connection must be closed.
                let reported_size = self.pending_size.saturating_add(size as usize);
                self.pending.clear();
                self.pending_size = 0;
                self.state = DecoderState::Defunct;
                return Err(ChunkError::MessageTooLarge {
                    size: reported_size,
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

    /// Drain a MessageBuf into a Vec<u8> for content assertions.
    fn collect(mut buf: MessageBuf) -> Vec<u8> {
        let mut out = Vec::with_capacity(buf.remaining());
        while buf.has_remaining() {
            let c = buf.chunk();
            out.extend_from_slice(c);
            let n = c.len();
            buf.advance(n);
        }
        out
    }

    /// True iff `slice`'s storage lies within `original`'s allocation range.
    fn ptr_within(slice: &Bytes, original_ptr: *const u8, original_len: usize) -> bool {
        let start = slice.as_ptr() as usize;
        let base = original_ptr as usize;
        start >= base && start < base + original_len
    }

    // ---- Basic decoder behavior ----

    #[test]
    fn decode_empty_message() {
        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(Bytes::from_static(&[0x00, 0x00])).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].remaining(), 0);
    }

    #[test]
    fn decode_single_chunk() {
        let mut dec = ChunkDecoder::new();
        let payload = [1, 2, 3, 4, 5];
        let mut data = vec![0x00, 0x05];
        data.extend_from_slice(&payload);
        data.extend_from_slice(&[0x00, 0x00]);
        let msgs = dec.feed(Bytes::from(data)).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(collect(msgs.into_iter().next().unwrap()), payload.to_vec());
    }

    #[test]
    fn decode_multi_chunk() {
        let mut dec = ChunkDecoder::new();
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x03, 0xAA, 0xBB, 0xCC]);
        data.extend_from_slice(&[0x00, 0x02, 0xDD, 0xEE]);
        data.extend_from_slice(&[0x00, 0x00]);

        let msgs = dec.feed(Bytes::from(data)).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            collect(msgs.into_iter().next().unwrap()),
            vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE]
        );
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
        assert_eq!(
            collect(msgs.into_iter().next().unwrap()),
            vec![0x01, 0x02, 0x03]
        );
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
        assert_eq!(
            collect(msgs.into_iter().next().unwrap()),
            vec![0x01, 0x02, 0x03, 0x04, 0x05]
        );
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
        assert_eq!(
            collect(all_msgs.into_iter().next().unwrap()),
            payload.to_vec()
        );
    }

    #[test]
    fn decode_multiple_messages_one_feed() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x02, 0x01, 0x02, 0x00, 0x00]);
        data.extend_from_slice(&[0x00, 0x01, 0x03, 0x00, 0x00]);

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(Bytes::from(data)).unwrap();
        assert_eq!(msgs.len(), 2);
        let mut it = msgs.into_iter();
        assert_eq!(collect(it.next().unwrap()), vec![0x01, 0x02]);
        assert_eq!(collect(it.next().unwrap()), vec![0x03]);
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
        match result {
            Err(ChunkError::MessageTooLarge { size, max }) => {
                assert_eq!(size, 20);
                assert_eq!(max, 10);
            }
            Err(e) => panic!("expected MessageTooLarge, got error: {e}"),
            Ok(_) => panic!("expected MessageTooLarge, got Ok"),
        }
    }

    #[test]
    fn decode_defunct_after_too_large() {
        let mut dec = ChunkDecoder::with_max_message_size(10);
        let result = dec.feed(Bytes::from_static(&[0x00, 0x14]));
        assert!(matches!(result, Err(ChunkError::MessageTooLarge { .. })));
        let result = dec.feed(Bytes::from_static(&[0x00, 0x01, 0x42, 0x00, 0x00]));
        assert!(matches!(result, Err(ChunkError::Defunct)));
    }

    // ---- Zero-copy verification ----

    #[test]
    fn decode_single_chunk_is_zero_copy() {
        // Single-chunk message in one feed → emitted MessageBuf has one
        // segment that is a slice of the input allocation.
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
        assert_eq!(msgs[0].segment_count(), 1);
        assert!(
            ptr_within(&msgs[0].segments()[0], input_ptr, input_len),
            "single chunk must share allocation with input"
        );
    }

    #[test]
    fn decode_multi_chunk_is_zero_copy() {
        // Multi-chunk message → MessageBuf has N segments, EACH a slice of
        // the input. This is the headline assertion: even multi-chunk
        // messages no longer trigger a copy.
        let mut wire = Vec::new();
        wire.extend_from_slice(&[0x00, 0x03, 0xAA, 0xBB, 0xCC]); // chunk 1
        wire.extend_from_slice(&[0x00, 0x02, 0xDD, 0xEE]); // chunk 2
        wire.extend_from_slice(&[0x00, 0x00]); // terminator
        let input = Bytes::from(wire);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(input).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].segment_count(), 2);
        for seg in msgs[0].segments() {
            assert!(
                ptr_within(seg, input_ptr, input_len),
                "every segment must be a slice of the input allocation"
            );
        }
    }

    #[test]
    fn decode_fragmented_is_zero_copy() {
        // Payload split across two feeds. Each segment must point into
        // *its own* feed's allocation — no inter-buffer copying.
        let feed1 = Bytes::from(vec![0x00, 0x05, 0x01, 0x02]); // header + 2 payload bytes
        let feed1_ptr = feed1.as_ptr();
        let feed1_len = feed1.len();

        let feed2 = Bytes::from(vec![0x03, 0x04, 0x05, 0x00, 0x00]); // 3 payload + terminator
        let feed2_ptr = feed2.as_ptr();
        let feed2_len = feed2.len();

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(feed1).unwrap();
        assert!(msgs.is_empty());
        let msgs = dec.feed(feed2).unwrap();
        assert_eq!(msgs.len(), 1);
        let segs = msgs[0].segments();
        assert_eq!(segs.len(), 2, "one segment per feed contribution");
        assert!(
            ptr_within(&segs[0], feed1_ptr, feed1_len),
            "first segment must be from feed 1"
        );
        assert!(
            ptr_within(&segs[1], feed2_ptr, feed2_len),
            "second segment must be from feed 2"
        );
    }

    #[test]
    fn decode_multiple_messages_zero_copy() {
        // Two complete single-chunk messages in one feed — each msg
        // has one segment that's a slice of the input.
        let mut wire = Vec::new();
        wire.extend_from_slice(&[0x00, 0x02, 0x01, 0x02, 0x00, 0x00]);
        wire.extend_from_slice(&[0x00, 0x03, 0x0A, 0x0B, 0x0C, 0x00, 0x00]);
        let input = Bytes::from(wire);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(input).unwrap();
        assert_eq!(msgs.len(), 2);
        for msg in &msgs {
            assert_eq!(msg.segment_count(), 1);
            assert!(ptr_within(&msg.segments()[0], input_ptr, input_len));
        }
    }

    #[test]
    fn decode_mixed_complete_and_partial() {
        // First message complete in this feed; second one's first chunk
        // begins but doesn't terminate.
        let mut wire = Vec::new();
        wire.extend_from_slice(&[0x00, 0x02, 0x01, 0x02, 0x00, 0x00]); // msg 1
        wire.extend_from_slice(&[0x00, 0x03, 0x0A, 0x0B, 0x0C]); // msg 2 first chunk, no terminator
        let input = Bytes::from(wire);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut dec = ChunkDecoder::new();
        let msgs = dec.feed(input).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(ptr_within(&msgs[0].segments()[0], input_ptr, input_len));

        let msgs = dec
            .feed(Bytes::from_static(&[0x00, 0x02, 0x0D, 0x0E, 0x00, 0x00]))
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            collect(msgs.into_iter().next().unwrap()),
            vec![0x0A, 0x0B, 0x0C, 0x0D, 0x0E]
        );
    }

    // ---- MessageBuf Buf-trait correctness ----

    #[test]
    fn messagebuf_buf_impl_single_segment() {
        let mut buf = MessageBuf::from_chunks(vec![Bytes::from_static(&[1, 2, 3, 4])]);
        assert_eq!(buf.remaining(), 4);
        assert_eq!(buf.chunk(), &[1, 2, 3, 4]);
        assert_eq!(buf.get_u8(), 1);
        assert_eq!(buf.remaining(), 3);
        assert_eq!(buf.get_u16(), 0x0203);
        assert_eq!(buf.remaining(), 1);
        buf.advance(1);
        assert_eq!(buf.remaining(), 0);
    }

    #[test]
    fn messagebuf_buf_impl_multi_segment_advance() {
        let mut buf = MessageBuf::from_chunks(vec![
            Bytes::from_static(&[1, 2]),
            Bytes::from_static(&[3, 4]),
            Bytes::from_static(&[5, 6]),
        ]);
        assert_eq!(buf.remaining(), 6);
        buf.advance(3); // crosses segment 1→2
        assert_eq!(buf.remaining(), 3);
        assert_eq!(buf.chunk(), &[4]); // mid-segment-2
        buf.advance(1);
        assert_eq!(buf.chunk(), &[5, 6]);
    }

    #[test]
    fn messagebuf_copy_to_bytes_within_segment_is_zero_copy() {
        let original = Bytes::from(vec![10, 20, 30, 40, 50]);
        let ptr = original.as_ptr();
        let mut buf = MessageBuf::from_chunks(vec![original]);
        let got = buf.copy_to_bytes(3);
        assert_eq!(&got[..], &[10, 20, 30]);
        assert_eq!(got.as_ptr() as usize, ptr as usize);
    }

    #[test]
    fn messagebuf_copy_to_bytes_across_segments_allocates() {
        let mut buf = MessageBuf::from_chunks(vec![
            Bytes::from_static(&[1, 2]),
            Bytes::from_static(&[3, 4]),
        ]);
        let got = buf.copy_to_bytes(3);
        assert_eq!(&got[..], &[1, 2, 3]);
        assert_eq!(buf.remaining(), 1);
        assert_eq!(buf.chunk(), &[4]);
    }

    #[test]
    fn messagebuf_empty_chunks_are_skipped() {
        // Empty segments anywhere in the input (front, middle, trailing) must
        // be filtered out so the invariant `chunk() non-empty iff remaining > 0`
        // holds throughout advance / copy_to_bytes.
        let mut buf = MessageBuf::from_chunks(vec![
            Bytes::new(),
            Bytes::from_static(&[1]),
            Bytes::new(),
            Bytes::from_static(&[2]),
            Bytes::new(),
        ]);
        assert_eq!(buf.remaining(), 2);
        assert_eq!(buf.segment_count(), 2);
        assert_eq!(buf.chunk(), &[1]);
        buf.advance(1);
        // After consuming the first segment, the next must still be non-empty
        // (middle Bytes::new() was filtered at construction).
        assert_eq!(buf.remaining(), 1);
        assert_eq!(buf.chunk(), &[2]);
        buf.advance(1);
        assert_eq!(buf.remaining(), 0);
        assert!(buf.chunk().is_empty());
    }

    #[test]
    fn messagebuf_chunk_non_empty_invariant_during_drain() {
        // While remaining() > 0, chunk() must always return a non-empty slice
        // so loops like `while has_remaining { advance(chunk().len()) }` make
        // forward progress.
        let mut buf = MessageBuf::from_chunks(vec![
            Bytes::from_static(&[1, 2]),
            Bytes::from_static(&[3]),
            Bytes::from_static(&[4, 5, 6]),
        ]);
        let mut steps = 0;
        while buf.has_remaining() {
            let n = buf.chunk().len();
            assert!(n > 0, "chunk() must be non-empty while remaining > 0");
            buf.advance(n);
            steps += 1;
            assert!(steps < 10, "loop is not making progress");
        }
        assert_eq!(buf.remaining(), 0);
    }

    #[test]
    fn messagebuf_copy_to_bytes_zero_on_empty_buf() {
        // copy_to_bytes(0) on a fully-consumed buf must not panic — required
        // for reading empty strings/bytes at end of message.
        let mut buf = MessageBuf::from_chunks(vec![]);
        assert_eq!(buf.remaining(), 0);
        let got = buf.copy_to_bytes(0);
        assert!(got.is_empty());
    }

    #[test]
    fn messagebuf_copy_to_bytes_zero_on_drained_buf() {
        // After draining all bytes, copy_to_bytes(0) still works.
        let mut buf = MessageBuf::from_chunks(vec![Bytes::from_static(&[1, 2, 3])]);
        buf.advance(3);
        assert_eq!(buf.remaining(), 0);
        assert!(buf.copy_to_bytes(0).is_empty());
    }

    #[test]
    fn decode_oversized_chunk_rejected_at_header() {
        // A chunk whose header announces a size > max_message_size is
        // rejected immediately on header parse — before any payload bytes
        // are accepted — and transitions the decoder to Defunct.
        //
        // Note: the `checked_add` guard in `handle_header` also defends
        // against `pending_size + size` wrapping when `max_message_size`
        // is configured near `usize::MAX`, but constructing that scenario
        // requires accumulating ~usize::MAX bytes which is not feasible
        // in a unit test. The guard is exercised by the code path here;
        // its overflow behavior is verified by inspection.
        let mut dec = ChunkDecoder::with_max_message_size(100);
        let mut data = vec![0xFF, 0xFF]; // header announces size 65535 > max 100
        data.extend_from_slice(&[0x42; 10]);
        let result = dec.feed(Bytes::from(data));
        assert!(matches!(result, Err(ChunkError::MessageTooLarge { .. })));
        // Subsequent feeds return Defunct.
        let result = dec.feed(Bytes::from_static(&[0x00, 0x01, 0x42, 0x00, 0x00]));
        assert!(matches!(result, Err(ChunkError::Defunct)));
    }
}
