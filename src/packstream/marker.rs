/// PackStream marker enum — parsed representation of a marker byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Null,
    True,
    False,
    Float64,
    TinyInt(i8), // -16..=127, value IS the byte
    Int8,
    Int16,
    Int32,
    Int64,
    TinyString(u8), // size 0..=15
    String8,
    String16,
    String32,
    TinyList(u8), // size 0..=15
    List8,
    List16,
    List32,
    TinyMap(u8), // size 0..=15
    Map8,
    Map16,
    Map32,
    Bytes8,
    Bytes16,
    Bytes32,
    TinyStruct(u8), // size 0..=15
    Struct8,
    Struct16,
}

/// Errors related to marker construction, encoding, or decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MarkerError {
    /// The byte does not correspond to any valid PackStream marker.
    #[error("invalid PackStream marker: 0x{0:02X}")]
    InvalidByte(u8),
    /// TinyInt value is outside the valid range (-16..=127).
    #[error("TinyInt value {0} out of range (-16..=127)")]
    TinyIntOutOfRange(i8),
    /// Tiny container/string size exceeds 0x0F.
    #[error("tiny size {0} out of range (0..=15)")]
    TinySizeOutOfRange(u8),
}

impl Marker {
    /// Parse a raw byte into a `Marker`.
    pub fn from_byte(b: u8) -> Result<Marker, MarkerError> {
        match b {
            // TinyInt positive range: 0x00..=0x7F
            0x00..=0x7F => Ok(Marker::TinyInt(b as i8)),
            // TinyString: 0x80..=0x8F
            0x80..=0x8F => Ok(Marker::TinyString(b & 0x0F)),
            // TinyList: 0x90..=0x9F
            0x90..=0x9F => Ok(Marker::TinyList(b & 0x0F)),
            // TinyMap: 0xA0..=0xAF
            0xA0..=0xAF => Ok(Marker::TinyMap(b & 0x0F)),
            // TinyStruct: 0xB0..=0xBF
            0xB0..=0xBF => Ok(Marker::TinyStruct(b & 0x0F)),
            // Singleton markers
            0xC0 => Ok(Marker::Null),
            0xC1 => Ok(Marker::Float64),
            0xC2 => Ok(Marker::False),
            0xC3 => Ok(Marker::True),
            // Integer markers
            0xC8 => Ok(Marker::Int8),
            0xC9 => Ok(Marker::Int16),
            0xCA => Ok(Marker::Int32),
            0xCB => Ok(Marker::Int64),
            // Bytes markers
            0xCC => Ok(Marker::Bytes8),
            0xCD => Ok(Marker::Bytes16),
            0xCE => Ok(Marker::Bytes32),
            // String markers
            0xD0 => Ok(Marker::String8),
            0xD1 => Ok(Marker::String16),
            0xD2 => Ok(Marker::String32),
            // List markers
            0xD4 => Ok(Marker::List8),
            0xD5 => Ok(Marker::List16),
            0xD6 => Ok(Marker::List32),
            // Map markers
            0xD8 => Ok(Marker::Map8),
            0xD9 => Ok(Marker::Map16),
            0xDA => Ok(Marker::Map32),
            // Struct markers
            0xDC => Ok(Marker::Struct8),
            0xDD => Ok(Marker::Struct16),
            // TinyInt negative range: 0xF0..=0xFF
            0xF0..=0xFF => Ok(Marker::TinyInt(b as i8)),
            // Anything else is invalid
            _ => Err(MarkerError::InvalidByte(b)),
        }
    }

    /// Encode this marker back to its byte representation.
    ///
    /// Returns an error if a `Tiny*` variant holds a value outside its valid range.
    pub fn to_byte(&self) -> Result<u8, MarkerError> {
        match self {
            Marker::Null => Ok(0xC0),
            Marker::True => Ok(0xC3),
            Marker::False => Ok(0xC2),
            Marker::Float64 => Ok(0xC1),
            Marker::TinyInt(v) => {
                if !(-16..=127).contains(v) {
                    return Err(MarkerError::TinyIntOutOfRange(*v));
                }
                Ok(*v as u8)
            }
            Marker::Int8 => Ok(0xC8),
            Marker::Int16 => Ok(0xC9),
            Marker::Int32 => Ok(0xCA),
            Marker::Int64 => Ok(0xCB),
            Marker::TinyString(size) => {
                if *size > 0x0F {
                    return Err(MarkerError::TinySizeOutOfRange(*size));
                }
                Ok(0x80 | size)
            }
            Marker::String8 => Ok(0xD0),
            Marker::String16 => Ok(0xD1),
            Marker::String32 => Ok(0xD2),
            Marker::TinyList(size) => {
                if *size > 0x0F {
                    return Err(MarkerError::TinySizeOutOfRange(*size));
                }
                Ok(0x90 | size)
            }
            Marker::List8 => Ok(0xD4),
            Marker::List16 => Ok(0xD5),
            Marker::List32 => Ok(0xD6),
            Marker::TinyMap(size) => {
                if *size > 0x0F {
                    return Err(MarkerError::TinySizeOutOfRange(*size));
                }
                Ok(0xA0 | size)
            }
            Marker::Map8 => Ok(0xD8),
            Marker::Map16 => Ok(0xD9),
            Marker::Map32 => Ok(0xDA),
            Marker::Bytes8 => Ok(0xCC),
            Marker::Bytes16 => Ok(0xCD),
            Marker::Bytes32 => Ok(0xCE),
            Marker::TinyStruct(size) => {
                if *size > 0x0F {
                    return Err(MarkerError::TinySizeOutOfRange(*size));
                }
                Ok(0xB0 | size)
            }
            Marker::Struct8 => Ok(0xDC),
            Marker::Struct16 => Ok(0xDD),
        }
    }

    /// Encode this marker to its byte representation, panicking on invalid `Tiny*` values.
    ///
    /// Use this when the marker is known to be valid (e.g., singleton markers or
    /// `Tiny*` variants constructed from already-validated values).
    pub fn byte(self) -> u8 {
        match self.to_byte() {
            Ok(b) => b,
            Err(e) => panic!("invalid marker: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_markers_round_trip() {
        let singletons = [
            (Marker::Null, 0xC0),
            (Marker::Float64, 0xC1),
            (Marker::False, 0xC2),
            (Marker::True, 0xC3),
            (Marker::Int8, 0xC8),
            (Marker::Int16, 0xC9),
            (Marker::Int32, 0xCA),
            (Marker::Int64, 0xCB),
            (Marker::Bytes8, 0xCC),
            (Marker::Bytes16, 0xCD),
            (Marker::Bytes32, 0xCE),
            (Marker::String8, 0xD0),
            (Marker::String16, 0xD1),
            (Marker::String32, 0xD2),
            (Marker::List8, 0xD4),
            (Marker::List16, 0xD5),
            (Marker::List32, 0xD6),
            (Marker::Map8, 0xD8),
            (Marker::Map16, 0xD9),
            (Marker::Map32, 0xDA),
            (Marker::Struct8, 0xDC),
            (Marker::Struct16, 0xDD),
        ];
        for (marker, byte) in singletons {
            assert_eq!(
                marker.to_byte().unwrap(),
                byte,
                "{marker:?} should encode to 0x{byte:02X}"
            );
            assert_eq!(
                Marker::from_byte(byte).unwrap(),
                marker,
                "0x{byte:02X} should decode to {marker:?}"
            );
        }
    }

    #[test]
    fn tiny_int_positive_boundary() {
        assert_eq!(Marker::from_byte(0x00).unwrap(), Marker::TinyInt(0));
        assert_eq!(Marker::from_byte(0x7F).unwrap(), Marker::TinyInt(127));
        assert_eq!(Marker::TinyInt(0).to_byte().unwrap(), 0x00);
        assert_eq!(Marker::TinyInt(127).to_byte().unwrap(), 0x7F);
    }

    #[test]
    fn tiny_int_negative_boundary() {
        assert_eq!(Marker::from_byte(0xF0).unwrap(), Marker::TinyInt(-16));
        assert_eq!(Marker::from_byte(0xFF).unwrap(), Marker::TinyInt(-1));
        assert_eq!(Marker::TinyInt(-16).to_byte().unwrap(), 0xF0);
        assert_eq!(Marker::TinyInt(-1).to_byte().unwrap(), 0xFF);
    }

    #[test]
    fn tiny_string_boundaries() {
        assert_eq!(Marker::from_byte(0x80).unwrap(), Marker::TinyString(0));
        assert_eq!(Marker::from_byte(0x8F).unwrap(), Marker::TinyString(15));
        assert_eq!(Marker::TinyString(0).to_byte().unwrap(), 0x80);
        assert_eq!(Marker::TinyString(15).to_byte().unwrap(), 0x8F);
    }

    #[test]
    fn tiny_list_boundaries() {
        assert_eq!(Marker::from_byte(0x90).unwrap(), Marker::TinyList(0));
        assert_eq!(Marker::from_byte(0x9F).unwrap(), Marker::TinyList(15));
        assert_eq!(Marker::TinyList(0).to_byte().unwrap(), 0x90);
        assert_eq!(Marker::TinyList(15).to_byte().unwrap(), 0x9F);
    }

    #[test]
    fn tiny_map_boundaries() {
        assert_eq!(Marker::from_byte(0xA0).unwrap(), Marker::TinyMap(0));
        assert_eq!(Marker::from_byte(0xAF).unwrap(), Marker::TinyMap(15));
        assert_eq!(Marker::TinyMap(0).to_byte().unwrap(), 0xA0);
        assert_eq!(Marker::TinyMap(15).to_byte().unwrap(), 0xAF);
    }

    #[test]
    fn tiny_struct_boundaries() {
        assert_eq!(Marker::from_byte(0xB0).unwrap(), Marker::TinyStruct(0));
        assert_eq!(Marker::from_byte(0xBF).unwrap(), Marker::TinyStruct(15));
        assert_eq!(Marker::TinyStruct(0).to_byte().unwrap(), 0xB0);
        assert_eq!(Marker::TinyStruct(15).to_byte().unwrap(), 0xBF);
    }

    #[test]
    fn invalid_reserved_bytes() {
        for byte in [0xC4, 0xC5, 0xC6, 0xC7] {
            assert_eq!(
                Marker::from_byte(byte),
                Err(MarkerError::InvalidByte(byte)),
                "0x{byte:02X} should be invalid"
            );
        }
    }

    #[test]
    fn all_valid_bytes_round_trip() {
        for byte in 0x00u8..=0xFF {
            if let Ok(marker) = Marker::from_byte(byte) {
                assert_eq!(
                    marker.to_byte().unwrap(),
                    byte,
                    "round-trip failed for 0x{byte:02X}"
                );
            }
        }
    }

    #[test]
    fn invalid_marker_display() {
        let err = MarkerError::InvalidByte(0xC4);
        assert_eq!(format!("{err}"), "invalid PackStream marker: 0xC4");
    }

    // --- out-of-range tests ---

    #[test]
    fn tiny_int_out_of_range() {
        assert_eq!(
            Marker::TinyInt(-17).to_byte(),
            Err(MarkerError::TinyIntOutOfRange(-17))
        );
        assert_eq!(
            Marker::TinyInt(i8::MIN).to_byte(),
            Err(MarkerError::TinyIntOutOfRange(i8::MIN))
        );
    }

    #[test]
    fn tiny_string_out_of_range() {
        assert_eq!(
            Marker::TinyString(16).to_byte(),
            Err(MarkerError::TinySizeOutOfRange(16))
        );
    }

    #[test]
    fn tiny_list_out_of_range() {
        assert_eq!(
            Marker::TinyList(16).to_byte(),
            Err(MarkerError::TinySizeOutOfRange(16))
        );
    }

    #[test]
    fn tiny_map_out_of_range() {
        assert_eq!(
            Marker::TinyMap(16).to_byte(),
            Err(MarkerError::TinySizeOutOfRange(16))
        );
    }

    #[test]
    fn tiny_struct_out_of_range() {
        assert_eq!(
            Marker::TinyStruct(16).to_byte(),
            Err(MarkerError::TinySizeOutOfRange(16))
        );
    }
}
