//! The `/data/proximos/radio/` layout — `plan/control/radio/lorawan.md` §4.
//!
//! ```text
//! /data/proximos/radio/            owner proximos, dirs created by NNN-radio-init.sh
//! ├── mqtt-password                0600, minted by the renderer/secret machinery
//! ├── node_registry.json           the one Node registry, 0600 — key material (node_registry.md)
//! ├── lorawan/
//! │   ├── config.json              converged ids (lorawan.md §5)
//! │   ├── nodes.json               enrollment records, 0600 — holds key material
//! │   ├── provisioned              marker: a gateway was actually registered
//! │   ├── chirpstack-api-token-admin
//! │   ├── chirpstack-api-token-tenant
//! │   └── chirpstack-api-token-readonly
//! ├── p2p/
//! │   ├── sessions.db              session DB + node registry
//! │   ├── tx_lead                  the learned modem TX lead
//! │   ├── net_id                   generated on the first `create --radio p2p`
//! │   └── provisioned              marker: the northbridge probe succeeded
//! └── northbridge/                 the Northbridge firmware update (northbridge_update.md §3.4)
//!     ├── update-state.json        the P2P task's record: state, retry budget, last result
//!     ├── update-request.json      `northbridge-update`'s request, a generation bump
//!     └── request/                 a `--file` hotfix, copied in by the CLI
//!         ├── northbridge.bin
//!         └── northbridge.json
//! ```
//!
//! Division of labour with the migration is exact: `NNN-radio-init.sh` creates the
//! **directories only**; `create` writes the contents. Neither step depends on the other
//! having produced file contents, which is what lets a factory-reset `/data` and a first boot
//! be the same code path.
//!
//! Two properties this module owns, because getting them wrong is silent:
//!
//! - **Writes are atomic.** A truncated `config.json` after a power cut would read as "never
//!   provisioned" and re-converge against a ChirpStack that already has the objects.
//! - **Secrets are 0600 and stay 0600**, including across a rewrite.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Component, Error, Kind, Result, Transport};

/// The component's state root. Not configurable: it is a `/data` contract, and the unit
/// file, the tmpfiles fragment and the Landlock path table all name it literally.
pub const DEFAULT_ROOT: &str = "/data/proximos/radio";

/// Mode for anything holding key material or a credential (`system/design.md` §10).
#[cfg(unix)]
pub const SECRET_MODE: u32 = 0o600;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// Which files a caller may ask for, so paths are never assembled from strings at call sites
/// — decision #6 requires an enumerable file surface, and this is what makes the Landlock and
/// AppArmor tables reviewable against the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    MqttPassword,
    LorawanConfig,
    LorawanNodes,
    LorawanProvisioned,
    TokenAdmin,
    TokenTenant,
    TokenReadonly,
    P2pSessions,
    P2pNetId,
    P2pProvisioned,
    /// The one Node registry, both carriers (`node_registry.md`).
    NodeRegistry,
    /// The P2P central's learned modem TX lead, so a restart does not aim its first ACK with a
    /// stale seed.
    P2pTxLead,
    /// The Northbridge update's own record (`northbridge_update.md` §3.4): written by the P2P
    /// task only, read by `northbridge-update` for its result.
    NbUpdateState,
    /// `northbridge-update`'s request: written by the CLI only, polled by the P2P task.
    NbUpdateRequest,
    /// A `--file` hotfix image and its manifest, copied in by the CLI: the service cannot read
    /// the caller's path (`PrivateTmp`, `ProtectHome`).
    NbRequestImage,
    NbRequestManifest,
}

impl Slot {
    const fn rel(self) -> &'static str {
        match self {
            Slot::MqttPassword => "mqtt-password",
            Slot::LorawanConfig => "lorawan/config.json",
            Slot::LorawanNodes => "lorawan/nodes.json",
            Slot::LorawanProvisioned => "lorawan/provisioned",
            Slot::TokenAdmin => "lorawan/chirpstack-api-token-admin",
            Slot::TokenTenant => "lorawan/chirpstack-api-token-tenant",
            Slot::TokenReadonly => "lorawan/chirpstack-api-token-readonly",
            Slot::P2pSessions => "p2p/sessions.db",
            Slot::P2pNetId => "p2p/net_id",
            Slot::P2pProvisioned => "p2p/provisioned",
            Slot::NodeRegistry => "node_registry.json",
            Slot::P2pTxLead => "p2p/tx_lead",
            Slot::NbUpdateState => "northbridge/update-state.json",
            Slot::NbUpdateRequest => "northbridge/update-request.json",
            Slot::NbRequestImage => "northbridge/request/northbridge.bin",
            Slot::NbRequestManifest => "northbridge/request/northbridge.json",
        }
    }

    /// Whether this slot holds a credential or key material.
    const fn is_secret(self) -> bool {
        matches!(
            self,
            Slot::MqttPassword
                | Slot::LorawanNodes
                // Nothing writes it yet, but the P2P session DB is where `app_key` and the
                // derived `session_key` land (p2p.md §7). Classifying it after something
                // starts writing it means the first write is the one that goes out 0644.
                | Slot::P2pSessions
                | Slot::NodeRegistry
                | Slot::TokenAdmin
                | Slot::TokenTenant
                | Slot::TokenReadonly
        )
    }

    const fn transport(self) -> Option<Transport> {
        match self {
            Slot::MqttPassword | Slot::NodeRegistry => None,
            Slot::LorawanConfig
            | Slot::LorawanNodes
            | Slot::LorawanProvisioned
            | Slot::TokenAdmin
            | Slot::TokenTenant
            | Slot::TokenReadonly => Some(Transport::Lorawan),
            Slot::P2pSessions
            | Slot::P2pNetId
            | Slot::P2pProvisioned
            | Slot::P2pTxLead
            | Slot::NbUpdateState
            | Slot::NbUpdateRequest
            | Slot::NbRequestImage
            | Slot::NbRequestManifest => Some(Transport::P2p),
        }
    }

    const fn component(self) -> Component {
        match self {
            Slot::MqttPassword => Component::MqttBroker,
            Slot::LorawanConfig => Component::ConfigJson,
            Slot::LorawanNodes => Component::EnrollmentRecord,
            Slot::LorawanProvisioned => Component::ConfigJson,
            Slot::TokenAdmin | Slot::TokenTenant | Slot::TokenReadonly => Component::TokenFile,
            Slot::P2pSessions | Slot::P2pTxLead => Component::SessionDb,
            Slot::P2pNetId | Slot::P2pProvisioned | Slot::NodeRegistry => Component::NodeRegistry,
            Slot::NbUpdateState
            | Slot::NbUpdateRequest
            | Slot::NbRequestImage
            | Slot::NbRequestManifest => Component::NorthbridgeFirmware,
        }
    }
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

    /// Read a slot, or `Ok(None)` when it is absent.
    ///
    /// Absent and unreadable are deliberately different: absent is "bootstrap has not run",
    /// unreadable is `StorageError`. Collapsing them would make a permissions mistake look
    /// like a fresh device and trigger a re-converge.
    pub fn read(&self, slot: Slot) -> Result<Option<String>> {
        match std::fs::read_to_string(self.path(slot)) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.storage_err(slot, format!("cannot read: {e}"))),
        }
    }

    /// [`read`](Self::read) for a binary slot.
    pub fn read_bytes(&self, slot: Slot) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(slot)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.storage_err(slot, format!("cannot read: {e}"))),
        }
    }

    /// Read a slot that must be present, mapping absence to `NotBootstrapped`.
    pub fn require(&self, slot: Slot) -> Result<String> {
        self.read(slot)?.ok_or_else(|| {
            Error::new(
                Kind::NotBootstrapped,
                slot.transport(),
                slot.component(),
                format!(
                    "{} is absent: bootstrap has not run, or /data was wiped",
                    slot.rel()
                ),
            )
        })
    }

    /// Hold an advisory exclusive lock over one slot, for a read-modify-write that must not
    /// interleave with another process's.
    ///
    /// [`write`](Self::write) is atomic *per writer* — temp file, then `rename`. That is not the
    /// same as exclusive: when two processes each load, mutate and save, the second one loaded
    /// before the first committed, so the first update is lost. The P2P session DB has exactly
    /// that shape and exactly two writers — the radio runtime's event loop and a `control.radio`
    /// CLI process — which is why a `node-send` issued while the host was running used to
    /// disappear.
    ///
    /// The lock is `flock(2)` on a sidecar `<slot>.lock`, deliberately not on the slot file: the
    /// slot is *replaced* by `rename`, so a lock held on its inode would stop guarding it the
    /// moment the first writer committed. It is advisory, which is the right strength here —
    /// readers that do not ask still see a complete document, because `rename` is atomic; they
    /// may simply see the previous one.
    pub fn lock(&self, slot: Slot) -> Result<SlotLock> {
        let path = self.path(slot).with_extension("lock");
        self.create_parent(slot, &path)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true).truncate(false);
        // The lock file sits beside key material; it carries none itself, but there is no reason
        // for it to be the one loose mode in the directory.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(SECRET_MODE);
        }
        let file = opts.open(&path).map_err(|e| {
            self.storage_err(
                slot,
                format!("cannot open lock file {}: {e}", path.display()),
            )
        })?;
        // A lock file a root CLI created must stay openable by the `proximos` CLI after it.
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            let _ = adopt_dir_owner(&path, dir);
        }
        #[cfg(unix)]
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|e| self.storage_err(slot, format!("cannot lock {}: {e}", path.display())))?;
        Ok(SlotLock { _file: file })
    }

    /// Write a slot atomically, creating parent directories if the migration has not run.
    ///
    /// Atomic because a torn `config.json` reads as "never provisioned": the content goes to
    /// a temp file in the same directory (so `rename` cannot cross a filesystem), is flushed,
    /// and only then replaces the target.
    pub fn write(&self, slot: Slot, contents: &str) -> Result<()> {
        self.write_bytes(slot, contents.as_bytes())
    }

    /// [`write`](Self::write) for a binary slot (a `--file` hotfix image).
    pub fn write_bytes(&self, slot: Slot, contents: &[u8]) -> Result<()> {
        let staged = self.stage(slot, contents)?;
        staged.sync()?;
        staged.commit()
    }

    /// The first half of [`write`](Self::write): the contents in the temp file beside the slot,
    /// not yet flushed and not yet in place. [`StagedWrite::sync`] flushes them,
    /// [`StagedWrite::commit`] renames them over the slot and syncs the directory.
    ///
    /// Split so a caller storing several files can write them all before flushing any: the
    /// flushes then follow each other and a journaling filesystem serves them with one commit
    /// rather than one each (#130, the P2P central's outbox entry and `sessions.db`). Dropped
    /// uncommitted, the temp file is removed and the slot keeps what it had.
    pub fn stage(&self, slot: Slot, contents: &[u8]) -> Result<StagedWrite<'_>> {
        let path = self.path(slot);
        self.create_parent(slot, &path)?;

        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| self.storage_err(slot, format!("cannot create a temp file: {e}")))?;
        let written = set_mode(&f, slot, self, &tmp).and_then(|()| {
            f.write_all(contents)
                .map_err(|e| self.storage_err(slot, format!("cannot write: {e}")))
        });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(StagedWrite {
            store: self,
            slot,
            path,
            tmp,
            file: Some(f),
        })
    }

    /// Remove a slot. Absence is success — `wipe` and `clear` must be re-runnable.
    pub fn remove(&self, slot: Slot) -> Result<()> {
        match std::fs::remove_file(self.path(slot)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.storage_err(slot, format!("cannot remove: {e}"))),
        }
    }

    /// Write a per-arm `provisioned` marker.
    ///
    /// Withheld on degraded-green: LoRaWAN writes it only when a gateway was actually
    /// registered, P2P only when the northbridge probe succeeded. Degraded-green and
    /// provisioned-complete are two different states and this records the second one.
    pub fn mark_provisioned(&self, transport: Transport) -> Result<()> {
        self.write(provisioned_slot(transport), "")
    }

    pub fn is_provisioned(&self, transport: Transport) -> bool {
        self.exists(provisioned_slot(transport))
    }

    pub fn clear_provisioned(&self, transport: Transport) -> Result<()> {
        self.remove(provisioned_slot(transport))
    }

    /// Create the missing directories between the root and `path`, each taking the owner of
    /// its parent. The migration creates only `lorawan/` and `p2p/`; a directory a root CLI
    /// creates first (`northbridge/request/` from `northbridge-update --file`) would otherwise
    /// stay root-owned, and the `proximos` service could not write beside it.
    fn create_parent(&self, slot: Slot, path: &Path) -> Result<()> {
        let Some(dir) = path.parent() else {
            return Ok(());
        };
        if dir.is_dir() {
            return Ok(());
        }
        let missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.exists()).collect();
        std::fs::create_dir_all(dir)
            .map_err(|e| self.storage_err(slot, format!("cannot create {}: {e}", dir.display())))?;
        #[cfg(unix)]
        for d in missing.into_iter().rev() {
            if let Some(parent) = d.parent() {
                // Best effort: only root can give a directory away, and only a root writer
                // needs to.
                let _ = adopt_dir_owner(d, parent);
            }
        }
        #[cfg(not(unix))]
        let _ = missing;
        Ok(())
    }

    fn storage_err(&self, slot: Slot, why: String) -> Error {
        Error::new(
            Kind::StorageError,
            slot.transport(),
            slot.component(),
            format!("{}: {why}", self.path(slot).display()),
        )
    }
}

/// An exclusive advisory lock over one [`Slot`], released when it is dropped.
///
/// Returned by [`Store::lock`]. Bind it to a name for as long as the critical section lasts —
/// `let _ = store.lock(..)?` drops it on the spot and locks nothing, which is why the callers
/// spell it `let _db_lock = ..`. Holding it across an `.await` holds it against every other
/// process too, so keep the critical section to the load-mutate-save itself.
#[must_use = "the lock is released the moment the guard is dropped"]
pub struct SlotLock {
    _file: std::fs::File,
}

/// A slot write in its temp file ([`Store::stage`]): flushed by [`Self::sync`], put in place by
/// [`Self::commit`], removed when dropped before that.
#[derive(Debug)]
pub struct StagedWrite<'a> {
    store: &'a Store,
    slot: Slot,
    path: PathBuf,
    tmp: PathBuf,
    file: Option<std::fs::File>,
}

impl StagedWrite<'_> {
    /// Flush the contents to the disk. Idempotent; [`Self::commit`] does it if not done yet.
    pub fn sync(&self) -> Result<()> {
        if let Some(f) = &self.file {
            f.sync_all().map_err(|e| {
                self.store
                    .storage_err(self.slot, format!("cannot flush: {e}"))
            })?;
        }
        Ok(())
    }

    /// Replace the slot with the staged contents, durably: flushed, renamed, and the directory
    /// synced.
    pub fn commit(mut self) -> Result<()> {
        self.sync()?;
        self.file = None;
        let (store, slot) = (self.store, self.slot);
        std::fs::rename(&self.tmp, &self.path).map_err(|e| {
            store.storage_err(slot, format!("cannot replace {}: {e}", self.path.display()))
        })?;
        // In place: nothing left for `drop` to clean up.
        let path = std::mem::take(&mut self.path);
        self.tmp = PathBuf::new();
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            adopt_dir_owner(&path, dir)
                .map_err(|e| store.storage_err(slot, format!("cannot set the owner: {e}")))?;
        }

        // The file's own contents are durable (`sync_all` above), but the rename that gives
        // them their name lives in the parent directory, and that entry is not durable until
        // the directory itself is synced. Without this a power cut right after a write can
        // leave the old file — or no file — despite the write having "succeeded", which for
        // `config.json` or a token file is a Hub that comes back up unprovisioned.
        if let Some(dir) = path.parent() {
            std::fs::File::open(dir)
                .and_then(|d| d.sync_all())
                .map_err(|e| {
                    store.storage_err(slot, format!("cannot flush {}: {e}", dir.display()))
                })?;
        }
        Ok(())
    }
}

impl Drop for StagedWrite<'_> {
    fn drop(&mut self) {
        // Leave no debris behind on a write that was not put in place.
        if !self.tmp.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// A file the store writes takes the owner of its directory.
///
/// The tree is `proximos:proximos` (`010-radio-init.sh`) and every writer is meant to run as
/// `proximos`. Until decision #9's monolith lands, the agent that executes the portal's
/// signed `node-add` is still `fiber.service`, which runs as root — and a root-owned 0600
/// `nodes.json` is unreadable to `proximosctl` and `proximos-control` running as `proximos`.
/// A no-op when the writer already is the directory's owner, which is every other caller.
#[cfg(unix)]
fn adopt_dir_owner(path: &Path, dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let owner = std::fs::metadata(dir)?;
    let file = std::fs::metadata(path)?;
    if file.uid() == owner.uid() && file.gid() == owner.gid() {
        return Ok(());
    }
    std::os::unix::fs::chown(path, Some(owner.uid()), Some(owner.gid()))
}

const fn provisioned_slot(t: Transport) -> Slot {
    match t {
        Transport::Lorawan => Slot::LorawanProvisioned,
        Transport::P2p => Slot::P2pProvisioned,
    }
}

/// Secrets are created 0600 and stay 0600 across a rewrite.
///
/// The mode is set on the temp file **before** any content is written, so the secret is never
/// briefly world-readable — a rename cannot widen it afterwards.
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
    /// F14: `p2p/sessions.db` will hold `app_key` and the derived `session_key`, so it has to
    /// be classified secret before anything writes it — not after.
    #[test]
    fn the_p2p_session_db_is_secret() {
        assert!(super::Slot::P2pSessions.is_secret());
    }

    use super::*;

    fn temp_store() -> (Store, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "control-radio-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        (Store::new(&base), base)
    }

    #[test]
    fn write_then_read_round_trips_and_creates_parents() {
        let (s, base) = temp_store();
        s.write(Slot::LorawanConfig, "{}").expect("writes");
        assert_eq!(
            s.read(Slot::LorawanConfig).expect("reads"),
            Some("{}".to_string())
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn an_absent_slot_reads_as_none_but_requires_as_not_bootstrapped() {
        let (s, base) = temp_store();
        assert_eq!(s.read(Slot::TokenAdmin).expect("reads"), None);
        let err = s
            .require(Slot::TokenAdmin)
            .expect_err("must be NotBootstrapped");
        assert_eq!(err.kind(), Kind::NotBootstrapped);
        assert_eq!(err.component(), Component::TokenFile);
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn secrets_are_0600_on_create_and_stay_0600_on_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let (s, base) = temp_store();
        for slot in [
            Slot::TokenAdmin,
            Slot::TokenTenant,
            Slot::TokenReadonly,
            Slot::LorawanNodes,
            Slot::MqttPassword,
        ] {
            s.write(slot, "secret-v1").expect("writes");
            let mode = std::fs::metadata(s.path(slot))
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{slot:?} on create");

            s.write(slot, "secret-v2").expect("rewrites");
            let mode = std::fs::metadata(s.path(slot))
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{slot:?} after rewrite");
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_failed_write_leaves_no_temp_file_behind() {
        let (s, base) = temp_store();
        s.write(Slot::LorawanConfig, "{}").expect("writes");
        let leftovers: Vec<_> = std::fs::read_dir(base.join("lorawan"))
            .expect("dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// #130: a staged write is invisible until committed; dropped uncommitted, it leaves the
    /// slot as it was and no temp file behind.
    #[test]
    fn a_staged_write_lands_only_on_commit() {
        let (s, base) = temp_store();
        s.write(Slot::P2pSessions, "v1").expect("writes");
        let staged = s.stage(Slot::P2pSessions, b"v2").expect("stages");
        assert_eq!(
            s.read(Slot::P2pSessions).expect("reads").as_deref(),
            Some("v1")
        );
        staged.sync().expect("syncs");
        assert_eq!(
            s.read(Slot::P2pSessions).expect("reads").as_deref(),
            Some("v1")
        );
        staged.commit().expect("commits");
        assert_eq!(
            s.read(Slot::P2pSessions).expect("reads").as_deref(),
            Some("v2")
        );

        drop(s.stage(Slot::P2pSessions, b"v3").expect("stages"));
        assert_eq!(
            s.read(Slot::P2pSessions).expect("reads").as_deref(),
            Some("v2")
        );
        let tmp = s.path(Slot::P2pSessions).with_extension("tmp");
        assert!(!tmp.exists(), "the dropped stage left {}", tmp.display());
        let _ = std::fs::remove_dir_all(base);
    }

    /// Markers are per-arm: one arm can be provisioned-complete while the other stays
    /// degraded-green and re-converges next boot.
    #[test]
    fn provisioned_markers_are_per_arm() {
        let (s, base) = temp_store();
        assert!(!s.is_provisioned(Transport::Lorawan));
        assert!(!s.is_provisioned(Transport::P2p));

        s.mark_provisioned(Transport::Lorawan).expect("marks");
        assert!(s.is_provisioned(Transport::Lorawan));
        assert!(
            !s.is_provisioned(Transport::P2p),
            "the other arm must be untouched"
        );

        s.clear_provisioned(Transport::Lorawan).expect("clears");
        assert!(!s.is_provisioned(Transport::Lorawan));
        // Idempotent — wipe and clear must be re-runnable.
        s.clear_provisioned(Transport::Lorawan)
            .expect("clearing twice is fine");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn removing_an_absent_slot_is_success() {
        let (s, base) = temp_store();
        s.remove(Slot::LorawanConfig)
            .expect("absent remove is success");
        let _ = std::fs::remove_dir_all(base);
    }

    /// The shipped paths, spelled out — the unit file, the tmpfiles fragment and the Landlock
    /// table all name these literally, so a rename here is a cross-file change.
    #[test]
    fn the_shipped_layout_is_the_documented_one() {
        let s = Store::at_default_root();
        let p = |slot| s.path(slot).display().to_string();
        assert_eq!(
            p(Slot::LorawanConfig),
            "/data/proximos/radio/lorawan/config.json"
        );
        assert_eq!(
            p(Slot::LorawanNodes),
            "/data/proximos/radio/lorawan/nodes.json"
        );
        assert_eq!(
            p(Slot::LorawanProvisioned),
            "/data/proximos/radio/lorawan/provisioned"
        );
        assert_eq!(
            p(Slot::TokenAdmin),
            "/data/proximos/radio/lorawan/chirpstack-api-token-admin"
        );
        assert_eq!(
            p(Slot::TokenTenant),
            "/data/proximos/radio/lorawan/chirpstack-api-token-tenant"
        );
        assert_eq!(
            p(Slot::TokenReadonly),
            "/data/proximos/radio/lorawan/chirpstack-api-token-readonly"
        );
        assert_eq!(p(Slot::P2pSessions), "/data/proximos/radio/p2p/sessions.db");
        assert_eq!(p(Slot::P2pNetId), "/data/proximos/radio/p2p/net_id");
        assert_eq!(
            p(Slot::P2pProvisioned),
            "/data/proximos/radio/p2p/provisioned"
        );
        assert_eq!(p(Slot::MqttPassword), "/data/proximos/radio/mqtt-password");
        // Under the unit's one ReadWritePaths row, not /data/proximos/northbridge.
        assert_eq!(
            p(Slot::NbUpdateState),
            "/data/proximos/radio/northbridge/update-state.json"
        );
        assert_eq!(
            p(Slot::NbUpdateRequest),
            "/data/proximos/radio/northbridge/update-request.json"
        );
        assert_eq!(
            p(Slot::NbRequestImage),
            "/data/proximos/radio/northbridge/request/northbridge.bin"
        );
        assert_eq!(
            p(Slot::NbRequestManifest),
            "/data/proximos/radio/northbridge/request/northbridge.json"
        );
    }

    #[test]
    fn a_binary_slot_round_trips() {
        let (s, base) = temp_store();
        let bytes = [0u8, 0xFF, 0x7E, 0x7D, 0x00];
        s.write_bytes(Slot::NbRequestImage, &bytes).expect("writes");
        assert_eq!(
            s.read_bytes(Slot::NbRequestImage).expect("reads"),
            Some(bytes.to_vec())
        );
        assert_eq!(s.read_bytes(Slot::NbRequestManifest).expect("reads"), None);
        let _ = std::fs::remove_dir_all(base);
    }
}
