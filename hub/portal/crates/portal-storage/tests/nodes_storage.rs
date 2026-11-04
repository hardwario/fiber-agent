//! M5-B: Node (STICKER) + Beacon (EYE) persistence — dedup semantics,
//! raw-message ring buffer, threshold change history, backfill lifecycle.

use portal_storage::{
    EyeReading, NewBackfillJob, NewFieldThreshold, NodeCounterReading, NodeEvent, NodeFieldReading,
    Storage, JOB_DONE, JOB_PENDING, JOB_RECEIVING, JOB_REQUESTED, SOURCE_LIVE, SOURCE_REPLAY,
    THRESHOLD_SCOPE_NODE,
};
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn field_row(ts: i64, value: f64, source: &str) -> NodeFieldReading {
    NodeFieldReading {
        hostname: "hub-1".into(),
        node_key: "70b3d5e050011234".into(),
        field: "temperature".into(),
        ts,
        value: Some(value),
        alarm_state: Some("NORMAL".into()),
        source: source.into(),
    }
}

// --- field readings -------------------------------------------------------

#[test]
fn field_reading_insert_or_ignore_dedups_on_natural_key() {
    let (_dir, s) = open();

    assert!(s
        .insert_field_reading(&field_row(1000, 20.0, SOURCE_LIVE))
        .expect("insert"));
    // history replay frame re-delivers the same (field, ts) — harmless
    assert!(!s
        .insert_field_reading(&field_row(1000, 99.0, SOURCE_REPLAY))
        .expect("dup insert"));

    let rows = s
        .query_field_readings("hub-1", "70b3d5e050011234", "temperature", 0, 2000)
        .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value, Some(20.0)); // first write wins
    assert_eq!(rows[0].source, SOURCE_LIVE);
}

#[test]
fn field_reading_batch_reports_only_new_inserts() {
    let (_dir, s) = open();

    assert!(s
        .insert_field_reading(&field_row(1000, 20.0, SOURCE_LIVE))
        .expect("seed"));

    let batch = vec![
        field_row(1000, 20.0, SOURCE_REPLAY), // dup
        field_row(1060, 20.5, SOURCE_REPLAY),
        field_row(1120, 21.0, SOURCE_REPLAY),
    ];
    let inserted = s.insert_field_readings(&batch).expect("batch");
    assert_eq!(inserted, 2, "points_inserted must skip natural-key dups");

    let rows = s
        .query_field_readings("hub-1", "70b3d5e050011234", "temperature", 0, 5000)
        .expect("query");
    assert_eq!(rows.len(), 3);
    assert!(
        rows.windows(2).all(|w| w[0].ts < w[1].ts),
        "ordered by ts ASC"
    );
}

#[test]
fn latest_field_reading_and_extent() {
    let (_dir, s) = open();

    assert_eq!(
        s.latest_field_reading("hub-1", "70b3d5e050011234", "temperature")
            .expect("query empty"),
        None
    );
    assert_eq!(
        s.field_extent("hub-1", "70b3d5e050011234", "temperature")
            .expect("extent empty"),
        None
    );

    for (ts, v) in [(1000, 20.0), (1120, 21.0), (1060, 20.5)] {
        s.insert_field_reading(&field_row(ts, v, SOURCE_LIVE))
            .expect("insert");
    }

    let latest = s
        .latest_field_reading("hub-1", "70b3d5e050011234", "temperature")
        .expect("query")
        .expect("row");
    assert_eq!(latest.ts, 1120);
    assert_eq!(
        s.field_extent("hub-1", "70b3d5e050011234", "temperature")
            .expect("extent"),
        Some((1000, 1120))
    );
}

/// N14: what a restart refills the in-memory `fields` from — each field's newest value,
/// whatever its age relative to the other fields, and nothing from another Node.
#[test]
fn latest_node_fields_takes_each_fields_newest_value() {
    let (_dir, s) = open();
    assert!(s
        .latest_node_fields("hub-1", "70b3d5e050011234")
        .expect("empty")
        .is_empty());

    for (ts, v) in [(1000, 20.0), (1120, 21.0), (1060, 20.5)] {
        s.insert_field_reading(&field_row(ts, v, SOURCE_LIVE))
            .expect("insert");
    }
    let humidity = NodeFieldReading {
        field: "humidity".into(),
        ..field_row(900, 55.0, SOURCE_LIVE)
    };
    let other_node = NodeFieldReading {
        node_key: "70b3d5e050019999".into(),
        ..field_row(2000, 99.0, SOURCE_LIVE)
    };
    s.insert_field_readings(&[humidity, other_node])
        .expect("insert");

    let mut latest = s
        .latest_node_fields("hub-1", "70b3d5e050011234")
        .expect("query");
    latest.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        latest,
        [
            ("humidity".to_string(), 55.0),
            ("temperature".to_string(), 21.0)
        ]
    );
}

/// STICKER 2.0.0: a quantity sent as `null` after its last value (a `no_data` event, no
/// reading row) is not refilled after a restart, and is named as nulled instead. A value
/// newer than the `null` brings it back.
#[test]
fn a_null_newer_than_the_last_value_is_not_refilled() {
    let (_dir, s) = open();
    let node = "70b3d5e050011234";
    s.insert_field_reading(&field_row(1000, 21.0, SOURCE_LIVE))
        .expect("insert");
    s.insert_field_reading(&NodeFieldReading {
        field: "humidity".into(),
        ..field_row(1000, 55.0, SOURCE_LIVE)
    })
    .expect("insert");
    let no_data = |ts: i64, field: &str| NodeEvent {
        hostname: "hub-1".into(),
        node_key: node.into(),
        event_type: "no_data".into(),
        ts,
        seq: 0,
        payload: Some(format!(
            r#"{{"type":"no_data","ts":"t","field":"{field}"}}"#
        )),
    };
    s.insert_event(&no_data(1060, "temperature"))
        .expect("event");
    // An older null than the value does not count.
    s.insert_event(&no_data(900, "humidity")).expect("event");

    assert_eq!(
        s.latest_node_fields("hub-1", node).expect("query"),
        [("humidity".to_string(), 55.0)]
    );
    assert_eq!(
        s.nulled_node_fields("hub-1", node).expect("nulled"),
        ["temperature".to_string()]
    );

    s.insert_field_reading(&field_row(1120, 22.0, SOURCE_LIVE))
        .expect("insert");
    let mut latest = s.latest_node_fields("hub-1", node).expect("query");
    latest.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        latest,
        [
            ("humidity".to_string(), 55.0),
            ("temperature".to_string(), 22.0)
        ]
    );
    assert!(s
        .nulled_node_fields("hub-1", node)
        .expect("nulled")
        .is_empty());
}

/// `nodes_with_readings` answers "which Nodes have ever reported", and it
/// answers it with a loose index scan — one seek per Node rather than one
/// visit per reading (§21 review round 2, item 1). The shape under test is
/// the shape the rewrite could plausibly get wrong: many readings per Node
/// must collapse to one key each, several fields on one Node must not
/// duplicate it, another Hub's Node must not leak in, and an empty table
/// must not return the recursion's terminating `NULL` as a key.
#[test]
fn nodes_with_readings_returns_each_node_once_across_many_readings() {
    let (_dir, s) = open();

    assert!(
        s.nodes_with_readings("hub-1").expect("empty").is_empty(),
        "no readings, no keys — and no NULL terminator either"
    );

    let mut rows = Vec::new();
    for (node_key, field) in [
        ("70b3d5e050011234", "temperature"),
        ("70b3d5e050011234", "humidity"),
        ("70b3d5e05001aaaa", "temperature"),
    ] {
        for ts in 0..200 {
            rows.push(NodeFieldReading {
                hostname: "hub-1".into(),
                node_key: node_key.into(),
                field: field.into(),
                ts,
                value: Some(ts as f64),
                alarm_state: None,
                source: SOURCE_LIVE.into(),
            });
        }
    }
    // A second Hub's Node, which this Hub's answer must not contain.
    rows.push(NodeFieldReading {
        hostname: "hub-2".into(),
        node_key: "70b3d5e05001bbbb".into(),
        field: "temperature".into(),
        ts: 0,
        value: Some(1.0),
        alarm_state: None,
        source: SOURCE_LIVE.into(),
    });
    assert_eq!(s.insert_field_readings(&rows).expect("seed"), 601);

    assert_eq!(
        s.nodes_with_readings("hub-1").expect("query"),
        std::collections::HashSet::from([
            "70b3d5e050011234".to_string(),
            "70b3d5e05001aaaa".to_string(),
        ]),
        "600 readings over two Nodes are two keys, not 600"
    );
    assert_eq!(
        s.nodes_with_readings("hub-2").expect("query"),
        std::collections::HashSet::from(["70b3d5e05001bbbb".to_string()]),
    );
    assert!(
        s.nodes_with_readings("hub-nope").expect("query").is_empty(),
        "a Hub with no readings at all"
    );

    // The single-Node half of the same question agrees with the fleet half.
    assert_eq!(
        s.node_fields_with_readings("hub-1", "70b3d5e050011234")
            .expect("one"),
        std::collections::HashSet::from(["humidity".to_string(), "temperature".to_string()]),
        "400 readings over two fields are two fields"
    );
    assert!(s
        .node_fields_with_readings("hub-1", "70b3d5e05001bbbb")
        .expect("other Hub's Node")
        .is_empty());
}

// --- counters ---------------------------------------------------------------

#[test]
fn counter_insert_dedups_and_queries_in_order() {
    let (_dir, s) = open();
    let row = |ts: i64, value: i64| NodeCounterReading {
        hostname: "hub-1".into(),
        node_key: "70b3d5e050011234".into(),
        counter: "motion_count".into(),
        ts,
        value: Some(value),
    };

    assert!(s.insert_counter_reading(&row(1000, 5)).expect("insert"));
    assert!(!s.insert_counter_reading(&row(1000, 6)).expect("dup"));
    assert_eq!(
        s.insert_counter_readings(&[row(1000, 7), row(1060, 8)])
            .expect("batch"),
        1
    );

    let rows = s
        .query_counters("hub-1", "70b3d5e050011234", "motion_count", 0, 5000)
        .expect("query");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].value, Some(5)); // first write wins
    assert_eq!(rows[1].ts, 1060);
}

// --- events -------------------------------------------------------------

#[test]
fn event_insert_or_ignore_and_type_filter() {
    let (_dir, s) = open();
    let event = |event_type: &str, ts: i64| NodeEvent {
        hostname: "hub-1".into(),
        node_key: "70b3d5e050011234".into(),
        event_type: event_type.into(),
        ts,
        seq: 0,
        payload: Some(r#"{"type":"alarm"}"#.into()),
    };

    assert!(s.insert_event(&event("alarm", 1000)).expect("insert"));
    assert!(!s
        .insert_event(&event("alarm", 1000))
        .expect("idempotent re-delivery"));
    assert!(s
        .insert_event(&event("boot", 1000))
        .expect("other type, same ts"));
    assert!(s.insert_event(&event("alarm", 1200)).expect("later alarm"));

    let all = s
        .query_events("hub-1", "70b3d5e050011234", None, 0, 5000, None)
        .expect("query all");
    assert_eq!(all.len(), 3);
    assert!(all.windows(2).all(|w| w[0].ts >= w[1].ts), "newest first");

    let alarms = s
        .query_events("hub-1", "70b3d5e050011234", Some("alarm"), 0, 5000, None)
        .expect("query alarms");
    assert_eq!(alarms.len(), 2);
    assert!(alarms.iter().all(|e| e.event_type == "alarm"));
}

/// devices.backend.md §23 (V27): the events of one message share its `ts`, and `seq` keeps
/// them apart. Same type, same `ts`, another `seq` is another row; the same `seq` again is a
/// redelivery. Newest first means the higher `seq` first within a second.
#[test]
fn events_of_one_message_are_kept_apart_by_seq() {
    let (_dir, s) = open();
    let event = |seq: i64, slot: i64| NodeEvent {
        hostname: "hub-1".into(),
        node_key: "70b3d5e050011234".into(),
        event_type: "alarm".into(),
        ts: 1000,
        seq,
        payload: Some(format!(r#"{{"type":"alarm","slot":{slot}}}"#)),
    };

    for (seq, slot) in [(1, 0), (2, 1), (257, 2)] {
        assert!(s.insert_event(&event(seq, slot)).expect("insert"), "{seq}");
    }
    assert!(!s.insert_event(&event(2, 1)).expect("redelivery"));

    let rows = s
        .query_events("hub-1", "70b3d5e050011234", None, 0, 5000, None)
        .expect("query");
    let seqs: Vec<i64> = rows.iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        vec![257, 2, 1],
        "within one ts, the later event first"
    );
}

/// V27 lands on a V26 database holding events: every row survives with `seq = 0`, and the
/// new key takes a second event of the same type and `ts` that the old key refused.
#[test]
fn v22_keeps_the_events_of_a_v20_database() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    drop(Storage::open_at_version(&path, None, 26).expect("open at V26"));
    {
        // The repository writes V27's columns, so the V26 rows go in by hand.
        let conn = rusqlite::Connection::open(&path).expect("raw open");
        for (event_type, ts, payload) in [
            (
                "alarm_batch",
                1000,
                r#"{"type":"alarm_batch","page_index":0}"#,
            ),
            ("alarm", 1000, r#"{"type":"alarm","slot":0}"#),
            ("boot", 900, r#"{"type":"boot"}"#),
        ] {
            conn.execute(
                "INSERT INTO node_events (hostname, node_key, event_type, ts, payload)
                 VALUES ('hub-1', '70b3d5e050011234', ?1, ?2, ?3)",
                rusqlite::params![event_type, ts, payload],
            )
            .expect("a V26 row");
        }
        let refused = conn
            .execute(
                "INSERT OR IGNORE INTO node_events (hostname, node_key, event_type, ts, payload)
                 VALUES ('hub-1', '70b3d5e050011234', 'alarm', 1000, '{}')",
                [],
            )
            .expect("insert or ignore");
        assert_eq!(refused, 0, "the V26 key keeps one alarm per second");
    }

    let s = Storage::open(&path, None).expect("migrate to V27");
    let rows = s
        .query_events("hub-1", "70b3d5e050011234", None, 0, 5000, None)
        .expect("query");
    // Two rows of one second and one seq: no order between them to rely on.
    let mut got: Vec<(&str, i64, i64, Option<&str>)> = rows
        .iter()
        .map(|e| (e.event_type.as_str(), e.ts, e.seq, e.payload.as_deref()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("alarm", 1000, 0, Some(r#"{"type":"alarm","slot":0}"#)),
            (
                "alarm_batch",
                1000,
                0,
                Some(r#"{"type":"alarm_batch","page_index":0}"#)
            ),
            ("boot", 900, 0, Some(r#"{"type":"boot"}"#)),
        ],
        "every row and its payload, with seq 0"
    );

    assert!(s
        .insert_event(&NodeEvent {
            hostname: "hub-1".into(),
            node_key: "70b3d5e050011234".into(),
            event_type: "alarm".into(),
            ts: 1000,
            seq: 2,
            payload: Some(r#"{"type":"alarm","slot":1}"#.into()),
        })
        .expect("insert"));
    assert_eq!(
        s.query_events("hub-1", "70b3d5e050011234", Some("alarm"), 0, 5000, None)
            .expect("query")
            .len(),
        2,
        "the second alarm of that second is kept"
    );
}

// --- raw messages ---------------------------------------------------------

#[test]
fn raw_messages_capped_at_newest_10_per_node() {
    let (_dir, s) = open();

    for i in 0..12 {
        s.insert_raw_message(
            "hub-1",
            "70b3d5e050011234",
            1000 + i,
            &format!("payload-{i}"),
        )
        .expect("insert");
    }
    // another node is untouched by the trim
    s.insert_raw_message("hub-1", "70b3d5e050019999", 1, "other-node")
        .expect("insert other");

    let rows = s
        .list_raw_messages("hub-1", "70b3d5e050011234", Some(50))
        .expect("list");
    assert_eq!(rows.len(), 10, "ring buffer keeps only the newest 10");
    assert_eq!(rows[0].payload, "payload-11", "newest first");
    assert_eq!(rows[9].payload, "payload-2", "oldest two trimmed");

    let other = s
        .list_raw_messages("hub-1", "70b3d5e050019999", None)
        .expect("list other");
    assert_eq!(other.len(), 1);
}

/// Ten per port, not per node: a burst on one port (a 14-page config read on fPort 85) must
/// not push the Node's telemetry out of the ring.
#[test]
fn raw_messages_are_capped_per_port() {
    let (_dir, s) = open();
    for i in 0..3 {
        s.insert_raw_message(
            "hub-1",
            "aa11",
            1000 + i,
            &format!(r#"{{"fport":2,"n":{i}}}"#),
        )
        .expect("insert");
    }
    for i in 0..14 {
        s.insert_raw_message(
            "hub-1",
            "aa11",
            2000 + i,
            &format!(r#"{{"fport":85,"n":{i}}}"#),
        )
        .expect("insert");
    }
    let rows = s
        .list_raw_messages("hub-1", "aa11", Some(50))
        .expect("list");
    let port = |p: &str| rows.iter().filter(|r| r.payload.contains(p)).count();
    assert_eq!(port(r#""fport":2"#), 3, "telemetry survives the burst");
    assert_eq!(port(r#""fport":85"#), 10, "the burst keeps its newest 10");
    assert!(rows.iter().any(|r| r.payload.contains(r#""n":13"#)));
    assert!(!rows
        .iter()
        .any(|r| r.payload.contains(r#""fport":85,"n":3}"#)));
}

// --- eye readings --------------------------------------------------------

#[test]
fn eye_reading_message_id_dedup_returns_false() {
    let (_dir, s) = open();
    let reading = |ts: i64, event_type: &str, message_id: &str| EyeReading {
        hostname: "hub-1".into(),
        mac: "AA:BB:CC:DD:EE:FF".into(),
        ts,
        temperature_c: Some(4.2),
        humidity_pct: Some(61),
        event_type: event_type.into(),
        message_id: Some(message_id.into()),
    };

    // live 30 s bucket: same synthetic message_id from a second gateway
    assert!(s
        .insert_eye_reading(&reading(900, "live", "AA:BB:CC:DD:EE:FF-live-30"))
        .expect("insert"));
    assert!(
        !s.insert_eye_reading(&reading(915, "live", "AA:BB:CC:DD:EE:FF-live-30"))
            .expect("same bucket, other gateway"),
        "duplicate message_id must report false even with a different ts"
    );

    // export/eye path: same natural key is also a dup
    assert!(s
        .insert_eye_reading(&reading(1000, "recording", "msg-1"))
        .expect("insert export row"));
    assert!(!s
        .insert_eye_reading(&reading(1000, "recording", "msg-2"))
        .expect("same natural key"));

    // NULL message_id rows never collide through the partial unique index
    let mut no_id = reading(2000, "advertising", "unused");
    no_id.message_id = None;
    assert!(s.insert_eye_reading(&no_id).expect("null message_id"));
    no_id.ts = 2030;
    assert!(s
        .insert_eye_reading(&no_id)
        .expect("second null message_id"));

    let rows = s
        .query_eye_readings("hub-1", "AA:BB:CC:DD:EE:FF", 0, 5000, None)
        .expect("query");
    assert_eq!(rows.len(), 4);
    assert!(
        rows.windows(2).all(|w| w[0].ts <= w[1].ts),
        "ordered by ts ASC"
    );
}

// --- thresholds ----------------------------------------------------------

#[test]
fn threshold_upsert_writes_history_only_on_value_change() {
    let (_dir, s) = open();
    let row = |warning_high: f64, enabled: bool| NewFieldThreshold {
        hostname: "hub-1".into(),
        scope: THRESHOLD_SCOPE_NODE.into(),
        entity: "70b3d5e050011234".into(),
        field: "temperature".into(),
        critical_low: Some(-2.0),
        warning_low: Some(0.0),
        warning_high: Some(warning_high),
        critical_high: Some(10.0),
        enabled,
    };
    let history = |field: Option<&str>| {
        s.threshold_history("hub-1", THRESHOLD_SCOPE_NODE, "70b3d5e050011234", field)
            .expect("history")
    };

    // first write = a change, gets a history row
    assert!(s
        .upsert_field_threshold(&row(8.0, true), Some("michal"))
        .expect("first upsert"));
    assert_eq!(history(None).len(), 1);

    // firmware echoes identical values every ~30 s — must be a no-op
    assert!(!s
        .upsert_field_threshold(&row(8.0, true), Some("firmware-echo"))
        .expect("identical upsert"));
    assert_eq!(history(None).len(), 1, "no-op writes no history");

    // actual value change
    assert!(s
        .upsert_field_threshold(&row(7.5, true), Some("michal"))
        .expect("value change"));
    let hist = history(Some("temperature"));
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].warning_high, Some(7.5), "newest first");
    assert_eq!(hist[0].actor.as_deref(), Some("michal"));

    // enabled-only toggle updates the row but writes no history
    assert!(s
        .upsert_field_threshold(&row(7.5, false), Some("michal"))
        .expect("disable"));
    assert_eq!(history(None).len(), 2);
    let stored = s
        .get_field_threshold(
            "hub-1",
            THRESHOLD_SCOPE_NODE,
            "70b3d5e050011234",
            "temperature",
        )
        .expect("get")
        .expect("row");
    assert!(!stored.enabled);
    assert!(stored.updated_at.is_some());

    // list + delete
    assert_eq!(
        s.list_field_thresholds("hub-1", THRESHOLD_SCOPE_NODE, "70b3d5e050011234")
            .expect("list")
            .len(),
        1
    );
    assert!(s
        .delete_field_threshold(
            "hub-1",
            THRESHOLD_SCOPE_NODE,
            "70b3d5e050011234",
            "temperature"
        )
        .expect("delete"));
    assert!(!s
        .delete_field_threshold(
            "hub-1",
            THRESHOLD_SCOPE_NODE,
            "70b3d5e050011234",
            "temperature"
        )
        .expect("second delete"));
}

// --- backfill jobs ---------------------------------------------------------

fn new_job(time_from: i64, time_to: i64) -> NewBackfillJob {
    NewBackfillJob {
        hostname: "hub-1".into(),
        node_key: "70b3d5e050011234".into(),
        time_from,
        time_to,
        trigger: "manual".into(),
        samples_expected: Some(120),
    }
}

#[test]
fn backfill_lifecycle_credit_gating_and_promotion() {
    let (_dir, s) = open();
    let job = s.create_job(&new_job(1000, 2000)).expect("create");
    assert_eq!(job.status, JOB_PENDING);
    assert_eq!(job.samples_received, 0);
    assert!(!job.device_refused);
    assert!(job.requested_at.is_none());

    // frames must not credit a pending job
    assert!(
        s.credit_frames(job.job_id, 10, 10, 1500)
            .expect("credit pending")
            .is_none(),
        "only requested/receiving jobs are creditable"
    );

    let job = s
        .update_job_status(job.job_id, JOB_REQUESTED, None, false)
        .expect("to requested")
        .expect("row");
    assert_eq!(job.status, JOB_REQUESTED);
    assert!(
        job.requested_at.is_some(),
        "requested_at stamped on the transition"
    );

    // first credited frame promotes requested -> receiving
    let job = s
        .credit_frames(job.job_id, 60, 60, 1600)
        .expect("credit")
        .expect("credited");
    assert_eq!(job.status, JOB_RECEIVING);
    assert_eq!(job.samples_received, 60);
    assert_eq!(job.last_sample_at, Some(1600));

    // Replayed frame: 60 seen, 0 inserted -> no double-count, and the
    // clock does not move either. This assertion used to read
    // `Some(1700)`: `last_sample_at` was stamped on `points_seen`, so a
    // frame that inserted nothing still looked like progress. It is
    // stamped on `points_inserted` now (plan/portal/mqtt/design.md §6.2) —
    // rumqttc replays unacked publishes on reconnect regardless of
    // `clean_session`, so this frame is a duplicate, not new data.
    let job = s
        .credit_frames(job.job_id, 60, 0, 1700)
        .expect("credit replay")
        .expect("credited");
    assert_eq!(job.samples_received, 60, "dedup'd frames credit nothing");
    assert_eq!(
        job.last_sample_at,
        Some(1600),
        "a frame that inserted nothing does not advance the clock"
    );

    let job = s.increment_retry(job.job_id).expect("retry").expect("row");
    assert_eq!(job.retry_count, 1);

    let job = s
        .update_job_status(job.job_id, JOB_DONE, None, false)
        .expect("to done")
        .expect("row");
    assert_eq!(job.status, JOB_DONE);
    assert!(
        s.credit_frames(job.job_id, 5, 5, 1900)
            .expect("credit done")
            .is_none(),
        "terminal jobs must not be credited"
    );
}

#[test]
fn backfill_cancel_only_from_pending() {
    let (_dir, s) = open();
    let pending = s.create_job(&new_job(1000, 2000)).expect("create");
    assert!(s.cancel_job(pending.job_id).expect("cancel pending"));
    assert!(!s.cancel_job(pending.job_id).expect("already cancelled"));

    let requested = s.create_job(&new_job(3000, 4000)).expect("create 2nd");
    s.update_job_status(requested.job_id, JOB_REQUESTED, None, false)
        .expect("to requested");
    assert!(
        !s.cancel_job(requested.job_id).expect("cancel requested"),
        "only pending is freely cancellable"
    );

    assert!(!s.cancel_job(9999).expect("missing job"));
}

#[test]
fn backfill_refusal_and_failure_metadata() {
    let (_dir, s) = open();
    let job = s.create_job(&new_job(1000, 2000)).expect("create");
    let job = s
        .update_job_status(job.job_id, "failed", Some("history_unavailable"), true)
        .expect("fail")
        .expect("row");
    assert_eq!(job.status, "failed");
    assert_eq!(job.error_message.as_deref(), Some("history_unavailable"));
    assert!(job.device_refused, "window permanently unserveable");
}

#[test]
fn backfill_overlap_detection_ignores_terminal_jobs() {
    let (_dir, s) = open();
    let active = s.create_job(&new_job(1000, 2000)).expect("active job");
    let cancelled = s.create_job(&new_job(5000, 6000)).expect("to cancel");
    assert!(s.cancel_job(cancelled.job_id).expect("cancel"));

    // overlapping window -> 409 material
    let hits = s
        .find_overlapping("70b3d5e050011234", 1500, 2500)
        .expect("overlap");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].job_id, active.job_id);

    // disjoint window
    assert!(s
        .find_overlapping("70b3d5e050011234", 2500, 3000)
        .expect("disjoint")
        .is_empty());

    // cancelled job's window is free again
    assert!(s
        .find_overlapping("70b3d5e050011234", 5000, 6000)
        .expect("cancelled window")
        .is_empty());

    // other node unaffected
    assert!(s
        .find_overlapping("70b3d5e050019999", 1000, 2000)
        .expect("other node")
        .is_empty());
}

#[test]
fn backfill_list_and_active_jobs() {
    let (_dir, s) = open();
    let a = s.create_job(&new_job(1000, 2000)).expect("a");
    let b = s.create_job(&new_job(3000, 4000)).expect("b");
    s.update_job_status(a.job_id, JOB_REQUESTED, None, false)
        .expect("a requested");
    let c = s.create_job(&new_job(5000, 6000)).expect("c");
    s.update_job_status(c.job_id, JOB_DONE, None, false)
        .expect("c done");

    let listed = s.list_jobs("70b3d5e050011234", None).expect("list");
    assert_eq!(
        listed.iter().map(|j| j.job_id).collect::<Vec<_>>(),
        vec![c.job_id, b.job_id, a.job_id],
        "newest first"
    );
    assert_eq!(
        s.list_jobs("70b3d5e050011234", Some(2))
            .expect("limited")
            .len(),
        2
    );

    let active = s.active_jobs("hub-1").expect("active");
    assert_eq!(
        active.iter().map(|j| j.job_id).collect::<Vec<_>>(),
        vec![a.job_id, b.job_id],
        "done job excluded, oldest first"
    );

    assert!(s.get_job(a.job_id).expect("get").is_some());
    assert!(s.get_job(9999).expect("get missing").is_none());
}

// --- removal ----------------------------------------------------------------

#[test]
fn removing_a_device_deletes_every_threshold_of_that_entity_only() {
    let (_dir, s) = open();
    let row = |entity: &str, field: &str| NewFieldThreshold {
        hostname: "hub-1".into(),
        scope: portal_storage::THRESHOLD_SCOPE_EYE.into(),
        entity: entity.into(),
        field: field.into(),
        critical_low: Some(10.0),
        warning_low: Some(20.0),
        warning_high: Some(80.0),
        critical_high: Some(90.0),
        enabled: true,
    };
    for (entity, field) in [
        ("7C:D9:F4:12:50:88", "humidity"),
        ("7C:D9:F4:12:50:88", "temperature"),
        ("7C:D9:F4:10:A8:E2", "humidity"),
    ] {
        s.upsert_field_threshold(&row(entity, field), Some("admin"))
            .expect("upsert");
    }

    let gone = s
        .delete_field_thresholds_for_entity(
            "hub-1",
            portal_storage::THRESHOLD_SCOPE_EYE,
            "7C:D9:F4:12:50:88",
        )
        .expect("delete");
    assert_eq!(gone, 2);
    assert!(s
        .list_field_thresholds(
            "hub-1",
            portal_storage::THRESHOLD_SCOPE_EYE,
            "7C:D9:F4:12:50:88"
        )
        .expect("list")
        .is_empty());
    assert_eq!(
        s.list_field_thresholds(
            "hub-1",
            portal_storage::THRESHOLD_SCOPE_EYE,
            "7C:D9:F4:10:A8:E2"
        )
        .expect("list")
        .len(),
        1,
        "another Beacon keeps its bounds"
    );
    assert!(
        !s.threshold_history(
            "hub-1",
            portal_storage::THRESHOLD_SCOPE_EYE,
            "7C:D9:F4:12:50:88",
            None
        )
        .expect("history")
        .is_empty(),
        "what was set stays on record"
    );
}

// --- default bands and the battery (plan/portal/threshold_defaults.md) --------

fn node_row(entity: &str, field: &str, low: f64, enabled: bool) -> NewFieldThreshold {
    NewFieldThreshold {
        hostname: "hub-1".into(),
        scope: THRESHOLD_SCOPE_NODE.into(),
        entity: entity.into(),
        field: field.into(),
        critical_low: Some(low),
        warning_low: None,
        warning_high: None,
        critical_high: Some(low + 50.0),
        enabled,
    }
}

/// §3: a default fills only a field with no row, in any state, and says who wrote it.
#[test]
fn default_bands_fill_only_the_fields_without_a_row() {
    let (_dir, s) = open();
    s.upsert_field_threshold(&node_row("aa11", "temperature", 5.0, true), Some("admin"))
        .expect("an operator's band");
    s.upsert_field_threshold(&node_row("aa11", "humidity", 20.0, false), Some("admin"))
        .expect("a band switched off");

    let defaults = [
        node_row("aa11", "temperature", -10.0, false),
        node_row("aa11", "humidity", 0.0, false),
        node_row("aa11", "w1_1_temperature", -10.0, false),
    ];
    let inserted = s
        .insert_missing_field_thresholds(&defaults, "system:defaults")
        .expect("seed");
    assert_eq!(inserted, ["w1_1_temperature"]);

    let rows = s
        .list_field_thresholds("hub-1", THRESHOLD_SCOPE_NODE, "aa11")
        .expect("list");
    let get = |f: &str| rows.iter().find(|r| r.field == f).expect(f).clone();
    assert_eq!(get("temperature").critical_low, Some(5.0));
    assert!(get("temperature").enabled);
    assert_eq!(get("humidity").critical_low, Some(20.0));
    assert!(!get("humidity").enabled);
    assert_eq!(get("w1_1_temperature").critical_low, Some(-10.0));
    assert!(!get("w1_1_temperature").enabled, "a default is stored off");

    let history = s
        .threshold_history(
            "hub-1",
            THRESHOLD_SCOPE_NODE,
            "aa11",
            Some("w1_1_temperature"),
        )
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].actor.as_deref(), Some("system:defaults"));

    // Again: nothing more, no history either.
    assert!(s
        .insert_missing_field_thresholds(&defaults, "system:defaults")
        .expect("seed again")
        .is_empty());
    assert_eq!(
        s.threshold_history("hub-1", THRESHOLD_SCOPE_NODE, "aa11", None)
            .expect("history")
            .len(),
        3
    );
}

/// §5: a field that stopped being thresholdable goes from every Node and Hub, and only it.
#[test]
fn a_named_field_is_deleted_everywhere_and_nothing_else() {
    let (_dir, s) = open();
    for (entity, field) in [
        ("aa11", "voltage"),
        ("aa11", "temperature"),
        ("bb22", "voltage"),
    ] {
        s.upsert_field_threshold(&node_row(entity, field, 2.0, true), Some("admin"))
            .expect("upsert");
    }
    let mut other_hub = node_row("cc33", "voltage", 2.0, true);
    other_hub.hostname = "hub-2".into();
    s.upsert_field_threshold(&other_hub, Some("admin"))
        .expect("upsert");

    let gone = s
        .delete_field_thresholds_named(THRESHOLD_SCOPE_NODE, "voltage")
        .expect("delete");
    assert_eq!(
        gone,
        [
            ("hub-1".to_string(), "aa11".to_string()),
            ("hub-1".to_string(), "bb22".to_string()),
            ("hub-2".to_string(), "cc33".to_string()),
        ]
    );
    let left = s
        .list_field_thresholds("hub-1", THRESHOLD_SCOPE_NODE, "aa11")
        .expect("list");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].field, "temperature");
    assert!(s
        .delete_field_thresholds_named(THRESHOLD_SCOPE_NODE, "voltage")
        .expect("again")
        .is_empty());
}

/// The Channels card's "Received" column: each field's newest stored time, and the
/// newest raw uplink of the Node, nothing from another Node.
#[test]
fn latest_field_times_and_the_newest_raw_message() {
    let (_dir, s) = open();
    let node = "70b3d5e050011234";
    assert!(s
        .latest_node_field_times("hub-1", node)
        .expect("q")
        .is_empty());
    assert_eq!(s.latest_raw_message_at("hub-1", node).expect("q"), None);
    for (ts, v) in [(1000, 20.0), (1120, 21.0)] {
        s.insert_field_reading(&field_row(ts, v, SOURCE_LIVE))
            .expect("insert");
    }
    s.insert_field_reading(&NodeFieldReading {
        node_key: "70b3d5e050019999".into(),
        ..field_row(5000, 1.0, SOURCE_LIVE)
    })
    .expect("insert");
    assert_eq!(
        s.latest_node_field_times("hub-1", node).expect("q"),
        [("temperature".to_string(), 1120)]
    );
    s.insert_raw_message("hub-1", node, 1500, r#"{"fport":2}"#)
        .expect("raw");
    s.insert_raw_message("hub-1", node, 1400, r#"{"fport":3}"#)
        .expect("raw");
    assert_eq!(
        s.latest_raw_message_at("hub-1", node).expect("q"),
        Some(1500)
    );
}
