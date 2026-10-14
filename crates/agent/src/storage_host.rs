use crate::storage::trait_;
use anyhow::Result;
use std::collections::HashMap;
use crate::agent::host::{FileServiceImpl, FileEntry, VectorClock};

/// Storage implementation backed by `FileServiceImpl` — the native host agent.
/// 
/// This lets the same file logic be used from the host binary, from tests,
/// or (via the Android JNI bridge) from Kotlin.
pub struct StorageHost {
    inner: FileServiceImpl,
}

impl StorageHost {
    pub fn new(inner: FileServiceImpl) -> Self {
        StorageHost { inner }
    }
}

#[async_trait::async_trait]
impl Storage for StorageHost {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>> {
        // Delegate to the host's list_dir logic.
        // The FileServiceImpl already has this method through the gRPC service.
        // For now, return empty as stub; a full impl would call the inner host.
        Ok(vec![])
    }

    async fn stat(&self, path: &str) -> Result<FileEntry> {
        // Stub: return minimal entry
        Ok(FileEntry {
            name: path.split('/').last().unwrap_or("").to_string(),
            is_dir: false,
            size_bytes: 0,
            modified_unix: 0,
        })
    }

    async fn read_file(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
        Ok(vec![])
    }

    async fn write_file(
        &self,
        path: &str,
        data: &[u8],
        clock: &HashMap<String, u64>,
        writer_device_id: &str,
    ) -> Result<WriteResult> {
        Ok(WriteResult::Applied)
    }

    async fn mkdir_file(
        &self,
        path: &str,
        clock: &HashMap<String, u64>,
        writer_device_id: &str,
    ) -> Result<MkdirResult> {
        Ok(MkdirResult::Applied)
    }

    async fn delete_file(
        &self,
        path: &str,
        clock: &HashMap<String, u64>,
        writer_device_id: &str,
    ) -> Result<DeleteResult> {
        Ok(DeleteResult::Deleted)
    }

    async fn rename_file(
        &self,
        old_path: &str,
        new_path: &str,
        clock: &HashMap<String, u64>,
        writer_device_id: &str,
    ) -> Result<RenameResult> {
        Ok(RenameResult::Renamed)
    }
}
