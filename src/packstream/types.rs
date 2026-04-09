/// PackStream value type tags (for type inspection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackStreamType {
    Null,
    Boolean,
    Integer,
    Float,
    String,
    Bytes,
    List,
    Map,
    Struct,
}

/// Bolt structure tags for graph entities and temporal/spatial types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StructTag {
    Node = 0x4E,                // 'N'
    Relationship = 0x52,        // 'R'
    UnboundRelationship = 0x72, // 'r'
    Path = 0x50,                // 'P'
    Point2D = 0x58,             // 'X'
    Point3D = 0x59,             // 'Y'
    Date = 0x44,                // 'D'
    Time = 0x54,                // 'T'
    LocalTime = 0x74,           // 't'
    DateTimeZoneId = 0x66,      // 'f'
    LocalDateTime = 0x64,       // 'd'
    Duration = 0x45,            // 'E'
}

impl StructTag {
    pub fn as_byte(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for StructTag {
    type Error = InvalidStructTag;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x4E => Ok(StructTag::Node),
            0x52 => Ok(StructTag::Relationship),
            0x72 => Ok(StructTag::UnboundRelationship),
            0x50 => Ok(StructTag::Path),
            0x58 => Ok(StructTag::Point2D),
            0x59 => Ok(StructTag::Point3D),
            0x44 => Ok(StructTag::Date),
            0x54 => Ok(StructTag::Time),
            0x74 => Ok(StructTag::LocalTime),
            0x66 => Ok(StructTag::DateTimeZoneId),
            0x64 => Ok(StructTag::LocalDateTime),
            0x45 => Ok(StructTag::Duration),
            _ => Err(InvalidStructTag(value)),
        }
    }
}

/// Error returned when a byte does not correspond to a known struct tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidStructTag(pub u8);

impl core::fmt::Display for InvalidStructTag {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "unknown struct tag: 0x{:02X}", self.0)
    }
}

impl std::error::Error for InvalidStructTag {}
