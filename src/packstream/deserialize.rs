// PackStream read (deserialization) — zero-copy reader.

use crate::packstream::marker;

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
        if self.pos + n > self.data.len() {
            return Err(PackStreamError::UnexpectedEof);
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
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

    // ---- size helpers for containers/strings/bytes ----

    /// Read a string length from a marker byte (already consumed).
    fn string_len(&mut self, m: u8) -> Result<usize, PackStreamError> {
        match m {
            0x80..=0x8F => Ok((m & 0x0F) as usize),
            marker::STRING8 => Ok(self.read_u8()? as usize),
            marker::STRING16 => Ok(self.read_u16()? as usize),
            marker::STRING32 => Ok(self.read_u32()? as usize),
            _ => Err(PackStreamError::InvalidMarker(m)),
        }
    }

    fn bytes_len(&mut self, m: u8) -> Result<usize, PackStreamError> {
        match m {
            marker::BYTES8 => Ok(self.read_u8()? as usize),
            marker::BYTES16 => Ok(self.read_u16()? as usize),
            marker::BYTES32 => Ok(self.read_u32()? as usize),
            _ => Err(PackStreamError::InvalidMarker(m)),
        }
    }

    fn list_len(&mut self, m: u8) -> Result<usize, PackStreamError> {
        match m {
            0x90..=0x9F => Ok((m & 0x0F) as usize),
            marker::LIST8 => Ok(self.read_u8()? as usize),
            marker::LIST16 => Ok(self.read_u16()? as usize),
            marker::LIST32 => Ok(self.read_u32()? as usize),
            _ => Err(PackStreamError::InvalidMarker(m)),
        }
    }

    fn map_len(&mut self, m: u8) -> Result<usize, PackStreamError> {
        match m {
            0xA0..=0xAF => Ok((m & 0x0F) as usize),
            marker::MAP8 => Ok(self.read_u8()? as usize),
            marker::MAP16 => Ok(self.read_u16()? as usize),
            marker::MAP32 => Ok(self.read_u32()? as usize),
            _ => Err(PackStreamError::InvalidMarker(m)),
        }
    }

    fn struct_len(&mut self, m: u8) -> Result<usize, PackStreamError> {
        match m {
            0xB0..=0xBF => Ok((m & 0x0F) as usize),
            marker::STRUCT8 => Ok(self.read_u8()? as usize),
            marker::STRUCT16 => Ok(self.read_u16()? as usize),
            _ => Err(PackStreamError::InvalidMarker(m)),
        }
    }

    // ---- public read methods ----

    /// Consume a NULL marker.
    pub fn read_null(&mut self) -> Result<(), PackStreamError> {
        let m = self.read_u8()?;
        if m == marker::NULL {
            Ok(())
        } else {
            Err(PackStreamError::InvalidMarker(m))
        }
    }

    /// Read a boolean value.
    pub fn read_bool(&mut self) -> Result<bool, PackStreamError> {
        let m = self.read_u8()?;
        match m {
            marker::TRUE => Ok(true),
            marker::FALSE => Ok(false),
            _ => Err(PackStreamError::InvalidMarker(m)),
        }
    }

    /// Read an integer value from any int marker (TINY_INT, INT8, INT16, INT32, INT64).
    pub fn read_int(&mut self) -> Result<i64, PackStreamError> {
        let m = self.read_u8()?;
        match m {
            // TINY_INT: the marker byte IS the value (interpreted as signed)
            _ if (m as i8) >= marker::TINY_INT_MIN => Ok((m as i8) as i64),
            marker::INT8 => Ok(self.read_i8()? as i64),
            marker::INT16 => Ok(self.read_i16()? as i64),
            marker::INT32 => Ok(self.read_i32()? as i64),
            marker::INT64 => self.read_i64(),
            _ => Err(PackStreamError::InvalidMarker(m)),
        }
    }

    /// Read a 64-bit float value.
    pub fn read_float(&mut self) -> Result<f64, PackStreamError> {
        let m = self.read_u8()?;
        if m != marker::FLOAT64 {
            return Err(PackStreamError::InvalidMarker(m));
        }
        let bytes = self.read_exact(8)?;
        Ok(f64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Read a string value — **zero-copy**: returns a slice into the original buffer.
    pub fn read_string(&mut self) -> Result<&'a str, PackStreamError> {
        let m = self.read_u8()?;
        let len = self.string_len(m)?;
        let bytes = self.read_exact(len)?;
        std::str::from_utf8(bytes).map_err(|_| PackStreamError::InvalidUtf8)
    }

    /// Read a byte array — **zero-copy**: returns a slice into the original buffer.
    pub fn read_bytes(&mut self) -> Result<&'a [u8], PackStreamError> {
        let m = self.read_u8()?;
        let len = self.bytes_len(m)?;
        self.read_exact(len)
    }

    /// Read a list header and return the number of elements.
    pub fn read_list_header(&mut self) -> Result<usize, PackStreamError> {
        let m = self.read_u8()?;
        self.list_len(m)
    }

    /// Read a map header and return the number of entries.
    pub fn read_map_header(&mut self) -> Result<usize, PackStreamError> {
        let m = self.read_u8()?;
        self.map_len(m)
    }

    /// Read a struct header and return `(tag_byte, num_fields)`.
    pub fn read_struct_header(&mut self) -> Result<(u8, usize), PackStreamError> {
        let m = self.read_u8()?;
        let num_fields = self.struct_len(m)?;
        let tag = self.read_u8()?;
        Ok((tag, num_fields))
    }

    /// Skip any value without deserializing it. Handles nested structures recursively.
    pub fn skip_value(&mut self) -> Result<(), PackStreamError> {
        let m = self.read_u8()?;
        match m {
            // Null
            marker::NULL => Ok(()),

            // Boolean
            marker::TRUE | marker::FALSE => Ok(()),

            // Tiny int (positive 0x00..=0x7F or negative 0xF0..=0xFF)
            _ if (m as i8) >= marker::TINY_INT_MIN => Ok(()),

            // INT8
            marker::INT8 => {
                self.read_exact(1)?;
                Ok(())
            }
            // INT16
            marker::INT16 => {
                self.read_exact(2)?;
                Ok(())
            }
            // INT32
            marker::INT32 => {
                self.read_exact(4)?;
                Ok(())
            }
            // INT64
            marker::INT64 => {
                self.read_exact(8)?;
                Ok(())
            }

            // FLOAT64
            marker::FLOAT64 => {
                self.read_exact(8)?;
                Ok(())
            }

            // String
            0x80..=0x8F | marker::STRING8 | marker::STRING16 | marker::STRING32 => {
                let len = self.string_len(m)?;
                self.read_exact(len)?;
                Ok(())
            }

            // Bytes
            marker::BYTES8 | marker::BYTES16 | marker::BYTES32 => {
                let len = self.bytes_len(m)?;
                self.read_exact(len)?;
                Ok(())
            }

            // List
            0x90..=0x9F | marker::LIST8 | marker::LIST16 | marker::LIST32 => {
                let len = self.list_len(m)?;
                for _ in 0..len {
                    self.skip_value()?;
                }
                Ok(())
            }

            // Map
            0xA0..=0xAF | marker::MAP8 | marker::MAP16 | marker::MAP32 => {
                let len = self.map_len(m)?;
                for _ in 0..len {
                    self.skip_value()?; // key
                    self.skip_value()?; // value
                }
                Ok(())
            }

            // Struct
            0xB0..=0xBF | marker::STRUCT8 | marker::STRUCT16 => {
                let num_fields = self.struct_len(m)?;
                self.read_exact(1)?; // tag byte
                for _ in 0..num_fields {
                    self.skip_value()?;
                }
                Ok(())
            }

            _ => Err(PackStreamError::InvalidMarker(m)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packstream::marker;

    // ---- Null ----

    #[test]
    fn read_null() {
        let data = [marker::NULL];
        let mut r = PackStreamReader::new(&data);
        assert!(r.read_null().is_ok());
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_null_wrong_marker() {
        let data = [marker::TRUE];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(
            r.read_null(),
            Err(PackStreamError::InvalidMarker(marker::TRUE))
        );
    }

    // ---- Boolean ----

    #[test]
    fn read_bool_true() {
        let data = [marker::TRUE];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_bool().unwrap(), true);
    }

    #[test]
    fn read_bool_false() {
        let data = [marker::FALSE];
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
        let data = [marker::INT8, 0x9C];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -100);
    }

    #[test]
    fn read_int8_minus_17() {
        // -17 is the first value below TINY_INT range
        let data = [marker::INT8, (-17i8 as u8)];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -17);
    }

    #[test]
    fn read_int8_minus_128() {
        let data = [marker::INT8, 0x80]; // -128 as i8
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -128);
    }

    #[test]
    fn read_int16() {
        // 1000 = 0x03E8
        let data = [marker::INT16, 0x03, 0xE8];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 1000);
    }

    #[test]
    fn read_int16_negative() {
        // -1000 as i16 = 0xFC18
        let data = [marker::INT16, 0xFC, 0x18];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -1000);
    }

    #[test]
    fn read_int32() {
        // 100_000 = 0x000186A0
        let data = [marker::INT32, 0x00, 0x01, 0x86, 0xA0];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 100_000);
    }

    #[test]
    fn read_int32_negative() {
        let val: i32 = -100_000;
        let bytes = val.to_be_bytes();
        let data = [marker::INT32, bytes[0], bytes[1], bytes[2], bytes[3]];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), -100_000);
    }

    #[test]
    fn read_int64() {
        let val: i64 = 1_000_000_000_000;
        let bytes = val.to_be_bytes();
        let mut data = vec![marker::INT64];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), 1_000_000_000_000);
    }

    #[test]
    fn read_int64_min() {
        let val: i64 = i64::MIN;
        let bytes = val.to_be_bytes();
        let mut data = vec![marker::INT64];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_int().unwrap(), i64::MIN);
    }

    // ---- Float ----

    #[test]
    fn read_float() {
        let val: f64 = 3.14;
        let bytes = val.to_be_bytes();
        let mut data = vec![marker::FLOAT64];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert!((r.read_float().unwrap() - 3.14).abs() < f64::EPSILON);
    }

    #[test]
    fn read_float_zero() {
        let bytes = 0.0f64.to_be_bytes();
        let mut data = vec![marker::FLOAT64];
        data.extend_from_slice(&bytes);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_float().unwrap(), 0.0);
    }

    #[test]
    fn read_float_negative() {
        let val: f64 = -1.5;
        let bytes = val.to_be_bytes();
        let mut data = vec![marker::FLOAT64];
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
        let mut data = vec![marker::STRING8, 40];
        data.extend_from_slice(s.as_bytes());
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_string().unwrap(), s);
    }

    #[test]
    fn read_string16() {
        let s = "x".repeat(300);
        let len = 300u16;
        let mut data = vec![marker::STRING16];
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
        let mut data = vec![marker::BYTES8, 4];
        data.extend_from_slice(&payload);
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_bytes().unwrap(), &payload);
    }

    #[test]
    fn read_bytes_zero_copy() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut data = vec![marker::BYTES8, 4];
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
        let mut data = vec![marker::BYTES16];
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
        let data = [marker::LIST8, 20];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_list_header().unwrap(), 20);
    }

    #[test]
    fn read_list16_header() {
        let data = [marker::LIST16, 0x01, 0x00]; // 256
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_list_header().unwrap(), 256);
    }

    #[test]
    fn read_list32_header() {
        let data = [marker::LIST32, 0x00, 0x01, 0x00, 0x00]; // 65536
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
        let data = [marker::MAP8, 20];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(r.read_map_header().unwrap(), 20);
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
        let data = [marker::STRUCT8, 5, 0x52]; // 5 fields, tag = RELATIONSHIP
        let mut r = PackStreamReader::new(&data);
        let (tag, fields) = r.read_struct_header().unwrap();
        assert_eq!(tag, 0x52);
        assert_eq!(fields, 5);
    }

    // ---- skip_value ----

    #[test]
    fn skip_null() {
        let data = [marker::NULL];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_bool() {
        let data = [marker::TRUE];
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
        let data = [marker::INT8, 0x9C];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int16() {
        let data = [marker::INT16, 0x03, 0xE8];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int32() {
        let data = [marker::INT32, 0x00, 0x01, 0x86, 0xA0];
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_int64() {
        let mut data = vec![marker::INT64];
        data.extend_from_slice(&42i64.to_be_bytes());
        let mut r = PackStreamReader::new(&data);
        r.skip_value().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn skip_float() {
        let mut data = vec![marker::FLOAT64];
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
        let data = vec![marker::BYTES8, 3, 0x01, 0x02, 0x03];
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
        data.push(marker::NULL); // field 2: null
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
        let data = [marker::INT16, 0x03]; // missing second byte
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
        let data = [marker::NULL];
        let mut r = PackStreamReader::new(&data);
        assert_eq!(
            r.read_bool(),
            Err(PackStreamError::InvalidMarker(marker::NULL))
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
        let data = [marker::NULL, marker::TRUE];
        let r = PackStreamReader::new(&data);
        assert_eq!(r.peek(), Some(marker::NULL));
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
        data.push(marker::NULL);
        // true
        data.push(marker::TRUE);
        // int 42
        data.push(42u8);
        // string "hi"
        data.push(0x82);
        data.extend_from_slice(b"hi");
        // float 1.0
        data.push(marker::FLOAT64);
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
}
