//! The delivery deadline of a Node's pending configuration: `node_config_deadline` (V43,
//! plan/portal/config_sync.md §3, §13 Q10–Q11, Q20c).
//!
//! The Portal sends a Node its whole pending configuration as one message; until the Node
//! confirms it, the Node detail counts down to `deadline_at`, and past it the Node gets its
//! `cfgsync:` device fault. Each send (the first one and every "Send again") restarts the
//! deadline and counts an attempt.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::Storage;

/// One Node's deadline row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfigDeadline {
    pub hostname: String,
    pub node_key: String,
    /// Unix seconds of the last send.
    pub sent_at: i64,
    /// Unix seconds the Node has to confirm by.
    pub deadline_at: i64,
    /// Sends since the Node last confirmed.
    pub attempts: i64,
}

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NodeConfigDeadline> {
    Ok(NodeConfigDeadline {
        hostname: r.get(0)?,
        node_key: r.get(1)?,
        sent_at: r.get(2)?,
        deadline_at: r.get(3)?,
        attempts: r.get(4)?,
    })
}

impl Storage {
    /// A send of the Node's message: `sent_at = now`, the deadline restarts at `now + window_s`,
    /// one more attempt.
    pub fn note_node_config_sent(
        &self,
        hostname: &str,
        node_key: &str,
        now: i64,
        window_s: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO node_config_deadline (hostname, node_key, sent_at, deadline_at, attempts)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT (hostname, node_key) DO UPDATE SET
                 sent_at = excluded.sent_at,
                 deadline_at = excluded.deadline_at,
                 attempts = node_config_deadline.attempts + 1",
            params![hostname, node_key, now, now + window_s],
        )?;
        Ok(())
    }

    /// The Node's deadline row, if a message waits for it.
    pub fn node_config_deadline(
        &self,
        hostname: &str,
        node_key: &str,
    ) -> Result<Option<NodeConfigDeadline>> {
        Ok(self
            .conn
            .query_row(
                "SELECT hostname, node_key, sent_at, deadline_at, attempts
                 FROM node_config_deadline
                 WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2",
                params![hostname, node_key],
                from_row,
            )
            .optional()?)
    }

    /// Every deadline row, for the sweep that opens the `cfgsync:` faults.
    pub fn list_node_config_deadlines(&self) -> Result<Vec<NodeConfigDeadline>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, node_key, sent_at, deadline_at, attempts
             FROM node_config_deadline ORDER BY hostname, node_key",
        )?;
        let rows = stmt.query_map([], from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The Node confirmed (or has nothing pending any more): its deadline goes.
    pub fn clear_node_config_deadline(&self, hostname: &str, node_key: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM node_config_deadline WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2",
            params![hostname, node_key],
        )? > 0)
    }
}

#[cfg(test)]
mod tests {
    use crate::Storage;

    #[test]
    fn a_send_restarts_the_deadline_and_counts_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(&dir.path().join("p.db"), None).unwrap();
        assert!(s.node_config_deadline("hub", "aa").unwrap().is_none());
        s.note_node_config_sent("hub", "aa", 100, 14_400).unwrap();
        s.note_node_config_sent("HUB", "aa", 200, 14_400).unwrap();
        let row = s.node_config_deadline("Hub", "aa").unwrap().unwrap();
        assert_eq!(
            (row.sent_at, row.deadline_at, row.attempts),
            (200, 14_600, 2)
        );
        assert_eq!(s.list_node_config_deadlines().unwrap().len(), 1);
        assert!(s.clear_node_config_deadline("hub", "aa").unwrap());
        assert!(s.node_config_deadline("hub", "aa").unwrap().is_none());
    }
}
