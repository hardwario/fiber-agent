//! `summarize_minutes` / `summarize_node_field` / `summarize_eye_field` —
//! the probe-detail window's aggregate line (MB3, monitoring.backend.md §6
//! WP3.1).
//!
//! Every assertion here is against a **hand-computed control set**, which
//! is what §11 asks for: an aggregate that agrees with itself proves
//! nothing, and the median in particular has two cases (odd and even) that
//! an off-by-one in the `ROW_NUMBER` offset gets exactly half right.

use portal_storage::{EyeField, EyeReading, MinuteReading, NodeFieldReading, Storage, SOURCE_LIVE};
use tempfile::tempdir;

const HOST: &str = "hub-1";
const LINE: i64 = 1;
const NODE: &str = "n1";
const MAC: &str = "AA:BB:CC:DD:EE:01";

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn minute(ts: i64, avg_c: Option<f64>) -> MinuteReading {
    MinuteReading {
        hostname: HOST.into(),
        sensor_line: LINE,
        minute_ts: ts,
        // Deliberately wide of `avg_c`: the wired summary is over `avg_c`
        // alone (decision 2026-09-08), so a min/max leaking into the
        // statistics would show up here as -100/+100.
        min_c: Some(-100.0),
        avg_c,
        max_c: Some(100.0),
        sample_count: Some(60),
        disconnect_count: Some(0),
        worst_alarm: None,
        source: SOURCE_LIVE.into(),
        data_hmac: None,
    }
}

fn node_row(ts: i64, value: Option<f64>) -> NodeFieldReading {
    NodeFieldReading {
        hostname: HOST.into(),
        node_key: NODE.into(),
        field: "temperature".into(),
        ts,
        value,
        alarm_state: None,
        source: SOURCE_LIVE.into(),
    }
}

fn eye_row(ts: i64, t: Option<f64>, h: Option<i64>) -> EyeReading {
    EyeReading {
        hostname: HOST.into(),
        mac: MAC.into(),
        ts,
        temperature_c: t,
        humidity_pct: h,
        event_type: "live".into(),
        message_id: Some(format!("m-{ts}")),
    }
}

/// Seeds the same control values into all three tables, so a difference in
/// an answer is a difference in the SQL and not in the data.
fn seed_all(storage: &Storage, values: &[f64]) {
    for (i, v) in values.iter().enumerate() {
        let ts = 1_000 + i as i64 * 60;
        storage
            .upsert_minute(&minute(ts, Some(*v)))
            .expect("minute");
        storage
            .insert_field_reading(&node_row(ts, Some(*v)))
            .expect("node reading");
        storage
            .insert_eye_reading(&eye_row(ts, Some(*v), Some(*v as i64)))
            .expect("eye reading");
    }
}

fn wide_window() -> (i64, i64) {
    (0, 1_000_000)
}

#[test]
fn median_of_an_odd_window_is_the_middle_sample() {
    let (_dir, storage) = open();
    // Sorted: 1, 2, 3, 10, 20 -> middle is 3, mean is 7.2. The control set
    // is deliberately skewed: the old one (8, 1, 5, 9, 2) had median and
    // mean both equal to 5, so it could not have told the two apart — a
    // `summarize_minutes` that returned the average would have passed
    // (review item 19, 2026-09-10). Inserted out of order on purpose too:
    // the median is an ordered offset, not "the third row stored".
    seed_all(&storage, &[10.0, 1.0, 3.0, 20.0, 2.0]);
    let (from, to) = wide_window();

    let s = storage
        .summarize_minutes(HOST, LINE, from, to)
        .expect("summary");
    assert_eq!(s.count, 5);
    assert_eq!(s.median, Some(3.0));
    assert_ne!(s.median, s.avg, "the median is not the mean");
    assert_eq!(s.min, Some(1.0));
    assert_eq!(s.max, Some(20.0));
    assert_eq!(s.avg, Some(7.2)); // (10+1+3+20+2)/5
}

#[test]
fn median_of_an_even_window_is_the_mean_of_the_two_middles() {
    let (_dir, storage) = open();
    // Sorted: 1, 2, 4, 9 -> the two middles are 2 and 4, so 3.
    seed_all(&storage, &[9.0, 1.0, 4.0, 2.0]);
    let (from, to) = wide_window();

    let s = storage
        .summarize_minutes(HOST, LINE, from, to)
        .expect("summary");
    assert_eq!(s.count, 4);
    assert_eq!(s.median, Some(3.0));
    assert_eq!(s.avg, Some(4.0)); // (9+1+4+2)/4
}

#[test]
fn an_empty_window_is_zero_count_and_no_statistics() {
    let (_dir, storage) = open();
    seed_all(&storage, &[1.0, 2.0, 3.0]);

    // A window entirely before the seeded range.
    let s = storage
        .summarize_minutes(HOST, LINE, 0, 500)
        .expect("summary");
    assert_eq!(s.count, 0);
    assert_eq!((s.min, s.max, s.avg, s.median), (None, None, None, None));

    // And a probe that has never stored anything at all.
    let other = storage
        .summarize_minutes(HOST, 7, 0, 1_000_000)
        .expect("summary");
    assert_eq!(other.count, 0);
    assert_eq!(other.median, None);
}

#[test]
fn a_single_sample_is_its_own_min_max_avg_and_median() {
    let (_dir, storage) = open();
    seed_all(&storage, &[21.5]);
    let (from, to) = wide_window();

    let s = storage
        .summarize_minutes(HOST, LINE, from, to)
        .expect("summary");
    assert_eq!(s.count, 1);
    assert_eq!(
        (s.min, s.max, s.avg, s.median),
        (Some(21.5), Some(21.5), Some(21.5), Some(21.5))
    );
}

#[test]
fn null_values_are_excluded_from_every_statistic() {
    let (_dir, storage) = open();
    // Three real values and two NULL rows. A NULL is a stored row with no
    // reading; counting it would make `avg` disagree with `count * mean`,
    // and would shift the median by half a row.
    for (i, v) in [Some(2.0), None, Some(4.0), None, Some(6.0)]
        .into_iter()
        .enumerate()
    {
        let ts = 1_000 + i as i64 * 60;
        storage.upsert_minute(&minute(ts, v)).expect("minute");
        storage
            .insert_field_reading(&node_row(ts, v))
            .expect("node reading");
        storage
            .insert_eye_reading(&eye_row(ts, v, v.map(|v| v as i64)))
            .expect("eye reading");
    }
    let (from, to) = wide_window();

    for s in [
        storage
            .summarize_minutes(HOST, LINE, from, to)
            .expect("wired"),
        storage
            .summarize_node_field(HOST, NODE, "temperature", from, to)
            .expect("node"),
        storage
            .summarize_eye_field(HOST, MAC, EyeField::Temperature, from, to)
            .expect("beacon"),
    ] {
        assert_eq!(s.count, 3, "the two NULL rows are not samples");
        assert_eq!(s.min, Some(2.0));
        assert_eq!(s.max, Some(6.0));
        assert_eq!(s.avg, Some(4.0));
        assert_eq!(s.median, Some(4.0));
    }
}

#[test]
fn the_window_is_half_open() {
    let (_dir, storage) = open();
    // One row exactly on each bound. `[from, to)` keeps the first and drops
    // the second — the same convention `query_minutes` and every other
    // reader in this crate uses, so a window's summary and its chart cover
    // the same seconds.
    for ts in [100, 200] {
        storage
            .upsert_minute(&minute(ts, Some(ts as f64)))
            .expect("minute");
        storage
            .insert_field_reading(&node_row(ts, Some(ts as f64)))
            .expect("node reading");
        storage
            .insert_eye_reading(&eye_row(ts, Some(ts as f64), Some(ts)))
            .expect("eye reading");
    }

    let wired = storage.summarize_minutes(HOST, LINE, 100, 200).expect("w");
    assert_eq!(wired.count, 1);
    assert_eq!(wired.max, Some(100.0), "the upper bound is exclusive");

    let node = storage
        .summarize_node_field(HOST, NODE, "temperature", 100, 200)
        .expect("n");
    assert_eq!(node.count, 1);
    assert_eq!(node.max, Some(100.0));

    let beacon = storage
        .summarize_eye_field(HOST, MAC, EyeField::Temperature, 100, 200)
        .expect("b");
    assert_eq!(beacon.count, 1);
    assert_eq!(beacon.max, Some(100.0));
}

#[test]
fn all_three_probe_kinds_summarise_the_same_control_set() {
    let (_dir, storage) = open();
    // Sorted: 1, 3, 4, 7, 10 -> min 1, max 10, avg 5, median 4.
    seed_all(&storage, &[7.0, 1.0, 10.0, 3.0, 4.0]);
    let (from, to) = wide_window();

    let wired = storage
        .summarize_minutes(HOST, LINE, from, to)
        .expect("wired");
    let node = storage
        .summarize_node_field(HOST, NODE, "temperature", from, to)
        .expect("node");
    let beacon = storage
        .summarize_eye_field(HOST, MAC, EyeField::Temperature, from, to)
        .expect("beacon");

    for (what, s) in [("wired", wired), ("node", node), ("beacon", beacon)] {
        assert_eq!(s.count, 5, "{what}");
        assert_eq!(s.min, Some(1.0), "{what}");
        assert_eq!(s.max, Some(10.0), "{what}");
        assert_eq!(s.avg, Some(5.0), "{what}");
        assert_eq!(s.median, Some(4.0), "{what}");
    }

    // The wired probe's `min_c`/`max_c` were seeded at -100/+100 and must
    // not have reached any statistic: all five are over `avg_c` alone.
    assert_eq!(wired.min, Some(1.0));
    assert_eq!(wired.max, Some(10.0));

    // Humidity is an INTEGER column, and the summary CASTs it to REAL: the
    // same five samples give the same five statistics as the two REAL
    // columns beside it, average included. (The comment here used to
    // promise an even-count median while asserting an odd one — review
    // item 20, 2026-09-10; the between-two-integers case is
    // `median_of_an_even_window_is_the_mean_of_the_two_middles` above.)
    let humidity = storage
        .summarize_eye_field(HOST, MAC, EyeField::Humidity, from, to)
        .expect("humidity");
    assert_eq!(humidity.count, 5);
    assert_eq!(humidity.median, Some(4.0));
    assert_eq!(humidity.avg, Some(5.0));

    // And one probe's window never picks up another's rows.
    let other_line = storage
        .summarize_minutes(HOST, 6, from, to)
        .expect("other line");
    assert_eq!(other_line.count, 0);
    let other_field = storage
        .summarize_node_field(HOST, NODE, "humidity", from, to)
        .expect("other field");
    assert_eq!(other_field.count, 0);
    let other_host = storage
        .summarize_eye_field("hub-2", MAC, EyeField::Temperature, from, to)
        .expect("other host");
    assert_eq!(other_host.count, 0);
}

/// decisions.md #24: an alarm's peak is the extreme stored value inside its
/// window, with the time it was stored, from the same series the chart draws
/// (wired `avg_c`, not `max_c`). A LOW alarm's peak is the minimum.
#[test]
fn peak_in_window_finds_the_extreme_and_when() {
    use portal_storage::PeakSource;
    let (_dir, storage) = open();
    // ts 1000, 1060, 1120, 1180, 1240
    seed_all(&storage, &[5.0, 9.0, 13.0, 7.0, 2.0]);

    for source in [
        PeakSource::Wired { line: LINE },
        PeakSource::Node {
            node_key: NODE,
            field: "temperature",
        },
        PeakSource::Eye {
            mac: MAC,
            field: EyeField::Temperature,
        },
    ] {
        assert_eq!(
            storage
                .peak_in_window(HOST, source, 1_000, 1_240, true)
                .unwrap(),
            Some((13.0, 1_120)),
            "{source:?}"
        );
        assert_eq!(
            storage
                .peak_in_window(HOST, source, 1_000, 1_241, false)
                .unwrap(),
            Some((2.0, 1_240)),
            "{source:?}, window end is exclusive"
        );
        assert_eq!(
            storage
                .peak_in_window(HOST, source, 5_000, 6_000, true)
                .unwrap(),
            None,
            "{source:?}, empty window"
        );
    }
}
