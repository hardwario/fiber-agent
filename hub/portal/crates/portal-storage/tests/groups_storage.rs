//! Sensor groups: CRUD and the membership rules the monitoring page relies on.

use portal_storage::{MoveDirection, NewSensorGroupMember, Storage};
use tempfile::tempdir;

fn open_storage() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");
    (dir, storage)
}

fn probe(line: i64) -> NewSensorGroupMember {
    NewSensorGroupMember {
        hostname: "hub".into(),
        sensor_line: Some(line),
        node_key: None,
        mac: None,
        fields: None,
    }
}

#[test]
fn create_keeps_members_in_submitted_order() {
    let (_dir, s) = open_storage();
    let id = s
        .create_sensor_group("Cold Storage", Some("freezers"), &[probe(3), probe(1)])
        .expect("create");

    let members = s.list_group_members(id).expect("members");
    assert_eq!(members.len(), 2);
    // sort_order follows the submitted order, not the line number.
    assert_eq!(members[0].sensor_line, Some(3));
    assert_eq!(members[1].sensor_line, Some(1));
}

/// The dialog submits a group's whole membership on every save, so update is
/// a replace. An update that merged instead would make removing a sensor
/// impossible from the UI.
#[test]
fn update_replaces_membership_wholesale() {
    let (_dir, s) = open_storage();
    let id = s
        .create_sensor_group("A", None, &[probe(0), probe(1)])
        .expect("create");
    assert!(s
        .update_sensor_group(id, "B", Some("desc"), &[probe(7)])
        .expect("update"));

    let group = s.get_sensor_group(id).expect("get").expect("exists");
    assert_eq!(group.name, "B");
    assert_eq!(group.description.as_deref(), Some("desc"));

    let members = s.list_group_members(id).expect("members");
    assert_eq!(members.len(), 1, "the previous two are gone, not merged");
    assert_eq!(members[0].sensor_line, Some(7));
}

#[test]
fn updating_a_missing_group_reports_it_rather_than_creating_one() {
    let (_dir, s) = open_storage();
    assert!(!s
        .update_sensor_group(404, "ghost", None, &[])
        .expect("update"));
    assert!(s.list_sensor_groups().expect("list").is_empty());
}

/// The unique index COALESCEs the three identity columns, so NULLs compare
/// equal and a repeated probe collapses instead of failing the whole save.
#[test]
fn a_duplicate_member_is_ignored_not_an_error() {
    let (_dir, s) = open_storage();
    let id = s
        .create_sensor_group("A", None, &[probe(0), probe(0)])
        .expect("create");
    assert_eq!(s.list_group_members(id).expect("members").len(), 1);
}

#[test]
fn deleting_a_group_takes_its_members_with_it() {
    let (_dir, s) = open_storage();
    let id = s
        .create_sensor_group("A", None, &[probe(0)])
        .expect("create");

    assert!(s.delete_sensor_group(id).expect("delete"));
    assert!(s.get_sensor_group(id).expect("get").is_none());
    assert!(
        s.list_all_group_members().expect("members").is_empty(),
        "ON DELETE CASCADE — foreign_keys is on in Storage::open"
    );
    assert!(!s.delete_sensor_group(id).expect("delete again"));
}

#[test]
fn node_member_round_trips_its_field_selection() {
    let (_dir, s) = open_storage();
    let id = s
        .create_sensor_group(
            "A",
            None,
            &[NewSensorGroupMember {
                hostname: "hub".into(),
                sensor_line: None,
                node_key: Some("aabbccdd00000001".into()),
                mac: None,
                fields: Some(vec!["temperature".into(), "humidity".into()]),
            }],
        )
        .expect("create");

    let m = &s.list_group_members(id).expect("members")[0];
    assert_eq!(m.node_key.as_deref(), Some("aabbccdd00000001"));
    assert_eq!(m.sensor_line, None);
    assert_eq!(
        m.fields.as_ref().expect("fields"),
        &vec!["temperature".to_string(), "humidity".to_string()]
    );
}

/// `None` is the "all fields" sentinel and must survive the round trip as
/// NULL, not as an empty list (which would render a member with no cards).
#[test]
fn absent_field_selection_stays_absent() {
    let (_dir, s) = open_storage();
    let id = s
        .create_sensor_group(
            "A",
            None,
            &[NewSensorGroupMember {
                hostname: "hub".into(),
                sensor_line: None,
                node_key: None,
                mac: Some("AA:BB:CC:DD:EE:01".into()),
                fields: None,
            }],
        )
        .expect("create");
    assert_eq!(s.list_group_members(id).expect("members")[0].fields, None);
}

/// `ORDER BY sort_order, name COLLATE NOCASE`: the column decides, and the
/// case-insensitive name is only the tiebreak. Since §19 (F6) a create
/// writes `MAX + 1`, so freshly created groups keep their creation order —
/// the tiebreak is asserted against a deliberately flattened column, which
/// is the state a pre-§19 database is in.
#[test]
fn groups_sort_by_the_column_and_break_ties_by_name_case_insensitively() {
    let (dir, s) = open_storage();
    for name in ["zulu", "Alpha", "mike"] {
        s.create_sensor_group(name, None, &[]).expect("create");
    }
    assert_eq!(
        names(&s),
        ["zulu", "Alpha", "mike"],
        "distinct sort_order values, so creation order stands"
    );

    flatten_sort_order(&dir.path().join("portal.db"));
    assert_eq!(names(&s), ["Alpha", "mike", "zulu"], "the NOCASE tiebreak");
}

/// One query feeds the whole page, so it must come back grouped and ordered.
#[test]
fn list_all_members_is_ordered_by_group_then_position() {
    let (_dir, s) = open_storage();
    let a = s
        .create_sensor_group("A", None, &[probe(5), probe(2)])
        .expect("create");
    let b = s
        .create_sensor_group("B", None, &[probe(1)])
        .expect("create");

    let all = s.list_all_group_members().expect("members");
    let shape: Vec<_> = all
        .iter()
        .map(|m| (m.group_id, m.sensor_line.expect("line")))
        .collect();
    assert_eq!(shape, vec![(a, 5), (a, 2), (b, 1)]);
}

// ---------------------------------------------------------------------
// Ordering (MB0, monitoring.backend.md §3 WP0.3)
// ---------------------------------------------------------------------

/// The page's order, which is `list_sensor_groups`' order.
fn names(s: &Storage) -> Vec<String> {
    s.list_sensor_groups()
        .expect("list")
        .into_iter()
        .map(|g| g.name)
        .collect()
}

/// Sets every `sort_order` back to V8's default, through a second
/// connection — the public API deliberately offers no way to write the
/// column directly.
fn flatten_sort_order(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).expect("side door");
    conn.execute("UPDATE sensor_groups SET sort_order = 0", [])
        .expect("flatten");
}

fn ids(s: &Storage) -> Vec<i64> {
    s.list_sensor_groups()
        .expect("list")
        .into_iter()
        .map(|g| g.id)
        .collect()
}

/// The first move has to do two things at once: turn a flat `sort_order`
/// column into a real order (a swap of two equal values would be a no-op),
/// and then swap the pair. Both have to be in the database, not in the
/// caller's memory — hence the reopen.
///
/// Rows created since §19 (F6) already carry distinct `MAX + 1` values, so
/// the renumbering branch now only fires for groups predating that change —
/// which is exactly the upgrade case it exists for.
#[test]
fn a_move_promotes_the_implicit_name_order_and_survives_a_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let s = Storage::open(&path, None).expect("open");
    for name in ["a", "b", "c"] {
        s.create_sensor_group(name, None, &[]).expect("create");
    }
    // Flatten the column to what V8 left behind, so this test still covers
    // the promotion branch rather than the ordinary swap. Since §19 (F6)
    // `create_sensor_group` writes `MAX + 1`, so only a database that
    // predates that change is ever flat — which is the case this branch
    // exists for.
    flatten_sort_order(&path);
    assert_eq!(
        names(&s),
        ["a", "b", "c"],
        "every sort_order is 0, so the page falls back to the name order"
    );

    let c = ids(&s)[2];
    assert!(s.move_sensor_group(c, MoveDirection::Up).expect("move up"));
    assert_eq!(names(&s), ["a", "c", "b"]);

    drop(s);
    let s = Storage::open(&path, None).expect("reopen");
    assert_eq!(
        names(&s),
        ["a", "c", "b"],
        "the new order is a column, not process state"
    );
}

/// The second move is the interesting one: it runs against distinct
/// `sort_order` values and must swap them rather than renumber again.
#[test]
fn a_move_back_returns_the_group_to_where_it_started() {
    let (_dir, s) = open_storage();
    for name in ["a", "b", "c"] {
        s.create_sensor_group(name, None, &[]).expect("create");
    }
    let c = ids(&s)[2];

    assert!(s.move_sensor_group(c, MoveDirection::Up).expect("up"));
    assert_eq!(names(&s), ["a", "c", "b"]);
    assert!(s.move_sensor_group(c, MoveDirection::Down).expect("down"));
    assert_eq!(names(&s), ["a", "b", "c"]);
}

/// The arrows at the ends of the list are rendered disabled, so the only way
/// to ask for one of these is a hand-made POST. `false` rather than an error:
/// nothing went wrong, there was simply nowhere to go — and the route audits
/// that distinction.
#[test]
fn the_ends_of_the_list_and_an_unknown_group_are_refused_no_ops() {
    let (_dir, s) = open_storage();
    for name in ["a", "b"] {
        s.create_sensor_group(name, None, &[]).expect("create");
    }
    let ids = ids(&s);

    assert!(!s
        .move_sensor_group(ids[0], MoveDirection::Up)
        .expect("first up"));
    assert!(!s
        .move_sensor_group(ids[1], MoveDirection::Down)
        .expect("last down"));
    assert!(!s
        .move_sensor_group(9_999, MoveDirection::Up)
        .expect("unknown id"));
    assert_eq!(names(&s), ["a", "b"], "a refused move changes nothing");
}

/// F6. A new group goes to the END of the page. `sort_order` used to be left
/// at V8's default `0`, so once a move had renumbered everything to `0..n-1`
/// the first group also held `0` — a new group tied with it and the
/// `name COLLATE NOCASE` tiebreak dropped it into the middle of an
/// arrangement the operator had just made.
#[test]
fn a_new_group_is_created_last_even_after_a_move_has_renumbered() {
    let (_dir, s) = open_storage();
    for name in ["alpha", "beta"] {
        s.create_sensor_group(name, None, &[]).expect("create");
    }

    // The operator arranges the page: beta first.
    let beta = ids(&s)[1];
    assert!(s.move_sensor_group(beta, MoveDirection::Up).expect("move"));
    assert_eq!(names(&s), ["beta", "alpha"]);

    // A new group whose name sorts first must still land last.
    s.create_sensor_group("aaa", None, &[]).expect("create");
    assert_eq!(
        names(&s),
        ["beta", "alpha", "aaa"],
        "the new group appends, it does not jump the arrangement"
    );

    // And the next one after it, so the rule is MAX + 1 and not "one past
    // the renumbered block".
    s.create_sensor_group("aab", None, &[]).expect("create");
    assert_eq!(names(&s), ["beta", "alpha", "aaa", "aab"]);
}

/// V40: a group carries a location; setting it is apart from the save, and
/// `None` clears it again.
#[test]
fn group_location_is_set_listed_and_cleared() {
    let (_dir, s) = open_storage();
    let id = s
        .create_sensor_group("Cold", None, &[probe(0)])
        .expect("create");
    assert!(s.list_sensor_group_locations().expect("list").is_empty());

    assert!(s
        .set_sensor_group_location(id, Some("Sklad A"))
        .expect("set"));
    assert_eq!(
        s.list_sensor_group_locations().expect("list"),
        vec![(id, "Sklad A".to_string())]
    );

    assert!(s.set_sensor_group_location(id, None).expect("clear"));
    assert!(s.list_sensor_group_locations().expect("list").is_empty());
    assert!(!s
        .set_sensor_group_location(id + 99, Some("x"))
        .expect("missing"));
}
