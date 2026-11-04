//! The `/data/proximos/ble/` layout — `plan/control/ble/plan.md` §5 "`/data` layout".
//!
//! ```text
//! /data/proximos/ble/              owner proximos, dirs created by 020-ble-init.sh
//! ├── central/
//! │   ├── beacons.json             0600 — enrollment records incl. per-beacon provisioning state
//! │   ├── archive/                 downloaded EN12830 segments, pending export
//! │   └── provisioned              marker — the adapter was probed and scanning started
//! └── peripheral/
//!     └── provisioned              marker — the FB00 service registered
//! ```
//!
//! **There is no `pin` file** (T0.2): the FB01 credential is an ephemeral session token that lives
//! in memory for the session only, so there is nothing here to rotate or exfiltrate.
//!
//! Division of labour with the migration is exact: `020-ble-init.sh` creates the **directories
//! only**; `create` writes the contents. Neither step depends on the other having produced file
//! contents, which is what lets a factory-reset `/data` and a first boot be the same code path.
//!
//! Two properties this module owns, because getting them wrong is silent:
//!
//! - **Writes are atomic, and the parent directory is synced too.** `sync_all` on the file makes its
//!   *contents* durable, but the rename that gives them their name lives in the parent directory and
//!   is not durable until the directory itself is synced. Without that, a power cut just after a
//!   "successful" write can leave the old `beacons.json` — a Hub that comes back having forgotten a
//!   give-up and re-dials a beacon that already refused it five times.
//! - **`beacons.json` is 0600 and stays 0600**, including across a rewrite.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Component, Error, Kind, Result, Role};
use crate::mac::MacAddr;

/// The component's state root. Not configurable at the type level: it is a `/data` contract, and the
/// unit file, the tmpfiles fragment and the Landlock path table all name it literally. The env
/// override (`PROXIMOS_BLE_DATA`) is a dispatch-level concern, not a store-level one.
pub const DEFAULT_ROOT: &str = "/data/proximos/ble";

/// Mode for anything holding a credential or per-device record (`system/design.md` §10).
#[cfg(unix)]
pub const SECRET_MODE: u32 = 0o600;

/// Which files a caller may ask for, so paths are never assembled from strings at call sites —
/// decision #6 requires an enumerable file surface, and this is what makes the Landlock and AppArmor
/// tables reviewable against the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Beacons,
    CentralProvisioned,
    ArchiveDir,
    PeripheralProvisioned,
}

impl Slot {
    pub const ALL: [Slot; 4] = [
        Slot::Beacons,
        Slot::CentralProvisioned,
        Slot::ArchiveDir,
        Slot::PeripheralProvisioned,
    ];

    pub const fn rel(self) -> &'static str {
        match self {
            Slot::Beacons => "central/beacons.json",
            Slot::CentralProvisioned => "central/provisioned",
            Slot::ArchiveDir => "central/archive",
            Slot::PeripheralProvisioned => "peripheral/provisioned",
        }
    }

    /// `beacons.json` holds the per-beacon record; the doc pins it 0600. The markers are empty
    /// files and the archive directory is not a file at all.
    pub const fn is_secret(self) -> bool {
        matches!(self, Slot::Beacons)
    }

    pub const fn role(self) -> Role {
        match self {
            Slot::Beacons | Slot::CentralProvisioned | Slot::ArchiveDir => Role::Central,
            Slot::PeripheralProvisioned => Role::Peripheral,
        }
    }

    pub const fn component(self) -> Component {
        match self {
            Slot::Beacons | Slot::ArchiveDir => Component::BeaconRegistry,
            Slot::CentralProvisioned | Slot::PeripheralProvisioned => Component::DeviceConfig,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    /// The shipped location.
    pub fn at_default_root() -> Self {
        Store::new(DEFAULT_ROOT)
    }

    pub fn path(&self, slot: Slot) -> PathBuf {
        self.root.join(slot.rel())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn exists(&self, slot: Slot) -> bool {
        self.path(slot).exists()
    }

    /// One archive file per beacon. The MAC's colons become dashes so the name is path-safe.
    pub fn archive_path(&self, mac: &MacAddr) -> PathBuf {
        self.path(Slot::ArchiveDir)
            .join(format!("{}.jsonl", mac.as_file_stem()))
    }

    /// The raw chunks of every download from one beacon, next to its archive file
    /// (beacon_history.md §7 Q6 layer 2): what the block CRC will be identified from.
    pub fn archive_chunks_path(&self, mac: &MacAddr) -> PathBuf {
        self.path(Slot::ArchiveDir)
            .join(format!("{}.chunks.jsonl", mac.as_file_stem()))
    }

    /// Create the directory tree `020-ble-init.sh` would have created. Idempotent, so a wiped
    /// `/data` and a first boot are the same code path.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.root.join("central"),
            self.path(Slot::ArchiveDir),
            self.root.join("peripheral"),
        ] {
            std::fs::create_dir_all(&dir).map_err(|e| {
                Error::new(
                    Kind::StorageError,
                    None,
                    Component::BeaconRegistry,
                    format!("cannot create {}: {e}", dir.display()),
                )
            })?;
        }
        Ok(())
    }

    /// Read a slot, or `Ok(None)` when it is absent.
    ///
    /// Absent and unreadable are deliberately different: absent is "bootstrap has not run",
    /// unreadable is `StorageError`. Collapsing them would make a permissions mistake look like a
    /// fresh device and silently discard every persisted give-up.
    pub fn read(&self, slot: Slot) -> Result<Option<String>> {
        match std::fs::read_to_string(self.path(slot)) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.storage_err(slot, format!("cannot read: {e}"))),
        }
    }

    /// Read a slot that must be present, mapping absence to `NotBootstrapped`.
    pub fn require(&self, slot: Slot) -> Result<String> {
        self.read(slot)?.ok_or_else(|| {
            Error::new(
                Kind::NotBootstrapped,
                slot.role(),
                slot.component(),
                format!(
                    "{} is absent: bootstrap has not run, or /data was wiped",
                    slot.rel()
                ),
            )
        })
    }

    /// Write a slot atomically, creating parent directories if the migration has not run.
    pub fn write(&self, slot: Slot, contents: &str) -> Result<()> {
        let path = self.path(slot);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                self.storage_err(slot, format!("cannot create {}: {e}", dir.display()))
            })?;
        }

        let tmp = path.with_extension("tmp");
        {
            let f = std::fs::File::create(&tmp)
                .map_err(|e| self.storage_err(slot, format!("cannot create a temp file: {e}")))?;
            // Mode first, content second: the secret is never even briefly world-readable.
            set_mode(&f, slot, self, &tmp)?;
            let mut f = f;
            f.write_all(contents.as_bytes())
                .map_err(|e| self.storage_err(slot, format!("cannot write: {e}")))?;
            f.sync_all()
                .map_err(|e| self.storage_err(slot, format!("cannot flush: {e}")))?;
        }

        std::fs::rename(&tmp, &path).map_err(|e| {
            // Leave no debris behind on a failed replace.
            let _ = std::fs::remove_file(&tmp);
            self.storage_err(slot, format!("cannot replace {}: {e}", path.display()))
        })?;

        // The rename lives in the parent directory; that entry is not durable until the directory
        // itself is synced. See the module header.
        if let Some(dir) = path.parent() {
            std::fs::File::open(dir)
                .and_then(|d| d.sync_all())
                .map_err(|e| {
                    self.storage_err(slot, format!("cannot flush {}: {e}", dir.display()))
                })?;
        }
        Ok(())
    }

    /// Remove a slot. Absence is success — factory reset must be re-runnable.
    pub fn remove(&self, slot: Slot) -> Result<()> {
        match std::fs::remove_file(self.path(slot)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.storage_err(slot, format!("cannot remove: {e}"))),
        }
    }

    /// Write a per-role `provisioned` marker.
    ///
    /// **Withheld on degraded-green**: central writes it only when the adapter was probed and
    /// scanning started, peripheral only when the FB00 service registered. Degraded-green and
    /// provisioned-complete are two different states and this records the second one, so the next
    /// boot re-probes instead of assuming.
    pub fn mark_provisioned(&self, role: Role) -> Result<()> {
        self.write(provisioned_slot(role), "")
    }

    pub fn is_provisioned(&self, role: Role) -> bool {
        self.exists(provisioned_slot(role))
    }

    pub fn clear_provisioned(&self, role: Role) -> Result<()> {
        self.remove(provisioned_slot(role))
    }

    fn storage_err(&self, slot: Slot, why: String) -> Error {
        Error::new(
            Kind::StorageError,
            slot.role(),
            slot.component(),
            format!("{}: {why}", self.path(slot).display()),
        )
    }
}

const fn provisioned_slot(role: Role) -> Slot {
    match role {
        Role::Central => Slot::CentralProvisioned,
        Role::Peripheral => Slot::PeripheralProvisioned,
    }
}

/// Secrets are created 0600 and stay 0600 across a rewrite.
#[cfg(unix)]
fn set_mode(f: &std::fs::File, slot: Slot, store: &Store, tmp: &Path) -> Result<()> {
    if !slot.is_secret() {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(SECRET_MODE))
        .map_err(|e| {
            store.storage_err(
                slot,
                format!("cannot set mode 0600 on {}: {e}", tmp.display()),
            )
        })
}

#[cfg(not(unix))]
fn set_mode(_f: &std::fs::File, _slot: Slot, _store: &Store, _tmp: &Path) -> Result<()> {
    // Dev hosts only; the shipped target is Linux and the mode rule is enforced there.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-rolled so the crate needs no `tempfile` dev-dependency, exactly as control-radio does.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "control-ble-store-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            TempDir(base)
        }
        fn store(&self) -> Store {
            Store::new(&self.0)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn writes_are_atomic_and_leave_no_temp_file() {
        let t = TempDir::new("atomic");
        let s = t.store();
        s.write(Slot::Beacons, "{\"version\":1}").expect("writes");
        assert_eq!(
            s.read(Slot::Beacons).expect("reads"),
            Some("{\"version\":1}".to_string())
        );
        let leftovers: Vec<_> = std::fs::read_dir(t.0.join("central"))
            .expect("dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn secrets_are_created_0600_and_stay_0600_across_a_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new("mode");
        let s = t.store();
        for pass in ["v1", "v2"] {
            s.write(Slot::Beacons, pass).expect("writes");
            let mode = std::fs::metadata(s.path(Slot::Beacons))
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "beacons.json after {pass}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn absent_and_unreadable_are_different_errors() {
        let t = TempDir::new("unreadable");
        let s = t.store();
        // Absent is not an error at all.
        assert_eq!(s.read(Slot::Beacons).expect("reads"), None);

        use std::os::unix::fs::PermissionsExt;
        s.write(Slot::Beacons, "{}").expect("writes");
        std::fs::set_permissions(
            s.path(Slot::Beacons),
            std::fs::Permissions::from_mode(0o000),
        )
        .expect("chmod 000");
        let outcome = s.read(Slot::Beacons);
        // Restore first, so TempDir's drop can clean up whatever the assertion below does.
        let _ = std::fs::set_permissions(
            s.path(Slot::Beacons),
            std::fs::Permissions::from_mode(0o600),
        );
        // Root ignores the mode bits, so this half of the property is only observable
        // unprivileged — which is how CI runs it (`setpriv`). An `Ok` here *is* the privileged
        // case: nothing to assert, and no reason to fail the suite. Deliberately not probing the
        // euid to branch on it — that needs `unsafe`, and in this crate only
        // `central/en12830/att.rs` may use it.
        if let Err(err) = outcome {
            assert_eq!(err.kind(), Kind::StorageError);
            assert_eq!(err.component(), Component::BeaconRegistry);
        }
    }

    #[test]
    fn require_maps_absence_to_not_bootstrapped() {
        let t = TempDir::new("require");
        let err = t
            .store()
            .require(Slot::Beacons)
            .expect_err("must be NotBootstrapped");
        assert_eq!(err.kind(), Kind::NotBootstrapped);
        assert_eq!(err.component(), Component::BeaconRegistry);
        assert_eq!(err.role(), Some(Role::Central));
    }

    #[test]
    fn markers_are_per_role() {
        let t = TempDir::new("markers");
        let s = t.store();
        assert!(!s.is_provisioned(Role::Central));
        assert!(!s.is_provisioned(Role::Peripheral));

        s.mark_provisioned(Role::Central).expect("marks");
        assert!(s.is_provisioned(Role::Central));
        assert!(
            !s.is_provisioned(Role::Peripheral),
            "the other role must be untouched"
        );

        s.clear_provisioned(Role::Central).expect("clears");
        assert!(!s.is_provisioned(Role::Central));
        // Idempotent — factory reset must be re-runnable.
        s.clear_provisioned(Role::Central)
            .expect("clearing twice is fine");
    }

    #[test]
    fn removing_an_absent_slot_is_success() {
        let t = TempDir::new("remove");
        t.store()
            .remove(Slot::Beacons)
            .expect("absent remove is success");
    }

    #[test]
    fn ensure_dirs_is_idempotent_and_creates_the_documented_tree() {
        let t = TempDir::new("dirs");
        let s = t.store();
        s.ensure_dirs().expect("first");
        s.ensure_dirs().expect("second");
        assert!(t.0.join("central").is_dir());
        assert!(t.0.join("central/archive").is_dir());
        assert!(t.0.join("peripheral").is_dir());
    }

    #[test]
    fn the_archive_path_is_one_file_per_beacon_and_path_safe() {
        let s = Store::at_default_root();
        let mac = MacAddr::parse("7c:d9:f4:13:10:de").expect("mac");
        assert_eq!(
            s.archive_path(&mac).display().to_string(),
            "/data/proximos/ble/central/archive/7C-D9-F4-13-10-DE.jsonl"
        );
        assert_eq!(
            s.archive_chunks_path(&mac).display().to_string(),
            "/data/proximos/ble/central/archive/7C-D9-F4-13-10-DE.chunks.jsonl"
        );
    }

    /// The shipped paths, spelled out — the unit file, the tmpfiles fragment and the Landlock table
    /// all name these literally, so a rename here is a cross-file change.
    #[test]
    fn the_shipped_layout_is_the_documented_one() {
        let s = Store::at_default_root();
        let p = |slot| s.path(slot).display().to_string();
        assert_eq!(p(Slot::Beacons), "/data/proximos/ble/central/beacons.json");
        assert_eq!(
            p(Slot::CentralProvisioned),
            "/data/proximos/ble/central/provisioned"
        );
        assert_eq!(p(Slot::ArchiveDir), "/data/proximos/ble/central/archive");
        assert_eq!(
            p(Slot::PeripheralProvisioned),
            "/data/proximos/ble/peripheral/provisioned"
        );
        // T0.2: there is no `pin` slot, and no path under the root spells one.
        for slot in Slot::ALL {
            assert!(!slot.rel().contains("pin"), "{slot:?}");
        }
    }

    #[test]
    fn only_the_beacon_record_is_a_secret() {
        assert!(Slot::Beacons.is_secret());
        assert!(!Slot::CentralProvisioned.is_secret());
        assert!(!Slot::ArchiveDir.is_secret());
        assert!(!Slot::PeripheralProvisioned.is_secret());
    }
}
