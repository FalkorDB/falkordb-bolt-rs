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

/// Error returned when a byte is not a valid PackStream marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMarker(pub u8);

impl core::fmt::Display for InvalidMarker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid PackStream marker: 0x{:02X}", self.0)
    }
}

impl std::error::Error for InvalidMarker {}

impl Marker {
    /// Parse a raw byte into a `Marker`.
    pub fn from_byte(b: u8) -> Result<Marker, InvalidMarker> {
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
            _ => Err(InvalidMarker(b)),
        }
    }

    /// Encode this marker back to its byte representation.
    pub fn to_byte(&self) -> u8 {
        match self {
            Marker::Null => 0xC0,
            Marker::True => 0xC3,
            Marker::False => 0xC2,
            Marker::Float64 => 0xC1,
            Marker::TinyInt(v) => *v as u8,
            Marker::Int8 => 0xC8,
            Marker::Int16 => 0xC9,
            Marker::Int32 => 0xCA,
            Marker::Int64 => 0xCB,
            Marker::TinyString(size) => 0x80 | size,
            Marker::String8 => 0xD0,
            Marker::String16 => 0xD1,
            Marker::String32 => 0xD2,
            Marker::TinyList(size) => 0x90 | size,
            Marker::List8 => 0xD4,
            Marker::List16 => 0xD5,
            Marker::List32 => 0xD6,
            Marker::TinyMap(size) => 0xA0 | size,
            Marker::Map8 => 0xD8,
            Marker::Map16 => 0xD9,
            Marker::Map32 => 0xDA,
            Marker::Bytes8 => 0xCC,
            Marker::Bytes16 => 0xCD,
            Marker::Bytes32 => 0xCE,
            Marker::TinyStruct(size) => 0xB0 | size,
            Marker::Struct8 => 0xDC,
            Marker::Struct16 => 0xDD,
        }
    }
}
