//! Minting the tenant and read-only tokens — `plan/control/radio/lorawan.md` §4 steps 3–4.
//!
//! The offline `chirpstack create-api-key` step mints exactly one key, the global admin one,
//! before the service has ever listened. The other two are **tenant-scoped**, so they cannot be
//! minted until a tenant id exists to scope them to — which is why §4 puts them on the first
//! `create` run, right after tenant resolution.
//!
//! **The admin API key cannot mint them.** ChirpStack v4 authenticates
//! `InternalService.{CreateApiKey,ListApiKeys,DeleteApiKey}` against session claims only; an
//! API-key bearer token is refused there whatever its scope (measured against 4.19.0 — see
//! `tests/integration/README.md`). So the sequence is Login → mint, using the password the
//! seed migration generated and `chirpstack-bootstrap.service` applied. §4's "`Login` is never
//! used programmatically at all" does not survive contact with the API and is carried to the
//! plan-feedback set; what does survive is the property it was written for — **`admin`/`admin`
//! never exists on a listening socket**, because the password this reads is the random seeded
//! one, not the factory default.
//!
//! Idempotency is decided **before any RPC**, from token presence on disk: a converged Hub
//! re-running `create` makes no ChirpStack call at all from here. That is asserted rather than
//! assumed, because the cost of getting it wrong is a fresh key row on every boot.

use std::path::{Path, PathBuf};

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::client::ChirpStack;
use crate::lorawan::model::ApiKeySpec;
use crate::lorawan::token::{Presence, Scope, Tokens};

/// The admin password `create` logs in with to mint the tenant/read-only tokens on first boot.
///
/// A **`proximos`-readable copy** that `chirpstack-bootstrap.service` (root) writes into this
/// component's own state tree, 0600 `proximos:proximos`, next to the tokens. The canonical copy the
/// seed migration generates is `/data/chirpstack/admin-password`, 0600 `root:root` inside the 0700
/// `chirpstack:chirpstack` directory - which `radio-setup`, running as `proximos`, cannot even
/// traverse, so reading it there fails `StorageError` and the whole first-boot converge dies before
/// a single object is created. Cross-file contract with `chirpstack-bootstrap.sh`'s
/// `RADIO_PASSWORD_FILE`.
pub const ADMIN_PASSWORD_FILE: &str = "/data/proximos/radio/lorawan/chirpstack-admin-password";

/// Overrides [`ADMIN_PASSWORD_FILE`]. The compose stack in `tests/integration/` runs
/// ChirpStack in a container and the harness on the host, so there is no `/data/chirpstack`
/// to read — same convention as `PROXIMOS_CONFIG` and `PROXIMOS_RADIO_DATA`.
pub const ADMIN_PASSWORD_FILE_ENV: &str = "PROXIMOS_CHIRPSTACK_ADMIN_PASSWORD_FILE";

/// ChirpStack's seeded default user. The `email` field is literally this, not an address.
pub const ADMIN_USER: &str = "admin";

/// The `name` every key this component mints carries, so `ListApiKeys` can identify its own
/// rows. `control-radio-admin` — the one the offline bootstrap step mints — follows the same
/// shape; the two spellings below are a cross-file contract with `chirpstack-bootstrap.sh`.
pub const KEY_NAME_PREFIX: &str = "control-radio-";

/// The `ApiKey.name` for a scope: `control-radio-tenant`, `control-radio-readonly`.
pub fn key_name(scope: Scope) -> String {
    format!("{KEY_NAME_PREFIX}{}", scope.as_str())
}

/// The shipped password-file path, with the environment override applied.
pub fn admin_password_path() -> PathBuf {
    match std::env::var(ADMIN_PASSWORD_FILE_ENV) {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => PathBuf::from(ADMIN_PASSWORD_FILE),
    }
}

/// What one `mint_missing` run did. Empty on the converged path — which is the assertion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MintReport {
    /// The scopes whose token files were written, in mint order.
    pub minted: Vec<Scope>,
    /// The ids of stale key rows deleted first — a half-failed previous mint.
    pub pruned: Vec<String>,
}

impl MintReport {
    /// True when both tokens were already present and nothing was asked of ChirpStack.
    pub fn is_noop(&self) -> bool {
        self.minted.is_empty() && self.pruned.is_empty()
    }
}

/// Mints the two tenant-scoped tokens `create` needs, and nothing else.
///
/// Deliberately not part of [`crate::lorawan::converge::Converger`]: converge's contract is
/// "ask before creating, never update", and this one *does* replace a server-side row it finds.
/// Keeping it separate keeps that exception visible.
pub struct Bootstrap<'a> {
    pub client: &'a dyn ChirpStack,
    pub tokens: &'a Tokens,
    /// Where the admin password is read from. A field rather than an environment read at the
    /// point of use, so the state table below is testable without process-global state.
    pub password_file: PathBuf,
}

impl<'a> Bootstrap<'a> {
    /// The shipped wiring: password file from [`admin_password_path`].
    pub fn new(client: &'a dyn ChirpStack, tokens: &'a Tokens) -> Self {
        Bootstrap {
            client,
            tokens,
            password_file: admin_password_path(),
        }
    }

    /// The same, with an explicit password file.
    pub fn with_password_file(
        client: &'a dyn ChirpStack,
        tokens: &'a Tokens,
        password_file: impl AsRef<Path>,
    ) -> Self {
        Bootstrap {
            client,
            tokens,
            password_file: password_file.as_ref().to_path_buf(),
        }
    }

    /// Mint whichever of the tenant/read-only tokens is absent, scoped to `tenant_id`.
    ///
    /// Both present is the steady state and returns without a single RPC — not even the login.
    /// Otherwise: log in, prune any key row this component previously minted **under the name
    /// of a scope being re-minted** (the residue of a mint that wrote the ChirpStack row and
    /// then failed to write the token file), mint, and persist 0600 through [`Tokens::store`].
    ///
    /// A present token's row is never pruned: deleting it would revoke a credential still in
    /// use and turn every later call into `Unauthenticated`, which is the failure this whole
    /// function exists to avoid.
    pub async fn mint_missing(&self, tenant_id: &str) -> Result<MintReport> {
        let missing: Vec<Scope> = [Scope::Tenant, Scope::ReadOnly]
            .into_iter()
            .filter(|s| self.tokens.presence(*s) == Presence::Absent)
            .collect();
        if missing.is_empty() {
            return Ok(MintReport::default());
        }

        // Before the login, so a Hub whose bootstrap oneshot never ran says exactly that
        // (exit 10) rather than failing an authentication it was never able to attempt.
        let password = self.read_admin_password()?;
        let session = self.client.login(ADMIN_USER, &password).await?;

        let existing = self.client.list_api_keys(&session, Some(tenant_id)).await?;
        let mut report = MintReport::default();

        for scope in missing {
            let name = key_name(scope);
            for stale in existing.iter().filter(|k| k.name == name) {
                // Already gone is success — the same rule converge applies to every delete.
                match self.client.delete_api_key(&session, &stale.id).await {
                    Ok(()) => report.pruned.push(stale.id.clone()),
                    Err(e) if e.kind() == Kind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            let minted = self
                .client
                .create_api_key(
                    &session,
                    &ApiKeySpec {
                        name,
                        // Never admin: the whole point of §4's split is that the frequently
                        // triggered paths hold a key that cannot delete the Application.
                        is_admin: false,
                        tenant_id: Some(tenant_id.to_string()),
                        is_read_only: scope == Scope::ReadOnly,
                    },
                )
                .await?;
            self.tokens.store(scope, &minted.token)?;
            report.minted.push(scope);
        }

        Ok(report)
    }

    /// Read the seeded admin password.
    ///
    /// Absent or empty is `NotBootstrapped` (exit 10), the same verdict an absent token file
    /// gets: `chirpstack-bootstrap.service` has not run, or `/data` was wiped, and no retry
    /// helps. Unreadable is `StorageError`, mirroring [`crate::store::Store::read`]'s split —
    /// a permissions mistake must not read as a fresh device.
    ///
    /// The component is `token-file`. `plan.md` §6's vocabulary is closed and has no value for
    /// ChirpStack's own credential store; `token-file` is the nearest true statement (this is
    /// a credential file, absent), and `message` names the path so the runbook is unambiguous.
    /// Flagged for the design-doc pass: a `chirpstack-admin-password` value would be exact.
    fn read_admin_password(&self) -> Result<String> {
        let path = self.password_file.display();
        let raw = match std::fs::read_to_string(&self.password_file) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::new(
                    Kind::NotBootstrapped,
                    Transport::Lorawan,
                    Component::TokenFile,
                    format!(
                        "{path} is absent: the ChirpStack bootstrap step has not run, so there \
                         is no admin password to mint the tenant API keys with"
                    ),
                ))
            }
            Err(e) => {
                return Err(Error::new(
                    Kind::StorageError,
                    Transport::Lorawan,
                    Component::TokenFile,
                    format!("{path}: cannot read the ChirpStack admin password: {e}"),
                ))
            }
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // The `@dev` blank-password gate, again: refuse to authenticate as nobody.
            return Err(Error::new(
                Kind::NotBootstrapped,
                Transport::Lorawan,
                Component::TokenFile,
                format!("{path} is empty: re-run the ChirpStack bootstrap step"),
            ));
        }
        Ok(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lorawan::fake::{FakeChirpStack, DEFAULT_TENANT_ID, FAKE_ADMIN_PASSWORD};
    use crate::store::Store;

    /// A temp dir that removes itself; the crate deliberately has no `tempfile` dependency.
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        fake: FakeChirpStack,
        tokens: Tokens,
        password_file: PathBuf,
        _dir: TempDir,
    }

    fn fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "control-radio-bootstrap-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let password_file = dir.join("admin-password");
        std::fs::write(&password_file, format!("{FAKE_ADMIN_PASSWORD}\n")).expect("seeds");
        Fixture {
            fake: FakeChirpStack::new(),
            tokens: Tokens::new(Store::new(dir.join("radio"))),
            password_file,
            _dir: TempDir(dir),
        }
    }

    impl Fixture {
        fn bootstrap(&self) -> Bootstrap<'_> {
            Bootstrap::with_password_file(&self.fake, &self.tokens, &self.password_file)
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// The idempotent row of §4 step 4: "if the files already exist, this step is skipped" —
    /// and skipped means *no RPC*, not a login-then-notice.
    #[test]
    fn both_tokens_present_makes_no_call_at_all() {
        let f = fixture("noop");
        f.tokens.store(Scope::Tenant, "jwt-t").expect("seeds");
        f.tokens.store(Scope::ReadOnly, "jwt-r").expect("seeds");

        let report = rt()
            .block_on(f.bootstrap().mint_missing(DEFAULT_TENANT_ID))
            .expect("a converged Hub mints nothing");

        assert!(report.is_noop(), "{report:?}");
        assert!(
            f.fake.calls().is_empty(),
            "a converged create must not touch ChirpStack from here: {:?}",
            f.fake.calls()
        );
        // And the existing credentials are untouched, not re-minted over.
        assert_eq!(f.tokens.get(Scope::Tenant).expect("reads"), "jwt-t");
        assert_eq!(f.tokens.get(Scope::ReadOnly).expect("reads"), "jwt-r");
    }

    /// A fresh Hub: both minted, at the scopes §4's table assigns them.
    #[test]
    fn both_absent_mints_both_at_the_declared_scopes() {
        let f = fixture("both");
        let report = rt()
            .block_on(f.bootstrap().mint_missing(DEFAULT_TENANT_ID))
            .expect("mints");

        assert_eq!(report.minted, vec![Scope::Tenant, Scope::ReadOnly]);
        assert!(report.pruned.is_empty(), "nothing stale to prune");
        assert_eq!(
            f.fake.calls(),
            vec!["login", "list_api_keys", "create_api_key", "create_api_key"]
        );
        let names: Vec<String> = f.fake.api_keys().into_iter().map(|k| k.name).collect();
        assert_eq!(
            names,
            vec!["control-radio-tenant", "control-radio-readonly"]
        );
        assert_eq!(
            f.tokens.get(Scope::Tenant).expect("reads"),
            "jwt.for.control-radio-tenant"
        );
        assert_eq!(
            f.tokens.get(Scope::ReadOnly).expect("reads"),
            "jwt.for.control-radio-readonly"
        );
        // The admin token is bootstrap-service business and is never minted here.
        assert_eq!(f.tokens.presence(Scope::Admin), Presence::Absent);
    }

    /// Half-failed mint: the ChirpStack row exists, the token file does not. The row is pruned
    /// and re-minted, so the pair converges to exactly one key — and the *other* scope's row,
    /// whose token file is intact, is left alone rather than revoked.
    #[test]
    fn one_missing_prunes_and_mints_only_that_one() {
        let f = fixture("one");
        f.tokens.store(Scope::Tenant, "jwt-t").expect("seeds");
        let live_tenant_row = f.fake.seed_api_key("control-radio-tenant");
        let orphan = f.fake.seed_api_key("control-radio-readonly");

        let report = rt()
            .block_on(f.bootstrap().mint_missing(DEFAULT_TENANT_ID))
            .expect("mints");

        assert_eq!(report.minted, vec![Scope::ReadOnly]);
        assert_eq!(report.pruned, vec![orphan.clone()]);
        assert_eq!(
            f.fake.calls(),
            vec!["login", "list_api_keys", "delete_api_key", "create_api_key"],
            "prune precedes the mint, and only the missing scope is touched"
        );

        let rows: Vec<(String, String)> = f
            .fake
            .api_keys()
            .into_iter()
            .map(|k| (k.id, k.name))
            .collect();
        assert!(
            rows.iter().any(|(id, _)| *id == live_tenant_row),
            "the row backing the surviving token file must not be revoked: {rows:?}"
        );
        assert!(
            !rows.iter().any(|(id, _)| *id == orphan),
            "the orphaned readonly row must be gone: {rows:?}"
        );
        assert_eq!(f.tokens.get(Scope::Tenant).expect("reads"), "jwt-t");
        assert_eq!(
            f.tokens.get(Scope::ReadOnly).expect("reads"),
            "jwt.for.control-radio-readonly"
        );
    }

    /// No password file: `NotBootstrapped` (exit 10) decided locally, before the login — the
    /// distinction §6 draws between "the credential is absent" and "the far side refused it".
    #[test]
    fn an_absent_password_file_is_not_bootstrapped_before_any_rpc() {
        let f = fixture("nopw");
        std::fs::remove_file(&f.password_file).expect("removes");

        let err = rt()
            .block_on(f.bootstrap().mint_missing(DEFAULT_TENANT_ID))
            .expect_err("must refuse");

        assert_eq!(err.kind(), Kind::NotBootstrapped);
        assert_eq!(err.exit_code(), 10);
        assert_eq!(err.component(), Component::TokenFile);
        assert_eq!(err.transport(), Some(Transport::Lorawan));
        assert!(
            err.message().contains("admin-password"),
            "the message must name the file: {}",
            err.message()
        );
        assert!(
            f.fake.calls().is_empty(),
            "the refusal is local: {:?}",
            f.fake.calls()
        );
    }

    /// An empty file is the blank-password gate, not a blank password to try.
    #[test]
    fn an_empty_password_file_is_refused_rather_than_used() {
        let f = fixture("emptypw");
        std::fs::write(&f.password_file, "  \n").expect("empties");

        let err = rt()
            .block_on(f.bootstrap().mint_missing(DEFAULT_TENANT_ID))
            .expect_err("must refuse");

        assert_eq!(err.kind(), Kind::NotBootstrapped);
        assert!(f.fake.calls().is_empty());
    }

    /// A rejected password is `Unauthenticated` (exit 11), never `NotBootstrapped`: the file
    /// is there and ChirpStack refused it, which is a different runbook — and nothing is
    /// minted or written on the way out.
    #[test]
    fn a_rejected_login_is_unauthenticated() {
        let f = fixture("badpw");
        std::fs::write(&f.password_file, "not-the-seeded-one").expect("writes");

        let err = rt()
            .block_on(f.bootstrap().mint_missing(DEFAULT_TENANT_ID))
            .expect_err("must refuse");

        assert_eq!(err.kind(), Kind::Unauthenticated);
        assert_eq!(err.exit_code(), 11);
        assert!(
            !err.kind().is_retryable(),
            "a bad password is not transient"
        );
        assert_eq!(f.fake.calls(), vec!["login"], "it stops at the login");
        assert_eq!(f.tokens.presence(Scope::Tenant), Presence::Absent);
        assert_eq!(f.tokens.presence(Scope::ReadOnly), Presence::Absent);
    }

    /// The key names are a cross-file contract: `chirpstack-bootstrap.sh` mints
    /// `control-radio-admin` under the same prefix, and the prune above matches on them.
    #[test]
    fn the_key_names_are_the_documented_ones() {
        assert_eq!(key_name(Scope::Tenant), "control-radio-tenant");
        assert_eq!(key_name(Scope::ReadOnly), "control-radio-readonly");
        assert_eq!(key_name(Scope::Admin), "control-radio-admin");
    }

    /// The password file is the shipped path unless the compose harness overrides it.
    #[test]
    fn the_shipped_password_path_is_the_documented_one() {
        assert_eq!(
            ADMIN_PASSWORD_FILE,
            "/data/proximos/radio/lorawan/chirpstack-admin-password"
        );
        assert_eq!(
            ADMIN_PASSWORD_FILE_ENV,
            "PROXIMOS_CHIRPSTACK_ADMIN_PASSWORD_FILE"
        );
    }

    /// Tokens land 0600 — they are minted here, so this is where the mode is proved.
    #[cfg(unix)]
    #[test]
    fn minted_tokens_are_written_0600() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture("mode");
        rt().block_on(f.bootstrap().mint_missing(DEFAULT_TENANT_ID))
            .expect("mints");
        let store = Store::new(f._dir.0.join("radio").to_str().expect("utf-8").to_string());
        for slot in [
            crate::store::Slot::TokenTenant,
            crate::store::Slot::TokenReadonly,
        ] {
            let mode = std::fs::metadata(store.path(slot))
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{slot:?}");
        }
    }
}
