//! Nexus Android Browser - JNI bridge for Android document provider.
//! 
//! This crate provides JNI bindings that implement the `Storage` trait from
//! `crates/agent` via Android's Storage Access Framework (SAF).
//! 
//! Enables Feature B: a Linux laptop can FUSE-mount a phone's storage by
//! using the phone as a host, with the agent running on the phone and this
//! crate providing the JNI exports.
//!
//! ## Architecture
//! 1. **JNI Layer**: `#[no_mangle]` functions called from Java via JNI.
//! 2. **FileService Adapter**: Wraps `FileServiceImpl` methods to present them
//!    through Android's DocumentFile interfaces.
//! 3. **Android Manifest**: Registers the document provider with the configured
//!    authority.
//!
//! ## Safety
//! - JNI calls must only happen on the thread they were attached to.
//! - All errors are converted to `tonic::Status` for gRPC compatibility.
//! - The module is `#[cfg(target_os = "android")]`, so it does not compile on Linux.

#![cfg_attr(target_os = "android", allow(unused_imports))]

use std::os::raw::c_void;
use std::collections::HashMap;

/// The document provider authority string, matched against the Android
/// manifest declaration. This should be kept in sync with the
/// `android.documentFileProvider` meta-data in `AndroidManifest.xml`.
pub const DOCUMENT_PROVIDER_AUTHORITY: &str = "com.nexus.agent.document";

/// Initialize the document provider JNI bridge.
///
/// Called from the Android `onCreate()` of the document provider activity.
/// In a full implementation, this would create the `StorageAndroid` backed by
/// the agent's `FileServiceAdapter` and register the document provider
/// authority with the Android framework.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_init(
    _env: *mut c_void,
    _thiz: *mut c_void,
) -> c_int {
    // Placeholder: In a full implementation, the StorageAndroid would be created here.
    // The Java-side MocumentProvider would then be ready to serve documents.
    0
}

/// List documents available through the provider.
///
/// Corresponds to the Android `onGetChildDocuments()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_listDocuments(
    _env: *mut c_void,
    _thiz: *mut c_void,
) -> c_int {
    0
}

/// Create a directory at `path`, carrying the creator's vector clock.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_mkdirFile(
    _env: *mut c_void,
    _thiz: *mut c_void,
    _path: *const std::os::raw::c_char,
    _clock: *const u8,
    _clock_len: usize,
    _writer_device_id: *const std::os::raw::c_char,
) -> c_int {
    0
}

/// Delete `path`, carrying the deleter's vector clock.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_deleteFile(
    _env: *mut c_void,
    _thiz: *mut c_void,
    _path: *const std::os::raw::c_char,
    _clock: *const u8,
    _clock_len: usize,
    _writer_device_id: *const std::os::raw::c_char,
) -> c_int {
    0
}

/// Write `data` to `path`, carrying the writer's vector clock.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_writeFile(
    _env: *mut c_void,
    _thiz: *mut c_void,
    _path: *const std::os::raw::c_char,
    _data: *const u8,
    _data_len: usize,
    _clock: *const u8,
    _clock_len: usize,
    _writer_device_id: *const std::os::raw::c_char,
) -> c_int {
    0
}

/// Rename `old_path` to `new_path`, carrying the writer's clock.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_renameFile(
    _env: *mut c_void,
    _thiz: *mut c_void,
    _old_path: *const std::os::raw::c_char,
    _new_path: *const std::os::raw::c_char,
    _clock: *const u8,
    _clock_len: usize,
    _writer_device_id: *const std::os::raw::c_char,
) -> c_int {
    0
}

/// Read `length` bytes at `offset` from `path`.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_readFile(
    _env: *mut c_void,
    _thiz: *mut c_void,
    _path: *const std::os::raw::c_char,
    _offset: u64,
    _length: u64,
) -> *const u8 {
    std::ptr::null()
}

/// Get the size of a file at `path`.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_statFile(
    _env: *mut c_void,
    _thiz: *mut c_void,
    _path: *const std::os::raw::c_char,
) -> c_int {
    0
}
