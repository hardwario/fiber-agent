//! M6-B: alarm lifecycle, notes, notification configs, delivery history.

use portal_storage::{
    AlarmListFilter, AlarmSeverityChange, AlarmSort, DeviceReportConfig, NewAlarmEvent,
    NewDeviceMeta, NewReportHistory, ReportHistoryFilter, Storage, StorageError,
    HISTORY_SOURCE_REPORT, HISTORY_SOURCE_SYSTEM_ALERT, HISTORY_STATUS_FAILED, HISTORY_STATUS_SENT,
};
use tempfile::tempdir;

fn open_storage() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");
    (dir, storage)
}

fn raise(hostname: &str, sensor_id: &str, severity: &str, triggered_at: &str) -> NewAlarmEvent {
    NewAlarmEvent {
        hostname: hostname.into(),
        sensor_id: sensor_id.into(),
        sensor_name: Some(format!("sensor {sensor_id}")),
        sensor_location: Some("cold room".into()),
        alarm_type: "HIGH".into(),
        severity: severity.into(),
        from_state: Some("NORMAL".into()),
        to_state: Some(severity.to_uppercase()),
        message: Some("above threshold".into()),
        value: Some(9.5),
        threshold: Some(8.0),
        triggered_at: triggered_at.into(),
        cleared_at: None,
        acknowledged_at: None,
        acknowledged_by_user_id: None,
        acknowledged_by_username: None,
        native_key: None,
    }
}

#[test]
fn active_alarm_is_per_sensor_and_ignores_cleared() {
    let (_dir, storage) = open_storage();

    let id_a = storage
        .insert_alarm_event(&raise("hub-1", "line-1", "warning", "2026-08-18 10:00:00"))
        .expect("insert line-1");
    let id_b = storage
        .insert_alarm_event(&raise("hub-1", "line-2", "critical", "2026-08-18 10:01:00"))
        .expect("insert line-2");

    let active = storage
        .active_alarm("hub-1", "line-1")
        .expect("lookup")
        .expect("line-1 active");
    assert_eq!(active.id, id_a);
    assert_eq!(active.sensor_location.as_deref(), Some("cold room"));

    // clearing line-1 leaves line-2 untouched
    assert!(storage
        .clear_alarm(&id_a, "2026-08-18 10:05:00", None)
        .expect("clear"));
    assert!(storage
        .active_alarm("hub-1", "line-1")
        .expect("lookup")
        .is_none());
    assert_eq!(
        storage
            .active_alarm("hub-1", "line-2")
            .expect("lookup")
            .expect("line-2 still active")
            .id,
        id_b
    );

    let all_active = storage.active_alarms("hub-1").expect("active_alarms");
    assert_eq!(all_active.len(), 1);
    assert_eq!(all_active[0].id, id_b);
    assert_eq!(storage.count_active(Some("hub-1")).expect("count"), 1);
    assert_eq!(storage.count_active(Some("hub-2")).expect("count"), 0);
}

#[test]
fn severity_update_in_place() {
    let (_dir, storage) = open_storage();
    let id = storage
        .insert_alarm_event(&raise("hub-1", "line-1", "warning", "2026-08-18 10:00:00"))
        .expect("insert");

    assert!(storage
        .update_alarm_severity(
            &id,
            AlarmSeverityChange {
                severity: "critical",
                alarm_type: "HIGH",
                to_state: Some("CRITICAL"),
                value: Some(12.3),
                threshold: Some(12.0),
                message: Some("escalated"),
                clear_acknowledgement: false,
            }
        )
        .expect("update severity"));

    let row = storage.get_alarm(&id).expect("get").expect("row");
    assert_eq!(row.severity, "critical");
    assert_eq!(row.alarm_type, "HIGH");
    assert_eq!(row.value, Some(12.3));
    assert_eq!(row.message.as_deref(), Some("escalated"));
    // the crossed bound follows the new severity, and the previous to_state
    // shifts into from_state like update_alarm_state does
    assert_eq!(row.threshold, Some(12.0));
    assert_eq!(row.to_state.as_deref(), Some("CRITICAL"));
    assert_eq!(row.from_state.as_deref(), Some("WARNING"));
    // no new row was created
    let (_, total) = storage
        .list_alarms(&AlarmListFilter::default())
        .expect("list");
    assert_eq!(total, 1);

    assert!(!storage
        .update_alarm_severity(
            "no-such-id",
            AlarmSeverityChange {
                severity: "critical",
                alarm_type: "HIGH",
                to_state: Some("CRITICAL"),
                value: None,
                threshold: None,
                message: None,
                clear_acknowledgement: false,
            }
        )
        .expect("update missing"));
}

/// decisions.md #24: falling back to warning moves `severity` but not
/// `max_severity`, and the list's severity filter and the metric row read
/// the worst level, so a critical excursion stays findable as critical.
#[test]
fn max_severity_keeps_the_worst_level_reached() {
    let (_dir, storage) = open_storage();
    let id = storage
        .insert_alarm_event(&raise("hub-1", "line-1", "warning", "2026-08-18 10:00:00"))
        .expect("insert");
    assert_eq!(
        storage.get_alarm(&id).unwrap().unwrap().max_severity,
        "warning"
    );

    let change = |severity| AlarmSeverityChange {
        severity,
        alarm_type: "HIGH",
        to_state: Some(if severity == "critical" {
            "CRITICAL"
        } else {
            "WARNING"
        }),
        value: Some(10.0),
        threshold: Some(8.0),
        message: None,
        clear_acknowledgement: false,
    };
    storage
        .update_alarm_severity(&id, change("critical"))
        .unwrap();
    storage
        .update_alarm_severity(&id, change("warning"))
        .unwrap();

    let row = storage.get_alarm(&id).unwrap().unwrap();
    assert_eq!(row.severity, "warning", "where it sits now");
    assert_eq!(row.max_severity, "critical", "the worst it got");

    let filter = AlarmListFilter {
        severity: Some("critical".into()),
        ..AlarmListFilter::default()
    };
    assert_eq!(storage.list_alarms(&filter).unwrap().1, 1);
    let counts = storage.count_summary_by_hub(Some("hub-1")).unwrap()[0].1;
    assert_eq!((counts.critical, counts.warning), (1, 0));
}

/// decisions.md #28c: an escalation that clears the acknowledgement leaves
/// the row unacknowledged, back in the unacknowledged list, and keeps the
/// acknowledgement's note: notes are history.
#[test]
fn an_escalation_can_clear_the_acknowledgement_and_keeps_the_note() {
    let (_dir, storage) = open_storage();
    let id = storage
        .insert_alarm_event(&raise("hub-1", "line-1", "warning", "2026-08-18 10:00:00"))
        .expect("insert");
    storage
        .acknowledge_alarm(&id, Some(7), "mgr1", "2026-08-18 10:01:00")
        .expect("ack");
    storage
        .insert_alarm_note(&id, Some(7), "mgr1", "checked, door was open")
        .expect("note");

    storage
        .update_alarm_severity(
            &id,
            AlarmSeverityChange {
                severity: "critical",
                alarm_type: "HIGH",
                to_state: Some("CRITICAL"),
                value: Some(12.3),
                threshold: Some(12.0),
                message: None,
                clear_acknowledgement: true,
            },
        )
        .expect("escalate");

    let row = storage.get_alarm(&id).unwrap().unwrap();
    assert_eq!(row.acknowledged_at, None);
    assert_eq!(row.acknowledged_by_username, None);
    assert_eq!(row.acknowledged_by_user_id, None);
    let unacked = AlarmListFilter {
        include_acknowledged: false,
        ..AlarmListFilter::default()
    };
    assert_eq!(storage.list_alarms(&unacked).unwrap().1, 1, "waits again");
    let notes = storage.list_alarm_notes(&id).unwrap();
    assert_eq!(notes.len(), 1, "the note stays");
}

#[test]
fn state_update_in_place_shifts_from_state() {
    let (_dir, storage) = open_storage();
    let mut event = raise("hub-1", "line-1", "warning", "2026-08-18 10:00:00");
    event.to_state = Some("DISCONNECTED".into());
    let id = storage.insert_alarm_event(&event).expect("insert");

    assert!(storage
        .update_alarm_state(&id, "RECONNECTING", Some("link flapping"))
        .expect("update state"));

    let row = storage.get_alarm(&id).expect("get").expect("row");
    assert_eq!(row.from_state.as_deref(), Some("DISCONNECTED"));
    assert_eq!(row.to_state.as_deref(), Some("RECONNECTING"));
    assert_eq!(row.message.as_deref(), Some("link flapping"));
}

#[test]
fn acknowledge_roundtrip() {
    let (_dir, storage) = open_storage();
    let id = storage
        .insert_alarm_event(&raise("hub-1", "line-1", "critical", "2026-08-18 10:00:00"))
        .expect("insert");

    let acked = storage
        .acknowledge_alarm(&id, Some(7), "michal", "2026-08-18 10:02:00")
        .expect("ack")
        .expect("row returned");
    assert_eq!(
        acked.acknowledged_at.as_deref(),
        Some("2026-08-18 10:02:00")
    );
    assert_eq!(acked.acknowledged_by_user_id, Some(7));
    assert_eq!(acked.acknowledged_by_username.as_deref(), Some("michal"));

    // persisted, and unknown ids return None
    let row = storage.get_alarm(&id).expect("get").expect("row");
    assert_eq!(row.acknowledged_by_username.as_deref(), Some("michal"));

    // HS-12: a second acknowledge keeps the first acknowledger and time.
    let again = storage
        .acknowledge_alarm(&id, Some(9), "anna", "2026-08-18 11:00:00")
        .expect("re-ack")
        .expect("row returned");
    assert_eq!(again.acknowledged_by_username.as_deref(), Some("michal"));
    assert_eq!(again.acknowledged_by_user_id, Some(7));
    assert_eq!(
        again.acknowledged_at.as_deref(),
        Some("2026-08-18 10:02:00")
    );
    assert!(storage
        .acknowledge_alarm("no-such-id", None, "x", "2026-08-18 10:03:00")
        .expect("ack missing")
        .is_none());
}

#[test]
fn native_key_duplicate_is_conflict() {
    let (_dir, storage) = open_storage();
    let mut event = raise("hub-1", "line-1", "warning", "2026-08-18 10:00:00");
    event.native_key = Some("hub-1:3:17".into());

    storage.insert_alarm_event(&event).expect("first insert");
    let err = storage
        .insert_alarm_event(&event)
        .expect_err("duplicate native_key must fail");
    assert!(matches!(err, StorageError::Conflict(_)), "got {err:?}");

    // NULL native_key rows never conflict with each other
    let plain = raise("hub-1", "line-2", "warning", "2026-08-18 10:01:00");
    storage.insert_alarm_event(&plain).expect("null key #1");
    storage.insert_alarm_event(&plain).expect("null key #2");
}

#[test]
fn list_alarms_orders_by_the_column_the_page_asked_for() {
    let (_dir, storage) = open_storage();
    // Names deliberately out of step with the times: a sort that silently
    // fell back to `triggered_at` would still look sorted otherwise.
    for (sensor, at) in [
        ("line-3", "2026-08-18 10:00:00"),
        ("line-1", "2026-08-18 10:01:00"),
        ("line-2", "2026-08-18 10:02:00"),
    ] {
        storage
            .insert_alarm_event(&raise("hub-1", sensor, "warning", at))
            .expect("insert");
    }

    let names = |filter: &AlarmListFilter| -> Vec<String> {
        storage
            .list_alarms(filter)
            .expect("list")
            .0
            .iter()
            .map(|a| a.sensor_id.clone())
            .collect()
    };

    // The default is newest first and owes nothing to the name.
    assert_eq!(
        names(&AlarmListFilter::default()),
        ["line-2", "line-1", "line-3"]
    );
    assert_eq!(
        names(&AlarmListFilter {
            sort: AlarmSort::Name,
            sort_desc: false,
            ..Default::default()
        }),
        ["line-1", "line-2", "line-3"]
    );
    assert_eq!(
        names(&AlarmListFilter {
            sort: AlarmSort::Name,
            sort_desc: true,
            ..Default::default()
        }),
        ["line-3", "line-2", "line-1"]
    );
    // Ordering applies to the page, not to the rows that happen to be on it:
    // page two of an ascending sort is the last name, not the last insert.
    assert_eq!(
        names(&AlarmListFilter {
            sort: AlarmSort::Name,
            sort_desc: false,
            limit: 2,
            offset: 2,
            ..Default::default()
        }),
        ["line-3"]
    );
}

#[test]
fn alarm_sort_keys_round_trip() {
    // The query string carries `key()` and has to come back as the same
    // variant, or a sorted link would quietly reset to the default order.
    for sort in [
        AlarmSort::Start,
        AlarmSort::End,
        AlarmSort::Location,
        AlarmSort::Name,
        AlarmSort::Type,
        AlarmSort::Value,
    ] {
        assert_eq!(AlarmSort::from_key(sort.key()), Some(sort));
    }
    assert_eq!(AlarmSort::from_key("; DROP TABLE alarm_events"), None);
}

#[test]
fn list_alarms_filter_matrix() {
    let (_dir, storage) = open_storage();

    // hub-1: active warning, active critical, cleared critical, acked warning
    let _w1 = storage
        .insert_alarm_event(&raise("hub-1", "line-1", "warning", "2026-08-18 10:00:00"))
        .expect("insert");
    let c1 = storage
        .insert_alarm_event(&raise("hub-1", "line-2", "critical", "2026-08-18 10:01:00"))
        .expect("insert");
    let c2 = storage
        .insert_alarm_event(&raise("hub-1", "line-3", "critical", "2026-08-18 10:02:00"))
        .expect("insert");
    storage
        .clear_alarm(&c2, "2026-08-18 10:10:00", None)
        .expect("clear c2");
    let w2 = storage
        .insert_alarm_event(&raise("hub-1", "line-4", "warning", "2026-08-18 10:03:00"))
        .expect("insert");
    storage
        .acknowledge_alarm(&w2, Some(1), "michal", "2026-08-18 10:04:00")
        .expect("ack w2");
    // hub-2: one active warning
    storage
        .insert_alarm_event(&raise("hub-2", "line-1", "warning", "2026-08-18 10:05:00"))
        .expect("insert");

    // no filter: everything, newest first
    let (rows, total) = storage
        .list_alarms(&AlarmListFilter::default())
        .expect("list all");
    assert_eq!(total, 5);
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].hostname, "hub-2");

    // exclude acknowledged
    let (rows, total) = storage
        .list_alarms(&AlarmListFilter {
            include_acknowledged: false,
            ..Default::default()
        })
        .expect("list unacked");
    assert_eq!(total, 4);
    assert!(rows.iter().all(|a| a.acknowledged_at.is_none()));

    // exclude cleared
    let (_, total) = storage
        .list_alarms(&AlarmListFilter {
            include_cleared: false,
            ..Default::default()
        })
        .expect("list uncleared");
    assert_eq!(total, 4);

    // hostname + severity combo
    let (rows, total) = storage
        .list_alarms(&AlarmListFilter {
            hostname: Some("hub-1".into()),
            severity: Some("critical".into()),
            ..Default::default()
        })
        .expect("list hub-1 critical");
    assert_eq!(total, 2);
    assert_eq!(rows[0].id, c2); // 10:02 before 10:01
    assert_eq!(rows[1].id, c1);

    // alarm_type filter matches everything here
    let (_, total) = storage
        .list_alarms(&AlarmListFilter {
            alarm_type: Some("HIGH".into()),
            ..Default::default()
        })
        .expect("list HIGH");
    assert_eq!(total, 5);

    // pagination: total stays 5, pages don't overlap
    let (page1, total) = storage
        .list_alarms(&AlarmListFilter {
            limit: 2,
            offset: 0,
            ..Default::default()
        })
        .expect("page 1");
    let (page2, total2) = storage
        .list_alarms(&AlarmListFilter {
            limit: 2,
            offset: 2,
            ..Default::default()
        })
        .expect("page 2");
    assert_eq!((total, total2), (5, 5));
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 2);
    assert!(page1.iter().all(|a| page2.iter().all(|b| a.id != b.id)));

    // limit is clamped up from nonsense values
    let (rows, _) = storage
        .list_alarms(&AlarmListFilter {
            limit: 0,
            ..Default::default()
        })
        .expect("limit 0 clamps to 1");
    assert_eq!(rows.len(), 1);
}

#[test]
fn notes_roundtrip_and_cascade_on_alarm_delete() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open");

    let id = storage
        .insert_alarm_event(&raise("hub-1", "line-1", "warning", "2026-08-18 10:00:00"))
        .expect("insert alarm");

    let note1 = storage
        .insert_alarm_note(&id, Some(7), "michal", "first note")
        .expect("note 1");
    let note2 = storage
        .insert_alarm_note(&id, None, "system", "ack reason")
        .expect("note 2");
    assert_eq!(note1.alarm_id, id);
    assert!(!note1.created_at.is_empty());

    let notes = storage.list_alarm_notes(&id).expect("list notes");
    assert_eq!(
        notes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        vec![note1.id.as_str(), note2.id.as_str()],
        "oldest first"
    );

    // delete the alarm through a second connection (FKs on) — notes cascade
    {
        let conn = rusqlite::Connection::open(&path).expect("second connection");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("fk pragma");
        conn.execute("DELETE FROM alarm_events WHERE id = ?1", [&id])
            .expect("delete alarm");
    }
    assert!(storage.get_alarm(&id).expect("get").is_none());
    assert!(storage
        .list_alarm_notes(&id)
        .expect("list after delete")
        .is_empty());
}

#[test]
fn global_config_singleton_defaults_and_upsert() {
    let (_dir, storage) = open_storage();

    let cfg = storage.get_notify_global_config().expect("defaults");
    assert_eq!(cfg.provider, "none");
    assert_eq!(cfg.from_name, "ProXimos Monitor");
    assert!(cfg.sendgrid_api_key_enc.is_none());
    assert!(cfg.smtp_use_tls);
    assert!(!cfg.smtp_use_ssl);
    assert!(!cfg.webhook_enabled);
    assert_eq!(cfg.webhook_method, "POST");
    assert_eq!(cfg.webhook_timeout_s, 10);
    assert_eq!(cfg.webhook_max_attempts, 3);
    assert!(cfg.updated_at.is_none());

    let mut wanted = cfg.clone();
    wanted.provider = "smtp".into();
    wanted.from_email = Some("alerts@example.com".into());
    wanted.smtp_host = Some("mail.example.com".into());
    wanted.smtp_port = Some(587);
    wanted.smtp_password_enc = Some(vec![1, 2, 3, 4]);
    wanted.webhook_enabled = true;
    wanted.webhook_url = Some("https://hooks.example.com/x".into());
    wanted.webhook_headers_enc = Some(vec![9, 9]);
    storage
        .upsert_notify_global_config(&wanted)
        .expect("upsert");

    let stored = storage.get_notify_global_config().expect("reload");
    assert!(stored.updated_at.is_some(), "repo stamps updated_at");
    wanted.updated_at = stored.updated_at.clone();
    assert_eq!(stored, wanted);
}

#[test]
fn device_report_config_upsert_roundtrip() {
    let (_dir, storage) = open_storage();

    assert!(storage
        .get_device_report_config("hub-1")
        .expect("get missing")
        .is_none());

    let cfg = DeviceReportConfig {
        hostname: "hub-1".into(),
        recipients: vec!["a@example.com".into(), "b@example.com".into()],
        enabled: true,
        notify_on_alarm: true,
        notify_on_clear: true,
        notify_on_ack: false,
        updated_at: None,
    };
    storage.upsert_device_report_config(&cfg).expect("insert");

    let stored = storage
        .get_device_report_config("hub-1")
        .expect("get")
        .expect("row");
    assert_eq!(stored.recipients, cfg.recipients);
    assert!(stored.enabled && stored.notify_on_clear);
    assert!(stored.updated_at.is_some());

    // second upsert replaces in place
    let cfg2 = DeviceReportConfig {
        recipients: vec![],
        enabled: false,
        ..cfg
    };
    storage.upsert_device_report_config(&cfg2).expect("update");
    let stored = storage
        .get_device_report_config("hub-1")
        .expect("get")
        .expect("row");
    assert!(stored.recipients.is_empty());
    assert!(!stored.enabled);
}

#[test]
fn system_alert_config_defaults_and_upsert() {
    let (_dir, storage) = open_storage();

    let cfg = storage.get_system_alert_config().expect("defaults");
    assert!(cfg.recipients.is_empty());
    assert!(cfg.battery_enabled);
    assert_eq!(cfg.battery_threshold, 20);
    assert!(cfg.network_enabled && cfg.storage_enabled && cfg.device_offline_enabled);
    assert_eq!(cfg.storage_threshold, 90);
    assert!(!cfg.version_mismatch_enabled);
    assert!(cfg.expected_version.is_none());

    let mut wanted = cfg.clone();
    wanted.recipients = vec!["ops@example.com".into()];
    wanted.battery_threshold = 15;
    wanted.version_mismatch_enabled = true;
    wanted.expected_version = Some("2.4.0".into());
    storage.upsert_system_alert_config(&wanted).expect("upsert");

    let stored = storage.get_system_alert_config().expect("reload");
    assert!(stored.updated_at.is_some());
    wanted.updated_at = stored.updated_at.clone();
    assert_eq!(stored, wanted);
}

#[test]
fn report_history_insert_and_filters() {
    let (_dir, storage) = open_storage();

    let entry =
        |hostname: Option<&str>, event_type: &str, status: &str, source: &str, sent_at: &str| {
            NewReportHistory {
                hostname: hostname.map(Into::into),
                event_type: event_type.into(),
                recipients: vec!["a@example.com".into()],
                subject: Some("subject".into()),
                message: None,
                status: status.into(),
                error_message: (status == HISTORY_STATUS_FAILED).then(|| "smtp timeout".into()),
                provider: Some("smtp".into()),
                source: source.into(),
                sent_at: Some(sent_at.into()),
            }
        };

    storage
        .insert_report_history(&entry(
            Some("hub-1"),
            "alarm",
            HISTORY_STATUS_SENT,
            HISTORY_SOURCE_REPORT,
            "2026-08-18 10:00:00",
        ))
        .expect("insert 1");
    storage
        .insert_report_history(&entry(
            Some("hub-1"),
            "alarm_clear",
            HISTORY_STATUS_FAILED,
            HISTORY_SOURCE_REPORT,
            "2026-08-18 11:00:00",
        ))
        .expect("insert 2");
    storage
        .insert_report_history(&entry(
            Some("hub-2"),
            "battery_low",
            HISTORY_STATUS_SENT,
            HISTORY_SOURCE_SYSTEM_ALERT,
            "2026-08-18 12:00:00",
        ))
        .expect("insert 3");
    // sent_at: None takes the DB default (datetime('now') > our fixtures)
    storage
        .insert_report_history(&NewReportHistory {
            sent_at: None,
            ..entry(None, "test", HISTORY_STATUS_SENT, HISTORY_SOURCE_REPORT, "")
        })
        .expect("insert 4");

    // unfiltered: newest first, total = 4
    let (rows, total) = storage
        .list_report_history(&ReportHistoryFilter::default())
        .expect("list all");
    assert_eq!(total, 4);
    assert_eq!(rows[0].event_type, "test", "datetime('now') sorts newest");
    assert_eq!(rows[0].recipients, vec!["a@example.com".to_string()]);

    // status filter
    let (rows, total) = storage
        .list_report_history(&ReportHistoryFilter {
            status: Some(HISTORY_STATUS_FAILED.into()),
            ..Default::default()
        })
        .expect("failed only");
    assert_eq!(total, 1);
    assert_eq!(rows[0].error_message.as_deref(), Some("smtp timeout"));

    // source filter
    let (rows, total) = storage
        .list_report_history(&ReportHistoryFilter {
            source: Some(HISTORY_SOURCE_SYSTEM_ALERT.into()),
            ..Default::default()
        })
        .expect("system alerts only");
    assert_eq!(total, 1);
    assert_eq!(rows[0].event_type, "battery_low");

    // event_type + hostname
    let (_, total) = storage
        .list_report_history(&ReportHistoryFilter {
            event_type: Some("alarm".into()),
            hostname: Some("hub-1".into()),
            ..Default::default()
        })
        .expect("alarm@hub-1");
    assert_eq!(total, 1);

    // since/until window catches the middle two fixtures
    let (rows, total) = storage
        .list_report_history(&ReportHistoryFilter {
            since: Some("2026-08-18 10:30:00".into()),
            until: Some("2026-08-18 12:30:00".into()),
            ..Default::default()
        })
        .expect("window");
    assert_eq!(total, 2);
    assert_eq!(rows[0].event_type, "battery_low");
    assert_eq!(rows[1].event_type, "alarm_clear");

    // pagination
    let (page, total) = storage
        .list_report_history(&ReportHistoryFilter {
            limit: 2,
            offset: 2,
            ..Default::default()
        })
        .expect("page 2");
    assert_eq!(total, 4);
    assert_eq!(page.len(), 2);
}

// ---- MB3: the probe-detail window's two sensor filters ----------------
//
// `AlarmListFilter` filtered by hostname, type, severity and
// acknowledgement and never by sensor (monitoring.backend.md §6 WP3.2);
// the window needs one probe's whole alarm history, value classes and
// liveness alike, and the encodings are prefix-shaped.

/// Seeds one Hub with alarms on a wired line, a Node and a Beacon —
/// including a `native:` slot and MB3's `liveness` component, the two
/// non-field things that share a radio probe's prefix.
fn seed_probe_alarms(storage: &Storage) {
    for (sensor_id, alarm_type) in [
        ("line-1", "HIGH"),
        ("line-2", "HIGH"),
        ("lrw:n1:Temperature", "HIGH"),
        ("lrw:n1:native:3", "LOW"),
        ("lrw:n1:liveness", "OFFLINE"),
        ("lrw:n10:Temperature", "HIGH"),
        ("eye:AA:BB:CC:DD:EE:01:Humidity", "HIGH"),
        ("eye:AA:BB:CC:DD:EE:01:liveness", "OFFLINE"),
        ("eye:AA:BB:CC:DD:EE:02:Humidity", "HIGH"),
    ] {
        let mut row = raise("hub-1", sensor_id, "warning", "2026-09-09T10:00:00Z");
        row.alarm_type = alarm_type.into();
        storage.insert_alarm_event(&row).expect("insert");
    }
}

#[test]
fn alarms_filter_by_exact_sensor_id() {
    let (_dir, storage) = open_storage();
    seed_probe_alarms(&storage);

    // A wired probe's `line-{n}` is the whole key — there is nothing to
    // prefix on, and a prefix match would make `line-1` swallow `line-10`.
    let (rows, total) = storage
        .list_alarms(&AlarmListFilter {
            sensor_id: Some("line-1".into()),
            ..Default::default()
        })
        .expect("list");
    assert_eq!(total, 1);
    assert_eq!(rows[0].sensor_id, "line-1");

    let (_, none) = storage
        .list_alarms(&AlarmListFilter {
            sensor_id: Some("line-9".into()),
            ..Default::default()
        })
        .expect("list");
    assert_eq!(none, 0);
}

#[test]
fn alarms_filter_by_sensor_id_prefix() {
    let (_dir, storage) = open_storage();
    seed_probe_alarms(&storage);

    // One Node's whole history: the field label, the native slot and the
    // liveness outage, and NOT the Node whose key merely starts the same
    // way — the prefix ends in the separating colon, which is what keeps
    // `n1` from matching `n10`.
    let (rows, total) = storage
        .list_alarms(&AlarmListFilter {
            sensor_id_prefix: Some("lrw:n1:".into()),
            ..Default::default()
        })
        .expect("list");
    assert_eq!(total, 3);
    let mut ids: Vec<&str> = rows.iter().map(|a| a.sensor_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["lrw:n1:Temperature", "lrw:n1:liveness", "lrw:n1:native:3"]
    );

    // A MAC is full of colons, so the Beacon prefix is the case that would
    // break a parser that split on them — it is a `substr` compare, not a
    // parse, and not a `LIKE` pattern either.
    let (rows, total) = storage
        .list_alarms(&AlarmListFilter {
            sensor_id_prefix: Some("eye:AA:BB:CC:DD:EE:01:".into()),
            ..Default::default()
        })
        .expect("list");
    assert_eq!(total, 2);
    assert!(rows
        .iter()
        .all(|a| a.sensor_id.starts_with("eye:AA:BB:CC:DD:EE:01:")));

    // The two filters AND with the existing ones rather than replacing them.
    let (_, offline_only) = storage
        .list_alarms(&AlarmListFilter {
            sensor_id_prefix: Some("lrw:n1:".into()),
            alarm_type: Some("OFFLINE".into()),
            ..Default::default()
        })
        .expect("list");
    assert_eq!(offline_only, 1);
}

/// The free-text search is a SQL clause, and the one part of it that fails
/// silently is the escaping: an operator typing `%` into a search box means
/// a per-cent sign, not "match anything".
///
/// It searches the three columns the operator reads on the row — name, raw id
/// and location — because that is what the client-side pass it replaced
/// searched, and because those are the three a person can see to type.
#[test]
fn the_alarm_search_matches_three_columns_and_takes_wildcards_literally() {
    let (_dir, storage) = open_storage();
    let mut rows = vec![
        raise("hub", "line-1", "warning", "2026-09-01T10:00:00Z"),
        raise("hub", "line-2", "warning", "2026-09-01T11:00:00Z"),
        raise(
            "hub",
            "lrw:aabb:Temperature",
            "warning",
            "2026-09-01T12:00:00Z",
        ),
    ];
    rows[0].sensor_name = Some("Freezer".into());
    rows[0].sensor_location = Some("Zone G".into());
    rows[1].sensor_name = Some("100% humidity probe".into());
    rows[1].sensor_location = Some("Zone H".into());
    rows[2].sensor_name = Some("Temperature".into());
    rows[2].sensor_location = Some("Cold Room A".into());
    for row in &rows {
        storage.insert_alarm_event(row).expect("insert");
    }

    let find = |q: &str| -> Vec<String> {
        let (page, _) = storage
            .list_alarms(&AlarmListFilter {
                search: Some(q.to_string()),
                ..Default::default()
            })
            .expect("list");
        page.iter().map(|a| a.sensor_id.clone()).collect()
    };

    assert_eq!(find("freezer"), ["line-1"], "name, case-insensitively");
    assert_eq!(find("zone h"), ["line-2"], "location");
    assert_eq!(find("lrw:aabb"), ["lrw:aabb:Temperature"], "the raw id");

    // The wildcards. `%` bare would match every row; `_` bare would match any
    // single character, so `Zone_G` would find "Zone G" by accident rather
    // than because the operator asked for an underscore.
    assert_eq!(
        find("100%"),
        ["line-2"],
        "a per-cent sign is a per-cent sign"
    );
    assert_eq!(
        find("%"),
        ["line-2"],
        "and a lone one finds the row that contains one — not, as an \
         unescaped LIKE would have it, all three"
    );
    assert!(find("Zone_G").is_empty(), "an underscore is an underscore");
}

/// The search reaches the operator's metadata of the device an alarm belongs
/// to (name, description, location) and the live-registry targets the
/// caller resolves, not only the columns stored on the alarm row.
#[test]
fn search_matches_device_description_hub_and_targets() {
    let (_dir, storage) = open_storage();
    for (hub, sensor) in [
        ("hub-1", "lrw:a1b2c3d4e5f60708:Temperature"),
        ("hub-1", "eye:AA:BB:CC:DD:EE:01:Temperature"),
        ("hub-1", "line-2"),
        ("hub-2", "line-2"),
        ("hub-2", "sys:power"),
        ("hub-1", "cal:node:a1b2c3d4e5f60708"),
        ("hub-2", "cal:probe:3"),
    ] {
        storage
            .insert_alarm_event(&raise(hub, sensor, "warning", "2026-08-18 10:00:00"))
            .expect("insert");
    }
    let meta = |scope: &str, host: &str, entity: &str, desc: &str| {
        let mut m = NewDeviceMeta::new(host, scope, entity);
        m.description = Some(desc.into());
        storage.upsert_device_meta(&m).expect("meta");
    };
    meta("node", "hub-1", "A1B2C3D4E5F60708", "freezer north");
    meta("eye", "hub-1", "AA:BB:CC:DD:EE:01", "Mrazak sklad");
    meta("probe", "hub-2", "2", "boiler pipe");
    meta("hub", "hub-2", "hub-2", "basement rack");
    meta("probe", "hub-2", "line-3", "return pipe");

    let hits = |q: &str, targets: Vec<(String, String)>| -> Vec<String> {
        let mut v: Vec<String> = storage
            .list_alarms(&AlarmListFilter {
                search: Some(q.into()),
                search_targets: targets,
                ..Default::default()
            })
            .expect("list")
            .0
            .into_iter()
            .map(|a| format!("{}/{}", a.hostname, a.sensor_id))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        hits("FREEZER", vec![]),
        [
            "hub-1/cal:node:a1b2c3d4e5f60708",
            "hub-1/lrw:a1b2c3d4e5f60708:Temperature"
        ]
    );
    assert_eq!(hits("return pipe", vec![]), ["hub-2/cal:probe:3"]);
    assert_eq!(
        hits("mrazak", vec![]),
        ["hub-1/eye:AA:BB:CC:DD:EE:01:Temperature"]
    );
    assert_eq!(hits("boiler", vec![]), ["hub-2/line-2"]);
    assert_eq!(hits("basement", vec![]), ["hub-2/sys:power"]);
    assert_eq!(
        hits("hub-2", vec![]),
        ["hub-2/cal:probe:3", "hub-2/line-2", "hub-2/sys:power"]
    );
    assert_eq!(
        hits("nothing-stored", vec![("hub-1".into(), "line-2".into())]),
        ["hub-1/line-2"]
    );
    assert_eq!(
        hits(
            "nothing-stored",
            vec![(String::new(), "EYE:aa:bb:cc:dd:ee:01:".into())]
        ),
        ["hub-1/eye:AA:BB:CC:DD:EE:01:Temperature"]
    );
}

/// The Quantity filter: a radio alarm by the field its id ends in (any
/// case), a wired probe as temperature, nothing system- or calibration-wide.
#[test]
fn sensor_field_filters_by_quantity() {
    let (_dir, storage) = open_storage();
    for sensor in [
        "lrw:a1b2c3d4e5f60708:Temperature",
        "lrw:a1b2c3d4e5f60708:Humidity",
        "eye:AA:BB:CC:DD:EE:01:Humidity",
        "line-2",
        "sys:power",
        "cal:probe:3",
    ] {
        storage
            .insert_alarm_event(&raise("hub-1", sensor, "warning", "2026-08-18 10:00:00"))
            .expect("insert");
    }
    let hits = |field: &str| -> Vec<String> {
        let mut v: Vec<String> = storage
            .list_alarms(&AlarmListFilter {
                sensor_field: Some(field.into()),
                ..Default::default()
            })
            .expect("list")
            .0
            .into_iter()
            .map(|a| a.sensor_id)
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        hits("temperature"),
        ["line-2", "lrw:a1b2c3d4e5f60708:Temperature"]
    );
    assert_eq!(
        hits("humidity"),
        [
            "eye:AA:BB:CC:DD:EE:01:Humidity",
            "lrw:a1b2c3d4e5f60708:Humidity"
        ]
    );
}
