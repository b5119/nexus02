use crate::snapshot::{AppSnapshot, ConflictEntry, ConflictPolicy, ConflictSet, StateEntry};
use crate::MigrateError;
use nexus_proto::migrate::v1::{
    migrate_service_client::MigrateServiceClient, AppSnapshot as ProtoAppSnapshot,
    ConflictPolicy as ProtoConflictPolicy, PullSnapshotRequest, PushSnapshotRequest,
    StateEntry as ProtoStateEntry,
};
use tonic::transport::Channel;

pub struct MigrateClient {
    client: MigrateServiceClient<Channel>,
    _auth_token: String,
}

impl MigrateClient {
    pub async fn connect(addr: String, _auth_token: String) -> Result<Self, MigrateError> {
        let endpoint = tonic::transport::Endpoint::new(addr)
            .map_err(|e| MigrateError::Transport(format!("invalid address: {e}")))?;
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| MigrateError::Transport(format!("connect failed: {e}")))?;
        Ok(Self {
            client: MigrateServiceClient::new(channel),
            _auth_token,
        })
    }

    pub async fn push_snapshot(
        &mut self,
        snapshot: AppSnapshot,
    ) -> Result<ConflictSet, MigrateError> {
        let proto_snapshot = snapshot_to_proto(&snapshot);
        let request = tonic::Request::new(PushSnapshotRequest {
            snapshot: Some(proto_snapshot),
        });
        let response = self
            .client
            .push_snapshot(request)
            .await
            .map_err(|e| MigrateError::Transport(format!("push failed: {e}")))?;
        let resp = response.into_inner();
        Ok(ConflictSet {
            conflicts: resp
                .conflicts
                .into_iter()
                .map(|ce| ConflictEntry {
                    key: ce.key,
                    local_value: ce.local_value,
                    remote_value: ce.remote_value,
                    schema_version: 0,
                })
                .collect(),
            ..Default::default()
        })
    }

    pub async fn pull_snapshot(
        &mut self,
        clock: &std::collections::BTreeMap<String, u64>,
    ) -> Result<AppSnapshot, MigrateError> {
        let request = tonic::Request::new(PullSnapshotRequest {
            device_clock: Some(nexus_proto::migrate::v1::VectorClock {
                counters: clock.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            }),
        });
        let response = self
            .client
            .pull_snapshot(request)
            .await
            .map_err(|e| MigrateError::Transport(format!("pull failed: {e}")))?;
        let resp = response.into_inner();
        proto_to_snapshot(resp.snapshot)
    }
}

fn snapshot_to_proto(snapshot: &AppSnapshot) -> ProtoAppSnapshot {
    ProtoAppSnapshot {
        device_id: snapshot.device_id.clone(),
        clock: Some(nexus_proto::migrate::v1::VectorClock {
            counters: snapshot
                .vector_clock
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        }),
        keys: snapshot
            .keys
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ProtoStateEntry {
                        value: v.value.clone(),
                        policy: match v.conflict_policy {
                            ConflictPolicy::LastWriteWins => ProtoConflictPolicy::LastWriteWins,
                            ConflictPolicy::AppMerge => ProtoConflictPolicy::AppMerge,
                            ConflictPolicy::KeepBoth => ProtoConflictPolicy::KeepBoth,
                        } as i32,
                        schema_version: v.schema_version,
                    },
                )
            })
            .collect(),
    }
}

fn proto_to_snapshot(proto: Option<ProtoAppSnapshot>) -> Result<AppSnapshot, MigrateError> {
    let proto = proto.ok_or_else(|| MigrateError::Transport("empty snapshot".into()))?;
    let mut keys = std::collections::HashMap::new();
    for (k, v) in proto.keys {
        let policy = match v.policy {
            0 => ConflictPolicy::LastWriteWins,
            1 => ConflictPolicy::AppMerge,
            2 => ConflictPolicy::KeepBoth,
            other => {
                return Err(MigrateError::Transport(format!(
                    "unknown conflict policy: {other}"
                )));
            }
        };
        keys.insert(
            k,
            StateEntry {
                value: v.value,
                conflict_policy: policy,
                schema_version: v.schema_version,
            },
        );
    }
    Ok(AppSnapshot {
        device_id: proto.device_id,
        vector_clock: proto
            .clock
            .map(|c| c.counters.into_iter().collect())
            .unwrap_or_default(),
        keys,
    })
}
