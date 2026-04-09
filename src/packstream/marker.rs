// PackStream marker byte constants.

// Null
pub const NULL: u8 = 0xC0;

// Boolean
pub const TRUE: u8 = 0xC3;
pub const FALSE: u8 = 0xC2;

// Float
pub const FLOAT64: u8 = 0xC1;

// Integer markers
pub const INT8: u8 = 0xC8;
pub const INT16: u8 = 0xC9;
pub const INT32: u8 = 0xCA;
pub const INT64: u8 = 0xCB;

// Tiny int range: values -16..=127 are encoded inline as a single byte.
pub const TINY_INT_MIN: i8 = -16; // 0xF0 as i8
pub const TINY_INT_MAX: i8 = 127; // 0x7F as i8

// String markers
pub const TINY_STRING_NIBBLE: u8 = 0x80; // range 0x80..0x8F, size in low nibble
pub const STRING8: u8 = 0xD0;
pub const STRING16: u8 = 0xD1;
pub const STRING32: u8 = 0xD2;

// List markers
pub const TINY_LIST_NIBBLE: u8 = 0x90; // range 0x90..0x9F, size in low nibble
pub const LIST8: u8 = 0xD4;
pub const LIST16: u8 = 0xD5;
pub const LIST32: u8 = 0xD6;

// Map markers
pub const TINY_MAP_NIBBLE: u8 = 0xA0; // range 0xA0..0xAF, size in low nibble
pub const MAP8: u8 = 0xD8;
pub const MAP16: u8 = 0xD9;
pub const MAP32: u8 = 0xDA;

// Bytes markers
pub const BYTES8: u8 = 0xCC;
pub const BYTES16: u8 = 0xCD;
pub const BYTES32: u8 = 0xCE;

// Struct markers
pub const TINY_STRUCT_NIBBLE: u8 = 0xB0; // range 0xB0..0xBF, size in low nibble
pub const STRUCT8: u8 = 0xDC;
pub const STRUCT16: u8 = 0xDD;
