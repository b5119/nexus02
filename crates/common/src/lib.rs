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
// JournaledMap — ordered map persisted as a snapshot plus an append-only journal
// ---------------------------------------------------------------------------
//
// The stores used to rewrite the WHOLE JSON file on every put/remove, which is
// O(n) per operation (measured: ~185 ms per put at 50,000 entries, taken while
// holding both this mutex and the host's global write lock).
//
// Complexity (n = entries, L = key length, k = operations in a batch,
// m = matches of a prefix scan):
//
//   get / contains           O(L log n)
//   put / remove             O(L log n) + one journal append; compaction adds
//                            O(1) amortized (see below)
//   remove_many / put_many   O(k L log n) + a SINGLE journal append
//   rekey_subtree            O(L log n + m L log n) + a single journal append
//   prefix scan              O(L log n + m)
//   compaction (rewrite)     O(n), but only after max(COMPACT_MIN_OPS, n) journal
//                            operations, so it is O(1) amortized per operation
//   open                     O(n + journal ops), then one compaction
//
// Crash safety: the snapshot is replaced atomically (temp file, fsync, rename).
// Journal replay is idempotent (last operation per key wins), so a crash between
// "snapshot renamed" and "journal truncated" replays harmlessly. A torn final
// journal line ends replay and is discarded by the compaction that `open` runs.

const COMPACT_MIN_OPS: usize = 1024;

/// Owned form, used only to read journal lines back.
#[derive(Deserialize)]
#[serde(tag = "op")]
enum JournalOp<V> {
    #[serde(rename = "put")]
    Put { k: String, v: V },
    #[serde(rename = "rm")]
    Rm { k: String },
}

/// Borrowed form, used only to write journal lines without cloning values.
#[derive(Serialize)]
#[serde(tag = "op")]
enum JournalOpRef<'a, V> {
    #[serde(rename = "put")]
    Put { k: &'a str, v: &'a V },
    #[serde(rename = "rm")]
    Rm { k: &'a str },
}

fn invalid_data(e: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

struct JournaledMap<V> {
    snapshot: PathBuf,
    journal: PathBuf,
    map: BTreeMap<String, V>,
    pending_ops: usize,
}

impl<V: Serialize + serde::de::DeserializeOwned + Clone> JournaledMap<V> {
    /// `base` is the already-parsed snapshot (callers handle legacy formats).
    fn open(snapshot: PathBuf, base: BTreeMap<String, V>) -> std::io::Result<Self> {
        if let Some(parent) = snapshot.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let journal = snapshot.with_extension("json.journal");
        let mut map = base;
        let mut journal_has_data = false;
        match std::fs::read_to_string(&journal) {
            Ok(raw) => {
                journal_has_data = !raw.trim().is_empty();
                for line in raw.lines().filter(|l| !l.trim().is_empty()) {
                    match serde_json::from_str::<JournalOp<V>>(line) {
                        Ok(JournalOp::Put { k, v }) => {
                            map.insert(k, v);
                        }
                        Ok(JournalOp::Rm { k }) => {
                            map.remove(&k);
                        }
                        // Torn or corrupt line: keep what came before it.
                        Err(_) => break,
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut this = Self {
            snapshot,
            journal,
            map,
            pending_ops: 0,
        };
        // Start every run with an empty journal so a torn tail can never be
        // followed by fresh appends.
        if journal_has_data {
            this.compact()?;
        }
        Ok(this)
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key)
    }

    fn put(&mut self, key: &str, value: V) -> std::io::Result<()> {
        let mut lines = String::new();
        push_put(&mut lines, key, &value)?;
        self.map.insert(key.to_string(), value);
        self.append(&lines, 1)
    }

    fn remove_many(&mut self, keys: &[String]) -> std::io::Result<usize> {
        let mut lines = String::new();
        let mut removed = 0;
        for k in keys {
            if self.map.remove(k).is_some() {
                push_rm(&mut lines, k)?;
                removed += 1;
            }
        }
        self.append(&lines, removed)?;
        Ok(removed)
    }

    /// Entries whose key starts with `prefix`, in key order.
    fn with_prefix(&self, prefix: &str) -> Vec<(&String, &V)> {
        self.map
            .range::<str, _>((
                std::ops::Bound::Included(prefix),
                std::ops::Bound::Unbounded,
            ))
            .take_while(|(k, _)| k.starts_with(prefix))
            .collect()
    }

    /// Move `from` and everything under `from/` to `to` and `to/...`, keeping
    /// each value unchanged. One journal append for the whole move.
    fn rekey_subtree(&mut self, from: &str, to: &str) -> std::io::Result<usize> {
        let dir_prefix = format!("{from}/");
        let mut old_keys: Vec<String> = Vec::new();
        if self.map.contains_key(from) {
            old_keys.push(from.to_string());
        }
        old_keys.extend(
            self.with_prefix(&dir_prefix)
                .into_iter()
                .map(|(k, _)| k.clone()),
        );

        let mut moved: Vec<(String, String, V)> = Vec::with_capacity(old_keys.len());
        for old in old_keys {
            if let Some(value) = self.map.remove(&old) {
                let new = if old == from {
                    to.to_string()
                } else {
                    format!("{to}/{}", &old[dir_prefix.len()..])
                };
                moved.push((old, new, value));
            }
        }
        let mut lines = String::new();
        for (old, _, _) in &moved {
            push_rm(&mut lines, old)?;
        }
        let count = moved.len();
        for (_, new, value) in moved {
            push_put(&mut lines, &new, &value)?;
            self.map.insert(new, value);
        }
        self.append(&lines, count * 2)?;
        Ok(count)
    }

    fn append(&mut self, lines: &str, ops: usize) -> std::io::Result<()> {
        use std::io::Write;
        if lines.is_empty() {
            return Ok(());
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)?;
        file.write_all(lines.as_bytes())?;
        self.pending_ops += ops;
        if self.pending_ops >= COMPACT_MIN_OPS.max(self.map.len()) {
            self.compact()?;
        }
        Ok(())
    }

    fn compact(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        let json = serde_json::to_string(&self.map).map_err(invalid_data)?;
        let tmp = self.snapshot.with_extension("json.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.snapshot)?;
        std::fs::File::create(&self.journal)?; // truncate
        self.pending_ops = 0;
        Ok(())
    }
}

fn push_put<V: Serialize>(out: &mut String, key: &str, value: &V) -> std::io::Result<()> {
    let line =
        serde_json::to_string(&JournalOpRef::Put { k: key, v: value }).map_err(invalid_data)?;
    out.push_str(&line);
    out.push('\n');
    Ok(())
}

fn push_rm(out: &mut String, key: &str) -> std::io::Result<()> {
    let op: JournalOpRef<'_, ()> = JournalOpRef::Rm { k: key };
    let line = serde_json::to_string(&op).map_err(invalid_data)?;
    out.push_str(&line);
    out.push('\n');
    Ok(())
}

fn read_snapshot<V, F>(path: &std::path::Path, upgrade: F) -> std::io::Result<BTreeMap<String, V>>
where
    F: FnOnce(&str) -> std::result::Result<BTreeMap<String, V>, serde_json::Error>,
{
    if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        upgrade(&raw).map_err(invalid_data)
    } else {
        Ok(BTreeMap::new())
    }
}

// ---------------------------------------------------------------------------
// ClockStore — per-agent clock metadata
// ---------------------------------------------------------------------------

pub struct ClockStore {
    inner: Mutex<JournaledMap<ClockEntry>>,
}

impl ClockStore {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let base = read_snapshot(&path, |raw| {
            let map: BTreeMap<String, ClockFileValue> = serde_json::from_str(raw)?;
            Ok(map
                .into_iter()
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
                .collect())
        })?;
        Ok(Self {
            inner: Mutex::new(JournaledMap::open(path, base)?),
        })
    }

    pub fn get(&self, key: &str) -> VectorClock {
        let map = self.inner.lock().unwrap();
        map.get(key).map(|e| e.clock.clone()).unwrap_or_default()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.inner.lock().unwrap().get(key).is_some()
    }

    pub fn put(&self, key: &str, clock: VectorClock) -> std::io::Result<()> {
        self.inner.lock().unwrap().put(
            key,
            ClockEntry {
                clock,
                last_updated_at: now_unix(),
            },
        )
    }

    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        self.remove_many(&[key.to_string()]).map(|_| ())
    }

    /// Remove many keys with a single journal append. Returns how many existed.
    pub fn remove_many(&self, keys: &[String]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().remove_many(keys)
    }

    /// Move `from` and every key under `from/` to `to` / `to/...`, preserving
    /// each entry's clock and timestamp, in one atomic batch.
    pub fn rekey_subtree(&self, from: &str, to: &str) -> std::io::Result<usize> {
        self.inner.lock().unwrap().rekey_subtree(from, to)
    }

    /// Entries whose key starts with `prefix`: O(log n + m), not O(n).
    pub fn entries_with_prefix(&self, prefix: &str) -> Vec<(String, ClockEntry)> {
        let map = self.inner.lock().unwrap();
        map.with_prefix(prefix)
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn entries(&self) -> Vec<(String, ClockEntry)> {
        let map = self.inner.lock().unwrap();
        map.map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// TombstoneStore — tracks deleted paths for conflict detection + GC
// ---------------------------------------------------------------------------

pub struct TombstoneStore {
    inner: Mutex<JournaledMap<TombstoneEntry>>,
}

impl TombstoneStore {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let base = read_snapshot(&path, |raw| {
            let map: BTreeMap<String, TombstoneFileValue> = serde_json::from_str(raw)?;
            Ok(map
                .into_iter()
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
                .collect())
        })?;
        Ok(Self {
            inner: Mutex::new(JournaledMap::open(path, base)?),
        })
    }

    pub fn get(&self, key: &str) -> VectorClock {
        let map = self.inner.lock().unwrap();
        map.get(key).map(|e| e.clock.clone()).unwrap_or_default()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.inner.lock().unwrap().get(key).is_some()
    }

    pub fn put(&self, key: &str, clock: VectorClock) -> std::io::Result<()> {
        let mut map = self.inner.lock().unwrap();
        // Re-deleting keeps the original created_at so the TTL runs from the
        // first delete.
        let created_at = map.get(key).map(|e| e.created_at).unwrap_or_else(now_unix);
        map.put(key, TombstoneEntry { clock, created_at })
    }

    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        self.remove_many(&[key.to_string()]).map(|_| ())
    }

    pub fn remove_many(&self, keys: &[String]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().remove_many(keys)
    }

    pub fn rekey_subtree(&self, from: &str, to: &str) -> std::io::Result<usize> {
        self.inner.lock().unwrap().rekey_subtree(from, to)
    }

    pub fn entries_with_prefix(&self, prefix: &str) -> Vec<(String, TombstoneEntry)> {
        let map = self.inner.lock().unwrap();
        map.with_prefix(prefix)
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn entries(&self) -> Vec<(String, TombstoneEntry)> {
        let map = self.inner.lock().unwrap();
        map.map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
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

    // ---- journaled store behaviour ----

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-journal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn journal_replays_puts_and_removes_after_reopen() {
        let dir = temp_dir("replay");
        let file = dir.join("clocks.json");
        {
            let store = ClockStore::open(file.clone()).unwrap();
            store.put("a", clock(&[("x", 1)])).unwrap();
            store.put("b", clock(&[("x", 2)])).unwrap();
            store.put("a", clock(&[("x", 3)])).unwrap();
            store.remove("b").unwrap();
        }
        let store = ClockStore::open(file).unwrap();
        assert_eq!(store.get("a"), clock(&[("x", 3)]));
        assert!(!store.contains("b"));
        assert_eq!(store.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_journal_tail_is_ignored_and_later_writes_survive() {
        use std::io::Write;
        let dir = temp_dir("torn");
        let file = dir.join("clocks.json");
        {
            let store = ClockStore::open(file.clone()).unwrap();
            store.put("keep", clock(&[("x", 1)])).unwrap();
        }
        // Simulate a crash mid-append: a partial line with no newline.
        let journal = file.with_extension("json.journal");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(b"{\"op\":\"put\",\"k\":\"half")
            .unwrap();
        {
            let store = ClockStore::open(file.clone()).unwrap();
            assert!(store.contains("keep"));
            assert!(!store.contains("half"));
            store.put("after", clock(&[("x", 9)])).unwrap();
        }
        let store = ClockStore::open(file).unwrap();
        assert!(store.contains("keep"), "pre-crash data survives");
        assert_eq!(
            store.get("after"),
            clock(&[("x", 9)]),
            "post-crash write survives"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_bounds_journal_and_keeps_state() {
        let dir = temp_dir("compact");
        let file = dir.join("clocks.json");
        let store = ClockStore::open(file.clone()).unwrap();
        for i in 0..(COMPACT_MIN_OPS * 3) {
            store
                .put(&format!("k{}", i % 50), clock(&[("x", i as u64)]))
                .unwrap();
        }
        let journal_len = std::fs::metadata(file.with_extension("json.journal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(
            journal_len < 200 * COMPACT_MIN_OPS as u64,
            "journal must be compacted, was {journal_len} bytes"
        );
        assert_eq!(store.len(), 50);
        drop(store);
        let store = ClockStore::open(file).unwrap();
        assert_eq!(store.len(), 50);
        let last = COMPACT_MIN_OPS * 3 - 1;
        assert_eq!(
            store.get(&format!("k{}", last % 50)),
            clock(&[("x", last as u64)])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_subtree_moves_key_and_descendants_only() {
        let dir = temp_dir("rekey");
        let file = dir.join("clocks.json");
        let store = ClockStore::open(file.clone()).unwrap();
        for k in ["d", "d/a.txt", "d/sub/b.txt", "d2/c.txt", "dx", "e/d/f.txt"] {
            store.put(k, clock(&[("x", 1)])).unwrap();
        }
        let moved = store.rekey_subtree("d", "d.conflict-w-1").unwrap();
        assert_eq!(moved, 3);
        for k in [
            "d.conflict-w-1",
            "d.conflict-w-1/a.txt",
            "d.conflict-w-1/sub/b.txt",
        ] {
            assert!(store.contains(k), "{k} should exist");
        }
        for k in ["d", "d/a.txt", "d/sub/b.txt"] {
            assert!(!store.contains(k), "{k} should be gone");
        }
        for k in ["d2/c.txt", "dx", "e/d/f.txt"] {
            assert!(store.contains(k), "{k} must be untouched");
        }
        drop(store);
        let store = ClockStore::open(file).unwrap();
        assert!(
            store.contains("d.conflict-w-1/sub/b.txt"),
            "move is durable"
        );
        assert!(!store.contains("d/a.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefix_scan_matches_only_prefix() {
        let dir = temp_dir("prefix");
        let store = ClockStore::open(dir.join("clocks.json")).unwrap();
        for k in ["a/1", "a/2", "ab/3", "b/4"] {
            store.put(k, clock(&[("x", 1)])).unwrap();
        }
        let keys: Vec<String> = store
            .entries_with_prefix("a/")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, vec!["a/1".to_string(), "a/2".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redelete_keeps_original_tombstone_timestamp() {
        let dir = temp_dir("tomb-ts");
        let store = TombstoneStore::open(dir.join("tombstones.json")).unwrap();
        store.put("f", clock(&[("a", 1)])).unwrap();
        let first = store.entries()[0].1.created_at;
        store.put("f", clock(&[("a", 2)])).unwrap();
        assert_eq!(store.entries()[0].1.created_at, first);
        assert_eq!(store.get("f"), clock(&[("a", 2)]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
