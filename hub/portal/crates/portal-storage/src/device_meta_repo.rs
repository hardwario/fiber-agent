//! Per-device operator metadata and overrides: `device_meta` (V9, plus
//! `serial` from V14 and `name` from V16).
//!
//! The row is sparse by design — it exists only once an operator has said
//! something about a device, and every column but `backfill_auto` reads as
//! "inherit the configured default" when absent. Callers therefore treat a
//! missing row and an all-`None` row identically; nothing here creates a row
//! just to hold defaults.
//!
//! Unlike `threshold_repo`, there is no history table: these are operator
//! preferences, and the audit trail for who changed one is the `audit_log`
//! row the route writes (devices.backend.md §3, WP0.1).

use rusqlite::params;

use crate::error::Result;
use crate::models::{DeviceMeta, NewDeviceMeta};
use crate::Storage;

const DEVICE_META_COLUMNS: &str = "hostname, scope, entity, name, description, created_at, \
     added_by, offline_k_override, backfill_auto, store_interval_override, serial, location";

impl Storage {
    /// Upsert one device's metadata.
    ///
    /// `created_at` is stamped on insert and **never** updated: it is the
    /// registration time the Node list sorts by (§4), so rewriting it on
    /// every description edit would silently reshuffle the operator's list.
    pub fn upsert_device_meta(&self, row: &NewDeviceMeta) -> Result<()> {
        self.conn.execute(
            "INSERT INTO device_meta
                (hostname, scope, entity, name, description, added_by,
                 offline_k_override, backfill_auto, store_interval_override, serial, location)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (hostname, scope, entity) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                added_by = excluded.added_by,
                offline_k_override = excluded.offline_k_override,
                backfill_auto = excluded.backfill_auto,
                store_interval_override = excluded.store_interval_override,
                serial = excluded.serial,
                location = excluded.location",
            params![
                row.hostname,
                row.scope,
                row.entity,
                row.name,
                row.description,
                row.added_by,
                row.offline_k_override,
                row.backfill_auto,
                row.store_interval_override,
                row.serial,
                row.location,
            ],
        )?;
        Ok(())
    }

    /// One device's metadata, or `None` when the operator has never touched
    /// it — which means "every default", not "unknown device".
    pub fn get_device_meta(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
    ) -> Result<Option<DeviceMeta>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DEVICE_META_COLUMNS} FROM device_meta
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3"
        ))?;
        let mut rows = stmt.query_map(params![hostname, scope, entity], device_meta_from_row)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Every metadata row for one hostname and scope, ordered by `entity`.
    ///
    /// The bulk read the render and detector paths use: both need metadata
    /// for *all* Nodes (or all Beacons) at once, and doing that as one query
    /// rather than one per device is what keeps a 500-row list off a
    /// per-row round trip (§4's performance gate).
    pub fn list_device_meta(&self, hostname: &str, scope: &str) -> Result<Vec<DeviceMeta>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DEVICE_META_COLUMNS} FROM device_meta
             WHERE hostname = ?1 AND scope = ?2
             ORDER BY entity ASC"
        ))?;
        let rows = stmt.query_map(params![hostname, scope], device_meta_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every row that carries an operator name or location, across all Hubs
    /// and scopes. Loaded once at start so the store can lay the Portal's
    /// names and locations over what the Hubs report (decision #41).
    pub fn list_device_meta_labels(&self) -> Result<Vec<DeviceMeta>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DEVICE_META_COLUMNS} FROM device_meta
             WHERE name IS NOT NULL OR location IS NOT NULL
             ORDER BY hostname, scope, entity"
        ))?;
        let rows = stmt.query_map([], device_meta_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Drop one device's metadata; `false` = there was none. Used when a
    /// device is removed from the fleet, so a later re-registration starts
    /// clean rather than inheriting a predecessor's overrides.
    pub fn delete_device_meta(&self, hostname: &str, scope: &str, entity: &str) -> Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM device_meta WHERE hostname = ?1 AND scope = ?2 AND entity = ?3",
            params![hostname, scope, entity],
        )?;
        Ok(deleted > 0)
    }
}

fn device_meta_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceMeta> {
    Ok(DeviceMeta {
        hostname: r.get(0)?,
        scope: r.get(1)?,
        entity: r.get(2)?,
        name: r.get(3)?,
        description: r.get(4)?,
        created_at: r.get(5)?,
        added_by: r.get(6)?,
        offline_k_override: r.get(7)?,
        backfill_auto: r.get(8)?,
        store_interval_override: r.get(9)?,
        serial: r.get(10)?,
        location: r.get(11)?,
    })
}

impl Storage {
    /// Every distinct non-blank `location` held portal-side (Hub and probe
    /// rows), one input to the derived suggestion list (metadata.cs.md).
    pub fn distinct_meta_locations(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT location FROM device_meta
             WHERE location IS NOT NULL AND trim(location) <> ''",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// [`Self::list_device_meta`] as a map keyed by `entity`.
    ///
    /// The bulk shape the render and detector paths want: the offline sweep
    /// and the Node list both need metadata for the whole fleet at once, and
    /// a per-device query there is what fails devices.backend.md §4's
    /// 500-row performance gate.
    pub fn device_meta_map(
        &self,
        hostname: &str,
        scope: &str,
    ) -> Result<std::collections::HashMap<String, DeviceMeta>> {
        Ok(self
            .list_device_meta(hostname, scope)?
            .into_iter()
            .map(|row| (row.entity.clone(), row))
            .collect())
    }
}
