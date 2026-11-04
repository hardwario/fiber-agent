//! Switched-off sensors: `sensor_disabled` (V33, GitLab #120).
//!
//! A row switches one sensor of a Node (`scope = 'node'`, entity = node_key)
//! or a Beacon (`scope = 'eye'`, entity = MAC) off. Its values are not stored:
//! [`Storage::insert_field_readings`] drops a switched-off Node field and the
//! Beacon inserts write `NULL` into its column. Switching off deletes what was
//! stored of the sensor in the same transaction ([`Storage::disable_sensor`]),
//! so a sensor is either on with its history or off with none.

use std::collections::HashSet;

use rusqlite::params;

use crate::error::Result;
use crate::models::{THRESHOLD_SCOPE_EYE, THRESHOLD_SCOPE_NODE};
use crate::Storage;

/// One switched-off sensor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisabledSensor {
    pub hostname: String,
    pub scope: String,
    pub entity: String,
    pub field: String,
}

/// What [`Storage::disable_sensor`] deleted, for the caller's audit entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SensorPurge {
    /// `node_field_readings` rows deleted, or `eye_readings` rows whose
    /// column was set to `NULL`.
    pub readings: usize,
    pub alarms: usize,
    pub threshold_history: usize,
    pub group_members: usize,
}

impl Storage {
    /// Every switched-off sensor, on every Hub. Loaded once at start into the
    /// state store, which keeps it from then on.
    pub fn list_disabled_sensors(&self) -> Result<Vec<DisabledSensor>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, scope, entity, field FROM sensor_disabled
             ORDER BY hostname, scope, entity, field",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DisabledSensor {
                hostname: r.get(0)?,
                scope: r.get(1)?,
                entity: r.get(2)?,
                field: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The switched-off fields of one Node or Beacon.
    pub fn disabled_sensor_fields(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
    ) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT field FROM sensor_disabled
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3",
        )?;
        let rows = stmt.query_map(params![hostname, scope, entity], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<HashSet<String>>>()?)
    }

    /// Switches one sensor off and deletes what was stored of it, in one
    /// transaction (devices.node.md, amendment 2026-10-13):
    ///
    /// - its readings: the Node's `node_field_readings` rows, or the Beacon's
    ///   column (and its min/max) in `eye_readings`;
    /// - its alarms, open ones included: `alarm_events` rows of `sensor_id`
    ///   (`lrw:{node}:{label}` / `eye:{mac}:{label}`, the caller builds it), and
    ///   their notes by cascade;
    /// - the history of its bounds (`field_threshold_history`);
    /// - its name in a group member's explicit field selection.
    ///
    /// The bound itself (`field_thresholds`) stays: on a Node it may be held by
    /// the Node and the Hub too, and the caller drafts its removal so that one
    /// Send takes it off all three. Nothing grades a switched-off sensor
    /// meanwhile. Returns `None` when the sensor was off already.
    pub fn disable_sensor(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: &str,
        alarm_sensor_id: &str,
        actor: &str,
    ) -> Result<Option<SensorPurge>> {
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO sensor_disabled (hostname, scope, entity, field, actor)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![hostname, scope, entity, field, actor],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        let mut purge = SensorPurge::default();
        if scope == THRESHOLD_SCOPE_NODE {
            purge.readings = tx.execute(
                "DELETE FROM node_field_readings
                 WHERE hostname = ?1 AND node_key = ?2 AND field = ?3",
                params![hostname, entity, field],
            )?;
        } else if scope == THRESHOLD_SCOPE_EYE {
            let sql = match field {
                "temperature" => Some(
                    "UPDATE eye_readings SET temperature_c = NULL,
                        temperature_min_c = NULL, temperature_max_c = NULL
                     WHERE hostname = ?1 AND mac = ?2 AND temperature_c IS NOT NULL",
                ),
                "humidity" => Some(
                    "UPDATE eye_readings SET humidity_pct = NULL,
                        humidity_min_pct = NULL, humidity_max_pct = NULL
                     WHERE hostname = ?1 AND mac = ?2 AND humidity_pct IS NOT NULL",
                ),
                _ => None,
            };
            if let Some(sql) = sql {
                purge.readings = tx.execute(sql, params![hostname, entity])?;
            }
        }
        purge.alarms = tx.execute(
            "DELETE FROM alarm_events WHERE hostname = ?1 AND sensor_id = ?2",
            params![hostname, alarm_sensor_id],
        )?;
        purge.threshold_history = tx.execute(
            "DELETE FROM field_threshold_history
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4",
            params![hostname, scope, entity, field],
        )?;
        purge.group_members = drop_from_group_selections(&tx, hostname, scope, entity, field)?;
        tx.commit()?;
        Ok(Some(purge))
    }

    /// Switches one sensor back on. It starts with no history: what it reports
    /// from now on is stored. `false` = it was on already.
    pub fn enable_sensor(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: &str,
    ) -> Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM sensor_disabled
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4",
            params![hostname, scope, entity, field],
        )?;
        Ok(deleted > 0)
    }

    /// Every switched-off sensor of one entity goes when the device is removed,
    /// so the next device registered under the same key starts with all on.
    pub fn delete_disabled_sensors_for_entity(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM sensor_disabled WHERE hostname = ?1 AND scope = ?2 AND entity = ?3",
            params![hostname, scope, entity],
        )?)
    }
}

/// Removes `field` from the explicit field selection of every group member
/// that names this entity. A member with no selection (`fields IS NULL`, every
/// field) needs nothing: the switched-off field is no longer reported to it.
fn drop_from_group_selections(
    tx: &rusqlite::Connection,
    hostname: &str,
    scope: &str,
    entity: &str,
    field: &str,
) -> Result<usize> {
    let column = if scope == THRESHOLD_SCOPE_NODE {
        "node_key"
    } else {
        "mac"
    };
    let members: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id, fields FROM sensor_group_members
             WHERE hostname = ?1 AND {column} = ?2 AND fields IS NOT NULL"
        ))?;
        let rows = stmt.query_map(params![hostname, entity], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut changed = 0;
    for (id, fields) in members {
        let Ok(list) = serde_json::from_str::<Vec<String>>(&fields) else {
            continue;
        };
        if !list.iter().any(|f| f == field) {
            continue;
        }
        let kept: Vec<&String> = list.iter().filter(|f| *f != field).collect();
        let json = serde_json::to_string(&kept).unwrap_or_else(|_| "[]".to_string());
        changed += tx.execute(
            "UPDATE sensor_group_members SET fields = ?1 WHERE id = ?2",
            params![json, id],
        )?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use crate::models::{
        EyeReading, NewAlarmEvent, NewFieldThreshold, NodeFieldReading, SOURCE_LIVE,
        THRESHOLD_SCOPE_EYE, THRESHOLD_SCOPE_NODE,
    };
    use crate::Storage;

    fn storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Storage::open(&dir.path().join("t.db"), None).expect("open");
        (dir, s)
    }

    fn reading(field: &str, ts: i64) -> NodeFieldReading {
        NodeFieldReading {
            hostname: "hub-a".into(),
            node_key: "aa11".into(),
            field: field.into(),
            ts,
            value: Some(21.5),
            alarm_state: None,
            source: SOURCE_LIVE.into(),
        }
    }

    fn alarm(sensor_id: &str) -> NewAlarmEvent {
        NewAlarmEvent {
            hostname: "hub-a".into(),
            sensor_id: sensor_id.into(),
            sensor_name: None,
            sensor_location: None,
            alarm_type: "threshold".into(),
            severity: "critical".into(),
            from_state: None,
            to_state: Some("CRITICAL".into()),
            message: None,
            value: Some(40.0),
            threshold: Some(30.0),
            triggered_at: "2026-10-13T10:00:00Z".into(),
            cleared_at: None,
            acknowledged_at: None,
            acknowledged_by_user_id: None,
            acknowledged_by_username: None,
            native_key: None,
        }
    }

    fn band(scope: &str, entity: &str, field: &str) -> NewFieldThreshold {
        NewFieldThreshold {
            hostname: "hub-a".into(),
            scope: scope.into(),
            entity: entity.into(),
            field: field.into(),
            critical_low: Some(0.0),
            warning_low: None,
            warning_high: None,
            critical_high: Some(30.0),
            enabled: true,
        }
    }

    fn count(s: &Storage, sql: &str) -> i64 {
        s.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn switching_a_node_sensor_off_deletes_its_data_and_keeps_its_band() {
        let (_dir, s) = storage();
        s.insert_field_readings(&[reading("temperature", 1), reading("humidity", 1)])
            .unwrap();
        s.insert_alarm_event(&alarm("lrw:aa11:Temperature"))
            .unwrap();
        s.insert_alarm_event(&alarm("lrw:aa11:Humidity")).unwrap();
        s.upsert_field_threshold(
            &band(THRESHOLD_SCOPE_NODE, "aa11", "temperature"),
            Some("t"),
        )
        .unwrap();
        s.conn
            .execute_batch(
                "INSERT INTO sensor_groups (name) VALUES ('g');
                 INSERT INTO sensor_group_members (group_id, hostname, node_key, fields)
                 VALUES (1, 'hub-a', 'aa11', '[\"temperature\",\"humidity\"]');",
            )
            .unwrap();

        let purge = s
            .disable_sensor(
                "hub-a",
                THRESHOLD_SCOPE_NODE,
                "aa11",
                "temperature",
                "lrw:aa11:Temperature",
                "tester",
            )
            .unwrap()
            .expect("was on");
        assert_eq!(purge.readings, 1);
        assert_eq!(purge.alarms, 1);
        assert_eq!(purge.group_members, 1);
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM node_field_readings WHERE field = 'temperature'"
            ),
            0
        );
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM node_field_readings WHERE field = 'humidity'"
            ),
            1
        );
        assert_eq!(count(&s, "SELECT COUNT(*) FROM alarm_events"), 1);
        let fields: String = s
            .conn
            .query_row("SELECT fields FROM sensor_group_members", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fields, "[\"humidity\"]");
        // The band stays for the caller to draft its removal.
        assert!(s
            .get_field_threshold("hub-a", THRESHOLD_SCOPE_NODE, "aa11", "temperature")
            .unwrap()
            .is_some());

        // Off: nothing more of it is stored, the other field still is.
        s.insert_field_readings(&[reading("temperature", 2), reading("humidity", 2)])
            .unwrap();
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM node_field_readings WHERE field = 'temperature'"
            ),
            0
        );
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM node_field_readings WHERE field = 'humidity'"
            ),
            2
        );
        // Twice off is a no-op.
        assert!(s
            .disable_sensor(
                "hub-a",
                THRESHOLD_SCOPE_NODE,
                "aa11",
                "temperature",
                "x",
                "t"
            )
            .unwrap()
            .is_none());
        assert_eq!(s.list_disabled_sensors().unwrap().len(), 1);

        // On again: stored from the next value, with no history.
        assert!(s
            .enable_sensor("hub-a", THRESHOLD_SCOPE_NODE, "aa11", "temperature")
            .unwrap());
        assert!(!s
            .enable_sensor("hub-a", THRESHOLD_SCOPE_NODE, "aa11", "temperature")
            .unwrap());
        s.insert_field_readings(&[reading("temperature", 3)])
            .unwrap();
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM node_field_readings WHERE field = 'temperature'"
            ),
            1
        );
    }

    #[test]
    fn switching_a_beacon_sensor_off_nulls_its_column_only() {
        let (_dir, s) = storage();
        let row = |ts: i64| EyeReading {
            hostname: "hub-a".into(),
            mac: "AA:BB:CC:DD:EE:01".into(),
            ts,
            temperature_c: Some(4.5),
            humidity_pct: Some(60),
            event_type: "live".into(),
            message_id: None,
        };
        s.insert_eye_reading(&row(1)).unwrap();
        s.insert_alarm_event(&alarm("eye:AA:BB:CC:DD:EE:01:Humidity"))
            .unwrap();
        let purge = s
            .disable_sensor(
                "hub-a",
                THRESHOLD_SCOPE_EYE,
                "AA:BB:CC:DD:EE:01",
                "humidity",
                "eye:AA:BB:CC:DD:EE:01:Humidity",
                "tester",
            )
            .unwrap()
            .expect("was on");
        assert_eq!((purge.readings, purge.alarms), (1, 1));
        s.insert_eye_reading(&row(2)).unwrap();
        let rows = s
            .query_eye_readings("hub-a", "AA:BB:CC:DD:EE:01", 0, 10, None)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.humidity_pct.is_none()));
        assert!(rows.iter().all(|r| r.temperature_c == Some(4.5)));
        assert_eq!(
            s.disabled_sensor_fields("hub-a", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01")
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["humidity".to_string()]
        );
        assert_eq!(
            s.delete_disabled_sensors_for_entity("hub-a", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01")
                .unwrap(),
            1
        );
        assert!(s.list_disabled_sensors().unwrap().is_empty());
    }
}
