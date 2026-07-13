//! The HOST role: implements FileService over gRPC, serving this
//! device's own files to whatever client agent connects (typically
//! the Dell's nexus-fs FUSE mount).
//!
//! Deliberately naive for milestone 1 — direct std::fs calls,
//! no caching, no auth/pairing check yet (that's the control-plane
//! piece, added once this data-plane path is proven). Do not expose
//! this on an untrusted network as-is.

// tonic's service trait mandates `Result<_, tonic::Status>`, and Status is a
// large (~176 byte) error type that can't be boxed without breaking the trait.
// Scoped to this module (the only place that surface lives in this crate)
// rather than crate-wide, so the lint stays active everywhere else.
#![allow(clippy::result_large_err)]

use anyhow::Result;
use nexus_common::{ClockOrder, ClockStore, TombstoneStore, VectorClock};
use nexus_proto::fs::v1::{
    delete_file_response,
    file_service_server::{FileService, FileServiceServer},
    rename_file_response, write_file_response, DeleteFileRequest, DeleteFileResponse, FileEntry,
    ListDirRequest, ListDirResponse, ReadFileChunk, ReadFileRequest, RenameFileRequest,
    RenameFileResponse, StatRequest, StatResponse, WriteFileChunk, WriteFileRequest,
    WriteFileResponse,
};
use nexus_proto::stream::v1::stream_service_server::StreamServiceServer;
use rustls_pki_types::CertificateDer;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_stream::Stream;
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

/// gRPC metadata header carrying the shared-secret auth token (ADR 0004).
const AUTH_HEADER: &str = "x-nexus-token";

const CHUNK_SIZE: usize = 64 * 1024; // 64KiB — small enough to keep memory flat,
                                     // large enough to not drown in RPC overhead
                                     // over a phone's wifi link.

/// Build a combined PEM trust anchor bundle from all paired peer certs.
/// Returns `None` when the store is empty (rustls rejects an empty root store).
fn build_peer_ca_pem(peers: &crate::pairing::PeersStore) -> Option<String> {
    let entries = peers.list();
    if entries.is_empty() {
        return None;
    }
    let mut combined = String::new();
    for (_, entry) in &entries {
        combined.push_str(entry.cert_pem.trim());
        combined.push('\n');
    }
    Some(combined)
}

/// Extract the DeviceId (UUID) from a DER-encoded certificate's DNS SAN.
fn extract_device_id_from_cert(cert_der: &CertificateDer<'_>) -> Option<nexus_common::DeviceId> {
    let params = rcgen::CertificateParams::from_ca_cert_der(cert_der).ok()?;
    for san in &params.subject_alt_names {
        if let rcgen::SanType::DnsName(name) = san {
            if let Ok(u) = uuid::Uuid::try_parse(name.as_str()) {
                return Some(nexus_common::DeviceId(u));
            }
        }
    }
    None
}

/// A temp file that is cleaned up on drop unless committed. Ensures no
/// partial writes are left behind if a streaming write handler errors out.
struct TempFile {
    path: PathBuf,
    committed: bool,
}

impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn mark_committed(&mut self) {
        self.committed = true;
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Sanitize a `writer_device_id` so it is safe to use as part of a filename.
/// Replaces any character that is not alphanumeric, `-`, or `_` with `_`,
/// preventing path-separator injection in conflict-file naming.
fn sanitize_device_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub struct FileServiceImpl {
    root: PathBuf,
    /// Per-file vector clocks for LIVE files (path -> clock).
    clocks: Arc<ClockStore>,
    /// Tombstones for DELETED files (path -> the clock at delete time). A path
    /// lives in exactly one of `clocks` (live) or `tombstones` (deleted); this
    /// is what lets a later concurrent edit be detected instead of silently
    /// resurrecting the file. See ADR 0008.
    tombstones: Arc<TombstoneStore>,
    /// Serializes the read-compare-apply critical section of WriteFile/DeleteFile
    /// so two concurrent ops on the same path can't interleave and corrupt the
    /// conflict decision. Coarse (one mutating op at a time, any path) but
    /// correct; see ADR 0005 for the scalability note.
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl FileServiceImpl {
    /// Normalizes a client path to the relative key used in the clock store
    /// (leading slashes trimmed; "/" maps to "").
    fn clock_key(requested: &str) -> String {
        requested.trim_start_matches('/').to_string()
    }

    /// Like `resolve`, but for a path that may not exist yet (a new file). We
    /// canonicalize the *parent* directory (which must exist and be inside
    /// root, blocking `..`/symlink escapes) and re-attach the leaf name. The
    /// leaf itself is not followed, so this can't be tricked into writing
    /// outside root via a not-yet-existing component.
    fn resolve_for_write(&self, requested: &str) -> Result<PathBuf, Status> {
        let candidate = self.root.join(requested.trim_start_matches('/'));
        let parent = candidate
            .parent()
            .ok_or_else(|| Status::invalid_argument("path has no parent"))?;
        let parent_canon = parent
            .canonicalize()
            .map_err(|_| Status::not_found("parent directory not found"))?;
        if !parent_canon.starts_with(&self.root) {
            return Err(Status::permission_denied("path escapes served root"));
        }
        let file_name = candidate
            .file_name()
            .ok_or_else(|| Status::invalid_argument("path has no file name"))?;
        Ok(parent_canon.join(file_name))
    }
    /// Resolves a client-supplied relative path against the served root,
    /// rejecting any attempt to escape it — both `../..` traversal and
    /// symlink-based escapes (a symlink inside root pointing outside it).
    ///
    /// Why this is safe: `canonicalize()` fully resolves `..` segments AND
    /// follows every symlink to a real absolute path *before* the prefix
    /// check. `self.root` is itself canonicalized in `run()`, so we compare
    /// canonical-vs-canonical. `Path::starts_with` matches whole path
    /// components, so a sibling like `/srv/share-evil` does not count as being
    /// under `/srv/share`. See the tests at the bottom of this file, which
    /// exercise both attack shapes.
    ///
    /// Residual gap (out of scope for milestone 1, LAN-trust): there is a TOCTOU
    /// window between canonicalize and the subsequent open — a symlink swapped
    /// in that instant could still be followed. Closing it needs openat2/
    /// O_NOFOLLOW-style resolution; not worth it before the control plane.
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

                // Include the file's current clock so a reading client can
                // sync its knowledge before editing (ADR 0007).
                let clock = self.clocks.get(&Self::clock_key(&req.path));

                Ok(Response::new(StatResponse {
                    entry: Some(FileEntry {
                        name,
                        is_dir: meta.is_dir(),
                        size_bytes: meta.len(),
                        modified_unix,
                    }),
                    found: true,
                    clock: Some(clock_to_proto(&clock)),
                }))
            }
            Err(_) => Ok(Response::new(StatResponse {
                entry: None,
                found: false,
                clock: None,
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

    async fn write_file(
        &self,
        request: Request<WriteFileRequest>,
    ) -> Result<Response<WriteFileResponse>, Status> {
        let req = request.into_inner();
        let key = Self::clock_key(&req.path);
        if key.is_empty() {
            return Err(Status::invalid_argument("cannot write the root path"));
        }
        let incoming = clock_from_proto(&req.clock);
        let writer = if req.writer_device_id.is_empty() {
            "unknown".to_string()
        } else {
            req.writer_device_id.clone()
        };

        // Serialize the whole compare-and-apply so two concurrent writes to the
        // same path can't race on the conflict decision (ADR 0005).
        let _guard = self.write_lock.lock().await;

        // If the path was deleted (tombstoned), this write either resurrects it
        // (the writer had seen the delete) or is a delete-vs-edit conflict.
        // Either way the data-preserving outcome is to KEEP the edited file —
        // see ADR 0008.
        let tombstone = self.tombstones.get(&key);
        if !tombstone.0.is_empty() {
            let dest = self.resolve_for_write(&req.path)?;
            let is_conflict = matches!(incoming.compare(&tombstone), ClockOrder::Concurrent);

            // Stale write (predates the delete) — the delete is newer, stays dead.
            if matches!(incoming.compare(&tombstone), ClockOrder::DominatedBy) {
                tracing::warn!(path = %req.path, "ignoring stale write to a deleted file (the delete is newer)");
                return Ok(Response::new(WriteFileResponse {
                    result: write_file_response::Result::Stale as i32,
                    clock: Some(clock_to_proto(&tombstone)),
                    conflict_path: String::new(),
                }));
            }

            // Dominates/Equal (intentional resurrect) OR Concurrent (delete-vs-edit
            // conflict): write the edited content, drop the tombstone, go live.
            tokio::fs::write(&dest, &req.data)
                .await
                .map_err(|e| Status::internal(format!("write failed: {e}")))?;
            let merged = tombstone.merge(&incoming);
            self.tombstones
                .remove(&key)
                .map_err(|e| Status::internal(format!("clearing tombstone failed: {e}")))?;
            self.clocks
                .put(&key, merged.clone())
                .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
            if is_conflict {
                tracing::warn!(path = %req.path, "delete-vs-edit conflict — kept the edited file (data preserved)");
            }
            return Ok(Response::new(WriteFileResponse {
                result: if is_conflict {
                    write_file_response::Result::Conflict as i32
                } else {
                    write_file_response::Result::Applied as i32
                },
                clock: Some(clock_to_proto(&merged)),
                conflict_path: String::new(),
            }));
        }

        let stored = self.clocks.get(&key);
        let dest = self.resolve_for_write(&req.path)?;

        match incoming.compare(&stored) {
            // Newer-or-equal, unambiguous (also covers a brand-new file, whose
            // stored clock is empty): write through.
            ClockOrder::Dominates | ClockOrder::Equal => {
                tokio::fs::write(&dest, &req.data)
                    .await
                    .map_err(|e| Status::internal(format!("write failed: {e}")))?;
                let merged = stored.merge(&incoming);
                self.clocks
                    .put(&key, merged.clone())
                    .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
                Ok(Response::new(WriteFileResponse {
                    result: write_file_response::Result::Applied as i32,
                    clock: Some(clock_to_proto(&merged)),
                    conflict_path: String::new(),
                }))
            }

            // Incoming is stale — the host already holds a dominating version.
            // Don't clobber it; the client should pull the newer version.
            ClockOrder::DominatedBy => {
                tracing::warn!(path = %req.path, "ignoring stale write (host has a newer version)");
                Ok(Response::new(WriteFileResponse {
                    result: write_file_response::Result::Stale as i32,
                    clock: Some(clock_to_proto(&stored)),
                    conflict_path: String::new(),
                }))
            }

            // Genuine conflict: keep both. The original is left untouched; the
            // incoming version is written to a sibling and logged loudly.
            ClockOrder::Concurrent => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let conflict_rel = format!("{key}.conflict-{writer}-{ts}");
                let conflict_dest = self.resolve_for_write(&conflict_rel)?;
                tokio::fs::write(&conflict_dest, &req.data)
                    .await
                    .map_err(|e| Status::internal(format!("writing conflict copy failed: {e}")))?;
                // The conflict copy keeps the incoming clock; the original's
                // stored clock is deliberately left as-is.
                self.clocks
                    .put(&conflict_rel, incoming.clone())
                    .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
                tracing::warn!(
                    path = %req.path,
                    conflict = %conflict_rel,
                    "concurrent edit detected — kept both versions (original untouched)"
                );
                Ok(Response::new(WriteFileResponse {
                    result: write_file_response::Result::Conflict as i32,
                    clock: Some(clock_to_proto(&stored)),
                    conflict_path: conflict_rel,
                }))
            }
        }
    }

    async fn write_file_stream(
        &self,
        request: Request<tonic::Streaming<WriteFileChunk>>,
    ) -> Result<Response<WriteFileResponse>, Status> {
        let mut stream = request.into_inner();

        // First chunk must carry metadata.
        let init = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty write stream"))?;
        let path = if init.path.is_empty() {
            return Err(Status::invalid_argument("first chunk must set path"));
        } else {
            init.path
        };
        let incoming = clock_from_proto(&init.clock);
        let writer = if init.writer_device_id.is_empty() {
            "unknown".to_string()
        } else {
            sanitize_device_id(&init.writer_device_id)
        };
        let key = Self::clock_key(&path);
        if key.is_empty() {
            return Err(Status::invalid_argument("cannot write the root path"));
        }

        // Write chunks to a temp file. Using UUID for uniqueness so concurrent
        // streams to the same path don't collide. TempFile's Drop ensures cleanup
        // if the handler exits before committing (rename).
        let dest = self.resolve_for_write(&path)?;
        let parent = dest.parent().unwrap_or(&self.root);
        let tmp_name = format!(".nexus-write-{}", uuid::Uuid::new_v4());
        let mut tmp = TempFile::new(parent.join(&tmp_name));

        {
            let mut f = tokio::fs::File::create(tmp.path())
                .await
                .map_err(|e| Status::internal(format!("create temp file failed: {e}")))?;
            f.write_all(&init.data)
                .await
                .map_err(|e| Status::internal(format!("write temp file failed: {e}")))?;
            while let Some(chunk) = stream.message().await? {
                f.write_all(&chunk.data)
                    .await
                    .map_err(|e| Status::internal(format!("write temp file failed: {e}")))?;
            }
            f.sync_all()
                .await
                .map_err(|e| Status::internal(format!("sync temp file failed: {e}")))?;
        }

        // Critical section: clock comparison + final rename (same logic as write_file).
        let _guard = self.write_lock.lock().await;

        // Check tombstone.
        let tombstone = self.tombstones.get(&key);
        if !tombstone.0.is_empty() {
            if matches!(incoming.compare(&tombstone), ClockOrder::DominatedBy) {
                tracing::warn!(%path, "ignoring stale streaming write to a deleted file");
                return Ok(Response::new(WriteFileResponse {
                    result: write_file_response::Result::Stale as i32,
                    clock: Some(clock_to_proto(&tombstone)),
                    conflict_path: String::new(),
                }));
            }
            let is_conflict = matches!(incoming.compare(&tombstone), ClockOrder::Concurrent);
            tokio::fs::rename(tmp.path(), &dest)
                .await
                .map_err(|e| Status::internal(format!("final rename failed: {e}")))?;
            tmp.mark_committed();
            let merged = tombstone.merge(&incoming);
            self.tombstones
                .remove(&key)
                .map_err(|e| Status::internal(format!("clearing tombstone failed: {e}")))?;
            self.clocks
                .put(&key, merged.clone())
                .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
            if is_conflict {
                tracing::warn!(%path, "streaming delete-vs-edit conflict — kept the edited file");
            }
            return Ok(Response::new(WriteFileResponse {
                result: if is_conflict {
                    write_file_response::Result::Conflict as i32
                } else {
                    write_file_response::Result::Applied as i32
                },
                clock: Some(clock_to_proto(&merged)),
                conflict_path: String::new(),
            }));
        }

        let stored = self.clocks.get(&key);

        match incoming.compare(&stored) {
            ClockOrder::Dominates | ClockOrder::Equal => {
                tokio::fs::rename(tmp.path(), &dest)
                    .await
                    .map_err(|e| Status::internal(format!("final rename failed: {e}")))?;
                tmp.mark_committed();
                let merged = stored.merge(&incoming);
                self.clocks
                    .put(&key, merged.clone())
                    .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
                Ok(Response::new(WriteFileResponse {
                    result: write_file_response::Result::Applied as i32,
                    clock: Some(clock_to_proto(&merged)),
                    conflict_path: String::new(),
                }))
            }
            ClockOrder::DominatedBy => {
                tracing::warn!(%path, "ignoring stale streaming write");
                Ok(Response::new(WriteFileResponse {
                    result: write_file_response::Result::Stale as i32,
                    clock: Some(clock_to_proto(&stored)),
                    conflict_path: String::new(),
                }))
            }
            ClockOrder::Concurrent => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let conflict_rel = format!("{key}.conflict-{writer}-{ts}");
                let conflict_dest = self.resolve_for_write(&conflict_rel)?;
                tokio::fs::rename(tmp.path(), &conflict_dest)
                    .await
                    .map_err(|e| Status::internal(format!("rename conflict failed: {e}")))?;
                tmp.mark_committed();
                self.clocks
                    .put(&conflict_rel, incoming.clone())
                    .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
                tracing::warn!(
                    %path,
                    conflict = %conflict_rel,
                    "concurrent streaming edit detected — kept both versions"
                );
                Ok(Response::new(WriteFileResponse {
                    result: write_file_response::Result::Conflict as i32,
                    clock: Some(clock_to_proto(&stored)),
                    conflict_path: conflict_rel,
                }))
            }
        }
    }

    async fn delete_file(
        &self,
        request: Request<DeleteFileRequest>,
    ) -> Result<Response<DeleteFileResponse>, Status> {
        let req = request.into_inner();
        let key = Self::clock_key(&req.path);
        if key.is_empty() {
            return Err(Status::invalid_argument("cannot delete the root path"));
        }
        let incoming = clock_from_proto(&req.clock);

        let _guard = self.write_lock.lock().await;

        // Already deleted — idempotent; just advance the tombstone clock.
        let tombstone = self.tombstones.get(&key);
        if !tombstone.0.is_empty() {
            let merged = tombstone.merge(&incoming);
            self.tombstones
                .put(&key, merged.clone())
                .map_err(|e| Status::internal(format!("persisting tombstone failed: {e}")))?;
            return Ok(Response::new(DeleteFileResponse {
                result: delete_file_response::Result::Deleted as i32,
                clock: Some(clock_to_proto(&merged)),
            }));
        }

        let stored = self.clocks.get(&key);

        // The file must exist to delete it; if it's already gone, nothing to do.
        let dest = match self.resolve(&req.path) {
            Ok(p) => p,
            // The file is genuinely gone (canonicalize couldn't find it) —
            // nothing to delete.
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Ok(Response::new(DeleteFileResponse {
                    result: delete_file_response::Result::NotFound as i32,
                    clock: Some(clock_to_proto(&stored)),
                }));
            }
            // A real rejection (e.g. a path-escape attempt is permission_denied)
            // — propagate it rather than masking it as NOT_FOUND.
            Err(status) => return Err(status),
        };

        match incoming.compare(&stored) {
            // Deleter saw the latest (or the file was untracked): delete + tombstone.
            ClockOrder::Dominates | ClockOrder::Equal => {
                tokio::fs::remove_file(&dest)
                    .await
                    .map_err(|e| Status::internal(format!("delete failed: {e}")))?;
                let merged = stored.merge(&incoming);
                // Persist the tombstone BEFORE clearing the live clock: if the
                // second op fails, the path is in both stores, and both handlers
                // check tombstones first, so it degrades safely to "deleted".
                self.tombstones
                    .put(&key, merged.clone())
                    .map_err(|e| Status::internal(format!("persisting tombstone failed: {e}")))?;
                self.clocks
                    .remove(&key)
                    .map_err(|e| Status::internal(format!("clearing clock failed: {e}")))?;
                Ok(Response::new(DeleteFileResponse {
                    result: delete_file_response::Result::Deleted as i32,
                    clock: Some(clock_to_proto(&merged)),
                }))
            }
            // Stale delete: the file has edits the deleter never saw. Keep it.
            ClockOrder::DominatedBy => {
                tracing::warn!(path = %req.path, "ignoring stale delete (file has a newer version)");
                Ok(Response::new(DeleteFileResponse {
                    result: delete_file_response::Result::Stale as i32,
                    clock: Some(clock_to_proto(&stored)),
                }))
            }
            // Concurrent delete-vs-edit: keep the file (the edit wins; ADR 0008).
            ClockOrder::Concurrent => {
                tracing::warn!(path = %req.path, "delete-vs-edit conflict — keeping the file (concurrent edit preserved)");
                Ok(Response::new(DeleteFileResponse {
                    result: delete_file_response::Result::Conflict as i32,
                    clock: Some(clock_to_proto(&stored)),
                }))
            }
        }
    }

    async fn rename_file(
        &self,
        request: Request<RenameFileRequest>,
    ) -> Result<Response<RenameFileResponse>, Status> {
        let req = request.into_inner();
        let old_key = Self::clock_key(&req.old_path);
        let new_key = Self::clock_key(&req.new_path);
        if old_key.is_empty() || new_key.is_empty() {
            return Err(Status::invalid_argument("cannot rename root path"));
        }
        let incoming = clock_from_proto(&req.clock);

        let _guard = self.write_lock.lock().await;

        // Check if old_path has a tombstone (was deleted).
        let old_tombstone = self.tombstones.get(&old_key);
        if !old_tombstone.0.is_empty() {
            if matches!(incoming.compare(&old_tombstone), ClockOrder::DominatedBy) {
                tracing::warn!(old_path = %req.old_path, "ignoring stale rename of a deleted file");
                return Ok(Response::new(RenameFileResponse {
                    result: rename_file_response::Result::Stale as i32,
                    clock: Some(clock_to_proto(&old_tombstone)),
                    conflict_path: String::new(),
                }));
            }
            let is_conflict = matches!(incoming.compare(&old_tombstone), ClockOrder::Concurrent);
            // Old path was deleted — can't rename from it. "Resurrect" the file
            // at the new path (mirrors delete-vs-edit handling in write_file).
            let new_dest = self.resolve_for_write(&req.new_path)?;
            tokio::fs::write(&new_dest, &[])
                .await
                .map_err(|e| Status::internal(format!("create at new path failed: {e}")))?;
            let merged = old_tombstone.merge(&incoming);
            self.tombstones
                .remove(&old_key)
                .map_err(|e| Status::internal(format!("clearing tombstone failed: {e}")))?;
            let _ = self.tombstones.remove(&new_key);
            self.clocks
                .put(&new_key, merged.clone())
                .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
            if is_conflict {
                tracing::warn!(old_path = %req.old_path, new_path = %req.new_path,
                    "delete-vs-rename conflict — file resurrected at new path");
            }
            return Ok(Response::new(RenameFileResponse {
                result: if is_conflict {
                    rename_file_response::Result::Conflict as i32
                } else {
                    rename_file_response::Result::Renamed as i32
                },
                clock: Some(clock_to_proto(&merged)),
                conflict_path: String::new(),
            }));
        }

        let old_stored = self.clocks.get(&old_key);

        let old_dest = match self.resolve(&req.old_path) {
            Ok(p) => p,
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Ok(Response::new(RenameFileResponse {
                    result: rename_file_response::Result::NotFound as i32,
                    clock: Some(clock_to_proto(&old_stored)),
                    conflict_path: String::new(),
                }));
            }
            Err(status) => return Err(status),
        };
        let new_dest = self.resolve_for_write(&req.new_path)?;

        match incoming.compare(&old_stored) {
            ClockOrder::Dominates | ClockOrder::Equal => {
                tokio::fs::rename(&old_dest, &new_dest)
                    .await
                    .map_err(|e| Status::internal(format!("rename failed: {e}")))?;
                let merged = old_stored.merge(&incoming);
                self.clocks
                    .remove(&old_key)
                    .map_err(|e| Status::internal(format!("removing old clock failed: {e}")))?;
                let _ = self.tombstones.remove(&new_key);
                self.clocks
                    .put(&new_key, merged.clone())
                    .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
                Ok(Response::new(RenameFileResponse {
                    result: rename_file_response::Result::Renamed as i32,
                    clock: Some(clock_to_proto(&merged)),
                    conflict_path: String::new(),
                }))
            }
            ClockOrder::DominatedBy => {
                tracing::warn!(old_path = %req.old_path, "ignoring stale rename (old path has newer edits)");
                Ok(Response::new(RenameFileResponse {
                    result: rename_file_response::Result::Stale as i32,
                    clock: Some(clock_to_proto(&old_stored)),
                    conflict_path: String::new(),
                }))
            }
            // Concurrent rename-vs-edit has open semantics (GitHub issue #26,
            // ADR 0009). Proceed with rename; the concurrent edit will surface
            // as a write to a now-missing path.
            ClockOrder::Concurrent => {
                tokio::fs::rename(&old_dest, &new_dest)
                    .await
                    .map_err(|e| Status::internal(format!("rename failed: {e}")))?;
                let merged = old_stored.merge(&incoming);
                self.clocks
                    .remove(&old_key)
                    .map_err(|e| Status::internal(format!("removing old clock failed: {e}")))?;
                let _ = self.tombstones.remove(&new_key);
                self.clocks
                    .put(&new_key, merged.clone())
                    .map_err(|e| Status::internal(format!("persisting clock failed: {e}")))?;
                tracing::warn!(
                    old_path = %req.old_path,
                    new_path = %req.new_path,
                    "rename-vs-edit edge case — rename applied, concurrent edits at old_path \
                     will resolve via the normal write path (ADR 0009)"
                );
                Ok(Response::new(RenameFileResponse {
                    result: rename_file_response::Result::Renamed as i32,
                    clock: Some(clock_to_proto(&merged)),
                    conflict_path: String::new(),
                }))
            }
        }
    }
}

fn clock_to_proto(c: &VectorClock) -> nexus_proto::fs::v1::VectorClock {
    nexus_proto::fs::v1::VectorClock {
        counters: c.0.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    }
}

fn clock_from_proto(c: &Option<nexus_proto::fs::v1::VectorClock>) -> VectorClock {
    match c {
        Some(pc) => VectorClock(pc.counters.iter().map(|(k, v)| (k.clone(), *v)).collect()),
        None => VectorClock::new(),
    }
}

/// Constant-time-ish comparison of the presented token against the expected
/// one — avoids leaking length/content via response timing. Cheap insurance;
/// the real limits of this auth model are documented in ADR 0004.
fn token_matches(presented: &[u8], expected: &[u8]) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in presented.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Garbage collection — periodic sweep (ADR 0011)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GcConfig {
    pub interval: std::time::Duration,
    pub tombstone_ttl: std::time::Duration,
    pub max_entries: usize,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Per-entry lock-and-remove helper: tries write_lock with 100ms timeout,
/// then does the actual removal. Returns true if the entry was removed.
async fn gc_remove_entry<F>(write_lock: &tokio::sync::Mutex<()>, key: &str, do_remove: F) -> bool
where
    F: FnOnce() -> std::io::Result<()>,
{
    let guard =
        tokio::time::timeout(std::time::Duration::from_millis(100), write_lock.lock()).await;

    match guard {
        Ok(guard) => {
            let result = do_remove();
            drop(guard);
            tokio::task::yield_now().await;
            result.is_ok()
        }
        Err(_) => {
            tracing::debug!(%key, "GC skipping contested entry");
            false
        }
    }
}

/// Run one GC sweep cycle: TTL pass then cap enforcement.
/// `now` is a unix timestamp in seconds (injected for testability).
async fn gc_sweep(
    root: &Path,
    clocks: &ClockStore,
    tombstones: &TombstoneStore,
    write_lock: &tokio::sync::Mutex<()>,
    config: &GcConfig,
    now: u64,
) -> anyhow::Result<()> {
    let ttl_secs = config.tombstone_ttl.as_secs();

    // Phase 1: TTL-based tombstone eviction
    let mut tombstone_removed = 0u64;
    for (key, entry) in tombstones.entries() {
        if entry.created_at > 0 && now.saturating_sub(entry.created_at) >= ttl_secs {
            let file_path = root.join(&key);
            // TOCTOU: file existence is checked outside write_lock, so a
            // concurrent write could recreate the file between this check
            // and the removal. Worst case: we remove a valid entry one sweep
            // early — acceptable per ADR 0011 (conservative default).
            if !file_path.exists() {
                let removed = gc_remove_entry(write_lock, &key, || tombstones.remove(&key)).await;
                if removed {
                    tombstone_removed += 1;
                }
            }
        }
    }

    // Phase 2: Orphaned clock entry removal (no file on disk, no tombstone)
    let mut orphaned_removed = 0u64;
    for (key, _) in clocks.entries() {
        let file_path = root.join(&key);
        if !file_path.exists() && tombstones.get(&key).0.is_empty() {
            let removed = gc_remove_entry(write_lock, &key, || clocks.remove(&key)).await;
            if removed {
                orphaned_removed += 1;
            }
        }
    }

    // Phase 3: Hard cap enforcement (after TTL pass)
    let clock_count = clocks.len();
    let tombstone_count = tombstones.len();

    if tombstone_count > config.max_entries {
        let mut entries = tombstones.entries();
        entries.sort_by_key(|(_, e)| e.created_at);
        let excess = tombstone_count - config.max_entries;
        for (key, _) in entries.iter().take(excess) {
            gc_remove_entry(write_lock, key, || tombstones.remove(key)).await;
        }
    }

    if clock_count > config.max_entries {
        let mut entries: Vec<_> = clocks.entries();
        entries.retain(|(key, _)| !root.join(key).exists() && tombstones.get(key).0.is_empty());

        if entries.is_empty() {
            tracing::warn!(
                clock_count = clock_count,
                max_entries = config.max_entries,
                "clock store exceeds hard cap but all entries are for live files — increase --max-store-entries"
            );
        } else {
            entries.sort_by_key(|(_, e)| e.last_updated_at);
            let target_remove = clock_count.saturating_sub(config.max_entries);
            for (key, _) in entries.iter().take(target_remove) {
                gc_remove_entry(write_lock, key, || clocks.remove(key)).await;
            }
        }
    }

    tracing::info!(
        tombstone_removed = tombstone_removed,
        orphaned_clock_removed = orphaned_removed,
        clock_count = clocks.len(),
        tombstone_count = tombstones.len(),
        "GC sweep complete"
    );

    Ok(())
}

async fn gc_loop(
    root: PathBuf,
    clocks: Arc<ClockStore>,
    tombstones: Arc<TombstoneStore>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    config: GcConfig,
) {
    loop {
        tokio::time::sleep(config.interval).await;
        let now = unix_now();
        if let Err(e) = gc_sweep(&root, &clocks, &tombstones, &write_lock, &config, now).await {
            tracing::warn!("GC sweep failed: {e}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    serve_dir: String,
    port: u16,
    auth_token: String,
    device_id: nexus_common::DeviceId,
    gc_interval_hours: u64,
    tombstone_ttl_hours: u64,
    max_store_entries: usize,
    enable_streaming: bool,
    fps: u32,
    quality: String,
) -> Result<()> {
    let root = PathBuf::from(&serve_dir)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&serve_dir));

    std::fs::create_dir_all(&root)?;

    let cfg_dir = crate::config::config_dir()?;
    let clocks = Arc::new(ClockStore::open(cfg_dir.join("clocks.json"))?);
    let tombstones = Arc::new(TombstoneStore::open(cfg_dir.join("tombstones.json"))?);
    let write_lock = Arc::new(tokio::sync::Mutex::new(()));

    // Spawn the GC loop before starting the server.
    let gc_config = GcConfig {
        interval: std::time::Duration::from_secs(gc_interval_hours * 3600),
        tombstone_ttl: std::time::Duration::from_secs(tombstone_ttl_hours * 3600),
        max_entries: max_store_entries,
    };
    tokio::spawn(gc_loop(
        root.clone(),
        clocks.clone(),
        tombstones.clone(),
        write_lock.clone(),
        gc_config,
    ));

    let addr = format!("0.0.0.0:{port}").parse()?;
    let service = FileServiceImpl {
        root,
        clocks,
        tombstones,
        write_lock,
    };

    // Load (or generate on first run) the self-signed TLS identity.
    let tls = crate::config::load_or_create_tls_identity(&device_id)?;
    let identity = Identity::from_pem(&tls.cert_pem, &tls.key_pem);

    // Open PeersStore and build a combined trust anchor for client certs.
    // When no peers exist, skip client_ca_root (rustls rejects empty roots).
    let peers_store = match crate::pairing::PeersStore::open() {
        Ok(store) => Arc::new(store),
        Err(e) => {
            tracing::warn!(error = %e, "peers.json unavailable; mTLS disabled");
            Arc::new(crate::pairing::PeersStore::empty())
        }
    };

    let mut tls_config = ServerTlsConfig::new().identity(identity);
    if let Some(combined) = build_peer_ca_pem(&peers_store) {
        tls_config = tls_config
            .client_ca_root(Certificate::from_pem(combined))
            .client_auth_optional(true);
        tracing::info!(
            "mTLS enabled: {} paired device(s) in trust store",
            peers_store.list().len()
        );
    } else {
        tracing::info!("no paired devices yet — mTLS disabled, token auth only");
    }

    let expected = auth_token;
    let interceptor = {
        let peers = peers_store.clone();
        move |req: Request<()>| -> Result<Request<()>, Status> {
            // Path A — mTLS: client presented a certificate verified by
            // WebPkiClientVerifier against the peer trust store.
            if let Some(certs) = req
                .extensions()
                .get::<TlsConnectInfo<TcpConnectInfo>>()
                .and_then(|tls| tls.peer_certs())
            {
                if let Some(cert_der) = certs.first() {
                    let device_id = extract_device_id_from_cert(cert_der);
                    match device_id {
                        Some(ref did) if peers.verify_cert_der(did, cert_der) => return Ok(req),
                        _ => return Err(Status::unauthenticated("client certificate not trusted")),
                    }
                }
            }

            // Path B — token fallback (no client cert presented).
            match req.metadata().get(AUTH_HEADER) {
                Some(t) if token_matches(t.as_bytes(), expected.as_bytes()) => Ok(req),
                _ => Err(Status::unauthenticated("missing or invalid auth token")),
            }
        }
    };

    tracing::info!(cert = %tls.cert_path.display(), "TLS enabled (self-signed); clients must trust this cert");
    tracing::info!(%addr, "FileService listening (TLS + mTLS cert auth + token fallback)");

    let router =
        Server::builder()
            .tls_config(tls_config)?
            .add_service(FileServiceServer::with_interceptor(
                service,
                interceptor.clone(),
            ));

    let router = if enable_streaming {
        let capture = tokio::task::spawn_blocking(move || {
            nexus_stream::capture::ScreenCapture::new(fps as f64)
        })
        .await
        .map_err(|e| anyhow::anyhow!("capture init failed: {e}"))??;
        let (width, height) = capture.dimensions();
        let encoder = nexus_stream::encode::Encoder::new(width, height)?;
        let injector = nexus_stream::inject::Injector::new()?;

        let stream_host = nexus_stream::host::run_stream_host(capture, encoder, injector).await?;
        let stream_svc = nexus_stream::host::StreamHostService::new(Arc::new(stream_host));

        tracing::info!(width, height, fps, quality, "screen streaming enabled");

        router.add_service(StreamServiceServer::with_interceptor(
            stream_svc,
            interceptor,
        ))
    } else {
        router
    };

    router.serve(addr).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use nexus_common::TombstoneEntry;

    fn clock(pairs: &[(&str, u64)]) -> VectorClock {
        VectorClock(pairs.iter().map(|(d, c)| (d.to_string(), *c)).collect())
    }

    fn temp_store() -> std::sync::Arc<ClockStore> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "nexus-test-clocks-{}-{}.json",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        std::sync::Arc::new(ClockStore::open(path).unwrap())
    }

    fn temp_tombstone_store() -> std::sync::Arc<TombstoneStore> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "nexus-test-tombstones-{}-{}.json",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        std::sync::Arc::new(TombstoneStore::open(path).unwrap())
    }

    fn svc(root: &std::path::Path) -> FileServiceImpl {
        FileServiceImpl {
            root: root.canonicalize().unwrap(),
            clocks: temp_store(),
            tombstones: temp_tombstone_store(),
            write_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn pclock(pairs: &[(&str, u64)]) -> Option<nexus_proto::fs::v1::VectorClock> {
        Some(nexus_proto::fs::v1::VectorClock {
            counters: pairs.iter().map(|(d, c)| (d.to_string(), *c)).collect(),
        })
    }

    fn write_req(
        path: &str,
        data: &[u8],
        clock: &[(&str, u64)],
        writer: &str,
    ) -> Request<WriteFileRequest> {
        Request::new(WriteFileRequest {
            path: path.to_string(),
            data: data.to_vec(),
            clock: pclock(clock),
            writer_device_id: writer.to_string(),
        })
    }

    fn rename_req(
        old_path: &str,
        new_path: &str,
        clock: &[(&str, u64)],
        writer: &str,
    ) -> Request<RenameFileRequest> {
        Request::new(RenameFileRequest {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
            clock: pclock(clock),
            writer_device_id: writer.to_string(),
        })
    }

    fn delete_req(path: &str, clock: &[(&str, u64)], writer: &str) -> Request<DeleteFileRequest> {
        Request::new(DeleteFileRequest {
            path: path.to_string(),
            clock: pclock(clock),
            writer_device_id: writer.to_string(),
        })
    }

    fn conflict_files(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".conflict-"))
            .collect()
    }

    // Test 1: client writes a new file; host reflects it; clock shows the
    // client's counter incremented.
    #[tokio::test]
    async fn test1_client_writes_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        let resp = s
            .write_file(write_req("new.txt", b"hello", &[("dell", 1)], "dell"))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.result, write_file_response::Result::Applied as i32);
        assert_eq!(std::fs::read(dir.path().join("new.txt")).unwrap(), b"hello");
        assert_eq!(clock_from_proto(&resp.clock).get("dell"), 1);
        assert_eq!(s.clocks.get("new.txt").get("dell"), 1); // persisted
    }

    // Test 2: sequential edits (client, then host having seen it) — one clock
    // dominates the other, so NO conflict.
    #[tokio::test]
    async fn test2_sequential_edits_no_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        let r1 = s
            .write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r1.result, write_file_response::Result::Applied as i32);

        // host writes v2 on top of the client's version: {dell:1,host:1} dominates {dell:1}
        let r2 = s
            .write_file(write_req(
                "f.txt",
                b"v2",
                &[("dell", 1), ("host", 1)],
                "host",
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            r2.result,
            write_file_response::Result::Applied as i32,
            "dominance must resolve cleanly, not as a conflict"
        );
        assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"v2");
        assert!(
            conflict_files(dir.path()).is_empty(),
            "no conflict file expected"
        );
    }

    // Test 3: genuine concurrent edit — host and client both advance from the
    // same base independently — IS detected as a conflict and keeps both.
    #[tokio::test]
    async fn test3_concurrent_edit_is_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        // shared base from the client
        s.write_file(write_req("f.txt", b"base", &[("dell", 1)], "dell"))
            .await
            .unwrap();
        // host edits on top of base -> {dell:1, host:1}
        let rh = s
            .write_file(write_req(
                "f.txt",
                b"host-edit",
                &[("dell", 1), ("host", 1)],
                "host",
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(rh.result, write_file_response::Result::Applied as i32);

        // client, NOT having seen the host edit, advances its own copy from base
        // -> {dell:2}; concurrent with {dell:1,host:1}
        let rc = s
            .write_file(write_req("f.txt", b"client-edit", &[("dell", 2)], "dell"))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(
            rc.result,
            write_file_response::Result::Conflict as i32,
            "concurrent edit must be detected as a conflict"
        );
        assert!(!rc.conflict_path.is_empty());
        // original is untouched (still the host's version)
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"host-edit"
        );
        // the incoming version is preserved in the conflict copy
        assert_eq!(
            std::fs::read(dir.path().join(&rc.conflict_path)).unwrap(),
            b"client-edit"
        );
        // named for the writing device
        assert!(
            rc.conflict_path.contains(".conflict-dell-"),
            "unexpected conflict name: {}",
            rc.conflict_path
        );
    }

    // ADR 0007: Stat returns the file's current clock so a reading client can
    // sync before editing.
    #[tokio::test]
    async fn stat_returns_the_files_clock() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        // A written file reports its stored clock.
        s.write_file(write_req("f.txt", b"hi", &[("dell", 3)], "dell"))
            .await
            .unwrap();
        let resp = s
            .stat(Request::new(StatRequest {
                path: "f.txt".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.found);
        assert_eq!(clock_from_proto(&resp.clock).get("dell"), 3);

        // A file that exists but was never written through WriteFile has an
        // empty clock (so a single editor of it won't spuriously conflict).
        std::fs::write(dir.path().join("plain.txt"), b"x").unwrap();
        let r2 = s
            .stat(Request::new(StatRequest {
                path: "plain.txt".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(r2.found);
        assert_eq!(clock_from_proto(&r2.clock), VectorClock::new());
    }

    // ---- delete-vs-edit (ADR 0008) ------------------------------------------

    fn exists(dir: &std::path::Path, name: &str) -> bool {
        dir.join(name).exists()
    }

    // Clean delete: the deleter's clock dominates the file's -> file removed, a
    // tombstone is recorded.
    #[tokio::test]
    async fn delete_clean_records_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        s.write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap();

        let r = s
            .delete_file(delete_req("f.txt", &[("dell", 2)], "dell"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r.result, delete_file_response::Result::Deleted as i32);
        assert!(!exists(dir.path(), "f.txt"), "file should be gone");
        assert_eq!(
            s.tombstones.get("f.txt").get("dell"),
            2,
            "tombstone recorded"
        );
        assert_eq!(
            s.clocks.get("f.txt"),
            VectorClock::new(),
            "live clock cleared"
        );
    }

    // Stale delete: the delete clock is dominated by the file's current clock
    // (the deleter never saw the latest edit) -> ignored, file kept.
    #[tokio::test]
    async fn delete_stale_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        s.write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap();
        s.write_file(write_req("f.txt", b"v2", &[("dell", 2)], "dell"))
            .await
            .unwrap();

        let r = s
            .delete_file(delete_req("f.txt", &[("dell", 1)], "dell"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r.result, delete_file_response::Result::Stale as i32);
        assert!(
            exists(dir.path(), "f.txt"),
            "stale delete must not remove the file"
        );
    }

    // Concurrent delete-vs-edit (delete arrives): the file has an edit the
    // deleter never saw -> CONFLICT, file KEPT (the edit wins).
    #[tokio::test]
    async fn delete_concurrent_with_edit_keeps_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        s.write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap();
        // dell edits again -> host at {dell:2}
        s.write_file(write_req("f.txt", b"v2", &[("dell", 2)], "dell"))
            .await
            .unwrap();
        // phone deletes based on its stale view {dell:1} -> {dell:1, phone:1},
        // concurrent with {dell:2}
        let r = s
            .delete_file(delete_req("f.txt", &[("dell", 1), ("phone", 1)], "phone"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r.result, delete_file_response::Result::Conflict as i32);
        assert!(
            exists(dir.path(), "f.txt"),
            "concurrent edit must be preserved"
        );
        assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"v2");
        assert_eq!(
            s.tombstones.get("f.txt"),
            VectorClock::new(),
            "no tombstone on conflict"
        );
    }

    // Concurrent edit-vs-delete (edit arrives after a tombstone): resurrect the
    // file with the edited content, CONFLICT.
    #[tokio::test]
    async fn write_concurrent_with_tombstone_resurrects() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        s.write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap();
        s.delete_file(delete_req("f.txt", &[("dell", 2)], "dell"))
            .await
            .unwrap();
        assert!(!exists(dir.path(), "f.txt"));

        // phone edits from its stale view {dell:1} -> {dell:1, phone:1},
        // concurrent with the tombstone {dell:2}
        let r = s
            .write_file(write_req(
                "f.txt",
                b"phone-edit",
                &[("dell", 1), ("phone", 1)],
                "phone",
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r.result, write_file_response::Result::Conflict as i32);
        assert!(
            exists(dir.path(), "f.txt"),
            "edit should resurrect the file"
        );
        assert_eq!(
            std::fs::read(dir.path().join("f.txt")).unwrap(),
            b"phone-edit"
        );
        assert_eq!(
            s.tombstones.get("f.txt"),
            VectorClock::new(),
            "tombstone cleared"
        );
    }

    // Intentional resurrect: an edit that dominates the tombstone (the writer
    // saw the delete and writes anyway) recreates the file, no conflict.
    #[tokio::test]
    async fn write_dominating_tombstone_resurrects_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        s.write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap();
        s.delete_file(delete_req("f.txt", &[("dell", 2)], "dell"))
            .await
            .unwrap();
        let r = s
            .write_file(write_req("f.txt", b"recreated", &[("dell", 3)], "dell"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r.result, write_file_response::Result::Applied as i32);
        assert!(exists(dir.path(), "f.txt"));
        assert_eq!(s.tombstones.get("f.txt"), VectorClock::new());
    }

    // A stale write to a deleted file (predates the delete) stays dead.
    #[tokio::test]
    async fn write_stale_to_tombstone_stays_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());
        s.write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap();
        s.delete_file(delete_req("f.txt", &[("dell", 2)], "dell"))
            .await
            .unwrap();
        let r = s
            .write_file(write_req("f.txt", b"stale", &[("dell", 1)], "dell"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r.result, write_file_response::Result::Stale as i32);
        assert!(
            !exists(dir.path(), "f.txt"),
            "stale write must not resurrect"
        );
    }

    // A delete that escapes the served root must be rejected as permission_denied,
    // not masked as NOT_FOUND.
    #[tokio::test]
    async fn delete_rejects_path_escape() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("share");
        std::fs::create_dir(&root).unwrap();
        // a real file just outside the served root
        std::fs::write(parent.path().join("secret.txt"), b"s").unwrap();
        let s = svc(&root);

        let err = s
            .delete_file(delete_req("../secret.txt", &[("dell", 1)], "dell"))
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::PermissionDenied,
            "path escape must be denied, not reported NOT_FOUND"
        );
        assert!(
            parent.path().join("secret.txt").exists(),
            "the outside file must be untouched"
        );
    }

    // ---- rename / move (ADR 0009) --------------------------------------------

    // Simple same-directory rename: content + clock survive intact.
    #[tokio::test]
    async fn rename_same_directory_preserves_clock_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        s.write_file(write_req("old.txt", b"hello", &[("dell", 1)], "dell"))
            .await
            .unwrap();

        let r = s
            .rename_file(rename_req("old.txt", "new.txt", &[("dell", 2)], "dell"))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(r.result, rename_file_response::Result::Renamed as i32);
        // Content is at the new path
        assert!(exists(dir.path(), "new.txt"));
        assert!(!exists(dir.path(), "old.txt"));
        assert_eq!(std::fs::read(dir.path().join("new.txt")).unwrap(), b"hello");
        // Clock moved to new path
        assert_eq!(s.clocks.get("new.txt").get("dell"), 2);
        assert_eq!(s.clocks.get("old.txt"), VectorClock::new());
    }

    // Cross-directory move: same guarantees.
    #[tokio::test]
    async fn rename_cross_directory_moves_clock_and_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let s = svc(dir.path());

        s.write_file(write_req("old.txt", b"cross-dir", &[("dell", 1)], "dell"))
            .await
            .unwrap();

        let r = s
            .rename_file(rename_req(
                "old.txt",
                "subdir/moved.txt",
                &[("dell", 2)],
                "dell",
            ))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(r.result, rename_file_response::Result::Renamed as i32);
        assert!(!exists(dir.path(), "old.txt"));
        assert!(exists(dir.path(), "subdir/moved.txt"));
        assert_eq!(
            std::fs::read(dir.path().join("subdir/moved.txt")).unwrap(),
            b"cross-dir"
        );
        assert_eq!(s.clocks.get("subdir/moved.txt").get("dell"), 2);
        assert_eq!(s.clocks.get("old.txt"), VectorClock::new());
    }

    // Rename of a non-existent file returns NOT_FOUND.
    #[tokio::test]
    async fn rename_nonexistent_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        let r = s
            .rename_file(rename_req(
                "nonexistent.txt",
                "new.txt",
                &[("dell", 1)],
                "dell",
            ))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(r.result, rename_file_response::Result::NotFound as i32);
        assert!(!exists(dir.path(), "new.txt"));
    }

    // Rename stale: the old path has a newer edit the renaming device hasn't seen.
    // The rename is rejected (STALE) and the file stays at old_path.
    #[tokio::test]
    async fn rename_stale_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        s.write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap();
        // Another edit advances the clock
        s.write_file(write_req("f.txt", b"v2", &[("dell", 2)], "dell"))
            .await
            .unwrap();

        // Rename with a stale clock — the file already moved to {dell:2}
        let r = s
            .rename_file(rename_req("f.txt", "renamed.txt", &[("dell", 1)], "dell"))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(r.result, rename_file_response::Result::Stale as i32);
        assert!(exists(dir.path(), "f.txt"), "file stays at old path");
        assert!(!exists(dir.path(), "renamed.txt"), "rename did not happen");
        assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"v2");
    }

    // Rename of a deleted file: old path is tombstoned, rename is stale.
    #[tokio::test]
    async fn rename_of_deleted_file_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(dir.path());

        s.write_file(write_req("f.txt", b"v1", &[("dell", 1)], "dell"))
            .await
            .unwrap();
        s.delete_file(delete_req("f.txt", &[("dell", 2)], "dell"))
            .await
            .unwrap();

        // Try to rename the deleted file
        let r = s
            .rename_file(rename_req("f.txt", "renamed.txt", &[("dell", 1)], "dell"))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(r.result, rename_file_response::Result::Stale as i32);
        assert!(!exists(dir.path(), "renamed.txt"));
    }

    // -----------------------------------------------------------------------
    // GC tests T1–T5 (ADR 0011)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn t1_ttl_evicts_old_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let _tombstones = temp_tombstone_store();
        let clocks = temp_store();
        let write_lock = Arc::new(tokio::sync::Mutex::new(()));

        // Create tombstones with non-empty clocks so we can distinguish
        // "not found" from "found with empty clock".
        let mut raw = BTreeMap::new();
        raw.insert(
            "old.txt".to_string(),
            TombstoneEntry {
                clock: clock(&[("dell", 1)]),
                created_at: 1,
            },
        );
        raw.insert(
            "old2.txt".to_string(),
            TombstoneEntry {
                clock: clock(&[("dell", 1)]),
                created_at: 0,
            },
        );
        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("tombstones.json");
        let json = serde_json::to_string_pretty(&raw).unwrap();
        std::fs::write(&store_path, &json).unwrap();
        let ts = TombstoneStore::open(store_path).unwrap();

        let config = GcConfig {
            interval: std::time::Duration::from_secs(3600),
            tombstone_ttl: std::time::Duration::from_secs(1),
            max_entries: 50000,
        };
        gc_sweep(dir.path(), &clocks, &ts, &write_lock, &config, 100)
            .await
            .unwrap();

        // "old.txt" (created_at=1) past TTL → removed
        assert_eq!(ts.len(), 1, "only old2.txt should remain");
        assert!(
            ts.entries().iter().any(|(k, _)| k == "old2.txt"),
            "old2.txt should be kept"
        );
    }

    #[tokio::test]
    async fn t2_live_file_clock_entry_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let clocks = temp_store();
        let tombstones = temp_tombstone_store();
        let write_lock = Arc::new(tokio::sync::Mutex::new(()));

        // Create a real file and a clock entry for it.
        std::fs::write(dir.path().join("live.txt"), b"hello").unwrap();
        clocks.put("live.txt", clock(&[("dell", 1)])).unwrap();

        let config = GcConfig {
            interval: std::time::Duration::from_secs(3600),
            tombstone_ttl: std::time::Duration::from_secs(1),
            max_entries: 0, // aggressive cap
        };
        gc_sweep(dir.path(), &clocks, &tombstones, &write_lock, &config, 100)
            .await
            .unwrap();

        // Live file's clock entry MUST be preserved.
        assert_eq!(clocks.get("live.txt").get("dell"), 1);
    }

    #[tokio::test]
    async fn t3_hard_cap_evicts_oldest_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let _tombstones = temp_tombstone_store();
        let clocks = temp_store();
        let write_lock = Arc::new(tokio::sync::Mutex::new(()));

        // Create a tombstone store with entries directly.
        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("tombstones.json");
        let mut raw = BTreeMap::new();
        // Entry A: created_at = 1 (oldest)
        raw.insert(
            "a.txt".to_string(),
            TombstoneEntry {
                clock: clock(&[("dell", 1)]),
                created_at: 1,
            },
        );
        // Entry B: created_at = 2
        raw.insert(
            "b.txt".to_string(),
            TombstoneEntry {
                clock: clock(&[("dell", 1)]),
                created_at: 2,
            },
        );
        let json = serde_json::to_string_pretty(&raw).unwrap();
        std::fs::write(&store_path, &json).unwrap();
        let ts = TombstoneStore::open(store_path.clone()).unwrap();

        // Cap at 1 entry — should evict the oldest (a.txt).
        let config = GcConfig {
            interval: std::time::Duration::from_secs(3600),
            tombstone_ttl: std::time::Duration::from_secs(9999), // no TTL eviction
            max_entries: 1,
        };
        gc_sweep(dir.path(), &clocks, &ts, &write_lock, &config, 100)
            .await
            .unwrap();

        assert_eq!(ts.len(), 1, "only 1 entry should remain after cap eviction");
        assert!(
            ts.entries().iter().any(|(k, _)| k == "b.txt"),
            "b.txt should be kept"
        );
    }

    #[tokio::test]
    async fn t4_hard_cap_does_not_evict_live_clock_entries() {
        let dir = tempfile::tempdir().unwrap();
        let clocks = temp_store();
        let tombstones = temp_tombstone_store();
        let write_lock = Arc::new(tokio::sync::Mutex::new(()));

        // Create live files with clock entries.
        for i in 0..5 {
            let name = format!("live{i}.txt");
            std::fs::write(dir.path().join(&name), b"data").unwrap();
            clocks.put(&name, clock(&[("dell", 1)])).unwrap();
        }

        // Cap at 2 — should NOT evict live entries, just log a warning.
        let config = GcConfig {
            interval: std::time::Duration::from_secs(3600),
            tombstone_ttl: std::time::Duration::from_secs(1),
            max_entries: 2,
        };
        gc_sweep(dir.path(), &clocks, &tombstones, &write_lock, &config, 100)
            .await
            .unwrap();

        // All live entries still present.
        for i in 0..5 {
            let name = format!("live{i}.txt");
            assert_eq!(
                clocks.get(&name).get("dell"),
                1,
                "{name} should be preserved"
            );
        }
    }

    #[tokio::test]
    async fn t5_gc_skips_contested_entries() {
        let dir = tempfile::tempdir().unwrap();
        let clocks = temp_store();
        let _tombstones = temp_tombstone_store();
        let write_lock = Arc::new(tokio::sync::Mutex::new(()));

        // Create a tombstone past TTL.
        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("tombstones.json");
        let mut raw = BTreeMap::new();
        raw.insert(
            "contested.txt".to_string(),
            TombstoneEntry {
                clock: clock(&[("dell", 1)]),
                created_at: 1,
            },
        );
        let json = serde_json::to_string_pretty(&raw).unwrap();
        std::fs::write(&store_path, &json).unwrap();
        let ts = TombstoneStore::open(store_path).unwrap();

        // Hold the write_lock so GC can't acquire it.
        let _guard = write_lock.lock().await;

        let config = GcConfig {
            interval: std::time::Duration::from_secs(3600),
            tombstone_ttl: std::time::Duration::from_secs(1),
            max_entries: 50000,
        };
        gc_sweep(dir.path(), &clocks, &ts, &write_lock, &config, 100)
            .await
            .unwrap();

        // The contested entry was NOT removed (lock was held).
        assert_eq!(ts.len(), 1, "contested entry should not be removed");
    }

    #[tokio::test]
    async fn t11_mtls_integration() {
        let tmp = tempfile::tempdir().unwrap();
        let serve_dir = tmp.path().join("serve");
        std::fs::create_dir_all(&serve_dir).unwrap();

        // Generate self-signed TLS identities with unique Common Names so the
        // rustls WebPkiClientVerifier correctly rejects unpaired certs at the
        // TLS layer instead of conflating them via rcgen's default Subject DN.
        let server_id = nexus_common::DeviceId(uuid::Uuid::new_v4());
        let paired_id = nexus_common::DeviceId(uuid::Uuid::new_v4());
        let unpaired_id = nexus_common::DeviceId(uuid::Uuid::new_v4());

        let gen = |cn: &str, sans: Vec<String>| {
            let key_pair = rcgen::KeyPair::generate().unwrap();
            let mut params = rcgen::CertificateParams::new(sans).unwrap();
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, cn);
            let cert = params.self_signed(&key_pair).unwrap();
            rcgen::CertifiedKey { cert, key_pair }
        };
        let server = gen(
            "server",
            vec![
                "localhost".into(),
                "127.0.0.1".into(),
                server_id.to_string(),
            ],
        );
        let paired = gen(
            &paired_id.to_string(),
            vec![
                "localhost".into(),
                "127.0.0.1".into(),
                paired_id.to_string(),
            ],
        );
        let unpaired = gen(
            &unpaired_id.to_string(),
            vec![
                "localhost".into(),
                "127.0.0.1".into(),
                unpaired_id.to_string(),
            ],
        );

        let server_cert_pem = server.cert.pem();
        let server_key_pem = server.key_pair.serialize_pem();
        let paired_cert_pem = paired.cert.pem();
        let paired_key_pem = paired.key_pair.serialize_pem();
        let unpaired_cert_pem = unpaired.cert.pem();
        let unpaired_key_pem = unpaired.key_pair.serialize_pem();

        // PeersStore with only the paired device.
        let peers = Arc::new(crate::pairing::PeersStore::open_in(tmp.path()).unwrap());
        peers
            .add(&paired_id, paired_cert_pem.clone(), "paired-device".into())
            .unwrap();

        // Server TLS config — mirrors production logic in run().
        let ca_pem = build_peer_ca_pem(&peers).unwrap();
        let tls_config = ServerTlsConfig::new()
            .identity(Identity::from_pem(&server_cert_pem, &server_key_pem))
            .client_ca_root(Certificate::from_pem(ca_pem))
            .client_auth_optional(true);

        let auth_token = "test-token-12345";

        // Interceptor — identical to the production closure in run().
        let interceptor = {
            let peers = peers.clone();
            let expected = auth_token.to_string();
            move |req: Request<()>| -> Result<Request<()>, Status> {
                if let Some(certs) = req
                    .extensions()
                    .get::<TlsConnectInfo<TcpConnectInfo>>()
                    .and_then(|tls| tls.peer_certs())
                {
                    if let Some(cert_der) = certs.first() {
                        let device_id = extract_device_id_from_cert(cert_der);
                        match device_id {
                            Some(ref did) if peers.verify_cert_der(did, cert_der) => {
                                return Ok(req)
                            }
                            _ => {
                                return Err(Status::unauthenticated(
                                    "client certificate not trusted",
                                ))
                            }
                        }
                    }
                }
                match req.metadata().get(AUTH_HEADER) {
                    Some(t) if token_matches(t.as_bytes(), expected.as_bytes()) => Ok(req),
                    _ => Err(Status::unauthenticated("missing or invalid auth token")),
                }
            }
        };

        // Bind a TCP listener before spawning the server so the port is
        // reserved atomically — eliminates the TOCTOU race between probing
        // for a free port and the server binding it.
        let std_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binding test listener");
        std_listener
            .set_nonblocking(true)
            .expect("setting non-blocking");
        let port = std_listener
            .local_addr()
            .expect("getting local addr")
            .port();
        let tokio_listener =
            tokio::net::TcpListener::from_std(std_listener).expect("converting to tokio listener");
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(tokio_listener);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let svc = svc(&serve_dir);

        let server_handle = tokio::spawn(async move {
            Server::builder()
                .tls_config(tls_config)
                .unwrap()
                .add_service(FileServiceServer::with_interceptor(svc, interceptor))
                .serve_with_incoming_shutdown(incoming, async {
                    shutdown_rx.await.ok();
                })
                .await
        });

        let server_addr = format!("https://127.0.0.1:{port}");

        use tonic::transport::{Channel, ClientTlsConfig};

        // ── Case (a): paired client cert → authenticated ──
        let channel = Channel::from_shared(server_addr.clone())
            .unwrap()
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(server_cert_pem.clone()))
                    .identity(Identity::from_pem(&paired_cert_pem, &paired_key_pem))
                    .domain_name("localhost"),
            )
            .unwrap()
            .connect()
            .await;
        assert!(
            channel.is_ok(),
            "case (a): paired cert should authenticate at TLS layer"
        );
        let mut client =
            nexus_proto::fs::v1::file_service_client::FileServiceClient::new(channel.unwrap());
        let stat = client
            .stat(Request::new(nexus_proto::fs::v1::StatRequest {
                path: "/".to_string(),
            }))
            .await;
        assert!(
            stat.is_ok(),
            "case (a): stat should succeed with trusted cert"
        );

        // ── Case (b): unpaired client cert → rejected by interceptor ──
        let channel = Channel::from_shared(server_addr.clone())
            .unwrap()
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(server_cert_pem.clone()))
                    .identity(Identity::from_pem(&unpaired_cert_pem, &unpaired_key_pem))
                    .domain_name("localhost"),
            )
            .unwrap()
            .connect()
            .await;
        assert!(
            channel.is_ok(),
            "case (b): unpaired cert should connect (client_auth_optional)"
        );
        let mut client =
            nexus_proto::fs::v1::file_service_client::FileServiceClient::new(channel.unwrap());
        let stat = client
            .stat(Request::new(nexus_proto::fs::v1::StatRequest {
                path: "/".to_string(),
            }))
            .await;
        assert!(
            stat.is_err(),
            "case (b): stat should fail for unpaired cert (interceptor rejects)"
        );

        // ── Case (c): no client cert + valid token → token auth succeeds ──
        let channel = Channel::from_shared(server_addr.clone())
            .unwrap()
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(server_cert_pem))
                    .domain_name("localhost"),
            )
            .unwrap()
            .connect()
            .await;
        assert!(
            channel.is_ok(),
            "case (c): no cert should connect (client_auth_optional)"
        );
        let mut client =
            nexus_proto::fs::v1::file_service_client::FileServiceClient::new(channel.unwrap());
        let mut req = Request::new(nexus_proto::fs::v1::StatRequest {
            path: "/".to_string(),
        });
        req.metadata_mut().insert(
            AUTH_HEADER,
            auth_token
                .parse::<tonic::metadata::MetadataValue<_>>()
                .unwrap(),
        );
        let stat = client.stat(req).await;
        assert!(
            stat.is_ok(),
            "case (c): stat with valid token should succeed"
        );

        // Clean shutdown.
        shutdown_tx.send(()).unwrap();
        server_handle.await.unwrap().unwrap();
    }
}
