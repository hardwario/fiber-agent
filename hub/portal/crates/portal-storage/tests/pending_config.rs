//! B4/WP4.2: `pending_config` (V13) — the Node configuration draft
//! (plan/portal/devices.backend.md §7, spec plan/portal/devices.node.md
//! §3.5).
//!
//! The cases that matter are about a draft *surviving*: an edit that
//! overwrites, an error that is kept so it can be read and resent, a
//! removal that takes the whole draft with it — and the migration itself,
//! which is the part of B4 that could break every service at once if the
//! number were wrong.

use portal_storage::{
    HeldRevert, NewPendingConfig, Storage, PENDING_KIND_CONFIG, PENDING_KIND_THRESHOLD,
    PENDING_STATES, PENDING_STATE_ERROR, PENDING_STATE_PENDING, PENDING_STATE_SENDING,
    PENDING_STATE_UNDELIVERED,
};
use tempfile::tempdir;

const HOST: &str = "hub-1";
const NODE: &str = "70b3d5e050011234";

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn config_row(key: &str, value: &str, author: &str) -> NewPendingConfig {
    NewPendingConfig {
        hostname: HOST.into(),
        node_key: NODE.into(),
        param_key: key.into(),
        kind: PENDING_KIND_CONFIG.into(),
        desired_value: Some(value.into()),
        author: author.into(),
    }
}

#[test]
fn a_second_edit_of_the_same_key_overwrites_and_reopens_the_draft() {
    let (_dir, s) = open();
    s.upsert_pending(&config_row("application.interval_report", "600", "anna"))
        .expect("first edit");
    // The first send failed, and the row is carrying the reason.
    s.set_pending_state(
        HOST,
        NODE,
        "application.interval_report",
        None,
        PENDING_STATE_ERROR,
        Some("device did not confirm within 180 s"),
        true,
    )
    .expect("mark error");

    // Any manager may overwrite another's pending value (decision
    // 2026-09-07), and the correction clears the old failure rather than
    // leaving a fresh value wearing it.
    s.upsert_pending(&config_row("application.interval_report", "1200", "boris"))
        .expect("second edit");

    let rows = s.list_pending(HOST, NODE).expect("list");
    assert_eq!(rows.len(), 1, "one desired value per key, not a queue");
    let row = &rows[0];
    assert_eq!(row.desired_value.as_deref(), Some("1200"));
    assert_eq!(row.author, "boris", "the banner names who changed it last");
    assert_eq!(row.state, PENDING_STATE_PENDING);
    assert_eq!(row.attempts, 0);
    assert!(row.last_error.is_none(), "a new value has no old error");
}

#[test]
fn an_errored_row_survives_and_carries_its_reason() {
    let (_dir, s) = open();
    s.upsert_pending(&config_row("application.interval_sample", "60", "anna"))
        .expect("edit");
    assert!(s
        .set_pending_state(
            HOST,
            NODE,
            "application.interval_sample",
            None,
            PENDING_STATE_SENDING,
            None,
            true,
        )
        .expect("to sending"));
    assert!(s
        .set_pending_state(
            HOST,
            NODE,
            "application.interval_sample",
            None,
            PENDING_STATE_ERROR,
            Some("relay refused: node offline"),
            false,
        )
        .expect("to error"));

    let row = &s.list_pending(HOST, NODE).expect("list")[0];
    assert_eq!(row.state, PENDING_STATE_ERROR);
    assert_eq!(
        row.last_error.as_deref(),
        Some("relay refused: node offline")
    );
    assert_eq!(row.attempts, 1, "attempts counts publishes, not writes");

    // A state write against a key that is not in the draft is not an error;
    // it reports that there was nothing to move.
    assert!(!s
        .set_pending_state(HOST, NODE, "nope", None, PENDING_STATE_ERROR, None, false)
        .expect("absent key"));
}

#[test]
fn deleting_a_node_drops_its_whole_draft() {
    let (_dir, s) = open();
    s.upsert_pending(&config_row("application.interval_report", "600", "anna"))
        .expect("config");
    s.upsert_pending(&NewPendingConfig {
        param_key: "threshold.temperature.critical_high".into(),
        kind: PENDING_KIND_THRESHOLD.into(),
        desired_value: Some("8.0".into()),
        ..config_row("x", "y", "anna")
    })
    .expect("threshold");
    // A second Node's draft, to prove the delete is scoped.
    s.upsert_pending(&NewPendingConfig {
        node_key: "70b3d5e05001aaaa".into(),
        ..config_row("application.interval_report", "300", "anna")
    })
    .expect("other node");

    assert_eq!(s.list_pending(HOST, NODE).expect("list").len(), 2);
    assert_eq!(
        s.delete_pending_for_node(HOST, NODE).expect("delete"),
        2,
        "both halves of the draft go together"
    );
    assert!(s.list_pending(HOST, NODE).expect("list").is_empty());
    assert_eq!(
        s.list_pending(HOST, "70b3d5e05001aaaa")
            .expect("list other")
            .len(),
        1,
        "the other Node's draft is untouched"
    );

    // Removing a draft that is not there is a no-op, not an error: the
    // Node-removal path calls this unconditionally.
    assert_eq!(s.delete_pending_for_node(HOST, NODE).expect("again"), 0);
}

#[test]
fn the_state_check_rejects_an_invented_state() {
    let (_dir, s) = open();
    s.upsert_pending(&config_row("application.interval_report", "600", "anna"))
        .expect("edit");
    let err = s.set_pending_state(
        HOST,
        NODE,
        "application.interval_report",
        None,
        "halfway",
        None,
        false,
    );
    assert!(
        err.is_err(),
        "the migration's CHECK is the authority on the state vocabulary"
    );

    // And every state the CHECK does accept is writable, so the constant
    // list the i18n layer walks cannot drift from the schema.
    for state in PENDING_STATES {
        s.set_pending_state(
            HOST,
            NODE,
            "application.interval_report",
            None,
            state,
            None,
            false,
        )
        .unwrap_or_else(|e| panic!("state {state} should be accepted: {e}"));
    }

    // Same for `kind`.
    let bad_kind = s.upsert_pending(&NewPendingConfig {
        kind: "wishful".into(),
        ..config_row("application.interval_sample", "60", "anna")
    });
    assert!(bad_kind.is_err(), "kind is CHECKed too");
}

#[test]
fn a_restart_marks_an_interrupted_send_as_error() {
    let (_dir, s) = open();
    // A Hub-side row: the Node's part of a send ends `undelivered` instead
    // (`a_restart_leaves_the_nodes_part_undelivered`). The key is only a key.
    s.upsert_pending(&NewPendingConfig {
        kind: PENDING_KIND_THRESHOLD.into(),
        ..config_row("application.interval_report", "600", "anna")
    })
    .expect("edit");
    s.upsert_pending(&config_row("application.interval_sample", "60", "anna"))
        .expect("edit");
    s.set_pending_state(
        HOST,
        NODE,
        "application.interval_report",
        None,
        PENDING_STATE_SENDING,
        None,
        true,
    )
    .expect("to sending");

    // Nothing outlives the process that was waiting for the device, so the
    // `sending` row is unrecoverable rather than merely stale.
    assert_eq!(
        s.reset_sending_to_error("portal restarted during send")
            .expect("reset"),
        1,
        "only the stranded row is touched"
    );
    let rows = s.list_pending(HOST, NODE).expect("list");
    let stranded = rows
        .iter()
        .find(|r| r.param_key == "application.interval_report")
        .expect("row");
    assert_eq!(stranded.state, PENDING_STATE_ERROR);
    assert_eq!(
        stranded.last_error.as_deref(),
        Some("portal restarted during send")
    );
    let untouched = rows
        .iter()
        .find(|r| r.param_key == "application.interval_sample")
        .expect("row");
    assert_eq!(untouched.state, PENDING_STATE_PENDING);
    assert!(untouched.last_error.is_none());
}

#[test]
fn the_pending_badge_counts_every_nodes_draft_in_one_query() {
    let (_dir, s) = open();
    s.upsert_pending(&config_row("application.interval_report", "600", "anna"))
        .expect("edit");
    s.upsert_pending(&config_row("application.interval_sample", "60", "anna"))
        .expect("edit");
    s.upsert_pending(&NewPendingConfig {
        node_key: "70b3d5e05001aaaa".into(),
        ..config_row("application.interval_report", "300", "anna")
    })
    .expect("other node");

    let counts = s.count_pending_by_node(HOST).expect("count");
    assert_eq!(counts.get(NODE), Some(&2));
    assert_eq!(counts.get("70b3d5e05001aaaa"), Some(&1));
    assert_eq!(
        counts.get("70b3d5e05001bbbb"),
        None,
        "a Node with no draft is absent, which the badge reads as zero"
    );
    assert!(s
        .count_pending_by_node("some-other-hub")
        .expect("count")
        .is_empty());
}

// ---------------------------------------------------------------------
// The migration itself
// ---------------------------------------------------------------------

/// The hazard that cost B4 its planned migration number.
///
/// `Storage::open` runs refinery's **default** `Runner`
/// (`portal-storage/src/lib.rs`), and refinery 0.8.16 defaults
/// `abort_missing: true`. `refinery_core::traits::verify_migrations` then
/// returns `Kind::MissingVersion` for any on-disk migration numbered at or
/// below the highest version already applied that is not itself in
/// `refinery_schema_history` — so dropping a file into a *hole* in the
/// sequence stops `Storage::open` for every one of the portal's eight
/// connections, on every database that has already run past the hole.
///
/// B3 landed V11 while V10 was unwritten, so every existing database has
/// history 1-9 + 11. §10 of the backend plan had claimed V10 for
/// `pending_config`; B4 takes V13 instead and V10 is burned permanently.
/// This test is the guard: it fails the moment someone adds a migration
/// file for a burned number, which is cheaper than discovering it on an
/// operator's database.
#[test]
fn migration_versions_never_backfill_a_hole() {
    /// Numbers that must never appear in `migrations/` again. V10 was
    /// skipped in the wild before anything claimed it.
    const BURNED: [u32; 1] = [10];

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    let mut versions: Vec<u32> = std::fs::read_dir(&dir)
        .expect("migrations directory")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".sql"))
        .map(|name| {
            name.trim_start_matches('V')
                .split("__")
                .next()
                .expect("V<n>__name.sql")
                .parse::<u32>()
                .unwrap_or_else(|_| panic!("migration {name} is not named V<n>__<name>.sql"))
        })
        .collect();
    versions.sort_unstable();

    for burned in BURNED {
        assert!(
            !versions.contains(&burned),
            "V{burned} is burned: a database already applied a higher version with \
             V{burned} absent, so refinery's abort_missing would fail Storage::open. \
             Take the next free number above the highest applied instead."
        );
    }

    // Exactly one hole, and it is the burned one. MB3's V12 filled the gap
    // between V11 and B4's V13, which is legal precisely because no
    // database had applied V13 when it landed (see
    // `V12__sensor_events.sql`'s header for the evidence) — but a *second*
    // hole would be a second V10 waiting to happen, so the shape is
    // asserted rather than left to the next reader to notice.
    // V39/V40 live on hynek/ui-test, which merges to main before this
    // branch. Pending, not burned: drop this exception once ui-test lands.
    const PENDING_ELSEWHERE: [u32; 2] = [39, 40];
    let holes: Vec<u32> = (1..=*versions.last().expect("at least one migration"))
        .filter(|v| !versions.contains(v) && !PENDING_ELSEWHERE.contains(v))
        .collect();
    assert_eq!(
        holes, BURNED,
        "the only gap in the migration sequence must be the burned number"
    );

    versions.dedup();
    assert_eq!(
        versions.len(),
        std::fs::read_dir(&dir)
            .expect("migrations directory")
            .filter(|e| e
                .as_ref()
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".sql"))
            .count(),
        "two migration files share a version — refinery answers RepeatedVersion"
    );
}

/// The other half of the same hazard: V12 and V13 have to apply cleanly to
/// a database that has been running since V1 and already holds rows — not
/// only to the empty file every other test in this crate opens.
///
/// **The fixture is built at V11 on purpose** (§21 review, item 9). Opening
/// a fresh file twice would run the whole set the first time and merely
/// re-verify it the second, so the migrations under test would never see a
/// schema they did not themselves create — the test would have passed no
/// matter what V13 contained. `open_at_version` stops refinery at V11, the
/// last version that existed before MB3 and B4, which is exactly the shape
/// of the databases in the field.
#[test]
fn migrations_apply_over_a_populated_database() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");

    {
        // The pre-B4 world: V1..=V11, no `sensor_events`, no
        // `pending_config`, and telemetry in the tables that did exist.
        let old = Storage::open_at_version(&path, None, 11).expect("open at V11");
        assert!(
            old.list_pending(HOST, NODE).is_err(),
            "the fixture must predate pending_config, or it is testing nothing"
        );
        old.upsert_field_threshold(
            &portal_storage::NewFieldThreshold {
                hostname: HOST.into(),
                scope: "node".into(),
                entity: NODE.into(),
                field: "temperature".into(),
                critical_low: None,
                warning_low: None,
                warning_high: Some(6.0),
                critical_high: Some(8.0),
                enabled: true,
            },
            Some("anna"),
        )
        .expect("a row from before the migration under test");
    }

    // V12 and V13 now land on that populated older file.
    let s = Storage::open(&path, None).expect("migrate an existing V11 database");
    s.upsert_pending(&config_row("application.interval_report", "600", "anna"))
        .expect("the new table works after the upgrade");
    let rows = s.list_pending(HOST, NODE).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].desired_value.as_deref(), Some("600"));

    let kept = s
        .get_field_threshold(HOST, "node", NODE, "temperature")
        .expect("read back")
        .expect("the pre-migration row survived");
    assert_eq!(kept.critical_high, Some(8.0));

    // And re-opening an up-to-date file is still a no-op that verifies
    // every applied version against its checksum.
    drop(s);
    let again = Storage::open(&path, None).expect("re-open over existing data");
    assert_eq!(again.list_pending(HOST, NODE).expect("list").len(), 1);
}

// ---- §21 review, defect 3: the walk's re-read -------------------------
//
// A send walk plans against the draft it read, then spends up to three
// minutes per unit waiting for a device. Everything it writes afterwards
// carries the value it planned with, so an edit or a Discard that lands in
// that window cannot be spoken for by the walk it interrupted.

/// The state write misses once the row holds a different value, and the
/// newer value is left exactly as the operator typed it.
#[test]
fn a_conditional_state_write_misses_a_row_that_was_edited_under_it() {
    let (_dir, s) = open();
    s.upsert_pending(&config_row("application.interval_report", "600", "anna"))
        .expect("draft");

    // The walk marks it sending against the value it planned with.
    assert!(s
        .set_pending_state(
            HOST,
            NODE,
            "application.interval_report",
            Some(Some("600")),
            PENDING_STATE_SENDING,
            None,
            true,
        )
        .expect("to sending"));

    // Boris corrects the value while the device is still being waited on.
    s.upsert_pending(&config_row("application.interval_report", "1200", "boris"))
        .expect("edit under the walk");

    // Every later write the walk makes is against "600" and finds nothing.
    assert!(!s
        .set_pending_state(
            HOST,
            NODE,
            "application.interval_report",
            Some(Some("600")),
            PENDING_STATE_ERROR,
            Some("device did not confirm within 180 s"),
            false,
        )
        .expect("stale error write"));

    let row = &s.list_pending(HOST, NODE).expect("list")[0];
    assert_eq!(row.desired_value.as_deref(), Some("1200"));
    assert_eq!(
        row.state, PENDING_STATE_PENDING,
        "the new value is waiting to be sent, not wearing the old walk's failure"
    );
    assert_eq!(row.last_error, None);
}

/// The delete is the dangerous one: a device confirming the value that was
/// *sent* must not delete the newer value nobody has sent yet.
#[test]
fn a_confirmation_deletes_only_the_value_that_was_sent() {
    let (_dir, s) = open();
    s.upsert_pending(&config_row("application.interval_report", "600", "anna"))
        .expect("draft");
    s.upsert_pending(&config_row("application.interval_report", "1200", "boris"))
        .expect("edit under the walk");

    assert!(
        !s.delete_pending(HOST, NODE, "application.interval_report", Some(Some("600")),)
            .expect("stale delete"),
        "the walk's confirmation was for a value this row no longer holds"
    );
    assert_eq!(s.list_pending(HOST, NODE).expect("list").len(), 1);

    // The same delete against the value actually stored does land.
    assert!(s
        .delete_pending(
            HOST,
            NODE,
            "application.interval_report",
            Some(Some("1200")),
        )
        .expect("matching delete"));
    assert!(s.list_pending(HOST, NODE).expect("list").is_empty());
}

/// A NULL `desired_value` ("unset this bound") is a value like any other,
/// and `IS` rather than `=` is what makes the guard see it.
#[test]
fn an_unset_bound_matches_its_own_null_and_nothing_else() {
    let (_dir, s) = open();
    s.upsert_pending(&NewPendingConfig {
        kind: PENDING_KIND_THRESHOLD.into(),
        desired_value: None,
        ..config_row("threshold.temperature.critical_low", "", "anna")
    })
    .expect("draft an unset bound");

    assert!(!s
        .set_pending_state(
            HOST,
            NODE,
            "threshold.temperature.critical_low",
            Some(Some("null")),
            PENDING_STATE_SENDING,
            None,
            true,
        )
        .expect("wrong expectation"));
    assert!(s
        .set_pending_state(
            HOST,
            NODE,
            "threshold.temperature.critical_low",
            Some(None),
            PENDING_STATE_SENDING,
            None,
            true,
        )
        .expect("NULL matches NULL"));
    assert_eq!(
        s.list_pending(HOST, NODE).expect("list")[0].state,
        PENDING_STATE_SENDING
    );
}

/// NT-4 (night test 2026-09-26): a Hub arrives under more than one spelling, and the draft is
/// found, changed and removed under any of them without doubling a row.
#[test]
fn the_draft_is_the_same_under_any_spelling_of_the_hub() {
    let (_dir, s) = open();
    let mut row = config_row("application.interval_sample", "30", "anna");
    row.hostname = "FIBER-4E970F4A".into();
    s.upsert_pending(&row).expect("draft");
    assert_eq!(
        s.list_pending("fiber-4e970f4a", NODE).expect("list").len(),
        1
    );
    assert_eq!(
        s.count_pending_by_node("fiber-4e970f4a")
            .expect("count")
            .get(NODE),
        Some(&1)
    );

    row.hostname = "fiber-4e970f4a".into();
    row.desired_value = Some("45".into());
    s.upsert_pending(&row)
        .expect("edit under the other spelling");
    let rows = s.list_pending("FIBER-4E970F4A", NODE).expect("list");
    assert_eq!(rows.len(), 1, "replaced, not doubled");
    assert_eq!(rows[0].desired_value.as_deref(), Some("45"));

    assert!(s
        .set_pending_state(
            "FIBER-4E970F4A",
            NODE,
            "application.interval_sample",
            None,
            PENDING_STATE_ERROR,
            Some("no answer"),
            false,
        )
        .expect("state"));
    assert!(s
        .delete_pending("Fiber-4e970f4a", NODE, "application.interval_sample", None)
        .expect("delete"));
    assert!(s
        .list_pending("fiber-4e970f4a", NODE)
        .expect("list")
        .is_empty());
}

// ---- V25, PF-3: the Node's part of a send, after the Hub took it ------------

fn threshold_row(key: &str, value: &str) -> NewPendingConfig {
    NewPendingConfig {
        kind: PENDING_KIND_THRESHOLD.into(),
        ..config_row(key, value, "anna")
    }
}

/// `previous_value` is what the Hub held before the first send wrote the
/// draft there. An edit and a second send keep it: both find the Hub holding
/// the draft's own value, which a Discard must not restore.
#[test]
fn the_value_before_the_send_is_written_once_and_survives_an_edit() {
    let (_dir, s) = open();
    let key = "threshold.temperature.critical_high";
    s.upsert_pending(&threshold_row(key, "30.0"))
        .expect("draft");
    assert_eq!(
        s.list_pending(HOST, NODE).expect("list")[0].previous_value,
        None
    );

    assert!(s
        .set_pending_previous(HOST, NODE, key, Some(Some("30.0")), "25.0")
        .expect("previous"));
    assert!(
        !s.set_pending_previous(HOST, NODE, key, Some(Some("99.0")), "25.0")
            .expect("re-read"),
        "the re-read guards it like every walk write"
    );
    s.set_pending_previous(HOST, NODE, key, None, "30.0")
        .expect("second send");
    s.upsert_pending(&threshold_row(key, "35.0")).expect("edit");

    let row = &s.list_pending(HOST, NODE).expect("list")[0];
    assert_eq!(row.previous_value.as_deref(), Some("25.0"));
    assert_eq!(row.desired_value.as_deref(), Some("35.0"));
    assert_eq!(
        row.state, PENDING_STATE_PENDING,
        "the edit still reopens it"
    );

    // Under the Hub's other spelling the row is renamed, not dropped, so the
    // value before the send follows it.
    s.upsert_pending(&NewPendingConfig {
        hostname: HOST.to_ascii_uppercase(),
        ..threshold_row(key, "36.0")
    })
    .expect("edit under the other spelling");
    let rows = s.list_pending(HOST, NODE).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].previous_value.as_deref(), Some("25.0"));
}

/// An edit back to what the portal holds drops a row the Hub has not taken.
/// A row it has taken takes the held value back and is `undelivered` again
/// (edited away and back, the draft used to keep the detour). While a walk
/// writes the Hub, an untaken row is kept as `pending` instead of dropped.
#[test]
fn a_reverting_edit_keeps_a_row_the_hub_already_took() {
    let (_dir, s) = open();
    let sent = "threshold.temperature.critical_high";
    let fresh = "threshold.temperature.critical_low";
    let busy = "threshold.temperature.warning_low";
    s.upsert_pending(&threshold_row(sent, "35.0"))
        .expect("an edit away from the undelivered 30.0");
    s.upsert_pending(&threshold_row(fresh, "-5.0"))
        .expect("draft");
    s.upsert_pending(&threshold_row(busy, "1.0"))
        .expect("draft");
    s.set_pending_previous(HOST, NODE, sent, None, "25.0")
        .expect("previous");

    assert_eq!(
        s.revert_pending_to_held(HOST, NODE, sent, "30.0", false)
            .expect("revert"),
        Some(HeldRevert::Undelivered)
    );
    assert_eq!(
        s.revert_pending_to_held(HOST, NODE, fresh, "0.0", false)
            .expect("revert"),
        Some(HeldRevert::Dropped)
    );
    assert_eq!(
        s.revert_pending_to_held(HOST, NODE, busy, "0.0", true)
            .expect("revert"),
        Some(HeldRevert::Kept)
    );
    assert_eq!(
        s.revert_pending_to_held(HOST, NODE, "threshold.x.enabled", "true", false)
            .expect("revert"),
        None
    );
    let rows = s.list_pending(HOST, NODE).expect("list");
    assert_eq!(rows.len(), 2);
    let back = rows.iter().find(|r| r.param_key == sent).expect("row");
    assert_eq!(back.desired_value.as_deref(), Some("30.0"));
    assert_eq!(back.state, PENDING_STATE_UNDELIVERED);
    assert_eq!(back.previous_value.as_deref(), Some("25.0"));
    let kept = rows.iter().find(|r| r.param_key == busy).expect("row");
    assert_eq!(
        (kept.desired_value.as_deref(), kept.state.as_str()),
        (Some("0.0"), PENDING_STATE_PENDING)
    );
}

/// The Hub step lands in one transaction: the revert point on the Node's rows
/// whatever they hold now, `field_thresholds` mirrored, the Hub's own rows
/// dropped where they still hold the sent value — and the answer says whether
/// the draft was still the walk's plan.
#[test]
fn a_hub_step_lands_as_one_write_and_reports_an_edit_under_it() {
    let (_dir, s) = open();
    let node_row = "threshold.temperature.critical_high";
    let hub_row = "threshold.temperature.warning_high";
    s.upsert_pending(&threshold_row(node_row, "30.0"))
        .expect("draft");
    s.upsert_pending(&threshold_row(hub_row, "22.0"))
        .expect("draft");
    let threshold = portal_storage::NewFieldThreshold {
        hostname: HOST.into(),
        scope: "node".into(),
        entity: NODE.into(),
        field: "temperature".into(),
        critical_low: Some(-10.0),
        warning_low: Some(0.0),
        warning_high: Some(22.0),
        critical_high: Some(30.0),
        enabled: true,
    };
    let band = r#"{"critical_low":-10.0,"warning_low":0.0,"warning_high":20.0,"critical_high":25.0,"enabled":true}"#;
    // Somebody retypes the Node's bound while the Hub is being written.
    s.upsert_pending(&threshold_row(node_row, "35.0"))
        .expect("edit");
    let unchanged = s
        .land_hub_threshold(
            HOST,
            NODE,
            &portal_storage::HubLanding {
                hub_rows: &[(hub_row.into(), Some("22.0".into()))],
                node_rows: &[(node_row.into(), Some("30.0".into()), "25.0".into())],
                previous_band: Some(band),
                threshold: Some(&threshold),
                actor: Some("anna"),
            },
        )
        .expect("land");
    assert!(!unchanged, "the edit is reported");
    let rows = s.list_pending(HOST, NODE).expect("list");
    assert_eq!(rows.len(), 1, "the Hub's own row went");
    assert_eq!(rows[0].desired_value.as_deref(), Some("35.0"));
    assert_eq!(rows[0].previous_value.as_deref(), Some("25.0"));
    assert_eq!(rows[0].previous_band.as_deref(), Some(band));
    let held = s
        .get_field_threshold(HOST, "node", NODE, "temperature")
        .expect("read")
        .expect("mirrored");
    assert_eq!(
        held.critical_high,
        Some(30.0),
        "the Hub holds what was sent"
    );
}

/// The portal's own re-draft (PF-3b review) writes only where no row is.
#[test]
fn a_draft_row_is_inserted_only_where_there_is_none() {
    let (_dir, s) = open();
    let key = "threshold.temperature.critical_high";
    s.upsert_pending(&threshold_row(key, "28.0"))
        .expect("operator");
    assert!(!s
        .insert_pending_if_absent(&NewPendingConfig {
            hostname: HOST.to_ascii_uppercase(),
            ..threshold_row(key, "25.0")
        })
        .expect("insert"));
    assert!(s
        .insert_pending_if_absent(&threshold_row(
            "threshold.temperature.critical_low",
            "-10.0"
        ))
        .expect("insert"));
    let rows = s.list_pending(HOST, NODE).expect("list");
    let value = |k: &str| {
        rows.iter()
            .find(|r| r.param_key == k)
            .and_then(|r| r.desired_value.clone())
    };
    assert_eq!(
        value(key).as_deref(),
        Some("28.0"),
        "the operator's value stays"
    );
    assert_eq!(
        value("threshold.temperature.critical_low").as_deref(),
        Some("-10.0")
    );
}

/// A walk dropped mid-unit ends each row as a restart would.
#[test]
fn an_interrupted_row_ends_as_a_restart_would_end_it() {
    let (_dir, s) = open();
    let node_bound = "threshold.temperature.critical_high";
    let hub_bound = "threshold.temperature.warning_high";
    s.upsert_pending(&threshold_row(node_bound, "30.0"))
        .expect("draft");
    s.upsert_pending(&threshold_row(hub_bound, "22.0"))
        .expect("draft");
    s.set_pending_previous(HOST, NODE, node_bound, None, "25.0")
        .expect("previous");
    for key in [node_bound, hub_bound] {
        s.set_pending_state(HOST, NODE, key, None, PENDING_STATE_SENDING, None, true)
            .expect("sending");
    }
    assert_eq!(
        s.list_pending_in_state(PENDING_STATE_SENDING)
            .expect("list")
            .len(),
        2
    );
    for key in [node_bound, hub_bound] {
        assert!(s
            .mark_pending_interrupted(HOST, NODE, key, None, "send interrupted")
            .expect("mark"));
    }
    let rows = s.list_pending(HOST, NODE).expect("list");
    let state = |k: &str| {
        rows.iter()
            .find(|r| r.param_key == k)
            .map(|r| r.state.clone())
    };
    assert_eq!(
        state(node_bound).as_deref(),
        Some(PENDING_STATE_UNDELIVERED)
    );
    assert_eq!(state(hub_bound).as_deref(), Some(PENDING_STATE_ERROR));
}

/// A restart that caught the walk waiting for the Node leaves the Node's
/// rows `undelivered` (a config key, or a bound the Hub already took), and
/// every other `sending` row `error` as before.
#[test]
fn a_restart_leaves_the_nodes_part_undelivered() {
    let (_dir, s) = open();
    let node_bound = "threshold.temperature.critical_high";
    let hub_bound = "threshold.temperature.warning_high";
    s.upsert_pending(&config_row("application.interval_report", "600", "anna"))
        .expect("draft");
    s.upsert_pending(&threshold_row(node_bound, "30.0"))
        .expect("draft");
    s.upsert_pending(&threshold_row(hub_bound, "22.0"))
        .expect("draft");
    s.set_pending_previous(HOST, NODE, node_bound, None, "25.0")
        .expect("previous");
    for key in ["application.interval_report", node_bound, hub_bound] {
        s.set_pending_state(HOST, NODE, key, None, PENDING_STATE_SENDING, None, true)
            .expect("to sending");
    }
    assert!(s.list_undelivered_nodes().expect("list").is_empty());

    assert_eq!(
        s.reset_sending_to_error("portal restarted").expect("reset"),
        3
    );
    let state_of = |key: &str| {
        s.list_pending(HOST, NODE)
            .expect("list")
            .into_iter()
            .find(|r| r.param_key == key)
            .map(|r| r.state)
    };
    assert_eq!(
        state_of("application.interval_report").as_deref(),
        Some(PENDING_STATE_UNDELIVERED)
    );
    assert_eq!(
        state_of(node_bound).as_deref(),
        Some(PENDING_STATE_UNDELIVERED)
    );
    assert_eq!(state_of(hub_bound).as_deref(), Some(PENDING_STATE_ERROR));
    assert_eq!(
        s.list_undelivered_nodes().expect("list"),
        vec![(HOST.to_string(), NODE.to_string())]
    );
}

/// V25 lands on a V24 database holding a draft: the rows keep their values
/// and states, and gain an empty `previous_value`.
#[test]
fn v19_keeps_the_draft_of_a_v17_database() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    {
        let old = Storage::open_at_version(&path, None, 24).expect("open at V24");
        old.upsert_pending(&config_row("application.interval_report", "600", "anna"))
            .expect("draft");
        old.set_pending_state(
            HOST,
            NODE,
            "application.interval_report",
            None,
            PENDING_STATE_ERROR,
            Some("no answer"),
            true,
        )
        .expect("to error");
    }
    let s = Storage::open(&path, None).expect("migrate to V25");
    let rows = s.list_pending(HOST, NODE).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, PENDING_STATE_ERROR);
    assert_eq!(rows[0].last_error.as_deref(), Some("no answer"));
    assert_eq!(rows[0].attempts, 1);
    assert_eq!(rows[0].previous_value, None);
    s.set_pending_state(
        HOST,
        NODE,
        "application.interval_report",
        None,
        PENDING_STATE_UNDELIVERED,
        None,
        false,
    )
    .expect("the new state fits through the new CHECK");
}

/// plan/portal/threshold_defaults.md §5: the battery's drafts go with its bands — its
/// bounds and its switch, on every Node — and a field whose name merely starts the same
/// way, or a config key, stays.
#[test]
fn a_fields_threshold_drafts_go_and_nothing_else() {
    let (_dir, s) = open();
    for row in [
        threshold_row("threshold.voltage.critical_low", "2.8"),
        threshold_row("threshold.voltage.enabled", "true"),
        threshold_row("threshold.voltage_x.critical_low", "1"),
        threshold_row("threshold.temperature.critical_low", "-10"),
        config_row("application.interval_report", "900", "anna"),
    ] {
        s.upsert_pending(&row).expect("draft");
    }
    let mut other = threshold_row("threshold.voltage.critical_high", "4.2");
    other.node_key = "70b3d5e0500199aa".into();
    s.upsert_pending(&other).expect("draft");

    assert_eq!(
        s.delete_pending_threshold_field("voltage").expect("delete"),
        3
    );
    let mut left: Vec<String> = s
        .list_pending(HOST, NODE)
        .expect("list")
        .into_iter()
        .map(|r| r.param_key)
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "application.interval_report",
            "threshold.temperature.critical_low",
            "threshold.voltage_x.critical_low",
        ]
    );
}
