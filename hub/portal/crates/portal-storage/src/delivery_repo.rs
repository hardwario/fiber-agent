//! What each Node has confirmed of its alarm table: `node_threshold_delivery`
//! (V25, plan/portal/nodes_phase1.cs.md dodatek 2026-09-27, PF-3).
//!
//! The portal reads no alarm slots back from a Node (WP2), so the only thing
//! it can say about "is this band on the Node" is what the Node has acked.
//! One row per (Hub, Node, field) of the table: the band last confirmed and
//! when, plus whether a newer one is on its way or did not land in time.
//!
//! Every write replaces the whole table on the Node (`alarms_replace`), and
//! so [`Storage::confirm_node_delivery`] does here: a field the confirmed
//! table no longer carries has no rule on the Node any more and loses its row.

use rusqlite::params;

use crate::error::Result;
use crate::models::{
    NodeBand, NodeThresholdDelivery, DELIVERY_STATE_CONFIRMED, DELIVERY_STATE_DIFFERS,
    DELIVERY_STATE_PENDING, DELIVERY_STATE_UNDELIVERED, DELIVERY_STATE_UNKNOWN,
};
use crate::Storage;

const DELIVERY_COLUMNS: &str = "hostname, node_key, field, confirmed_low, confirmed_high, \
     confirmed_enabled, confirmed_at, state, updated_at, sent_low, sent_high, sent_enabled";

impl Storage {
    /// Every field row of one Node, by field.
    pub fn list_node_delivery(
        &self,
        hostname: &str,
        node_key: &str,
    ) -> Result<Vec<NodeThresholdDelivery>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM node_threshold_delivery
             WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2
             ORDER BY field ASC"
        ))?;
        let rows = stmt.query_map(params![hostname, node_key], delivery_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every field row of every Node of one Hub, by Node (config_sync.md §13 Q20e: the
    /// `/limits` page's "not synchronised" badge).
    pub fn list_node_delivery_by_hub(
        &self,
        hostname: &str,
    ) -> Result<std::collections::HashMap<String, Vec<NodeThresholdDelivery>>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM node_threshold_delivery
             WHERE hostname = ?1 COLLATE NOCASE
             ORDER BY node_key ASC, field ASC"
        ))?;
        let rows = stmt.query_map(params![hostname], delivery_from_row)?;
        let mut out: std::collections::HashMap<String, Vec<NodeThresholdDelivery>> =
            std::collections::HashMap::new();
        for row in rows {
            let row = row?;
            out.entry(row.node_key.clone()).or_default().push(row);
        }
        Ok(out)
    }

    /// A table carrying `bands` is on its way to the Node: each band the Node
    /// does not already hold is `pending`. A band the Node confirmed before,
    /// unchanged, stays `confirmed` with its time.
    pub fn mark_node_delivery_pending(
        &self,
        hostname: &str,
        node_key: &str,
        bands: &[NodeBand],
        now: i64,
    ) -> Result<()> {
        let held = self.list_node_delivery(hostname, node_key)?;
        for band in bands {
            let current = held.iter().find(|d| d.field == band.field);
            if current.is_some_and(|d| d.holds(band)) {
                continue;
            }
            self.conn.execute(
                "INSERT INTO node_threshold_delivery
                    (hostname, node_key, field, state, updated_at, sent_low, sent_high,
                     sent_enabled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (hostname, node_key, field) DO UPDATE SET
                    state = excluded.state, updated_at = excluded.updated_at,
                    sent_low = excluded.sent_low, sent_high = excluded.sent_high,
                    sent_enabled = excluded.sent_enabled",
                params![
                    hostname,
                    node_key,
                    band.field,
                    DELIVERY_STATE_PENDING,
                    now,
                    band.low,
                    band.high,
                    band.enabled
                ],
            )?;
        }
        Ok(())
    }

    /// The Node confirmed a table carrying exactly `bands`. Each is
    /// `confirmed`, at `now` unless the Node already held that very band
    /// (then its time stays: the Node has had it since). A field the table no
    /// longer carries loses its row — `alarms_replace` cleared its slot.
    pub fn confirm_node_delivery(
        &self,
        hostname: &str,
        node_key: &str,
        bands: &[NodeBand],
        now: i64,
    ) -> Result<()> {
        let held = self.list_node_delivery(hostname, node_key)?;
        for band in bands {
            let current = held.iter().find(|d| d.field == band.field);
            let since = match current {
                Some(d) if d.holds(band) => d.confirmed_at.unwrap_or(now),
                _ => now,
            };
            self.conn.execute(
                "INSERT INTO node_threshold_delivery
                    (hostname, node_key, field, confirmed_low, confirmed_high,
                     confirmed_enabled, confirmed_at, state, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (hostname, node_key, field) DO UPDATE SET
                    confirmed_low = excluded.confirmed_low,
                    confirmed_high = excluded.confirmed_high,
                    confirmed_enabled = excluded.confirmed_enabled,
                    confirmed_at = excluded.confirmed_at,
                    state = excluded.state,
                    updated_at = excluded.updated_at,
                    sent_low = NULL, sent_high = NULL, sent_enabled = NULL",
                params![
                    hostname,
                    node_key,
                    band.field,
                    band.low,
                    band.high,
                    band.enabled,
                    since,
                    DELIVERY_STATE_CONFIRMED,
                    now,
                ],
            )?;
        }
        for gone in held
            .iter()
            .filter(|d| !bands.iter().any(|b| b.field == d.field))
        {
            self.conn.execute(
                "DELETE FROM node_threshold_delivery
                 WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND field = ?3",
                params![hostname, node_key, gone.field],
            )?;
        }
        Ok(())
    }

    /// The table on its way did not land in time: every `pending` field of
    /// the Node is `undelivered`. Returns how many.
    pub fn mark_node_delivery_undelivered(&self, hostname: &str, node_key: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE node_threshold_delivery SET state = ?3, updated_at = ?4
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND state = ?5",
            params![
                hostname,
                node_key,
                DELIVERY_STATE_UNDELIVERED,
                now_unix(),
                DELIVERY_STATE_PENDING
            ],
        )?)
    }

    /// The send was discarded: what the Node last confirmed is what it holds
    /// again, and a field it never confirmed has no row. Only a band on its
    /// way (`pending`, `undelivered`) is forgotten: an `unknown` or `differs`
    /// row says what the Discard itself learned (PF-3b) and stays.
    pub fn forget_node_delivery_attempt(&self, hostname: &str, node_key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM node_threshold_delivery
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2
                AND state IN (?3, ?4) AND confirmed_at IS NULL",
            params![
                hostname,
                node_key,
                DELIVERY_STATE_PENDING,
                DELIVERY_STATE_UNDELIVERED
            ],
        )?;
        self.conn.execute(
            "UPDATE node_threshold_delivery SET state = ?5, updated_at = ?6,
                    sent_low = NULL, sent_high = NULL, sent_enabled = NULL
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND state IN (?3, ?4)",
            params![
                hostname,
                node_key,
                DELIVERY_STATE_PENDING,
                DELIVERY_STATE_UNDELIVERED,
                DELIVERY_STATE_CONFIRMED,
                now_unix()
            ],
        )?;
        Ok(())
    }

    /// A Discard took back frames the Node may already have had (PF-3b): each band of the
    /// Node on its way (`pending`, `undelivered`) — of `fields`, or all with `None` — is
    /// `unknown`: the Node may hold it or the one before. The band sent stays, for a late
    /// answer to name. Returns how many.
    pub fn mark_node_delivery_unknown(
        &self,
        hostname: &str,
        node_key: &str,
        fields: Option<&[String]>,
    ) -> Result<usize> {
        let mut changed = 0usize;
        for d in self.list_node_delivery(hostname, node_key)? {
            let on_its_way =
                d.state == DELIVERY_STATE_PENDING || d.state == DELIVERY_STATE_UNDELIVERED;
            if !on_its_way || fields.is_some_and(|f| !f.contains(&d.field)) {
                continue;
            }
            changed += self.conn.execute(
                "UPDATE node_threshold_delivery SET state = ?4, updated_at = ?5
                  WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND field = ?3",
                params![
                    hostname,
                    node_key,
                    d.field,
                    DELIVERY_STATE_UNKNOWN,
                    now_unix()
                ],
            )?;
        }
        Ok(changed)
    }

    /// The Node refused a discarded frame (PF-3b): it holds what it last confirmed, so each
    /// `unknown` band goes back to that (or loses its row, never confirmed).
    pub fn forget_node_delivery_unknown(&self, hostname: &str, node_key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM node_threshold_delivery
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND state = ?3
                AND confirmed_at IS NULL",
            params![hostname, node_key, DELIVERY_STATE_UNKNOWN],
        )?;
        self.conn.execute(
            "UPDATE node_threshold_delivery SET state = ?4, updated_at = ?5,
                    sent_low = NULL, sent_high = NULL, sent_enabled = NULL
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND state = ?3",
            params![
                hostname,
                node_key,
                DELIVERY_STATE_UNKNOWN,
                DELIVERY_STATE_CONFIRMED,
                now_unix()
            ],
        )?;
        Ok(())
    }

    /// The Node answered a discarded frame (PF-3b): each `unknown` band is on the Node after
    /// all — it holds the band sent, confirmed now. Where the portal holds that very band
    /// (`portal`, e.g. a field the Discard could not revert) the row is simply `confirmed`;
    /// everywhere else it `differs`. Returns the fields that differ, with the band the Node
    /// holds.
    pub fn node_took_discarded(
        &self,
        hostname: &str,
        node_key: &str,
        now: i64,
        portal: &[NodeBand],
    ) -> Result<Vec<NodeBand>> {
        let took: Vec<NodeBand> = self
            .list_node_delivery(hostname, node_key)?
            .into_iter()
            .filter(|d| d.state == DELIVERY_STATE_UNKNOWN)
            .map(|d| NodeBand {
                field: d.field,
                low: d.sent_low,
                high: d.sent_high,
                enabled: d.sent_enabled.unwrap_or(true),
            })
            .collect();
        let mut differs = Vec::new();
        for band in took {
            let same = portal
                .iter()
                .any(|p| p.field == band.field && bands_equal(p, &band));
            let state = if same {
                DELIVERY_STATE_CONFIRMED
            } else {
                DELIVERY_STATE_DIFFERS
            };
            self.conn.execute(
                "UPDATE node_threshold_delivery
                    SET confirmed_low = ?4, confirmed_high = ?5, confirmed_enabled = ?6,
                        confirmed_at = ?7, state = ?8, updated_at = ?7,
                        sent_low = NULL, sent_high = NULL, sent_enabled = NULL
                  WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND field = ?3",
                params![
                    hostname,
                    node_key,
                    band.field,
                    band.low,
                    band.high,
                    band.enabled,
                    now,
                    state
                ],
            )?;
            if !same {
                differs.push(band);
            }
        }
        Ok(differs)
    }

    /// Nodes of one Hub with a band in `differs` — the ones whose warning must be open.
    pub fn node_delivery_differs(&self, hostname: &str, node_key: &str) -> Result<Vec<String>> {
        Ok(self
            .list_node_delivery(hostname, node_key)?
            .into_iter()
            .filter(|d| d.state == DELIVERY_STATE_DIFFERS)
            .map(|d| d.field)
            .collect())
    }

    /// One field of a discarded send (a Discard that reverted only part of the
    /// draft, PF-3 review): as [`Self::forget_node_delivery_attempt`], for it
    /// alone.
    pub fn forget_node_delivery_field(
        &self,
        hostname: &str,
        node_key: &str,
        field: &str,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM node_threshold_delivery
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND field = ?3
                AND state IN (?4, ?5) AND confirmed_at IS NULL",
            params![
                hostname,
                node_key,
                field,
                DELIVERY_STATE_PENDING,
                DELIVERY_STATE_UNDELIVERED
            ],
        )?;
        self.conn.execute(
            "UPDATE node_threshold_delivery SET state = ?6, updated_at = ?7,
                    sent_low = NULL, sent_high = NULL, sent_enabled = NULL
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND field = ?3
                AND state IN (?4, ?5)",
            params![
                hostname,
                node_key,
                field,
                DELIVERY_STATE_PENDING,
                DELIVERY_STATE_UNDELIVERED,
                DELIVERY_STATE_CONFIRMED,
                now_unix()
            ],
        )?;
        Ok(())
    }

    /// The Node is gone: so is the record of what it held.
    pub fn delete_node_delivery(&self, hostname: &str, node_key: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM node_threshold_delivery
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2",
            params![hostname, node_key],
        )?)
    }

    /// After a restart nothing waits for a `pending` table any more: every one
    /// is `undelivered`, as its draft rows are (`reset_sending_to_error`).
    pub fn reset_node_delivery_pending(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE node_threshold_delivery SET state = ?1, updated_at = ?2 WHERE state = ?3",
            params![
                DELIVERY_STATE_UNDELIVERED,
                now_unix(),
                DELIVERY_STATE_PENDING
            ],
        )?)
    }
}

fn bands_equal(a: &NodeBand, b: &NodeBand) -> bool {
    let same = |x: Option<f64>, y: Option<f64>| match (x, y) {
        (Some(x), Some(y)) => (x - y).abs() < 1e-6,
        (None, None) => true,
        _ => false,
    };
    same(a.low, b.low) && same(a.high, b.high) && a.enabled == b.enabled
}

fn delivery_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NodeThresholdDelivery> {
    Ok(NodeThresholdDelivery {
        hostname: r.get(0)?,
        node_key: r.get(1)?,
        field: r.get(2)?,
        confirmed_low: r.get(3)?,
        confirmed_high: r.get(4)?,
        confirmed_enabled: r.get(5)?,
        confirmed_at: r.get(6)?,
        state: r.get(7)?,
        updated_at: r.get(8)?,
        sent_low: r.get(9)?,
        sent_high: r.get(10)?,
        sent_enabled: r.get(11)?,
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
