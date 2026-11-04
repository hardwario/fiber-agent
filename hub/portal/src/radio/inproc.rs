//! The testable core of the in-process seam.
//!
//! It builds a `control.radio` invocation and runs it through the **shipped** dispatch path
//! (`control_radio::dispatch::run_with_store`) — the exact code `proximosctl control.radio`
//! runs, driven against the live `TonicChirpStack` in production and the in-memory
//! `FakeChirpStack` under test. There is no parallel arrangement (plan/portal/p2p_integration.md
//! §2.1: "not a wire dialect: no new topic, no new signature scheme, no new parser").

use control_radio::cli::{
    Command, ControlRadio, NodeIdent, NodeSendArgs, NodeSendKind, RadioArg, RadioSelector,
};
use control_radio::config::RadioConfig;
use control_radio::dispatch::{run_with_store, Outcome};
use control_radio::lorawan::client::ChirpStack;
use control_radio::northbound::RetainedPublisher;
use control_radio::store::Store;

/// Build the `ControlRadio` invocation for a structured `node-send` addressed by DevEUI.
///
/// `radio` is `None` on a single-arm Hub, where `radio.mode` resolves the transport exactly as
/// the CLI does when the flag is omitted (`mode::resolve`). A `dual` Hub has to name it.
pub(crate) fn node_send_command(
    dev_eui: &str,
    what: NodeSendKind,
    radio: Option<RadioArg>,
) -> ControlRadio {
    ControlRadio {
        json: true,
        command: Command::NodeSend(NodeSendArgs {
            radio: RadioSelector { radio },
            ident: NodeIdent {
                serial: None,
                dev_eui: Some(dev_eui.to_string()),
            },
            hex: None,
            fport: None,
            what: Some(what),
        }),
    }
}

/// Run one structured `node-send` through the shipped dispatch path.
///
/// `client` is the ChirpStack seam, `None` for a P2P send: that arm queues into `sessions.db`
/// and never reaches ChirpStack. `topics` is `None`: `node-send` reads no retained topic.
/// `publisher` is the broker a `node-send` that coalesces announces the frame it withdrew on
/// (`status: superseded`, control.radio plan.md §5, amended 2026-09-28); it publishes nothing
/// else, and never retained.
pub(crate) async fn run_node_send(
    config: &RadioConfig,
    store_root: &str,
    client: Option<&dyn ChirpStack>,
    publisher: Option<&dyn RetainedPublisher>,
    dev_eui: &str,
    what: NodeSendKind,
    radio: Option<RadioArg>,
) -> control_radio::Result<Outcome> {
    let cli = node_send_command(dev_eui, what, radio);
    run_with_store(
        &cli,
        config,
        client,
        None,
        publisher,
        Store::new(store_root),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_radio::lorawan::fake::FakeChirpStack;
    use control_radio::lorawan::{Scope, Tokens};

    const RADIO_YAML: &str = include_str!("fixtures/radio-section.yaml");

    /// A fresh temp store root, seeded with the three ChirpStack tokens exactly as the
    /// control-radio dispatch tests do, so `node-send`'s seq allocation and any token read
    /// find a well-formed store.
    fn seeded_store_root(tag: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("portal-radio-seam-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tokens = Tokens::new(Store::new(&dir));
        for s in Scope::ALL {
            tokens.store(s, "jwt").expect("seed token");
        }
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn node_send_command_addresses_the_device_by_dev_eui() {
        let cli = node_send_command(
            "de11220000000001",
            NodeSendKind::GetConfig { page: 0, seq: None },
            None,
        );
        assert!(
            cli.json,
            "the in-process seam always asks for the machine shape"
        );
        match cli.command {
            Command::NodeSend(args) => {
                assert_eq!(args.ident.dev_eui.as_deref(), Some("de11220000000001"));
                assert!(args.ident.serial.is_none());
                assert!(
                    args.hex.is_none(),
                    "a structured send carries no raw payload"
                );
                assert!(
                    args.radio.radio.is_none(),
                    "--radio is resolved from radio.mode"
                );
                assert!(matches!(
                    args.what,
                    Some(NodeSendKind::GetConfig { page: 0, seq: None })
                ));
            }
            other => panic!("expected NodeSend, got {other:?}"),
        }
    }

    /// The portal's config refresh is `get-settings` (Command field 31), never a full
    /// `get-config` — the frame on the air proves which.
    #[tokio::test]
    async fn get_settings_enqueues_the_field_31_downlink_through_dispatch() {
        let config = RadioConfig::from_device_config_yaml(RADIO_YAML).expect("fixture parses");
        let fake = FakeChirpStack::new();
        let root = seeded_store_root("getset");

        run_node_send(
            &config,
            &root,
            Some(&fake),
            None,
            "de11220000000001",
            NodeSendKind::GetSettings,
            None,
        )
        .await
        .expect("get-settings dispatches");

        let downlinks = fake.downlinks();
        assert_eq!(downlinks.len(), 1, "exactly one downlink enqueued");
        assert_eq!(downlinks[0].f_port, 85);
        // `Command { seq 1, get_settings {} }` — sticker-firmware #429's `08<seq>fa0100`.
        assert_eq!(downlinks[0].data, vec![0x08, 0x01, 0xfa, 0x01, 0x00]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn get_config_enqueues_one_fport85_downlink_through_dispatch() {
        let config = RadioConfig::from_device_config_yaml(RADIO_YAML).expect("fixture parses");
        let fake = FakeChirpStack::new();
        let root = seeded_store_root("getcfg");

        let out = run_node_send(
            &config,
            &root,
            Some(&fake),
            None,
            "de11220000000001",
            NodeSendKind::GetConfig { page: 0, seq: None },
            None,
        )
        .await
        .expect("get-config dispatches");

        // The seam returns §5's acceptance envelope ...
        match out {
            Outcome::Json(v) => {
                assert_eq!(v["queued"], serde_json::json!(true));
                assert_eq!(v["fport"], serde_json::json!(85));
                assert_eq!(v["dev_eui"], serde_json::json!("de11220000000001"));
            }
            other => panic!("node-send returns Outcome::Json, got {other:?}"),
        }

        // ... and a real fPort-85 downlink reached ChirpStack through the shipped path.
        let downlinks = fake.downlinks();
        assert_eq!(downlinks.len(), 1, "exactly one downlink enqueued");
        assert_eq!(downlinks[0].f_port, 85, "fPort-85 command engine");
        assert_eq!(downlinks[0].dev_eui, "de11220000000001");
        assert!(
            !downlinks[0].confirmed,
            "node-send never queues a confirmed downlink"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn set_config_enqueues_a_fport85_setparam_downlink() {
        let config = RadioConfig::from_device_config_yaml(RADIO_YAML).expect("fixture parses");
        let fake = FakeChirpStack::new();
        let root = seeded_store_root("setcfg");

        let out = run_node_send(
            &config,
            &root,
            Some(&fake),
            None,
            "de11220000000001",
            NodeSendKind::SetConfig {
                assignments: vec!["application.interval_report=1800".to_string()],
                replace_alarms: false,
                supersede: false,
            },
            None,
        )
        .await
        .expect("set-config dispatches");

        match out {
            Outcome::Json(v) => {
                assert_eq!(v["queued"], serde_json::json!(true));
                assert_eq!(v["fport"], serde_json::json!(85));
            }
            other => panic!("set-config returns Outcome::Json, got {other:?}"),
        }

        let downlinks = fake.downlinks();
        assert_eq!(downlinks.len(), 1, "one key -> one SetParam downlink");
        assert_eq!(downlinks[0].f_port, 85);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Q-N1: the Portal's "read from the device" twice before the Node wakes queues one
    /// `GetInfo`, the newer, and the seam's broker gets the `superseded` document for the older
    /// one — the seam hands dispatch a publisher so a Portal send announces it like the CLI's.
    #[tokio::test]
    async fn a_second_get_info_coalesces_and_announces_the_first() {
        use control_radio::lorawan::fake::FakePublisher;
        let config = RadioConfig::from_device_config_yaml(RADIO_YAML).expect("fixture parses");
        let fake = FakeChirpStack::new();
        let broker = FakePublisher::new();
        let root = seeded_store_root("coalesce");
        let send = || {
            run_node_send(
                &config,
                &root,
                Some(&fake),
                Some(&broker),
                "de11220000000001",
                NodeSendKind::GetInfo,
                None,
            )
        };

        send().await.expect("the first get-info");
        let Outcome::Json(second) = send().await.expect("the second get-info") else {
            panic!("node-send returns Outcome::Json");
        };
        assert_eq!(second["seq"], 2);
        assert_eq!(second["coalesced"], serde_json::json!([1]));

        let eui = control_radio::lorawan::eui::Eui::parse("de11220000000001").expect("eui");
        let queued = fake.get_queue(&eui).await.expect("queue");
        assert_eq!(queued.len(), 1, "one frame waits");
        assert_eq!(
            queued[0].data,
            vec![0x08, 0x02, 0x22, 0x00],
            "the newer seq"
        );

        let events = broker.events();
        assert_eq!(events.len(), 1);
        assert!(events[0].0.ends_with("/nodes/de11220000000001/command"));
        let doc: serde_json::Value = serde_json::from_slice(&events[0].1).expect("json");
        assert_eq!(doc["status"], "superseded");
        assert_eq!(
            (doc["seq"].as_u64(), doc["superseded_by"].as_u64()),
            (Some(1), Some(2))
        );
        assert!(broker.published().is_empty(), "nothing retained");

        let _ = std::fs::remove_dir_all(&root);
    }
}
