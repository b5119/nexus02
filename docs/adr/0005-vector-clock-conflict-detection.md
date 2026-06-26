# ADR 0005: Multi-writer conflict detection via vector clocks

## Status
Accepted (conflict-detection core). The FUSE read-write path that exposes this
through a mount is a separate follow-up (see "Not yet solved").

## Context
Milestone 1 was read-only. To let more than one device edit the same files we
need to know, when two versions meet, whether one simply supersedes the other
or whether they were edited **independently** (a real conflict). A plain
"last write wins" by wall-clock time silently destroys data when two devices
edit offline and later sync; we want to *detect* that case, not paper over it.

Scope here is deliberately **"detect conflicts correctly," not "merge content
intelligently."**

## Decision

### Vector clocks
- Each file has a vector clock: `device_id -> logical counter`
  (`nexus_common::VectorClock`, a `BTreeMap<String, u64>`). It is metadata,
  stored alongside the file — never inside file content.
- A device increments **its own** counter on each local write.
- Comparison (`VectorClock::compare`) uses the standard version-vector partial
  order over the union of device ids (a missing counter reads as 0):
  - every counter `>=` and at least one `>` → **Dominates** (newer, unambiguous);
  - the mirror image → **DominatedBy** (stale);
  - all equal → **Equal**;
  - each side ahead on some device → **Concurrent** → this is the conflict.

### Storage
- Per-agent sidecar store (`nexus_common::ClockStore`): a single JSON map
  (relative path → clock) persisted atomically (temp file + rename), guarded by
  a mutex, living in the agent's config dir — **not** inside the served tree.
- **Why JSON, not sled/redb:** at milestone metadata scale an atomic JSON map
  is the lightest thing to integrate, pulls no heavy or cross-compile-fragile
  dependency, and keeps the Android build (ADR 0002) clean. The whole map is
  rewritten per `put`; fine at this scale, revisit if the clock set grows large.

### Write path + conflict policy (`WriteFile` RPC)
- The proto gains a unary `WriteFile(path, data, clock, writer_device_id)`
  alongside `ReadFile`. Whole-file for now; large-file streaming is a future
  refinement.
- The host compares the incoming clock against its stored clock for the path
  (under a per-write lock so concurrent writes to the same path can't race the
  decision) and:
  - **Dominates / Equal** → write through; stored clock becomes the merge
    (least upper bound). `APPLIED`.
  - **DominatedBy** → incoming is stale; the host keeps its newer version and
    does not clobber it. `STALE`.
  - **Concurrent** → **keep both**: the existing file is left **untouched**, the
    incoming version is written to a sibling
    `<name>.conflict-<writer-device-id>-<unix-ts>`, and a warning is logged.
    `CONFLICT`, with the conflict path returned.

### Why "keep both + rename" instead of auto-merge
Automatic content merging requires understanding file *semantics* — a 3-way
text merge means knowing the file is line-oriented text with a common ancestor;
merging two edited photos, SQLite DBs, or `.docx` files is either nonsense or
needs format-specific logic. Nexus is a generic file transport: it sees opaque
bytes and has neither the common ancestor content nor any notion of structure.
Guessing a merge would silently corrupt data — exactly the failure we're trying
to detect. Keeping both versions is lossless and puts the resolution in the
hands of whoever understands the file, which is the only correct default for a
content-agnostic system. (A future per-type merge plugin could opt specific
formats into smarter handling, but that's additive, not the baseline.)

## Not yet solved (named gaps, not pretended-handled)
- **No FUSE read-write yet.** This session implements and tests the conflict
  *core* (clock type, store, `WriteFile` handler, client `write_file`). Flipping
  the mount off read-only and adding `create`/`write`/`flush`/truncate with a
  write-back buffer is the next session; only then are writes drivable through
  an actual mount.
- **Directory-level conflicts** are not modeled — clocks are per file path. Two
  devices concurrently creating different entries in the same directory, or
  concurrent renames, are not detected as directory conflicts.
- **Delete-vs-edit** is not handled. There is no tombstone/clock for deletions,
  so "device A deletes while device B edits" is not detected as a conflict.
- **Rename/move** is treated as unrelated paths (no identity tracking across a
  rename), so an edit-vs-rename race is invisible.
- **Clock GC / compaction** — clocks accumulate device ids forever; no pruning
  of departed devices.
- **Conflict-of-conflicts** — editing a `.conflict-*` file is not specially
  handled; it's just another path.
- **Coarse write serialization** — one writer at a time across all paths (a
  single mutex), not per-path; correct but not concurrent-write-scalable.
- **No automatic content merge** — by design, see above.

## Consequences
- The host now keeps per-file vector clocks and can distinguish "newer" from
  "conflicting" writes, preserving both sides on conflict instead of losing one.
- `WriteFile` exists on the wire and in the client (`RemoteFs::write_file`),
  ready to be wired into the FUSE write path next.
- This is conflict *detection*, not resolution-by-merge; resolving a
  `.conflict-*` file is a human (or future per-format) decision.
