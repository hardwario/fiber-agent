//! The update policy: [`decide`] as a table, [`ensure_with`] end to end against the simulated
//! Northbridge of [`crate::p2p::nb_flash`], and the CLI's request round trip.

use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::p2p::modem;
use crate::p2p::nb_flash::image::tests::{manifest_for, manifest_json, test_image};
use crate::p2p::nb_flash::sim::{SimConfig, SimNb};
use crate::p2p::nb_flash::{Timeouts, Timing};

const BOOT: &str = "boot-1";

fn fast() -> Options {
    let t = Duration::from_millis(20);
    Options {
        timing: Timing {
            hdlc_rsp: t,
            reboot_settle: Duration::ZERO,
            boot: t,
            retry_pause: Duration::ZERO,
            bootloader: Timeouts {
                sync: t,
                reply: t,
                write: t,
                erase_base: t,
                erase_per_page: Duration::ZERO,
                checksum: t,
            },
        },
        ..Options::default()
    }
}

/// A scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "control-radio-nbupdate-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }

    fn store(&self) -> Store {
        Store::new(self.0.join("radio"))
    }

    /// A bundle directory holding `image` as `fw`, `sha`.
    fn bundle(&self, image: &[u8], fw: &str, sha: &str) -> PathBuf {
        let dir = self.0.join("bundle");
        std::fs::create_dir_all(&dir).expect("bundle dir");
        std::fs::write(dir.join("northbridge.bin"), image).expect("bin");
        std::fs::write(dir.join("northbridge.json"), manifest_json(image, fw, sha)).expect("json");
        dir
    }

    fn env(&self, bundle_dir: PathBuf, boot_id: &str) -> Env {
        Env {
            bundle_dir,
            boot_id: boot_id.to_string(),
            opts: fast(),
        }
    }

    fn no_bundle(&self) -> PathBuf {
        self.0.join("no-bundle")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn old() -> (Vec<u8>, Manifest) {
    let img = test_image(4096, 9);
    let m = manifest_for(&img, "0.4.0", "0a0b0c0d");
    (img, m)
}

fn new() -> (Vec<u8>, Manifest) {
    let img = test_image(6000 / 8 * 8, 3);
    let m = manifest_for(&img, "0.4.1", "1a2b3c4d");
    (img, m)
}

fn hotfix() -> (Vec<u8>, Manifest) {
    let img = test_image(3000 / 8 * 8, 5);
    let m = manifest_for(&img, "0.4.2", "5e5e5e5e");
    (img, m)
}

fn running_old(cfg: SimConfig) -> SimNb {
    let (img, m) = old();
    SimNb::running(cfg, &img, &m)
}

fn probe_of(m: &Manifest) -> ProbeInfo {
    (
        ModemInfo::default(),
        Some(AppImageId {
            image_len: m.image_len,
            crc32: m.crc32,
        }),
    )
}

async fn ensure_sim(
    sim: &mut SimNb,
    store: &Store,
    status: &NorthbridgeStatus,
    request: Option<&UpdateRequest>,
    env: &Env,
) -> Report {
    ensure_with(|| Ok(&mut *sim), store, status, request, env).await
}

/// One row of the [`decide`] table: name, probe, bundle, state, request, expected decision.
type Case<'a> = (
    &'a str,
    Option<ProbeInfo>,
    Option<&'a Manifest>,
    StateFile,
    Option<Manual<'a>>,
    Decision,
);

// --- decide ---------------------------------------------------------------------------------

#[test]
fn decide_covers_the_rules_of_section_3_4() {
    let (_, a) = old();
    let (_, b) = new();
    let fresh = StateFile::default();
    let spent = |crc32: u32, boot: &str| StateFile {
        budget: Some(BudgetKey {
            crc32,
            boot_id: boot.to_string(),
        }),
        attempts: MAX_ATTEMPTS,
        ..StateFile::default()
    };
    let old_image: ProbeInfo = (ModemInfo::default(), None);
    let force_b = Manual {
        force: true,
        image: &b,
    };
    let ask_b = Manual {
        force: false,
        image: &b,
    };

    let cases: Vec<Case> = vec![
        (
            "no bundle, no request",
            Some(probe_of(&a)),
            None,
            fresh.clone(),
            None,
            Decision::Off,
        ),
        (
            "no bundle, silent",
            None,
            None,
            fresh.clone(),
            None,
            Decision::Off,
        ),
        (
            "up to date",
            Some(probe_of(&b)),
            Some(&b),
            fresh.clone(),
            None,
            Decision::UpToDate { pinned: false },
        ),
        (
            "older running",
            Some(probe_of(&a)),
            Some(&b),
            fresh.clone(),
            None,
            Decision::Flash { nb_silent: false },
        ),
        (
            "newer running (rollback)",
            Some(probe_of(&b)),
            Some(&a),
            fresh.clone(),
            None,
            Decision::Flash { nb_silent: false },
        ),
        (
            "silent",
            None,
            Some(&b),
            fresh.clone(),
            None,
            Decision::Flash { nb_silent: true },
        ),
        (
            "180 B image",
            Some(old_image),
            Some(&b),
            fresh.clone(),
            None,
            Decision::NeedsMigration,
        ),
        (
            "180 B image, forced request",
            Some(old_image),
            Some(&a),
            fresh.clone(),
            Some(force_b),
            Decision::NeedsMigration,
        ),
        (
            "budget spent",
            Some(probe_of(&a)),
            Some(&b),
            spent(b.crc32, BOOT),
            None,
            Decision::BudgetExhausted {
                attempts: MAX_ATTEMPTS,
            },
        ),
        (
            "budget spent, silent",
            None,
            Some(&b),
            spent(b.crc32, BOOT),
            None,
            Decision::BudgetExhausted {
                attempts: MAX_ATTEMPTS,
            },
        ),
        (
            "budget spent in another boot",
            Some(probe_of(&a)),
            Some(&b),
            spent(b.crc32, "boot-0"),
            None,
            Decision::Flash { nb_silent: false },
        ),
        (
            "budget spent on another image",
            Some(probe_of(&a)),
            Some(&b),
            spent(a.crc32, BOOT),
            None,
            Decision::Flash { nb_silent: false },
        ),
        (
            "budget spent, request",
            Some(probe_of(&a)),
            Some(&b),
            spent(b.crc32, BOOT),
            Some(ask_b),
            Decision::BudgetExhausted {
                attempts: MAX_ATTEMPTS,
            },
        ),
        (
            "budget spent, forced request",
            Some(probe_of(&a)),
            Some(&b),
            spent(b.crc32, BOOT),
            Some(force_b),
            Decision::Flash { nb_silent: false },
        ),
        (
            "request, already running",
            Some(probe_of(&b)),
            Some(&a),
            fresh.clone(),
            Some(ask_b),
            Decision::UpToDate { pinned: false },
        ),
        (
            "forced request, already running",
            Some(probe_of(&b)),
            Some(&b),
            fresh.clone(),
            Some(force_b),
            Decision::Flash { nb_silent: false },
        ),
        (
            "request without a bundle",
            Some(probe_of(&a)),
            None,
            fresh.clone(),
            Some(ask_b),
            Decision::Flash { nb_silent: false },
        ),
    ];
    for (name, probe, bundled, state, request, want) in cases {
        let got = decide(probe.as_ref(), bundled, &state, BOOT, request.as_ref());
        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn a_hotfix_is_kept_until_the_bundle_changes() {
    let (_, bundled) = new();
    let (_, fix) = hotfix();
    let state = StateFile {
        pinned: Some(Pinned {
            crc32: fix.crc32,
            version: fix.version_string(),
            bundled_crc32: Some(bundled.crc32),
        }),
        ..StateFile::default()
    };
    assert_eq!(
        decide(Some(&probe_of(&fix)), Some(&bundled), &state, BOOT, None),
        Decision::UpToDate { pinned: true }
    );
    // A RAUC update brought another bundle: the hotfix goes.
    let (_, other) = old();
    assert_eq!(
        decide(Some(&probe_of(&fix)), Some(&other), &state, BOOT, None),
        Decision::Flash { nb_silent: false }
    );
}

// --- the state file -------------------------------------------------------------------------

#[test]
fn the_state_file_round_trips() {
    let s = Scratch::new("state");
    let store = s.store();
    assert_eq!(
        StateFile::load(&store).expect("absent"),
        StateFile::default()
    );
    let state = StateFile {
        state: UpdateState::Failed,
        bundled: Some("0.4.1+1a2b3c4d".into()),
        budget: Some(BudgetKey {
            crc32: 0xDEAD_BEEF,
            boot_id: BOOT.into(),
        }),
        attempts: 3,
        last_error: Some("erase: NACK".into()),
        last_attempt_at: Some("2026-10-08T12:00:00Z".into()),
        last_ok: Some(LastOk {
            version: "0.4.0+0a0b0c0d".into(),
            at: "2026-10-07T12:00:00Z".into(),
            elapsed_ms: 31_000,
        }),
        note: None,
        pinned: None,
        request_done_generation: 4,
        request_result: Some(RequestResult {
            generation: 4,
            ok: false,
            message: "no".into(),
        }),
    };
    state.save(&store).expect("save");
    assert_eq!(StateFile::load(&store).expect("load"), state);
    let raw: serde_json::Value =
        serde_json::from_str(&store.read(Slot::NbUpdateState).unwrap().unwrap()).unwrap();
    assert_eq!(raw["state"], "failed");
    assert_eq!(raw["budget"]["boot_id"], BOOT);
    // A corrupt file starts over rather than stopping the update for good.
    store.write(Slot::NbUpdateState, "{not json").unwrap();
    assert_eq!(StateFile::load(&store).expect("load"), StateFile::default());
}

// --- ensure, end to end ---------------------------------------------------------------------

#[tokio::test]
async fn an_up_to_date_northbridge_is_left_alone() {
    let s = Scratch::new("uptodate");
    let (img, m) = old();
    let env = s.env(s.bundle(&img, "0.4.0", "0a0b0c0d"), BOOT);
    let mut sim = running_old(SimConfig::default());
    let status = NorthbridgeStatus::default();
    let r = ensure_sim(&mut sim, &s.store(), &status, None, &env).await;
    assert_eq!(r.decision, Some(Decision::UpToDate { pinned: false }));
    assert!(r.ok);
    assert_eq!(
        sim.hdlc_commands,
        vec![modem::cmd::GET_INFO],
        "one probe, no flash"
    );
    assert!(sim.erased.is_empty());
    let state = StateFile::load(&s.store()).unwrap();
    assert_eq!(state.state, UpdateState::Idle);
    assert_eq!(state.bundled.as_deref(), Some(m.version_string().as_str()));
    let doc = status.document(0, true);
    assert_eq!(doc["update"]["state"], "idle");
    assert_eq!(doc["update"]["bundled"], "0.4.0+0a0b0c0d");
}

#[tokio::test]
async fn a_different_bundled_image_is_flashed() {
    let s = Scratch::new("mismatch");
    let (img, m) = new();
    let env = s.env(s.bundle(&img, "0.4.1", "1a2b3c4d"), BOOT);
    let mut sim = running_old(SimConfig::default());
    let status = NorthbridgeStatus::default();
    let r = ensure_sim(&mut sim, &s.store(), &status, None, &env).await;
    assert_eq!(r.decision, Some(Decision::Flash { nb_silent: false }));
    assert!(r.ok, "{}", r.message);
    assert_eq!(sim.region(nb_flash::image::APP_ADDR, img.len()), &img[..]);
    assert_eq!(
        sim.hdlc_commands[..2],
        [modem::cmd::GET_INFO, modem::cmd::ENTER_BOOTLOADER]
    );
    let state = StateFile::load(&s.store()).unwrap();
    assert_eq!(state.state, UpdateState::Idle);
    assert_eq!(state.attempts, 0);
    assert_eq!(state.last_error, None);
    assert_eq!(state.pinned, None);
    let ok = state.last_ok.expect("last_ok");
    assert_eq!(ok.version, m.version_string());
    assert!(state.last_attempt_at.is_some());
    // Run again: now it matches.
    let r = ensure_sim(&mut sim, &s.store(), &status, None, &env).await;
    assert_eq!(r.decision, Some(Decision::UpToDate { pinned: false }));
}

#[tokio::test]
async fn a_silent_northbridge_is_flashed_without_more_hdlc() {
    let s = Scratch::new("silent");
    let (img, _) = new();
    let env = s.env(s.bundle(&img, "0.4.1", "1a2b3c4d"), BOOT);
    let mut sim = SimNb::in_bootloader(SimConfig::default(), false);
    let r = ensure_sim(
        &mut sim,
        &s.store(),
        &NorthbridgeStatus::default(),
        None,
        &env,
    )
    .await;
    assert_eq!(r.decision, Some(Decision::Flash { nb_silent: true }));
    assert!(r.ok, "{}", r.message);
    assert_eq!(sim.region(nb_flash::image::APP_ADDR, img.len()), &img[..]);
    // The application only heard the GET_INFO after GO (the boot check).
    assert_eq!(sim.hdlc_commands, vec![modem::cmd::GET_INFO]);
}

#[tokio::test]
async fn three_failed_attempts_exhaust_the_budget_until_the_next_boot() {
    let s = Scratch::new("budget");
    let (img, m) = new();
    let bundle = s.bundle(&img, "0.4.1", "1a2b3c4d");
    let env = s.env(bundle.clone(), BOOT);
    // A bootloader that is not an STM32WL: every attempt fails at GET_ID.
    let cfg = SimConfig {
        pid: 0x0410,
        ..SimConfig::default()
    };
    let mut sim = running_old(cfg);
    let store = s.store();
    let status = NorthbridgeStatus::default();
    for attempt in 1..=MAX_ATTEMPTS {
        let r = ensure_sim(&mut sim, &store, &status, None, &env).await;
        assert!(!r.ok);
        assert!(
            matches!(r.decision, Some(Decision::Flash { .. })),
            "{attempt}"
        );
        let state = StateFile::load(&store).unwrap();
        assert_eq!(state.attempts, attempt);
        assert_eq!(
            state.budget,
            Some(BudgetKey {
                crc32: m.crc32,
                boot_id: BOOT.into()
            })
        );
        let want = if attempt == MAX_ATTEMPTS {
            UpdateState::Failed
        } else {
            UpdateState::Idle
        };
        assert_eq!(state.state, want, "{attempt}");
        assert!(state.last_error.unwrap().contains("GET_ID"));
    }
    assert_eq!(status.document(0, true)["update"]["state"], "failed");
    assert_eq!(status.document(0, true)["update"]["attempts"], MAX_ATTEMPTS);

    // The fourth start of the task: probe only, no flash.
    let before = sim.erased.len();
    let sent = sim.host_bytes;
    let r = ensure_sim(&mut sim, &store, &status, None, &env).await;
    assert_eq!(
        r.decision,
        Some(Decision::BudgetExhausted {
            attempts: MAX_ATTEMPTS
        })
    );
    assert_eq!(sim.erased.len(), before);
    assert!(
        sim.host_bytes - sent < 64,
        "only the probe went out: {}",
        sim.host_bytes - sent
    );
    assert_eq!(StateFile::load(&store).unwrap().state, UpdateState::Failed);

    // The Hub rebooted: a fresh budget.
    let env2 = s.env(bundle, "boot-2");
    let r = ensure_sim(&mut sim, &store, &status, None, &env2).await;
    assert!(matches!(r.decision, Some(Decision::Flash { .. })));
    assert_eq!(StateFile::load(&store).unwrap().attempts, 1);
}

#[tokio::test]
async fn without_a_bundle_the_update_is_off_and_the_port_stays_closed() {
    let s = Scratch::new("nobundle");
    let env = s.env(s.no_bundle(), BOOT);
    let status = NorthbridgeStatus::default();
    let r = ensure_with(
        || -> Result<&mut SimNb> { panic!("the port must not be opened") },
        &s.store(),
        &status,
        None,
        &env,
    )
    .await;
    assert_eq!(r.decision, Some(Decision::Off));
    assert!(r.ok);
    assert_eq!(status.document(0, true)["update"], serde_json::Value::Null);
    assert!(!s.store().exists(Slot::NbUpdateState), "nothing written");

    // An empty package directory is the same.
    std::fs::create_dir_all(s.no_bundle()).unwrap();
    let r = ensure_with(
        || -> Result<&mut SimNb> { panic!("the port must not be opened") },
        &s.store(),
        &status,
        None,
        &env,
    )
    .await;
    assert_eq!(r.decision, Some(Decision::Off));
}

#[tokio::test]
async fn an_old_180_byte_image_needs_the_migration_and_costs_no_attempt() {
    let s = Scratch::new("migration");
    let (img, _) = new();
    let env = s.env(s.bundle(&img, "0.4.1", "1a2b3c4d"), BOOT);
    let mut sim = running_old(SimConfig {
        info_len: 180,
        ..SimConfig::default()
    });
    let status = NorthbridgeStatus::default();
    for _ in 0..2 {
        let r = ensure_sim(&mut sim, &s.store(), &status, None, &env).await;
        assert_eq!(r.decision, Some(Decision::NeedsMigration));
        assert!(r.message.contains("§4"), "{}", r.message);
    }
    assert_eq!(sim.hdlc_commands, vec![modem::cmd::GET_INFO; 2]);
    let state = StateFile::load(&s.store()).unwrap();
    assert_eq!(state.attempts, 0);
    assert_eq!(state.state, UpdateState::Idle, "a note, not an alarm");
    assert!(state.note.unwrap().contains("J-Link migration"));
    let doc = status.document(0, true);
    assert_eq!(doc["update"]["state"], "idle");
    assert!(doc["update"]["note"].as_str().unwrap().contains("J-Link"));
}

// --- manual requests ------------------------------------------------------------------------

#[tokio::test]
async fn a_request_round_trips_through_the_state_file() {
    let s = Scratch::new("request");
    let store = s.store();
    let (img, _) = new();
    let env = s.env(s.bundle(&img, "0.4.1", "1a2b3c4d"), BOOT);
    assert_eq!(pending_request(&store, 0), None);

    let req = submit_request(&store, false, None).expect("submit");
    assert_eq!(
        (req.generation, req.source, req.force),
        (1, Source::Bundled, false)
    );
    assert_eq!(pending_request(&store, 0).as_ref(), Some(&req));
    assert_eq!(
        pending_request(&store, 1),
        None,
        "already handled in-process"
    );

    let mut sim = running_old(SimConfig::default());
    let status = NorthbridgeStatus::default();
    let r = ensure_sim(&mut sim, &store, &status, Some(&req), &env).await;
    assert!(r.ok, "{}", r.message);
    assert_eq!(pending_request(&store, 0), None, "answered");
    let res = wait_result(
        &store,
        1,
        Duration::from_millis(50),
        Duration::from_millis(5),
    )
    .await
    .expect("answered");
    assert!(res.ok);
    assert!(res.message.contains("0.4.1+1a2b3c4d"), "{}", res.message);

    // The same image again, without --force: done, nothing flashed.
    let erased = sim.erased.len();
    let req2 = submit_request(&store, false, None).expect("submit");
    assert_eq!(req2.generation, 2);
    let r = ensure_sim(&mut sim, &store, &status, Some(&req2), &env).await;
    assert_eq!(r.decision, Some(Decision::UpToDate { pinned: false }));
    assert!(r.ok);
    assert_eq!(sim.erased.len(), erased);

    // With --force it is flashed again.
    let req3 = submit_request(&store, true, None).expect("submit");
    let r = ensure_sim(&mut sim, &store, &status, Some(&req3), &env).await;
    assert_eq!(r.decision, Some(Decision::Flash { nb_silent: false }));
    assert!(r.ok, "{}", r.message);
    assert!(sim.erased.len() > erased);
    let state = StateFile::load(&store).unwrap();
    assert_eq!(state.request_done_generation, 3);
}

#[tokio::test]
async fn an_unanswered_request_times_out() {
    let s = Scratch::new("timeout");
    let store = s.store();
    let req = submit_request(&store, false, None).expect("submit");
    let e = wait_result(
        &store,
        req.generation,
        Duration::from_millis(30),
        Duration::from_millis(5),
    )
    .await
    .expect_err("nobody answers");
    assert_eq!(e.kind(), Kind::Timeout);
    assert_eq!(e.exit_code(), 21);
}

#[tokio::test]
async fn a_hotfix_is_flashed_from_the_store_and_kept_over_the_bundle() {
    let s = Scratch::new("hotfix");
    let store = s.store();
    let (img, bundled) = new();
    let env = s.env(s.bundle(&img, "0.4.1", "1a2b3c4d"), BOOT);
    let (fix_img, fix) = hotfix();
    let fix_json = manifest_json(&fix_img, "0.4.2", "5e5e5e5e");
    let req = submit_request(&store, false, Some((&fix_img, fix_json.as_bytes()))).expect("submit");
    assert_eq!(req.source, Source::File);
    assert_eq!(
        store.read_bytes(Slot::NbRequestImage).unwrap().as_deref(),
        Some(&fix_img[..])
    );

    let mut sim = running_old(SimConfig::default());
    let status = NorthbridgeStatus::default();
    let r = ensure_sim(&mut sim, &store, &status, Some(&req), &env).await;
    assert!(r.ok, "{}", r.message);
    assert_eq!(
        sim.region(nb_flash::image::APP_ADDR, fix_img.len()),
        &fix_img[..]
    );
    let state = StateFile::load(&store).unwrap();
    assert_eq!(
        state.pinned,
        Some(Pinned {
            crc32: fix.crc32,
            version: fix.version_string(),
            bundled_crc32: Some(bundled.crc32),
        })
    );

    // The next start of the task leaves the hotfix in place.
    let r = ensure_sim(&mut sim, &store, &status, None, &env).await;
    assert_eq!(r.decision, Some(Decision::UpToDate { pinned: true }));
    assert!(status.document(0, true)["update"]["note"]
        .as_str()
        .unwrap()
        .contains("0.4.2+5e5e5e5e"));

    // A request for the bundled image takes it back and drops the pin.
    let req = submit_request(&store, false, None).expect("submit");
    let r = ensure_sim(&mut sim, &store, &status, Some(&req), &env).await;
    assert!(r.ok, "{}", r.message);
    assert_eq!(StateFile::load(&store).unwrap().pinned, None);
}

#[tokio::test]
async fn a_request_for_a_bundle_the_image_does_not_carry_fails_cleanly() {
    let s = Scratch::new("nobundle-request");
    let store = s.store();
    let env = s.env(s.no_bundle(), BOOT);
    let req = submit_request(&store, false, None).expect("submit");
    let r = ensure_with(
        || -> Result<&mut SimNb> { panic!("the port must not be opened") },
        &store,
        &NorthbridgeStatus::default(),
        Some(&req),
        &env,
    )
    .await;
    assert!(!r.ok);
    let res = StateFile::load(&store).unwrap().request_result.unwrap();
    assert_eq!(res.generation, 1);
    assert!(!res.ok);
    assert!(res.message.contains("--file"), "{}", res.message);
}

// --- the CLI arm ----------------------------------------------------------------------------

fn alive() -> serde_json::Value {
    serde_json::json!({ "state": "alive" })
}

fn cli_request(wait_ms: Option<u64>) -> CliRequest {
    CliRequest {
        hotfix: None,
        force: false,
        wait: wait_ms.map(Duration::from_millis),
    }
}

#[tokio::test]
async fn the_cli_waits_for_the_task_to_answer() {
    let s = Scratch::new("cli-ok");
    let store = s.store();
    let (img, _) = new();
    let bundle = s.bundle(&img, "0.4.1", "1a2b3c4d");
    let env = s.env(bundle.clone(), BOOT);

    // The P2P task: picks the request up and answers it, as `run_once` does.
    let task_store = store.clone();
    let task = tokio::spawn(async move {
        let request = wait_for_request(&task_store, 0).await;
        let mut sim = running_old(SimConfig::default());
        let status = NorthbridgeStatus::default();
        ensure_sim(&mut sim, &task_store, &status, Some(&request), &env).await
    });

    let out = request_update(
        &store,
        &cli_request(Some(10_000)),
        &bundle,
        Some(&alive()),
        Duration::from_millis(10),
    )
    .await
    .expect("answered");
    assert_eq!(out.request.generation, 1);
    assert_eq!(out.request.source, Source::Bundled);
    assert_eq!(out.image, "0.4.1+1a2b3c4d");
    let r = out.result.expect("waited");
    assert!(r.ok);
    assert!(task.await.unwrap().ok);
}

#[tokio::test]
async fn the_cli_reports_a_failed_update_as_a_transport_error() {
    let s = Scratch::new("cli-fail");
    let store = s.store();
    let (img, _) = new();
    let bundle = s.bundle(&img, "0.4.1", "1a2b3c4d");
    let env = s.env(bundle.clone(), BOOT);
    let task_store = store.clone();
    tokio::spawn(async move {
        let request = wait_for_request(&task_store, 0).await;
        let mut sim = running_old(SimConfig {
            pid: 0x0410,
            ..SimConfig::default()
        });
        let status = NorthbridgeStatus::default();
        ensure_sim(&mut sim, &task_store, &status, Some(&request), &env).await
    });
    let e = request_update(
        &store,
        &cli_request(Some(10_000)),
        &bundle,
        Some(&alive()),
        Duration::from_millis(10),
    )
    .await
    .expect_err("the flash fails");
    assert_eq!(e.kind(), Kind::Transport);
    assert_eq!(e.exit_code(), 22);
    assert!(e.message().contains("GET_ID"), "{}", e.message());
}

#[tokio::test]
async fn the_cli_refuses_what_no_task_could_answer() {
    let s = Scratch::new("cli-refuse");
    let store = s.store();
    let (img, _) = new();
    let bundle = s.bundle(&img, "0.4.1", "1a2b3c4d");
    let poll = Duration::from_millis(5);
    let req = cli_request(Some(50));

    let e = request_update(&store, &req, &bundle, None, poll)
        .await
        .expect_err("no task");
    assert_eq!((e.kind(), e.exit_code()), (Kind::DaemonDown, 23));
    let disabled = serde_json::json!({ "state": "disabled" });
    let e = request_update(&store, &req, &bundle, Some(&disabled), poll)
        .await
        .expect_err("P2P off");
    assert_eq!((e.kind(), e.exit_code()), (Kind::P2pUnavailable, 32));
    // #158: the idle Northbridge task answers on a Hub without P2P.
    let idle = serde_json::json!({ "state": "disabled", "nb_task": "idle" });
    let no_wait = cli_request(None);
    request_update(&store, &no_wait, &bundle, Some(&idle), poll)
        .await
        .expect("the idle task takes the request");
    assert!(store.exists(Slot::NbUpdateRequest));
    std::fs::remove_file(store.path(Slot::NbUpdateRequest)).unwrap();
    let e = request_update(&store, &req, &s.no_bundle(), Some(&alive()), poll)
        .await
        .expect_err("nothing to flash");
    assert_eq!(e.kind(), Kind::InvalidArgument);
    assert!(e.message().contains("--file"), "{}", e.message());
    assert!(
        !store.exists(Slot::NbUpdateRequest),
        "refused before writing"
    );

    // A hotfix whose manifest does not describe it.
    let (fix_img, _) = hotfix();
    let bin = s.0.join("fix.bin");
    let json = s.0.join("fix.json");
    std::fs::write(&bin, &fix_img).unwrap();
    std::fs::write(&json, manifest_json(&img, "0.4.2", "5e5e5e5e")).unwrap();
    let bad = CliRequest {
        hotfix: Some((bin.clone(), json.clone())),
        ..req.clone()
    };
    let e = request_update(&store, &bad, &bundle, Some(&alive()), poll)
        .await
        .expect_err("mismatched hotfix");
    assert_eq!((e.kind(), e.exit_code()), (Kind::InvalidArgument, 2));
    assert!(
        !store.exists(Slot::NbUpdateRequest),
        "refused before writing"
    );

    // Nobody answers in time.
    let e = request_update(&store, &req, &bundle, Some(&alive()), poll)
        .await
        .expect_err("timeout");
    assert_eq!((e.kind(), e.exit_code()), (Kind::Timeout, 21));
}

#[tokio::test]
async fn the_cli_without_waiting_returns_the_written_request() {
    let s = Scratch::new("cli-nowait");
    let store = s.store();
    let (img, _) = new();
    let bundle = s.bundle(&img, "0.4.1", "1a2b3c4d");
    let (fix_img, _) = hotfix();
    let bin = s.0.join("fix.bin");
    let json = s.0.join("fix.json");
    std::fs::write(&bin, &fix_img).unwrap();
    std::fs::write(&json, manifest_json(&fix_img, "0.4.2", "5e5e5e5e")).unwrap();
    let req = CliRequest {
        hotfix: Some((bin, json)),
        force: true,
        wait: None,
    };
    let out = request_update(
        &store,
        &req,
        &bundle,
        Some(&alive()),
        Duration::from_millis(5),
    )
    .await
    .expect("written");
    assert_eq!(out.result, None);
    assert_eq!(out.image, "0.4.2+5e5e5e5e");
    assert_eq!(
        (out.request.source, out.request.force),
        (Source::File, true)
    );
    assert_eq!(pending_request(&store, 0), Some(out.request));
    assert_eq!(
        store.read_bytes(Slot::NbRequestImage).unwrap().as_deref(),
        Some(&fix_img[..])
    );
}
