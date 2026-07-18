# ADR 0016: Directory-Level Conflict Detection

## Status

Accepted

## Context

ADR 0005 introduced per-file-path vector clock conflict detection. ADR 0006 extended this to FUSE read/write mounts. ADR 0008 covered delete-vs-edit, and ADR 0009 covered rename/move. In all of these, the unit of conflict detection is the **individual file path**. Directory operations (creating a directory, deleting a directory while files exist inside it) are not modeled in the clock system.

Two concrete scenarios are unhandled:

1. **Concurrent mkdir at the same path**: Device A creates `/shared/foo/` and Device B creates `/shared/foo/` independently. Both succeed locally, but on sync the second write overwrites the first directory's clock entry with no conflict signal.

2. **Write-into-deleted-directory**: Device A deletes `/shared/work/` (which creates a tombstone for that path). Device B writes `/shared/work/report.txt`. Under the current file-level clock model, the write succeeds because the clock for `/shared/work/report.txt` is independent of the tombstone for `/shared/work/` — the system has no mechanism to detect that a parent was deleted.

This ADR addresses both scenarios with minimal changes to the existing vector clock infrastructure.

### Scope

| In scope | Out of scope |
|---|---|
| Concurrent mkdir at the same path → rename to `.conflict-*` | Recursive subtree conflicts (e.g. conflicting dir trees) |
| Write-into-deleted-directory → recreate the directory | Directory rename/move conflicts |
| Single-level directory conflicts | Changes to the FUSE layer (ADR 0006) |
| ADR documenting the policy | |

## Design Decisions

### 1. Directories Are Minimally Clock-Tracked for Conflict Detection

**Decision:** Directories are tracked in the vector clock store ONLY when created via the `MkdirFile` RPC. The clock entry for a directory records which device created it, enabling concurrent-mkdir detection. Directory clock entries are lightweight (a single clock entry per created directory) and do not participate in per-file conflict resolution.

Rationale: Without a clock entry, two devices creating the same directory independently cannot be detected as a conflict. The minimal tracking is limited to the mkdir path only — directory deletion does not produce a tombstone (see §4), and writes into a directory do not consult the directory's clock entry.

### 2. Concurrent mkdir → Conflict Rename

**Decision:** Two devices creating the same directory independently via `MkdirFile` is detected as a vector-clock conflict. When the second `MkdirFile` arrives and the path already exists as a directory with a clock entry from a different device, the handler renames the existing directory to `<path>.conflict-<device_id_short>-<timestamp>` and creates the new directory.

The rename follows the same pattern as file-level conflicts (ADR 0005): the incoming operation wins, the existing entry is preserved under a conflict suffix, and both clocks are kept.

A separate case — when `write_file` or `write_file_stream` targets a path whose parent is a file (not a directory) — is not yet implemented. The current `resolve_for_write` would fail with a "parent not found" error in that case. This is tracked for a future milestone.

### 3. Write-into-Deleted-Directory → Recreate

**Decision:** When a write targets a path whose parent directory has been tombstoned, the handler recreates the directory before writing the file. The file write proceeds normally; the parent directory's tombstone is not removed (it remains as a record of the deletion).

This follows the same "edit wins" semantics as delete-vs-edit on files (ADR 0008): if one device deletes a directory and another writes into it, the write succeeds and the directory is implicitly resurrected.

### 4. Deletion of Non-Empty Directory

**Decision:** Deleting a directory that still contains files succeeds — the files become orphaned. The GC system (ADR 0011) will clean up orphaned clock entries on the next sweep.

This is consistent with the existing file-level semantics where deleting a file only tombstones the clock entry, not the data itself.

### 5. Known Gaps

- **Recursive subtree conflicts**: If two devices concurrently create deep directory trees under the same parent (e.g. `/a/b/c/d` vs `/a/b/c/e`), the first-level parent `/a/b/c/` may enter a conflicting state. This is explicitly deferred.
- **Directory rename**: Renaming a directory that contains files is not modeled. This is a future milestone.

## References

- ADR 0005: Vector Clock Conflict Detection
- ADR 0006: FUSE Read/Write Mount
- ADR 0008: Delete-vs-Edit Conflicts
- ADR 0009: Rename/Move Support
- ADR 0011: Clock/Tombstone GC
