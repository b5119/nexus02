pub mod conflict;
pub mod schema;
pub mod snapshot;
pub mod transport;

#[cfg(target_os = "android")]
pub mod android;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

pub use snapshot::{AppSnapshot, ConflictEntry, ConflictPolicy, ConflictSet, StateEntry};

// ── Error type ────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("schema version mismatch for key '{key}': incoming={incoming}, local={local}")]
    SchemaMismatch {
        key: String,
        incoming: u32,
        local: u32,
    },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("unsupported operation: {0}")]
    UnsupportedOperation(String),
    #[error("callback failed: {0}")]
    CallbackFailed(String),
}

// ── Per-key registration ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct KeyRegistration {
    pub policy: ConflictPolicy,
    pub schema_version: u32,
}

// ── MigratableApp trait (ADR 0012) ────────────────────────────────────────

pub trait MigratableApp: Send + Sync {
    fn export_state(&self) -> Result<AppSnapshot, MigrateError>;

    fn import_state(&self, snapshot: AppSnapshot) -> Result<(), MigrateError>;

    fn merge(
        &self,
        _key: &str,
        _local: Vec<u8>,
        _remote: Vec<u8>,
    ) -> Result<Vec<u8>, MigrateError> {
        Err(MigrateError::UnsupportedOperation("AppMerge".into()))
    }

    fn migrate_schema(
        &self,
        _key: &str,
        _from_version: u32,
        _value: Vec<u8>,
    ) -> Result<Vec<u8>, MigrateError> {
        Err(MigrateError::UnsupportedOperation("migrate_schema".into()))
    }
}

// ── Global state (JNI bridge) ─────────────────────────────────────────────

static GLOBAL: OnceLock<Mutex<GlobalState>> = OnceLock::new();

struct GlobalState {
    app: Box<dyn MigratableApp>,
    registrations: HashMap<String, KeyRegistration>,
    device_id: String,
    vector_clock: nexus_common::VectorClock,
    store: HashMap<String, StateEntry>,
}

/// Initialise the global SDK state.  Must be called before any JNI entry
/// point.  Returns an error if already initialised.
pub fn init_global(app: Box<dyn MigratableApp>, device_id: String) -> Result<(), MigrateError> {
    let state = GlobalState {
        app,
        registrations: HashMap::new(),
        device_id,
        vector_clock: nexus_common::VectorClock::new(),
        store: HashMap::new(),
    };
    GLOBAL
        .set(Mutex::new(state))
        .map_err(|_| MigrateError::CallbackFailed("global SDK already initialised".into()))
}

fn with_global<F, T>(f: F) -> Result<T, MigrateError>
where
    F: FnOnce(&mut GlobalState) -> Result<T, MigrateError>,
{
    let guard = GLOBAL
        .get()
        .ok_or_else(|| MigrateError::CallbackFailed("global SDK not initialised".into()))?;
    let mut state = guard
        .lock()
        .map_err(|e| MigrateError::CallbackFailed(format!("lock poisoned: {e}")))?;
    f(&mut state)
}

/// Register a key with the global SDK.
pub fn register_key_internal(key: &str, registration: KeyRegistration) {
    let _ = with_global(|state| {
        state.registrations.insert(key.to_string(), registration);
        state.vector_clock.increment(&state.device_id);
        Ok(())
    });
}

/// Export a snapshot via the global SDK.
pub fn export_snapshot() -> AppSnapshot {
    with_global(|state| {
        let mut snapshot = state.app.export_state()?;
        snapshot.device_id = state.device_id.clone();
        state.vector_clock.increment(&state.device_id);
        snapshot.vector_clock = state.vector_clock.0.clone();
        Ok(snapshot)
    })
    .unwrap_or_else(|e| {
        tracing::warn!("export_snapshot failed: {e}");
        AppSnapshot {
            device_id: String::new(),
            vector_clock: BTreeMap::new(),
            keys: HashMap::new(),
        }
    })
}

/// Import a snapshot via the global SDK.
pub fn import_snapshot(snapshot: AppSnapshot) -> Result<ConflictSet, MigrateError> {
    with_global(|state| {
        let local = state.app.export_state()?;

        // ── schema version check — drop keys that cannot be handled ──
        let mut remote = snapshot;
        let schema_dropped_keys = filter_schema_keys(
            &mut remote,
            &state.registrations,
            &mut |key, from_version, old| {
                state
                    .app
                    .migrate_schema(key, from_version, old)
                    .map_err(|e| {
                        MigrateError::CallbackFailed(format!(
                            "migrate_schema failed for '{key}': {e}"
                        ))
                    })
            },
        );

        // ── resolve conflicts ──
        let (mut resolved_keys, mut conflict_set) = conflict::resolve_conflicts(&local, &remote)?;

        // ── handle AppMerge conflicts ──
        let mut i = 0;
        while i < conflict_set.conflicts.len() {
            let ce = &conflict_set.conflicts[i];
            if let Some(reg) = state.registrations.get(&ce.key) {
                if reg.policy == ConflictPolicy::AppMerge {
                    let merged = state
                        .app
                        .merge(&ce.key, ce.local_value.clone(), ce.remote_value.clone())
                        .map_err(|e| {
                            MigrateError::CallbackFailed(format!(
                                "merge failed for '{}': {e}",
                                ce.key
                            ))
                        })?;
                    resolved_keys.insert(
                        ce.key.clone(),
                        StateEntry {
                            value: merged,
                            conflict_policy: ConflictPolicy::AppMerge,
                            schema_version: ce.schema_version,
                        },
                    );
                    conflict_set.conflicts.swap_remove(i);
                    continue;
                }
            }
            i += 1;
        }

        // ── update vector clock & build resolved snapshot ──
        state
            .vector_clock
            .merge(&nexus_common::VectorClock(remote.vector_clock));
        let resolved = AppSnapshot {
            device_id: state.device_id.clone(),
            vector_clock: state.vector_clock.0.clone(),
            keys: resolved_keys,
        };

        state.app.import_state(resolved)?;
        conflict_set.schema_dropped_keys = schema_dropped_keys;
        Ok(conflict_set)
    })
}

/// Shared schema version check: for each key in `remote.keys`, drop keys
/// that are too new or need migration when migration fails. Returns the list
/// of dropped key names (removed from `remote.keys` in place).
fn filter_schema_keys<M>(
    remote: &mut AppSnapshot,
    registrations: &HashMap<String, KeyRegistration>,
    migrator: &mut M,
) -> Vec<String>
where
    M: FnMut(&str, u32, Vec<u8>) -> Result<Vec<u8>, MigrateError>,
{
    let mut schema_dropped_keys: Vec<String> = Vec::new();
    let mut keys_to_remove: Vec<String> = Vec::new();
    for (key, entry) in &mut remote.keys {
        let reg = match registrations.get(key) {
            Some(r) => r.clone(),
            None => continue,
        };
        match schema::check_schema_version(key, entry.schema_version, reg.schema_version) {
            Ok(schema::SchemaAction::Current) => {}
            Ok(schema::SchemaAction::Migrate { from_version, .. }) => {
                let old = std::mem::take(&mut entry.value);
                match migrator(key, from_version, old) {
                    Ok(migrated) => {
                        entry.value = migrated;
                        entry.schema_version = reg.schema_version;
                    }
                    Err(e) => {
                        tracing::warn!("schema migration failed for '{key}': {e} — dropping key");
                        schema_dropped_keys.push(key.clone());
                        keys_to_remove.push(key.clone());
                    }
                }
            }
            Err(e) => {
                tracing::warn!("schema mismatch for '{key}': {e} — dropping key");
                schema_dropped_keys.push(key.clone());
                keys_to_remove.push(key.clone());
            }
            Ok(schema::SchemaAction::TooNew { .. }) => {
                schema_dropped_keys.push(key.clone());
                keys_to_remove.push(key.clone());
            }
        }
    }
    for key in &keys_to_remove {
        remote.keys.remove(key);
    }
    schema_dropped_keys
}

/// Set a key's value in the global state store.
/// Uses the registered policy if the key exists, otherwise LastWriteWins.
pub fn put_state_value(key: &str, value: Vec<u8>) {
    if let Err(e) = with_global(|state| {
        let policy = state
            .registrations
            .get(key)
            .map(|r| r.policy)
            .unwrap_or(ConflictPolicy::LastWriteWins);
        let schema_version = state
            .registrations
            .get(key)
            .map(|r| r.schema_version)
            .unwrap_or(0);
        state.store.insert(
            key.to_string(),
            StateEntry {
                value,
                conflict_policy: policy,
                schema_version,
            },
        );
        Ok(())
    }) {
        tracing::warn!("put_state_value: {e}");
    }
}

/// Get a key's value from the global state store.
pub fn get_state_value(key: &str) -> Option<Vec<u8>> {
    match with_global(|state| Ok(state.store.get(key).map(|e| e.value.clone()))) {
        Ok(val) => val,
        Err(e) => {
            tracing::warn!("get_state_value: {e}");
            None
        }
    }
}

// ── MigrateSdk (Rust-native API) ─────────────────────────────────────────

pub struct MigrateSdk {
    app: Box<dyn MigratableApp>,
    registrations: HashMap<String, KeyRegistration>,
    device_id: String,
    vector_clock: nexus_common::VectorClock,
}

impl MigrateSdk {
    pub fn new(app: Box<dyn MigratableApp>, device_id: String) -> Self {
        Self {
            app,
            registrations: HashMap::new(),
            device_id,
            vector_clock: nexus_common::VectorClock::new(),
        }
    }

    pub fn register_key(&mut self, key: String, policy: ConflictPolicy, schema_version: u32) {
        self.registrations.insert(
            key,
            KeyRegistration {
                policy,
                schema_version,
            },
        );
        self.vector_clock.increment(&self.device_id);
    }

    pub fn export_snapshot(&mut self) -> Result<AppSnapshot, MigrateError> {
        let mut snapshot = self.app.export_state()?;
        snapshot.device_id = self.device_id.clone();
        self.vector_clock.increment(&self.device_id);
        snapshot.vector_clock = self.vector_clock.0.clone();
        Ok(snapshot)
    }

    pub fn import_snapshot(
        &mut self,
        mut remote: AppSnapshot,
    ) -> Result<ConflictSet, MigrateError> {
        let local = self.app.export_state()?;

        // ── schema version check — drop keys that cannot be handled ──
        let schema_dropped_keys = filter_schema_keys(
            &mut remote,
            &self.registrations,
            &mut |key, from_version, old| {
                self.app
                    .migrate_schema(key, from_version, old)
                    .map_err(|e| {
                        MigrateError::CallbackFailed(format!(
                            "migrate_schema failed for '{key}': {e}"
                        ))
                    })
            },
        );

        // ── resolve conflicts ──
        let (mut resolved_keys, mut conflict_set) = conflict::resolve_conflicts(&local, &remote)?;

        // ── handle AppMerge conflicts ──
        let mut i = 0;
        while i < conflict_set.conflicts.len() {
            let ce = &conflict_set.conflicts[i];
            if let Some(reg) = self.registrations.get(&ce.key) {
                if reg.policy == ConflictPolicy::AppMerge {
                    let merged = self
                        .app
                        .merge(&ce.key, ce.local_value.clone(), ce.remote_value.clone())
                        .map_err(|e| {
                            MigrateError::CallbackFailed(format!(
                                "merge failed for '{}': {e}",
                                ce.key
                            ))
                        })?;
                    resolved_keys.insert(
                        ce.key.clone(),
                        StateEntry {
                            value: merged,
                            conflict_policy: ConflictPolicy::AppMerge,
                            schema_version: ce.schema_version,
                        },
                    );
                    conflict_set.conflicts.swap_remove(i);
                    continue;
                }
            }
            i += 1;
        }

        // ── update vector clock & build resolved snapshot ──
        self.vector_clock
            .merge(&nexus_common::VectorClock(remote.vector_clock));
        let resolved = AppSnapshot {
            device_id: self.device_id.clone(),
            vector_clock: self.vector_clock.0.clone(),
            keys: resolved_keys,
        };

        self.app.import_state(resolved)?;
        conflict_set.schema_dropped_keys = schema_dropped_keys;
        Ok(conflict_set)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TestApp {
        store: Arc<std::sync::Mutex<HashMap<String, StateEntry>>>,
    }

    impl MigratableApp for TestApp {
        fn export_state(&self) -> Result<AppSnapshot, MigrateError> {
            let store = self.store.lock().unwrap();
            Ok(AppSnapshot {
                device_id: "test".into(),
                vector_clock: BTreeMap::from([("test".into(), 1u64)]),
                keys: store.clone(),
            })
        }

        fn import_state(&self, snapshot: AppSnapshot) -> Result<(), MigrateError> {
            let mut store = self.store.lock().unwrap();
            *store = snapshot.keys;
            Ok(())
        }

        fn merge(
            &self,
            _key: &str,
            local: Vec<u8>,
            remote: Vec<u8>,
        ) -> Result<Vec<u8>, MigrateError> {
            // simple merge: pick longer value
            if local.len() >= remote.len() {
                Ok(local)
            } else {
                Ok(remote)
            }
        }
    }

    fn make_sdk_with(store: Arc<std::sync::Mutex<HashMap<String, StateEntry>>>) -> MigrateSdk {
        let app = TestApp { store };
        let mut sdk = MigrateSdk::new(Box::new(app), "test-device".into());
        sdk.register_key("k1".into(), ConflictPolicy::LastWriteWins, 1);
        sdk.register_key("k2".into(), ConflictPolicy::AppMerge, 1);
        sdk.register_key("k3".into(), ConflictPolicy::KeepBoth, 1);
        sdk
    }

    fn make_sdk() -> MigrateSdk {
        make_sdk_with(Arc::new(std::sync::Mutex::new(HashMap::new())))
    }

    #[test]
    fn t1_export_round_trip() {
        let mut sdk = make_sdk();
        let snapshot = sdk.export_snapshot().unwrap();
        assert_eq!(snapshot.device_id, "test-device");
        assert!(!snapshot.vector_clock.is_empty());
    }

    #[test]
    fn t2_import_last_write_wins() {
        let mut sdk = make_sdk();
        let remote = AppSnapshot {
            device_id: "remote".into(),
            vector_clock: BTreeMap::from([("remote".into(), 1u64)]),
            keys: HashMap::from([(
                "k1".into(),
                StateEntry {
                    value: b"remote-value".to_vec(),
                    conflict_policy: ConflictPolicy::LastWriteWins,
                    schema_version: 1,
                },
            )]),
        };
        let conflicts = sdk.import_snapshot(remote).unwrap();
        assert!(conflicts.conflicts.is_empty());
    }

    #[test]
    fn t3_import_app_merge_resolves_conflict() {
        // Seed local with "k2" — value must be longer than remote's "short"
        // so TestApp::merge (picks longer value) returns the local one.
        let store = Arc::new(std::sync::Mutex::new(HashMap::from([(
            "k2".into(),
            StateEntry {
                value: b"local-value-here".to_vec(),
                conflict_policy: ConflictPolicy::AppMerge,
                schema_version: 1,
            },
        )])));
        let mut sdk = make_sdk_with(store);
        sdk.export_snapshot().unwrap();

        let remote = AppSnapshot {
            device_id: "remote".into(),
            vector_clock: BTreeMap::from([("remote".into(), 1u64), ("test-device".into(), 1u64)]),
            keys: HashMap::from([(
                "k2".into(),
                StateEntry {
                    value: b"short".to_vec(),
                    conflict_policy: ConflictPolicy::AppMerge,
                    schema_version: 1,
                },
            )]),
        };
        let conflicts = sdk.import_snapshot(remote).unwrap();
        assert!(conflicts.conflicts.is_empty());

        // Verify merged value was written to local state.
        // TestApp::merge picks the longer value, and
        // b"local-value-here" (15 bytes) > b"short" (5 bytes).
        let exported = sdk.export_snapshot().unwrap();
        let entry = exported.keys.get("k2").unwrap();
        assert_eq!(entry.value, b"local-value-here");
    }

    #[test]
    fn t4_import_keep_both_produces_conflict() {
        let store = Arc::new(std::sync::Mutex::new(HashMap::from([(
            "k3".into(),
            StateEntry {
                value: b"local-val".to_vec(),
                conflict_policy: ConflictPolicy::KeepBoth,
                schema_version: 1,
            },
        )])));
        let mut sdk = make_sdk_with(store);
        sdk.export_snapshot().unwrap();

        let remote = AppSnapshot {
            device_id: "remote".into(),
            vector_clock: BTreeMap::from([("remote".into(), 1u64), ("test-device".into(), 1u64)]),
            keys: HashMap::from([(
                "k3".into(),
                StateEntry {
                    value: b"remote-val".to_vec(),
                    conflict_policy: ConflictPolicy::KeepBoth,
                    schema_version: 1,
                },
            )]),
        };
        let conflicts = sdk.import_snapshot(remote).unwrap();
        assert_eq!(conflicts.conflicts.len(), 1);
        assert_eq!(conflicts.conflicts[0].key, "k3");
    }

    #[test]
    fn t5_schema_mismatch_drops_key() {
        let mut sdk = make_sdk();
        let remote = AppSnapshot {
            device_id: "remote".into(),
            vector_clock: BTreeMap::new(),
            keys: HashMap::from([(
                "k1".into(),
                StateEntry {
                    value: b"v".to_vec(),
                    conflict_policy: ConflictPolicy::LastWriteWins,
                    schema_version: 99,
                },
            )]),
        };
        let result = sdk.import_snapshot(remote).unwrap();
        assert_eq!(result.schema_dropped_keys, vec!["k1"]);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn t6_import_with_schema_migration() {
        struct MigratingApp {
            store: Arc<std::sync::Mutex<HashMap<String, StateEntry>>>,
        }

        impl MigratableApp for MigratingApp {
            fn export_state(&self) -> Result<AppSnapshot, MigrateError> {
                let store = self.store.lock().unwrap();
                Ok(AppSnapshot {
                    device_id: "test".into(),
                    vector_clock: BTreeMap::new(),
                    keys: store.clone(),
                })
            }

            fn import_state(&self, snapshot: AppSnapshot) -> Result<(), MigrateError> {
                let mut store = self.store.lock().unwrap();
                *store = snapshot.keys;
                Ok(())
            }

            fn migrate_schema(
                &self,
                _key: &str,
                _from_version: u32,
                value: Vec<u8>,
            ) -> Result<Vec<u8>, MigrateError> {
                let mut v = value;
                v.push(b'-');
                v.push(b'm');
                Ok(v)
            }
        }

        let store = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let app = MigratingApp {
            store: store.clone(),
        };
        let mut sdk = MigrateSdk::new(Box::new(app), "test-dev".into());
        sdk.register_key("k1".into(), ConflictPolicy::LastWriteWins, 2);

        let remote = AppSnapshot {
            device_id: "remote".into(),
            vector_clock: BTreeMap::new(),
            keys: HashMap::from([(
                "k1".into(),
                StateEntry {
                    value: b"old-schema".to_vec(),
                    conflict_policy: ConflictPolicy::LastWriteWins,
                    schema_version: 1,
                },
            )]),
        };
        let _ = sdk.import_snapshot(remote).unwrap();
        let exported = sdk.export_snapshot().unwrap();
        let entry = exported.keys.get("k1").unwrap();
        assert_eq!(entry.schema_version, 2);
        assert!(entry.value.ends_with(b"-m"));
    }

    #[test]
    fn t7_global_api_round_trip() {
        let store = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let app = TestApp {
            store: store.clone(),
        };
        init_global(Box::new(app), "global-dev".into()).unwrap();
        register_key_internal(
            "g1",
            KeyRegistration {
                policy: ConflictPolicy::LastWriteWins,
                schema_version: 1,
            },
        );
        let snapshot = export_snapshot();
        assert_eq!(snapshot.device_id, "global-dev");
        let conflicts = import_snapshot(snapshot).unwrap();
        assert!(conflicts.conflicts.is_empty());
    }
}
