//! `alarm_events` + `alarm_notes` repositories.
//!
//! M1 wrote raise-only rows; M6-B adds the lifecycle the alarm engine and
//! REST layer need: in-place severity/state updates, clear, acknowledge,
//! active-alarm lookups, the filtered listing behind rest.md
//! `GET /api/alarms`, and per-alarm notes (`AlarmNoteDB` — acknowledge
//! reasons are stored as notes too).

use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::error::{is_unique_violation, Result, StorageError};
use crate::models::{AlarmEvent, AlarmListFilter, AlarmNote, AlarmSort, NewAlarmEvent};
use crate::Storage;

const ALARM_COLUMNS: &str = "id, hostname, sensor_id, sensor_name, sensor_location, alarm_type, \
                             severity, from_state, to_state, message, value, threshold, \
                             triggered_at, cleared_at, acknowledged_at, acknowledged_by_user_id, \
                             acknowledged_by_username, native_key, created_at, \
                             COALESCE(max_severity, severity), end_reason";

fn row_to_alarm(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlarmEvent> {
    Ok(AlarmEvent {
        id: r.get(0)?,
        hostname: r.get(1)?,
        sensor_id: r.get(2)?,
        sensor_name: r.get(3)?,
        sensor_location: r.get(4)?,
        alarm_type: r.get(5)?,
        severity: r.get(6)?,
        from_state: r.get(7)?,
        to_state: r.get(8)?,
        message: r.get(9)?,
        value: r.get(10)?,
        threshold: r.get(11)?,
        triggered_at: r.get(12)?,
        cleared_at: r.get(13)?,
        acknowledged_at: r.get(14)?,
        acknowledged_by_user_id: r.get(15)?,
        acknowledged_by_username: r.get(16)?,
        native_key: r.get(17)?,
        created_at: r.get(18)?,
        max_severity: r.get(19)?,
        end_reason: r.get(20)?,
    })
}

fn row_to_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlarmNote> {
    Ok(AlarmNote {
        id: r.get(0)?,
        alarm_id: r.get(1)?,
        user_id: r.get(2)?,
        username: r.get(3)?,
        content: r.get(4)?,
        created_at: r.get(5)?,
    })
}

/// What [`Storage::count_summary_by_hub`] answers: the open alarms of one Hub, split
/// the way the `/alarms` metric row shows them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlarmCounts {
    pub active: i64,
    pub critical: i64,
    pub warning: i64,
    pub unacknowledged: i64,
}

/// Arguments of [`Storage::update_alarm_severity`], grouped because the
/// three `&str` fields are trivially swappable positionally. `to_state` is
/// always `Some` from the alarm engine; `None` nulls the column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlarmSeverityChange<'a> {
    pub severity: &'a str,
    pub alarm_type: &'a str,
    pub to_state: Option<&'a str>,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub message: Option<&'a str>,
    /// Resets the acknowledgement (who and when; the notes stay): the row
    /// escalated to critical, which nobody acknowledged (decisions.md #28c).
    pub clear_acknowledgement: bool,
}

impl Storage {
    /// Insert a new alarm event with a generated uuid v4 id; returns the id.
    /// A duplicate `native_key` (fPort-3 re-delivery) returns
    /// [`StorageError::Conflict`] instead of a second row.
    pub fn insert_alarm_event(&self, event: &NewAlarmEvent) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO alarm_events
                    (id, hostname, sensor_id, sensor_name, sensor_location, alarm_type,
                     severity, from_state, to_state, message, value, threshold,
                     triggered_at, cleared_at, acknowledged_at, acknowledged_by_user_id,
                     acknowledged_by_username, native_key, max_severity)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16, ?17, ?18, ?7)",
                params![
                    id,
                    event.hostname,
                    event.sensor_id,
                    event.sensor_name,
                    event.sensor_location,
                    event.alarm_type,
                    event.severity,
                    event.from_state,
                    event.to_state,
                    event.message,
                    event.value,
                    event.threshold,
                    event.triggered_at,
                    event.cleared_at,
                    event.acknowledged_at,
                    event.acknowledged_by_user_id,
                    event.acknowledged_by_username,
                    event.native_key,
                ],
            )
            .map_err(|e| {
                if is_unique_violation(&e) {
                    StorageError::Conflict(format!(
                        "alarm with native_key '{}' already exists",
                        event.native_key.as_deref().unwrap_or("")
                    ))
                } else {
                    e.into()
                }
            })?;
        Ok(id)
    }

    /// One alarm by id, or `None`.
    pub fn get_alarm(&self, id: &str) -> Result<Option<AlarmEvent>> {
        self.conn
            .query_row(
                &format!("SELECT {ALARM_COLUMNS} FROM alarm_events WHERE id = ?1"),
                params![id],
                row_to_alarm,
            )
            .optional()
            .map_err(Into::into)
    }

    /// In-place escalation/de-escalation of an open alarm. Writes the whole
    /// re-derived classification, not just the severity: `alarm_type` and
    /// `threshold` belong to the level the alarm now sits at, and the
    /// previous `to_state` moves into `from_state` (same convention as
    /// [`Storage::update_alarm_state`]). `clear_acknowledgement` resets who
    /// acknowledged it and when (decisions.md #28c); the notes stay. Returns
    /// `false` when no such row.
    pub fn update_alarm_severity(&self, id: &str, change: AlarmSeverityChange<'_>) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE alarm_events
             SET severity = ?2, alarm_type = ?3, from_state = to_state, to_state = ?4,
                 value = ?5, threshold = ?6, message = ?7,
                 max_severity = CASE
                     WHEN ?2 = 'critical' THEN 'critical'
                     ELSE COALESCE(max_severity, severity)
                 END,
                 acknowledged_at = CASE WHEN ?8 THEN NULL ELSE acknowledged_at END,
                 acknowledged_by_user_id =
                     CASE WHEN ?8 THEN NULL ELSE acknowledged_by_user_id END,
                 acknowledged_by_username =
                     CASE WHEN ?8 THEN NULL ELSE acknowledged_by_username END
             WHERE id = ?1",
            params![
                id,
                change.severity,
                change.alarm_type,
                change.to_state,
                change.value,
                change.threshold,
                change.message,
                change.clear_acknowledgement,
            ],
        )?;
        Ok(n > 0)
    }

    /// In-place state transition (e.g. DISCONNECTED↔RECONNECTING folding):
    /// the previous `to_state` becomes `from_state` so the row records the
    /// latest transition. Returns `false` when no such row.
    pub fn update_alarm_state(
        &self,
        id: &str,
        to_state: &str,
        message: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE alarm_events
             SET from_state = to_state, to_state = ?2, message = ?3
             WHERE id = ?1",
            params![id, to_state, message],
        )?;
        Ok(n > 0)
    }

    /// Mark an alarm cleared. Returns `false` when no such row. A `Some`
    /// `message` replaces the row's message (it says why the row closed, see
    /// `portal_domain::alarm_engine::on_wired_snapshot`); `None` keeps it.
    pub fn clear_alarm(&self, id: &str, cleared_at: &str, message: Option<&str>) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE alarm_events SET cleared_at = ?2, message = COALESCE(?3, message)
             WHERE id = ?1",
            params![id, cleared_at, message],
        )?;
        Ok(n > 0)
    }

    /// Acknowledge an alarm (idempotent — a re-ack overwrites the stamp) and
    /// return the updated row, or `None` when no such alarm. The ack reason
    /// note is the caller's job ([`Storage::insert_alarm_note`]).
    /// Closes one open alarm that nothing measured the end of, recording why
    /// (`end_reason`: `device_removed`, `hub_removed`, `rule_removed`).
    /// `false` when the row is gone or already closed, so a repeat is a no-op.
    pub fn close_alarm_with_reason(
        &self,
        id: &str,
        cleared_at: &str,
        end_reason: &str,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE alarm_events SET cleared_at = ?2, end_reason = ?3
             WHERE id = ?1 AND cleared_at IS NULL",
            params![id, cleared_at, end_reason],
        )?;
        Ok(n > 0)
    }

    /// Moves one open alarm to another Hub hostname (a renamed Hub), so the
    /// readings arriving under the new name evaluate it again.
    pub fn rekey_alarm(&self, id: &str, hostname: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE alarm_events SET hostname = ?2 WHERE id = ?1 AND cleared_at IS NULL",
            params![id, hostname],
        )?;
        Ok(n > 0)
    }

    /// Every open alarm on every Hub — the orphan sweep's input.
    pub fn all_active_alarms(&self) -> Result<Vec<AlarmEvent>> {
        let sql = format!(
            "SELECT {ALARM_COLUMNS} FROM alarm_events WHERE cleared_at IS NULL \
             ORDER BY triggered_at"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], row_to_alarm)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn acknowledge_alarm(
        &self,
        id: &str,
        user_id: Option<i64>,
        username: &str,
        at: &str,
    ) -> Result<Option<AlarmEvent>> {
        self.conn
            .query_row(
                &format!(
                    // HS-12 (Hub sweep 2026-09-26): a second acknowledge keeps the first
                    // acknowledger and time; it only adds its note.
                    "UPDATE alarm_events
                     SET acknowledged_at = COALESCE(acknowledged_at, ?2),
                         acknowledged_by_user_id = CASE WHEN acknowledged_at IS NULL
                             THEN ?3 ELSE acknowledged_by_user_id END,
                         acknowledged_by_username = CASE WHEN acknowledged_at IS NULL
                             THEN ?4 ELSE acknowledged_by_username END
                     WHERE id = ?1
                     RETURNING {ALARM_COLUMNS}"
                ),
                params![id, at, user_id, username],
                row_to_alarm,
            )
            .optional()
            .map_err(Into::into)
    }

    /// The open (`cleared_at IS NULL`) alarm for one sensor, or `None`. The
    /// engine keeps at most one open row per `(hostname, sensor_id)`; if
    /// history ever holds more, the newest `triggered_at` wins.
    pub fn active_alarm(&self, hostname: &str, sensor_id: &str) -> Result<Option<AlarmEvent>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {ALARM_COLUMNS} FROM alarm_events
                     WHERE hostname = ?1 AND sensor_id = ?2 AND cleared_at IS NULL
                     ORDER BY triggered_at DESC, created_at DESC
                     LIMIT 1"
                ),
                params![hostname, sensor_id],
                row_to_alarm,
            )
            .optional()
            .map_err(Into::into)
    }

    /// All open alarms for a hostname, newest `triggered_at` first.
    pub fn active_alarms(&self, hostname: &str) -> Result<Vec<AlarmEvent>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ALARM_COLUMNS} FROM alarm_events
             WHERE hostname = ?1 AND cleared_at IS NULL
             ORDER BY triggered_at DESC, created_at DESC"
        ))?;
        let rows = stmt.query_map(params![hostname], row_to_alarm)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// All open alarms for a hostname in any letter case, newest `triggered_at` first.
    ///
    /// For a caller that can run before the portal knows the Hub's display spelling: the
    /// retained documents a Hub replays on connect can arrive ahead of its first
    /// agent-spelled topic, and an exact match then finds none of the rows written under
    /// that spelling. Each row carries its own `hostname` for the follow-up writes.
    pub fn active_alarms_any_case(&self, hostname: &str) -> Result<Vec<AlarmEvent>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ALARM_COLUMNS} FROM alarm_events
             WHERE hostname = ?1 COLLATE NOCASE AND cleared_at IS NULL
             ORDER BY triggered_at DESC, created_at DESC"
        ))?;
        let rows = stmt.query_map(params![hostname], row_to_alarm)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Number of open alarms for a hostname.
    /// Open alarms on one Hub, or on every Hub when `hostname` is `None`
    /// (devices.hub.md §12 E: the sidebar badge and the unfiltered `/alarms`
    /// row count the whole portal, not the first Hub in the store).
    pub fn count_active(&self, hostname: Option<&str>) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM alarm_events
                 WHERE (?1 IS NULL OR hostname = ?1) AND cleared_at IS NULL",
                params![hostname],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// The four numbers the `/alarms` metric row shows, per Hub, in one pass:
    /// open alarms, of those the critical and the warning ones, and how many
    /// nobody has acknowledged. Per Hub so the caller can leave out the Hubs
    /// the live registry no longer knows (decisions.md #30d). `None` counts
    /// every Hub; a Hub with no open alarm has no entry.
    pub fn count_summary_by_hub(
        &self,
        hostname: Option<&str>,
    ) -> Result<Vec<(String, AlarmCounts)>> {
        let mut stmt = self.conn.prepare(
            "SELECT
               hostname,
               COUNT(*),
               SUM(CASE WHEN COALESCE(max_severity, severity) = 'critical' THEN 1 ELSE 0 END),
               SUM(CASE WHEN COALESCE(max_severity, severity) = 'warning' THEN 1 ELSE 0 END),
               SUM(CASE WHEN acknowledged_at IS NULL THEN 1 ELSE 0 END)
             FROM alarm_events
             WHERE (?1 IS NULL OR hostname = ?1) AND cleared_at IS NULL
             GROUP BY hostname",
        )?;
        let rows = stmt.query_map(params![hostname], |r| {
            Ok((
                r.get(0)?,
                AlarmCounts {
                    active: r.get(1)?,
                    critical: r.get(2)?,
                    warning: r.get(3)?,
                    unacknowledged: r.get(4)?,
                },
            ))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Filtered, paginated alarm listing (rest.md `GET /api/alarms`), newest
    /// `triggered_at` first. Returns `(page, total)` where `total` counts
    /// matching rows *before* limit/offset. `filter.limit` is clamped to
    /// `1..=1000`.
    pub fn list_alarms(&self, filter: &AlarmListFilter) -> Result<(Vec<AlarmEvent>, i64)> {
        let search_sql: String;
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if !filter.include_acknowledged {
            clauses.push("acknowledged_at IS NULL");
        }
        if !filter.include_cleared {
            clauses.push("cleared_at IS NULL");
        }
        if let Some(hostname) = &filter.hostname {
            clauses.push("hostname = ?");
            params.push(Box::new(hostname.clone()));
        }
        if let Some(since) = &filter.triggered_since {
            clauses.push("replace(substr(triggered_at, 1, 19), 'T', ' ') >= ?");
            params.push(Box::new(since.clone()));
        }
        if let Some(until) = &filter.triggered_until {
            clauses.push("replace(substr(triggered_at, 1, 19), 'T', ' ') <= ?");
            params.push(Box::new(until.clone()));
        }
        if let Some(to_state) = &filter.to_state {
            clauses.push("to_state = ?");
            params.push(Box::new(to_state.clone()));
        }
        if let Some(alarm_type) = &filter.alarm_type {
            clauses.push("alarm_type = ?");
            params.push(Box::new(alarm_type.clone()));
        }
        if let Some(severity) = &filter.severity {
            clauses.push("COALESCE(max_severity, severity) = ?");
            params.push(Box::new(severity.clone()));
        }
        if let Some(sensor_id) = &filter.sensor_id {
            clauses.push("sensor_id = ?");
            params.push(Box::new(sensor_id.clone()));
        }
        // `substr`, not `LIKE`: see `AlarmListFilter::sensor_id_prefix`. The
        // prefix is bound twice — once for the length, once for the
        // comparison — because SQLite has no positional re-use of a
        // parameter in this dialect position.
        if let Some(prefix) = &filter.sensor_id_prefix {
            clauses.push("substr(sensor_id, 1, length(?)) = ?");
            params.push(Box::new(prefix.clone()));
            params.push(Box::new(prefix.clone()));
        }
        if let Some(field) = &filter.sensor_field {
            // The id ends in the field's label (`:Temperature`), so the match
            // ignores case.
            let suffix = format!(":{}", field.to_lowercase());
            let suffix_clause = "lower(substr(sensor_id, -length(?))) = ?";
            if field == "temperature" {
                clauses.push(
                    "(instr(sensor_id, ':') = 0 OR lower(substr(sensor_id, -length(?))) = ?)",
                );
            } else {
                clauses.push(suffix_clause);
            }
            params.push(Box::new(suffix.clone()));
            params.push(Box::new(suffix));
        }

        // Case-insensitive substring over what the operator reads on a row
        // (`like_contains` does the escaping): the probe's name, raw id and
        // location, the Hub, and the operator's metadata of the device the
        // alarm belongs to — its name, description and location
        // (`device_meta`, matched to the alarm through the sensor_id shape
        // `alarm_target` decodes). `search_targets` adds what only the
        // live registry knows.
        //
        // Anonymous `?` with the pattern bound once per use — the shape
        // `sensor_id_prefix` above already uses. A numbered `?1` would NOT
        // work: every other clause binds anonymously, so `?1` would name the
        // first of those rather than this one.
        if let Some(search) = &filter.search {
            let pattern = crate::like_contains(search);
            let mut ors = vec![
                "sensor_name LIKE ? ESCAPE '~'".to_string(),
                "sensor_id LIKE ? ESCAPE '~'".to_string(),
                "sensor_location LIKE ? ESCAPE '~'".to_string(),
                "hostname LIKE ? ESCAPE '~'".to_string(),
                // The alarm's device, by the sensor_id shapes `alarm_target`
                // decodes, plus the calibration reminder's `cal:<scope>:<id>`.
                // A probe's entity is the wire line, stored as "N" or "line-N".
                "EXISTS (SELECT 1 FROM device_meta m WHERE \
                   (m.name LIKE ? ESCAPE '~' OR m.description LIKE ? ESCAPE '~' \
                    OR m.location LIKE ? ESCAPE '~') AND ( \
                   (m.scope = 'node' AND (lower(substr(sensor_id, 1, length(m.entity) + 5)) \
                      IN ('lrw:' || lower(m.entity) || ':', 'p2p:' || lower(m.entity) || ':') \
                      OR lower(sensor_id) = 'cal:node:' || lower(m.entity))) \
                   OR (m.scope = 'eye' AND (upper(substr(sensor_id, 1, length(m.entity) + 5)) \
                      = 'EYE:' || upper(m.entity) || ':' \
                      OR upper(sensor_id) = 'CAL:EYE:' || upper(m.entity))) \
                   OR (m.scope = 'probe' AND m.hostname = alarm_events.hostname \
                      AND sensor_id IN ('line-' || replace(m.entity, 'line-', ''), \
                                        'cal:probe:' || replace(m.entity, 'line-', ''))) \
                   OR (m.scope = 'hub' AND m.hostname = alarm_events.hostname \
                      AND sensor_id NOT LIKE 'lrw:%' AND sensor_id NOT LIKE 'p2p:%' \
                      AND sensor_id NOT LIKE 'eye:%' AND sensor_id NOT LIKE 'line-%' \
                      AND sensor_id NOT LIKE 'cal:%')))"
                    .to_string(),
            ];
            for _ in 0..7 {
                params.push(Box::new(pattern.clone()));
            }
            for (host, prefix) in filter.search_targets.iter().take(500) {
                ors.push(
                    "((? = '' OR hostname = ?) AND lower(substr(sensor_id, 1, length(?))) = lower(?))"
                        .to_string(),
                );
                params.push(Box::new(host.clone()));
                params.push(Box::new(host.clone()));
                params.push(Box::new(prefix.clone()));
                params.push(Box::new(prefix.clone()));
            }
            search_sql = format!("({})", ors.join(" OR "));
            clauses.push(&search_sql);
        }

        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) FROM alarm_events {where_sql}");
        let total: i64 = self.conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |r| r.get(0),
        )?;

        let limit = filter.limit.clamp(1, 1000);
        // The direction is two literals and the column an enum variant, so
        // the ORDER BY is built from the whitelist alone. Every sort but the
        // default one falls back to newest-first inside equal values, which
        // is the order the operator reads the log in anyway.
        let dir = if filter.sort_desc { "DESC" } else { "ASC" };
        let order_sql = match filter.sort {
            AlarmSort::Start => format!("triggered_at {dir}, created_at {dir}"),
            other => format!("{} {dir}, triggered_at DESC", other.order_sql()),
        };
        let list_sql = format!(
            "SELECT {ALARM_COLUMNS} FROM alarm_events
             {where_sql}
             ORDER BY {order_sql}
             LIMIT ? OFFSET ?"
        );
        let mut stmt = self.conn.prepare(&list_sql)?;
        let mut all_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        all_params.push(&limit);
        all_params.push(&filter.offset);

        let rows = stmt.query_map(all_params.as_slice(), row_to_alarm)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok((out, total))
    }

    /// Most recent alarm events for a hostname, newest `triggered_at` first.
    pub fn list_recent_alarms(&self, hostname: &str, limit: i64) -> Result<Vec<AlarmEvent>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ALARM_COLUMNS} FROM alarm_events
             WHERE hostname = ?1
             ORDER BY triggered_at DESC
             LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![hostname, limit], row_to_alarm)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Add a note to an alarm (generated uuid v4 id, DB-stamped
    /// `created_at`); returns the stored row. The alarm must exist (FK) —
    /// callers wanting a 404 should `get_alarm` first.
    pub fn insert_alarm_note(
        &self,
        alarm_id: &str,
        user_id: Option<i64>,
        username: &str,
        content: &str,
    ) -> Result<AlarmNote> {
        let id = Uuid::new_v4().to_string();
        self.conn
            .query_row(
                "INSERT INTO alarm_notes (id, alarm_id, user_id, username, content)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 RETURNING id, alarm_id, user_id, username, content, created_at",
                params![id, alarm_id, user_id, username, content],
                row_to_note,
            )
            .map_err(Into::into)
    }

    /// The reason given with an alarm's current acknowledgement, or `None`
    /// when it is not acknowledged. The reason is stored as the note
    /// [`Self::insert_alarm_note`] writes right after
    /// [`Self::acknowledge_alarm`], in the same transaction, so it is the
    /// acknowledging user's first note from the ack's second on. The two
    /// clocks differ in shape (`created_at` is SQLite's `datetime('now')`,
    /// `acknowledged_at` RFC 3339), hence `datetime(?)` on the stamp.
    pub fn ack_reason(&self, alarm: &AlarmEvent) -> Result<Option<String>> {
        let (Some(at), Some(user)) = (
            alarm.acknowledged_at.as_deref(),
            alarm.acknowledged_by_username.as_deref(),
        ) else {
            return Ok(None);
        };
        self.conn
            .query_row(
                "SELECT content FROM alarm_notes
                 WHERE alarm_id = ?1 AND username = ?2 AND created_at >= datetime(?3)
                 ORDER BY created_at ASC, rowid ASC
                 LIMIT 1",
                params![alarm.id, user, at],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// All notes for an alarm, oldest first.
    pub fn list_alarm_notes(&self, alarm_id: &str) -> Result<Vec<AlarmNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, alarm_id, user_id, username, content, created_at
             FROM alarm_notes
             WHERE alarm_id = ?1
             ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![alarm_id], row_to_note)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
