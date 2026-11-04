//! The Node pending-configuration draft: `pending_config` (V13).
//!
//! devices.node.md §3.5 asks for a draft that is server-side, persistent and
//! shared: a manager edits a threshold or an interval, the value is written
//! here rather than relayed, and every other user's banner shows it with
//! who made it and when. One deliberate "send configuration" then walks the
//! draft; a unit the device confirms is deleted, a unit that fails keeps its
//! reason.
//!
//! Two rules the table itself cannot express, both enforced here:
//!
//! * **An edit reopens the draft.** [`Storage::upsert_pending`] resets
//!   `state`, `attempts` and `last_error` on conflict, so correcting a value
//!   that errored clears the error rather than leaving a fresh value wearing
//!   an old failure. `created_at` is *not* preserved across an edit — the
//!   banner's "changed by X at HH:MM" is about this value, not about the
//!   first time anyone touched this key.
//! * **`sending` is a lie after a restart.** Nothing outlives the process
//!   that was waiting for the device, so
//!   [`Storage::reset_sending_to_error`] runs at service construction and
//!   turns every stranded `sending` row into an `error` the operator can
//!   see and resend — or into `undelivered` for a row of the Node's part,
//!   whose Hub half already landed (V25, PF-3).
//! * **`previous_value` is written once** (V25, PF-3). It records what the
//!   Hub held before a send wrote this row's value there, which is what a
//!   Discard puts back; an edit or a resend must not replace it with a value
//!   that is itself only the draft's.
//!
//! There is no history table. The `audit_log` row the send route writes is
//! the trail for who pushed a configuration to a device (devices.backend.md
//! §3, WP0.1); a draft edit is not yet a change to anything.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::models::{
    NewFieldThreshold, NewPendingConfig, PendingConfigRow, PENDING_STATE_PENDING,
    PENDING_STATE_UNDELIVERED,
};
use crate::Storage;

/// One field's Hub write as [`Storage::land_hub_threshold`] records it.
#[derive(Debug, Clone, Copy)]
pub struct HubLanding<'a> {
    /// The rows only the Hub carries, with the value sent.
    pub hub_rows: &'a [(String, Option<String>)],
    /// The field's rows riding the Node unit: key, value sent, bound before.
    pub node_rows: &'a [(String, Option<String>, String)],
    /// The field's band before this send, as JSON.
    pub previous_band: Option<&'a str>,
    /// What `field_thresholds` mirrors; `None` writes no threshold row.
    pub threshold: Option<&'a NewFieldThreshold>,
    pub actor: Option<&'a str>,
}

/// What [`Storage::revert_pending_to_held`] did to the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldRevert {
    /// The Hub had not taken it: the draft row went (N17).
    Dropped,
    /// The Hub had not taken it, and a walk is writing the Hub: kept as
    /// `pending` with the held value.
    Kept,
    /// The Hub holds the value and the Node does not: `undelivered` again.
    Undelivered,
}

const PENDING_COLUMNS: &str = "hostname, node_key, param_key, kind, desired_value, author, \
     created_at, state, attempts, last_error, updated_at, previous_value, previous_band, \
     previous_order";

impl Storage {
    /// Writes one desired value into the draft, replacing whatever that key
    /// held.
    ///
    /// The conflict branch resets the send machine (`state = 'pending'`,
    /// `attempts = 0`, `last_error = NULL`) and re-stamps both timestamps:
    /// per the decision of 2026-09-07 any manager may overwrite another's
    /// pending value, and the banner then shows the *new* author and time
    /// with no dialog. A row that was mid-`sending` is overwritten too, and
    /// the walk finds out: every state write and the final delete it makes
    /// carries the `desired_value` it planned with (see
    /// [`Storage::set_pending_state`]), so an edit that lands under a walk
    /// makes the next conditional write miss and stops it.
    ///
    /// `previous_value` survives the edit (V25): the Hub still holds what an
    /// earlier send wrote, and a Discard must still know what came before it.
    pub fn upsert_pending(&self, row: &NewPendingConfig) -> Result<()> {
        let now = now_unix();
        // A Hub arrives under more than one spelling (`FIBER-…` from the agent, `fiber-…` from
        // the v2 topics, and since #94 `DeviceState::hostname` follows the latest), and every
        // read here matches the hostname case-insensitively (NT-4, night test 2026-09-26). The
        // same key under another spelling is replaced, not doubled: the conflict target below
        // compares exactly. Renamed first where it can be, so the row's `previous_value` (V25)
        // follows it to the new spelling; only a row both spellings already hold is dropped.
        self.conn.execute(
            "UPDATE OR IGNORE pending_config SET hostname = ?1
              WHERE hostname = ?1 COLLATE NOCASE AND hostname <> ?1
                AND node_key = ?2 AND param_key = ?3",
            params![row.hostname, row.node_key, row.param_key],
        )?;
        self.conn.execute(
            "DELETE FROM pending_config
              WHERE hostname = ?1 COLLATE NOCASE AND hostname <> ?1
                AND node_key = ?2 AND param_key = ?3",
            params![row.hostname, row.node_key, row.param_key],
        )?;
        self.conn.execute(
            "INSERT INTO pending_config
                (hostname, node_key, param_key, kind, desired_value, author,
                 created_at, state, attempts, last_error, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, NULL, ?7)
             ON CONFLICT (hostname, node_key, param_key) DO UPDATE SET
                kind = excluded.kind,
                desired_value = excluded.desired_value,
                author = excluded.author,
                created_at = excluded.created_at,
                state = excluded.state,
                attempts = 0,
                last_error = NULL,
                updated_at = excluded.updated_at",
            params![
                row.hostname,
                row.node_key,
                row.param_key,
                row.kind,
                row.desired_value,
                row.author,
                now,
                PENDING_STATE_PENDING,
            ],
        )?;
        Ok(())
    }

    /// Writes a draft row only where the key has none, under any spelling of
    /// the Hub (PF-3b review): the portal's own re-draft must never overwrite
    /// what an operator drafted. `true` = written.
    pub fn insert_pending_if_absent(&self, row: &NewPendingConfig) -> Result<bool> {
        let now = now_unix();
        let written = self.conn.execute(
            "INSERT INTO pending_config
                (hostname, node_key, param_key, kind, desired_value, author,
                 created_at, state, attempts, last_error, updated_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, NULL, ?7
              WHERE NOT EXISTS (
                SELECT 1 FROM pending_config
                 WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3)",
            params![
                row.hostname,
                row.node_key,
                row.param_key,
                row.kind,
                row.desired_value,
                row.author,
                now,
                PENDING_STATE_PENDING,
            ],
        )?;
        Ok(written > 0)
    }

    /// One Node's whole draft, ordered by `param_key` so the banner's rows
    /// do not reshuffle between two renders of the same draft.
    pub fn list_pending(&self, hostname: &str, node_key: &str) -> Result<Vec<PendingConfigRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PENDING_COLUMNS} FROM pending_config
             WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2
             ORDER BY param_key ASC"
        ))?;
        let rows = stmt.query_map(params![hostname, node_key], pending_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every Node's draft on one Hub, keyed by `node_key` — the `/limits`
    /// page's per-channel state, as one query rather than one per Node.
    pub fn list_pending_by_node(
        &self,
        hostname: &str,
    ) -> Result<HashMap<String, Vec<PendingConfigRow>>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PENDING_COLUMNS} FROM pending_config
             WHERE hostname = ?1
             ORDER BY node_key ASC, param_key ASC"
        ))?;
        let rows = stmt.query_map(params![hostname], pending_from_row)?;
        let mut out: HashMap<String, Vec<PendingConfigRow>> = HashMap::new();
        for row in rows {
            let row = row?;
            out.entry(row.node_key.clone()).or_default().push(row);
        }
        Ok(out)
    }

    /// How many draft rows each Node on one Hub is carrying — the `/nodes`
    /// list's pending badge, as **one** grouped query rather than one per
    /// row (§4's performance gate). Nodes with no draft are absent from the
    /// map, which is what the template's `default(0)` reads.
    pub fn count_pending_by_node(&self, hostname: &str) -> Result<HashMap<String, i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT node_key, COUNT(*) FROM pending_config
             WHERE hostname = ?1 COLLATE NOCASE
             GROUP BY node_key",
        )?;
        let rows = stmt.query_map(params![hostname], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (key, count) = row?;
            out.insert(key, count);
        }
        Ok(out)
    }

    /// Moves one unit through the send machine. `error` carries the reason
    /// (`None` clears it); `bump_attempts` is set on the transition into
    /// `sending`, so `attempts` counts relay publishes rather than state
    /// writes.
    ///
    /// **`expect_value` is the walk's re-read** (§21 review, defect 3).
    /// `Some(v)` matches only a row still holding exactly `v` in
    /// `desired_value` — including `Some(None)` for a row whose value is
    /// SQL NULL — so a walk that planned one value cannot mark, fail or
    /// otherwise speak for a row somebody has since edited. `None` matches
    /// the row whatever it holds, for callers that are not walking anything.
    ///
    /// `false` = no row matched, which a walk reads as **stop**: either a
    /// Discard removed the row or an edit replaced the value under it, and
    /// in both cases what the walk is holding is no longer what the operator
    /// asked for.
    // Eight, and each one is load-bearing: three identify the row, one is
    // the walk's re-read, and three are the write. Grouping them into a
    // struct would only move the same list one indirection away.
    #[allow(clippy::too_many_arguments)]
    pub fn set_pending_state(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
        expect_value: Option<Option<&str>>,
        state: &str,
        error: Option<&str>,
        bump_attempts: bool,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE pending_config
                SET state = ?4,
                    last_error = ?5,
                    attempts = attempts + ?6,
                    updated_at = ?7
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3
                AND (?8 = 0 OR desired_value IS ?9)",
            params![
                hostname,
                node_key,
                param_key,
                state,
                error,
                i64::from(bump_attempts),
                now_unix(),
                i64::from(expect_value.is_some()),
                expect_value.flatten(),
            ],
        )?;
        Ok(changed > 0)
    }

    /// Drops one confirmed (or discarded) unit; `false` = there was none.
    ///
    /// `expect_value` is [`Self::set_pending_state`]'s, and it matters most
    /// here: a walk deletes a row on the strength of a device confirmation,
    /// and confirming the value that was *sent* must never delete the newer
    /// value somebody typed while the walk was waiting.
    pub fn delete_pending(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
        expect_value: Option<Option<&str>>,
    ) -> Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM pending_config
             WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3
               AND (?4 = 0 OR desired_value IS ?5)",
            params![
                hostname,
                node_key,
                param_key,
                i64::from(expect_value.is_some()),
                expect_value.flatten(),
            ],
        )?;
        Ok(deleted > 0)
    }

    /// Records what the Hub held for this row's bound before a send wrote the
    /// drafted value there (V25, PF-3). **Only the first call counts**: a
    /// resend, or a second walk after an edit, finds the Hub holding the
    /// draft's own earlier value, and writing that here would make a Discard
    /// restore the draft instead of what the Hub had. `expect_value` is
    /// [`Self::set_pending_state`]'s re-read. `false` = no row matched.
    pub fn set_pending_previous(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
        expect_value: Option<Option<&str>>,
        previous: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE pending_config
                SET previous_value = COALESCE(previous_value, ?4)
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3
                AND (?5 = 0 OR desired_value IS ?6)",
            params![
                hostname,
                node_key,
                param_key,
                previous,
                i64::from(expect_value.is_some()),
                expect_value.flatten(),
            ],
        )?;
        Ok(changed > 0)
    }

    /// An edit that puts a bound back to what the portal holds (N17 with PF-3).
    ///
    /// A row the Hub has not taken yet goes, as before. A row the Hub already
    /// took (`previous_value` set) stays: the portal holds `held` only since
    /// the Hub does, and the Node still waits for it, so the row takes `held`
    /// back and is `undelivered` again — the Hub has it, the Node does not
    /// (PF-3 review, item 4: edited away and back, the draft kept the detour).
    /// `keep_unsent` keeps an untaken row too, as `pending` with `held`: while
    /// a walk is writing the Hub it must survive for the walk to record what
    /// the Hub held on it.
    pub fn revert_pending_to_held(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
        held: &str,
        keep_unsent: bool,
    ) -> Result<Option<HeldRevert>> {
        let taken: Option<bool> = self
            .conn
            .query_row(
                "SELECT previous_value IS NOT NULL FROM pending_config
                 WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3",
                params![hostname, node_key, param_key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(taken) = taken else {
            return Ok(None);
        };
        if !taken && !keep_unsent {
            self.delete_pending(hostname, node_key, param_key, None)?;
            return Ok(Some(HeldRevert::Dropped));
        }
        let (state, note) = if taken {
            (
                PENDING_STATE_UNDELIVERED,
                Some("the Hub holds this value; the Node does not yet"),
            )
        } else {
            (PENDING_STATE_PENDING, None)
        };
        self.conn.execute(
            "UPDATE pending_config
                SET desired_value = ?4, state = ?5, last_error = ?6, attempts = 0,
                    updated_at = ?7
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3",
            params![hostname, node_key, param_key, held, state, note, now_unix()],
        )?;
        Ok(Some(if taken {
            HeldRevert::Undelivered
        } else {
            HeldRevert::Kept
        }))
    }

    /// The Hub took one field's band (PF-3 review, item 1), recorded as one
    /// transaction so no edit or Discard lands between its halves:
    ///
    /// * `node_rows` — the field's rows that ride the Node unit — get what
    ///   the Hub held before (`previous_value`, `previous_band`), **whatever
    ///   they hold now**: an edit while the Hub was being written is a new
    ///   intention, and a Discard of it must still find the revert point;
    /// * `field_thresholds` mirrors the Hub, which holds `threshold` now;
    /// * `hub_rows`, the rows only the Hub carries, go where they still hold
    ///   the value sent.
    ///
    /// **The revert point is the oldest row's** (PF-3 review rounds 2 and 3):
    /// each row keeps the band of its own first landing with its landing
    /// order (`previous_order`), and a Discard reverts the field to the band of
    /// its oldest row still in the draft. A second send that only added a
    /// bound cannot move it to the band between the sends; a late ack that
    /// finishes the first send's rows moves it on to the band after it — the
    /// one the Node now confirmed.
    ///
    /// `true` = every one of those rows still held the value the walk sent,
    /// so its plan is still the draft; `false` stops the walk.
    pub fn land_hub_threshold(
        &self,
        hostname: &str,
        node_key: &str,
        landing: &HubLanding<'_>,
    ) -> Result<bool> {
        let HubLanding {
            hub_rows,
            node_rows,
            previous_band,
            threshold,
            actor,
        } = *landing;
        let tx = self.conn.unchecked_transaction()?;
        let order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(previous_order), 0) + 1 FROM pending_config",
            [],
            |r| r.get(0),
        )?;
        let mut held: HashMap<String, Option<String>> = HashMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT param_key, desired_value FROM pending_config
                 WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2",
            )?;
            let rows = stmt.query_map(params![hostname, node_key], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (key, value) = row?;
                held.insert(key, value);
            }
        }
        let as_sent = |key: &String, value: &Option<String>| held.get(key) == Some(value);
        let unchanged = hub_rows.iter().all(|(k, v)| as_sent(k, v))
            && node_rows.iter().all(|(k, v, _)| as_sent(k, v));
        for (key, _, previous) in node_rows {
            tx.execute(
                "UPDATE pending_config
                    SET previous_value = COALESCE(previous_value, ?4),
                        previous_band = COALESCE(previous_band, ?5),
                        previous_order = COALESCE(previous_order, ?6)
                  WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3",
                params![hostname, node_key, key, previous, previous_band, order],
            )?;
        }
        if let Some(threshold) = threshold {
            self.upsert_field_threshold_in(&tx, threshold, actor)?;
        }
        for (key, value) in hub_rows {
            tx.execute(
                "DELETE FROM pending_config
                 WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3
                   AND desired_value IS ?4",
                params![hostname, node_key, key, value],
            )?;
        }
        tx.commit()?;
        Ok(unchanged)
    }

    /// Every row in one state, across all Nodes — the start-up pass that
    /// reads what a restart is about to turn from `sending` (PF-3 review).
    pub fn list_pending_in_state(&self, state: &str) -> Result<Vec<PendingConfigRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PENDING_COLUMNS} FROM pending_config WHERE state = ?1
             ORDER BY hostname, node_key, param_key"
        ))?;
        let rows = stmt.query_map(params![state], pending_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// A walk dropped mid-unit (the interrupt guard): the row ends as a
    /// restart would end it — `undelivered` for the Node's part (a config
    /// key, or a bound the Hub already took), `error` otherwise. Guarded by
    /// `expect_value` like every walk write.
    pub fn mark_pending_interrupted(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
        expect_value: Option<Option<&str>>,
        message: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE pending_config
                SET state = CASE
                        WHEN kind = 'config' OR previous_value IS NOT NULL THEN 'undelivered'
                        ELSE 'error'
                    END,
                    last_error = ?4, updated_at = ?5
              WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2 AND param_key = ?3
                AND (?6 = 0 OR desired_value IS ?7)",
            params![
                hostname,
                node_key,
                param_key,
                message,
                now_unix(),
                i64::from(expect_value.is_some()),
                expect_value.flatten(),
            ],
        )?;
        Ok(changed > 0)
    }

    /// Every `(hostname, node_key)` with a row in `undelivered` (PF-3): the
    /// Nodes whose "configuration not delivered" warning must be open, for
    /// the sweep that re-checks them after a start.
    pub fn list_undelivered_nodes(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT hostname, node_key FROM pending_config
             WHERE state = 'undelivered' ORDER BY hostname, node_key",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Drops one Node's whole draft — the Discard button, and the cleanup
    /// that rides a Node removal beside `delete_device_meta` (§19, F7: a
    /// draft left behind is inherited by the next device registered at that
    /// DevEUI). Returns how many rows went.
    pub fn delete_pending_for_node(&self, hostname: &str, node_key: &str) -> Result<usize> {
        let deleted = self.conn.execute(
            "DELETE FROM pending_config WHERE hostname = ?1 COLLATE NOCASE AND node_key = ?2",
            params![hostname, node_key],
        )?;
        Ok(deleted)
    }

    /// Drops every threshold draft row of one field — its bounds and its `enabled` — on
    /// every Node. For a field that stopped being thresholdable (the battery,
    /// plan/portal/threshold_defaults.md §5): a Send would push a band the Hub refuses.
    /// Returns how many rows went.
    pub fn delete_pending_threshold_field(&self, field: &str) -> Result<usize> {
        let prefix = format!("threshold.{field}.");
        let deleted = self.conn.execute(
            "DELETE FROM pending_config
             WHERE kind = ?1 AND substr(param_key, 1, length(?2)) = ?2",
            params![crate::models::PENDING_KIND_THRESHOLD, prefix],
        )?;
        Ok(deleted)
    }

    /// Turns every `sending` row in the database into an `error` carrying
    /// `reason` — or into `undelivered` for a row of the Node's part (PF-3):
    /// a config key, or a threshold row whose Hub half already landed
    /// (`previous_value` set). Those were waiting for the Node, which may
    /// never have got them, while the Hub and the portal hold the value.
    ///
    /// Run once at `PendingService` construction. `sending` means "relayed,
    /// waiting for the device" and the thing that was waiting was a task in
    /// a process that is gone, so after a restart the state is unrecoverable
    /// rather than merely stale: nothing will ever confirm it, and leaving
    /// it would show a spinner that never resolves. Returns how many rows
    /// were stranded.
    pub fn reset_sending_to_error(&self, reason: &str) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE pending_config
                SET state = CASE
                        WHEN kind = 'config' OR previous_value IS NOT NULL THEN 'undelivered'
                        ELSE 'error'
                    END,
                    last_error = ?1, updated_at = ?2
              WHERE state = 'sending'",
            params![reason, now_unix()],
        )?;
        Ok(changed)
    }
}

fn pending_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingConfigRow> {
    Ok(PendingConfigRow {
        hostname: r.get(0)?,
        node_key: r.get(1)?,
        param_key: r.get(2)?,
        kind: r.get(3)?,
        desired_value: r.get(4)?,
        author: r.get(5)?,
        created_at: r.get(6)?,
        state: r.get(7)?,
        attempts: r.get(8)?,
        last_error: r.get(9)?,
        updated_at: r.get(10)?,
        previous_value: r.get(11)?,
        previous_band: r.get(12)?,
        previous_order: r.get(13)?,
    })
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
