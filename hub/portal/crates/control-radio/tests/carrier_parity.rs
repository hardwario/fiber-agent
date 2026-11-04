//! One command sequence, driven down both carriers, compared document for document.
//!
//! Everything else in this suite proves one arm at a time. This proves the property the
//! transport-neutral decode core exists for: *a Node reachable over LoRaWAN and over P2P
//! produces the same northbound documents, the same cache slots and the same `node-get`
//! answers*. If the two arms ever diverge — a key added on one side, a page reassembled
//! differently, a dump published twice — it fails here rather than on a Hub.
//!
//! The node is deliberately one node: one serial, one DevEUI, enrolled on the LoRaWAN arm and
//! registered on the P2P arm. That is the fixture a parity claim needs; giving the two arms
//! different identifiers would only prove that a difference can be renamed away.
//!
//! What it does **not** cover is listed in the crate README under
//! "What tests/carrier_parity.rs does not cover".
mod common;

use clap::Parser;
use common::{event, hexbytes, join_request, response_with_seq, rx_event, uplink};
use control_radio::cache::{Cache, CacheSlot};
use control_radio::cli::ControlRadioCli;
use control_radio::config::RadioConfig;
use control_radio::decode::snapshot::ConfigDocument;
use control_radio::dispatch::{run_with_store, Outcome};
use control_radio::error::Transport;
use control_radio::lorawan::client::ChirpStack;
use control_radio::lorawan::enroll::{EnrollmentRecord, NodesFile};
use control_radio::lorawan::fake::FakeChirpStack;
use control_radio::lorawan::{Eui, Scope, Tokens};
use control_radio::northbound::{Area, Topics};
use control_radio::p2p::db::SessionsDb;
use control_radio::p2p::fake::SentCmd;
use control_radio::p2p::fake::{FakeModem, RecordingPublisher};
use control_radio::p2p::modem::Event;
use control_radio::p2p::runtime::Central;
use control_radio::p2p::session::RegisteredNode;
use control_radio::p2p::tower::link::{self, TwrCmd, TwrTx, TxOutcome};
use control_radio::store::Store;
use control_radio::tasks::uplink::{self as up, Context, State};
use serde_json::Value;

const PLAN_EXAMPLE: &str = include_str!("fixtures/radio-section.yaml");

/// One node, two carriers. The DevEUI is the identity on both — on the P2P air since #73,
/// and in the registry; the serial is the enrollment record's `serial_number` attribute.
const SERIAL: &str = "80e007b2";
const DEV: &str = "70b3d57ed80051b2";
const APP_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";
const APP_KEY: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
];
const NET_ID: u32 = 0x5d0e_3be3;

/// The `Info` submessage the bench Node answers `get-info` with, `claim_token` included so the
/// §9.7 redaction is exercised on both arms.
const INFO_SUBMESSAGE: &str = "08011004200228918b808708300e400150c61a58037080104a04deadbeef";
/// `ConfigDump` pages 0 and 1 of 2 — the `lorawan` group, then `application`; the envelope
/// numbers them (#425, [`paged`]).
const DUMP_PAGES: [&str; 2] = ["1a020801", "2202100a"];
/// A v2.0.0 `HistoryFrame`: t0 1782198249, one mb-temperature column (`channels` 00), one
/// record of 24.32 °C, 900 s apart.
const HISTORY_FRAME: &str = "18e9dfe8d10622028009308407520100";

/// `response` numbered page `index` of `count` in the envelope (fields 12/13, #425).
fn paged(mut response: Vec<u8>, index: u8, count: u8) -> Vec<u8> {
    response.extend([0x60, index, 0x68, count]);
    response
}

/// STICKER 0413's `GetConfig` on sticker-firmware 7be011e, six pages as decrypted off the P2P
/// air (bench PS-1, 2026-09-27, seq 55 in byte 2): 40 keys, one alarm rule (`alarm_0`). The
/// v2.0.0 key table no longer names `application.history_sensors` (field 5, reserved since
/// 435273c6) and the four GP bool caps (`sensors` 3, 4, 7, 19, #498), so 35 render.
const GET_CONFIG_7BE011E: [&str; 6] = [
    "01083722181a16080010021800200128003208587607000000041378026806",
    "010837221f1a173a0800000000000000005204000000006806700580010022040800103c60016806",
    "010837221b220b183c2001280330e01238012a0c08001000180020002800300060026806",
    "010837221c2a16380040004800500078008001008801009001009801003202080a60036806",
    "010837222232161a1103000000000000704100000c4200000000a00100420808a0cff89d03100760046806",
    "01083722044202180e60056806",
];
/// The boot settings-info 0413 sent on both carriers (P4-1, 2026-09-27): seq 0, one page,
/// the 28-byte `ConfigDump` body `2206103c…4800` (intervals, history, nine `cap_*`).
const BOOT_PAGE_0413: &str = "01221c2206103c183c20012a12080010001800200028003000380040004800";
/// The alarm rule the Portal wrote for a critical band of 15..36 °C (E3), in the v2.0.0
/// 18-byte layout (mb, channel 0, sensor type 1); the bench wrote the 17-byte v1.4 one.
const RULE_15_36: &str = "030000010000000070410000104200000000";

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn store(tag: &str) -> (Store, TempDir) {
    let dir =
        std::env::temp_dir().join(format!("control-radio-parity-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (Store::new(&dir), TempDir(dir))
}

fn cli(args: &[&str]) -> ControlRadioCli {
    let mut full = vec!["control.radio"];
    full.extend_from_slice(args);
    ControlRadioCli::try_parse_from(full).expect("parses")
}

fn dual_config() -> RadioConfig {
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    RadioConfig::from_device_config_yaml(&dual).expect("dual is a valid config")
}

fn radio() -> control_radio::p2p::modem::RadioConfig {
    control_radio::p2p::modem::RadioConfig {
        freq_hz: 868_100_000,
        sf: 10,
        bw: 0,
        cr: 1,
        preamble: 8,
        tx_power_dbm: 14,
        flags: 0,
    }
}

/// The keys that *may* differ between two carriers carrying the same frame, and nothing else.
///
/// Kept explicit and short on purpose: every entry is a claim that the field is a property of
/// the carrier rather than of the message, and anything else that differs is a parity bug.
/// `transport`/`id` are how a consumer is told which arm answered, `received_at` and the
/// `message_id` derived from it are when this Hub heard the frame (P2P has no LNS timestamp),
/// and `warning` reports the frame's own version byte on whichever arm decoded it.
const CARRIER_KEYS: [&str; 5] = ["transport", "id", "received_at", "message_id", "warning"];

fn normalise(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(map) = v.as_object_mut() {
        for k in CARRIER_KEYS {
            map.remove(k);
        }
        // `node-get` wraps the cached document in `value`, and the rendered `config` document
        // (!99, node_reported_state.md §2.2) carries its own `received_at`.
        if let Some(inner) = map.get_mut("value").and_then(Value::as_object_mut) {
            for k in CARRIER_KEYS {
                inner.remove(k);
            }
        }
    }
    v
}

/// Every publish the LoRaWAN pipeline owes for one event, as `(area, document)`.
///
/// `Area::Info` is dropped: the retained per-node `info` is refreshed by every LoRaWAN uplink,
/// and the P2P arm does not refresh it per frame — pairing state there is the join's, not the
/// uplink's. That is a genuine carrier difference, not a document difference.
fn lorawan_publishes(out: &up::Outcome) -> Vec<(Area, Value)> {
    out.publishes
        .iter()
        .filter_map(|p| {
            let area = Area::from_name(p.topic.rsplit('/').next()?)?;
            if area == Area::Info {
                return None;
            }
            Some((area, serde_json::from_slice(&p.payload).expect("json")))
        })
        .collect()
}

fn p2p_publishes(pubh: &RecordingPublisher, from: usize) -> Vec<(Area, Value)> {
    pubh.published()[from..]
        .iter()
        .map(|p| (p.area, serde_json::from_slice(&p.payload).expect("json")))
        .collect()
}

fn compare(label: &str, lorawan: &[(Area, Value)], p2p: &[(Area, Value)]) {
    assert_eq!(
        lorawan.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        p2p.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        "{label}: the two arms published on different areas"
    );
    for ((area, l), (_, p)) in lorawan.iter().zip(p2p) {
        assert_eq!(
            normalise(l),
            normalise(p),
            "{label} on {area:?}: the documents differ"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_same_command_sequence_yields_the_same_documents_on_both_carriers() {
    let mut arms = Arms::new("sequence").await;

    // `node-get` is gated on the daemon heartbeat on the LoRaWAN arm.
    let alive = arms.lw_store.root().join("radio-alive");
    std::env::set_var(
        control_radio::tasks::heartbeat::ALIVE_PATH_ENV,
        alive.as_os_str(),
    );
    control_radio::tasks::heartbeat::touch_at(&alive).expect("touches");

    // ---- one command sequence, queued on both arms ---------------------------------------
    let sequence: [&[&str]; 4] = [
        &["get-info"],
        &["get-config"],
        &["set-config", "application.interval_report=120"],
        &["cmd", "history-replay"],
    ];
    for tail in sequence {
        arms.node_send(tail).await;
    }

    let lorawan_frames = arms.lorawan_frames();
    let p2p_frames = arms.p2p_queue();
    assert_eq!(lorawan_frames.len(), 4, "one frame per command");
    assert_eq!(
        lorawan_frames, p2p_frames,
        "the encoder is one encoder: the same command is the same bytes on both carriers, \
         including §9.7's per-device seq 1..4 from two independent fresh caches"
    );

    // ---- the answers ---------------------------------------------------------------------
    let golden_history = hexbytes(HISTORY_FRAME);
    // `(command index, response bytes)` — the config dump answers in two pages, both echoing
    // the seq of the one `get-config` that asked for it.
    let answers: Vec<Vec<u8>> = vec![
        response_with_seq(3, &hexbytes(INFO_SUBMESSAGE), 1),
        paged(response_with_seq(4, &hexbytes(DUMP_PAGES[0]), 2), 0, 2),
        paged(response_with_seq(4, &hexbytes(DUMP_PAGES[1]), 2), 1, 2),
        response_with_seq(2, &[], 3),
        response_with_seq(5, &golden_history, 4),
    ];

    // Which queued command each answer belongs to: the two config-dump pages both answer the
    // one `get-config`, and every other answer is one-to-one.
    const ANSWERS_COMMAND: [usize; 5] = [0, 1, 1, 2, 3];

    for (i, answer) in answers.iter().enumerate() {
        let (lorawan, p2p) = arms
            .answer(Some(&lorawan_frames[ANSWERS_COMMAND[i]]), answer)
            .await;
        compare(&format!("answer {i}"), &lorawan, &p2p);
    }
    assert_eq!(
        arms.wire.delivered, lorawan_frames,
        "every queued command went out as a 0x56 whose body is the fPort-85 downlink, byte \
         for byte"
    );

    // ---- the cache the CLI reads ----------------------------------------------------------
    for slot in [CacheSlot::DeviceInfo, CacheSlot::Config] {
        let (l, p) = arms.cached(slot);
        // `at` is when this Hub wrote it, which is the one thing the two runs cannot share.
        assert_eq!(
            normalise(&l),
            normalise(&p),
            "the {slot:?} cache slot differs"
        );
    }

    // ---- and what `node-get` answers ------------------------------------------------------
    for what in ["config", "device-info"] {
        let l = node_get(
            &arms.config,
            &arms.lw_store,
            &["--radio", "lorawan", "--serial", SERIAL, what],
        )
        .await;
        let p = node_get(
            &arms.config,
            &arms.p2p_store,
            &["--radio", "p2p", "--serial", "0x80e007b2", what],
        )
        .await;
        assert_eq!(l["transport"], "lorawan");
        assert_eq!(p["transport"], "p2p");
        assert_eq!(
            normalise(&l),
            normalise(&p),
            "`node-get {what}` differs by carrier"
        );
        assert_eq!(
            normalise(&l["value"]),
            normalise(&p["value"]),
            "`node-get {what}` value differs by carrier"
        );
    }
}

/// `node_reported_state.md` §2.2, "The Hub accumulates", on both carriers (PH-2, bench
/// 2026-09-27: over P2P the Node's boot settings-info replaced a 40-key `get-config` with 12
/// keys). A partial answer — the boot settings-info, a `get-param` — merges into the config
/// the Hub holds; a complete answer to a `get-config` replaces it, so a cleared alarm rule
/// goes. The retained `config` and the cache slot are one document on each arm, and the same
/// document on both.
#[tokio::test(flavor = "current_thread")]
async fn a_partial_config_answer_merges_and_a_full_get_config_replaces_on_both_carriers() {
    let mut arms = Arms::new("fold").await;
    // The second `get-config` goes out once the first has been answered: sent while the first
    // still waited, it would take its place in the queue (Q-N1, plan.md §5).
    arms.node_send(&["get-config"]).await;
    arms.node_send(&["get-param", "application.interval_report"])
        .await;
    let frames = arms.lorawan_frames();
    assert_eq!(frames, arms.p2p_queue(), "one encoder, seq 1..2 on both");

    // `ConfigDump` bodies, the two-page ones numbered in the envelope (#425). Groups:
    // lorawan (3) sub_band = 1, adr; application (4) interval_sample, interval_report;
    // sensors (5) cap_hall_left, cap_w1_sensors; alarms (6) alarm_0, alarm_1; w1 slots (7).
    let full_1 = [
        "1a0410012001 2205103c188407",
        "2a0408014001 32081a0203002202 0401",
    ];
    // w1 slot 1 = 2: dallas in the v2.0.0 registry ids (#430; 1 was dallas in v1.4.0).
    let boot = "2202101e 2a020800 3a0402000000";
    let get_param = "22021878";
    let full_3 = ["1a0410012001 2204101e1878", "2a0408004001 32041a020300"];
    let dump = |body: &str, seq: u32| response_with_seq(4, &hexbytes(&body.replace(' ', "")), seq);
    let page = |body: &str, seq: u32, index: u8| paged(dump(body, seq), index, 2);

    // ---- a full `get-config` (seq 1): eight keys ------------------------------------------
    let (lorawan, p2p) = arms.answer(Some(&frames[0]), &page(full_1[0], 1, 0)).await;
    compare("seq 1 page 0", &lorawan, &p2p);
    let full = arms.config_answer(None, &page(full_1[1], 1, 1)).await;
    assert_eq!(full.seq, 1);
    assert_eq!(full.config.len(), 8, "{:?}", full.config);
    arms.node_send(&["get-config"]).await;
    let frames = arms.lorawan_frames();
    assert_eq!(
        arms.p2p_queue(),
        frames[1..],
        "seq 1 answered, seq 2..3 waiting, on both"
    );

    // ---- the boot settings-info (seq 0): merged, the full dump's keys stay ---------------
    let booted = arms.config_answer(None, &dump(boot, 0)).await;
    assert_eq!(booted.seq, 0);
    assert_eq!(
        booted.config.len(),
        8,
        "the boot dump is a subset and must not drop the lorawan and alarms groups: {:?}",
        booted.config
    );
    assert_eq!(
        booted.config["application.interval_sample"], 30,
        "newer wins"
    );
    assert_eq!(booted.config["sensors.cap_hall_left"], false);
    assert_eq!(booted.config["application.interval_report"], 900);
    assert_eq!(booted.config["lorawan.sub_band"], 1);
    assert_eq!(booted.config["alarms.alarm_1"], "0401");
    assert_eq!(
        booted.w1_slot_type.as_deref(),
        Some(&["dallas", "empty", "empty", "empty"].map(String::from)[..])
    );

    // ---- a `get-param` (seq 2): merged ----------------------------------------------------
    let param = arms
        .config_answer(Some(&frames[1]), &dump(get_param, 2))
        .await;
    assert_eq!(param.seq, 2);
    assert_eq!(param.config.len(), 8, "{:?}", param.config);
    assert_eq!(param.config["application.interval_report"], 120);
    assert_eq!(param.config["application.interval_sample"], 30);
    assert_eq!(param.config["alarms.alarm_0"], "0300");

    // ---- a full `get-config` (seq 3) without alarm_1: replaced ----------------------------
    let (lorawan, p2p) = arms.answer(Some(&frames[2]), &page(full_3[0], 3, 0)).await;
    compare("seq 3 page 0", &lorawan, &p2p);
    let replaced = arms.config_answer(None, &page(full_3[1], 3, 1)).await;
    assert_eq!(replaced.seq, 3);
    assert!(
        !replaced.config.contains_key("alarms.alarm_1"),
        "the rule the device cleared is gone: {:?}",
        replaced.config
    );
    assert_eq!(replaced.config.len(), 7, "{:?}", replaced.config);
    assert_eq!(replaced.config["alarms.alarm_0"], "0300");
    assert_eq!(
        replaced.w1_slot_type, booted.w1_slot_type,
        "only the boot dump carries the 1-Wire slots, so they survive a get-config"
    );
}

/// The alarm slots (`alarms.alarm_N`) a config holds, in key order.
fn alarm_slots(doc: &ConfigDocument) -> Vec<String> {
    doc.config
        .keys()
        .filter(|k| {
            k.strip_prefix("alarms.alarm_")
                .is_some_and(|n| n.parse::<u8>().is_ok())
        })
        .cloned()
        .collect()
}

/// One page of [`GET_CONFIG_7BE011E`] answering `seq` instead of 55.
fn get_config_page(hex: &str, seq: u8) -> Vec<u8> {
    let mut page = hexbytes(hex);
    assert_eq!(page[1..3], [0x08, 0x37], "the capture's seq 55");
    assert!(seq < 0x80, "a one-byte varint");
    page[2] = seq;
    page
}

/// P4-1, bench 2026-09-27 (c57, STICKER 0413), step by step and on both carriers: the full
/// `get-config`, the boot settings-info, the Portal's alarm-table write (critical_high
/// 35 -> 36, `--replace-alarms`) and its ack, the reboot's settings-info, then the move to
/// LoRaWAN and its boot. The bench saw 40 keys on P2P before the write and 55 on LoRaWAN
/// after the move; the 15 extra were `alarm_1`..`alarm_15` as zeros, which the firmware never
/// sends, folded in by the ack of the replace.
///
/// Q-N3 (node_reported_state.md §2.2, the ack fold withdrawn 2026-09-28): an ack changes
/// nothing in the config, on either carrier. Both arms hold the same document at every step,
/// and it is the 40 keys the Node reported, through the write, its ack, the reboot and the
/// move.
#[tokio::test(flavor = "current_thread")]
async fn p4_1_the_bench_config_sequence_folds_one_document_on_both_carriers() {
    let mut arms = Arms::new("p4-1").await;
    let full = arms.full_get_config().await;
    assert_eq!(full.config.len(), 35, "{:?}", full.config.keys());
    assert_eq!(alarm_slots(&full), ["alarms.alarm_0"]);

    // ---- the boot settings-info merges: still 35 ------------------------------------------
    let boot = hexbytes(BOOT_PAGE_0413);
    let booted = arms.config_answer(None, &boot).await;
    assert_eq!((booted.seq, booted.page_count), (0, 1));
    assert_eq!(booted.config.len(), 35, "{:?}", booted.config.keys());
    assert_eq!(alarm_slots(&booted), ["alarms.alarm_0"]);
    let held = arms.cached(CacheSlot::Config);

    // ---- the Portal's alarm-table write (seq 2) and its ack: no config --------------------
    arms.node_send(&[
        "set-config",
        "--replace-alarms",
        &format!("alarms.alarm_0={RULE_15_36}"),
    ])
    .await;
    let write = arms.lorawan_frames()[1].clone();
    assert_eq!(
        arms.p2p_queue().last(),
        Some(&write),
        "one encoder: the same frame on both carriers"
    );
    let (lorawan, p2p) = arms
        .answer(Some(&write), &response_with_seq(2, &[], 2))
        .await;
    compare("the ack of the write", &lorawan, &p2p);
    assert_eq!(
        lorawan.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        [Area::Command],
        "the ack goes out on command, and nothing on config"
    );
    assert_eq!(
        arms.cached(CacheSlot::Config),
        held,
        "both caches hold the config as the Node reported it"
    );

    // ---- the reboot's settings-info merges over that --------------------------------------
    let rebooted = arms.config_answer(None, &boot).await;
    assert_eq!(rebooted.config, booted.config);
    assert_eq!(alarm_slots(&rebooted), ["alarms.alarm_0"]);

    // ---- the move: the one cache goes with the Node, and LoRaWAN boots --------------------
    let before_move = arms.cached(CacheSlot::Config).1;
    arms.move_cache_to_lorawan();
    let on_lorawan = arms.lorawan_config(&boot);
    assert_eq!(
        normalise(&serde_json::to_value(&on_lorawan).expect("json")),
        normalise(&before_move),
        "the LoRaWAN boot folds to what P2P held before the move"
    );
    assert_eq!(on_lorawan.config.len(), 35);
}

/// The other half of P4-1: without the alarm-table write, the Node moved from P2P to LoRaWAN
/// keeps its 40 keys through the same boot page. The carrier change adds nothing.
#[tokio::test(flavor = "current_thread")]
async fn p4_1_a_move_without_the_alarm_write_keeps_the_40_keys() {
    let mut arms = Arms::new("p4-1-move").await;
    arms.full_get_config().await;
    let boot = hexbytes(BOOT_PAGE_0413);
    let on_p2p = arms.config_answer(None, &boot).await;
    assert_eq!(on_p2p.config.len(), 35);

    arms.move_cache_to_lorawan();
    let on_lorawan = arms.lorawan_config(&boot);
    assert_eq!(on_lorawan.config, on_p2p.config);
    assert_eq!(alarm_slots(&on_lorawan), ["alarms.alarm_0"]);
}

/// Q-N1 (plan.md §5 `node-send`, amended 2026-09-28) on both carriers: two `cmd clock-sync`
/// leave one frame queued, the newer, byte for byte the same on both. A frame already on the
/// air, the P2P head delivered and the ChirpStack item `is_pending`, stays on both, and the
/// next ask queues behind it.
#[tokio::test(flavor = "current_thread")]
async fn two_clock_syncs_coalesce_into_one_frame_on_both_carriers() {
    let mut arms = Arms::new("coalesce").await;
    arms.node_send(&["cmd", "clock-sync"]).await;
    arms.node_send(&["cmd", "clock-sync"]).await;
    let queued = arms.lorawan_queue().await;
    assert_eq!(queued, arms.p2p_queue(), "one encoder, one decision");
    let seqs = |frames: &[Vec<u8>]| -> Vec<Option<u32>> {
        frames
            .iter()
            .map(|f| control_radio::lorawan::queue::downlink_seq(f))
            .collect()
    };
    assert_eq!(seqs(&queued), [Some(2)], "the newer seq only");

    // On the air on both: P2P delivers it, ChirpStack has it pending.
    arms.deliver(&queued[0]).await;
    arms.chirpstack
        .mark_head_pending(&Eui::parse(DEV).expect("eui"));
    arms.node_send(&["cmd", "clock-sync"]).await;
    let queued = arms.lorawan_queue().await;
    assert_eq!(queued, arms.p2p_queue());
    assert_eq!(
        seqs(&queued),
        [Some(2), Some(3)],
        "the new one queues behind"
    );
}

/// One node on both arms: enrolled on LoRaWAN behind a fake ChirpStack, registered and joined
/// on P2P behind a fake modem. What the tests drive and compare.
struct Arms {
    config: RadioConfig,
    topics: Topics,
    nodes: NodesFile,
    chirpstack: FakeChirpStack,
    lw_store: Store,
    lw_state: State,
    p2p_store: Store,
    pubh: RecordingPublisher,
    central: Central<RecordingPublisher>,
    modem: FakeModem,
    wire: Wire,
    _dirs: [TempDir; 2],
}

impl Arms {
    async fn new(tag: &str) -> Self {
        let config = dual_config();
        let topics = Topics::new(control_radio::northbound::hostname());

        // ---- the LoRaWAN arm: an enrolled node and a fake ChirpStack ---------------------
        let (lw_store, lw_dir) = store(&format!("{tag}-lorawan"));
        let tokens = Tokens::new(lw_store.clone());
        for s in Scope::ALL {
            tokens.store(s, "jwt").expect("seed");
        }
        let mut nodes = NodesFile::default();
        nodes.by_serial.insert(
            SERIAL.into(),
            EnrollmentRecord {
                serial_number: SERIAL.into(),
                dev_eui: Eui::parse(DEV).expect("eui"),
                profile: "default".into(),
                activation: "otaa".into(),
                join_eui: None,
                name: None,
                app_key: Some(APP_KEY_HEX.into()),
            },
        );
        nodes.save(&lw_store).expect("nodes.json");

        // ---- the P2P arm: the same node, registered and joined ---------------------------
        let (p2p_store, p2p_dir) = store(&format!("{tag}-p2p"));
        let mut db = SessionsDb::load(&p2p_store).expect("load");
        db.upsert_node(RegisteredNode {
            serial: SERIAL.into(),
            dev_eui: DEV.into(),
            profile: "default".into(),
            app_key: APP_KEY_HEX.into(),
            name: None,
        });
        db.save(&p2p_store).expect("save");

        let pubh = RecordingPublisher::new();
        let mut central = Central::new(p2p_store.clone(), NET_ID, radio(), pubh.clone());
        let mut modem = FakeModem::new();
        central.init_modem(&mut modem).await.expect("init");
        central
            .on_event(
                &mut modem,
                rx_event(
                    join_request(&APP_KEY, &Eui::parse(DEV).unwrap().to_bytes(), 5),
                    1_000,
                ),
            )
            .await
            .expect("the registered node joins");
        let addr = SessionsDb::load(&p2p_store)
            .expect("reload")
            .sessions
            .get(DEV)
            .expect("a live session")
            .addr;

        Arms {
            config,
            topics,
            nodes,
            chirpstack: FakeChirpStack::new(),
            lw_store,
            lw_state: State::new(),
            p2p_store,
            pubh,
            central,
            modem,
            wire: Wire {
                addr,
                counter: 1,
                t_ms: 2_000,
                seq: 0,
                seen: 0,
                delivered: Vec::new(),
            },
            _dirs: [lw_dir, p2p_dir],
        }
    }

    /// `node-send` one command on both arms.
    async fn node_send(&self, tail: &[&str]) {
        for (radio_arg, ident, store) in [
            ("lorawan", SERIAL, &self.lw_store),
            ("p2p", "0x80e007b2", &self.p2p_store),
        ] {
            let mut args = vec!["node-send", "--radio", radio_arg, "--serial", ident];
            args.extend_from_slice(tail);
            run_with_store(
                &cli(&args).inner,
                &self.config,
                Some(&self.chirpstack as &dyn ChirpStack),
                None,
                None,
                store.clone(),
            )
            .await
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    /// Every fPort-85 downlink the LoRaWAN arm has queued, in order.
    fn lorawan_frames(&self) -> Vec<Vec<u8>> {
        self.chirpstack
            .downlinks()
            .into_iter()
            .map(|d| {
                assert_eq!(d.f_port, 85, "the command engine is fPort 85 by protocol");
                d.data
            })
            .collect()
    }

    /// The P2P node's downlink queue, as bodies.
    fn p2p_queue(&self) -> Vec<Vec<u8>> {
        SessionsDb::load(&self.p2p_store)
            .expect("reload")
            .sessions
            .get(DEV)
            .expect("session")
            .downlink_queue
            .iter()
            .map(|q| q.body.clone())
            .collect()
    }

    /// Answer on both arms, once `after` (a queued command's body) has gone out on P2P, and
    /// return what each arm published for it.
    async fn answer(
        &mut self,
        after: Option<&[u8]>,
        answer: &[u8],
    ) -> (Vec<(Area, Value)>, Vec<(Area, Value)>) {
        if let Some(after) = after {
            self.deliver(after).await;
            // And on LoRaWAN, where ChirpStack drops it from the queue once transmitted.
            self.chirpstack
                .transmitted(&Eui::parse(DEV).expect("eui"), after);
        }

        let mark = self.pubh.published().len();
        self.wire
            .feed(&mut self.central, &mut self.modem, 85, answer)
            .await;

        let out = self.lorawan(answer);
        (lorawan_publishes(&out), p2p_publishes(&self.pubh, mark))
    }

    /// Pump application uplinks on P2P until the gateway has put `frame` (a queued command's
    /// body) on the air. Nothing can be answered before it has been delivered. The gateway
    /// sends a pushed command after the next uplink, so pumping application uplinks is what
    /// drains the queue — and an answer is itself such an uplink.
    async fn deliver(&mut self, frame: &[u8]) {
        let mut pumps = 0;
        while !self.wire.delivered.iter().any(|d| d == frame) {
            assert!(pumps < 4, "the queued 0x56 never went out");
            pumps += 1;
            self.wire
                .feed(
                    &mut self.central,
                    &mut self.modem,
                    2,
                    &[control_radio::decode::APP_PROTO_VERSION],
                )
                .await;
        }
    }

    /// What ChirpStack still holds queued for the node, as bodies, in order.
    async fn lorawan_queue(&self) -> Vec<Vec<u8>> {
        self.chirpstack
            .get_queue(&Eui::parse(DEV).expect("eui"))
            .await
            .expect("queue")
            .into_iter()
            .map(|q| q.data)
            .collect()
    }

    /// One fPort-85 uplink through the LoRaWAN arm alone, cache write included.
    fn lorawan(&mut self, answer: &[u8]) -> up::Outcome {
        let ctx = Context {
            config: &self.config,
            nodes: &self.nodes,
            topics: &self.topics,
        };
        let lw_cache = Cache::new(&self.lw_store, Transport::Lorawan);
        let mut out = up::process(&event(DEV, 85, answer), &ctx, &mut self.lw_state);
        // What `proximos-control`'s broker loop does after `process`: fold the held config
        // into a completed dump. The P2P central does it itself (PH-2).
        up::fold_prior(&mut out, &self.topics, |id| {
            lw_cache.read(id).ok().flatten()
        });
        up::commit(&lw_cache, &out).expect("cache writes");
        out
    }

    /// [`Arms::lorawan`] with an answer that completes a dump: the `config` it published,
    /// which must also be what it cached.
    fn lorawan_config(&mut self, answer: &[u8]) -> ConfigDocument {
        let out = self.lorawan(answer);
        let published = lorawan_publishes(&out)
            .into_iter()
            .find(|(a, _)| *a == Area::Config)
            .map(|(_, v)| v)
            .expect("a completed dump publishes the retained config");
        assert_eq!(
            published,
            self.cached(CacheSlot::Config).0,
            "LoRaWAN: cache = retained"
        );
        serde_json::from_value(published).expect("a config document")
    }

    /// `node-move` P2P -> LoRaWAN as far as the config is concerned: one cache per Node
    /// (node_registry.md §3, R5), so the LoRaWAN arm goes on from what the P2P arm held.
    fn move_cache_to_lorawan(&self) {
        let from = Cache::new(&self.p2p_store, Transport::P2p)
            .path(DEV)
            .expect("path");
        let to = Cache::new(&self.lw_store, Transport::Lorawan)
            .path(DEV)
            .expect("path");
        std::fs::copy(from, to).expect("the cache moves with the Node");
    }

    /// `get-config` (seq 1) answered on both arms with the six 7be011e pages: the 40-key
    /// config both arms then hold.
    async fn full_get_config(&mut self) -> ConfigDocument {
        self.node_send(&["get-config"]).await;
        let frame = self.lorawan_frames()[0].clone();
        assert_eq!(
            self.p2p_queue(),
            std::slice::from_ref(&frame),
            "one encoder, seq 1 on both"
        );
        let pages = GET_CONFIG_7BE011E.map(|h| get_config_page(h, 1));
        for (i, page) in pages[..5].iter().enumerate() {
            let (lorawan, p2p) = self.answer((i == 0).then_some(&frame[..]), page).await;
            compare(&format!("get-config page {i}"), &lorawan, &p2p);
        }
        let full = self.config_answer(None, &pages[5]).await;
        assert_eq!((full.seq, full.page_count), (1, 6));
        full
    }

    /// [`Arms::answer`] with an answer that completes a dump: the arms must publish the same
    /// `config`, and each must cache what it published. That document is returned.
    async fn config_answer(&mut self, after: Option<&[u8]>, answer: &[u8]) -> ConfigDocument {
        let (lorawan, p2p) = self.answer(after, answer).await;
        compare("config answer", &lorawan, &p2p);
        let published = |docs: &[(Area, Value)]| {
            docs.iter()
                .find(|(a, _)| *a == Area::Config)
                .map(|(_, v)| v.clone())
                .expect("a completed dump publishes the retained config")
        };
        let (lw_cached, p2p_cached) = self.cached(CacheSlot::Config);
        assert_eq!(published(&lorawan), lw_cached, "LoRaWAN: cache = retained");
        assert_eq!(published(&p2p), p2p_cached, "P2P: cache = retained");
        serde_json::from_value(published(&p2p)).expect("a config document")
    }

    /// One cache slot as each arm holds it: `(lorawan, p2p)`.
    fn cached(&self, slot: CacheSlot) -> (Value, Value) {
        let read = |store: &Store, transport| {
            Cache::new(store, transport)
                .read(DEV)
                .expect("read")
                .expect("filled")
                .slot(slot)
                .unwrap_or_else(|| panic!("{slot:?} on {transport:?}"))
                .value
        };
        (
            read(&self.lw_store, Transport::Lorawan),
            read(&self.p2p_store, Transport::P2p),
        )
    }
}

/// The Node and its gateway, as the central sees them: the uplink counter, plus every command
/// the gateway has delivered so far.
struct Wire {
    addr: u32,
    counter: u32,
    t_ms: u64,
    /// The gateway's delivery seq of the next ring event (link doc §4.1).
    seq: u8,
    /// How much of the modem's command log has been looked at.
    seen: usize,
    delivered: Vec<Vec<u8>>,
}

impl Wire {
    fn next_seq(&mut self) -> u8 {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        seq
    }

    /// Send one uplink, then deliver in its window whatever the central had pushed before it,
    /// reporting each as `DELIVERED` (`EVT_TWR_TX`).
    async fn feed(
        &mut self,
        central: &mut Central<RecordingPublisher>,
        modem: &mut FakeModem,
        port: u8,
        body: &[u8],
    ) {
        let pending: Vec<(u16, Vec<u8>)> = modem.sent[self.seen..]
            .iter()
            .filter_map(|c| match c {
                SentCmd::Twr(TwrCmd::QueuePush {
                    addr,
                    item,
                    flags,
                    plaintext,
                    ..
                }) if *addr == self.addr && flags & link::PUSH_CONFIRMED != 0 => {
                    Some((*item, plaintext.clone()))
                }
                _ => None,
            })
            .collect();
        self.seen = modem.sent.len();
        central
            .on_event(
                modem,
                uplink(
                    self.next_seq(),
                    self.addr,
                    self.counter,
                    self.t_ms,
                    port,
                    body,
                ),
            )
            .await
            .unwrap_or_else(|e| panic!("uplink port {port} counter {}: {e}", self.counter));
        self.counter += 1;
        self.t_ms += 1_000;
        for (item, plaintext) in pending {
            assert_eq!(
                &plaintext[..2],
                &[0x81, 86],
                "a command goes out on port 86"
            );
            self.delivered.push(plaintext[2..].to_vec());
            let seq = self.next_seq();
            central
                .on_event(
                    modem,
                    Event::TwrTx {
                        seq,
                        tx: TwrTx {
                            addr: self.addr,
                            item,
                            outcome: TxOutcome::Delivered,
                            gw_counter: self.counter,
                            node_ack_counter: 0,
                            ack_rssi: None,
                        },
                    },
                )
                .await
                .expect("tx");
        }
    }
}

async fn node_get(config: &RadioConfig, store: &Store, tail: &[&str]) -> Value {
    let mut args = vec!["node-get"];
    args.extend_from_slice(tail);
    match run_with_store(&cli(&args).inner, config, None, None, None, store.clone())
        .await
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    {
        Outcome::Json(v) => v,
        other => panic!("{args:?}: {other:?}"),
    }
}
