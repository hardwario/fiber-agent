//! `AuthService`: the auth core wrapping `portal-storage`'s users/sessions/
//! audit repositories with password hashing, session tokens, and the v1
//! self-guard semantics (plan/v1/viewer/rest.md "Superuser / session",
//! `backend/app/auth/superuser.py`).
//!
//! Opens its own [`Storage`] connection on the shared `data_dir`/`db_key` —
//! a second SQLite connection under WAL is fine; the persist thread
//! (`src/persist.rs`) is untouched.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argon2::password_hash::rand_core::OsRng as PasswordOsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng as TokenOsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

use portal_storage::{
    AuditEntry, AuditFilter, NewAuditEntry, NewUser, SessionRow, SpillGate, Storage, StorageError,
    User,
};

use super::throttle::Throttle;
use super::types::{AuthError, CurrentUser, Role, SelfGuardReason, ValidationReason};

/// Sliding-expiry idle timeout: a session dies if unused for this long.
const IDLE_TTL_SECS: i64 = 24 * 3600;
/// A session's `last_seen_at`/`expires_at` are only re-written (extra DB
/// write) when older than this, not on every single request.
const BUMP_INTERVAL_SECS: i64 = 3600;
/// However much a session is used, it ends this long after login (R-11). The cookie lives as
/// long (`auth::cookies`).
pub(crate) const ABSOLUTE_TTL_SECS: i64 = 7 * 24 * 3600;

/// Fields for the admin "create user" API — deliberately excludes
/// `is_superuser`: the only way to become a superuser is `create_superuser`
/// during first-run setup.
#[derive(Debug, Clone)]
pub struct NewUserInput {
    pub username: String,
    pub password: String,
    pub full_name: Option<String>,
    pub job_title: Option<String>,
    pub user_role: Role,
}

#[derive(Clone)]
pub struct AuthService {
    db: crate::db::Db,
    /// Audit rows are evidence and are never pruned, so a write refused during
    /// a full VACUUM is a lost record rather than a delayed one. They go
    /// through `crate::durable` instead of being logged and dropped.
    spill: Arc<SpillGate>,
    /// Failed logins per client and account, v1's 10 a minute (LE-11).
    login_throttle: Arc<Throttle>,
    /// Failed logins per client over all accounts, so trying one password on many names is
    /// bounded too. A successful login never clears it (R2-2, review 2 of !102).
    login_ip_throttle: Arc<Throttle>,
    /// Setup attempts per client, v1's 5 a minute.
    setup_throttle: Arc<Throttle>,
}

/// v1's limits (slowapi on `/login` and `/setup`).
const LOGIN_ATTEMPTS_PER_MINUTE: usize = 10;
const LOGIN_ATTEMPTS_PER_CLIENT_PER_MINUTE: usize = 30;
const SETUP_ATTEMPTS_PER_MINUTE: usize = 5;

/// The longest username a failed-login audit row keeps (`validate_username` allows 50); a
/// refused request can carry a form body of megabytes (R2-3).
const AUDIT_USERNAME_MAX: usize = 64;

/// `username` cut to what an audit row or a log line keeps.
fn audit_username(username: &str) -> String {
    username.chars().take(AUDIT_USERNAME_MAX).collect()
}

/// A client without an address (tests, a peer the route could not read) shares one bucket.
fn throttle_key(ip: Option<&str>) -> &str {
    ip.unwrap_or("-")
}

impl AuthService {
    /// Opens (creating if needed) `<data_dir>/portal.db` — the same path
    /// `persist::spawn` uses. Sync: called once at startup, before the
    /// tokio runtime's async machinery is needed for anything else.
    pub fn open(
        data_dir: &str,
        db_key: Option<&str>,
        spill: Arc<SpillGate>,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(data_dir)?;
        let path = Path::new(data_dir).join("portal.db");
        let storage = Storage::open(&path, db_key)?;
        Ok(Self {
            db: crate::db::Db::from_storage(storage, "auth"),
            spill,
            login_throttle: Arc::new(Throttle::new(
                LOGIN_ATTEMPTS_PER_MINUTE,
                Duration::from_secs(60),
            )),
            login_ip_throttle: Arc::new(Throttle::new(
                LOGIN_ATTEMPTS_PER_CLIENT_PER_MINUTE,
                Duration::from_secs(60),
            )),
            setup_throttle: Arc::new(Throttle::new(
                SETUP_ATTEMPTS_PER_MINUTE,
                Duration::from_secs(60),
            )),
        })
    }

    /// `true` until the first superuser is created.
    pub async fn setup_required(&self) -> bool {
        match self.db.run(|s| s.count_superusers()).await {
            Ok(count) => count == 0,
            Err(err) => {
                // Fail closed: on a transient DB error, don't reopen the
                // first-run setup path on an already-provisioned system.
                tracing::error!(%err, "failed to count superusers");
                false
            }
        }
    }

    /// Counts one setup attempt from `ip`; `Err` once it has used v1's five a minute.
    pub fn setup_attempt(&self, ip: Option<&str>) -> Result<(), AuthError> {
        self.setup_throttle
            .try_acquire(throttle_key(ip))
            .map_err(|refused| AuthError::TooManyAttempts {
                retry_after: refused.retry_after,
            })
    }

    /// First-run superuser creation. Refused once any superuser exists.
    pub async fn create_superuser(
        &self,
        username: String,
        password: String,
        full_name: Option<String>,
    ) -> Result<User, AuthError> {
        if !self.setup_required().await {
            return Err(AuthError::SetupAlreadyDone);
        }
        validate_username(&username)?;
        validate_full_name(full_name.as_deref())?;
        validate_password(&password)?;

        let password_hash = hash_password(&password)?;
        let new_user = NewUser {
            username,
            password_hash,
            full_name,
            job_title: None,
            user_role: Role::Admin.as_str().to_string(),
            is_superuser: true,
        };

        let user = self.db.run(move |s| s.create_user(&new_user)).await?;
        // C36-11: the row names the superuser it created as its actor too.
        self.audit(
            "setup",
            Some(&to_current_user(&user)),
            None,
            Some(&user.username),
            None,
            true,
        )
        .await;
        Ok(user)
    }

    /// Verifies credentials and opens a new session. Always performs an
    /// argon2 verify (against a cached dummy hash when the username is
    /// unknown) so response timing can't be used to enumerate usernames.
    /// A disabled account fails with the same [`AuthError::InvalidCredentials`]
    /// as a wrong password (no enabled/disabled oracle).
    pub async fn login(
        &self,
        username: String,
        password: String,
        ip: Option<String>,
    ) -> Result<(String, CurrentUser), AuthError> {
        // Counted before the first await and before the argon2 verify, so parallel requests
        // cannot all pass (R2-1) and a flood costs no hashing either.
        let client = throttle_key(ip.as_deref()).to_string();
        // Cut like the audit row: a form body can carry megabytes, and the key lives in memory.
        let account = format!(
            "{client}|{}",
            username
                .chars()
                .take(AUDIT_USERNAME_MAX)
                .collect::<String>()
                .to_lowercase()
        );
        let admitted = match self.login_ip_throttle.try_acquire(&client) {
            // S37-2: the row says which limit it was, so a client's lockout does not read as
            // an attack on whichever account happened to hit it.
            Err(refused) => Err((refused, "client")),
            Ok(()) => self
                .login_throttle
                .try_acquire(&account)
                .map_err(|refused| {
                    self.login_ip_throttle.release(&client);
                    (refused, "account")
                }),
        };
        if let Err((refused, limit)) = admitted {
            // One row per lockout, not one per refused request (R2-3).
            if refused.first {
                self.audit_login_refusal(&username, ip.as_deref(), limit, refused.retry_after)
                    .await;
            }
            return Err(AuthError::TooManyAttempts {
                retry_after: refused.retry_after,
            });
        }
        let lookup = username.clone();
        let found = self
            .db
            .run(move |s| s.get_user_by_username(&lookup))
            .await?;

        let verified = match &found {
            Some(user) => verify_password(&password, &user.password_hash),
            None => {
                let _ = verify_password(&password, dummy_hash());
                false
            }
        };

        let user = match found {
            Some(user) if verified && user.enabled => user,
            _ => {
                self.audit_login_failure(&username, ip.as_deref()).await;
                return Err(AuthError::InvalidCredentials);
            }
        };
        // A success is not a failure: its own attempt is taken back, and this account's count
        // from this client starts over. Failures on other accounts stay counted (R2-2).
        self.login_ip_throttle.release(&client);
        self.login_throttle.clear(&account);

        let raw_token = generate_token();
        let token_hash = sha256_hex(&raw_token);
        let now = unix_now();
        let user_id = user.id;
        self.db
            .run(move |s| {
                s.create_session(&SessionRow {
                    token_hash,
                    user_id,
                    created_at: now,
                    last_seen_at: now,
                    expires_at: now + IDLE_TTL_SECS,
                })
            })
            .await?;
        self.db.run(move |s| s.touch_last_login(user_id)).await?;

        let current = to_current_user(&user);
        self.audit("login", Some(&current), ip.as_deref(), None, None, true)
            .await;
        Ok((raw_token, current))
    }

    async fn audit_login_failure(&self, username: &str, ip: Option<&str>) {
        // #103: the audit table is not watched; the journal is.
        tracing::warn!(
            event = "login_failed",
            username = %audit_username(username),
            ip = ip.unwrap_or("-"),
            "failed login"
        );
        self.audit_failed_login(username, ip, None).await;
    }

    async fn audit_login_refusal(
        &self,
        username: &str,
        ip: Option<&str>,
        limit: &str,
        retry_after: Duration,
    ) {
        tracing::warn!(
            event = "login_refused",
            username = %audit_username(username),
            ip = ip.unwrap_or("-"),
            limit,
            retry_after_secs = retry_after.as_secs(),
            "login locked out: too many attempts"
        );
        let details = serde_json::json!({ "refused": "too_many_attempts", "limit": limit });
        self.audit_failed_login(username, ip, Some(details.to_string()))
            .await;
    }

    async fn audit_failed_login(&self, username: &str, ip: Option<&str>, details: Option<String>) {
        let entry = NewAuditEntry {
            event_type: "login".to_string(),
            username: Some(audit_username(username)),
            user_id: None,
            ip: ip.map(str::to_string).or_else(super::current_client_ip),
            target: None,
            details,
            success: false,
        };
        let spill = Arc::clone(&self.spill);
        if let Err(err) = self
            .db
            .run(move |s| crate::durable::insert_audit(&spill, s, &entry))
            .await
        {
            tracing::warn!(%err, "failed to write audit entry for failed login");
        }
    }

    /// Resolves a raw bearer token to its [`CurrentUser`], sliding the
    /// session's expiry forward when it hasn't been touched recently.
    /// `None` for an unknown, expired, or disabled-user session (storage
    /// already excludes disabled users' sessions — see `session_repo.rs`).
    pub async fn validate(&self, token: &str) -> Option<CurrentUser> {
        let token_hash = sha256_hex(token);
        let lookup_hash = token_hash.clone();
        let found = match self.db.run(move |s| s.get_session(&lookup_hash)).await {
            Ok(found) => found,
            Err(err) => {
                tracing::error!(%err, "session lookup failed");
                return None;
            }
        };

        let (session, user) = found?;
        let now = unix_now();

        // R-11 (review of !102): the idle expiry slides while a session is used, so without a
        // cap a session in daily use never ended. It ends ABSOLUTE_TTL_SECS after login.
        if session.expires_at <= now || now - session.created_at >= ABSOLUTE_TTL_SECS {
            let expired_hash = token_hash;
            if let Err(err) = self
                .db
                .run(move |s| portal_storage::with_retry(|| s.delete_session(&expired_hash)))
                .await
            {
                tracing::warn!(%err, "failed to delete expired session");
            }
            return None;
        }

        if now - session.last_seen_at >= BUMP_INTERVAL_SECS {
            let new_expires = now + IDLE_TTL_SECS;
            let bump_hash = token_hash;
            if let Err(err) = self
                .db
                .run(move |s| {
                    portal_storage::with_retry(|| s.bump_session(&bump_hash, now, new_expires))
                })
                .await
            {
                tracing::warn!(%err, "failed to bump session expiry");
            }
        }

        Some(to_current_user(&user))
    }

    /// Deletes the session; best-effort (logout should succeed from the
    /// caller's point of view even if the row was already gone).
    pub async fn logout(&self, token: &str, actor: Option<&CurrentUser>) {
        let token_hash = sha256_hex(token);
        if let Err(err) = self
            .db
            .run(move |s| portal_storage::with_retry(|| s.delete_session(&token_hash)))
            .await
        {
            tracing::warn!(%err, "failed to delete session on logout");
        }
        self.audit("logout", actor, None, None, None, true).await;
    }

    /// Verifies `old_password`, sets `new_password`, purges every session
    /// for the user (including the one used to call this), and opens a
    /// fresh one — the caller re-issues the session cookie from the
    /// returned raw token.
    pub async fn change_password(
        &self,
        user_id: i64,
        old_password: &str,
        new_password: &str,
    ) -> Result<String, AuthError> {
        validate_password(new_password)?;

        let user = self
            .db
            .run(move |s| s.get_user(user_id))
            .await?
            .ok_or(AuthError::InvalidCredentials)?;

        if !verify_password(old_password, &user.password_hash) {
            return Err(AuthError::InvalidCredentials);
        }

        let new_hash = hash_password(new_password)?;
        self.db
            .run(move |s| s.set_password(user_id, &new_hash))
            .await?;
        self.db
            .run(move |s| s.delete_sessions_for_user(user_id))
            .await?;

        let raw_token = generate_token();
        let token_hash = sha256_hex(&raw_token);
        let now = unix_now();
        self.db
            .run(move |s| {
                s.create_session(&SessionRow {
                    token_hash,
                    user_id,
                    created_at: now,
                    last_seen_at: now,
                    expires_at: now + IDLE_TTL_SECS,
                })
            })
            .await?;

        let current = to_current_user(&user);
        self.audit("password_change", Some(&current), None, None, None, true)
            .await;
        Ok(raw_token)
    }

    /// All users, ordered by username; disabled accounts are excluded
    /// unless `include_disabled`.
    pub async fn list_users(&self, include_disabled: bool) -> Result<Vec<User>, AuthError> {
        Ok(self.db.run(move |s| s.list_users(include_disabled)).await?)
    }

    /// Single user lookup by id, for the users page's master-detail panel
    /// (`routes/user_routes.rs`'s `user_detail` handler). `None` means "no
    /// such user" (a stale row, or a since-deleted account), not an error.
    pub async fn get_user(&self, id: i64) -> Result<Option<User>, AuthError> {
        Ok(self.db.run(move |s| s.get_user(id)).await?)
    }

    /// Admin "create user". Never creates a superuser — see [`NewUserInput`].
    pub async fn create_user(
        &self,
        actor: &CurrentUser,
        input: NewUserInput,
    ) -> Result<User, AuthError> {
        validate_username(&input.username)?;
        validate_full_name(input.full_name.as_deref())?;
        validate_password(&input.password)?;
        let username_for_audit = input.username.clone();
        let password_hash = hash_password(&input.password)?;

        let new_user = NewUser {
            username: input.username,
            password_hash,
            full_name: input.full_name,
            job_title: input.job_title,
            user_role: input.user_role.as_str().to_string(),
            is_superuser: false,
        };

        match self.db.run(move |s| s.create_user(&new_user)).await {
            Ok(user) => {
                self.audit(
                    "user_create",
                    Some(actor),
                    None,
                    Some(&user.username),
                    None,
                    true,
                )
                .await;
                Ok(user)
            }
            Err(StorageError::Conflict(_)) => {
                self.audit(
                    "user_create",
                    Some(actor),
                    None,
                    Some(&username_for_audit),
                    None,
                    false,
                )
                .await;
                Err(AuthError::UsernameTaken)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Changes a user's full name and/or role (plan/portal/user_admin.md,
    /// decisions.md #34). `None` means the field was not submitted; for the
    /// name, a blank string clears it. Only a field whose value changes is
    /// guarded, written and audited (`user_name_change`, `user_role_change`,
    /// one row each with from/to); a refusal writes that field's event as
    /// failed with `{"refused": slug}` and writes nothing at all.
    ///
    /// Never touches `is_superuser`, so the existing disable/delete guards
    /// keep an enabled superuser, i.e. an admin-capable account, whatever
    /// roles change. No session is purged: the role is read per request.
    pub async fn update_user(
        &self,
        actor: &CurrentUser,
        id: i64,
        full_name: Option<&str>,
        user_role: Option<&str>,
    ) -> Result<User, AuthError> {
        let target = self
            .db
            .run(move |s| s.get_user(id))
            .await?
            .ok_or_else(|| AuthError::Validation(ValidationReason::UserNotFound))?;

        let mut name_change = None;
        if let Some(raw) = full_name {
            let checked = normalize_full_name(raw).and_then(|name| {
                if name == target.full_name {
                    return Ok(None);
                }
                if target.is_superuser && !actor.is_superuser {
                    return Err(AuthError::SelfGuard(SelfGuardReason::NameChangeSuperuser));
                }
                Ok(Some(name))
            });
            match checked {
                Ok(change) => name_change = change,
                Err(err) => return Err(self.refuse("user_name_change", actor, &target, err).await),
            }
        }

        let mut role_change = None;
        if let Some(raw) = user_role {
            let checked = raw
                .parse::<Role>()
                .map_err(|_| AuthError::Validation(ValidationReason::InvalidRole))
                .and_then(|role| {
                    if role.as_str() == target.user_role {
                        return Ok(None);
                    }
                    if target.id == actor.id {
                        return Err(AuthError::SelfGuard(SelfGuardReason::RoleChangeSelf));
                    }
                    // The flag outranks the column: the change would do nothing.
                    if target.is_superuser {
                        return Err(AuthError::SelfGuard(SelfGuardReason::RoleChangeSuperuser));
                    }
                    // Admins are peers, as for delete.
                    if target.user_role == Role::Admin.as_str() && !actor.is_superuser {
                        return Err(AuthError::SelfGuard(SelfGuardReason::RoleChangeAdmin));
                    }
                    Ok(Some(role))
                });
            match checked {
                Ok(change) => role_change = change,
                Err(err) => return Err(self.refuse("user_role_change", actor, &target, err).await),
            }
        }

        if name_change.is_none() && role_change.is_none() {
            return Ok(target);
        }
        // Only the fields that change are written: the other one stays as
        // stored rather than as `target` read it, so a concurrent edit of it
        // by another admin is not undone.
        let new_role = role_change.map_or(target.user_role.clone(), |r| r.as_str().to_string());
        let write_name = name_change.clone();
        let write_role = role_change.map(|_| new_role.clone());
        let updated = self
            .db
            .run(move |s| {
                s.update_user(
                    id,
                    write_name.as_ref().map(|n| n.as_deref()),
                    write_role.as_deref(),
                )
            })
            .await?
            .ok_or_else(|| AuthError::Validation(ValidationReason::UserNotFound))?;

        if let Some(to) = name_change {
            let details = serde_json::json!({ "from": target.full_name, "to": to });
            self.audit(
                "user_name_change",
                Some(actor),
                None,
                Some(&target.username),
                Some(details.to_string()),
                true,
            )
            .await;
        }
        if role_change.is_some() {
            let details = serde_json::json!({ "from": target.user_role, "to": new_role });
            self.audit(
                "user_role_change",
                Some(actor),
                None,
                Some(&target.username),
                Some(details.to_string()),
                true,
            )
            .await;
        }
        Ok(updated)
    }

    /// The failed audit row for a refused [`Self::update_user`] field
    /// (decisions.md #28 (e)); hands the error back for the caller.
    async fn refuse(
        &self,
        event_type: &str,
        actor: &CurrentUser,
        target: &User,
        err: AuthError,
    ) -> AuthError {
        let details = serde_json::json!({ "refused": err.slug() });
        self.audit(
            event_type,
            Some(actor),
            None,
            Some(&target.username),
            Some(details.to_string()),
            false,
        )
        .await;
        err
    }

    /// Enable/disable a user. Guards: an admin can't disable their own
    /// account, and can't disable the last enabled superuser. Disabling
    /// purges the target's sessions.
    pub async fn set_enabled(
        &self,
        actor: &CurrentUser,
        id: i64,
        enabled: bool,
    ) -> Result<User, AuthError> {
        let target = self
            .db
            .run(move |s| s.get_user(id))
            .await?
            .ok_or_else(|| AuthError::Validation(ValidationReason::UserNotFound))?;
        guard_privileged(actor, &target)?;
        if !enabled {
            if id == actor.id {
                return Err(AuthError::SelfGuard(SelfGuardReason::DisableSelf));
            }
            if target.is_superuser && target.enabled {
                let count = self.db.run(|s| s.count_superusers()).await?;
                if count <= 1 {
                    return Err(AuthError::SelfGuard(SelfGuardReason::DisableLastSuperuser));
                }
            }
        }

        let changed = self.db.run(move |s| s.set_enabled(id, enabled)).await?;
        if !changed {
            return Err(AuthError::Validation(ValidationReason::UserNotFound));
        }
        if !enabled {
            self.db.run(move |s| s.delete_sessions_for_user(id)).await?;
        }

        let user = self
            .db
            .run(move |s| s.get_user(id))
            .await?
            .ok_or_else(|| AuthError::Validation(ValidationReason::UserNotFound))?;

        self.audit(
            if enabled {
                "user_enable"
            } else {
                "user_disable"
            },
            Some(actor),
            None,
            Some(&user.username),
            None,
            true,
        )
        .await;
        Ok(user)
    }

    /// Deletes a user. Guards: can't delete your own account, only a
    /// superuser can delete a superuser or an admin, can't delete the last
    /// enabled superuser. Sessions cascade via `ON DELETE CASCADE`.
    pub async fn delete_user(&self, actor: &CurrentUser, id: i64) -> Result<(), AuthError> {
        if id == actor.id {
            return Err(AuthError::SelfGuard(SelfGuardReason::DeleteSelf));
        }
        let target = self
            .db
            .run(move |s| s.get_user(id))
            .await?
            .ok_or_else(|| AuthError::Validation(ValidationReason::UserNotFound))?;

        // The bootstrap account outranks the admin role: a role-admin must
        // not be able to remove a superuser (user_detail.html hides the
        // button for the same case; this is the authoritative check).
        if target.is_superuser && !actor.is_superuser {
            return Err(AuthError::SelfGuard(SelfGuardReason::DeleteSuperuser));
        }

        // Admins are peers, not each other's managers: deleting an admin
        // account also takes the superuser.
        if target.user_role == Role::Admin.as_str() && !actor.is_superuser {
            return Err(AuthError::SelfGuard(SelfGuardReason::DeleteAdmin));
        }

        if target.is_superuser && target.enabled {
            let count = self.db.run(|s| s.count_superusers()).await?;
            if count <= 1 {
                return Err(AuthError::SelfGuard(SelfGuardReason::DeleteLastSuperuser));
            }
        }

        let deleted = self.db.run(move |s| s.delete_user(id)).await?;
        if !deleted {
            return Err(AuthError::Validation(ValidationReason::UserNotFound));
        }

        self.audit(
            "user_delete",
            Some(actor),
            None,
            Some(&target.username),
            None,
            true,
        )
        .await;
        Ok(())
    }

    /// Admin password reset — not permitted on the actor's own account
    /// (use [`Self::change_password`] instead, per v1). Purges the target's
    /// sessions.
    pub async fn reset_password(
        &self,
        actor: &CurrentUser,
        id: i64,
        new_password: &str,
    ) -> Result<(), AuthError> {
        if id == actor.id {
            return Err(AuthError::SelfGuard(
                SelfGuardReason::UseChangePasswordForSelf,
            ));
        }
        validate_password(new_password)?;

        let target = self
            .db
            .run(move |s| s.get_user(id))
            .await?
            .ok_or_else(|| AuthError::Validation(ValidationReason::UserNotFound))?;
        guard_privileged(actor, &target)?;

        let new_hash = hash_password(new_password)?;
        let changed = self.db.run(move |s| s.set_password(id, &new_hash)).await?;
        if !changed {
            return Err(AuthError::Validation(ValidationReason::UserNotFound));
        }
        self.db.run(move |s| s.delete_sessions_for_user(id)).await?;

        self.audit(
            "password_reset",
            Some(actor),
            None,
            Some(&target.username),
            None,
            true,
        )
        .await;
        Ok(())
    }

    /// Filtered, paginated audit log read.
    pub async fn audit_page(
        &self,
        filter: AuditFilter,
    ) -> Result<(Vec<AuditEntry>, i64), AuthError> {
        Ok(self.db.run(move |s| s.list_audit(&filter)).await?)
    }

    /// The log's own numbers for the metric row above `/audit`.
    pub async fn audit_counts(&self) -> Result<portal_storage::AuditCounts, AuthError> {
        Ok(self.db.run(|s| s.audit_counts()).await?)
    }

    /// The values the log's two identity columns actually hold, for the
    /// filter dropdowns — see `audit_filter_options` for why they come from
    /// the log rather than from the live user list.
    pub async fn audit_filter_options(&self) -> Result<(Vec<String>, Vec<String>), AuthError> {
        Ok(self.db.run(|s| s.audit_filter_options()).await?)
    }

    /// [`Self::audit`] for an actor known only by name: a background write
    /// done on a user's behalf (config sync re-signs with the editor's key,
    /// and the editor may no longer have an account). `None` = the portal.
    pub async fn audit_as(
        &self,
        event_type: &str,
        username: Option<&str>,
        target: Option<&str>,
        details_json: Option<String>,
        success: bool,
    ) {
        let entry = NewAuditEntry {
            event_type: event_type.to_string(),
            username: username.map(str::to_string),
            user_id: None,
            ip: None,
            target: target.map(str::to_string),
            details: details_json,
            success,
        };
        let spill = Arc::clone(&self.spill);
        if let Err(err) = self
            .db
            .run(move |s| crate::durable::insert_audit(&spill, s, &entry))
            .await
        {
            tracing::warn!(%err, event_type, "failed to write audit entry");
        }
    }

    /// Fire-and-forget audit write: errors are logged, never propagated —
    /// an audit failure must not block the action it's describing.
    pub async fn audit(
        &self,
        event_type: &str,
        actor: Option<&CurrentUser>,
        ip: Option<&str>,
        target: Option<&str>,
        details_json: Option<String>,
        success: bool,
    ) {
        let entry = NewAuditEntry {
            event_type: event_type.to_string(),
            username: actor.map(|u| u.username.clone()),
            user_id: actor.map(|u| u.id),
            // C36-11: a route that does not pass one gets the request's own client.
            ip: ip.map(str::to_string).or_else(super::current_client_ip),
            target: target.map(str::to_string),
            details: details_json,
            success,
        };
        let spill = Arc::clone(&self.spill);
        if let Err(err) = self
            .db
            .run(move |s| crate::durable::insert_audit(&spill, s, &entry))
            .await
        {
            tracing::warn!(%err, event_type, "failed to write audit entry");
        }
    }
}

fn validate_username(username: &str) -> Result<(), AuthError> {
    let len = username.chars().count();
    if !(3..=50).contains(&len) {
        return Err(AuthError::Validation(ValidationReason::UsernameLength));
    }
    if username
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(AuthError::Validation(ValidationReason::UsernameChars));
    }
    Ok(())
}

/// A full name given at creation passes the same rule as an edit's (LE-14,
/// `normalize_full_name`).
fn validate_full_name(full_name: Option<&str>) -> Result<(), AuthError> {
    full_name.map_or(Ok(()), |n| normalize_full_name(n).map(drop))
}

fn validate_password(password: &str) -> Result<(), AuthError> {
    let len = password.chars().count();
    if !(8..=128).contains(&len) {
        return Err(AuthError::Validation(ValidationReason::PasswordLength));
    }
    Ok(())
}

/// Trimmed; blank means no name; at most 100 characters, no control
/// characters (user_admin.md §2).
fn normalize_full_name(raw: &str) -> Result<Option<String>, AuthError> {
    let name = raw.trim();
    if name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(AuthError::Validation(ValidationReason::FullNameInvalid));
    }
    Ok((!name.is_empty()).then(|| name.to_string()))
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut PasswordOsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| AuthError::Validation(ValidationReason::HashingFailed(err.to_string())))
}

fn verify_password(password: &str, phc_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// A cached argon2 hash of a fixed dummy password, verified against on an
/// unknown-username login so that path costs the same as a real verify
/// (no username-enumeration timing oracle).
static DUMMY_HASH: OnceLock<String> = OnceLock::new();

fn dummy_hash() -> &'static str {
    DUMMY_HASH.get_or_init(|| hash_password("timing-oracle-defeater").unwrap_or_default())
}

/// A superuser or an admin is managed by a superuser only (LE-6): the same rule `delete_user`
/// applies, now for resetting a password and for enabling or disabling the account too.
fn guard_privileged(actor: &CurrentUser, target: &User) -> Result<(), AuthError> {
    let privileged = target.is_superuser || target.user_role == Role::Admin.as_str();
    if privileged && !actor.is_superuser && target.id != actor.id {
        return Err(AuthError::SelfGuard(
            SelfGuardReason::ManageSuperuserOrAdmin,
        ));
    }
    Ok(())
}

pub(crate) fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    TokenOsRng.fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

pub(crate) fn sha256_hex(input: &str) -> String {
    hex_encode(&Sha256::digest(input.as_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // `write!` into a `String` is infallible; nothing to propagate.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn to_current_user(user: &User) -> CurrentUser {
    let user_role = user.user_role.parse().unwrap_or_else(|_| {
        tracing::warn!(role = %user.user_role, "unrecognized user_role; defaulting to viewer");
        Role::Viewer
    });
    CurrentUser {
        id: user.id,
        username: user.username.clone(),
        full_name: user.full_name.clone(),
        user_role,
        is_superuser: user.is_superuser,
        monitoring_view_mode: user.monitoring_view_mode.clone(),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    const PASSWORD: &str = "hunter2-hunter2";

    fn open_service() -> (tempfile::TempDir, AuthService) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path").to_string();
        let service =
            AuthService::open(&path, None, crate::durable::test_gate(&path)).expect("open");
        (dir, service)
    }

    #[tokio::test]
    async fn setup_login_validate_logout_roundtrip() {
        let (_dir, auth) = open_service();
        assert!(auth.setup_required().await);

        let created = auth
            .create_superuser("root".into(), PASSWORD.into(), Some("Root".into()))
            .await
            .expect("create superuser");
        assert!(created.is_superuser);
        assert!(!auth.setup_required().await);

        let (token, current) = auth
            .login("root".into(), PASSWORD.into(), Some("127.0.0.1".into()))
            .await
            .expect("login");
        assert_eq!(current.username, "root");
        assert!(current.is_admin());

        let validated = auth.validate(&token).await.expect("validate");
        assert_eq!(validated.id, current.id);

        auth.logout(&token, Some(&validated)).await;
        assert!(auth.validate(&token).await.is_none());
    }

    #[tokio::test]
    async fn setup_refused_once_done() {
        let (_dir, auth) = open_service();
        auth.create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("first setup");

        let err = auth
            .create_superuser("root2".into(), PASSWORD.into(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::SetupAlreadyDone));
    }

    #[tokio::test]
    async fn wrong_password_and_unknown_user_both_invalid_credentials() {
        let (_dir, auth) = open_service();
        auth.create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");

        let wrong = auth
            .login("root".into(), "totally-wrong-pw".into(), None)
            .await
            .unwrap_err();
        assert!(matches!(wrong, AuthError::InvalidCredentials));

        let unknown = auth
            .login("nobody".into(), "whatever-password".into(), None)
            .await
            .unwrap_err();
        assert!(matches!(unknown, AuthError::InvalidCredentials));
    }

    /// LE-11: v1's ten failed logins a minute per client; the eleventh is refused before the
    /// password is checked, even a right one, and the lockout is audited once. Another client
    /// is not affected. A success resets its own account's count but not the failures on
    /// another account (R2-2), and the client as a whole is capped too.
    #[tokio::test]
    async fn failed_logins_are_throttled_per_client() {
        let (_dir, auth) = open_service();
        auth.create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");
        let from = |ip: &str| Some(ip.to_string());
        let wrong =
            |user: &str, ip: &str| auth.login(user.into(), "totally-wrong-pw".into(), from(ip));

        for _ in 0..9 {
            let err = wrong("root", "192.0.2.1").await.unwrap_err();
            assert!(matches!(err, AuthError::InvalidCredentials));
        }
        auth.login("root".into(), PASSWORD.into(), from("192.0.2.1"))
            .await
            .expect("the tenth attempt, right, resets root's count");
        for _ in 0..10 {
            let _ = wrong("root", "192.0.2.1").await;
        }
        for _ in 0..3 {
            let err = auth
                .login("root".into(), PASSWORD.into(), from("192.0.2.1"))
                .await
                .unwrap_err();
            assert!(matches!(err, AuthError::TooManyAttempts { .. }), "{err:?}");
        }
        auth.login("root".into(), PASSWORD.into(), from("192.0.2.2"))
            .await
            .expect("another client logs in");

        let refused: Vec<_> = auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("login".into()),
                success: Some(false),
                limit: 100,
                ..Default::default()
            })
            .await
            .expect("audit")
            .0
            .into_iter()
            .filter(|r| r.details.is_some())
            .collect();
        assert_eq!(
            refused.len(),
            1,
            "one row for the lockout, not one per request"
        );
        assert_eq!(
            refused[0].details.as_deref(),
            Some(r#"{"limit":"account","refused":"too_many_attempts"}"#)
        );
        assert_eq!(refused[0].ip.as_deref(), Some("192.0.2.1"));

        // R2-2: a success on one's own account does not free another account's count, and
        // spraying names from one client stops at the client cap.
        let root = auth
            .login("root".into(), PASSWORD.into(), from("192.0.2.9"))
            .await
            .expect("root")
            .1;
        auth.create_user(
            &root,
            NewUserInput {
                username: "viewer".into(),
                password: PASSWORD.into(),
                full_name: None,
                job_title: None,
                user_role: Role::Viewer,
            },
        )
        .await
        .expect("viewer");
        for _ in 0..9 {
            let _ = wrong("root", "192.0.2.3").await;
        }
        auth.login("viewer".into(), PASSWORD.into(), from("192.0.2.3"))
            .await
            .expect("own account");
        assert!(matches!(
            wrong("root", "192.0.2.3").await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(
            matches!(
                wrong("root", "192.0.2.3").await,
                Err(AuthError::TooManyAttempts { .. })
            ),
            "the viewer's login did not reset root's count"
        );
        let mut sprayed = 0;
        for i in 0..40 {
            match wrong(&format!("user{i}"), "192.0.2.4").await {
                Err(AuthError::InvalidCredentials) => sprayed += 1,
                Err(AuthError::TooManyAttempts { .. }) => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(sprayed, LOGIN_ATTEMPTS_PER_CLIENT_PER_MINUTE);
    }

    /// C36-11: an audit row written while a request is served carries its client, though the
    /// route passed none; the setup row names the superuser it created.
    #[tokio::test]
    async fn audit_rows_carry_the_requests_client() {
        let (_dir, auth) = open_service();
        crate::auth::with_client_ip("192.0.2.5", async {
            auth.create_superuser("root".into(), PASSWORD.into(), None)
                .await
                .expect("setup");
            auth.audit("alarm_ack", None, None, Some("a1"), None, true)
                .await;
        })
        .await;
        let rows = auth
            .audit_page(portal_storage::AuditFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .expect("audit")
            .0;
        let row = |event: &str| rows.iter().find(|r| r.event_type == event).expect(event);
        assert_eq!(row("alarm_ack").ip.as_deref(), Some("192.0.2.5"));
        assert_eq!(row("setup").ip.as_deref(), Some("192.0.2.5"));
        assert_eq!(row("setup").username.as_deref(), Some("root"));
    }

    #[test]
    fn setup_attempts_are_throttled_per_client() {
        let (_dir, auth) = open_service();
        for _ in 0..SETUP_ATTEMPTS_PER_MINUTE {
            auth.setup_attempt(Some("192.0.2.1"))
                .expect("within the limit");
        }
        assert!(matches!(
            auth.setup_attempt(Some("192.0.2.1")),
            Err(AuthError::TooManyAttempts { .. })
        ));
        auth.setup_attempt(Some("192.0.2.2"))
            .expect("another client");
    }

    #[tokio::test]
    async fn disabled_user_cannot_login_and_existing_session_dies() {
        let (_dir, auth) = open_service();
        let root = auth
            .create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");
        let root_current = to_current_user(&root);

        let member = auth
            .create_user(
                &root_current,
                NewUserInput {
                    username: "alice".into(),
                    password: "password123".into(),
                    full_name: None,
                    job_title: None,
                    user_role: Role::Viewer,
                },
            )
            .await
            .expect("create user");

        let (token, _) = auth
            .login("alice".into(), "password123".into(), None)
            .await
            .expect("login");
        assert!(auth.validate(&token).await.is_some());

        auth.set_enabled(&root_current, member.id, false)
            .await
            .expect("disable");

        assert!(auth.validate(&token).await.is_none());
        let err = auth
            .login("alice".into(), "password123".into(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn sliding_expiry_bumps_after_bump_interval() {
        let (_dir, auth) = open_service();
        auth.create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");
        let (token, _) = auth
            .login("root".into(), PASSWORD.into(), None)
            .await
            .expect("login");

        let token_hash = sha256_hex(&token);
        let backdated = unix_now() - BUMP_INTERVAL_SECS - 10;
        {
            let storage = auth.db.lock().expect("lock");
            storage
                .bump_session(&token_hash, backdated, backdated + IDLE_TTL_SECS)
                .expect("backdate");
        }

        assert!(auth.validate(&token).await.is_some());

        let storage = auth.db.lock().expect("lock");
        let (session, _) = storage
            .get_session(&token_hash)
            .expect("get session")
            .expect("present");
        assert!(session.last_seen_at > backdated);
    }

    /// R-11: a session used every day still ends ABSOLUTE_TTL_SECS after login.
    #[tokio::test]
    async fn a_session_ends_a_week_after_login_however_used() {
        let (_dir, auth) = open_service();
        let root = auth
            .create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");
        let now = unix_now();
        let token = "a-week-old-token";
        {
            let storage = auth.db.lock().expect("lock");
            storage
                .create_session(&SessionRow {
                    token_hash: sha256_hex(token),
                    user_id: root.id,
                    created_at: now - ABSOLUTE_TTL_SECS - 1,
                    last_seen_at: now,
                    expires_at: now + IDLE_TTL_SECS,
                })
                .expect("old session");
        }
        assert!(auth.validate(token).await.is_none());
    }

    #[tokio::test]
    async fn change_password_invalidates_old_sessions() {
        let (_dir, auth) = open_service();
        auth.create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");
        let (old_token, current) = auth
            .login("root".into(), PASSWORD.into(), None)
            .await
            .expect("login");

        let new_token = auth
            .change_password(current.id, PASSWORD, "new-password-1")
            .await
            .expect("change password");

        assert!(auth.validate(&old_token).await.is_none());
        assert!(auth.validate(&new_token).await.is_some());

        let err = auth
            .login("root".into(), PASSWORD.into(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidCredentials));
        auth.login("root".into(), "new-password-1".into(), None)
            .await
            .expect("login with new password");
    }

    #[tokio::test]
    async fn self_guards_and_last_superuser() {
        let (_dir, auth) = open_service();
        let root = auth
            .create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");
        let root_current = to_current_user(&root);

        let err = auth
            .set_enabled(&root_current, root.id, false)
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::SelfGuard(_)));

        let err = auth.delete_user(&root_current, root.id).await.unwrap_err();
        assert!(matches!(err, AuthError::SelfGuard(_)));

        let err = auth
            .reset_password(&root_current, root.id, "another-password")
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::SelfGuard(_)));

        // A *different* admin trying to remove the sole superuser must also
        // be blocked — the guard is about the last superuser, not "self".
        let other_admin = auth
            .create_user(
                &root_current,
                NewUserInput {
                    username: "bob".into(),
                    password: "password123".into(),
                    full_name: None,
                    job_title: None,
                    user_role: Role::Admin,
                },
            )
            .await
            .expect("create admin");
        let other_admin_current = to_current_user(&other_admin);

        let err = auth
            .set_enabled(&other_admin_current, root.id, false)
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::SelfGuard(_)));

        // Delete is refused for a role-admin actor regardless of superuser
        // count — the DeleteSuperuser guard fires before the last-superuser
        // one.
        let err = auth
            .delete_user(&other_admin_current, root.id)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AuthError::SelfGuard(SelfGuardReason::DeleteSuperuser)
        ));

        // The superuser can still delete an ordinary admin — the guard is
        // about superuser targets, not about who may delete at all.
        auth.delete_user(&root_current, other_admin.id)
            .await
            .expect("superuser deletes role-admin");
    }

    #[tokio::test]
    async fn only_a_superuser_can_delete_an_admin() {
        let (_dir, auth) = open_service();
        let root = auth
            .create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");
        let root_current = to_current_user(&root);

        let input = |name: &str, role: Role| NewUserInput {
            username: name.into(),
            password: "password123".into(),
            full_name: None,
            job_title: None,
            user_role: role,
        };
        let alice = auth
            .create_user(&root_current, input("alice", Role::Admin))
            .await
            .expect("create alice");
        let bob = auth
            .create_user(&root_current, input("bob", Role::Admin))
            .await
            .expect("create bob");
        let carol = auth
            .create_user(&root_current, input("carol", Role::Viewer))
            .await
            .expect("create carol");
        let alice_current = to_current_user(&alice);

        let err = auth.delete_user(&alice_current, bob.id).await.unwrap_err();
        assert!(matches!(
            err,
            AuthError::SelfGuard(SelfGuardReason::DeleteAdmin)
        ));

        // LE-6: nor reset an admin's password or disable them.
        let err = auth
            .reset_password(&alice_current, bob.id, "a long enough password")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AuthError::SelfGuard(SelfGuardReason::ManageSuperuserOrAdmin)
        ));
        let err = auth
            .set_enabled(&alice_current, bob.id, false)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AuthError::SelfGuard(SelfGuardReason::ManageSuperuserOrAdmin)
        ));

        // An admin still manages non-admin users…
        auth.delete_user(&alice_current, carol.id)
            .await
            .expect("admin deletes viewer");
        // …and the superuser still deletes admins.
        auth.delete_user(&root_current, bob.id)
            .await
            .expect("superuser deletes admin");
    }

    /// LE-14: padded, spaced or control-character usernames and an oversized full name are
    /// refused before anything is stored.
    #[test]
    fn usernames_and_full_names_are_checked() {
        for bad in ["   ", " pad ", "has space", "nul\u{0}x"] {
            assert!(
                matches!(
                    validate_username(bad),
                    Err(AuthError::Validation(ValidationReason::UsernameLength))
                        | Err(AuthError::Validation(ValidationReason::UsernameChars))
                ),
                "{bad:?}"
            );
        }
        assert!(validate_username("jan.novak@hala").is_ok());
        assert!(validate_full_name(Some(&"x".repeat(201))).is_err());
        assert!(validate_full_name(Some("Jan Novák")).is_ok());
        assert!(validate_full_name(None).is_ok());
    }

    #[tokio::test]
    async fn create_user_conflict_maps_to_username_taken() {
        let (_dir, auth) = open_service();
        let root = auth
            .create_superuser("root".into(), PASSWORD.into(), None)
            .await
            .expect("setup");
        let root_current = to_current_user(&root);

        let input = || NewUserInput {
            username: "alice".into(),
            password: "password123".into(),
            full_name: None,
            job_title: None,
            user_role: Role::Viewer,
        };
        auth.create_user(&root_current, input())
            .await
            .expect("first create");

        let err = auth.create_user(&root_current, input()).await.unwrap_err();
        assert!(matches!(err, AuthError::UsernameTaken));
    }

    /// The seeded superuser plus an admin, a manager and a viewer it created.
    async fn team(auth: &AuthService) -> (CurrentUser, User, User, User) {
        let root = auth
            .create_superuser("root".into(), PASSWORD.into(), Some("Root".into()))
            .await
            .expect("setup");
        let root = to_current_user(&root);
        let alice = create(auth, &root, "alice", Role::Admin).await;
        let mia = create(auth, &root, "mia", Role::Manager).await;
        let vic = create(auth, &root, "vic", Role::Viewer).await;
        (root, alice, mia, vic)
    }

    async fn create(auth: &AuthService, actor: &CurrentUser, name: &str, role: Role) -> User {
        auth.create_user(
            actor,
            NewUserInput {
                username: name.into(),
                password: "password123".into(),
                full_name: Some(format!("{name} Novák")),
                job_title: None,
                user_role: role,
            },
        )
        .await
        .expect("create user")
    }

    async fn audit_rows(auth: &AuthService, event: &str) -> Vec<AuditEntry> {
        let filter = AuditFilter {
            event_type: Some(event.to_string()),
            limit: 100,
            ..Default::default()
        };
        auth.audit_page(filter).await.expect("audit").0
    }

    fn details(row: &AuditEntry) -> serde_json::Value {
        serde_json::from_str(row.details.as_deref().expect("details")).expect("json")
    }

    /// user_admin.md §3: every guard, with its slug, and nothing written.
    #[tokio::test]
    async fn update_user_guards_refuse_with_their_slugs() {
        let (_dir, auth) = open_service();
        let (root, alice, mia, _vic) = team(&auth).await;
        let alice_c = to_current_user(&alice);
        let bob = create(&auth, &root, "bob", Role::Admin).await;

        let cases: [(i64, Option<&str>, Option<&str>, &str); 7] = [
            (
                root.id,
                Some("Someone"),
                None,
                "self_guard_name_change_superuser",
            ),
            (
                alice.id,
                None,
                Some("viewer"),
                "self_guard_role_change_self",
            ),
            (
                root.id,
                None,
                Some("viewer"),
                "self_guard_role_change_superuser",
            ),
            (
                bob.id,
                None,
                Some("manager"),
                "self_guard_role_change_admin",
            ),
            (mia.id, None, Some("overlord"), "invalid_role"),
            (mia.id, Some(&"x".repeat(101)), None, "full_name_invalid"),
            (mia.id, Some("Mia\u{7}"), None, "full_name_invalid"),
        ];
        for (id, name, role, slug) in cases {
            let err = auth
                .update_user(&alice_c, id, name, role)
                .await
                .expect_err(slug);
            assert_eq!(err.slug(), slug);
        }
        assert_eq!(auth.get_user(mia.id).await.unwrap().unwrap(), mia);
        assert_eq!(auth.get_user(bob.id).await.unwrap().unwrap(), bob);

        // A superuser renames itself, and anyone renames themselves.
        auth.update_user(&root, root.id, Some("Root Renamed"), None)
            .await
            .expect("a superuser renames itself");
        auth.update_user(&alice_c, alice.id, Some("Alice"), None)
            .await
            .expect("an admin renames itself");
    }

    #[tokio::test]
    async fn a_superuser_demotes_an_admin_and_an_admin_promotes_a_manager() {
        let (_dir, auth) = open_service();
        let (root, alice, mia, _vic) = team(&auth).await;

        let alice = auth
            .update_user(&root, alice.id, None, Some("viewer"))
            .await
            .expect("superuser demotes admin");
        assert_eq!(alice.user_role, "viewer");

        let bob = to_current_user(&create(&auth, &root, "bob", Role::Admin).await);
        let mia = auth
            .update_user(&bob, mia.id, None, Some("admin"))
            .await
            .expect("admin promotes manager");
        assert_eq!(mia.user_role, "admin");
    }

    #[tokio::test]
    async fn a_blank_name_clears_it_and_unchanged_fields_write_nothing() {
        let (_dir, auth) = open_service();
        let (root, _alice, mia, _vic) = team(&auth).await;

        // The same values, padded: nothing changes, nothing is audited.
        auth.update_user(&root, mia.id, Some("  mia Novák "), Some("manager"))
            .await
            .expect("no-op");
        assert!(audit_rows(&auth, "user_name_change").await.is_empty());
        assert!(audit_rows(&auth, "user_role_change").await.is_empty());

        let cleared = auth
            .update_user(&root, mia.id, Some("   "), None)
            .await
            .expect("clear");
        assert_eq!(cleared.full_name, None);
    }

    /// §4: one row per changed field with from/to; a refusal is a failed row.
    #[tokio::test]
    async fn each_changed_field_is_audited_with_from_and_to() {
        let (_dir, auth) = open_service();
        let (root, alice, mia, _vic) = team(&auth).await;

        auth.update_user(&root, mia.id, Some("Mia Nová"), Some("viewer"))
            .await
            .expect("edit");

        let names = audit_rows(&auth, "user_name_change").await;
        assert_eq!(names.len(), 1);
        assert!(names[0].success);
        assert_eq!(names[0].target.as_deref(), Some("mia"));
        assert_eq!(names[0].username.as_deref(), Some("root"));
        assert_eq!(
            details(&names[0]),
            serde_json::json!({"from": "mia Novák", "to": "Mia Nová"})
        );
        let roles = audit_rows(&auth, "user_role_change").await;
        assert_eq!(roles.len(), 1);
        assert_eq!(
            details(&roles[0]),
            serde_json::json!({"from": "manager", "to": "viewer"})
        );

        // Clearing records `null`.
        auth.update_user(&root, mia.id, Some(""), None)
            .await
            .expect("clear");
        let names = audit_rows(&auth, "user_name_change").await;
        assert!(names
            .iter()
            .any(|r| details(r) == serde_json::json!({"from": "Mia Nová", "to": null})));

        // A refusal writes the same event, failed, with the slug.
        auth.update_user(&to_current_user(&alice), alice.id, None, Some("viewer"))
            .await
            .unwrap_err();
        let roles = audit_rows(&auth, "user_role_change").await;
        let failed: Vec<_> = roles.iter().filter(|r| !r.success).collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(
            details(failed[0]),
            serde_json::json!({"refused": "self_guard_role_change_self"})
        );
    }

    /// §3 "No lockout is possible": with every role-admin demoted, an enabled
    /// account still holds the admin permission set.
    #[tokio::test]
    async fn demoting_every_admin_leaves_an_admin_capable_account() {
        let (_dir, auth) = open_service();
        let (root, alice, _mia, _vic) = team(&auth).await;
        auth.update_user(&root, alice.id, None, Some("viewer"))
            .await
            .expect("demote");

        let users = auth.list_users(false).await.unwrap();
        assert!(users
            .iter()
            .all(|u| u.user_role != "admin" || u.is_superuser));
        assert!(users
            .iter()
            .any(|u| u.enabled && to_current_user(u).has_role(Role::Admin)));
    }
}
