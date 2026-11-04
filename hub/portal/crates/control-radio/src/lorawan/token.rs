//! The three-token credential model — `plan/control/radio/lorawan.md` §4 (AC 2, ledger #24).
//!
//! ChirpStack's own `ApiKey` model has exactly three axes — `{is_admin, tenant_id,
//! is_read_only}` — and no finer per-resource split exists. So the split here **does not
//! reduce what any one token is capable of inside ChirpStack; it reduces which token a given
//! code path is trusted with.**
//!
//! | Token | Scope | Used by |
//! |---|---|---|
//! | admin | global admin | tenant resolution and `ApplicationService.Create` inside `create`; also `DeviceService.Get`/`Delete` (2026-08-25 finding, below) |
//! | tenant | tenant-scoped read-write | profile/gateway creation, `DeviceService.Create`/`CreateKeys`/`GetKeys`/`UpdateKeys`/`Activate`, and every `gateway-*` command |
//! | readonly | tenant-scoped read-only | `gateway-list`/`gateway-info`/`status` health polling |
//!
//! **`DeviceService.Get`/`Delete` need the admin token, measured against ChirpStack 4.19.0 —
//! not a convenience choice.** ChirpStack authorizes a tenant key for these two RPCs with an
//! `EXISTS` join against the device row itself; for a `dev_eui` that does not exist yet (the
//! ask-first check §7's enrollment upsert makes before every `create_device`, or a `remove` on
//! an already-gone device) that join matches nothing and ChirpStack returns `Unauthenticated`,
//! indistinguishable from a real permission failure — never `NotFound`. Every other
//! `DeviceService` call in this client is scoped by `application_id`/an already-created
//! device, so the tenant token resolves correctly for those.
//!
//! In steady state the read-only token is by far the most-used credential, and the exposure
//! profile follows the same ordering: **the credential that moves most often is the one that
//! can change nothing.**
//!
//! Tokens are read **per call**, never cached in a long-lived handle. That is what preserves
//! v1's rotate-without-restart property: replacing a token file takes effect on the next RPC,
//! with no service restart and no reconnect.

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::store::{Slot, Store};

/// Which credential a call is trusted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Global admin. Tenant resolution, application creation, and `DeviceService.Get`/`Delete`
    /// (a tenant key cannot authorize either against a `dev_eui` that doesn't exist yet — see
    /// the module doc).
    Admin,
    /// Tenant-scoped read-write. Everything that mutates devices, profiles or gateways.
    Tenant,
    /// Tenant-scoped read-only. Health polling and status.
    ReadOnly,
}

impl Scope {
    const fn slot(self) -> Slot {
        match self {
            Scope::Admin => Slot::TokenAdmin,
            Scope::Tenant => Slot::TokenTenant,
            Scope::ReadOnly => Slot::TokenReadonly,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Scope::Admin => "admin",
            Scope::Tenant => "tenant",
            Scope::ReadOnly => "readonly",
        }
    }

    pub const ALL: [Scope; 3] = [Scope::Admin, Scope::Tenant, Scope::ReadOnly];
}

/// Reads the three token files. Holds no token value — only where to find them.
#[derive(Debug, Clone)]
pub struct Tokens {
    store: Store,
}

/// Whether a token file is present, for `status --json`'s `tokens` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    Present,
    Absent,
}

impl Tokens {
    pub fn new(store: Store) -> Self {
        Tokens { store }
    }

    /// Read the token for a scope. Absent means bootstrap never ran, or `/data` was wiped —
    /// which is `NotBootstrapped`, deliberately distinct from `Unauthenticated` (a token that
    /// exists and is refused). Conflating them would send an operator to the wrong runbook.
    pub fn get(&self, scope: Scope) -> Result<String> {
        let raw = self.store.require(scope.slot())?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // An empty file is the `@dev` blank-password gate in its v2 form: refuse to
            // start rather than authenticate as nobody.
            return Err(Error::new(
                Kind::NotBootstrapped,
                Transport::Lorawan,
                Component::TokenFile,
                format!(
                    "the {} API token file is empty: re-run the bootstrap step",
                    scope.as_str()
                ),
            ));
        }
        Ok(trimmed.to_string())
    }

    pub fn store(&self, scope: Scope, token: &str) -> Result<()> {
        self.store.write(scope.slot(), token.trim())
    }

    pub fn remove(&self, scope: Scope) -> Result<()> {
        self.store.remove(scope.slot())
    }

    pub fn presence(&self, scope: Scope) -> Presence {
        match self.store.read(scope.slot()) {
            Ok(Some(s)) if !s.trim().is_empty() => Presence::Present,
            _ => Presence::Absent,
        }
    }

    /// The `tokens` block of `status --json`.
    pub fn presence_report(&self) -> std::collections::BTreeMap<&'static str, Presence> {
        Scope::ALL
            .iter()
            .map(|s| (s.as_str(), self.presence(*s)))
            .collect()
    }

    /// True once the admin token exists — the one the offline bootstrap step mints, and the
    /// precondition for `create` being able to mint the other two.
    pub fn is_bootstrapped(&self) -> bool {
        self.presence(Scope::Admin) == Presence::Present
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tokens() -> (Tokens, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "control-radio-tokens-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        (Tokens::new(Store::new(&base)), base)
    }

    /// §10's "three-token bootstrap state table, one row per token".
    #[test]
    fn each_scope_reads_its_own_file() {
        let (t, base) = temp_tokens();
        for scope in Scope::ALL {
            t.store(scope, &format!("jwt-for-{}", scope.as_str()))
                .expect("stores");
        }
        for scope in Scope::ALL {
            assert_eq!(
                t.get(scope).expect("reads"),
                format!("jwt-for-{}", scope.as_str())
            );
        }
        // No cross-talk: removing one leaves the others readable.
        t.remove(Scope::Tenant).expect("removes");
        assert_eq!(
            t.get(Scope::Tenant).expect_err("gone").kind(),
            Kind::NotBootstrapped
        );
        assert!(t.get(Scope::Admin).is_ok());
        assert!(t.get(Scope::ReadOnly).is_ok());
        let _ = std::fs::remove_dir_all(base);
    }

    /// Absent is `NotBootstrapped`, never `Unauthenticated`: the runbooks differ.
    #[test]
    fn an_absent_token_is_not_bootstrapped() {
        let (t, base) = temp_tokens();
        for scope in Scope::ALL {
            let err = t.get(scope).expect_err("absent");
            assert_eq!(err.kind(), Kind::NotBootstrapped, "{scope:?}");
            assert_eq!(err.component(), Component::TokenFile);
        }
        let _ = std::fs::remove_dir_all(base);
    }

    /// An empty file is the v2 form of `@dev`'s blank-password start-up gate.
    #[test]
    fn an_empty_token_file_is_refused_rather_than_used() {
        let (t, base) = temp_tokens();
        t.store(Scope::Admin, "   \n  ").expect("stores");
        let err = t.get(Scope::Admin).expect_err("empty must be refused");
        assert_eq!(err.kind(), Kind::NotBootstrapped);
        assert_eq!(t.presence(Scope::Admin), Presence::Absent);
        let _ = std::fs::remove_dir_all(base);
    }

    /// Rotate-without-restart: the value is read per call, so replacing the file takes effect
    /// on the next read with no handle to invalidate.
    #[test]
    fn a_rotated_token_takes_effect_on_the_next_read() {
        let (t, base) = temp_tokens();
        t.store(Scope::Tenant, "old").expect("stores");
        assert_eq!(t.get(Scope::Tenant).expect("reads"), "old");
        t.store(Scope::Tenant, "new").expect("rotates");
        assert_eq!(t.get(Scope::Tenant).expect("reads"), "new");
        let _ = std::fs::remove_dir_all(base);
    }

    /// Surrounding whitespace is a file-editing artefact, not part of the JWT.
    #[test]
    fn surrounding_whitespace_is_stripped() {
        let (t, base) = temp_tokens();
        t.store(Scope::Admin, "  eyJhbGciOi.body.sig \n")
            .expect("stores");
        assert_eq!(t.get(Scope::Admin).expect("reads"), "eyJhbGciOi.body.sig");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn presence_report_is_the_status_json_block() {
        let (t, base) = temp_tokens();
        t.store(Scope::Admin, "a").expect("stores");
        let report = t.presence_report();
        assert_eq!(report["admin"], Presence::Present);
        assert_eq!(report["tenant"], Presence::Absent);
        assert_eq!(report["readonly"], Presence::Absent);
        assert!(t.is_bootstrapped());

        let json = serde_json::to_value(&report).expect("serializes");
        assert_eq!(json["admin"], "present");
        assert_eq!(json["tenant"], "absent");
        let _ = std::fs::remove_dir_all(base);
    }
}
