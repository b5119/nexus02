use crate::snapshot::{AppSnapshot, ConflictEntry, ConflictPolicy, ConflictSet, StateEntry};
use crate::MigrateError;
use std::collections::HashMap;

/// Resolve conflicts between the local and remote snapshots.
/// `remote_policies` maps key -> policy from the remote snapshot.
/// Returns the resolved `keys` map and any unresolved conflicts.
pub fn resolve_conflicts(
    local: &AppSnapshot,
    remote: &AppSnapshot,
) -> Result<(HashMap<String, StateEntry>, ConflictSet), MigrateError> {
    // Compare snapshot-level clocks.
    let local_clock = nexus_common::VectorClock(local.vector_clock.clone());
    let remote_clock = nexus_common::VectorClock(remote.vector_clock.clone());

    // If one completely dominates, take it wholesale.
    match local_clock.compare(&remote_clock) {
        nexus_common::ClockOrder::Dominates | nexus_common::ClockOrder::Equal => {
            // Local is newer or equal — keep local, remote keys win for new entries.
            let mut resolved = local.keys.clone();
            for (key, entry) in &remote.keys {
                if !resolved.contains_key(key) {
                    resolved.insert(key.clone(), entry.clone());
                }
            }
            return Ok((resolved, ConflictSet::default()));
        }
        nexus_common::ClockOrder::DominatedBy => {
            // Remote is strictly newer — use remote entirely.
            return Ok((remote.keys.clone(), ConflictSet::default()));
        }
        nexus_common::ClockOrder::Concurrent => { /* fall through to per-key */ }
    }

    let mut resolved = local.keys.clone();
    let mut conflicts = Vec::new();

    for (key, remote_entry) in &remote.keys {
        let local_entry = resolved.get(key);

        match local_entry {
            None => {
                // Key exists only in remote — add it.
                resolved.insert(key.clone(), remote_entry.clone());
            }
            Some(local_entry) => {
                // Key in both — resolve per policy.
                match remote_entry.conflict_policy {
                    ConflictPolicy::LastWriteWins => {
                        // Remote wins in concurrent case.
                        resolved.insert(key.clone(), remote_entry.clone());
                    }
                    ConflictPolicy::AppMerge => {
                        // Caller will invoke the app's merge() with these.
                        // For now, mark as conflict. The SDK's import() handles this.
                        // We store the remote value as the tentative value.
                        // Actually, we need the app to decide. Return both.
                        conflicts.push(ConflictEntry {
                            key: key.clone(),
                            local_value: local_entry.value.clone(),
                            remote_value: remote_entry.value.clone(),
                            schema_version: remote_entry.schema_version,
                        });
                        // Keep local as tentative — caller will update if merge returns.
                    }
                    ConflictPolicy::KeepBoth => {
                        conflicts.push(ConflictEntry {
                            key: key.clone(),
                            local_value: local_entry.value.clone(),
                            remote_value: remote_entry.value.clone(),
                            schema_version: remote_entry.schema_version,
                        });
                        // Keep both — local stays.
                    }
                }
            }
        }
    }

    Ok((resolved, ConflictSet { conflicts }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::StateEntry;

    fn snapshot(
        device_id: &str,
        clock: &[(&str, u64)],
        keys: Vec<(&str, &[u8], ConflictPolicy, u32)>,
    ) -> AppSnapshot {
        AppSnapshot {
            device_id: device_id.to_string(),
            vector_clock: clock.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            keys: keys
                .into_iter()
                .map(|(k, v, p, s)| {
                    (
                        k.to_string(),
                        StateEntry {
                            value: v.to_vec(),
                            conflict_policy: p,
                            schema_version: s,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn remote_dominates_uses_remote_values() {
        let local = snapshot("a", &[("a", 1)], vec![]);
        let remote = snapshot(
            "b",
            &[("a", 2)],
            vec![("k", b"remote", ConflictPolicy::LastWriteWins, 1)],
        );
        let (resolved, _) = resolve_conflicts(&local, &remote).unwrap();
        assert_eq!(resolved.get("k").unwrap().value, b"remote");
    }

    #[test]
    fn local_dominates_keeps_local_values() {
        let local = snapshot(
            "a",
            &[("a", 2)],
            vec![("k", b"local", ConflictPolicy::LastWriteWins, 1)],
        );
        let remote = snapshot(
            "b",
            &[("a", 1)],
            vec![("k", b"remote", ConflictPolicy::LastWriteWins, 1)],
        );
        let (resolved, _) = resolve_conflicts(&local, &remote).unwrap();
        assert_eq!(resolved.get("k").unwrap().value, b"local");
    }

    #[test]
    fn concurrent_last_write_wins_uses_remote() {
        let local = snapshot(
            "a",
            &[("a", 1)],
            vec![("k", b"local", ConflictPolicy::LastWriteWins, 1)],
        );
        let remote = snapshot(
            "b",
            &[("b", 1)],
            vec![("k", b"remote", ConflictPolicy::LastWriteWins, 1)],
        );
        let (resolved, _) = resolve_conflicts(&local, &remote).unwrap();
        assert_eq!(resolved.get("k").unwrap().value, b"remote");
    }

    #[test]
    fn concurrent_keep_both_produces_conflict() {
        let local = snapshot(
            "a",
            &[("a", 1)],
            vec![("k", b"local", ConflictPolicy::KeepBoth, 1)],
        );
        let remote = snapshot(
            "b",
            &[("b", 1)],
            vec![("k", b"remote", ConflictPolicy::KeepBoth, 1)],
        );
        let (resolved, conflicts) = resolve_conflicts(&local, &remote).unwrap();
        assert_eq!(resolved.get("k").unwrap().value, b"local");
        assert_eq!(conflicts.conflicts.len(), 1);
        assert_eq!(conflicts.conflicts[0].key, "k");
    }

    #[test]
    fn new_key_from_remote_is_added() {
        let local = snapshot("a", &[("a", 1)], vec![]);
        let remote = snapshot(
            "b",
            &[("a", 2)],
            vec![("k", b"val", ConflictPolicy::LastWriteWins, 1)],
        );
        let (resolved, _) = resolve_conflicts(&local, &remote).unwrap();
        assert_eq!(resolved.get("k").unwrap().value, b"val");
    }
}
