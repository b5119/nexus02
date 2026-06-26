# Contributing to Nexus

## Before you touch code

Read [docs/adr/](adr/) — especially ADR 0001 if you're about to
suggest "just make Android mount things too." It's been considered
and the answer is in there.

## Platform you're testing on

This project deliberately spans several OSes with different
capabilities. State your platform in your PR description:

| Platform | Can be HOST | Can be CLIENT (FUSE mount) |
|---|---|---|
| Linux | yes | yes (`fuser`) |
| macOS | yes | yes (macFUSE, untested) |
| Windows | yes | planned (WinFsp, not started) |
| Android | yes (within SAF limits) | no — see ADR 0001 |

## System prerequisites

Two non-Rust dependencies trip up first builds — install them before `cargo build`:

- **`protoc`** (Protocol Buffers compiler) — `tonic-build` invokes it to codegen
  the gRPC layer; without it `nexus-proto` fails to build.
- **A FUSE userspace library** — `nexus-fs` (the `nexus-mount` binary) links
  against it. Linux: `libfuse3-dev` + `pkg-config`. macOS: install
  [macFUSE](https://osxfuse.github.io/). Not needed if you only build
  `nexus-agent` (e.g. the Android host build, which excludes `nexus-fs`).

```bash
# Debian/Ubuntu
sudo apt install -y protobuf-compiler libfuse3-dev pkg-config
# Fedora
sudo dnf install -y protobuf-compiler fuse3-devel pkgconf-pkg-config
# Arch
sudo pacman -S protobuf fuse3 pkgconf
# macOS (Homebrew) — plus macFUSE from the link above
brew install protobuf pkg-config
```

A Rust toolchain via [rustup](https://rustup.rs/) (stable) is assumed.

## Build setup

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi  # only if touching Android build
cargo install cargo-ndk     # only if touching Android build
cargo build                  # default target, Linux/macOS/Windows
```

If you're on a memory-constrained machine (anything with 8GB RAM or
less, especially while running other heavy tools like Android Studio
alongside), keep `~/.cargo/config.toml`'s `[build] jobs` capped —
this repo's own `.cargo/config.toml` already sets `jobs = 2` for
anyone building inside this directory.

## Before opening a PR

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Commit messages

Reference the layer/milestone, e.g.:
```
fs: fix inode table leak on repeated lookups of same path
agent: add SAF-aware directory listing for Android host role
```

## Adding a new crate

If you're starting Layer 1 (streaming) or Layer 4 (migration SDK),
add it under `crates/` and register it in the root `Cargo.toml`
`[workspace] members` list. Use `nexus-common` for shared types rather
than redefining `DeviceId` etc. locally.
