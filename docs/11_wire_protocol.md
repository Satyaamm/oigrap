# PostgreSQL Wire Protocol

oigrap speaks the PostgreSQL frontend/backend protocol version 3 over TCP. This protocol is the public interface of the database. Any client that works with PostgreSQL works with oigrap without modification.

The wire layer is implemented in Go. It handles TCP connections, the protocol state machine, session management, and result encoding. It does not interpret SQL or touch storage — it routes between the network and the internal query engine.

---

## Protocol overview

The PostgreSQL wire protocol is a binary message protocol. Each message has:
- A single byte identifying the message type
- A 4-byte big-endian integer giving the total message length (including the 4 bytes of length itself)
- A variable-length payload

Exception: the startup message has no type byte.

```
Regular message format:
  [type: u8][length: u32 big-endian][payload: (length-4) bytes]

Startup message format:
  [length: u32 big-endian][protocol_version: u32][key=value pairs...]
```

---

## Connection lifecycle

```
Client                              Server
  |                                   |
  |-- TCP SYN ----------------------->|
  |<- TCP SYN-ACK --------------------|
  |-- TCP ACK ----------------------->|
  |                                   |
  |  (optional TLS negotiation)       |
  |-- SSLRequest (8 bytes) ---------->|
  |<- 'S' (TLS) or 'N' (no TLS) -----|
  |  (if TLS: TLS handshake here)     |
  |                                   |
  |-- StartupMessage ----------------->|
  |   protocol=196608 (3.0)           |
  |   user=alice                      |
  |   database=mydb                   |
  |                                   |
  |<- AuthenticationOk ---------------|
  |  (or AuthenticationMD5Password,   |
  |   AuthenticationSASL for SCRAM)   |
  |                                   |
  |<- ParameterStatus (repeated) -----|
  |   server_version=15.0             |
  |   client_encoding=UTF8            |
  |   TimeZone=UTC                    |
  |   ...                             |
  |                                   |
  |<- BackendKeyData -----------------|
  |   pid=12345, secret_key=98765     |
  |   (used for cancel requests)      |
  |                                   |
  |<- ReadyForQuery 'I' --------------|
  |   (I=idle, T=in transaction,      |
  |    E=in failed transaction)       |
  |                                   |
  |  --- now ready for queries ---    |
```

### SSLRequest

Before the startup message, the client may send an SSLRequest (8 bytes: length=8, code=80877103). The server responds with a single byte: 'S' to proceed with TLS, 'N' to decline.

### Authentication

oigrap supports:
- AuthenticationOk (no password required, for trust auth)
- AuthenticationMD5Password (legacy, compatible)
- AuthenticationSASL with SCRAM-SHA-256 (modern, secure)

For SCRAM-SHA-256:
1. Server sends AuthenticationSASL listing supported mechanisms
2. Client sends SASLInitialResponse with SCRAM client-first-message
3. Server sends AuthenticationSASLContinue with server-first-message (challenge)
4. Client sends SASLResponse with client-final-message (proof)
5. Server sends AuthenticationSASLFinal with server-final-message (verification)
6. Server sends AuthenticationOk

---

## Simple query protocol

The simple query protocol is the original (and simpler) protocol. The client sends one Query message; the server responds with results and a final ReadyForQuery.

```
Client sends Query('I' type byte):
  SELECT name FROM users WHERE id = 1;

Server responds:
  RowDescription('T' type byte):
    field_count: 2 (u16)
    fields[0]:
      name:          "name" (null-terminated)
      table_oid:     12345 (u32)
      col_attr_num:  1 (u16)
      type_oid:      25 (u32) -- TEXT
      type_size:     -1 (i16) -- variable length
      type_modifier: -1 (i32)
      format_code:   0 (u16) -- 0=text, 1=binary

  DataRow('D' type byte) for each result row:
    field_count: 2 (u16)
    fields[0]:
      length: 5 (i32)   -- -1 means NULL
      data:   "Alice"

  CommandComplete('C' type byte):
    tag: "SELECT 1"  -- null-terminated

  ReadyForQuery('Z' type byte):
    status: 'I'  -- idle
```

### Encoding result data

In text format (format_code=0): values are encoded as UTF-8 strings.
- INT: "42"
- FLOAT: "3.14"
- BOOL: "t" or "f"
- TIMESTAMP: "2024-01-15 10:30:00"
- NULL: field length = -1, no data bytes
- ARRAY: "{1,2,3}" or "{{1,2},{3,4}}" for 2D

In binary format (format_code=1): values are encoded in native binary:
- INT32: 4 bytes big-endian
- INT64: 8 bytes big-endian
- FLOAT64: 8 bytes IEEE 754 big-endian
- TEXT: raw UTF-8 bytes (no null terminator)
- BOOL: 1 byte, 0=false 1=true

Binary format is significantly more efficient for large result sets (no parsing overhead on client).

---

## Extended query protocol

The extended query protocol separates parsing, binding, and execution. This enables prepared statements and parameterized queries.

```
Flow:

Client -> Parse('P'):
  statement_name: "stmt1" (empty string = unnamed prepared statement)
  query: "SELECT * FROM users WHERE id = $1 AND plan = $2"
  param_types: []  (0 = infer from query)

Server -> ParseComplete('1')

Client -> Describe('D'):
  type: 'S' (statement) or 'P' (portal)
  name: "stmt1"

Server -> ParameterDescription('t'):
  num_params: 2
  param_types: [20, 25]  -- INT8, TEXT (PostgreSQL type OIDs)

Server -> RowDescription('T'):
  (column descriptions as above)

Client -> Bind('B'):
  portal_name: ""         (empty = unnamed portal)
  statement_name: "stmt1"
  param_format_codes: [0, 0]  -- text format for both params
  params: ["42", "enterprise"]
  result_format_codes: [0]    -- text result format

Server -> BindComplete('2')

Client -> Execute('E'):
  portal_name: ""
  max_rows: 0   -- 0 = no limit

Server -> DataRow, DataRow, ... CommandComplete

Client -> Sync('S')

Server -> ReadyForQuery('Z')
```

### Prepared statements

Prepared statements are parsed and planned once, then executed many times with different parameters. This amortizes parsing and planning cost.

```rust
struct PreparedStatement {
    name: String,
    query: String,
    physical_plan: PhysicalPlan,   // cached optimized plan
    param_types: Vec<DataType>,
    result_schema: Schema,
}
```

The plan cache must be invalidated when schema changes (ALTER TABLE, DROP INDEX, etc.) affect the plan. oigrap uses a generation counter: each schema change increments the counter. Cached plans store the generation at which they were created and are re-planned if the generation has advanced.

### Portals

A portal is a bound prepared statement with specific parameter values. Portals can be executed multiple times (with max_rows to fetch a partial result set at a time). This is how cursor-based fetching works.

```
Bind -> Execute(max_rows=100) -> DataRow*100 -> PortalSuspended
     -> Execute(max_rows=100) -> DataRow*100 -> PortalSuspended
     -> Execute(max_rows=0)   -> DataRow*N   -> CommandComplete
```

---

## Wire protocol implementation

```go
type Connection struct {
    conn      net.Conn
    buf       *bufio.ReadWriter
    sessionID uint64
    state     ConnState        // Startup, Auth, Idle, InQuery, InTransaction
    session   *Session
}

type Session struct {
    user        string
    database    string
    txnState    TxnState       // None, Active, Failed
    prepared    map[string]*PreparedStmt
    portals     map[string]*Portal
    params      map[string]string  // client_encoding, TimeZone, etc.
    cancelKey   CancelKey
}

func (c *Connection) Handle() {
    defer c.conn.Close()
    if err := c.handleStartup(); err != nil {
        c.sendError(err)
        return
    }
    for {
        msg, err := c.readMessage()
        if err != nil { return }
        switch msg.Type {
        case 'Q': c.handleSimpleQuery(msg)
        case 'P': c.handleParse(msg)
        case 'B': c.handleBind(msg)
        case 'E': c.handleExecute(msg)
        case 'D': c.handleDescribe(msg)
        case 'C': c.handleClose(msg)
        case 'S': c.handleSync(msg)
        case 'X': return  // Terminate
        case 'd': c.handleCopyData(msg)
        case 'c': c.handleCopyDone(msg)
        case 'f': c.handleCopyFail(msg)
        }
    }
}
```

### Message reading

```go
func (c *Connection) readMessage() (Message, error) {
    typeBuf := make([]byte, 1)
    if _, err := io.ReadFull(c.buf, typeBuf); err != nil {
        return Message{}, err
    }
    msgType := typeBuf[0]

    var lenBuf [4]byte
    if _, err := io.ReadFull(c.buf, lenBuf[:]); err != nil {
        return Message{}, err
    }
    length := binary.BigEndian.Uint32(lenBuf[:]) - 4  // subtract the 4-byte length field

    payload := make([]byte, length)
    if _, err := io.ReadFull(c.buf, payload); err != nil {
        return Message{}, err
    }

    return Message{Type: msgType, Payload: payload}, nil
}
```

### Error response format

PostgreSQL errors have a rich structured format. Every error sent to the client uses this format:

```
ErrorResponse('E' type byte):
  fields (null-terminated key-value pairs, terminated by null byte):
    'S': severity      -- "ERROR", "FATAL", "PANIC", "WARNING", "NOTICE", "INFO"
    'V': severity_v2   -- same as S (required in protocol v3)
    'C': code          -- SQLSTATE code (5 chars, e.g., "42P01" = undefined_table)
    'M': message       -- primary human-readable error message
    'D': detail        -- additional detail (optional)
    'H': hint          -- suggestion for fixing (optional)
    'P': position      -- character position in query string (optional)
    'W': where         -- context (e.g., function call stack)
    'F': file          -- source file (for internal errors)
    'L': line          -- source line number
    'R': routine       -- source function name
    '\0'               -- terminator
```

SQLSTATE codes (5-character codes from SQL standard):
- 42P01: undefined_table
- 42703: undefined_column
- 23503: foreign_key_violation
- 23505: unique_violation
- 40001: serialization_failure
- 40P01: deadlock_detected
- 08006: connection_failure

---

## Cancel request

A client can cancel an in-progress query by sending a cancel request on a new TCP connection (not the existing one):

```
New TCP connection:
  CancelRequest (16 bytes, no type byte):
    length:     16 (u32)
    cancel_code: 80877102 (u32)
    pid:         12345 (u32)   -- from BackendKeyData
    secret_key:  98765 (u32)   -- from BackendKeyData
```

The server receives this, validates the pid + secret_key, and sets a cancellation flag on the session with that pid. The execution engine's operators check this flag on each `next_batch` call and terminate early if cancelled.

---

## Connection pooling

oigrap does not include a built-in connection pooler. Users run pgBouncer or pgpool-II in front of oigrap, as they do with PostgreSQL. oigrap is fully compatible with PgBouncer's transaction-mode pooling (the most efficient mode).

The internal connection manager accepts up to `max_connections` simultaneous connections (default 100, configurable). Connections beyond this limit receive an error.
