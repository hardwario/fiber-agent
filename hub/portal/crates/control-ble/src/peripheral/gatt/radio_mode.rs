//! FB1B — the Hub's radio mode: read what it runs and may run, write the provisioning choice.
//! `plan/control/radio/node_registry.md` §12, `plan/control/ble/peripheral.md` §6.6.
//!
//! **One characteristic, read and write.** A read serves the state
//! `{mode, chosen, lorawan_card, allowed, default}`. A write `{mode}` goes to the same apply path
//! as the signed `radio-set-mode` (the host's [`crate::host::RadioModeControl`]), and from then on
//! a read on this connection carries the write's `{success, message}` **in front of** the state,
//! which is fresh on every read — so the write-then-read convention of §4 holds, and the phone
//! still sees what the Hub now says rather than a copy taken at write time.
//!
//! **What must arrive whole, and what may be shortened.** The document has to fit one PDU
//! (§5.1). The state fields are load-bearing; the result's `message` is prose, and a refusal
//! from the Hub can run to 200 bytes. So when the whole document does not fit, the message is
//! truncated with an ellipsis, the §4 ladder's rule for a name — never a state field, and never
//! `success`. A link too small for the state itself gets an ATT-level error, as every JSON read.

use std::borrow::Cow;

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::host::{Origin, RadioMode, RadioModeStatus};
use crate::peripheral::buffers::PeerAddr;

/// FB1B's write. `deny_unknown_fields` and a closed mode set, as FB17's role: a phone speaking a
/// shape this Hub does not implement gets an ATT error rather than a mode nobody read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadioModeRequest {
    pub mode: RadioMode,
}

/// The read document: the last write's result on this connection, if any, then the state.
#[derive(serde::Serialize)]
struct Document<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<Cow<'a, str>>,
    #[serde(flatten)]
    status: &'a RadioModeStatus,
}

/// The bytes FB1B serves on a link of `mtu`: the whole message when it fits, else as much of it
/// as fits, ending in `…`.
pub fn document(
    status: &RadioModeStatus,
    result: Option<&WriteResult>,
    mtu: u16,
) -> Result<Vec<u8>, ReqError> {
    let encode = |message: Option<Cow<'_, str>>| {
        serde_json::to_vec(&Document {
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
    // Shorten from the end, one character at a time, until the document fits. Bounded by the
    // message's length, and only ever run on an over-long refusal.
    let mut cut: Vec<(usize, char)> = result.message.char_indices().collect();
    while cut.pop().is_some() {
        let end = cut.last().map_or(0, |&(i, c)| i + c.len_utf8());
        let short = format!("{}…", &result.message[..end]);
        let body = encode(Some(Cow::Owned(short)))?;
        if body.len() <= budget {
            return Ok(body);
        }
    }
    // Not even an empty message fits: `serve_json` turns this into the ATT error.
    encode(Some(Cow::Borrowed("")))
}

/// FB1B — the state, and after a write on this connection, its result.
pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let status = ctx.host.radio.read().map_err(|_| ReqError::Failed)?;
    let result = ctx.radio_mode_results.read(&peer);
    let body = document(&status, result.as_ref(), mtu)?;
    super::serve_json(&body, mtu, offset)
}

/// FB1B — choose the radio mode.
pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: RadioModeRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    let outcome = ctx.host.radio.set(Origin::Ble, req.mode);
    ctx.radio_mode_results.store(
        peer,
        if outcome.success {
            WriteResult::ok(outcome.message)
        } else {
            WriteResult::fail(outcome.message)
        },
    );
    // The write succeeded even when the change was refused, as on FB17: the reason is the
    // read's business, and an ATT error here would throw it away.
    Ok(())
}
