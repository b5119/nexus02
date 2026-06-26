//! Thin wrapper around the generated tonic client for FileService.
//! Kept separate from filesystem.rs so the FUSE trait implementation
//! (which is synchronous, per the fuser API) doesn't get tangled up
//! with async/await directly — see filesystem.rs for how that boundary
//! is bridged.
//!
//! Every call is made over TLS and carries the shared-secret auth token as
//! gRPC metadata (see ADR 0004). The token/cert come from the host agent's
//! config dir.

use anyhow::{Context, Result};
use nexus_proto::fs::v1::FileEntry;
use nexus_proto::fs::v1::{
    file_service_client::FileServiceClient, ListDirRequest, ReadFileRequest, StatRequest,
    WriteFileRequest, WriteFileResponse,
};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
use tonic::Request;

/// gRPC metadata header carrying the shared-secret auth token (ADR 0004).
const AUTH_HEADER: &str = "x-nexus-token";

#[derive(Clone)]
pub struct RemoteFs {
    client: FileServiceClient<Channel>,
    token: MetadataValue<Ascii>,
}

impl RemoteFs {
    /// Connects to a remote agent over TLS — verifying its self-signed cert
    /// against `ca_pem` (the cert the agent generated) — and authenticates with
    /// `token`. Eagerly probes the connection with a `stat("/")` so a wrong
    /// token or unreachable host fails *here*, at mount time, with a clear
    /// error rather than later as an opaque EIO on the first `ls`.
    pub async fn connect(addr: String, ca_pem: String, token: String) -> Result<Self> {
        let token: MetadataValue<Ascii> = token
            .parse()
            .context("auth token must be valid ASCII")?;

        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca_pem))
            // The agent's self-signed cert has SAN localhost/127.0.0.1; verify
            // the server identity against that name regardless of dialed IP.
            .domain_name("localhost");

        let channel = Channel::from_shared(addr)
            .context("invalid remote address (expected e.g. https://127.0.0.1:50051)")?
            .tls_config(tls)
            .context("configuring client TLS")?
            .connect()
            .await
            .context("connecting to remote agent (TLS handshake failed?)")?;

        let me = Self {
            client: FileServiceClient::new(channel),
            token,
        };

        // Auth + reachability probe — fail fast on a bad token.
        me.stat("/")
            .await
            .context("auth/connectivity check failed (wrong token, or agent unreachable?)")?;

        Ok(me)
    }

    /// Wraps a request body with the auth-token metadata header.
    fn authed<T>(&self, msg: T) -> Request<T> {
        let mut req = Request::new(msg);
        req.metadata_mut().insert(AUTH_HEADER, self.token.clone());
        req
    }

    pub async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>> {
        let mut client = self.client.clone();
        let resp = client
            .list_dir(self.authed(ListDirRequest {
                path: path.to_string(),
            }))
            .await?;
        Ok(resp.into_inner().entries)
    }

    pub async fn stat(&self, path: &str) -> Result<Option<FileEntry>> {
        Ok(self.stat_full(path).await?.map(|(entry, _clock)| entry))
    }

    /// Like `stat`, but also returns the file's vector clock so the FUSE layer
    /// can sync this client's clock knowledge on a read-intent open (ADR 0007).
    pub async fn stat_full(
        &self,
        path: &str,
    ) -> Result<Option<(FileEntry, nexus_common::VectorClock)>> {
        let mut client = self.client.clone();
        let resp = client
            .stat(self.authed(StatRequest {
                path: path.to_string(),
            }))
            .await?;
        let inner = resp.into_inner();
        if !inner.found {
            return Ok(None);
        }
        let entry = match inner.entry {
            Some(e) => e,
            None => return Ok(None),
        };
        let clock = match inner.clock {
            Some(c) => nexus_common::VectorClock(c.counters.into_iter().collect()),
            None => nexus_common::VectorClock::new(),
        };
        Ok(Some((entry, clock)))
    }

    /// Reads a byte range and returns it fully buffered.
    /// Fine for milestone 1 with 64KiB-ish FUSE read requests;
    /// revisit if profiling shows this is a bottleneck on large files.
    pub async fn read_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
        let mut client = self.client.clone();
        let mut stream = client
            .read_file(self.authed(ReadFileRequest {
                path: path.to_string(),
                offset,
                length,
            }))
            .await?
            .into_inner();

        let mut buf = Vec::with_capacity(length as usize);
        while let Some(chunk) = stream.message().await? {
            buf.extend_from_slice(&chunk.data);
        }
        Ok(buf)
    }

    /// Writes `data` to `path` on the host, carrying this client's vector clock
    /// (with its own counter already incremented). Returns the host's decision
    /// (Applied / Stale / Conflict) and the authoritative clock.
    ///
    /// Wired into the FUSE `write`/`create`/flush path (ADR 0006).
    pub async fn write_file(
        &self,
        path: &str,
        data: Vec<u8>,
        clock: &nexus_common::VectorClock,
        writer_device_id: &str,
    ) -> Result<WriteFileResponse> {
        let mut client = self.client.clone();
        let resp = client
            .write_file(self.authed(WriteFileRequest {
                path: path.to_string(),
                data,
                clock: Some(nexus_proto::fs::v1::VectorClock {
                    counters: clock.0.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                }),
                writer_device_id: writer_device_id.to_string(),
            }))
            .await?;
        Ok(resp.into_inner())
    }
}
