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
│   │   └── value.rs              # BoltValue enum
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

PackStream serialization is implemented manually (direct byte manipulation) via `PackStreamWriter`/`PackStreamReader`. No serde dependency. This matches the C implementation's approach, avoids data model mismatches (PackStream's tagged structs, positional fields, and multi-width integers don't map well to serde's model), and preserves the streaming writer pattern needed for the C FFI. Debug output uses `Debug`/`Display` trait implementations on `BoltValue` and message types.

---

### Layer 1: PackStream

#### `packstream/value.rs` - BoltValue enum

```rust
/// All values that can be represented in the Bolt protocol.
/// Maps to PackStream types + Bolt structure semantics.
pub enum BoltValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Bytes(Vec<u8>),
    String(String),
    List(Vec<BoltValue>),
    Map(Vec<(String, BoltValue)>),   // Ordered key-value pairs

    // Graph structures (Bolt structure tags)
    Node(BoltNode),
    Relationship(BoltRelationship),
    UnboundRelationship(BoltUnboundRelationship),
    Path(BoltPath),

    // Temporal structures
    Date(BoltDate),
    Time(BoltTime),
    LocalTime(BoltLocalTime),
    DateTime(BoltDateTime),
    DateTimeZoneId(BoltDateTimeZoneId),
    LocalDateTime(BoltLocalDateTime),
    Duration(BoltDuration),

    // Spatial structures
    Point2D(BoltPoint2D),
    Point3D(BoltPoint3D),
}

pub struct BoltNode {
    pub id: i64,
    pub labels: Vec<String>,
    pub properties: Vec<(String, BoltValue)>,
    pub element_id: String,
}

pub struct BoltRelationship {
    pub id: i64,
    pub start_node_id: i64,
    pub end_node_id: i64,
    pub rel_type: String,
    pub properties: Vec<(String, BoltValue)>,
    pub element_id: String,
    pub start_node_element_id: String,
    pub end_node_element_id: String,
}

pub struct BoltUnboundRelationship {
    pub id: i64,
    pub rel_type: String,
    pub properties: Vec<(String, BoltValue)>,
    pub element_id: String,
}

pub struct BoltPath {
    pub nodes: Vec<BoltNode>,
    pub relationships: Vec<BoltUnboundRelationship>,
    pub indices: Vec<i64>,
}

// Similar structs for temporal/spatial (fields matching Bolt spec)
```

#### `packstream/serialize.rs` - Writer

```rust
/// Writes PackStream-encoded data to a byte buffer.
/// Streaming API: call methods sequentially to build messages.
pub struct PackStreamWriter {
    buf: BytesMut,
}

impl PackStreamWriter {
    pub fn new() -> Self;
    pub fn write_null(&mut self);
    pub fn write_bool(&mut self, value: bool);
    pub fn write_int(&mut self, value: i64);      // Auto-selects TINY/8/16/32/64
    pub fn write_float(&mut self, value: f64);
    pub fn write_string(&mut self, value: &str);
    pub fn write_bytes(&mut self, value: &[u8]);
    pub fn write_list_header(&mut self, size: u32);
    pub fn write_map_header(&mut self, size: u32);
    pub fn write_struct_header(&mut self, tag: u8, size: u32);

    /// Write a complete BoltValue (recursive)
    pub fn write_value(&mut self, value: &BoltValue);

    pub fn into_bytes(self) -> BytesMut;
    pub fn as_bytes(&self) -> &[u8];
    pub fn clear(&mut self);
}
```

#### `packstream/deserialize.rs` - Reader

```rust
/// Reads PackStream-encoded data from a byte buffer.
pub struct PackStreamReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PackStreamReader<'a> {
    pub fn new(data: &'a [u8]) -> Self;
    pub fn read_value(&mut self) -> Result<BoltValue, PackStreamError>;
    pub fn read_null(&mut self) -> Result<(), PackStreamError>;
    pub fn read_bool(&mut self) -> Result<bool, PackStreamError>;
    pub fn read_int(&mut self) -> Result<i64, PackStreamError>;
    pub fn read_float(&mut self) -> Result<f64, PackStreamError>;
    pub fn read_string(&mut self) -> Result<&'a str, PackStreamError>;
    pub fn read_list_header(&mut self) -> Result<u32, PackStreamError>;
    pub fn read_map_header(&mut self) -> Result<u32, PackStreamError>;
    pub fn read_struct_header(&mut self) -> Result<(u8, u32), PackStreamError>;
    pub fn remaining(&self) -> usize;
}
```

---

### Layer 2: Protocol Messages

#### `protocol/message.rs`

```rust
/// Messages sent by the client.
/// Each variant has strongly-typed fields. Optional/version-gated fields use Option<T>.
/// The parser populates fields based on negotiated version.
pub enum BoltRequest {
    Hello(HelloMessage),
    Logon(LogonMessage),          // 5.1+
    Logoff,                       // 5.1+
    Run(RunMessage),
    Pull(PullMessage),
    Discard(DiscardMessage),
    Begin(BeginMessage),
    Commit,
    Rollback,
    Reset,
    Route(RouteMessage),
    Telemetry(TelemetryMessage),  // 5.4+
    Goodbye,
}

/// HELLO (0x01) - Connection initialization.
/// The parser reads the extra map and populates typed fields.
pub struct HelloMessage {
    pub user_agent: String,
    pub bolt_agent: Option<BoltAgent>,            // 5.3+
    pub routing: Option<BoltValue>,               // Optional routing context
    pub patch_bolt: Vec<String>,                   // Patch negotiation
    pub notification_filter: Option<NotificationFilter>,  // 5.2+
}

pub struct BoltAgent {
    pub product: String,
    pub platform: Option<String>,
    pub language: Option<String>,
    pub language_details: Option<String>,
}

/// LOGON (0x6A) - Authentication (5.1+).
pub struct LogonMessage {
    pub scheme: String,              // "basic", "bearer", "kerberos", "none"
    pub principal: Option<String>,   // Username (basic/kerberos)
    pub credentials: Option<String>, // Password/token
    pub realm: Option<String>,       // Multi-realm support
}

/// RUN (0x10) - Execute query.
pub struct RunMessage {
    pub query: String,
    pub parameters: Vec<(String, BoltValue)>,
    pub extra: RunExtra,
}

/// Typed extra fields for RUN/BEGIN.
/// Optional fields are None when not provided or when version doesn't support them.
pub struct RunExtra {
    pub bookmarks: Vec<String>,
    pub tx_timeout: Option<i64>,       // Milliseconds
    pub tx_metadata: Option<BoltValue>,
    pub mode: Option<String>,          // "r" or "w"
    pub db: Option<String>,            // Target database name
    pub imp_user: Option<String>,      // Impersonated user
    pub notification_filter: Option<NotificationFilter>,  // 5.2+
}

pub struct PullMessage { pub n: i64, pub qid: i64 }
pub struct DiscardMessage { pub n: i64, pub qid: i64 }
pub struct BeginMessage { pub extra: RunExtra }  // Same extra fields as RUN
pub struct RouteMessage {
    pub routing: BoltValue,
    pub bookmarks: Vec<String>,
    pub db: Option<String>,
}
pub struct TelemetryMessage { pub api: i64 }

/// Messages sent by the server.
/// Success/Failure have typed metadata for common response patterns.
pub enum BoltResponse {
    Success { metadata: Vec<(String, BoltValue)> },
    Failure { code: String, message: String },
    Ignored,
    Record { fields: Vec<BoltValue> },
}

/// Parsing: version-aware deserialization.
/// Each message parser reads the PackStream struct fields and maps them to typed fields.
/// Version-gated fields are skipped/ignored if the negotiated version doesn't support them.
impl BoltRequest {
    pub fn parse(reader: &mut PackStreamReader, version: BoltVersion) -> Result<Self, BoltError> {
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

/// Response serialization: version-aware.
/// The writer serializes typed response structs into PackStream, respecting version constraints.
impl BoltResponse {
    pub fn write(&self, writer: &mut PackStreamWriter, version: BoltVersion) { /* ... */ }
}
```

Parsing: `BoltRequest::parse(reader: &mut PackStreamReader) -> Result<BoltRequest>` reads the struct tag and dispatches to the correct variant.

Serialization: `BoltResponse::write(writer: &mut PackStreamWriter)` writes the struct tag + fields.

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

impl BoltState {
    /// Given current state + request type + response type, return next state.
    /// Returns Err if the transition is invalid (protocol violation).
    pub fn transition(
        &self,
        request: &BoltRequest,
        response_type: ResponseType,
    ) -> Result<BoltState, BoltProtocolError>;
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
pub trait BoltHandler: Send + Sync {
    /// Called on HELLO message. Return metadata map for SUCCESS response.
    fn hello(
        &self,
        conn: &mut BoltConnection,
        extra: &BoltValue,
    ) -> Result<Vec<(String, BoltValue)>, BoltError>;

    /// Called on LOGON message. Return Ok(()) for success, Err for failure.
    fn authenticate(
        &self,
        conn: &mut BoltConnection,
        auth: &BoltValue,
    ) -> Result<(), BoltError>;

    /// Called on RUN message. Should execute the query and prepare results.
    /// Return SUCCESS metadata (fields, t_first, qid).
    fn run(
        &self,
        conn: &mut BoltConnection,
        query: &str,
        parameters: &[(String, BoltValue)],
        extra: &[(String, BoltValue)],
    ) -> Result<Vec<(String, BoltValue)>, BoltError>;

    /// PULL is handled internally by the crate - it drains pre-serialized
    /// RECORDs from the MessageBuffer. No host callback needed.

    /// DISCARD is handled internally by the crate - it discards buffered
    /// RECORDs without sending them. No host callback needed.

    /// Called on BEGIN message.
    fn begin(
        &self,
        conn: &mut BoltConnection,
        extra: &[(String, BoltValue)],
    ) -> Result<(), BoltError>;

    /// Called on COMMIT.
    fn commit(&self, conn: &mut BoltConnection) -> Result<(), BoltError>;

    /// Called on ROLLBACK.
    fn rollback(&self, conn: &mut BoltConnection) -> Result<(), BoltError>;

    /// Called on ROUTE message.
    fn route(
        &self,
        conn: &mut BoltConnection,
        routing: &BoltValue,
        bookmarks: &[String],
        extra: &[(String, BoltValue)],
    ) -> Result<Vec<(String, BoltValue)>, BoltError>;

    /// Called on RESET message.
    fn reset(&self, conn: &mut BoltConnection) -> Result<(), BoltError>;

    /// Called on GOODBYE (connection closing).
    fn goodbye(&self, conn: &mut BoltConnection);
}
```

#### `server/connection.rs` - Per-Connection State

```rust
/// Represents a single Bolt client connection.
/// Owns the state machine, buffers, and provides methods to write responses.
pub struct BoltConnection {
    state: BoltState,
    version: BoltVersion,
    is_websocket: bool,
    write_buf: BytesMut,
    read_buf: BytesMut,
    chunk_decoder: ChunkDecoder,
    writer: PackStreamWriter,
    user_data: *mut c_void,  // Opaque pointer for host app (e.g., RedisModuleCtx)
}

impl BoltConnection {
    /// Write a SUCCESS response with metadata.
    pub fn write_success(&mut self, metadata: &[(String, BoltValue)]);

    /// Write a FAILURE response.
    pub fn write_failure(&mut self, code: &str, message: &str);

    /// Write a RECORD response (one result row).
    pub fn write_record(&mut self, fields: &[BoltValue]);

    /// Write an IGNORED response.
    pub fn write_ignored(&mut self);

    /// End current message (write zero-chunk terminator).
    pub fn end_message(&mut self);

    /// Get raw bytes ready to send to the socket.
    pub fn take_write_bytes(&mut self) -> BytesMut;

    /// Feed raw bytes received from socket. Returns parsed requests.
    pub fn feed_data(&mut self, data: &[u8]) -> Result<Vec<BoltRequest>, BoltError>;

    /// Process a single request through the handler, updating state.
    pub fn process_request(
        &mut self,
        request: BoltRequest,
        handler: &dyn BoltHandler,
    ) -> Result<(), BoltError>;

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

**Principle**: The Bolt crate has NO knowledge of graphs, schemas, or IDs. It receives fully-resolved `BoltValue`s from the host app. The host is responsible for resolving bare IDs into full Bolt entities.

#### Bolt expects fully-materialized compound objects:

```
Node(0x4E, 4 fields):
  id: Integer                        (e.g., 42)
  labels: List<String>               (e.g., ["Person", "Employee"])
  properties: Map<String, Value>     (e.g., {"name": "Alice", "age": 30})
  element_id: String                 (e.g., "node_42")

Relationship(0x52, 8 fields):
  id: Integer                        (e.g., 7)
  start_node_id: Integer             (e.g., 42)
  end_node_id: Integer               (e.g., 99)
  type: String                       (e.g., "KNOWS")
  properties: Map<String, Value>     (e.g., {"since": 2020})
  element_id: String                 (e.g., "relationship_7")
  start_node_element_id: String      (e.g., "node_42")
  end_node_element_id: String        (e.g., "node_99")

UnboundRelationship(0x72, 4 fields):  (used inside Path)
  id: Integer
  type: String
  properties: Map<String, Value>
  element_id: String

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

#### Resolution responsibility: HOST resolves, crate receives `BoltValue`

The host app (FalkorDB C or Rust) resolves IDs before passing values to the Bolt crate:

**In FalkorDB Rust (next-gen)** - `value_to_bolt()` in `src/bolt.rs`:
```rust
fn value_to_bolt(runtime: &Runtime, value: Value) -> BoltValue {
    match value {
        Value::Node(id) => {
            let g = runtime.g.borrow();
            // Check if node was deleted during this query
            let dn = runtime.deleted_nodes.borrow();
            if let Some(deleted) = dn.get(&id) {
                BoltValue::Node(BoltNode {
                    id: u64::from(id) as i64,
                    labels: deleted.labels.iter()
                        .map(|lid| g.get_label_name(*lid).to_string()).collect(),
                    properties: deleted.attrs.iter()
                        .map(|(k, v)| (k.to_string(), value_to_bolt(runtime, v.clone()))).collect(),
                    element_id: format!("node_{}", u64::from(id)),
                })
            } else {
                BoltValue::Node(BoltNode {
                    id: u64::from(id) as i64,
                    labels: g.get_node_labels(id).map(|s| s.to_string()).collect(),
                    properties: g.get_node_all_attrs(id).iter()
                        .map(|(k, v)| (k.to_string(), value_to_bolt(runtime, v.clone()))).collect(),
                    element_id: format!("node_{}", u64::from(id)),
                })
            }
        }
        Value::Relationship(rel) => {
            let (rel_id, src, dst) = *rel;
            let g = runtime.g.borrow();
            let dr = runtime.deleted_relationships.borrow();
            let (type_name, props) = if let Some(deleted) = dr.get(&rel_id) {
                (g.get_type_name(deleted.type_id).to_string(),
                 deleted.attrs.iter().map(|(k,v)| (k.to_string(), value_to_bolt(runtime, v.clone()))).collect())
            } else {
                let tid = g.get_relationship_type_id(rel_id);
                (g.get_type_name(tid).to_string(),
                 g.get_relationship_all_attrs(rel_id).iter()
                    .map(|(k,v)| (k.to_string(), value_to_bolt(runtime, v.clone()))).collect())
            };
            BoltValue::Relationship(BoltRelationship {
                id: u64::from(rel_id) as i64,
                start_node_id: u64::from(src) as i64,
                end_node_id: u64::from(dst) as i64,
                rel_type: type_name,
                properties: props,
                element_id: format!("relationship_{}", u64::from(rel_id)),
                start_node_element_id: format!("node_{}", u64::from(src)),
                end_node_element_id: format!("node_{}", u64::from(dst)),
            })
        }
        Value::Path(values) => {
            // Convert alternating [Node, Rel, Node, Rel, ..., Node] into Bolt Path
            let mut nodes = Vec::new();
            let mut rels = Vec::new();
            let mut indices = Vec::new();
            for (i, v) in values.iter().enumerate() {
                if i % 2 == 0 {
                    // Node
                    if let BoltValue::Node(n) = value_to_bolt(runtime, v.clone()) {
                        nodes.push(n);
                    }
                } else {
                    // Relationship - convert to UnboundRelationship for Path
                    if let BoltValue::Relationship(r) = value_to_bolt(runtime, v.clone()) {
                        let node_before = &nodes[nodes.len() - 1];
                        let idx = rels.len() as i64 + 1;
                        // Direction: positive if forward (src matches prev node), negative if backward
                        if r.start_node_id == node_before.id {
                            indices.push(idx);
                        } else {
                            indices.push(-idx);
                        }
                        indices.push(nodes.len() as i64); // index of next node
                        rels.push(BoltUnboundRelationship {
                            id: r.id,
                            rel_type: r.rel_type,
                            properties: r.properties,
                            element_id: r.element_id,
                        });
                    }
                }
            }
            BoltValue::Path(BoltPath { nodes, relationships: rels, indices })
        }
        // Scalars pass through directly
        Value::Null => BoltValue::Null,
        Value::Bool(b) => BoltValue::Boolean(b),
        Value::Int(i) => BoltValue::Integer(i),
        Value::Float(f) => BoltValue::Float(f),
        Value::String(s) => BoltValue::String(s.to_string()),
        Value::List(l) => BoltValue::List(l.iter().map(|v| value_to_bolt(runtime, v.clone())).collect()),
        Value::Map(m) => BoltValue::Map(m.iter().map(|(k,v)| (k.to_string(), value_to_bolt(runtime, v.clone()))).collect()),
        Value::Point(p) => BoltValue::Point2D(BoltPoint2D { srid: 4326, x: p.longitude as f64, y: p.latitude as f64 }),
        Value::VecF32(v) => BoltValue::List(v.iter().map(|f| BoltValue::Float(*f as f64)).collect()),
        // Temporal types
        Value::Datetime(ts) => BoltValue::DateTime(BoltDateTime { seconds: ts, nanoseconds: 0, tz_offset: 0 }),
        Value::Date(d) => BoltValue::Date(BoltDate { days_since_epoch: d }),
        Value::Time(t) => BoltValue::Time(BoltTime { nanoseconds: t, tz_offset: 0 }),
        Value::Duration(d) => BoltValue::Duration(BoltDuration { months: 0, days: 0, seconds: d, nanoseconds: 0 }),
        _ => BoltValue::Null,
    }
}
```

**In FalkorDB C** - the existing `resultset_replybolt.c` already does this resolution. The pattern stays the same but calls the Rust FFI:
```c
// Current C code in _ResultSet_BoltReplyWithNode already resolves:
// - Labels via NODE_GET_LABELS() + Schema_GetName()
// - Properties via GraphEntity_GetAttributes() + AttributeSet iteration
// - element_id via sprintf("node_%lu", id)
// This logic stays in C, but calls bolt_reply_node() from the Rust crate
```

**Then the Bolt crate just serializes `BoltValue` to PackStream:**
```rust
// Inside PackStreamWriter - no graph knowledge needed
fn write_bolt_node(writer: &mut PackStreamWriter, node: &BoltNode) {
    writer.write_struct_header(0x4E, 4);
    writer.write_int(node.id);
    writer.write_list_header(node.labels.len() as u32);
    for label in &node.labels { writer.write_string(label); }
    writer.write_map_header(node.properties.len() as u32);
    for (k, v) in &node.properties { writer.write_string(k); writer.write_value(v); }
    writer.write_string(&node.element_id);
}
```

---

### Layer 4: C FFI API

#### `ffi/c_api.rs`

```rust
// --- Opaque handle types ---
pub type BoltConnectionHandle = *mut BoltConnection;
pub type BoltValueHandle = *mut BoltValue;

// --- Callback function pointer types for C ---
pub type BoltAuthCallback = extern "C" fn(
    conn: BoltConnectionHandle,
    scheme: *const c_char, scheme_len: u32,
    principal: *const c_char, principal_len: u32,
    credentials: *const c_char, credentials_len: u32,
    user_data: *mut c_void,
) -> bool;

pub type BoltRunCallback = extern "C" fn(
    conn: BoltConnectionHandle,
    query: *const c_char, query_len: u32,
    params_buf: *const u8, params_len: u32,  // PackStream-encoded parameters
    extra_buf: *const u8, extra_len: u32,    // PackStream-encoded extras
    user_data: *mut c_void,
);

pub type BoltBeginCallback = extern "C" fn(
    conn: BoltConnectionHandle,
    extra_buf: *const u8, extra_len: u32,
    user_data: *mut c_void,
);

pub type BoltCommitCallback = extern "C" fn(conn: BoltConnectionHandle, user_data: *mut c_void);
pub type BoltRollbackCallback = extern "C" fn(conn: BoltConnectionHandle, user_data: *mut c_void);

// --- Server lifecycle ---
#[no_mangle] pub extern "C" fn bolt_server_listen(port: u16) -> i32;
#[no_mangle] pub extern "C" fn bolt_server_accept(listen_fd: i32) -> BoltConnectionHandle;

// --- Connection lifecycle ---
#[no_mangle] pub extern "C" fn bolt_connection_feed_data(
    conn: BoltConnectionHandle, data: *const u8, len: u32
) -> i32;  // returns number of requests parsed
#[no_mangle] pub extern "C" fn bolt_connection_process(
    conn: BoltConnectionHandle, user_data: *mut c_void
) -> i32;
#[no_mangle] pub extern "C" fn bolt_connection_get_write_data(
    conn: BoltConnectionHandle, out_ptr: *mut *const u8, out_len: *mut u32
);
#[no_mangle] pub extern "C" fn bolt_connection_free(conn: BoltConnectionHandle);

// --- Set callbacks ---
#[no_mangle] pub extern "C" fn bolt_set_auth_callback(cb: BoltAuthCallback);
#[no_mangle] pub extern "C" fn bolt_set_run_callback(cb: BoltRunCallback);
#[no_mangle] pub extern "C" fn bolt_set_begin_callback(cb: BoltBeginCallback);
#[no_mangle] pub extern "C" fn bolt_set_commit_callback(cb: BoltCommitCallback);
#[no_mangle] pub extern "C" fn bolt_set_rollback_callback(cb: BoltRollbackCallback);

// --- Reply helpers (PackStream value writers, used inside buffer or immediate writes) ---
#[no_mangle] pub extern "C" fn bolt_reply_null(conn: BoltConnectionHandle);
#[no_mangle] pub extern "C" fn bolt_reply_bool(conn: BoltConnectionHandle, value: bool);
#[no_mangle] pub extern "C" fn bolt_reply_int(conn: BoltConnectionHandle, value: i64);
#[no_mangle] pub extern "C" fn bolt_reply_float(conn: BoltConnectionHandle, value: f64);
#[no_mangle] pub extern "C" fn bolt_reply_string(conn: BoltConnectionHandle, data: *const c_char, len: u32);
#[no_mangle] pub extern "C" fn bolt_reply_list(conn: BoltConnectionHandle, size: u32);
#[no_mangle] pub extern "C" fn bolt_reply_map(conn: BoltConnectionHandle, size: u32);
#[no_mangle] pub extern "C" fn bolt_reply_structure(conn: BoltConnectionHandle, tag: u8, size: u32);

// --- Message-level helpers (immediate writes to client) ---
#[no_mangle] pub extern "C" fn bolt_reply_success(conn: BoltConnectionHandle, metadata_count: u32);
#[no_mangle] pub extern "C" fn bolt_reply_failure(conn: BoltConnectionHandle, code: *const c_char, msg: *const c_char);
#[no_mangle] pub extern "C" fn bolt_end_message(conn: BoltConnectionHandle);
#[no_mangle] pub extern "C" fn bolt_flush_immediate(conn: BoltConnectionHandle);

// --- Buffer API (for ResultSetFormatter to buffer RECORDs during execution) ---
#[no_mangle] pub extern "C" fn bolt_buffer_record_begin(conn: BoltConnectionHandle, field_count: u32);
#[no_mangle] pub extern "C" fn bolt_buffer_record_end(conn: BoltConnectionHandle);
#[no_mangle] pub extern "C" fn bolt_buffer_stats_begin(conn: BoltConnectionHandle);
#[no_mangle] pub extern "C" fn bolt_buffer_complete(conn: BoltConnectionHandle);
#[no_mangle] pub extern "C" fn bolt_buffer_error(
    conn: BoltConnectionHandle, code: *const c_char, code_len: u32,
    message: *const c_char, message_len: u32,
);
// PULL is handled internally by the crate (drains from buffer). No pull callback needed.

// --- Read helpers (for parsing parameters in RUN callback) ---
#[no_mangle] pub extern "C" fn bolt_read_type(data: *const u8) -> i32;
#[no_mangle] pub extern "C" fn bolt_read_int_value(data: *mut *const u8) -> i64;
#[no_mangle] pub extern "C" fn bolt_read_string_value(data: *mut *const u8, out_len: *mut u32) -> *const c_char;
// ... etc. matching current C API patterns in FalkorDB
```

The C API is designed to be a **near drop-in replacement** for the current `bolt_*` functions in FalkorDB's `src/bolt/bolt.h`, with the addition of callback registration. A `cbindgen`-generated header file will be produced at build time.

---

### C API Usage Examples

**Design principle**: The graph engine (execution layer) emits rows via the `ResultSetFormatter` interface. The Bolt formatter (`resultset_replybolt.c`) calls `bolt_reply_*` FFI functions to serialize values into the per-connection `MessageBuffer`. The PULL handler in the Rust crate simply drains pre-serialized records from the buffer and writes them to the socket. **The PULL callback does NOT access the graph.**

```
Execution Engine                   ResultSetFormatter (Bolt)           Rust Crate
─────────────────                  ─────────────────────────           ──────────
plan ready         ──EmitHeader──► bolt_flush_immediate(conn)     ──► send RUN SUCCESS
  │                                  (fields, qid)                    immediately
  ▼
for each row       ──EmitRow────► bolt_buffer_record_begin(conn)  ──► buffer RECORD
  │                                 bolt_reply_*(conn, ...)             (pre-serialized)
  │                                 bolt_buffer_record_end(conn)
  ▼
execution done     ──EmitStats──► bolt_buffer_complete(conn)      ──► store stats
                                                                       mark complete
                                    ─── later ───
                                  PULL {n} arrives                ──► bolt_pull_from_buffer()
                                                                       drain N records
                                                                       send to client
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
    BoltConnectionHandle conn = bolt_server_accept(fd);
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
    BoltConnectionHandle conn = (BoltConnectionHandle)user_data;

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
    BoltConnectionHandle conn,
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

The RUN callback starts query execution. The execution engine uses the Bolt `ResultSetFormatter` to emit results into the connection's message buffer. The PULL handler (inside the crate) drains from that buffer later.

```c
// bolt_bridge.c - RUN callback implementation
void falkordb_run_handler(
    BoltConnectionHandle conn,
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
    //      EmitHeader → bolt_flush_immediate(conn)     // sends RUN SUCCESS to client
    //      EmitRow    → bolt_buffer_record_*(conn)     // buffers RECORD in MessageBuffer
    //      EmitStats  → bolt_buffer_complete(conn)     // stores stats, marks done
    //
    //    All graph access (node labels, properties, etc.) happens HERE during
    //    execution, NOT during PULL. The formatter resolves entities and
    //    serializes them into pre-built PackStream bytes in the buffer.
    ExecuteQuery(gc, query, params, conn);
}
```

#### Example 4: ResultSetFormatter - EmitHeader (sends RUN SUCCESS)

```c
// resultset_replybolt.c - called by execution engine when plan is ready
void ResultSet_ReplyWithBoltHeader(ResultSet *set) {
    BoltConnectionHandle conn = set->bolt_client;

    // Send RUN SUCCESS immediately with column names
    bolt_reply_success(conn, 3);
      bolt_reply_string(conn, "t_first", 7);
      bolt_reply_int(conn, 0);
      bolt_reply_string(conn, "fields", 6);
      bolt_reply_list(conn, set->column_count);
      for (uint i = 0; i < set->column_count; i++) {
          bolt_reply_string(conn, set->columns[i], strlen(set->columns[i]));
      }
      bolt_reply_string(conn, "qid", 3);
      bolt_reply_int(conn, 0);
    bolt_end_message(conn);
    bolt_flush_immediate(conn);  // send to client NOW (not buffered)
}
```

#### Example 5: ResultSetFormatter - EmitRow (buffers RECORD)

This is the key function. Each result row is serialized into a RECORD message and appended to the MessageBuffer. The graph engine calls this during execution - all entity resolution happens here.

```c
// resultset_replybolt.c - called for EACH result row during execution
void ResultSet_EmitBoltRow(ResultSet *set, SIValue **row) {
    BoltConnectionHandle conn = set->bolt_client;
    GraphContext *gc = QueryCtx_GetGraphCtx();

    // Begin a new RECORD in the buffer
    bolt_buffer_record_begin(conn, set->column_count);

    // Serialize each column value into the RECORD
    for (uint i = 0; i < set->column_count; i++) {
        bolt_reply_si_value(conn, gc, *row[i]);
    }

    // Finalize the RECORD in the buffer
    bolt_buffer_record_end(conn);
}
```

#### Example 6: bolt_reply_si_value - Recursive Value Serialization

This helper resolves FalkorDB types into Bolt PackStream format. For compound types (Node, Edge, Path), it resolves all data from the graph. This is the **only place** where graph access happens during serialization.

```c
// resultset_replybolt.c (host app code, NOT in the crate)
void bolt_reply_si_value(BoltConnectionHandle conn, GraphContext *gc, SIValue v) {
    switch (SI_TYPE(v)) {
    case T_NULL:
        bolt_reply_null(conn);
        break;
    case T_BOOL:
        bolt_reply_bool(conn, v.longval);
        break;
    case T_INT64:
        bolt_reply_int(conn, v.longval);
        break;
    case T_DOUBLE:
        bolt_reply_float(conn, v.doubleval);
        break;
    case T_STRING:
    case T_INTERN_STRING:
        bolt_reply_string(conn, v.stringval, strlen(v.stringval));
        break;

    case T_NODE: {
        // --- Resolve node from graph ---
        Node *node = v.ptrval;
        uint lbls_count;
        NODE_GET_LABELS(gc->g, node, lbls_count);
        const AttributeSet set = GraphEntity_GetAttributes((GraphEntity *)node);
        int prop_count = AttributeSet_Count(set);

        // Write Node structure: tag=0x4E, 4 fields
        bolt_reply_structure(conn, 0x4E, 4);
          bolt_reply_int(conn, node->id);
          bolt_reply_list(conn, lbls_count);
          for (int i = 0; i < lbls_count; i++) {
              Schema *s = GraphContext_GetSchemaByID(gc, labels[i], SCHEMA_NODE);
              bolt_reply_string(conn, Schema_GetName(s), strlen(Schema_GetName(s)));
          }
          bolt_reply_map(conn, prop_count);
          for (int i = 0; i < prop_count; i++) {
              SIValue val; AttributeID attr_id;
              AttributeSet_GetIdx(set, i, &attr_id, &val);
              const char *key = GraphContext_GetAttributeString(gc, attr_id);
              bolt_reply_string(conn, key, strlen(key));
              bolt_reply_si_value(conn, gc, val);  // recursive
          }
          char eid[32];
          int eid_len = sprintf(eid, "node_%" PRIu64, node->id);
          bolt_reply_string(conn, eid, eid_len);
        break;
    }

    case T_EDGE: {
        // --- Resolve edge from graph ---
        Edge *edge = v.ptrval;
        Schema *s = GraphContext_GetSchemaByID(gc, Edge_GetRelationID(edge), SCHEMA_EDGE);
        const char *type = Schema_GetName(s);
        const AttributeSet set = GraphEntity_GetAttributes((GraphEntity *)edge);
        int prop_count = AttributeSet_Count(set);

        // Write Relationship structure: tag=0x52, 8 fields
        bolt_reply_structure(conn, 0x52, 8);
          bolt_reply_int(conn, edge->id);
          bolt_reply_int(conn, edge->src_id);
          bolt_reply_int(conn, edge->dest_id);
          bolt_reply_string(conn, type, strlen(type));
          bolt_reply_map(conn, prop_count);
          for (int i = 0; i < prop_count; i++) {
              SIValue val; AttributeID attr_id;
              AttributeSet_GetIdx(set, i, &attr_id, &val);
              const char *key = GraphContext_GetAttributeString(gc, attr_id);
              bolt_reply_string(conn, key, strlen(key));
              bolt_reply_si_value(conn, gc, val);
          }
          char eid[48];
          int eid_len = sprintf(eid, "relationship_%" PRIu64, edge->id);
          bolt_reply_string(conn, eid, eid_len);
          eid_len = sprintf(eid, "node_%" PRIu64, edge->src_id);
          bolt_reply_string(conn, eid, eid_len);
          eid_len = sprintf(eid, "node_%" PRIu64, edge->dest_id);
          bolt_reply_string(conn, eid, eid_len);
        break;
    }

    case T_PATH: {
        // --- Resolve path (nodes + edges) from graph ---
        size_t node_count = SIPath_NodeCount(v);
        size_t edge_count = SIPath_EdgeCount(v);

        // Path structure: tag=0x50, 3 fields
        bolt_reply_structure(conn, 0x50, 3);

          // Field 1: nodes
          bolt_reply_list(conn, node_count);
          for (int i = 0; i < node_count; i++) {
              SIValue n = SIPath_GetNode(v, i);
              bolt_reply_si_value(conn, gc, n);  // recurses into T_NODE case
          }

          // Field 2: rels (UnboundRelationship: tag=0x72, 4 fields)
          bolt_reply_list(conn, edge_count);
          for (int i = 0; i < edge_count; i++) {
              Edge *e = SIPath_GetRelationship(v, i).ptrval;
              Schema *s = GraphContext_GetSchemaByID(gc, Edge_GetRelationID(e), SCHEMA_EDGE);
              const AttributeSet eset = GraphEntity_GetAttributes((GraphEntity *)e);
              int pc = AttributeSet_Count(eset);
              bolt_reply_structure(conn, 0x72, 4);
                bolt_reply_int(conn, e->id);
                bolt_reply_string(conn, Schema_GetName(s), strlen(Schema_GetName(s)));
                bolt_reply_map(conn, pc);
                for (int j = 0; j < pc; j++) {
                    SIValue val; AttributeID attr_id;
                    AttributeSet_GetIdx(eset, j, &attr_id, &val);
                    const char *key = GraphContext_GetAttributeString(gc, attr_id);
                    bolt_reply_string(conn, key, strlen(key));
                    bolt_reply_si_value(conn, gc, val);
                }
                char eid[48];
                int eid_len = sprintf(eid, "relationship_%" PRIu64, e->id);
                bolt_reply_string(conn, eid, eid_len);
          }

          // Field 3: indices (traversal order)
          bolt_reply_list(conn, edge_count * 2);
          for (int i = 0; i < edge_count; i++) {
              Edge *e = SIPath_GetRelationship(v, i).ptrval;
              Node *prev = SIPath_GetNode(v, i).ptrval;
              if (e->src_id == prev->id) {
                  bolt_reply_int(conn, i + 1);
              } else {
                  bolt_reply_int(conn, -(i + 1));
              }
              bolt_reply_int(conn, i + 1);
          }
        break;
    }

    case T_ARRAY:
        bolt_reply_list(conn, SIArray_Length(v));
        for (int i = 0; i < SIArray_Length(v); i++)
            bolt_reply_si_value(conn, gc, SIArray_Get(v, i));
        break;
    case T_MAP:
        bolt_reply_map(conn, Map_KeyCount(v));
        for (uint i = 0; i < Map_KeyCount(v); i++) {
            bolt_reply_si_value(conn, gc, v.map[i].key);
            bolt_reply_si_value(conn, gc, v.map[i].val);
        }
        break;
    case T_POINT:
        bolt_reply_structure(conn, 0x58, 3);  // Point2D
        bolt_reply_int(conn, 4326);
        bolt_reply_float(conn, v.point.longitude);
        bolt_reply_float(conn, v.point.latitude);
        break;
    case T_VECTOR_F32: {
        uint32_t dim = SIVector_Dim(v);
        bolt_reply_list(conn, dim);
        float *vals = (float *)SIVector_Elements(v);
        for (uint i = 0; i < dim; i++)
            bolt_reply_float(conn, (double)vals[i]);
        break;
    }
    }
}
```

#### Example 7: ResultSetFormatter - EmitStats (marks execution complete)

```c
// resultset_replybolt.c - called when execution finishes
void ResultSet_EmitBoltStats(ResultSet *set) {
    BoltConnectionHandle conn = set->bolt_client;

    // Store stats in the buffer (will be sent as final SUCCESS during PULL)
    bolt_buffer_stats_begin(conn);
    if (set->stats.nodes_created)        { bolt_reply_string(conn, "nodes-created", 13);        bolt_reply_int(conn, set->stats.nodes_created); }
    if (set->stats.nodes_deleted)        { bolt_reply_string(conn, "nodes-deleted", 13);        bolt_reply_int(conn, set->stats.nodes_deleted); }
    if (set->stats.relationships_created){ bolt_reply_string(conn, "relationships-created", 21); bolt_reply_int(conn, set->stats.relationships_created); }
    if (set->stats.relationships_deleted){ bolt_reply_string(conn, "relationships-deleted", 21); bolt_reply_int(conn, set->stats.relationships_deleted); }
    if (set->stats.properties_set)       { bolt_reply_string(conn, "properties-set", 14);       bolt_reply_int(conn, set->stats.properties_set); }
    if (set->stats.labels_added)         { bolt_reply_string(conn, "labels-added", 12);         bolt_reply_int(conn, set->stats.labels_added); }
    if (set->stats.labels_removed)       { bolt_reply_string(conn, "labels-removed", 14);       bolt_reply_int(conn, set->stats.labels_removed); }
    if (set->stats.indices_created)      { bolt_reply_string(conn, "indexes-added", 13);        bolt_reply_int(conn, set->stats.indices_created); }
    if (set->stats.indices_deleted)      { bolt_reply_string(conn, "indexes-removed", 15);      bolt_reply_int(conn, set->stats.indices_deleted); }
    bolt_buffer_complete(conn);  // marks execution_complete = true
}
```

#### Example 8: PULL - Handled Entirely by the Crate

PULL does NOT need a host callback. The crate handles it internally by draining from the MessageBuffer:

```rust
// Inside the crate (server/connection.rs) - NO graph access needed
fn handle_pull(&mut self, n: i64, _qid: i64) -> Result<(), BoltError> {
    let drained = self.message_buffer.drain(n);  // drain N pre-serialized RECORDs

    // Write drained records to the connection's write buffer
    for record_bytes in drained {
        self.write_buf.extend_from_slice(&record_bytes);
    }

    // Determine if there are more records
    if self.message_buffer.has_more() {
        self.write_success(&[("has_more".into(), BoltValue::Boolean(true))]);
    } else {
        // Final SUCCESS with stats (stored by bolt_buffer_complete)
        let stats = self.message_buffer.take_stats().unwrap_or_default();
        self.write_success(&stats);
    }
    self.end_message();
    Ok(())
}
```

This means `bolt_set_pull_callback` is NOT needed. PULL is internal to the crate.

#### Example 9: SHOW DATABASES Compatibility

```c
// Neo4j drivers call "SHOW DATABASES" on connect - must return hardcoded response.
// This is handled in the RUN callback as a special case BEFORE reaching the graph engine.
void falkordb_run_handler(BoltConnectionHandle conn, const char *query, ...) {
    if (strncmp(query, "SHOW DATABASES", 14) == 0) {
        // RUN SUCCESS with 13 column names
        bolt_reply_success(conn, 3);
          bolt_reply_string(conn, "t_first", 7);
          bolt_reply_int(conn, 0);
          bolt_reply_string(conn, "fields", 6);
          bolt_reply_list(conn, 13);
            bolt_reply_string(conn, "name", 4);
            bolt_reply_string(conn, "type", 4);
            bolt_reply_string(conn, "aliases", 7);
            bolt_reply_string(conn, "access", 6);
            bolt_reply_string(conn, "address", 7);
            bolt_reply_string(conn, "role", 4);
            bolt_reply_string(conn, "writer", 6);
            bolt_reply_string(conn, "requestedStatus", 15);
            bolt_reply_string(conn, "currentStatus", 13);
            bolt_reply_string(conn, "statusMessage", 13);
            bolt_reply_string(conn, "default", 7);
            bolt_reply_string(conn, "home", 4);
            bolt_reply_string(conn, "constituents", 12);
          bolt_reply_string(conn, "qid", 3);
          bolt_reply_int(conn, 0);
        bolt_end_message(conn);
        bolt_flush_immediate(conn);

        // Buffer one RECORD row
        bolt_buffer_record_begin(conn, 13);
          bolt_reply_string(conn, "falkordb", 8);
          bolt_reply_string(conn, "standard", 8);
          bolt_reply_list(conn, 0);
          bolt_reply_string(conn, "read-write", 10);
          bolt_reply_string(conn, "localhost:7687", 14);
          bolt_reply_string(conn, "primary", 7);
          bolt_reply_bool(conn, true);
          bolt_reply_string(conn, "online", 6);
          bolt_reply_string(conn, "online", 6);
          bolt_reply_string(conn, "", 0);
          bolt_reply_bool(conn, true);
          bolt_reply_bool(conn, true);
          bolt_reply_list(conn, 0);
        bolt_buffer_record_end(conn);
        bolt_buffer_complete(conn);  // marks done, PULL will drain
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
│  │  BoltConnection (per client)                                         │   │
│  │  ──────────────────────────                                          │   │
│  │  state: BoltState          message_buffer: MessageBuffer             │   │
│  │  version: BoltVersion      write_buf: BytesMut                       │   │
│  │  is_websocket: bool        user_data: *void                          │   │
│  └──────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  ┌──────────────────────────┐  ┌────────────────────────────────────────┐  │
│  │  BoltHandler trait       │  │  C FFI (extern "C")                    │  │
│  │  ────────────────        │  │  ──────────────────                    │  │
│  │  hello()                 │  │  bolt_server_listen()                  │  │
│  │  authenticate()          │  │  bolt_server_accept()                  │  │
│  │  run()                   │  │  bolt_connection_feed_data()           │  │
│  │  pull()                  │  │  bolt_reply_* (write functions)        │  │
│  │  begin/commit/rollback() │  │  bolt_buffer_record_begin/end()       │  │
│  │  route()                 │  │  bolt_set_*_callback()                │  │
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
│   value_to_bolt()         │    │   bolt_reply_si_value() helper            │
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
  │                                   │                                │─── parse query
  │                                   │                                │─── create plan
  │◄── SUCCESS {fields, qid} ───────│◄── Ok({fields, qid}) ─────────│
  │                                   │ state: STREAMING               │─── start execution
  │                                   │                                │─── EmitRow → buffer
  │                                   │                                │─── EmitRow → buffer
  │─── PULL {n: -1} ────────────────►│──── handler.pull() ───────────►│
  │◄── RECORD [val, val, ...] ──────│◄── drain buffer ───────────────│
  │◄── RECORD [val, val, ...] ──────│                                │
  │◄── SUCCESS {stats, t_last} ─────│◄── execution complete ────────│
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

```rust
pub struct MessageBuffer {
    records: VecDeque<BytesMut>,
    execution_complete: bool,
    final_stats: Option<Vec<(String, BoltValue)>>,
    /// Error encountered during execution (replaces final_stats)
    error: Option<BoltError>,
}
```

Three scenarios:

#### 1. Error during RUN (parse/plan error) - before execution starts

The handler returns Err from `run()`. The crate sends FAILURE immediately, no records are buffered.

```c
// In the run callback:
void my_run_handler(BoltConnectionHandle conn, const char *query, ...) {
    // Parse query
    if (parse_failed) {
        bolt_reply_failure(conn,
            "Neo.ClientError.Statement.SyntaxError",
            "Invalid input 'MTCH': expected 'MATCH'");
        bolt_end_message(conn);
        bolt_finish_write(conn);
        return;  // Connection moves to FAILED state
    }
    // ... normal execution ...
}
```

#### 2. Error during execution (runtime error) - after RUN SUCCESS already sent

The execution thread encounters an error mid-execution. It sets an error on the buffer instead of more records.

```c
// During execution, when an error is detected:
// (e.g., in ErrorCtx_EmitException or an error callback)
void bolt_buffer_error(BoltConnectionHandle conn,
                       const char *code, uint32_t code_len,
                       const char *message, uint32_t message_len);
// This stores the error and marks execution_complete = true.
// No more records will be buffered after this.
```

When PULL arrives:
```c
int bolt_pull_from_buffer(BoltConnectionHandle conn, int64_t n) {
    // 1. Drain any records that were buffered BEFORE the error
    //    (these represent valid partial results)
    // 2. If buffer has an error:
    //    - Send FAILURE {code, message} instead of final SUCCESS
    //    - Connection state → FAILED
    // 3. Client must send RESET to recover to READY state
}
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
void bolt_reply_failure(BoltConnectionHandle conn,
                        const char *code, uint32_t code_len,
                        const char *message, uint32_t message_len);

// Buffer an error during execution (for runtime errors in EmitRow/execution)
// After this call, no more records can be buffered.
// The error will be sent as FAILURE when PULL drains the buffer.
void bolt_buffer_error(BoltConnectionHandle conn,
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
    fn authenticate(&self, conn: &mut BoltConnection, auth: &BoltValue) -> Result<(), BoltError> {
        // Extract scheme/principal/credentials from auth map
        // Call Redis AUTH via the context if needed (pluggable)
    }

    fn run(&self, conn: &mut BoltConnection, query: &str, params: &[(String, BoltValue)], extra: &[(String, BoltValue)]) -> Result<Vec<(String, BoltValue)>, BoltError> {
        // 1. Extract graph name from extra["db"] (default "falkordb")
        // 2. Convert BoltValue params to FalkorDB params format
        // 3. Parse query, create plan, execute
        // 4. Store ResultSummary in connection state for PULL
        // 5. Return SUCCESS metadata with fields + qid
    }

    fn pull(&self, conn: &mut BoltConnection, n: i64, qid: i64) -> Result<(), BoltError> {
        // Iterate over stored ResultSummary rows
        // For each row, convert Value → BoltValue, write RECORD
        // Write final SUCCESS with stats
    }
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
                let fields: Vec<BoltValue> = active.column_names.iter()
                    .map(|name| value_to_bolt(&active.runtime, row.get(name).unwrap()))
                    .collect();
                conn.write_record(&fields);
                conn.end_message();
                count += 1;
            }
            None => break, // exhausted
        }
    }

    // Check if more rows
    let has_more = active.iterator.size_hint().1 != Some(0); // approximate
    if has_more {
        conn.write_success(&[("has_more".into(), BoltValue::Boolean(true))]);
    } else {
        // Final SUCCESS with stats
        conn.write_success(&stats_to_bolt_map(&active.stats));
    }
    conn.end_message();
}
```

**Impact on falkordb-rs-next-gen**: The current `Runtime::query()` returns `ResultSummary` (fully materialized `Vec<Env>`). To support true streaming, the runtime needs to expose an **iterator-based API** that yields rows one at a time. This is a change to `graph/src/runtime/runtime.rs` - the `run()` function already returns an iterator internally, we just need to avoid collecting it into a Vec.

---

### Buffered Reply Pattern (Decouples Execution from PULL)

The execution engine is protocol-agnostic. It writes pre-serialized RECORD messages into a per-connection buffer immediately during execution. PULL just drains from the buffer.

```
RUN arrives → SUCCESS (sent immediately with column names)
              ↓
         Execution thread starts
              ↓
         EmitRow(row1) → serialize RECORD → append to MessageBuffer
         EmitRow(row2) → serialize RECORD → append to MessageBuffer
         EmitRow(row3) → serialize RECORD → append to MessageBuffer
         EmitStats()   → store stats, mark execution_complete=true
              ...                        ↑
                                    PULL {n:2} arrives
                                         ↓
                           Drain 2 RECORDs from buffer → send to client
                           SUCCESS {has_more: true} → send to client
                                         ↑
                                    PULL {n:-1} arrives
                                         ↓
                           Drain remaining → send to client
                           SUCCESS {stats} → send to client (final)
```

#### MessageBuffer (per-connection, thread-safe)

```rust
pub struct MessageBuffer {
    records: VecDeque<BytesMut>,                   // Pre-serialized RECORD messages
    execution_complete: bool,                       // True when engine is done
    final_stats: Option<Vec<(String, BoltValue)>>,  // Set on completion
}
```

#### Buffer API (exposed via FFI for C)

```c
// Called by EmitRow (execution thread) - serializes RECORD into buffer
void bolt_buffer_record_begin(BoltConnectionHandle conn, uint32_t field_count);
// ... write fields via bolt_reply_* functions ...
void bolt_buffer_record_end(BoltConnectionHandle conn);

// Called by EmitStats (execution thread) - marks execution complete
void bolt_buffer_complete(BoltConnectionHandle conn);

// Called by PULL handler (main thread) - drains n records from buffer
// Returns number of records sent. Writes SUCCESS with has_more or final stats.
int bolt_pull_from_buffer(BoltConnectionHandle conn, int64_t n);

// Send RUN SUCCESS immediately (before execution starts)
void bolt_flush_immediate(BoltConnectionHandle conn);
```

#### PULL scenarios

| Scenario | Buffer state | Action |
|---|---|---|
| PULL before any rows buffered | Empty, execution running | Return SUCCESS {has_more: true}, 0 records |
| PULL with partial buffer | N records available, execution running | Drain min(n, N) records, SUCCESS {has_more: true} |
| PULL after execution complete | Records remaining | Drain, if last batch: SUCCESS {stats}, else: SUCCESS {has_more: true} |
| PULL, buffer empty, exec complete | Empty, done | SUCCESS {stats} (final, 0 records) |

#### Thread safety
Producer (execution thread) and consumer (PULL on main thread) access the buffer concurrently. Use a Mutex<VecDeque<BytesMut>> internally. Contention is low since producer appends and consumer pops from the front.

#### How FalkorDB C's ResultSetFormatter maps to this

```
EmitHeader() → bolt_flush_immediate(conn)         // Send RUN SUCCESS now
EmitRow()    → bolt_buffer_record_begin/end(conn)  // Buffer, don't send
EmitStats()  → bolt_buffer_complete(conn)          // Mark done, store stats
                                                    // PULL handler drains later
```

This means `ResultSet_ReplyWithBoltHeader`, `ResultSet_EmitBoltRow`, and `ResultSet_EmitBoltStats` in `resultset_replybolt.c` need only minor changes: replace direct write functions with buffer functions. The execution engine and `ResultSet_AddRecord` flow remain unchanged.

#### 4. Add `value_to_bolt()` conversion in `src/bolt.rs`

Convert `graph::runtime::value::Value` → `BoltValue`:

```rust
fn value_to_bolt(runtime: &Runtime, value: Value) -> BoltValue {
    match value {
        Value::Null => BoltValue::Null,
        Value::Bool(b) => BoltValue::Boolean(b),
        Value::Int(i) => BoltValue::Integer(i),
        Value::Float(f) => BoltValue::Float(f),
        Value::String(s) => BoltValue::String(s.to_string()),
        Value::List(l) => BoltValue::List(l.iter().map(|v| value_to_bolt(runtime, v.clone())).collect()),
        Value::Map(m) => BoltValue::Map(m.iter().map(|(k,v)| (k.to_string(), value_to_bolt(runtime, v.clone()))).collect()),
        Value::Node(id) => {
            let g = runtime.g.borrow();
            BoltValue::Node(BoltNode {
                id: u64::from(id) as i64,
                labels: g.get_node_label_ids(id).map(|lid| g.get_label_name(lid).to_string()).collect(),
                properties: g.get_node_attrs(id).iter().map(|key| {
                    (key.clone(), value_to_bolt(runtime, g.get_node_attribute(id, key).unwrap()))
                }).collect(),
                element_id: format!("node_{}", u64::from(id)),
            })
        }
        Value::Relationship(rel) => { /* similar */ }
        Value::Path(p) => { /* convert alternating nodes/rels */ }
        Value::Point(p) => BoltValue::Point2D(BoltPoint2D { srid: 4326, x: p.longitude, y: p.latitude }),
        // ... temporal types, VecF32
    }
}
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
4. `packstream/value.rs` - BoltValue enum and struct definitions
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

### Phase 4: C FFI
1. `ffi/c_api.rs` - All extern "C" functions
2. `build.rs` - cbindgen header generation
3. Test: compile and link from a C test program
4. Match reply API names to existing FalkorDB C bolt API for easy migration

### Phase 5: FalkorDB C Integration
1. Create `src/bolt_bridge.c` with callback implementations
2. Update CMakeLists.txt to link Rust static library
3. Replace `src/bolt/` calls with new FFI calls in `resultset_replybolt.c`
4. Update `bolt_api.c` event loop handlers → new bridge
5. Test: connect Neo4j Browser to FalkorDB via new Rust bolt

### Phase 6: falkordb-rs-next-gen Integration
1. Add `src/bolt.rs` with `FalkorBoltHandler` + `value_to_bolt()`
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

    /// Serialize a value, respecting version-specific struct layouts.
    fn write_node(&self, writer: &mut PackStreamWriter, node: &BoltNode) {
        if self.version.minor >= 0 {
            // 5.0+: 4 fields (id, labels, properties, element_id)
            writer.write_struct_header(0x4E, 4);
            // ...
            writer.write_string(&node.element_id);
        }
    }
}
```

#### Adding a new Bolt version (e.g., 6.0)

When Bolt 6.0 ships, the changes are:

1. **Update version negotiation** (`handshake.rs`): Add 6.0 to accepted version ranges
2. **Add new types** (`value.rs`): Add `BoltValue::Vector(BoltVector)`, `BoltValue::UnsupportedType(...)`
3. **Add new struct tags** (`marker.rs`): `VECTOR_TAG = 0x56`, `UNSUPPORTED_TYPE_TAG = 0x3F`
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

### Unit Tests
- PackStream round-trip for all types (null, bool, int variants, float, string, bytes, list, map, structs)
- Message parsing for all 13 request types
- State machine transitions (valid + invalid paths)
- Chunking encoder/decoder with various message sizes
- WebSocket frame encode/decode

### Integration Tests
- Connect with **Neo4j Python driver** (`neo4j` pip package):
  ```python
  from neo4j import GraphDatabase
  driver = GraphDatabase.driver("bolt://localhost:7687", auth=("falkordb", ""))
  with driver.session() as session:
      result = session.run("RETURN 1 AS n")
      print(result.single()["n"])
  ```
- Connect with **Neo4j JavaScript driver**
- Connect with **Neo4j Browser** (WebSocket transport)
- Test all value types: nodes, relationships, paths, integers, floats, strings, lists, maps, points
- Test transactions: BEGIN → RUN → PULL → COMMIT
- Test error handling: invalid queries → FAILURE response
- Test RESET after FAILURE
- Test ROUTE response (for driver routing)
- Test SHOW DATABASES compatibility (hardcoded response like current C impl)

### Compatibility
- Test with FalkorDB's existing test suite after integration
- Test with falkordb-rs-next-gen's existing test suite after integration
- Run Neo4j driver conformance tests where applicable
