# ADR 0007: Sync vector clocks on read-intent open (closing the read-then-edit gap)

## Status
Accepted

## Context
ADR 0006 left a known gap: a client learned a file's vector clock only from a
`WriteFile` response. `Stat`/`ReadFile` carried no clock, so a device that
*read* a file (which it had never written) and then edited it had an **empty**
local clock — its edit was flagged concurrent (a conflict) even though it
causally followed the version it had just read. That over-detects conflicts;
safe (never loses data) but annoying enough to make the read-write mount
frustrating in normal use ("I opened a file someone else made, edited it, and
got a spurious conflict").

## Decision

### Clock on Stat
`StatResponse` gains a `VectorClock clock` field. The host fills it from its
clock store for the path (empty for an untracked or not-found file).

### Sync on read-intent *open*, not on every stat/lookup
The FUSE client (`nexus-fs`) syncs its per-file clock knowledge from the host in
the **`open`** callback — but only when the open requests read access
(`O_RDONLY` or `O_RDWR`, via `flags & O_ACCMODE`). It does a `stat_full`, then
merges (least upper bound) the host clock into its local `ClockStore` for that
path. A subsequent local edit increments from that synced base, so it dominates
the version it read and is **not** a conflict.

Crucially this is **not** done on `lookup`/`getattr`. If every path resolution
synced the clock, a connected client would always see the latest version and a
conflict could essentially never occur — defeating the whole point. Tying the
sync to *opening the file to read its content* gives a clean, defensible rule:

- **Read the file, then edit → no conflict.** Your edit builds on what you read.
- **Blindly overwrite without reading (e.g. `>` truncation, `cp` over a file),
  and it changed underneath you → still a conflict.** You never saw the other
  version, so concurrent edits are correctly flagged.

The write base is taken from the client's `ClockStore` at flush time, which the
read-open populated; a write-only/truncating open deliberately does not sync.

## Verified end-to-end (two mounts, real FUSE)
- Device B `cat`s a file A created, then edits it → host applies cleanly, clock
  carries both A's and B's counters, **no `.conflict-*`**.
- Device B overwrites a file A created **without** reading it first → concurrent
  → `.conflict-B-<ts>` created, original untouched (the ADR-0005/0006 behavior,
  still intact).

## Still not solved (carried forward / new edges)
- **`ls -l` / pure `stat` does not sync.** Only an actual content-read *open*
  does. Listing a directory or stat-ing a file is not "reading its version."
  Defensible, but means a tool that stats-then-writes without opening-for-read
  still blind-writes. (In practice, opening a file for writing that you want to
  preserve means reading it; pure overwrite tools are the blind-write case by
  design.)
- **Extra RPC per read-open.** Each read-intent open does one `stat_full`. Fine
  at this scale; could be folded into the read stream later.
- **Clock still not on `ReadFile`/`ListDir` bodies.** Sync rides on the open's
  stat; the streaming read path itself remains clock-free.
- Everything still open from ADR 0005/0006 (delete-vs-edit, rename, directory
  conflicts, whole-file buffering, conflicts surfacing at flush) is unchanged.

## Consequences
- The read-write mount no longer false-conflicts on the common "open, edit, save"
  flow, while genuine concurrent/offline edits remain detected.
- `Stat` now exposes per-file clocks, which a future control plane / sync
  protocol can also use.
