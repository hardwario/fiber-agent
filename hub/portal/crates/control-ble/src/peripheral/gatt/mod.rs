//! The FB00 GATT server — `plan/control/ble/peripheral.md` §2–§5.
//!
//! **It is not an authorization boundary.** Under decision #10, FB00 is one of four front-ends onto
//! the one in-process authorization chokepoint. The session authenticates *the door*; every mutation
//! still passes the chokepoint, and BLE is no more trusted than MQTT was. A session-gated write is
//! not a pre-authorized one.
//!
//! Every handler here is a **pure function** of `(ctx, peer, mtu, offset, value)`, so the whole
//! surface is unit-testable without BlueZ. The `bluer` closures in [`app`] do nothing but extract
//! `device_address`, `mtu` and `offset` from the request and call these.
//!
//! Two contract defects peripheral.md §5 settles, both enforced here:
//!
//! - **§5.1 — a refusal that cannot fit is an ATT-level error, not a payload.** v1's MTU-too-small
//!   refusal was itself 131 bytes and fired only below 107, so at the 23-byte default the phone
//!   received `{"error":"mtu_too_smal` — *the guard emitted exactly what it existed to prevent.* No
//!   JSON is short enough to help at MTU 23, so every read path checks its own MTU and returns
//!   `ReqError` when the answer will not fit.
//! - **§5.2 — every read honours the offset.** Both v1 read paths ignored it, so a central issuing a
//!   READ_BLOB at offset 22 received the whole value from byte 0 again and concatenated a duplicated
//!   prefix onto what it already had: silent corruption, worse than a failure.

pub mod app;
pub mod auth;
pub mod beacon_add;
pub mod beacon_list;
pub mod beacon_remove;
pub mod cluster_join;
pub mod device_info;
pub mod export_destination;
pub mod label;
pub mod lan;
pub mod node_add;
pub mod node_list;
pub mod node_remove;
pub mod pairing_code;
pub mod radio_mode;
pub mod setup;
pub mod time;
pub mod wifi;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use bluer::gatt::local::ReqError;
use bluer::Uuid;

use crate::central::adapter::AdapterSlot;
use crate::central::state::CentralState;
use crate::config::PeripheralConfig;
use crate::host::{Host, NodeAddResponse};
use crate::mac::MacAddr;

use super::buffers::{PeerAddr, ResponseBuffers};
use super::events::EventBus;
use super::session::SessionSlot;

/// The FB00 service and its characteristics: 16-bit aliases in the Bluetooth base UUID.
pub const fn fb(alias: u16) -> Uuid {
    Uuid::from_u128(0x0000_0000_0000_1000_8000_0080_5f9b_34fb | ((alias as u128) << 96))
}

pub const SVC_FB00: u16 = 0xFB00;
pub const CH_AUTH: u16 = 0xFB01;
pub const CH_WIFI_SCAN: u16 = 0xFB02;
pub const CH_WIFI_CONNECT: u16 = 0xFB03;
pub const CH_WIFI_STATUS: u16 = 0xFB04;
// FB05/FB06 are DELETED, not defaulted off: v1 spawned a PTY-backed shell over BLE and omitted the
// characteristics when a config key said so. CP-1 removed the capability outright, so there is no
// config key in v2 because there is nothing to switch.
pub const CH_DEVICE_INFO: u16 = 0xFB07;
pub const CH_WIFI_DISCONNECT: u16 = 0xFB08;
pub const CH_LAN_CONFIG: u16 = 0xFB09;
pub const CH_DEVICE_LABEL: u16 = 0xFB0A;
pub const CH_TIME_SET: u16 = 0xFB0B;
pub const CH_LAN_STATUS: u16 = 0xFB0C;
pub const CH_NODE_ADD: u16 = 0xFB0D;
pub const CH_BEACON_ADD: u16 = 0xFB0E;
pub const CH_BEACON_LIST: u16 = 0xFB0F;
pub const CH_BEACON_REMOVE: u16 = 0xFB10;
pub const CH_NODE_LIST: u16 = 0xFB11;
pub const CH_NODE_REMOVE: u16 = 0xFB12;
// FB13–FB15 are RESERVED here even though their semantics belong to the radio set, so a parallel
// slice cannot claim a colliding number. They are deliberately **not registered**.
pub const CH_RESERVED_P2P_PAIRING: u16 = 0xFB13;
pub const CH_RESERVED_P2P_NODE_LIST: u16 = 0xFB14;
pub const CH_RESERVED_P2P_NODE_REMOVE: u16 = 0xFB15;
// FB16+ was "unallocated" until GitLab #58–#61 claimed the next five for the last of #10's asks
// (`plan/control/ble/peripheral.md` §2, amended 2026-09-16). The numbering steps over FB13–FB15
// rather than reclaiming them: the reservation exists so a parallel slice cannot collide, and
// spending it because it happened to be free is the one thing that would defeat it.
pub const CH_SETUP_STATE: u16 = 0xFB16;
pub const CH_SETUP_COMPLETE: u16 = 0xFB17;
pub const CH_EXPORT_DESTINATION: u16 = 0xFB18;
// FB19 is RETIRED, and its number stays spent. It served v1's one *shared* client broker password
// (`/data/mosquitto/fiber_password`, which nothing on a v2 Hub creates). Under
// `plan/control/cluster.md` (decision #38) every Client Hub has its own account, minted on the
// Server by `cluster-client-add`, and a credential leaves a Server only through the portal's
// audited Add-hub flow, for one named Client — never as a read any session can repeat. Not
// registered, and never reallocated: an app built against the old table that still reads FB19
// must get "no such characteristic", not some later characteristic's answer.
pub const CH_RETIRED_CONNECTION_INFO: u16 = 0xFB19;
pub const CH_PAIRING_CODE: u16 = 0xFB1A;
// FB1B: the radio mode, read and write — the provisioning choice of
// `plan/control/radio/node_registry.md` §12 (peripheral.md §2, amended 2026-09-27).
pub const CH_RADIO_MODE: u16 = 0xFB1B;
// FB1C: the cluster join — a Client Hub joins a Server (peripheral.md §2 and §6.7, amended
// 2026-10-02; plan/control/cluster.md §8).
pub const CH_CLUSTER_JOIN: u16 = 0xFB1C;

/// Exactly the characteristics this build registers, in allocation order.
pub const REGISTERED: [u16; 22] = [
    CH_AUTH,
    CH_WIFI_SCAN,
    CH_WIFI_CONNECT,
    CH_WIFI_STATUS,
    CH_DEVICE_INFO,
    CH_WIFI_DISCONNECT,
    CH_LAN_CONFIG,
    CH_DEVICE_LABEL,
    CH_TIME_SET,
    CH_LAN_STATUS,
    CH_NODE_ADD,
    CH_BEACON_ADD,
    CH_BEACON_LIST,
    CH_BEACON_REMOVE,
    CH_NODE_LIST,
    CH_NODE_REMOVE,
    CH_SETUP_STATE,
    CH_SETUP_COMPLETE,
    CH_EXPORT_DESTINATION,
    CH_PAIRING_CODE,
    CH_RADIO_MODE,
    CH_CLUSTER_JOIN,
];

/// Reserved and deliberately absent from [`REGISTERED`].
pub const RESERVED: [u16; 3] = [
    CH_RESERVED_P2P_PAIRING,
    CH_RESERVED_P2P_NODE_LIST,
    CH_RESERVED_P2P_NODE_REMOVE,
];

/// Deleted in v2, and asserted absent so a future edit cannot quietly reinstate a shell over BLE.
pub const DELETED: [u16; 2] = [0xFB05, 0xFB06];

/// Built, served, and then withdrawn: absent from [`REGISTERED`] and never to be reallocated, so a
/// phone that still asks gets "no such characteristic" rather than another one's answer.
pub const RETIRED: [u16; 1] = [CH_RETIRED_CONNECTION_INFO];

/// The write-then-read result shape v1 established on FB0D/FB0E and the app team asked to keep.
///
/// A refusal with an empty message must stay **distinguishable from a success**, which is why
/// `success` is always present rather than inferred from an empty body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriteResult {
    pub success: bool,
    pub message: String,
}

impl WriteResult {
    pub fn ok(message: impl Into<String>) -> WriteResult {
        WriteResult {
            success: true,
            message: message.into(),
        }
    }
    pub fn fail(message: impl Into<String>) -> WriteResult {
        WriteResult {
            success: false,
            message: message.into(),
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .unwrap_or_else(|_| br#"{"success":false,"message":"internal"}"#.to_vec())
    }
}

/// Everything the handlers share.
pub struct GattCtx {
    pub host: Host,
    pub session: Arc<SessionSlot>,
    pub slot: Arc<AdapterSlot>,
    pub central: Arc<RwLock<CentralState>>,
    pub cfg: PeripheralConfig,
    /// This Hub's own adapter address, for the QR payload and FB07.
    pub mac: MacAddr,
    /// The one peer that has authenticated. Per peer, and it dies with the connection.
    auth_peer: Mutex<Option<PeerAddr>>,
    pub list_buffers: ResponseBuffers<Vec<u8>>,
    pub remove_results: ResponseBuffers<WriteResult>,
    /// FB11's pages and FB12's results — separate buffers from FB0F/FB10's, so a phone can walk the
    /// Beacon list and the Node list on one connection without either consuming the other's cursor.
    pub node_list_buffers: ResponseBuffers<Vec<u8>>,
    pub node_remove_results: ResponseBuffers<WriteResult>,
    pub beacon_add_results: ResponseBuffers<WriteResult>,
    pub node_add_results: ResponseBuffers<NodeAddResponse>,
    pub label_results: ResponseBuffers<WriteResult>,
    /// FB17's results — its own buffer, so a setup completion and a beacon enrollment on one
    /// connection cannot consume each other's answer.
    pub setup_results: ResponseBuffers<WriteResult>,
    /// FB18's results.
    pub export_results: ResponseBuffers<WriteResult>,
    /// FB1A's results.
    pub pairing_results: ResponseBuffers<WriteResult>,
    /// FB1B's last write result per peer, served in front of the state on every read after it.
    pub radio_mode_results: ResponseBuffers<WriteResult>,
    /// FB1C's join per peer: pending while it runs, then its result. Shared with the thread the
    /// join runs on, which stores the result when it finishes.
    pub cluster_join_results: Arc<ResponseBuffers<cluster_join::JoinSlot>>,
    /// FB1A's rate limit: `(session_generation, window_started_at_monotonic, attempts_in_window)`.
    ///
    /// The first rate limit in this crate. Everywhere else the session is the limit — which is
    /// sound for a token that has to be read off an LCD, and is not for a six-character code
    /// space an authenticated peer could walk.
    ///
    /// Both companions of the count are there because the peer being limited can move the obvious
    /// alternatives:
    ///
    /// - the window is measured on [`crate::host::SystemClock::monotonic_secs`], not the wall
    ///   clock, because FB0B lets that same authenticated peer set the wall clock to any epoch
    ///   between 2020 and 2100 — five attempts and one `{"epoch": now + 61}` write would otherwise
    ///   buy five more, forever;
    /// - the count belongs to the **session**, not to a connection. The FB01 token outlives a
    ///   disconnect (only the front-panel button, `end_phone_session` or the idle timeout end a
    ///   session), so resetting on disconnect would mean reconnecting and re-writing the same
    ///   token bought a fresh window. Recording the generation the attempts were spent in resets
    ///   them exactly when a new operator stands at the front panel — and also covers the session
    ///   that idled out, whose successor must not inherit a stranger's used attempts.
    ///
    /// One counter rather than one per peer, because only one peer is ever authenticated at a time
    /// and FB1A is gated: a second central has nothing to spend.
    pairing_attempts: Mutex<(u64, u64, u32)>,
    /// A second LAN write while one is in flight is `InProgress`, not a queued second reconfigure.
    pub lan_in_flight: AtomicBool,
    /// Where display-worthy transitions go. Lossy and unsubscribed on a headless Hub.
    pub events: EventBus,
    /// The central role's dial queue, for FB0E's enrollment follow-up. `None` when this Hub runs
    /// no central role, and in the handler tests.
    pub dial: Option<tokio::sync::mpsc::Sender<crate::tasks::DialJob>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl GattCtx {
    pub fn new(
        host: Host,
        session: Arc<SessionSlot>,
        slot: Arc<AdapterSlot>,
        central: Arc<RwLock<CentralState>>,
        cfg: PeripheralConfig,
        mac: MacAddr,
        events: EventBus,
    ) -> GattCtx {
        GattCtx {
            host,
            session,
            slot,
            central,
            cfg,
            mac,
            auth_peer: Mutex::new(None),
            list_buffers: ResponseBuffers::new(),
            remove_results: ResponseBuffers::new(),
            node_list_buffers: ResponseBuffers::new(),
            node_remove_results: ResponseBuffers::new(),
            beacon_add_results: ResponseBuffers::new(),
            node_add_results: ResponseBuffers::new(),
            label_results: ResponseBuffers::new(),
            setup_results: ResponseBuffers::new(),
            export_results: ResponseBuffers::new(),
            pairing_results: ResponseBuffers::new(),
            radio_mode_results: ResponseBuffers::new(),
            cluster_join_results: Arc::new(ResponseBuffers::new()),
            pairing_attempts: Mutex::new((0, 0, 0)),
            lan_in_flight: AtomicBool::new(false),
            events,
            dial: None,
        }
    }

    /// Hand FB0E the central role's dial queue, so a phone enrollment gets the same follow-up
    /// dial as the signed and the CLI one. Without it — the peripheral running with no central
    /// role — the enrollment still lands and simply is not followed up.
    pub fn with_dial(
        mut self,
        dial: Option<tokio::sync::mpsc::Sender<crate::tasks::DialJob>>,
    ) -> Self {
        self.dial = dial;
        self
    }

    pub fn is_authenticated_for(&self, peer: PeerAddr) -> bool {
        *lock(&self.auth_peer) == Some(peer)
    }

    pub fn authenticated_peer(&self) -> Option<PeerAddr> {
        *lock(&self.auth_peer)
    }

    pub fn set_authenticated(&self, peer: PeerAddr) {
        *lock(&self.auth_peer) = Some(peer);
    }

    pub fn clear_authentication(&self) {
        *lock(&self.auth_peer) = None;
    }

    /// The gate every characteristic except FB07 passes through.
    ///
    /// Bumps the idle timer first — a live session that is being used must not time out — then
    /// requires that **this** peer is the authenticated one. `NotAuthorized` is an ATT-level error,
    /// so it is legible at any MTU.
    pub fn gate(&self, peer: PeerAddr) -> Result<(), ReqError> {
        self.session.touch_if_live(self.host.clock.as_ref());
        if !self.session.is_live(self.host.clock.as_ref()) {
            self.clear_authentication();
            return Err(ReqError::NotAuthorized);
        }
        if !self.is_authenticated_for(peer) {
            return Err(ReqError::NotAuthorized);
        }
        Ok(())
    }

    /// May FB1A take another attempt?
    ///
    /// A fixed window rather than a token bucket: the thing being bounded is an operator retyping a
    /// code a few times versus a peer walking a 31^6 space, and a window is the shape that is
    /// obvious to read in a log. Counting happens here rather than in the handler so the state has
    /// one owner.
    ///
    /// The window runs on the monotonic source and the count is stamped with the session
    /// generation — see [`GattCtx::pairing_attempts`] for why neither is the wall clock or the
    /// connection.
    pub fn pairing_attempt_allowed(&self) -> bool {
        let now = self.host.clock.monotonic_secs();
        let generation = self.session.generation();
        let mut state = lock(&self.pairing_attempts);
        let (spent_in, window_start, attempts) = *state;
        // A different session is a different operator: the attempts belong to the one that spent
        // them and do not follow the characteristic into the next session.
        if spent_in != generation
            || now.saturating_sub(window_start) >= super::gatt::pairing_code::window_secs()
        {
            *state = (generation, now, 1);
            return true;
        }
        if attempts >= super::gatt::pairing_code::max_attempts() {
            return false;
        }
        *state = (generation, window_start, attempts + 1);
        true
    }

    /// Everything a disconnect must forget for one peer.
    pub fn on_disconnect(&self, peer: PeerAddr) {
        self.list_buffers.forget(&peer);
        self.remove_results.forget(&peer);
        self.node_list_buffers.forget(&peer);
        self.node_remove_results.forget(&peer);
        self.beacon_add_results.forget(&peer);
        self.node_add_results.forget(&peer);
        self.label_results.forget(&peer);
        self.setup_results.forget(&peer);
        self.export_results.forget(&peer);
        self.pairing_results.forget(&peer);
        self.radio_mode_results.forget(&peer);
        // A finished join's result goes with the connection. A join that is still running keeps
        // its `Pending`: it finishes and stores its result for an address that may reconnect
        // (§6.7), and until then a reconnected phone's rewrite is `InProgress`, not a second join
        // racing the first. The Hub reboots seconds after a success anyway.
        self.cluster_join_results
            .forget_if(&peer, |slot| *slot != cluster_join::JoinSlot::Pending);
        if self.is_authenticated_for(peer) {
            // The connection's authentication dies with it; the *session* does not, and neither
            // does FB1A's counter. Clearing it here used to be the cheapest way to defeat the
            // limit: the FB01 token is still valid after a disconnect, so five attempts, a
            // disconnect, a reconnect and the same token bought five more — and authenticating a
            // second central with that token and dropping it wiped the first one's count without
            // even a reconnect. The counter is keyed to the session generation instead
            // ([`GattCtx::pairing_attempts`]), which is what a new operator actually changes.
            self.clear_authentication();
        }
    }
}

/// Serve a read, honouring the offset — **§5.2**.
///
/// A value longer than the link can carry is served in slices from the offset requested; the paging
/// contract sits *above* this rather than replacing it. An offset past the end is
/// `InvalidOffset`, never a silent re-send of the whole value from byte 0.
pub fn slice_at_offset(value: &[u8], offset: u16) -> Result<Vec<u8>, ReqError> {
    let at = offset as usize;
    if at > value.len() {
        return Err(ReqError::InvalidOffset);
    }
    Ok(value[at..].to_vec())
}

/// Serve a JSON read that must arrive whole — **§5.1**.
///
/// If the document cannot fit the link at offset 0, the answer is an ATT-level error rather than a
/// truncated fragment. Once it fits, the offset slicing of §5.2 delivers it.
///
/// The ceiling is judged on the **whole value at every offset**, not only at offset 0. Exempting
/// continuation reads assumed the client had already received a first slice from us, and nothing
/// makes that true: a central may issue READ_BLOB at offset 1 without ever reading at 0, and on an
/// over-MTU FB19 payload that returned a fragment of a plaintext broker credential from the read
/// §5.1 exists to refuse. A value this link cannot carry is refused however it is asked for.
pub fn serve_json(value: &[u8], mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    if !fits(value.len(), mtu) {
        return Err(ReqError::Failed);
    }
    slice_at_offset(value, offset)
}

/// Can a value of `len` bytes be delivered on a link of `mtu`?
///
/// A single PDU carries `mtu - 1` bytes. A larger value is deliverable **only** by continuation
/// reads, which the paging contract avoids relying on — so a value that needs them is refused here
/// and the page builder is expected to have bounded it already.
fn fits(len: usize, mtu: u16) -> bool {
    len <= (mtu.saturating_sub(1) as usize)
}

/// The upper bound on any inbound write, so a phone cannot make the Hub allocate without limit.
pub const MAX_WRITE_BYTES: usize = 1024;
/// FB0D carries key material and a profile, so it gets a larger bound.
pub const MAX_NODE_ADD_BYTES: usize = 4096;

/// Parse an inbound JSON write, bounded.
pub fn parse_write<T: serde::de::DeserializeOwned>(
    value: &[u8],
    max: usize,
) -> Result<T, ReqError> {
    if value.len() > max {
        return Err(ReqError::InvalidValueLength);
    }
    serde_json::from_slice::<T>(value).map_err(|_| ReqError::InvalidValueLength)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uuid_alias_expands_into_the_bluetooth_base() {
        assert_eq!(
            fb(SVC_FB00).to_string(),
            "0000fb00-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            fb(CH_AUTH).to_string(),
            "0000fb01-0000-1000-8000-00805f9b34fb"
        );
        assert_eq!(
            fb(CH_BEACON_REMOVE).to_string(),
            "0000fb10-0000-1000-8000-00805f9b34fb"
        );
    }

    #[test]
    fn fb05_and_fb06_are_not_registered() {
        // CP-1 deleted the BLE shell outright. There is no config key because there is nothing to
        // switch, and this assertion is what stops a future edit reinstating it quietly.
        for deleted in DELETED {
            assert!(
                !REGISTERED.contains(&deleted),
                "{deleted:#06X} is the BLE terminal and must not exist"
            );
        }
    }

    #[test]
    fn the_registered_set_is_exactly_the_documented_twenty_two() {
        assert_eq!(REGISTERED.len(), 22);
        assert_eq!(
            REGISTERED,
            [
                0xFB01, 0xFB02, 0xFB03, 0xFB04, 0xFB07, 0xFB08, 0xFB09, 0xFB0A, 0xFB0B, 0xFB0C,
                0xFB0D, 0xFB0E, 0xFB0F, 0xFB10, 0xFB11, 0xFB12, 0xFB16, 0xFB17, 0xFB18, 0xFB1A,
                0xFB1B, 0xFB1C
            ]
        );
        // FB19 is retired (cluster.md §7): not served, and its number not reused.
        assert_eq!(RETIRED, [0xFB19]);
        for r in RETIRED {
            assert!(!REGISTERED.contains(&r), "{r:#06X} is retired");
            assert!(!RESERVED.contains(&r) && !DELETED.contains(&r));
        }
        // The remaining reservations exist so a parallel slice cannot claim a colliding number, and
        // they are reserved *without* being served. FB11/FB12 left this set when they were built;
        // FB13–FB15 stay, because the headless-client scenario they serve is phase 2 regardless
        // (decisions.md #8).
        assert_eq!(RESERVED, [0xFB13, 0xFB14, 0xFB15]);
        for r in RESERVED {
            assert!(!REGISTERED.contains(&r), "{r:#06X} is phase 2");
        }
        // No overlap and no duplicates.
        let mut all = REGISTERED.to_vec();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), REGISTERED.len());
    }

    #[test]
    fn a_read_offset_is_honoured_and_past_the_end_is_invalid_offset() {
        let value = b"0123456789".to_vec();
        assert_eq!(slice_at_offset(&value, 0).expect("ok"), value);
        assert_eq!(slice_at_offset(&value, 4).expect("ok"), b"456789".to_vec());
        // Exactly at the end is an empty slice, which is how a client learns it is finished.
        assert_eq!(slice_at_offset(&value, 10).expect("ok"), Vec::<u8>::new());
        assert_eq!(
            slice_at_offset(&value, 11).expect_err("past the end"),
            ReqError::InvalidOffset
        );
        // The v1 defect: offset 4 must NOT return the whole value from byte 0 again.
        assert_ne!(slice_at_offset(&value, 4).expect("ok"), value);
    }

    #[test]
    fn a_document_too_large_for_the_link_is_an_att_error_not_a_fragment() {
        let big = vec![b'x'; 200];
        assert_eq!(
            serve_json(&big, 23, 0).expect_err("cannot fit"),
            ReqError::Failed,
            "at MTU 23 no JSON message is short enough to help, so the refusal is not a payload"
        );
        // At a negotiated MTU it fits and is served whole, and the offset still slices it.
        assert_eq!(serve_json(&big, 512, 0).expect("fits").len(), 200);
        assert_eq!(serve_json(&big, 512, 22).expect("fits").len(), 178);
    }

    #[test]
    fn a_value_that_cannot_fit_is_refused_at_every_offset_not_only_at_zero() {
        // The exemption for continuation reads assumed the client had already received a first
        // slice from us. Nothing makes that true — a central may open at offset 1 — so on an
        // over-MTU value it handed out a fragment of exactly what offset 0 had refused.
        let big = vec![b'x'; 200];
        for offset in [0, 1, 22, 199, 200] {
            assert_eq!(
                serve_json(&big, 23, offset).expect_err("cannot fit at any offset"),
                ReqError::Failed,
                "offset {offset}"
            );
        }
    }

    #[test]
    fn the_mtu_boundary_is_exactly_one_pdu_and_serve_json_honours_it() {
        // The rule stated as the two values either side of it, so a future edit to `fits` that
        // shifts it by one is a failing test rather than a truncated read.
        let mtu = 185u16;
        let exact = vec![b'x'; mtu as usize - 1];
        assert_eq!(
            serve_json(&exact, mtu, 0)
                .expect("mtu - 1 is one PDU")
                .len(),
            184
        );
        assert_eq!(
            serve_json(&exact, mtu, 10).expect("and it slices").len(),
            174
        );
        let one_too_many = vec![b'x'; mtu as usize];
        assert_eq!(
            serve_json(&one_too_many, mtu, 0).expect_err("one byte over"),
            ReqError::Failed
        );
    }

    #[test]
    fn the_fit_rule_is_one_pdu_of_mtu_minus_one() {
        assert!(fits(22, 23));
        assert!(!fits(23, 23));
        assert!(fits(184, 185));
        assert!(!fits(185, 185));
        assert!(!fits(1, 0), "no underflow at mtu 0");
    }

    #[test]
    fn a_write_result_distinguishes_a_refusal_with_an_empty_message_from_a_success() {
        let fail = WriteResult::fail("");
        let json = String::from_utf8(fail.to_bytes()).expect("utf8");
        assert!(json.contains(r#""success":false"#), "{json}");
        let ok = WriteResult::ok("");
        assert!(
            String::from_utf8(ok.to_bytes())
                .expect("utf8")
                .contains(r#""success":true"#),
            "an empty message must never be what distinguishes them"
        );
        assert_ne!(fail.to_bytes(), ok.to_bytes());
    }

    #[test]
    fn an_oversized_write_is_refused_before_it_is_parsed() {
        let huge = vec![b'{'; MAX_WRITE_BYTES + 1];
        assert_eq!(
            parse_write::<serde_json::Value>(&huge, MAX_WRITE_BYTES).expect_err("too big"),
            ReqError::InvalidValueLength
        );
        assert_eq!(
            parse_write::<serde_json::Value>(b"not json", MAX_WRITE_BYTES).expect_err("bad json"),
            ReqError::InvalidValueLength
        );
        let v: serde_json::Value =
            parse_write(br#"{"a":1}"#, MAX_WRITE_BYTES).expect("valid and small");
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn the_node_add_bound_is_larger_because_it_carries_key_material() {
        const { assert!(MAX_NODE_ADD_BYTES > MAX_WRITE_BYTES) };
        assert_eq!(MAX_NODE_ADD_BYTES, 4096);
        assert_eq!(MAX_WRITE_BYTES, 1024);
    }
}
