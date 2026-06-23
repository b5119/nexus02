# Nexus

Personal device mesh: unified file access and remote control across
your own devices, built from scratch in Rust.

This is **not** a generic "virtualize any device" tool — see
[docs/adr/0001-android-fuse-limitation.md](docs/adr/0001-android-fuse-limitation.md)
for why that idea doesn't hold up, and what Nexus does instead.

## Current milestone: Layer 2 (filesystem virtualization)

Goal: mount a phone's storage on a Linux laptop as a real, lazy-loaded
FUSE filesystem — `ls`, `cat`, `cp` all work against it like it's a
local directory, but nothing is actually copied until read.

```
┌─────────────┐   gRPC (FileService)   ┌──────────────┐
│   Android    │ ─────────────────────> │  Dell/Linux   │
│ nexus-agent  │  ListDir/Stat/ReadFile │  nexus-agent   │
│  (HOST)      │ <───────────────────── │  nexus-fs      │
└─────────────┘                        │  (FUSE mount)  │
                                        └──────────────┘
```

## Workspace layout

```
crates/
├── common/   shared types (DeviceId, FileEntry, errors)
├── proto/    gRPC schema (file_service.proto) + generated code
├── agent/    daemon — runs on every device, implements the HOST role
└── fs/       FUSE client — Linux/macOS only, implements the CLIENT/mount role
```

## Quickstart (Linux, milestone 1)

```bash
# Build everything (capped to 2 parallel jobs — see .cargo/config.toml)
cargo build

# Terminal 1: run the host agent, serving a directory
./target/debug/nexus-agent --serve-dir ~/nexus-test-share --port 50051

# Terminal 2: mount it
mkdir -p ~/nexus-mount
./target/debug/nexus-mount --remote http://127.0.0.1:50051 --mountpoint ~/nexus-mount

# Terminal 3: prove it works
ls ~/nexus-mount
cat ~/nexus-mount/some-file.txt

# Unmount when done
fusermount3 -u ~/nexus-mount
```

## Android (host role only — see ADR 0001)

Cross-compilation setup: see
[docs/adr/0002-android-cross-compilation.md](docs/adr/0002-android-cross-compilation.md).
Not yet wired into an actual installable app — currently just a
cross-compiled binary you can push via `adb` for testing.

## Status

- [x] Workspace scaffold
- [x] `FileService` proto (ListDir, Stat, ReadFile)
- [x] Host agent (Linux) — serves a local directory over gRPC
- [x] FUSE client (Linux) — mounts a remote agent's files read-only
- [x] Compile-verified on real hardware (Dell Latitude E6530, i7-3520M)
- [x] Loopback test passed (Dell → Dell): byte-exact reads, including a
      chunk-boundary-spanning offset read on a 200KB file
- [x] Android cross-compilation (`cargo-ndk`, arm64-v8a + armeabi-v7a) — see
      [docs/adr/0002](docs/adr/0002-android-cross-compilation.md)
- [x] **Android host role tested against real hardware** (TECNO KL4,
      Android 14): phone served files over gRPC, Dell mounted them via FUSE,
      byte-exact verified including a chunk-boundary offset read on a 150KB
      file. Run manually via `adb shell` (shell uid, not yet a packaged app —
      see open question below).
- [ ] Write support + conflict resolution (vector clocks)
- [ ] Pairing / auth (control plane) — currently zero auth, LAN-trust only
- [ ] Layer 1 (remote control / streaming) — not started
- [ ] Layer 4 (app-cooperative migration SDK) — not started

### Open question: packaging the agent as a real Android app

The phone-host test above ran the agent manually as the `adb shell` user
(uid 2000), which has direct `sdcard_rw` filesystem access and therefore
never touches Android's scoped-storage rules. That result is real, but it
does **not** prove the agent works once packaged inside an actual app —
an app runs under its own uid with no raw `sdcard_rw` access, and SAF
(Storage Access Framework) becomes mandatory at that point. This is a
distinct, harder problem layered on top of the FUSE/cross-compilation
work already proven, not yet solved. See
[docs/adr/0002](docs/adr/0002-android-cross-compilation.md) for the
specific open questions this raises (foreground-service wrapper shape,
SAF permission flow, install/update mechanism for the embedded binary).

## Security note

Milestone 1 has **no authentication**. Any device on the network that
can reach the host agent's port can read every file under `--serve-dir`.
Do not run this on an untrusted network. Pairing/auth is planned but
not yet built — see the Status checklist.
