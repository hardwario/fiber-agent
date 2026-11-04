//! Per-field alarm thresholds for Nodes and EYE beacons:
//! `field_thresholds` + `field_threshold_history`.
//!
//! History is an audit trail of *actual* value changes: an upsert that
//! repeats the current four threshold values writes no history row (the
//! firmware echoes authoritative threshold lists on every aggregate
//! publish — mqtt.md `lorawan/sensors` "reconciled with actor
//! firmware-echo" — so no-op writes are the common case).

use rusqlite::params;

use crate::error::Result;
use crate::models::{FieldThreshold, FieldThresholdChange, NewFieldThreshold};
use crate::Storage;

impl Storage {
    /// Upsert a threshold row. Returns `false` when the stored row already
    /// matches (values and `enabled`) — nothing written. A history row is
    /// appended only when the four threshold values actually changed
    /// (first write included); toggling `enabled` alone updates the row
    /// but writes no history.
    pub fn upsert_field_threshold(
        &self,
        row: &NewFieldThreshold,
        actor: Option<&str>,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = self.upsert_field_threshold_in(&tx, row, actor)?;
        tx.commit()?;
        Ok(changed)
    }

    /// [`Self::upsert_field_threshold`] inside a transaction the caller holds, so a write that
    /// must land with others can (`land_hub_threshold`, PF-3).
    pub(crate) fn upsert_field_threshold_in(
        &self,
        tx: &rusqlite::Connection,
        row: &NewFieldThreshold,
        actor: Option<&str>,
    ) -> Result<bool> {
        // Bounds and switch only: the upsert neither reads nor writes the hysteresis (V32), so
        // it still runs on a pre-V32 schema, which the migration fixtures build.
        let existing = {
            let mut stmt = self.conn.prepare(
                "SELECT critical_low, warning_low, warning_high, critical_high, enabled
                 FROM field_thresholds
                 WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4",
            )?;
            let mut rows = stmt.query_map(
                params![row.hostname, row.scope, row.entity, row.field],
                |r| {
                    Ok(ExistingBand {
                        critical_low: r.get(0)?,
                        warning_low: r.get(1)?,
                        warning_high: r.get(2)?,
                        critical_high: r.get(3)?,
                        enabled: r.get(4)?,
                    })
                },
            )?;
            rows.next().transpose()?
        };

        let values_changed = match &existing {
            Some(e) => {
                e.critical_low != row.critical_low
                    || e.warning_low != row.warning_low
                    || e.warning_high != row.warning_high
                    || e.critical_high != row.critical_high
            }
            None => true,
        };
        let enabled_changed = existing.as_ref().map(|e| e.enabled) != Some(row.enabled);
        if !values_changed && !enabled_changed {
            return Ok(false);
        }

        tx.execute(
            "INSERT INTO field_thresholds
                (hostname, scope, entity, field,
                 critical_low, warning_low, warning_high, critical_high,
                 enabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))
             ON CONFLICT (hostname, scope, entity, field) DO UPDATE SET
                critical_low = excluded.critical_low,
                warning_low = excluded.warning_low,
                warning_high = excluded.warning_high,
                critical_high = excluded.critical_high,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at",
            params![
                row.hostname,
                row.scope,
                row.entity,
                row.field,
                row.critical_low,
                row.warning_low,
                row.warning_high,
                row.critical_high,
                row.enabled,
            ],
        )?;
        if values_changed {
            tx.execute(
                "INSERT INTO field_threshold_history
                    (hostname, scope, entity, field,
                     critical_low, warning_low, warning_high, critical_high, actor)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.hostname,
                    row.scope,
                    row.entity,
                    row.field,
                    row.critical_low,
                    row.warning_low,
                    row.warning_high,
                    row.critical_high,
                    actor,
                ],
            )?;
        }
        Ok(true)
    }

    /// Writes **only** the `enabled` flag, leaving the four stored bounds
    /// exactly where they were. `true` = the stored flag actually moved.
    ///
    /// B4 split a Node threshold edit in two (devices.backend.md §21.3): the
    /// bounds become a `pending_config` draft and reach this table only when
    /// a send walk drains, while `enabled` is a portal-side switch over
    /// whether a stored threshold is evaluated at all — not a device setting
    /// — and keeps writing through immediately.
    ///
    /// Routing that write through [`Self::upsert_field_threshold`] with the
    /// form's bounds is what leaked the draft into "current" (§21 review,
    /// defect 1): the whole-row upsert wrote the *drafted* numbers, so they
    /// survived a Discard and `plan_send` then filled a field's untouched
    /// bounds from a table that already held the draft.
    ///
    /// A row that does not exist yet is inserted with **no** bounds rather
    /// than with the draft's. `merge_node_thresholds` lets the firmware echo
    /// win on bounds and this table win on `enabled`, so an all-NULL row
    /// says exactly what it means: the operator has an opinion about whether
    /// this threshold is evaluated and none about where it fires.
    ///
    /// **A missing row already means `enabled`** (§21 review round 2, item
    /// 10). `merge_node_thresholds` defaults a field with no stored row to
    /// `enabled: true`, so `enabled = true` on an unstored field changes
    /// nothing and writes nothing — the earlier `None == Some(true)`
    /// comparison never matched and inserted an all-NULL row on every first
    /// threshold draft, a row that said only what the absence of a row
    /// already said. Switching a threshold **off** on an unstored field is
    /// the case that does need the row: that is an opinion the default does
    /// not carry.
    ///
    /// Writes no history row — that table records value changes and this
    /// changes no value, the same rule [`Self::upsert_field_threshold`]
    /// already follows for an `enabled`-only edit.
    pub fn set_field_threshold_enabled(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: &str,
        enabled: bool,
    ) -> Result<bool> {
        let existing = self.get_field_threshold(hostname, scope, entity, field)?;
        if existing.map(|e| e.enabled).unwrap_or(true) == enabled {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO field_thresholds
                (hostname, scope, entity, field,
                 critical_low, warning_low, warning_high, critical_high,
                 enabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, NULL, ?5, datetime('now'))
             ON CONFLICT (hostname, scope, entity, field) DO UPDATE SET
                enabled = excluded.enabled,
                updated_at = excluded.updated_at",
            params![hostname, scope, entity, field, enabled],
        )?;
        Ok(true)
    }

    /// One threshold row, or `None`.
    pub fn get_field_threshold(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: &str,
    ) -> Result<Option<FieldThreshold>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, scope, entity, field,
                    critical_low, warning_low, warning_high, critical_high,
                    enabled, updated_at, hysteresis
             FROM field_thresholds
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4",
        )?;
        let mut rows =
            stmt.query_map(params![hostname, scope, entity, field], threshold_from_row)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// All threshold rows for one entity, ordered by `field`.
    pub fn list_field_thresholds(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
    ) -> Result<Vec<FieldThreshold>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, scope, entity, field,
                    critical_low, warning_low, warning_high, critical_high,
                    enabled, updated_at, hysteresis
             FROM field_thresholds
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3
             ORDER BY field ASC",
        )?;
        let rows = stmt.query_map(params![hostname, scope, entity], threshold_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The threshold rows of one entity under every Hub hostname: a renamed
    /// Hub's rows and a cluster's Server-held rows both count when asking
    /// whether a field still has a rule (the orphan-alarm sweep).
    pub fn field_thresholds_any_hub(
        &self,
        scope: &str,
        entity: &str,
    ) -> Result<Vec<FieldThreshold>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, scope, entity, field,
                    critical_low, warning_low, warning_high, critical_high,
                    enabled, updated_at, hysteresis
             FROM field_thresholds
             WHERE scope = ?1 AND entity = ?2 COLLATE NOCASE
             ORDER BY hostname, field",
        )?;
        let rows = stmt.query_map(params![scope, entity], threshold_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every threshold row of one scope under every hostname, ordered by
    /// `(hostname, entity, field)`: the Portal's wired-line bounds loaded at start
    /// (decision #41).
    pub fn list_field_thresholds_in_scope(&self, scope: &str) -> Result<Vec<FieldThreshold>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, scope, entity, field,
                    critical_low, warning_low, warning_high, critical_high,
                    enabled, updated_at, hysteresis
             FROM field_thresholds
             WHERE scope = ?1
             ORDER BY hostname, entity, field",
        )?;
        let rows = stmt.query_map(params![scope], threshold_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every threshold row for one hostname and scope, ordered by
    /// `(entity, field)`.
    ///
    /// The whole-fleet read, as against [`Self::list_field_thresholds`]'s
    /// one-entity read. A list of 500 Nodes needs per-field threshold state
    /// on every row (devices.backend.md §4), and doing that one entity at a
    /// time is the shape that fails that section's performance gate.
    pub fn list_field_thresholds_for_scope(
        &self,
        hostname: &str,
        scope: &str,
    ) -> Result<Vec<FieldThreshold>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, scope, entity, field,
                    critical_low, warning_low, warning_high, critical_high,
                    enabled, updated_at, hysteresis
             FROM field_thresholds
             WHERE hostname = ?1 AND scope = ?2
             ORDER BY entity ASC, field ASC",
        )?;
        let rows = stmt.query_map(params![hostname, scope], threshold_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Undoes an [`Self::upsert_field_threshold`] of `row` whose device
    /// command then failed (plan/decisions.md #28a: a band is stored before
    /// its command is sent). `previous` goes back, or the row is deleted when
    /// there was none, and the history row the upsert appended is dropped, so
    /// the history lists only bands the device took.
    pub fn revert_field_threshold(
        &self,
        row: &NewFieldThreshold,
        previous: Option<&FieldThreshold>,
    ) -> Result<()> {
        let values_changed = previous.is_none_or(|p| {
            p.critical_low != row.critical_low
                || p.warning_low != row.warning_low
                || p.warning_high != row.warning_high
                || p.critical_high != row.critical_high
        });
        let tx = self.conn.unchecked_transaction()?;
        match previous {
            Some(p) => {
                tx.execute(
                    "UPDATE field_thresholds
                     SET critical_low = ?5, warning_low = ?6, warning_high = ?7,
                         critical_high = ?8, enabled = ?9, updated_at = ?10
                     WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4",
                    params![
                        p.hostname,
                        p.scope,
                        p.entity,
                        p.field,
                        p.critical_low,
                        p.warning_low,
                        p.warning_high,
                        p.critical_high,
                        p.enabled,
                        p.updated_at,
                    ],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM field_thresholds
                     WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4",
                    params![row.hostname, row.scope, row.entity, row.field],
                )?;
            }
        }
        if values_changed {
            tx.execute(
                "DELETE FROM field_threshold_history
                 WHERE id = (SELECT MAX(id) FROM field_threshold_history
                             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4)
                   AND critical_low IS ?5 AND warning_low IS ?6
                   AND warning_high IS ?7 AND critical_high IS ?8",
                params![
                    row.hostname,
                    row.scope,
                    row.entity,
                    row.field,
                    row.critical_low,
                    row.warning_low,
                    row.warning_high,
                    row.critical_high,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete one threshold row; `false` = no such row. (Deletion is not
    /// recorded in history — v1 semantics: history tracks value changes.)
    pub fn delete_field_threshold(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: &str,
    ) -> Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM field_thresholds
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4",
            params![hostname, scope, entity, field],
        )?;
        Ok(deleted > 0)
    }

    /// Delete every threshold row of one entity — what a device removal does,
    /// so the next device registered at the same MAC or `node_key` does not
    /// inherit bounds nobody set for it. Returns how many rows went. The
    /// change history stays: it records what was set, and when.
    pub fn delete_field_thresholds_for_entity(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
    ) -> Result<usize> {
        let deleted = self.conn.execute(
            "DELETE FROM field_thresholds
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3",
            params![hostname, scope, entity],
        )?;
        Ok(deleted)
    }

    /// Deletes every row of one `field` in one `scope`, on every Hub and entity, and returns
    /// the `(hostname, entity)` of each row that went, for the caller's audit trail.
    ///
    /// For a field that stopped being thresholdable: the Node's battery
    /// (plan/portal/threshold_defaults.md §5), whose band could only duplicate the Node's
    /// own low-battery alarm. The change history stays, like on every deletion here.
    pub fn delete_field_thresholds_named(
        &self,
        scope: &str,
        field: &str,
    ) -> Result<Vec<(String, String)>> {
        let tx = self.conn.unchecked_transaction()?;
        let rows = {
            let mut stmt = tx.prepare(
                "SELECT hostname, entity FROM field_thresholds
                 WHERE scope = ?1 AND field = ?2
                 ORDER BY hostname ASC, entity ASC",
            )?;
            let rows = stmt.query_map(params![scope, field], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<(String, String)>>>()?
        };
        tx.execute(
            "DELETE FROM field_thresholds WHERE scope = ?1 AND field = ?2",
            params![scope, field],
        )?;
        tx.commit()?;
        Ok(rows)
    }

    /// Inserts each row that has no row yet for its `(hostname, scope, entity, field)`, and
    /// leaves every existing row exactly as it is, whatever it holds. Returns the fields it
    /// inserted. Each insert writes one history row naming `actor`.
    ///
    /// The default bands a new sensor gets (plan/portal/threshold_defaults.md §3): written
    /// once, switched off, and never over an operator's own row — a band they set, and a
    /// threshold they switched off, both win. One transaction, so a sensor gets all its
    /// defaults or none.
    pub fn insert_missing_field_thresholds(
        &self,
        rows: &[NewFieldThreshold],
        actor: &str,
    ) -> Result<Vec<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut inserted = Vec::new();
        for row in rows {
            let added = tx.execute(
                "INSERT INTO field_thresholds
                    (hostname, scope, entity, field,
                     critical_low, warning_low, warning_high, critical_high,
                     enabled, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))
                 ON CONFLICT (hostname, scope, entity, field) DO NOTHING",
                params![
                    row.hostname,
                    row.scope,
                    row.entity,
                    row.field,
                    row.critical_low,
                    row.warning_low,
                    row.warning_high,
                    row.critical_high,
                    row.enabled,
                ],
            )?;
            if added == 0 {
                continue;
            }
            tx.execute(
                "INSERT INTO field_threshold_history
                    (hostname, scope, entity, field,
                     critical_low, warning_low, warning_high, critical_high, actor)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.hostname,
                    row.scope,
                    row.entity,
                    row.field,
                    row.critical_low,
                    row.warning_low,
                    row.warning_high,
                    row.critical_high,
                    actor,
                ],
            )?;
            inserted.push(row.field.clone());
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Change history for an entity, newest first, optionally narrowed to
    /// one `field`.
    pub fn threshold_history(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: Option<&str>,
    ) -> Result<Vec<FieldThresholdChange>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, hostname, scope, entity, field,
                    critical_low, warning_low, warning_high, critical_high,
                    actor, changed_at
             FROM field_threshold_history
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3
               AND (?4 IS NULL OR field = ?4)
             ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![hostname, scope, entity, field], |r| {
            Ok(FieldThresholdChange {
                id: r.get(0)?,
                hostname: r.get(1)?,
                scope: r.get(2)?,
                entity: r.get(3)?,
                field: r.get(4)?,
                critical_low: r.get(5)?,
                warning_low: r.get(6)?,
                warning_high: r.get(7)?,
                critical_high: r.get(8)?,
                actor: r.get(9)?,
                changed_at: r.get(10)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// What [`Storage::upsert_field_threshold_in`] compares a write against.
struct ExistingBand {
    critical_low: Option<f64>,
    warning_low: Option<f64>,
    warning_high: Option<f64>,
    critical_high: Option<f64>,
    enabled: bool,
}

pub(crate) fn threshold_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FieldThreshold> {
    Ok(FieldThreshold {
        hostname: r.get(0)?,
        scope: r.get(1)?,
        entity: r.get(2)?,
        field: r.get(3)?,
        critical_low: r.get(4)?,
        warning_low: r.get(5)?,
        warning_high: r.get(6)?,
        critical_high: r.get(7)?,
        enabled: r.get(8)?,
        updated_at: r.get(9)?,
        hysteresis: r.get(10)?,
    })
}

impl Storage {
    /// [`Self::list_field_thresholds_for_scope`] grouped by `entity` — one
    /// query for the whole fleet, in the shape a list render indexes by.
    pub fn field_thresholds_by_entity(
        &self,
        hostname: &str,
        scope: &str,
    ) -> Result<std::collections::HashMap<String, Vec<FieldThreshold>>> {
        let mut out: std::collections::HashMap<String, Vec<FieldThreshold>> =
            std::collections::HashMap::new();
        for row in self.list_field_thresholds_for_scope(hostname, scope)? {
            out.entry(row.entity.clone()).or_default().push(row);
        }
        Ok(out)
    }
}

// Hysteresis (V32, decision #41): a column of the row, read and written on its own so the
// bounds' upsert, history and revert paths stay as they are. NULL = the default of the
// field's quantity.
impl Storage {
    /// One field's own hysteresis, `None` when it follows the default (or has no row).
    pub fn field_threshold_hysteresis(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: &str,
    ) -> Result<Option<f64>> {
        let mut stmt = self.conn.prepare(
            "SELECT hysteresis FROM field_thresholds
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND field = ?4",
        )?;
        let mut rows = stmt.query_map(params![hostname, scope, entity, field], |r| {
            r.get::<_, Option<f64>>(0)
        })?;
        Ok(rows.next().transpose()?.flatten())
    }

    /// Every field of one entity that has a hysteresis of its own, by field.
    pub fn field_threshold_hysteresis_map(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
    ) -> Result<std::collections::HashMap<String, f64>> {
        let mut stmt = self.conn.prepare(
            "SELECT field, hysteresis FROM field_thresholds
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3 AND hysteresis IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![hostname, scope, entity], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (field, h) = row?;
            out.insert(field, h);
        }
        Ok(out)
    }

    /// Sets one field's hysteresis; `None` puts it back on the default. A field with no row
    /// yet gets one with no bounds and switched on, which is what a missing row already
    /// means ([`Self::set_field_threshold_enabled`]). `true` = the stored value moved.
    pub fn set_field_threshold_hysteresis(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: &str,
        hysteresis: Option<f64>,
    ) -> Result<bool> {
        let exists = self
            .get_field_threshold(hostname, scope, entity, field)?
            .is_some();
        if exists && self.field_threshold_hysteresis(hostname, scope, entity, field)? == hysteresis
        {
            return Ok(false);
        }
        if !exists && hysteresis.is_none() {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO field_thresholds
                (hostname, scope, entity, field, enabled, updated_at, hysteresis)
             VALUES (?1, ?2, ?3, ?4, 1, datetime('now'), ?5)
             ON CONFLICT (hostname, scope, entity, field) DO UPDATE SET
                hysteresis = excluded.hysteresis,
                updated_at = excluded.updated_at",
            params![hostname, scope, entity, field, hysteresis],
        )?;
        Ok(true)
    }
}

#[cfg(test)]
mod hysteresis_tests {
    use crate::models::{NewFieldThreshold, THRESHOLD_SCOPE_HUB, THRESHOLD_SCOPE_NODE};
    use crate::Storage;

    fn storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Storage::open(&dir.path().join("t.db"), None).expect("open");
        (dir, s)
    }

    #[test]
    fn a_hub_line_is_a_threshold_row_with_its_own_hysteresis() {
        let (_dir, s) = storage();
        s.upsert_field_threshold(
            &NewFieldThreshold {
                hostname: "hub-a".into(),
                scope: THRESHOLD_SCOPE_HUB.into(),
                entity: "3".into(),
                field: "temperature".into(),
                critical_low: Some(-10.0),
                warning_low: Some(0.0),
                warning_high: Some(40.0),
                critical_high: Some(50.0),
                enabled: true,
            },
            Some("tester"),
        )
        .expect("upsert");
        assert_eq!(
            s.field_threshold_hysteresis("hub-a", THRESHOLD_SCOPE_HUB, "3", "temperature")
                .unwrap(),
            None
        );
        assert!(s
            .set_field_threshold_hysteresis(
                "hub-a",
                THRESHOLD_SCOPE_HUB,
                "3",
                "temperature",
                Some(1.0)
            )
            .unwrap());
        assert!(!s
            .set_field_threshold_hysteresis(
                "hub-a",
                THRESHOLD_SCOPE_HUB,
                "3",
                "temperature",
                Some(1.0)
            )
            .unwrap());
        // A bounds upsert leaves the hysteresis alone.
        s.upsert_field_threshold(
            &NewFieldThreshold {
                hostname: "hub-a".into(),
                scope: THRESHOLD_SCOPE_HUB.into(),
                entity: "3".into(),
                field: "temperature".into(),
                critical_low: Some(-5.0),
                warning_low: Some(0.0),
                warning_high: Some(40.0),
                critical_high: Some(50.0),
                enabled: true,
            },
            Some("tester"),
        )
        .expect("upsert");
        let map = s
            .field_threshold_hysteresis_map("hub-a", THRESHOLD_SCOPE_HUB, "3")
            .unwrap();
        assert_eq!(map.get("temperature"), Some(&1.0));
    }

    #[test]
    fn a_hysteresis_on_a_field_without_a_row_adds_a_bare_enabled_row() {
        let (_dir, s) = storage();
        assert!(!s
            .set_field_threshold_hysteresis("h", THRESHOLD_SCOPE_NODE, "n1", "humidity", None)
            .unwrap());
        assert!(s
            .get_field_threshold("h", THRESHOLD_SCOPE_NODE, "n1", "humidity")
            .unwrap()
            .is_none());
        assert!(s
            .set_field_threshold_hysteresis("h", THRESHOLD_SCOPE_NODE, "n1", "humidity", Some(5.0))
            .unwrap());
        let row = s
            .get_field_threshold("h", THRESHOLD_SCOPE_NODE, "n1", "humidity")
            .unwrap()
            .expect("row");
        assert!(row.enabled);
        assert_eq!(row.critical_low, None);
        // Back to the default.
        assert!(s
            .set_field_threshold_hysteresis("h", THRESHOLD_SCOPE_NODE, "n1", "humidity", None)
            .unwrap());
        assert_eq!(
            s.field_threshold_hysteresis("h", THRESHOLD_SCOPE_NODE, "n1", "humidity")
                .unwrap(),
            None
        );
    }
}
