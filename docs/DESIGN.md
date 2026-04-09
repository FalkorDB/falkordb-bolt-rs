# falkordb-bolt-rs: Bolt Protocol Implementation Plan

## Context

FalkorDB (C) and falkordb-rs-next-gen (Rust) are both Redis modules that use RESP as their client-server protocol. To become a drop-in replacement for Neo4j, we need to implement the Neo4j Bolt protocol (v5.8). FalkorDB C already has a working but unmaintained Bolt stub (`src/bolt/`). This project creates a standalone Rust crate that both projects can use - the C project via FFI, the Rust project natively.

**Decisions**: Bolt 5.8 | Replace C impl via FFI | Pluggable auth | TCP + WebSocket | True streaming PULL

**Multi-version support**: During handshake, the server accepts client proposals for Bolt 5.1-5.8 and negotiates the highest mutually supported version. Version differences are handled internally in the protocol layer (e.g., pre-5.1: auth in HELLO; 5.1+: separate LOGON/LOGOFF; 5.4+: TELEMETRY message accepted/ignored). The `BoltHandler` trait API is version-agnostic.

**Protocol evolution**: See "Protocol Versioning & Extensibility" section below.

**Compact vs Verbose**: These are FalkorDB RESP-specific concepts (CLI vs SDK format). Neo4j drivers do NOT have this distinction. Bolt always uses PackStream binary encoding. `reply_verbose()` and `reply_compact()` remain RESP-only; Bolt reply logic lives entirely inside the `BoltHandler` implementation.

**Bolt is a separate connection path**: Bolt connections do NOT go through the RESP command pipeline. They have their own TCP listener, their own connection lifecycle, and their own handler. The `BoltHandler::run()` receives the query directly from the Bolt RUN message and calls the graph engine directly - no `graph.QUERY` command, no `--bolt` flag, no `CommandDispatch`.

---

## Part 1: `falkordb-bolt-rs` Crate Architecture

### Crate Structure

```
falkordb-bolt-rs/
├── Cargo.toml
├── src/
│   ├── lib.rs                    # Public API re-exports
│   ├── packstream/
│   │   ├── mod.rs
│   │   ├── marker.rs             # PackStream marker byte constants
│   │   ├── serialize.rs          # PackStream write (serialization)
│   │   ├── deserialize.rs        # PackStream read (deserialization)
│   │   └── types.rs              # PackStream type tag constants
│   ├── protocol/
│   │   ├── mod.rs
│   │   ├── message.rs            # Request/Response message enums
│   │   ├── state.rs              # Connection state machine
│   │   ├── handshake.rs          # Version negotiation (magic bytes + version ranges)
│   │   └── chunking.rs           # Chunked transfer encoding (16-bit size headers)
│   ├── transport/
│   │   ├── mod.rs
│   │   ├── tcp.rs                # TCP listener + accept
│   │   ├── websocket.rs          # WS handshake (HTTP upgrade) + frame wrapping
│   │   └── tls.rs                # Optional TLS wrapper (feature-gated)
│   ├── server/
│   │   ├── mod.rs
│   │   ├── connection.rs         # Per-connection handler (owns state machine + buffers)
│   │   ├── handler.rs            # BoltHandler trait (pluggable command dispatch)
│   │   └── event_loop.rs         # Event loop integration (fd-based callbacks for Redis)
│   └── ffi/
│       ├── mod.rs
│       └── c_api.rs              # C FFI: opaque handles, callback types, extern "C" functions
```

### Feature Flags (Cargo.toml)

```toml
[features]
default = ["websocket"]
websocket = ["dep:sha1", "dep:base64"]
tls = ["dep:rustls"]
ffi = []                          # Build C API (cbindgen header generation)
```

### Dependencies

```toml
[dependencies]
bytes = "1"         # Buffer management (BytesMut for zero-copy)
sha1 = { version = "0.10", optional = true }    # WebSocket handshake
base64 = { version = "0.22", optional = true }  # WebSocket handshake
rustls = { version = "0.23", optional = true }  # TLS support
log = "0.4"         # Logging

[build-dependencies]
cbindgen = "0.28"   # Generate C headers from Rust FFI
```

### Serialization: No serde

PackStream serialization is implemented manually (direct byte manipulation) via `PackStreamWriter`/`PackStreamReader`. No serde dependency. This matches the C implementation's approach, avoids data model mismatches (PackStream's tagged structs, positional fields, and multi-width integers don't map well to serde's model), and preserves the streaming writer pattern needed for the C FFI.

### Zero-Copy Design Principle

The crate has **no intermediate value type**. Both read and write paths operate directly on byte buffers via the streaming `PackStreamWriter`/`PackStreamReader` API.

#### Where copies happen and how we eliminate them

There are 4 data flow paths. Each is analyzed for copies:

```
PATH 1: Client → Server (reading incoming messages)
─────────────────────────────────────────────────────
Wire bytes → de-chunk → contiguous message buffer
                              │
                     PackStreamReader borrows from this buffer
                              │
                     ┌────────┴────────────┐
                     │                      │
              Rust (BoltHandler)      C (FFI callback)
              Reader returns &str     Raw PackStream bytes
              borrowed from buffer    passed as (ptr, len)
              Nothing parsed upfront  C parses with bolt_read_*
              for scalar fields       directly from buffer
```

**Reading (wire → host)**: The `ChunkDecoder` produces a contiguous `BytesMut` message buffer. The `PackStreamReader` borrows from this buffer — `read_string()` returns `&'a str` (zero-copy slice into the buffer). For the Rust API, the `BoltHandler` trait receives borrowed references. For the C API, the raw PackStream bytes are passed as `(ptr, len)` and the host uses `bolt_read_*` functions to parse directly — identical to the existing C pattern.

**When copies are unavoidable on read**: If the handler needs to store a value beyond the lifetime of the message buffer (e.g., stash query text for logging), it must explicitly `.to_owned()`. This is the caller's choice, not forced by the crate.

```
PATH 2: Server → Client (writing outgoing messages)
─────────────────────────────────────────────────────
                     ┌────────────────────────────┐
                     │                              │
              Rust (BoltHandler)              C (FFI)
              conn.write_string(&str)         bolt_reply_string(ptr, len)
              conn.write_int(i64)             bolt_reply_int(i64)
              Directly into BytesMut          Directly into BytesMut
              No intermediate types           No intermediate types
                     │                              │
                     └──────────┬───────────────────┘
                                │
                     PackStreamWriter encodes
                     directly into connection write buffer
                                │
                         chunk → wire
```

**Writing (host → wire)**: Both paths write directly into the connection's `BytesMut` write buffer via `PackStreamWriter`. The `bolt_reply_*` FFI functions and `conn.write_*` Rust methods encode PackStream markers + data in-place. No intermediate types constructed. This matches the existing C implementation's approach exactly.

```
PATH 3: Execution → QueryBuffer (buffered records)
───────────────────────────────────────────────────
              ┌────────────────────────────┐
              │                              │
       Rust (value_to_bolt)            C (bolt_reply_si_value)
       write_runtime_value()          bolt_reply_*(client, ...)
       into buffer via writer          routes to buffer when
       No intermediate copy            inside record_begin/end
              │                              │
              └──────────┬───────────────────┘
                         │
              Pre-serialized bytes stored in
              QueryBuffer.records (VecDeque<BytesMut>)
```

**Buffered records**: Records are serialized directly into pre-built PackStream bytes in the `QueryBuffer`. When PULL drains them, the bytes are `memcpy`'d to the connection's write buffer — no deserialization/re-serialization.

```
PATH 4: QueryBuffer → Client (PULL draining)
─────────────────────────────────────────────
QueryBuffer.records.pop_front() → pre-serialized BytesMut
              │
    conn.write_buf.extend_from_slice(record_bytes)
              │
    Already PackStream — just copy raw bytes to wire.
    No parsing, no deserialization, just memcpy.
```

#### No intermediate value type

There is **no intermediate value enum** in the crate. Both read and write paths use the streaming API directly:
- **Writing**: Host calls `conn.write_int()`, `conn.write_string()`, `conn.write_node_header()` etc. to encode values directly into the buffer.
- **Reading**: Host reads with `reader.read_int()`, `reader.read_string()` (borrowed `&str`), `reader.skip_value()` etc. directly from the buffer.

The host application's own value types (e.g., `Value` in falkordb-rs-next-gen, `SIValue` in FalkorDB C) are serialized/deserialized directly to/from PackStream bytes without any intermediate representation.

---

### Layer 1: PackStream

#### `packstream/types.rs` - Type tag constants

```rust
/// PackStream value type tags (for bolt_read_type / type inspection).
pub enum PackStreamType {
    Null,
    Boolean,
    Integer,
    Float,
    Bytes,
    String,
    List,
    Map,
    Struct,
}

/// Bolt structure tags for graph entities and temporal/spatial types.
pub mod struct_tag {
    pub const NODE: u8 = 0x4E;
    pub const RELATIONSHIP: u8 = 0x52;
    pub const UNBOUND_RELATIONSHIP: u8 = 0x72;
    pub const PATH: u8 = 0x50;
    pub const POINT_2D: u8 = 0x58;
    pub const POINT_3D: u8 = 0x59;
    pub const DATE: u8 = 0x44;
    pub const TIME: u8 = 0x54;
    pub const LOCAL_TIME: u8 = 0x74;
    pub const DATE_TIME: u8 = 0x49;
    pub const DATE_TIME_ZONE_ID: u8 = 0x69;
    pub const LOCAL_DATE_TIME: u8 = 0x64;
    pub const DURATION: u8 = 0x45;
}
```

#### `packstream/serialize.rs` - Writer

```rust
/// Writes PackStream-encoded data directly to a byte buffer.
/// Streaming API: call methods sequentially to build messages.
/// All writes go directly into the buffer — no intermediate value types.
pub struct PackStreamWriter {
    buf: BytesMut,
}

impl PackStreamWriter {
    pub fn new() -> Self;
    pub fn write_null(&mut self);
    pub fn write_bool(&mut self, value: bool);
    pub fn write_int(&mut self, value: i64);      // Auto-selects TINY/8/16/32/64
    pub fn write_float(&mut self, value: f64);
    pub fn write_string(&mut self, value: &str);   // Borrows, no copy of input
    pub fn write_bytes(&mut self, value: &[u8]);   // Borrows, no copy of input
    pub fn write_list_header(&mut self, size: u32);
    pub fn write_map_header(&mut self, size: u32);
    pub fn write_struct_header(&mut self, tag: u8, size: u32);

    pub fn into_bytes(self) -> BytesMut;
    pub fn as_bytes(&self) -> &[u8];
    pub fn clear(&mut self);
}
```

#### `packstream/deserialize.rs` - Reader (zero-copy)

```rust
/// Reads PackStream-encoded data from a byte buffer.
/// Returns borrowed references where possible (zero-copy for strings/bytes).
/// The lifetime 'a ties returned references to the buffer — they are valid
/// as long as the underlying message buffer is alive.
pub struct PackStreamReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PackStreamReader<'a> {
    pub fn new(data: &'a [u8]) -> Self;

    // --- Zero-copy reads (return references into buffer) ---
    pub fn read_string(&mut self) -> Result<&'a str, PackStreamError>;
    pub fn read_bytes(&mut self) -> Result<&'a [u8], PackStreamError>;

    // --- Scalar reads (copy by value, cheap) ---
    pub fn read_null(&mut self) -> Result<(), PackStreamError>;
    pub fn read_bool(&mut self) -> Result<bool, PackStreamError>;
    pub fn read_int(&mut self) -> Result<i64, PackStreamError>;
    pub fn read_float(&mut self) -> Result<f64, PackStreamError>;

    // --- Container header reads (no data copy, just size) ---
    pub fn read_list_header(&mut self) -> Result<u32, PackStreamError>;
    pub fn read_map_header(&mut self) -> Result<u32, PackStreamError>;
    pub fn read_struct_header(&mut self) -> Result<(u8, u32), PackStreamError>;

    // --- Skip (advance past a value without reading it) ---
    pub fn skip_value(&mut self) -> Result<(), PackStreamError>;

    pub fn remaining(&self) -> usize;
}
```

**Key design decisions for the reader:**

1. **`read_string()` returns `&'a str`** — zero-copy reference into the message buffer. PackStream strings are length-prefixed, so the reader knows the exact byte range. Since `ChunkDecoder` produces contiguous `BytesMut`, the string is always a valid contiguous slice.

2. **`skip_value()`** — advances the cursor past a value without allocating anything. Used when the handler doesn't need certain fields (e.g., skipping notification_filter in RUN extras).

---

### Layer 2: Protocol Messages

#### `protocol/message.rs`

```rust
/// Messages sent by the client.
/// Lifetime 'a borrows from the message buffer — string fields are zero-copy
/// references into the de-chunked buffer, not owned allocations.
pub enum BoltRequest<'a> {
    Hello(HelloMessage<'a>),
    Logon(LogonMessage<'a>),      // 5.1+
    Logoff,                       // 5.1+
    Run(RunMessage<'a>),
    Pull(PullMessage),
    Discard(DiscardMessage),
    Begin(BeginMessage<'a>),
    Commit,
    Rollback,
    Reset,
    Route(RouteMessage<'a>),
    Telemetry(TelemetryMessage),  // 5.4+
    Goodbye,
}

/// HELLO (0x01) - Connection initialization.
/// String fields borrow from the message buffer.
pub struct HelloMessage<'a> {
    pub user_agent: &'a str,
    pub bolt_agent: Option<BoltAgent<'a>>,            // 5.3+
    pub routing: Option<PackStreamSlice<'a>>,          // Raw PackStream bytes for routing context
    pub patch_bolt: Vec<&'a str>,                      // Patch negotiation
    // notification_filter is skipped (not needed by handler)
}

pub struct BoltAgent<'a> {
    pub product: &'a str,
    pub platform: Option<&'a str>,
    pub language: Option<&'a str>,
    pub language_details: Option<&'a str>,
}

/// LOGON (0x6A) - Authentication (5.1+).
pub struct LogonMessage<'a> {
    pub scheme: &'a str,              // "basic", "bearer", "kerberos", "none"
    pub principal: Option<&'a str>,   // Username (basic/kerberos)
    pub credentials: Option<&'a str>, // Password/token
    pub realm: Option<&'a str>,       // Multi-realm support
}

/// RUN (0x10) - Execute query.
/// query borrows from buffer. Parameters and extras are passed as raw
/// PackStream slices — the handler parses only what it needs.
pub struct RunMessage<'a> {
    pub query: &'a str,                     // Zero-copy from buffer
    pub parameters: PackStreamSlice<'a>,    // Raw PackStream bytes (map)
    pub extra: RunExtra<'a>,
}

/// PackStream slice — a reference to raw PackStream-encoded data in the buffer.
/// Avoids parsing nested structures that the handler may not need.
/// Provides a reader() method for on-demand zero-copy parsing.
pub struct PackStreamSlice<'a> {
    data: &'a [u8],
}

impl<'a> PackStreamSlice<'a> {
    /// Create a reader to parse this slice on demand (zero-copy).
    pub fn reader(&self) -> PackStreamReader<'a>;

    /// Get the raw bytes (for C FFI passthrough).
    pub fn as_bytes(&self) -> &'a [u8];

    pub fn len(&self) -> usize;
}

/// Typed extra fields for RUN/BEGIN.
/// String fields borrow from buffer. Complex fields use PackStreamSlice
/// so they're only parsed if the handler needs them.
pub struct RunExtra<'a> {
    pub db: Option<&'a str>,            // Target database name (most commonly needed)
    pub mode: Option<&'a str>,          // "r" or "w"
    pub tx_timeout: Option<i64>,        // Milliseconds
    pub imp_user: Option<&'a str>,      // Impersonated user
    pub bookmarks: Option<PackStreamSlice<'a>>,    // Parsed on demand
    pub tx_metadata: Option<PackStreamSlice<'a>>,  // Parsed on demand
    // notification_filter skipped (not needed by handler)
}

pub struct PullMessage { pub n: i64, pub qid: i64 }
pub struct DiscardMessage { pub n: i64, pub qid: i64 }
pub struct BeginMessage<'a> { pub extra: RunExtra<'a> }
pub struct RouteMessage<'a> {
    pub routing: PackStreamSlice<'a>,
    pub bookmarks: Option<PackStreamSlice<'a>>,
    pub db: Option<&'a str>,
}
pub struct TelemetryMessage { pub api: i64 }

/// No BoltResponse enum — responses are written directly to the connection's
/// write buffer via conn.write_success_header(), conn.write_failure(), etc.

/// Parsing: version-aware, zero-copy deserialization.
/// String fields are borrowed from the message buffer.
/// Complex nested structures (parameters, bookmarks, metadata) are captured as
/// PackStreamSlice references — only parsed on demand by the handler.
impl<'a> BoltRequest<'a> {
    pub fn parse(reader: &mut PackStreamReader<'a>, version: BoltVersion) -> Result<Self, BoltError> {
        let (tag, _size) = reader.read_struct_header()?;
        match tag {
            0x01 => Ok(BoltRequest::Hello(HelloMessage::parse(reader, version)?)),
            0x6A => {
                if version.minor < 1 { return Err(BoltError::unsupported("LOGON", "5.1")); }
                Ok(BoltRequest::Logon(LogonMessage::parse(reader, version)?))
            }
            // ...
        }
    }
}

/// Responses: written directly via conn.write_success_header(),
/// conn.write_failure(), conn.write_ignored(). No response enum needed.
```

Parsing: `BoltRequest::parse(reader)` reads struct tag and dispatches. String fields are borrowed from the buffer. Nested structures (parameters, extras) are captured as `PackStreamSlice` — raw byte references that can be parsed on demand.

Serialization: Responses are written directly via `conn.write_success()`, `conn.write_failure()`, etc. No intermediate `BoltResponse` enum is constructed during normal operation.

#### `protocol/state.rs` - Connection State Machine

```rust
pub enum BoltState {
    Negotiation,
    Authentication,
    Ready,
    Streaming,
    TxReady,
    TxStreaming,
    Failed,
    Interrupted,
    Defunct,
}

pub enum ResponseType { Success, Failure }

impl BoltState {
    /// Given current state + request type + response type, return next state.
    /// Returns Err if the transition is invalid (protocol violation).
    pub fn transition(
        &self,
        request: &BoltRequest,
        response_type: ResponseType,
    ) -> Result<BoltState, BoltProtocolError>;

    /// Handle the <INTERRUPT> signal (triggered by RESET arrival).
    /// This is separate from message processing — it fires immediately.
    pub fn interrupt(&self) -> Result<BoltState, BoltProtocolError> {
        match self {
            Ready | Streaming | TxReady | TxStreaming
            | Failed | Interrupted => Ok(Interrupted),
            // Cannot interrupt during Negotiation/Authentication
            _ => Err(BoltProtocolError::InvalidInterrupt),
        }
    }

    /// Check if a message should be IGNORED in the current state.
    /// In INTERRUPTED state, everything except RESET and GOODBYE is ignored.
    /// In FAILED state, everything except RESET, ROLLBACK, and GOODBYE is ignored.
    pub fn should_ignore(&self, request: &BoltRequest) -> bool {
        match self {
            Interrupted => !matches!(request, BoltRequest::Reset | BoltRequest::Goodbye),
            Failed => !matches!(request,
                BoltRequest::Reset | BoltRequest::Rollback | BoltRequest::Goodbye),
            _ => false,
        }
    }
}
```

#### `protocol/handshake.rs`

```rust
pub const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// Parse client handshake (magic + 4 version proposals), return negotiated version.
pub fn negotiate_version(data: &[u8; 20]) -> Result<BoltVersion, HandshakeError>;

/// Write server version response (4 bytes).
pub fn write_version_response(version: BoltVersion) -> [u8; 4];

pub struct BoltVersion {
    pub major: u8,
    pub minor: u8,
}
```

#### `protocol/chunking.rs`

```rust
/// Encodes a complete message into chunks with 16-bit size headers + zero terminator.
pub fn chunk_message(message: &[u8], max_chunk_size: u16) -> BytesMut;

/// Accumulates chunks from the wire, returns complete messages.
pub struct ChunkDecoder {
    buffer: BytesMut,
}

impl ChunkDecoder {
    pub fn new() -> Self;
    /// Feed raw bytes. Returns complete de-chunked messages (if any).
    pub fn feed(&mut self, data: &[u8]) -> Vec<BytesMut>;
}
```

---

### Layer 3: Server & Connection

#### `server/handler.rs` - Pluggable Command Dispatch

```rust
/// Trait that the host application implements to handle Bolt commands.
/// This is the main integration point for FalkorDB / falkordb-rs-next-gen.
///
/// Zero-copy design: String arguments borrow from the message buffer.
/// The handler must not store these references beyond the method call.
/// If the handler needs to keep data (e.g., query text for logging),
/// it must explicitly call .to_owned() / .to_string().
///
/// Complex nested data (parameters, extras) are passed as PackStreamSlice —
/// raw PackStream bytes that the handler can parse on demand using .reader().
pub trait BoltHandler: Send + Sync {
    /// Called on HELLO message.
    /// Write SUCCESS metadata directly to conn.
    fn hello(
        &self,
        conn: &mut BoltConnection,
        msg: &HelloMessage<'_>,
    ) -> Result<(), BoltError>;

    /// Called on LOGON message. Return Ok(()) for success, Err for failure.
    fn authenticate(
        &self,
        conn: &mut BoltConnection,
        msg: &LogonMessage<'_>,
    ) -> Result<(), BoltError>;

    /// Called on RUN message. Should execute the query and prepare results.
    /// The handler writes SUCCESS metadata directly to conn (fields, qid, t_first
    /// are added by the crate). Parameters and extras are PackStreamSlice —
    /// parse on demand with .reader() instead of pre-parsing everything.
    fn run(
        &self,
        conn: &mut BoltConnection,
        msg: &RunMessage<'_>,
    ) -> Result<(), BoltError>;

    /// PULL is handled internally by the crate - it drains pre-serialized
    /// RECORDs from the QueryBuffer. No host callback needed.

    /// DISCARD is handled internally by the crate - it discards buffered
    /// RECORDs without sending them. No host callback needed.

    /// Called on BEGIN message.
    fn begin(
        &self,
        conn: &mut BoltConnection,
        msg: &BeginMessage<'_>,
    ) -> Result<(), BoltError>;

    /// Called on COMMIT.
    fn commit(&self, conn: &mut BoltConnection) -> Result<(), BoltError>;

    /// Called on ROLLBACK.
    fn rollback(&self, conn: &mut BoltConnection) -> Result<(), BoltError>;

    /// Called on ROUTE message.
    fn route(
        &self,
        conn: &mut BoltConnection,
        msg: &RouteMessage<'_>,
    ) -> Result<(), BoltError>;

    /// Called on RESET message.
    fn reset(&self, conn: &mut BoltConnection) -> Result<(), BoltError>;

    /// Called on GOODBYE (connection closing).
    fn goodbye(&self, conn: &mut BoltConnection);
}
```

#### `server/connection.rs` - Per-Connection State

```rust
/// Represents a single Bolt client connection.
/// Owns the state machine, read/write buffers, and provides methods to write responses.
/// Does NOT own the query result buffer — buffers are managed separately (see QueryBuffer).
pub struct BoltConnection {
    state: BoltState,
    version: BoltVersion,
    is_websocket: bool,
    write_buf: BytesMut,
    read_buf: BytesMut,
    chunk_decoder: ChunkDecoder,
    writer: PackStreamWriter,
    user_data: *mut c_void,  // Opaque pointer for host app (e.g., RedisModuleCtx)
    interrupt_flag: Arc<InterruptFlag>,  // Shared with execution thread for RESET
}

impl BoltConnection {
    // --- Message-level writers (protocol framing) ---

    /// Write a SUCCESS response header. Caller then writes metadata key-value pairs.
    pub fn write_success_header(&mut self, metadata_count: u32);

    /// Write a FAILURE response.
    pub fn write_failure(&mut self, code: &str, message: &str);

    /// Write a RECORD header. Caller then writes field values.
    pub fn write_record_header(&mut self, field_count: u32);

    /// Write an IGNORED response.
    pub fn write_ignored(&mut self);

    // --- Streaming PackStream writers (zero-copy, direct to buffer) ---
    // These write directly into the current target buffer (connection write_buf
    // or QueryBuffer, depending on context).

    pub fn write_null(&mut self);
    pub fn write_bool(&mut self, value: bool);
    pub fn write_int(&mut self, value: i64);
    pub fn write_float(&mut self, value: f64);
    pub fn write_string(&mut self, value: &str);    // Borrows, encodes in-place
    pub fn write_bytes(&mut self, value: &[u8]);
    pub fn write_list_header(&mut self, size: u32);
    pub fn write_map_header(&mut self, size: u32);
    pub fn write_struct_header(&mut self, tag: u8, size: u32); // For temporal/spatial types

    // --- High-level entity writers (zero-copy) ---
    // Write struct header + fixed fields. Caller then writes labels/properties.
    pub fn write_node_header(&mut self, id: i64, label_count: u32, prop_count: u32);
    pub fn write_relationship_header(&mut self, id: i64, start_id: i64, end_id: i64,
        rel_type: &str, prop_count: u32);
    pub fn write_unbound_relationship_header(&mut self, id: i64, rel_type: &str, prop_count: u32);
    pub fn write_path_header(&mut self, node_count: u32, rel_count: u32, index_count: u32);
    pub fn write_point2d(&mut self, srid: i64, x: f64, y: f64);


    /// End current message (write zero-chunk terminator).
    pub fn end_message(&mut self);

    /// Get raw bytes ready to send to the socket.
    pub fn take_write_bytes(&mut self) -> BytesMut;

    /// Feed raw bytes received from socket. De-chunks into complete messages.
    pub fn feed_data(&mut self, data: &[u8]) -> Result<(), BoltError>;

    /// Check if there are complete messages ready for processing.
    pub fn has_pending_messages(&self) -> bool;

    /// Process the next complete message through the handler.
    /// The message is parsed with borrowed references into the internal buffer.
    /// The buffer is valid until the next feed_data() call.

    /// Process a single request through the handler, updating state.
    /// If state is INTERRUPTED, all messages except RESET/GOODBYE get IGNORED.
    /// If state is FAILED, all messages except RESET/ROLLBACK/GOODBYE get IGNORED.
    pub fn process_request(
        &mut self,
        request: BoltRequest,
        handler: &dyn BoltHandler,
    ) -> Result<(), BoltError>;

    /// Handle <INTERRUPT> signal (called when RESET arrives, before queued processing).
    /// Sets interrupt_flag, transitions state to INTERRUPTED.
    pub fn signal_interrupt(&mut self);

    /// Get the interrupt flag (shared with execution thread).
    pub fn interrupt_flag(&self) -> Arc<InterruptFlag>;

    /// Get/set opaque user data pointer.
    pub fn user_data(&self) -> *mut c_void;
    pub fn set_user_data(&mut self, data: *mut c_void);
}
```

#### `server/event_loop.rs` - fd-based Integration for Redis

```rust
/// Callback signatures matching Redis Module event loop API.
/// The host app registers these with its event loop (e.g., RedisModule_EventLoopAdd).
pub type EventLoopReadCallback = extern "C" fn(fd: i32, user_data: *mut c_void, mask: i32);
pub type EventLoopWriteCallback = extern "C" fn(fd: i32, user_data: *mut c_void, mask: i32);

/// Create a TCP listener on the given port.
/// Returns the listening fd for the host app to register with its event loop.
pub fn bolt_listen(port: u16) -> Result<i32, BoltError>;

/// Accept a new connection on the listening fd.
/// Returns a new BoltConnection (opaque handle for FFI).
pub fn bolt_accept(listen_fd: i32) -> Result<Box<BoltConnection>, BoltError>;
```

---

### Layer 3.5: Record Serialization - Entity Resolution Strategy

**Principle**: The Bolt crate has NO knowledge of graphs, schemas, or IDs. The host app resolves IDs and writes fully-resolved entities directly to the buffer using the streaming writer API.

#### Bolt expects fully-materialized compound objects:

> **Note**: `element_id` fields on the wire are String types derived from the integer `id`. The crate generates these automatically (e.g., `id=42` → `element_id="node_42"`). The host app only provides integer IDs.

```
Node(0x4E, 4 fields):
  id: Integer                        (e.g., 42)
  labels: List<String>               (e.g., ["Person", "Employee"])
  properties: Map<String, Value>     (e.g., {"name": "Alice", "age": 30})
  element_id: String                 (e.g., "node_42" — derived from id by crate)

Relationship(0x52, 8 fields):
  id: Integer                        (e.g., 7)
  start_node_id: Integer             (e.g., 42)
  end_node_id: Integer               (e.g., 99)
  type: String                       (e.g., "KNOWS")
  properties: Map<String, Value>     (e.g., {"since": 2020})
  element_id: String                 (e.g., "relationship_7" — derived from id by crate)
  start_node_element_id: String      (e.g., "node_42" — derived from start_node_id by crate)
  end_node_element_id: String        (e.g., "node_99" — derived from end_node_id by crate)

UnboundRelationship(0x72, 4 fields):  (used inside Path)
  id: Integer
  type: String
  properties: Map<String, Value>
  element_id: String                 (derived from id by crate)

Path(0x50, 3 fields):
  nodes: List<Node>                  (all unique nodes)
  rels: List<UnboundRelationship>    (all unique relationships)
  indices: List<Integer>             (traversal order: +i = forward through rel i, -i = backward)
```

#### FalkorDB stores bare IDs, not materialized entities:

| FalkorDB C | FalkorDB Rust | What's missing for Bolt |
|---|---|---|
| `Node { id, *attributes }` | `Value::Node(NodeId)` | Labels require `NODE_GET_LABELS()` + Schema name lookup |
| `Edge { id, src_id, dest_id, relationID, *attributes }` | `Value::Relationship(Box<(RelId, NodeId, NodeId)>)` | Type name requires Schema lookup; properties require graph query |
| `Path { *nodes, *edges }` | `Value::Path(ThinVec<Value>)` | Each node/edge inside needs full resolution |

#### Resolution responsibility: HOST resolves, crate writes directly

The host app (FalkorDB C or Rust) resolves IDs and writes values directly to the buffer using the streaming writer. No intermediate value types are created.

**In FalkorDB Rust (next-gen)** - `write_runtime_value()` in `src/bolt.rs`:
```rust
/// Write a runtime Value directly to the connection buffer.
/// Data flows directly: graph → writer buffer. No intermediate types.
fn write_runtime_value(conn: &mut BoltConnection, runtime: &Runtime, value: Value) {
    match value {
        Value::Node(id) => {
            let g = runtime.g.borrow();
            let labels: Vec<_> = g.get_node_labels(id).collect();
            let attrs = g.get_node_all_attrs(id);
            conn.write_node_header(u64::from(id) as i64, labels.len() as u32, attrs.len() as u32);
            for label in &labels { conn.write_string(label); }
            for (k, v) in &attrs {
                conn.write_string(k);
                write_runtime_value(conn, runtime, v.clone()); // recursive
            }
        }
        Value::Relationship(rel) => {
            let (rel_id, src, dst) = *rel;
            let g = runtime.g.borrow();
            let tid = g.get_relationship_type_id(rel_id);
            let type_name = g.get_type_name(tid);
            let attrs = g.get_relationship_all_attrs(rel_id);
            conn.write_relationship_header(
                u64::from(rel_id) as i64,
                u64::from(src) as i64,
                u64::from(dst) as i64,
                type_name, attrs.len() as u32,
            );
            for (k, v) in &attrs {
                conn.write_string(k);
                write_runtime_value(conn, runtime, v.clone());
            }
        }
        Value::Path(values) => {
            // Path requires pre-counting nodes and edges
            let node_count = (values.len() + 1) / 2;
            let edge_count = values.len() / 2;
            conn.write_path_header(node_count as u32, edge_count as u32, (edge_count * 2) as u32);
            // Write nodes
            for v in values.iter().step_by(2) {
                write_runtime_value(conn, runtime, v.clone());
            }
            // Write unbound relationships
            for v in values.iter().skip(1).step_by(2) {
                if let Value::Relationship(rel) = v {
                    let (rel_id, _, _) = **rel;
                    let g = runtime.g.borrow();
                    let tid = g.get_relationship_type_id(rel_id);
                    let type_name = g.get_type_name(tid);
                    let attrs = g.get_relationship_all_attrs(rel_id);
                    conn.write_unbound_relationship_header(
                        u64::from(rel_id) as i64, type_name, attrs.len() as u32,
                    );
                    for (k, v) in &attrs {
                        conn.write_string(k);
                        write_runtime_value(conn, runtime, v.clone());
                    }
                }
            }
            // Write traversal indices
            // (direction logic based on src_id matching previous node)
        }
        // Scalars — write directly, no allocation
        Value::Null => conn.write_null(),
        Value::Bool(b) => conn.write_bool(b),
        Value::Int(i) => conn.write_int(i),
        Value::Float(f) => conn.write_float(f),
        Value::String(s) => conn.write_string(&s),
        Value::List(l) => {
            conn.write_list_header(l.len() as u32);
            for v in l.iter() { write_runtime_value(conn, runtime, v.clone()); }
        }
        Value::Map(m) => {
            conn.write_map_header(m.len() as u32);
            for (k, v) in m.iter() {
                conn.write_string(k);
                write_runtime_value(conn, runtime, v.clone());
            }
        }
        Value::Point(p) => conn.write_point2d(4326, p.longitude as f64, p.latitude as f64),
        Value::VecF32(v) => {
            conn.write_list_header(v.len() as u32);
            for f in v.iter() { conn.write_float(*f as f64); }
        }
        // Temporal types — write struct header + fields directly
        Value::Datetime(ts) => {
            conn.write_struct_header(struct_tag::DATE_TIME, 3);
            conn.write_int(ts); conn.write_int(0); conn.write_int(0);
        }
        Value::Date(d) => {
            conn.write_struct_header(struct_tag::DATE, 1);
            conn.write_int(d);
        }
        Value::Time(t) => {
            conn.write_struct_header(struct_tag::TIME, 2);
            conn.write_int(t); conn.write_int(0);
        }
        Value::Duration(d) => {
            conn.write_struct_header(struct_tag::DURATION, 4);
            conn.write_int(0); conn.write_int(0); conn.write_int(d); conn.write_int(0);
        }
        _ => conn.write_null(),
    }
}
```

**In FalkorDB C** - the existing `resultset_replybolt.c` already does this resolution. The pattern stays the same but calls the Rust FFI:
```c
// Current C code in _ResultSet_BoltReplyWithNode already resolves:
// - Labels via NODE_GET_LABELS() + Schema_GetName()
// - Properties via GraphEntity_GetAttributes() + AttributeSet iteration
// This logic stays in C, calling bolt_reply_node() / bolt_reply_string() etc.
// from the Rust crate. No intermediate value types on either side.
```

---

### Layer 4: C FFI API

**Zero-copy in the C API:**
- **Writing**: `bolt_reply_*` functions encode directly into the connection's write buffer. No intermediate types.
- **Reading**: Callback arguments (`query`, `scheme`, `principal`, `credentials`) are pointers INTO the message buffer — zero-copy. `params_buf`/`extra_buf` are raw PackStream bytes that the C code parses on demand with `bolt_read_*` functions, which also return pointers into the buffer.
- **Buffered records**: `bolt_buffer_record_begin/end` writes directly to the QueryBuffer. PULL copies pre-serialized bytes to the wire — no re-serialization.

#### `ffi/c_api.rs`

```rust
// --- Opaque handle types ---
pub type BoltClient = *mut BoltConnection;

// --- Callback function pointer types for C ---
pub type BoltAuthCallback = extern "C" fn(
    conn: BoltClient,
    scheme: *const c_char, scheme_len: u32,
    principal: *const c_char, principal_len: u32,
    credentials: *const c_char, credentials_len: u32,
    user_data: *mut c_void,
) -> bool;

pub type BoltRunCallback = extern "C" fn(
    conn: BoltClient,
    query: *const c_char, query_len: u32,
    params_buf: *const u8, params_len: u32,  // PackStream-encoded parameters
    extra_buf: *const u8, extra_len: u32,    // PackStream-encoded extras
    user_data: *mut c_void,
);

pub type BoltBeginCallback = extern "C" fn(
    conn: BoltClient,
    extra_buf: *const u8, extra_len: u32,
    user_data: *mut c_void,
);

pub type BoltCommitCallback = extern "C" fn(conn: BoltClient, user_data: *mut c_void);
pub type BoltRollbackCallback = extern "C" fn(conn: BoltClient, user_data: *mut c_void);

// --- Server lifecycle ---
#[no_mangle] pub extern "C" fn bolt_server_listen(port: u16) -> i32;
#[no_mangle] pub extern "C" fn bolt_server_accept(listen_fd: i32) -> BoltClient;

// --- Connection lifecycle ---
#[no_mangle] pub extern "C" fn bolt_connection_feed_data(
    conn: BoltClient, data: *const u8, len: u32
) -> i32;  // returns number of requests parsed
#[no_mangle] pub extern "C" fn bolt_connection_process(
    conn: BoltClient, user_data: *mut c_void
) -> i32;
#[no_mangle] pub extern "C" fn bolt_connection_get_write_data(
    conn: BoltClient, out_ptr: *mut *const u8, out_len: *mut u32
);
#[no_mangle] pub extern "C" fn bolt_connection_free(conn: BoltClient);

// --- Set callbacks ---
#[no_mangle] pub extern "C" fn bolt_set_auth_callback(cb: BoltAuthCallback);
#[no_mangle] pub extern "C" fn bolt_set_run_callback(cb: BoltRunCallback);
#[no_mangle] pub extern "C" fn bolt_set_begin_callback(cb: BoltBeginCallback);
#[no_mangle] pub extern "C" fn bolt_set_commit_callback(cb: BoltCommitCallback);
#[no_mangle] pub extern "C" fn bolt_set_rollback_callback(cb: BoltRollbackCallback);

// --- Reply helpers (write PackStream values to the client) ---
// Context-aware: if called inside bolt_buffer_record_begin/end, writes go to
// the client's active QueryBuffer. Otherwise writes go to the connection's
// write buffer for immediate sending.
#[no_mangle] pub extern "C" fn bolt_reply_null(client: BoltClient);
#[no_mangle] pub extern "C" fn bolt_reply_bool(client: BoltClient, value: bool);
#[no_mangle] pub extern "C" fn bolt_reply_int(client: BoltClient, value: i64);
#[no_mangle] pub extern "C" fn bolt_reply_float(client: BoltClient, value: f64);
#[no_mangle] pub extern "C" fn bolt_reply_string(client: BoltClient, data: *const c_char, len: u32);
#[no_mangle] pub extern "C" fn bolt_reply_list(client: BoltClient, size: u32);
#[no_mangle] pub extern "C" fn bolt_reply_map(client: BoltClient, size: u32);

// --- Graph entity reply helpers ---
// High-level functions that encapsulate the Bolt protocol structure format.
// The C host code calls these instead of raw bolt_reply_structure(tag, fields).
// Internally they write the correct PackStream struct header + fields.
// The crate generates element_id strings internally from integer IDs
// (e.g., id=42 → "node_42", id=7 → "relationship_7").
//
// Usage pattern:
//   bolt_reply_node(client, id, label_count, prop_count)
//     → write labels via bolt_reply_string() x label_count
//     → write properties via bolt_reply_string(key) + bolt_reply_*(value) x prop_count
//   (node is auto-completed after label_count + prop_count values are written)

// Node: begins a Node structure. Caller then writes labels and properties.
// element_id is generated internally as "node_{id}".
#[no_mangle] pub extern "C" fn bolt_reply_node(
    client: BoltClient, id: i64,
    label_count: u32, prop_count: u32,
);

// Relationship: begins a Relationship structure. Caller then writes properties.
// element_id, start_node_element_id, end_node_element_id are generated internally
// as "relationship_{id}", "node_{start_node_id}", "node_{end_node_id}".
#[no_mangle] pub extern "C" fn bolt_reply_relationship(
    client: BoltClient, id: i64,
    start_node_id: i64, end_node_id: i64,
    rel_type: *const c_char, rel_type_len: u32,
    prop_count: u32,
);

// UnboundRelationship (used inside Paths): begins structure. Caller writes properties.
// element_id is generated internally as "relationship_{id}".
#[no_mangle] pub extern "C" fn bolt_reply_unbound_relationship(
    client: BoltClient, id: i64,
    rel_type: *const c_char, rel_type_len: u32,
    prop_count: u32,
);

// Path: begins a Path structure. Caller then writes nodes, rels, and indices.
#[no_mangle] pub extern "C" fn bolt_reply_path(
    client: BoltClient,
    node_count: u32, rel_count: u32, index_count: u32,
);

// Point2D (WGS84)
#[no_mangle] pub extern "C" fn bolt_reply_point2d(
    client: BoltClient, srid: i64, x: f64, y: f64,
);

// --- Message-level helpers ---
#[no_mangle] pub extern "C" fn bolt_reply_success(client: BoltClient, metadata_count: u32);
#[no_mangle] pub extern "C" fn bolt_reply_failure(client: BoltClient, code: *const c_char, msg: *const c_char);
#[no_mangle] pub extern "C" fn bolt_end_message(client: BoltClient);
#[no_mangle] pub extern "C" fn bolt_flush_immediate(client: BoltClient);

// --- Buffer API (for ResultSetFormatter to buffer RECORDs during execution) ---
// The C API uses BoltClient throughout. The crate internally manages QueryBuffers
// (identified by buffer_id / qid) and maps them to clients. The buffer_id is
// opaque to C — it only appears as the "qid" field in the RUN SUCCESS response,
// which the crate generates. When PULL arrives (possibly on a different connection),
// the crate uses the qid from the PULL message to find the right buffer.
#[no_mangle] pub extern "C" fn bolt_buffer_record_begin(client: BoltClient, field_count: u32);
#[no_mangle] pub extern "C" fn bolt_buffer_record_end(client: BoltClient);
#[no_mangle] pub extern "C" fn bolt_buffer_stats_begin(client: BoltClient);
#[no_mangle] pub extern "C" fn bolt_buffer_complete(client: BoltClient);  // starts 10s cleanup timer
#[no_mangle] pub extern "C" fn bolt_buffer_error(
    client: BoltClient, code: *const c_char, code_len: u32,
    message: *const c_char, message_len: u32,
);  // starts 10s cleanup timer
// PULL is handled internally by the crate (uses qid from PULL message to find buffer).

// --- Read helpers (zero-copy parsing of PackStream data) ---
// These operate on the raw PackStream bytes passed to callbacks (params_buf, extra_buf).
// The cursor (data) is advanced past each read. String reads return a pointer INTO
// the original buffer — zero-copy, no allocation. The pointer is valid as long as
// the callback is executing (the buffer is owned by the crate).
//
// Usage pattern in C:
//   const uint8_t *cursor = params_buf;
//   int type = bolt_read_type(cursor);
//   if (type == BVT_STRING) {
//       uint32_t len;
//       const char *str = bolt_read_string_value(&cursor, &len);
//       // str points into params_buf — zero-copy, valid during callback
//   }
#[no_mangle] pub extern "C" fn bolt_read_type(data: *const u8) -> i32;
#[no_mangle] pub extern "C" fn bolt_read_int_value(data: *mut *const u8) -> i64;
#[no_mangle] pub extern "C" fn bolt_read_float_value(data: *mut *const u8) -> f64;
#[no_mangle] pub extern "C" fn bolt_read_bool_value(data: *mut *const u8) -> bool;
// Returns pointer INTO the buffer (zero-copy). Valid during callback lifetime.
#[no_mangle] pub extern "C" fn bolt_read_string_value(data: *mut *const u8, out_len: *mut u32) -> *const c_char;
#[no_mangle] pub extern "C" fn bolt_read_list_size(data: *mut *const u8) -> u32;
#[no_mangle] pub extern "C" fn bolt_read_map_size(data: *mut *const u8) -> u32;
// Skip past a value without reading it (for fields the handler doesn't need).
#[no_mangle] pub extern "C" fn bolt_read_skip(data: *mut *const u8);
```

The C API is designed to be a **near drop-in replacement** for the current `bolt_*` functions in FalkorDB's `src/bolt/bolt.h`, with the addition of callback registration. A `cbindgen`-generated header file will be produced at build time.

---

### C API Usage Examples

**Design principle**: The graph engine (execution layer) emits rows via the `ResultSetFormatter` interface. The Bolt formatter (`resultset_replybolt.c`) calls `bolt_reply_*` FFI functions to serialize values. The crate internally manages a `QueryBuffer` (connection-agnostic, identified by `qid`) to hold pre-serialized records. The PULL handler drains from the buffer — potentially on a **different connection** than the one that sent RUN. The C API only uses `BoltClient`; buffer management is internal to the crate.

```
Execution Engine                   ResultSetFormatter (Bolt)           Rust Crate (internal)
─────────────────                  ─────────────────────────           ────────────────────
plan ready         ──EmitHeader──► bolt_flush_immediate(client)   ──► send RUN SUCCESS
  │                                  (fields, qid=buffer_id)          immediately
  ▼
for each row       ──EmitRow────► bolt_buffer_record_begin(client)──► writes to QueryBuffer
  │                                 bolt_reply_*(client, ...)           (pre-serialized)
  │                                 bolt_buffer_record_end(client)
  ▼
execution done     ──EmitStats──► bolt_buffer_complete(client)    ──► store stats, start 10s timer
                                                                       mark complete
                                    ─── later ───
                                  PULL {n, qid} arrives           ──► crate looks up QueryBuffer
                                  (possibly different connection!)      by qid, drains N records,
                                                                       sends to requesting client
```

#### Example 1: Full Connection Lifecycle (server setup + accept)

```c
#include "falkordb_bolt.h"

// --- Module initialization ---
int BoltInit(RedisModuleCtx *ctx, int port) {
    // 1. Register callbacks
    bolt_set_auth_callback(my_auth_handler);
    bolt_set_run_callback(my_run_handler);
    bolt_set_begin_callback(my_begin_handler);
    bolt_set_commit_callback(my_commit_handler);
    bolt_set_rollback_callback(my_rollback_handler);
    bolt_set_route_callback(my_route_handler);

    // 2. Start listening
    int listen_fd = bolt_server_listen(port);
    if (listen_fd < 0) return REDISMODULE_ERR;

    // 3. Register with Redis event loop
    RedisModule_EventLoopAdd(listen_fd, REDISMODULE_EVENTLOOP_READABLE,
                             my_accept_handler, ctx);
    return REDISMODULE_OK;
}

// --- Accept handler (called by Redis event loop) ---
void my_accept_handler(int fd, void *user_data, int mask) {
    RedisModuleCtx *ctx = (RedisModuleCtx *)user_data;
    BoltClient conn = bolt_server_accept(fd);
    if (!conn) return;

    // Attach Redis context for later use in callbacks
    bolt_connection_set_user_data(conn, ctx);

    // Register read handler for this connection
    int client_fd = bolt_connection_fd(conn);
    RedisModule_EventLoopAdd(client_fd, REDISMODULE_EVENTLOOP_READABLE,
                             my_read_handler, conn);
}

// --- Read handler (called when data arrives on client socket) ---
void my_read_handler(int fd, void *user_data, int mask) {
    BoltClient conn = (BoltClient)user_data;

    // Read from socket into connection buffer
    char buf[4096];
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n <= 0) {
        // Disconnected
        RedisModule_EventLoopDel(fd, REDISMODULE_EVENTLOOP_READABLE);
        bolt_connection_free(conn);
        return;
    }

    // Feed data and process messages (triggers callbacks)
    bolt_connection_feed_data(conn, (uint8_t *)buf, n);
    int requests = bolt_connection_process(conn);

    // If there's data to write, register write handler
    uint32_t write_len = 0;
    const uint8_t *write_data = NULL;
    bolt_connection_get_write_data(conn, &write_data, &write_len);
    if (write_len > 0) {
        RedisModule_EventLoopAdd(fd, REDISMODULE_EVENTLOOP_WRITABLE,
                                 my_write_handler, conn);
    }
}
```

#### Example 2: Authentication Callback

```c
bool my_auth_handler(
    BoltClient conn,
    const char *scheme, uint32_t scheme_len,
    const char *principal, uint32_t principal_len,
    const char *credentials, uint32_t credentials_len,
    void *user_data
) {
    RedisModuleCtx *ctx = (RedisModuleCtx *)user_data;

    if (principal_len == 0 && credentials_len == 0) {
        // No credentials - check if we can run without auth
        RedisModuleCallReply *reply = RedisModule_Call(ctx, "PING", "");
        bool ok = RedisModule_CallReplyType(reply) != REDISMODULE_REPLY_ERROR;
        RedisModule_FreeCallReply(reply);
        return ok;
    }

    // Try Redis ACL AUTH
    RedisModuleCallReply *reply = RedisModule_Call(ctx, "AUTH", "b",
                                                    credentials, credentials_len);
    bool ok = RedisModule_CallReplyType(reply) != REDISMODULE_REPLY_ERROR;
    RedisModule_FreeCallReply(reply);
    return ok;
}
```

#### Example 3: RUN Callback (bolt_bridge.c)

The RUN callback starts query execution. The crate internally creates a `QueryBuffer` when the RUN message is processed. The execution engine uses the Bolt `ResultSetFormatter` to emit results — the crate routes writes to the internal buffer. The PULL handler (inside the crate) drains from that buffer later — potentially from a different connection.

```c
// bolt_bridge.c - RUN callback implementation
void falkordb_run_handler(
    BoltClient client,
    const char *query, uint32_t query_len,
    const uint8_t *params_buf, uint32_t params_len,
    const uint8_t *extra_buf, uint32_t extra_len,
    void *user_data
) {
    // 1. Extract db name from extra (or default to "falkordb")
    char *db = parse_db_from_extra(extra_buf, extra_len);

    // 2. Open graph key directly
    GraphContext *gc = GraphContext_Retrieve(ctx, db);

    // 3. Parse + plan + execute the query.
    //    The execution engine calls ResultSetFormatter callbacks:
    //      EmitHeader → bolt_flush_immediate(client)           // sends RUN SUCCESS (with qid)
    //      EmitRow    → bolt_buffer_record_begin/end(client)   // crate routes to internal QueryBuffer
    //      EmitStats  → bolt_buffer_complete(client)           // stores stats, starts 10s timer
    //
    //    All graph access (node labels, properties, etc.) happens HERE during
    //    execution, NOT during PULL. The formatter resolves entities and
    //    serializes them into pre-built PackStream bytes in the buffer.
    //
    //    The C code never sees buffer_id — the crate manages it internally
    //    and includes it as "qid" in the RUN SUCCESS response.
    ExecuteQuery(gc, query, params, client);
}
```

#### Example 4: ResultSetFormatter - EmitHeader (sends RUN SUCCESS)

The RUN SUCCESS response contains 3 metadata fields:
- **`fields`** (List\<String\>): Column names of the result set. Always present — empty list `[]` for queries with no result columns (e.g., CREATE without RETURN).
- **`qid`** (Integer): Query identifier — internally this is the `buffer_id`. The **crate generates** this field automatically; the C code just provides column names.
- **`t_first`** (Integer): Time in milliseconds until first record is available. Auto-generated by the crate.

> **Note on protocol vs implementation**: The Bolt protocol spec ([message specification](https://neo4j.com/docs/bolt/current/bolt/message/)) lists `fields` and `t_first` as standard RUN SUCCESS metadata but does not explicitly mark them as mandatory. The spec provides no guidance on what happens for queries without result columns (e.g., CREATE). However, Neo4j's implementation always includes `fields` (as `[]` when there are no columns), and all Neo4j drivers expect it. We follow Neo4j's behavior to ensure driver compatibility.

#### RUN outcome flows

The host C code must always end the RUN callback in one of these ways:

| Scenario | What C code does | Bolt response |
|---|---|---|
| Query with columns (`MATCH ... RETURN a, b`) | `EmitHeader` with column names | `SUCCESS {fields: ["a", "b"], qid, t_first}` |
| Query without columns (`CREATE ...`) | `EmitHeader` with 0 columns | `SUCCESS {fields: [], qid, t_first}` |
| Parse/plan error | `bolt_reply_failure(client, code, msg)` | `FAILURE {code, message}` → state: FAILED |

**`EmitHeader` is always called on success** — even for mutations with no RETURN clause, it sends an empty `fields` list. There is no case where RUN SUCCESS is sent without `fields`. If the host cannot reach `EmitHeader` (e.g., parse error, plan error), it must call `bolt_reply_failure` instead.

> **Note on `ResultSet_ReplyWithBoltHeader`**: This function already exists in the current FalkorDB C codebase (`src/resultset/formatters/resultset_replybolt.c:279`). It is part of the existing `ResultSetFormatter` interface — not something new introduced by this design. The only change is that it will call the Rust FFI `bolt_reply_*` functions instead of the old C `bolt_reply_*` functions from `src/bolt/bolt.h`. The function signature and role remain the same.

```c
// resultset_replybolt.c - EXISTING function, modified to call Rust FFI
void ResultSet_ReplyWithBoltHeader(ResultSet *set) {
    BoltClient client = set->bolt_client;

    // Send RUN SUCCESS immediately with column names.
    // The crate automatically includes qid (= internal buffer_id) and t_first
    // in the SUCCESS metadata. The C code just provides the column names.
    // For CREATE/DELETE without RETURN, column_count is 0 → fields: []
    bolt_reply_success(client, 1);
      bolt_reply_string(client, "fields", 6);
      bolt_reply_list(client, set->column_count);
      for (uint i = 0; i < set->column_count; i++) {
          bolt_reply_string(client, set->columns[i], strlen(set->columns[i]));
      }
    bolt_end_message(client);
    bolt_flush_immediate(client);  // send to client NOW (not buffered)
}
```

#### Example 5: ResultSetFormatter - EmitRow (buffers RECORD)

This is the key function. Each result row is serialized into a RECORD message and appended to the internal QueryBuffer. The graph engine calls this during execution — all entity resolution happens here. The C code only uses `BoltClient`; the crate routes `bolt_reply_*` calls to the buffer internally when inside a `record_begin/end` block.

```c
// resultset_replybolt.c - called for EACH result row during execution
void ResultSet_EmitBoltRow(ResultSet *set, SIValue **row) {
    BoltClient client = set->bolt_client;
    GraphContext *gc = QueryCtx_GetGraphCtx();

    // Begin a new RECORD in the buffer
    // (crate routes subsequent bolt_reply_* calls to the internal QueryBuffer)
    bolt_buffer_record_begin(client, set->column_count);

    // Serialize each column value into the RECORD
    for (uint i = 0; i < set->column_count; i++) {
        bolt_reply_si_value(client, gc, *row[i]);
    }

    // Finalize the RECORD in the buffer
    bolt_buffer_record_end(client);
}
```

#### Example 6: bolt_reply_si_value - Recursive Value Serialization

This is a **host app function** (lives in `resultset_replybolt.c`, NOT in the Rust crate). It resolves FalkorDB's internal types into Bolt PackStream format. For compound types (Node, Edge, Path), it queries the graph to resolve labels, properties, and type names — this graph access happens entirely in the C host code. The Rust crate never accesses the graph; it only receives the already-resolved PackStream bytes via `bolt_reply_*` calls, which it routes to the QueryBuffer since we're inside a `record_begin/end` block.

```c
// resultset_replybolt.c — HOST APP code, NOT in the Rust crate.
// Graph access (NODE_GET_LABELS, GraphEntity_GetAttributes, etc.) happens here.
// The crate only sees the resulting bolt_reply_* calls.
//
// Note: No protocol details (struct tags, field counts) are exposed here.
// High-level functions like bolt_reply_node(), bolt_reply_relationship(), etc.
// encapsulate the Bolt wire format internally.
void bolt_reply_si_value(BoltClient client, GraphContext *gc, SIValue v) {
    switch (SI_TYPE(v)) {
    case T_NULL:
        bolt_reply_null(client);
        break;
    case T_BOOL:
        bolt_reply_bool(client, v.longval);
        break;
    case T_INT64:
        bolt_reply_int(client, v.longval);
        break;
    case T_DOUBLE:
        bolt_reply_float(client, v.doubleval);
        break;
    case T_STRING:
    case T_INTERN_STRING:
        bolt_reply_string(client, v.stringval, strlen(v.stringval));
        break;

    case T_NODE: {
        Node *node = v.ptrval;
        uint lbls_count;
        NODE_GET_LABELS(gc->g, node, lbls_count);
        const AttributeSet set = GraphEntity_GetAttributes((GraphEntity *)node);
        int prop_count = AttributeSet_Count(set);

        // bolt_reply_node writes struct header + id + element_id internally.
        // Caller then writes label_count labels + prop_count key-value pairs.
        bolt_reply_node(client, node->id, lbls_count, prop_count);
          // Write labels
          for (int i = 0; i < lbls_count; i++) {
              Schema *s = GraphContext_GetSchemaByID(gc, labels[i], SCHEMA_NODE);
              bolt_reply_string(client, Schema_GetName(s), strlen(Schema_GetName(s)));
          }
          // Write properties (key-value pairs)
          for (int i = 0; i < prop_count; i++) {
              SIValue val; AttributeID attr_id;
              AttributeSet_GetIdx(set, i, &attr_id, &val);
              const char *key = GraphContext_GetAttributeString(gc, attr_id);
              bolt_reply_string(client, key, strlen(key));
              bolt_reply_si_value(client, gc, val);  // recursive
          }
        break;
    }

    case T_EDGE: {
        Edge *edge = v.ptrval;
        Schema *s = GraphContext_GetSchemaByID(gc, Edge_GetRelationID(edge), SCHEMA_EDGE);
        const char *type = Schema_GetName(s);
        const AttributeSet set = GraphEntity_GetAttributes((GraphEntity *)edge);
        int prop_count = AttributeSet_Count(set);

        // bolt_reply_relationship writes struct header + fixed fields + element_ids internally.
        // Caller then writes prop_count key-value pairs.
        bolt_reply_relationship(client, edge->id,
            edge->src_id, edge->dest_id,
            type, strlen(type), prop_count);
          for (int i = 0; i < prop_count; i++) {
              SIValue val; AttributeID attr_id;
              AttributeSet_GetIdx(set, i, &attr_id, &val);
              const char *key = GraphContext_GetAttributeString(gc, attr_id);
              bolt_reply_string(client, key, strlen(key));
              bolt_reply_si_value(client, gc, val);
          }
        break;
    }

    case T_PATH: {
        size_t node_count = SIPath_NodeCount(v);
        size_t edge_count = SIPath_EdgeCount(v);

        // bolt_reply_path writes the Path struct header internally.
        // Caller then writes: node_count nodes, rel_count unbound rels, index_count indices.
        bolt_reply_path(client, node_count, edge_count, edge_count * 2);

          // Write nodes
          for (int i = 0; i < node_count; i++) {
              SIValue n = SIPath_GetNode(v, i);
              bolt_reply_si_value(client, gc, n);  // recurses into T_NODE
          }

          // Write unbound relationships
          for (int i = 0; i < edge_count; i++) {
              Edge *e = SIPath_GetRelationship(v, i).ptrval;
              Schema *s = GraphContext_GetSchemaByID(gc, Edge_GetRelationID(e), SCHEMA_EDGE);
              const AttributeSet eset = GraphEntity_GetAttributes((GraphEntity *)e);
              int pc = AttributeSet_Count(eset);
              bolt_reply_unbound_relationship(client, e->id,
                  Schema_GetName(s), strlen(Schema_GetName(s)),
                  pc);
                for (int j = 0; j < pc; j++) {
                    SIValue val; AttributeID attr_id;
                    AttributeSet_GetIdx(eset, j, &attr_id, &val);
                    const char *key = GraphContext_GetAttributeString(gc, attr_id);
                    bolt_reply_string(client, key, strlen(key));
                    bolt_reply_si_value(client, gc, val);
                }
          }

          // Write traversal indices
          for (int i = 0; i < edge_count; i++) {
              Edge *e = SIPath_GetRelationship(v, i).ptrval;
              Node *prev = SIPath_GetNode(v, i).ptrval;
              if (e->src_id == prev->id)
                  bolt_reply_int(client, i + 1);
              else
                  bolt_reply_int(client, -(i + 1));
              bolt_reply_int(client, i + 1);
          }
        break;
    }

    case T_ARRAY:
        bolt_reply_list(client, SIArray_Length(v));
        for (int i = 0; i < SIArray_Length(v); i++)
            bolt_reply_si_value(client, gc, SIArray_Get(v, i));
        break;
    case T_MAP:
        bolt_reply_map(client, Map_KeyCount(v));
        for (uint i = 0; i < Map_KeyCount(v); i++) {
            bolt_reply_si_value(client, gc, v.map[i].key);
            bolt_reply_si_value(client, gc, v.map[i].val);
        }
        break;
    case T_POINT:
        bolt_reply_point2d(client, 4326, v.point.longitude, v.point.latitude);
        break;
    case T_VECTOR_F32: {
        uint32_t dim = SIVector_Dim(v);
        bolt_reply_list(client, dim);
        float *vals = (float *)SIVector_Elements(v);
        for (uint i = 0; i < dim; i++)
            bolt_reply_float(client, (double)vals[i]);
        break;
    }
    }
}
```

#### Example 7: ResultSetFormatter - EmitStats (marks execution complete)

```c
// resultset_replybolt.c - called when execution finishes
void ResultSet_EmitBoltStats(ResultSet *set) {
    BoltClient client = set->bolt_client;

    // Store stats in the buffer (will be sent as final SUCCESS during PULL)
    bolt_buffer_stats_begin(client);
    if (set->stats.nodes_created)        { bolt_reply_string(client, "nodes-created", 13);        bolt_reply_int(client, set->stats.nodes_created); }
    if (set->stats.nodes_deleted)        { bolt_reply_string(client, "nodes-deleted", 13);        bolt_reply_int(client, set->stats.nodes_deleted); }
    if (set->stats.relationships_created){ bolt_reply_string(client, "relationships-created", 21); bolt_reply_int(client, set->stats.relationships_created); }
    if (set->stats.relationships_deleted){ bolt_reply_string(client, "relationships-deleted", 21); bolt_reply_int(client, set->stats.relationships_deleted); }
    if (set->stats.properties_set)       { bolt_reply_string(client, "properties-set", 14);       bolt_reply_int(client, set->stats.properties_set); }
    if (set->stats.labels_added)         { bolt_reply_string(client, "labels-added", 12);         bolt_reply_int(client, set->stats.labels_added); }
    if (set->stats.labels_removed)       { bolt_reply_string(client, "labels-removed", 14);       bolt_reply_int(client, set->stats.labels_removed); }
    if (set->stats.indices_created)      { bolt_reply_string(client, "indexes-added", 13);        bolt_reply_int(client, set->stats.indices_created); }
    if (set->stats.indices_deleted)      { bolt_reply_string(client, "indexes-removed", 15);      bolt_reply_int(client, set->stats.indices_deleted); }
    bolt_buffer_complete(client);  // marks execution_complete = true, starts 10s timer
}
```

#### Example 8: PULL - Handled Entirely by the Crate (connection-agnostic)

PULL does NOT need a host callback. The crate handles it internally:
1. Extracts `qid` from the PULL message (this is the internal `buffer_id`)
2. Looks up the `QueryBuffer` in the registry by `qid`
3. Drains records into **whichever connection** sent the PULL (may differ from RUN)

```rust
// Inside the crate (server/connection.rs) - NO graph access, NO C callback
fn handle_pull(conn: &mut BoltConnection, registry: &QueryBufferRegistry,
               n: i64, qid: i64) -> Result<(), BoltError> {
    // Look up the buffer by qid — NOT tied to any specific connection
    let buffer = registry.get_by_id(qid)
        .ok_or(BoltError::new("Neo.ClientError.Request.Invalid", "Unknown qid"))?;

    // Drain N pre-serialized RECORDs from the buffer
    let drained = buffer.drain(n);
    for record_bytes in &drained {
        conn.write_buf.extend_from_slice(record_bytes);
    }

    if buffer.has_more() {
        conn.write_success_header(1);
        conn.write_string("has_more");
        conn.write_bool(true);
    } else if let Some(err) = buffer.take_error() {
        conn.write_failure(&err.code, &err.message);
        registry.remove(qid);  // cancel timer, free buffer
    } else {
        // Write stats as SUCCESS metadata (pre-serialized by buffer)
        let stats_bytes = buffer.take_stats_bytes().unwrap_or_default();
        conn.write_buf.extend_from_slice(&stats_bytes);
        registry.remove(qid);  // cancel timer, free buffer
    }
    conn.end_message();
    Ok(())
}
```

The C API does NOT have a `bolt_set_pull_callback`. PULL is fully internal. The `qid` in the PULL message is the crate's internal `buffer_id`, which it generated during RUN.

#### Example 9: SHOW DATABASES Compatibility

```c
// Neo4j drivers call "SHOW DATABASES" on connect - must return hardcoded response.
// This is handled in the RUN callback as a special case BEFORE reaching the graph engine.
void falkordb_run_handler(BoltClient client, const char *query, ...) {
    if (strncmp(query, "SHOW DATABASES", 14) == 0) {
        // RUN SUCCESS with 13 column names
        // (crate auto-includes qid and t_first)
        bolt_reply_success(client, 1);
          bolt_reply_string(client, "fields", 6);
          bolt_reply_list(client, 13);
            bolt_reply_string(client, "name", 4);
            bolt_reply_string(client, "type", 4);
            bolt_reply_string(client, "aliases", 7);
            bolt_reply_string(client, "access", 6);
            bolt_reply_string(client, "address", 7);
            bolt_reply_string(client, "role", 4);
            bolt_reply_string(client, "writer", 6);
            bolt_reply_string(client, "requestedStatus", 15);
            bolt_reply_string(client, "currentStatus", 13);
            bolt_reply_string(client, "statusMessage", 13);
            bolt_reply_string(client, "default", 7);
            bolt_reply_string(client, "home", 4);
            bolt_reply_string(client, "constituents", 12);
        bolt_end_message(client);
        bolt_flush_immediate(client);

        // Buffer one RECORD row
        bolt_buffer_record_begin(client, 13);
          bolt_reply_string(client, "falkordb", 8);
          bolt_reply_string(client, "standard", 8);
          bolt_reply_list(client, 0);
          bolt_reply_string(client, "read-write", 10);
          bolt_reply_string(client, "localhost:7687", 14);
          bolt_reply_string(client, "primary", 7);
          bolt_reply_bool(client, true);
          bolt_reply_string(client, "online", 6);
          bolt_reply_string(client, "online", 6);
          bolt_reply_string(client, "", 0);
          bolt_reply_bool(client, true);
          bolt_reply_bool(client, true);
          bolt_reply_list(client, 0);
        bolt_buffer_record_end(client);
        bolt_buffer_complete(client);  // marks done, starts 10s timer, PULL will drain
        return;
    }
    // ... normal query handling via ExecuteQuery() ...
}
```

---

---

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        Neo4j Driver (Python/Java/JS/Go)                     │
└───────────────────────────────────┬─────────────────────────────────────────┘
                                    │ TCP / WebSocket
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         falkordb-bolt-rs (this crate)                       │
│                                                                             │
│  ┌──────────────┐  ┌──────────────────┐  ┌───────────────────────────────┐ │
│  │  Transport    │  │  Protocol        │  │  PackStream                   │ │
│  │  ─────────    │  │  ────────        │  │  ──────────                   │ │
│  │  TCP listener │  │  Handshake       │  │  Serialize: write_null,       │ │
│  │  WS upgrade   │  │  State machine   │  │    write_int, write_string,   │ │
│  │  TLS (opt)    │  │  Message parse   │  │    write_node, write_record   │ │
│  │  Chunking     │  │  Version-aware   │  │  Deserialize: read_value,     │ │
│  └──────────────┘  └──────────────────┘  │    read_map, read_struct      │ │
│                                           └───────────────────────────────┘ │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │  BoltConnection (per client)       QueryBufferRegistry (global)      │   │
│  │  ──────────────────────────        ────────────────────────────      │   │
│  │  state: BoltState                  buffers: HashMap<i64, QueryBuffer>│   │
│  │  version: BoltVersion              next_id: AtomicI64               │   │
│  │  write_buf: BytesMut                                                │   │
│  │  is_websocket: bool          QueryBuffer (per query, conn-agnostic) │   │
│  │  user_data: *void              records: VecDeque<BytesMut>          │   │
│  │                                execution_complete: bool              │   │
│  │                                expiry_timer: Option<TimerHandle>     │   │
│  └──────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  ┌──────────────────────────┐  ┌────────────────────────────────────────┐  │
│  │  BoltHandler trait       │  │  C FFI (extern "C")                    │  │
│  │  ────────────────        │  │  ──────────────────                    │  │
│  │  hello()                 │  │  bolt_server_listen()                  │  │
│  │  authenticate()          │  │  bolt_server_accept()                  │  │
│  │  run()                   │  │  bolt_connection_feed_data()           │  │
│  │  begin/commit/rollback() │  │  bolt_reply_* (conn writes)           │  │
│  │  route()                 │  │  bolt_reply_*_buf (buffer writes)     │  │
│  │                          │  │  bolt_buffer_record_begin/end()        │  │
│  └──────────────────────────┘  └────────────────────────────────────────┘  │
└───────────┬─────────────────────────────────┬───────────────────────────────┘
            │ Rust API (BoltHandler trait)     │ C FFI (function pointers)
            ▼                                  ▼
┌───────────────────────────┐    ┌────────────────────────────────────────────┐
│ falkordb-rs-next-gen      │    │ FalkorDB (C)                               │
│ ─────────────────────     │    │ ──────────                                 │
│                           │    │                                            │
│ src/bolt.rs:              │    │ src/bolt_bridge.c:                         │
│   FalkorBoltHandler impl  │    │   Callback implementations                │
│   write_runtime_value()   │    │   bolt_reply_si_value() helper            │
│                           │    │                                            │
│ graph/ crate:             │    │ src/resultset/formatters/:                 │
│   Runtime, Value enum     │    │   resultset_replybolt.c (uses FFI)        │
│   get_node_labels()       │    │                                            │
│   get_node_all_attrs()    │    │ Execution engine:                          │
│                           │    │   EmitHeader → bolt_flush_immediate()      │
│ Redis event loop via      │    │   EmitRow → bolt_buffer_record_begin/end() │
│   RedisModule_EventLoop   │    │   EmitStats → bolt_buffer_complete()       │
└───────────────────────────┘    └────────────────────────────────────────────┘
```

## Connection Lifecycle Flow

```
Client                          falkordb-bolt-rs                    FalkorDB
  │                                   │                                │
  │─── TCP connect ──────────────────►│                                │
  │─── 0x6060B017 + 4 versions ─────►│                                │
  │◄── chosen version (5.8) ─────────│                                │
  │                                   │ state: NEGOTIATION             │
  │─── HELLO {user_agent, ...} ─────►│──── handler.hello() ──────────►│
  │◄── SUCCESS {server, conn_id} ────│◄── Ok({server, conn_id}) ─────│
  │                                   │ state: AUTHENTICATION          │
  │─── LOGON {scheme, creds} ───────►│──── handler.authenticate() ───►│──── Redis ACL AUTH
  │◄── SUCCESS {} ───────────────────│◄── Ok(()) ────────────────────│◄── OK
  │                                   │ state: READY                   │
  │─── RUN {query, params, extra} ──►│──── handler.run() ────────────►│
  │                                   │ crate creates QueryBuffer       │─── parse query
  │                                   │                                │─── create plan
  │◄── SUCCESS {fields, qid} ───────│◄── Ok({fields}) ────────────│
  │                                   │ state: STREAMING               │─── start execution
  │                                   │                                │─── EmitRow → QueryBuffer
  │                                   │                                │─── EmitRow → QueryBuffer
  │                                   │                                │─── EmitStats → complete + 10s timer
  │─── PULL {n:-1, qid} ────────────►│ crate drains QueryBuffer(qid)  │
  │◄── RECORD [val, val, ...] ──────│   (no host callback needed)    │
  │◄── RECORD [val, val, ...] ──────│                                │
  │◄── SUCCESS {stats, t_last} ─────│ buffer freed, timer cancelled  │
  │                                   │ state: READY                   │
  │─── GOODBYE ─────────────────────►│                                │
  │◄── [connection closed] ─────────│                                │
```

## Error Handling

Errors can occur at several points during query execution. The Bolt protocol uses the FAILURE response message to report errors, which transitions the connection to the FAILED state.

### Error Points and Handling

```
RUN arrives
  ├── Parse error (syntax error, unknown function, etc.)
  │     └── Send FAILURE immediately, state → FAILED
  │
  ├── Plan error (constraint violation, missing index, etc.)
  │     └── Send FAILURE immediately, state → FAILED
  │
  └── Execution starts successfully
        └── Send SUCCESS {fields, qid}, state → STREAMING
              │
              ├── Runtime error during execution (type error, division by zero,
              │   constraint violation, timeout, OOM, etc.)
              │     └── Buffer ERROR marker instead of more records
              │         On next PULL: send FAILURE, state → FAILED
              │
              └── Execution completes normally
                    └── On PULL: send RECORDs + SUCCESS {stats}
```

### FAILURE Response Format (Bolt protocol)

```
FAILURE (tag 0x7F, 1 field):
  metadata: Map {
    "code": String,      // e.g., "Neo.ClientError.Statement.SyntaxError"
    "message": String,   // Human-readable error description
  }
```

### Error handling in the buffered reply pattern

The `QueryBuffer` (see Buffered Reply Pattern section above) includes an `error` field. Error handling uses the same connection-agnostic buffer approach.

Three scenarios:

#### 1. Error during RUN (parse/plan error) - before execution starts

The handler returns Err from `run()`. The crate sends FAILURE immediately on the connection. No QueryBuffer is created.

```c
// In the run callback:
void falkordb_run_handler(BoltClient client, const char *query, ...) {
    // Parse query
    if (parse_failed) {
        bolt_reply_failure(client,
            "Neo.ClientError.Statement.SyntaxError",
            "Invalid input 'MTCH': expected 'MATCH'");
        bolt_end_message(client);
        bolt_flush_immediate(client);
        return;  // Connection moves to FAILED state
    }
    // ... normal execution ...
}
```

#### 2. Error during execution (runtime error) - after RUN SUCCESS already sent

The execution thread encounters an error mid-execution. It sets an error on the QueryBuffer via the `BoltClient`. This also starts the 10-second cleanup timer.

```c
// During execution, when an error is detected:
// (e.g., in ErrorCtx_EmitException or an error callback)
bolt_buffer_error(client,
                  "Neo.ClientError.Statement.TypeError",
                  "Type mismatch: expected Integer but was String");
// This stores the error, marks execution_complete = true, and starts the 10s timer.
// No more records will be buffered after this.
```

When PULL arrives (crate handles internally):
```
PULL {n, qid} arrives on some connection:
  1. Crate looks up QueryBuffer by qid
  2. Drains any records that were buffered BEFORE the error
  3. If buffer has an error:
     - Send FAILURE {code, message} instead of final SUCCESS
     - Connection state → FAILED
     - Cancel timer, free buffer
  4. Client must send RESET to recover to READY state
```

#### 3. Recovery from FAILED state

After a FAILURE, the connection is in FAILED state. Any messages except RESET and GOODBYE are IGNORED.

```
Client                          falkordb-bolt-rs
  │                                   │
  │─── RUN "bad query" ─────────────►│
  │◄── FAILURE {code, message} ─────│ state: FAILED
  │                                   │
  │─── RUN "good query" ───────────►│
  │◄── IGNORED ─────────────────────│ state: FAILED (can't process)
  │                                   │
  │─── RESET ───────────────────────►│
  │◄── SUCCESS {} ──────────────────│ state: READY (recovered)
  │                                   │
  │─── RUN "good query" ───────────►│ (works normally now)
```

#### Error codes mapping

FalkorDB errors should map to Neo4j-compatible error codes:

| FalkorDB Error | Bolt Error Code | When |
|---|---|---|
| Syntax error | `Neo.ClientError.Statement.SyntaxError` | Parse failure |
| Type mismatch | `Neo.ClientError.Statement.TypeError` | Runtime type error |
| Division by zero | `Neo.ClientError.Statement.ArithmeticError` | Runtime arithmetic error |
| Query timeout | `Neo.TransientError.Transaction.Terminated` | Timeout exceeded |
| Constraint violation | `Neo.ClientError.Schema.ConstraintValidationFailed` | Unique/property constraint |
| Auth failure | `Neo.ClientError.Security.Unauthorized` | LOGON failed |
| Unknown graph | `Neo.ClientError.Database.DatabaseNotFound` | Graph key doesn't exist |
| General error | `Neo.DatabaseError.General.UnknownError` | Catch-all |

#### C API for error handling

```c
// Send FAILURE response immediately (for parse/plan errors in RUN callback)
void bolt_reply_failure(BoltClient client,
                        const char *code, uint32_t code_len,
                        const char *message, uint32_t message_len);

// Buffer an error during execution (for runtime errors in EmitRow/execution)
// After this call, no more records can be buffered. Starts the 10s cleanup timer.
// The error will be sent as FAILURE when PULL drains the buffer.
void bolt_buffer_error(BoltClient client,
                       const char *code, uint32_t code_len,
                       const char *message, uint32_t message_len);
```

#### How FalkorDB C integrates error handling

The current `ErrorCtx_EmitException()` in `errors.c:99` already checks for `bolt_client`:
```c
void ErrorCtx_EmitException(void) {
    if (ctx->error != NULL) {
        bolt_client_t *bolt_client = QueryCtx_GetBoltClient();
        if (bolt_client != NULL) {
            // Current: sends FAILURE directly
            // New: call bolt_buffer_error() if mid-execution,
            //      or bolt_reply_failure() if during RUN
        }
    }
}
```

The change: instead of directly writing FAILURE to the bolt client, call the appropriate buffer/reply function from the Rust crate.

---

### RESET & Interrupted State

RESET is the most complex message in the Bolt protocol. Unlike all other messages, it has **dual behavior**: an out-of-band interrupt signal that jumps ahead in the message queue, and a queued message that recovers the connection to READY.

#### How RESET works (per [Bolt spec](https://neo4j.com/docs/bolt/current/bolt/server-state/))

When a client sends RESET:

1. **`<INTERRUPT>` signal**: On arrival at the server, RESET immediately generates an out-of-band interrupt that:
   - Stops any currently executing unit of work (e.g., a running query)
   - Transitions the state machine to **INTERRUPTED** (from any of: READY, STREAMING, TX_READY, TX_STREAMING, FAILED, or INTERRUPTED itself)

2. **Queued RESET message**: The RESET also queues in the normal message pipeline. All messages **ahead** of RESET in the queue are responded to with **IGNORED** and the state stays INTERRUPTED. When the RESET's turn comes:
   - On success: responds with **SUCCESS {}**, state → **READY**
   - On failure: responds with **FAILURE {}**, state → **DEFUNCT** (connection closed)

3. **Side effects of successful RESET**:
   - Any open transaction is rolled back
   - Any active QueryBuffer is discarded (error + free)
   - Connection state returns to READY (as if HELLO + LOGON had just completed)

#### State transition table for INTERRUPTED

| Current State | Message | Response | New State |
|---|---|---|---|
| INTERRUPTED | RUN | IGNORED | INTERRUPTED |
| INTERRUPTED | PULL | IGNORED | INTERRUPTED |
| INTERRUPTED | DISCARD | IGNORED | INTERRUPTED |
| INTERRUPTED | BEGIN | IGNORED | INTERRUPTED |
| INTERRUPTED | COMMIT | IGNORED | INTERRUPTED |
| INTERRUPTED | ROLLBACK | IGNORED | INTERRUPTED |
| INTERRUPTED | RESET | SUCCESS {} | READY |
| INTERRUPTED | RESET | FAILURE {} | DEFUNCT |
| INTERRUPTED | GOODBYE | (disconnect) | DEFUNCT |

#### Implementation in the crate

The key challenge: RESET must **interrupt a running query** that may be executing on a different thread (the graph engine thread), while the RESET arrives on the event loop thread via the socket.

```rust
/// Per-connection interrupt flag, shared between the event loop and execution thread.
/// Set by the event loop when RESET arrives; polled by the execution engine.
pub struct InterruptFlag {
    interrupted: AtomicBool,
}

impl InterruptFlag {
    pub fn set(&self) { self.interrupted.store(true, Ordering::Release); }
    pub fn clear(&self) { self.interrupted.store(false, Ordering::Release); }
    pub fn is_set(&self) -> bool { self.interrupted.load(Ordering::Acquire); }
}
```

The `BoltConnection` owns an `Arc<InterruptFlag>` which is shared with the execution context:

```rust
pub struct BoltConnection {
    state: BoltState,
    // ...
    interrupt_flag: Arc<InterruptFlag>,
    /// Messages queued between <INTERRUPT> signal and RESET processing.
    /// Each gets an IGNORED response.
    pending_ignores: u32,
}
```

#### Processing flow when RESET arrives

```
Event loop thread                          Execution thread
────────────────                           ────────────────
RESET arrives on socket
  │
  ├── 1. Set interrupt_flag (atomic)  ───► execution polls flag
  │                                        → query aborts early
  │                                        → bolt_buffer_error(client, ...)
  │
  ├── 2. Transition state → INTERRUPTED
  │
  ├── 3. Count queued unprocessed messages
  │      (these will get IGNORED responses)
  │
  ├── 4. Process queued messages:
  │      for each message ahead of RESET:
  │        → write IGNORED response
  │        → state stays INTERRUPTED
  │
  ├── 5. Process the RESET itself:
  │      → roll back any open transaction (handler.rollback())
  │      → discard active QueryBuffer if any
  │      → clear interrupt_flag
  │      → write SUCCESS {}
  │      → state → READY
  │
  └── Done. Connection is clean.
```

#### How the execution engine cooperates

The execution engine (host app) must periodically check the interrupt flag during query execution. This is NOT a new concept — FalkorDB C already has `QueryCtx_GetStatus()` which checks for timeout. The interrupt flag integrates with this:

**In FalkorDB C**:
```c
// The interrupt flag pointer is passed to the execution context during RUN
// In QueryCtx or similar, check periodically:
bool should_abort = bolt_connection_is_interrupted(client);
if (should_abort) {
    // Stop execution, emit error to buffer
    bolt_buffer_error(client,
        "Neo.TransientError.Transaction.Terminated",
        "The transaction has been terminated (RESET)");
    return;
}
```

**In FalkorDB Rust (next-gen)**:
```rust
// The Arc<InterruptFlag> is accessible from the Runtime
// Check during iteration:
if conn.interrupt_flag.is_set() {
    return Err(BoltError::interrupted());
}
```

#### C FFI additions for interrupt

```c
// Check if a connection has been interrupted (called by execution engine)
#[no_mangle] pub extern "C" fn bolt_connection_is_interrupted(client: BoltClient) -> bool;
```

#### What happens to the QueryBuffer on RESET

When RESET triggers INTERRUPTED:
1. If a QueryBuffer is actively being written to (execution in progress):
   - The interrupt flag causes execution to abort
   - Execution calls `bolt_buffer_error()` which marks the buffer as complete with error
2. When RESET is processed (state → READY):
   - If the buffer hasn't been consumed by PULL yet, it is discarded and freed
   - The 10-second timer (if started) is cancelled
3. The `client_to_buffer` mapping is cleared for this client

#### Sequence diagram: RESET during query execution

```
Client                   Event Loop Thread           Execution Thread
  │                           │                           │
  │─── RUN {query} ─────────►│                           │
  │◄── SUCCESS {fields,qid} ─│──── handler.run() ───────►│
  │                           │                           │── executing...
  │                           │                           │── EmitRow → buffer
  │─── RESET ────────────────►│                           │── EmitRow → buffer
  │                           │── set interrupt_flag ────►│
  │                           │── state → INTERRUPTED     │── polls flag → abort
  │                           │                           │── bolt_buffer_error()
  │                           │                           │── (done)
  │                           │                           │
  │                           │── process queued msgs:    │
  │                           │   (none in this case)     │
  │                           │                           │
  │                           │── process RESET:          │
  │                           │   discard QueryBuffer     │
  │                           │   clear interrupt_flag    │
  │◄── SUCCESS {} ───────────│── state → READY           │
  │                           │                           │
  │─── RUN {new query} ─────►│  (connection is clean)    │
```

#### Sequence diagram: RESET with pipelined messages

Drivers often pipeline messages. If a client sends RUN + PULL + RESET, the RUN and PULL arrive before RESET:

```
Client                   Event Loop Thread
  │                           │
  │─── RUN {query} ─────────►│  (queued)
  │─── PULL {n:-1} ─────────►│  (queued)
  │─── RESET ────────────────►│
  │                           │── <INTERRUPT> signal → state: INTERRUPTED
  │                           │
  │                           │── process RUN:
  │◄── IGNORED ──────────────│   (state is INTERRUPTED, ignore)
  │                           │
  │                           │── process PULL:
  │◄── IGNORED ──────────────│   (state is INTERRUPTED, ignore)
  │                           │
  │                           │── process RESET:
  │◄── SUCCESS {} ───────────│   state → READY
```

#### Multiple RESETs

If multiple RESETs are pipelined, each additional RESET while in INTERRUPTED state generates another `<INTERRUPT>` (no-op since already interrupted). The first RESET transitions to READY; subsequent RESETs in READY state also succeed (READY → INTERRUPTED → READY):

```
Client                   Event Loop Thread
  │─── RESET ────────────────►│── state → INTERRUPTED
  │─── RESET ────────────────►│── (already INTERRUPTED, no-op)
  │                           │── process first RESET:
  │◄── SUCCESS {} ───────────│── state → READY
  │                           │── process second RESET:
  │                           │── <INTERRUPT> → INTERRUPTED → process → READY
  │◄── SUCCESS {} ───────────│── state → READY
```

---

## Part 2: Integration with FalkorDB (C)

### Changes Required

#### 1. Remove `src/bolt/` directory
Replace the entire `src/bolt/` directory with calls to the Rust crate's C FFI.

#### 2. Link the Rust crate
- Build `falkordb-bolt-rs` as a static library (`crate-type = ["staticlib", "cdylib", "rlib"]`)
- Add to FalkorDB's build system (CMakeLists.txt): link against `libfalkordb_bolt.a`
- Include the cbindgen-generated `falkordb_bolt.h` header

#### 3. Remove `--bolt` flag from command pipeline
The current C implementation hacks Bolt into the RESP command pipeline by constructing fake args with `--bolt` and calling `CommandDispatch()`. This approach is eliminated. Bolt has its own connection path that calls the graph engine directly, NOT through Redis commands.

#### 4. Modify `src/module.c` - Registration
Replace `BoltApi_Register(ctx)` with:
```c
int listen_fd = bolt_server_listen(port);
RedisModule_EventLoopAdd(listen_fd, REDISMODULE_EVENTLOOP_READABLE, BoltAcceptHandler, ctx);
```

Register callbacks:
```c
bolt_set_auth_callback(falkordb_auth_handler);
bolt_set_run_callback(falkordb_run_handler);
bolt_set_begin_callback(falkordb_begin_handler);
bolt_set_commit_callback(falkordb_commit_handler);
bolt_set_rollback_callback(falkordb_rollback_handler);
```

#### 5. New file: `src/bolt_bridge.c`
Thin bridge implementing the callbacks. The `falkordb_run_handler` callback:
1. Extracts graph name from extras `db` field (or defaults to "falkordb")
2. Opens the graph key directly via `RedisModule_OpenKey`
3. Calls the graph engine's query execution function directly (same code path used by `Graph_Query` in `cmd_query.c` but without the RESP command overhead)
4. Uses the Rust crate's reply functions (`bolt_reply_*`) to serialize results directly to the Bolt connection

This is a departure from the current approach in `bolt_api.c:306-390` where `BoltRunCommand()` fakes a `CommandDispatch()` call. Instead, the bolt bridge calls the graph engine at the same level as the command handlers do, bypassing the Redis command dispatcher entirely.

#### 6. Modify `src/resultset/formatters/resultset_replybolt.c`
Replace `bolt_reply_*` calls from the old C implementation with calls to the new FFI functions. The function signatures are designed to match, so changes are mostly mechanical:
- `bolt_reply_string(client, str, len)` → same name, same signature in FFI
- `bolt_reply_structure(client, BST_NODE, 4)` → same
- `bolt_client_reply_for(...)` → `bolt_reply_success(...)` or `bolt_reply_record(...)`
- `bolt_client_end_message(...)` → `bolt_end_message(...)`
- `bolt_client_finish_write(...)` → `bolt_finish_write(...)`

Note: This file is used during Phase 1 streaming (buffer-all). In Phase 2+, the Bolt bridge will emit rows directly during execution instead of using the ResultSetFormatter pattern.

**Streaming in FalkorDB C - Phased Approach**:

The current C implementation buffers ALL rows before emitting:
- `ResultSet_AddRecord()` (`resultset.c:128`) copies each row to `set->cells` DataBlock during execution
- `ResultSet_Reply()` (`resultset.c:222`) loops over all buffered cells AFTER execution, calling `formatter->EmitRow()` per row
- This means: execute everything → buffer everything → serialize everything

For true Bolt streaming with `PULL {n}`, we need 3 phases:

1. **Phase 1 (initial migration)**: Keep buffer-all behavior. RUN executes and buffers all results. PULL flushes the buffer through the Rust crate's serialization. Simple, works now.

2. **Phase 2 (emit-as-you-go)**: For the Bolt formatter only, change `ResultSet_AddRecord()` to call `EmitRow()` immediately instead of buffering. This gives streaming output during execution but doesn't support suspending execution (PULL with count > -1 just flushes what's available).

3. **Phase 3 (full streaming)**: Suspendable plan execution. RUN starts the plan iterator and stores it on the connection. PULL resumes the iterator for N rows. Requires:
   - Storing execution plan state on `bolt_client_t` between PULL calls
   - MVCC snapshot held across the RUN→PULL lifecycle
   - Lock management: read lock or snapshot must persist across PULL boundaries
   - Major refactor of execution engine to support cooperative yielding

The **Rust project** is better positioned for Phase 3 since `Runtime::run()` already returns an iterator internally - we just avoid `.collect()` and yield rows lazily.

#### 7. Event loop handlers
New handlers that wrap the Rust crate:
- `BoltAcceptHandler` → calls `bolt_server_accept()`, registers read handler
- `BoltReadHandler` → reads from socket, calls `bolt_connection_feed_data()` + `bolt_connection_process()`
- `BoltResponseHandler` → calls `bolt_connection_get_write_data()`, writes to socket

---

## Part 3: Integration with falkordb-rs-next-gen (Rust)

### Changes Required

#### 1. Add dependency to `Cargo.toml`
```toml
falkordb-bolt = { path = "../falkordb-bolt-rs" }
```

#### 2. Create `src/bolt.rs` - Bolt handler implementation

Implement the `BoltHandler` trait:

```rust
pub struct FalkorBoltHandler {
    // Access to graph storage (same Arc<RwLock<ThreadedGraph>> pattern)
}

impl BoltHandler for FalkorBoltHandler {
    fn authenticate(&self, conn: &mut BoltConnection, msg: &LogonMessage<'_>) -> Result<(), BoltError> {
        // msg.scheme, msg.principal, msg.credentials are &str borrowed from buffer
        // Call Redis AUTH via the context if needed (pluggable)
    }

    fn run(&self, conn: &mut BoltConnection, msg: &RunMessage<'_>) -> Result<(), BoltError> {
        // msg.query is &str (zero-copy from buffer)
        // msg.parameters is PackStreamSlice — parse on demand with msg.parameters.reader()
        // msg.extra.db is Option<&str> (zero-copy)
        // 1. Extract graph name from msg.extra.db (default "falkordb")
        // 2. Parse parameters from PackStreamSlice
        // 3. Parse query, create plan
        // 4. Store execution iterator in connection state for PULL
        // 5. Write SUCCESS metadata with fields + qid directly via conn.write_*
    }

    // PULL is handled internally by the crate — no handler callback.
    // The crate drains pre-serialized records from the QueryBuffer.
    // ... etc.
}
```

#### 3. True Streaming RUN/PULL Model

Unlike the current C implementation which buffers all results during RUN, the Rust implementation uses **true streaming**:

- **RUN**: Parses query, creates execution plan, returns SUCCESS with column names. Does NOT execute yet.
- **PULL {n}**: Lazily executes/resumes the plan iterator, yields up to `n` rows as RECORD messages. Returns SUCCESS with `has_more: true` if more rows remain, or final SUCCESS with stats when done.
- **DISCARD {n}**: Advances the iterator without serializing, discards `n` rows.

This requires the `BoltHandler::run()` to store an **execution iterator** (not a materialized `Vec<Env>`) in per-connection state, and `pull()` to resume iteration.

```rust
/// Per-query state stored on the connection between RUN and PULL.
struct ActiveQuery {
    column_names: Vec<String>,
    iterator: Box<dyn Iterator<Item = Env> + Send>,
    stats: QueryStatistics,
    runtime: Runtime,
}

fn reply_bolt_pull(
    conn: &mut BoltConnection,
    active: &mut ActiveQuery,
    n: i64,  // -1 = all remaining
) {
    let mut count = 0;
    let limit = if n == -1 { usize::MAX } else { n as usize };

    while count < limit {
        match active.iterator.next() {
            Some(row) => {
                conn.write_record_header(active.column_names.len() as u32);
                for name in &active.column_names {
                    let value = row.get(name).unwrap();
                    write_runtime_value(conn, &active.runtime, value.clone());
                }
                conn.end_message();
                count += 1;
            }
            None => break, // exhausted
        }
    }

    // Check if more rows
    let has_more = active.iterator.size_hint().1 != Some(0); // approximate
    if has_more {
        conn.write_success_header(1);
        conn.write_string("has_more");
        conn.write_bool(true);
    } else {
        // Final SUCCESS with stats — write directly
        write_stats(conn, &active.stats);
    }
    conn.end_message();
}
```

**Impact on falkordb-rs-next-gen**: The current `Runtime::query()` returns `ResultSummary` (fully materialized `Vec<Env>`). To support true streaming, the runtime needs to expose an **iterator-based API** that yields rows one at a time. This is a change to `graph/src/runtime/runtime.rs` - the `run()` function already returns an iterator internally, we just need to avoid collecting it into a Vec.

---

### Buffered Reply Pattern (Decouples Execution from PULL)

The execution engine is protocol-agnostic. It writes pre-serialized RECORD messages into a **QueryBuffer** during execution. The QueryBuffer is **not owned by any connection** — it is a standalone object identified by a `buffer_id`. This is critical because the PULL message that consumes records may arrive on a **different connection** than the one that sent RUN.

```
RUN arrives on conn A
  → create QueryBuffer (buffer_id = 42)
  → SUCCESS (sent immediately on conn A with fields, qid=buffer_id)
              ↓
         Execution starts
              ↓
         EmitRow(row1) → serialize RECORD → append to QueryBuffer(42)
         EmitRow(row2) → serialize RECORD → append to QueryBuffer(42)
         EmitRow(row3) → serialize RECORD → append to QueryBuffer(42)
         EmitStats()   → store stats, mark complete → start 10s timeout
              ...                        ↑
                                    PULL {n:2, qid:42} arrives (possibly on conn B!)
                                         ↓
                           Drain 2 RECORDs from QueryBuffer(42) → send on conn B
                           SUCCESS {has_more: true} → send on conn B
                                         ↑
                                    PULL {n:-1, qid:42} arrives
                                         ↓
                           Drain remaining → send to client
                           SUCCESS {stats} → send to client (final)
                           QueryBuffer(42) is freed
```

#### QueryBuffer (standalone, connection-agnostic, thread-safe)

The `QueryBuffer` is an **internal crate type** — the C API never sees it or its `buffer_id` directly. The crate:
1. Creates a `QueryBuffer` when processing a RUN message
2. Stores the `buffer_id` as `qid` in the RUN SUCCESS response
3. Maps the `BoltClient` to its active buffer (so `bolt_buffer_record_begin(client, ...)` routes correctly)
4. When PULL arrives with `{qid}`, looks up the buffer by `qid` — which may be on a different connection

```rust
/// A query result buffer, identified by a unique buffer_id.
/// Created on RUN, consumed by PULL, potentially from a different connection.
pub struct QueryBuffer {
    buffer_id: i64,
    records: VecDeque<BytesMut>,                   // Pre-serialized RECORD messages
    execution_complete: bool,                       // True when engine is done (records or error)
    final_stats: Option<BytesMut>,                   // Pre-serialized SUCCESS metadata bytes
    error: Option<BoltError>,                       // Error during execution
    expiry_timer: Option<TimerHandle>,              // 10s cleanup timer, started on completion
}

/// Global registry of active query buffers.
/// Thread-safe: accessed by execution threads (producer) and event loop (consumer).
pub struct QueryBufferRegistry {
    buffers: Mutex<HashMap<i64, QueryBuffer>>,
    /// Maps BoltClient → active buffer_id (so bolt_reply_* inside record_begin/end
    /// can find the right buffer without exposing buffer_id to C)
    client_to_buffer: Mutex<HashMap<*mut BoltConnection, i64>>,
    next_id: AtomicI64,
}

impl QueryBufferRegistry {
    /// Create a new buffer and associate it with a client. Returns buffer_id.
    pub fn create_for_client(&self, client: *mut BoltConnection) -> i64;

    /// Get the active buffer for a client (used by bolt_reply_* inside record blocks).
    pub fn get_for_client(&self, client: *mut BoltConnection) -> Option<&mut QueryBuffer>;

    /// Get a buffer by ID (used by PULL handler — may be different connection).
    pub fn get_by_id(&self, buffer_id: i64) -> Option<&mut QueryBuffer>;

    /// Remove and free a buffer, cancel timer.
    pub fn remove(&self, buffer_id: i64);
}
```

#### Buffer completion timeout

When the execution engine marks a buffer as complete (via `bolt_buffer_complete` or `bolt_buffer_error`), a **10-second expiry timer** starts. If the buffer is not fully consumed by a PULL within 10 seconds, it is automatically cleaned up and freed. This prevents memory leaks from abandoned queries (e.g., client disconnects before sending PULL).

```
Execution completes → bolt_buffer_complete(buffer_id)
                       → execution_complete = true
                       → start 10-second timer

Timer fires (10s later):
  → if buffer still has unconsumed records → log warning, free buffer
  → if buffer already freed (consumed by PULL) → no-op

PULL fully consumes buffer:
  → cancel timer, free buffer immediately
```

#### Buffer API (exposed via FFI for C)

The C API uses `BoltClient` for all operations. The crate internally manages `QueryBuffer` instances and maps them to clients. The `buffer_id` is never exposed to C — it only appears as the `qid` field in Bolt messages.

```c
// Called by EmitRow (execution thread) - serializes RECORD into buffer
// The crate knows which QueryBuffer belongs to this client.
void bolt_buffer_record_begin(BoltClient client, uint32_t field_count);
// ... write fields via bolt_reply_*(client, ...) — crate routes to buffer ...
void bolt_buffer_record_end(BoltClient client);

// Called by EmitStats (execution thread) - marks execution complete, starts 10s timer
void bolt_buffer_complete(BoltClient client);

// Buffer an error during execution - marks complete, starts 10s timer
void bolt_buffer_error(BoltClient client,
                       const char *code, uint32_t code_len,
                       const char *message, uint32_t message_len);

// Send data immediately on a connection (e.g., RUN SUCCESS before execution starts)
void bolt_flush_immediate(BoltClient client);

// PULL is handled internally by the crate:
// - Crate receives PULL {n, qid} from the wire
// - Looks up QueryBuffer by qid (the buffer_id)
// - Drains N records from the buffer into the requesting connection
// - No C callback needed
```

#### PULL scenarios

| Scenario | Buffer state | Action |
|---|---|---|
| PULL before any rows buffered | Empty, execution running | Return SUCCESS {has_more: true}, 0 records |
| PULL with partial buffer | N records available, execution running | Drain min(n, N) records, SUCCESS {has_more: true} |
| PULL after execution complete | Records remaining | Drain, if last batch: SUCCESS {stats}, else: SUCCESS {has_more: true} |
| PULL, buffer empty, exec complete | Empty, done | SUCCESS {stats} (final, 0 records), free buffer |
| PULL on different connection | Any | Works — buffer_id is the lookup key, not the connection |
| No PULL within 10s of completion | Complete, unconsumed | Timer fires → buffer freed, log warning |

#### Thread safety
Producer (execution thread) and consumer (PULL on event loop thread) access the buffer concurrently. `QueryBuffer` internals are behind a `Mutex`. Contention is low since producer appends and consumer pops from the front. The `QueryBufferRegistry` uses a separate `Mutex<HashMap>` for the global map.

#### How FalkorDB C's ResultSetFormatter maps to this

```
EmitHeader() → bolt_flush_immediate(client)         // Send RUN SUCCESS now (crate includes qid)
EmitRow()    → bolt_buffer_record_begin/end(client)  // Crate routes bolt_reply_* to QueryBuffer
EmitStats()  → bolt_buffer_complete(client)          // Mark done, store stats, start 10s timer
                                                      // PULL handler drains later (any connection)
```

This means `ResultSet_ReplyWithBoltHeader`, `ResultSet_EmitBoltRow`, and `ResultSet_EmitBoltStats` in `resultset_replybolt.c` need only minor changes: replace direct write functions with the new `bolt_reply_*` / `bolt_buffer_*` calls. The C code always uses `BoltClient` — no `buffer_id` exposed. The execution engine and `ResultSet_AddRecord` flow remain unchanged.

#### 4. Add `write_runtime_value()` in `src/bolt.rs`

Writes `graph::runtime::value::Value` directly to the connection buffer — no intermediate types:

```rust
// See the detailed write_runtime_value() implementation in Part 1
// "Resolution responsibility" section. It writes directly to conn using:
//   conn.write_node_header(id, label_count, prop_count)
//   conn.write_string(label)
//   conn.write_int(value)
//   conn.write_relationship_header(...)
//   etc.
// No intermediate types. Data flows: graph → writer buffer.
```

#### 5. Bolt runs on a separate connection path (no command handler changes)

Bolt connections do NOT flow through `graph_query` / `graph_ro_query`. Those command handlers remain RESP-only (verbose/compact). The Bolt path is entirely separate:

```
RESP path:  Redis client → graph.QUERY cmd → graph_query() → reply_verbose/compact → RESP
Bolt path:  Neo4j driver → Bolt TCP → BoltHandler::run() → reply_bolt() → PackStream
```

The `FalkorBoltHandler` implementation directly calls the graph engine (`ThreadedGraph::execute_query()`), bypassing the Redis command pipeline entirely. This means:
- No `--bolt` flag needed anywhere
- `reply_verbose()` and `reply_compact()` are untouched
- The graph engine's `Runtime` and `ResultSummary` types are reused by both paths
- The only shared code between RESP and Bolt is the graph engine itself

#### 6. Bolt server registration in module init

The Rust next-gen project has NO event loop integration today. The `redis-module` crate (v2.1.3) provides raw FFI bindings for `RedisModule_EventLoopAdd` but no safe wrapper. Registration requires `unsafe`:

```rust
// In graph_init():
let listen_fd = falkordb_bolt::bolt_listen(bolt_port)?;

// Register listening socket with Redis event loop (unsafe - raw C API)
unsafe {
    let add_fn = redis_module::raw::RedisModule_EventLoopAdd.unwrap();
    add_fn(
        listen_fd,
        redis_module::raw::REDISMODULE_EVENTLOOP_READABLE as c_int,
        Some(bolt_accept_handler),  // extern "C" fn(fd: i32, user_data: *mut c_void, mask: i32)
        global_ctx as *mut c_void,
    );
}

// Accept handler (extern "C" for Redis event loop callback)
unsafe extern "C" fn bolt_accept_handler(fd: i32, user_data: *mut c_void, _mask: i32) {
    let conn = falkordb_bolt::bolt_accept(fd);
    // ... register read handler for this connection
    let add_fn = redis_module::raw::RedisModule_EventLoopAdd.unwrap();
    add_fn(
        conn.fd(),
        redis_module::raw::REDISMODULE_EVENTLOOP_READABLE as c_int,
        Some(bolt_read_handler),
        conn as *mut c_void,
    );
}
```

For comparison, the **C project** does the same thing natively:
```c
// In module init (bolt_api.c:802):
socket_t bolt = socket_bind(port);
RedisModule_EventLoopAdd(bolt, REDISMODULE_EVENTLOOP_READABLE, BoltAcceptHandler, global_ctx);

// Accept handler registers per-client handlers:
RedisModule_EventLoopAdd(socket, REDISMODULE_EVENTLOOP_READABLE, BoltHandshakeHandler, client);
// After handshake:
RedisModule_EventLoopAdd(fd, REDISMODULE_EVENTLOOP_READABLE, BoltReadHandler, client);
// When data ready to write:
RedisModule_EventLoopAdd(fd, REDISMODULE_EVENTLOOP_WRITABLE, BoltResponseHandler, client);
```

Both projects use the same underlying Redis C API (`RedisModule_EventLoopAdd`). The Rust project calls it via `unsafe` raw bindings.

---

## Part 4: Implementation Phases

### Phase 1: PackStream Core (this repo, ~first milestone)
1. `packstream/marker.rs` - All PackStream marker constants
2. `packstream/serialize.rs` - PackStreamWriter with all type serializers
3. `packstream/deserialize.rs` - PackStreamReader with all type parsers
4. `packstream/types.rs` - PackStream type tags and Bolt struct tag constants
5. Unit tests: round-trip serialize/deserialize for every type

### Phase 2: Protocol Layer
1. `protocol/handshake.rs` - Version negotiation (magic bytes + v5.8 range matching)
2. `protocol/chunking.rs` - Chunk encoder/decoder
3. `protocol/message.rs` - Request parsing and response serialization
4. `protocol/state.rs` - State machine transitions
5. Unit tests: message encoding matches Bolt spec examples

### Phase 3: Server + Transport
1. `transport/tcp.rs` - TCP listener, accept, non-blocking socket setup
2. `transport/websocket.rs` - HTTP upgrade handshake, WS frame encode/decode
3. `server/connection.rs` - BoltConnection with full lifecycle
4. `server/handler.rs` - BoltHandler trait
5. `server/event_loop.rs` - fd-based integration functions
6. Integration test: connect with `neo4j-driver` (Python), run HELLO/LOGON

### Phase 4: Standalone Integration Tests (mock handler + real drivers)
1. `tests/mock_handler.rs` - MockBoltHandler with canned responses
2. `tests/common/mod.rs` - Test harness (start mock server on random port)
3. `tests/integration.rs` - Rust-side integration tests (connection lifecycle, query, values, errors, transactions)
4. `tests/drivers/python/` - Python driver test suite against mock server
5. Verify all value types, error recovery, RESET, transactions with real Neo4j Python driver
6. Test WebSocket transport with Neo4j Browser or JS driver

### Phase 5: C FFI
1. `ffi/c_api.rs` - All extern "C" functions
2. `build.rs` - cbindgen header generation
3. Test: compile and link from a C test program
4. Match reply API names to existing FalkorDB C bolt API for easy migration

### Phase 6: FalkorDB C Integration
1. Create `src/bolt_bridge.c` with callback implementations
2. Update CMakeLists.txt to link Rust static library
3. Replace `src/bolt/` calls with new FFI calls in `resultset_replybolt.c`
4. Update `bolt_api.c` event loop handlers → new bridge
5. Test: connect Neo4j Browser to FalkorDB via new Rust bolt

### Phase 7: falkordb-rs-next-gen Integration
1. Add `src/bolt.rs` with `FalkorBoltHandler` + `write_runtime_value()`
2. Add `reply_bolt()` to `src/lib.rs`
3. Register Bolt listener in module init
4. Add `--bolt` flag handling to command dispatch
5. Test: connect Neo4j Python driver, run queries

---

## Part 5: Protocol Versioning & Extensibility

### How the crate supports multiple versions and evolves over time

The design uses a **single codebase with version-aware branching**, NOT separate implementations per version.

#### Version-aware parsing and serialization

```rust
/// Negotiated version stored on each connection.
pub struct BoltVersion {
    pub major: u8,
    pub minor: u8,
}

impl BoltConnection {
    /// Parse a request message, respecting the negotiated version.
    fn parse_request(&self, reader: &mut PackStreamReader) -> Result<BoltRequest, BoltError> {
        let (tag, size) = reader.read_struct_header()?;
        match tag {
            0x01 => self.parse_hello(reader, size),
            0x6A if self.version.minor >= 1 => self.parse_logon(reader, size),
            0x54 if self.version.minor >= 4 => self.parse_telemetry(reader, size),
            0x6A => Err(BoltError::UnsupportedMessage("LOGON requires Bolt >= 5.1")),
            // ...
            _ => Err(BoltError::UnknownMessage(tag)),
        }
    }

    /// Streaming node writer, respecting version-specific struct layouts.
    /// Called internally by conn.write_node_header().
    fn write_node_header_internal(&self, writer: &mut PackStreamWriter, id: i64,
                                   label_count: u32, prop_count: u32) {
        // 5.0+: 4 fields (id, labels, properties, element_id)
        writer.write_struct_header(0x4E, 4);
        writer.write_int(id);
        writer.write_list_header(label_count);
        // ... caller writes labels + properties ...
        // element_id generated from id at the end
    }
}
```

#### Adding a new Bolt version (e.g., 6.0)

When Bolt 6.0 ships, the changes are:

1. **Update version negotiation** (`handshake.rs`): Add 6.0 to accepted version ranges
2. **Add new struct tags** (`types.rs`): `VECTOR: u8 = 0x56`, etc.
3. **Add new streaming writer methods**: `conn.write_vector_header()` etc.
4. **Version-gate serialization**: `if version.major >= 6 { write_vector(...) }`
5. **Update BoltHandler trait** (if needed): Add default method implementations for new messages

The key principle: **new versions ADD; they don't change existing behavior**. All version-specific differences are handled in the protocol layer via `if version >= X` checks. The `BoltHandler` trait remains stable - new optional messages get default implementations:

```rust
pub trait BoltHandler {
    // New in 5.4, default implementation accepts and ignores
    fn telemetry(&self, _conn: &mut BoltConnection, _api: i64) -> Result<(), BoltError> {
        Ok(())  // Default: accept and ignore
    }

    // Future: New in 6.0, default returns error
    fn vector_query(&self, ...) -> Result<(), BoltError> {
        Err(BoltError::Unsupported("Vector queries not implemented"))
    }
}
```

This means **existing integrations don't break** when the crate adds support for newer protocol versions. Consumers can opt-in to new features by implementing new trait methods.

---

## Part 5b: Bolt 5.8 Specifics

Features to support (cumulative from 5.0 to 5.8):
- **5.0**: element_id fields on Node/Relationship (string-based IDs alongside integer IDs)
- **5.1**: Separated LOGON/LOGOFF messages (auth separated from HELLO)
- **5.2**: Notification filtering in BEGIN/RUN extras
- **5.3**: Bolt agent field in HELLO
- **5.4**: TELEMETRY message (can be accepted and ignored)
- **5.7**: Manifest v1 handshake (enhanced version negotiation) - support legacy 4-version handshake too
- **5.8**: Latest stable, no additional breaking changes from 5.7

Version negotiation: Accept client proposals for 5.1-5.8 range. Respond with 5.8 (or highest mutually supported).

---

## Part 6: Verification & Testing

### Level 1: Unit Tests (no network, no external dependencies)

Run as `cargo test` in the crate. Test the building blocks in isolation.

- **PackStream round-trip**: serialize → deserialize for every type (null, bool, all int widths, float, string, bytes, list, map, struct headers). Verify minimal encoding (e.g., `42` uses TINY_INT not INT64).
- **Message parsing**: For all 13 request types, construct raw PackStream bytes and parse into the corresponding message struct. Verify borrowed `&str` fields point to the correct buffer range.
- **Message serialization**: For SUCCESS/FAILURE/IGNORED/RECORD, write to a buffer and verify the bytes match the spec.
- **State machine transitions**: Test all valid transitions (READY + RUN → STREAMING, etc.) and verify invalid ones return errors. Test `should_ignore()` in FAILED and INTERRUPTED states.
- **Chunking**: Encode/decode with small messages (single chunk), large messages (multi-chunk), and edge cases (exactly chunk-sized, empty message, max chunk size 65535).
- **WebSocket frame**: Encode/decode with and without masking. Test fragmented frames.
- **Version negotiation**: Test handshake parsing with various client version proposals, verify correct version selection.

### Level 2: Standalone Crate Integration Tests (real drivers, mock handler)

**This is the critical standalone validation phase.** The crate is tested as a self-contained Bolt server using a `MockBoltHandler` that returns hardcoded responses. No FalkorDB needed. Real Neo4j drivers connect over TCP/WebSocket.

#### MockBoltHandler

The mock handler lives in `tests/mock_handler.rs` and implements `BoltHandler`:

```rust
/// A mock handler that serves hardcoded responses.
/// Used to test the full protocol stack (transport → chunking → handshake →
/// state machine → message parsing → response serialization) with real drivers.
struct MockBoltHandler {
    /// If set, run() returns these column names and canned records.
    /// This allows testing value serialization without a real graph engine.
    canned_responses: HashMap<String, CannedQuery>,
}

struct CannedQuery {
    columns: Vec<String>,
    /// Each record is a closure that writes fields directly to the connection.
    /// This tests the streaming writer API.
    records: Vec<Box<dyn Fn(&mut BoltConnection)>>,
}

impl BoltHandler for MockBoltHandler {
    fn hello(&self, conn: &mut BoltConnection, msg: &HelloMessage<'_>) -> Result<(), BoltError> {
        // Write SUCCESS with server info
        conn.write_success_header(3);
        conn.write_string("server"); conn.write_string("FalkorDB/mock");
        conn.write_string("connection_id"); conn.write_string("mock-1");
        conn.write_string("hints"); conn.write_map_header(0);
        conn.end_message();
        Ok(())
    }

    fn authenticate(&self, conn: &mut BoltConnection, msg: &LogonMessage<'_>) -> Result<(), BoltError> {
        // Accept any credentials
        conn.write_success_header(0);
        conn.end_message();
        Ok(())
    }

    fn run(&self, conn: &mut BoltConnection, msg: &RunMessage<'_>) -> Result<(), BoltError> {
        // Look up canned response by query text
        match self.canned_responses.get(msg.query) {
            Some(canned) => {
                // Buffer records
                for record_fn in &canned.records {
                    bolt_buffer_record_begin(conn, canned.columns.len() as u32);
                    record_fn(conn);
                    bolt_buffer_record_end(conn);
                }
                bolt_buffer_complete(conn);
                // Write RUN SUCCESS (crate adds qid)
                Ok(())
            }
            None => Err(BoltError::new(
                "Neo.ClientError.Statement.SyntaxError",
                &format!("Unknown mock query: {}", msg.query),
            )),
        }
    }

    // ... begin/commit/rollback/reset/route with simple hardcoded responses
}
```

#### Test suite structure

Tests live in `tests/` directory and run the mock server on a random port:

```rust
// tests/common/mod.rs
fn start_mock_server(handler: MockBoltHandler) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        // Accept connections, run bolt protocol with mock handler
    });
    (port, handle)
}
```

#### Test categories

**A. Connection lifecycle (handshake, auth, goodbye)**

| Test | What it verifies |
|------|-----------------|
| `test_handshake_version_negotiation` | Driver connects, negotiates Bolt 5.x. Verify server picks highest mutual version. |
| `test_hello_success` | HELLO succeeds, driver receives server metadata. |
| `test_auth_basic` | LOGON with basic auth succeeds. |
| `test_auth_failure` | LOGON with bad credentials → FAILURE, connection still usable after RESET. |
| `test_goodbye_clean_disconnect` | GOODBYE closes connection cleanly. |

**B. Query execution (RUN, PULL, DISCARD)**

| Test | What it verifies |
|------|-----------------|
| `test_return_scalar` | `RETURN 1 AS n` → driver receives integer `1`. Tests basic RUN/PULL cycle. |
| `test_return_string` | `RETURN 'hello' AS s` → driver receives string. |
| `test_return_multiple_columns` | `RETURN 1 AS a, 'x' AS b` → driver receives 2 fields per record. |
| `test_return_multiple_rows` | Canned 100 rows. `PULL {n: 10}` → 10 records + `has_more: true`. Another PULL → remaining. |
| `test_pull_all` | `PULL {n: -1}` → all records in one batch. |
| `test_discard` | RUN → DISCARD → no records sent, SUCCESS with stats. |
| `test_pull_from_different_connection` | RUN on conn A, PULL with matching `qid` on conn B. Verifies QueryBuffer is connection-agnostic. |

**C. Value type serialization**

Each test uses a canned query that returns a specific value type, then verifies the driver deserializes it correctly.

| Test | Canned response | Driver verification |
|------|----------------|-------------------|
| `test_value_null` | `RETURN null AS n` | `result["n"] is None` |
| `test_value_bool` | `RETURN true AS b` | `result["b"] == True` |
| `test_value_int_tiny` | `RETURN 42 AS n` | `result["n"] == 42` |
| `test_value_int_large` | `RETURN 9999999999 AS n` | Verify large int round-trip |
| `test_value_float` | `RETURN 3.14 AS f` | `result["f"] == 3.14` |
| `test_value_string` | `RETURN 'hello' AS s` | `result["s"] == "hello"` |
| `test_value_string_unicode` | `RETURN '日本語' AS s` | UTF-8 round-trip |
| `test_value_list` | `RETURN [1,2,3] AS l` | `result["l"] == [1, 2, 3]` |
| `test_value_map` | `RETURN {a:1, b:2} AS m` | `result["m"] == {"a": 1, "b": 2}` |
| `test_value_node` | Canned Node(id=1, labels=["Person"], props={name:"Alice"}) | Driver receives a Node object with correct fields |
| `test_value_relationship` | Canned Relationship(id=1, start=1, end=2, type="KNOWS", props={}) | Driver receives Relationship with correct start/end |
| `test_value_path` | Canned Path with 3 nodes, 2 relationships | Driver receives Path, can traverse nodes/rels |
| `test_value_point2d` | Canned Point2D(srid=4326, x=1.5, y=2.5) | Driver receives spatial Point |
| `test_value_datetime` | Canned DateTime struct | Driver receives datetime |
| `test_value_duration` | Canned Duration struct | Driver receives duration |
| `test_value_nested` | List of Maps containing Nodes | Deep nesting round-trip |

**D. Error handling and recovery**

| Test | What it verifies |
|------|-----------------|
| `test_syntax_error` | Unknown query → FAILURE with error code. Driver receives ClientException. |
| `test_failed_state_ignores` | After FAILURE, send RUN → IGNORED. Then RESET → SUCCESS, state recovered. |
| `test_reset_during_streaming` | RUN → partial PULL → RESET → SUCCESS. Connection is clean for new queries. |
| `test_reset_with_pipelined_messages` | Driver pipelines RUN + PULL + RESET. First two get IGNORED, RESET succeeds. |
| `test_buffer_expiry_timer` | RUN → buffer records → wait >10s without PULL → buffer is freed. Next PULL → FAILURE (unknown qid). |

**E. Transactions**

| Test | What it verifies |
|------|-----------------|
| `test_explicit_transaction` | BEGIN → RUN → PULL → COMMIT. Driver receives results within transaction. |
| `test_transaction_rollback` | BEGIN → RUN → ROLLBACK. No side effects. |
| `test_transaction_error` | BEGIN → RUN (error) → FAILURE → RESET → recovered. |

**F. Transport variants**

| Test | What it verifies |
|------|-----------------|
| `test_tcp_connection` | Standard TCP bolt:// connection works. |
| `test_websocket_connection` | WebSocket bolt+s:// / ws connection works (HTTP upgrade, framing). |
| `test_large_message` | Query with >64KB parameter string. Verifies multi-chunk handling. |

**G. Driver compatibility matrix**

Run the same test suite against multiple official Neo4j drivers:

| Driver | Package | Connection URI |
|--------|---------|---------------|
| Python | `neo4j` (pip) | `bolt://localhost:{port}` |
| JavaScript | `neo4j-driver` (npm) | `bolt://localhost:{port}` |
| Java | `org.neo4j.driver:neo4j-java-driver` (maven) | `bolt://localhost:{port}` |
| Go | `github.com/neo4j/neo4j-go-driver` | `bolt://localhost:{port}` |

The Python driver is the primary test driver (easiest to script). Other drivers are tested in CI to catch driver-specific protocol behavior.

```python
# tests/drivers/python/test_mock_server.py
import pytest
from neo4j import GraphDatabase

@pytest.fixture
def driver(mock_server_port):
    d = GraphDatabase.driver(f"bolt://localhost:{mock_server_port}", auth=("test", "test"))
    yield d
    d.close()

def test_return_scalar(driver):
    with driver.session() as session:
        result = session.run("RETURN 1 AS n")
        record = result.single()
        assert record["n"] == 1

def test_return_node(driver):
    with driver.session() as session:
        result = session.run("RETURN_NODE")  # canned query key
        record = result.single()
        node = record["n"]
        assert node.id == 1
        assert "Person" in node.labels
        assert node["name"] == "Alice"

def test_error_recovery(driver):
    with driver.session() as session:
        with pytest.raises(Exception):
            session.run("INVALID_QUERY").consume()
        # Connection should recover — next query works
        result = session.run("RETURN 1 AS n")
        assert result.single()["n"] == 1
```

#### Running standalone tests

```bash
# Start mock server + run Python driver tests
cargo test --test integration    # Rust-side mock server tests
cd tests/drivers/python && pytest  # Driver-side tests (requires neo4j pip package)
```

### Level 3: FalkorDB Integration Tests (after integration)

After the crate is integrated into FalkorDB C or falkordb-rs-next-gen:

- Run FalkorDB's existing test suite — all existing tests must pass (no regression)
- Run falkordb-rs-next-gen's existing test suite — same
- Run the **same driver test suite from Level 2** but against the real FalkorDB server instead of the mock. This verifies that real graph queries produce correct Bolt responses.
- Neo4j Browser connects and can browse the graph
- Test with `SHOW DATABASES` / `SHOW DEFAULT DATABASE` compatibility responses
- Stress test: concurrent connections, large result sets, connection drops
