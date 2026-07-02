# ADR 0012: App-Cooperative Migration SDK

## Status

Accepted

## Context

Layers 0–3 (FUSE mount, conflict detection, GC) handle file-level
state. Layer 4 is the first to reach above the filesystem: apps that
link the Nexus SDK can opt in to migrating their own application
state across devices — contacts, bookmarks, game saves, settings,
whatever the app defines as "state".

This is NOT generic app migration. It only works for apps that
explicitly link this library and implement its interface. That
boundary is deliberate — Nexus is a filesystem mesh, not an app-
migration platform. The SDK is a convenience layer on top of the
existing transport and conflict-detection primitives.

## Design Questions

### 1. What does the core SDK trait look like?

**Resolution: One trait with required export/import, optional merge
and migrate_schema callbacks with default error-returning impls.**

```rust
pub trait MigratableApp: Send + Sync {
    /// Export the app's complete current state as a snapshot.
    fn export_state(&self) -> Result<AppSnapshot, MigrateError>;

    /// Import a fully resolved snapshot (conflicts already handled).
    fn import_state(&self, snapshot: AppSnapshot) -> Result<(), MigrateError>;

    /// Merge two concurrent values for a key with ConflictPolicy::AppMerge.
    /// Default: returns UnsupportedOperation error.
    fn merge(&self, _key: &str, _local: Vec<u8>, _remote: Vec<u8>)
        -> Result<Vec<u8>, MigrateError>
    { Err(MigrateError::UnsupportedOperation("AppMerge".into())) }

    /// Migrate a value from an older schema_version to the current one.
    /// Default: returns UnsupportedOperation error.
    fn migrate_schema(&self, _key: &str, _from_version: u32, _value: Vec<u8>)
        -> Result<Vec<u8>, MigrateError>
    { Err(MigrateError::UnsupportedOperation("migrate_schema".into())) }
}
```

**Rationale:**
- Required methods (`export_state`, `import_state`) are the minimal
  contract — an app that does nothing else can still participate.
- Optional methods have safe defaults (return error) so the trait
  doesn't force apps using LastWriteWins-only to implement merge.
- The trait is `Send + Sync` so the SDK can hand it across threads
  (the transport layer runs on a tokio runtime).
- An Android developer implementing the Kotlin interface writes the
  same four methods; the JNI bridge maps directly.

### 2. What is AppSnapshot?

**Resolution: A versioned, serializable container with per-key
conflict policies.**

```rust
pub struct AppSnapshot {
    pub device_id: String,
    pub vector_clock: BTreeMap<String, u64>,
    pub keys: HashMap<String, StateEntry>,
}

pub struct StateEntry {
    pub value: Vec<u8>,
    pub conflict_policy: ConflictPolicy,
    pub schema_version: u32,
}
```

`DeviceId` from nexus-common is avoided in the snapshot itself
because the snapshot may be serialized/deserialized by the JNI
layer where `DeviceId` wrapping `Uuid` adds friction. A plain
device-id string is more portable. The SDK converts to/from
`DeviceId` at the transport boundary.

`vector_clock` uses `BTreeMap<String, u64>` (same shape as
`VectorClock` in nexus-common) for deterministic ordering and
straightforward proto serialization.

`schema_version` is a u32 per key — not per snapshot — so an app
can evolve different subsystems independently.

### 3. Transport

**Resolution: New proto service MigrateService with two unary RPCs,
reusing the existing token auth.**

```protobuf
service MigrateService {
  rpc PushSnapshot(PushSnapshotRequest) returns (PushSnapshotResponse);
  rpc PullSnapshot(PullSnapshotRequest) returns (PullSnapshotResponse);
}
```

File lives in `crates/proto/proto/migrate_service.proto`.

`PushSnapshot` sends the local snapshot to a remote host. The host
resolves conflicts per-key using the declared policies and returns
any unresolved conflicts (KeepBoth keys that actually conflicted).
The transport client in the SDK calls this after `export_state`.

`PullSnapshot` requests the remote host's current snapshot. The
transport client calls this before `import_state`.

Both RPCs carry the existing auth token in the message body (the
gRPC interceptor from host.rs injects it into metadata; the SDK
transport client does the same).

**Why unary, not streaming:** App snapshots are expected to be
small (kilobytes to low megabytes — app preferences, bookmarks, a
few hundred contact entries). Streaming is not worth the complexity.

### 4. Android bindings

**Resolution: JNI via the `jni` crate, cfg-gated to
`target_os = "android"`. Kotlin-facing class exposes three methods.**

```rust
// Rust side, in crates/migrate/src/android/mod.rs
#[cfg(target_os = "android")]
pub mod android {
    // JNI functions registered via #[no_mangle] extern "C" fns
    // that delegate to a global MigratableApp impl.
}
```

```kotlin
// Kotlin side — what the Android developer writes
class MyApp : Application() {
    override fun onCreate() {
        super.onCreate()
        NexusMigrate.register("user_prefs", ConflictPolicy.LAST_WRITE_WINS)
        NexusMigrate.register("bookmarks", ConflictPolicy.APP_MERGE) {
            key, local, remote -> mergeBookmarks(local, remote)
        }
    }

    fun onSyncRequested() {
        // trigger export to paired devices
        val snapshot = NexusMigrate.export(this)
        // ... transport handled by SDK ...
    }
}
```

**Minimum viable Kotlin interface:**
- `NexusMigrate.register(key: String, policy: ConflictPolicy)`
- `NexusMigrate.export(context: Context): ByteArray`
- `NexusMigrate.import(context: Context, snapshot: ByteArray)`
- `NexusMigrate.setMergeHandler(key: String, handler: MergeHandler)`
- `NexusMigrate.setSchemaMigrationHandler(key: String, handler: SchemaMigrationHandler)`

The `register` call sets per-key policies. `export` serializes
`AppSnapshot` to bytes (the app stores them or the SDK transports
them). `import` deserializes and applies.

The JNI layer is deliberately thin — a global `MigratableApp`
implementation that the `#[no_mangle]` functions delegate to.
The Kotlin side manages its own storage and triggers.

**Note:** The JNI bindings are marked as a minimum-viable first
pass. Idiomatic Kotlin ergonomics (suspend functions, Flow, etc.)
are deferred to a follow-up PR.

### 5. Schema versioning

**Resolution: Each `StateEntry` has `schema_version: u32`. On import,
if the incoming version exceeds the app's declared version for that
key, `migrate_schema` is called. If unregistered, the import returns
an error.**

The app registers the current schema version per key at the same
time it registers the conflict policy:

```rust
sdk.register("bookmarks", ConflictPolicy::AppMerge, 2);
```

When an incoming snapshot has `schema_version: 1` (for example):

```
if incoming.version < app.current_version:
    // up-call to migrate_schema(key, incoming.version, incoming.value)
    // app returns the migrated bytes at the current schema version
    // SDK stores the result with app.current_version

if incoming.version > app.current_version:
    // error — incoming is from a newer app version; the app on this
    // device should be updated first

if incoming.version == app.current_version:
    // proceed normally through conflict resolution
```

If `migrate_schema` is not implemented (default returns error),
schema version mismatches fail hard rather than silently producing
corrupted state. This is the safe default: the app must explicitly
opt in to schema evolution.

### 6. JNI / Android bindings

**Resolution: A cfg-gated `android` module with six `#[no_mangle]`
functions, backed by its own global state (independent of the Rust-native
`MigrateSdk` / `MigratableApp` path).**

The JNI layer does NOT expose the `MigratableApp` trait to Kotlin.
Instead it manages a simple in-memory key-value store (`HashMap<String,
StateEntry>`) that the Kotlin side populates via `putStateValue` /
`getStateValue` and exports via `exportSnapshot` / `importSnapshot`.
This avoids passing JNI objects through the trait system, which is
fragile across GC cycles.

#### Naming convention

Package: `com.vectorzero.nexus.migrate`
Class: `NexusMigrate` (Kotlin `object`)

| Kotlin function                      | JNI symbol                                                              |
|--------------------------------------|-------------------------------------------------------------------------|
| `NexusMigrate.init(deviceId)`        | `Java_com_vectorzero_nexus_migrate_NexusMigrate_init`                   |
| `NexusMigrate.registerKey(...)`      | `Java_com_vectorzero_nexus_migrate_NexusMigrate_registerKey`            |
| `NexusMigrate.exportSnapshot()`      | `Java_com_vectorzero_nexus_migrate_NexusMigrate_exportSnapshot`         |
| `NexusMigrate.importSnapshot(...)`   | `Java_com_vectorzero_nexus_migrate_NexusMigrate_importSnapshot`         |
| `NexusMigrate.putStateValue(...)`    | `Java_com_vectorzero_nexus_migrate_NexusMigrate_putStateValue`          |
| `NexusMigrate.getValue(key)`         | `Java_com_vectorzero_nexus_migrate_NexusMigrate_getStateValue`          |

#### Kotlin companion file

`crates/migrate/android/NexusMigrate.kt` serves as the reference /
documentation contract. It is not compiled by Cargo; an Android app
copies or submodules this file into its own source tree.

```kotlin
object NexusMigrate {
    init { System.loadLibrary("nexus_migrate") }
    external fun init(deviceId: String)
    external fun registerKey(key: String, policy: Int, schemaVersion: Int)
    external fun exportSnapshot(): ByteArray
    external fun importSnapshot(snapshotBytes: ByteArray): String
    external fun putStateValue(key: String, value: ByteArray)
    external fun getValue(key: String): ByteArray
}
```

#### Crate-type change

The crate produces both `cdylib` (the `.so` loaded by `System.loadLibrary`
on Android) and `rlib` (the static library used by Rust-native consumers).
The `[lib]` section in `Cargo.toml`:

```toml
[lib]
name = "nexus_migrate"
crate-type = ["cdylib", "rlib"]
```

The `cdylib` output is `target/<triple>/release/libnexus_migrate.so`.
The `rlib` output continues to serve the existing `cargo test` and
`cargo build -p nexus-migrate` workflows unchanged.

#### Known limitations (JNI-specific)

- **AppMerge via single dispatch (issue #34):** The JNI layer currently
  does not register per-key merge callbacks. When an `importSnapshot`
  encounters a concurrent AppMerge key, the conflict is returned in the
  JSON response for the Kotlin side to handle manually. Per-key merge
  handler registration (matching the original `setMergeHandler` sketch)
  is deferred until after the JNI bindings exist, so the Kotlin-side
  impact is visible before the API is redesigned.
- **Schema migration callbacks:** Not yet exposed via JNI. If an incoming
  snapshot has an older schema version, the value is returned as a
  conflict rather than migrated automatically.
- **Transport layer:** `PushSnapshot` / `PullSnapshot` RPCs exist in the
  proto but are not wired into this SDK layer yet.
- **Idiomatic Kotlin ergonomics:** Suspend functions, `Flow`-based
  conflict streams, etc. are deferred to a follow-up PR.

## Consequences

- Apps that link the SDK get a principled state migration path
  using the same vector-clock conflict model as the filesystem layer.
- The three conflict policies cover the spectrum from fully
  automatic (LastWriteWins) through app-in-the-loop (AppMerge) to
  never-lose-data (KeepBoth), matching Layers 0–3's philosophy.
- Schema versioning adds complexity but is necessary for real apps
  that evolve; the default-fail behavior is the safe path.
- JNI bindings are a minimal pass; idiomatic Kotlin ergonomics
  require follow-up work.
- The crate is a new dependency that apps must opt into — it does
  not affect existing agents, FUSE mounts, or file-level conflict
  detection.

## References

- ADR 0005: Vector Clock Conflict Detection
- ADR 0008: Delete-vs-Edit Conflicts (tombstones)
- ADR 0011: Clock and Tombstone Garbage Collection
- Issue #31: Implement clock/tombstone GC per ADR 0011
