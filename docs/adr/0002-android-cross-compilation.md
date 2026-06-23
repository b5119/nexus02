# ADR 0002: Android cross-compilation via cargo-ndk

## Status
Proposed — setup steps below are not yet executed on the dev machine.

## Context
`nexus-agent` needs to run on Android as a native binary (no JVM/JNI
bridge for the core logic — just a plain ARM64 executable invoked by
a thin Android app wrapper running it as a foreground service).
Rust's standard toolchain doesn't cross-compile to Android out of the
box; it needs the Android NDK's clang toolchain as the linker.

## Decision
Use `cargo-ndk` to manage the cross-compilation, rather than hand-rolling
linker flags. Setup sequence (run once on the Dell):

```bash
# 1. Install the Android NDK (via Android Studio's SDK Manager, or standalone)
#    Record the install path — typically ~/Android/Sdk/ndk/<version>/

# 2. Install cargo-ndk
cargo install cargo-ndk

# 3. Add the Android targets to rustup
rustup target add aarch64-linux-android armv7-linux-androideabi

# 4. Build the agent crate specifically for Android
#    (NOT the workspace default — fs/ is excluded automatically since
#    it's not in scope for Android per ADR 0001)
cargo ndk -t arm64-v8a -t armeabi-v7a -o ./target/android build --release -p nexus-agent
```

The `.cargo/config.toml` linker paths in this repo are placeholders
pointing at `.ndk/...` relative to `$HOME` — update them to match
wherever the NDK actually lands after step 1, or override via
`ANDROID_NDK_HOME` if cargo-ndk picks that up directly (check
`cargo ndk --help` for the version installed, behavior has changed
across releases).

## Open questions
- Does the agent run as a raw binary launched by a minimal Kotlin
  wrapper (foreground Service that just `exec`s it), or does it need
  to actually link against JNI to call SAF APIs directly? Current
  assumption: raw binary + a *very* thin Kotlin shim that handles SAF
  permission grants and passes resolved file descriptors down — this
  needs validating once Layer 1 (the FUSE path) is solid, not before.
- Not yet decided how the agent gets *installed* on the phone — likely
  bundled inside an APK as a JNI lib / raw asset, extracted on first
  run. Revisit once milestone 1 is proven and milestone 2 (Android host)
  starts.

## Update (post-implementation)

Cross-compilation was actually run on 2026-06-23 (cargo-ndk 4.1.2;
built against NDK 28.2.13676358, with 27.0.12077973 also installed —
both present via Android Studio's SDK Manager). Corrections to the steps
above:

1. **The `-o` flag does not apply to a `bin`-crate target.** The proposed
   command above uses `-o ./target/android`, which tells cargo-ndk to
   *collect* build artifacts into that directory. cargo-ndk's collection
   step only looks for `cdylib` outputs (`.so` files destined for an APK's
   `jniLibs/`). `nexus-agent` is intentionally a `[[bin]]` (a plain ARM
   executable `exec`'d by a wrapper, per this ADR), so it produces no
   `.so`, and cargo-ndk exits non-zero with:

   ```
   error: No usable artifacts produced by cargo
   error: Did you set the crate-type in Cargo.toml to include 'cdylib'?
   ```

   This is a post-build collection error, **not** a compile/link failure —
   the binaries are built successfully before it fires. The correct command
   for a binary crate omits `-o`:

   ```bash
   cargo ndk -t arm64-v8a -t armeabi-v7a build --release -p nexus-agent
   ```

   The compiled binaries then live at:

   ```
   target/aarch64-linux-android/release/nexus-agent
   target/armv7-linux-androideabi/release/nexus-agent
   ```

   (Copy them wherever you need manually; there is no `.so` to harvest.)
   If/when the agent is ever repackaged as a JNI shared library instead of
   a standalone binary, `-o` becomes relevant again — but that contradicts
   the "raw binary + thin Kotlin shim" assumption above and would be its
   own decision.

2. **`.cargo/config.toml` linker paths** were placeholders (`.ndk/...`
   relative to `$HOME`). They are now absolute paths into
   `~/Android/Sdk/ndk/28.2.13676358/...`. Note cargo config does not expand
   `~` or env vars in linker paths, so absolute paths are required; cargo-ndk
   also injects the linker via env vars at build time (env overrides config),
   so those entries mainly serve a bare `cargo build --target ...` without
   cargo-ndk.

3. **Output binaries confirmed as genuine Android ARM ELF** via `file` —
   distinguishable from a generic Linux-ARM build by the
   `/system/bin/linker64` / `/system/bin/linker` interpreter path (a
   generic Linux-ARM build would show `/lib/ld-linux-aarch64.so.1`).

## Update (real phone test result)

Ran the cross-compiled `arm64-v8a` binary on a real device (TECNO KL4,
Android 14 / SDK 34) via `adb shell`, serving `/sdcard/nexus-test` and
mounted successfully from the Dell over a `adb forward`'d port. Byte-exact
verified, including a chunk-boundary-spanning offset read — see the
project README's Status section for the full result.

One real bug surfaced and was fixed with **no code change**: under
`adb shell`, `$HOME` is `/`, which is read-only, so
`AgentConfig::load_or_create()` (in `crates/agent/src/config.rs`) failed
trying to write `/.config/nexus/agent.json`. The fix was simply to set
`NEXUS_CONFIG_DIR=/data/local/tmp/nexus-cfg` — the config-loading code
already checked for that env var first, specifically anticipating this
exact Android scenario. This confirms the original design decision (an
overridable config dir for non-standard-`$HOME` environments) was sound.
See ADR 0003 for the full write-up of that finding.

**This test does not retire the SAF open question above.** It succeeded
specifically because the `adb shell` user runs as uid 2000, which has
direct `sdcard_rw`/`sdcard_r` group membership and therefore bypasses
scoped storage entirely. A real packaged app runs under its own
per-app uid with no such access — SAF will be mandatory once the agent
moves from "manually run via adb" to "installed app." The foreground-
service wrapper and SAF integration remain open, unsolved problems.
