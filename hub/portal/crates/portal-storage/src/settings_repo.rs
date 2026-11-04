//! `portal_settings` (V19): installation-wide display settings, one row.

use rusqlite::params;

use crate::error::Result;
use crate::Storage;

impl Storage {
    /// The IANA zone every instant is displayed in (plan/decisions.md #25).
    /// V19 seeds the row, so there is always an answer.
    pub fn display_timezone(&self) -> Result<String> {
        self.conn
            .query_row(
                "SELECT display_timezone FROM portal_settings WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// Stores a zone name as given; validating it is the caller's job, which
    /// holds the zone database.
    pub fn set_display_timezone(&self, zone: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE portal_settings SET display_timezone = ?1, updated_at = datetime('now')
             WHERE id = 1",
            params![zone],
        )?;
        Ok(())
    }
}
