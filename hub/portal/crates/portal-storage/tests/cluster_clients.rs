//! C5: `cluster_clients` (V28) — the Server's Client Hub registry
//! (plan/portal/devices.hub.md §12 J, plan/control/cluster.md §7, §9).

use portal_storage::{
    ClusterInsert, NewClusterClient, Storage, CLUSTER_STATE_COMPLETE, CLUSTER_STATE_FAILED,
    CLUSTER_STATE_PENDING,
};
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn new_client(hostname: &str, eui: Option<&str>) -> NewClusterClient {
    NewClusterClient {
        hostname: hostname.into(),
        gateway_eui: eui.map(str::to_string),
        peer_username: format!("peer-{hostname}"),
        registered_by: Some("admin".into()),
        registration_error: "did not finish".into(),
    }
}

fn inserted(result: ClusterInsert) -> portal_storage::ClusterClient {
    match result {
        ClusterInsert::Inserted(row) => *row,
        other => panic!("expected an insert, got {other:?}"),
    }
}

#[test]
fn a_new_row_is_failed_until_its_answer_arrives() {
    let (_dir, s) = open();
    let row = inserted(
        s.insert_cluster_client(&new_client("proximos-1a2b3c4d", Some("70b3d54900aa0002")))
            .unwrap(),
    );
    assert_eq!(row.registration_state, CLUSTER_STATE_FAILED);
    assert_eq!(row.registration_error.as_deref(), Some("did not finish"));
    assert_eq!(row.peer_username, "peer-proximos-1a2b3c4d");
    assert_eq!(row.registered_by.as_deref(), Some("admin"));
    assert!(row.completed_at.is_none());
    assert_eq!(s.list_cluster_clients().unwrap(), vec![row]);
}

#[test]
fn the_registration_walks_failed_pending_complete_and_back_on_a_retry() {
    let (_dir, s) = open();
    let h = "proximos-1a2b3c4d";
    inserted(s.insert_cluster_client(&new_client(h, None)).unwrap());

    let row = s
        .record_cluster_result(h, "peer-proximos-1a2b3c4d", None)
        .unwrap()
        .unwrap();
    assert_eq!(row.registration_state, CLUSTER_STATE_PENDING);
    assert!(row.registration_error.is_none());

    assert!(s.mark_cluster_client_complete(h).unwrap());
    let row = s.get_cluster_client(h).unwrap().unwrap();
    assert_eq!(row.registration_state, CLUSTER_STATE_COMPLETE);
    assert!(row.completed_at.is_some());
    assert!(
        !s.mark_cluster_client_complete(h).unwrap(),
        "complete is not moved again"
    );

    // A Retry rotates the password: the Client must join again.
    let row = s
        .begin_cluster_attempt(h, "did not finish", None)
        .unwrap()
        .unwrap();
    assert_eq!(row.registration_state, CLUSTER_STATE_FAILED);
    assert!(row.completed_at.is_none());
    let row = s
        .record_cluster_result(h, "peer-proximos-1a2b3c4d", Some("gateway failed"))
        .unwrap()
        .unwrap();
    assert_eq!(row.registration_state, CLUSTER_STATE_FAILED);
    assert_eq!(row.registration_error.as_deref(), Some("gateway failed"));
    assert!(
        !s.mark_cluster_client_complete(h).unwrap(),
        "an incomplete registration stays incomplete whatever the Client does"
    );
}

#[test]
fn a_duplicate_hostname_or_eui_writes_nothing() {
    let (_dir, s) = open();
    inserted(
        s.insert_cluster_client(&new_client("proximos-1a2b3c4d", Some("70b3d54900aa0002")))
            .unwrap(),
    );
    assert_eq!(
        s.insert_cluster_client(&new_client("proximos-1a2b3c4d", None))
            .unwrap(),
        ClusterInsert::HostnameTaken
    );
    assert_eq!(
        s.insert_cluster_client(&new_client("proximos-00000002", Some("70b3d54900aa0002")))
            .unwrap(),
        ClusterInsert::GatewayEuiTaken
    );
    assert_eq!(s.list_cluster_clients().unwrap().len(), 1);
    // Two Clients without a gateway do not collide on a NULL EUI.
    inserted(
        s.insert_cluster_client(&new_client("proximos-00000003", None))
            .unwrap(),
    );
    inserted(
        s.insert_cluster_client(&new_client("proximos-00000004", None))
            .unwrap(),
    );
}

#[test]
fn the_schema_refuses_a_hostname_or_eui_that_is_not_canonical() {
    let (_dir, s) = open();
    for (hostname, eui) in [
        ("PROXIMOS-1A2B3C4D", None),
        ("proximos-1a2b3c4", None),
        ("fiber-1a2b3c4d", None),
        ("proximos-1a2b3c4g", None),
        ("proximos-1a2b3c4d", Some("70:b3:d5:49:00:aa:00:02")),
        ("proximos-1a2b3c4d", Some("70B3D54900AA0002")),
    ] {
        assert!(
            s.insert_cluster_client(&new_client(hostname, eui)).is_err(),
            "{hostname} / {eui:?} must be refused by the table itself"
        );
    }
    assert!(s.list_cluster_clients().unwrap().is_empty());
}

#[test]
fn a_failed_removal_keeps_the_row_and_a_completed_one_deletes_it() {
    let (_dir, s) = open();
    let h = "proximos-1a2b3c4d";
    inserted(s.insert_cluster_client(&new_client(h, None)).unwrap());
    let row = s
        .record_cluster_removal_error(h, "gateway failed")
        .unwrap()
        .unwrap();
    assert_eq!(row.removal_error.as_deref(), Some("gateway failed"));
    assert!(s.delete_cluster_client(h).unwrap());
    assert!(s.get_cluster_client(h).unwrap().is_none());
    assert!(!s.delete_cluster_client(h).unwrap());
}

/// The registry is the one table whose purpose brushes a secret, so the
/// schema itself is pinned: no column could hold the Client's password.
#[test]
fn there_is_no_column_for_a_password() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("portal.db");
    drop(Storage::open(&path, None).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('cluster_clients')")
        .unwrap();
    let columns: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "hostname",
            "gateway_eui",
            "peer_username",
            "registration_state",
            "registration_error",
            "removal_error",
            "registered_at",
            "registered_by",
            "last_attempt_at",
            "completed_at",
        ]
    );
}

/// V28 lands on a database that has been running since V1.
#[test]
fn v28_applies_over_a_v27_database() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("portal.db");
    drop(Storage::open_at_version(&path, None, 27).unwrap());
    let s = Storage::open(&path, None).unwrap();
    assert!(s.list_cluster_clients().unwrap().is_empty());
}
