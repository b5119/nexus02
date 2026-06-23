//! Thin wrapper around the generated tonic client for FileService.
//! Kept separate from filesystem.rs so the FUSE trait implementation
//! (which is synchronous, per the fuser API) doesn't get tangled up
//! with async/await directly — see filesystem.rs for how that boundary
//! is bridged.

use anyhow::Result;
use nexus_proto::fs::v1::{
    file_service_client::FileServiceClient, ListDirRequest, ReadFileRequest, StatRequest,
};
use nexus_proto::fs::v1::FileEntry;
use tonic::transport::Channel;

#[derive(Clone)]
pub struct RemoteFs {
    client: FileServiceClient<Channel>,
}

impl RemoteFs {
    pub async fn connect(addr: String) -> Result<Self> {
        let client = FileServiceClient::connect(addr).await?;
        Ok(Self { client })
    }

    pub async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>> {
        let mut client = self.client.clone();
        let resp = client
            .list_dir(ListDirRequest {
                path: path.to_string(),
            })
            .await?;
        Ok(resp.into_inner().entries)
    }

    pub async fn stat(&self, path: &str) -> Result<Option<FileEntry>> {
        let mut client = self.client.clone();
        let resp = client
            .stat(StatRequest {
                path: path.to_string(),
            })
            .await?;
        let inner = resp.into_inner();
        Ok(if inner.found { inner.entry } else { None })
    }

    /// Reads a byte range and returns it fully buffered.
    /// Fine for milestone 1 with 64KiB-ish FUSE read requests;
    /// revisit if profiling shows this is a bottleneck on large files.
    pub async fn read_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
        let mut client = self.client.clone();
        let mut stream = client
            .read_file(ReadFileRequest {
                path: path.to_string(),
                offset,
                length,
            })
            .await?
            .into_inner();

        let mut buf = Vec::with_capacity(length as usize);
        while let Some(chunk) = stream.message().await? {
            buf.extend_from_slice(&chunk.data);
        }
        Ok(buf)
    }
}
