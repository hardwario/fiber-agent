//! Node (STICKER) persistence: `node_field_readings`,
//! `node_counter_readings`, `node_events`, `node_raw_messages`.
//!
//! All reading/event inserts are `INSERT OR IGNORE` on the natural key —
//! history replay frames (mqtt.md `lorawan/sensors/{dev_eui}/history`)
//! re-insert the same rows harmlessly, and the insert-count return values
//! let the backfill layer credit only genuinely new points.
//!
//! Time ranges are `[from_ts, to_ts)` (inclusive lower, exclusive upper),
//! matching `history_repo`.

use std::collections::HashSet;

use rusqlite::params;

use crate::error::Result;
use crate::models::{NodeCounterReading, NodeEvent, NodeFieldReading, NodeRawMessage};
use crate::Storage;

/// Raw-uplink ring buffer depth per Node (mqtt.md: last 10 per Node).
const RAW_MESSAGES_KEEP: i64 = 10;

impl Storage {
    // --- node_field_readings -------------------------------------------

    /// Insert one field reading; returns `false` when the natural key
    /// `(hostname, node_key, field, ts)` already exists (duplicate).
    pub fn insert_field_reading(&self, row: &NodeFieldReading) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO node_field_readings
                (hostname, node_key, field, ts, value, alarm_state, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.hostname,
                row.node_key,
                row.field,
                row.ts,
                row.value,
                row.alarm_state,
                row.source,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Insert a batch of field readings in one transaction; returns the
    /// number of rows actually inserted (duplicates ignored) — the
    /// `points_inserted` half of `sticker_history_progress`.
    pub fn insert_field_readings(&self, rows: &[NodeFieldReading]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut inserted = 0usize;
        {
            // A switched-off sensor is not stored, whichever path its value came by: live,
            // history replay or v1 (sensor_disabled, V33, GitLab #120).
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO node_field_readings
                    (hostname, node_key, field, ts, value, alarm_state, source)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
                 WHERE NOT EXISTS (
                     SELECT 1 FROM sensor_disabled
                     WHERE hostname = ?1 AND scope = 'node' AND entity = ?2 AND field = ?3)",
            )?;
            for row in rows {
                inserted += stmt.execute(params![
                    row.hostname,
                    row.node_key,
                    row.field,
                    row.ts,
                    row.value,
                    row.alarm_state,
                    row.source,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Field readings for `[from_ts, to_ts)`, ordered by `ts` ASC.
    pub fn query_field_readings(
        &self,
        hostname: &str,
        node_key: &str,
        field: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<NodeFieldReading>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, node_key, field, ts, value, alarm_state, source
             FROM node_field_readings
             WHERE hostname = ?1 AND node_key = ?2 AND field = ?3
               AND ts >= ?4 AND ts < ?5
             ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(
            params![hostname, node_key, field, from_ts, to_ts],
            field_reading_from_row,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// `(field, ts)` of every field reading this Node stored in `[from_ts, to_ts)`. The history
    /// sink reads it to leave out a replayed record a live reading already covers
    /// (devices.node.md §3.9, amendment 2026-09-25, item 5).
    pub fn field_reading_times(
        &self,
        hostname: &str,
        node_key: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<(String, i64)>> {
        self.reading_times(
            "node_field_readings",
            "field",
            hostname,
            node_key,
            from_ts,
            to_ts,
        )
    }

    /// The same for counters.
    pub fn counter_reading_times(
        &self,
        hostname: &str,
        node_key: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<(String, i64)>> {
        self.reading_times(
            "node_counter_readings",
            "counter",
            hostname,
            node_key,
            from_ts,
            to_ts,
        )
    }

    fn reading_times(
        &self,
        table: &str,
        name_column: &str,
        hostname: &str,
        node_key: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {name_column}, ts FROM {table}
             WHERE hostname = ?1 AND node_key = ?2 AND ts >= ?3 AND ts < ?4"
        ))?;
        let rows = stmt.query_map(params![hostname, node_key, from_ts, to_ts], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every distinct second at which this Node stored *any* field reading,
    /// ascending — "when did this Node report", independent of which field it
    /// reports.
    ///
    /// The gap detector (devices.backend.md §6, B3) used to stand `temperature`
    /// in for this, which answered "no gaps, ever" for a Node that reports only
    /// humidity, or CO₂, or anything else (§19, F5). Distinct timestamps rather
    /// than a per-field scan because a gap is a property of the Node's silence,
    /// not of one column: a Node reporting two fields at different cadences has
    /// a hole only where *both* stop.
    pub fn node_reading_timestamps(
        &self,
        hostname: &str,
        node_key: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT ts
             FROM node_field_readings
             WHERE hostname = ?1 AND node_key = ?2
               AND ts >= ?3 AND ts < ?4
             ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![hostname, node_key, from_ts, to_ts], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The Node keys on one Hub that have **any** stored field reading.
    ///
    /// One grouped existence query for the whole fleet, never one per row —
    /// §4's performance gate, the same reason `count_pending_by_node`
    /// exists. `render::waiting_for_join` needs it because
    /// `NodeState::last_seen` is in-memory only: after a restart the store
    /// has forgotten every uplink it ever saw, and this table has not
    /// (§21 review, item 8).
    ///
    /// **A loose index scan, not a `DISTINCT`** (§21 review round 2, item 1).
    /// The caller only ever asks "any?", so the cost has to scale with the
    /// number of *Nodes*, not with the number of readings they have ever
    /// stored — this runs on every `/nodes` render and on every Node SSE
    /// event, against a table that grows forever until retention trims it.
    ///
    /// `SELECT DISTINCT node_key … WHERE hostname = ?` reads like an index
    /// lookup and is not one:
    ///
    /// ```text
    /// SEARCH node_field_readings USING COVERING INDEX
    ///        sqlite_autoindex_node_field_readings_1 (hostname=?)
    /// ```
    ///
    /// One range over *every index entry for the Hub* — a million readings
    /// across ten Nodes is a million entries visited to answer ten
    /// questions. SQLite has no loose-index-scan planner, so the skip is
    /// written out by hand: seek the smallest `node_key`, then repeatedly
    /// seek the smallest one strictly greater. The primary key is
    /// `(hostname, node_key, field, ts)`, so each step is one `min()` seek
    /// on that index prefix:
    ///
    /// ```text
    /// CO-ROUTINE n
    ///   SETUP
    ///     SEARCH node_field_readings USING COVERING INDEX … (hostname=?)
    ///   RECURSIVE STEP
    ///     SCAN n
    ///     CORRELATED SCALAR SUBQUERY 2
    ///       SEARCH node_field_readings USING COVERING INDEX …
    ///              (hostname=? AND node_key>?)
    /// ```
    ///
    /// O(nodes · log rows) instead of O(rows). The alternative — a cached
    /// set per hostname invalidated on insert — was not taken: readings
    /// arrive on every telemetry publish and the cache would be invalidated
    /// roughly as often as it is read, for a correctness risk this has none
    /// of.
    ///
    /// The trailing `WHERE k IS NOT NULL` drops the terminator row: the
    /// recursion ends by producing one `NULL` (no key is greater than the
    /// last), which is also what an empty table yields on the first step.
    pub fn nodes_with_readings(&self, hostname: &str) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE n(k) AS (
                 SELECT min(node_key) FROM node_field_readings
                 WHERE hostname = ?1
                 UNION ALL
                 SELECT (SELECT min(node_key) FROM node_field_readings
                         WHERE hostname = ?1 AND node_key > n.k)
                 FROM n WHERE n.k IS NOT NULL
             )
             SELECT k FROM n WHERE k IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![hostname], |r| r.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// The fields this one Node has any stored reading for — the detail
    /// page's half of [`Self::nodes_with_readings`], and the same loose index
    /// scan one level down the primary key: one `min(field)` seek per field,
    /// not a walk over every reading. Empty = the Node has never reported.
    pub fn node_fields_with_readings(
        &self,
        hostname: &str,
        node_key: &str,
    ) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE f(k) AS (
                 SELECT min(field) FROM node_field_readings
                 WHERE hostname = ?1 AND node_key = ?2
                 UNION ALL
                 SELECT (SELECT min(field) FROM node_field_readings
                         WHERE hostname = ?1 AND node_key = ?2 AND field > f.k)
                 FROM f WHERE f.k IS NOT NULL
             )
             SELECT k FROM f WHERE k IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![hostname, node_key], |r| r.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Most recent reading for a field, or `None`.
    pub fn latest_field_reading(
        &self,
        hostname: &str,
        node_key: &str,
        field: &str,
    ) -> Result<Option<NodeFieldReading>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, node_key, field, ts, value, alarm_state, source
             FROM node_field_readings
             WHERE hostname = ?1 AND node_key = ?2 AND field = ?3
             ORDER BY ts DESC
             LIMIT 1",
        )?;
        let mut rows =
            stmt.query_map(params![hostname, node_key, field], field_reading_from_row)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Each field's most recent value for one Node, the in-memory `fields` map a restart lost:
    /// the portal refills it from here, so the detail page shows the last value rather than
    /// a dash until the next uplink (N14, combined23). Rows without a value are skipped, and
    /// so is a field the Node sent as `null` since ([`Self::nulled_node_fields`]): its last
    /// real value is not current.
    pub fn latest_node_fields(&self, hostname: &str, node_key: &str) -> Result<Vec<(String, f64)>> {
        let sql = format!(
            "SELECT r.field, r.value
             FROM node_field_readings r
             WHERE r.hostname = ?1 AND r.node_key = ?2 AND r.value IS NOT NULL
               AND r.ts = (SELECT MAX(l.ts) FROM node_field_readings l
                           WHERE l.hostname = r.hostname AND l.node_key = r.node_key
                             AND l.field = r.field AND l.value IS NOT NULL)
               AND NOT EXISTS ({NULLED_SINCE})
             GROUP BY r.field",
            NULLED_SINCE = nulled_since("r.field"),
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![hostname, node_key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// When each field and count of one Node was last stored, `(name, MAX(ts))`: the
    /// "Received" column of the Channels card (ui_components.md §2.6). Read-only; rows
    /// without a value are skipped, as in [`Self::latest_node_fields`]. A name stored both
    /// as a reading and as a count appears twice; the caller keeps the newer.
    pub fn latest_node_field_times(
        &self,
        hostname: &str,
        node_key: &str,
    ) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT field, MAX(ts) FROM node_field_readings
             WHERE hostname = ?1 AND node_key = ?2 AND value IS NOT NULL
             GROUP BY field
             UNION ALL
             SELECT counter, MAX(ts) FROM node_counter_readings
             WHERE hostname = ?1 AND node_key = ?2 AND value IS NOT NULL
             GROUP BY counter",
        )?;
        let rows = stmt.query_map(params![hostname, node_key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// When the newest raw uplink of one Node was received (the `node_raw_messages`
    /// ring), `None` without one: the Radio rows' time on the Channels card.
    pub fn latest_raw_message_at(&self, hostname: &str, node_key: &str) -> Result<Option<i64>> {
        let at = self.conn.query_row(
            "SELECT MAX(received_at) FROM node_raw_messages
             WHERE hostname = ?1 AND node_key = ?2",
            params![hostname, node_key],
            |r| r.get::<_, Option<i64>>(0),
        )?;
        Ok(at)
    }

    /// The quantities whose newest report was `null` (sticker-firmware v2.0.0: the sensor is
    /// on and had no valid sample): a `no_data` event naming the field, stored no earlier than
    /// the field's newest value or count. The restart refill marks them so, instead of bringing
    /// back the value that `null` replaced. No reading row is stored for a `null`; the event is
    /// the record of it.
    pub fn nulled_node_fields(&self, hostname: &str, node_key: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.field FROM (
                 SELECT json_extract(payload, '$.field') AS field, MAX(ts) AS ts
                 FROM node_events
                 WHERE hostname = ?1 AND node_key = ?2 AND event_type = 'no_data'
                 GROUP BY 1
             ) e
             WHERE e.field IS NOT NULL
               AND e.ts >= COALESCE((SELECT MAX(ts) FROM node_field_readings
                                     WHERE hostname = ?1 AND node_key = ?2
                                       AND field = e.field AND value IS NOT NULL), 0)
               AND e.ts >= COALESCE((SELECT MAX(ts) FROM node_counter_readings
                                     WHERE hostname = ?1 AND node_key = ?2
                                       AND counter = e.field AND value IS NOT NULL), 0)
             ORDER BY e.field",
        )?;
        let rows = stmt.query_map(params![hostname, node_key], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Each sensor count's most recent value for one Node (`*_count`: `motion_count`,
    /// `input_b_count`, ...), the twin of [`Self::latest_node_fields`] for the in-memory
    /// `counters` map (GitLab #134). Frame counters are left out: they say nothing after a
    /// restart. Rows without a value are skipped, and so is a count the Node sent as `null`
    /// since ([`Self::nulled_node_fields`]).
    pub fn latest_node_counters(
        &self,
        hostname: &str,
        node_key: &str,
    ) -> Result<Vec<(String, i64)>> {
        let sql = format!(
            "SELECT r.counter, r.value
             FROM node_counter_readings r
             WHERE r.hostname = ?1 AND r.node_key = ?2 AND r.value IS NOT NULL
               AND substr(r.counter, -6) = '_count'
               AND r.ts = (SELECT MAX(l.ts) FROM node_counter_readings l
                           WHERE l.hostname = r.hostname AND l.node_key = r.node_key
                             AND l.counter = r.counter AND l.value IS NOT NULL)
               AND NOT EXISTS ({NULLED_SINCE})
             GROUP BY r.counter",
            NULLED_SINCE = nulled_since("r.counter"),
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![hostname, node_key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// `(MIN(ts), MAX(ts))` for a field, or `None` if it has no rows —
    /// bounds zoom/backfill range pickers.
    pub fn field_extent(
        &self,
        hostname: &str,
        node_key: &str,
        field: &str,
    ) -> Result<Option<(i64, i64)>> {
        let extent = self.conn.query_row(
            "SELECT MIN(ts), MAX(ts)
             FROM node_field_readings
             WHERE hostname = ?1 AND node_key = ?2 AND field = ?3",
            params![hostname, node_key, field],
            |r| {
                let min_ts: Option<i64> = r.get(0)?;
                let max_ts: Option<i64> = r.get(1)?;
                Ok(min_ts.zip(max_ts))
            },
        )?;
        Ok(extent)
    }

    // --- node_counter_readings -----------------------------------------

    /// Insert one counter reading; `false` = duplicate natural key.
    pub fn insert_counter_reading(&self, row: &NodeCounterReading) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO node_counter_readings
                (hostname, node_key, counter, ts, value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.hostname, row.node_key, row.counter, row.ts, row.value],
        )?;
        Ok(changed > 0)
    }

    /// Batch counter insert in one transaction; returns rows actually
    /// inserted (duplicates ignored).
    pub fn insert_counter_readings(&self, rows: &[NodeCounterReading]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO node_counter_readings
                    (hostname, node_key, counter, ts, value)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for row in rows {
                inserted += stmt.execute(params![
                    row.hostname,
                    row.node_key,
                    row.counter,
                    row.ts,
                    row.value,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Counter readings for `[from_ts, to_ts)`, ordered by `ts` ASC.
    pub fn query_counters(
        &self,
        hostname: &str,
        node_key: &str,
        counter: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<NodeCounterReading>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, node_key, counter, ts, value
             FROM node_counter_readings
             WHERE hostname = ?1 AND node_key = ?2 AND counter = ?3
               AND ts >= ?4 AND ts < ?5
             ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![hostname, node_key, counter, from_ts, to_ts], |r| {
            Ok(NodeCounterReading {
                hostname: r.get(0)?,
                node_key: r.get(1)?,
                counter: r.get(2)?,
                ts: r.get(3)?,
                value: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // --- node_events -----------------------------------------------------

    /// Insert one event; `false` = duplicate natural key
    /// `(hostname, node_key, event_type, ts, seq)` (idempotent re-delivery).
    pub fn insert_event(&self, row: &NodeEvent) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO node_events
                (hostname, node_key, event_type, ts, seq, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.hostname,
                row.node_key,
                row.event_type,
                row.ts,
                row.seq,
                row.payload
            ],
        )?;
        Ok(changed > 0)
    }

    /// Events for `[from_ts, to_ts)`, newest first, optionally filtered by
    /// `event_type`. `limit` is clamped to `1..=1000` (default 100 when
    /// `None`).
    ///
    /// Within one `ts`, the higher `seq` first: the events of one message share its `ts`,
    /// and the one later in the message (on a later page) is the newer, so "Last alarm" is the
    /// message's last alarm and not whichever row SQLite happened to return first.
    pub fn query_events(
        &self,
        hostname: &str,
        node_key: &str,
        event_type: Option<&str>,
        from_ts: i64,
        to_ts: i64,
        limit: Option<i64>,
    ) -> Result<Vec<NodeEvent>> {
        let limit = limit.unwrap_or(100).clamp(1, 1000);
        let mut stmt = self.conn.prepare(
            "SELECT hostname, node_key, event_type, ts, seq, payload
             FROM node_events
             WHERE hostname = ?1 AND node_key = ?2
               AND (?3 IS NULL OR event_type = ?3)
               AND ts >= ?4 AND ts < ?5
             ORDER BY ts DESC, seq DESC
             LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            params![hostname, node_key, event_type, from_ts, to_ts, limit],
            |r| {
                Ok(NodeEvent {
                    hostname: r.get(0)?,
                    node_key: r.get(1)?,
                    event_type: r.get(2)?,
                    ts: r.get(3)?,
                    seq: r.get(4)?,
                    payload: r.get(5)?,
                })
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // --- node_raw_messages ------------------------------------------------

    /// Append a raw uplink payload and trim the ring buffer to the newest
    /// 10 rows **per port** for this `(hostname, node_key)`; returns the new row's id.
    ///
    /// The port is the payload's own `fport` (2 telemetry, 3 alarm batch, 85 the command
    /// engine — every fPort-85 frame is kept too). A payload that names none, as every row
    /// written before per-port rings did, is its own group. Per port, not per node: a Node
    /// streaming a 14-page config read must not push its last telemetry out of the ring. The
    /// grouping is read from the payload rather than a column, so no migration is needed. A
    /// Node keeps a few dozen rows at most, so reading them back to trim is cheap.
    pub fn insert_raw_message(
        &self,
        hostname: &str,
        node_key: &str,
        received_at: i64,
        payload: &str,
    ) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO node_raw_messages (hostname, node_key, received_at, payload)
             VALUES (?1, ?2, ?3, ?4)",
            params![hostname, node_key, received_at, payload],
        )?;
        let id = tx.last_insert_rowid();
        let stale: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id, payload FROM node_raw_messages
                 WHERE hostname = ?1 AND node_key = ?2
                 ORDER BY id DESC",
            )?;
            let rows = stmt.query_map(params![hostname, node_key], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut kept: std::collections::HashMap<Option<i64>, i64> =
                std::collections::HashMap::new();
            let mut stale = Vec::new();
            for row in rows {
                let (row_id, payload) = row?;
                let port = raw_message_port(&payload);
                let n = kept.entry(port).or_insert(0);
                *n += 1;
                if *n > RAW_MESSAGES_KEEP {
                    stale.push(row_id);
                }
            }
            stale
        };
        for row_id in stale {
            tx.execute(
                "DELETE FROM node_raw_messages WHERE id = ?1",
                params![row_id],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Newest-first raw messages for a Node, all ports together. `limit` is clamped to
    /// `1..=50` (default 10 when `None`); pass 50 for every port's ring.
    pub fn list_raw_messages(
        &self,
        hostname: &str,
        node_key: &str,
        limit: Option<i64>,
    ) -> Result<Vec<NodeRawMessage>> {
        let limit = limit.unwrap_or(RAW_MESSAGES_KEEP).clamp(1, 50);
        let mut stmt = self.conn.prepare(
            "SELECT id, hostname, node_key, received_at, payload
             FROM node_raw_messages
             WHERE hostname = ?1 AND node_key = ?2
             ORDER BY id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![hostname, node_key, limit], |r| {
            Ok(NodeRawMessage {
                id: r.get(0)?,
                hostname: r.get(1)?,
                node_key: r.get(2)?,
                received_at: r.get(3)?,
                payload: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// The ring group of a raw message: its payload's `fport`, or `None` for one that names none.
pub fn raw_message_port(payload: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()?
        .get("fport")?
        .as_i64()
}

/// The `NOT EXISTS` body that drops a value the Node replaced with `null` since: a `no_data`
/// event (control-radio `decode::reading`) naming `field_col`, stored at or after the row `r`.
/// One report cannot carry a value and a `null` for the same quantity, so a tie is the `null`.
fn nulled_since(field_col: &str) -> String {
    format!(
        "SELECT 1 FROM node_events e
         WHERE e.hostname = r.hostname AND e.node_key = r.node_key
           AND e.event_type = 'no_data' AND e.ts >= r.ts
           AND json_extract(e.payload, '$.field') = {field_col}"
    )
}

fn field_reading_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NodeFieldReading> {
    Ok(NodeFieldReading {
        hostname: r.get(0)?,
        node_key: r.get(1)?,
        field: r.get(2)?,
        ts: r.get(3)?,
        value: r.get(4)?,
        alarm_state: r.get(5)?,
        source: r.get(6)?,
    })
}
