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
}

impl FileServiceImpl {
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

    let addr = format!("0.0.0.0:{port}").parse()?;
    let service = FileServiceImpl { root };

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

    fn impl_for(root: &std::path::Path) -> FileServiceImpl {
        FileServiceImpl {
            root: root.canonicalize().unwrap(),
        }
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
}
