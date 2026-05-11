// PackStream read (deserialization) — streaming, multi-segment reader.

use bytes::{Buf, Bytes};

use crate::packstream::marker::Marker;

/// Errors that can occur during PackStream deserialization.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PackStreamError {
    /// Not enough bytes remaining in the buffer.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// The marker byte does not match the expected type.
    #[error("invalid marker: 0x{0:02X}")]
    InvalidMarker(u8),
    /// A string value contains invalid UTF-8.
    #[error("invalid UTF-8 in string")]
    InvalidUtf8,
}

/// A UTF-8-valid string backed by a refcounted [`Bytes`].
///
/// Cheap to clone (Arc refcount bump). Derefs to `&str` for ergonomic use.
/// The UTF-8 invariant is enforced when constructed by [`PackStreamReader::read_string`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackStreamString(Bytes);

impl PackStreamString {
    /// Construct from already-validated UTF-8 bytes.
    ///
    /// # Safety
    /// The caller must guarantee `bytes` is valid UTF-8. Used internally by
    /// the reader after `std::str::from_utf8` succeeds.
    pub(crate) unsafe fn from_utf8_unchecked(bytes: Bytes) -> Self {
        Self(bytes)
    }

    /// View as `&str`. No allocation, no copy.
    pub fn as_str(&self) -> &str {
        // SAFETY: invariant enforced at construction by `read_string`.
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }

    /// Consume into the underlying refcounted bytes.
    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

impl std::ops::Deref for PackStreamString {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for PackStreamString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Reads PackStream-encoded data from any [`Buf`] source.
///
/// Variable-length values (`read_string`, `read_bytes`) return refcounted
/// [`Bytes`] / [`PackStreamString`]. When the value fits within the
/// current segment of the underlying `Buf` (the typical case), the returned
/// bytes share the source allocation — zero copy. When a value straddles
/// segment boundaries, one allocation copies the value into a fresh buffer.
///
/// All multi-byte integer reads use the `Buf` trait's big-endian getters —
/// safe across segment boundaries and on strict-alignment architectures.
pub struct PackStreamReader<B: Buf> {
    buf: B,
}

impl<B: Buf> PackStreamReader<B> {
    /// Create a new reader over any `Buf` source.
    pub fn new(buf: B) -> Self {
        Self { buf }
    }

    /// Peek at the next byte without consuming it.
    pub fn peek(&self) -> Option<u8> {
        if self.buf.remaining() == 0 {
            None
        } else {
            self.buf.chunk().first().copied()
        }
    }

    /// Number of bytes remaining.
    pub fn remaining(&self) -> usize {
        self.buf.remaining()
    }

    // ---- internal helpers ----

    fn ensure(&self, n: usize) -> Result<(), PackStreamError> {
        if self.buf.remaining() < n {
            Err(PackStreamError::UnexpectedEof)
        } else {
            Ok(())
        }
    }

    fn read_u8(&mut self) -> Result<u8, PackStreamError> {
        self.ensure(1)?;
        Ok(self.buf.get_u8())
    }

    fn read_i8(&mut self) -> Result<i8, PackStreamError> {
        self.ensure(1)?;
        Ok(self.buf.get_i8())
    }

    fn read_i16(&mut self) -> Result<i16, PackStreamError> {
        self.ensure(2)?;
        Ok(self.buf.get_i16())
    }

    fn read_i32(&mut self) -> Result<i32, PackStreamError> {
        self.ensure(4)?;
        Ok(self.buf.get_i32())
    }

    fn read_i64(&mut self) -> Result<i64, PackStreamError> {
        self.ensure(8)?;
        Ok(self.buf.get_i64())
    }

    fn read_u16(&mut self) -> Result<u16, PackStreamError> {
        self.ensure(2)?;
        Ok(self.buf.get_u16())
    }

    fn read_u32(&mut self) -> Result<u32, PackStreamError> {
        self.ensure(4)?;
        Ok(self.buf.get_u32())
    }

    /// Read `n` bytes as a refcounted `Bytes`. Zero-copy when the data lies
    /// within a single underlying segment; allocates only when straddling.
    fn read_exact_bytes(&mut self, n: usize) -> Result<Bytes, PackStreamError> {
        self.ensure(n)?;
        Ok(self.buf.copy_to_bytes(n))
    }

    /// Advance past `n` bytes without retaining them (for `skip_value`).
    fn skip_bytes(&mut self, n: usize) -> Result<(), PackStreamError> {
        self.ensure(n)?;
        self.buf.advance(n);
        Ok(())
    }

    // ---- marker parsing ----

    fn read_marker(&mut self) -> Result<Marker, PackStreamError> {
        let b = self.read_u8()?;
        Marker::from_byte(b).map_err(|_| PackStreamError::InvalidMarker(b))
    }

    // ---- size helpers for containers/strings/bytes ----

    fn string_len(&mut self, marker: Marker) -> Result<usize, PackStreamError> {
        match marker {
            Marker::TinyString(size) => Ok(size as usize),
            Marker::String8 => Ok(self.read_u8()? as usize),
            Marker::String16 => Ok(self.read_u16()? as usize),
            Marker::String32 => Ok(self.read_u32()? as usize),
            _ => Err(PackStreamError::InvalidMarker(marker.byte())),
        }
    }

    fn bytes_len(&mut self, marker: Marker) -> Result<usize, PackStreamError> {
        match marker {
            Marker::Bytes8 => Ok(self.read_u8()? as usize),
            Marker::Bytes16 => Ok(self.read_u16()? as usize),
            Marker::Bytes32 => Ok(self.read_u32()? as usize),
            _ => Err(PackStreamError::InvalidMarker(marker.byte())),
        }
    }

    fn list_len(&mut self, marker: Marker) -> Result<u32, PackStreamError> {
        match marker {
            Marker::TinyList(size) => Ok(size as u32),
            Marker::List8 => Ok(self.read_u8()? as u32),
            Marker::List16 => Ok(self.read_u16()? as u32),
            Marker::List32 => Ok(self.read_u32()?),
            _ => Err(PackStreamError::InvalidMarker(marker.byte())),
        }
    }

    fn map_len(&mut self, marker: Marker) -> Result<u32, PackStreamError> {
        match marker {
            Marker::TinyMap(size) => Ok(size as u32),
            Marker::Map8 => Ok(self.read_u8()? as u32),
            Marker::Map16 => Ok(self.read_u16()? as u32),
            Marker::Map32 => Ok(self.read_u32()?),
            _ => Err(PackStreamError::InvalidMarker(marker.byte())),
        }
    }

    fn struct_len(&mut self, marker: Marker) -> Result<u32, PackStreamError> {
        match marker {
            Marker::TinyStruct(size) => Ok(size as u32),
            Marker::Struct8 => Ok(self.read_u8()? as u32),
            Marker::Struct16 => Ok(self.read_u16()? as u32),
            _ => Err(PackStreamError::InvalidMarker(marker.byte())),
        }
    }

    // ---- public read methods ----

    /// Consume a NULL marker.
    pub fn read_null(&mut self) -> Result<(), PackStreamError> {
        let marker = self.read_marker()?;
        if marker == Marker::Null {
            Ok(())
        } else {
            Err(PackStreamError::InvalidMarker(marker.byte()))
        }
    }

    /// Read a boolean value.
    pub fn read_bool(&mut self) -> Result<bool, PackStreamError> {
        let marker = self.read_marker()?;
        match marker {
            Marker::True => Ok(true),
            Marker::False => Ok(false),
            _ => Err(PackStreamError::InvalidMarker(marker.byte())),
        }
    }

    /// Read an integer value from any int marker (TINY_INT, INT8, INT16, INT32, INT64).
    pub fn read_int(&mut self) -> Result<i64, PackStreamError> {
        let marker = self.read_marker()?;
        match marker {
            Marker::TinyInt(v) => Ok(v as i64),
            Marker::Int8 => Ok(self.read_i8()? as i64),
            Marker::Int16 => Ok(self.read_i16()? as i64),
            Marker::Int32 => Ok(self.read_i32()? as i64),
            Marker::Int64 => self.read_i64(),
            _ => Err(PackStreamError::InvalidMarker(marker.byte())),
        }
    }

    /// Read a 64-bit float value.
    pub fn read_float(&mut self) -> Result<f64, PackStreamError> {
        let marker = self.read_marker()?;
        if marker != Marker::Float64 {
            return Err(PackStreamError::InvalidMarker(marker.byte()));
        }
        self.ensure(8)?;
        Ok(self.buf.get_f64())
    }

    /// Read a string value as a refcounted [`PackStreamString`].
    ///
    /// Zero-copy when the value fits within a single segment of the
    /// underlying `Buf`. Allocates once when the value straddles segments.
    /// UTF-8 is validated before return.
    pub fn read_string(&mut self) -> Result<PackStreamString, PackStreamError> {
        let marker = self.read_marker()?;
        let len = self.string_len(marker)?;
        let bytes = self.read_exact_bytes(len)?;
        std::str::from_utf8(&bytes).map_err(|_| PackStreamError::InvalidUtf8)?;
        // SAFETY: validated immediately above.
        Ok(unsafe { PackStreamString::from_utf8_unchecked(bytes) })
    }

    /// Read a byte array as refcounted [`Bytes`].
    ///
    /// Zero-copy when the value fits within a single segment of the
    /// underlying `Buf`. Allocates once when the value straddles segments.
    pub fn read_bytes(&mut self) -> Result<Bytes, PackStreamError> {
        let marker = self.read_marker()?;
        let len = self.bytes_len(marker)?;
        self.read_exact_bytes(len)
    }

    /// Read a list header and return the number of elements.
    pub fn read_list_header(&mut self) -> Result<u32, PackStreamError> {
        let marker = self.read_marker()?;
        self.list_len(marker)
    }

    /// Read a map header and return the number of entries.
    pub fn read_map_header(&mut self) -> Result<u32, PackStreamError> {
        let marker = self.read_marker()?;
        self.map_len(marker)
    }

    /// Read a struct header and return `(tag_byte, num_fields)`.
    pub fn read_struct_header(&mut self) -> Result<(u8, u32), PackStreamError> {
        let marker = self.read_marker()?;
        let num_fields = self.struct_len(marker)?;
        let tag = self.read_u8()?;
        Ok((tag, num_fields))
    }

    /// Skip any value without deserializing it. Uses a flat `u64` counter
    /// instead of recursion — safe against deeply nested untrusted input
    /// without risking stack overflow or heap allocation.
    pub fn skip_value(&mut self) -> Result<(), PackStreamError> {
        let mut remaining: u64 = 1;

        while remaining > 0 {
            remaining -= 1;

            let marker = self.read_marker()?;
            match marker {
                // Zero-byte scalars — marker is the entire value.
                Marker::Null | Marker::True | Marker::False | Marker::TinyInt(_) => {}

                // Fixed-width scalars — skip the payload bytes.
                Marker::Int8 => self.skip_bytes(1)?,
                Marker::Int16 => self.skip_bytes(2)?,
                Marker::Int32 => self.skip_bytes(4)?,
                Marker::Int64 | Marker::Float64 => self.skip_bytes(8)?,

                // Variable-length byte sequences — read length, skip that many bytes.
                Marker::TinyString(_) | Marker::String8 | Marker::String16 | Marker::String32 => {
                    let len = self.string_len(marker)?;
                    self.skip_bytes(len)?;
                }
                Marker::Bytes8 | Marker::Bytes16 | Marker::Bytes32 => {
                    let len = self.bytes_len(marker)?;
                    self.skip_bytes(len)?;
                }

                // Containers — add child count to remaining values.
                Marker::TinyList(_) | Marker::List8 | Marker::List16 | Marker::List32 => {
                    let len = self.list_len(marker)? as u64;
                    remaining = remaining
                        .checked_add(len)
                        .ok_or(PackStreamError::UnexpectedEof)?;
                }
                Marker::TinyMap(_) | Marker::Map8 | Marker::Map16 | Marker::Map32 => {
                    // Each map entry has a key + value = 2 values per entry.
                    let entries = (self.map_len(marker)? as u64)
                        .checked_mul(2)
                        .ok_or(PackStreamError::UnexpectedEof)?;
                    remaining = remaining
                        .checked_add(entries)
                        .ok_or(PackStreamError::UnexpectedEof)?;
                }
                Marker::TinyStruct(_) | Marker::Struct8 | Marker::Struct16 => {
                    let num_fields = self.struct_len(marker)? as u64;
                    self.skip_bytes(1)?; // tag byte
                    remaining = remaining
                        .checked_add(num_fields)
                        .ok_or(PackStreamError::UnexpectedEof)?;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packstream::marker::Marker;

    /// Helper: build a reader from a Vec — `Bytes::from(vec)` reuses the
    /// vec's allocation, so pointer-equality tests are meaningful.
    fn reader(data: Vec<u8>) -> PackStreamReader<Bytes> {
        PackStreamReader::new(Bytes::from(data))
    }

    // ---- Null ----

    #[test]
    fn read_null() {
        let mut r = reader(vec![Marker::Null.byte()]);
        assert!(r.read_null().is_ok());
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_null_wrong_marker() {
        let mut r = reader(vec![Marker::True.byte()]);
        assert_eq!(
            r.read_null(),
            Err(PackStreamError::InvalidMarker(Marker::True.byte()))
        );
    }

    // ---- Boolean ----

    #[test]
    fn read_bool_true() {
        let mut r = reader(vec![Marker::True.byte()]);
        assert!(r.read_bool().unwrap());
    }

    #[test]
    fn read_bool_false() {
        let mut r = reader(vec![Marker::False.byte()]);
        assert!(!r.read_bool().unwrap());
    }

    // ---- Integer ----

    #[test]
    fn read_tiny_int_zero() {
        let mut r = reader(vec![0x00]);
        assert_eq!(r.read_int().unwrap(), 0);
    }

    #[test]
    fn read_tiny_int_positive() {
        let mut r = reader(vec![42u8]);
        assert_eq!(r.read_int().unwrap(), 42);
    }

    #[test]
    fn read_tiny_int_max() {
        let mut r = reader(vec![127u8]);
        assert_eq!(r.read_int().unwrap(), 127);
    }

    #[test]
    fn read_tiny_int_negative() {
        let mut r = reader(vec![0xFFu8]); // -1
        assert_eq!(r.read_int().unwrap(), -1);
    }

    #[test]
    fn read_tiny_int_min() {
        let mut r = reader(vec![0xF0u8]); // -16
        assert_eq!(r.read_int().unwrap(), -16);
    }

    #[test]
    fn read_int8() {
        let mut r = reader(vec![Marker::Int8.byte(), 0x9C]);
        assert_eq!(r.read_int().unwrap(), -100);
    }

    #[test]
    fn read_int8_minus_17() {
        let mut r = reader(vec![Marker::Int8.byte(), (-17i8 as u8)]);
        assert_eq!(r.read_int().unwrap(), -17);
    }

    #[test]
    fn read_int8_minus_128() {
        let mut r = reader(vec![Marker::Int8.byte(), 0x80]);
        assert_eq!(r.read_int().unwrap(), -128);
    }

    #[test]
    fn read_int16() {
        let mut r = reader(vec![Marker::Int16.byte(), 0x03, 0xE8]);
        assert_eq!(r.read_int().unwrap(), 1000);
    }

    #[test]
    fn read_int16_negative() {
        let mut r = reader(vec![Marker::Int16.byte(), 0xFC, 0x18]);
        assert_eq!(r.read_int().unwrap(), -1000);
    }

    #[test]
    fn read_int32() {
        let mut r = reader(vec![Marker::Int32.byte(), 0x00, 0x01, 0x86, 0xA0]);
        assert_eq!(r.read_int().unwrap(), 100_000);
    }

    #[test]
    fn read_int32_negative() {
        let val: i32 = -100_000;
        let bytes = val.to_be_bytes();
        let mut r = reader(vec![
            Marker::Int32.byte(),
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
        ]);
        assert_eq!(r.read_int().unwrap(), -100_000);
    }

    #[test]
    fn read_int64() {
        let val: i64 = 1_000_000_000_000;
        let mut data = vec![Marker::Int64.byte()];
        data.extend_from_slice(&val.to_be_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_int().unwrap(), 1_000_000_000_000);
    }

    #[test]
    fn read_int64_min() {
        let val: i64 = i64::MIN;
        let mut data = vec![Marker::Int64.byte()];
        data.extend_from_slice(&val.to_be_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_int().unwrap(), i64::MIN);
    }

    // ---- Float ----

    #[test]
    fn read_float() {
        let val: f64 = 3.14;
        let mut data = vec![Marker::Float64.byte()];
        data.extend_from_slice(&val.to_be_bytes());
        let mut r = reader(data);
        assert!((r.read_float().unwrap() - 3.14).abs() < f64::EPSILON);
    }

    #[test]
    fn read_float_zero() {
        let mut data = vec![Marker::Float64.byte()];
        data.extend_from_slice(&0.0f64.to_be_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_float().unwrap(), 0.0);
    }

    #[test]
    fn read_float_negative() {
        let mut data = vec![Marker::Float64.byte()];
        data.extend_from_slice(&(-1.5f64).to_be_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_float().unwrap(), -1.5);
    }

    // ---- String ----

    #[test]
    fn read_tiny_string_empty() {
        let mut r = reader(vec![0x80]);
        assert_eq!(r.read_string().unwrap().as_str(), "");
    }

    #[test]
    fn read_tiny_string() {
        let mut data = vec![0x85];
        data.extend_from_slice(b"hello");
        let mut r = reader(data);
        assert_eq!(r.read_string().unwrap().as_str(), "hello");
    }

    #[test]
    fn read_string8() {
        let s = "a]".repeat(20); // 40 bytes > 15
        let mut data = vec![Marker::String8.byte(), 40];
        data.extend_from_slice(s.as_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_string().unwrap().as_str(), s);
    }

    #[test]
    fn read_string16() {
        let s = "x".repeat(300);
        let len = 300u16;
        let mut data = vec![Marker::String16.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(s.as_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_string().unwrap().as_str(), s);
    }

    #[test]
    fn read_string32() {
        let s = "a".repeat(70_000);
        let len = s.len() as u32;
        let mut data = vec![Marker::String32.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(s.as_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_string().unwrap().as_str(), s);
    }

    #[test]
    fn read_string_invalid_utf8() {
        let mut data = vec![0x82]; // TINY_STRING length 2
        data.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8
        let mut r = reader(data);
        assert_eq!(r.read_string(), Err(PackStreamError::InvalidUtf8));
    }

    #[test]
    fn read_string_zero_copy_within_segment() {
        // String fits in one segment of the input Bytes — returned
        // PackStreamString must share that allocation (zero copy).
        let mut data = vec![0x85]; // TINY_STRING length 5
        data.extend_from_slice(b"hello");
        let input = Bytes::from(data);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut r = PackStreamReader::new(input);
        let s = r.read_string().unwrap();
        let s_ptr = s.as_str().as_ptr();
        let base = input_ptr as usize;
        let off = s_ptr as usize - base;
        assert!(
            off < input_len,
            "zero-copy: returned string must point into the input allocation"
        );
        assert_eq!(off, 1, "string payload starts after the 1-byte marker");
    }

    // ---- Bytes ----

    #[test]
    fn read_bytes8() {
        let payload = [0x01, 0x02, 0x03, 0x04];
        let mut data = vec![Marker::Bytes8.byte(), 4];
        data.extend_from_slice(&payload);
        let mut r = reader(data);
        assert_eq!(&r.read_bytes().unwrap()[..], &payload);
    }

    #[test]
    fn read_bytes_zero_copy_within_segment() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut data = vec![Marker::Bytes8.byte(), 4];
        data.extend_from_slice(&payload);
        let input = Bytes::from(data);
        let input_ptr = input.as_ptr();
        let input_len = input.len();

        let mut r = PackStreamReader::new(input);
        let b = r.read_bytes().unwrap();
        let off = b.as_ptr() as usize - input_ptr as usize;
        assert!(
            off < input_len,
            "zero-copy: bytes must point into the input allocation"
        );
        assert_eq!(off, 2, "bytes payload starts after marker + length byte");
    }

    #[test]
    fn read_bytes16() {
        let payload = vec![0xAB; 300];
        let len = 300u16;
        let mut data = vec![Marker::Bytes16.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(&payload);
        let mut r = reader(data);
        assert_eq!(&r.read_bytes().unwrap()[..], &payload[..]);
    }

    #[test]
    fn read_bytes32() {
        let payload = vec![0x42; 70_000];
        let len = payload.len() as u32;
        let mut data = vec![Marker::Bytes32.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(&payload);
        let mut r = reader(data);
        assert_eq!(&r.read_bytes().unwrap()[..], &payload[..]);
    }

    // ---- List header ----

    #[test]
    fn read_tiny_list_header() {
        let mut r = reader(vec![0x93]); // TINY_LIST with 3 elements
        assert_eq!(r.read_list_header().unwrap(), 3);
    }

    #[test]
    fn read_list8_header() {
        let mut r = reader(vec![Marker::List8.byte(), 20]);
        assert_eq!(r.read_list_header().unwrap(), 20);
    }

    #[test]
    fn read_list16_header() {
        let mut r = reader(vec![Marker::List16.byte(), 0x01, 0x00]); // 256
        assert_eq!(r.read_list_header().unwrap(), 256);
    }

    #[test]
    fn read_list32_header() {
        let mut r = reader(vec![Marker::List32.byte(), 0x00, 0x01, 0x00, 0x00]); // 65536
        assert_eq!(r.read_list_header().unwrap(), 65536);
    }

    // ---- Map header ----

    #[test]
    fn read_tiny_map_header() {
        let mut r = reader(vec![0xA2]); // TINY_MAP with 2 entries
        assert_eq!(r.read_map_header().unwrap(), 2);
    }

    #[test]
    fn read_map8_header() {
        let mut r = reader(vec![Marker::Map8.byte(), 20]);
        assert_eq!(r.read_map_header().unwrap(), 20);
    }

    #[test]
    fn read_map16_header() {
        let len = 300u16;
        let mut data = vec![Marker::Map16.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_map_header().unwrap(), 300);
    }

    #[test]
    fn read_map32_header() {
        let len = 70_000u32;
        let mut data = vec![Marker::Map32.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        let mut r = reader(data);
        assert_eq!(r.read_map_header().unwrap(), 70_000);
    }

    // ---- Struct header ----

    #[test]
    fn read_tiny_struct_header() {
        let mut r = reader(vec![0xB3, 0x4E]); // TINY_STRUCT with 3 fields, tag NODE
        let (tag, fields) = r.read_struct_header().unwrap();
        assert_eq!(tag, 0x4E);
        assert_eq!(fields, 3);
    }

    #[test]
    fn read_struct8_header() {
        let mut r = reader(vec![Marker::Struct8.byte(), 5, 0x52]); // 5 fields, tag RELATIONSHIP
        let (tag, fields) = r.read_struct_header().unwrap();
        assert_eq!(tag, 0x52);
        assert_eq!(fields, 5);
    }

    #[test]
    fn read_struct16_header() {
        let num_fields = 300u16;
        let mut data = vec![Marker::Struct16.byte()];
        data.extend_from_slice(&num_fields.to_be_bytes());
        data.push(0x4E); // tag = NODE
        let mut r = reader(data);
        let (tag, fields) = r.read_struct_header().unwrap();
        assert_eq!(tag, 0x4E);
        assert_eq!(fields, 300);
    }

    // ---- skip_value ----

    #[test]
    fn skip_null() {
        let mut r = reader(vec![Marker::Null.byte()]);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_bool() {
        let mut r = reader(vec![Marker::True.byte()]);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_tiny_int() {
        let mut r = reader(vec![42u8]);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int8() {
        let mut r = reader(vec![Marker::Int8.byte(), 0x9C]);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int16() {
        let mut r = reader(vec![Marker::Int16.byte(), 0x03, 0xE8]);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int32() {
        let mut r = reader(vec![Marker::Int32.byte(), 0x00, 0x01, 0x86, 0xA0]);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int64() {
        let mut data = vec![Marker::Int64.byte()];
        data.extend_from_slice(&42i64.to_be_bytes());
        let mut r = reader(data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_float() {
        let mut data = vec![Marker::Float64.byte()];
        data.extend_from_slice(&3.14f64.to_be_bytes());
        let mut r = reader(data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_string() {
        let mut data = vec![0x85];
        data.extend_from_slice(b"hello");
        let mut r = reader(data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_bytes() {
        let mut r = reader(vec![Marker::Bytes8.byte(), 3, 0x01, 0x02, 0x03]);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_list() {
        let mut data = vec![0x92]; // TINY_LIST 2
        data.push(42u8); // tiny int 42
        data.push(0x82); // TINY_STRING length 2
        data.extend_from_slice(b"hi");
        let mut r = reader(data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_map() {
        let mut data = vec![0xA1]; // TINY_MAP 1
        data.push(0x81); // TINY_STRING length 1 (key)
        data.push(b'a');
        data.push(1u8); // tiny int 1 (value)
        let mut r = reader(data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_struct() {
        let mut data = vec![0xB2, 0x4E]; // TINY_STRUCT 2 fields, tag NODE
        data.push(42u8);
        data.push(Marker::Null.byte());
        let mut r = reader(data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_nested_list_in_map() {
        let mut data = vec![0xA1]; // TINY_MAP 1
        data.push(0x83); // TINY_STRING "key"
        data.extend_from_slice(b"key");
        data.push(0x93); // TINY_LIST 3
        data.push(1u8);
        data.push(2u8);
        data.push(3u8);
        let mut r = reader(data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    // ---- Error cases ----

    #[test]
    fn unexpected_eof_empty() {
        let mut r = reader(vec![]);
        assert_eq!(r.read_null(), Err(PackStreamError::UnexpectedEof));
    }

    #[test]
    fn unexpected_eof_truncated_int16() {
        let mut r = reader(vec![Marker::Int16.byte(), 0x03]); // missing second byte
        assert_eq!(r.read_int(), Err(PackStreamError::UnexpectedEof));
    }

    #[test]
    fn unexpected_eof_truncated_string() {
        let mut r = reader(vec![0x85, b'h', b'e']); // says 5 bytes but only 2 present
        assert_eq!(r.read_string(), Err(PackStreamError::UnexpectedEof));
    }

    #[test]
    fn wrong_marker_for_bool() {
        let mut r = reader(vec![Marker::Null.byte()]);
        assert_eq!(
            r.read_bool(),
            Err(PackStreamError::InvalidMarker(Marker::Null.byte()))
        );
    }

    #[test]
    fn wrong_marker_for_float() {
        let mut r = reader(vec![0x42]); // tiny int, not float
        assert_eq!(r.read_float(), Err(PackStreamError::InvalidMarker(0x42)));
    }

    // ---- Peek & remaining ----

    #[test]
    fn peek_returns_next_byte() {
        let r = reader(vec![Marker::Null.byte(), Marker::True.byte()]);
        assert_eq!(r.peek(), Some(Marker::Null.byte()));
        assert_eq!(r.remaining(), 2);
    }

    #[test]
    fn peek_empty() {
        let r = reader(vec![]);
        assert_eq!(r.peek(), None);
    }

    // ---- Multiple reads in sequence ----

    #[test]
    fn read_multiple_values() {
        let mut data = Vec::new();
        data.push(Marker::Null.byte());
        data.push(Marker::True.byte());
        data.push(42u8);
        data.push(0x82);
        data.extend_from_slice(b"hi");
        data.push(Marker::Float64.byte());
        data.extend_from_slice(&1.0f64.to_be_bytes());

        let mut r = reader(data);
        r.read_null().unwrap();
        assert!(r.read_bool().unwrap());
        assert_eq!(r.read_int().unwrap(), 42);
        assert_eq!(r.read_string().unwrap().as_str(), "hi");
        assert_eq!(r.read_float().unwrap(), 1.0);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_string_unicode() {
        let s = "hello \u{1F600}"; // emoji
        let bytes = s.as_bytes();
        let len = bytes.len();
        assert!(len <= 15);
        let mut data = vec![0x80 | len as u8];
        data.extend_from_slice(bytes);
        let mut r = reader(data);
        assert_eq!(r.read_string().unwrap().as_str(), s);
    }

    #[test]
    fn skip_then_read() {
        let mut data = Vec::new();
        data.push(0x87);
        data.extend_from_slice(b"skip me");
        data.push(99u8);

        let mut r = reader(data);
        r.skip_value().unwrap();
        assert_eq!(r.read_int().unwrap(), 99);
        assert_eq!(r.remaining(), 0);
    }

    // ---- Multi-segment reads via MessageBuf ----

    mod multi_segment {
        use super::*;
        use crate::protocol::chunking::MessageBuf;

        /// Build a MessageBuf from explicit Bytes segments.
        fn buf_from(chunks: Vec<Vec<u8>>) -> MessageBuf {
            MessageBuf::from_chunks(chunks.into_iter().map(Bytes::from).collect())
        }

        #[test]
        fn read_bytes_within_segment_is_zero_copy() {
            // Payload + marker all in one segment → zero-copy slice.
            let mut wire = vec![Marker::Bytes8.byte(), 4];
            wire.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
            let input = Bytes::from(wire);
            let input_ptr = input.as_ptr();
            let input_len = input.len();

            let buf = MessageBuf::from_chunks(vec![input]);
            let mut r = PackStreamReader::new(buf);
            let b = r.read_bytes().unwrap();
            let off = b.as_ptr() as usize - input_ptr as usize;
            assert!(off < input_len, "should share allocation");
        }

        #[test]
        fn read_bytes_across_segments_allocates_correctly() {
            // First segment ends mid-payload; second has the rest.
            let buf = buf_from(vec![
                vec![Marker::Bytes8.byte(), 4, 0xDE, 0xAD], // marker, length, first 2 of payload
                vec![0xBE, 0xEF],                           // last 2 of payload
            ]);
            let mut r = PackStreamReader::new(buf);
            let b = r.read_bytes().unwrap();
            assert_eq!(&b[..], &[0xDE, 0xAD, 0xBE, 0xEF]);
        }

        #[test]
        fn read_string_across_segments_validates_utf8() {
            let buf = buf_from(vec![
                vec![0x85, b'h', b'e'], // marker + first 2 chars
                vec![b'l', b'l', b'o'], // last 3 chars
            ]);
            let mut r = PackStreamReader::new(buf);
            assert_eq!(r.read_string().unwrap().as_str(), "hello");
        }

        #[test]
        fn read_int_across_segments() {
            // Int32 marker in segment 1, value bytes split across segments 1 and 2.
            let buf = buf_from(vec![
                vec![Marker::Int32.byte(), 0x00, 0x01], // marker + 2 of 4 value bytes
                vec![0x86, 0xA0],                       // last 2 value bytes
            ]);
            let mut r = PackStreamReader::new(buf);
            assert_eq!(r.read_int().unwrap(), 100_000);
        }

        #[test]
        fn skip_value_across_segments() {
            // String "skip me" + tiny int 99, payload bytes split across 3 segments.
            let buf = buf_from(vec![
                vec![0x87, b's', b'k'],
                vec![b'i', b'p', b' '],
                vec![b'm', b'e', 99u8],
            ]);
            let mut r = PackStreamReader::new(buf);
            r.skip_value().unwrap();
            assert_eq!(r.read_int().unwrap(), 99);
            assert_eq!(r.remaining(), 0);
        }
    }

    // ---- Round-trip tests (write with PackStreamWriter, read back) ----

    mod round_trip {
        use super::*;
        use crate::packstream::serialize::PackStreamWriter;

        fn reader_from(w: PackStreamWriter) -> PackStreamReader<Bytes> {
            PackStreamReader::new(w.into_bytes().freeze())
        }

        #[test]
        fn null() {
            let mut w = PackStreamWriter::new();
            w.write_null();
            let mut r = reader_from(w);
            r.read_null().unwrap();
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn bool_true_false() {
            let mut w = PackStreamWriter::new();
            w.write_bool(true);
            w.write_bool(false);
            let mut r = reader_from(w);
            assert!(r.read_bool().unwrap());
            assert!(!r.read_bool().unwrap());
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn int_all_encodings() {
            let values: &[i64] = &[
                0,
                127,
                -16,
                -1,
                -17,
                -128,
                128,
                -129,
                i16::MAX as i64,
                i16::MIN as i64,
                i32::MAX as i64,
                i32::MIN as i64,
                i64::MAX,
                i64::MIN,
            ];
            let mut w = PackStreamWriter::new();
            for &v in values {
                w.write_int(v);
            }
            let mut r = reader_from(w);
            for &v in values {
                assert_eq!(r.read_int().unwrap(), v, "round-trip failed for {v}");
            }
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn float() {
            let values: &[f64] = &[0.0, 1.5, -1.5, f64::MAX, f64::MIN, f64::INFINITY];
            let mut w = PackStreamWriter::new();
            for &v in values {
                w.write_float(v);
            }
            let mut r = reader_from(w);
            for &v in values {
                assert_eq!(r.read_float().unwrap(), v);
            }
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn string() {
            let strings = [
                "",
                "a",
                "hello world!!!!",
                &"x".repeat(16),
                &"y".repeat(255),
                &"z".repeat(256),
            ];
            let mut w = PackStreamWriter::new();
            for s in &strings {
                w.write_string(s);
            }
            let mut r = reader_from(w);
            for s in &strings {
                assert_eq!(r.read_string().unwrap().as_str(), *s);
            }
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn bytes() {
            let payloads: &[&[u8]] = &[&[], &[0xDE, 0xAD], &vec![0x42; 256]];
            let mut w = PackStreamWriter::new();
            for b in payloads {
                w.write_bytes(b);
            }
            let mut r = reader_from(w);
            for b in payloads {
                assert_eq!(&r.read_bytes().unwrap()[..], *b);
            }
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn list_header() {
            let sizes: &[u32] = &[0, 15, 16, 255, 256, 65536];
            let mut w = PackStreamWriter::new();
            for &s in sizes {
                w.write_list_header(s);
            }
            let mut r = reader_from(w);
            for &s in sizes {
                assert_eq!(r.read_list_header().unwrap(), s);
            }
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn map_header() {
            let sizes: &[u32] = &[0, 15, 16, 255, 256, 65536];
            let mut w = PackStreamWriter::new();
            for &s in sizes {
                w.write_map_header(s);
            }
            let mut r = reader_from(w);
            for &s in sizes {
                assert_eq!(r.read_map_header().unwrap(), s);
            }
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn struct_header() {
            let cases: &[(u8, u32)] = &[(0x4E, 3), (0x52, 0), (0x45, 16), (0x4E, 256)];
            let mut w = PackStreamWriter::new();
            for &(tag, size) in cases {
                w.write_struct_header(tag, size);
            }
            let mut r = reader_from(w);
            for &(tag, size) in cases {
                let (t, s) = r.read_struct_header().unwrap();
                assert_eq!(t, tag);
                assert_eq!(s, size);
            }
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn composite_map() {
            let mut w = PackStreamWriter::new();
            w.write_map_header(3);
            w.write_string("name");
            w.write_string("Alice");
            w.write_string("age");
            w.write_int(30);
            w.write_string("scores");
            w.write_list_header(2);
            w.write_int(95);
            w.write_int(87);

            let mut r = reader_from(w);

            assert_eq!(r.read_map_header().unwrap(), 3);
            assert_eq!(r.read_string().unwrap().as_str(), "name");
            assert_eq!(r.read_string().unwrap().as_str(), "Alice");
            assert_eq!(r.read_string().unwrap().as_str(), "age");
            assert_eq!(r.read_int().unwrap(), 30);
            assert_eq!(r.read_string().unwrap().as_str(), "scores");
            assert_eq!(r.read_list_header().unwrap(), 2);
            assert_eq!(r.read_int().unwrap(), 95);
            assert_eq!(r.read_int().unwrap(), 87);
            assert_eq!(r.remaining(), 0);
        }
    }
}
