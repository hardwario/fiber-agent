use portal_storage::{
    AlarmEvent, MinuteReading, NewAlarmEvent, RawReading, Storage, SOURCE_LIVE, SOURCE_REPLAY,
};
use tempfile::tempdir;

fn minute_row(minute_ts: i64, avg_c: f64, source: &str) -> MinuteReading {
    MinuteReading {
        hostname: "hub-1".into(),
        sensor_line: 3,
        minute_ts,
        min_c: Some(avg_c - 0.5),
        avg_c: Some(avg_c),
        max_c: Some(avg_c + 0.5),
        sample_count: Some(12),
        disconnect_count: Some(0),
        worst_alarm: None,
        source: source.into(),
        data_hmac: None,
    }
}

#[test]
fn open_and_migrate_on_fresh_file() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open+migrate fresh db");

    // schema is usable right after migration
    assert_eq!(
        storage
            .latest_minute("hub-1", 0)
            .expect("query latest_minute"),
        None
    );
}

#[test]
fn migrate_idempotent_on_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    {
        Storage::open(&path, None).expect("first open");
    }
    // reopening re-runs the migration runner against an already-migrated db
    Storage::open(&path, None).expect("second open should be a no-op migration");
}

#[test]
fn upsert_replaces_existing_live_row() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open");

    storage
        .upsert_minute(&minute_row(1000, 20.0, SOURCE_LIVE))
        .expect("first upsert");
    storage
        .upsert_minute(&minute_row(1000, 21.5, SOURCE_LIVE))
        .expect("second upsert replaces");

    let row = storage
        .latest_minute("hub-1", 3)
        .expect("query")
        .expect("row present");
    assert_eq!(row.avg_c, Some(21.5));
    assert_eq!(row.source, SOURCE_LIVE);
}

#[test]
fn replay_source_preserved_against_later_live_upsert() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open");

    storage
        .upsert_minute(&minute_row(2000, 19.0, SOURCE_REPLAY))
        .expect("replay upsert");
    // a later live sweep for the same minute must not clobber the replay row
    storage
        .upsert_minute(&minute_row(2000, 99.0, SOURCE_LIVE))
        .expect("live upsert (no-op by source rule)");

    let row = storage
        .latest_minute("hub-1", 3)
        .expect("query")
        .expect("row present");
    assert_eq!(row.avg_c, Some(19.0));
    assert_eq!(row.source, SOURCE_REPLAY);

    // a subsequent replay for the same minute may still update it
    storage
        .upsert_minute(&minute_row(2000, 18.0, SOURCE_REPLAY))
        .expect("replay upsert overwrites replay");
    let row = storage
        .latest_minute("hub-1", 3)
        .expect("query")
        .expect("row present");
    assert_eq!(row.avg_c, Some(18.0));
}

#[test]
fn raw_insert_is_dedup_safe() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open");

    let row = RawReading {
        hostname: "hub-1".into(),
        sensor_line: 3,
        ts: 42,
        min_c: Some(19.5),
        avg_c: Some(20.0),
        max_c: Some(20.5),
        sample_count: Some(4),
    };
    storage.insert_raw(&row).expect("first insert");
    storage
        .insert_raw(&row)
        .expect("duplicate insert is ignored");

    assert_eq!(storage.count_raw("hub-1", 3).expect("count"), 1);
}

#[test]
fn ledger_dedup_returns_false_on_repeat() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open");

    assert!(storage
        .record_export("msg-1", "probe_1m", Some(1000))
        .expect("first record"));
    assert!(!storage
        .record_export("msg-1", "probe_1m", Some(1000))
        .expect("duplicate record"));
}

#[test]
fn alarm_insert_and_list_roundtrip() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open");

    let new_event = NewAlarmEvent {
        hostname: "hub-1".into(),
        sensor_id: "line-3".into(),
        sensor_name: Some("Freezer".into()),
        sensor_location: None,
        alarm_type: "TEMP_HIGH".into(),
        severity: "CRITICAL".into(),
        from_state: Some("NORMAL".into()),
        to_state: Some("CRITICAL".into()),
        message: Some("above threshold".into()),
        value: Some(9.0),
        threshold: Some(8.0),
        triggered_at: "2026-08-18T10:00:00Z".into(),
        cleared_at: None,
        acknowledged_at: None,
        acknowledged_by_user_id: None,
        acknowledged_by_username: None,
        native_key: None,
    };

    let id = storage
        .insert_alarm_event(&new_event)
        .expect("insert alarm");
    assert!(!id.is_empty());

    let events: Vec<AlarmEvent> = storage
        .list_recent_alarms("hub-1", 10)
        .expect("list recent");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, id);
    assert_eq!(events[0].sensor_id, "line-3");
    assert_eq!(events[0].severity, "CRITICAL");
}

#[test]
fn file_is_unreadable_without_the_key() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");

    {
        let storage =
            Storage::open(&path, Some("correct-horse-battery-staple")).expect("open with key");
        storage
            .upsert_minute(&minute_row(1000, 20.0, SOURCE_LIVE))
            .expect("write a row so the file has real pages");
    }

    // Opening without the key must fail once migrations try to touch the
    // (encrypted, unreadable) schema.
    let err = Storage::open(&path, None);
    assert!(
        err.is_err(),
        "expected open without key to fail on an encrypted db"
    );
}

/// V18/V19 defaults: e-mails in Czech, webhook lifecycle events off, the
/// display zone Europe/Prague — and all three survive a round trip.
#[test]
fn notification_language_webhook_events_and_display_zone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let s = portal_storage::Storage::open(&dir.path().join("portal.db"), None).expect("open");
    let mut cfg = s.get_notify_global_config().unwrap();
    assert_eq!(
        (cfg.email_lang.as_str(), cfg.webhook_lifecycle),
        ("cs", false)
    );
    // V21: no portal address until an admin sets one.
    assert_eq!(cfg.public_url, None);
    cfg.email_lang = "en".into();
    cfg.webhook_lifecycle = true;
    cfg.public_url = Some("https://proximos-ce3d59f8.local".into());
    s.upsert_notify_global_config(&cfg).unwrap();
    let back = s.get_notify_global_config().unwrap();
    assert_eq!(
        (back.email_lang.as_str(), back.webhook_lifecycle),
        ("en", true)
    );
    assert_eq!(
        back.public_url.as_deref(),
        Some("https://proximos-ce3d59f8.local")
    );

    assert_eq!(s.display_timezone().unwrap(), "Europe/Prague");
    s.set_display_timezone("America/New_York").unwrap();
    assert_eq!(s.display_timezone().unwrap(), "America/New_York");
}

/// V20 (decisions.md #31): the default alarm recipients start empty, round
/// trip, and survive a save of the rest of the global config. A Hub's own
/// row can be removed so the Hub falls back to them.
#[test]
fn default_alarm_recipients_and_removing_a_hub_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let s = portal_storage::Storage::open(&dir.path().join("portal.db"), None).expect("open");
    assert!(s.default_alarm_recipients().unwrap().is_empty());
    s.set_default_alarm_recipients(&["ops@example.com".to_string()])
        .unwrap();
    let cfg = s.get_notify_global_config().unwrap();
    s.upsert_notify_global_config(&cfg).unwrap();
    assert_eq!(
        s.default_alarm_recipients().unwrap(),
        vec!["ops@example.com".to_string()]
    );

    let row = portal_storage::DeviceReportConfig {
        hostname: "h1".into(),
        recipients: vec!["a@b.c".into()],
        enabled: true,
        notify_on_alarm: true,
        notify_on_clear: false,
        notify_on_ack: false,
        updated_at: None,
    };
    s.upsert_device_report_config(&row).unwrap();
    s.delete_device_report_config("h1").unwrap();
    assert!(s.get_device_report_config("h1").unwrap().is_none());
    s.delete_device_report_config("h1").unwrap();
}
