# ADR 0017: Layer 2 — Android file access (Kotlin client first, host behind a `Storage` trait)

## Status

Accepted. Supersedes the unfinished `saf_bridge` / `nexus-android-browser` scaffolding.

## Context

"Layer 2" had been described three different ways: the README said "mount a phone's
storage on a Linux laptop", `saf_bridge.rs` said "the host appears as a document
provider in the OS picker", and `nexus-android-browser` said "expose the agent's
file store through SAF". These are different features:

| Feature | Android is | Needs |
|---|---|---|
| A. The tablet's file picker shows a Linux host's files | **client** | an Android `DocumentsProvider` that calls `FileService` |
| B. The Dell mounts the phone's storage | **host** | an agent serving scoped storage through SAF |

The scaffolding (about 900 lines in `crates/agent/src/saf_bridge.rs` plus the stub
crate `crates/android-browser`) was never compiled: it sat behind
`cfg(target_os = "android")`, `nexus-agent` could not be built for Android at all
(it depended unconditionally on `nexus-stream`, whose PipeWire dependency
`libspa-sys` does not cross-compile), its JNI symbol names matched neither each other
nor the app's package (`com.vectorzero.nexus`), and no Kotlin side existed.

## Decision

1. **Feature A is built first, in Kotlin only.** The Android app already generates
   `FileService` stubs from `crates/proto/proto` and has `GrpcClient`, `TrustStore`
   and `HostStore`. A `DocumentsProvider` maps onto existing RPCs
   (`queryChildDocuments` to `ListDir`, `queryDocument` to `Stat`, `openDocument` to
   `ReadFile`/`WriteFile`). Random-access reads use
   `StorageManager.openProxyFileDescriptor` (API 26 = our `minSdk`). No Rust, JNI or
   NDK work is needed.
2. **Feature B comes later, behind a `Storage` trait** extracted from
   `FileServiceImpl` (list, stat, read-range, write, delete, rename). The local
   filesystem is the first implementation; an Android implementation can be a Kotlin
   foreground service or a Rust core with a narrow I/O callback. That choice is
   deferred until A ships.
3. **The Rust SAF scaffolding is deleted** (`saf_bridge.rs`, `crates/android-browser`).
   It is in git history if wanted.
4. **`nexus-stream` is a non-Android dependency of `nexus-agent`**
   (`[target.'cfg(not(target_os = "android"))'.dependencies]`). On Android,
   `serve --enable-streaming` returns an error instead of failing to build.
5. **CI checks the Android target** (`android-check` job: `cargo ndk -t arm64-v8a check
   -p nexus-agent -p nexus-migrate`), so Linux-only code cannot leak into Android
   builds unnoticed again.

## Consequences

- The tablet file-picker feature no longer depends on fixing Android Rust builds, and
  can ship independently.
- The agent builds for Android again; `nexus-migrate` needed one import fix
  (`jni::Outcome`) found by the new CI job.
- Android hosting (B) remains unbuilt. Until then, the only proven Android host is the
  `adb shell` experiment recorded in the README, which bypasses scoped storage.
- Vector-clock logic may exist twice (Rust host, Kotlin client). Shared test vectors
  should keep them consistent.
- Naming: the provider is `NexusDocumentsProvider` (not "Mocument").

## Alternatives considered

- **Finish the Rust JNI bridge.** Rejected for A: it adds a native layer between two
  things that already speak gRPC and Kotlin, and the three-way JNI name mismatch shows
  how easily it goes wrong without a compiler checking it.
- **Make Android a FUSE client.** Not possible (ADR 0001).
- **Do A and B together.** Rejected: B is the harder, riskier half and would delay the
  visible win.
