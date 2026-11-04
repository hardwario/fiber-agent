//! FB1C — the cluster join: write the join bundle, read the state and the join's result.
//! `plan/control/ble/peripheral.md` §6.7, `plan/control/cluster.md` §8.
//!
//! **Read before a write** serves the state, `{role, joinable}` and on a Client `server`.
//!
//! **A write** is the join bundle `{"v","h","p","f"}`. The shape is checked here — the four keys
//! and no other, `h` of 1 to 63 characters, `p` of 22, `f` of 43 — and a write that fails it is
//! an ATT error, *Invalid Attribute Value Length*, with nothing changed. Anything else is the
//! host's to judge: the write succeeds at once and the join runs on a thread of its own, because
//! it takes seconds (DNS, HTTPS, TLS to the broker) and this handler runs on the BLE runtime.
//!
//! **A read after a write** carries `"pending":true` in front of the state while the join runs,
//! and `{success, message}` once it has finished. The result stays until the next write or the
//! disconnect; a disconnect while the join is pending forgets nothing, so the result lands for
//! an address that reconnects. A second write while one is pending is `InProgress`.
//!
//! **What may be shortened**, as on FB1B: only the result's `message`, with an ellipsis, never a
//! state field and never `success`. A link too small for the state itself gets an ATT error.

use std::borrow::Cow;

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::host::{ClusterJoinRequest, ClusterJoinStatus, Origin};
use crate::peripheral::buffers::PeerAddr;

/// The bundle is about 115 bytes with a `<hostname>.local` Server and at most about 156 with a
/// 63-character one; anything much larger is not a bundle.
pub const MAX_JOIN_WRITE_BYTES: usize = 256;
/// `h`: at most 63 characters (§6.7).
pub const MAX_HOST_CHARS: usize = 63;
/// `p`: 128 bits of base64url.
pub const PASSWORD_CHARS: usize = 22;
/// `f`: SHA-256 in base64url.
pub const FINGERPRINT_CHARS: usize = 43;

/// One peer's join: running, or finished with its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinSlot {
    Pending,
    Done(WriteResult),
}

#[derive(serde::Serialize)]
struct Document<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pending: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<Cow<'a, str>>,
    #[serde(flatten)]
    status: &'a ClusterJoinStatus,
}

/// The bytes FB1C serves on a link of `mtu`.
pub fn document(
    status: &ClusterJoinStatus,
    slot: Option<&JoinSlot>,
    mtu: u16,
) -> Result<Vec<u8>, ReqError> {
    let result = match slot {
        Some(JoinSlot::Done(r)) => Some(r),
        _ => None,
    };
    let pending = matches!(slot, Some(JoinSlot::Pending)).then_some(true);
    let encode = |message: Option<Cow<'_, str>>| {
        serde_json::to_vec(&Document {
            pending,
            success: result.map(|r| r.success),
            message,
            status,
        })
        .map_err(|_| ReqError::Failed)
    };
    let Some(result) = result else {
        return encode(None);
    };
    let whole = encode(Some(Cow::Borrowed(result.message.as_str())))?;
    let budget = mtu.saturating_sub(1) as usize;
    if whole.len() <= budget {
        return Ok(whole);
    }
    let mut cut: Vec<(usize, char)> = result.message.char_indices().collect();
    while cut.pop().is_some() {
        let end = cut.last().map_or(0, |&(i, c)| i + c.len_utf8());
        let body = encode(Some(Cow::Owned(format!("{}…", &result.message[..end]))))?;
        if body.len() <= budget {
            return Ok(body);
        }
    }
    encode(Some(Cow::Borrowed("")))
}

/// FB1C — the state, and after a write on this connection, the join's progress or result.
pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let status = ctx.host.cluster.read().map_err(|_| ReqError::Failed)?;
    let slot = ctx.cluster_join_results.read(&peer);
    let body = document(&status, slot.as_ref(), mtu)?;
    super::serve_json(&body, mtu, offset)
}

/// The shape §6.7 makes an ATT error. Lengths in characters, as the contract states them.
fn well_formed(req: &ClusterJoinRequest) -> bool {
    let h = req.h.chars().count();
    (1..=MAX_HOST_CHARS).contains(&h)
        && req.p.chars().count() == PASSWORD_CHARS
        && req.f.chars().count() == FINGERPRINT_CHARS
}

/// FB1C — join the Server the bundle names. Returns at once; the join runs on its own thread.
pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: ClusterJoinRequest = super::parse_write(value, MAX_JOIN_WRITE_BYTES)?;
    if !well_formed(&req) {
        return Err(ReqError::InvalidValueLength);
    }
    if ctx.cluster_join_results.read(&peer) == Some(JoinSlot::Pending) {
        return Err(ReqError::InProgress);
    }
    ctx.cluster_join_results.store(peer, JoinSlot::Pending);

    let cluster = ctx.host.cluster.clone();
    let results = ctx.cluster_join_results.clone();
    let spawned = std::thread::Builder::new()
        .name("fb1c-cluster-join".to_string())
        .spawn(move || {
            let outcome = cluster.join(Origin::Ble, &req);
            results.store(
                peer,
                JoinSlot::Done(if outcome.success {
                    WriteResult::ok(outcome.message)
                } else {
                    WriteResult::fail(outcome.message)
                }),
            );
        });
    if spawned.is_err() {
        ctx.cluster_join_results.store(
            peer,
            JoinSlot::Done(WriteResult::fail("the Hub could not start the join")),
        );
    }
    // The write succeeded even when the join will be refused: the reason is the read's business.
    Ok(())
}
