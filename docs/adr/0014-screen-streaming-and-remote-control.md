# ADR 0014 — Screen Streaming and Remote Control

**Status:** Accepted  
**Date:** 2026-07-05  
**Drivers:** Remote desktop / remote control for LAN-connected devices

---

## 1. Architecture Overview

Two roles, symmetrical:

- **Host** (the device being controlled): captures screen, encodes H.264, serves frames via `StreamService` RPC, receives `InputEvent` RPCs and injects them into the local input subsystem.
- **Viewer** (the controlling device): receives H.264 frames, decodes and displays them, captures user input and sends `InputEvent` RPCs.

Any device can play either role. For this milestone:

| Role        | Platform  | Mechanism                                                              |
|-------------|-----------|------------------------------------------------------------------------|
| Host        | Linux     | X11 capture (`XShmGetImage`) + FFmpeg H.264 encode (QSV preferred, libx264 fallback) + uinput injection |
| Viewer      | Linux     | FFmpeg H.264 decode + winit/pixels display + keyboard/mouse capture   |
| Viewer      | Android   | MediaCodec H.264 decode + SurfaceView display + touch event capture    |
| Host        | Android   | **Deferred** — requires `MediaProjection` + `AccessibilityService`     |

Communication always uses the existing mTLS/token auth layer (same interceptor as `FileService`). No new authentication surface is introduced.

---

## 2. Transport Protocol

A new gRPC service lives in `crates/proto/proto/stream_service.proto`:

```protobuf
service StreamService {
  rpc RemoteControl(stream InputEvent) returns (stream VideoFrame);
}
```

### VideoFrame

| Field         | Type    | Description                                        |
|---------------|---------|----------------------------------------------------|
| sequence      | uint64  | Monotonically increasing frame counter             |
| timestamp_ms  | uint64  | Capture timestamp (host monotonic clock)            |
| width         | uint32  | Frame width in pixels                              |
| height        | uint32  | Frame height in pixels                             |
| data          | bytes   | Encoded H.264 NAL units                            |
| keyframe      | bool    | True if this frame is a keyframe (IDR)             |

### InputEvent

| Field       | Type   | Description                                         |
|-------------|--------|-----------------------------------------------------|
| event_type  | enum   | KEYBOARD = 0, MOUSE = 1, TOUCH = 2                  |
| key_code    | uint32 | Linux keycode (when type == KEYBOARD)                |
| x           | uint32 | X coordinate (mouse/touch), normalized to host res   |
| y           | uint32 | Y coordinate (mouse/touch), normalized to host res   |
| button      | enum   | NONE = 0, LEFT = 1, RIGHT = 2, MIDDLE = 3            |
| action      | enum   | PRESS = 0, RELEASE = 1, MOVE = 2                     |

### Auth

The `StreamService` RPCs are served on the same gRPC server (port 50051) and pass through the existing mTLS interceptor. There is no unauthenticated path. A host in stream mode only accepts connections from paired devices (same `peers.json` check as `FileService`).

---

## 3. Encoding Pipeline (Linux Host)

The encoding pipeline runs on the host in three stages:

### 3.1 Capture

- `XOpenDisplay(nullptr)` — connect to the default X11 display.
- `XGetWindowAttributes(display, root)` — get root window dimensions.
- `XShmCreateImage` + `XShmGetImage` — zero-copy shared-memory capture from the root window.
- Runs at a configurable frame rate (default 30 FPS, `--fps` flag) via `tokio::time::interval`.
- Output: raw BGRA pixel data.

### 3.2 Scaling

If the captured resolution exceeds 1920×1080, the frame is scaled down to 1080p using FFmpeg's `sws_scale`. Most remote-control scenarios do not need 4K, and downscaling saves bandwidth.

### 3.3 Encoding

- **Preferred:** QSV (`h264_qsv`) — Intel Quick Sync hardware encoding via `libmfx` or `oneVPL`.
- **Fallback:** `libx264` — software encoding, always available wherever FFmpeg is installed.
- Keyframe interval: every 60 frames (2 seconds at 30 FPS).
- Encoding is logged at startup: `INFO  encoder: using h264_qsv` or `INFO  encoder: using libx264`.

---

## 4. Input Injection (Linux Host)

The host creates virtual input devices via the `/dev/uinput` kernel interface:

- One virtual keyboard (`/dev/uinput` with `UI_SET_EVBIT(EV_KEY)` + `UI_SET_EVBIT(EV_SYN)`).
- One virtual mouse (`/dev/uinput` with `UI_SET_EVBIT(EV_KEY)` + `UI_SET_EVBIT(EV_REL)` for relative motion, or `EV_ABS` for absolute).

### Permission requirements

The host binary checks `/dev/uinput` accessibility at startup. If the device is not accessible, it returns a clear error:

```
ERROR /dev/uinput not accessible.
To use input injection:
  sudo usermod -a -G input $USER   # add user to input group
  # or run with CAP_SYS_ADMIN
  # or run as root
```

Do not silently fail. Do not panic.

### Event translation

| InputEvent                 | uinput event                        |
|----------------------------|-------------------------------------|
| KEYBOARD / PRESS           | `EV_KEY` + keycode + `EV_SYN`      |
| KEYBOARD / RELEASE         | `EV_KEY` + keycode (value=0) + `EV_SYN` |
| MOUSE / MOVE               | `EV_REL` REL_X/REL_Y               |
| MOUSE / PRESS (LEFT)       | `EV_KEY` BTN_LEFT + `EV_SYN`       |
| MOUSE / RELEASE (LEFT)     | `EV_KEY` BTN_LEFT (0) + `EV_SYN`   |
| TOUCH / PRESS              | `EV_ABS` ABS_X/ABS_Y (absolute) + `EV_KEY` BTN_TOUCH + `EV_SYN` |
| TOUCH / MOVE               | `EV_ABS` ABS_X/ABS_Y + `EV_SYN`    |
| TOUCH / RELEASE            | `EV_KEY` BTN_TOUCH (0) + `EV_SYN`  |

---

## 5. Decoding + Display (Linux Viewer)

- **Decode:** FFmpeg (`avcodec`) H.264 decoding — consistent with the encoding pipeline.
- **Window:** `winit` — cross-platform window management.
- **Rendering:** `pixels` crate — raw framebuffer display. Each decoded frame is copied into the pixel buffer and rendered via the winit event loop's `request_redraw()`.
- **Window title:** `"Nexus — <host_device_id>"`.
- **FPS counter:** displayed in the window title, updated every second.
- **Input capture:** winit `WindowEvent::KeyboardInput` and `WindowEvent::CursorMoved` / `MouseInput` are translated to `InputEvent` RPCs and sent to the host via the bidirectional stream.

---

## 6. Android Viewer

A Kotlin app in `android/` at the repository root.

### Technology choices

- **Decode:** `MediaCodec` (Android's built-in hardware H.264 decoder) — not FFmpeg. MediaCodec is faster, uses less battery, and has no licensing concerns on Android.
- **Display:** `SurfaceView` — MediaCodec can render decoded frames directly to a Surface (zero-copy path). Do not use `ByteBuffer` output mode.
- **Transport:** `io.grpc:grpc-kotlin-stub` + `grpc-okhttp` for the `StreamService` RPC.
- **Pairing:** Reuses the existing `PairService` proto — same protocol as the Rust client.
- **Min SDK:** 26 (Android 8.0) — `MediaCodec` H.264 decode is stable from API 21, but 26 provides better surface handling.
- **Target SDK:** 34 (Android 14).

### PairingActivity

- Text fields for host address (e.g. `192.168.1.x:50052`) and 6-digit pairing code.
- "Pair" button calls `PairService.RequestPair` via gRPC.
- On success: stores host cert + device ID in `SharedPreferences`.
- Shows confirmation: `"Paired with <device_id>"`.

### StreamSurface

- `SurfaceView` with a `MediaCodec` decoder configured for `"video/avc"`.
- Receives `VideoFrame` RPCs from the gRPC client.
- Feeds H.264 data to `MediaCodec`'s input buffer.
- MediaCodec renders decoded frames directly to the `Surface` (zero-copy).

### InputHandler

- `OnTouchListener` on the `SurfaceView`.
- Translates `MotionEvent` (`ACTION_DOWN` / `ACTION_UP` / `ACTION_MOVE`) to `InputEvent` RPCs.
- Coordinates are normalized to the host screen resolution (sent in the first `VideoFrame`).

### MainActivity

- Lists paired hosts from `SharedPreferences`.
- Tap a host → starts `StreamActivity`.

---

## 7. Security

- All streaming RPCs go through the existing gRPC auth interceptor (mTLS client cert or `x-nexus-token` header). No new authentication surface.
- A host in stream mode only accepts stream connections from paired devices — same `peers.json` check as `FileService`.
- The stream protocol does not add any new unauthenticated network-visible endpoint.
- Both the host binary (`nexus-agent stream-host`) and the viewer binary (`nexus-viewer`) require either a valid client cert from a paired device or a valid auth token.

---

## 8. Known Limitations (This Milestone)

| Limitation                | Reason                                                       |
|---------------------------|--------------------------------------------------------------|
| Android as host           | Requires `MediaProjection` + `AccessibilityService` — deferred to a future milestone |
| Wayland support           | X11 only for this milestone. Wayland requires PipeWire screen capture (different API) |
| Audio streaming           | Not implemented. No audio in the streaming protocol.         |
| Multi-monitor             | First monitor only. The host captures the root window of the default X11 display. |
| WAN / NAT traversal       | LAN-only. No WebRTC, TURN, or STUN. The viewer connects directly to the host's IP. |
| Adaptive bitrate          | Not implemented in the first pass. The encoder uses fixed quality / bitrate settings. |
| Clipboard sync            | Not implemented. Clipboard is not streamed between host and viewer. |
| File transfer             | Not part of this milestone. Use the existing `FileService` for file transfer. |
| Relative mouse mode       | The first pass uses absolute coordinates for touch/trackpad. Relative mode (for games) is deferred. |

---

## 9. Dependencies

The following system packages must be installed for the Rust streaming crate to build:

```bash
# Required for crates/stream
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev

# Already installed (X11 capture):
# libx11-dev libxext-dev libxshmfence-dev
```

For the Android app: Android Studio (or Android SDK 34 + build tools) is required to build the APK.

---

## 10. CLI Interface

```text
nexus-agent stream-host [--fps 30] [--quality medium]
    Start streaming this device's screen.
    Requires X11 display, /dev/uinput access, and FFmpeg.

nexus-viewer --host <address> [--trusted | --token <token>]
    Connect to a stream host and display its screen.
```

---

## 11. Test Plan

| Test | Description |
|------|-------------|
| T1   | `VideoFrame` proto serialization round-trip |
| T2   | `InputEvent` proto serialization round-trip |
| T3   | Encoder initializes without panic (QSV or libx264) |
| T4   | uinput device creation returns clear error when `/dev/uinput` not accessible |
| T5   | winit window creation in headless mode (`EventLoopBuilder::with_user_event()`) |
| T6   | Loopback stream: synthetic BGRA buffer → encode → receive → verify sequence number |
