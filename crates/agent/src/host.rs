//! The HOST role: implements FileService over gRPC, serving this
//! device's own files to whatever client agent connects (typically
//! the Dell's nexus-fs FUSE mount).
//!
//! Deliberately naive for milestone 1 — direct std::fs calls,
//! no caching, no auth/pairing check yet (that's the control-plane
//! piece, added once this data-plane path is proven). Do not expose
//! this on an untrusted network as-is.

use anyhow::Result;
use nexus_common::{ClockOrder, ClockStore, VectorClock};
use nexus_proto::fs::v1::{
    file_service_server::{FileService, FileServiceServer},
    write_file_response, FileEntry, ListDirRequest, ListDirResponse, ReadFileChunk, ReadFileRequest,
    StatRequest, StatResponse, WriteFileRequest, WriteFileResponse,
};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_stream::Stream;
use tonic::{
    transport::{Identity, Server, ServerTlsConfig},
    Request, Response, Status,
};

/// gRPC metadata header carrying the shared-secret auth token (ADR 0004).
const AUTH_HEADER: &str = "x-nexus-token";

const CHUNK_SIZE: usize = 64 * 1024; // 64KiB — small enough to keep memory flat,
                                      // large enough to not drown in RPC overhead
                                      // over a phone's wifi link.

pub struct FileServiceImpl {
    root: PathBuf,
    /// Per-file vector clocks (path -> clock). Shared, internally synchronized.
    clocks: Arc<ClockStore>,
    /// Serializes the read-compare-write critical section of WriteFile so two
    /// concurrent writes to the same path can't interleave and corrupt the
    /// conflict decision. Coarse (one writer at a time, any path) but correct;
    /// see ADR 0005 for the scalability note.
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

pub async fn run(serve_dir: String, port: u16, auth_token: String) -> Result<()> {
    let root = PathBuf::from(&serve_dir)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&serve_dir));

    std::fs::create_dir_all(&root)?;

    // Per-file vector clocks live next to the rest of the agent's state.
    let clocks_path = crate::config::config_dir()?.join("clocks.json");
    let clocks = Arc::new(ClockStore::open(clocks_path)?);

    let addr = format!("0.0.0.0:{port}").parse()?;
    let service = FileServiceImpl {
        root,
        clocks,
        write_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    // Load (or generate on first run) the self-signed TLS identity.
    let tls = crate::config::load_or_create_tls_identity()?;
    let identity = Identity::from_pem(&tls.cert_pem, &tls.key_pem);

    // Auth interceptor: runs before any FileService method dispatches, so an
    // unauthenticated request is rejected before it can reach resolve() or
    // touch the filesystem. The expected token is moved into the closure.
    let expected = auth_token;
    let interceptor = move |req: Request<()>| -> Result<Request<()>, Status> {
        match req.metadata().get(AUTH_HEADER) {
            Some(t) if token_matches(t.as_bytes(), expected.as_bytes()) => Ok(req),
            _ => Err(Status::unauthenticated("missing or invalid auth token")),
        }
    };

    tracing::info!(cert = %tls.cert_path.display(), "TLS enabled (self-signed); clients must trust this cert");
    tracing::info!(%addr, "FileService listening (TLS + token auth)");

    Server::builder()
        .tls_config(ServerTlsConfig::new().identity(identity))?
        .add_service(FileServiceServer::with_interceptor(service, interceptor))
        .serve(addr)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_store() -> std::sync::Arc<ClockStore> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir()
            .join(format!("nexus-test-clocks-{}-{}.json", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        std::sync::Arc::new(ClockStore::open(path).unwrap())
    }

    fn svc(root: &std::path::Path) -> FileServiceImpl {
        FileServiceImpl {
            root: root.canonicalize().unwrap(),
            clocks: temp_store(),
            write_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn impl_for(root: &std::path::Path) -> FileServiceImpl {
        svc(root)
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

    #[test]
    fn resolves_legit_paths_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/x.txt"), b"x").unwrap();
        let svc = impl_for(dir.path());

        assert!(svc.resolve("hello.txt").unwrap().ends_with("hello.txt"));
        assert!(svc.resolve("sub/x.txt").unwrap().ends_with("x.txt"));
        // a leading slash is treated as root-relative, never absolute
        assert!(svc.resolve("/hello.txt").unwrap().ends_with("hello.txt"));
    }

    #[test]
    fn rejects_dotdot_traversal() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("share");
        fs::create_dir(&root).unwrap();
        // a secret sibling OUTSIDE the served root
        fs::write(parent.path().join("secret.txt"), b"top secret").unwrap();
        let svc = impl_for(&root);

        let err = svc.resolve("../secret.txt").unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::PermissionDenied,
            "../ escape must be denied"
        );
        // deep traversal to a real system file must never succeed
        assert!(svc.resolve("../../../../../../etc/passwd").is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("share");
        fs::create_dir(&root).unwrap();
        let outside = parent.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"top secret").unwrap();

        // a symlink INSIDE root pointing to a dir OUTSIDE it
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        // and one pointing directly at an outside file
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
        let svc = impl_for(&root);

        assert_eq!(
            svc.resolve("escape/secret.txt").unwrap_err().code(),
            tonic::Code::PermissionDenied,
            "symlinked-dir escape must be denied"
        );
        assert_eq!(
            svc.resolve("link.txt").unwrap_err().code(),
            tonic::Code::PermissionDenied,
            "symlinked-file escape must be denied"
        );
    }

    #[test]
    fn token_compare_is_correct() {
        assert!(token_matches(b"abc123", b"abc123"));
        assert!(!token_matches(b"abc123", b"abc124"));
        assert!(!token_matches(b"abc", b"abc123")); // length mismatch
        assert!(!token_matches(b"", b"x"));
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
            .write_file(write_req("f.txt", b"v2", &[("dell", 1), ("host", 1)], "host"))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            r2.result,
            write_file_response::Result::Applied as i32,
            "dominance must resolve cleanly, not as a conflict"
        );
        assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"v2");
        assert!(conflict_files(dir.path()).is_empty(), "no conflict file expected");
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
            .write_file(write_req("f.txt", b"host-edit", &[("dell", 1), ("host", 1)], "host"))
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
        assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"host-edit");
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
}
