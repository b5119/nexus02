# ADR 0010: Streaming Writes (Large-File Support)

## Status

Accepted

## Context

The current write path buffers the entire file in memory (`FileBuf.data` on
the FUSE client) and sends it as a single gRPC `WriteFile` message on
flush/release. This works for small files (the test suite uses files up to a
few KB) but fails for realistic large files (photos, videos, databases) in two
ways:

1. **Memory pressure:** The FUSE client holds the complete file content in RAM
   while the file is open for writing *and* the host receives it as one
   monolithic allocation. A 1 GB video requires 1 GB + 1 GB peak.
2. **gRPC message limit:** tonic's default max message size is 4 MB. Any
   file larger than that is rejected at the transport layer.

The read path already solved this via `ReadFile`'s server-streaming in 64 KB
chunks (the `CHUNK_SIZE` constant in host.rs). The write path needs the same
treatment, but in reverse: the client streams chunks *to* the host rather than
the host streaming chunks *to* the client.

## Design Questions

### 1. What does "streaming write" mean at the FUSE layer?

FUSE delivers writes as arbitrary partial chunks via repeated `write()`
callbacks. The current implementation accumulates them all into
`FileBuf.data` and flushes the complete buffer on `release()`/`fsync()`.

**Two approaches:**

- **A — Stream each individual FUSE write() chunk immediately.** The client
  sends every partial chunk to the host as it arrives. The host must handle
  partially-written files, out-of-order chunks, overlapping writes, and
  interrupted streams. This requires the host to maintain write-in-progress
  state, handle rollback on failure, and solve the general problem of
  remote random-access writes. This is a significant increase in complexity.

- **B — Stream the accumulated buffer on flush.** The merge-and-accumulate
  boundary (FUSE write → `FileBuf.data`) stays exactly as it is. The change
  is only in how `flush_buffer` sends the data: instead of one monolithic
  `WriteFile` gRPC call, it streams the buffer in 64 KB chunks via a new
  client-streaming RPC. The host receives all chunks, reassembles them into
  the file, applies conflict detection at the *end* of the stream (same
  logical point as the current unary call), and returns the same
  `WriteFileResponse`.

**Recommendation: Approach B.** Justification:

- Zero change to the conflict-detection semantics — the vector clock is still
  compared at flush time, exactly as today.
- No new state on the host — the host accumulates the stream into a temp file
  and atomically renames it to the final path on success, or discards it on
  error. This is simpler than handling partial/interleaved writes.
- The memory pressure improvement is real: the FUSE client still buffers the
  full file, but the host no longer needs a second full copy in a single gRPC
  message buffer. The host receives and writes chunks sequentially to a temp
  file, so peak memory on the host drops from `file_size` to `CHUNK_SIZE`.
- If future optimization requires eliminating the client-side buffer too,
  that can be a separate ADR (it would require the host to handle streaming
  random-access writes, which is approach A — deferred).

### 2. What is the chunk size?

**64 KB.** Same as the read path (`CHUNK_SIZE` in host.rs). Consistency across
read and write paths simplifies reasoning, and 64 KB is a well-known sweet
spot for FUSE (it matches the kernel's preferred FUSE transfer size on many
configurations). There is no specific reason to deviate.

### 3. Does the host's conflict detection change?

**No.** The vector clock comparison against stored clocks and tombstones
happens at the *end* of the stream, after all data has been written to a temp
file but before the final atomic rename. This is the same logical moment as
the current unary `WriteFile` handler. The clock is sent in the first chunk's
metadata and held until the stream completes. If the stream is interrupted
mid-way, the temp file is discarded and the stored clock is untouched —
no partial write is visible.

### 4. Metadata placement

The first chunk of the client stream carries the metadata (path, clock,
writer_device_id) in addition to its data payload. Subsequent chunks carry
data only. This avoids a separate handshake RPC while keeping the common
case (data-heavy chunks) compact.

When `path` is non-empty the host treats it as a stream init. When `path` is
empty the host treats it as a continuation data chunk. This is unambiguous
because the path is always known at stream start and never changes mid-stream.

## Consequences

- The existing `WriteFile` unary RPC is kept in the proto for backward
  compatibility (direct callers) but the FUSE layer always uses the new
  streaming RPC.
- The host's `write_file_stream` handler needs a temp-file strategy:
  write incoming chunks to a temporary path, then rename on success.
  This ensures a mid-stream failure doesn't leave a partial file at the
  target path.
- The `CHUNK_SIZE` constant is reused from the read path — no new tuning.
- Existing tests pass unchanged. New tests verify large files (>4 MB to
  exercise chunk boundaries beyond tonic's default message limit) and
  byte-exact comparison.
- ADR 0006's note about "unary/whole-file for now; large-file streaming is
  a future refinement" is resolved.

## References

- ADR 0005: Vector Clock Conflict Detection
- ADR 0006: FUSE Read-Write Mount
- ADR 0008: Delete-vs-Edit Conflicts
- Issue #8: Large-file streaming for writes
