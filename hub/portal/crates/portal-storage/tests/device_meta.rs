//! B0/WP0.2: `device_meta` (V9) — the per-device metadata and override row
//! every later stage of plan/portal/devices.backend.md reads.
//!
//! The cases that matter are about *absence*: a missing row, a `NULL`
//! override and an explicit `0` override are three different things, and
//! the callers branch on the difference.

use portal_storage::{
    DeviceMeta, NewDeviceMeta, Storage, THRESHOLD_SCOPE_EYE, THRESHOLD_SCOPE_HUB,
    THRESHOLD_SCOPE_NODE,
};
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn node_meta(entity: &str) -> NewDeviceMeta {
    NewDeviceMeta::new("hub-1", THRESHOLD_SCOPE_NODE, entity)
}

#[test]
fn absent_row_is_not_an_error() {
    let (_dir, s) = open();
    let got = s
        .get_device_meta("hub-1", THRESHOLD_SCOPE_NODE, "70b3d5e050011234")
        .expect("query");
    assert!(got.is_none(), "a device nobody has configured has no row");
    assert!(s
        .list_device_meta("hub-1", THRESHOLD_SCOPE_NODE)
        .expect("list")
        .is_empty());
}

#[test]
fn round_trips_every_column() {
    let (_dir, s) = open();
    let row = NewDeviceMeta {
        description: Some("cold room, top shelf".into()),
        added_by: Some("alice".into()),
        offline_k_override: Some(6),
        backfill_auto: false,
        store_interval_override: None,
        serial: None,
        ..node_meta("70b3d5e050011234")
    };
    s.upsert_device_meta(&row).expect("insert");

    let got = s
        .get_device_meta("hub-1", THRESHOLD_SCOPE_NODE, "70b3d5e050011234")
        .expect("query")
        .expect("row present");
    assert_eq!(got.description.as_deref(), Some("cold room, top shelf"));
    assert_eq!(got.added_by.as_deref(), Some("alice"));
    assert_eq!(got.offline_k_override, Some(6));
    assert!(!got.backfill_auto);
    assert_eq!(got.store_interval_override, None);
    assert!(
        !got.created_at.is_empty(),
        "created_at is stamped on insert"
    );
}

/// `NULL` = inherit the YAML default; `0` = detection disabled for this one
/// device. If these ever collapse into one representation the detector
/// silently starts alarming on a device the operator switched off, so the
/// distinction is asserted at the storage layer where it originates.
#[test]
fn null_and_zero_overrides_are_distinct() {
    let (_dir, s) = open();
    s.upsert_device_meta(&NewDeviceMeta {
        offline_k_override: None,
        ..node_meta("inherits")
    })
    .expect("insert inherit");
    s.upsert_device_meta(&NewDeviceMeta {
        offline_k_override: Some(0),
        ..node_meta("disabled")
    })
    .expect("insert disabled");

    let by_entity = |e: &str| -> DeviceMeta {
        s.get_device_meta("hub-1", THRESHOLD_SCOPE_NODE, e)
            .expect("query")
            .expect("row present")
    };
    assert_eq!(by_entity("inherits").offline_k_override, None);
    assert_eq!(by_entity("disabled").offline_k_override, Some(0));
}

/// `created_at` is the registration time the Node list sorts by, so an edit
/// must not move a device to the top of the list.
#[test]
fn upsert_updates_fields_but_never_created_at() {
    let (_dir, s) = open();
    s.upsert_device_meta(&NewDeviceMeta {
        description: Some("first".into()),
        ..node_meta("70b3d5e050011234")
    })
    .expect("insert");
    let first = s
        .get_device_meta("hub-1", THRESHOLD_SCOPE_NODE, "70b3d5e050011234")
        .expect("query")
        .expect("row");

    s.upsert_device_meta(&NewDeviceMeta {
        description: Some("second".into()),
        offline_k_override: Some(2),
        ..node_meta("70b3d5e050011234")
    })
    .expect("update");
    let second = s
        .get_device_meta("hub-1", THRESHOLD_SCOPE_NODE, "70b3d5e050011234")
        .expect("query")
        .expect("row");

    assert_eq!(second.description.as_deref(), Some("second"));
    assert_eq!(second.offline_k_override, Some(2));
    assert_eq!(second.created_at, first.created_at);
}

/// Same `entity` string under both scopes is two rows — the key is
/// (hostname, scope, entity), exactly as `field_thresholds` keys minus the
/// field.
#[test]
fn scope_and_hostname_are_part_of_the_key() {
    let (_dir, s) = open();
    for (host, scope) in [
        ("hub-1", THRESHOLD_SCOPE_NODE),
        ("hub-1", THRESHOLD_SCOPE_EYE),
        ("hub-2", THRESHOLD_SCOPE_NODE),
    ] {
        s.upsert_device_meta(&NewDeviceMeta {
            description: Some(format!("{host}/{scope}")),
            ..NewDeviceMeta::new(host, scope, "AA:BB:CC:DD:EE:FF")
        })
        .expect("insert");
    }

    assert_eq!(
        s.list_device_meta("hub-1", THRESHOLD_SCOPE_NODE)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.list_device_meta("hub-1", THRESHOLD_SCOPE_EYE)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.list_device_meta("hub-2", THRESHOLD_SCOPE_NODE)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.get_device_meta("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:FF")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("hub-1/eye")
    );
}

#[test]
fn delete_reports_whether_a_row_was_there() {
    let (_dir, s) = open();
    s.upsert_device_meta(&node_meta("70b3d5e050011234"))
        .expect("insert");
    assert!(s
        .delete_device_meta("hub-1", THRESHOLD_SCOPE_NODE, "70b3d5e050011234")
        .expect("delete"));
    assert!(!s
        .delete_device_meta("hub-1", THRESHOLD_SCOPE_NODE, "70b3d5e050011234")
        .expect("delete again"));
}

/// The migration has to apply to a database that already carries rows from
/// V1–V8, not just to a fresh file — that is the upgrade path on a device.
///
/// **The fixture stops at V8** (B6 review, item 13). Opening a fresh file
/// twice runs the whole set the first time and re-verifies it the second, so
/// V9 would never see a schema it did not itself create and the test would
/// pass whatever V9 contained. `open_at_version` builds the pre-V9 shape,
/// puts a row in a table that already existed there, and only then lets V9
/// land on it — the same fixture shape `pending_config.rs`'s
/// `migrations_apply_over_a_populated_database` uses.
#[test]
fn v9_applies_over_a_populated_database() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    {
        let old = Storage::open_at_version(&path, None, 8).expect("open at V8");
        assert!(
            old.list_device_meta("hub-1", THRESHOLD_SCOPE_NODE).is_err(),
            "the fixture must predate device_meta, or it is testing nothing"
        );
        // A row in a table that did exist at V8, so the upgrade has real
        // data underneath it rather than an empty file.
        old.upsert_field_threshold(
            &portal_storage::NewFieldThreshold {
                hostname: "hub-1".into(),
                scope: THRESHOLD_SCOPE_NODE.into(),
                entity: "70b3d5e050011234".into(),
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

    // V9 (and everything after it) now lands on that populated V8 database.
    let s = Storage::open(&path, None).expect("migrate an existing V8 database");
    s.upsert_device_meta(&node_meta("70b3d5e050011234"))
        .expect("the new table works after the upgrade");
    assert_eq!(
        s.list_device_meta("hub-1", THRESHOLD_SCOPE_NODE)
            .expect("list")
            .len(),
        1
    );
    let kept = s
        .get_field_threshold(
            "hub-1",
            THRESHOLD_SCOPE_NODE,
            "70b3d5e050011234",
            "temperature",
        )
        .expect("read back")
        .expect("the pre-migration row survived");
    assert_eq!(kept.critical_high, Some(8.0));
}

// ---- V14 (B6): the Beacon's serial number ---------------------------

/// devices.beacon.md §3.1 makes the serial the Beacon's primary identity in
/// its detail header, and the BTSMP1 advertisement carries none — so this
/// column is the only place the portal can ever hold it, and the value
/// arrives because a human read the label. `None` is the honest answer for
/// every Beacon registered before B6.
#[test]
fn a_beacons_serial_round_trips_and_is_optional() {
    let (_dir, s) = open();

    let bare = NewDeviceMeta::new("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01");
    s.upsert_device_meta(&bare)
        .expect("insert without a serial");
    assert_eq!(
        s.get_device_meta("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01")
            .expect("get")
            .expect("row")
            .serial,
        None,
        "V14 is nullable and nothing backfills it"
    );

    let with_serial = NewDeviceMeta {
        serial: Some("6845019001".into()),
        ..NewDeviceMeta::new("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:02")
    };
    s.upsert_device_meta(&with_serial).expect("insert");
    assert_eq!(
        s.get_device_meta("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:02")
            .expect("get")
            .expect("row")
            .serial
            .as_deref(),
        Some("6845019001")
    );

    // The bulk read the list and the detail page both go through.
    let listed = s
        .list_device_meta("hub-1", THRESHOLD_SCOPE_EYE)
        .expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[1].serial.as_deref(), Some("6845019001"));
}

/// V14 is an `ALTER TABLE` on a table that already holds rows, which is the
/// upgrade path on a device that has been running since B0. It must apply
/// over the top of them without touching what is there.
///
/// **Built at V13** (B6 review, item 13): the point is a `device_meta` that
/// predates the `serial` column and already has rows in it. Opening a fresh
/// file twice would create the column first and populate it afterwards,
/// which is not the upgrade path and would pass whatever V14 said.
#[test]
fn v14_applies_over_a_populated_device_meta() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    {
        // The pre-B6 world: `device_meta` exists (V9) and has no `serial`.
        let old = Storage::open_at_version(&path, None, 13).expect("open at V13");
        assert!(
            old.upsert_device_meta(&NewDeviceMeta::new(
                "hub-1",
                THRESHOLD_SCOPE_EYE,
                "AA:BB:CC:DD:EE:02"
            ))
            .is_err(),
            "the fixture must predate the serial column, or it is testing nothing"
        );
    }
    {
        // Seeded through raw SQL for that very reason: the repo's own writer
        // names `serial`, which is the column this migration adds.
        let conn = rusqlite::Connection::open(&path).expect("open the V13 file directly");
        conn.execute(
            "INSERT INTO device_meta (hostname, scope, entity, description, store_interval_override)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "hub-1",
                THRESHOLD_SCOPE_EYE,
                "AA:BB:CC:DD:EE:01",
                "cold room",
                5i64
            ],
        )
        .expect("insert into the pre-V14 table");
    }
    // V14 now lands on that populated V13 database.
    let s = Storage::open(&path, None).expect("migrate an existing V13 database");
    let row = s
        .get_device_meta("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01")
        .expect("get")
        .expect("the row survived");
    assert_eq!(row.description.as_deref(), Some("cold room"));
    assert_eq!(row.store_interval_override, Some(5));
    // The new column is NULL for every row that predates it — the honest
    // answer for a Beacon registered before B6, not an empty string.
    assert_eq!(row.serial, None);
    // And it is writable now that the migration has run.
    s.upsert_device_meta(&NewDeviceMeta {
        serial: Some("6845019001".into()),
        ..NewDeviceMeta::new("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01")
    })
    .expect("write the new column");
    assert_eq!(
        s.get_device_meta("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01")
            .expect("get")
            .expect("row")
            .serial
            .as_deref(),
        Some("6845019001")
    );
}

// ---- V16: the Hub's scope and its name column ------------------------

/// devices.hub.md §3.1 stores a Hub's operator name and description here
/// with `scope = 'hub'` and `entity =` its own hostname. V9's CHECK allowed
/// only `('node', 'eye')` — §3.1's OQ 7 landed after that migration shipped
/// — so V16 rebuilt the table to widen it. Without the rebuild this insert
/// fails with `CHECK constraint failed`, which is the whole reason V16 is
/// not a one-line `ALTER TABLE`.
#[test]
fn a_hub_names_itself_and_the_scope_is_accepted() {
    let (_dir, s) = open();

    s.upsert_device_meta(&NewDeviceMeta {
        name: Some("Warehouse B".into()),
        description: Some("cabinet by the loading dock".into()),
        ..NewDeviceMeta::new("hub-1", THRESHOLD_SCOPE_HUB, "hub-1")
    })
    .expect("scope 'hub' is allowed from V16 on");

    let row = s
        .get_device_meta("hub-1", THRESHOLD_SCOPE_HUB, "hub-1")
        .expect("get")
        .expect("row");
    assert_eq!(row.name.as_deref(), Some("Warehouse B"));
    assert_eq!(
        row.description.as_deref(),
        Some("cabinet by the loading dock")
    );

    // Blank clears the name rather than leaving the previous one behind:
    // there is no "inherit" state for a name, the reader falls back to
    // `device_label` and then to the hostname.
    s.upsert_device_meta(&NewDeviceMeta::new("hub-1", THRESHOLD_SCOPE_HUB, "hub-1"))
        .expect("upsert");
    assert_eq!(
        s.get_device_meta("hub-1", THRESHOLD_SCOPE_HUB, "hub-1")
            .expect("get")
            .expect("row")
            .name,
        None
    );

    // A scope outside the CHECK is still refused — V16 widened the set, it
    // did not drop the constraint.
    assert!(s
        .upsert_device_meta(&NewDeviceMeta::new("hub-1", "gateway", "hub-1"))
        .is_err());
}

/// V16 rebuilds the table (SQLite cannot widen a CHECK in place), so unlike
/// V14's `ALTER TABLE` it moves every existing row by hand. Built at V14 for
/// the reason `v14_applies_over_a_populated_device_meta` gives: a fresh file
/// would create the new shape first and prove nothing about the copy.
#[test]
fn v16_carries_every_existing_row_through_the_rebuild() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    {
        Storage::open_at_version(&path, None, 14).expect("open at V14");
    }
    {
        // Seeded through raw SQL, for the reason V14's test gives: the
        // repo's own writer names `name`, which is a column this migration
        // adds.
        let conn = rusqlite::Connection::open(&path).expect("open the V14 file directly");
        conn.execute(
            "INSERT INTO device_meta
                (hostname, scope, entity, description, store_interval_override, serial)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "hub-1",
                THRESHOLD_SCOPE_EYE,
                "AA:BB:CC:DD:EE:01",
                "cold room",
                5i64,
                "6845019001"
            ],
        )
        .expect("insert into the pre-V16 table");
        assert!(
            conn.execute(
                "INSERT INTO device_meta (hostname, scope, entity) VALUES (?1, ?2, ?3)",
                rusqlite::params!["hub-1", THRESHOLD_SCOPE_HUB, "hub-1"],
            )
            .is_err(),
            "the fixture must predate the widened CHECK, or it is testing nothing"
        );
    }

    let s = Storage::open(&path, None).expect("migrate an existing V14 database");
    let row = s
        .get_device_meta("hub-1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01")
        .expect("get")
        .expect("the row survived the rebuild");
    assert_eq!(row.description.as_deref(), Some("cold room"));
    assert_eq!(row.store_interval_override, Some(5));
    assert_eq!(row.serial.as_deref(), Some("6845019001"));
    // Every row that predates the column reads as unnamed, not as "".
    assert_eq!(row.name, None);
}
