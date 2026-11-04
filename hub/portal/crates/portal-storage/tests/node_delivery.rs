//! V25 `node_threshold_delivery` (PF-3): what a Node has confirmed of its
//! alarm table, per field, and when.

use portal_storage::{
    NodeBand, Storage, DELIVERY_STATE_CONFIRMED, DELIVERY_STATE_DIFFERS, DELIVERY_STATE_PENDING,
    DELIVERY_STATE_UNDELIVERED, DELIVERY_STATE_UNKNOWN,
};
use tempfile::tempdir;

const HOST: &str = "hub-1";
const NODE: &str = "70b3d5e050011234";

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn band(field: &str, low: f64, high: f64) -> NodeBand {
    NodeBand {
        field: field.into(),
        low: Some(low),
        high: Some(high),
        enabled: true,
    }
}

fn state_of(s: &Storage, field: &str) -> Option<(String, Option<i64>, Option<f64>)> {
    s.list_node_delivery(HOST, NODE)
        .expect("list")
        .into_iter()
        .find(|d| d.field == field)
        .map(|d| (d.state, d.confirmed_at, d.confirmed_high))
}

/// Sent, not confirmed in time, confirmed late: pending, undelivered,
/// confirmed with the time of the confirmation.
#[test]
fn a_band_goes_pending_then_undelivered_then_confirmed() {
    let (_dir, s) = open();
    let sent = [band("temperature", -10.0, 30.0)];
    s.mark_node_delivery_pending(HOST, NODE, &sent, 100)
        .expect("pending");
    assert_eq!(
        state_of(&s, "temperature"),
        Some((DELIVERY_STATE_PENDING.into(), None, None))
    );
    assert_eq!(
        s.mark_node_delivery_undelivered(HOST, NODE)
            .expect("undelivered"),
        1
    );
    assert_eq!(
        state_of(&s, "temperature"),
        Some((DELIVERY_STATE_UNDELIVERED.into(), None, None))
    );
    s.confirm_node_delivery(HOST, NODE, &sent, 200)
        .expect("confirmed");
    assert_eq!(
        state_of(&s, "temperature"),
        Some((DELIVERY_STATE_CONFIRMED.into(), Some(200), Some(30.0)))
    );
}

/// A table re-sent with a band the Node already holds leaves that band
/// confirmed at its old time; a changed band gets the new time, and a field
/// the table no longer carries loses its row.
#[test]
fn a_confirmed_table_keeps_the_time_of_an_unchanged_band() {
    let (_dir, s) = open();
    s.confirm_node_delivery(
        HOST,
        NODE,
        &[
            band("temperature", -10.0, 30.0),
            band("humidity", 10.0, 90.0),
        ],
        100,
    )
    .expect("first");

    let next = [
        band("temperature", -10.0, 30.0),
        band("pressure", 900.0, 1100.0),
    ];
    s.mark_node_delivery_pending(HOST, NODE, &next, 150)
        .expect("pending");
    assert_eq!(
        state_of(&s, "temperature").map(|t| t.0).as_deref(),
        Some(DELIVERY_STATE_CONFIRMED),
        "unchanged, so nothing is on its way for it"
    );
    s.confirm_node_delivery(HOST, NODE, &next, 200)
        .expect("second");
    assert_eq!(state_of(&s, "temperature").and_then(|t| t.1), Some(100));
    assert_eq!(state_of(&s, "pressure").and_then(|t| t.1), Some(200));
    assert_eq!(state_of(&s, "humidity"), None, "its slot was cleared");
}

/// A discarded send: the Node holds what it last confirmed, and a field it
/// never confirmed has no row. A restart turns a pending band undelivered.
#[test]
fn a_discard_restores_the_last_confirmation_and_a_restart_ends_pending() {
    let (_dir, s) = open();
    s.confirm_node_delivery(HOST, NODE, &[band("temperature", -10.0, 25.0)], 100)
        .expect("confirmed");
    let sent = [
        band("temperature", -10.0, 30.0),
        band("humidity", 10.0, 90.0),
    ];
    s.mark_node_delivery_pending(HOST, NODE, &sent, 150)
        .expect("pending");
    assert_eq!(s.reset_node_delivery_pending().expect("reset"), 2);
    assert_eq!(
        state_of(&s, "humidity").map(|t| t.0).as_deref(),
        Some(DELIVERY_STATE_UNDELIVERED)
    );

    s.forget_node_delivery_attempt(HOST, NODE).expect("discard");
    assert_eq!(
        state_of(&s, "temperature"),
        Some((DELIVERY_STATE_CONFIRMED.into(), Some(100), Some(25.0)))
    );
    assert_eq!(state_of(&s, "humidity"), None);

    assert_eq!(s.delete_node_delivery(HOST, NODE).expect("removed"), 1);
    assert!(s.list_node_delivery(HOST, NODE).expect("list").is_empty());
}

/// V26, PF-3b: a Discard that took back a frame the Node may have had leaves the band
/// `unknown` — the Discard's own forget does not touch it — and the Node's late answer turns it
/// into `differs` (or `confirmed`, where the portal holds that very band); a refusal puts back
/// the last confirmation.
#[test]
fn a_discarded_frame_leaves_the_band_unknown_until_the_node_answers() {
    let (_dir, s) = open();
    s.confirm_node_delivery(HOST, NODE, &[band("temperature", -10.0, 22.0)], 100)
        .expect("confirmed");
    s.mark_node_delivery_pending(
        HOST,
        NODE,
        &[
            band("temperature", -10.0, 23.0),
            band("humidity", 10.0, 90.0),
        ],
        150,
    )
    .expect("pending");
    assert_eq!(
        s.mark_node_delivery_unknown(HOST, NODE, None)
            .expect("unknown"),
        2
    );
    s.forget_node_delivery_attempt(HOST, NODE).expect("discard");
    assert_eq!(
        state_of(&s, "temperature"),
        Some((DELIVERY_STATE_UNKNOWN.into(), Some(100), Some(22.0))),
        "not \"yes, 100\": the Node may hold 23"
    );

    let differs = s
        .node_took_discarded(HOST, NODE, 200, &[band("humidity", 10.0, 90.0)])
        .expect("late answer");
    assert_eq!(differs, vec![band("temperature", -10.0, 23.0)]);
    assert_eq!(
        state_of(&s, "temperature"),
        Some((DELIVERY_STATE_DIFFERS.into(), Some(200), Some(23.0)))
    );
    assert_eq!(
        state_of(&s, "humidity").map(|t| t.0).as_deref(),
        Some(DELIVERY_STATE_CONFIRMED),
        "the portal holds that band too"
    );

    // A refusal instead.
    s.mark_node_delivery_pending(HOST, NODE, &[band("humidity", 10.0, 95.0)], 300)
        .expect("pending");
    s.mark_node_delivery_unknown(HOST, NODE, Some(&["humidity".to_string()]))
        .expect("unknown");
    s.forget_node_delivery_unknown(HOST, NODE).expect("refused");
    assert_eq!(
        state_of(&s, "humidity"),
        Some((DELIVERY_STATE_CONFIRMED.into(), Some(200), Some(90.0)))
    );
    let _ = (DELIVERY_STATE_PENDING, DELIVERY_STATE_UNDELIVERED);
}
