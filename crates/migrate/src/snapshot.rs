use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictPolicy {
    LastWriteWins,
    AppMerge,
    KeepBoth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateEntry {
    pub value: Vec<u8>,
    pub conflict_policy: ConflictPolicy,
    pub schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSnapshot {
    pub device_id: String,
    pub vector_clock: std::collections::BTreeMap<String, u64>,
    pub keys: HashMap<String, StateEntry>,
}

#[derive(Debug, Clone)]
pub struct ConflictEntry {
    pub key: String,
    pub local_value: Vec<u8>,
    pub remote_value: Vec<u8>,
    pub schema_version: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ConflictSet {
    pub conflicts: Vec<ConflictEntry>,
}
