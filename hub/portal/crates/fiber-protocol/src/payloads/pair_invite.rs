//! `{prefix}/{host}/pair/invite` — QoS 1, **not retained**. The device asking the portal on the
//! same machine to run its pairing flow, so the operator does not need the LAN.
//!
//! GitLab #61, `plan/control/ble/peripheral.md` §6.5. The direction is the point: every other
//! pairing topic is portal → device (`pair/request`) or device → portal in reply
//! (`pair/response`). This one exists because the party holding the operator's attention is the
//! phone, over BLE, and the phone cannot reach the portal's HTTP surface.
//!
//! **Never retained.** A retained invite would re-pair the portal on every reconnect, long after
//! the operator walked away and the front-panel window closed. It is a one-shot signal or it is
//! nothing.
//!
//! **The code is carried but is not a credential.** The device never compares it — it is the
//! PBKDF2 password the *response* is encrypted under, so a wrong one fails at the portal when it
//! cannot decrypt. It is `Option` because the portal drops it entirely for a device on its own
//! machine, which encrypts under the fixed local password instead.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairInviteMsg {
    /// The six-character front-panel pairing code, as the operator typed it into the phone.
    /// Absent when the caller has none to offer — the local path does not need one.
    #[serde(default)]
    pub code: Option<String>,
    /// Which surface asked, for the audit row. Free-form, because the set of surfaces that can ask
    /// is not this message's business to close.
    #[serde(default)]
    pub origin: Option<String>,
}
