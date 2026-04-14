// PackStream read (deserialization) — zero-copy reader.

use crate::packstream::marker::{Marker, MarkerError};

/// Errors that can occur during PackStream deserialization.
#[derive(Debug, Clone, PartialEq)]
pub enum PackStreamError {
    /// Not enough bytes remaining in the buffer.
    UnexpectedEof,
    /// The marker byte does not match the expected type.
    InvalidMarker(u8),
    /// A string value contains invalid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for PackStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackStreamError::UnexpectedEof => write!(f, "unexpected end of input"),
            PackStreamError::InvalidMarker(m) => write!(f, "invalid marker: 0x{m:02X}"),
            PackStreamError::InvalidUtf8 => write!(f, "invalid UTF-8 in string"),
        }
    }
}

impl std::error::Error for PackStreamError {}

/// Reads PackStream-encoded data from a byte buffer.
///
/// Returns borrowed references where possible (zero-copy for strings/bytes).
/// The lifetime `'a` ties returned references to the buffer — they are valid
/// as long as the underlying message buffer is alive.
///
/// All multi-byte integer reads use `from_be_bytes()` on byte slices — never
/// raw pointer casts (which cause UB on strict-alignment architectures).
pub struct PackStreamReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PackStreamReader<'a> {
    /// Create a new reader over the given byte slice.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Peek at the next byte without consuming it.
    pub fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    /// Number of bytes remaining.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    // ---- internal helpers ----

    fn read_u8(&mut self) -> Result<u8, PackStreamError> {
        if self.pos >= self.data.len() {
            return Err(PackStreamError::UnexpectedEof);
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_exact(&mut self, n: usize) -> Result<&'a [u8], PackStreamError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(PackStreamError::UnexpectedEof)?;
        if end > self.data.len() {
            return Err(PackStreamError::UnexpectedEof);
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_i8(&mut self) -> Result<i8, PackStreamError> {
        Ok(self.read_u8()? as i8)
    }

    fn read_i16(&mut self) -> Result<i16, PackStreamError> {
        let bytes = self.read_exact(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn read_i32(&mut self) -> Result<i32, PackStreamError> {
        let bytes = self.read_exact(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i64(&mut self) -> Result<i64, PackStreamError> {
        let bytes = self.read_exact(8)?;
        Ok(i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_u16(&mut self) -> Result<u16, PackStreamError> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, PackStreamError> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    // ---- marker parsing ----

    fn read_marker(&mut self) -> Result<Marker, PackStreamError> {
        let b = self.read_u8()?;
        Marker::from_byte(b).map_err(|e| match e {
            MarkerError::InvalidByte(b) => PackStreamError::InvalidMarker(b),
            _ => unreachable!("from_byte only returns InvalidByte"),
        })
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
        let bytes = self.read_exact(8)?;
        Ok(f64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Read a string value — **zero-copy**: returns a slice into the original buffer.
    pub fn read_string(&mut self) -> Result<&'a str, PackStreamError> {
        let marker = self.read_marker()?;
        let len = self.string_len(marker)?;
        let bytes = self.read_exact(len)?;
        std::str::from_utf8(bytes).map_err(|_| PackStreamError::InvalidUtf8)
    }

    /// Read a byte array — **zero-copy**: returns a slice into the original buffer.
    pub fn read_bytes(&mut self) -> Result<&'a [u8], PackStreamError> {
        let marker = self.read_marker()?;
        let len = self.bytes_len(marker)?;
        self.read_exact(len)
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
                Marker::Int8 => {
                    self.read_exact(1)?;
                }
                Marker::Int16 => {
                    self.read_exact(2)?;
                }
                Marker::Int32 => {
                    self.read_exact(4)?;
                }
                Marker::Int64 | Marker::Float64 => {
                    self.read_exact(8)?;
                }

                // Variable-length byte sequences — read length, skip that many bytes.
                Marker::TinyString(_) | Marker::String8 | Marker::String16 | Marker::String32 => {
                    let len = self.string_len(marker)?;
                    self.read_exact(len)?;
                }
                Marker::Bytes8 | Marker::Bytes16 | Marker::Bytes32 => {
                    let len = self.bytes_len(marker)?;
                    self.read_exact(len)?;
                }

                // Containers — add child count to remaining values.
                Marker::TinyList(_) | Marker::List8 | Marker::List16 | Marker::List32 => {
                    remaining += self.list_len(marker)? as u64;
                }
                Marker::TinyMap(_) | Marker::Map8 | Marker::Map16 | Marker::Map32 => {
                    // Each map entry has a key + value = 2 values per entry.
                    // u32 * 2 always fits in u64 so no overflow.
                    remaining += self.map_len(marker)? as u64 * 2;
                }
                Marker::TinyStruct(_) | Marker::Struct8 | Marker::Struct16 => {
                    let num_fields = self.struct_len(marker)?;
                    self.read_exact(1)?; // tag byte
                    remaining += num_fields as u64;
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

    // ---- Null ----

    #[test]
    fn read_null() {
        let data = [Marker::Null.byte()];
        let mut r = PackStreamReader::new(&data);
        assert!(r.read_null().is_ok());
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_null_wrong_marker() {
        let data = [Marker::True.byte()];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(
            r.read_null(),
            Err(PackStreamError::InvalidMarker(Marker::True.byte()))
        );
    }

    // ---- Boolean ----

    #[test]
    fn read_bool_true() {
        let data = [Marker::True.byte()];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_bool().unwrap(), true);
    }

    #[test]
    fn read_bool_false() {
        let data = [Marker::False.byte()];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_bool().unwrap(), false);
    }

    // ---- Integer ----

    #[test]
    fn read_tiny_int_zero() {
        let data = [0x00];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 0);
    }

    #[test]
    fn read_tiny_int_positive() {
        let data = [42u8];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 42);
    }

    #[test]
    fn read_tiny_int_max() {
        let data = [127u8];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 127);
    }

    #[test]
    fn read_tiny_int_negative() {
        // -1 as u8 = 0xFF, -16 as u8 = 0xF0
        let data = [0xFFu8]; // -1
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -1);
    }

    #[test]
    fn read_tiny_int_min() {
        let data = [0xF0u8]; // -16
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -16);
    }

    #[test]
    fn read_int8() {
        // INT8 marker + value -100 (0x9C)
        let data = [Marker::Int8.byte(), 0x9C];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -100);
    }

    #[test]
    fn read_int8_minus_17() {
        // -17 is the first value below TINY_INT range
        let data = [Marker::Int8.byte(), (-17i8 as u8)];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -17);
    }

    #[test]
    fn read_int8_minus_128() {
        let data = [Marker::Int8.byte(), 0x80]; // -128 as i8
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -128);
    }

    #[test]
    fn read_int16() {
        // 1000 = 0x03E8
        let data = [Marker::Int16.byte(), 0x03, 0xE8];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 1000);
    }

    #[test]
    fn read_int16_negative() {
        // -1000 as i16 = 0xFC18
        let data = [Marker::Int16.byte(), 0xFC, 0x18];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -1000);
    }

    #[test]
    fn read_int32() {
        // 100_000 = 0x000186A0
        let data = [Marker::Int32.byte(), 0x00, 0x01, 0x86, 0xA0];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 100_000);
    }

    #[test]
    fn read_int32_negative() {
        let val: i32 = -100_000;
        let bytes = val.to_be_bytes();
        let data = [Marker::Int32.byte(), bytes[0], bytes[1], bytes[2], bytes[3]];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -100_000);
    }

    #[test]
    fn read_int64() {
        let val: i64 = 1_000_000_000_000;
        let bytes = val.to_be_bytes();
        let mut data = vec![Marker::Int64.byte()];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 1_000_000_000_000);
    }

    #[test]
    fn read_int64_min() {
        let val: i64 = i64::MIN;
        let bytes = val.to_be_bytes();
        let mut data = vec![Marker::Int64.byte()];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), i64::MIN);
    }

    // ---- Float ----

    #[test]
    fn read_float() {
        let val: f64 = 3.14;
        let bytes = val.to_be_bytes();
        let mut data = vec![Marker::Float64.byte()];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert!((r.read_float().unwrap() - 3.14).abs() < f64::EPSILON);
    }

    #[test]
    fn read_float_zero() {
        let bytes = 0.0f64.to_be_bytes();
        let mut data = vec![Marker::Float64.byte()];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_float().unwrap(), 0.0);
    }

    #[test]
    fn read_float_negative() {
        let val: f64 = -1.5;
        let bytes = val.to_be_bytes();
        let mut data = vec![Marker::Float64.byte()];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_float().unwrap(), -1.5);
    }

    // ---- String ----

    #[test]
    fn read_tiny_string_empty() {
        let data = [0x80]; // TINY_STRING with length 0
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string().unwrap(), "");
    }

    #[test]
    fn read_tiny_string() {
        // TINY_STRING with length 5 + "hello"
        let mut data = vec![0x85];
        data.extend_from_slice(b"hello");
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string().unwrap(), "hello");
    }

    #[test]
    fn read_string8() {
        let s = "a]".repeat(20); // 40 bytes > 15
        let mut data = vec![Marker::String8.byte(), 40];
        data.extend_from_slice(s.as_bytes());
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string().unwrap(), s);
    }

    #[test]
    fn read_string16() {
        let s = "x".repeat(300);
        let len = 300u16;
        let mut data = vec![Marker::String16.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(s.as_bytes());
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string().unwrap(), s);
    }

    #[test]
    fn read_string32() {
        let s = "a".repeat(70_000);
        let len = s.len() as u32;
        let mut data = vec![Marker::String32.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(s.as_bytes());
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string().unwrap(), s);
    }

    #[test]
    fn read_string_invalid_utf8() {
        let mut data = vec![0x82]; // TINY_STRING length 2
        data.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string(), Err(PackStreamError::InvalidUtf8));
    }

    #[test]
    fn read_string_zero_copy() {
        let mut data = vec![0x85]; // TINY_STRING length 5
        data.extend_from_slice(b"hello");
        let r_data = &data[..];
        let mut reader = PackStreamReader::new(r_data);
        let s = reader.read_string().unwrap();
        // The returned string slice should point directly into the input buffer.
        let s_ptr = s.as_ptr();
        let buf_ptr = r_data[1..].as_ptr(); // skip marker byte
        assert_eq!(
            s_ptr, buf_ptr,
            "zero-copy: string should point into original buffer"
        );
    }

    // ---- Bytes ----

    #[test]
    fn read_bytes8() {
        let payload = [0x01, 0x02, 0x03, 0x04];
        let mut data = vec![Marker::Bytes8.byte(), 4];
        data.extend_from_slice(&payload);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_bytes().unwrap(), &payload);
    }

    #[test]
    fn read_bytes_zero_copy() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut data = vec![Marker::Bytes8.byte(), 4];
        data.extend_from_slice(&payload);
        let r_data = &data[..];
        let mut reader = PackStreamReader::new(r_data);
        let b = reader.read_bytes().unwrap();
        let b_ptr = b.as_ptr();
        let buf_ptr = r_data[2..].as_ptr(); // skip marker + length byte
        assert_eq!(
            b_ptr, buf_ptr,
            "zero-copy: bytes should point into original buffer"
        );
    }

    #[test]
    fn read_bytes16() {
        let payload = vec![0xAB; 300];
        let len = 300u16;
        let mut data = vec![Marker::Bytes16.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(&payload);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_bytes().unwrap(), &payload[..]);
    }

    #[test]
    fn read_bytes32() {
        let payload = vec![0x42; 70_000];
        let len = payload.len() as u32;
        let mut data = vec![Marker::Bytes32.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(&payload);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_bytes().unwrap(), &payload[..]);
    }

    // ---- List header ----

    #[test]
    fn read_tiny_list_header() {
        let data = [0x93]; // TINY_LIST with 3 elements
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_list_header().unwrap(), 3);
    }

    #[test]
    fn read_list8_header() {
        let data = [Marker::List8.byte(), 20];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_list_header().unwrap(), 20);
    }

    #[test]
    fn read_list16_header() {
        let data = [Marker::List16.byte(), 0x01, 0x00]; // 256
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_list_header().unwrap(), 256);
    }

    #[test]
    fn read_list32_header() {
        let data = [Marker::List32.byte(), 0x00, 0x01, 0x00, 0x00]; // 65536
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_list_header().unwrap(), 65536);
    }

    // ---- Map header ----

    #[test]
    fn read_tiny_map_header() {
        let data = [0xA2]; // TINY_MAP with 2 entries
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_map_header().unwrap(), 2);
    }

    #[test]
    fn read_map8_header() {
        let data = [Marker::Map8.byte(), 20];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_map_header().unwrap(), 20);
    }

    #[test]
    fn read_map16_header() {
        let len = 300u16;
        let mut data = vec![Marker::Map16.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_map_header().unwrap(), 300);
    }

    #[test]
    fn read_map32_header() {
        let len = 70_000u32;
        let mut data = vec![Marker::Map32.byte()];
        data.extend_from_slice(&len.to_be_bytes());
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_map_header().unwrap(), 70_000);
    }

    // ---- Struct header ----

    #[test]
    fn read_tiny_struct_header() {
        // TINY_STRUCT with 3 fields, tag = 0x4E (NODE)
        let data = [0xB3, 0x4E];
        let mut r = PackStreamReader::new(&data);
        let (tag, fields) = r.read_struct_header().unwrap();
        assert_eq!(tag, 0x4E);
        assert_eq!(fields, 3);
    }

    #[test]
    fn read_struct8_header() {
        let data = [Marker::Struct8.byte(), 5, 0x52]; // 5 fields, tag = RELATIONSHIP
        let mut r = PackStreamReader::new(&data);
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
        let mut r = PackStreamReader::new(&data);
        let (tag, fields) = r.read_struct_header().unwrap();
        assert_eq!(tag, 0x4E);
        assert_eq!(fields, 300);
    }

    // ---- skip_value ----

    #[test]
    fn skip_null() {
        let data = [Marker::Null.byte()];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_bool() {
        let data = [Marker::True.byte()];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_tiny_int() {
        let data = [42u8];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int8() {
        let data = [Marker::Int8.byte(), 0x9C];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int16() {
        let data = [Marker::Int16.byte(), 0x03, 0xE8];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int32() {
        let data = [Marker::Int32.byte(), 0x00, 0x01, 0x86, 0xA0];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int64() {
        let mut data = vec![Marker::Int64.byte()];
        data.extend_from_slice(&42i64.to_be_bytes());
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_float() {
        let mut data = vec![Marker::Float64.byte()];
        data.extend_from_slice(&3.14f64.to_be_bytes());
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_string() {
        let mut data = vec![0x85]; // TINY_STRING length 5
        data.extend_from_slice(b"hello");
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_bytes() {
        let data = vec![Marker::Bytes8.byte(), 3, 0x01, 0x02, 0x03];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_list() {
        // List of 2 elements: [42, "hi"]
        let mut data = vec![0x92]; // TINY_LIST 2
        data.push(42u8); // tiny int 42
        data.push(0x82); // TINY_STRING length 2
        data.extend_from_slice(b"hi");
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_map() {
        // Map with 1 entry: {"a": 1}
        let mut data = vec![0xA1]; // TINY_MAP 1
        data.push(0x81); // TINY_STRING length 1 (key)
        data.push(b'a');
        data.push(1u8); // tiny int 1 (value)
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_struct() {
        // Struct with 2 fields, tag 0x4E
        let mut data = vec![0xB2, 0x4E]; // TINY_STRUCT 2 fields, tag NODE
        data.push(42u8); // field 1: tiny int
        data.push(Marker::Null.byte()); // field 2: null
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_nested_list_in_map() {
        // Map {key: [1, 2, 3]}
        let mut data = vec![0xA1]; // TINY_MAP 1
        data.push(0x83); // TINY_STRING "key"
        data.extend_from_slice(b"key");
        data.push(0x93); // TINY_LIST 3
        data.push(1u8);
        data.push(2u8);
        data.push(3u8);
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    // ---- Error cases ----

    #[test]
    fn unexpected_eof_empty() {
        let data: [u8; 0] = [];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_null(), Err(PackStreamError::UnexpectedEof));
    }

    #[test]
    fn unexpected_eof_truncated_int16() {
        let data = [Marker::Int16.byte(), 0x03]; // missing second byte
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int(), Err(PackStreamError::UnexpectedEof));
    }

    #[test]
    fn unexpected_eof_truncated_string() {
        let data = [0x85, b'h', b'e']; // says 5 bytes but only 2 present
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string(), Err(PackStreamError::UnexpectedEof));
    }

    #[test]
    fn wrong_marker_for_bool() {
        let data = [Marker::Null.byte()];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(
            r.read_bool(),
            Err(PackStreamError::InvalidMarker(Marker::Null.byte()))
        );
    }

    #[test]
    fn wrong_marker_for_float() {
        let data = [0x42]; // tiny int, not float
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_float(), Err(PackStreamError::InvalidMarker(0x42)));
    }

    // ---- Peek & remaining ----

    #[test]
    fn peek_returns_next_byte() {
        let data = [Marker::Null.byte(), Marker::True.byte()];
        let r = PackStreamReader::new(&data);
        assert_eq!(r.peek(), Some(Marker::Null.byte()));
        assert_eq!(r.remaining(), 2);
    }

    #[test]
    fn peek_empty() {
        let data: [u8; 0] = [];
        let r = PackStreamReader::new(&data);
        assert_eq!(r.peek(), None);
    }

    // ---- Multiple reads in sequence ----

    #[test]
    fn read_multiple_values() {
        let mut data = Vec::new();
        // null
        data.push(Marker::Null.byte());
        // true
        data.push(Marker::True.byte());
        // int 42
        data.push(42u8);
        // string "hi"
        data.push(0x82);
        data.extend_from_slice(b"hi");
        // float 1.0
        data.push(Marker::Float64.byte());
        data.extend_from_slice(&1.0f64.to_be_bytes());

        let mut r = PackStreamReader::new(&data);
        r.read_null().unwrap();
        assert_eq!(r.read_bool().unwrap(), true);
        assert_eq!(r.read_int().unwrap(), 42);
        assert_eq!(r.read_string().unwrap(), "hi");
        assert_eq!(r.read_float().unwrap(), 1.0);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_string_unicode() {
        let s = "hello \u{1F600}"; // emoji
        let bytes = s.as_bytes();
        let len = bytes.len();
        assert!(len <= 15); // fits in TINY_STRING
        let mut data = vec![0x80 | len as u8];
        data.extend_from_slice(bytes);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string().unwrap(), s);
    }

    #[test]
    fn skip_then_read() {
        let mut data = Vec::new();
        // First value: string "skip me"
        data.push(0x87);
        data.extend_from_slice(b"skip me");
        // Second value: int 99
        data.push(99u8);

        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.read_int().unwrap(), 99);
        assert_eq!(r.remaining(), 0);
    }

    // ---- Round-trip tests (write with PackStreamWriter, read back) ----

    mod round_trip {
        use super::*;
        use crate::packstream::serialize::PackStreamWriter;

        #[test]
        fn null() {
            let mut w = PackStreamWriter::new();
            w.write_null();
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
            r.read_null().unwrap();
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn bool_true_false() {
            let mut w = PackStreamWriter::new();
            w.write_bool(true);
            w.write_bool(false);
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
            assert_eq!(r.read_bool().unwrap(), true);
            assert_eq!(r.read_bool().unwrap(), false);
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
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
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
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
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
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
            for s in &strings {
                assert_eq!(r.read_string().unwrap(), *s);
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
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
            for b in payloads {
                assert_eq!(r.read_bytes().unwrap(), *b);
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
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
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
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
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
            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);
            for &(tag, size) in cases {
                let (t, s) = r.read_struct_header().unwrap();
                assert_eq!(t, tag);
                assert_eq!(s, size);
            }
            assert_eq!(r.remaining(), 0);
        }

        #[test]
        fn composite_map() {
            // Write a map {"name": "Alice", "age": 30, "scores": [95, 87]}
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

            let buf = w.into_bytes();
            let mut r = PackStreamReader::new(&buf);

            assert_eq!(r.read_map_header().unwrap(), 3);
            assert_eq!(r.read_string().unwrap(), "name");
            assert_eq!(r.read_string().unwrap(), "Alice");
            assert_eq!(r.read_string().unwrap(), "age");
            assert_eq!(r.read_int().unwrap(), 30);
            assert_eq!(r.read_string().unwrap(), "scores");
            assert_eq!(r.read_list_header().unwrap(), 2);
            assert_eq!(r.read_int().unwrap(), 95);
            assert_eq!(r.read_int().unwrap(), 87);
            assert_eq!(r.remaining(), 0);
        }
    }
}
