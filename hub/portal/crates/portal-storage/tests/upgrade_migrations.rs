//! Migrations that change existing rows, each run over a database built at
//! the version before it (`Storage::open_at_version`), so the migration sees
//! data it did not create.

use portal_storage::Storage;
use tempfile::tempdir;

/// decisions.md #36: an installation with e-mail set up keeps English after
/// V18's cs default; one without e-mail gets cs.
#[test]
fn v22_keeps_english_where_email_is_set_up() {
    for (provider, lang) in [("smtp", "en"), ("sendgrid", "en"), ("none", "cs")] {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("portal.db");
        drop(Storage::open_at_version(&path, None, 21).expect("open at V21"));
        rusqlite::Connection::open(&path)
            .expect("open fixture")
            .execute("UPDATE notify_global_config SET provider = ?1", [provider])
            .expect("set the provider");

        let s = Storage::open(&path, None).expect("migrate to the newest version");
        let cfg = s.get_notify_global_config().expect("read the config");
        assert_eq!(cfg.email_lang, lang, "provider {provider}");
    }
}

/// V23: the link alarms the pre-UX-058 rule opened for an unused uplink can
/// never clear, so the upgrade closes every open `sys:wifi` / `sys:ethernet`
/// row. Other open alarms, and link alarms already closed, are left as they
/// were.
#[test]
fn v23_closes_open_link_alarms_only() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let alarm = |sensor_id: &str, cleared_at: Option<&str>| portal_storage::NewAlarmEvent {
        hostname: "proximos-1a2b3c4d".into(),
        sensor_id: sensor_id.into(),
        sensor_name: None,
        sensor_location: None,
        alarm_type: "CONNECTIVITY".into(),
        severity: "warning".into(),
        from_state: None,
        to_state: None,
        message: Some("link down".into()),
        value: None,
        threshold: None,
        triggered_at: "2026-09-20T00:00:00Z".into(),
        cleared_at: cleared_at.map(Into::into),
        acknowledged_at: None,
        acknowledged_by_user_id: None,
        acknowledged_by_username: None,
        native_key: None,
    };
    let (wifi, ethernet, power, closed) = {
        let old = Storage::open_at_version(&path, None, 22).expect("open at V22");
        let insert = |a| old.insert_alarm_event(&a).expect("seed alarm");
        (
            insert(alarm("sys:wifi", None)),
            insert(alarm("sys:ethernet", None)),
            insert(alarm("sys:power", None)),
            insert(alarm("sys:wifi", Some("2026-09-21T00:00:00Z"))),
        )
    };

    let s = Storage::open(&path, None).expect("migrate to the newest version");
    let get = |id: &str| s.get_alarm(id).expect("read").expect("row exists");
    for id in [&wifi, &ethernet] {
        let row = get(id);
        assert!(row.cleared_at.is_some(), "{} is closed", row.sensor_id);
        assert!(row.message.as_deref().is_some_and(|m| m.contains("UX-058")));
    }
    assert!(get(&power).cleared_at.is_none(), "sys:power stays open");
    let already = get(&closed);
    assert_eq!(already.cleared_at.as_deref(), Some("2026-09-21T00:00:00Z"));
    assert_eq!(already.message.as_deref(), Some("link down"));
}
