//! Node enrollment — `lorawan.md` §7, §7.1.
//!
//! The headline case is the v1 defect: re-adding a Node with a new AppKey must actually
//! rotate the key. In v1 the add reported success and the device never joined again.

use control_radio::config::RadioConfig;
use control_radio::lorawan::client::ChirpStack;
use control_radio::lorawan::converge::Converger;
use control_radio::lorawan::enroll::{EnrollRequest, Enroller, NodeRef, NodesFile};
use control_radio::lorawan::fake::FakeChirpStack;
use control_radio::lorawan::keyfile;
use control_radio::lorawan::{ConvergedState, Eui, Scope, Tokens};
use control_radio::store::{Slot, Store};
use control_radio::Kind;

const PLAN_EXAMPLE: &str = include_str!("fixtures/radio-section.yaml");
const KEY_A: &str = "00112233445566778899aabbccddeeff";
const KEY_B: &str = "ffeeddccbbaa99887766554433221100";

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    fake: FakeChirpStack,
    store: Store,
    state: ConvergedState,
    _dir: TempDir,
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// A Hub that has already run `create`, which is the precondition for enrolling anything.
fn provisioned(tag: &str) -> Fixture {
    let dir =
        std::env::temp_dir().join(format!("control-radio-enroll-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir);
    let tokens = Tokens::new(store.clone());
    for s in Scope::ALL {
        tokens.store(s, "jwt").expect("seed");
    }
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();

    rt().block_on(async {
        Converger {
            client: &fake,
            config: &config,
            tokens: &tokens,
            store: &store,
        }
        .create(Some(&eui("0011223344556677")), false)
        .await
        .expect("create");
    });

    let state = ConvergedState::from_json(
        &store
            .read(Slot::LorawanConfig)
            .expect("read")
            .expect("written"),
    )
    .expect("parses");

    Fixture {
        fake,
        store,
        state,
        _dir: TempDir(dir),
    }
}

impl Fixture {
    fn enroller(&self) -> Enroller<'_> {
        Enroller {
            client: &self.fake,
            store: &self.store,
            state: &self.state,
        }
    }
    fn nodes(&self) -> NodesFile {
        NodesFile::load(&self.store).expect("loads")
    }
}

fn eui(s: &str) -> Eui {
    Eui::parse(s).expect("valid")
}

fn otaa(key_hex: &str) -> keyfile::KeyMaterial {
    keyfile::parse(&format!("{{\"app_key\":\"{key_hex}\"}}")).expect("parses")
}

fn req<'a>(serial: &'a str, dev: &str, material: keyfile::KeyMaterial) -> EnrollRequest<'a> {
    EnrollRequest {
        serial,
        dev_eui: eui(dev),
        profile: "default",
        otaa: true,
        join_eui: None,
        name: None,
        material,
    }
}

#[test]
fn an_otaa_enrollment_creates_the_device_and_places_the_key_in_both_fields() {
    let f = provisioned("otaa");
    rt().block_on(async {
        let out = f
            .enroller()
            .enroll(req("SN-1", "1111111111111111", otaa(KEY_A)))
            .await
            .expect("enrols");
        assert_eq!(out.serial_number, "SN-1");
        assert!(
            !out.rotated_keys,
            "a first enrollment creates, it does not rotate"
        );
        assert_eq!(f.fake.device_count(), 1);

        let stored = f
            .fake
            .stored_keys(&eui("1111111111111111"))
            .expect("keys stored");
        assert_eq!(
            stored.app_key, stored.nwk_key,
            "§9.2: one key into both fields"
        );

        // The local record makes --serial addressing work with no ChirpStack call.
        let nodes = f.nodes();
        assert_eq!(nodes.by_serial["SN-1"].dev_eui.as_str(), "1111111111111111");
        assert_eq!(
            NodeRef::Serial("SN-1".into())
                .resolve(&nodes)
                .expect("resolves")
                .as_str(),
            "1111111111111111"
        );
    });
}

/// The v1 defect, closed: re-adding with a new AppKey must rotate the stored key.
#[test]
fn re_adding_with_a_new_app_key_actually_rotates_it() {
    let f = provisioned("rotate");
    rt().block_on(async {
        f.enroller()
            .enroll(req("SN-1", "1111111111111111", otaa(KEY_A)))
            .await
            .expect("first enrol");
        let first = f.fake.stored_keys(&eui("1111111111111111")).expect("keys");

        let out = f
            .enroller()
            .enroll(req("SN-1", "1111111111111111", otaa(KEY_B)))
            .await
            .expect("re-enrol");

        assert!(out.rotated_keys, "the outcome must say the key was rotated");
        let second = f.fake.stored_keys(&eui("1111111111111111")).expect("keys");
        assert_ne!(
            first.app_key, second.app_key,
            "v1's defect: the old key silently survived"
        );
        assert_eq!(
            second.app_key, second.nwk_key,
            "both fields rotate together"
        );
        assert_eq!(
            f.fake.device_count(),
            1,
            "and no duplicate device is created"
        );
    });
}

/// §7.1: refused locally, before any RPC, with an error that names the scope decision.
#[test]
fn abp_enrollment_is_refused_before_any_rpc() {
    let f = provisioned("abp");
    rt().block_on(async {
        let material = keyfile::parse(&format!(
            "{{\"devaddr\":\"01020304\",\"nwkskey\":\"{KEY_A}\",\"appskey\":\"{KEY_B}\"}}"
        ))
        .expect("parses");

        f.fake.clear_calls();
        let mut r = req("SN-ABP", "2222222222222222", material);
        r.otaa = false;
        let err = f.enroller().enroll(r).await.expect_err("must be refused");

        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert!(
            err.message().contains("phase 1"),
            "must name the scope decision"
        );
        assert!(
            err.message().contains("not deprecated"),
            "and say ABP is not being dropped"
        );
        assert_eq!(
            f.fake.calls(),
            Vec::<String>::new(),
            "not one RPC may have been made"
        );
        assert_eq!(f.fake.device_count(), 0);

        // The ChirpStack side stays prepared regardless: the ABP profile exists.
        assert_eq!(f.fake.profile_count(), 2);
    });
}

/// The mismatch rule is enforced before any RPC too.
#[test]
fn otaa_with_abp_shaped_key_material_is_refused_locally() {
    let f = provisioned("mismatch");
    rt().block_on(async {
        let material = keyfile::parse(&format!(
            "{{\"devaddr\":\"01020304\",\"nwkskey\":\"{KEY_A}\",\"appskey\":\"{KEY_B}\"}}"
        ))
        .expect("parses");
        f.fake.clear_calls();
        let err = f
            .enroller()
            .enroll(req("SN-X", "3333333333333333", material))
            .await
            .expect_err("must be refused");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert_eq!(
            f.fake.calls(),
            Vec::<String>::new(),
            "refused before any RPC"
        );
    });
}

#[test]
fn an_unknown_profile_is_not_found_and_names_what_to_do() {
    let f = provisioned("profile");
    rt().block_on(async {
        let mut r = req("SN-1", "1111111111111111", otaa(KEY_A));
        r.profile = "no-such-profile";
        let err = f.enroller().enroll(r).await.expect_err("must be NotFound");
        assert_eq!(err.kind(), Kind::NotFound);
        assert!(err.message().contains("no-such-profile"));
    });
}

#[test]
fn removal_is_idempotent_and_drops_the_local_record() {
    let f = provisioned("remove");
    rt().block_on(async {
        f.enroller()
            .enroll(req("SN-1", "1111111111111111", otaa(KEY_A)))
            .await
            .expect("enrol");
        assert_eq!(f.fake.device_count(), 1);

        let gone = f
            .enroller()
            .remove(&NodeRef::Serial("SN-1".into()))
            .await
            .expect("removes");
        assert_eq!(gone.as_str(), "1111111111111111");
        assert_eq!(f.fake.device_count(), 0);
        assert!(
            f.nodes().by_serial.is_empty(),
            "the local record goes with it"
        );

        // Second removal: the record is gone, so the serial no longer resolves.
        let err = f
            .enroller()
            .remove(&NodeRef::Serial("SN-1".into()))
            .await
            .expect_err("serial no longer known");
        assert_eq!(err.kind(), Kind::NotFound);
    });
}

/// A device deleted out-of-band is not an error: removal must be re-runnable.
#[test]
fn removing_a_device_chirpstack_already_lost_is_success() {
    let f = provisioned("gone");
    rt().block_on(async {
        f.enroller()
            .enroll(req("SN-1", "1111111111111111", otaa(KEY_A)))
            .await
            .expect("enrol");
        f.fake
            .delete_device(&eui("1111111111111111"))
            .await
            .expect("out-of-band delete");

        f.enroller()
            .remove(&NodeRef::Serial("SN-1".into()))
            .await
            .expect("must still succeed and clean up locally");
        assert!(f.nodes().by_serial.is_empty());
    });
}

/// §5's asymmetric corner: a device with no enrollment record is still reachable by
/// `--dev-eui`, and a `--serial` miss names the enrollment record, not ChirpStack.
#[test]
fn dev_eui_addressing_works_without_an_enrollment_record() {
    let f = provisioned("addressing");
    let nodes = f.nodes();
    assert!(nodes.by_serial.is_empty());

    assert_eq!(
        NodeRef::DevEui(eui("4444444444444444"))
            .resolve(&nodes)
            .expect("dev_eui addresses ChirpStack directly")
            .as_str(),
        "4444444444444444"
    );

    let err = NodeRef::Serial("unknown".into())
        .resolve(&nodes)
        .expect_err("no mapping");
    assert_eq!(err.kind(), Kind::NotFound);
    assert_eq!(err.component(), control_radio::Component::EnrollmentRecord);
}

/// nodes.json holds key material, so it must be 0600 like every other secret.
#[cfg(unix)]
#[test]
fn the_enrollment_record_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let f = provisioned("mode");
    rt().block_on(async {
        f.enroller()
            .enroll(req("SN-1", "1111111111111111", otaa(KEY_A)))
            .await
            .expect("enrol");
    });
    let mode = std::fs::metadata(f.store.path(Slot::LorawanNodes))
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

/// §5: `--serial` addresses **one** node.
///
/// `nodes.json` was keyed by serial, so two serials could name one `dev_eui`, and removing one
/// used to take the other with it. The Node registry (node_registry.md) is keyed by the DevEUI,
/// so the shape cannot arise any more: a second serial written for the same DevEUI replaces the
/// first, and removing by it removes the one record.
#[test]
fn one_dev_eui_is_one_record_whatever_serial_names_it() {
    let f = provisioned("shared-eui-remove");
    let e = f.enroller();
    let shared = "0011223344556677";

    rt().block_on(async {
        e.enroll(req("s-first", shared, otaa(KEY_A)))
            .await
            .expect("first enrolls");
        let mut nodes = f.nodes();
        let mut dup = nodes.by_serial["s-first"].clone();
        dup.serial_number = "s-second".to_string();
        nodes.by_serial.remove("s-first");
        nodes.by_serial.insert("s-second".to_string(), dup);
        nodes.save(&f.store).expect("saved");

        let nodes = f.nodes();
        assert_eq!(nodes.by_serial.len(), 1, "one record per DevEUI");
        assert!(nodes.by_serial.contains_key("s-second"));

        e.remove(&NodeRef::Serial("s-second".to_string()))
            .await
            .expect("removes");
    });

    assert!(f.nodes().by_serial.is_empty(), "the record is gone");
}

/// The other direction (Nodes test, 2026-09-23): the same serial with a *different* dev_eui
/// used to overwrite the record, orphaning the first device in ChirpStack and its retained
/// `info`. Moving a serial is a removal plus a new enrollment, so the move is refused.
#[test]
fn a_serial_cannot_move_to_another_dev_eui_by_re_enrolling() {
    let f = provisioned("serial-move");
    let e = f.enroller();

    let err = rt().block_on(async {
        e.enroll(req("s-move", "0011223344556677", otaa(KEY_A)))
            .await
            .expect("first enrolls");
        e.enroll(req("s-move", "0011223344556678", otaa(KEY_B)))
            .await
            .expect_err("the serial may not move to another device")
    });
    assert_eq!(err.kind(), Kind::AlreadyExists);
    assert!(
        err.to_string().contains("0011223344556677"),
        "the refusal names the device the serial is on: {err}"
    );
    let nodes = f.nodes();
    assert_eq!(
        nodes.by_serial["s-move"].dev_eui.as_str(),
        "0011223344556677",
        "the record is untouched"
    );
}

/// …and the ambiguity should not be creatable in the first place: a second serial claiming an
/// already-enrolled `dev_eui` is refused, so `--dev-eui` addressing stays single-valued.
#[test]
fn a_second_serial_cannot_claim_an_already_enrolled_dev_eui() {
    let f = provisioned("shared-eui-enroll");
    let e = f.enroller();
    let shared = "0011223344556677";

    let err = rt().block_on(async {
        e.enroll(req("s-first", shared, otaa(KEY_A)))
            .await
            .expect("first enrolls");
        e.enroll(req("s-second", shared, otaa(KEY_B)))
            .await
            .expect_err("a second serial on the same dev_eui must be refused")
    });
    assert_eq!(err.kind(), Kind::AlreadyExists);
    assert!(
        err.to_string().contains("s-first"),
        "the refusal must name the serial already holding it: {err}"
    );

    // Re-enrolling the *same* serial is still a key rotation, not a conflict.
    rt().block_on(async {
        e.enroll(req("s-first", shared, otaa(KEY_B)))
            .await
            .expect("re-adding the same serial still rotates")
    });
}

/// D3 (#73): the node topic family dropped its `{transport}` level, so the transport has to
/// ride inside the retained `info` document instead. Both LoRaWAN producers — `node-add`'s
/// retained publish and every uplink's refresh — render this record, so asserting it here
/// covers them both.
#[test]
fn an_enrollment_record_renders_its_transport() {
    use control_radio::lorawan::enroll::{EnrollmentRecord, KeyDisclosure};

    let record = EnrollmentRecord {
        serial_number: "80e007b2".into(),
        dev_eui: Eui::parse("70b3d57ed0000abe").expect("valid"),
        profile: "default".into(),
        app_key: Some(KEY_A.into()),
        activation: "otaa".into(),
        join_eui: None,
        name: None,
    };

    let v = record.to_json(KeyDisclosure::Omitted);
    assert_eq!(
        v["transport"], "lorawan",
        "a consumer reads the carrier from the payload now, not from the topic"
    );
    assert_eq!(v["dev_eui"], "70b3d57ed0000abe");
    assert_eq!(v["serial_number"], "80e007b2");
    assert!(
        v.get("app_key").is_none(),
        "KeyDisclosure::Omitted still omits the key"
    );
}

/// node_registry.md §3 (R3): ChirpStack mirrors the registry. A device of a Node the registry
/// holds on P2P is deleted, a device the registry does not know is adopted, and a LoRaWAN Node
/// with no device gets one with its keys.
#[test]
fn chirpstack_follows_the_node_registry() {
    use control_radio::lorawan::mirror::reconcile;
    use control_radio::p2p::db::SessionsDb;
    use control_radio::p2p::session::RegisteredNode;
    use control_radio::registry::Registry;
    use control_radio::Transport;

    let f = provisioned("mirror");
    let app = f.state.application_id().expect("app").to_string();
    const MOVED: &str = "5876070000000413";
    const STRAY: &str = "5876070000009999";
    const MISSING: &str = "5876070000005722";

    rt().block_on(async {
        // Enrolled on LoRaWAN, then registered on P2P: a move, and a stale device.
        f.enroller()
            .enroll(req("80e0684d", MOVED, otaa(KEY_A)))
            .await
            .expect("enrolls");
        let mut db = SessionsDb::load(&f.store).expect("load");
        db.upsert_node(RegisteredNode {
            serial: "80e0684d".into(),
            dev_eui: MOVED.into(),
            profile: "default".into(),
            app_key: KEY_A.into(),
            name: None,
        });
        db.save(&f.store).expect("save");

        // A device someone added by hand.
        f.fake.seed_device(
            &app,
            control_radio::lorawan::model::Device {
                dev_eui: eui(STRAY),
                name: "2162169999".into(),
                device_profile_id: f
                    .state
                    .profile_id(control_radio::lorawan::state::Activation::Otaa, "default")
                    .expect("profile")
                    .to_string(),
                last_seen_at: None,
            },
        );

        // A LoRaWAN Node the registry holds and ChirpStack does not.
        let mut nodes = NodesFile::load(&f.store).expect("load");
        nodes.by_serial.insert(
            "80e007da".into(),
            control_radio::lorawan::enroll::EnrollmentRecord {
                serial_number: "80e007da".into(),
                dev_eui: eui(MISSING),
                profile: "default".into(),
                activation: "otaa".into(),
                join_eui: None,
                name: None,
                app_key: Some(KEY_B.into()),
            },
        );
        nodes.save(&f.store).expect("save");

        let report = reconcile(&f.fake, &f.store, &f.state, false)
            .await
            .expect("reconciles");
        assert_eq!(report.deleted, vec![MOVED.to_string()]);
        assert_eq!(report.adopted, vec![STRAY.to_string()]);
        assert_eq!(report.created, vec![MISSING.to_string()]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);

        assert!(f.fake.get_device(&eui(MOVED)).await.expect("get").is_none());
        assert!(f
            .fake
            .get_device(&eui(MISSING))
            .await
            .expect("get")
            .is_some());
        assert!(f.fake.stored_keys(&eui(MISSING)).is_some());

        let reg = Registry::load(&f.store).expect("load").expect("exists");
        assert_eq!(reg.get(MOVED).map(|n| n.radio), Some(Transport::P2p));
        assert_eq!(reg.get(STRAY).map(|n| n.radio), Some(Transport::Lorawan));
        assert_eq!(reg.get(STRAY).map(|n| n.serial.as_str()), Some("80e0188f"));

        let again = reconcile(&f.fake, &f.store, &f.state, false)
            .await
            .expect("reconciles");
        assert!(again.is_empty(), "a second pass changes nothing: {again:?}");
    });
}
