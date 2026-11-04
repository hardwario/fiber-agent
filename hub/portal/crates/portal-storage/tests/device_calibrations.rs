//! `device_calibrations` (V35, deletion columns V36) — calibration
//! certificates of Nodes and Beacons (plan/portal/calibration.cs.md §4).

use portal_storage::{
    NewDeviceCalibration, Storage, StorageError, THRESHOLD_SCOPE_EYE, THRESHOLD_SCOPE_NODE,
};
use tempfile::tempdir;

const HOST: &str = "hub-1";
const NODE: &str = "70b3d5e050011234";
const MAC: &str = "AA:BB:CC:DD:EE:01";
/// 2026-10-31T00:00:00Z — the expiry date of the fixtures' certificate.
const OCT_31: i64 = 1_793_404_800;
const DAY: i64 = 86_400;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn upload(id: &str, scope: &str, entity: &str, valid_until: i64) -> NewDeviceCalibration {
    NewDeviceCalibration {
        id: id.into(),
        hostname: HOST.into(),
        scope: scope.into(),
        entity: entity.into(),
        valid_until,
        file_name: format!("{id}.pdf"),
        file_size: 1_234,
        sha256: "ab".repeat(32),
        note: Some("lab report 42".into()),
        uploaded_by: "novak".into(),
    }
}

#[test]
fn an_upload_round_trips_and_the_repo_stamps_the_time() {
    let (_dir, storage) = open();
    storage
        .insert_device_calibration(&upload("c1", THRESHOLD_SCOPE_NODE, NODE, OCT_31))
        .expect("insert");

    let row = storage
        .get_device_calibration("c1")
        .expect("get")
        .expect("the row exists");
    assert_eq!(row.hostname, HOST);
    assert_eq!(row.scope, THRESHOLD_SCOPE_NODE);
    assert_eq!(row.entity, NODE);
    assert_eq!(row.valid_until, OCT_31);
    assert_eq!(row.file_name, "c1.pdf");
    assert_eq!(row.file_size, 1_234);
    assert_eq!(row.note.as_deref(), Some("lab report 42"));
    assert_eq!(row.uploaded_by, "novak");
    assert!(row.uploaded_at > 0, "stamped at insert");

    assert!(storage
        .get_device_calibration("nope")
        .expect("get")
        .is_none());
}

/// The newest upload is the current calibration and leads the list, even
/// when two uploads share a second — `rowid` breaks that tie.
#[test]
fn one_devices_list_is_newest_first_and_scoped_to_that_device() {
    let (_dir, storage) = open();
    for (id, days) in [("old", 0), ("mid", 30), ("new", 60)] {
        storage
            .insert_device_calibration(&upload(id, THRESHOLD_SCOPE_NODE, NODE, OCT_31 + days * DAY))
            .expect("insert");
    }
    // Same entity string under the other scope, and another Node: neither
    // belongs to this device's list.
    storage
        .insert_device_calibration(&upload("eye", THRESHOLD_SCOPE_EYE, NODE, OCT_31))
        .expect("insert");
    storage
        .insert_device_calibration(&upload(
            "other",
            THRESHOLD_SCOPE_NODE,
            "70b3d5e05001ffff",
            OCT_31,
        ))
        .expect("insert");

    let ids: Vec<String> = storage
        .list_device_calibrations(HOST, THRESHOLD_SCOPE_NODE, NODE)
        .expect("list")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, ["new", "mid", "old"]);
    assert_eq!(
        storage
            .count_device_calibrations(HOST, THRESHOLD_SCOPE_NODE, NODE)
            .expect("count"),
        3
    );
    assert!(storage
        .list_device_calibrations("hub-2", THRESHOLD_SCOPE_NODE, NODE)
        .expect("list")
        .is_empty());
}

/// "Upravit datum" moves the current certificate only; the history keeps
/// the dates its documents carry.
#[test]
fn a_date_change_moves_only_the_current_certificate() {
    let (_dir, storage) = open();
    storage
        .insert_device_calibration(&upload("old", THRESHOLD_SCOPE_EYE, MAC, OCT_31))
        .expect("insert");
    storage
        .insert_device_calibration(&upload("new", THRESHOLD_SCOPE_EYE, MAC, OCT_31 + DAY))
        .expect("insert");

    let changed = storage
        .update_current_calibration_valid_until(HOST, THRESHOLD_SCOPE_EYE, MAC, OCT_31 + 90 * DAY)
        .expect("update");
    assert_eq!(changed, Some(("new".to_string(), OCT_31 + DAY)));

    let rows = storage
        .list_device_calibrations(HOST, THRESHOLD_SCOPE_EYE, MAC)
        .expect("list");
    assert_eq!(rows[0].valid_until, OCT_31 + 90 * DAY);
    assert_eq!(rows[1].valid_until, OCT_31, "history is not rewritten");

    // A device with nothing uploaded has nothing to move.
    assert_eq!(
        storage
            .update_current_calibration_valid_until(HOST, THRESHOLD_SCOPE_NODE, NODE, OCT_31)
            .expect("update"),
        None
    );
}

/// The sweep reads one row per device — its newest — across every Hub.
#[test]
fn the_sweep_sees_exactly_the_current_certificate_of_each_device() {
    let (_dir, storage) = open();
    storage
        .insert_device_calibration(&upload("n-old", THRESHOLD_SCOPE_NODE, NODE, OCT_31))
        .expect("insert");
    storage
        .insert_device_calibration(&upload("n-new", THRESHOLD_SCOPE_NODE, NODE, OCT_31 + DAY))
        .expect("insert");
    storage
        .insert_device_calibration(&upload("b-only", THRESHOLD_SCOPE_EYE, MAC, OCT_31))
        .expect("insert");
    let mut elsewhere = upload("h2", THRESHOLD_SCOPE_NODE, NODE, OCT_31);
    elsewhere.hostname = "hub-2".into();
    storage
        .insert_device_calibration(&elsewhere)
        .expect("insert");

    let mut current: Vec<(String, String)> = storage
        .list_current_calibrations()
        .expect("list current")
        .into_iter()
        .map(|c| (c.hostname, c.id))
        .collect();
    current.sort();
    assert_eq!(
        current,
        [
            (HOST.to_string(), "b-only".to_string()),
            (HOST.to_string(), "n-new".to_string()),
            ("hub-2".to_string(), "h2".to_string()),
        ]
    );
}

/// The CHECKs refuse what a route bug could write: a `hub` scope (the Hub
/// itself has no certificate; its probes are `probe`, V38), an empty file,
/// a duplicate id.
#[test]
fn the_table_refuses_rows_no_route_should_write() {
    let (_dir, storage) = open();

    let err = storage
        .insert_device_calibration(&upload("hub", "hub", "PROXIMOS-0001", OCT_31))
        .expect_err("the Hub itself has no certificate");
    assert!(matches!(err, StorageError::Sqlite(_)), "{err:?}");

    let mut empty = upload("empty", THRESHOLD_SCOPE_NODE, NODE, OCT_31);
    empty.file_size = 0;
    storage
        .insert_device_calibration(&empty)
        .expect_err("a certificate has bytes");

    storage
        .insert_device_calibration(&upload("dup", THRESHOLD_SCOPE_NODE, NODE, OCT_31))
        .expect("first");
    storage
        .insert_device_calibration(&upload("dup", THRESHOLD_SCOPE_NODE, NODE, OCT_31))
        .expect_err("the id names a file, so it is unique");
}

/// Amendment 2026-10-13: a deleted certificate stays in the list as the
/// record of its deletion, but it is no longer current — not for the
/// sweep, not for the date edit, not for the cap — and the previous one
/// takes over. A second delete of the same row changes nothing.
#[test]
fn a_deleted_certificate_is_kept_as_a_record_but_is_never_current() {
    let (_dir, storage) = open();
    storage
        .insert_device_calibration(&upload("old", THRESHOLD_SCOPE_NODE, NODE, OCT_31))
        .expect("insert");
    storage
        .insert_device_calibration(&upload("new", THRESHOLD_SCOPE_NODE, NODE, OCT_31 + DAY))
        .expect("insert");

    assert!(storage
        .mark_device_calibration_deleted("new", "admin1", "wrong device")
        .expect("delete"));
    assert!(
        !storage
            .mark_device_calibration_deleted("new", "admin2", "again")
            .expect("delete"),
        "already deleted"
    );
    assert!(!storage
        .mark_device_calibration_deleted("nope", "admin1", "x")
        .expect("delete"));

    let rows = storage
        .list_device_calibrations(HOST, THRESHOLD_SCOPE_NODE, NODE)
        .expect("list");
    assert_eq!(rows.len(), 2, "the row stays");
    assert_eq!(rows[0].id, "new");
    assert!(rows[0].is_deleted());
    assert!(rows[0].deleted_at.is_some_and(|t| t > 0));
    assert_eq!(rows[0].deleted_by.as_deref(), Some("admin1"));
    assert_eq!(rows[0].delete_message.as_deref(), Some("wrong device"));
    assert!(!rows[1].is_deleted());

    let current: Vec<String> = storage
        .list_current_calibrations()
        .expect("current")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(
        current,
        ["old"],
        "the previous certificate is current again"
    );
    assert_eq!(
        storage
            .count_device_calibrations(HOST, THRESHOLD_SCOPE_NODE, NODE)
            .expect("count"),
        1
    );
    assert_eq!(
        storage
            .update_current_calibration_valid_until(
                HOST,
                THRESHOLD_SCOPE_NODE,
                NODE,
                OCT_31 + 9 * DAY
            )
            .expect("update"),
        Some(("old".to_string(), OCT_31)),
        "the date edit moves the current, not the deleted one"
    );

    storage
        .mark_device_calibration_deleted("old", "admin1", "lab withdrew it")
        .expect("delete");
    assert!(storage
        .list_current_calibrations()
        .expect("current")
        .is_empty());
    assert_eq!(
        storage
            .update_current_calibration_valid_until(HOST, THRESHOLD_SCOPE_NODE, NODE, OCT_31)
            .expect("update"),
        None
    );
}

/// V38 (amendment 2026-10-14): a Hub probe's certificate is `probe` + the
/// wire line, the same line on two Hubs is two devices, and the rebuilt
/// table keeps V36's deletion columns.
#[test]
fn a_hub_probe_is_its_hub_and_line() {
    let (_dir, storage) = open();
    let mut a = upload("p-a", "probe", "2", OCT_31);
    a.hostname = "hub-a".into();
    let mut b = upload("p-b", "probe", "2", OCT_31);
    b.hostname = "hub-b".into();
    storage
        .insert_device_calibration(&a)
        .expect("probe on hub-a");
    storage
        .insert_device_calibration(&b)
        .expect("probe on hub-b");

    let on_a = storage
        .list_device_calibrations("hub-a", "probe", "2")
        .expect("list");
    assert_eq!(on_a.len(), 1);
    assert_eq!(on_a[0].id, "p-a");
    assert_eq!(
        storage.list_current_calibrations().expect("current").len(),
        2,
        "one current certificate per Hub and line"
    );

    storage
        .mark_device_calibration_deleted("p-a", "admin1", "wrong probe")
        .expect("delete");
    let on_a = storage
        .list_device_calibrations("hub-a", "probe", "2")
        .expect("list");
    assert!(on_a[0].is_deleted(), "V36's columns survive the rebuild");
    assert_eq!(on_a[0].delete_message.as_deref(), Some("wrong probe"));
}
