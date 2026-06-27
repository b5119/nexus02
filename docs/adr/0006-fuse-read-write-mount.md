# ADR 0006: Read-write FUSE mount (whole-file write-back) wiring conflict detection

## Status
Accepted

## Context
ADR 0005 built the multi-writer conflict-detection *core* (vector clocks,
`ClockStore`, the `WriteFile` RPC, the host's keep-both policy) but the mount
was still `MountOption::RO` — there was no way to actually write through it. This
ADR covers flipping the mount to read-write and wiring local writes into that
conflict detection, so a `write()`/`create()` on the mount becomes a `WriteFile`
that the host evaluates.

## Decision

### Whole-file write-back buffer
FUSE delivers writes as arbitrary `(ino, offset, data)` chunks, but the
`WriteFile` RPC is whole-file. So `nexus-fs` keeps a per-inode in-memory buffer
(`FileBuf`) for files open for writing:
- `create` makes an empty, dirty buffer (so even a zero-byte new file is written
  through).
- `write` lazily seeds the buffer with the file's current host content
  (`ensure_loaded`: stat for size, one ranged read) on first touch, then applies
  the chunk in place and marks it dirty.
- `setattr(size=…)` implements truncate: `size==0` resets to an empty buffer
  (no host read needed — the common `>` redirection case); `size>0` resizes.
- `getattr`/`lookup` are buffer-aware: an open or freshly-created file reports
  its in-memory size and exists even before it's flushed to the host.

### One flush per write session
The buffer is flushed to the host **once**, on `release` (and on `fsync`).
`flush` is a no-op for the write-back: it can fire multiple times per open (once
per `close()` of a dup'd fd), and flushing there too caused the file to be sent
— and the clock incremented — twice (observed as a clock counter of 2 for a
single `echo >`). Flushing only on `release`/`fsync` makes it exactly one
increment per write session.

### Client identity + clock memory
The client now has its own persistent `DeviceId` (`crates/fs/src/config.rs`,
mirroring the agent's `config.rs` / ADR 0003: `client.json`, `NEXUS_CONFIG_DIR`
override) and a `ClockStore` (`client-clocks.json`). On flush it:
1. reads its last-known clock for the path, increments **its own** counter,
2. sends `WriteFile` with that clock,
3. adopts the host's authoritative clock from the response (merge), so the next
   write builds on it.

The host's existing logic (ADR 0005) then decides Applied / Stale / Conflict.
Conflicts are surfaced as a logged WARNING on the client and the `.conflict-*`
file on the host; the local `write()` still returns success (the bytes are
safely preserved server-side) — see "new gaps" for why.

### Mount is now RW
`MountOption::RO` is dropped; file perms are `0o644`/dirs `0o755`.

## Verified end-to-end (through the real mount, not just gRPC)
- New file written through the mount lands on the host with clock `{client:1}`.
- Same device overwriting its own file → clean update, clock advances to
  `{client:2}`, no conflict (client clock adoption works).
- Two mounts = two devices: device B writing a file it never synced is
  concurrent with device A's version → `.conflict-<B>-<ts>` created, original
  (A's) untouched — the ADR-0005 conflict result, now proven through FUSE.

## New gaps this surfaced (named, not papered over)
- ~~**Reads don't sync clocks.**~~ **RESOLVED in ADR 0007.** `Stat` now carries
  the clock and the client syncs on a read-intent `open`, so read-then-edit no
  longer false-conflicts while blind overwrites still do. (Originally: a device
  that only read a file then edited it had an empty clock and was flagged
  concurrent — safe over-detection, but annoying.)
- **Conflicts are detected at flush, not at `write()`.** With write-back, the
  `write()` syscall has already returned success by the time the conflict is
  known (on close). We log it and keep both server-side rather than failing the
  syscall. An app that wrote expecting last-write-wins won't see an error; it
  must look for `.conflict-*` files. (Returning EIO from `release`/`fsync` was
  considered but is both unreliable — `close()` ignores it — and worse UX.)
- **Truncated/oversimplified inode lifecycle still applies.** The LRU bound from
  the inode-table work can still evict an inode the kernel references; with
  writes in play this is the same trade-off as before (a stale-handle error on
  that one entry), not made worse, but not fixed either.
- **No `mkdir`/`rmdir`/`rename`.** `unlink` (file delete) is now supported with
  delete-vs-edit conflict detection — see **ADR 0008**. Directory operations and
  rename are still unsupported, so rename conflicts stay out of scope.
- **Whole-file buffering in memory.** A write opens and buffers the entire file;
  fine for the small files we target, not for very large ones. Streaming
  write-back is a future refinement (same note as `WriteFile` being unary).
- **Reads of an open dirty file see host content, not the unflushed buffer.**
  `read` still goes to the host; within a single process, write-then-read before
  flush would see stale bytes. Not exercised by normal `echo`/`cat` flows but a
  real limitation.
- **Coarse host-side write serialization** (one writer at a time across all
  paths) carries over from ADR 0005.

## Consequences
- `nexus-mount` is read-write; the README quickstart now shows writing through
  the mount.
- The client persists a DeviceId + clock memory under the config dir, like the
  agent.
- Conflict *resolution* remains a human (or future per-format) task: the
  `.conflict-*` files are produced but not auto-merged (ADR 0005).
