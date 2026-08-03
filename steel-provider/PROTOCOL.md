# `steel-provider` Wire Protocol

## Transport

The server accepts a single endpoint as its first command-line argument:

- A **Unix socket path**, e.g. `/tmp/steel-provider.sock` (default).
- A **TCP address**, e.g. `0.0.0.0:4096`.

## Framing

All integers are **big-endian**. The request is fixed-size and is sent without
a length prefix; the response is variable-size and is length-prefixed with a
`u32` payload length.

## Request (client → server)

The request is always exactly 16 bytes:

```
u64 seed            big-endian, world seed
i32 chunk_x         big-endian, chunk coordinate
i32 chunk_z         big-endian, chunk coordinate
```

## Response (server → client)

The response is a frame: `u32 payload_length` followed by a payload of exactly
that many bytes.

```
u32 payload_length = 4 + len(data)
u32 status         big-endian; 0 = ok, 1 = error
byte[] data
```

- **status 0 (ok)** — `data` is the generated chunk serialized in the
  Minecraft network format (paletted chunk sections), produced by
  `serialize_chunk_sections()`.

  See https://minecraft.wiki/w/Java_Edition_protocol/Chunk_format#Data_structure
  for more details. The data array here is the same as the one mentioned on the wiki.

- **status 1 (error)** — `data` is a UTF-8 error message.

## Connection semantics

- A connection is **synchronous and serial**: send one request, read one
  response, repeat. The server processes requests on a connection in order.
- The server accepts **multiple concurrent connections**; each is handled on
  its own thread with.
- The server caches one `WorldgenContext` per seed, created on first use, so
  repeated requests for the same seed reuse the setup work.
- A client can close the connection (or the server closes it) after any
  response; the server returns to accept() when it reads EOF.
