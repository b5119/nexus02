use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Linux,
    MacOs,
    Windows,
    Android,
}

impl DeviceKind {
    pub fn supports_fuse_client(&self) -> bool {
        matches!(
            self,
            DeviceKind::Linux | DeviceKind::MacOs | DeviceKind::Windows
        )
    }
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock(pub BTreeMap<String, u64>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockOrder {
    Equal,
    Dominates,
    DominatedBy,
    Concurrent,
}

impl VectorClock {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn get(&self, device: &str) -> u64 {
        self.0.get(device).copied().unwrap_or(0)
    }

    pub fn increment(&mut self, device: &str) {
        *self.0.entry(device.to_string()).or_insert(0) += 1;
    }

    pub fn compare(&self, other: &VectorClock) -> ClockOrder {
        let mut self_greater = false;
        let mut other_greater = false;

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

    pub fn merge(&self, other: &VectorClock) -> VectorClock {
        let mut out = self.0.clone();
        for (device, &counter) in &other.0 {
            let slot = out.entry(device.clone()).or_insert(0);
            *slot = (*slot).max(counter);
        }
        VectorClock(out)
    }
}

// ---------------------------------------------------------------------------
// Clock and tombstone entries with timestamps (GC — see ADR 0011)
// ---------------------------------------------------------------------------

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TombstoneEntry {
    pub clock: VectorClock,
    #[serde(default)]
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClockEntry {
    pub clock: VectorClock,
    #[serde(default)]
    pub last_updated_at: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TombstoneFileValue {
    Old(VectorClock),
    New(TombstoneEntry),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ClockFileValue {
    Old(VectorClock),
    New(ClockEntry),
}

// ---------------------------------------------------------------------------
// ClockStore — per-agent clock metadata
// ---------------------------------------------------------------------------

pub struct ClockStore {
    path: PathBuf,
    inner: Mutex<BTreeMap<String, ClockEntry>>,
}

impl ClockStore {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let map: BTreeMap<String, ClockFileValue> = serde_json::from_str(&raw)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            map.into_iter()
                .map(|(k, v)| match v {
                    ClockFileValue::Old(clock) => (
                        k,
                        ClockEntry {
                            clock,
                            last_updated_at: 0,
                        },
                    ),
                    ClockFileValue::New(entry) => (k, entry),
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    pub fn get(&self, key: &str) -> VectorClock {
        let map = self.inner.lock().unwrap();
        map.get(key).map(|e| e.clock.clone()).unwrap_or_default()
    }

    pub fn put(&self, key: &str, clock: VectorClock) -> std::io::Result<()> {
        let mut map = self.inner.lock().unwrap();
        if let Some(entry) = map.get_mut(key) {
            entry.clock = clock;
            entry.last_updated_at = now_unix();
        } else {
            map.insert(
                key.to_string(),
                ClockEntry {
                    clock,
                    last_updated_at: now_unix(),
                },
            );
        }
        persist(&self.path, &*map)
    }

    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        let mut map = self.inner.lock().unwrap();
        if map.remove(key).is_some() {
            persist(&self.path, &*map)?;
        }
        Ok(())
    }

    pub fn entries(&self) -> Vec<(String, ClockEntry)> {
        let map = self.inner.lock().unwrap();
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn len(&self) -> usize {
        let map = self.inner.lock().unwrap();
        map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// TombstoneStore — tracks deleted paths for conflict detection + GC
// ---------------------------------------------------------------------------

pub struct TombstoneStore {
    path: PathBuf,
    inner: Mutex<BTreeMap<String, TombstoneEntry>>,
}

impl TombstoneStore {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            let map: BTreeMap<String, TombstoneFileValue> = serde_json::from_str(&raw)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            map.into_iter()
                .map(|(k, v)| match v {
                    TombstoneFileValue::Old(clock) => (
                        k,
                        TombstoneEntry {
                            clock,
                            created_at: 0,
                        },
                    ),
                    TombstoneFileValue::New(entry) => (k, entry),
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    pub fn get(&self, key: &str) -> VectorClock {
        let map = self.inner.lock().unwrap();
        map.get(key).map(|e| e.clock.clone()).unwrap_or_default()
    }

    pub fn put(&self, key: &str, clock: VectorClock) -> std::io::Result<()> {
        let mut map = self.inner.lock().unwrap();
        if let Some(entry) = map.get_mut(key) {
            entry.clock = clock;
        } else {
            map.insert(
                key.to_string(),
                TombstoneEntry {
                    clock,
                    created_at: now_unix(),
                },
            );
        }
        persist(&self.path, &*map)
    }

    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        let mut map = self.inner.lock().unwrap();
        if map.remove(key).is_some() {
            persist(&self.path, &*map)?;
        }
        Ok(())
    }

    pub fn entries(&self) -> Vec<(String, TombstoneEntry)> {
        let map = self.inner.lock().unwrap();
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn len(&self) -> usize {
        let map = self.inner.lock().unwrap();
        map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// Persistence helper — atomic write via temp file + rename
// ---------------------------------------------------------------------------

fn persist<T: Serialize>(path: &std::path::Path, map: &BTreeMap<String, T>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
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
        assert_eq!(
            clock(&[("a", 1), ("b", 1)]).compare(&clock(&[("a", 1)])),
            ClockOrder::Dominates
        );
        assert_eq!(
            clock(&[("a", 1)]).compare(&clock(&[("a", 1), ("b", 1)])),
            ClockOrder::DominatedBy
        );
        assert_eq!(
            clock(&[("a", 1)]).compare(&VectorClock::new()),
            ClockOrder::Dominates
        );
    }

    #[test]
    fn concurrent_is_a_conflict() {
        assert_eq!(
            clock(&[("dell", 2)]).compare(&clock(&[("dell", 1), ("phone", 1)])),
            ClockOrder::Concurrent
        );
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
        let store = ClockStore::open(file).unwrap();
        assert_eq!(store.get("dir/f.txt"), clock(&[("dell", 3)]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tombstone_store_roundtrips_and_persists() {
        let dir = std::env::temp_dir().join(format!("nexus-tombstone-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("tombstones.json");

        {
            let store = TombstoneStore::open(file.clone()).unwrap();
            assert_eq!(store.get("missing"), VectorClock::new());
            store.put("del.txt", clock(&[("dell", 2)])).unwrap();
        }
        let store = TombstoneStore::open(file).unwrap();
        assert_eq!(store.get("del.txt"), clock(&[("dell", 2)]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tombstone_entry_gets_created_at() {
        let dir = std::env::temp_dir().join(format!(
            "nexus-tombstone-created-at-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("tombstones.json");

        let store = TombstoneStore::open(file).unwrap();
        store.put("f.txt", clock(&[("a", 1)])).unwrap();
        let entries = store.entries();
        let (_, entry) = entries.iter().find(|(k, _)| k == "f.txt").unwrap();
        assert!(
            entry.created_at > 0,
            "new tombstone should have created_at set"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
