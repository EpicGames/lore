# Protocols
There are two protocol implementations in place, QUIC and gRPC. The goal is to use QUIC for the high throughput low latency operations (GET, PUT and QUERY) while using gRPC as the one-shot command protocol for everything else. Connections and commands from a single client can be routed to different endpoints, and commands can be multiplexed over multiple connections.

# QUIC

## Streams
The client can use up to 32 parallel QUIC streams on each connection. Commands can be issued in a streaming fashion where the next command can be sent before the reply has been read. The reply to a command is not guaranteed to be sent on the same stream as the original command, and will be delivered out of order to how commands were sent.

When a connection is made the connect command must be sent on stream 0 before any other commands are issued on any stream.

## Protocol header
Both command and reply have the same 8 byte header defined as

```
union {
    uint8_t data[8];
    struct {
        uint32_t cmd : 8;
        uint32_t size_status : 23;
        uint32_t error : 1;
        uint32_t id;
    };
}
```

or in plain text
- 1 byte for command type
- 3 bytes for size or status (low 23 bits) and error flag (highest bits)
- 4 bytes for command ID

## Commands
Each command starts with the 4 byte header described above, then a command payload which must be exactly the size given in the `size_status` field of the header. Each payload is specific to the command type.

For commands the error bit must be set to 0 and must be ignored by the server.

If the command ends in a variable length field, the remainder of the command payload as given by the `size_status` field is the length of the variable length field.

Before any other command can be sent the client must send a connect command to authenticate and associate the connection to a repository.

Command types:
```
CONNECT = 0
GET = 1
PUT = 2
QUERY = 3
LOAD = 5
PING = 6
BRANCH_CREATE = 7
BRANCH_DESTROY = 8
BRANCH_PUSH = 9
```

The GET, PUT and QUERY are the high frequency commands used to transfer data.

## Replies
Each reply starts with the same header as for commands.

The `size_status` field hold the length of the reply payload. If the error bit is set the `size_status` field instead hold the status error code and the payload is 0 bytes.

The `id` and `cmd` fields match the command for which the reply is sent. Note that the reply may be delivered out of order and may be sent on a different stream than the command was originally sent over.

### Connect
Authenticate and associate the connection to a repository.

Command payload:
```
[16 bytes] Repository ID
[...variable...] Authentication token
```

Reply payload:
```
No payload
```

### Get
Get the metadata and payload for a fragment in the immutable store.

Command payload:
```
[32 bytes] Content hash
[16 bytes] Address context
```

Reply payload:
```
[4 bytes] Fragment flags
[4 bytes] Payload size
[8 bytes] Uncompressed and reassembled content size
[...variable...] Payload
```

### Put
Put the metadata and payload for a fragment in the immutable store. Partial puts without any payload are allowed if the content can be deduplicated with another already existing fragment with the same content hash in the same repository.

Payload:
```
[32 bytes] Content hash
[16 bytes] Address context
[4 bytes] Fragment flags
[4 bytes] Payload size
[8 bytes] Uncompressed and reassembled content size
[...variable...] Payload, must either be zero length or be equal to the payload size field in length
```

Reply payload:
```
No payload
```

### Query
Query if a fragment exist in the immutable store.

Payload:
```
[...variable...] Addresses to query, each must be a multiple of a 48 byte fragment address as given by the GET command:
    [32 bytes] Content hash
    [16 bytes] Address context
```

Reply payload:
```
[...variable...] Status for each fragment in query, 1 byte each
    [1 byte] Status
        0 = fragment does not exist
        1 = fragment exists in the given repository and context
        2 = fragment exist in repository but not in context
```

### Load
Load a value from the mutable store.

Command payload:
```
[32 bytes] Key to load
```

Reply payload:
```
[32 bytes] Value stored for the key
```

### Ping
Ping the server. The server will return the same timestamp value as given in the command.

Command payload
```
[8 bytes] Timestamp
```

Reply payload
```
[8 bytes] Timestamp
```

### Branch create
Create a named branch. The branch identifier is the first 16 bytes of the BLAKE3 hash of the branch name.

Command payload:
```
[16 bytes] Branch identifier
[32 bytes] Revision hash of initial latest pointer (branch point)
[16 bytes] Parent branch identifier
[...variable...] Branch name string, must match the branch identifier when hashed
```

Reply payload
```
[32 bytes] Current latest pointer revision hash
```

### Branch destroy
Destroy a named branch

Command payload
```
[16 bytes] Branch identifier
[32 bytes] Current latest pointer
```

Reply payload:
```
No payload
```

### Branch push
Push a new latest pointer for branch. Server will validate the revision latest pointer chain and all referenced fragments before allowing a new latest pointer to be pushed.

Command payload
```
[16 bytes] Branch identifier
[32 bytes] New latest pointer
```

Reply payload
```
[32 bytes] Current latest pointer revision hash
```

# gRPC

gRPC proto definitions are available in the `lore-proto` crate.

In general the gRPC protos match the QUIC protocol command and reply definitions as seen above.

## Authentication
Authentication token must be supplied in the `authorization` metadata for each request with the format `Bearer <token>`

## Streaming
GET and PUT requests are streaming, all other requests are one shot.

# Authentication
Current QUIC and gRPC server implementation expects a JWT token as supplied by the login service as the authentication bearer.
