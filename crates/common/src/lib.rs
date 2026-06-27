//! Shared types used by every nexus crate: agent, fs, and (later)
//! stream and migrate-sdk. Keeping these here avoids circular deps
//! and means the wire format (DeviceId, FileEntry, etc.) only has
//! one definition that proto/agent/fs all agree on.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
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
        matches!(
            self,
            DeviceKind::Linux | DeviceKind::MacOs | DeviceKind::Windows
        )
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

// ---------------------------------------------------------------------------
// Vector clocks (multi-writer conflict detection — see docs/adr/0005)
// ---------------------------------------------------------------------------

/// A version vector for a single file: device id (as string) -> logical counter.
/// Stored as per-agent metadata, never inside file content. A device increments
/// *its own* counter on each local write.
///
/// BTreeMap (not HashMap) for deterministic serialization/iteration — makes the
/// JSON store stable and tests reproducible.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock(pub BTreeMap<String, u64>);

/// Result of comparing two vector clocks. `Concurrent` is the only one that
/// represents a real conflict (independent edits that neither supersedes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockOrder {
    Equal,
    /// `self` strictly dominates `other` (self is the newer, unambiguous version).
    Dominates,
    /// `other` strictly dominates `self` (self is stale).
    DominatedBy,
    /// Neither dominates — a genuine conflict.
    Concurrent,
}

impl VectorClock {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn get(&self, device: &str) -> u64 {
        self.0.get(device).copied().unwrap_or(0)
    }

    /// Increment this device's own counter (call on a local write).
    pub fn increment(&mut self, device: &str) {
        *self.0.entry(device.to_string()).or_insert(0) += 1;
    }

    /// Compare against another clock by the standard version-vector partial order:
    /// dominance requires every counter >= the other's (missing == 0) with at
    /// least one strictly greater; if each has a counter the other lacks/trails,
    /// they're concurrent.
    pub fn compare(&self, other: &VectorClock) -> ClockOrder {
        let mut self_greater = false;
        let mut other_greater = false;

        // Walk the union of device ids present in either clock.
        for device in self.0.keys().chain(other.0.keys()) {
            let a = self.get(device);
            let b = other.get(device);
            if a > b {
                self_greater = true;
            } else if b > a {
                other_greater = true;
            }
        }

        match (self_greater, other_greater) {
            (false, false) => ClockOrder::Equal,
            (true, false) => ClockOrder::Dominates,
            (false, true) => ClockOrder::DominatedBy,
            (true, true) => ClockOrder::Concurrent,
        }
    }

    /// Pairwise max of the two clocks (the least upper bound). Used when a write
    /// is applied so the stored clock subsumes everything seen so far.
    pub fn merge(&self, other: &VectorClock) -> VectorClock {
        let mut out = self.0.clone();
        for (device, &counter) in &other.0 {
            let slot = out.entry(device.clone()).or_insert(0);
            *slot = (*slot).max(counter);
        }
        VectorClock(out)
    }
}

/// A tiny, thread-safe, per-agent metadata store mapping a (relative) file path
/// to its vector clock. Persisted as a single atomic JSON file.
///
/// Deliberately NOT an embedded DB (sled/redb): for milestone-scale metadata an
/// atomic JSON map is lighter to integrate, has no heavy/Cross-compile-fragile
/// dependencies, and keeps the Android build clean — see ADR 0005. The whole map
/// is rewritten on each put (temp file + rename for atomicity); fine at this
/// scale, revisit if the clock set ever gets large.
pub struct ClockStore {
    path: PathBuf,
    inner: Mutex<BTreeMap<String, VectorClock>>,
}

impl ClockStore {
    /// Open (or initialize) the store at `path`. Missing file => empty store.
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let map: BTreeMap<String, VectorClock> = serde_json::from_str(&raw)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            map
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    /// The clock for `key`, or an empty clock if the path is unknown.
    pub fn get(&self, key: &str) -> VectorClock {
        let map = self.inner.lock().unwrap();
        map.get(key).cloned().unwrap_or_default()
    }

    /// Store `clock` for `key` and persist the whole map atomically.
    pub fn put(&self, key: &str, clock: VectorClock) -> std::io::Result<()> {
        let mut map = self.inner.lock().unwrap();
        map.insert(key.to_string(), clock);
        persist(&self.path, &map)
    }

    /// Remove `key` (if present) and persist. Used to move a path between the
    /// live-clock store and the tombstone store (ADR 0008).
    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        let mut map = self.inner.lock().unwrap();
        if map.remove(key).is_some() {
            persist(&self.path, &map)?;
        }
        Ok(())
    }
}

fn persist(path: &std::path::Path, map: &BTreeMap<String, VectorClock>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // temp + rename so a crash mid-write can't truncate the store
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    fn clock(pairs: &[(&str, u64)]) -> VectorClock {
        VectorClock(pairs.iter().map(|(d, c)| (d.to_string(), *c)).collect())
    }

    #[test]
    fn equal_clocks() {
        assert_eq!(
            clock(&[("a", 1)]).compare(&clock(&[("a", 1)])),
            ClockOrder::Equal
        );
        assert_eq!(
            VectorClock::new().compare(&VectorClock::new()),
            ClockOrder::Equal
        );
    }

    #[test]
    fn dominance() {
        // {a:1,b:1} dominates {a:1}
        assert_eq!(
            clock(&[("a", 1), ("b", 1)]).compare(&clock(&[("a", 1)])),
            ClockOrder::Dominates
        );
        // and the reverse is DominatedBy
        assert_eq!(
            clock(&[("a", 1)]).compare(&clock(&[("a", 1), ("b", 1)])),
            ClockOrder::DominatedBy
        );
        // a non-empty clock dominates the empty one (new file case)
        assert_eq!(
            clock(&[("a", 1)]).compare(&VectorClock::new()),
            ClockOrder::Dominates
        );
    }

    #[test]
    fn concurrent_is_a_conflict() {
        // Dell bumped its own counter while phone independently bumped its own:
        // {dell:2} vs {dell:1,phone:1} — neither dominates.
        assert_eq!(
            clock(&[("dell", 2)]).compare(&clock(&[("dell", 1), ("phone", 1)])),
            ClockOrder::Concurrent
        );
        // disjoint clocks are also concurrent
        assert_eq!(
            clock(&[("a", 1)]).compare(&clock(&[("b", 1)])),
            ClockOrder::Concurrent
        );
    }

    #[test]
    fn increment_and_merge() {
        let mut c = clock(&[("a", 1)]);
        c.increment("a");
        c.increment("b");
        assert_eq!(c.get("a"), 2);
        assert_eq!(c.get("b"), 1);

        let merged = clock(&[("a", 2), ("b", 1)]).merge(&clock(&[("a", 1), ("c", 5)]));
        assert_eq!(merged, clock(&[("a", 2), ("b", 1), ("c", 5)]));
    }

    #[test]
    fn store_roundtrips_and_persists() {
        let dir =
            std::env::temp_dir().join(format!("nexus-clockstore-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("clocks.json");

        {
            let store = ClockStore::open(file.clone()).unwrap();
            assert_eq!(store.get("missing"), VectorClock::new());
            store.put("dir/f.txt", clock(&[("dell", 3)])).unwrap();
        }
        // reopen: state survived
        let store = ClockStore::open(file).unwrap();
        assert_eq!(store.get("dir/f.txt"), clock(&[("dell", 3)]));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
