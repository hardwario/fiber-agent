//! Calibration certificates of Nodes and Beacons — `device_calibrations`
//! (V35, deletion columns V36, plan/portal/calibration.cs.md §4).
//!
//! The rows only; the PDFs live on disk under the portal's data directory
//! and the `portal` crate's `calibration` module owns them. A device's
//! **current** calibration is its newest row that is not deleted — every
//! read below orders by `uploaded_at` and then `rowid`, so two uploads
//! inside one second still have a defined newest.
//!
//! No row is ever removed: an old certificate is the history the card
//! lists, removing a device keeps them as well (§4), and an admin's delete
//! (amendment 2026-10-13) only marks the row — the record of who deleted
//! which certificate and why outlives the PDF.

use rusqlite::{params, OptionalExtension, Row};

use crate::error::Result;
use crate::models::{DeviceCalibration, NewDeviceCalibration};
use crate::Storage;

const CALIBRATION_COLUMNS: &str = "id, hostname, scope, entity, valid_until, file_name, \
     file_size, sha256, note, uploaded_by, uploaded_at, deleted_at, deleted_by, delete_message";

/// "Newest first" in one place, so the card, the date edit and the sweep
/// cannot disagree about which row is current.
const NEWEST_FIRST: &str = "uploaded_at DESC, rowid DESC";

fn calibration_from_row(r: &Row<'_>) -> rusqlite::Result<DeviceCalibration> {
    Ok(DeviceCalibration {
        id: r.get(0)?,
        hostname: r.get(1)?,
        scope: r.get(2)?,
        entity: r.get(3)?,
        valid_until: r.get(4)?,
        file_name: r.get(5)?,
        file_size: r.get(6)?,
        sha256: r.get(7)?,
        note: r.get(8)?,
        uploaded_by: r.get(9)?,
        uploaded_at: r.get(10)?,
        deleted_at: r.get(11)?,
        deleted_by: r.get(12)?,
        delete_message: r.get(13)?,
    })
}

impl Storage {
    /// Writes one uploaded certificate. It becomes the device's current
    /// calibration: it is the newest row by construction.
    ///
    /// `uploaded_at` is stamped here, not taken from the caller, for the
    /// reason `insert_sensor_event` gives: the upload moment is the record
    /// of writing the row and must not be settable by whoever writes it.
    pub fn insert_device_calibration(&self, row: &NewDeviceCalibration) -> Result<()> {
        self.conn.execute(
            "INSERT INTO device_calibrations
                (id, hostname, scope, entity, valid_until, file_name, file_size,
                 sha256, note, uploaded_by, uploaded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.id,
                row.hostname,
                row.scope,
                row.entity,
                row.valid_until,
                row.file_name,
                row.file_size,
                row.sha256,
                row.note,
                row.uploaded_by,
                now_unix(),
            ],
        )?;
        Ok(())
    }

    /// One device's certificates, newest first, deleted ones included (the
    /// card lists them as deleted). The current one is the first that is not
    /// deleted.
    pub fn list_device_calibrations(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
    ) -> Result<Vec<DeviceCalibration>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CALIBRATION_COLUMNS} FROM device_calibrations
             WHERE hostname = ?1 AND scope = ?2 AND entity = ?3
             ORDER BY {NEWEST_FIRST}"
        ))?;
        let rows = stmt.query_map(params![hostname, scope, entity], calibration_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// How many certificates one device holds — the upload route's cap
    /// (§4: at most 50 per device) is checked against this. A deleted one
    /// does not count: the cap bounds the files on disk, and a deleted row
    /// has none.
    pub fn count_device_calibrations(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
    ) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM device_calibrations
                 WHERE hostname = ?1 AND scope = ?2 AND entity = ?3
                   AND deleted_at IS NULL",
                params![hostname, scope, entity],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// One certificate by id, or `None`. The download route checks the row
    /// belongs to the device in its URL before serving the file: an id is a
    /// global handle, and a page must not hand out another device's PDF.
    pub fn get_device_calibration(&self, id: &str) -> Result<Option<DeviceCalibration>> {
        self.conn
            .query_row(
                &format!("SELECT {CALIBRATION_COLUMNS} FROM device_calibrations WHERE id = ?1"),
                params![id],
                calibration_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Moves the current certificate's date without a new PDF (§2's
    /// "Upravit datum"). Returns `(id, previous valid_until)` of the row it
    /// changed, or `None` when the device has no calibration at all.
    ///
    /// Only the current row (the newest not deleted) moves: an older
    /// certificate's date is a fact about that document, and rewriting
    /// history is not what the button is for.
    pub fn update_current_calibration_valid_until(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        valid_until: i64,
    ) -> Result<Option<(String, i64)>> {
        let current: Option<(String, i64)> = self
            .conn
            .query_row(
                &format!(
                    "SELECT id, valid_until FROM device_calibrations
                     WHERE hostname = ?1 AND scope = ?2 AND entity = ?3
                       AND deleted_at IS NULL
                     ORDER BY {NEWEST_FIRST} LIMIT 1"
                ),
                params![hostname, scope, entity],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, previous)) = current else {
            return Ok(None);
        };
        self.conn.execute(
            "UPDATE device_calibrations SET valid_until = ?2 WHERE id = ?1",
            params![id, valid_until],
        )?;
        Ok(Some((id, previous)))
    }

    /// The current certificate of every device on every Hub, for the
    /// reminder sweep — one row per `(hostname, scope, entity)` that holds a
    /// certificate not deleted.
    pub fn list_current_calibrations(&self) -> Result<Vec<DeviceCalibration>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CALIBRATION_COLUMNS} FROM device_calibrations AS c
             WHERE c.rowid = (
                 SELECT n.rowid FROM device_calibrations AS n
                 WHERE n.hostname = c.hostname AND n.scope = c.scope AND n.entity = c.entity
                   AND n.deleted_at IS NULL
                 ORDER BY n.uploaded_at DESC, n.rowid DESC
                 LIMIT 1
             )
             ORDER BY c.hostname, c.scope, c.entity"
        ))?;
        let rows = stmt.query_map([], calibration_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Marks one certificate deleted (amendment 2026-10-13): who, when,
    /// why. `false` when no row with this id stands — unknown, or deleted
    /// already — so a repeated delete changes nothing and says so.
    ///
    /// `deleted_at` is stamped here for `insert_device_calibration`'s
    /// reason.
    pub fn mark_device_calibration_deleted(
        &self,
        id: &str,
        deleted_by: &str,
        message: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE device_calibrations
                SET deleted_at = ?2, deleted_by = ?3, delete_message = ?4
              WHERE id = ?1 AND deleted_at IS NULL",
            params![id, now_unix(), deleted_by, message],
        )?;
        Ok(changed == 1)
    }
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
