/**
 * NexusMigrate — Kotlin companion for the nexus-migrate JNI library.
 *
 * Loads `libnexus_migrate.so` and exposes six `external` functions that
 * map 1:1 to the Rust `#[no_mangle] extern "C"` symbols in
 * `crates/migrate/src/android/mod.rs`.
 *
 * ## Usage
 *
 * ```kotlin
 * class MyApp : Application() {
 *     override fun onCreate() {
 *         super.onCreate()
 *         NexusMigrate.init(deviceId = uniqueDeviceIdentifier(this))
 *         NexusMigrate.registerKey("bookmarks", 1, 1)  // AppMerge, v1
 *         NexusMigrate.registerKey("settings",  0, 1)  // LastWriteWins, v1
 *     }
 *
 *     fun onSyncTriggered() {
 *         NexusMigrate.putStateValue("bookmarks", exportBookmarks())
 *         val snapshot = NexusMigrate.exportSnapshot()
 *         // … send snapshot bytes to remote device …
 *     }
 *
 *     fun onRemoteSnapshotReceived(raw: ByteArray) {
 *         val conflictJson = NexusMigrate.importSnapshot(raw)
 *         if (conflictJson != "{}") {
 *             // handle conflicts from KeepBoth / AppMerge keys
 *         }
 *     }
 * }
 * ```
 *
 * ## Policy constants
 *
 * | Value | Policy           |
 * |-------|------------------|
 * | 0     | LastWriteWins    |
 * | 1     | AppMerge         |
 * | 2     | KeepBoth         |
 *
 * ## Thread safety
 *
 * All JNI functions acquire the same internal `Mutex`.  Calls from
 * multiple threads are serialised.  Keep `exportSnapshot` / `importSnapshot`
 * off the main thread.
 *
 * ## Known limitations
 *
 * - AppMerge resolution uses single-dispatch (issue #34); per-key merge
 *   handlers are a follow-up.
 * - Schema migration callbacks are not yet exposed via JNI.
 * - Transport (PushSnapshot / PullSnapshot RPCs) is not wired into this
 *   SDK layer yet.
 */
object NexusMigrate {

    init {
        System.loadLibrary("nexus_migrate")
    }

    // ── Lifecycle ──────────────────────────────────────────────────────

    /**
     * Initialise the JNI layer with a device identifier.
     * Must be called once before any other function.
     */
    external fun init(deviceId: String)

    // ── Key registration ───────────────────────────────────────────────

    /**
     * Register a key with its conflict policy and current schema version.
     *
     * @param key            Application-level key name (e.g. "bookmarks")
     * @param policy         0 = LastWriteWins, 1 = AppMerge, 2 = KeepBoth
     * @param schemaVersion  Current schema version for this key's values
     */
    external fun registerKey(key: String, policy: Int, schemaVersion: Int)

    // ── Snapshot export/import ─────────────────────────────────────────

    /**
     * Export the current local state as a JSON-encoded [AppSnapshot].
     *
     * @return JSON-encoded AppSnapshot as UTF-8 bytes,
     *         or null if the SDK is not initialized or serialization fails.
     */
    external fun exportSnapshot(): ByteArray?

    /**
     * Import a remote snapshot, resolve conflicts, and apply the result.
     *
     * @param snapshotBytes  UTF-8 JSON bytes of an [AppSnapshot] produced
     *                       by a peer's [exportSnapshot].
     * @return JSON object with any unresolved conflicts:
     *         `{"conflicts":[...],"schema_dropped_keys":["key1","key2"]}`
     *         `conflicts` lists keys with concurrent edits that could not be
     *         resolved automatically. `schema_dropped_keys` lists keys that
     *         were omitted from the merge because their schema version could
     *         not be handled (too new or migration unavailable).
     *         Returns `"{}"` when no conflicts remain and no keys were dropped.
     */
    external fun importSnapshot(snapshotBytes: ByteArray): String

    // ── Direct state access ────────────────────────────────────────────

    /**
     * Write a key's current value into the local state store.
     * The value is associated with the key's registered policy and schema
     * version (defaults to LastWriteWins / 0 if not registered).
     */
    external fun putStateValue(key: String, value: ByteArray)

    /**
     * Read a key's current value from the local state store.
     *
     * @return The stored bytes, or null if the key was never set, was removed
     *         by an import, or an error occurred. Check for null before use —
     *         do not assume an empty array.
     */
    external fun getStateValue(key: String): ByteArray?
}
