use portal_storage::{
    AuditFilter, AuditSort, NewAuditEntry, NewUser, SessionRow, Storage, StorageError,
};
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open+migrate");
    (dir, storage)
}

fn new_user(username: &str) -> NewUser {
    NewUser {
        username: username.into(),
        password_hash: "hash".into(),
        full_name: Some("Jane Doe".into()),
        job_title: Some("Technician".into()),
        user_role: "viewer".into(),
        is_superuser: false,
    }
}

#[test]
fn user_crud_roundtrip() {
    let (_dir, storage) = open();

    let created = storage.create_user(&new_user("alice")).expect("create");
    assert_eq!(created.username, "alice");
    assert!(!created.is_superuser);
    assert!(created.enabled);

    let fetched = storage.get_user(created.id).expect("get").expect("present");
    assert_eq!(fetched, created);

    let updated = storage
        .update_user(created.id, Some(Some("Jane R. Doe")), Some("manager"))
        .expect("update")
        .expect("present");
    assert_eq!(updated.full_name.as_deref(), Some("Jane R. Doe"));
    assert_eq!(updated.job_title.as_deref(), Some("Technician")); // unchanged
    assert_eq!(updated.user_role, "manager");

    // `Some(None)` clears the name; `None` keeps what is stored.
    let cleared = storage
        .update_user(created.id, Some(None), None)
        .expect("update")
        .expect("present");
    assert_eq!(cleared.full_name, None);
    assert_eq!(cleared.user_role, "manager");

    // MR !103 review: a role-only update does not write back a name read
    // before another admin's rename landed, and the other way round.
    storage
        .update_user(created.id, Some(Some("Renamed")), None)
        .expect("rename");
    let role_only = storage
        .update_user(created.id, None, Some("viewer"))
        .expect("role")
        .expect("present");
    assert_eq!(role_only.full_name.as_deref(), Some("Renamed"));
    assert_eq!(role_only.user_role, "viewer");

    assert!(storage
        .set_password(created.id, "new-hash")
        .expect("set_password"));
    assert_eq!(
        storage
            .get_user(created.id)
            .expect("get")
            .expect("present")
            .password_hash,
        "new-hash"
    );

    assert!(storage.set_enabled(created.id, false).expect("disable"));
    assert!(
        !storage
            .get_user(created.id)
            .expect("get")
            .expect("present")
            .enabled
    );

    assert!(storage.delete_user(created.id).expect("delete"));
    assert_eq!(storage.get_user(created.id).expect("get"), None);
    assert!(!storage.delete_user(created.id).expect("delete again"));
}

#[test]
fn duplicate_username_is_conflict() {
    let (_dir, storage) = open();
    storage.create_user(&new_user("bob")).expect("first");

    let err = storage.create_user(&new_user("bob")).unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)), "got {err:?}");
}

#[test]
fn username_lookup_is_case_insensitive() {
    let (_dir, storage) = open();
    storage.create_user(&new_user("Carol")).expect("create");

    let found = storage
        .get_user_by_username("carol")
        .expect("lookup")
        .expect("present");
    assert_eq!(found.username, "Carol");

    // and the unique constraint is case-insensitive too
    let err = storage.create_user(&new_user("CAROL")).unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)));
}

#[test]
fn full_names_for_resolves_a_batch_and_skips_the_nameless() {
    let (_dir, storage) = open();
    storage.create_user(&new_user("admin")).expect("create");
    storage
        .create_user(&NewUser {
            full_name: None,
            ..new_user("bob")
        })
        .expect("create");
    storage
        .create_user(&NewUser {
            full_name: Some("   ".into()),
            ..new_user("blank")
        })
        .expect("create");

    let names = storage
        .full_names_for(&["admin", "ADMIN", "bob", "blank", "gone", "system:backfill"])
        .expect("lookup");
    assert_eq!(
        names.len(),
        2,
        "only users that have a full name: {names:?}"
    );
    assert_eq!(names["admin"], "Jane Doe");
    assert_eq!(
        names["ADMIN"], "Jane Doe",
        "keyed by the spelling asked with"
    );

    let none: [&str; 0] = [];
    assert!(storage.full_names_for(&none).expect("empty").is_empty());
}

#[test]
fn list_users_excludes_disabled_unless_asked() {
    let (_dir, storage) = open();
    let a = storage.create_user(&new_user("dave")).expect("create a");
    storage.create_user(&new_user("erin")).expect("create b");
    storage.set_enabled(a.id, false).expect("disable a");

    let enabled_only = storage.list_users(false).expect("list enabled");
    assert_eq!(enabled_only.len(), 1);
    assert_eq!(enabled_only[0].username, "erin");

    let all = storage.list_users(true).expect("list all");
    assert_eq!(all.len(), 2);
}

#[test]
fn count_superusers_counts_only_enabled_admins() {
    let (_dir, storage) = open();
    let mut admin = new_user("root");
    admin.user_role = "admin".into();
    admin.is_superuser = true;
    let root = storage.create_user(&admin).expect("create admin");
    assert_eq!(storage.count_superusers().expect("count"), 1);

    storage.set_enabled(root.id, false).expect("disable");
    assert_eq!(storage.count_superusers().expect("count"), 0);
}

#[test]
fn touch_last_login_sets_timestamp() {
    let (_dir, storage) = open();
    let user = storage.create_user(&new_user("frank")).expect("create");
    assert_eq!(user.last_login, None);

    storage.touch_last_login(user.id).expect("touch");
    let fetched = storage.get_user(user.id).expect("get").expect("present");
    assert!(fetched.last_login.is_some());
}

fn session_row(token_hash: &str, user_id: i64, created_at: i64, expires_at: i64) -> SessionRow {
    SessionRow {
        token_hash: token_hash.into(),
        user_id,
        created_at,
        last_seen_at: created_at,
        expires_at,
    }
}

#[test]
fn session_roundtrip_and_disabled_user_excluded() {
    let (_dir, storage) = open();
    let user = storage.create_user(&new_user("gina")).expect("create");

    storage
        .create_session(&session_row("tok-1", user.id, 1_000, 2_000))
        .expect("create session");

    let (session, fetched_user) = storage
        .get_session("tok-1")
        .expect("get_session")
        .expect("present");
    assert_eq!(session.user_id, user.id);
    assert_eq!(fetched_user.username, "gina");

    storage.set_enabled(user.id, false).expect("disable user");
    assert_eq!(
        storage.get_session("tok-1").expect("get_session"),
        None,
        "session for a disabled user must not resolve"
    );
}

#[test]
fn bump_session_extends_expiry() {
    let (_dir, storage) = open();
    let user = storage.create_user(&new_user("hank")).expect("create");
    storage
        .create_session(&session_row("tok-2", user.id, 1_000, 2_000))
        .expect("create session");

    storage.bump_session("tok-2", 1_500, 5_000).expect("bump");

    let (session, _) = storage
        .get_session("tok-2")
        .expect("get_session")
        .expect("present");
    assert_eq!(session.last_seen_at, 1_500);
    assert_eq!(session.expires_at, 5_000);
}

#[test]
fn session_delete_and_prune() {
    let (_dir, storage) = open();
    let user = storage.create_user(&new_user("iris")).expect("create");
    storage
        .create_session(&session_row("tok-3", user.id, 1_000, 2_000))
        .expect("create");
    storage
        .create_session(&session_row("tok-4", user.id, 1_000, 9_999))
        .expect("create");

    assert!(storage.delete_session("tok-3").expect("delete"));
    assert!(!storage.delete_session("tok-3").expect("delete again"));

    let pruned = storage.prune_expired_sessions(9_999).expect("prune");
    assert_eq!(pruned, 1);
    assert_eq!(storage.get_session("tok-4").expect("get"), None);
}

#[test]
fn delete_sessions_for_user_removes_all() {
    let (_dir, storage) = open();
    let user = storage.create_user(&new_user("jack")).expect("create");
    storage
        .create_session(&session_row("tok-5", user.id, 1_000, 2_000))
        .expect("create");
    storage
        .create_session(&session_row("tok-6", user.id, 1_000, 2_000))
        .expect("create");

    let removed = storage
        .delete_sessions_for_user(user.id)
        .expect("delete all");
    assert_eq!(removed, 2);
    assert_eq!(storage.get_session("tok-5").expect("get"), None);
}

fn audit_entry(event_type: &str, username: &str, success: bool) -> NewAuditEntry {
    NewAuditEntry {
        event_type: event_type.into(),
        username: Some(username.into()),
        user_id: None,
        ip: Some("127.0.0.1".into()),
        target: None,
        details: None,
        success,
    }
}

#[test]
fn audit_insert_and_list_roundtrip() {
    let (_dir, storage) = open();
    let id = storage
        .insert_audit(&audit_entry("login", "alice", true))
        .expect("insert");
    assert!(id > 0);

    let (rows, total) = storage
        .list_audit(&AuditFilter {
            limit: 10,
            ..Default::default()
        })
        .expect("list");
    assert_eq!(total, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_type, "login");
    assert_eq!(rows[0].username.as_deref(), Some("alice"));
}

#[test]
fn audit_orders_by_the_column_the_page_asked_for() {
    let (_dir, storage) = open();
    // Inserted in an order that matches neither the event name nor the user,
    // so a sort falling back to `ts` would be visible.
    for (event, user) in [("logout", "carol"), ("login", "alice"), ("delete", "bob")] {
        storage
            .insert_audit(&audit_entry(event, user, true))
            .expect("insert");
    }

    let events = |sort: AuditSort, desc: bool| -> Vec<String> {
        storage
            .list_audit(&AuditFilter {
                limit: 10,
                sort,
                sort_desc: desc,
                ..Default::default()
            })
            .expect("list")
            .0
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    };

    // Newest first is the default, and `id` breaks the tie when three rows
    // share a second — which they do here, `ts` being `datetime('now')`.
    assert_eq!(
        events(AuditSort::Time, true),
        ["delete", "login", "logout"],
        "newest first"
    );
    assert_eq!(
        events(AuditSort::Time, false),
        ["logout", "login", "delete"],
        "oldest first — the reason this list got a sort at all"
    );
    assert_eq!(
        events(AuditSort::Event, false),
        ["delete", "login", "logout"]
    );
    assert_eq!(
        events(AuditSort::Event, true),
        ["logout", "login", "delete"]
    );
    // Sorting on another column still reaches the right row.
    assert_eq!(
        events(AuditSort::User, false),
        ["login", "delete", "logout"]
    );
}

#[test]
fn audit_sort_keys_round_trip() {
    for sort in [
        AuditSort::Time,
        AuditSort::Event,
        AuditSort::User,
        AuditSort::Ip,
        AuditSort::Target,
        AuditSort::Result,
    ] {
        assert_eq!(AuditSort::from_key(sort.key()), Some(sort));
    }
    assert_eq!(AuditSort::from_key("ts; DELETE FROM audit_log"), None);
}

#[test]
fn audit_filter_by_event_type_and_success() {
    let (_dir, storage) = open();
    storage
        .insert_audit(&audit_entry("login", "alice", true))
        .expect("insert 1");
    storage
        .insert_audit(&audit_entry("login", "bob", false))
        .expect("insert 2");
    storage
        .insert_audit(&audit_entry("logout", "alice", true))
        .expect("insert 3");

    let (rows, total) = storage
        .list_audit(&AuditFilter {
            event_type: Some("login".into()),
            success: Some(false),
            limit: 10,
            ..Default::default()
        })
        .expect("list");
    assert_eq!(total, 1);
    assert_eq!(rows[0].username.as_deref(), Some("bob"));
}

#[test]
fn audit_filter_by_since_until() {
    let (_dir, storage) = open();
    storage
        .insert_audit(&audit_entry("login", "alice", true))
        .expect("insert");

    let (rows, total) = storage
        .list_audit(&AuditFilter {
            since: Some("2000-01-01T00:00:00".into()),
            until: Some("2100-01-01T00:00:00".into()),
            limit: 10,
            ..Default::default()
        })
        .expect("list within range");
    assert_eq!(total, 1);
    assert_eq!(rows.len(), 1);

    let (rows, total) = storage
        .list_audit(&AuditFilter {
            since: Some("2100-01-01T00:00:00".into()),
            limit: 10,
            ..Default::default()
        })
        .expect("list outside range");
    assert_eq!(total, 0);
    assert_eq!(rows.len(), 0);
}

#[test]
fn audit_pagination_totals() {
    let (_dir, storage) = open();
    for i in 0..5 {
        storage
            .insert_audit(&audit_entry("login", &format!("user-{i}"), true))
            .expect("insert");
    }

    let (page1, total1) = storage
        .list_audit(&AuditFilter {
            limit: 2,
            offset: 0,
            ..Default::default()
        })
        .expect("page 1");
    assert_eq!(total1, 5);
    assert_eq!(page1.len(), 2);

    let (page2, total2) = storage
        .list_audit(&AuditFilter {
            limit: 2,
            offset: 4,
            ..Default::default()
        })
        .expect("page 2");
    assert_eq!(total2, 5);
    assert_eq!(page2.len(), 1);
}

#[test]
fn audit_limit_is_clamped() {
    let (_dir, storage) = open();
    storage
        .insert_audit(&audit_entry("login", "alice", true))
        .expect("insert");

    // limit 0 clamps up to 1, not down to "no rows"
    let (rows, _) = storage
        .list_audit(&AuditFilter {
            limit: 0,
            ..Default::default()
        })
        .expect("list");
    assert_eq!(rows.len(), 1);
}

/// #107: "Active users (24h)" counts users who were signed in, not names
/// typed into the login form. Rows are shaped the way `AuthService` writes
/// them: a failed login carries the typed username and no `user_id`, an
/// authenticated action carries both.
#[test]
fn audit_counts_only_authenticated_users_as_actors() {
    let (_dir, storage) = open();
    let admin = storage.create_user(&new_user("admin")).expect("create");
    let as_admin = |event_type: &str| NewAuditEntry {
        user_id: Some(admin.id),
        ..audit_entry(event_type, "admin", true)
    };

    // Someone spraying `/login` with names of their choosing, and the real
    // admin mistyping a password once.
    for name in ["root", "guest", "x' OR 1=1", "admin2"] {
        storage
            .insert_audit(&audit_entry("login", name, false))
            .expect("insert failed login");
    }
    storage
        .insert_audit(&audit_entry("login", "admin", false))
        .expect("insert wrong password");
    // The portal acting on its own: a username, no user.
    storage
        .insert_audit(&audit_entry("node_backfill_auto", "system:backfill", true))
        .expect("insert system row");
    // The admin then signs in and does something — two rows, one actor.
    storage
        .insert_audit(&as_admin("login"))
        .expect("insert login");
    storage
        .insert_audit(&as_admin("alarm_ack"))
        .expect("insert action");

    let counts = storage.audit_counts().expect("counts");
    assert_eq!(counts.total, 8);
    assert_eq!(counts.last_day, 8);
    assert_eq!(counts.failed_day, 5, "failed logins still count as failed");
    assert_eq!(counts.actors_day, 1, "only the signed-in admin is an actor");

    // A user deleted since still acted today: `audit_log.user_id` has no
    // foreign key, so the row keeps the id.
    let bob = storage.create_user(&new_user("bob")).expect("create bob");
    storage
        .insert_audit(&NewAuditEntry {
            user_id: Some(bob.id),
            ..audit_entry("password_change", "bob", true)
        })
        .expect("insert bob");
    assert!(storage.delete_user(bob.id).expect("delete bob"));
    assert_eq!(storage.audit_counts().expect("counts").actors_day, 2);
}
