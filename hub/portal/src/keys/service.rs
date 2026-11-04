//! `KeyService`: custody of the device CA seed (handed over at pairing)
//! and of per-user Ed25519 signing keys + certificates minted from it.
//!
//! Opens its own [`Storage`] connection on the shared `data_dir`/`db_key`
//! (connection #4, after persist / auth / read-db) — another SQLite
//! connection under WAL is fine, same reasoning as `auth::AuthService`.
//!
//! # Key wrapping (at-rest format)
//!
//! Seeds are never stored in plaintext. Wrap key: `SHA-256("fiber-viewer-
//! keywrap" || db_key_utf8)` when the config has a `db_key`; in dev
//! (`db_key: None`) the same derivation runs over 16 zero bytes instead —
//! a fixed, worthless key, but it keeps the storage format identical and
//! blobs opaque even in dev (logged as a warning at open). Blob layout:
//! `12-byte random nonce || AES-256-GCM ciphertext+tag`; fresh nonce per
//! wrap, no AAD. Plaintext is always exactly the 32-byte Ed25519 seed.
//!
//! # Certificate re-minting
//!
//! [`KeyService::ensure_user_key`] re-mints the user certificate (with the
//! union of stored ∪ requested permissions) whenever the stored one lacks
//! a requested permission. This replaces v1's `409
//! certificate_missing_permission` refresh dance for phase 1 — cheap and
//! safe here because the portal holds the device CA seed anyway. The
//! signing keypair itself is reused, only the certificate changes.

use std::path::Path;
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use zeroize::Zeroizing;

use fiber_protocol::certificate::{sign_certificate, Certificate, CertificatePayload};
use fiber_protocol::pairing::derive_public_hex;
use fiber_protocol::Level;
use portal_storage::{
    NewDeviceIdentity, NewServiceDeviceKey, NewUserDeviceKey, Storage, StorageError,
};

use crate::auth::CurrentUser;

/// Domain-separation prefix for the wrap-key derivation.
///
/// **Keeps the pre-rename `fiber-viewer` spelling on purpose.** This byte
/// string is an input to the wrap key, so changing it makes every seed
/// already sealed on disk undecryptable — the device CA seed included, with
/// no error until something tries to sign. The component is named portal; this
/// constant is a stored-data format and does not follow it.
const WRAP_CONTEXT: &[u8] = b"fiber-viewer-keywrap";
/// AES-GCM standard 96-bit nonce, prepended to every wrapped blob.
const NONCE_LEN: usize = 12;
/// User certificates are minted with a 1-year validity (v1 parity).
const USER_CERT_TTL_DAYS: i64 = 365;

/// Errors surfaced by [`KeyService`].
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("device '{0}' is not paired")]
    NotPaired(String),
    #[error("key wrap/unwrap failed: {0}")]
    Crypto(String),
    #[error("certificate error: {0}")]
    Certificate(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// What [`KeyService::device_identity`] exposes about a paired device —
/// deliberately no seed material, wrapped or otherwise.
#[derive(Debug, Clone)]
pub struct DeviceIdentityInfo {
    pub hostname: String,
    pub has_ca: bool,
    pub ca_id: Option<String>,
    /// 64 hex chars — the device CA's Ed25519 public key.
    pub ca_public_key: String,
    pub paired_by_user_id: Option<i64>,
    pub paired_at: String,
}

/// A user's unwrapped signing material for one device, ready for the
/// envelope signer. `seed` zeroizes on drop — don't copy it around.
pub struct UserSigningKey {
    /// `"user-{id}-{username}"`.
    pub signer_id: String,
    pub seed: Zeroizing<[u8; 32]>,
    /// The minted certificate as it rides on signed commands.
    pub certificate: serde_json::Value,
    /// The highest [`Level`] this signer's **current** role may reach
    /// (`perms::level_ceiling`), stamped at mint time.
    ///
    /// It rides on the key rather than being passed to `relay::execute`
    /// separately so that the role floor cannot be forgotten at a call
    /// site: there is no way to execute a command without a key, and the
    /// key was minted from the live role a moment earlier. It is
    /// deliberately *not* read back off the certificate — `ensure_user_key`
    /// re-mints with the union and never removes a permission, so a
    /// demoted user's certificate still carries the old `level:*` strings
    /// (design.md §4.4 step 2).
    pub ceiling: Level,
}

/// The `signer_id` of the portal's own background signer (design.md §5.4).
pub const PORTAL_SERVICE_SIGNER: &str = "portal-service";

/// Exactly what that signer may do: read a Node's history, and nothing
/// above `read`. Kept as a literal pair rather than derived from the
/// registry so that widening it is a deliberate edit here, visible in a
/// diff, rather than a side effect of adding a registry row.
const PORTAL_SERVICE_PERMISSIONS: [&str; 2] = ["get_status", "level:read"];

/// The `signer_id` of the configuration-sync fallback signer (§13 Q8).
pub const PORTAL_SYNC_SIGNER: &str = "portal-sync";

/// `portal-sync`'s permissions: each Hub/Beacon/Node desired verb's
/// permission and the `level:*` of its level, deduplicated and sorted so the
/// stored list compares equal across restarts. Derived from
/// `fiber_protocol::DESIRED_VERBS` on purpose: the list *is* "the sync verbs".
pub fn portal_sync_permissions() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for verb in fiber_protocol::config_sync::DESIRED_VERBS {
        if let Some(spec) = fiber_protocol::lookup(verb) {
            out.push(spec.permission.to_string());
            out.push(spec.level.permission());
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The `signer_id` of the cluster radio follower (decision #47): the Server's portal sets a
/// Client's radio mode to its own with no operator in the loop.
pub const PORTAL_CLUSTER_SIGNER: &str = "portal-cluster";
/// Exactly `radio-set-mode`'s permission and tier, literal for the same reason as
/// [`PORTAL_SERVICE_PERMISSIONS`].
const PORTAL_CLUSTER_PERMISSIONS: [&str; 2] = ["radio.manage", "level:admin"];

/// The ceiling stamped on the service key — the live-role equivalent for a
/// signer that has no role.
const PORTAL_SERVICE_CEILING: Level = Level::Read;

/// The background signer for the automatic Beacon archive download
/// (plan/portal/beacon_history.md §4.2). A signer of its own rather than a
/// wider [`PORTAL_SERVICE_PERMISSIONS`]: `beacon-history` needs
/// `beacon.manage`, and the Node backfill key has no business carrying it.
/// At [`Level::Read`] the only verb `beacon.manage` reaches is
/// `beacon-history` — every other Beacon verb is `manage` or above, which
/// the ceiling refuses before signing and the device refuses again for want
/// of `level:manage`.
pub const PORTAL_BEACON_SERVICE_SIGNER: &str = "portal-service-beacon";
const PORTAL_BEACON_SERVICE_PERMISSIONS: [&str; 2] = ["beacon.manage", "level:read"];

#[derive(Clone)]
pub struct KeyService {
    db: crate::db::Db,
    wrap_key: Arc<Zeroizing<[u8; 32]>>,
}

impl KeyService {
    /// Opens (creating if needed) `<data_dir>/portal.db` — the same path
    /// the persist/auth/read-db connections use — and derives the wrap key
    /// from `db_key` (see module docs for the dev fallback).
    pub fn open(data_dir: &str, db_key: Option<&str>) -> Result<Self, StorageError> {
        std::fs::create_dir_all(data_dir)?;
        let path = Path::new(data_dir).join("portal.db");
        let storage = Storage::open(&path, db_key)?;

        let wrap_key = match db_key {
            Some(key) => derive_wrap_key(key.as_bytes()),
            None => {
                tracing::warn!(
                    "no db_key configured; signing keys are wrapped under a fixed dev key \
                     (dev mode only — provides no confidentiality)"
                );
                derive_wrap_key(&[0u8; 16])
            }
        };

        Ok(Self {
            db: crate::db::Db::from_storage(storage, "keys"),
            wrap_key: Arc::new(wrap_key),
        })
    }

    /// Persist what the device handed over at pairing (mqtt.md "Pairing"):
    /// the decrypted CA seed (wrapped here before it touches disk), the
    /// CA public key/id, and the admin certificate. Re-pairing overwrites.
    pub async fn store_pairing(
        &self,
        hostname: &str,
        ca_id: Option<String>,
        ca_public_key_hex: String,
        ca_seed: [u8; 32],
        admin_certificate_json: Option<serde_json::Value>,
        paired_by: Option<i64>,
    ) -> Result<(), KeyError> {
        let ca_seed = Zeroizing::new(ca_seed);
        let ca_seed_enc = self.wrap(ca_seed.as_ref())?;

        let row = NewDeviceIdentity {
            hostname: hostname.to_string(),
            ca_id,
            ca_public_key: ca_public_key_hex,
            ca_seed_enc,
            admin_certificate: admin_certificate_json.map(|v| v.to_string()),
            paired_by_user_id: paired_by,
        };
        // Retry, not spill: pairing is request-scoped and the handler already
        // redirects with `store_failed` on failure, so the operator is told and
        // can click again. Retrying only stops a maintenance run from being the
        // reason a pairing needs a second click.
        self.db
            .run(move |s| portal_storage::with_retry(|| s.upsert_device_identity(&row)))
            .await?;
        Ok(())
    }

    /// Pairing status for a device — no seed exposure, wrapped or not.
    pub async fn device_identity(
        &self,
        hostname: &str,
    ) -> Result<Option<DeviceIdentityInfo>, KeyError> {
        let host = hostname.to_string();
        let row = self.db.run(move |s| s.get_device_identity(&host)).await?;
        Ok(row.map(|r| DeviceIdentityInfo {
            hostname: r.hostname,
            has_ca: !r.ca_seed_enc.is_empty(),
            ca_id: r.ca_id,
            ca_public_key: r.ca_public_key,
            paired_by_user_id: r.paired_by_user_id,
            paired_at: r.paired_at,
        }))
    }

    /// Whether `hostname` is paired, for a page that has to say so before anything is signed
    /// (GitLab #109) and for backfill's decision #18 gate. The identity is looked up under the
    /// spelling given and its lower and upper case, since a Hub arrives under more than one
    /// (NT-4). A lookup that fails reads as "not paired": the page then says what every signed
    /// command to that Hub would run into just then.
    pub async fn is_paired(&self, hostname: &str) -> bool {
        for spelling in [
            hostname.to_string(),
            hostname.to_ascii_lowercase(),
            hostname.to_ascii_uppercase(),
        ] {
            if matches!(self.device_identity(&spelling).await, Ok(Some(_))) {
                return true;
            }
        }
        false
    }

    /// Unpair: portal-local only (mqtt.md quirk #8 — no on-device
    /// revocation exists in the protocol). Deletes the device identity and
    /// every user key minted under its CA. [`KeyError::NotPaired`] if the
    /// hostname wasn't paired.
    pub async fn unpair(&self, hostname: &str) -> Result<(), KeyError> {
        let host = hostname.to_string();
        let deleted = self
            .db
            .run(move |s| {
                let deleted = s.delete_device_identity(&host)?;
                for key in s.list_user_device_keys(&host)? {
                    s.delete_user_device_key(key.user_id, &host)?;
                }
                // The portal's own background signer goes with the pairing
                // too: its certificate was minted under the CA that is
                // being dropped, so leaving the row would only preserve a
                // key nothing can verify.
                s.delete_service_device_keys(&host)?;
                Ok(deleted)
            })
            .await?;
        if !deleted {
            return Err(KeyError::NotPaired(hostname.to_string()));
        }
        Ok(())
    }

    /// Returns `user`'s signing key + certificate for `hostname`, minting
    /// them on first use: fresh random seed, certificate signed by the
    /// device CA with `permissions`, role from `user.user_role`, 1-year
    /// validity. Subsequent calls unwrap and return the stored key; if the
    /// stored certificate is missing any requested permission it is
    /// re-minted with the union (see module docs), reusing the keypair.
    pub async fn ensure_user_key(
        &self,
        user: &CurrentUser,
        hostname: &str,
        permissions: &[String],
    ) -> Result<UserSigningKey, KeyError> {
        let signer_id = format!("user-{}-{}", user.id, user.username);

        let host = hostname.to_string();
        let user_id = user.id;
        let existing = self
            .db
            .run(move |s| s.get_user_device_key(user_id, &host))
            .await?;

        if let Some(row) = existing {
            let stored: Vec<String> = serde_json::from_str(&row.permissions).map_err(|err| {
                KeyError::Certificate(format!("stored permissions unparseable: {err}"))
            })?;
            let seed = self.unwrap_seed(&row.seed_enc)?;

            if permissions.iter().all(|p| stored.contains(p)) {
                let certificate = serde_json::from_str(&row.certificate).map_err(|err| {
                    KeyError::Certificate(format!("stored certificate unparseable: {err}"))
                })?;
                return Ok(UserSigningKey {
                    signer_id,
                    seed,
                    certificate,
                    ceiling: crate::perms::level_ceiling(user),
                });
            }

            // Requested ⊄ stored: re-mint with the union, same keypair.
            let mut merged = stored;
            for p in permissions {
                if !merged.contains(p) {
                    merged.push(p.clone());
                }
            }
            return self
                .mint_and_store(user, hostname, signer_id, seed, merged)
                .await;
        }

        // First use for this (user, device): fresh random seed.
        let mut seed = Zeroizing::new([0u8; 32]);
        OsRng.fill_bytes(seed.as_mut());
        self.mint_and_store(user, hostname, signer_id, seed, permissions.to_vec())
            .await
    }

    /// The portal's own background signer (plan/portal/mqtt/design.md
    /// §5.4).
    ///
    /// Backfill has no user behind it — it is a loop, and there is nobody
    /// to attribute its `get_sticker_history` to. Rather than keep the
    /// registry bypass it had (a flat-dialect publish that skipped the
    /// envelope entirely, which decision #10 forbids), it signs as
    /// [`PORTAL_SERVICE_SIGNER`]: one Ed25519 key minted under the same
    /// portal-held device CA the users' keys are minted under, carrying
    /// **exactly** `get_status` and `level:read`. The device trusts it
    /// through the CA it already trusts, and the audit rows on the Hub
    /// carry `signer_id: portal-service` rather than nothing at all.
    ///
    /// The narrow permission list is the point: this key is reachable from
    /// a background task with no operator in the loop, so it must not be
    /// able to do anything but read. Its ceiling is [`Level::Read`], so
    /// `relay::execute` refuses anything above `read` with it before
    /// signing, and the device refuses it again at step 8 for want of
    /// `level:operate`.
    ///
    /// Minted on first use and reused after, like a user key. Unlike one,
    /// it is never re-minted with a union: the permission list is fixed in
    /// code, and a stored key whose list has drifted from it is replaced.
    pub async fn ensure_service_key(&self, hostname: &str) -> Result<UserSigningKey, KeyError> {
        self.ensure_fixed_service_key(
            hostname,
            PORTAL_SERVICE_SIGNER,
            &PORTAL_SERVICE_PERMISSIONS,
            PORTAL_SERVICE_CEILING,
        )
        .await
    }

    /// [`Self::ensure_service_key`]'s twin for the automatic Beacon archive
    /// download: [`PORTAL_BEACON_SERVICE_SIGNER`], same CA, same ceiling.
    pub async fn ensure_beacon_service_key(
        &self,
        hostname: &str,
    ) -> Result<UserSigningKey, KeyError> {
        self.ensure_fixed_service_key(
            hostname,
            PORTAL_BEACON_SERVICE_SIGNER,
            &PORTAL_BEACON_SERVICE_PERMISSIONS,
            PORTAL_SERVICE_CEILING,
        )
        .await
    }

    /// The `portal-sync` signer (plan/portal/config_sync.md §13 Q8): signs
    /// a desired-document item whose editor's account is gone or disabled.
    /// Same CA as every user certificate; it carries only the permissions
    /// of the desired verbs plus their `level:*` strings, so the Hub accepts
    /// it with no change on its side.
    pub async fn ensure_sync_service_key(
        &self,
        hostname: &str,
    ) -> Result<UserSigningKey, KeyError> {
        let owned = portal_sync_permissions();
        let permissions: Vec<&str> = owned.iter().map(String::as_str).collect();
        self.ensure_fixed_service_key(hostname, PORTAL_SYNC_SIGNER, &permissions, Level::Manage)
            .await
    }

    /// The `portal-cluster` signer (decision #47): signs only the `radio-set-mode` that brings a
    /// Client to the Server's radio mode.
    pub async fn ensure_cluster_service_key(
        &self,
        hostname: &str,
    ) -> Result<UserSigningKey, KeyError> {
        self.ensure_fixed_service_key(
            hostname,
            PORTAL_CLUSTER_SIGNER,
            &PORTAL_CLUSTER_PERMISSIONS,
            Level::Admin,
        )
        .await
    }

    /// A background signer whose permission list is fixed in code: minted
    /// on first use, reused after, replaced when the stored list drifted.
    async fn ensure_fixed_service_key(
        &self,
        hostname: &str,
        signer: &str,
        permissions: &[&str],
        ceiling: Level,
    ) -> Result<UserSigningKey, KeyError> {
        let signer_id = signer.to_string();
        let wanted: Vec<String> = permissions.iter().map(|p| p.to_string()).collect();

        let host = hostname.to_string();
        let lookup_signer = signer_id.clone();
        let existing = self
            .db
            .run(move |s| s.get_service_device_key(&lookup_signer, &host))
            .await?;

        if let Some(row) = existing {
            let stored: Vec<String> = serde_json::from_str(&row.permissions).map_err(|err| {
                KeyError::Certificate(format!("stored permissions unparseable: {err}"))
            })?;
            let seed = self.unwrap_seed(&row.seed_enc)?;
            if stored == wanted {
                let certificate = serde_json::from_str(&row.certificate).map_err(|err| {
                    KeyError::Certificate(format!("stored certificate unparseable: {err}"))
                })?;
                return Ok(UserSigningKey {
                    signer_id,
                    seed,
                    certificate,
                    ceiling,
                });
            }
            // Exact set, not a union: this signer's list is fixed in code.
            return self
                .mint_service_key(hostname, signer_id, seed, wanted, ceiling)
                .await;
        }

        let mut seed = Zeroizing::new([0u8; 32]);
        OsRng.fill_bytes(seed.as_mut());
        self.mint_service_key(hostname, signer_id, seed, wanted, ceiling)
            .await
    }

    async fn mint_service_key(
        &self,
        hostname: &str,
        signer_id: String,
        seed: Zeroizing<[u8; 32]>,
        permissions: Vec<String>,
        ceiling: Level,
    ) -> Result<UserSigningKey, KeyError> {
        let public_key_hex = derive_public_hex(&seed);
        let ca_seed = self.ca_seed(hostname).await?;
        let (issued_at, expires_at) = certificate_validity()?;

        let payload = CertificatePayload {
            signer_id: signer_id.clone(),
            full_name: format!("{} portal (background)", proximos_brand::PRODUCT),
            // Not one of the three user roles: the device reads `role` as a
            // plain string and never gates on it, and calling this signer a
            // "viewer" would put a non-user into every role-shaped reading
            // of the audit trail.
            role: "service".to_string(),
            public_key_ed25519: public_key_hex.clone(),
            permissions: permissions.clone(),
            issued_at,
            expires_at,
            issuer: "device_ca".to_string(),
        };
        let certificate_signature = sign_certificate(&ca_seed, &payload);
        let certificate = serde_json::to_value(&Certificate {
            payload,
            certificate_signature,
        })
        .map_err(|err| KeyError::Certificate(format!("certificate serialization: {err}")))?;

        let permissions_json = serde_json::to_string(&permissions)
            .map_err(|err| KeyError::Certificate(format!("permissions serialization: {err}")))?;
        let fingerprint = fingerprint_of_hex_key(&public_key_hex);
        let row = NewServiceDeviceKey {
            signer_id: signer_id.clone(),
            hostname: hostname.to_string(),
            seed_enc: self.wrap(seed.as_ref())?,
            public_key_hex,
            fingerprint,
            certificate: certificate.to_string(),
            permissions: permissions_json,
        };
        self.db
            .run(move |s| s.upsert_service_device_key(&row))
            .await?;

        Ok(UserSigningKey {
            signer_id,
            seed,
            certificate,
            ceiling,
        })
    }

    /// Unwraps and returns the device CA seed — for the relay signing
    /// admin/bootstrap confirms, if ever needed. [`KeyError::NotPaired`]
    /// if the hostname has no stored identity.
    pub async fn ca_seed(&self, hostname: &str) -> Result<Zeroizing<[u8; 32]>, KeyError> {
        let host = hostname.to_string();
        let row = self
            .db
            .run(move |s| s.get_device_identity(&host))
            .await?
            .ok_or_else(|| KeyError::NotPaired(hostname.to_string()))?;
        self.unwrap_seed(&row.ca_seed_enc)
    }

    /// Mints a certificate for `seed`'s public key signed by `hostname`'s
    /// CA, persists the (wrapped) key + certificate, and returns the
    /// signing material.
    async fn mint_and_store(
        &self,
        user: &CurrentUser,
        hostname: &str,
        signer_id: String,
        seed: Zeroizing<[u8; 32]>,
        permissions: Vec<String>,
    ) -> Result<UserSigningKey, KeyError> {
        let public_key_hex = derive_public_hex(&seed);
        let ca_seed = self.ca_seed(hostname).await?;

        let (issued_at, expires_at) = certificate_validity()?;
        let payload = CertificatePayload {
            signer_id: signer_id.clone(),
            // The device parses `full_name` as a plain string; fall back
            // to the username for accounts without one.
            full_name: user
                .full_name
                .clone()
                .unwrap_or_else(|| user.username.clone()),
            role: user.user_role.as_str().to_string(),
            public_key_ed25519: public_key_hex.clone(),
            permissions: permissions.clone(),
            issued_at,
            expires_at,
            issuer: "device_ca".to_string(),
        };
        let certificate_signature = sign_certificate(&ca_seed, &payload);
        let certificate = Certificate {
            payload,
            certificate_signature,
        };
        let certificate = serde_json::to_value(&certificate)
            .map_err(|err| KeyError::Certificate(format!("certificate serialization: {err}")))?;

        let permissions_json = serde_json::to_string(&permissions)
            .map_err(|err| KeyError::Certificate(format!("permissions serialization: {err}")))?;

        let fingerprint = fingerprint_of_hex_key(&public_key_hex);
        let row = NewUserDeviceKey {
            user_id: user.id,
            hostname: hostname.to_string(),
            seed_enc: self.wrap(seed.as_ref())?,
            public_key_hex,
            fingerprint,
            certificate: certificate.to_string(),
            permissions: permissions_json,
        };
        self.db.run(move |s| s.upsert_user_device_key(&row)).await?;

        Ok(UserSigningKey {
            signer_id,
            seed,
            certificate,
            ceiling: crate::perms::level_ceiling(user),
        })
    }

    fn cipher(&self) -> Result<Aes256Gcm, KeyError> {
        Aes256Gcm::new_from_slice(&**self.wrap_key)
            .map_err(|_| KeyError::Crypto("bad wrap key length".to_string()))
    }

    /// Wraps a 32-byte seed: `nonce || AES-256-GCM(seed)` (module docs).
    fn wrap(&self, plaintext: &[u8]) -> Result<Vec<u8>, KeyError> {
        let cipher = self.cipher()?;

        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);

        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| KeyError::Crypto("seed encryption failed".to_string()))?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Reverses [`Self::wrap`]; enforces the exactly-32-byte plaintext.
    fn unwrap_seed(&self, blob: &[u8]) -> Result<Zeroizing<[u8; 32]>, KeyError> {
        if blob.len() <= NONCE_LEN {
            return Err(KeyError::Crypto("wrapped blob too short".to_string()));
        }
        let (nonce, ciphertext) = blob.split_at(NONCE_LEN);

        let cipher = self.cipher()?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(Nonce::from_slice(nonce), ciphertext)
                .map_err(|_| KeyError::Crypto("seed decryption failed".to_string()))?,
        );

        let bytes: [u8; 32] = plaintext
            .as_slice()
            .try_into()
            .map_err(|_| KeyError::Crypto("unwrapped seed is not 32 bytes".to_string()))?;
        Ok(Zeroizing::new(bytes))
    }
}

/// `SHA-256(WRAP_CONTEXT || material)` — the AES-256 wrap key.
fn derive_wrap_key(material: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(WRAP_CONTEXT);
    hasher.update(material);
    Zeroizing::new(hasher.finalize().into())
}

/// Portal-local display fingerprint: first 16 hex chars of
/// `SHA-256(public_key_hex as ASCII)`. Not a protocol value — only shown
/// in key-status UI/API, so it doesn't need v1 byte-parity.
fn fingerprint_of_hex_key(public_key_hex: &str) -> String {
    let digest = Sha256::digest(public_key_hex.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        // Writing to a String is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// RFC3339 `(issued_at, expires_at)` for a freshly minted certificate —
/// the device parses these with `DateTime::parse_from_rfc3339`.
fn certificate_validity() -> Result<(String, String), KeyError> {
    let now = time::OffsetDateTime::now_utc();
    let expires = now + time::Duration::days(USER_CERT_TTL_DAYS);
    let issued_at = now
        .format(&Rfc3339)
        .map_err(|err| KeyError::Certificate(format!("timestamp formatting: {err}")))?;
    let expires_at = expires
        .format(&Rfc3339)
        .map_err(|err| KeyError::Certificate(format!("timestamp formatting: {err}")))?;
    Ok((issued_at, expires_at))
}

#[cfg(test)]
mod tests {
    use fiber_protocol::certificate::verify_certificate;
    use portal_storage::NewUser;
    use tempfile::tempdir;

    use crate::auth::Role;

    use super::*;

    fn open_service(db_key: Option<&str>) -> (tempfile::TempDir, KeyService) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path").to_string();
        let service = KeyService::open(&path, db_key).expect("open");
        (dir, service)
    }

    fn create_user(service: &KeyService, username: &str) -> CurrentUser {
        let storage = service.db.lock().expect("lock");
        let user = storage
            .create_user(&NewUser {
                username: username.into(),
                password_hash: "hash".into(),
                full_name: Some("Alice Doe".into()),
                job_title: None,
                user_role: "manager".into(),
                is_superuser: false,
            })
            .expect("create user");
        CurrentUser {
            id: user.id,
            username: user.username,
            full_name: user.full_name,
            user_role: Role::Manager,
            is_superuser: false,
            monitoring_view_mode: "table-simplified".into(),
        }
    }

    /// Pairs `hostname` with a fresh CA and returns the CA public key hex.
    async fn pair(service: &KeyService, hostname: &str, ca_seed: [u8; 32]) -> String {
        let ca_public = derive_public_hex(&ca_seed);
        service
            .store_pairing(
                hostname,
                Some("ca-test".into()),
                ca_public.clone(),
                ca_seed,
                Some(serde_json::json!({"signer_id": "admin"})),
                None,
            )
            .await
            .expect("store pairing");
        ca_public
    }

    fn perms(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wrap_unwrap_roundtrip_dev_and_prod_keys() {
        let (_dir, dev) = open_service(None);
        let (_dir2, prod) = open_service(Some("super-secret-db-key"));
        let seed = [42u8; 32];

        for service in [&dev, &prod] {
            let blob = service.wrap(&seed).expect("wrap");
            assert_ne!(&blob[NONCE_LEN..], &seed[..], "never plaintext at rest");
            let unwrapped = service.unwrap_seed(&blob).expect("unwrap");
            assert_eq!(*unwrapped, seed);
        }

        // Fresh nonce per wrap: same plaintext, different blobs.
        let a = dev.wrap(&seed).expect("wrap a");
        let b = dev.wrap(&seed).expect("wrap b");
        assert_ne!(a, b);

        // A dev-wrapped blob must not unwrap under a real key (and vice
        // versa) — the wrap key actually participates.
        let dev_blob = dev.wrap(&seed).expect("wrap");
        assert!(matches!(
            prod.unwrap_seed(&dev_blob),
            Err(KeyError::Crypto(_))
        ));
    }

    #[tokio::test]
    async fn pairing_store_get_unpair() {
        let (_dir, service) = open_service(None);
        let ca_public = pair(&service, "fiber-01", [7u8; 32]).await;

        let info = service
            .device_identity("fiber-01")
            .await
            .expect("get")
            .expect("present");
        assert!(info.has_ca);
        assert_eq!(info.ca_public_key, ca_public);
        assert_eq!(info.ca_id.as_deref(), Some("ca-test"));

        // The stored CA seed round-trips through the wrap.
        assert_eq!(
            *service.ca_seed("fiber-01").await.expect("ca seed"),
            [7u8; 32]
        );

        service.unpair("fiber-01").await.expect("unpair");
        assert!(service
            .device_identity("fiber-01")
            .await
            .expect("get")
            .is_none());
        assert!(matches!(
            service.unpair("fiber-01").await,
            Err(KeyError::NotPaired(_))
        ));
        assert!(matches!(
            service.ca_seed("fiber-01").await,
            Err(KeyError::NotPaired(_))
        ));
    }

    #[tokio::test]
    async fn ensure_user_key_mints_once_then_reuses() {
        let (_dir, service) = open_service(None);
        pair(&service, "fiber-01", [7u8; 32]).await;
        let user = create_user(&service, "alice");

        let first = service
            .ensure_user_key(&user, "fiber-01", &perms(&["set_threshold"]))
            .await
            .expect("mint");
        assert_eq!(first.signer_id, format!("user-{}-alice", user.id));

        let second = service
            .ensure_user_key(&user, "fiber-01", &perms(&["set_threshold"]))
            .await
            .expect("reuse");
        assert_eq!(*second.seed, *first.seed);
        assert_eq!(second.certificate, first.certificate);
    }

    #[tokio::test]
    async fn permission_superset_reminets_with_union_same_keypair() {
        let (_dir, service) = open_service(None);
        pair(&service, "fiber-01", [7u8; 32]).await;
        let user = create_user(&service, "alice");

        let first = service
            .ensure_user_key(&user, "fiber-01", &perms(&["set_threshold"]))
            .await
            .expect("mint");

        let widened = service
            .ensure_user_key(&user, "fiber-01", &perms(&["set_interval"]))
            .await
            .expect("re-mint");

        // Same keypair, new certificate carrying the union.
        assert_eq!(*widened.seed, *first.seed);
        assert_ne!(widened.certificate, first.certificate);
        let cert_perms: Vec<String> =
            serde_json::from_value(widened.certificate["permissions"].clone())
                .expect("permissions array");
        assert_eq!(cert_perms, perms(&["set_threshold", "set_interval"]));
    }

    #[tokio::test]
    async fn minted_certificate_verifies_against_stored_ca_public_key() {
        let (_dir, service) = open_service(None);
        let ca_public = pair(&service, "fiber-01", [7u8; 32]).await;
        let user = create_user(&service, "alice");

        let key = service
            .ensure_user_key(&user, "fiber-01", &perms(&["set_threshold", "get_status"]))
            .await
            .expect("mint");

        let cert: Certificate =
            serde_json::from_value(key.certificate.clone()).expect("certificate shape");
        verify_certificate(&ca_public, &cert).expect("verifies against device CA");
        assert_eq!(
            cert.payload.public_key_ed25519,
            derive_public_hex(&key.seed)
        );
        assert_eq!(cert.payload.role, "manager");
        assert_eq!(cert.payload.issuer, "device_ca");

        // Tampering breaks verification.
        let mut tampered = cert.clone();
        tampered.payload.permissions.push("factory_reset".into());
        assert!(verify_certificate(&ca_public, &tampered).is_err());
    }

    #[tokio::test]
    async fn unpair_deletes_user_keys_too() {
        let (_dir, service) = open_service(None);
        pair(&service, "fiber-01", [7u8; 32]).await;
        let user = create_user(&service, "alice");
        service
            .ensure_user_key(&user, "fiber-01", &perms(&["set_threshold"]))
            .await
            .expect("mint");

        service.unpair("fiber-01").await.expect("unpair");

        let row = {
            let storage = service.db.lock().expect("lock");
            storage
                .get_user_device_key(user.id, "fiber-01")
                .expect("get")
        };
        assert!(row.is_none());
    }
}
