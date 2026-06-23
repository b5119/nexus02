//! The HOST role: implements FileService over gRPC, serving this
//! device's own files to whatever client agent connects (typically
//! the Dell's nexus-fs FUSE mount).
//!
//! Deliberately naive for milestone 1 — direct std::fs calls,
//! no caching, no auth/pairing check yet (that's the control-plane
//! piece, added once this data-plane path is proven). Do not expose
//! this on an untrusted network as-is.

use anyhow::Result;
use nexus_proto::fs::v1::{
    file_service_server::{FileService, FileServiceServer},
    FileEntry, ListDirRequest, ListDirResponse, ReadFileChunk, ReadFileRequest, StatRequest,
    StatResponse,
};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_stream::Stream;
use tonic::{transport::Server, Request, Response, Status};

const CHUNK_SIZE: usize = 64 * 1024; // 64KiB — small enough to keep memory flat,
                                      // large enough to not drown in RPC overhead
                                      // over a phone's wifi link.

pub struct FileServiceImpl {
    root: PathBuf,
}

impl FileServiceImpl {
    /// Resolves a client-supplied relative path against the served root,
    /// rejecting any attempt to escape it via `..`. This is the one piece
    /// of "security" milestone 1 has — everything else (auth, pairing,
    /// encryption) is explicitly out of scope until the control plane exists.
    fn resolve(&self, requested: &str) -> Result<PathBuf, Status> {
        let candidate = self.root.join(requested.trim_start_matches('/'));
        let canonical = candidate
            .canonicalize()
            .map_err(|_| Status::not_found("path not found"))?;

        if !canonical.starts_with(&self.root) {
            return Err(Status::permission_denied("path escapes served root"));
        }
        Ok(canonical)
    }
}

#[tonic::async_trait]
impl FileService for FileServiceImpl {
    async fn list_dir(
        &self,
        request: Request<ListDirRequest>,
    ) -> Result<Response<ListDirResponse>, Status> {
        let req = request.into_inner();
        let dir_path = self.resolve(&req.path)?;

        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&dir_path)
            .await
            .map_err(|e| Status::internal(format!("read_dir failed: {e}")))?;

        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| Status::internal(format!("next_entry failed: {e}")))?
        {
            let meta = entry
                .metadata()
                .await
                .map_err(|e| Status::internal(format!("metadata failed: {e}")))?;

            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                size_bytes: meta.len(),
                modified_unix,
            });
        }

        Ok(Response::new(ListDirResponse { entries }))
    }

    async fn stat(&self, request: Request<StatRequest>) -> Result<Response<StatResponse>, Status> {
        let req = request.into_inner();

        match self.resolve(&req.path) {
            Ok(path) => {
                let meta = tokio::fs::metadata(&path)
                    .await
                    .map_err(|e| Status::internal(format!("metadata failed: {e}")))?;

                let modified_unix = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                let name = Path::new(&req.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();

                Ok(Response::new(StatResponse {
                    entry: Some(FileEntry {
                        name,
                        is_dir: meta.is_dir(),
                        size_bytes: meta.len(),
                        modified_unix,
                    }),
                    found: true,
                }))
            }
            Err(_) => Ok(Response::new(StatResponse {
                entry: None,
                found: false,
            })),
        }
    }

    type ReadFileStream = Pin<Box<dyn Stream<Item = Result<ReadFileChunk, Status>> + Send>>;

    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<Self::ReadFileStream>, Status> {
        let req = request.into_inner();
        let path = self.resolve(&req.path)?;

        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| Status::internal(format!("open failed: {e}")))?;

        file.seek(std::io::SeekFrom::Start(req.offset))
            .await
            .map_err(|e| Status::internal(format!("seek failed: {e}")))?;

        let remaining = req.length;

        let stream = async_stream::try_stream! {
            let mut left = remaining;
            let mut buf = vec![0u8; CHUNK_SIZE];

            while left > 0 {
                let want = std::cmp::min(left as usize, CHUNK_SIZE);
                let n = file.read(&mut buf[..want]).await
                    .map_err(|e| Status::internal(format!("read failed: {e}")))?;

                if n == 0 {
                    break; // EOF before requested length — fine, just stop
                }

                yield ReadFileChunk { data: buf[..n].to_vec() };
                left -= n as u64;
            }
        };

        Ok(Response::new(Box::pin(stream)))
    }
}

pub async fn run(serve_dir: String, port: u16) -> Result<()> {
    let root = PathBuf::from(&serve_dir)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&serve_dir));

    std::fs::create_dir_all(&root)?;

    let addr = format!("0.0.0.0:{port}").parse()?;
    let service = FileServiceImpl { root };

    tracing::info!(%addr, "FileService listening");

    Server::builder()
        .add_service(FileServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
