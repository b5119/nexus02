# ADR 0009: Rename/Move Support

## Status

Accepted

## Context

We need to support renaming and moving files (issue #5). The FUSE `rename` operation covers both same-directory renames and cross-directory moves via its `parent`/`newparent` inode parameters.

Key design questions:
1. Does a rename preserve the file's vector clock history?
2. Does fuser's `rename` trait handle both same-directory and cross-directory moves?

## Decision

### 1. Vector Clock Preservation (YES)

**A rename preserves the file's vector clock history.** The inode/identity doesn't change — only the name (and possibly parent directory) does. Resetting the clock on rename would:
- Lose conflict history, making concurrent edits after a rename appear as "new file" writes
- Break the causal chain that vector clocks are designed to track
- Allow a renamed file to be silently overwritten by a stale write from another device that didn't see the rename

The vector clock is keyed by *path* in our stores (live clocks and tombstones). On rename, we must move the clock entry from the old path to the new path in both stores, preserving the clock value exactly.

### 2. Fuser Rename Trait Signature

Confirmed: fuser's `Filesystem::rename` signature is:

```rust
fn rename(
    &mut self,
    _req: &Request<'_>,
    parent: u64,
    name: &OsStr,
    newparent: u64,
    newname: &OsStr,
    flags: u32,
    reply: ReplyEmpty,
)
```

- `parent` + `name` = old path (source)
- `newparent` + `newname` = new path (destination)
- `flags` = `RENAME_NOREPLACE` (fail if dest exists) and `RENAME_EXCHANGE` (swap) from `renameat2`

This handles both same-directory rename (`parent == newparent`) and cross-directory move (`parent != newparent`) uniformly. Our implementation must handle both.

### 3. RPC Design

Add a `RenameFile` RPC to `FileService` (mirroring `WriteFile`/`DeleteFile` pattern):

```protobuf
rpc RenameFile (RenameFileRequest) returns (RenameFileResponse);

message RenameFileRequest {
  string old_path = 1;
  string new_path = 2;
  VectorClock clock = 3;           // clock for OLD path (after incrementing own counter)
  string writer_device_id = 4;
}

message RenameFileResponse {
  enum Result {
    RENAMED = 0;       // success, clock moved to new_path
    STALE = 1;         // incoming clock dominated by stored (another device moved/deleted it first)
    CONFLICT = 2;      // concurrent rename/delete/edit — see below
    NOT_FOUND = 3;     // old_path doesn't exist
  }
  Result result = 1;
  VectorClock clock = 2;          // authoritative clock for NEW path after operation
  string conflict_path = 3;       // set for CONFLICT (edit-vs-rename: edited content at new path)
}
```

**Conflict cases:**
- **Rename-vs-edit (concurrent):** Device A renames, Device B edits old path. Host sees concurrent clocks. Outcome: keep both — the edit becomes a new file at the *new* path (since that's where the file "is" now), original at old path is gone (renamed). Actually, simpler: the edit at old path is treated as a write to a deleted path — delete-vs-edit logic applies (ADR 0008). The incoming write resurrects at old path as a conflict copy. But the rename also succeeded. We have two files now: one at new_path (renamed), one at old_path.conflict-* (the concurrent edit). This is correct — both user intents preserved.
- **Rename-vs-delete (concurrent):** Device A renames, Device B deletes old path. Same as above — delete-vs-edit logic: the delete is a tombstone, the rename "resurrects" with new name. Outcome: file at new_path, no conflict copy (delete-vs-edit CONFLICT has empty conflict_path per ADR 0008).
- **Rename-vs-rename (concurrent):** Two devices rename same file to different names. First wins (RENAMED), second gets STALE (its clock dominated by first's tombstone-like entry). Actually need to think about this... the second rename's old_path no longer exists. It would get NOT_FOUND or STALE depending on clock comparison.

### 4. Host-Side Implementation

- Serialize under the same `write_lock` as WriteFile/DeleteFile
- Look up clock for `old_path` in live clocks AND tombstones
- Compare incoming clock vs stored (same logic as WriteFile)
- On success (RENAMED):
  - `tokio::fs::rename(old, new)` on disk
  - Move clock entry: `clocks.remove(old_path)`, `clocks.put(new_path, merged_clock)`
  - Move tombstone entry if present: `tombstones.remove(old_path)`, `tombstones.put(new_path, merged_clock)`
- On STALE/NOT_FOUND: no filesystem change, return stored clock for old_path
- On CONFLICT: apply the rename (disk rename), move clock to new_path, ALSO write conflict copy at old_path.conflict-* with incoming clock (mirroring delete-vs-edit but preserving both)

### 5. Client-Side (FUSE) Implementation

In `filesystem.rs`:
- Implement `rename` method
- Resolve `parent`+`name` → old_path, `newparent`+`newname` → new_path via InodeTable
- Get current clock for old_path from `client_clocks`, increment own counter
- Call `RemoteFs::rename_file(old_path, new_path, &clock, &device_id)`
- On RENAMED: update InodeTable mapping (old_path → new_path, same inode), move write_buffer if any, move client_clocks entry
- On STALE: return EIO (or ESTALE if available) — kernel will retry lookup
- On CONFLICT: update InodeTable for new_path (rename succeeded), create conflict entry for old_path.conflict-* in InodeTable, warn user
- On NOT_FOUND: return ENOENT

**Critical:** Do NOT delete+recreate the inode. Update InodeTable's path mapping in place. The inode number must stay the same so open file handles, mmap, etc. remain valid.

**Overwrite semantics:** If `new_path` already maps to an inode in `InodeTable.rename_path`, that destination inode is removed from `path_by_ino`/`ino_by_path` before the source mapping is moved. On the host, any existing tombstone at `new_path` is cleared before the moved clock is persisted, so a rename cannot inherit a stale deletion marker.

**No directory renames (yet):** `Filesystem::rename` rejects directory renames with `EISDIR` because the inode table does not remap cached descendant entries (`/dir/file.txt` would keep the old prefix). Directory rename support requires subtree path rewriting in the inode table — deferred until it's actually needed.

**No rename flags (yet):** Non-zero flags (`RENAME_NOREPLACE`, `RENAME_EXCHANGE`) are rejected with `EINVAL` in the FUSE handler. Propagating these through the gRPC layer and `tokio::fs::rename` is future work.

### 6. Deferred Scope: Concurrent Rename-vs-Edit

**OUT OF SCOPE for this session:** The case where Device A renames a file while Device B concurrently edits the *old* path.

This is explicitly tracked as an open gap. The current design handles it via the CONFLICT path (delete-vs-edit style: both versions kept), but the semantics are subtle:
- User on Device A expects the file at `new_name`
- User on Device B expects their edit at `old_name`
- Result: file at `new_name` (renamed), conflict file at `old_name.conflict-*` (concurrent edit)

This is arguably correct (both intents preserved) but the UX is surprising. A future ADR should revisit whether to:
- Auto-apply the edit to the new path (requires tracking "rename intent" in clocks)
- Notify the user on Device B that the file was moved
- Other resolution strategies

**New GitHub issue to create:** "Concurrent rename-vs-edit: clarify/resolve semantics" (similar framing to #6 directory-level conflicts).

## Consequences

- Vector clocks correctly track file identity across renames
- Cross-directory moves work identically to same-directory renames
- Existing test suite (23 tests) must stay green
- New tests for rename scenarios added

## References

- ADR 0005: Vector Clock Conflict Detection
- ADR 0008: Delete-vs-Edit Conflicts
- Issue #5: Rename/Move Support
- Issue #6: Directory-Level Conflicts (deferred, similar framing)