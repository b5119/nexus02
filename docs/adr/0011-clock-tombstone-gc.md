# ADR 0011: Clock and Tombstone Garbage Collection

## Status

Accepted

## Context

Two in-memory stores have grown unboundedly since the conflict-detection work
(ADR 0005, ADR 0008):

1. **Clock store** (`clocks.json`) — one `VectorClock` entry per file path that
   has ever been written. Paths that no longer exist (deleted, then GC'd from
   the filesystem; renamed away) still have entries. The store rewrites the
   entire map to disk on every mutation (O(n) per write), so bloat increases
   both memory usage and I/O cost on every write.

2. **Tombstone store** (`tombstones.json`) — one `VectorClock` entry per deleted
   path, kept forever. ADR 0008 explicitly named this as unsolved ("Tombstones
   accumulate forever, issue #7"). Each tombstone is removed *only* when the
   path is re-created (write after delete), but tombstones for paths that stay
   deleted are never cleaned up.

Neither store has eviction, expiry, or size bounds. On a long-running agent
serving a large or frequently-changing directory tree, both will grow
indefinitely — wasting memory, slowing the O(n) serialization on every write,
and eventually risking OOM.

## Design Questions

### 1. What triggers GC?

**Recommendation: hybrid — event-based for obvious cases, plus a periodic
background sweep for old entries.**

Event-based (already partially done):
- **Tombstone cleared on re-create** (write after delete) — already implemented
  in `write_file`, `write_file_stream`, and `rename_file` handlers. No change.
- **Clock entry removed on confirmed deletion** — already implemented in
  `delete_file` (moved to tombstone on clean delete). No change.
- **Clock entry moved on rename** (source path loses its clock; dest path gains
  it) — already implemented in `rename_file`. No change.

Periodic background sweep (new):
- A tokio task spawned in `host::run()` after store initialization.
- Runs every configurable interval (default: 6 hours).
- Sweeps both stores: removes entries that are past TTL **and** have no
  corresponding file on disk.
- After TTL sweep, enforces hard size caps — evicts oldest entries from each
  store if still over cap.

**Why not purely size-based:** A size-based approach (evict when cap hit) is
reactive — the agent approaches the cap asymptotically and only cleans up when
it's already nearly full. A periodic sweep proactively reclaims space on a
regular cadence, keeping steady-state memory usage well below the cap. The
hybrid gives both: regular cleanup under normal conditions, and a safety net
for pathological workloads or misconfiguration.

### 2. What is "old enough" for a tombstone?

A tombstone can be safely removed when no connected client could plausibly
hold a stale clock for that path. Since we have no client registry or pairing
control plane yet, we cannot know this precisely.

**TTL: 24 hours by default, configurable.**

Justification:
- 24 hours is conservative: a client that hasn't synced in 24 hours has a
  connectivity problem, not a normal operating state.
- A stale clock that arrives after the tombstone is GC'd would be treated as
  a new file write (dominating an empty clock), which is safe — the worst that
  happens is a file "resurrects" from a very old edit that nobody remembered.
  This is the same semantic as "no tombstone existed", which is a safe
  degradation.
- Configurable via CLI flag (`--tombstone-ttl-hours`) so operators can tune for
  their sync frequency.

**Explicit limitation:** TTL-based GC is safe but conservative. A control plane
with a client registry could shrink this window to the actual maximum staleness
of any connected client (or to zero by explicitly acknowledging tombstones).
This is deferred to the pairing/control-plane milestone.

### 3. Clock entries for paths that no longer exist on disk

**Evict on confirmed deletion (already done), keep TTL only for uncertainty.**

The delete handler already moves the clock to the tombstone store and removes
it from the clock store. This is the primary cleanup path.

The TTL sweep handles the edge case where a clock entry exists for a path that
has no file on disk AND no tombstone — this can happen from partial failures
(e.g., the `tombstones.put` succeeded but `clocks.remove` crashed, or vice
versa; or a file was deleted out-of-band via raw filesystem access). In that
case, the clock entry is orphaned and the TTL sweep will clean it up just like
a tombstone.

**Key difference from tombstones:** Clock entries for live files are **never**
removed by GC. The sweep only considers entries whose path has no corresponding
file on disk. A live file's clock entry is essential for conflict detection.

### 4. Size bounds as a safety net

**Hard cap: 50,000 entries per store, configurable.**

Even with TTL-based GC, add a hard cap so a misconfigured or pathological
workload cannot OOM the agent:
- Default: 50,000 entries per store.
- Configurable via CLI flag (`--max-store-entries`).
- Applied after TTL sweep on each GC cycle: if the store is still over cap
  after removing all TTL-eligible entries, evict the oldest entries until
  under cap.
- "Oldest" is determined by the `created_at` timestamp for tombstones, and
  by `last_updated_at` (new field) for clock entries.
- **Safety constraint:** The cap never evicts clock entries for live files — it
  only evicts orphaned clock entries (no file on disk). If the store is full of
  live-file entries, the cap is a signal that the operator should increase it,
  not a trigger to delete conflict-detection data.

## Implementation

### Tombstone timestamp tracking

The current `ClockStore` stores `BTreeMap<String, VectorClock>` with no
timestamp metadata. To support TTL-based eviction, each tombstone entry needs
a `created_at` timestamp.

**Approach:** Introduce a `TombstoneStore` wrapper (or a new persisted type) in
the `nexus-common` crate that stores:
```rust
struct TombstoneEntry {
    clock: VectorClock,
    created_at: u64,  // unix timestamp in seconds
}
```

Persisted to `tombstones.json`. The format change is backward-compatible:
entries without a `created_at` field (existing tombstones) are treated as
`created_at = 0` and are only evicted by the hard cap, not by TTL.

**Alternative considered — separate metadata file:** A parallel
`tombstone-meta.json` file that maps path → timestamp. Rejected because it
doubles the I/O on every tombstone write and creates an atomicity hazard
(two files to keep in sync).

### Clock entry timestamp tracking

Add an optional `last_updated_at` field to the clock store. Same approach:
a new wrapper type in `clocks.json` that adds the timestamp. Old-format entries
get `last_updated_at = 0`.

This enables the hard-cap eviction to evict the *oldest* orphaned clock entries
first, rather than arbitrary entries.

### GC task lifecycle

```rust
// Spawned in host::run() after store initialization
tokio::spawn(gc_loop(
    root: PathBuf,
    clocks: Arc<ClockStore>,
    tombstones: Arc<TombstoneStore>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    config: GcConfig,
));

// Every config.interval:
async fn gc_loop(...) {
    loop {
        tokio::time::sleep(config.interval).await;
        if let Err(e) = gc_sweep(&root, &clocks, &tombstones, &write_lock, &config).await {
            tracing::warn!("GC sweep failed: {e}");
        }
        report_sizes(&clocks, &tombstones);
    }
}
```

### Locking strategy

The GC task and the write/delete/rename handlers both touch the clock and
tombstone stores. Coordination is critical.

**Per-store locking** (`std::sync::Mutex` inside `ClockStore`):
Individual store operations (get/put/remove) are already serialized at the
store level. GC's individual removals are safe at this level — they cannot
conflict with handler removals of different keys.

**Global `write_lock`** (`tokio::sync::Mutex<()>`):
The `write_lock` protects the higher-level consistency invariant (clock +
tombstone + file on disk are in agreement). GC must coordinate with it.

**Strategy — lock per entry, not per sweep:**
1. GC collects a list of candidate entries (old TTL, no file on disk) without
   holding `write_lock`.
2. For each candidate, GC acquires `write_lock`, removes the entry, releases
   `write_lock`.
3. Between entries, GC calls `tokio::task::yield_now()` to let other tasks
   make progress.
4. If GC cannot acquire `write_lock` within 100 ms for a given entry, it skips
   that entry and tries again on the next sweep.

This is safe because:
- Each entry is independent — there is no cross-entry invariant that the sweep
  must preserve atomically.
- TTL provides a safety margin: an entry that is seconds past TTL today will
  still be past TTL tomorrow. There is no urgency.
- Skipping a contested entry (because a write/delete/rename holds the lock)
  is the correct backoff: the handler is probably touching that exact path,
  and we should not interrupt it.

**Why not hold `write_lock` for the entire sweep:** A full sweep of 50,000
entries could take seconds or minutes, blocking all write/delete/rename
operations. This violates the "GC should never block real operations" design
goal.

### Hard-cap eviction after TTL sweep

After the TTL sweep removes age-eligible entries, check sizes. If a store
still exceeds the cap:
1. For orphaned clock entries (no file on disk), sort by `last_updated_at`
   ascending and evict oldest-first until under cap.
2. For tombstones, sort by `created_at` ascending and evict oldest-first.

If the clock store is over cap but all entries are for live files, log a
warning — the operator should increase the cap.

### Configuration (CLI flags)

New flags on `nexus-agent` (alongside existing `--serve-dir` and `--port`):

```
--gc-interval-hours    GC sweep interval in hours (default: 6)
--tombstone-ttl-hours  Tombstone TTL in hours (default: 24)
--max-store-entries    Hard cap per store (default: 50000)
```

### Logging

On each GC sweep, emit an INFO-level line:
```
GC sweep: removed 14 tombstone(s), 3 orphaned clock(s);
  sizes: clocks=240 (180 live, 60 orphaned), tombstones=95
```

This makes the agent's memory footprint observable without needing a metrics
system.

## Consequences

- Tombstones and orphaned clock entries are eventually removed, bounding
  the memory footprint of both stores.
- The first GC sweep after upgrading an existing agent will see many
  `created_at = 0` entries (from before this feature) and will only evict them
  via the hard cap — operators with large existing stores should set
  `--max-store-entries` appropriately or expect the first few sweeps to be
  cap-driven.
- Write/delete/rename latency is unaffected in the steady state — GC only
  acquires `write_lock` per-entry and yields between entries.
- A pathological workload that creates 50,000+ unique files per GC interval
  will still hit the hard cap, but this is a corner case the default can be
  tuned for.
- The GC gap explicitly documented in ADR 0008 and referenced in issue #7 is
  closed.
- Conflict files (`.conflict-*`) on the filesystem are **not** cleaned up by
  this GC — they remain a manual-cleanup concern (out of scope).

## References

- ADR 0005: Vector Clock Conflict Detection
- ADR 0006: FUSE Read-Write Mount
- ADR 0008: Delete-vs-Edit Conflicts (tombstones)
- Issue #7: Clock and tombstone GC
