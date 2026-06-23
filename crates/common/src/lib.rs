//! Shared types used by every nexus crate: agent, fs, and (later)
//! stream and migrate-sdk. Keeping these here avoids circular deps
//! and means the wire format (DeviceId, FileEntry, etc.) only has
//! one definition that proto/agent/fs all agree on.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Uniquely identifies a device in the mesh (Dell, phone, tablet, etc).
/// Generated once on first agent run and persisted to disk —
/// see agent::config for where this gets stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub Uuid);

impl DeviceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DeviceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What kind of device this is — affects which capabilities the agent
/// advertises. A phone, for instance, will (for now) only ever act as
/// a host, never mount a remote filesystem itself, per the Android
/// FUSE limitation discussed in the architecture doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Linux,
    MacOs,
    Windows,
    Android,
}

impl DeviceKind {
    /// Whether this device kind can mount a remote filesystem via FUSE/WinFsp.
    /// Android returns false — it can only be browsed via the in-app UI,
    /// never mounted as a real filesystem for other apps to see.
    pub fn supports_fuse_client(&self) -> bool {
        matches!(self, DeviceKind::Linux | DeviceKind::MacOs | DeviceKind::Windows)
    }
}

/// A single file or directory entry, as returned by the host agent's
/// ListDir RPC. Deliberately minimal for milestone 1 — no permissions,
/// no extended attributes yet, just enough for a read-only mount to work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub modified_unix: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum NexusError {
    #[error("device not paired: {0}")]
    NotPaired(DeviceId),

    #[error("remote agent unreachable: {0}")]
    Unreachable(String),

    #[error("path not found: {0}")]
    NotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, NexusError>;
