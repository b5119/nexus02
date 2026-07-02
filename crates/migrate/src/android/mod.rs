//! Android JNI bindings for nexus-migrate.
//!
//! This module exposes the migration SDK to Kotlin/Java Android apps
//! via JNI. Each `#[no_mangle] extern "C"` function maps to a method
//! on the Kotlin `NexusMigrate` companion object.
//!
//! The JNI layer holds a global reference to a callback object that
//! the Kotlin side registers. This is a minimum-viable first pass;
//! idiomatic Kotlin ergonomics (suspend functions, Flow, etc.) are
//! deferred to a follow-up PR.

use jni::objects::{JClass, JString, JValue};
use jni::sys::{jbyteArray, jstring};
use jni::JNIEnv;

use crate::snapshot::{AppSnapshot, ConflictPolicy, StateEntry};

// ── Global callback state ────────────────────────────────────────────
// The Kotlin side registers a single global callback object. All JNI
// calls delegate through it. This avoids passing JNI objects through
// the Rust trait system, which is fragile across GC cycles.

static CALLBACKS: std::sync::OnceLock<Box<dyn AndroidCallbacks + Send + Sync>> =
    std::sync::OnceLock::new();

/// Trait the Kotlin side implements via JNI callbacks.
pub trait AndroidCallbacks: Send + Sync {
    fn export_state(&self) -> Vec<u8>;
    fn import_state(&self, data: &[u8]);
    fn merge(&self, key: &str, local: &[u8], remote: &[u8]) -> Vec<u8>;
    fn migrate_schema(&self, key: &str, from_version: u32, value: &[u8]) -> Vec<u8>;
}

// ── JNI entry points ─────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn Java_com_nexus_migrate_NexusMigrate_nativeRegisterKey(
    mut env: JNIEnv,
    _class: JClass,
    key: JString,
    policy: jni::sys::jint,
    schema_version: jni::sys::jint,
) {
    let key_str: String = env
        .get_string(&key)
        .expect("failed to get key string")
        .into();
    let policy = match policy {
        0 => ConflictPolicy::LastWriteWins,
        1 => ConflictPolicy::AppMerge,
        2 => ConflictPolicy::KeepBoth,
        _ => {
            tracing::warn!("unknown conflict policy from Kotlin: {policy}");
            return;
        }
    };
    let reg = crate::KeyRegistration {
        policy,
        schema_version: schema_version as u32,
    };
    crate::register_key_internal(&key_str, reg);
    tracing::debug!("registered key: {key_str} policy={policy:?}");
}

#[no_mangle]
pub extern "C" fn Java_com_nexus_migrate_NexusMigrate_nativeExport(
    mut env: JNIEnv,
    _class: JClass,
) -> jbyteArray {
    let snapshot = crate::export_snapshot();
    let bytes = serde_json::to_vec(&snapshot).unwrap_or_default();
    let output = env
        .byte_array_from_slice(&bytes)
        .expect("failed to create byte array");
    output.into_raw()
}

#[no_mangle]
pub extern "C" fn Java_com_nexus_migrate_NexusMigrate_nativeImport(
    mut env: JNIEnv,
    _class: JClass,
    snapshot_bytes: jbyteArray,
) {
    let bytes = env
        .convert_byte_array(&snapshot_bytes)
        .expect("failed to read byte array");
    let snapshot: AppSnapshot = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to deserialize snapshot: {e}");
            return;
        }
    };
    let _ = crate::import_snapshot(snapshot);
}
