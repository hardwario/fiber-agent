//! Manual operator events on a probe — `sensor_events` (V12).
//!
//! The third source of the probe-detail window's event list, beside the two
//! `alarm_events` classes (plan/portal/monitoring.md §6.3/§6.5). Create,
//! list, read one, delete. **No update**: a correction is a delete plus a
//! new row, which keeps the audit trail complete (§6.5) and keeps this repo
//! to four statements.
//!
//! Rows are addressed by [`ProbeIdentity`], the same three-way encoding
//! `sensor_group_members` uses, so an event follows its probe between
//! groups without anything having to move it — it was never attached to a
//! group.

use rusqlite::{params, OptionalExtension, Row};

use crate::error::Result;
use crate::models::{NewSensorEvent, ProbeIdentity, SensorEvent};
use crate::Storage;

const EVENT_COLUMNS: &str =
    "id, hostname, sensor_line, node_key, mac, ts, text, author, author_user_id, created_at";

fn event_from_row(r: &Row<'_>) -> rusqlite::Result<SensorEvent> {
    let line: Option<i64> = r.get(2)?;
    let node_key: Option<String> = r.get(3)?;
    let mac: Option<String> = r.get(4)?;
    // The V12 CHECK guarantees exactly one is non-NULL; the fallback is
    // unreachable short of a hand-edited database, and answering `Line(-1)`
    // there beats panicking inside a query_map.
    let probe = match (line, node_key, mac) {
        (Some(line), _, _) => ProbeIdentity::Line(line),
        (_, Some(key), _) => ProbeIdentity::Node(key),
        (_, _, Some(mac)) => ProbeIdentity::Mac(mac),
        _ => ProbeIdentity::Line(-1),
    };
    Ok(SensorEvent {
        id: r.get(0)?,
        hostname: r.get(1)?,
        probe,
        ts: r.get(5)?,
        text: r.get(6)?,
        author: r.get(7)?,
        author_user_id: r.get(8)?,
        created_at: r.get(9)?,
    })
}

impl Storage {
    /// Writes one manual event and returns its id.
    ///
    /// `created_at` is stamped here rather than taken from the caller: `ts`
    /// is the operator's chosen (possibly backdated) event time, and the
    /// two must not be settable from the same place, or a backdated note
    /// would also backdate the record of writing it.
    ///
    /// An over-long `text` is refused by V12's CHECK as a
    /// [`StorageError`](crate::StorageError) rather than truncated — the
    /// route validates first, and a POST that got past it is a bug that
    /// should be loud.
    pub fn insert_sensor_event(&self, event: &NewSensorEvent) -> Result<i64> {
        let (line, node_key, mac) = event.probe.columns();
        self.conn.execute(
            "INSERT INTO sensor_events
                (hostname, sensor_line, node_key, mac, ts, text, author, author_user_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                event.hostname,
                line,
                node_key,
                mac,
                event.ts,
                event.text,
                event.author,
                event.author_user_id,
                now_unix(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// One probe's events in `[from_ts, to_ts)`, oldest first.
    ///
    /// Half-open like every other window in this crate, and ordered
    /// ascending because the window's event list reads as a timeline.
    /// Scoped to one probe *and* one Hub: a `sensor_line` is only unique
    /// under its hostname.
    pub fn list_sensor_events(
        &self,
        hostname: &str,
        probe: &ProbeIdentity,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<SensorEvent>> {
        let (line, node_key, mac) = probe.columns();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM sensor_events
             WHERE hostname = ?1
               AND COALESCE(sensor_line, -1) = COALESCE(?2, -1)
               AND COALESCE(node_key, '')   = COALESCE(?3, '')
               AND COALESCE(mac, '')        = COALESCE(?4, '')
               AND ts >= ?5 AND ts < ?6
             ORDER BY ts ASC, id ASC"
        ))?;
        let rows = stmt.query_map(
            params![hostname, line, node_key, mac, from_ts, to_ts],
            event_from_row,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// One event by id, or `None`. The delete route reads it first so the
    /// audit row can name the probe the event belonged to.
    pub fn get_sensor_event(&self, id: i64) -> Result<Option<SensorEvent>> {
        self.conn
            .query_row(
                &format!("SELECT {EVENT_COLUMNS} FROM sensor_events WHERE id = ?1"),
                params![id],
                event_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Deletes one event **of one probe on one Hub**. `false` means there
    /// was no such row — which the route audits as an unsuccessful delete
    /// rather than reporting as an error, the same stance
    /// `delete_sensor_group` takes.
    ///
    /// The scope is in the statement, not only in the caller: an `id` is a
    /// global handle, and `sensor_line` 1 exists on every Hub, so a delete
    /// that trusted the id alone would let one probe's window remove
    /// another's note (review item 1, 2026-09-10). Same three-column
    /// `COALESCE` shape as [`Self::list_sensor_events`].
    pub fn delete_sensor_event(
        &self,
        hostname: &str,
        probe: &ProbeIdentity,
        id: i64,
    ) -> Result<bool> {
        let (line, node_key, mac) = probe.columns();
        let n = self.conn.execute(
            "DELETE FROM sensor_events
              WHERE id = ?1
                AND hostname = ?2
                AND COALESCE(sensor_line, -1) = COALESCE(?3, -1)
                AND COALESCE(node_key, '')   = COALESCE(?4, '')
                AND COALESCE(mac, '')        = COALESCE(?5, '')",
            params![id, hostname, line, node_key, mac],
        )?;
        Ok(n > 0)
    }
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
