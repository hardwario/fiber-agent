//! The alarm-slot layout the portal last wrote to a Node: `node_alarm_layout` (V24,
//! plan/portal/nodes_phase1.cs.md WP2).

use rusqlite::{params, OptionalExtension};

use crate::{Result, Storage};

impl Storage {
    /// The layout last sent to this Node, slot order, or `None` if the portal never sent one.
    pub fn node_alarm_layout(&self, hostname: &str, node_key: &str) -> Result<Option<Vec<String>>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT layout FROM node_alarm_layout WHERE hostname = ?1 AND node_key = ?2",
                params![hostname, node_key],
                |row| row.get(0),
            )
            .optional()?;
        // A row that no longer parses is treated as never sent: the next comparison then
        // asks for a resend rather than trusting a layout it cannot read.
        Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
    }

    /// Records the layout a successful alarm-table send wrote.
    pub fn set_node_alarm_layout(
        &self,
        hostname: &str,
        node_key: &str,
        layout: &[String],
        sent_at: i64,
    ) -> Result<()> {
        let json = serde_json::to_string(layout).unwrap_or_else(|_| "[]".to_string());
        self.conn.execute(
            "INSERT INTO node_alarm_layout (hostname, node_key, layout, sent_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (hostname, node_key) DO UPDATE SET
                layout = excluded.layout, sent_at = excluded.sent_at",
            params![hostname, node_key, json, sent_at],
        )?;
        Ok(())
    }

    /// Forgets a removed Node's layout.
    pub fn delete_node_alarm_layout(&self, hostname: &str, node_key: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM node_alarm_layout WHERE hostname = ?1 AND node_key = ?2",
            params![hostname, node_key],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use crate::Storage;

    #[test]
    fn a_layout_round_trips_and_is_replaced_and_forgotten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Storage::open(&dir.path().join("t.db"), None).expect("open");
        assert_eq!(s.node_alarm_layout("h1", "aa11").expect("read"), None);

        let first = vec!["temperature".to_string(), "humidity".to_string()];
        s.set_node_alarm_layout("h1", "aa11", &first, 1)
            .expect("write");
        assert_eq!(
            s.node_alarm_layout("h1", "aa11").expect("read"),
            Some(first)
        );

        let second = vec!["temperature".to_string(), "w1_1_temperature".to_string()];
        s.set_node_alarm_layout("h1", "aa11", &second, 2)
            .expect("replace");
        assert_eq!(
            s.node_alarm_layout("h1", "aa11").expect("read"),
            Some(second)
        );

        assert_eq!(s.delete_node_alarm_layout("h1", "aa11").expect("delete"), 1);
        assert_eq!(s.node_alarm_layout("h1", "aa11").expect("read"), None);
    }
}
