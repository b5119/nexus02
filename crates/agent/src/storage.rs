/// Storage trait abstracting file operations for the Nexus device mesh.
/// 
/// Implementations:
///   - `StorageHost` — backed by `FileServiceImpl` (the native host agent)
///   - `StorageAndroid` — via JNI bridge (`crates/android-browser`), serving through SAF
///   
/// The signatures mirror the `nexus.fs.v1.FileService` gRPC service but use
/// simple Rust types so they can be called from any context — the host binary,
/// the Android bridge, or test suites.
pub trait Storage: Send + Sync {
    /// List children of `path` on the host.
    async fn list_dir(&self, path: &str) -> anyhow::Result<Vec<FileEntry>>;

    /// Stat `path` on the host. Returns `FileEntry` + found-flag.
    async fn stat(&self, path: &str) -> anyhow::Result<FileEntry>;

    /// Read `length` bytes at `offset` from `path`.
    async fn read_file(&self, path: &str, offset: u64, length: u64) -> anyhow::Result<Vec<u8>>;

    /// Write `data` to `path`, carrying the writer's vector clock.
    async fn write_file(
        &self,
        path: &str,
        data: &[u8],
        clock: &std::collections::HashMap<String, u64>,
        writer_device_id: &str,
    ) -> anyhow::Result<WriteResult>;

    /// Create a directory at `path`.
    async fn mkdir_file(
        &self,
        path: &str,
        clock: &std::collections::HashMap<String, u64>,
        writer_device_id: &str,
    ) -> anyhow::Result<MkdirResult>;

    /// Delete `path`.
    async fn delete_file(
        &self,
        path: &str,
        clock: &std::collections::HashMap<String, u64>,
        writer_device_id: &str,
    ) -> anyhow::Result<DeleteResult>;

    /// Rename `old_path` to `new_path`.
    async fn rename_file(
        &self,
        old_path: &str,
        new_path: &str,
        clock: &std::collections::HashMap<String, u64>,
        writer_device_id: &str,
    ) -> anyhow::Result<RenameResult>;
}

/// File entry returned by list_dir / stat.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub modified_unix: i64,
}

/// Result variants for file operations.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteResult {
    Applied,
    Stale,
    Conflict,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MkdirResult {
    Applied,
    Conflict,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeleteResult {
    Deleted,
    Stale,
    Conflict,
    NotFound,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RenameResult {
    Renamed,
    Stale,
    Conflict,
    NotFound,
}

/// A vector clock: device-id -> counter.
#[derive(Clone, Debug, Default)]
pub struct VectorClock {
    pub counters: std::collections::HashMap<String, u64>,
}
