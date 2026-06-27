# ADR 0008: Delete-vs-edit conflict detection (tombstones)

## Status
Accepted

## Context
ADR 0005/0006 named **delete-vs-edit** as unsolved: there was no delete operation
at all through the mount, and no record of a deletion — so if one device deleted
a file while another edited it, the edit could silently resurrect the file (or a
delete could silently wipe an unseen edit). This ADR adds deletes and detects the
delete-vs-edit conflict, keeping with the project's "detect, never silently lose
data" stance.

## Decision

### Tombstones as a second clock store
A deleted path is recorded as a **tombstone** — the deleting device's vector
clock at delete time — so a later concurrent edit can be detected rather than
silently resurrecting the file. Implemented as a **second `ClockStore`**
(`tombstones.json`) alongside the live `clocks.json`, reusing the existing type:

- A path lives in exactly one of `clocks` (live) or `tombstones` (deleted).
- **No change to `ClockStore`'s serialization** — so the client store and the
  existing live store are untouched; no migration.

### `DeleteFile` RPC + tombstone-aware `WriteFile`
A new unary `DeleteFile(path, clock, writer_device_id)` is added. The deleter,
like a writer, increments **its own** counter first (so the incoming clock always
carries the deleter's mark — important for the matrix below). Both handlers run
under the same per-path serialization lock.

`DeleteFile` (incoming delete clock vs the live file's clock):
- **Dominates / Equal** (deleter saw the latest, or the file was untracked) →
  delete the file, move its clock to a tombstone. `DELETED`.
- **DominatedBy** (the file has edits the deleter never saw) → stale delete,
  **keep the file**. `STALE`.
- **Concurrent** → delete-vs-edit conflict → **keep the file** (the edit wins).
  `CONFLICT`.

`WriteFile`, when the path is **tombstoned** (incoming edit clock vs tombstone):
- **Dominates / Equal** (writer saw the delete and writes anyway) → intentional
  resurrect: write the file, clear the tombstone. `APPLIED`.
- **DominatedBy** (the edit predates the delete) → stays deleted. `STALE`.
- **Concurrent** → delete-vs-edit conflict → **resurrect with the edited
  content** (the edit wins), clear the tombstone. `CONFLICT`.

### Why "keep the file / edit wins" on a delete-vs-edit conflict
This is the data-preserving reading of ADR 0005's "keep both". A delete has no
content to stash in a `.conflict-*` sibling (unlike edit-vs-edit), so "keep both"
degenerates to a choice — and the safe choice is to **preserve the edit**:
losing an edit is unrecoverable, whereas a delete is trivially repeatable once
the operator sees the (loudly logged) conflict. So both directions converge on
"the file survives, the conflict is logged."

### Through the mount
FUSE `unlink` stamps the client's clock and calls `DeleteFile`. The local `rm`
always *appears* to succeed; on `CONFLICT`/`STALE` the host keeps the file and it
**reappears on the next lookup** — which is the signal that the delete conflicted
(plus a client-side WARN). This mirrors how write conflicts surface at flush
rather than at the syscall (ADR 0006), an inherent property of the async,
last-write-reconciled model.

## Verified
- Unit (host handler) matrix: clean delete records a tombstone; stale delete is
  ignored; concurrent delete-vs-edit keeps the file; a concurrent edit resurrects
  a tombstoned file; an edit dominating a tombstone resurrects cleanly; a stale
  edit to a tombstone stays deleted.
- End-to-end through the mount: `rm` removes a file on the host; a delete racing a
  concurrent edit leaves the file in place with the edit, and logs the conflict.

## Not yet solved (named)
- **Tombstone GC.** Tombstones accumulate forever (same family as the clock-GC
  gap, issue #7). A real system prunes them once all peers have observed the
  delete; we don't track peers yet.
- **Directory deletes** (`rmdir`) and **directory-level conflicts** are still out
  of scope (issue #6). Only file `unlink`.
- **Rename** is still unsupported (issue #5), so delete-vs-rename races aren't
  modeled.
- **Conflict surfaces late** (at the next lookup / in logs), not at the `rm`
  syscall — by design, see above.
- The coarse one-writer-at-a-time host lock (ADR 0005) now also serializes
  deletes.

## Consequences
- The mount is delete-capable, and delete-vs-edit races are detected and resolved
  data-safely instead of silently losing a side.
- The host keeps a growing tombstone set until GC exists.
