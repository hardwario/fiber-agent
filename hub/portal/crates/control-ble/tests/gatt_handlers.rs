//! The FB00 handlers — `plan/control/ble/peripheral.md` §2–§5.
//!
//! Every handler is a pure function of `(ctx, peer, mtu, offset, value)`, so the whole surface is
//! exercised here without a controller. What BlueZ does with the results is `gatt::app`'s business
//! and is the only part left to HIL.

use std::sync::{Arc, RwLock};

use bluer::gatt::local::ReqError;

use control_ble::central::adapter::AdapterSlot;
use control_ble::central::enroll;
use control_ble::central::state::CentralState;
use control_ble::config::{EyeSection, PeripheralConfig};
use control_ble::host::{
    ClusterJoinOutcome, ClusterJoinStatus, ConfigStore, FakeClock, Host, HubRole,
    MemoryClusterJoin, MemoryConfigStore, MemoryExportDestination, MemoryNodeEnroll,
    MemoryPortalPairing, MemoryRadioMode, MemorySetupState, NeverStandby, NodeEntry, NoopSubsystem,
    NullPublisher, NullReadingSink, Origin, RadioMode, RadioModeOutcome, RadioModeStatus,
    SetupStatus, StaticDeviceInfo, SystemClock, UnavailableNetwork, UnwiredClusterJoin,
    UnwiredExportDestination, UnwiredNodeEnroll, UnwiredPortalPairing, UnwiredRadioMode,
    UnwiredSetupState,
};
use control_ble::mac::MacAddr;
use control_ble::peripheral::events::{EventBus, PeripheralEvent};
use control_ble::peripheral::gatt::{
    self, auth, beacon_add, beacon_list, beacon_remove, cluster_join, device_info,
    export_destination, label, lan, node_add, node_list, node_remove, pairing_code, radio_mode,
    setup, time, wifi, GattCtx, WriteResult,
};
use control_ble::peripheral::paging::{BeaconList, ListRead};
use control_ble::peripheral::session::SessionSlot;
use control_ble::tasks::{DialJob, DialKind};
use tokio::sync::mpsc;

const PEER_A: [u8; 6] = [0xAA; 6];
const PEER_B: [u8; 6] = [0xBB; 6];
const BEACON: &str = "AA:BB:CC:DD:EE:FF";
const MTU: u16 = 185;
/// A join bundle as the Server's portal shows it: a `<hostname>.local` Server, 22 + 43 chars.
const BUNDLE: &str = r#"{"v":1,"h":"proximos-1a2b3c4d.local","p":"Zm9vYmFyYmF6cXV4cXV1eA","f":"4Fg1yj5x8x2R0T3y4bWcL9e6t5Qa8n2rJ7kPq0sVwXz"}"#;

struct Fixture {
    ctx: Arc<GattCtx>,
    clock: Arc<FakeClock>,
    config: Arc<MemoryConfigStore>,
    session: Arc<SessionSlot>,
    hub_mac: MacAddr,
    /// Only wired in [`fixture_with_nodes`]; the default fixture leaves the Node seam unwired, which
    /// is itself the subject of one FB11 test.
    nodes: Option<Arc<MemoryNodeEnroll>>,
    /// FB16/FB17's seam. Always wired, and seeded un-provisioned — which is the state a Hub out of
    /// the box is in, and the one both characteristics are interesting against.
    setup: Arc<MemorySetupState>,
    /// FB18's seam.
    export: Arc<MemoryExportDestination>,
    /// FB1A's seam.
    pairing: Arc<MemoryPortalPairing>,
    /// FB1B's seam, seeded as a Hub out of the box without the card.
    radio: Arc<MemoryRadioMode>,
    /// FB1C's seam, seeded as a Hub out of the box: no role, joinable.
    cluster: Arc<MemoryClusterJoin>,
}

fn fixture(section: EyeSection) -> Fixture {
    build_fixture(section, None, None)
}

/// The variant FB11/FB12 need: a Node seam that holds a list and removes from the one it serves.
fn fixture_with_nodes(entries: Vec<NodeEntry>) -> Fixture {
    build_fixture(
        EyeSection::default(),
        Some(Arc::new(MemoryNodeEnroll::with_entries(entries))),
        None,
    )
}

/// A Hub that also runs the central role: FB0E has a dial queue to hand its follow-up to.
fn fixture_with_dial(section: EyeSection) -> (Fixture, mpsc::Receiver<DialJob>) {
    let (tx, rx) = mpsc::channel(4);
    (build_fixture(section, None, Some(tx)), rx)
}

fn node(deveui: &str, serial: Option<&str>, name: Option<&str>) -> NodeEntry {
    NodeEntry {
        deveui: deveui.to_string(),
        serial: serial.map(str::to_string),
        name: name.map(str::to_string),
    }
}

fn build_fixture(
    section: EyeSection,
    nodes: Option<Arc<MemoryNodeEnroll>>,
    dial: Option<mpsc::Sender<DialJob>>,
) -> Fixture {
    let clock = Arc::new(FakeClock::at(1_700_000_000));
    let config = Arc::new(MemoryConfigStore::with_section(section));
    let session = Arc::new(SessionSlot::new());
    let hub_mac = MacAddr::parse("B8:27:EB:11:22:33").expect("mac");
    let setup = Arc::new(MemorySetupState::default());
    let export = Arc::new(MemoryExportDestination::default());
    let pairing = Arc::new(MemoryPortalPairing::default());
    let radio = Arc::new(MemoryRadioMode::default());
    let cluster = Arc::new(MemoryClusterJoin::default());
    let host = Host {
        config: config.clone(),
        publisher: Arc::new(NullPublisher),
        readings: Arc::new(NullReadingSink),
        subsystem: Arc::new(NoopSubsystem),
        standby: Arc::new(NeverStandby),
        network: Arc::new(UnavailableNetwork),
        clock: clock.clone(),
        device: Arc::new(StaticDeviceInfo::for_tests()),
        nodes: match nodes.clone() {
            Some(n) => n,
            None => Arc::new(UnwiredNodeEnroll),
        },
        setup: setup.clone(),
        export: export.clone(),
        pairing: pairing.clone(),
        radio: radio.clone(),
        cluster: cluster.clone(),
    };
    let ctx = Arc::new(
        GattCtx::new(
            host,
            session.clone(),
            Arc::new(AdapterSlot::new()),
            Arc::new(RwLock::new(CentralState::default())),
            PeripheralConfig::default(),
            hub_mac,
            EventBus::new(),
        )
        .with_dial(dial),
    );
    Fixture {
        ctx,
        clock,
        config,
        session,
        hub_mac,
        nodes,
        setup,
        export,
        pairing,
        radio,
        cluster,
    }
}

/// Start a session and authenticate `peer`, the way a phone does.
fn authenticate(f: &Fixture, peer: [u8; 6]) {
    f.session
        .start(f.clock.as_ref(), 300, &f.hub_mac, "test-hub");
    let tok = f
        .session
        .qr_payload(&f.hub_mac, "test-hub")
        .expect("live")
        .tok;
    auth::write(&f.ctx, peer, tok.as_bytes()).expect("authenticates");
}

/// Authenticate `peer` against the session that is **already live**, the way a phone does when the
/// link drops and it reconnects with the token it still holds.
///
/// Deliberately not [`authenticate`], which starts a session — that is the front-panel button, and
/// the difference between the two is exactly what FB1A's counter is keyed to.
fn reauthenticate(f: &Fixture, peer: [u8; 6]) {
    let tok = f
        .session
        .qr_payload(&f.hub_mac, "test-hub")
        .expect("the session outlives a disconnect")
        .tok;
    auth::write(&f.ctx, peer, tok.as_bytes()).expect("authenticates");
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("a complete JSON document")
}

#[test]
fn fb07_is_readable_without_a_session() {
    // The accepted dependency: the phone recognises a Hub before it has any credential.
    let f = fixture(EyeSection::default());
    let body = device_info::read(&f.ctx, MTU, 0).expect("FB07 is not gated");
    let v = json(&body);
    assert_eq!(v["hostname"], "test-hub");
    assert_eq!(v["mac"], "B8:27:EB:11:22:33");
    assert!(v["version"].is_string());
    assert!(v["uptime"].is_string());
    // And it carries nothing about the Hub's *state* — that is §6.3's ask and it is gated.
    for state_ish in [
        "provisioned",
        "beacons",
        "tags",
        "enabled",
        "session",
        "wifi",
        "lan",
        "alarm",
    ] {
        assert!(
            v.get(state_ish).is_none(),
            "FB07 must not carry state: {state_ish}"
        );
    }
    assert_eq!(
        v.as_object().expect("object").len(),
        4,
        "four identity fields, nothing else"
    );
}

#[test]
fn everything_else_is_not_authorized_without_a_session() {
    let f = fixture(EyeSection::default());
    // Every gated read.
    for (name, r) in [
        ("FB02", wifi::scan_read(&f.ctx, PEER_A, MTU, 0)),
        ("FB04", wifi::status_read(&f.ctx, PEER_A, MTU, 0)),
        ("FB0A", label::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB0B", time::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB0C", lan::status_read(&f.ctx, PEER_A, MTU, 0)),
        ("FB0D", node_add::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB0E", beacon_add::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB0F", beacon_list::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB10", beacon_remove::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB11", node_list::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB12", node_remove::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB16", setup::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB17", setup::read_result(&f.ctx, PEER_A, MTU, 0)),
        ("FB18", export_destination::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB1A", pairing_code::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB1B", radio_mode::read(&f.ctx, PEER_A, MTU, 0)),
        ("FB1C", cluster_join::read(&f.ctx, PEER_A, MTU, 0)),
    ] {
        assert_eq!(
            r.expect_err(&format!("{name} must be gated")),
            ReqError::NotAuthorized,
            "{name}: NotAuthorized is an ATT-level error, legible at any MTU"
        );
    }
    // Every gated write.
    for (name, r) in [
        (
            "FB03",
            wifi::connect_write(&f.ctx, PEER_A, br#"{"ssid":"x","password":"y"}"#),
        ),
        ("FB08", wifi::disconnect_write(&f.ctx, PEER_A, b"")),
        (
            "FB09",
            lan::config_write(&f.ctx, PEER_A, br#"{"mode":"dhcp"}"#),
        ),
        (
            "FB0A",
            label::write(&f.ctx, PEER_A, br#"{"label":"Freezer room"}"#),
        ),
        (
            "FB0B",
            time::write(&f.ctx, PEER_A, br#"{"epoch":1700000000}"#),
        ),
        (
            "FB0E",
            beacon_add::write(&f.ctx, PEER_A, br#"{"mac":"AA:BB:CC:DD:EE:FF"}"#),
        ),
        (
            "FB10",
            beacon_remove::write(&f.ctx, PEER_A, br#"{"mac":"AA:BB:CC:DD:EE:FF"}"#),
        ),
        (
            "FB12",
            node_remove::write(&f.ctx, PEER_A, br#"{"deveui":"0011223344556677"}"#),
        ),
        (
            "FB17",
            setup::write(&f.ctx, PEER_A, br#"{"role":"server"}"#),
        ),
        (
            "FB18",
            export_destination::write(&f.ctx, PEER_A, br#"{"host":"hub-1.local"}"#),
        ),
        (
            "FB1A",
            pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#),
        ),
        (
            "FB1B",
            radio_mode::write(&f.ctx, PEER_A, br#"{"mode":"p2p"}"#),
        ),
        (
            "FB1C",
            cluster_join::write(&f.ctx, PEER_A, BUNDLE.as_bytes()),
        ),
    ] {
        assert_eq!(
            r.expect_err(&format!("{name} must be gated")),
            ReqError::NotAuthorized,
            "{name}"
        );
    }
    for (name, r) in [
        (
            "FB0F",
            beacon_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#),
        ),
        (
            "FB11",
            node_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#),
        ),
    ] {
        assert_eq!(
            r.expect_err(&format!("{name} gated")),
            ReqError::NotAuthorized,
            "{name}"
        );
    }
    // Nothing was mutated by any of those attempts.
    assert_eq!(f.config.write_count(), 0);
    assert_eq!(
        f.setup.status(),
        SetupStatus::default(),
        "an ungated FB17 must not have marked this Hub provisioned"
    );
    assert!(
        f.export.requests().is_empty(),
        "an ungated FB18 must not have reached the destination seam at all"
    );
    assert!(
        f.pairing.submitted().is_empty(),
        "an ungated FB1A must not have reached the portal"
    );
    assert!(
        f.radio.writes().is_empty(),
        "an ungated FB1B must not have reached the radio-mode seam"
    );
    assert!(
        f.cluster.joins().is_empty(),
        "an ungated FB1C must not have reached the join"
    );
}

#[test]
fn a_second_peer_cannot_ride_the_first_peers_session() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    label::read(&f.ctx, PEER_A, MTU, 0).expect("A is authenticated");
    assert_eq!(
        label::read(&f.ctx, PEER_B, MTU, 0).expect_err("B is not"),
        ReqError::NotAuthorized,
        "authentication is per peer, not per session"
    );
}

#[test]
fn fb01_success_and_failure_shapes() {
    let f = fixture(EyeSection::default());
    // Before any session, a read says so plainly.
    let v = json(&auth::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
    assert_eq!(v["message"], "Not authenticated");

    f.session
        .start(f.clock.as_ref(), 300, &f.hub_mac, "test-hub");
    // A wrong token is an ATT error, and leaves the peer unauthenticated.
    assert_eq!(
        auth::write(&f.ctx, PEER_A, b"WRONG1").expect_err("wrong token"),
        ReqError::NotAuthorized
    );
    assert!(!f.ctx.is_authenticated_for(PEER_A));

    let tok = f
        .session
        .qr_payload(&f.hub_mac, "test-hub")
        .expect("live")
        .tok;
    auth::write(&f.ctx, PEER_A, tok.as_bytes()).expect("right token");
    let v = json(&auth::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true);
    assert_eq!(v["message"], "Authenticated");
    // …and the *other* peer still reads its own answer, not this one.
    let v = json(&auth::read(&f.ctx, PEER_B, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
}

#[test]
fn a_wrong_guess_from_a_second_phone_does_not_knock_the_first_one_out() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let _ = auth::write(&f.ctx, PEER_B, b"NOPE12");
    assert!(
        f.ctx.is_authenticated_for(PEER_A),
        "B's failure must not de-authenticate A mid-flow"
    );
}

#[test]
fn an_expired_session_de_authenticates_on_the_next_operation() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    label::read(&f.ctx, PEER_A, MTU, 0).expect("alive");
    f.clock.set(1_700_000_000 + 301);
    assert_eq!(
        label::read(&f.ctx, PEER_A, MTU, 0).expect_err("expired"),
        ReqError::NotAuthorized
    );
    assert_eq!(
        f.ctx.authenticated_peer(),
        None,
        "the gate clears the peer when the session has died"
    );
}

#[test]
fn fb0f_write_then_read_is_idempotent_and_read_before_write_is_no_request() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);

    // Rule 2: a read with no prior write is a defined error, never a default first page.
    let v = json(&beacon_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["error"], "no_request");
    assert!(v.get("items").is_none(), "not a page");

    enroll::add(&f.ctx.host, Origin::Cli, BEACON, Some("Freezer 1")).expect("enrolls");
    beacon_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#).expect("writes");

    // Rule 3: reads are idempotent.
    let first = beacon_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable");
    let second = beacon_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable");
    assert_eq!(first, second, "the cursor advances only on write");
    let v = json(&first);
    assert_eq!(v["total"], 1);
    assert_eq!(v["count"], 1);
    assert_eq!(v["items"][0]["mac"], BEACON);
    assert_eq!(v["items"][0]["name"], "Freezer 1");
}

#[test]
fn fb0f_matches_beacon_list_page_byte_for_byte() {
    // The CLI's `beacon-list --page N --mtu M` exists precisely so the paging contract is checkable
    // without a phone. If they ever diverge, that check is worthless.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for i in 0..12 {
        let mac = format!("AA:BB:CC:DD:EE:{i:02X}");
        enroll::add(&f.ctx.host, Origin::Cli, &mac, Some("Freezer aisle 3")).expect("enrolls");
    }
    let section = f.config.load().expect("load");
    let list = BeaconList::from_tags(&section.tags);

    for page in 0..5u16 {
        beacon_list::write(
            &f.ctx,
            PEER_A,
            MTU,
            format!(r#"{{"page":{page}}}"#).as_bytes(),
        )
        .expect("writes");
        let over_gatt = beacon_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable");
        let over_cli = ListRead::to_bytes(&control_ble::peripheral::paging::build_page(
            &list,
            page,
            MTU,
            PeripheralConfig::default().max_page_bytes,
        ));
        assert_eq!(
            over_gatt, over_cli,
            "page {page}: FB0F and `beacon-list --page` must be the same bytes"
        );
    }
}

#[test]
fn fb0f_at_mtu_23_returns_an_att_error_not_a_fragment() {
    // §5.1: v1's refusal was 131 bytes and fired only below 107, so at MTU 23 the phone received
    // `{"error":"mtu_too_smal` — the guard emitting exactly what it existed to prevent.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    enroll::add(&f.ctx.host, Origin::Cli, BEACON, Some("Freezer 1")).expect("enrolls");
    beacon_list::write(&f.ctx, PEER_A, 23, br#"{"page":0}"#).expect("writes");
    assert_eq!(
        beacon_list::read(&f.ctx, PEER_A, 23, 0).expect_err("cannot fit"),
        ReqError::Failed,
        "an ATT-level error is legible at any MTU because it is not carried in the value"
    );
}

#[test]
fn no_read_at_mtu_23_ever_returns_bytes_that_are_not_a_complete_json_document() {
    // The general form of §5.1: every read path takes the MTU into account. A path that does not is
    // the same bug in waiting.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    enroll::add(&f.ctx.host, Origin::Cli, BEACON, Some("Freezer 1")).expect("enrolls");
    beacon_list::write(&f.ctx, PEER_A, 23, br#"{"page":0}"#).expect("writes");
    beacon_add::write(&f.ctx, PEER_A, br#"{"mac":"00:11:22:33:44:55"}"#).expect("writes");
    beacon_remove::write(&f.ctx, PEER_A, br#"{"mac":"00:11:22:33:44:55"}"#).expect("writes");
    // FB11's write refuses on this fixture — its Node seam is unwired, and §6.1 says that must not
    // be answered as an empty list. The read below still has to be legible at MTU 23.
    let _ = node_list::write(&f.ctx, PEER_A, 23, br#"{"page":0}"#);
    node_remove::write(&f.ctx, PEER_A, br#"{"deveui":"0011223344556677"}"#).expect("writes");
    setup::write(&f.ctx, PEER_A, br#"{"role":"server"}"#).expect("writes");
    export_destination::write(&f.ctx, PEER_A, br#"{"host":"hub-1.local"}"#).expect("writes");
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    radio_mode::write(&f.ctx, PEER_A, br#"{"mode":"p2p"}"#).expect("writes");

    let reads: Vec<(&str, Result<Vec<u8>, ReqError>)> = vec![
        ("FB01", auth::read(&f.ctx, PEER_A, 23, 0)),
        ("FB07", device_info::read(&f.ctx, 23, 0)),
        ("FB0A", label::read(&f.ctx, PEER_A, 23, 0)),
        ("FB0B", time::read(&f.ctx, PEER_A, 23, 0)),
        ("FB0C", lan::status_read(&f.ctx, PEER_A, 23, 0)),
        ("FB0D", node_add::read(&f.ctx, PEER_A, 23, 0)),
        ("FB0E", beacon_add::read(&f.ctx, PEER_A, 23, 0)),
        ("FB0F", beacon_list::read(&f.ctx, PEER_A, 23, 0)),
        ("FB10", beacon_remove::read(&f.ctx, PEER_A, 23, 0)),
        ("FB11", node_list::read(&f.ctx, PEER_A, 23, 0)),
        ("FB12", node_remove::read(&f.ctx, PEER_A, 23, 0)),
        ("FB16", setup::read(&f.ctx, PEER_A, 23, 0)),
        ("FB17", setup::read_result(&f.ctx, PEER_A, 23, 0)),
        ("FB18", export_destination::read(&f.ctx, PEER_A, 23, 0)),
        ("FB1A", pairing_code::read(&f.ctx, PEER_A, 23, 0)),
        ("FB1B", radio_mode::read(&f.ctx, PEER_A, 23, 0)),
        ("FB1C", cluster_join::read(&f.ctx, PEER_A, 23, 0)),
    ];
    for (name, r) in reads {
        match r {
            // Refusing is correct at MTU 23.
            Err(_) => {}
            Ok(bytes) => {
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_else(|e| {
                    panic!("{name} returned a fragment at MTU 23: {e}; bytes {bytes:?}")
                });
                assert!(
                    bytes.len() <= 22,
                    "{name} returned {} bytes, which cannot arrive in one PDU at MTU 23",
                    bytes.len()
                );
            }
        }
    }
}

#[test]
fn read_blob_offsets_return_the_correct_slice_and_invalid_offset_past_the_end() {
    // §5.2: v1 ignored the offset, so a READ_BLOB at offset 22 received the whole value from byte 0
    // again and the client concatenated a duplicated prefix. Silent corruption.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for i in 0..8 {
        let mac = format!("AA:BB:CC:DD:EE:{i:02X}");
        enroll::add(&f.ctx.host, Origin::Cli, &mac, Some("Cold room 12 north")).expect("enrolls");
    }
    beacon_list::write(&f.ctx, PEER_A, 512, br#"{"page":0}"#).expect("writes");
    let whole = beacon_list::read(&f.ctx, PEER_A, 512, 0).expect("readable");
    assert!(whole.len() > 30);

    let tail = beacon_list::read(&f.ctx, PEER_A, 512, 22).expect("readable");
    assert_eq!(tail, whole[22..].to_vec(), "the offset is honoured");
    assert_ne!(tail, whole, "and it is NOT the whole value again");

    // Reassembling from slices gives back exactly the value — no duplicated prefix.
    let mut rebuilt = beacon_list::read(&f.ctx, PEER_A, 512, 0).expect("readable")[..20].to_vec();
    rebuilt.extend(beacon_list::read(&f.ctx, PEER_A, 512, 20).expect("readable"));
    assert_eq!(rebuilt, whole);

    // Exactly at the end is an empty slice; past it is an error.
    let at_end = u16::try_from(whole.len()).expect("fits");
    assert!(beacon_list::read(&f.ctx, PEER_A, 512, at_end)
        .expect("readable")
        .is_empty());
    assert_eq!(
        beacon_list::read(&f.ctx, PEER_A, 512, at_end + 1).expect_err("past the end"),
        ReqError::InvalidOffset
    );
}

#[test]
fn fb10_and_the_cli_remove_are_indistinguishable_at_the_store_layer() {
    // AC 4, stated at the store layer rather than the API layer: two surfaces that agree today
    // because someone wrote them carefully is not the same as two that cannot disagree.
    let via_gatt = {
        let f = fixture(EyeSection::default());
        authenticate(&f, PEER_A);
        enroll::add(&f.ctx.host, Origin::Cli, BEACON, Some("Freezer 1")).expect("enrolls");
        beacon_remove::write(&f.ctx, PEER_A, br#"{"mac":"AA:BB:CC:DD:EE:FF"}"#).expect("writes");
        let section = f.config.load().expect("load");
        let actions: Vec<&'static str> =
            f.config.audit_entries().iter().map(|a| a.action).collect();
        (section, actions)
    };
    let via_cli = {
        let f = fixture(EyeSection::default());
        enroll::add(&f.ctx.host, Origin::Cli, BEACON, Some("Freezer 1")).expect("enrolls");
        enroll::remove(&f.ctx.host, Origin::Cli, BEACON).expect("removes");
        let section = f.config.load().expect("load");
        let actions: Vec<&'static str> =
            f.config.audit_entries().iter().map(|a| a.action).collect();
        (section, actions)
    };
    assert_eq!(via_gatt.0, via_cli.0, "identical EyeSection");
    assert_eq!(via_gatt.1, via_cli.1, "identical audit actions");
    assert_eq!(via_gatt.1, vec!["beacon-add", "beacon-remove"]);
}

#[test]
fn fb10_on_an_unknown_mac_is_success_false_with_a_message() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    beacon_remove::write(&f.ctx, PEER_A, br#"{"mac":"00:11:22:33:44:55"}"#)
        .expect("the write itself succeeds");
    let v = json(&beacon_remove::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false, "not an error — the FB10 contract");
    assert!(
        v["message"]
            .as_str()
            .expect("string")
            .contains("not enrolled"),
        "{v}"
    );
}

#[test]
fn fb0e_reports_the_monitoring_outcome() {
    // §6.2's ask has two halves: turn monitoring on, *or* tell the app clearly that it is off.
    let f = fixture(EyeSection {
        enabled: false,
        ..EyeSection::default()
    });
    authenticate(&f, PEER_A);
    beacon_add::write(
        &f.ctx,
        PEER_A,
        br#"{"mac":"AA:BB:CC:DD:EE:FF","name":"Freezer 1"}"#,
    )
    .expect("writes");
    let v = json(&beacon_add::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true);
    let msg = v["message"].as_str().expect("string");
    assert!(msg.contains("monitoring"), "{msg}");
    // No loop is hosted here, so the honest answer is `unavailable` — never silence.
    assert!(msg.contains("unavailable"), "{msg}");
    assert!(
        f.config.load().expect("load").enabled,
        "the flag still flipped"
    );
}

#[test]
fn fb0e_queues_the_same_enrollment_follow_up_as_the_other_two_surfaces() {
    let (f, mut rx) = fixture_with_dial(EyeSection::default());
    authenticate(&f, PEER_A);
    beacon_add::write(&f.ctx, PEER_A, br#"{"mac":"AA:BB:CC:DD:EE:FF"}"#).expect("writes");
    assert_eq!(
        rx.try_recv().expect("a follow-up was queued"),
        DialJob {
            mac: MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac"),
            kind: DialKind::ReadConfig,
        },
        "configuring on enroll is off, so a phone enrollment only reads"
    );

    // Without a central role there is no queue, and the enrollment still lands.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    beacon_add::write(&f.ctx, PEER_A, br#"{"mac":"AA:BB:CC:DD:EE:FF"}"#).expect("writes");
    let v = json(&beacon_add::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true);
}

#[test]
fn fb0e_on_a_malformed_mac_is_a_read_with_a_reason_not_a_lost_error() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    beacon_add::write(&f.ctx, PEER_A, br#"{"mac":"AA:BB:CC:DD:EE"}"#).expect("write succeeds");
    let v = json(&beacon_add::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
    assert!(
        !v["message"].as_str().expect("string").is_empty(),
        "a refusal with an empty message would be indistinguishable from a shrug"
    );
}

#[test]
fn disconnect_clears_this_peers_buffers_only() {
    // Every per-peer buffer, not only FB0F's: one that outlived a disconnect would serve one
    // phone's inventory to whoever connects next.
    let f = fixture_with_nodes(vec![node("0011223344556677", Some("SN-1"), None)]);
    authenticate(&f, PEER_A);
    beacon_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#).expect("writes");
    node_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#).expect("writes");
    node_remove::write(&f.ctx, PEER_A, br#"{"serial":"SN-9"}"#).expect("writes");
    setup::write(&f.ctx, PEER_A, br#"{"role":"server"}"#).expect("writes");
    export_destination::write(&f.ctx, PEER_A, br#"{"host":"hub-1.local"}"#).expect("writes");
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    radio_mode::write(&f.ctx, PEER_A, br#"{"mode":"p2p"}"#).expect("writes");
    // A second phone has to authenticate to get its own buffer; the session allows one peer at a
    // time, so re-authenticate as B.
    auth::write(
        &f.ctx,
        PEER_B,
        f.session
            .qr_payload(&f.hub_mac, "test-hub")
            .expect("live")
            .tok
            .as_bytes(),
    )
    .expect("B authenticates");
    beacon_list::write(&f.ctx, PEER_B, MTU, br#"{"page":0}"#).expect("writes");
    node_list::write(&f.ctx, PEER_B, MTU, br#"{"page":0}"#).expect("writes");
    node_remove::write(&f.ctx, PEER_B, br#"{"serial":"SN-9"}"#).expect("writes");
    setup::write(&f.ctx, PEER_B, br#"{"role":"client"}"#).expect("writes");
    export_destination::write(&f.ctx, PEER_B, br#"{"host":"hub-1.local"}"#).expect("writes");
    pairing_code::write(&f.ctx, PEER_B, br#"{"code":"23456X"}"#).expect("writes");
    radio_mode::write(&f.ctx, PEER_B, br#"{"mode":"off"}"#).expect("writes");

    f.ctx.on_disconnect(PEER_A);
    assert!(
        f.ctx.list_buffers.read(&PEER_A).is_none(),
        "A's buffer is gone"
    );
    assert!(
        f.ctx.list_buffers.read(&PEER_B).is_some(),
        "B's buffer survives"
    );
    assert!(f.ctx.node_list_buffers.read(&PEER_A).is_none());
    assert!(f.ctx.node_remove_results.read(&PEER_A).is_none());
    assert!(f.ctx.node_list_buffers.read(&PEER_B).is_some());
    assert!(f.ctx.node_remove_results.read(&PEER_B).is_some());
    assert!(f.ctx.setup_results.read(&PEER_A).is_none());
    assert!(f.ctx.setup_results.read(&PEER_B).is_some());
    assert!(f.ctx.export_results.read(&PEER_A).is_none());
    assert!(f.ctx.export_results.read(&PEER_B).is_some());
    assert!(f.ctx.pairing_results.read(&PEER_A).is_none());
    assert!(f.ctx.pairing_results.read(&PEER_B).is_some());
    assert!(f.ctx.radio_mode_results.read(&PEER_A).is_none());
    assert!(f.ctx.radio_mode_results.read(&PEER_B).is_some());
    // B was the authenticated peer, so A's disconnect must not have de-authenticated it.
    assert!(f.ctx.is_authenticated_for(PEER_B));

    f.ctx.on_disconnect(PEER_B);
    assert!(f.ctx.list_buffers.read(&PEER_B).is_none());
    assert!(f.ctx.node_list_buffers.read(&PEER_B).is_none());
    assert!(f.ctx.node_remove_results.read(&PEER_B).is_none());
    assert!(f.ctx.setup_results.read(&PEER_B).is_none());
    assert!(f.ctx.export_results.read(&PEER_B).is_none());
    assert!(f.ctx.pairing_results.read(&PEER_B).is_none());
    assert!(f.ctx.radio_mode_results.read(&PEER_B).is_none());
    assert_eq!(f.ctx.authenticated_peer(), None);
}

#[test]
fn a_second_lan_write_while_one_is_in_flight_is_in_progress() {
    // Reconfiguring an interface twice concurrently is how a Hub loses its own network.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    f.ctx
        .lan_in_flight
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        lan::config_write(&f.ctx, PEER_A, br#"{"mode":"dhcp"}"#).expect_err("in flight"),
        ReqError::InProgress
    );
}

#[test]
fn a_static_lan_config_without_an_address_is_refused() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    assert_eq!(
        lan::config_write(&f.ctx, PEER_A, br#"{"mode":"static"}"#).expect_err("needs ipv4"),
        ReqError::InvalidValueLength,
        "static with no address would apply an empty configuration"
    );
    // And more than three resolvers is refused too.
    let body = br#"{"mode":"static","ipv4":{"address":"10.0.0.2","prefix":24,"gateway":"10.0.0.1","dns":["1.1.1.1","8.8.8.8","9.9.9.9","4.4.4.4"]}}"#;
    assert_eq!(
        lan::config_write(&f.ctx, PEER_A, body).expect_err("too many dns"),
        ReqError::InvalidValueLength
    );
}

#[test]
fn fb0b_refuses_an_epoch_outside_the_plausible_window() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for bad in [0i64, 1, 1_500_000_000, 5_000_000_000] {
        assert_eq!(
            time::write(&f.ctx, PEER_A, format!(r#"{{"epoch":{bad}}}"#).as_bytes())
                .expect_err("out of range"),
            ReqError::InvalidValueLength,
            "{bad}"
        );
    }
    // In range, the write goes through to the clock seam and really moves it — the one-shot
    // bootstrap an RTC-less Hub needs.
    time::write(&f.ctx, PEER_A, br#"{"epoch":1700000050}"#).expect("in range");
    let v = json(&time::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["epoch"], 1_700_000_050);
    assert_eq!(v["synchronized"], true);
}

#[test]
fn a_large_clock_step_does_not_de_authenticate_the_phone_that_made_it() {
    // The case this guards: an RTC-less Hub boots to 1970 and the phone bootstraps it to the
    // present. The jump is decades — far more than the 300 s idle timeout — so a wall-clock idle
    // timer would expire the session as a side effect of its own successful operation.
    let f = fixture(EyeSection::default());
    f.clock.set(control_ble::host::EPOCH_2020);
    authenticate(&f, PEER_A);

    time::write(&f.ctx, PEER_A, br#"{"epoch":1800000000"#.as_ref())
        .expect_err("malformed body is still refused");
    time::write(&f.ctx, PEER_A, br#"{"epoch":1800000000}"#).expect("a decade-long step");

    assert!(
        f.ctx.is_authenticated_for(PEER_A),
        "a clock step is not idleness: the phone must survive the write it just made"
    );
    // And the session keeps working afterwards.
    label::read(&f.ctx, PEER_A, MTU, 0).expect("still authenticated");
    let v = json(&time::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["epoch"], 1_800_000_000);
}

#[test]
fn fb0b_reports_a_failure_from_the_clock_seam_rather_than_a_silent_ok() {
    // The harness's `WallClock` refuses to set the system clock. A phone must learn that, not be
    // told the bootstrap worked.
    let clock: Arc<dyn control_ble::host::SystemClock> = Arc::new(control_ble::host::WallClock);
    let session = Arc::new(SessionSlot::new());
    let hub_mac = MacAddr::parse("B8:27:EB:11:22:33").expect("mac");
    let host = Host {
        config: Arc::new(MemoryConfigStore::default()),
        publisher: Arc::new(NullPublisher),
        readings: Arc::new(NullReadingSink),
        subsystem: Arc::new(NoopSubsystem),
        standby: Arc::new(NeverStandby),
        network: Arc::new(UnavailableNetwork),
        clock: clock.clone(),
        device: Arc::new(StaticDeviceInfo::for_tests()),
        nodes: Arc::new(UnwiredNodeEnroll),
        setup: Arc::new(UnwiredSetupState),
        export: Arc::new(UnwiredExportDestination),
        pairing: Arc::new(UnwiredPortalPairing),
        radio: Arc::new(UnwiredRadioMode),
        cluster: Arc::new(UnwiredClusterJoin),
    };
    let ctx = Arc::new(GattCtx::new(
        host,
        session.clone(),
        Arc::new(AdapterSlot::new()),
        Arc::new(RwLock::new(CentralState::default())),
        PeripheralConfig::default(),
        hub_mac,
        EventBus::new(),
    ));
    session.start(clock.as_ref(), 300, &hub_mac, "test-hub");
    let tok = session.qr_payload(&hub_mac, "test-hub").expect("live").tok;
    auth::write(&ctx, PEER_A, tok.as_bytes()).expect("authenticates");
    assert_eq!(
        time::write(&ctx, PEER_A, br#"{"epoch":1800000000}"#).expect_err("unwired"),
        ReqError::Failed
    );
}

#[test]
fn fb0a_bounds_the_label_and_refuses_control_characters() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let long = "x".repeat(label::MAX_LABEL_CHARS + 1);
    assert_eq!(
        label::write(
            &f.ctx,
            PEER_A,
            format!(r#"{{"label":"{long}"}}"#).as_bytes()
        )
        .expect_err("too long"),
        ReqError::InvalidValueLength
    );
    assert_eq!(
        label::write(&f.ctx, PEER_A, br#"{"label":"Freezer\n1"}"#).expect_err("control char"),
        ReqError::InvalidValueLength
    );
    // The harness refuses to write a label at all, and says so rather than reporting success.
    assert_eq!(
        label::write(&f.ctx, PEER_A, br#"{"label":"Freezer room"}"#).expect_err("unwired"),
        ReqError::Failed
    );
    let v = json(&label::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(
        v["label"], "test-hub",
        "the read still reports the current one"
    );
}

#[test]
fn fb0d_reads_the_documented_default_before_any_write() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let v = json(&node_add::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["pending"], false);
    assert_eq!(v["success"], false);
    assert_eq!(v["message"], "");
    assert_eq!(v["deveui"], "");

    node_add::write(
        &f.ctx,
        PEER_A,
        br#"{"deveui":"0011223344556677","appkey":"k","name":"n","serial_number":"s"}"#,
    )
    .expect("writes");
    let v = json(&node_add::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["deveui"], "0011223344556677");
    assert!(v["message"].as_str().expect("string").contains("not wired"));
}

#[test]
fn fb0d_rejects_an_unknown_field_rather_than_ignoring_it() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    assert_eq!(
        node_add::write(
            &f.ctx,
            PEER_A,
            br#"{"deveui":"a","appkey":"k","name":"n","serial_number":"s","surprise":1}"#
        )
        .expect_err("unknown field"),
        ReqError::InvalidValueLength
    );
}

#[test]
fn removing_over_fb10_makes_the_beacon_rediscoverable() {
    // UI-13: the beacon reappears as `discovered: true`, which is correct rather than a failed
    // removal. The tracked entry must be dropped, not left as an enrolled one.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    enroll::add(&f.ctx.host, Origin::Cli, BEACON, None).expect("enrolls");
    let mac = MacAddr::parse(BEACON).expect("mac");
    {
        let mut central = f.ctx.central.write().expect("lock");
        let enrolled = std::collections::BTreeSet::from([mac]);
        central.entry(mac, None, &enrolled);
        assert!(central.beacons.contains_key(&mac));
    }
    beacon_remove::write(&f.ctx, PEER_A, br#"{"mac":"AA:BB:CC:DD:EE:FF"}"#).expect("writes");
    assert!(
        !f.ctx
            .central
            .read()
            .expect("lock")
            .beacons
            .contains_key(&mac),
        "the tracked entry is dropped, so the next frame rediscovers it as a stranger"
    );
}

#[test]
fn the_registered_characteristic_set_is_exactly_the_documented_one() {
    // The whole allocation table, as a list. FB05/FB06 deleted, FB13–FB15 reserved-not-served.
    assert_eq!(
        gatt::REGISTERED,
        [
            0xFB01, 0xFB02, 0xFB03, 0xFB04, 0xFB07, 0xFB08, 0xFB09, 0xFB0A, 0xFB0B, 0xFB0C, 0xFB0D,
            0xFB0E, 0xFB0F, 0xFB10, 0xFB11, 0xFB12, 0xFB16, 0xFB17, 0xFB18, 0xFB1A, 0xFB1B, 0xFB1C
        ]
    );
    assert_eq!(
        gatt::RESERVED,
        [0xFB13, 0xFB14, 0xFB15],
        "FB11/FB12 left the reserved set when they were built; the P2P three stay"
    );
    assert_eq!(
        gatt::RETIRED,
        [0xFB19],
        "FB19 is retired (plan/control/cluster.md §7): its number stays spent"
    );
    for absent in gatt::DELETED
        .iter()
        .chain(gatt::RESERVED.iter())
        .chain(gatt::RETIRED.iter())
    {
        assert!(!gatt::REGISTERED.contains(absent), "{absent:#06X}");
    }
}

#[test]
fn a_write_result_with_an_empty_message_is_still_distinguishable() {
    assert_ne!(
        WriteResult::ok("").to_bytes(),
        WriteResult::fail("").to_bytes()
    );
}

/// The seam that replaces v1's `event_router.rs`: a host with an LCD learns about auth without
/// reaching into the session.
#[test]
fn auth_outcomes_reach_a_subscriber_without_carrying_the_token() {
    let f = fixture(EyeSection::default());
    let mut rx = f.ctx.events.subscribe();
    f.session
        .start(f.clock.as_ref(), 300, &f.hub_mac, "test-hub");
    let tok = f
        .session
        .qr_payload(&f.hub_mac, "test-hub")
        .expect("live")
        .tok;

    auth::write(&f.ctx, PEER_A, b"wrong!").expect_err("refused");
    assert_eq!(
        rx.try_recv().expect("an event"),
        PeripheralEvent::AuthFailed {
            addr: "AA:AA:AA:AA:AA:AA".into()
        }
    );

    auth::write(&f.ctx, PEER_A, tok.as_bytes()).expect("accepted");
    let ok = rx.try_recv().expect("an event");
    assert_eq!(
        ok,
        PeripheralEvent::AuthSucceeded {
            addr: "AA:AA:AA:AA:AA:AA".into()
        }
    );
    let json = serde_json::to_string(&ok).expect("serialize");
    assert!(
        !json.contains(&tok),
        "the token must not ride the event: {json}"
    );
}

// -------------------------------------------------------------------------------------------
// FB11 / FB12 — the Node half of §6.1's inventory ask
// -------------------------------------------------------------------------------------------

/// Walk FB11 the way the app does: write a page, read it, stop when one comes back empty.
fn walk_nodes(f: &Fixture, peer: [u8; 6], mtu: u16) -> (Vec<serde_json::Value>, String) {
    let mut seen = Vec::new();
    let mut rev = String::new();
    for page in 0..64u16 {
        node_list::write(
            &f.ctx,
            peer,
            mtu,
            format!(r#"{{"page":{page}}}"#).as_bytes(),
        )
        .expect("writes");
        let v = json(&node_list::read(&f.ctx, peer, mtu, 0).expect("readable"));
        assert!(v.get("error").is_none(), "page {page} refused: {v}");
        assert_eq!(v["page"], page, "a page must report its own index");
        let items = v["items"].as_array().expect("items").clone();
        assert_eq!(v["count"], items.len(), "count must match items");
        let this_rev = v["rev"].as_str().expect("rev").to_string();
        if rev.is_empty() {
            rev = this_rev;
        } else {
            assert_eq!(rev, this_rev, "every page of a walk carries the same rev");
        }
        if items.is_empty() {
            return (seen, rev);
        }
        seen.extend(items);
    }
    panic!("pagination did not terminate");
}

#[test]
fn fb11_read_before_write_is_no_request_never_a_default_first_page() {
    // Rule 2 of the paging contract, which FB11 adopts rather than re-decides.
    let f = fixture_with_nodes(vec![node(
        "0011223344556677",
        Some("SN-1"),
        Some("Fridge 1"),
    )]);
    authenticate(&f, PEER_A);
    let v = json(&node_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["error"], "no_request");
    assert!(
        v.get("items").is_none(),
        "a refusal is not half a page: {v}"
    );
}

#[test]
fn fb11_serves_every_node_exactly_once_in_deveui_order() {
    let f = fixture_with_nodes(vec![
        node("ffffffffffffffff", Some("SN-3"), Some("Freezer 3")),
        node("0011223344556677", Some("SN-1"), Some("Fridge 1")),
        node("8899aabbccddeeff", Some("SN-2"), None),
    ]);
    authenticate(&f, PEER_A);

    let (seen, _rev) = walk_nodes(&f, PEER_A, MTU);
    let euis: Vec<&str> = seen
        .iter()
        .map(|i| i["deveui"].as_str().expect("deveui"))
        .collect();
    assert_eq!(
        euis,
        vec!["0011223344556677", "8899aabbccddeeff", "ffffffffffffffff"],
        "the walked SEQUENCE is the sorted list — comparing counts would miss a duplicate that \
         replaced a missing item"
    );
    // The item shape the app team gets: the serial rides along, and an absent one is absent rather
    // than an empty string it would render as a blank label.
    assert_eq!(seen[0]["serial"], "SN-1");
    assert_eq!(seen[0]["name"], "Fridge 1");
    assert!(seen[1].get("name").is_none(), "{}", seen[1]);
}

#[test]
fn fb11_on_a_hub_that_cannot_look_is_an_att_error_not_an_empty_list() {
    // §6.1's whole point is that the list is a view of the Hub. `total: 0` is a claim about the
    // site; a seam that failed is a claim about the Hub, and the phone has to be able to tell them
    // apart. An ATT-level error is legible at any MTU, and carries no misleading payload.
    let f = fixture_with_nodes(vec![node("0011223344556677", Some("SN-1"), None)]);
    authenticate(&f, PEER_A);
    f.nodes
        .as_ref()
        .expect("wired")
        .fail_list_with("the device config is unreadable");
    assert_eq!(
        node_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#).expect_err("cannot look"),
        ReqError::Failed
    );
    // And nothing stale was left behind for the read to serve.
    let v = json(&node_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["error"], "no_request");
}

#[test]
fn fb11_an_empty_hub_is_an_empty_page_not_an_error() {
    let f = fixture_with_nodes(Vec::new());
    authenticate(&f, PEER_A);
    node_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#).expect("writes");
    let v = json(&node_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["total"], 0);
    assert_eq!(v["count"], 0);
    assert!(v["items"].as_array().expect("items").is_empty());
}

#[test]
fn fb11_a_malformed_request_is_a_buffered_reason_not_an_att_error() {
    // The phone must get to read back *which* key it got wrong — an ATT error has no room to say.
    let f = fixture_with_nodes(vec![node("0011223344556677", Some("SN-1"), None)]);
    authenticate(&f, PEER_A);
    node_list::write(&f.ctx, PEER_A, MTU, br#"{"pge":1}"#).expect("the write itself succeeds");
    let v = json(&node_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["error"], "bad_request");
    assert!(!v["message"].as_str().expect("message").is_empty());
}

#[test]
fn fb11_and_fb0f_do_not_consume_each_others_cursor() {
    // One connection, two inventories. A shared buffer would make walking one list corrupt the
    // other, which is the sort of defect that only shows up on a site that has both.
    let f = fixture_with_nodes(vec![node("0011223344556677", Some("SN-1"), None)]);
    authenticate(&f, PEER_A);
    enroll::add(&f.ctx.host, Origin::Cli, BEACON, Some("Freezer 1")).expect("enrolls");

    beacon_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#).expect("writes");
    node_list::write(&f.ctx, PEER_A, MTU, br#"{"page":0}"#).expect("writes");

    let beacons = json(&beacon_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    let nodes = json(&node_list::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(beacons["items"][0]["mac"], BEACON);
    assert_eq!(nodes["items"][0]["deveui"], "0011223344556677");
    assert_ne!(beacons["rev"], nodes["rev"], "two lists, two revs");
}

#[test]
fn fb12_removes_what_fb11_then_stops_showing_and_the_rev_moves() {
    // The property that makes the list a view of the Hub rather than a claim: what FB11 serves
    // after FB12 has run is what FB12 actually did.
    let f = fixture_with_nodes(vec![
        node("0011223344556677", Some("SN-1"), Some("Fridge 1")),
        node("8899aabbccddeeff", Some("SN-2"), Some("Fridge 2")),
    ]);
    authenticate(&f, PEER_A);
    let (before, rev_before) = walk_nodes(&f, PEER_A, MTU);
    assert_eq!(before.len(), 2);

    node_remove::write(&f.ctx, PEER_A, br#"{"serial":"SN-1"}"#).expect("writes");
    let v = json(&node_remove::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true, "{v}");
    assert!(v["message"]
        .as_str()
        .expect("message")
        .contains("0011223344556677"));

    let (after, rev_after) = walk_nodes(&f, PEER_A, MTU);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["deveui"], "8899aabbccddeeff");
    assert_ne!(
        rev_before, rev_after,
        "the rev moves on a real change, which is what restarts a stale walk"
    );
}

#[test]
fn fb12_by_deveui_and_by_serial_are_the_same_removal() {
    // `plan/control/radio/plan.md` §5 "One identifier": a caller never needs to know the transport
    // to know how to address a node, whichever identifier it happens to hold.
    let seed = || {
        vec![
            node("0011223344556677", Some("SN-1"), Some("Fridge 1")),
            node("8899aabbccddeeff", Some("SN-2"), None),
        ]
    };
    let by_eui = fixture_with_nodes(seed());
    let by_serial = fixture_with_nodes(seed());
    authenticate(&by_eui, PEER_A);
    authenticate(&by_serial, PEER_A);

    node_remove::write(&by_eui.ctx, PEER_A, br#"{"deveui":"0011223344556677"}"#).expect("writes");
    node_remove::write(&by_serial.ctx, PEER_A, br#"{"serial":"SN-1"}"#).expect("writes");

    assert_eq!(
        by_eui.nodes.as_ref().expect("wired").entries(),
        by_serial.nodes.as_ref().expect("wired").entries(),
        "both spellings must leave the same list behind, not merely both report success"
    );
    assert_eq!(walk_nodes(&by_eui, PEER_A, MTU).0.len(), 1);
}

#[test]
fn fb12_needs_exactly_one_identifier_and_says_which_way_it_was_wrong() {
    let f = fixture_with_nodes(vec![node("0011223344556677", Some("SN-1"), None)]);
    authenticate(&f, PEER_A);
    for (label, body, expect) in [
        ("neither", &br#"{}"#[..], "one of"),
        (
            "both",
            &br#"{"deveui":"0011223344556677","serial":"SN-1"}"#[..],
            "not both",
        ),
    ] {
        node_remove::write(&f.ctx, PEER_A, body).expect("the write itself succeeds");
        let v = json(&node_remove::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
        assert_eq!(v["success"], false, "{label}: {v}");
        assert!(
            v["message"].as_str().expect("message").contains(expect),
            "{label}: {v}"
        );
    }
    // Nothing was removed by any of those.
    assert_eq!(f.nodes.as_ref().expect("wired").entries().len(), 1);

    // A shape this Hub does not implement — FB10's `{mac}` at FB12 — is refused at the ATT level,
    // because the answer is "wrong characteristic", not "wrong value".
    assert_eq!(
        node_remove::write(&f.ctx, PEER_A, br#"{"mac":"AA:BB:CC:DD:EE:FF"}"#)
            .expect_err("unknown key"),
        ReqError::InvalidValueLength
    );
}

#[test]
fn fb12_on_an_unknown_node_is_success_false_with_a_message() {
    // FB10's contract, kept: an installer's typo has to come back as something they can read.
    let f = fixture_with_nodes(vec![node("0011223344556677", Some("SN-1"), None)]);
    authenticate(&f, PEER_A);
    node_remove::write(&f.ctx, PEER_A, br#"{"serial":"SN-9"}"#).expect("writes");
    let v = json(&node_remove::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
    assert!(
        v["message"].as_str().expect("message").contains("SN-9"),
        "the refusal names the identifier that missed: {v}"
    );
    assert_eq!(f.nodes.as_ref().expect("wired").entries().len(), 1);
}

#[test]
fn fb12_read_before_any_write_is_a_refusal_rather_than_a_stale_success() {
    let f = fixture_with_nodes(vec![node("0011223344556677", Some("SN-1"), None)]);
    authenticate(&f, PEER_A);
    let v = json(&node_remove::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
    assert!(!v["message"].as_str().expect("message").is_empty());
}

#[test]
fn fb12_on_an_ambiguous_serial_removes_nothing() {
    // Guessing which of two devices sharing a serial the installer meant would turn a data defect
    // into a deleted device.
    let f = fixture_with_nodes(vec![
        node("0011223344556677", Some("SN-DUP"), None),
        node("8899aabbccddeeff", Some("SN-DUP"), None),
    ]);
    authenticate(&f, PEER_A);
    node_remove::write(&f.ctx, PEER_A, br#"{"serial":"SN-DUP"}"#).expect("writes");
    let v = json(&node_remove::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false, "{v}");
    assert_eq!(
        f.nodes.as_ref().expect("wired").entries().len(),
        2,
        "nothing was removed"
    );
    assert_eq!(walk_nodes(&f, PEER_A, MTU).0.len(), 2);
}

// -------------------------------------------------------------------------------------------
// FB16 / FB17 — the provisioned/role read and the setup-complete write (GitLab #58, §6.3)
// -------------------------------------------------------------------------------------------

#[test]
fn fb16_before_any_setup_says_not_provisioned_with_a_null_role() {
    // The state a Hub out of the box is in, and the one the app branches on to decide whether to
    // run its wizard at all.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let v = json(&setup::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["provisioned"], false);
    assert!(
        v["role"].is_null(),
        "an explicit null, not an absent key: {v}"
    );
}

#[test]
fn fb17_marks_setup_complete_and_fb16_says_so_to_a_second_phone() {
    // AC 1 of #58: a second phone reads `provisioned=true` after the first phone's wizard, with no
    // shared app state. The two peers here share nothing but the Hub.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    setup::write(&f.ctx, PEER_A, br#"{"role":"server"}"#).expect("the write succeeds");
    let result = json(&setup::read_result(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(result["success"], true);
    assert!(
        result["message"]
            .as_str()
            .expect("string")
            .contains("server"),
        "the result says which role was recorded: {result}"
    );

    // A second phone, authenticating on its own, with no buffer of its own.
    auth::write(
        &f.ctx,
        PEER_B,
        f.session
            .qr_payload(&f.hub_mac, "test-hub")
            .expect("live")
            .tok
            .as_bytes(),
    )
    .expect("B authenticates");
    let v = json(&setup::read(&f.ctx, PEER_B, MTU, 0).expect("readable"));
    assert_eq!(v["provisioned"], true);
    assert_eq!(v["role"], "server");
}

#[test]
fn fb17_is_idempotent_and_records_the_origin() {
    // A retried write — a dropped ACK, a phone re-running the last wizard step — lands on the same
    // state rather than toggling anything.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    setup::write(&f.ctx, PEER_A, br#"{"role":"client"}"#).expect("writes");
    setup::write(&f.ctx, PEER_A, br#"{"role":"client"}"#).expect("writes again");
    assert_eq!(
        f.setup.status(),
        SetupStatus {
            provisioned: true,
            role: Some(HubRole::Client),
        }
    );
    let writes = f.setup.writes.lock().expect("writes").clone();
    assert_eq!(
        writes,
        vec![
            (Origin::Ble, HubRole::Client),
            (Origin::Ble, HubRole::Client)
        ],
        "both reached the seam as BLE-origin writes, not as a silent no-op"
    );
}

#[test]
fn fb17_refuses_an_unknown_role_and_an_unknown_key_without_writing() {
    // `deny_unknown_fields` and a closed role set: a phone speaking a shape this Hub does not
    // implement is told so, rather than marking setup complete with a role nobody read.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for body in [
        &br#"{"role":"gateway"}"#[..],
        &br#"{"role":"server","extra":1}"#[..],
        &br#"{}"#[..],
        &br#"not json"#[..],
    ] {
        assert_eq!(
            setup::write(&f.ctx, PEER_A, body).expect_err("refused"),
            ReqError::InvalidValueLength,
            "body {:?}",
            String::from_utf8_lossy(body)
        );
    }
    assert_eq!(
        f.setup.status(),
        SetupStatus::default(),
        "nothing was written by any refusal"
    );
}

#[test]
fn fb16_reports_a_hub_that_cannot_look_as_an_att_error_not_as_unprovisioned() {
    // The distinction the whole ask rests on: `provisioned: false` is a claim about this Hub,
    // a failure is a claim about whether anyone could ask. A phone that cannot tell them apart
    // walks the wizard again against a configured Hub.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    f.setup.make_unreadable("config unreadable");
    assert_eq!(
        setup::read(&f.ctx, PEER_A, MTU, 0).expect_err("cannot answer"),
        ReqError::Failed
    );
}

#[test]
fn fb17_read_before_any_write_is_a_refusal_with_a_reason() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let v = json(&setup::read_result(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
    assert!(
        !v["message"].as_str().expect("string").is_empty(),
        "a refusal with an empty message is a shrug"
    );
}

#[test]
fn fb16_and_fb17_honour_the_read_offset() {
    // §5.2, per read path: a READ_BLOB at a non-zero offset returns that slice, never the whole
    // value from byte 0 again.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    setup::write(&f.ctx, PEER_A, br#"{"role":"server"}"#).expect("writes");

    for (name, whole) in [
        (
            "FB16",
            setup::read(&f.ctx, PEER_A, 512, 0).expect("readable"),
        ),
        (
            "FB17",
            setup::read_result(&f.ctx, PEER_A, 512, 0).expect("readable"),
        ),
    ] {
        assert!(whole.len() > 12, "{name} is long enough to slice");
        let read_at = |off: u16| match name {
            "FB16" => setup::read(&f.ctx, PEER_A, 512, off),
            _ => setup::read_result(&f.ctx, PEER_A, 512, off),
        };
        let tail = read_at(10).expect("readable");
        assert_eq!(tail, whole[10..].to_vec(), "{name}: the offset is honoured");
        assert_ne!(tail, whole, "{name}: and it is not the whole value again");

        let mut rebuilt = whole[..10].to_vec();
        rebuilt.extend(tail);
        assert_eq!(rebuilt, whole, "{name}: slices reassemble exactly");

        let at_end = u16::try_from(whole.len()).expect("fits");
        assert!(read_at(at_end).expect("readable").is_empty(), "{name}");
        assert_eq!(
            read_at(at_end + 1).expect_err("past the end"),
            ReqError::InvalidOffset,
            "{name}"
        );
    }
}

// -------------------------------------------------------------------------------------------
// FB18 — the MQTT export destination write (GitLab #59, §6.4)
// -------------------------------------------------------------------------------------------

#[test]
fn fb18_saves_a_destination_and_says_when_it_applies() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    export_destination::write(
        &f.ctx,
        PEER_A,
        br#"{"host":"hub-1.local","username":"fiber","password":"s3cret"}"#,
    )
    .expect("the write succeeds");

    let v = json(&export_destination::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true);
    assert!(
        v["message"].as_str().expect("string").contains("restart"),
        "there is no live reload, so the message must not imply a running export: {v}"
    );

    let requests = f.export.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, Origin::Ble);
    assert_eq!(requests[0].1.host, "hub-1.local");
    assert_eq!(requests[0].1.username, "fiber");
    assert_eq!(requests[0].1.password, "s3cret");
    assert!(
        requests[0].1.enabled,
        "a phone that sent a destination meant to turn it on"
    );
}

#[test]
fn fb18_hands_the_seams_refusal_back_verbatim_rather_than_judging_the_credentials_itself() {
    // Which credential pairs are acceptable — a password without a username would silently enrol
    // an *anonymous* export — is `ConfigApplier::apply_export_destination`'s rule, and is tested
    // in control-core against the config file it writes. FB18's own job is smaller and is what is
    // pinned here: the request reaches the seam as parsed, and the seam's refusal comes back to
    // the phone as a readable result rather than an ATT error or a swallowed success.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    f.export
        .set_outcome(control_ble::host::ExportOutcome::refused(
            "a password without a username would enrol an anonymous export",
        ));
    export_destination::write(
        &f.ctx,
        PEER_A,
        br#"{"host":"hub-1.local","password":"s3cret"}"#,
    )
    .expect("the write itself succeeds");

    let v = json(&export_destination::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false, "not an ATT error — the reason matters");
    assert_eq!(
        v["message"], "a password without a username would enrol an anonymous export",
        "the seam's words, unedited: {v}"
    );

    let requests = f.export.requests();
    assert_eq!(requests.len(), 1, "the seam is the one that decides");
    assert_eq!(
        requests[0].1.username, "",
        "and it saw the half-filled pair"
    );
    assert_eq!(requests[0].1.password, "s3cret");
}

#[test]
fn fb18_forwards_blank_credentials_instead_of_refusing_them_on_its_own_authority() {
    // Both blank is a legitimate anonymous export — the image itself ships one — so a handler that
    // refused it would be a second, divergent copy of a rule that lives one crate down. The
    // defaults are the handler's business, though: the absent keys must arrive as empty strings
    // and not as a shape the applier has to guess at.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    export_destination::write(&f.ctx, PEER_A, br#"{"host":"hub-1.local"}"#).expect("writes");
    let v = json(&export_destination::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true, "{v}");

    let requests = f.export.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1.username, "");
    assert_eq!(requests[0].1.password, "");
    assert!(requests[0].1.enabled, "absent `enabled` defaults to on");
}

#[test]
fn fb18_refuses_a_port_or_any_other_unknown_key() {
    // The port is not a parameter — 8883 over TLS is the settled contract. A phone that sends one
    // is speaking a shape this Hub does not implement and is told so, rather than having it
    // silently dropped and believing the port took effect.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for body in [
        &br#"{"host":"hub-1.local","port":1883}"#[..],
        &br#"{"host":"hub-1.local","ca":"-----BEGIN"}"#[..],
        &br#"{}"#[..],
    ] {
        assert_eq!(
            export_destination::write(&f.ctx, PEER_A, body).expect_err("refused"),
            ReqError::InvalidValueLength,
            "body {:?}",
            String::from_utf8_lossy(body)
        );
    }
    assert!(
        f.export.requests().is_empty(),
        "a body this Hub does not implement never reaches the seam"
    );
}

#[test]
fn fb18_read_before_any_write_is_a_refusal_with_a_reason() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let v = json(&export_destination::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
    assert!(!v["message"].as_str().expect("string").is_empty());
}

// -------------------------------------------------------------------------------------------
// FB1A — the portal pairing code (GitLab #61, §6.5)
// -------------------------------------------------------------------------------------------

#[test]
fn fb1a_hands_a_well_formed_code_to_the_portal() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("the write succeeds");

    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true, "{v}");
    assert_eq!(
        f.pairing.submitted(),
        vec![(Origin::Ble, "23456X".to_string())]
    );
}

#[test]
fn fb1a_normalizes_what_a_phone_keyboard_produces() {
    // Lowercase comes free from a phone keyboard and the alphabet has no lowercase to confuse it
    // with; a trailing newline from a BLE client is not a mistake worth refusing.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    pairing_code::write(&f.ctx, PEER_A, b"{\"code\":\" 23456x \\n\"}").expect("writes");
    assert_eq!(
        f.pairing.submitted(),
        vec![(Origin::Ble, "23456X".to_string())]
    );
}

#[test]
fn fb1a_refuses_the_ambiguous_characters_and_says_which_code_it_wanted() {
    // §3's warning: three different six-character codes exist and an operator meets all of them.
    // A refusal that does not name the one it wanted sends them to the wrong screen.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for code in ["2345O6", "23451X", "2345", "234567X"] {
        let body = format!(r#"{{"code":"{code}"}}"#);
        pairing_code::write(&f.ctx, PEER_A, body.as_bytes()).expect("the write succeeds");
        let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
        assert_eq!(v["success"], false, "{code}: {v}");
        let msg = v["message"].as_str().expect("string");
        assert!(
            msg.contains("display"),
            "{code}: names where to look — {msg}"
        );
    }
    assert!(
        f.pairing.submitted().is_empty(),
        "a malformed code never reaches the portal"
    );
}

#[test]
fn fb1a_is_rate_limited_so_the_code_space_cannot_be_walked() {
    // The first rate limit in this crate, and it needs to exist: the session is the limit
    // everywhere else, which is sound for a token an attacker must read off an LCD and is not for
    // a 31^6 code space an authenticated peer could walk.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for i in 0..5 {
        pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
        let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
        assert_eq!(v["success"], true, "attempt {i}: {v}");
    }
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("the write succeeds");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false, "the sixth in the window: {v}");
    assert!(
        v["message"].as_str().expect("string").contains("too many"),
        "{v}"
    );
    assert_eq!(
        f.pairing.submitted().len(),
        5,
        "only five reached the portal"
    );

    // The window is fixed, not a ban: an operator who mistyped five times can retype after it.
    f.clock.advance(61);
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true, "the window reopened: {v}");
}

#[test]
fn fb1a_rate_limit_counts_malformed_attempts_too() {
    // Otherwise the limit is free to bypass: a walker sends garbage to reset nothing and valid
    // codes only when it wants one counted.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for _ in 0..5 {
        pairing_code::write(&f.ctx, PEER_A, br#"{"code":"2345O6"}"#).expect("writes");
    }
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false, "{v}");
    assert!(f.pairing.submitted().is_empty());
}

#[test]
fn fb1a_refuses_an_unknown_key() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    assert_eq!(
        pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X","hub":"x"}"#)
            .expect_err("refused"),
        ReqError::InvalidValueLength
    );
}

#[test]
fn fb1a_read_before_any_write_is_a_refusal_with_a_reason() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
    assert!(!v["message"].as_str().expect("string").is_empty());
}

#[test]
fn fb1a_rate_limit_survives_an_unrelated_centrals_disconnect() {
    // BLE allows several centrals. Only the authenticated peer can spend FB1A attempts, so a
    // stray one connecting and dropping must not hand the peer that is walking the code space a
    // fresh window — which is what clearing the counter on *any* disconnect would do.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for _ in 0..5 {
        pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    }

    // PEER_B never authenticated; its connection going away is not PEER_A's business.
    f.ctx.on_disconnect(PEER_B);

    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("the write succeeds");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false, "still limited: {v}");
    assert_eq!(
        f.pairing.submitted().len(),
        5,
        "only the first five got through"
    );
}

#[test]
fn fb1a_rate_limit_is_keyed_to_the_session_and_a_reconnect_does_not_reopen_it() {
    // The other half, and the cheapest way the limit used to be defeated. A disconnect is not the
    // end of a session: the FB01 token is minted by the front-panel button and dies with
    // `end_phone_session` or the 300 s idle timeout, so a peer that drops, reconnects and writes
    // the *same* token is still the operator who already spent five attempts. Clearing the counter
    // on its disconnect made the limit cost one reconnect.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for _ in 0..5 {
        pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    }

    f.ctx.on_disconnect(PEER_A);
    reauthenticate(&f, PEER_A);
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("the write succeeds");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(
        v["success"], false,
        "the same session, so the same window: {v}"
    );
    assert!(
        v["message"].as_str().expect("string").contains("too many"),
        "{v}"
    );
    assert_eq!(f.pairing.submitted().len(), 5, "still only five");

    // And the reset that is real: a new session at the front panel is a new operator, who must not
    // be stuck behind the previous one's window.
    authenticate(&f, PEER_A);
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true, "a new session starts fresh: {v}");
}

#[test]
fn fb1a_rate_limit_survives_a_second_peer_authenticating_on_the_same_token_and_dropping() {
    // The variant that needed no reconnect at all: authenticate a second central with the token
    // the first one is using, drop it, and the first peer's counter was cleared for it. The count
    // belongs to the session both of them authenticated against, so neither peer's departure
    // touches it.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for _ in 0..5 {
        pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    }

    reauthenticate(&f, PEER_B);
    f.ctx.on_disconnect(PEER_B);

    reauthenticate(&f, PEER_A);
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("the write succeeds");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false, "still limited: {v}");
    assert_eq!(f.pairing.submitted().len(), 5);
}

#[test]
fn fb1a_rate_limit_is_not_reopened_by_an_fb0b_clock_write() {
    // The limit exists for exactly the peer that can write FB0B. Measured against the wall clock,
    // five attempts plus one `{"epoch": now + 61}` bought five more — and again, indefinitely,
    // since FB0B accepts any epoch from 2020 to 2100. The window runs on the monotonic source, so
    // stepping the clock buys nothing; only waiting does.
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for _ in 0..5 {
        pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    }

    let jumped = f.clock.now_unix() + 3_600;
    time::write(
        &f.ctx,
        PEER_A,
        format!(r#"{{"epoch":{jumped}}}"#).as_bytes(),
    )
    .expect("the clock write itself is legitimate");
    assert_eq!(f.clock.now_unix(), jumped, "the clock really did move");

    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("the write succeeds");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(
        v["success"], false,
        "an hour of wall clock is not a wait: {v}"
    );
    assert!(
        v["message"].as_str().expect("string").contains("too many"),
        "{v}"
    );
    assert_eq!(
        f.pairing.submitted().len(),
        5,
        "only five ever reached the portal"
    );

    // Elapsed time, which nobody can write, still reopens it.
    f.clock.advance_monotonic(61);
    pairing_code::write(&f.ctx, PEER_A, br#"{"code":"23456X"}"#).expect("writes");
    let v = json(&pairing_code::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true, "the window reopened on real time: {v}");
}

// -------------------------------------------------------------------------------------------
// FB1B — the radio mode read and write (node_registry.md §12, peripheral.md §6.6)
// -------------------------------------------------------------------------------------------

fn card_hub() -> RadioModeStatus {
    RadioModeStatus {
        mode: RadioMode::Lorawan,
        chosen: false,
        lorawan_card: true,
        allowed: vec![
            RadioMode::Off,
            RadioMode::P2p,
            RadioMode::Lorawan,
            RadioMode::Dual,
        ],
        default: RadioMode::Lorawan,
    }
}

#[test]
fn fb1b_reads_the_state_the_wizard_needs_and_no_result_before_a_write() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let v = json(&radio_mode::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(
        v,
        serde_json::json!({
            "mode": "p2p",
            "chosen": false,
            "lorawan_card": false,
            "allowed": ["off", "p2p"],
            "default": "p2p",
        }),
        "exactly the five state fields, and no success/message before a write"
    );
}

#[test]
fn fb1b_write_then_read_carries_the_result_and_the_fresh_state() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    radio_mode::write(&f.ctx, PEER_A, br#"{"mode":"off"}"#).expect("the write succeeds");
    assert_eq!(f.radio.writes(), vec![(Origin::Ble, RadioMode::Off)]);

    let v = json(&radio_mode::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], true);
    assert!(
        v["message"].as_str().expect("string").contains("off"),
        "{v}"
    );
    assert_eq!(
        v["mode"], "off",
        "the state is read fresh, after the write: {v}"
    );
    assert_eq!(v["chosen"], true);
    // Idempotent: a read retry serves the same document.
    assert_eq!(
        radio_mode::read(&f.ctx, PEER_A, MTU, 0).expect("readable"),
        radio_mode::read(&f.ctx, PEER_A, MTU, 0).expect("readable")
    );

    // Another phone sees the state, not this phone's result.
    reauthenticate(&f, PEER_B);
    let v = json(&radio_mode::read(&f.ctx, PEER_B, MTU, 0).expect("readable"));
    assert!(v.get("success").is_none(), "{v}");
    assert_eq!(v["mode"], "off");
}

#[test]
fn fb1b_a_refusal_keeps_its_reason_and_leaves_the_state_alone() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    f.radio.set_outcome(RadioModeOutcome::refused(
        "radio mode lorawan needs the RAK5146 LoRaWAN concentrator",
    ));
    radio_mode::write(&f.ctx, PEER_A, br#"{"mode":"lorawan"}"#)
        .expect("the write itself succeeds, so the reason can be read");
    let v = json(&radio_mode::read(&f.ctx, PEER_A, MTU, 0).expect("readable"));
    assert_eq!(v["success"], false);
    assert!(
        v["message"].as_str().expect("string").contains("RAK5146"),
        "{v}"
    );
    assert_eq!(v["mode"], "p2p");
    assert_eq!(v["chosen"], false);
}

#[test]
fn fb1b_refuses_an_unknown_mode_and_an_unknown_key_without_reaching_the_seam() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    for body in [
        &br#"{"mode":"both"}"#[..],
        &br#"{"mode":"LoRaWAN"}"#[..],
        &br#"{"mode":"p2p","reason":"x"}"#[..],
        &br#"{}"#[..],
        &br#"p2p"#[..],
    ] {
        assert_eq!(
            radio_mode::write(&f.ctx, PEER_A, body).expect_err("refused"),
            ReqError::InvalidValueLength,
            "body {:?}",
            String::from_utf8_lossy(body)
        );
    }
    assert!(f.radio.writes().is_empty());
}

#[test]
fn fb1b_on_a_hub_that_cannot_look_is_an_att_error() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    f.radio.make_unreadable("no config");
    assert_eq!(
        radio_mode::read(&f.ctx, PEER_A, MTU, 0).expect_err("cannot answer"),
        ReqError::Failed
    );
}

/// Every write the app sends fits one PDU at the 23-byte ATT default (20 bytes of value).
#[test]
fn fb1b_every_write_fits_the_default_mtu() {
    for mode in [
        RadioMode::Off,
        RadioMode::Lorawan,
        RadioMode::P2p,
        RadioMode::Dual,
    ] {
        let body = format!(r#"{{"mode":"{}"}}"#, mode.as_str());
        assert!(body.len() <= 20, "{body} is {} bytes", body.len());
    }
}

/// The largest state (a card, all four modes allowed) with a long refusal still arrives whole at
/// the MTU phones negotiate: the message is shortened, never a state field.
#[test]
fn fb1b_a_long_message_is_shortened_to_fit_and_the_state_is_whole() {
    let f = fixture(EyeSection::default());
    let radio = Arc::new(MemoryRadioMode::with_status(card_hub()));
    let ctx = GattCtx::new(
        Host {
            radio: radio.clone(),
            clock: f.clock.clone(),
            ..Host::in_memory()
        },
        f.session.clone(),
        Arc::new(AdapterSlot::new()),
        Arc::new(RwLock::new(CentralState::default())),
        PeripheralConfig::default(),
        f.hub_mac,
        EventBus::new(),
    );
    f.session
        .start(f.clock.as_ref(), 300, &f.hub_mac, "test-hub");
    let tok = f
        .session
        .qr_payload(&f.hub_mac, "test-hub")
        .expect("live")
        .tok;
    auth::write(&ctx, PEER_A, tok.as_bytes()).expect("authenticates");

    let long = "radio mode lorawan needs the RAK5146 LoRaWAN concentrator, and this Hub has \
                none (/dev/rak5146 with USB IDs 0483:5740 is missing; a fitted card that did \
                not enumerate needs a cold power cycle)";
    radio.set_outcome(RadioModeOutcome::refused(long));
    radio_mode::write(&ctx, PEER_A, br#"{"mode":"dual"}"#).expect("writes");

    // Plenty of room: the whole message.
    let v = json(&radio_mode::read(&ctx, PEER_A, 512, 0).expect("readable"));
    assert_eq!(v["message"], long);

    for mtu in [185u16, 247, 150] {
        let bytes = radio_mode::read(&ctx, PEER_A, mtu, 0).expect("fits once shortened");
        assert!(
            bytes.len() < mtu as usize,
            "mtu {mtu}: {} bytes",
            bytes.len()
        );
        let v = json(&bytes);
        assert_eq!(v["success"], false, "mtu {mtu}");
        let message = v["message"].as_str().expect("string");
        assert!(message.ends_with('…'), "mtu {mtu}: {message}");
        assert!(
            long.starts_with(message.trim_end_matches('…')),
            "mtu {mtu}: {message}"
        );
        assert_eq!(
            v["allowed"],
            serde_json::json!(["off", "p2p", "lorawan", "dual"])
        );
        assert_eq!(v["default"], "lorawan");
        assert_eq!(v["lorawan_card"], true);
    }

    // The state alone is 114 bytes (MTU 115); below that nothing legible fits, and the answer
    // is an ATT error rather than a fragment.
    assert_eq!(
        radio_mode::read(&ctx, PEER_A, 100, 0).expect_err("cannot fit"),
        ReqError::Failed
    );
}

#[test]
fn fb1b_honours_the_read_offset() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    radio_mode::write(&f.ctx, PEER_A, br#"{"mode":"p2p"}"#).expect("writes");
    let whole = radio_mode::read(&f.ctx, PEER_A, 512, 0).expect("readable");
    let tail = radio_mode::read(&f.ctx, PEER_A, 512, 10).expect("readable");
    assert_eq!(tail, whole[10..].to_vec());
    let at_end = u16::try_from(whole.len()).expect("fits");
    assert!(radio_mode::read(&f.ctx, PEER_A, 512, at_end)
        .expect("readable")
        .is_empty());
    assert_eq!(
        radio_mode::read(&f.ctx, PEER_A, 512, at_end + 1).expect_err("past the end"),
        ReqError::InvalidOffset
    );
}

/// The state document's size, pinned: the app team budgets its MTU on it (peripheral.md §6.6).
#[test]
fn fb1b_the_largest_state_is_114_bytes_so_it_needs_mtu_115() {
    let bytes = radio_mode::document(&card_hub(), None, 512).expect("encodes");
    assert_eq!(
        String::from_utf8(bytes.clone()).expect("utf8"),
        r#"{"mode":"lorawan","chosen":false,"lorawan_card":true,"allowed":["off","p2p","lorawan","dual"],"default":"lorawan"}"#
    );
    assert_eq!(bytes.len(), 114);
}

// ---------------------------------------------------------------------------------------------
// FB1C — the cluster join (peripheral.md §6.7)
// ---------------------------------------------------------------------------------------------

/// Poll FB1C until the join is no longer pending, the way §6.7 tells the app to.
fn fb1c_settled(f: &Fixture, peer: [u8; 6]) -> serde_json::Value {
    for _ in 0..200 {
        let v = json(&cluster_join::read(&f.ctx, peer, MTU, 0).expect("reads"));
        if v.get("pending").is_none() {
            return v;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("FB1C stayed pending");
}

#[test]
fn fb1c_reads_the_state_before_any_write() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let body = cluster_join::read(&f.ctx, PEER_A, MTU, 0).expect("reads");
    assert_eq!(
        String::from_utf8(body).unwrap(),
        r#"{"role":null,"joinable":true}"#,
        "§6.7's first example, byte for byte: role an explicit null, no server"
    );
}

#[test]
fn fb1c_joins_in_the_background_and_reports_pending_then_the_result() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let release = f.cluster.hold();
    cluster_join::write(&f.ctx, PEER_A, BUNDLE.as_bytes()).expect("the write succeeds at once");

    let pending = json(&cluster_join::read(&f.ctx, PEER_A, MTU, 0).expect("reads"));
    assert_eq!(pending["pending"], true);
    assert_eq!(pending["joinable"], true, "the state is read fresh");
    assert!(pending.get("success").is_none());
    assert_eq!(
        cluster_join::write(&f.ctx, PEER_A, BUNDLE.as_bytes()).expect_err("one at a time"),
        ReqError::InProgress
    );

    release.send(()).unwrap();
    let done = fb1c_settled(&f, PEER_A);
    assert_eq!(done["success"], true);
    assert_eq!(
        done["message"],
        "joined proximos-1a2b3c4d.local; restarting"
    );
    assert_eq!(done["role"], "client");
    assert_eq!(done["joinable"], false);
    assert_eq!(done["server"], "proximos-1a2b3c4d.local");

    let joins = f.cluster.joins();
    assert_eq!(joins.len(), 1, "the second write never reached the join");
    let (origin, bundle) = &joins[0];
    assert_eq!(*origin, Origin::Ble);
    assert_eq!(bundle.v, 1);
    assert_eq!(bundle.h, "proximos-1a2b3c4d.local");
    assert_eq!(bundle.p, "Zm9vYmFyYmF6cXV4cXV1eA");
    assert!(
        !format!("{bundle:?}").contains("Zm9vYmFy"),
        "the request's Debug never shows the password"
    );
}

#[test]
fn fb1c_carries_a_refusal_and_its_reason() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    f.cluster.set_outcome(ClusterJoinOutcome::refused(
        "the Server's CA does not match the bundle",
    ));
    cluster_join::write(&f.ctx, PEER_A, BUNDLE.as_bytes()).expect("writes");
    let done = fb1c_settled(&f, PEER_A);
    assert_eq!(done["success"], false);
    assert_eq!(done["message"], "the Server's CA does not match the bundle");
    assert_eq!(done["role"], serde_json::Value::Null);
    assert_eq!(done["joinable"], true);
}

/// The join refuses a Hub whose radio mode has no LoRaWAN (control-core's `cluster_join`, the
/// shared apply path): the refusal names the mode and reaches the app whole at the 185-byte
/// MTU phones negotiate, unshortened.
#[test]
fn fb1c_carries_a_radio_mode_refusal_whole() {
    for mode in ["off", "p2p"] {
        let f = fixture(EyeSection::default());
        authenticate(&f, PEER_A);
        let message = format!("radio mode is {mode}; set LoRaWAN (FB1B) before joining");
        f.cluster
            .set_outcome(ClusterJoinOutcome::refused(message.clone()));
        cluster_join::write(&f.ctx, PEER_A, BUNDLE.as_bytes()).expect("writes");
        let done = fb1c_settled(&f, PEER_A);
        assert_eq!(done["success"], false, "{mode}");
        assert_eq!(done["message"], message.as_str(), "{mode}: not shortened");
    }
}

/// §6.7: a missing or extra key, a wrong length, not JSON — an ATT error, nothing reaches the
/// join. A wrong `v` is not a shape error: it is the Hub's refusal, in the result.
#[test]
fn fb1c_refuses_a_malformed_bundle_at_the_att_level() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let p = "Zm9vYmFyYmF6cXV4cXV1eA";
    let fp = "4Fg1yj5x8x2R0T3y4bWcL9e6t5Qa8n2rJ7kPq0sVwXz";
    let long_host = format!("{}.local", "a".repeat(58));
    let cases = [
        "not json".to_string(),
        format!(r#"{{"v":1,"h":"x.local","p":"{p}"}}"#),
        format!(r#"{{"v":1,"h":"x.local","p":"{p}","f":"{fp}","u":"peer-x"}}"#),
        format!(r#"{{"v":1,"h":"x.local","p":"short","f":"{fp}"}}"#),
        format!(r#"{{"v":1,"h":"x.local","p":"{p}","f":"short"}}"#),
        format!(r#"{{"v":1,"h":"","p":"{p}","f":"{fp}"}}"#),
        format!(r#"{{"v":1,"h":"{long_host}","p":"{p}","f":"{fp}"}}"#),
        format!(r#"{{"v":"1","h":"x.local","p":"{p}","f":"{fp}"}}"#),
    ];
    for c in &cases {
        assert_eq!(
            cluster_join::write(&f.ctx, PEER_A, c.as_bytes()).expect_err(c),
            ReqError::InvalidValueLength,
            "{c}"
        );
    }
    assert!(f.cluster.joins().is_empty());
    let v = json(&cluster_join::read(&f.ctx, PEER_A, MTU, 0).unwrap());
    assert!(
        v.get("success").is_none() && v.get("pending").is_none(),
        "{v}"
    );

    let v2 = format!(r#"{{"v":2,"h":"x.local","p":"{p}","f":"{fp}"}}"#);
    cluster_join::write(&f.ctx, PEER_A, v2.as_bytes()).expect("the shape is fine");
    fb1c_settled(&f, PEER_A);
    assert_eq!(
        f.cluster.joins()[0].1.v,
        2,
        "the version is the Hub's to refuse"
    );
}

/// §6.7's size promises: the longest bundle fits one write at MTU 185 (182 bytes of value), the
/// state of a joined Hub fits a read, and a long result is shortened, never a state field.
#[test]
fn fb1c_fits_the_185_byte_mtu() {
    let p = "Zm9vYmFyYmF6cXV4cXV1eA";
    let fp = "4Fg1yj5x8x2R0T3y4bWcL9e6t5Qa8n2rJ7kPq0sVwXz";
    let host = format!("{}.local", "a".repeat(57));
    assert_eq!(host.len(), 63, "the longest h §6.7 allows");
    let longest = format!(r#"{{"v":1,"h":"{host}","p":"{p}","f":"{fp}"}}"#);
    assert!(longest.len() <= 182, "{} bytes", longest.len());
    assert!(
        BUNDLE.len() <= 120,
        "a .local bundle is about 115 bytes: {}",
        BUNDLE.len()
    );

    let host = format!("{}.local", "a".repeat(57));
    let status = ClusterJoinStatus {
        role: Some(HubRole::Client),
        joinable: false,
        server: Some(host.clone()),
    };
    let joined = WriteResult::ok(format!("joined {host}; restarting"));
    let body =
        cluster_join::document(&status, Some(&cluster_join::JoinSlot::Done(joined)), MTU).unwrap();
    assert!(body.len() <= 184, "{} bytes", body.len());
    let v = json(&body);
    assert_eq!(v["success"], true);
    assert_eq!(v["server"], host.as_str(), "a state field is never cut");
    assert!(v["message"].as_str().unwrap().ends_with('…'), "{v}");

    // Below the state's own size the read is an ATT error, never a fragment (§5.1).
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    assert!(cluster_join::read(&f.ctx, PEER_A, 23, 0).is_err());
}

#[test]
fn fb1c_that_cannot_answer_is_an_att_error() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    f.cluster.make_unreadable("the config cannot be read");
    assert_eq!(
        cluster_join::read(&f.ctx, PEER_A, MTU, 0).expect_err("cannot answer"),
        ReqError::Failed
    );
}

/// The result belongs to the connection that asked: another peer, or this one after a
/// disconnect, reads the state alone.
#[test]
fn fb1c_results_are_per_peer_and_die_with_the_connection() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    cluster_join::write(&f.ctx, PEER_A, BUNDLE.as_bytes()).expect("writes");
    fb1c_settled(&f, PEER_A);
    f.ctx.on_disconnect(PEER_A);
    reauthenticate(&f, PEER_B);
    let v = json(&cluster_join::read(&f.ctx, PEER_B, MTU, 0).unwrap());
    assert!(v.get("success").is_none(), "{v}");
    assert_eq!(v["role"], "client", "but the state is the Hub's");
}

/// §6.7: a join still running when the connection drops keeps its `Pending` for the address.
/// A phone that reconnects inside the join reads `pending` and a rewrite is `InProgress`, not a
/// second join; the first one's result is then read from the same address.
#[test]
fn fb1c_a_disconnect_during_a_join_keeps_it_pending_for_the_reconnect() {
    let f = fixture(EyeSection::default());
    authenticate(&f, PEER_A);
    let release = f.cluster.hold();
    cluster_join::write(&f.ctx, PEER_A, BUNDLE.as_bytes()).expect("writes");

    f.ctx.on_disconnect(PEER_A);
    reauthenticate(&f, PEER_A);
    let v = json(&cluster_join::read(&f.ctx, PEER_A, MTU, 0).unwrap());
    assert_eq!(v["pending"], true, "{v}");
    assert_eq!(
        cluster_join::write(&f.ctx, PEER_A, BUNDLE.as_bytes()).expect_err("still running"),
        ReqError::InProgress
    );

    release.send(()).unwrap();
    let done = fb1c_settled(&f, PEER_A);
    assert_eq!(done["success"], true);
    assert_eq!(
        f.cluster.joins().len(),
        1,
        "the rewrite never reached the join"
    );

    f.ctx.on_disconnect(PEER_A);
    assert!(
        f.ctx.cluster_join_results.read(&PEER_A).is_none(),
        "a finished result still dies with the connection"
    );
}

/// The sizes the §6.7 amendment states, measured: with the longest Server name (63 characters)
/// the state is 109 bytes, 124 while a join is pending, and a result 137 with its message cut to
/// nothing (140 with the ellipsis). Below each, the read is an ATT error rather than a fragment.
#[test]
fn fb1c_the_largest_reads_need_mtu_110_125_and_138() {
    let host = format!("{}.local", "a".repeat(57));
    let joined = ClusterJoinStatus {
        role: Some(HubRole::Client),
        joinable: false,
        server: Some(host.clone()),
    };
    let state = cluster_join::document(&joined, None, 512).unwrap();
    assert_eq!(state.len(), 109, "{}", String::from_utf8_lossy(&state));
    let pending =
        cluster_join::document(&joined, Some(&cluster_join::JoinSlot::Pending), 512).unwrap();
    assert_eq!(pending.len(), 124, "{}", String::from_utf8_lossy(&pending));
    assert!(String::from_utf8(pending)
        .unwrap()
        .starts_with(r#"{"pending":true,"role":"client""#));

    let done = cluster_join::JoinSlot::Done(WriteResult::ok(format!("joined {host}; restarting")));
    let at = |mtu: u16| {
        cluster_join::document(&joined, Some(&done), mtu)
            .ok()
            .filter(|b| b.len() < mtu as usize)
    };
    let v = json(&at(141).expect("a result fits at 141"));
    assert_eq!(v["message"], "…");
    assert_eq!(v["success"], true);
    assert_eq!(v["server"], host.as_str(), "a state field is never cut");
    let v = json(&at(138).expect("a result still fits at 138"));
    assert_eq!(v["message"], "", "with the message cut to nothing");
    assert_eq!(v["success"], true);
    assert!(at(137).is_none(), "below 138 a result does not fit");

    // The ordinary case: a `<hostname>.local` Server's whole success arrives at 185.
    let server = "proximos-1a2b3c4d.local";
    let status = ClusterJoinStatus {
        role: Some(HubRole::Client),
        joinable: false,
        server: Some(server.into()),
    };
    let ok = cluster_join::JoinSlot::Done(WriteResult::ok(format!("joined {server}; restarting")));
    let body = cluster_join::document(&status, Some(&ok), MTU).unwrap();
    assert_eq!(
        json(&body)["message"],
        format!("joined {server}; restarting"),
        "uncut at 185"
    );
}
