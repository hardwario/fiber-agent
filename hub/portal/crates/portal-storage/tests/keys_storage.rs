//! `keys_repo` (device_identity + user_device_keys) integration tests.

use portal_storage::{NewDeviceIdentity, NewUser, NewUserDeviceKey, Storage};
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open+migrate");
    (dir, storage)
}

fn create_user(storage: &Storage, username: &str) -> i64 {
    storage
        .create_user(&NewUser {
            username: username.into(),
            password_hash: "hash".into(),
            full_name: None,
            job_title: None,
            user_role: "viewer".into(),
            is_superuser: false,
        })
        .expect("create user")
        .id
}

fn identity(hostname: &str, seed_enc: &[u8]) -> NewDeviceIdentity {
    NewDeviceIdentity {
        hostname: hostname.into(),
        ca_id: Some("ca-1".into()),
        ca_public_key: "ab".repeat(32),
        ca_seed_enc: seed_enc.to_vec(),
        admin_certificate: Some("{\"signer_id\":\"admin\"}".into()),
        paired_by_user_id: None,
    }
}

fn user_key(user_id: i64, hostname: &str) -> NewUserDeviceKey {
    NewUserDeviceKey {
        user_id,
        hostname: hostname.into(),
        seed_enc: vec![1, 2, 3],
        public_key_hex: "cd".repeat(32),
        fingerprint: "feedface".into(),
        certificate: "{\"signer_id\":\"user-1-alice\"}".into(),
        permissions: "[\"set_threshold\"]".into(),
    }
}

#[test]
fn device_identity_upsert_get_delete_roundtrip() {
    let (_dir, storage) = open();

    assert!(storage
        .get_device_identity("fiber-01")
        .expect("get")
        .is_none());

    storage
        .upsert_device_identity(&identity("fiber-01", &[9, 9, 9]))
        .expect("upsert");
    let row = storage
        .get_device_identity("fiber-01")
        .expect("get")
        .expect("present");
    assert_eq!(row.ca_id.as_deref(), Some("ca-1"));
    assert_eq!(row.ca_seed_enc, vec![9, 9, 9]);
    assert!(!row.paired_at.is_empty());

    // Re-pair: every field overwritten.
    let mut repaired = identity("fiber-01", &[7, 7]);
    repaired.ca_id = Some("ca-2".into());
    storage
        .upsert_device_identity(&repaired)
        .expect("re-upsert");
    let row = storage
        .get_device_identity("fiber-01")
        .expect("get")
        .expect("present");
    assert_eq!(row.ca_id.as_deref(), Some("ca-2"));
    assert_eq!(row.ca_seed_enc, vec![7, 7]);

    assert!(storage.delete_device_identity("fiber-01").expect("delete"));
    assert!(!storage.delete_device_identity("fiber-01").expect("gone"));
    assert!(storage
        .get_device_identity("fiber-01")
        .expect("get")
        .is_none());
}

#[test]
fn user_device_key_roundtrip_and_listing() {
    let (_dir, storage) = open();
    let alice = create_user(&storage, "alice");
    let bob = create_user(&storage, "bob");

    storage
        .upsert_user_device_key(&user_key(alice, "fiber-01"))
        .expect("upsert alice");
    storage
        .upsert_user_device_key(&user_key(bob, "fiber-01"))
        .expect("upsert bob");
    storage
        .upsert_user_device_key(&user_key(alice, "fiber-02"))
        .expect("upsert alice other host");

    let row = storage
        .get_user_device_key(alice, "fiber-01")
        .expect("get")
        .expect("present");
    assert_eq!(row.seed_enc, vec![1, 2, 3]);
    assert_eq!(row.permissions, "[\"set_threshold\"]");

    let listed = storage.list_user_device_keys("fiber-01").expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].user_id, alice);
    assert_eq!(listed[1].user_id, bob);

    // Upsert replaces in place (re-mint path).
    let mut reminted = user_key(alice, "fiber-01");
    reminted.permissions = "[\"set_threshold\",\"set_interval\"]".into();
    reminted.seed_enc = vec![4, 5];
    storage
        .upsert_user_device_key(&reminted)
        .expect("re-upsert");
    let row = storage
        .get_user_device_key(alice, "fiber-01")
        .expect("get")
        .expect("present");
    assert_eq!(row.seed_enc, vec![4, 5]);
    assert_eq!(row.permissions, "[\"set_threshold\",\"set_interval\"]");
    assert_eq!(
        storage
            .list_user_device_keys("fiber-01")
            .expect("list")
            .len(),
        2
    );

    assert!(storage
        .delete_user_device_key(alice, "fiber-01")
        .expect("delete"));
    assert!(!storage
        .delete_user_device_key(alice, "fiber-01")
        .expect("gone"));
    assert!(storage
        .get_user_device_key(alice, "fiber-01")
        .expect("get")
        .is_none());
}

#[test]
fn deleting_user_cascades_to_their_device_keys() {
    let (_dir, storage) = open();
    let alice = create_user(&storage, "alice");
    storage
        .upsert_user_device_key(&user_key(alice, "fiber-01"))
        .expect("upsert");

    assert!(storage.delete_user(alice).expect("delete user"));
    assert!(storage
        .get_user_device_key(alice, "fiber-01")
        .expect("get")
        .is_none());
}
