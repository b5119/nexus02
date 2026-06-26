# Architecture

A map of how Nexus fits together, for anyone reading the code for the first
time. The *why* behind most decisions lives in the ADRs (`docs/adr/`) — this
doc is the *what* and *where*, and points you at the right ADR for the *why*.

## The one core idea: asymmetric roles

Nexus is **not** "every device mounts every other device symmetrically." A
device plays one or both of two roles:

- **Host** — serves its own files out over gRPC (`FileService`). *Every*
  platform can host, including Android (within scoped-storage limits).
- **Client** — mounts a remote host's files locally via FUSE. Only Linux/macOS
  (and eventually Windows/WinFsp). **Android cannot be a FUSE client** — this is
  load-bearing and already decided; see [ADR 0001](docs/adr/0001-android-fuse-limitation.md)
  before proposing otherwise.

```
        ┌──────────────────────────┐         gRPC over TLS          ┌───────────────────────────┐
        │  HOST  (e.g. the phone)  │  (FileService: ListDir/Stat/  │  CLIENT (e.g. the Dell)   │
        │  nexus-agent             │   ReadFile stream/WriteFile)  │  nexus-mount (FUSE)       │
        │  - serves --serve-dir    │ <───────────────────────────> │  - kernel FUSE callbacks  │
        │  - per-file vector clocks│   token auth + self-signed    │  - block_on bridges sync  │
        │  - conflict detection    │   cert (ADR 0004)             │    FUSE → async gRPC      │
        └──────────────────────────┘                               └───────────────────────────┘
```

## Crates (`crates/`)

| Crate | Binary | Role | Notes |
|---|---|---|---|
| `common` | — | shared types | `DeviceId`, `FileEntry`, `VectorClock`/`ClockOrder`, `ClockStore`, errors. One definition everyone agrees on. |
| `proto` | — | gRPC schema | `proto/file_service.proto` + build-time `tonic` codegen. The wire contract. |
| `agent` | `nexus-agent` | **Host** daemon | Serves a directory over `FileService`; owns the authoritative per-file vector clocks + conflict policy. Builds for all targets incl. Android (`cargo-ndk`). |
| `fs` | `nexus-mount` | **Client** mount | FUSE filesystem; bridges sync kernel callbacks to async gRPC via `block_on`. Linux/macOS only (target-gated `fuser`). |

## Request flow (read and write)

- **Read**: kernel `lookup`/`getattr`/`readdir`/`read` → `nexus-fs` FUSE callback
  → `block_on` an async `FileService` RPC (`Stat`/`ListDir`/`ReadFile`) → host
  serves from `--serve-dir`. The host streams `ReadFile` in 64 KiB chunks.
- **Write**: kernel `create`/`write`/`setattr`/`release` → `nexus-fs` buffers the
  whole file in memory, then on `release`/`fsync` sends one `WriteFile` carrying
  the client's vector clock → the host compares clocks and decides
  *applied / stale / conflict* (keep-both on conflict). See ADRs 0005–0007.

## Two concepts worth understanding before editing

1. **The sync↔async bridge** (`crates/fs/src/filesystem.rs`): the kernel calls
   FUSE methods synchronously, one at a time, but our data source is async gRPC.
   Each callback `block_on`s the RPC. Documented at the top of that file.
2. **Inode table** (`crates/fs/src/filesystem.rs`): FUSE addresses everything by
   inode number, not path, so we keep a bounded (LRU) inode↔path map.

## Decision records (`docs/adr/`)

Read these before changing the area they cover.

| ADR | Topic |
|---|---|
| [0001](docs/adr/0001-android-fuse-limitation.md) | Android can't be a FUSE client (role asymmetry) |
| [0002](docs/adr/0002-android-cross-compilation.md) | Android cross-compilation via `cargo-ndk` |
| [0003](docs/adr/0003-config-dir-override.md) | Config dir must be overridable (`NEXUS_CONFIG_DIR`) |
| [0004](docs/adr/0004-shared-secret-auth-and-tls.md) | Shared-secret auth + self-signed TLS |
| [0005](docs/adr/0005-vector-clock-conflict-detection.md) | Vector-clock conflict detection (keep-both) |
| [0006](docs/adr/0006-fuse-read-write-mount.md) | Read-write FUSE mount (whole-file write-back) |
| [0007](docs/adr/0007-read-open-clock-sync.md) | Sync clocks on read-intent open |

## Status & security posture

Milestone 1 (read-write phone→Dell mount with conflict detection) works. The
data plane is authenticated + encrypted but **LAN-trust only** — it is *not*
hardened for hostile networks (no pairing UX, no revocation, token stored in
plaintext; see ADR 0004). Don't deploy it as production-secure. See the README
Status checklist for what's done vs. open, and `CONTRIBUTING.md` to build.
