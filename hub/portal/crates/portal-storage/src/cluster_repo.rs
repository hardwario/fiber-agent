//! The Server's Client Hub registry: `cluster_clients` (V28;
//! plan/portal/devices.hub.md §12 J, plan/control/cluster.md §7, §9).
//!
//! One row per Client Hub an admin registered with Add hub. The row never
//! holds the Client's broker password — there is no column for it (the
//! migration header says why) — and nothing here takes one as an argument.
//!
//! The state machine (cluster.md §7, §9):
//!
//! ```text
//!   insert ──► failed ("did not finish")
//!                │ answer: account ok, gateway ok|skipped ──► pending
//!                │ answer: anything else / no answer      ──► failed (+ reason)
//!   pending ──► complete            the Client's status reached this broker
//!   any     ──► failed | pending    Retry re-runs the operation
//!   any     ──► as it was           a Retry that never reached the agent
//! ```
//!
//! A completed removal deletes the row; a failed one keeps it and records
//! `removal_error`.

use rusqlite::{params, OptionalExtension};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::Storage;

/// Both effects of `cluster-client-add` were good; the Client has not yet
/// been seen on this Server's broker.
pub const CLUSTER_STATE_PENDING: &str = "pending";
/// "Registration incomplete": an effect failed, or no answer came.
pub const CLUSTER_STATE_FAILED: &str = "failed";
/// The Client's agent `status` reached this Server's broker.
pub const CLUSTER_STATE_COMPLETE: &str = "complete";

/// One `cluster_clients` row.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ClusterClient {
    /// `proximos-<8 hex>`, lowercase (decisions #17, #21).
    pub hostname: String,
    /// 16 lowercase hex, or `None` for a Client without a LoRaWAN gateway.
    pub gateway_eui: Option<String>,
    /// `peer-<hostname>`, as the agent reported it.
    pub peer_username: String,
    /// One of the `CLUSTER_STATE_*` constants.
    pub registration_state: String,
    /// Why the registration is incomplete; `None` unless `failed`.
    pub registration_error: Option<String>,
    /// Why the last removal did not complete; `None` when none failed.
    pub removal_error: Option<String>,
    /// `datetime('now')` of the first registration. A Retry keeps it.
    pub registered_at: String,
    /// Username of the admin who first registered it.
    pub registered_by: Option<String>,
    /// `datetime('now')` of the latest Add hub / Retry.
    pub last_attempt_at: String,
    /// When the Client was first seen after its latest attempt.
    pub completed_at: Option<String>,
}

/// What [`Storage::insert_cluster_client`] needs. Inserted as `failed` with
/// `registration_error`, so a portal that stops before the answer leaves an
/// honest row (V28 header).
#[derive(Debug, Clone, PartialEq)]
pub struct NewClusterClient {
    pub hostname: String,
    pub gateway_eui: Option<String>,
    pub peer_username: String,
    pub registered_by: Option<String>,
    pub registration_error: String,
}

/// The outcome of [`Storage::insert_cluster_client`]. A duplicate is an
/// answer, not an error: the form shows it at the field it names
/// (devices.hub.md §4 step 6) and nothing is written.
#[derive(Debug, Clone, PartialEq)]
pub enum ClusterInsert {
    /// Boxed: the row is many times the size of the two refusals.
    Inserted(Box<ClusterClient>),
    HostnameTaken,
    GatewayEuiTaken,
}

const COLUMNS: &str = "hostname, gateway_eui, peer_username, registration_state, \
     registration_error, removal_error, registered_at, registered_by, last_attempt_at, \
     completed_at";

fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ClusterClient> {
    Ok(ClusterClient {
        hostname: r.get(0)?,
        gateway_eui: r.get(1)?,
        peer_username: r.get(2)?,
        registration_state: r.get(3)?,
        registration_error: r.get(4)?,
        removal_error: r.get(5)?,
        registered_at: r.get(6)?,
        registered_by: r.get(7)?,
        last_attempt_at: r.get(8)?,
        completed_at: r.get(9)?,
    })
}

/// The duplicate a key violation at insert names, or `None` for any other
/// error. The primary key is the hostname; of the UNIQUE columns only
/// `gateway_eui` is not, and SQLite names the column in the message
/// ("UNIQUE constraint failed: cluster_clients.gateway_eui").
fn duplicate_of(e: &rusqlite::Error) -> Option<ClusterInsert> {
    let rusqlite::Error::SqliteFailure(f, message) = e else {
        return None;
    };
    match f.extended_code {
        rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => Some(ClusterInsert::HostnameTaken),
        rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
            if message
                .as_deref()
                .is_some_and(|m| m.contains("cluster_clients.gateway_eui")) =>
        {
            Some(ClusterInsert::GatewayEuiTaken)
        }
        rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => Some(ClusterInsert::HostnameTaken),
        _ => None,
    }
}

impl Storage {
    /// Every registered Client, oldest registration first.
    pub fn list_cluster_clients(&self) -> Result<Vec<ClusterClient>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM cluster_clients ORDER BY registered_at ASC, hostname ASC"
        ))?;
        let rows = stmt.query_map([], from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One Client by its (lowercase) hostname.
    pub fn get_cluster_client(&self, hostname: &str) -> Result<Option<ClusterClient>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM cluster_clients WHERE hostname = ?1"),
                params![hostname],
                from_row,
            )
            .optional()?)
    }

    /// Registers a new Client, refusing a hostname or gateway EUI that is
    /// already in the registry. The check and the insert run in one
    /// transaction, and the table's keys back it up, so two admins
    /// registering the same Client at once get one row and one duplicate.
    pub fn insert_cluster_client(&self, new: &NewClusterClient) -> Result<ClusterInsert> {
        let tx = self.conn.unchecked_transaction()?;
        let hostname_taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM cluster_clients WHERE hostname = ?1)",
            params![new.hostname],
            |r| r.get(0),
        )?;
        if hostname_taken {
            return Ok(ClusterInsert::HostnameTaken);
        }
        if let Some(eui) = &new.gateway_eui {
            let eui_taken: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM cluster_clients WHERE gateway_eui = ?1)",
                params![eui],
                |r| r.get(0),
            )?;
            if eui_taken {
                return Ok(ClusterInsert::GatewayEuiTaken);
            }
        }
        let inserted = tx.execute(
            "INSERT INTO cluster_clients
                (hostname, gateway_eui, peer_username, registration_state,
                 registration_error, registered_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                new.hostname,
                new.gateway_eui,
                new.peer_username,
                CLUSTER_STATE_FAILED,
                new.registration_error,
                new.registered_by,
            ],
        );
        match inserted {
            Ok(_) => {}
            // Another connection won between the check and the insert: say which key it took.
            Err(e) => return duplicate_of(&e).map_or_else(|| Err(e.into()), Ok),
        }
        let row = tx.query_row(
            &format!("SELECT {COLUMNS} FROM cluster_clients WHERE hostname = ?1"),
            params![new.hostname],
            from_row,
        )?;
        tx.commit()?;
        Ok(ClusterInsert::Inserted(Box::new(row)))
    }

    /// A Retry is starting: stamp `last_attempt_at` and mark the row
    /// `failed` with `reason` until the answer arrives, the same honest
    /// interim state an insert has. Clears `completed_at`, because a re-run
    /// rotates the password and the Client has to join again. `gateway_eui`
    /// replaces the row's EUI when set (#163), and keeps it when `None`.
    /// `None` = no such row.
    pub fn begin_cluster_attempt(
        &self,
        hostname: &str,
        reason: &str,
        gateway_eui: Option<&str>,
    ) -> Result<Option<ClusterClient>> {
        self.conn.execute(
            "UPDATE cluster_clients
                SET registration_state = ?2, registration_error = ?3,
                    last_attempt_at = datetime('now'), completed_at = NULL,
                    gateway_eui = COALESCE(?4, gateway_eui)
              WHERE hostname = ?1",
            params![hostname, CLUSTER_STATE_FAILED, reason, gateway_eui],
        )?;
        self.get_cluster_client(hostname)
    }

    /// A Retry that never reached the agent: nothing was minted or rotated, so
    /// the row goes back to what [`Self::begin_cluster_attempt`] found —
    /// state, reason, attempt time, completion and gateway EUI. Without this a refusal
    /// before signing would leave a working Client marked `failed` for good.
    /// `None` = no such row.
    pub fn restore_cluster_attempt(
        &self,
        previous: &ClusterClient,
    ) -> Result<Option<ClusterClient>> {
        self.conn.execute(
            "UPDATE cluster_clients
                SET registration_state = ?2, registration_error = ?3,
                    last_attempt_at = ?4, completed_at = ?5, gateway_eui = ?6
              WHERE hostname = ?1",
            params![
                previous.hostname,
                previous.registration_state,
                previous.registration_error,
                previous.last_attempt_at,
                previous.completed_at,
                previous.gateway_eui,
            ],
        )?;
        self.get_cluster_client(&previous.hostname)
    }

    /// Records the agent's answer to `cluster-client-add`: `pending` when
    /// both effects were good (`error = None`), `failed` with the reason
    /// otherwise. `peer_username` is the account the agent reports.
    pub fn record_cluster_result(
        &self,
        hostname: &str,
        peer_username: &str,
        error: Option<&str>,
    ) -> Result<Option<ClusterClient>> {
        let state = if error.is_none() {
            CLUSTER_STATE_PENDING
        } else {
            CLUSTER_STATE_FAILED
        };
        self.conn.execute(
            "UPDATE cluster_clients
                SET registration_state = ?2, registration_error = ?3, peer_username = ?4,
                    removal_error = NULL
              WHERE hostname = ?1",
            params![hostname, state, error, peer_username],
        )?;
        self.get_cluster_client(hostname)
    }

    /// `pending` → `complete`, the first time the Client's status reaches
    /// this broker after an attempt. `true` = the row moved. A `failed` row
    /// does not move: its registration is incomplete whatever the Client
    /// does, until a Retry makes it whole.
    pub fn mark_cluster_client_complete(&self, hostname: &str) -> Result<bool> {
        let moved = self.conn.execute(
            "UPDATE cluster_clients
                SET registration_state = ?2, completed_at = datetime('now')
              WHERE hostname = ?1 AND registration_state = ?3",
            params![hostname, CLUSTER_STATE_COMPLETE, CLUSTER_STATE_PENDING],
        )?;
        Ok(moved > 0)
    }

    /// The gateway an admin granted a registered Client (#164). `Ok(false)` = another row
    /// already holds that EUI (the table's unique key), and nothing changed.
    pub fn set_cluster_gateway_eui(&self, hostname: &str, eui: &str) -> Result<bool> {
        match self.conn.execute(
            "UPDATE cluster_clients SET gateway_eui = ?2 WHERE hostname = ?1",
            params![hostname, eui],
        ) {
            Ok(_) => Ok(true),
            Err(e) if matches!(duplicate_of(&e), Some(ClusterInsert::GatewayEuiTaken)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// A removal did not complete: keep the row and say why.
    pub fn record_cluster_removal_error(
        &self,
        hostname: &str,
        error: &str,
    ) -> Result<Option<ClusterClient>> {
        self.conn.execute(
            "UPDATE cluster_clients SET removal_error = ?2 WHERE hostname = ?1",
            params![hostname, error],
        )?;
        self.get_cluster_client(hostname)
    }

    /// A completed removal: the row goes. `false` = there was none.
    pub fn delete_cluster_client(&self, hostname: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM cluster_clients WHERE hostname = ?1",
            params![hostname],
        )? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key violation a racing insert hits names its column: a gateway
    /// EUI clash is "EUI taken", not "hostname taken". The errors are the
    /// real ones, from inserts that skip the pre-check.
    #[test]
    fn a_key_violation_names_the_key_it_hit() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(&dir.path().join("portal.db"), None).unwrap();
        let raw = |hostname: &str, eui: &str| {
            s.conn
                .execute(
                    "INSERT INTO cluster_clients
                        (hostname, gateway_eui, peer_username, registration_state)
                     VALUES (?1, ?2, 'peer', 'failed')",
                    params![hostname, eui],
                )
                .unwrap_err()
        };
        s.conn
            .execute(
                "INSERT INTO cluster_clients
                    (hostname, gateway_eui, peer_username, registration_state)
                 VALUES ('proximos-1a2b3c4d', '70b3d54900aa0002', 'peer', 'failed')",
                [],
            )
            .unwrap();

        let same_eui = raw("proximos-00000009", "70b3d54900aa0002");
        assert_eq!(
            duplicate_of(&same_eui),
            Some(ClusterInsert::GatewayEuiTaken),
            "{same_eui}"
        );
        let same_host = raw("proximos-1a2b3c4d", "70b3d54900aa0009");
        assert_eq!(
            duplicate_of(&same_host),
            Some(ClusterInsert::HostnameTaken),
            "{same_host}"
        );
        // A CHECK failure is not a duplicate.
        let bad = raw("fiber-1a2b3c4d", "70b3d54900aa0009");
        assert_eq!(duplicate_of(&bad), None, "{bad}");
    }
}
