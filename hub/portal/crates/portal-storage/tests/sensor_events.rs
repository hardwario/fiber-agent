//! `sensor_events` (V12) — the manual operator events on a probe (MB3,
//! monitoring.backend.md §6 WP3.4, spec monitoring.md §6.5).

use portal_storage::{NewSensorEvent, ProbeIdentity, Storage, StorageError};
use tempfile::tempdir;

const HOST: &str = "hub-1";

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn event(probe: ProbeIdentity, ts: i64, text: &str) -> NewSensorEvent {
    NewSensorEvent {
        hostname: HOST.into(),
        probe,
        ts,
        text: text.into(),
        author: "novak".into(),
        author_user_id: Some(7),
    }
}

#[test]
fn an_event_lands_on_exactly_one_identity_column() {
    let (_dir, storage) = open();

    // One event per probe kind. The V12 CHECK is what keeps a row naming
    // two probes (or none) out, and `ProbeIdentity` is what keeps a caller
    // from trying.
    for probe in [
        ProbeIdentity::Line(3),
        ProbeIdentity::Node("n1".into()),
        ProbeIdentity::Mac("AA:BB:CC:DD:EE:01".into()),
    ] {
        let id = storage
            .insert_sensor_event(&event(probe.clone(), 1_000, "battery replaced"))
            .expect("insert");
        let back = storage
            .get_sensor_event(id)
            .expect("get")
            .expect("the row exists");
        assert_eq!(back.probe, probe, "the identity round-trips as itself");
        assert_eq!(back.hostname, HOST);
        assert_eq!(back.author, "novak");
        assert_eq!(back.author_user_id, Some(7));
        // `created_at` is the repo's, never the caller's: a backdated `ts`
        // must not backdate the record of writing it.
        assert!(back.created_at > 0);
        assert!(back.created_at != back.ts);
    }
}

#[test]
fn text_longer_than_240_chars_is_refused_by_the_check() {
    let (_dir, storage) = open();

    let just_fits = "x".repeat(240);
    storage
        .insert_sensor_event(&event(ProbeIdentity::Line(0), 10, &just_fits))
        .expect("240 characters is the limit, not one past it");

    let too_long = "x".repeat(241);
    let err = storage
        .insert_sensor_event(&event(ProbeIdentity::Line(0), 20, &too_long))
        .expect_err("241 characters is refused");
    assert!(
        matches!(err, StorageError::Sqlite(_)),
        "the CHECK refuses it rather than the row being truncated: {err:?}"
    );

    // Empty is refused too — the route rejects it first, but the database
    // is the thing a hand-written POST cannot get past.
    storage
        .insert_sensor_event(&event(ProbeIdentity::Line(0), 30, ""))
        .expect_err("an empty note carries no information");

    // Exactly one row survived.
    let rows = storage
        .list_sensor_events(HOST, &ProbeIdentity::Line(0), 0, 1_000)
        .expect("list");
    assert_eq!(rows.len(), 1);
}

#[test]
fn listing_is_scoped_to_one_probe_and_one_window() {
    let (_dir, storage) = open();
    let line = ProbeIdentity::Line(1);

    for ts in [100, 200, 300] {
        storage
            .insert_sensor_event(&event(line.clone(), ts, &format!("at {ts}")))
            .expect("insert");
    }
    // Neighbours that must never appear in line 1's list: another line,
    // another probe kind, and the same identity under another Hub.
    storage
        .insert_sensor_event(&event(ProbeIdentity::Line(2), 200, "other line"))
        .expect("insert");
    storage
        .insert_sensor_event(&event(ProbeIdentity::Node("n1".into()), 200, "other kind"))
        .expect("insert");
    storage
        .insert_sensor_event(&NewSensorEvent {
            hostname: "hub-2".into(),
            ..event(line.clone(), 200, "other hub")
        })
        .expect("insert");

    // Half-open, like every other window in this crate.
    let rows = storage
        .list_sensor_events(HOST, &line, 100, 300)
        .expect("list");
    assert_eq!(
        rows.iter().map(|e| e.ts).collect::<Vec<_>>(),
        vec![100, 200],
        "the lower bound is included, the upper is not"
    );
    assert!(rows.iter().all(|e| e.text.starts_with("at ")));

    // Ascending, because the window's list reads as a timeline.
    let all = storage
        .list_sensor_events(HOST, &line, 0, 1_000)
        .expect("list");
    assert_eq!(
        all.iter().map(|e| e.ts).collect::<Vec<_>>(),
        vec![100, 200, 300]
    );
}

#[test]
fn deleting_an_event_reports_whether_it_existed() {
    let (_dir, storage) = open();
    let probe = ProbeIdentity::Mac("AA:BB:CC:DD:EE:01".into());
    let id = storage
        .insert_sensor_event(&event(probe.clone(), 500, "sensor maintenance"))
        .expect("insert");

    assert!(storage
        .delete_sensor_event(HOST, &probe, id)
        .expect("delete"));
    // The second delete is the audit trail's `success = false`: the route
    // records the attempt either way, and "there was no such row" is an
    // outcome, not an error.
    assert!(!storage
        .delete_sensor_event(HOST, &probe, id)
        .expect("delete again"));
    assert!(!storage
        .delete_sensor_event(HOST, &probe, 9_999)
        .expect("delete unknown"));

    assert!(storage.get_sensor_event(id).expect("get").is_none());
    assert!(storage
        .list_sensor_events(HOST, &probe, 0, 1_000)
        .expect("list")
        .is_empty());
}

/// Review item 1 (2026-09-10): an `id` is a global handle, so the delete is
/// scoped by Hub **and** probe in SQL. Line 1 exists on every Hub; without
/// the `hostname` clause a manager on Hub A's line 1 could delete Hub B's
/// note by editing the number in the form.
#[test]
fn a_delete_never_reaches_another_hubs_or_another_probes_row() {
    let (_dir, storage) = open();
    let line = ProbeIdentity::Line(1);

    let mine = storage
        .insert_sensor_event(&event(line.clone(), 100, "mine"))
        .expect("insert");
    let other_hub = storage
        .insert_sensor_event(&NewSensorEvent {
            hostname: "hub-2".into(),
            ..event(line.clone(), 100, "other hub")
        })
        .expect("insert");
    let other_probe = storage
        .insert_sensor_event(&event(ProbeIdentity::Line(2), 100, "other probe"))
        .expect("insert");

    // The id is real and the probe matches — only the Hub does not.
    assert!(!storage
        .delete_sensor_event(HOST, &line, other_hub)
        .expect("wrong hub"));
    // The id is real and the Hub matches — only the probe does not.
    assert!(!storage
        .delete_sensor_event(HOST, &line, other_probe)
        .expect("wrong probe"));

    assert!(storage.get_sensor_event(other_hub).expect("get").is_some());
    assert!(storage
        .get_sensor_event(other_probe)
        .expect("get")
        .is_some());
    assert!(storage
        .delete_sensor_event(HOST, &line, mine)
        .expect("mine"));
}
