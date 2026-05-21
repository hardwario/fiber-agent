//! Versioned migrations for `fiber.config.yaml`.
//!
//! The yaml on `/data/fiber/config/fiber.config.yaml` persists across firmware
//! upgrades. New firmware will routinely encounter older yaml shapes. This
//! module provides forward migrations on the raw `serde_yaml::Value` AST so
//! we can rename, move, add, or drop keys without forcing the user to edit
//! their on-device file by hand.
//!
//! **Usage:** `Config::from_file` calls `migrate_and_persist` before any
//! typed parsing. If a migration runs, the file is rewritten (with a
//! `.vN.bak` backup) so the next boot reads the new shape natively.
//!
//! **Versioning rule:** when changing the yaml shape (adding a field that
//! should appear in users' files, renaming, moving, dropping, or retyping
//! a key), bump [`CURRENT_CONFIG_VERSION`] and add a `migrate_vN_to_vN+1`
//! that produces the next shape. See `feedback_config_yaml_versioning.md`
//! in user memory.
//!
//! **Adding optional fields with `#[serde(default)]`** does NOT require a
//! version bump on its own — serde fills missing fields at parse time.
//! Bump only when the field SHOULD appear explicitly in users' files so
//! they can tune it without docs.

use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

/// Current target version for `fiber.config.yaml`. Increment when adding a
/// new `migrate_vN_to_vN+1` function below.
pub const CURRENT_CONFIG_VERSION: u32 = 1;

/// Read `path`, migrate forward in place if needed, write back with a
/// versioned backup of the original. Returns the migrated YAML as a string
/// so the caller can parse it into the typed `Config` without re-reading.
pub fn migrate_and_persist(path: &Path) -> Result<String, MigrationError> {
    let content = fs::read_to_string(path)
        .map_err(|e| MigrationError::Io(format!("read {:?}: {}", path, e)))?;

    let raw: Value = serde_yaml::from_str(&content)
        .map_err(|e| MigrationError::Parse(format!("parse {:?}: {}", path, e)))?;

    let from_version = read_version(&raw);
    if from_version == CURRENT_CONFIG_VERSION {
        return Ok(content);
    }
    if from_version > CURRENT_CONFIG_VERSION {
        return Err(MigrationError::NewerThanFirmware {
            file: from_version,
            firmware: CURRENT_CONFIG_VERSION,
        });
    }

    let migrated = migrate_chain(raw, from_version, CURRENT_CONFIG_VERSION)?;

    let backup = backup_path(path, from_version);
    fs::copy(path, &backup)
        .map_err(|e| MigrationError::Io(format!("backup to {:?}: {}", backup, e)))?;

    let new_content = serde_yaml::to_string(&migrated)
        .map_err(|e| MigrationError::Parse(format!("serialize: {}", e)))?;
    fs::write(path, &new_content)
        .map_err(|e| MigrationError::Io(format!("write {:?}: {}", path, e)))?;

    eprintln!(
        "[config] migrated yaml v{} → v{} (backup: {:?})",
        from_version, CURRENT_CONFIG_VERSION, backup
    );

    Ok(new_content)
}

fn read_version(raw: &Value) -> u32 {
    raw.get("config_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32
}

fn backup_path(path: &Path, from_version: u32) -> PathBuf {
    let stem = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{}.v{}.bak", stem, from_version))
}

fn migrate_chain(mut value: Value, from: u32, to: u32) -> Result<Value, MigrationError> {
    for step in from..to {
        value = match step {
            0 => migrate_v0_to_v1(value)?,
            // Future:
            //   1 => migrate_v1_to_v2(value)?,
            //   2 => migrate_v2_to_v3(value)?,
            other => return Err(MigrationError::UnknownStep(other)),
        };
    }
    Ok(value)
}

/// v0 → v1: insert `mqtt.export` defaults (save-and-feed pipeline).
///
/// v0 had no `config_version` field and no `mqtt.export` block. v1 adds
/// both. The export defaults are deliberately conservative:
/// `enabled: false` and one disabled `local` destination — operators flip
/// `enabled: true` per destination when they're ready.
fn migrate_v0_to_v1(mut value: Value) -> Result<Value, MigrationError> {
    let root = value.as_mapping_mut()
        .ok_or_else(|| MigrationError::Parse("root is not a mapping".into()))?;

    // 1. Stamp config_version at the top.
    root.insert(Value::String("config_version".into()), Value::Number(1u32.into()));

    // 2. Inject mqtt.export defaults if absent.
    let mqtt_key = Value::String("mqtt".into());
    if let Some(mqtt) = root.get_mut(&mqtt_key).and_then(|v| v.as_mapping_mut()) {
        let export_key = Value::String("export".into());
        if !mqtt.contains_key(&export_key) {
            mqtt.insert(export_key, default_export_block());
        }
    }

    Ok(value)
}

/// Defaults injected when a v0 yaml lacks `mqtt.export`: feature ON with
/// one always-on local destination. Save-and-feed is a core invariant of
/// the runtime (firmware DB ↔ viewer DB), not an optional feature, so we
/// default-enable it on migration. Operators on `localhost:1883` get the
/// pipeline running without touching the yaml; non-localhost destinations
/// still require manual config (host/credentials/TLS).
fn default_export_block() -> Value {
    let mut tls = serde_yaml::Mapping::new();
    tls.insert(Value::String("enabled".into()), Value::Bool(false));

    let mut local = serde_yaml::Mapping::new();
    local.insert(Value::String("broker_id".into()), Value::String("local".into()));
    local.insert(Value::String("enabled".into()), Value::Bool(true));
    local.insert(Value::String("host".into()), Value::String("localhost".into()));
    local.insert(Value::String("port".into()), Value::Number(1883.into()));
    local.insert(Value::String("username".into()), Value::String("".into()));
    local.insert(Value::String("password".into()), Value::String("".into()));
    local.insert(Value::String("tls".into()), Value::Mapping(tls));

    let mut export = serde_yaml::Mapping::new();
    export.insert(Value::String("enabled".into()), Value::Bool(true));
    export.insert(
        Value::String("streams".into()),
        Value::Sequence(vec![
            Value::String("sticker".into()),
            Value::String("probe".into()),
            Value::String("alarm".into()),
        ]),
    );
    export.insert(Value::String("batch_size".into()), Value::Number(200.into()));
    export.insert(Value::String("drain_interval_ms".into()), Value::Number(500.into()));
    export.insert(Value::String("publish_qos".into()), Value::Number(1.into()));
    export.insert(
        Value::String("destinations".into()),
        Value::Sequence(vec![Value::Mapping(local)]),
    );
    Value::Mapping(export)
}

#[derive(Debug)]
pub enum MigrationError {
    Io(String),
    Parse(String),
    NewerThanFirmware { file: u32, firmware: u32 },
    UnknownStep(u32),
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(s) => write!(f, "config migration I/O: {}", s),
            Self::Parse(s) => write!(f, "config migration parse: {}", s),
            Self::NewerThanFirmware { file, firmware } => write!(
                f,
                "yaml config_version {} is newer than firmware {}; refusing to downgrade",
                file, firmware,
            ),
            Self::UnknownStep(v) => write!(f, "no migration step defined for v{}", v),
        }
    }
}

impl std::error::Error for MigrationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v0_yaml_gets_config_version_and_export_block_injected() {
        let v0 = r#"
mqtt:
  enabled: true
  broker:
    host: localhost
ble:
  enabled: false
"#;
        let raw: Value = serde_yaml::from_str(v0).unwrap();
        let migrated = migrate_v0_to_v1(raw).unwrap();

        let root = migrated.as_mapping().unwrap();
        assert_eq!(
            root.get(&Value::String("config_version".into())).and_then(|v| v.as_u64()),
            Some(1),
        );

        let mqtt = root.get(&Value::String("mqtt".into())).unwrap().as_mapping().unwrap();
        let export = mqtt.get(&Value::String("export".into())).unwrap().as_mapping().unwrap();
        // Save-and-feed defaults to ON so the firmware↔viewer mirror is live
        // from first boot without operator intervention.
        assert_eq!(export.get(&Value::String("enabled".into())).unwrap().as_bool(), Some(true));
        assert_eq!(export.get(&Value::String("batch_size".into())).unwrap().as_u64(), Some(200));
        // And the always-on `local` destination must also default-enabled.
        let dests = export.get(&Value::String("destinations".into())).unwrap().as_sequence().unwrap();
        let local = dests[0].as_mapping().unwrap();
        assert_eq!(local.get(&Value::String("broker_id".into())).unwrap().as_str(), Some("local"));
        assert_eq!(local.get(&Value::String("enabled".into())).unwrap().as_bool(), Some(true));
    }

    #[test]
    fn v0_yaml_with_existing_export_block_keeps_user_values() {
        let v0 = r#"
config_version: 0
mqtt:
  export:
    enabled: true
    batch_size: 50
ble:
  enabled: false
"#;
        let raw: Value = serde_yaml::from_str(v0).unwrap();
        let migrated = migrate_v0_to_v1(raw).unwrap();

        let mqtt = migrated.as_mapping().unwrap()
            .get(&Value::String("mqtt".into())).unwrap()
            .as_mapping().unwrap();
        let export = mqtt.get(&Value::String("export".into())).unwrap().as_mapping().unwrap();
        // User had enabled=true with custom batch_size; migration must not stomp it.
        assert_eq!(export.get(&Value::String("enabled".into())).unwrap().as_bool(), Some(true));
        assert_eq!(export.get(&Value::String("batch_size".into())).unwrap().as_u64(), Some(50));
    }

    #[test]
    fn migrate_and_persist_writes_backup_and_updates_version() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        fs::write(
            &path,
            "mqtt:\n  enabled: true\nble:\n  enabled: false\n",
        )
        .unwrap();

        let migrated_str = migrate_and_persist(&path).unwrap();
        assert!(migrated_str.contains("config_version: 1"));
        assert!(migrated_str.contains("export"));

        // Backup exists with the prior version stamp.
        let backup = backup_path(&path, 0);
        assert!(backup.exists(), "expected backup at {:?}", backup);
        let _ = fs::remove_file(&backup);

        // Re-running is a no-op (no further backup, content stable).
        let re_run = migrate_and_persist(&path).unwrap();
        assert_eq!(re_run, migrated_str);
    }

    #[test]
    fn newer_yaml_than_firmware_is_rejected() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        fs::write(&path, "config_version: 999\nmqtt:\n  enabled: false\n").unwrap();

        let err = migrate_and_persist(&path).unwrap_err();
        match err {
            MigrationError::NewerThanFirmware { file, firmware } => {
                assert_eq!(file, 999);
                assert_eq!(firmware, CURRENT_CONFIG_VERSION);
            }
            other => panic!("expected NewerThanFirmware, got {:?}", other),
        }
    }
}
