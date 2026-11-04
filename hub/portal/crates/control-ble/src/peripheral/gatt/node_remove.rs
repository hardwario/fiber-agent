//! FB12 — Node remove.
//! `plan/control/ble/peripheral.md` §2, §4.
//!
//! > The acceptance criterion is stated at the **store layer**, not the API layer: FB12's effects and
//! > the signed `remove-node` command's effects must be identical, asserted by test. Two surfaces
//! > that agree today because someone wrote them carefully is not the same as two surfaces that
//! > cannot disagree.
//!
//! Hence: no logic here beyond parsing and the one-of check. The removal is
//! `NodeEnroll::remove`, which on the agent is `libs/lorawan/node_remove.rs` — the same call the
//! signed MQTT command makes. `control-core/tests/node_remove_simulated.rs` is where that identity
//! is asserted.
//!
//! **Two identifiers, exactly one per write.** `plan/control/radio/plan.md` §5 "One identifier"
//! (decided 2026-08-19) makes `serial` the operator-facing identity and `dev_eui` the universal
//! alternative, mutually exclusive: a caller never needs to know the transport to know how to
//! address a node, whichever identifier it happens to hold. `fiber-endpoints.md` §2.4 sketched
//! `{serial}` alone, and that document says its payload shapes are proposals — this is the shape
//! that agrees with the rest of the `node-*` surface.

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::host::NodeRemoveTarget;
use crate::peripheral::buffers::PeerAddr;

/// `{"deveui": …}` **xor** `{"serial": …}`.
///
/// Both fields are optional to serde so that "neither" and "both" reach the handler as a legible
/// refusal rather than as an `InvalidValueLength` the installer cannot act on. An *unknown* key is
/// still a parse failure, because that is a phone sending a shape this Hub does not implement.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRemoveRequest {
    #[serde(default)]
    pub deveui: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
}

impl NodeRemoveRequest {
    /// The one-of check, as a value rather than a pair of `if`s at the call site.
    pub fn target(&self) -> Result<NodeRemoveTarget, &'static str> {
        match (self.deveui.as_deref(), self.serial.as_deref()) {
            (Some(d), None) => Ok(NodeRemoveTarget::DevEui(d.to_string())),
            (None, Some(s)) => Ok(NodeRemoveTarget::Serial(s.to_string())),
            (Some(_), Some(_)) => Err(
                "write exactly one of \"deveui\" or \"serial\", not both — they could name \
                 different nodes",
            ),
            (None, None) => Err("write one of \"deveui\" or \"serial\" to say what to remove"),
        }
    }
}

pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: NodeRemoveRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    let result = match req.target() {
        Ok(target) => {
            let out = ctx.host.nodes.remove(&target);
            // An unknown identifier is `success: false` with a message, **not** an error — the FB10
            // contract, kept. There is no `ctx.central` bookkeeping to undo here: that set is the
            // Beacon scanner's, and a Node never appears in it.
            if out.success {
                WriteResult::ok(out.message)
            } else {
                WriteResult::fail(out.message)
            }
        }
        Err(why) => WriteResult::fail(why),
    };
    ctx.node_remove_results.store(peer, result);
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let r = ctx
        .node_remove_results
        .read(&peer)
        .unwrap_or_else(|| WriteResult::fail("no removal has been requested on this connection"));
    super::serve_json(&r.to_bytes(), mtu, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> NodeRemoveRequest {
        serde_json::from_str(body).expect("parses")
    }

    #[test]
    fn exactly_one_identifier_is_a_target_and_anything_else_is_a_reason() {
        assert_eq!(
            parse(r#"{"deveui":"0011223344556677"}"#)
                .target()
                .expect("one"),
            NodeRemoveTarget::DevEui("0011223344556677".to_string())
        );
        assert_eq!(
            parse(r#"{"serial":"SN-1"}"#).target().expect("one"),
            NodeRemoveTarget::Serial("SN-1".to_string())
        );
        // Both is a refusal rather than a preference order: the two could name different nodes, and
        // silently picking one would remove a device nobody asked about.
        let e = parse(r#"{"deveui":"0011223344556677","serial":"SN-1"}"#)
            .target()
            .expect_err("both");
        assert!(e.contains("not both"), "{e}");
        let e = parse("{}").target().expect_err("neither");
        assert!(e.contains("deveui") && e.contains("serial"), "{e}");
    }

    #[test]
    fn an_unknown_key_is_a_parse_failure_not_a_silently_ignored_field() {
        // A phone sending `{"mac": …}` is speaking FB10's shape at FB12. Accepting the write and
        // then refusing it as "neither identifier" would be true but unhelpful; refusing the parse
        // says the shape is wrong.
        assert!(
            serde_json::from_str::<NodeRemoveRequest>(r#"{"mac":"AA:BB:CC:DD:EE:FF"}"#).is_err()
        );
        assert!(serde_json::from_str::<NodeRemoveRequest>(r#"{"serial_number":"SN-1"}"#).is_err());
    }
}
