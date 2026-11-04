//! `sessions` repository. Tokens are identified by `token_hash` (a SHA-256
//! hex digest computed by the caller) — raw tokens are never stored here.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::models::{SessionRow, User};
use crate::user_repo::{row_to_user_at, USER_COLUMNS};
use crate::Storage;

impl Storage {
    /// Insert a new session row.
    pub fn create_session(&self, session: &SessionRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, last_seen_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.token_hash,
                session.user_id,
                session.created_at,
                session.last_seen_at,
                session.expires_at,
            ],
        )?;
        Ok(())
    }

    /// Look up a session by token hash, joined with its user. Returns `None`
    /// both when the token hash is unknown/expired-and-pruned *and* when the
    /// owning user is disabled — a disabled user has no valid sessions from
    /// the caller's point of view, even if the row still exists.
    pub fn get_session(&self, token_hash: &str) -> Result<Option<(SessionRow, User)>> {
        let user_cols: Vec<String> = USER_COLUMNS.split(", ").map(|c| format!("u.{c}")).collect();
        let sql = format!(
            "SELECT s.token_hash, s.user_id, s.created_at, s.last_seen_at, s.expires_at, {cols}
             FROM sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND u.enabled = 1",
            cols = user_cols.join(", ")
        );
        self.conn
            .query_row(&sql, params![token_hash], |r| {
                let session = SessionRow {
                    token_hash: r.get(0)?,
                    user_id: r.get(1)?,
                    created_at: r.get(2)?,
                    last_seen_at: r.get(3)?,
                    expires_at: r.get(4)?,
                };
                let user = row_to_user_at(r, 5)?;
                Ok((session, user))
            })
            .optional()
            .map_err(Into::into)
    }

    /// Extend a session's lifetime (sliding expiry) and bump its
    /// `last_seen_at`.
    pub fn bump_session(&self, token_hash: &str, last_seen_at: i64, expires_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_seen_at = ?1, expires_at = ?2 WHERE token_hash = ?3",
            params![last_seen_at, expires_at, token_hash],
        )?;
        Ok(())
    }

    /// Delete a single session (logout). Returns `false` if it didn't exist.
    pub fn delete_session(&self, token_hash: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM sessions WHERE token_hash = ?1",
            params![token_hash],
        )?;
        Ok(changed > 0)
    }

    /// Delete all sessions for a user (logout-all / password change / user
    /// disable). Returns the number removed.
    pub fn delete_sessions_for_user(&self, user_id: i64) -> Result<usize> {
        let changed = self
            .conn
            .execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])?;
        Ok(changed)
    }

    /// Delete sessions whose `expires_at` is at or before `now_unix`.
    /// Returns the number removed.
    pub fn prune_expired_sessions(&self, now_unix: i64) -> Result<usize> {
        let changed = self.conn.execute(
            "DELETE FROM sessions WHERE expires_at <= ?1",
            params![now_unix],
        )?;
        Ok(changed)
    }
}
