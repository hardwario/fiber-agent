//! `audit_log` repository.

use rusqlite::params;

use crate::error::Result;
use crate::models::{AuditEntry, AuditFilter, AuditSort, NewAuditEntry};
use crate::Storage;

/// What [`Storage::audit_counts`] answers: the log as a whole, and the last
/// day of it, the way the `/audit` metric row shows them. A day rather than
/// the whole log for the last three, because "12 failed actions, ever" is a
/// number nobody acts on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditCounts {
    pub total: i64,
    pub last_day: i64,
    pub failed_day: i64,
    pub actors_day: i64,
}

impl Storage {
    /// Insert an audit entry; returns its generated `id`.
    pub fn insert_audit(&self, entry: &NewAuditEntry) -> Result<i64> {
        let id: i64 = self.conn.query_row(
            "INSERT INTO audit_log (event_type, username, user_id, ip, target, details, success)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING id",
            params![
                entry.event_type,
                entry.username,
                entry.user_id,
                entry.ip,
                entry.target,
                entry.details,
                entry.success,
            ],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Filtered, paginated audit log query. Returns `(page, total)` where
    /// `total` is the count of rows matching the filter *before*
    /// limit/offset. `filter.limit` is clamped to `1..=1000`.
    /// The distinct `event_type` and `username` values the log actually holds,
    /// for the filter dropdowns.
    ///
    /// Read from `audit_log` itself, not from the code's list of event names
    /// or from the live `users` table. This is an evidence trail: it records
    /// what a user DID, including a user since deleted, and a dropdown built
    /// from current users would silently make those rows unreachable. The
    /// trade is a `DISTINCT` without an index on `username` — acceptable
    /// because this table holds operator actions, not telemetry: hundreds of
    /// rows a year, not millions. If that ever stops being true, the answer is
    /// an index, not a different source.
    pub fn audit_filter_options(&self) -> Result<(Vec<String>, Vec<String>)> {
        let collect = |sql: &str| -> Result<Vec<String>> {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
            let mut out = Vec::new();
            for row in rows {
                if let Some(value) = row? {
                    if !value.is_empty() {
                        out.push(value);
                    }
                }
            }
            Ok(out)
        };
        let events = collect("SELECT DISTINCT event_type FROM audit_log ORDER BY 1")?;
        let users = collect("SELECT DISTINCT username FROM audit_log ORDER BY 1")?;
        Ok((events, users))
    }

    /// The four numbers above `/audit`. One query: `ts` is written by
    /// `datetime('now')` and never by the caller, so every row is the same
    /// `YYYY-MM-DD HH:MM:SS` shape and a string comparison is a time
    /// comparison.
    ///
    /// The actors are authenticated users only: rows with a `user_id`, which
    /// only an action by a signed-in user carries (#107). A failed or refused
    /// login has none, and its `username` is whatever the client typed, so
    /// counting usernames let anyone inflate the tile by posting to `/login`;
    /// `system:*` rows have none either. Those rows count in the totals, and
    /// in `failed_day` when they failed, but never among the actors.
    /// Distinct `user_id` rather than `username` because `audit_log` has no
    /// foreign key to `users` and `users.id` is `AUTOINCREMENT`: a user
    /// deleted since still counts, and one who later reuses the name counts
    /// as someone else.
    pub fn audit_counts(&self) -> Result<AuditCounts> {
        let row = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(ts >= datetime('now', '-1 day')), 0),
                    COALESCE(SUM(ts >= datetime('now', '-1 day') AND success = 0), 0),
                    (SELECT COUNT(DISTINCT user_id) FROM audit_log
                      WHERE ts >= datetime('now', '-1 day')
                        AND user_id IS NOT NULL)
               FROM audit_log",
            [],
            |r| {
                Ok(AuditCounts {
                    total: r.get(0)?,
                    last_day: r.get(1)?,
                    failed_day: r.get(2)?,
                    actors_day: r.get(3)?,
                })
            },
        )?;
        Ok(row)
    }

    pub fn list_audit(&self, filter: &AuditFilter) -> Result<(Vec<AuditEntry>, i64)> {
        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(event_type) = &filter.event_type {
            clauses.push("event_type = ?");
            params.push(Box::new(event_type.clone()));
        }
        if let Some(username) = &filter.username {
            clauses.push("username = ?");
            params.push(Box::new(username.clone()));
        }
        if let Some(since) = &filter.since {
            clauses.push("ts >= ?");
            params.push(Box::new(since.clone()));
        }
        if let Some(until) = &filter.until {
            clauses.push("ts <= ?");
            params.push(Box::new(until.clone()));
        }
        // Three anonymous `?`, the pattern bound three times: every other
        // clause here binds anonymously too, so a numbered `?1` would name
        // the first of those rather than this one.
        if let Some(search) = &filter.search {
            clauses.push(
                "(event_type LIKE ? ESCAPE '~' OR username LIKE ? ESCAPE '~' OR target LIKE ? ESCAPE '~')",
            );
            let pattern = crate::like_contains(search);
            params.push(Box::new(pattern.clone()));
            params.push(Box::new(pattern.clone()));
            params.push(Box::new(pattern));
        }

        if let Some(success) = filter.success {
            clauses.push("success = ?");
            params.push(Box::new(success));
        }

        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) FROM audit_log {where_sql}");
        let total: i64 = self.conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |r| r.get(0),
        )?;

        let limit = filter.limit.clamp(1, 1000);
        // Whitelisted column + two literal directions; see the same shape in
        // `alarm_repo::list_alarms`. `id` breaks ties so paging is stable
        // when a column repeats (every row of one event type, say).
        let dir = if filter.sort_desc { "DESC" } else { "ASC" };
        let order_sql = match filter.sort {
            AuditSort::Time => format!("ts {dir}, id {dir}"),
            other => format!("{} {dir}, ts DESC, id DESC", other.order_sql()),
        };
        let list_sql = format!(
            "SELECT id, ts, event_type, username, user_id, ip, target, details, success
             FROM audit_log
             {where_sql}
             ORDER BY {order_sql}
             LIMIT ? OFFSET ?"
        );
        let mut stmt = self.conn.prepare(&list_sql)?;
        let mut all_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        all_params.push(&limit);
        all_params.push(&filter.offset);

        let rows = stmt.query_map(all_params.as_slice(), |r| {
            Ok(AuditEntry {
                id: r.get(0)?,
                ts: r.get(1)?,
                event_type: r.get(2)?,
                username: r.get(3)?,
                user_id: r.get(4)?,
                ip: r.get(5)?,
                target: r.get(6)?,
                details: r.get(7)?,
                success: r.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok((out, total))
    }
}
