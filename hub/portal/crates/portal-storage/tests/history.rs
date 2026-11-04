use portal_storage::{HourlyBucket, MinuteReading, RawReading, Storage, SOURCE_LIVE};
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open+migrate");
    (dir, storage)
}

fn minute(
    hostname: &str,
    sensor_line: i64,
    minute_ts: i64,
    min_c: Option<f64>,
    avg_c: Option<f64>,
    max_c: Option<f64>,
    sample_count: Option<i64>,
) -> MinuteReading {
    MinuteReading {
        hostname: hostname.into(),
        sensor_line,
        minute_ts,
        min_c,
        avg_c,
        max_c,
        sample_count,
        disconnect_count: Some(0),
        worst_alarm: None,
        source: SOURCE_LIVE.into(),
        data_hmac: None,
    }
}

fn raw(hostname: &str, sensor_line: i64, ts: i64, avg_c: f64) -> RawReading {
    RawReading {
        hostname: hostname.into(),
        sensor_line,
        ts,
        min_c: Some(avg_c - 0.1),
        avg_c: Some(avg_c),
        max_c: Some(avg_c + 0.1),
        sample_count: Some(1),
    }
}

const HOUR0: i64 = 0;
const HOUR1: i64 = 3_600;
const HOUR2: i64 = 7_200;
const HOUR3: i64 = 10_800;

/// Seeds `hub-1`/line 1 across 3 hours with data crafted for the hourly
/// bucketing math (weighted average, NULL sample_count fallback, a
/// fully-NULL minute), plus `hub-1`/line 2 and a different hostname as
/// isolation controls.
fn seed(storage: &Storage) {
    // Hour 0, line 1: a normal weighted-average case + one fully-NULL
    // minute (e.g. a disconnected sensor) that MIN/MAX/weighting must skip.
    storage
        .upsert_minute(&minute(
            "hub-1",
            1,
            HOUR0,
            Some(9.5),
            Some(10.0),
            Some(10.5),
            Some(10),
        ))
        .expect("seed");
    storage
        .upsert_minute(&minute(
            "hub-1",
            1,
            HOUR0 + 60,
            Some(19.0),
            Some(20.0),
            Some(21.0),
            Some(30),
        ))
        .expect("seed");
    storage
        .upsert_minute(&minute("hub-1", 1, HOUR0 + 120, None, None, None, None))
        .expect("seed");

    // Hour 1, line 1: both minutes have NULL sample_count -> weighted term
    // is NULL -> must fall back to a plain AVG(avg_c).
    storage
        .upsert_minute(&minute(
            "hub-1",
            1,
            HOUR1,
            Some(4.5),
            Some(5.0),
            Some(5.5),
            None,
        ))
        .expect("seed");
    storage
        .upsert_minute(&minute(
            "hub-1",
            1,
            HOUR1 + 60,
            Some(14.5),
            Some(15.0),
            Some(15.5),
            None,
        ))
        .expect("seed");

    // Hour 2, line 1: single minute.
    storage
        .upsert_minute(&minute(
            "hub-1",
            1,
            HOUR2,
            Some(29.0),
            Some(30.0),
            Some(31.0),
            Some(5),
        ))
        .expect("seed");

    // Line 2 and a different hostname: isolation controls, spanning the
    // same 3-hour window so a broken WHERE clause would leak them in.
    storage
        .upsert_minute(&minute(
            "hub-1",
            2,
            HOUR0,
            Some(99.0),
            Some(100.0),
            Some(101.0),
            Some(1),
        ))
        .expect("seed");
    storage
        .upsert_minute(&minute(
            "hub-1",
            2,
            HOUR2,
            Some(199.0),
            Some(200.0),
            Some(201.0),
            Some(1),
        ))
        .expect("seed");
    storage
        .upsert_minute(&minute(
            "hub-2",
            1,
            HOUR0,
            Some(-1.0),
            Some(-1.0),
            Some(-1.0),
            Some(1),
        ))
        .expect("seed");

    // Raw readings, line 1, spanning the same window, plus a line-2 control.
    storage
        .insert_raw(&raw("hub-1", 1, 10, 10.0))
        .expect("seed");
    storage
        .insert_raw(&raw("hub-1", 1, 70, 20.0))
        .expect("seed");
    storage
        .insert_raw(&raw("hub-1", 1, HOUR1 + 10, 5.0))
        .expect("seed");
    storage
        .insert_raw(&raw("hub-1", 1, HOUR2 + 10, 30.0))
        .expect("seed");
    storage
        .insert_raw(&raw("hub-1", 2, 10, 999.0))
        .expect("seed");
}

#[test]
fn query_minutes_range_is_inclusive_from_exclusive_to_and_ordered() {
    let (_dir, storage) = open();
    seed(&storage);

    // [HOUR0, HOUR2) must include HOUR0, HOUR0+60, HOUR0+120, HOUR1, HOUR1+60
    // but exclude HOUR2 itself.
    let rows = storage
        .query_minutes("hub-1", 1, HOUR0, HOUR2)
        .expect("query");
    let timestamps: Vec<i64> = rows.iter().map(|r| r.minute_ts).collect();
    assert_eq!(
        timestamps,
        vec![HOUR0, HOUR0 + 60, HOUR0 + 120, HOUR1, HOUR1 + 60]
    );

    // Narrowing the lower bound to HOUR0+60 excludes HOUR0 (inclusive lower
    // bound is exact, not "greater than").
    let rows = storage
        .query_minutes("hub-1", 1, HOUR0 + 60, HOUR2)
        .expect("query");
    assert_eq!(rows[0].minute_ts, HOUR0 + 60);
    assert_eq!(rows.len(), 4);

    // Narrowing the upper bound to HOUR0+120 excludes that exact minute
    // (exclusive upper bound).
    let rows = storage
        .query_minutes("hub-1", 1, HOUR0, HOUR0 + 120)
        .expect("query");
    let timestamps: Vec<i64> = rows.iter().map(|r| r.minute_ts).collect();
    assert_eq!(timestamps, vec![HOUR0, HOUR0 + 60]);

    // Isolation: line 2 and other hostnames never leak in.
    let rows = storage
        .query_minutes("hub-1", 1, HOUR0, HOUR3)
        .expect("query");
    assert!(rows.iter().all(|r| r.sensor_line == 1));
    assert!(rows.iter().all(|r| r.hostname == "hub-1"));
}

#[test]
fn query_raw_range_is_inclusive_from_exclusive_to_and_ordered() {
    let (_dir, storage) = open();
    seed(&storage);

    // Seeded raw ts: 10, 70, HOUR1+10, HOUR2+10.
    let rows = storage
        .query_raw("hub-1", 1, 10, HOUR1 + 10)
        .expect("query");
    let timestamps: Vec<i64> = rows.iter().map(|r| r.ts).collect();
    // ts=10 included (inclusive from), ts=HOUR1+10 excluded (exclusive to).
    assert_eq!(timestamps, vec![10, 70]);

    let rows = storage
        .query_raw("hub-1", 1, 70, HOUR1 + 10 + 1)
        .expect("query");
    let timestamps: Vec<i64> = rows.iter().map(|r| r.ts).collect();
    assert_eq!(timestamps, vec![70, HOUR1 + 10]);

    assert!(storage
        .query_raw("hub-1", 2, i64::MIN, i64::MAX)
        .expect("query")
        .iter()
        .all(|r| r.sensor_line == 2));
}

#[test]
fn query_hourly_bucketing_math() {
    let (_dir, storage) = open();
    seed(&storage);

    let buckets = storage
        .query_hourly("hub-1", 1, HOUR0, HOUR3)
        .expect("query");
    assert_eq!(buckets.len(), 3);

    // Hour 0: weighted average over the two non-NULL minutes, hand-computed
    // as (10.0*10 + 20.0*30) / (10+30) = 700/40 = 17.5. The fully-NULL
    // minute contributes nothing to MIN/MAX/weighting.
    assert_eq!(
        buckets[0],
        HourlyBucket {
            hour_ts: HOUR0,
            min_c: Some(9.5),
            avg_c: Some(17.5),
            max_c: Some(21.0),
            sample_count: Some(40),
        }
    );

    // Hour 1: both minutes have NULL sample_count, so SUM(sample_count) is
    // NULL and the weighted term must fall back to a plain AVG(avg_c) =
    // (5.0 + 15.0) / 2 = 10.0. sample_count stays NULL (nothing to sum).
    assert_eq!(
        buckets[1],
        HourlyBucket {
            hour_ts: HOUR1,
            min_c: Some(4.5),
            avg_c: Some(10.0),
            max_c: Some(15.5),
            sample_count: None,
        }
    );

    // Hour 2: single minute, weighted average degenerates to avg_c itself.
    assert_eq!(
        buckets[2],
        HourlyBucket {
            hour_ts: HOUR2,
            min_c: Some(29.0),
            avg_c: Some(30.0),
            max_c: Some(31.0),
            sample_count: Some(5),
        }
    );
}

#[test]
fn query_hourly_ordered_and_isolated_by_line_and_hostname() {
    let (_dir, storage) = open();
    seed(&storage);

    let buckets = storage
        .query_hourly("hub-1", 1, HOUR0, HOUR3)
        .expect("query");
    let hours: Vec<i64> = buckets.iter().map(|b| b.hour_ts).collect();
    assert_eq!(hours, vec![HOUR0, HOUR1, HOUR2]);

    let line2 = storage
        .query_hourly("hub-1", 2, HOUR0, HOUR3)
        .expect("query");
    assert_eq!(line2.len(), 2);
    assert_eq!(line2[0].avg_c, Some(100.0));
    assert_eq!(line2[1].avg_c, Some(200.0));
}

#[test]
fn minute_extent_bounds_and_none_when_empty() {
    let (_dir, storage) = open();
    seed(&storage);

    assert_eq!(
        storage.minute_extent("hub-1", 1).expect("extent"),
        Some((HOUR0, HOUR2))
    );
    assert_eq!(
        storage.minute_extent("hub-1", 2).expect("extent"),
        Some((HOUR0, HOUR2))
    );
    assert_eq!(
        storage.minute_extent("hub-1", 99).expect("extent, no rows"),
        None
    );
}

/// The composite primary key `(hostname, sensor_line, minute_ts)` must serve
/// `query_minutes`'s predicate + ordering as a left-prefix index — no full
/// table scan. Storage doesn't expose its raw connection, so this opens a
/// second (plain, unencrypted) connection onto the same dev-mode db file to
/// run `EXPLAIN QUERY PLAN` against the identical predicate shape.
#[test]
fn query_minutes_uses_primary_key_index_not_a_full_scan() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open+migrate");
    seed(&storage);
    drop(storage); // release the WAL writer before opening a second handle

    let conn = rusqlite::Connection::open(&path).expect("open plain conn for EXPLAIN");
    let mut stmt = conn
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT hostname, sensor_line, minute_ts, min_c, avg_c, max_c,
                    sample_count, disconnect_count, worst_alarm, source, data_hmac
             FROM readings_minute
             WHERE hostname = ?1 AND sensor_line = ?2
               AND minute_ts >= ?3 AND minute_ts < ?4
             ORDER BY minute_ts ASC",
        )
        .expect("prepare explain");
    let plan: Vec<String> = stmt
        .query_map(rusqlite::params!["hub-1", 1i64, HOUR0, HOUR3], |r| {
            r.get::<_, String>(3) // "detail" column
        })
        .expect("query plan")
        .collect::<std::result::Result<_, _>>()
        .expect("collect plan rows");
    let plan_text = plan.join(" / ");

    assert!(
        plan_text.contains("USING INDEX") || plan_text.contains("USING PRIMARY KEY"),
        "expected the primary key index to serve the range query, got: {plan_text}"
    );
    assert!(
        !plan_text
            .to_uppercase()
            .contains("SCAN TABLE READINGS_MINUTE")
            && !plan_text.contains("SCAN readings_minute USING INTEGER PRIMARY KEY"),
        "expected an indexed SEARCH, not a full table scan, got: {plan_text}"
    );
}
