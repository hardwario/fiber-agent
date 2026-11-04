//! `config_generation` / `config_sync_item` (V41): the Portal side of the
//! Hub configuration pull (plan/portal/config_sync.md §5, §9; decision #43).
//!
//! Every method takes `now` (unix seconds) from the caller so the alarm
//! rules built on these rows can be tested without a clock.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::models::FieldThreshold;
use crate::threshold_repo::threshold_from_row;
use crate::Storage;

/// One Hub's sync bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigGeneration {
    pub hub_id: String,
    pub gen: u64,
    pub hash: String,
    pub changed_at: i64,
    pub acked_gen: Option<u64>,
    pub acked_hash: Option<String>,
    pub acked_at: Option<i64>,
    pub last_req_at: Option<i64>,
    /// Earliest `failed_since` of the Hub's items still failed.
    pub failed_since: Option<i64>,
}

/// One key's line of a Hub ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSyncResult {
    pub key: String,
    pub ok: bool,
    pub error: Option<String>,
}

const GEN_COLUMNS: &str = "g.hub_id, g.gen, g.hash, g.changed_at, g.acked_gen, g.acked_hash,
     g.acked_at, g.last_req_at,
     (SELECT MIN(i.failed_since) FROM config_sync_item i
       WHERE i.hub_id = g.hub_id AND i.state = 'failed')";

fn gen_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConfigGeneration> {
    Ok(ConfigGeneration {
        hub_id: r.get(0)?,
        gen: r.get::<_, i64>(1)? as u64,
        hash: r.get(2)?,
        changed_at: r.get(3)?,
        acked_gen: r.get::<_, Option<i64>>(4)?.map(|g| g as u64),
        acked_hash: r.get(5)?,
        acked_at: r.get(6)?,
        last_req_at: r.get(7)?,
        failed_since: r.get(8)?,
    })
}

fn hub_key(hub: &str) -> String {
    hub.to_ascii_lowercase()
}

impl Storage {
    /// The Hub's row, if it ever had one.
    pub fn config_generation(&self, hub: &str) -> Result<Option<ConfigGeneration>> {
        self.conn
            .query_row(
                &format!("SELECT {GEN_COLUMNS} FROM config_generation g WHERE g.hub_id = ?1"),
                params![hub_key(hub)],
                gen_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every Hub's row, ordered by hub.
    pub fn list_config_generations(&self) -> Result<Vec<ConfigGeneration>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {GEN_COLUMNS} FROM config_generation g ORDER BY g.hub_id"
        ))?;
        let rows = stmt.query_map([], gen_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A save touched the Hub: `gen + 1` (1 for a Hub without a row) and
    /// the document's new `hash`. Answers the new generation.
    pub fn bump_config_generation(&self, hub: &str, hash: &str, now: i64) -> Result<u64> {
        let gen: i64 = self.conn.query_row(
            "INSERT INTO config_generation (hub_id, gen, hash, changed_at)
             VALUES (?1, 1, ?2, ?3)
             ON CONFLICT (hub_id) DO UPDATE
               SET gen = gen + 1, hash = excluded.hash, changed_at = excluded.changed_at
             RETURNING gen",
            params![hub_key(hub), hash, now],
            |r| r.get(0),
        )?;
        Ok(gen as u64)
    }

    /// The generation for `hash`: the stored one when the hash matches,
    /// otherwise bumped (a change no save hook saw, or the first request).
    pub fn ensure_config_generation(&self, hub: &str, hash: &str, now: i64) -> Result<u64> {
        match self.config_generation(hub)? {
            Some(row) if row.hash == hash => Ok(row.gen),
            _ => self.bump_config_generation(hub, hash, now),
        }
    }

    /// Who saved `key` last, at which generation; the item is pending again.
    pub fn note_config_sync_item(
        &self,
        hub: &str,
        key: &str,
        editor: &str,
        gen: u64,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO config_sync_item (hub_id, key, gen, editor, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)
             ON CONFLICT (hub_id, key) DO UPDATE
               SET gen = excluded.gen, editor = excluded.editor,
                   state = 'pending', updated_at = excluded.updated_at",
            params![hub_key(hub), key, gen as i64, editor, now],
        )?;
        Ok(())
    }

    /// `(key, editor)` of every item the Hub has a row for.
    pub fn config_sync_editors(&self, hub: &str) -> Result<Vec<(String, Option<String>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, editor FROM config_sync_item WHERE hub_id = ?1 ORDER BY key")?;
        let rows = stmt.query_map(params![hub_key(hub)], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// `(key, last_error)` of the Hub's failed items.
    pub fn config_sync_failures(&self, hub: &str) -> Result<Vec<(String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, last_error FROM config_sync_item
             WHERE hub_id = ?1 AND state = 'failed' ORDER BY key",
        )?;
        let rows = stmt.query_map(params![hub_key(hub)], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The Hub asked. A Hub never seen before gets a row at gen 0 so its
    /// requests are tracked (the next document bumps it).
    /// A Portal-side problem with the whole document (e.g. too large to
    /// send) as the failed pseudo-item `key`, so the `sync:` alarm's
    /// failed-item rule covers it; `None` marks it synced again.
    pub fn set_config_sync_item_error(
        &self,
        hub: &str,
        key: &str,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        let state = if error.is_some() { "failed" } else { "synced" };
        self.conn.execute(
            "INSERT INTO config_sync_item
               (hub_id, key, gen, state, last_error, failed_since, updated_at)
             VALUES (?1, ?2, 0, ?3, ?4, CASE WHEN ?3 = 'failed' THEN ?5 END, ?5)
             ON CONFLICT (hub_id, key) DO UPDATE
               SET state = excluded.state, last_error = excluded.last_error,
                   failed_since = CASE WHEN excluded.state = 'failed'
                                       THEN COALESCE(config_sync_item.failed_since, ?5)
                                  END,
                   updated_at = excluded.updated_at",
            params![hub_key(hub), key, state, error, now],
        )?;
        Ok(())
    }

    pub fn record_config_sync_req(&self, hub: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO config_generation (hub_id, gen, hash, changed_at, last_req_at)
             VALUES (?1, 0, '', ?2, ?2)
             ON CONFLICT (hub_id) DO UPDATE SET last_req_at = excluded.last_req_at",
            params![hub_key(hub), now],
        )?;
        Ok(())
    }

    /// The Hub applied `gen`: store it and each key's result. A failed key
    /// keeps the time it first failed; a key that succeeds is synced.
    pub fn record_config_sync_ack(
        &self,
        hub: &str,
        gen: u64,
        hash: &str,
        results: &[ConfigSyncResult],
        now: i64,
    ) -> Result<()> {
        let hub = hub_key(hub);
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO config_generation
               (hub_id, gen, hash, changed_at, acked_gen, acked_hash, acked_at)
             VALUES (?1, 0, '', ?4, ?2, ?3, ?4)
             ON CONFLICT (hub_id) DO UPDATE
               SET acked_gen = excluded.acked_gen, acked_hash = excluded.acked_hash,
                   acked_at = excluded.acked_at",
            params![hub, gen as i64, hash, now],
        )?;
        for r in results {
            let (state, error) = if r.ok {
                ("synced", None)
            } else {
                ("failed", Some(r.error.clone().unwrap_or_default()))
            };
            tx.execute(
                "INSERT INTO config_sync_item
                   (hub_id, key, gen, state, last_error, failed_since, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, CASE WHEN ?4 = 'failed' THEN ?6 END, ?6)
                 ON CONFLICT (hub_id, key) DO UPDATE
                   SET state = excluded.state, last_error = excluded.last_error,
                       failed_since = CASE WHEN excluded.state = 'failed'
                                           THEN COALESCE(config_sync_item.failed_since, ?6)
                                      END,
                       updated_at = excluded.updated_at",
                params![hub, r.key, gen as i64, state, error, now],
            )?;
        }
        // A key the applied document no longer has (renamed or dropped, e.g.
        // `hub/interval` → `hub/sample_interval`, Q19) is not pending or failed
        // any more. Rows of a later save (higher gen) and the Portal-side
        // `document/*` pseudo-items stay.
        let keys: Vec<&str> = results.iter().map(|r| r.key.as_str()).collect();
        let keys_json = serde_json::to_string(&keys).unwrap_or_else(|_| "[]".into());
        tx.execute(
            "DELETE FROM config_sync_item
              WHERE hub_id = ?1 AND gen <= ?2 AND key NOT LIKE 'document/%'
                AND key NOT IN (SELECT value FROM json_each(?3))",
            params![hub, gen as i64, keys_json],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Stores the value a save relayed for `key` (V42).
    pub fn upsert_config_desired(
        &self,
        hub: &str,
        key: &str,
        command: &str,
        params_json: &str,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO config_desired (hub_id, key, command, params, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (hub_id, key) DO UPDATE
               SET command = excluded.command, params = excluded.params,
                   updated_at = excluded.updated_at",
            params![hub_key(hub), key, command, params_json, now],
        )?;
        Ok(())
    }

    /// `(key, command, params_json)` of every stored value, by key.
    pub fn list_config_desired(&self, hub: &str) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, command, params FROM config_desired WHERE hub_id = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![hub_key(hub)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Whether the Hub's missing keys were already seeded from its state.
    pub fn config_seeded(&self, hub: &str) -> Result<bool> {
        let at: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT seeded_at FROM config_generation WHERE hub_id = ?1",
                params![hub_key(hub)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(at.flatten().is_some())
    }

    /// Fills the keys the Portal holds no value for with `rows`
    /// (`(key, command, params_json)`, a stored value always wins) and marks
    /// the Hub seeded. Answers how many rows were added.
    pub fn seed_config_desired(
        &self,
        hub: &str,
        rows: &[(String, String, String)],
        now: i64,
    ) -> Result<usize> {
        let hub = hub_key(hub);
        let tx = self.conn.unchecked_transaction()?;
        let mut added = 0;
        for (key, command, params_json) in rows {
            added += tx.execute(
                "INSERT OR IGNORE INTO config_desired (hub_id, key, command, params, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![hub, key, command, params_json, now],
            )?;
        }
        tx.execute(
            "INSERT INTO config_generation (hub_id, gen, hash, changed_at, seeded_at)
             VALUES (?1, 0, '', ?2, ?2)
             ON CONFLICT (hub_id) DO UPDATE SET seeded_at = excluded.seeded_at",
            params![hub, now],
        )?;
        tx.commit()?;
        Ok(added)
    }

    /// The wired-line (`hub`) and Beacon (`eye`) bounds the Portal holds for
    /// one Hub — the stored copy the desired document is built from.
    pub fn sync_field_thresholds(&self, hub: &str) -> Result<Vec<FieldThreshold>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, scope, entity, field,
                    critical_low, warning_low, warning_high, critical_high,
                    enabled, updated_at, hysteresis
             FROM field_thresholds
             WHERE hostname = ?1 COLLATE NOCASE AND scope IN ('hub', 'eye')
             ORDER BY scope, entity, field",
        )?;
        let rows = stmt.query_map(params![hub], threshold_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> (Storage, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(&dir.path().join("t.db"), None).unwrap();
        (s, dir)
    }

    #[test]
    fn seeding_never_overrides_a_stored_value() {
        let (s, _d) = storage();
        s.upsert_config_desired(
            "HUB",
            "hub/led_brightness",
            "set_led_brightness",
            "{\"brightness\":9}",
            1,
        )
        .unwrap();
        assert!(!s.config_seeded("hub").unwrap());
        let seed = vec![
            (
                "hub/led_brightness".to_string(),
                "set_led_brightness".to_string(),
                "{\"brightness\":1}".to_string(),
            ),
            (
                "hub/device_label".to_string(),
                "set_device_label".to_string(),
                "{\"label\":\"x\"}".to_string(),
            ),
        ];
        assert_eq!(s.seed_config_desired("hub", &seed, 2).unwrap(), 1);
        assert!(s.config_seeded("hub").unwrap());
        let rows = s.list_config_desired("hub").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].2, "{\"brightness\":9}");
    }

    #[test]
    fn generation_bumps_and_follows_the_hash() {
        let (s, _d) = storage();
        assert_eq!(s.bump_config_generation("HUB-A", "h1", 10).unwrap(), 1);
        assert_eq!(s.bump_config_generation("hub-a", "h2", 11).unwrap(), 2);
        assert_eq!(s.ensure_config_generation("hub-a", "h2", 12).unwrap(), 2);
        assert_eq!(s.ensure_config_generation("hub-a", "h3", 13).unwrap(), 3);
        let row = s.config_generation("Hub-A").unwrap().unwrap();
        assert_eq!((row.gen, row.hash.as_str(), row.changed_at), (3, "h3", 13));
    }

    #[test]
    fn acks_record_results_and_keep_the_first_failure_time() {
        let (s, _d) = storage();
        s.record_config_sync_req("hub", 5).unwrap();
        s.note_config_sync_item("hub", "hub/threshold/0", "alice", 1, 5)
            .unwrap();
        let fail = |at| {
            s.record_config_sync_ack(
                "hub",
                1,
                "h",
                &[ConfigSyncResult {
                    key: "hub/threshold/0".into(),
                    ok: false,
                    error: Some("bad".into()),
                }],
                at,
            )
            .unwrap()
        };
        fail(100);
        fail(200);
        let row = s.config_generation("hub").unwrap().unwrap();
        assert_eq!(row.acked_gen, Some(1));
        assert_eq!(row.last_req_at, Some(5));
        assert_eq!(row.failed_since, Some(100));
        assert_eq!(
            s.config_sync_failures("hub").unwrap(),
            vec![("hub/threshold/0".into(), Some("bad".into()))]
        );
        s.record_config_sync_ack(
            "hub",
            2,
            "h",
            &[ConfigSyncResult {
                key: "hub/threshold/0".into(),
                ok: true,
                error: None,
            }],
            300,
        )
        .unwrap();
        let row = s.config_generation("hub").unwrap().unwrap();
        assert_eq!((row.acked_gen, row.failed_since), (Some(2), None));
        assert_eq!(
            s.config_sync_editors("hub").unwrap(),
            vec![("hub/threshold/0".into(), Some("alice".into()))]
        );
    }
    #[test]
    fn an_ack_drops_keys_the_applied_document_no_longer_has() {
        let (s, _d) = storage();
        s.note_config_sync_item("hub", "hub/interval", "alice", 1, 5)
            .unwrap();
        s.set_config_sync_item_error("hub", "hub/interval", Some("out of range"), 6)
            .unwrap();
        s.note_config_sync_item("hub", "hub/sample_interval", "alice", 2, 7)
            .unwrap();
        s.note_config_sync_item("hub", "hub/threshold/0", "bob", 3, 8)
            .unwrap();
        s.record_config_sync_ack(
            "hub",
            2,
            "h",
            &[ConfigSyncResult {
                key: "hub/sample_interval".into(),
                ok: true,
                error: None,
            }],
            9,
        )
        .unwrap();
        let keys: Vec<String> = s
            .config_sync_editors("hub")
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert!(!keys.contains(&"hub/interval".to_string()), "{keys:?}");
        assert!(keys.contains(&"hub/sample_interval".to_string()));
        assert!(
            keys.contains(&"hub/threshold/0".to_string()),
            "later gen kept"
        );
        assert!(s.config_sync_failures("hub").unwrap().is_empty());
    }
}
