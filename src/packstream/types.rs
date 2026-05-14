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
    DateTime = 0x49,            // 'I'
    DateTimeZoneId = 0x69,      // 'i'
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
            0x49 => Ok(StructTag::DateTime),
            0x69 => Ok(StructTag::DateTimeZoneId),
            0x64 => Ok(StructTag::LocalDateTime),
            0x45 => Ok(StructTag::Duration),
            _ => Err(InvalidStructTag(value)),
        }
    }
}

/// Error returned when a byte does not correspond to a known struct tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unknown struct tag: 0x{0:02X}")]
pub struct InvalidStructTag(pub u8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_tag_round_trip() {
        let variants = [
            StructTag::Node,
            StructTag::Relationship,
            StructTag::UnboundRelationship,
            StructTag::Path,
            StructTag::Point2D,
            StructTag::Point3D,
            StructTag::Date,
            StructTag::Time,
            StructTag::LocalTime,
            StructTag::DateTime,
            StructTag::DateTimeZoneId,
            StructTag::LocalDateTime,
            StructTag::Duration,
        ];
        for tag in variants {
            let byte = tag as u8;
            let recovered = StructTag::try_from(byte).unwrap();
            assert_eq!(
                tag, recovered,
                "round-trip failed for {tag:?} (0x{byte:02X})"
            );
        }
    }

    #[test]
    fn struct_tag_specific_byte_values() {
        assert_eq!(StructTag::Node as u8, 0x4E);
        assert_eq!(StructTag::Relationship as u8, 0x52);
        assert_eq!(StructTag::UnboundRelationship as u8, 0x72);
        assert_eq!(StructTag::Path as u8, 0x50);
        assert_eq!(StructTag::Point2D as u8, 0x58);
        assert_eq!(StructTag::Point3D as u8, 0x59);
        assert_eq!(StructTag::Date as u8, 0x44);
        assert_eq!(StructTag::Time as u8, 0x54);
        assert_eq!(StructTag::LocalTime as u8, 0x74);
        assert_eq!(StructTag::DateTime as u8, 0x49);
        assert_eq!(StructTag::DateTimeZoneId as u8, 0x69);
        assert_eq!(StructTag::LocalDateTime as u8, 0x64);
        assert_eq!(StructTag::Duration as u8, 0x45);
    }

    #[test]
    fn struct_tag_invalid_byte() {
        assert_eq!(StructTag::try_from(0x00), Err(InvalidStructTag(0x00)));
        assert_eq!(StructTag::try_from(0xFF), Err(InvalidStructTag(0xFF)));
        assert_eq!(StructTag::try_from(0x66), Err(InvalidStructTag(0x66)));
    }

    #[test]
    fn struct_tag_as_byte() {
        assert_eq!(StructTag::Node.as_byte(), 0x4E);
        assert_eq!(StructTag::Duration.as_byte(), 0x45);
    }

    #[test]
    fn invalid_struct_tag_display() {
        let err = InvalidStructTag(0xAB);
        assert_eq!(format!("{err}"), "unknown struct tag: 0xAB");
    }
}
