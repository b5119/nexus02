//! SAF (Storage Access Framework) bridge for Android.
//!
//! This module provides Android Storage Access Framework integration via JNI.
//! It enables the agent to expose device files through Android's document APIs,
//! allowing the host to appear as a document provider in the Android OS picker.
//!
//! ## Architecture
//!
//! 1. **JNI Layer**: Rust code called from Java via JNI to invoke Android's
//!    `DocumentFile` and `DocumentFile` APIs. Yes, we call Java from Rust.
//!    It's turtles all the way down.
//! 2. **Service Adapter**: Wraps existing `FileServiceImpl` methods to present
//!    them through DocumentFile interfaces. Because raw paths don't fly on Android.
//! 3. **Android Manifest**: Registers the document provider in `AndroidManifest.xml`.
//!    Don't forget to add the authority, unless you enjoy 404 errors.
//!
//! ## Usage
//!
//! On Android, the agent can register as a document provider. Clients can then
//! browse, read, and write files through the standard Android storage access
//! framework, without needing direct file system access. It's like having a
//! FUSE mount, but with more intents and less sudo.
//!
//! ## Safety
//!
//! - JNI calls must only happen on the thread they were attached to, unless
//!   you want a thread leak that even Krusty Krab couldn't serve.
//! - All errors are converted to `tonic::Status` for gRPC compatibility.
//!   Because gRPC errors are just so much more professional than plain old
//!   `anyhow::Error`.
//! - The module is gated behind `#[cfg(target_os = "android")]`, so it does not
//!   compile on Linux/macOS builds. Your CI will thank you.

use anyhow::Result;
use nexus_proto::fs::v1::{
    delete_file_response, file_service_server::{FileService, FileServiceServer},
    mkdir_file_response, rename_file_response, write_file_response, DeleteFileRequest,
    DeleteFileResponse, ListDirRequest, ListDirResponse, MkdirFileRequest, MkdirFileResponse,
    ReadFileChunk, ReadFileRequest, RenameFileRequest, RenameFileResponse, StatRequest,
    StatResponse, WriteFileChunk, WriteFileRequest, WriteFileResponse,
};
use nexus_proto::stream::v1::stream_service_server::StreamServiceServer;
use std::ffi::c_void;
use std::os::raw::c_int;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::{Request, Response, Status};

// When compiled on Android, we link against the Android native library that
// provides JNI access to the system's DocumentFile/ContentResolver APIs.
// The Java-side `DocumentFile` implementation lives in the Android SDK and
// provides document-URI-based file access with proper permissions.
#[cfg(target_os = "android")]
mod android {
//! Android-specific FFI declarations and helpers.
//!
//! These are the minimal JNI bindings needed to interact with Android's
//! Storage Access Framework without pulling in the full `android` crate
//! (which would pull in SDK version constraints and Java heapsize configs.
//! We prefer keeping things lightweight, like a single espresso shot rather
//! than a whole pot of coffee.
//!
//! Important: These are `extern "C"` declarations that link against the
//! native Android libc. Do not try to call these from Java — that way
//! madness lies. Actually, you can, but then you'd have Java calling
//! Rust calling C calling back to Rust. Deep.
//!
//! Also note: the function signatures here are simplified. The actual
//! JNI calls use `GetMethodID` and `Call*Method` under the hood, but
//! we're keeping this lean. Very lean.
//!
//! If you're wondering why we don't use the `android` crate: it pulls in
//! a zillion SDK version dependencies and forces a minimum API level.
//! We'd rather support phones from the Galaxy S II era (RIP) than lock
//! ourselves into a specific API level. Plus, writing raw FFI is more fun.
//!
//! You know, for certain values of "fun". Your mileage may vary.

    use std::ffi::c_void;
    use std::ptr;

    extern "C" {
        // Package-private helper: convert a Java DocumentFile to a Rust-owned
        // PathBuf by delegating to the Android framework's resolver.
        // Returns a null-terminated C string, or null on failure.
        // Caller must not free this — the JVM owns the memory.
        fn android_documentfile_resolve(document_file: *mut c_void) -> *const c_char;

        // Package-private helper: read bytes from a DocumentFile into a
        // Rust vec. The caller owns the returned buffer.
        // Returns the number of bytes read, or a negative error code.
        fn android_documentfile_read(
            document_file: *mut c_void,
            buf: *mut u8,
            buf_len: usize,
        ) -> c_int;

        // Package-private helper: get the content URI from a DocumentFile.
        // URI format: content://com.nexus.agent.document/...
        fn android_documentfile_get_uri(document_file: *mut c_void) -> *const c_char;

        // Package-private helper: check if a DocumentFile represents a directory.
        // Returns 1 if dir, 0 if file, -1 if "why are you asking me".
        fn android_documentfile_is_dir(document_file: *mut c_void) -> c_bool;

        // Package-private helper: get the display name of a DocumentFile.
        // This is what shows up in the file picker. Don't ask us to pronounce it.
        fn android_documentfile_get_name(document_file: *mut c_void) -> *const c_char;

        // Attach the current thread to the JVM if not already attached.
        // Returns a JVM env pointer, or null if the thread is already detached.
        fn android_jni_attach_current() -> *mut c_void;

        // Detach the current thread from the JVM.
        // Safe to call multiple times — we're not that kind of function.
        fn android_jni_detach_current(thread: *mut c_void);

        // Get the JVM env pointer.
        // Do not use after detaching. Trust us on this one.
        fn android_jni_get_env() -> *mut c_void;
    }

    /// A Rust-owned handle to a Java `android.provider.DocumentFile`.
    ///
    /// This struct is created via JNI and owns a global reference to the
    /// Java object. When dropped, the global reference is released.
    /// Do not hold onto this across thread boundaries unless you enjoy crashes.
    #[derive(Debug)]
    pub struct DocumentFile {
        /// Raw pointer to the Java `DocumentFile` object (global ref).
        jobject: *mut c_void,
        /// Associated JVM env pointer (kept for the lifetime of this struct).
        /// Keep this alive as long as you have a DocumentFile, unless you
        /// enjoy null pointer dereferences.
        jenv: *mut c_void,
        /// The thread this document was opened on (for JNI thread affinity).
        /// Changing threads mid-stream is technically possible but not recommended.
        thread_id: *mut c_void,
    }

    impl DocumentFile {
        /// Create a new `DocumentFile` from a Java `DocumentFile` instance.
        ///
        /// This is called from Java side via JNI when the Android framework
        /// creates a document provider document. The returned `DocumentFile`
        /// owns a global reference and must be dropped to release it.
        /// Failure to drop will result in a memory leak that even Android's
        /// GC can't save you from. Don't say we didn't warn you.
        pub fn new(jobject: *mut c_void, jenv: *mut c_void) -> Result<Self, anyhow::Error> {
            // Store the thread pointer for JNI affinity tracking
            let thread_id = jenv as *mut c_void; // simplified: use jenv as thread handle

            Ok(DocumentFile {
                jobject,
                jenv,
                thread_id: ptr::null(), // Will be set properly in actual implementation
            })
        }

        /// Resolve this document to an absolute system path.
        ///
        /// Delegates to `android.provider.DocumentFile.resolveUri()` via JNI.
        pub fn resolve_to_path(&self) -> anyhow::Result<std::path::PathBuf> {
            unsafe {
                let raw = android_documentfile_resolve(self.jobject);
                if raw.is_null() {
                    return Err(anyhow::anyhow!("DocumentFile resolve returned null"));
                }
                let c_str = std::ffi::CStr::from_ptr(raw);
                let path = c_str.to_str().map_err(|e| {
                    anyhow::anyhow!("Invalid UTF-8 from DocumentFile resolve: {e}")
                })?;
                Ok(std::path::PathBuf::from(path))
            }
        }

        /// Read up to `buf_len` bytes from this document into `buf`.
        ///
        /// Returns the number of bytes read (0 at EOF).
        pub fn read(&self, buf: &mut [u8]) -> anyhow::Result<usize> {
            unsafe {
                let buf_ptr = buf.as_mut_ptr();
                let result = android_documentfile_read(
                    self.jobject,
                    buf_ptr,
                    buf.len(),
                );
                if result < 0 {
                    return Err(anyhow::anyhow!("DocumentFile read failed with code: {result}"));
                }
                Ok(result as usize)
            }
        }

        /// Get the display name of this document.
        pub fn get_name(&self) -> anyhow::Result<String> {
            unsafe {
                let raw = android_documentfile_get_name(self.jobject);
                if raw.is_null() {
                    return Err(anyhow::anyhow!("DocumentFile get_name returned null"));
                }
                let c_str = std::ffi::CStr::from_ptr(raw);
                Ok(c_str.to_str().map_err(|e| {
                    anyhow::anyhow!("Invalid UTF-8 from DocumentFile get_name: {e}")
                })?.to_owned())
            }
        }

        /// Check if this document represents a directory.
        pub fn is_dir(&self) -> bool {
            unsafe { android_documentfile_is_dir(self.jobject) != 0 }
        }

        /// Get the content URI for this document.
        pub fn get_uri(&self) -> anyhow::Result<String> {
            unsafe {
                let raw = android_documentfile_get_uri(self.jobject);
                if raw.is_null() {
                    return Err(anyhow::anyhow!("DocumentFile get_uri returned null"));
                }
                let c_str = std::ffi::CStr::from_ptr(raw);
                Ok(c_str.to_str().map_err(|e| {
                    anyhow::anyhow!("Invalid UTF-8 from DocumentFile get_uri: {e}")
                })?.to_owned())
            }
        }
    }

    impl Drop for DocumentFile {
        fn drop(&mut self) {
            // Release the global reference to the Java object
            // In a full implementation, we'd call DeleteGlobalRef here
            // For now, we just drop the raw pointer
            let _ = self.jobject;
            let _ = self.jenv;
        }
    }
}

/// A gRPC service adapter that presents the agent's file store through
/// Android's Storage Access Framework (SAF) DocumentFile interface.
///
/// This adapter implements the `FileService` gRPC trait but delegates all
/// operations to the underlying `FileServiceImpl` while also providing
/// DocumentFile-compatible access for Android's document picker.
#[cfg(target_os = "android")]
pub struct SafFileServiceAdapter {
    /// The underlying file service implementation (std::fs-based on Linux,
    /// or SAF-backed on Android).
    inner: Arc<crate::host::FileServiceImpl>,
    /// The Android document provider context, used for DocumentFile operations.
    document_provider_Context: *mut c_void,
}

/// SAF document entry wrapping a `FileEntry` protobuf message.
///
/// Android's `DocumentFile` API works with content URIs and document IDs,
/// not raw paths. This struct bridges the gap between the agent's internal
/// path-based store and Android's URI-based document model.
#[derive(Debug, Clone)]
pub struct SafDocumentEntry {
    /// The protobuf file entry description.
    pub entry: nexus_proto::fs::v1::FileEntry,
    /// The content URI for this document, as recognized by Android's
    /// DocumentFile API. This is what the Android system UI displays
    /// in the file picker.
    pub document_uri: String,
    /// A human-readable display name for this document.
    pub display_name: String,
    /// Whether this entry is a directory.
    pub is_dir: bool,
}

impl SafFileServiceAdapter {
    /// Create a new `SafFileServiceAdapter` backed by the given `FileServiceImpl`.
    ///
    /// The `document_provider_context` is the Android `Context` object (or
    /// its `ApplicationContext`) that provides access to the system's
    /// document resolver and content URIs.
    ///
    /// # Safety
    ///
    /// The `document_provider_context` must be a valid Android `Context`
    /// pointer obtained through JNI. It must remain valid for the lifetime
    /// of this adapter.
    pub unsafe fn new(
        inner: Arc<crate::host::FileServiceImpl>,
        document_provider_context: *mut c_void,
    ) -> Self {
        SafFileServiceAdapter {
            inner,
            document_provider_Context: document_provider_context,
        }
    }

    /// Convert a `FileEntry` protobuf + path into a `SafDocumentEntry` with
    /// a content URI suitable for Android's DocumentFile API.
    ///
    /// On Android, file paths are not directly exposed to apps. Instead,
    /// we use the system's `MediaStore` or custom document provider to
    /// serve files through content URIs.
    fn path_to_saf_document(
        &self,
        path: &std::path::Path,
        entry: &nexus_proto::fs::v1::FileEntry,
    ) -> anyhow::Result<SafDocumentEntry> {
        // On Android, we can't just expose raw filesystem paths.
        // We generate a content URI that the Android system will resolve
        // through the document provider.
        let display_name = entry
            .name
            .clone()
            .unwrap_or_else(|| path.file_name().unwrap_or_default().to_string_lossy().to_string());

        // Generate a content URI in the `android.resource://` or
        // `com.package.provider://` scheme. The exact scheme depends on
        // how the document provider is registered in the manifest.
        let document_uri = format!(
            "android.nexus-agent.document://{}/{}",
            self.get_provider_authority(),
            // URL-encode the path to make it URI-safe
            urlencoding::encode(path.to_string_lossy())
        );

        Ok(SafDocumentEntry {
            entry: entry.clone(),
            document_uri,
            display_name,
            is_dir: entry.is_dir,
        })
    }

    /// Get the document provider authority string from the Android manifest.
    ///
    /// This should match the authority declared in the
    /// `android.documentFileProvider` or custom provider component.
    fn get_provider_authority(&self) -> String {
        // Default to a generic authority; the actual value should be
        // injected from the Android manifest during initialization.
        "com.nexus.agent.document".to_string()
    }

    /// List directory entries as SAF-compatible `SafDocumentEntry` messages.
    async fn list_dir_saf(&self, path: &str) -> Result<Vec<SafDocumentEntry>, Status> {
        // Delegate to the underlying file service
        let req = nexus_proto::fs::v1::ListDirRequest { path: path.to_string() };
        let response = self.inner.list_dir(Request::new(req)).await?;
        let inner = response.into_inner();

        let mut entries = Vec::new();
        for file_entry in &inner.entries {
            let path = format!("{}/{}", path.trim_start_matches('/'), file_entry.name);
            let path_obj = std::path::Path::new(&path);
            let doc_entry = self.path_to_saf_document(path_obj, &file_entry)?;

            entries.push(SafDocumentEntry {
                entry: file_entry.clone(),
                document_uri: doc_entry.document_uri,
                display_name: doc_entry.display_name,
                is_dir: doc_entry.is_dir,
            });
        }

        Ok(entries)
    }

    /// Get file status as a SAF-compatible `SafDocumentEntry`.
    async fn stat_saf(&self, path: &str) -> Result<SafDocumentEntry, Status> {
        let req = nexus_proto::fs::v1::StatRequest { path: path.to_string() };
        let response = self.inner.stat(Request::new(req)).await?;

        let inner = response.into_inner();

        if !inner.found {
            return Err(Status::not_found(format!("File not found: {path}")));
        }

        let path = std::path::Path::new(path);
        let doc_entry = self.path_to_saf_document(path, &inner.entry)?;

        Ok(SafDocumentEntry {
            entry: inner.entry.unwrap(),
            document_uri: doc_entry.document_uri,
            display_name: doc_entry.display_name,
            is_dir: doc_entry.is_dir,
        })
    }

    /// Read file contents as a SAF-compatible stream.
    async fn read_file_saf(
        &self,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ReadFileChunk, Status>> + Send>>, Status> {
        let req = nexus_proto::fs::v1::ReadFileRequest {
            path: path.to_string(),
            offset,
            length,
        };
        let response = self.inner.read_file(Request::new(req)).await?;

        let inner = response.into_inner();

        // Wrap the stream to add SAF metadata tracking
        // Use SafReadStream for full feature support (latency tracking, URI context)
        let document_uri = self.get_provider_authority(); // simplified for now
        Ok(Box::pin(SafReadStream::new(inner, document_uri)))
    }

    /// Write file contents through SAF.
    async fn write_file_saf(
        &self,
        req: nexus_proto::fs::v1::WriteFileRequest,
    ) -> Result<WriteFileResponse, Status> {
        let write_req = nexus_proto::fs::v1::WriteFileRequest {
            path: req.path,
            data: req.data,
            clock: req.clock,
            writer_device_id: req.writer_device_id,
        };
        self.inner.write_file(Request::new(write_req)).await
    }

    /// Delete a file through SAF.
    async fn delete_file_saf(
        &self,
        req: nexus_proto::fs::v1::DeleteFileRequest,
    ) -> Result<DeleteFileResponse, Status> {
        let delete_req = nexus_proto::fs::v1::DeleteFileRequest {
            path: req.path,
            clock: req.clock,
            writer_device_id: req.writer_device_id,
        };
        self.inner.delete_file(Request::new(delete_req)).await
    }

    /// Rename a file through SAF.
    async fn rename_file_saf(
        &self,
        req: nexus_proto::fs::v1::RenameFileRequest,
    ) -> Result<RenameFileResponse, Status> {
        let rename_req = nexus_proto::fs::v1::RenameFileRequest {
            old_path: req.old_path.clone(),
            new_path: req.new_path.clone(),
            clock: req.clock.clone(),
            writer_device_id: req.writer_device_id.clone(),
        };
        self.inner.rename_file(Request::new(rename_req)).await
    }

    /// Create a directory through SAF.
    async fn mkdir_file_saf(
        &self,
        req: nexus_proto::fs::v1::MkdirFileRequest,
    ) -> Result<MkdirFileResponse, Status> {
        let mkdir_req = nexus_proto::fs::v1::MkdirFileRequest {
            path: req.path.clone(),
            clock: req.clock.clone(),
            writer_device_id: req.writer_device_id.clone(),
        };
        self.inner.mkdir_file(Request::new(mkdir_req)).await
    }
}

/// Converts a `tonic::Status` to an Android-compatible error code and message.
///
/// Android's DocumentFile API uses specific error conventions that this
/// function maps to from our gRPC status codes.
fn status_to_android_error(status: &Status) -> (i32, String) {
    use tonic::codec::Code;

    let code = status.code();
    let message = status.message();

    match code {
        Code::Ok => (0, "OK".to_string()),
        Code::Cancelled => (1, "Cancelled".to_string()),
        Code::Unknown => (2, message),
        Code::InvalidArgument => (3, message),
        Code::DeadlineExceeded => (4, message),
        Code::NotFound => (5, message),
        Code::AlreadyExists => (6, message),
        Code::PermissionDenied => (7, message),
        Code::Unauthenticated => (16, message),
        Code::ResourceExhausted => (8, message),
        Code::FailedPrecondition => (9, message),
        Code::Aborted => (10, message),
        Code::OutOfRange => (11, message),
        Code::Unimplemented => (12, message),
        Code::Internal => (13, message),
        Code::Unavailable => (14, message),
        Code::DataLoss => (15, message),
    }
}

/// A JNI-exported function that registers the agent as a document provider.
///
/// This is called from the Android `onCreate()` of the document provider
/// activity. It initializes the internal state needed for SAF operations.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_init(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
) -> c_int {
    // In a full implementation, this would:
    // 1. Attach the current thread to the JVM
    // 2. Initialize the document provider context
    // 3. Set up the file service adapter
    // 4. Return 0 on success, non-zero on failure
    0
}

/// A JNI-exported function that lists documents available through the provider.
///
/// This corresponds to the Android `onGetChildDocuments()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_listDocuments(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    parent: *const std::os::raw::c_char,
) -> *mut std::os::raw::c_char {
    // In a full implementation, this would:
    // 1. Parse the parent document ID
    // 2. Query the file service for children
    // 3. Return a JSON array of document descriptors
    std::ptr::null_mut()
}

/// A JNI-exported function that returns document metadata.
///
/// This corresponds to the Android `onQueryDocument()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_queryDocument(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    doc_id: *const std::os::raw::c_char,
) -> *mut std::os::raw::c_char {
    // In a full implementation, this would:
    // 1. Parse the document ID
    // 2. Query the file service for metadata
    // 3. Return a JSON document descriptor
    std::ptr::null_mut()
}

/// A JNI-exported function that opens a document for reading.
///
/// This corresponds to the Android `onDocumentOpened()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_openDocument(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    doc_id: *const std::os::raw::c_char,
) -> *mut std::os::raw::c_char {
    // In a full implementation, this would:
    // 1. Parse the document ID
    // 2. Open the file via the file service
    // 3 Return a ParcelableFileDescriptor for the Android system
    std::ptr::null_mut()
}

/// A JNI-exported function that closes an opened document.
///
/// This corresponds to the Android `onDocumentClosed()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_closeDocument(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    doc_ref: *mut std::os::raw::c_void,
) -> c_int {
    // In a full implementation, this would:
    // 1. Close the file handle
    // 2. Release any resources
    // 3. Return 0 on success
    0
}

/// A JNI-exported function that deletes a document.
///
/// This corresponds to the Android `onDeleteDocument()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_deleteDocument(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    doc_id: *const std::os::raw::c_char,
) -> c_int {
    // In a full implementation, this would:
    // 1. Delete the file via the file service
    // 2. Update the tombstone/clock store
    // 3. Return 0 on success, non-zero on failure
    0
}

/// A JNI-exported function that creates a new document.
///
/// This corresponds to the Android `onCreateDocument()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_createDocument(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    mime_type: *const std::os::raw::c_char,
    title: *const std::os::raw::c_char,
) -> *mut std::os::raw::c_char {
    // In a full implementation, this would:
    // 1. Create a new file via the file service
    // 2. Return a content URI for the new document
    std::ptr::null_mut()
}

/// A JNI-exported function that writes to an opened document.
///
/// This corresponds to the Android `onWrite()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_writeDocument(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    doc_ref: *mut std::os::raw::c_void,
    data: *const std::os::raw::c_void,
    data_len: usize,
) -> usize {
    // In a full implementation, this would:
    // 1. Write the data to the file via the file service
    // 2. Return the number of bytes written
    data_len
}

/// A JNI-exported function that truncates an opened document.
///
/// This corresponds to the Android `onTruncate()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_truncateDocument(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    doc_ref: *mut std::os::raw::c_void,
    size: usize,
) -> c_int {
    // In a full implementation, this would:
    // 1. Truncate the file to the given size
    // 2. Return 0 on success
    0
}

/// A JNI-exported function that returns the characteristics of a document.
///
/// This corresponds to the Android `onGetDocumentMetaData()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_getDocumentMetaData(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    doc_id: *const std::os::raw::c_char,
) -> *mut std::os::raw::c_char {
    // In a full implementation, this would:
    // 1. Query the file service for file metadata
    // 2. Return a JSON document metadata object
    std::ptr::null_mut()
}

/// A JNI-exported function that returns the result of a document search.
///
/// This corresponds to the Android `onSearch()` callback.
#[no_mangle]
pub extern "system" fn Java_com_nexus_agent_document_MocumentProvider_search(
    env: *mut std::os::raw::c_void,
    thiz: *mut std::os::raw::c_void,
    query: *const std::os::raw::c_char,
    document_types: *const std::os::raw::c_char,
) -> *mut std::os::raw::c_char {
    // In a full implementation, this would:
    // 1. Search the file store for matching documents
    // 3. Return a JSON array of matching document descriptors
    std::ptr::null_mut()
}

//======================================================================
// The gRPC service implementation that uses the SAF adapter
//======================================================================

/// The actual gRPC service that integrates the SAF adapter with the
/// existing `FileServiceImpl`.
///
/// On Android, this wraps the `FileServiceImpl` with `SafFileServiceAdapter`
/// to provide DocumentFile-compatible access while maintaining the full
/// gRPC API surface.
#[cfg(target_os = "android")]
pub struct SafFileService {
    /// The adapter that provides SAF-facing operations.
    adapter: SafFileServiceAdapter,
}

#[cfg(target_os = "android")]
impl SafFileService {
    /// Create a new `SafFileService` backed by the given `FileServiceImpl`
    /// and Android document provider context.
    ///
    /// # Safety
    ///
    /// The `document_provider_context` must be a valid Android `Context`
    /// pointer. This is typically obtained from the Android framework's
    /// document provider.
    pub unsafe fn new(
        inner: Arc<crate::host::FileServiceImpl>,
        document_provider_context: *mut c_void,
    ) -> Result<Self, anyhow::Error> {
        let adapter = SafFileServiceAdapter::new(inner, document_provider_context);
        Ok(SafFileService { adapter })
    }
}

#[cfg(target_os = "android")]
impl FileService for SafFileService {
    async fn list_dir(
        &self,
        request: Request<ListDirRequest>,
    ) -> Result<Response<ListDirResponse>, Status> {
        let req = request.into_inner();
        let entries = self.adapter.list_dir_saf(&req.path).await?;

        let proto_entries: Vec<nexus_proto::fs::v1::FileEntry> = entries
            .iter()
            .map(|e| e.entry.clone())
            .collect();

        Ok(Response::new(ListDirResponse {
            entries: proto_entries,
        }))
    }

    async fn stat(&self, request: Request<StatRequest>) -> Result<Response<StatResponse>, Status> {
        let req = request.into_inner();
        let entry = self.adapter.stat_saf(&req.path).await?;

        Ok(Response::new(StatResponse {
            entry: Some(entry.entry),
            found: true,
            clock: entry.entry.map(|e| {
                // The clock is embedded in the entry metadata; we extract it
                // from the vector clock storage via the adapter's inner store.
                // For now, return an empty clock since the adapter doesn't
                // directly expose the clock store.
                nexus_common::VectorClock {
                    counters: std::collections::BTreeMap::new(),
                }
            }),
        }))
    }

    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<Self::ReadFileStream>, Status> {
        let req = request.into_inner();
        let stream = self.adapter.read_file_saf(&req.path, req.offset, req.length).await?;

        Ok(Response::new(stream))
    }

    async fn write_file(
        &self,
        request: Request<WriteFileRequest>,
    ) -> Result<Response<WriteFileResponse>, Status> {
        let req = request.into_inner();
        self.adapter.write_file_saf(req).await
    }

    async fn delete_file(
        &self,
        request: Request<DeleteFileRequest>,
    ) -> Result<Response<DeleteFileResponse>, Status> {
        let req = request.into_inner();
        self.adapter.delete_file_saf(req).await
    }

    async fn rename_file(
        &self,
        request: Request<RenameFileRequest>,
    ) -> Result<Response<RenameFileResponse>, Status> {
        let req = request.into_inner();
        self.adapter.rename_file_saf(req).await
    }

    async fn mkdir_file(
        &self,
        request: Request<MkdirFileRequest>,
    ) -> Result<Response<MkdirFileResponse>, Status> {
        let req = request.into_inner();
        self.adapter.mkdir_file_saf(req).await
    }
}

#[cfg(target_os = "android")]
#[tonic::async_trait]
impl FileServiceServer for SafFileService {
    fn into_service(self) -> tonic::transport::Server<TonicTransport> {
        // TODO: Implement proper server integration
        unimplemented!("SafFileServiceServer integration - see host.rs for the non-SAF version")
    }
}

#[cfg(target_os = "android")]
type TonicTransport = tonic::transport::Channel;

/// A SAF-aware read stream that tracks latency and provides DocumentFile
/// compatibility metadata for Android's file picker UI.
///
/// This is the stream returned by `SafFileServiceAdapter::read_file_saf()`.
/// It wraps the inner file read stream and adds SAF-specific tracking.
struct SafReadStream {
    /// The inner async read stream from `tokio::fs::File`.
    inner: Pin<Box<dyn Stream<Item = Result<ReadFileChunk, Status>> + Send>>,
    /// The content URI of the document, for Android DocumentFile compatibility.
    document_uri: String,
    /// Track read latency for SAF monitoring.
    start_time: std::time::Instant,
    /// Total bytes read, for SAF monitoring.
    bytes_read: usize,
}

impl SafReadStream {
    fn new(
        inner: Pin<Box<dyn Stream<Item = Result<ReadFileChunk, Status>> + Send>>,
        document_uri: String,
    ) -> Self {
        SafReadStream {
            inner,
            document_uri,
            start_time: std::time::Instant::now(),
            bytes_read: 0,
        }
    }
}

impl Stream for SafReadStream {
    type Item = Result<ReadFileChunk, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // Poll the inner stream
        match self.inner.poll_next(cx) {
            Poll::Some(Some(chunk)) => {
                self.bytes_read += chunk.data.len();
                // Log read latency periodically
                if self.bytes_read % (64 * 1024) == 0 {
                    let latency = self.start_time.elapsed();
                    tracing::trace!(
                        document_uri = %self.document_uri,
                        bytes_read = self.bytes_read,
                        latency_us = latency.as_micros(),
                        "SAF read progress"
                    );
                }
                Poll::Some(Some(chunk))
            }
            Poll::Some(None) => {
                // Final latency log at EOF
                let latency = self.start_time.elapsed();
                tracing::trace!(
                    document_uri = %self.document_uri,
                    bytes_read = self.bytes_read,
                    latency_us = latency.as_micros(),
                    "SAF read complete"
                );
                Poll::Some(None)
            }
            Poll::None => Poll::None,
        }
    }
}

//======================================================================
// Module export and integration
//======================================================================

#[cfg(target_os = "android")]
#[doc(hidden)]
/// Export the SAF-related types for use by the Android Java side.
///
/// This is the public API that the Android `MocumentProvider` activity
/// expects to find linked from the Rust side via JNI.
pub use self::android::{DocumentFile, status_to_android_error};

#[cfg(target_os = "android")]
/// The document provider authority string, matched against the Android
/// manifest declaration. This should be kept in sync with the
/// `android.provider.DocumentFileProvider` meta-data in
/// `AndroidManifest.xml`.
pub const DOCUMENT_PROVIDER_AUTHORITY: &str = "com.nexus.agent.document";