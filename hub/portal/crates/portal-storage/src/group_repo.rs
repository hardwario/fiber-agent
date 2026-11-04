//! Sensor groups and their membership — what `/monitoring` renders.
//!
//! Membership is replaced **wholesale** on save rather than patched member by
//! member. v1 exposes `POST /members`, `DELETE /members` and
//! `PUT /members/reorder` because its dialog is a React component holding
//! local state and syncing it back; here the dialog is a plain form that
//! submits the group's full membership in one request, so the repository
//! only needs "these are the members now". Same UI, a fraction of the API.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::models::{NewSensorGroupMember, SensorGroup, SensorGroupMember};
use crate::Storage;

/// Which neighbour [`Storage::move_sensor_group`] swaps a group with, in the
/// page's own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDirection {
    Up,
    Down,
}

impl Storage {
    /// Every group, ordered the way the page shows them: `sort_order` first
    /// (reserved, always 0 today), then by name. Name ordering is
    /// case-insensitive so "freezer" and "Freezer" don't split.
    pub fn list_sensor_groups(&self) -> Result<Vec<SensorGroup>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, description, sort_order, created_at, location
               FROM sensor_groups
              ORDER BY sort_order, name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SensorGroup {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                    location: r.get(5)?,
                    sort_order: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_sensor_group(&self, id: i64) -> Result<Option<SensorGroup>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name, description, sort_order, created_at, location
                   FROM sensor_groups WHERE id = ?1",
                params![id],
                |r| {
                    Ok(SensorGroup {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        description: r.get(2)?,
                        location: r.get(5)?,
                        sort_order: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Every member of every group in one query — the monitoring page needs
    /// all of them at once, and one round-trip beats one per group.
    pub fn list_all_group_members(&self) -> Result<Vec<SensorGroupMember>> {
        self.query_members(
            "SELECT id, group_id, hostname, sensor_line, node_key, mac, fields, sort_order
                              FROM sensor_group_members
                             ORDER BY group_id, sort_order, id",
            params![],
        )
    }

    pub fn list_group_members(&self, group_id: i64) -> Result<Vec<SensorGroupMember>> {
        self.query_members(
            "SELECT id, group_id, hostname, sensor_line, node_key, mac, fields, sort_order
               FROM sensor_group_members
              WHERE group_id = ?1
              ORDER BY sort_order, id",
            params![group_id],
        )
    }

    fn query_members(&self, sql: &str, p: impl rusqlite::Params) -> Result<Vec<SensorGroupMember>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(p, |r| {
                let fields: Option<String> = r.get(6)?;
                Ok(SensorGroupMember {
                    id: r.get(0)?,
                    group_id: r.get(1)?,
                    hostname: r.get(2)?,
                    sensor_line: r.get(3)?,
                    node_key: r.get(4)?,
                    mac: r.get(5)?,
                    // A malformed `fields` blob degrades to "all fields"
                    // rather than failing the whole page — the column is a
                    // display preference, not a correctness input.
                    fields: fields.and_then(|s| serde_json::from_str(&s).ok()),
                    sort_order: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Creates a group and its membership in one transaction. Returns the new
    /// group's id.
    ///
    /// The group lands **last**: `sort_order = MAX(sort_order) + 1`, computed
    /// inside the same transaction so two concurrent creates cannot collide on
    /// one position. V8's column default is `0`, and once
    /// [`Self::move_sensor_group`] has renumbered the page to `0..n-1` the
    /// first group also holds `0` — a new group would then tie with it and the
    /// `name COLLATE NOCASE` tiebreak would drop it into the middle of an
    /// arrangement the operator had just made (devices-side review 2026-09-04,
    /// F6; monitoring.backend.md §13.6).
    pub fn create_sensor_group(
        &self,
        name: &str,
        description: Option<&str>,
        members: &[NewSensorGroupMember],
    ) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO sensor_groups (name, description, sort_order)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(sort_order), 0) + 1 FROM sensor_groups))",
            params![name, description],
        )?;
        let id = tx.last_insert_rowid();
        insert_members(&tx, id, members)?;
        tx.commit()?;
        Ok(id)
    }

    /// Renames a group and replaces its membership. Returns `false` if no such
    /// group exists.
    pub fn update_sensor_group(
        &self,
        id: i64,
        name: &str,
        description: Option<&str>,
        members: &[NewSensorGroupMember],
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let updated = tx.execute(
            "UPDATE sensor_groups SET name = ?2, description = ?3 WHERE id = ?1",
            params![id, name, description],
        )?;
        if updated == 0 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM sensor_group_members WHERE group_id = ?1",
            params![id],
        )?;
        insert_members(&tx, id, members)?;
        tx.commit()?;
        Ok(true)
    }

    /// Sets (or, with `None`, clears) a group's location (V40). Kept apart
    /// from `create_sensor_group` / `update_sensor_group` so the
    /// [`SensorGroup`] shape every caller builds stays as it is. Returns
    /// `false` for an unknown id.
    pub fn set_sensor_group_location(&self, id: i64, location: Option<&str>) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE sensor_groups SET location = ?2 WHERE id = ?1",
            params![id, location],
        )?;
        Ok(n > 0)
    }

    /// Every group's location that is set, as `(group id, location)`.
    pub fn list_sensor_group_locations(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, location FROM sensor_groups
              WHERE location IS NOT NULL AND trim(location) <> ''",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Members go with the group (`ON DELETE CASCADE`, and `foreign_keys` is
    /// on — see `Storage::open`).
    pub fn delete_sensor_group(&self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM sensor_groups WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Moves one group one place up or down in `list_sensor_groups`' order.
    ///
    /// Returns `false` for an unknown id and for the two no-ops (`Up` on the
    /// first group, `Down` on the last), so the caller can tell "nothing to
    /// do" apart from "done" — the route audits that outcome verbatim.
    ///
    /// `sort_order` was declared in V8 and never written: every row still
    /// holds the default `0`, so the page is *really* ordered by name and
    /// swapping two zeroes would change nothing. The first move that finds
    /// the column flat therefore renumbers **every** group to its current
    /// display index — promoting the implicit name order to an explicit one —
    /// and only then swaps the pair; later moves find distinct values and
    /// swap in place. One transaction, because a half-renumbered table would
    /// reorder the page arbitrarily.
    pub fn move_sensor_group(&self, id: i64, direction: MoveDirection) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;

        // The page's order, verbatim from `list_sensor_groups`.
        let mut rows: Vec<(i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT id, sort_order
                   FROM sensor_groups
                  ORDER BY sort_order, name COLLATE NOCASE",
            )?;
            let ordered = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ordered
        };

        let Some(pos) = rows.iter().position(|(row_id, _)| *row_id == id) else {
            return Ok(false);
        };
        let other = match direction {
            MoveDirection::Up if pos > 0 => pos - 1,
            MoveDirection::Down if pos + 1 < rows.len() => pos + 1,
            _ => return Ok(false),
        };

        // Strictly ascending is the only state a plain swap is meaningful in.
        if !rows.windows(2).all(|w| w[0].1 < w[1].1) {
            for (index, (row_id, order)) in rows.iter_mut().enumerate() {
                *order = index as i64;
                tx.execute(
                    "UPDATE sensor_groups SET sort_order = ?2 WHERE id = ?1",
                    params![*row_id, *order],
                )?;
            }
        }

        // Read from the unmutated snapshot so this is a swap, not two writes
        // of the same value.
        for (target, source) in [(pos, other), (other, pos)] {
            tx.execute(
                "UPDATE sensor_groups SET sort_order = ?2 WHERE id = ?1",
                params![rows[target].0, rows[source].1],
            )?;
        }

        tx.commit()?;
        Ok(true)
    }
}

/// `INSERT OR IGNORE` so a form that names the same entity twice saves the
/// first one instead of failing on the unique index. The dialog uses
/// checkboxes and cannot produce a duplicate, but a hand-made POST can.
fn insert_members(
    tx: &rusqlite::Transaction<'_>,
    group_id: i64,
    members: &[NewSensorGroupMember],
) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT OR IGNORE INTO sensor_group_members
            (group_id, hostname, sensor_line, node_key, mac, fields, sort_order)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for (i, m) in members.iter().enumerate() {
        let fields = m
            .fields
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .unwrap_or(None);
        stmt.execute(params![
            group_id,
            m.hostname,
            m.sensor_line,
            m.node_key,
            m.mac,
            fields,
            i as i64,
        ])?;
    }
    Ok(())
}
