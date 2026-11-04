//! Coalescing identical idempotent commands in a Node's queue — `plan/control/radio/plan.md` §5,
//! `node-send`, amended 2026-09-28 (Q-N1, bench F5).
//!
//! Two `cmd clock-sync` sent one after the other went out as two downlinks and brought two
//! Infos, each costing airtime and duty cycle on the gateway and on the Node. A second ask of
//! the same kind while the first still waits adds nothing the first does not bring, so
//! `node-send` withdraws the waiting frame and queues the new one under its new `seq`: the
//! Portal and the CLI wait for the answer to the command they sent last, and the Node answers
//! the one frame it gets under that frame's `seq`.
//!
//! **One decision, two withdrawals.** [`kind`] decides, from a frame's own bytes, whether and
//! as what it coalesces; the same function reads the new frame and every waiting one, so the
//! two carriers cannot disagree about it. Only taking a frame out of the queue is per carrier:
//! the P2P session queue ([`crate::p2p::db::SessionsDb::drop_queued_seqs`]) and ChirpStack's
//! ([`crate::lorawan::queue::withdraw_waiting`]), both the ways Discard already withdraws. A
//! frame already on the air, a P2P head delivered and waiting for its `0x55` or a ChirpStack
//! item that is `is_pending`, is never withdrawn; the new frame queues behind it.
//!
//! What the caller is told: the `node-send` result names the withdrawn frames
//! (`coalesced: [<seq>, …]`), the old `seq`'s `sent_commands` entry gets `superseded_by`
//! ([`crate::cache::Cache::note_superseded`]), and a `command` document with
//! `status: superseded` goes out for it ([`superseded_document`]), so a waiter can close a
//! request that will not be answered under its own `seq`.

use serde_json::{Map, Value};

use crate::cache::CacheDocument;
use crate::decode::command::{CommandTag, ALL};
use crate::decode::pb::{read_tag, read_varint};

/// What a command frame coalesces as, or `None` when it never does.
///
/// Only the asks that make the Node report or resync, and only in their plain `node-send`
/// spelling, with an empty argument: `cmd clock-sync`, `cmd force-send`, `get-info`,
/// `get-config` and (#97) `get-settings`. So `get-config --page N` (a non-empty argument) never coalesces, nor does a
/// `ClockSync` carrying the one-shot bootstrap time, which has no `node-send` spelling. Nothing
/// that writes or acts coalesces: `set-config`, `get-param`, `cmd reboot`, `reset-counters`,
/// `device-reset`, `history-replay` or a raw `--hex` that is not one of the four.
///
/// `continuation` is a frame sent under an explicit `--seq` (`get-config --seq S`, #96): it
/// completes the answer of exchange `S`, and a fresh `get-config` does not replace that, so it
/// never coalesces either way. For the new frame it is the caller's `--seq`; for a waiting one
/// the `sent_commands` record ([`CacheDocument::is_continuation`]).
pub fn kind(frame: &[u8], continuation: bool) -> Option<CommandTag> {
    if continuation {
        return None;
    }
    // `Command { seq = 1, <command> = N }`, the shape `Downlink::encode` writes: the `seq`
    // varint first, then the command's length-delimited argument, and nothing after it.
    let seq = read_tag(frame, 0);
    if seq.field != 1 || seq.wire != 0 {
        return None;
    }
    let at = read_varint(frame, seq.next).next;
    let command = read_tag(frame, at);
    if command.next <= at || command.wire != 2 {
        return None;
    }
    let len = read_varint(frame, command.next);
    if len.next <= command.next || len.value != 0 || len.next != frame.len() {
        return None;
    }
    let tag = ALL.iter().copied().find(|t| *t as u32 == command.field)?;
    matches!(
        tag,
        CommandTag::ClockSync
            | CommandTag::ForceSend
            | CommandTag::GetInfo
            | CommandTag::GetConfig
            | CommandTag::GetSettings
    )
    .then_some(tag)
}

/// Whether the waiting frame `frame`, sent under `seq`, is replaced by a new frame of `new`:
/// the same [`kind`], and not a continuation by the Hub's own record. `sent` is the Node's
/// cache document; without one nothing is a continuation.
pub fn replaces(new: CommandTag, seq: u32, frame: &[u8], sent: Option<&CacheDocument>) -> bool {
    let continuation = sent.is_some_and(|d| d.is_continuation(seq));
    kind(frame, continuation) == Some(new)
}

/// Whether the waiting frame `frame`, sent under `seq`, is withdrawn for a newer
/// `set-config --supersede` to the same Node (config_sync.md §13 Q20b): it is a `SetParam`
/// frame the Hub itself sent as part of an earlier `--supersede` write. A plain `set-config`
/// (the CLI's, the service card's) carries only some keys and is never withdrawn; nor is a
/// frame the Hub no longer remembers.
pub fn replaces_config(seq: u32, frame: &[u8], sent: Option<&CacheDocument>) -> bool {
    command_field(frame) == Some(CommandTag::SetParam as u32)
        && sent.is_some_and(|d| d.is_superseding(seq))
}

/// The command field number of a `Command { seq = 1, <command> = N }` frame, whatever its
/// argument.
fn command_field(frame: &[u8]) -> Option<u32> {
    let seq = read_tag(frame, 0);
    if seq.field != 1 || seq.wire != 0 {
        return None;
    }
    let at = read_varint(frame, seq.next).next;
    let command = read_tag(frame, at);
    (command.next > at && command.wire == 2).then_some(command.field)
}

/// The `command` document for a frame a newer one replaced: `status: superseded`, the `seq` it
/// went out under and the `seq` that answers for it now.
///
/// The shape of every other `command` document — `dev_eui`, `serial` when known, `seq`,
/// `kind` — plus `status` and `superseded_by`. It is the Hub's, not the Node's, so it carries
/// no `received_at`: `at` is when the Hub withdrew the frame. A consumer that routes on `kind`
/// sees a kind it can name rather than an answer.
pub fn superseded_document(
    dev_eui: &str,
    serial: Option<&str>,
    seq: u32,
    by: u32,
    at: &str,
) -> Value {
    let mut doc = Map::new();
    doc.insert("dev_eui".into(), Value::from(dev_eui));
    if let Some(serial) = serial {
        doc.insert("serial".into(), Value::from(serial));
    }
    doc.insert("seq".into(), Value::from(seq));
    doc.insert("kind".into(), Value::from(SUPERSEDED));
    doc.insert("status".into(), Value::from(SUPERSEDED));
    doc.insert("superseded_by".into(), Value::from(by));
    doc.insert("at".into(), Value::from(at));
    Value::Object(doc)
}

/// The `status` (and `kind`) of a [`superseded_document`].
pub const SUPERSEDED: &str = "superseded";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{NodeSendCmdArgs, NodeSendKind};
    use crate::decode::command_encode::{plan, Downlink};
    use crate::Transport;

    fn frame(d: &Downlink) -> Vec<u8> {
        d.encode(7, Transport::Lorawan).expect("encodes")
    }

    fn cmd(name: &str) -> NodeSendKind {
        NodeSendKind::Cmd(NodeSendCmdArgs {
            name: name.to_string(),
            from: None,
            to: None,
            counter: Vec::new(),
        })
    }

    /// The four asks, as `node-send` plans them, and on both carriers the same bytes.
    #[test]
    fn the_four_idempotent_asks_coalesce_as_themselves() {
        for (kind_, tag) in [
            (cmd("clock-sync"), CommandTag::ClockSync),
            (cmd("force-send"), CommandTag::ForceSend),
            (NodeSendKind::GetInfo, CommandTag::GetInfo),
            (
                NodeSendKind::GetConfig { page: 0, seq: None },
                CommandTag::GetConfig,
            ),
        ] {
            for t in [Transport::Lorawan, Transport::P2p] {
                let planned = plan(&kind_, t).expect("plans");
                let [only] = planned.downlinks.as_slice() else {
                    panic!("{kind_:?} is one frame");
                };
                let bytes = only.encode(1234, t).expect("encodes");
                assert_eq!(kind(&bytes, false), Some(tag), "{kind_:?} on {t:?}");
                assert_eq!(kind(&bytes, true), None, "a continuation never coalesces");
            }
        }
    }

    /// Nothing that writes or acts coalesces, and neither does a continuation page or the
    /// bootstrap clock sync.
    #[test]
    fn nothing_else_coalesces() {
        for d in [
            Downlink::GetConfig { page: 3 },
            Downlink::ClockSync {
                unix_time: Some(1_790_000_000),
            },
            Downlink::GetParam {
                fields: vec![(crate::decode::command_encode::ConfigGroup::Application, 2)],
            },
            Downlink::Reboot,
            Downlink::SettingsSave,
            Downlink::ReqHistory {
                from_unix: None,
                to_unix: None,
            },
            Downlink::W1Scan,
        ] {
            assert_eq!(kind(&frame(&d), false), None, "{d:?}");
        }
        for k in [
            NodeSendKind::SetConfig {
                assignments: vec!["application.interval_report=60".into()],
                replace_alarms: false,
                supersede: false,
            },
            cmd("reboot"),
            cmd("history-replay"),
        ] {
            let planned = plan(&k, Transport::P2p).expect("plans");
            for d in &planned.downlinks {
                assert_eq!(kind(&frame(d), false), None, "{k:?}");
            }
        }
        // Not a command frame at all: a raw payload, truncated or trailing bytes.
        for raw in [
            &[0x11, 0x22][..],
            &[0x08],
            &[0x08, 0x07, 0x22],
            &[0x08, 0x07, 0x22, 0x00, 0x00],
        ] {
            assert_eq!(kind(raw, false), None, "{raw:02x?}");
        }
        // A raw `--hex` that spells a plain get-info is one.
        assert_eq!(
            kind(&[0x08, 0x07, 0x22, 0x00], false),
            Some(CommandTag::GetInfo)
        );
    }

    /// A waiting frame of the same kind is replaced; one of another kind, or one the Hub
    /// recorded as a continuation, is not.
    #[test]
    fn a_waiting_frame_is_replaced_by_the_same_kind_only() {
        let clock = frame(&Downlink::ClockSync { unix_time: None });
        let config = frame(&Downlink::GetConfig { page: 0 });
        assert!(replaces(CommandTag::ClockSync, 7, &clock, None));
        assert!(!replaces(CommandTag::GetInfo, 7, &clock, None));

        let sent: CacheDocument = serde_json::from_value(serde_json::json!({
            "sent_commands": [ { "seq": 7, "tag": 5, "continuation": true } ],
        }))
        .expect("document");
        assert!(!replaces(CommandTag::GetConfig, 7, &config, Some(&sent)));
        assert!(replaces(CommandTag::GetConfig, 8, &config, Some(&sent)));
    }

    #[test]
    fn the_superseded_document_names_both_seqs() {
        let doc = superseded_document("5876070000000413", Some("0413"), 7, 9, "T");
        assert_eq!(
            doc,
            serde_json::json!({
                "dev_eui": "5876070000000413", "serial": "0413", "seq": 7,
                "kind": "superseded", "status": "superseded", "superseded_by": 9, "at": "T",
            })
        );
    }

    /// #97: a second settings read while the first still waits replaces it.
    #[test]
    fn a_settings_read_coalesces() {
        assert_eq!(
            kind(&frame(&Downlink::GetSettings), false),
            Some(CommandTag::GetSettings)
        );
    }
}
