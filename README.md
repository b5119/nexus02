# Nexus

Personal device mesh: unified file access and remote control across
your own devices, built from scratch in Rust.

This is **not** a generic "virtualize any device" tool — see
[docs/adr/0001-android-fuse-limitation.md](docs/adr/0001-android-fuse-limitation.md)
for why that idea doesn't hold up, and what Nexus does instead.

## A note on AI involvement

This project was **architected and directed by the maintainer** and **implemented
with Claude**. The split, concretely:

- **Human-directed:** the scoping, the feature sequencing (each milestone landed
  as a separate, reviewed iteration), the test strategy, the on-device hardware
  verification, and accepting or redirecting design proposals.
- **AI-generated under that direction:** the Rust implementation, and a good
  share of the design proposals themselves (reviewed before they landed).

Commits are authored under the maintainer's account; this note — together with
the commit history and the ADRs in [`docs/adr/`](docs/adr/) — is the canonical
record of how the work was split. It's stated up front because it's relevant to
how you should read the code and the claims here.

### Code review

Every PR is reviewed by two automated reviewers — [CodeRabbit](https://coderabbit.ai)
and GitHub Copilot — before it merges. Their findings are evaluated on merit,
not rubber-stamped: valid ones are fixed and the threads resolved, off-base ones
are declined with a reason. They've caught real issues here — for example a CI
credential-leak hardening gap (persisted GitHub token + unpinned actions, flagged
by CodeRabbit) and a security-relevant bug where `DeleteFile` masked a
path-escape rejection as a plain "not found" (flagged by Copilot, fixed + tested).
So bot review is a genuine part of the quality bar, alongside the CI gates
(build + test, fmt, clippy) and branch protection on `main`.

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

The data plane is authenticated + TLS-encrypted (shared secret over a
self-signed cert — see [docs/adr/0004](docs/adr/0004-shared-secret-auth-and-tls.md)).
On first run the agent generates a token and cert in its config dir
(`$HOME/.config/nexus/`, or `$NEXUS_CONFIG_DIR`); the client needs both.

```bash
# Prerequisites: protoc (gRPC codegen) and a FUSE lib (to mount). On Debian/Ubuntu:
#   sudo apt install -y protobuf-compiler libfuse3-dev pkg-config
# See CONTRIBUTING.md for other platforms.

# Build everything (capped to 2 parallel jobs — see .cargo/config.toml)
cargo build

# Terminal 1: run the host agent, serving a directory.
# First run generates agent.json (with the auth token) + cert.pem/key.pem
# under ~/.config/nexus/ and logs where they are.
./target/debug/nexus-agent --serve-dir ~/nexus-test-share --port 50051

# Grab the token and cert path the client will need:
TOKEN=$(python3 -c "import json;print(json.load(open('$HOME/.config/nexus/agent.json'))['auth_token'])")
CERT=$HOME/.config/nexus/cert.pem

# Terminal 2: mount it (note: https, plus --token and --ca-cert)
mkdir -p ~/nexus-mount
./target/debug/nexus-mount --remote https://127.0.0.1:50051 --mountpoint ~/nexus-mount \
    --token "$TOKEN" --ca-cert "$CERT"
# (--token / --ca-cert can also come from NEXUS_AUTH_TOKEN / NEXUS_CA_CERT.)

# Terminal 3: prove it works (read AND write — the mount is read-write)
ls ~/nexus-mount
cat ~/nexus-mount/some-file.txt
echo "edited from the Dell" > ~/nexus-mount/some-file.txt   # writes through to the host

# Unmount when done
fusermount3 -u ~/nexus-mount
```

Writes carry a vector clock; if two devices edit the same file independently
the host keeps both (`<name>.conflict-<device>-<ts>`) rather than losing one —
see [docs/adr/0005](docs/adr/0005-vector-clock-conflict-detection.md) and
[docs/adr/0006](docs/adr/0006-fuse-read-write-mount.md).

## Android (host role only — see ADR 0001)

Cross-compilation setup: see
[docs/adr/0002-android-cross-compilation.md](docs/adr/0002-android-cross-compilation.md).
Not yet wired into an actual installable app — currently just a
cross-compiled binary you can push via `adb` for testing.

## Status

- [x] Workspace scaffold
- [x] `FileService` proto (ListDir, Stat, ReadFile)
- [x] Host agent (Linux) — serves a local directory over gRPC
- [x] FUSE client (Linux) — mounts a remote agent's files (read-write)
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
- [x] **Data-plane auth + TLS** — shared-secret token over a self-signed
      cert; host rejects bad/missing tokens before touching the filesystem.
      Tested loopback (positive + wrong-token/no-token/wrong-cert negatives).
      See [docs/adr/0004](docs/adr/0004-shared-secret-auth-and-tls.md).
- [x] **Multi-writer conflict detection (vector clocks)** — every file carries
      a vector clock; concurrent edits are detected and BOTH kept
      (`.conflict-*`), never silently merged or lost. Proven at the protocol
      level and through the actual read-write mount (two independent client
      identities — two mounts on one machine — editing the same file → conflict
      file, original untouched). See
      [docs/adr/0005](docs/adr/0005-vector-clock-conflict-detection.md) +
      [docs/adr/0006](docs/adr/0006-fuse-read-write-mount.md). Reading a file
      then editing it no longer false-conflicts — the client syncs the clock on
      a read-intent open ([docs/adr/0007](docs/adr/0007-read-open-clock-sync.md)),
      while a blind overwrite of a changed file still conflicts. Remaining gaps
      named in the ADRs. Deletes are supported with **delete-vs-edit conflict
      detection** (tombstones — [docs/adr/0008](docs/adr/0008-delete-vs-edit-conflicts.md));
      rename is done ([docs/adr/0009](docs/adr/0009-rename-move-support.md));
      directory-level conflicts are still open.
- [ ] Full pairing / control plane (device identity, revocation, key rotation)
      — ADR 0004 is only the shared-secret step, not this
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

The data plane now has **shared-secret authentication over TLS** (ADR
0004): traffic is encrypted, and a client must present the agent's token
to read anything — the host rejects bad/missing tokens before touching
the filesystem. This is a real step up from the original "open and
plaintext" state.

It is **not** the full control plane, though: one flat secret per agent,
no pairing UX, no per-device revocation, no key rotation, and the token
sits in plaintext in the agent's config. Treat it as "authenticated,
encrypted LAN-trust" — fine for your own LAN, not hardened for hostile
networks. See [docs/adr/0004](docs/adr/0004-shared-secret-auth-and-tls.md)
for the precise threat model and the Status checklist for what's next.
