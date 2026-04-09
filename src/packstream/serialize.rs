use bytes::{BufMut, BytesMut};

use super::marker::Marker;

/// Writes PackStream-encoded data directly to a byte buffer.
/// Streaming API: call methods sequentially to build messages.
/// All writes go directly into the buffer — no intermediate value types.
pub struct PackStreamWriter {
    buf: BytesMut,
}

impl PackStreamWriter {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::new(),
        }
    }

    pub fn write_null(&mut self) {
        self.buf
            .put_u8(Marker::Null.to_byte().expect("valid marker"));
    }

    pub fn write_bool(&mut self, value: bool) {
        self.buf.put_u8(if value {
            Marker::True.to_byte().expect("valid marker")
        } else {
            Marker::False.to_byte().expect("valid marker")
        });
    }

    /// Auto-selects minimal encoding: TINY_INT (-16..=127, 1 byte),
    /// INT8 (-128..=-17), INT16, INT32, INT64.
    pub fn write_int(&mut self, value: i64) {
        if (-16..=127).contains(&value) {
            // TINY_INT: the value itself is the marker byte (as i8 cast to u8)
            self.buf.put_u8(
                Marker::TinyInt(value as i8)
                    .to_byte()
                    .expect("valid marker"),
            );
        } else if (i8::MIN as i64..=i8::MAX as i64).contains(&value) {
            self.buf
                .put_u8(Marker::Int8.to_byte().expect("valid marker"));
            self.buf.put_i8(value as i8);
        } else if (i16::MIN as i64..=i16::MAX as i64).contains(&value) {
            self.buf
                .put_u8(Marker::Int16.to_byte().expect("valid marker"));
            self.buf.put_i16(value as i16);
        } else if (i32::MIN as i64..=i32::MAX as i64).contains(&value) {
            self.buf
                .put_u8(Marker::Int32.to_byte().expect("valid marker"));
            self.buf.put_i32(value as i32);
        } else {
            self.buf
                .put_u8(Marker::Int64.to_byte().expect("valid marker"));
            self.buf.put_i64(value);
        }
    }

    pub fn write_float(&mut self, value: f64) {
        self.buf
            .put_u8(Marker::Float64.to_byte().expect("valid marker"));
        self.buf.put_f64(value);
    }

    pub fn write_string(&mut self, s: &str) {
        let len = s.len();
        self.write_string_header(len);
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn write_bytes(&mut self, b: &[u8]) {
        let len = b.len();
        if len <= u8::MAX as usize {
            self.buf
                .put_u8(Marker::Bytes8.to_byte().expect("valid marker"));
            self.buf.put_u8(len as u8);
        } else if len <= u16::MAX as usize {
            self.buf
                .put_u8(Marker::Bytes16.to_byte().expect("valid marker"));
            self.buf.put_u16(len as u16);
        } else {
            self.buf
                .put_u8(Marker::Bytes32.to_byte().expect("valid marker"));
            self.buf.put_u32(len as u32);
        }
        self.buf.extend_from_slice(b);
    }

    pub fn write_list_header(&mut self, size: usize) {
        if size < 16 {
            self.buf.put_u8(
                Marker::TinyList(size as u8)
                    .to_byte()
                    .expect("valid marker"),
            );
        } else if size <= u8::MAX as usize {
            self.buf
                .put_u8(Marker::List8.to_byte().expect("valid marker"));
            self.buf.put_u8(size as u8);
        } else if size <= u16::MAX as usize {
            self.buf
                .put_u8(Marker::List16.to_byte().expect("valid marker"));
            self.buf.put_u16(size as u16);
        } else {
            self.buf
                .put_u8(Marker::List32.to_byte().expect("valid marker"));
            self.buf.put_u32(size as u32);
        }
    }

    pub fn write_map_header(&mut self, size: usize) {
        if size < 16 {
            self.buf
                .put_u8(Marker::TinyMap(size as u8).to_byte().expect("valid marker"));
        } else if size <= u8::MAX as usize {
            self.buf
                .put_u8(Marker::Map8.to_byte().expect("valid marker"));
            self.buf.put_u8(size as u8);
        } else if size <= u16::MAX as usize {
            self.buf
                .put_u8(Marker::Map16.to_byte().expect("valid marker"));
            self.buf.put_u16(size as u16);
        } else {
            self.buf
                .put_u8(Marker::Map32.to_byte().expect("valid marker"));
            self.buf.put_u32(size as u32);
        }
    }

    pub fn write_struct_header(&mut self, tag: u8, num_fields: usize) {
        if num_fields < 16 {
            self.buf.put_u8(
                Marker::TinyStruct(num_fields as u8)
                    .to_byte()
                    .expect("valid marker"),
            );
        } else if num_fields <= u8::MAX as usize {
            self.buf
                .put_u8(Marker::Struct8.to_byte().expect("valid marker"));
            self.buf.put_u8(num_fields as u8);
        } else {
            self.buf
                .put_u8(Marker::Struct16.to_byte().expect("valid marker"));
            self.buf.put_u16(num_fields as u16);
        }
        self.buf.put_u8(tag);
    }

    pub fn into_bytes(self) -> BytesMut {
        self.buf
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    fn write_string_header(&mut self, len: usize) {
        if len < 16 {
            self.buf.put_u8(
                Marker::TinyString(len as u8)
                    .to_byte()
                    .expect("valid marker"),
            );
        } else if len <= u8::MAX as usize {
            self.buf
                .put_u8(Marker::String8.to_byte().expect("valid marker"));
            self.buf.put_u8(len as u8);
        } else if len <= u16::MAX as usize {
            self.buf
                .put_u8(Marker::String16.to_byte().expect("valid marker"));
            self.buf.put_u16(len as u16);
        } else {
            self.buf
                .put_u8(Marker::String32.to_byte().expect("valid marker"));
            self.buf.put_u32(len as u32);
        }
    }
}

impl Default for PackStreamWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null() {
        let mut w = PackStreamWriter::new();
        w.write_null();
        assert_eq!(w.as_bytes(), &[0xC0]);
    }

    #[test]
    fn test_bool_true() {
        let mut w = PackStreamWriter::new();
        w.write_bool(true);
        assert_eq!(w.as_bytes(), &[0xC3]);
    }

    #[test]
    fn test_bool_false() {
        let mut w = PackStreamWriter::new();
        w.write_bool(false);
        assert_eq!(w.as_bytes(), &[0xC2]);
    }

    // --- Integer tests ---

    #[test]
    fn test_tiny_int_zero() {
        let mut w = PackStreamWriter::new();
        w.write_int(0);
        assert_eq!(w.as_bytes(), &[0x00]);
    }

    #[test]
    fn test_tiny_int_positive_127() {
        let mut w = PackStreamWriter::new();
        w.write_int(127);
        assert_eq!(w.as_bytes(), &[0x7F]);
    }

    #[test]
    fn test_tiny_int_negative_16() {
        let mut w = PackStreamWriter::new();
        w.write_int(-16);
        // -16 as i8 = 0xF0
        assert_eq!(w.as_bytes(), &[0xF0]);
    }

    #[test]
    fn test_int8_negative_17() {
        let mut w = PackStreamWriter::new();
        w.write_int(-17);
        // -17 as i8 = 0xEF
        assert_eq!(w.as_bytes(), &[0xC8, 0xEF]);
    }

    #[test]
    fn test_int8_negative_128() {
        let mut w = PackStreamWriter::new();
        w.write_int(-128);
        assert_eq!(w.as_bytes(), &[0xC8, 0x80]);
    }

    #[test]
    fn test_int16_positive_128() {
        let mut w = PackStreamWriter::new();
        w.write_int(128);
        assert_eq!(w.as_bytes(), &[0xC9, 0x00, 0x80]);
    }

    #[test]
    fn test_int16_negative_129() {
        let mut w = PackStreamWriter::new();
        w.write_int(-129);
        // -129 as i16 big-endian = 0xFF7F
        assert_eq!(w.as_bytes(), &[0xC9, 0xFF, 0x7F]);
    }

    #[test]
    fn test_int16_min() {
        let mut w = PackStreamWriter::new();
        w.write_int(i16::MIN as i64);
        assert_eq!(w.as_bytes(), &[0xC9, 0x80, 0x00]);
    }

    #[test]
    fn test_int16_max() {
        let mut w = PackStreamWriter::new();
        w.write_int(i16::MAX as i64);
        assert_eq!(w.as_bytes(), &[0xC9, 0x7F, 0xFF]);
    }

    #[test]
    fn test_int32_min() {
        let mut w = PackStreamWriter::new();
        w.write_int(i32::MIN as i64);
        assert_eq!(w.as_bytes(), &[0xCA, 0x80, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn test_int32_max() {
        let mut w = PackStreamWriter::new();
        w.write_int(i32::MAX as i64);
        assert_eq!(w.as_bytes(), &[0xCA, 0x7F, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_int64_min() {
        let mut w = PackStreamWriter::new();
        w.write_int(i64::MIN);
        let mut expected = vec![0xCB];
        expected.extend_from_slice(&i64::MIN.to_be_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    #[test]
    fn test_int64_max() {
        let mut w = PackStreamWriter::new();
        w.write_int(i64::MAX);
        let mut expected = vec![0xCB];
        expected.extend_from_slice(&i64::MAX.to_be_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    #[test]
    fn test_int32_boundary_above_i16_max() {
        let mut w = PackStreamWriter::new();
        w.write_int(i16::MAX as i64 + 1);
        let val = (i16::MAX as i32 + 1).to_be_bytes();
        assert_eq!(w.as_bytes(), &[0xCA, val[0], val[1], val[2], val[3]]);
    }

    #[test]
    fn test_int32_boundary_below_i16_min() {
        let mut w = PackStreamWriter::new();
        w.write_int(i16::MIN as i64 - 1);
        let val = (i16::MIN as i32 - 1).to_be_bytes();
        assert_eq!(w.as_bytes(), &[0xCA, val[0], val[1], val[2], val[3]]);
    }

    #[test]
    fn test_int64_boundary_above_i32_max() {
        let mut w = PackStreamWriter::new();
        w.write_int(i32::MAX as i64 + 1);
        let mut expected = vec![0xCB];
        expected.extend_from_slice(&(i32::MAX as i64 + 1).to_be_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    #[test]
    fn test_int64_boundary_below_i32_min() {
        let mut w = PackStreamWriter::new();
        w.write_int(i32::MIN as i64 - 1);
        let mut expected = vec![0xCB];
        expected.extend_from_slice(&(i32::MIN as i64 - 1).to_be_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    // --- Float tests ---

    #[test]
    fn test_float() {
        let mut w = PackStreamWriter::new();
        w.write_float(1.5);
        let mut expected = vec![0xC1];
        expected.extend_from_slice(&1.5_f64.to_be_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    #[test]
    fn test_float_zero() {
        let mut w = PackStreamWriter::new();
        w.write_float(0.0);
        let mut expected = vec![0xC1];
        expected.extend_from_slice(&0.0_f64.to_be_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    // --- String tests ---

    #[test]
    fn test_string_empty() {
        let mut w = PackStreamWriter::new();
        w.write_string("");
        assert_eq!(w.as_bytes(), &[0x80]); // TINY_STRING | 0
    }

    #[test]
    fn test_string_tiny_max() {
        let mut w = PackStreamWriter::new();
        let s = "a".repeat(15);
        w.write_string(&s);
        let mut expected = vec![0x8F]; // TINY_STRING | 15
        expected.extend_from_slice(s.as_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    #[test]
    fn test_string8_boundary() {
        let mut w = PackStreamWriter::new();
        let s = "a".repeat(16);
        w.write_string(&s);
        let mut expected = vec![0xD0, 16]; // STRING8, len=16
        expected.extend_from_slice(s.as_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    #[test]
    fn test_string8_max() {
        let mut w = PackStreamWriter::new();
        let s = "a".repeat(255);
        w.write_string(&s);
        let mut expected = vec![0xD0, 255]; // STRING8, len=255
        expected.extend_from_slice(s.as_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    #[test]
    fn test_string16_boundary() {
        let mut w = PackStreamWriter::new();
        let s = "a".repeat(256);
        w.write_string(&s);
        let mut expected = vec![0xD1, 0x01, 0x00]; // STRING16, len=256 BE
        expected.extend_from_slice(s.as_bytes());
        assert_eq!(w.as_bytes(), &expected);
    }

    // --- Bytes tests ---

    #[test]
    fn test_bytes_empty() {
        let mut w = PackStreamWriter::new();
        w.write_bytes(&[]);
        assert_eq!(w.as_bytes(), &[0xCC, 0x00]); // BYTES8, len=0
    }

    #[test]
    fn test_bytes_nonempty() {
        let mut w = PackStreamWriter::new();
        w.write_bytes(&[0xDE, 0xAD]);
        assert_eq!(w.as_bytes(), &[0xCC, 0x02, 0xDE, 0xAD]);
    }

    #[test]
    fn test_bytes16_boundary() {
        let mut w = PackStreamWriter::new();
        let b = vec![0x42; 256];
        w.write_bytes(&b);
        let mut expected = vec![0xCD, 0x01, 0x00]; // BYTES16, len=256 BE
        expected.extend_from_slice(&b);
        assert_eq!(w.as_bytes(), &expected);
    }

    // --- List header tests ---

    #[test]
    fn test_list_header_empty() {
        let mut w = PackStreamWriter::new();
        w.write_list_header(0);
        assert_eq!(w.as_bytes(), &[0x90]); // TINY_LIST | 0
    }

    #[test]
    fn test_list_header_tiny_max() {
        let mut w = PackStreamWriter::new();
        w.write_list_header(15);
        assert_eq!(w.as_bytes(), &[0x9F]); // TINY_LIST | 15
    }

    #[test]
    fn test_list_header_8() {
        let mut w = PackStreamWriter::new();
        w.write_list_header(16);
        assert_eq!(w.as_bytes(), &[0xD4, 0x10]); // LIST8, size=16
    }

    #[test]
    fn test_list_header_16() {
        let mut w = PackStreamWriter::new();
        w.write_list_header(256);
        assert_eq!(w.as_bytes(), &[0xD5, 0x01, 0x00]); // LIST16, size=256 BE
    }

    // --- Map header tests ---

    #[test]
    fn test_map_header_empty() {
        let mut w = PackStreamWriter::new();
        w.write_map_header(0);
        assert_eq!(w.as_bytes(), &[0xA0]); // TINY_MAP | 0
    }

    #[test]
    fn test_map_header_tiny_max() {
        let mut w = PackStreamWriter::new();
        w.write_map_header(15);
        assert_eq!(w.as_bytes(), &[0xAF]); // TINY_MAP | 15
    }

    #[test]
    fn test_map_header_8() {
        let mut w = PackStreamWriter::new();
        w.write_map_header(16);
        assert_eq!(w.as_bytes(), &[0xD8, 0x10]); // MAP8, size=16
    }

    #[test]
    fn test_map_header_16() {
        let mut w = PackStreamWriter::new();
        w.write_map_header(256);
        assert_eq!(w.as_bytes(), &[0xD9, 0x01, 0x00]); // MAP16, size=256 BE
    }

    // --- Struct header tests ---

    #[test]
    fn test_struct_header_tiny() {
        let mut w = PackStreamWriter::new();
        w.write_struct_header(0x4E, 3); // NODE tag, 3 fields
        assert_eq!(w.as_bytes(), &[0xB3, 0x4E]); // TINY_STRUCT | 3, tag
    }

    #[test]
    fn test_struct_header_8() {
        let mut w = PackStreamWriter::new();
        w.write_struct_header(0x4E, 16);
        assert_eq!(w.as_bytes(), &[0xDC, 0x10, 0x4E]); // STRUCT8, 16, tag
    }

    #[test]
    fn test_struct_header_16() {
        let mut w = PackStreamWriter::new();
        w.write_struct_header(0x4E, 256);
        assert_eq!(w.as_bytes(), &[0xDD, 0x01, 0x00, 0x4E]); // STRUCT16, 256 BE, tag
    }

    // --- Utility tests ---

    #[test]
    fn test_into_bytes() {
        let mut w = PackStreamWriter::new();
        w.write_null();
        let bytes = w.into_bytes();
        assert_eq!(&bytes[..], &[0xC0]);
    }

    #[test]
    fn test_clear() {
        let mut w = PackStreamWriter::new();
        w.write_null();
        assert_eq!(w.as_bytes().len(), 1);
        w.clear();
        assert_eq!(w.as_bytes().len(), 0);
    }

    #[test]
    fn test_default() {
        let w = PackStreamWriter::default();
        assert_eq!(w.as_bytes().len(), 0);
    }

    #[test]
    fn test_multiple_writes() {
        let mut w = PackStreamWriter::new();
        w.write_null();
        w.write_bool(true);
        w.write_int(42);
        assert_eq!(w.as_bytes(), &[0xC0, 0xC3, 0x2A]);
    }
}
