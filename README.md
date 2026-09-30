# Nexus

Personal device mesh: unified file access and remote control across
your own devices, built from scratch in Rust.

This is **not** a generic "virtualize any device" tool — see
[docs/adr/0001-android-fuse-limitation.md](docs/adr/0001-android-fuse-limitation.md)
for why that idea doesn't hold up, and what Nexus does instead.

## Current milestone: Layer 1 (remote control / screen streaming)

Goal: stream the host's screen to a paired tablet/phone and forward touch/keyboard/mouse input back — full remote control.

```
┌─────────────┐   gRPC (StreamService)   ┌──────────────┐
│   Tablet      │ ─────────────────────> │  Dell/Linux    │
│ Nexus Viewer  │  H.264 video + input   │  nexus-agent   │
│  (viewer)     │ ───────────────────── │  (host)        │
└─────────────┘                         └──────────────┘
```

**Status: Layer 1 - 100% Complete ✅**

- [x] **Screen capture on Wayland** via portal PipeWire screencast (no X11 fallback).
  - Connects to the portal session's PipeWire remote (not the default instance) so buffers actually arrive.
  - Negotiates a real video format (BGRA/BGRx/RGBA/RGBx + size + framerate) and pumps the main loop so the screencast node starts streaming.
  - Verified: diagnostic harness shows ~96% non-black frames with real desktop pixel values.
- [x] **H.264 encoding** via `libx264` (ffmpeg-next) tuned for real-time:
  - IDR every ~0.5s (`keyint=15`), no B-frames (`bframes=0`), in-band SPS/PPS (`repeat-headers=1`), ~8 Mbps bitrate, `preset=ultrafast`, `tune=zerolatency`, `profile=baseline`, `vbv-bufsize=1000`, `vbv-maxrate=8000`, `ref=1`, `me=dia`, `subme=0`, `no-deblock=1`.
- [x] **Fixed encoder PTS bug**: the scaler (`sws_scale`) does not copy timestamps; every frame reached libx264 with `pts=0`, causing `non-strictly-monotonic PTS` spam and a flickering stream. Now the scaled frame inherits the source PTS and advances by the nominal frame period (~33 ms).
- [x] **Streaming gRPC** (bidirectional): `VideoFrame` from host, `InputEvent` from viewer.
- [x] **Android viewer app** (Kotlin, MediaCodec decoder, Material 3 UI):
  - Discovers hosts via mDNS (`_nexus._tcp`), pairs with a one-time 6-digit code (TLS trust-on-first-use).
  - **Touch mode**: absolute coordinates (tap = click).
  - **Pointer mode**: relative mouse (drag moves cursor, tap = left click, long-press = right click), two-finger scroll.
  - **Keyboard forwarding**: hardware/soft keyboard → Linux evdev keycodes.
  - **Auto-rotate + aspect fit/fill**: video letterboxes to stream aspect ratio; FAB toggles Fit ↔ Fill; stream survives rotation without full reconnect.
  - Polished UI: themed toolbar, host list with avatars, empty state, unpair confirmation, styled pairing screen.
- [x] **End-to-end tested**: tablet (IN-101, Android 14) paired with Dell (Wayland/GNOME, PipeWire), streaming 1920×1080 @ 30fps stable, touch/pointer/keyboard/scroll all functional.

**Not yet / open:**
- [ ] **Always-on host**: systemd user service / autostart so `serve --enable-streaming` runs on login; portal grant persistence (GNOME re-prompts per session currently).
- [ ] **Tablet auto-connect**: on app open, skip discover/pair and immediately stream to last-paired host.
- [ ] **Perf tuning**: adaptive bitrate/quality based on network, HW encoder (QSV/VAAPI) detection and fallback.
- [ ] **Clipboard sync** (text, image).
- [ ] **Multiple monitors** (select source, or composite).

### Layer 2 (filesystem virtualization / android-browser)

Goal: mount a phone's storage on a Linux laptop as a real, lazy-loaded
FUSE filesystem — `ls`, `cat`, `cp` all work against it like it's a
local directory, but nothing is actually copied until read.

Layer 2 Progress: **60% Complete**

- [x] **crates/android-browser crate** created with JNI exports for Android document provider
- [x] **SAF bridge module** (`crates/agent/src/saf_bridge.rs`) implemented with JNI FFI bindings
- [x] **Pairing improvements**: manual IP + 6-digit code + mDNS Discover button
- [ ] **Java/Kotlin MocumentProvider activity** - needs Android SDK API level fixes
- [ ] **AndroidManifest.xml** registration with document provider authority
- [ ] **SAF filesystem operations**: `listDir`, `stat`, `readFile`, `writeFile` via the adapter
- [ ] **Auto-connect on app start** using last-connected host preference

### Layer 3 (future - cross-device sync, AI features, etc.)

## Workspace layout

```
crates/
├── common/   shared types (DeviceId, FileEntry, errors)
├── proto/    gRPC schema (file_service.proto) + generated code
├── agent/    daemon — runs on every device, implements the HOST role
└── fs/       FUSE client — Linux/macOS only, implements the CLIENT/mount role
```

## Quickstart (Linux, milestone 1)

The data plane is authenticated + TLS-encrypted (shared secret over a
self-signed cert — see [docs/adr/0004](docs/adr/0004-shared-secret-auth-and-tls.md)).
On first run the agent generates a token and cert in its config dir
(`$HOME/.config/nexus/`, or `$NEXUS_CONFIG_DIR`); the client needs both.

```bash
# Prerequisites: protoc (gRPC codegen) and a FUSE lib (to mount). On Debian/Ubuntu:
#   sudo apt install -y protobuf-compiler libfuse3-dev pkg-config
# See CONTRIBUTING.md for other platforms.

# Build everything (capped to 2 parallel jobs — see .cargo/config.toml)
cargo build --workspace
```
