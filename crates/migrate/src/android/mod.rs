//! Android JNI bindings for nexus-migrate.
//!
//! Exposes six functions matching the Kotlin `NexusMigrate` companion object
//! (`com.vectorzero.nexus.migrate.NexusMigrate`).  The JNI layer manages its
//! own global state (device_id, vector clock, key registrations, key-value
//! store) independently of the Rust-native `MigrateSdk` / `MigratableApp` API.

use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jbyteArray, jint, jstring};
use jni::JNIEnv;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

use crate::snapshot::{AppSnapshot, ConflictEntry, ConflictPolicy, StateEntry};
use crate::KeyRegistration;

// ── JNI-global state ───────────────────────────────────────────────────

static JNI: OnceLock<Mutex<JniState>> = OnceLock::new();

struct JniState {
    device_id: String,
    vector_clock: nexus_common::VectorClock,
    registrations: HashMap<String, KeyRegistration>,
    store: HashMap<String, StateEntry>,
}

fn with_jni<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce(&mut JniState) -> Result<T, String>,
{
    let guard = JNI
        .get()
        .ok_or_else(|| "JNI state not initialised — call NexusMigrate.init() first".to_string())?;
    let mut state = guard
        .lock()
        .map_err(|e| format!("JNI lock poisoned: {e}"))?;
    f(&mut state)
}

fn local_snapshot(state: &JniState) -> AppSnapshot {
    AppSnapshot {
        device_id: state.device_id.clone(),
        vector_clock: state.vector_clock.0.clone(),
        keys: state.store.clone(),
    }
}

// ── Exportable conflict report ─────────────────────────────────────────

#[derive(Serialize)]
struct ConflictReport {
    conflicts: Vec<ConflictEntry>,
}

// ── JNI: init ──────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn Java_com_vectorzero_nexus_migrate_NexusMigrate_init(
    mut env: JNIEnv,
    _class: JClass,
    device_id: JString,
) {
    let device_id: String = match env.get_string(&device_id) {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::warn!("NexusMigrate.init: failed to read device_id string: {e}");
            return;
        }
    };

    let state = JniState {
        device_id,
        vector_clock: nexus_common::VectorClock::new(),
        registrations: HashMap::new(),
        store: HashMap::new(),
    };

    if JNI.set(Mutex::new(state)).is_err() {
        tracing::warn!("NexusMigrate.init: already initialised — call is a no-op");
    }
}

// ── JNI: registerKey ───────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn Java_com_vectorzero_nexus_migrate_NexusMigrate_registerKey(
    mut env: JNIEnv,
    _class: JClass,
    key: JString,
    policy: jint,
    schema_version: jint,
) {
    let key_str: String = match env.get_string(&key) {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::warn!("registerKey: failed to read key string: {e}");
            return;
        }
    };
    let policy = match policy {
        0 => ConflictPolicy::LastWriteWins,
        1 => ConflictPolicy::AppMerge,
        2 => ConflictPolicy::KeepBoth,
        _ => {
            tracing::warn!("registerKey: unknown conflict policy {policy} for key '{key_str}'");
            return;
        }
    };

    let _ = with_jni(|state| {
        state.registrations.insert(
            key_str.clone(),
            KeyRegistration {
                policy,
                schema_version: schema_version as u32,
            },
        );
        state.vector_clock.increment(&state.device_id);
        tracing::debug!("registered key '{key_str}' policy={policy:?} version={schema_version}");
        Ok(())
    });
}

// ── JNI: exportSnapshot ────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn Java_com_vectorzero_nexus_migrate_NexusMigrate_exportSnapshot(
    env: JNIEnv,
    _class: JClass,
) -> jbyteArray {
    let bytes = with_jni(|state| {
        state.vector_clock.increment(&state.device_id);
        let snapshot = local_snapshot(state);
        serde_json::to_vec(&snapshot).map_err(|e| format!("serialize snapshot: {e}"))
    })
    .unwrap_or_default();

    env.byte_array_from_slice(&bytes)
        .expect("exportSnapshot: failed to create byte array")
        .into_raw()
}

// ── JNI: importSnapshot ────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn Java_com_vectorzero_nexus_migrate_NexusMigrate_importSnapshot(
    env: JNIEnv,
    _class: JClass,
    snapshot_bytes: JByteArray,
) -> jstring {
    let bytes: Vec<u8> = match env.convert_byte_array(&snapshot_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("importSnapshot: failed to read byte array: {e}");
            return std::ptr::null_mut();
        }
    };

    let remote: AppSnapshot = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("importSnapshot: failed to deserialize snapshot: {e}");
            return std::ptr::null_mut();
        }
    };

    let report = with_jni(|state| {
        let local = local_snapshot(state);

        // ── schema version check ──
        let mut remote = remote;
        for (key, entry) in &mut remote.keys {
            if let Some(reg) = state.registrations.get(key) {
                let action = crate::schema::check_schema_version(
                    key,
                    entry.schema_version,
                    reg.schema_version,
                );
                match action {
                    Ok(crate::schema::SchemaAction::Current) => {}
                    Ok(crate::schema::SchemaAction::Migrate { from_version, .. }) => {
                        tracing::warn!(
                            "importSnapshot: schema migration needed for '{key}' \
                             v{from_version}→v{} — returning as conflict (issue #34)",
                            reg.schema_version,
                        );
                    }
                    Err(e) => {
                        tracing::warn!("importSnapshot: schema mismatch for '{key}': {e}");
                    }
                    _ => {}
                }
            }
        }

        // ── resolve conflicts ──
        let (resolved_keys, conflict_set) = crate::conflict::resolve_conflicts(&local, &remote)
            .map_err(|e| format!("resolve_conflicts: {e}"))?;

        // ── write resolved keys back to store ──
        state.store = resolved_keys;

        // ── merge vector clock ──
        state
            .vector_clock
            .merge(&nexus_common::VectorClock(remote.vector_clock));

        Ok(ConflictReport {
            conflicts: conflict_set.conflicts,
        })
    });

    let json = match report {
        Ok(r) => serde_json::to_string(&r).unwrap_or_else(|_| "{}".to_string()),
        Err(e) => {
            tracing::warn!("importSnapshot failed: {e}");
            "{}".to_string()
        }
    };

    env.new_string(&json)
        .unwrap_or_else(|e| {
            tracing::warn!("importSnapshot: failed to create return string: {e}");
            env.new_string("{}")
                .expect("fallback string creation failed")
        })
        .into_raw()
}

// ── JNI: putStateValue ─────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn Java_com_vectorzero_nexus_migrate_NexusMigrate_putStateValue(
    mut env: JNIEnv,
    _class: JClass,
    key: JString,
    value: JByteArray,
) {
    let key_str: String = match env.get_string(&key) {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::warn!("putStateValue: failed to read key string: {e}");
            return;
        }
    };
    let value_bytes: Vec<u8> = match env.convert_byte_array(&value) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("putStateValue: failed to read byte array: {e}");
            return;
        }
    };

    let _ = with_jni(|state| {
        let policy = state
            .registrations
            .get(&key_str)
            .map(|r| r.policy)
            .unwrap_or(ConflictPolicy::LastWriteWins);
        let schema_version = state
            .registrations
            .get(&key_str)
            .map(|r| r.schema_version)
            .unwrap_or(0);
        state.store.insert(
            key_str.clone(),
            StateEntry {
                value: value_bytes,
                conflict_policy: policy,
                schema_version,
            },
        );
        Ok(())
    });
}

// ── JNI: getStateValue ─────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn Java_com_vectorzero_nexus_migrate_NexusMigrate_getStateValue(
    mut env: JNIEnv,
    _class: JClass,
    key: JString,
) -> jbyteArray {
    let key_str: String = match env.get_string(&key) {
        Ok(s) => s.into(),
        Err(e) => {
            tracing::warn!("getStateValue: failed to read key string: {e}");
            return std::ptr::null_mut();
        }
    };

    let value = with_jni(|state| Ok(state.store.get(&key_str).map(|e| e.value.clone())))
        .ok()
        .flatten()
        .unwrap_or_default();

    env.byte_array_from_slice(&value)
        .expect("getStateValue: failed to create byte array")
        .into_raw()
}
