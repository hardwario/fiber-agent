//! `users` repository. No password hashing or token generation here — pure
//! persistence; that belongs to the wave-2 auth service.

use std::collections::HashMap;

use rusqlite::{params, params_from_iter, OptionalExtension};

use crate::error::{is_unique_violation, Result, StorageError};
use crate::models::{NewUser, User};
use crate::Storage;

pub(crate) const USER_COLUMNS: &str = "id, username, password_hash, full_name, job_title, \
                                        user_role, is_superuser, enabled, created_at, last_login, \
                                        monitoring_view_mode";

/// Reads a `User` from `USER_COLUMNS` starting at column `offset` — used
/// directly (offset 0) here, and by `session_repo`'s user-join query where
/// the user columns are appended after the session columns.
pub(crate) fn row_to_user_at(r: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(offset)?,
        username: r.get(offset + 1)?,
        password_hash: r.get(offset + 2)?,
        full_name: r.get(offset + 3)?,
        job_title: r.get(offset + 4)?,
        user_role: r.get(offset + 5)?,
        is_superuser: r.get(offset + 6)?,
        enabled: r.get(offset + 7)?,
        created_at: r.get(offset + 8)?,
        last_login: r.get(offset + 9)?,
        monitoring_view_mode: r.get(offset + 10)?,
    })
}

fn row_to_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    row_to_user_at(r, 0)
}

impl Storage {
    /// Insert a new user; returns `StorageError::Conflict` if `username` is
    /// already taken (case-insensitive, per the `COLLATE NOCASE` unique
    /// index).
    pub fn create_user(&self, new: &NewUser) -> Result<User> {
        self.conn
            .query_row(
                &format!(
                    "INSERT INTO users
                        (username, password_hash, full_name, job_title, user_role, is_superuser)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     RETURNING {USER_COLUMNS}"
                ),
                params![
                    new.username,
                    new.password_hash,
                    new.full_name,
                    new.job_title,
                    new.user_role,
                    new.is_superuser,
                ],
                row_to_user,
            )
            .map_err(|e| {
                if is_unique_violation(&e) {
                    StorageError::Conflict(format!("username '{}' already exists", new.username))
                } else {
                    e.into()
                }
            })
    }

    /// Look up a user by id.
    pub fn get_user(&self, id: i64) -> Result<Option<User>> {
        self.conn
            .query_row(
                &format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"),
                params![id],
                row_to_user,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Look up a user by username. Case-insensitive: `username` carries
    /// `COLLATE NOCASE`, so a plain `=` comparison already ignores case.
    pub fn get_user_by_username(&self, username: &str) -> Result<Option<User>> {
        self.conn
            .query_row(
                &format!("SELECT {USER_COLUMNS} FROM users WHERE username = ?1"),
                params![username],
                row_to_user,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Full names for the given usernames, in one query, keyed by the
    /// username exactly as passed in (the match is case-insensitive, like
    /// every username lookup). A name with no user row, or a user without a
    /// full name, is simply absent — that is the "show the username" case,
    /// so callers need no second branch for deleted users or system actors.
    pub fn full_names_for<S: AsRef<str>>(
        &self,
        usernames: &[S],
    ) -> Result<HashMap<String, String>> {
        let mut wanted: Vec<&str> = usernames.iter().map(AsRef::as_ref).collect();
        wanted.sort_unstable();
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; wanted.len()].join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT username, full_name FROM users
             WHERE username IN ({placeholders}) AND TRIM(COALESCE(full_name, '')) <> ''"
        ))?;
        // `COLLATE NOCASE` folds ASCII only, so ASCII folding maps each row
        // back to the spelling the caller asked with.
        let mut by_folded = HashMap::new();
        for row in stmt.query_map(params_from_iter(&wanted), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (username, full_name) = row?;
            by_folded.insert(username.to_ascii_lowercase(), full_name.trim().to_string());
        }
        Ok(wanted
            .into_iter()
            .filter_map(|u| {
                by_folded
                    .get(&u.to_ascii_lowercase())
                    .map(|n| (u.to_string(), n.clone()))
            })
            .collect())
    }

    /// All users, ordered by username; `enabled = 0` rows are excluded
    /// unless `include_disabled`.
    pub fn list_users(&self, include_disabled: bool) -> Result<Vec<User>> {
        let sql = if include_disabled {
            format!("SELECT {USER_COLUMNS} FROM users ORDER BY username")
        } else {
            format!("SELECT {USER_COLUMNS} FROM users WHERE enabled = 1 ORDER BY username")
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_user)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Sets the fields the caller changes and leaves the others as stored:
    /// `full_name` is `None` to keep the name, `Some(None)` to clear it;
    /// `user_role` is `None` to keep the role. A field the caller keeps is
    /// not written back from an earlier read, so two admins editing the same
    /// user at once, one the name and one the role, both land.
    /// Returns `None` if `id` doesn't exist.
    pub fn update_user(
        &self,
        id: i64,
        full_name: Option<Option<&str>>,
        user_role: Option<&str>,
    ) -> Result<Option<User>> {
        let changed = self.conn.execute(
            "UPDATE users
             SET full_name = CASE WHEN ?1 THEN ?2 ELSE full_name END,
                 user_role = COALESCE(?3, user_role)
             WHERE id = ?4",
            params![full_name.is_some(), full_name.flatten(), user_role, id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_user(id)
    }

    /// Set a new password hash. Returns `false` if `id` doesn't exist.
    pub fn set_password(&self, id: i64, password_hash: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2",
            params![password_hash, id],
        )?;
        Ok(changed > 0)
    }

    /// Enable/disable a user. Returns `false` if `id` doesn't exist.
    pub fn set_enabled(&self, id: i64, enabled: bool) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE users SET enabled = ?1 WHERE id = ?2",
            params![enabled, id],
        )?;
        Ok(changed > 0)
    }

    /// Delete a user (cascades to `sessions` via `ON DELETE CASCADE`).
    /// Returns `false` if `id` doesn't exist.
    pub fn delete_user(&self, id: i64) -> Result<bool> {
        let changed = self
            .conn
            .execute("DELETE FROM users WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }

    /// Stamp `last_login = now` for a successful login.
    pub fn touch_last_login(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE users SET last_login = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Count enabled superusers — used by the service layer's "can't
    /// disable/delete the last admin" guard.
    pub fn count_superusers(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM users WHERE is_superuser = 1 AND enabled = 1",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }
}

impl Storage {
    /// Stores this operator's `/monitoring` layout choice. The column has a
    /// CHECK constraint (V8), so an unknown mode is rejected by the database
    /// rather than silently stored — callers validate first and this is the
    /// backstop.
    pub fn set_monitoring_view_mode(&self, user_id: i64, mode: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE users SET monitoring_view_mode = ?2 WHERE id = ?1",
            rusqlite::params![user_id, mode],
        )?;
        Ok(())
    }
}
