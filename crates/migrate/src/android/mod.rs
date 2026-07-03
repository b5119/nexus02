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

use jni::objects::{JByteArray, JClass, JString};
use jni::sys::jbyteArray;
use jni::EnvUnowned;

use crate::snapshot::{AppSnapshot, ConflictPolicy};

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
pub extern "C" fn Java_com_nexus_migrate_NexusMigrate_nativeRegisterKey<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    key: JString<'local>,
    policy: jni::sys::jint,
    schema_version: jni::sys::jint,
) {
    let key_str: String = unowned_env
        .with_env(|env| key.try_to_string(env))
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
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
pub extern "C" fn Java_com_nexus_migrate_NexusMigrate_nativeExport<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jbyteArray {
    let snapshot = crate::export_snapshot();
    let bytes = serde_json::to_vec(&snapshot).unwrap_or_default();
    unowned_env
        .with_env(|env| -> Result<_, jni::errors::Error> {
            env.byte_array_from_slice(&bytes)
                .map(|jba| jba.into_raw() as jbyteArray)
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "C" fn Java_com_nexus_migrate_NexusMigrate_nativeImport<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    snapshot_bytes: jbyteArray,
) {
    let bytes: Vec<u8> = unowned_env
        .with_env(|env| -> Result<_, jni::errors::Error> {
            let jba = unsafe { JByteArray::from_raw(env, snapshot_bytes) };
            env.convert_byte_array(&jba)
        })
        .resolve::<jni::errors::ThrowRuntimeExAndDefault>();
    let snapshot: AppSnapshot = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to deserialize snapshot: {e}");
            return;
        }
    };
    let _ = crate::import_snapshot(snapshot);
}
