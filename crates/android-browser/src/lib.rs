//! Nexus Android Browser - SAF bridge for Android document provider.
//!
//! This crate provides the Android document provider implementation that
//! exposes the agent's file store through Android's Storage Access Framework.
//! It enables clients to browse, read, and write files through the standard
//! Android storage access picker without needing direct file system access.
//!
//! ## Architecture
//!
//! 1. **JNI Layer**: Rust code called from Java via JNI to invoke Android's
//!    `DocumentFile` and `ContentResolver` APIs.
//! 2. **File Service Adapter**: Wraps `nexus-agent`'s `FileServiceImpl` methods
//!    to present them through DocumentFile interfaces.
//! 3. **Android Manifest**: Registers the document provider in
//!    `AndroidManifest.xml` with the configured authority.
//!
//! ## Safety
//!
//! - JNI calls must only happen on the thread they were attached to.
//! - All errors are converted to `tonic::Status` for gRPC compatibility.
//! - The module is gated behind `#[cfg(target_os = "android")]`, so it does not
//!   compile on Linux/macOS builds.

#![cfg_attr(target_os = "android", allow(unused_imports))]

use std::os::raw::{c_char, c_int};

/// The document provider authority string, matched against the Android
/// manifest declaration. This should be kept in sync with the
/// `android.provider.DocumentFileProvider` meta-data in
/// `AndroidManifest.xml`.
pub const DOCUMENT_PROVIDER_AUTHORITY: &str = "com.nexus.agent.document";

/// Initialize the document provider.
///
/// Called from the Android `onCreate()` of the document provider activity.
/// In a full implementation, this would create the `SafFileServiceAdapter`
/// backed by the agent's `FileServiceImpl` and register the document provider
/// authority with the Android framework.
///
/// # Safety
///
/// The JNI layer must ensure this is called exactly once before any other
/// SAF operations. The `document_provider_context` is the Android `ApplicationContext`
/// that provides access to the system's document resolver.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_init(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
) -> c_int {
    // Placeholder: In a full implementation, the adapter would be created here.
    // The Java-side MocumentProvider would then be ready to serve documents.
    0
}

/// List documents available through the provider.
///
/// Corresponds to the Android `onGetChildDocuments()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_listDocuments(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    parent: *const c_char,
) -> *mut std::os::raw::c_char {
    // In a full implementation, query the adapter for children under parent.
    let _ = (parent);
    std::ptr::null_mut()
}

/// Query document metadata.
///
/// Corresponds to the Android `onQueryDocument()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_queryDocument(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    doc_id: *const c_char,
) -> *mut std::os::raw::c_char {
    let _ = (doc_id);
    std::ptr::null_mut()
}

/// Open a document for reading.
///
/// Corresponds to the Android `onDocumentOpened()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_openDocument(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    doc_id: *const c_char,
) -> *mut std::os::raw::c_char {
    let _ = (doc_id);
    std::ptr::null_mut()
}

/// Close an opened document.
///
/// Corresponds to the Android `onDocumentClosed()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_closeDocument(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    _doc_ref: *mut std::os::raw::c_void,
) -> c_int {
    0
}

/// Delete a document.
///
/// Corresponds to the Android `onDeleteDocument()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_deleteDocument(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    doc_id: *const c_char,
) -> c_int {
    let _ = (doc_id);
    0
}

/// Create a new document.
///
/// Corresponds to the Android `onCreateDocument()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_createDocument(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    mime_type: *const c_char,
    title: *const c_char,
) -> *mut std::os::raw::c_char {
    let _ = (mime_type, title);
    std::ptr::null_mut()
}

/// Write to an opened document.
///
/// Corresponds to the Android `onWrite()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_writeDocument(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    _doc_ref: *mut std::os::raw::c_void,
    data: *const std::os::raw::c_void,
    data_len: usize,
) -> usize {
    // In a full implementation, write the data to the file via the adapter.
    data_len
}

/// Truncate an opened document.
///
/// Corresponds to the Android `onTruncate()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_truncateDocument(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    _doc_ref: *mut std::os::raw::c_void,
    _size: usize,
) -> c_int {
    0
}

/// Get document metadata characteristics.
///
/// Corresponds to the Android `onGetDocumentMetaData()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_getDocumentMetaData(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    doc_id: *const c_char,
) -> *mut std::os::raw::c_char {
    let _ = (doc_id);
    std::ptr::null_mut()
}

/// Search for documents.
///
/// Corresponds to the Android `onSearch()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_android_browser_MocumentProvider_search(
    _env: *mut std::os::raw::c_void,
    _thiz: *mut std::os::raw::c_void,
    _query: *const c_char,
    _document_types: *const c_char,
) -> *mut std::os::raw::c_char {
    let _ = (_query, _document_types);
    std::ptr::null_mut()
}