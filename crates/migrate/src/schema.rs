use crate::MigrateError;

/// Result of a schema version check.
#[derive(Debug)]
pub enum SchemaAction {
    /// Versions match — proceed normally.
    Current,
    /// Incoming is older: migrate via callback.
    Migrate { from_version: u32, value: Vec<u8> },
    /// Incoming is newer than this device's app version: error.
    TooNew { incoming: u32, local: u32 },
}

/// Check the schema version of an incoming value against the local version.
/// Returns the action the import logic should take.
pub fn check_schema_version(
    key: &str,
    incoming_version: u32,
    local_version: u32,
) -> Result<SchemaAction, MigrateError> {
    if incoming_version == local_version {
        return Ok(SchemaAction::Current);
    }

    if incoming_version > local_version {
        return Err(MigrateError::SchemaMismatch {
            key: key.to_string(),
            incoming: incoming_version,
            local: local_version,
        });
    }

    // incoming_version < local_version — need to migrate
    Ok(SchemaAction::Migrate {
        from_version: incoming_version,
        value: vec![], // caller fills this
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_versions_is_current() {
        let action = check_schema_version("k", 1, 1).unwrap();
        assert!(matches!(action, SchemaAction::Current));
    }

    #[test]
    fn older_incoming_requires_migration() {
        let action = check_schema_version("k", 1, 2).unwrap();
        assert!(matches!(
            action,
            SchemaAction::Migrate {
                from_version: 1,
                ..
            }
        ));
    }

    #[test]
    fn newer_incoming_returns_error() {
        let err = check_schema_version("k", 2, 1).unwrap_err();
        assert!(matches!(err, MigrateError::SchemaMismatch { .. }));
    }
}
