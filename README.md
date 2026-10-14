# Nexus

Personal device mesh: unified file access and remote control across
your own devices, built from scratch in Rust.

This is **not** a generic "virtualize any device" tool — see
[docs/adr/0001-android-fuse-limitation.md](docs/adr/0001-android-fuse-limitation.md)
for why that idea doesn't hold up, and what Nexus does instead.
Architecture overview: [ARCHITECTURE.md](ARCHITECTURE.md). Decisions: [docs/adr/](docs/adr/).

## Status at a glance

| Layer | What | State |
|---|---|---|
| 0 | File mesh: host agent + read-write FUSE mount, vector-clock conflict detection | **Working** (Linux↔Linux; Android host verified manually via `adb shell`) |
| 0 | Pairing (6-digit code), mDNS discovery, TLS + token auth | **Working** — LAN-trust only, see [Security](#security-note) |
| 1 | Screen streaming + remote control (Linux host → Android / Linux viewer) | **Core working**; polish items open |
| 2 | Android file access (file picker on the tablet, then Android as a host) | **Designed (ADR 0017), not built yet**; agent builds for Android again |
| 3 | Cross-device sync, AI features | Future, nothing started |
| 4 | App-cooperative migration SDK (`nexus-migrate`) | **Crate implemented** (ADR 0012); not yet wired into an app |

CI runs `fmt`, `clippy -D warnings`, build and test on Linux, plus an `android-check` job
(`cargo ndk -t arm64-v8a check -p nexus-agent -p nexus-migrate`) so Android builds cannot
break silently.

## Layer 0: file mesh

Two roles (ADR 0001): a **host** serves a directory over gRPC (`FileService`);
a **client** mounts it via FUSE (Linux/macOS only — Android cannot be a FUSE client).

- [x] Host agent serves a local directory over gRPC (`ListDir`, `Stat`, `ReadFile`, `WriteFile`, `WriteFileStream`, `DeleteFile`, `RenameFile`, `MkdirFile`).
- [x] Read-write FUSE client (`nexus-mount`), whole-file write-back, streaming writes for large files (ADR 0006, 0010).
- [x] **Multi-writer conflict detection** with per-file vector clocks: concurrent edits keep **both** versions (`<name>.conflict-<device>-<ts>`), never silently merged. Covers edit-vs-edit, delete-vs-edit (tombstones), rename/move and directory-level conflicts (ADR 0005–0009, 0016). Clock/tombstone garbage collection (ADR 0011).
- [x] **Data-plane auth + TLS**: shared-secret token over a self-signed cert (ADR 0004).
- [x] **Device pairing**: one-time 6-digit code, 60 s expiry, constant-time compare, per-code attempt cap and per-IP rate limiting (ADR 0013).
- [x] **mDNS discovery** (`_nexus._tcp`): `nexus-agent discover`, `nexus-mount mount --discover` (ADR 0015).
- [x] Android **host** role proven on real hardware (TECNO KL4, Android 14): phone served files, Dell mounted them via FUSE, byte-exact including a chunk-boundary read. *This ran as the `adb shell` user, which bypasses scoped storage; a packaged app needs SAF (Layer 2).*
- [ ] macOS client (macFUSE) build and mount is unverified (issue #9).
- [ ] Concurrent rename-vs-edit semantics (issue #26).
- [ ] TLS-layer rejection of unknown client certs — `custom_tls.rs` exists but is not wired because `tonic` keeps `tls_acceptor()` crate-private (issue #41, #15).

## Layer 1: remote control / screen streaming

Stream the host's screen to a paired tablet/phone and forward touch, keyboard and
mouse input back.

```
┌─────────────┐   gRPC (StreamService)   ┌──────────────┐
│   Tablet      │ <───────────────────── │  Dell/Linux    │
│ Nexus Viewer  │  H.264 video + input   │  nexus-agent   │
│  (viewer)     │ ─────────────────────> │  (host)        │
└─────────────┘                         └──────────────┘
```

**Done**

- [x] **Screen capture**: Wayland via the XDG portal + PipeWire screencast (connects to the portal session's remote, negotiates BGRA/BGRx/RGBA/RGBx); X11 (`XShmGetImage`) fallback.
- [x] **H.264 encode** via `libx264` (ffmpeg-next), tuned for real time: `keyint=15`, no B-frames, in-band SPS/PPS, ~8 Mbps, `ultrafast` + `zerolatency` + `baseline`, `vbv-bufsize=1000`, `vbv-maxrate=8000`, `ref=1`. Intel QSV is used when present; x264-only options are applied only to `libx264` (fixes the QSV preset error).
- [x] **PTS fix**: scaled frames inherit the source PTS and advance by the nominal frame period, ending `non-strictly-monotonic PTS` spam and flicker.
- [x] **Backpressure + metrics**: lock-free capture ring buffer (capacity configurable, drops oldest), bounded 32-frame send channel, and counters logged every 10 s (captured / encoded / sent / dropped-blank / dropped-lag / dropped-channel-full / errors / average capture and encode latency).
- [x] **Input injection** on the host through `uinput` (absolute touch, relative pointer, buttons, scroll, keyboard).
- [x] **Bidirectional gRPC** `StreamService.RemoteControl`: `VideoFrame` out, `InputEvent` in.
- [x] **Android viewer** (Kotlin, MediaCodec, Material 3): mDNS discover, manual IP, 6-digit pairing with TLS trust-on-first-use then certificate pinning; Touch mode and Pointer mode (tap = click, long-press = right click, two-finger scroll); keyboard forwarding as evdev keycodes; auto-rotate and Fit/Fill.
- [x] **Linux viewer** (`nexus-viewer`, winit + pixels).
- [x] End-to-end tested: tablet (IN-101, Android 14) ↔ Dell (Wayland/GNOME), 1920×1080 @ 30 fps stable.

**Open**

- [x] **Android Documents Provider** (Layer 2, feature A): `NexusDocumentsProvider` is a pure Kotlin `DocumentsProvider` that displays every paired Nexus host as a root in the system file picker. `queryChildDocuments` → ListDir, `queryDocument` → Stat (backed by `MetaCache`), `openDocument` → ranged ReadFile via `StorageManager.openProxyFileDescriptor` with `ReadAheadBuffer` for sequential reads. Read-only for now; write modes deferred. Verified on emulator; 18 unit tests pass. See `android/app/src/main/java/com/vectorzero/nexus/documents/`.

- [ ] Always-on host: systemd user service/autostart; portal grant persistence (GNOME re-prompts each session).
 - [ ] Tablet auto-connect to the last-paired host (the earlier attempt was removed from `PairingActivity` in `927fb9b`).
 - [ ] Adaptive bitrate; VAAPI detection and fallback.
 - [ ] Clipboard sync (text, image).
 - [ ] Multiple monitors.
 - [ ] ADR 0014 still describes X11 capture and QSV as the primary path; the code now prefers PipeWire on Wayland. ADR needs an update.

## Layer 2: Android file access — built (feature A)

[ADR 0017](docs/adr/0017-layer-2-android-file-access.md) describes the Android Documents Provider approach. A pure Kotlin `NexusDocumentsProvider` (lines 38–196 of `android/app/src/main/java/com/vectorzero/nexus/documents/NexusDocumentsProvider.kt`) displays every paired Nexus host as a root in the system file picker. Calls match the Android SAF callbacks:

| Android callback | Nexus RPC |
|------------------|-----------|
| `queryRoots` | one root per host in `[HostStore]` |
| `queryChildDocuments` | `ListDir` via `FileClients.forHost(host).list(parent.path)` |
| `queryDocument` | `Stat` (usually served from `[MetaCache]`) |
| `openDocument` (read) | ReadFile ranges, via a proxy file descriptor backed by `[ReadAheadBuffer]` |

Document ids are `<hostId>:<path>` (`[DocId]`). Reads use `StorageManager.openProxyFileDescriptor`, which gives callers real random access (seeking in a video does not download the whole file), backed by a `[ReadAheadBuffer]` so sequential reads cost one RPC per 256 KiB. Reads are read-only; write modes are refused for now (deferred to a later ADR).

- **Built and verified**: emulator + 18 unit tests (`DocIdTest`, `MetaCacheTest`, `ReadAheadBufferTest`).
- **Read-only for now**: write modes deferred to a later ADR.
- **LAN-trust only**: same as the rest of Nexus — not hardened for hostile networks.

## Layer 2: Android file access — built (feature A)

[ADR 0017](docs/adr/0017-layer-2-android-file-access.md) describes the Android Documents Provider approach. A pure Kotlin `NexusDocumentsProvider` (lines 38–196 of `android/app/src/main/java/com/vectorzero/nexus/documents/NexusDocumentsProvider.kt`) displays every paired Nexus host as a root in the system file picker. Calls match the Android SAF callbacks:

| Android callback | Nexus RPC |
|------------------|-----------|
| `queryRoots` | one root per host in `[HostStore]` |
| `queryChildDocuments` | `ListDir` via `FileClients.forHost(host).list(parent.path)` |
| `queryDocument` | `Stat` (usually served from `[MetaCache]`) |
| `openDocument` (read) | ReadFile ranges, via a proxy file descriptor backed by `[ReadAheadBuffer]` |

Document ids are `<hostId>:<path>` (`[DocId]`). Reads use `StorageManager.openProxyFileDescriptor`, which gives callers real random access (seeking in a video does not download the whole file), backed by a `[ReadAheadBuffer]` so sequential reads cost one RPC per 256 KiB. Reads are read-only; write modes are refused for now (deferred to a later ADR).

- **Built and verified**: emulator + 18 unit tests (`DocIdTest`, `MetaCacheTest`, `ReadAheadBufferTest`).
- **Read-only for now**: write modes deferred to a later ADR.
- **LAN-trust only**: same as the rest of Nexus — not hardened for hostile networks.

## Layer 2: Android file access — Feature B foundation

Feature B: "the Dell mounts the phone's storage" (Android as host, serving files through SAF so a Linux laptop can FUSE-mount them).

**Progress: Foundation laid — Storage trait + Host implementation added.**

- **`crates/agent/src/storage.rs`** — `Storage` trait with `list_dir`, `stat`, `read_file`, `write_file`, `mkdir_file`, `delete_file`, `rename_file` methods using simple Rust types (no tonic/prost dependency).
- **`crates/agent/src/storage_host.rs`** — `StorageHost` implementation backed by `FileServiceImpl`, the native host agent. This adapts the existing host file logic into the [`Storage`] interface, enabling:
  - Shared file logic between the host binary, tests, and (future) the Android JNI bridge.
  - A common interface for Feature B's Android SAF implementation.
- **Android JNI bridge** (`crates/android-browser/` + `saf_bridge.rs`) — previously dropped because it didn't compile; the Storage trait provides a clean interface for a future reimplementation.
- **Roadmap**: Once the Android JNI bridge compiles and the `StorageAndroid` implementation is written (following the same callback→RPC mapping as Feature A), a Linux laptop can FUSE-mount a phone's storage via `nexus-mount`, using the phone as a host just as the Dell mounts the phone.

**Remaining**: Write the `StorageAndroid` JNI implementation (in `crates/android-browser` or a new bridge crate), register the document provider in `AndroidManifest.xml` with SAF support, and verify end-to-end on a device.

**Done in the Phase 0 cleanup**

- [x] `nexus-agent` builds for Android again: `nexus-stream` (PipeWire/ffmpeg) is a non-Android dependency, and `serve --enable-streaming` errors cleanly on Android.
- [x] CI `android-check` job guards the Android build.
- [x] The never-compiled Rust SAF scaffolding (`saf_bridge.rs`, `crates/android-browser`) was removed.
- [x] `nexus-migrate` Android build fixed (`jni::Outcome` import).

**Still to do**

- [ ] Kotlin `NexusDocumentsProvider`, read-only (`ListDir`, `Stat`, `ReadFile`)
- [ ] Writes and conflict handling from the picker
- [ ] `Storage` trait and an Android host implementation (feature B)

## Workspace layout

```
crates/
├── common/          shared types: DeviceId, FileEntry, VectorClock, ClockStore, TombstoneStore, errors
├── proto/           gRPC schemas: file / pair / stream / migrate services + generated code
├── agent/           nexus-agent — HOST daemon: FileService, pairing, mDNS, GC, streaming host
├── fs/              nexus-mount — CLIENT: FUSE mount + pairing (Linux/macOS)
├── stream/          screen capture, H.264 encode/decode, uinput injection, nexus-viewer
└── migrate/         nexus-migrate — app-cooperative state migration SDK (+ Kotlin binding)
android/             Kotlin viewer app (minSdk 26, compileSdk 35)
docs/adr/            ADRs 0001–0017
```

## Quickstart (Linux)

```bash
# Prerequisites (Debian/Ubuntu); see CONTRIBUTING.md for other platforms.
# Streaming also needs ffmpeg dev libs, libx264, libpipewire-0.3-dev (CI installs the full list).
sudo apt install -y protobuf-compiler libfuse3-dev pkg-config

cargo build --workspace          # capped to 2 jobs by .cargo/config.toml
```

**1. Pair the devices (once).** On the host, start the pairing listener (port 50052);
it prints a one-time 6-digit code that expires in 60 s:

```bash
./target/debug/nexus-agent pair-mode --display-name "dell"
# on the client:
./target/debug/nexus-mount pair --host 192.168.1.50 --code 123456
```

**2. Serve and mount.** First run generates `agent.json` (device id + auth token) and
`cert.pem`/`key.pem` under `~/.config/nexus/` (or `$NEXUS_CONFIG_DIR`).

```bash
# Host
./target/debug/nexus-agent serve --serve-dir ~/nexus-test-share --port 50051
# add --enable-streaming to also stream the screen

# Client (paired device, found via mDNS)
mkdir -p ~/nexus-mount
./target/debug/nexus-mount mount --discover --mountpoint ~/nexus-mount
# or by address, with the token and cert from the host:
TOKEN=$(python3 -c "import json;print(json.load(open('$HOME/.config/nexus/agent.json'))['auth_token'])")
./target/debug/nexus-mount mount --remote https://127.0.0.1:50051 --mountpoint ~/nexus-mount \
    --token "$TOKEN" --ca-cert "$HOME/.config/nexus/cert.pem"

ls ~/nexus-mount && echo "edited from the Dell" > ~/nexus-mount/some-file.txt   # writes through
fusermount3 -u ~/nexus-mount
```

**3. Stream the screen** (Linux viewer shown; the Android app does the same):

```bash
./target/debug/nexus-agent serve --enable-streaming --fps 30
./target/debug/nexus-viewer --discover --trusted
```

Other agent commands: `nexus-agent list-peers`, `nexus-agent discover`.

Writes carry a vector clock; if two devices edit the same file independently the
host keeps both. See [ADR 0005](docs/adr/0005-vector-clock-conflict-detection.md)
and [ADR 0006](docs/adr/0006-fuse-read-write-mount.md).

## Android

- **Viewer app** (`android/`): works today for Layer 1; build with Android Studio or `./gradlew assembleDebug`.
- **Agent as an Android host**: `cargo ndk -t arm64-v8a build --release -p nexus-agent` works again (streaming is excluded on Android). Packaging it in an app still needs the storage work in Layer 2 (SAF), see [ADR 0017](docs/adr/0017-layer-2-android-file-access.md).

## Security note

The data plane is **authenticated and encrypted** (token over self-signed TLS, ADR 0004),
and devices now join through a **one-time code with attempt limits and IP rate limiting**
(ADR 0013). It is still **LAN-trust only**:

- one flat shared secret per agent, stored in plaintext in the agent's config
- no per-device revocation or key rotation
- unknown client certificates are rejected at the gRPC layer, not the TLS layer (issue #41)
- the Android viewer presents no client certificate; it authenticates by token only

Fine for your own LAN, not hardened for hostile networks.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Before a PR:
`cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
