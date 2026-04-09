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
pub mod struct_tag {
    pub const NODE: u8 = 0x4E; // 'N'
    pub const RELATIONSHIP: u8 = 0x52; // 'R'
    pub const UNBOUND_RELATIONSHIP: u8 = 0x72; // 'r'
    pub const PATH: u8 = 0x50; // 'P'
    pub const POINT2D: u8 = 0x58; // 'X'
    pub const POINT3D: u8 = 0x59; // 'Y'
    pub const DATE: u8 = 0x44; // 'D'
    pub const TIME: u8 = 0x54; // 'T'
    pub const LOCAL_TIME: u8 = 0x74; // 't'
    pub const DATE_TIME_ZONE_ID: u8 = 0x66; // 'f'
    pub const LOCAL_DATE_TIME: u8 = 0x64; // 'd'
    pub const DURATION: u8 = 0x45; // 'E'
}
