//! Shared auth types: the authenticated-request principal, its role, and
//! the error set every [`crate::auth::AuthService`] method returns.

use std::str::FromStr;

use portal_storage::StorageError;
use serde::Serialize;
use thiserror::Error;

/// Authz role (`user_role` column). Distinct from `is_superuser`, which is
/// a separate bootstrap flag — see [`CurrentUser::is_admin`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Manager,
    Admin,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Manager => "manager",
            Role::Admin => "admin",
        }
    }

    /// Rank on the authz ladder — Viewer < Manager < Admin.
    ///
    /// Deliberately **not** a `PartialOrd`/`Ord` derive. Ordering the enum
    /// would invite `user.user_role >= Role::Manager` at call sites, which
    /// silently drops `is_superuser` — the bootstrap flag that sits
    /// *orthogonal* to this ladder ([`CurrentUser::is_admin`]; `perms.rs`'s
    /// own test asserts a superuser Viewer carries the Admin permission
    /// set). Keeping the rank private to this method means the only way to
    /// ask the question is [`CurrentUser::has_role`], which folds both axes.
    fn rank(self) -> u8 {
        match self {
            Role::Viewer => 0,
            Role::Manager => 1,
            Role::Admin => 2,
        }
    }

    /// `true` when this role is `min` or above on the ladder. Superuser-blind
    /// by construction — see [`Role::rank`]; callers holding a
    /// [`CurrentUser`] want [`CurrentUser::has_role`] instead.
    pub fn at_least(self, min: Role) -> bool {
        self.rank() >= min.rank()
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "viewer" => Ok(Role::Viewer),
            "manager" => Ok(Role::Manager),
            "admin" => Ok(Role::Admin),
            other => Err(format!("invalid role: {other}")),
        }
    }
}

/// The authenticated principal, as resolved from a session token. Never
/// carries the password hash — see `portal_storage::User` for the full row.
#[derive(Clone, Debug, Serialize)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub full_name: Option<String>,
    pub user_role: Role,
    pub is_superuser: bool,
    /// This operator's stored `/monitoring` layout (V8). Carried on the
    /// session's user rather than fetched per render — the monitoring page
    /// needs it on every request and it changes only when they pick another
    /// one. `crate::monitoring::VIEW_MODES` is the valid set.
    pub monitoring_view_mode: String,
}

impl CurrentUser {
    /// `true` for the `admin` role OR the bootstrap superuser flag — both
    /// gate v1's `require_admin`-equivalent surface (plan/v1/viewer/rest.md).
    pub fn is_admin(&self) -> bool {
        self.is_superuser || self.user_role == Role::Admin
    }

    /// `true` when this principal holds `min` or better — the single
    /// authority for every role gate in the process (devices.backend.md
    /// §3, WP0.1).
    ///
    /// Folds the two axes so no caller has to remember they exist: a
    /// superuser clears every bar regardless of `user_role`, and otherwise
    /// the ladder decides. `is_admin()` is exactly `has_role(Role::Admin)`
    /// and is kept as the spelling the admin-only surface already reads
    /// with.
    pub fn has_role(&self, min: Role) -> bool {
        self.is_superuser || self.user_role.at_least(min)
    }
}

/// Fixed reasons behind [`AuthError::SelfGuard`] — an actor tried a
/// role-management action on their own account, or on the last remaining
/// superuser. A typed enum (rather than a free-text message) so
/// [`AuthError::slug`] can match on the reason, not on `Display` text (M9:
/// the auth pages translate these, they don't render the English message).
#[derive(Debug, Clone, Copy, Error)]
pub enum SelfGuardReason {
    #[error("cannot disable your own account")]
    DisableSelf,
    #[error("cannot disable the last superuser")]
    DisableLastSuperuser,
    #[error("cannot delete your own account")]
    DeleteSelf,
    #[error("cannot delete the last superuser")]
    DeleteLastSuperuser,
    #[error("only a superuser can delete a superuser")]
    DeleteSuperuser,
    #[error("only a superuser can delete an admin")]
    DeleteAdmin,
    #[error("use change-password for your own account")]
    UseChangePasswordForSelf,
    #[error("only a superuser can change a superuser's name")]
    NameChangeSuperuser,
    #[error("cannot change your own role")]
    RoleChangeSelf,
    #[error("a superuser's role cannot be changed")]
    RoleChangeSuperuser,
    #[error("only a superuser can change an admin's role")]
    RoleChangeAdmin,
    /// LE-6 (local E2E 2026-09-26): an admin could reset a superuser's password and log in as
    /// them, or disable another admin, which `delete_user` already forbids.
    #[error("only a superuser can change a superuser or an admin")]
    ManageSuperuserOrAdmin,
}

impl SelfGuardReason {
    /// `auth.errors.<slug>` catalogue key suffix.
    fn slug(self) -> &'static str {
        match self {
            Self::DisableSelf => "self_guard_disable_self",
            Self::DisableLastSuperuser => "self_guard_disable_last_superuser",
            Self::DeleteSelf => "self_guard_delete_self",
            Self::DeleteLastSuperuser => "self_guard_delete_last_superuser",
            Self::DeleteSuperuser => "self_guard_delete_superuser",
            Self::DeleteAdmin => "self_guard_delete_admin",
            Self::UseChangePasswordForSelf => "self_guard_use_change_password",
            Self::NameChangeSuperuser => "self_guard_name_change_superuser",
            Self::RoleChangeSelf => "self_guard_role_change_self",
            Self::RoleChangeSuperuser => "self_guard_role_change_superuser",
            Self::RoleChangeAdmin => "self_guard_role_change_admin",
            Self::ManageSuperuserOrAdmin => "self_guard_manage_privileged",
        }
    }
}

/// Fixed reasons behind [`AuthError::Validation`] — a lookup/format/policy
/// failure distinct from the credential check itself. Same typed-over-
/// stringly-matched rationale as [`SelfGuardReason`].
#[derive(Debug, Error)]
pub enum ValidationReason {
    #[error("user not found")]
    UserNotFound,
    #[error("username must be 3..=50 characters")]
    UsernameLength,
    /// LE-14 (local E2E 2026-09-26): a padded or control-character username saved, and then
    /// could not log in.
    #[error("username may not contain spaces or control characters")]
    UsernameChars,
    #[error("password must be 8..=128 characters")]
    PasswordLength,
    #[error("full name must be at most 100 characters, without control characters")]
    FullNameInvalid,
    #[error("unknown role")]
    InvalidRole,
    #[error("password hashing failed: {0}")]
    HashingFailed(String),
}

impl ValidationReason {
    /// `auth.errors.<slug>` catalogue key suffix. `HashingFailed` carries no
    /// slug of its own — it's a library/internal failure, not something the
    /// operator caused, so it shares `internal_error` with the generic
    /// storage-failure bucket.
    fn slug(&self) -> &'static str {
        match self {
            Self::UserNotFound => "not_found",
            Self::UsernameLength => "username_length",
            Self::UsernameChars => "username_chars",
            Self::PasswordLength => "password_length",
            Self::FullNameInvalid => "full_name_invalid",
            Self::InvalidRole => "invalid_role",
            Self::HashingFailed(_) => "internal_error",
        }
    }
}

/// Errors surfaced by [`crate::auth::AuthService`].
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid username or password")]
    InvalidCredentials,
    #[error("username already taken")]
    UsernameTaken,
    #[error("{0}")]
    SelfGuard(SelfGuardReason),
    #[error("setup already completed")]
    SetupAlreadyDone,
    /// `retry_after` is what is left of the lockout, for the `Retry-After` header.
    #[error("too many attempts, try again in {}s", retry_after.as_secs())]
    TooManyAttempts { retry_after: std::time::Duration },
    #[error("{0}")]
    Validation(ValidationReason),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl AuthError {
    /// `auth.errors.<slug>` catalogue key (M9: auth route handlers pass
    /// this into the page context instead of `to_string()`'s English
    /// message — see `routes/auth_routes.rs` and `routes/user_routes.rs`).
    pub fn slug(&self) -> &'static str {
        match self {
            Self::InvalidCredentials => "invalid_credentials",
            Self::UsernameTaken => "username_taken",
            Self::SelfGuard(reason) => reason.slug(),
            Self::SetupAlreadyDone => "setup_already_done",
            Self::TooManyAttempts { .. } => "too_many_attempts",
            Self::Validation(reason) => reason.slug(),
            // #124: a full /data refused the session row. Not "try again".
            Self::Storage(e) if e.is_disk_full() => "storage_full",
            Self::Storage(_) => "internal_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The auth pages render error slugs as a *dynamic* catalogue key
    /// (`("auth.errors." ~ error)|t` in `templates/pages/login.html` and
    /// friends), which the i18n parity test cannot see: it proves `cs` and
    /// `en` agree, not that a key exists at all. A slug with no entry would
    /// print `auth.errors.something` on screen, so the coverage is asserted
    /// here, where the exhaustive `match` in `slug()` sits.
    #[test]
    fn every_auth_error_slug_has_a_translation() {
        let from_enums = [
            AuthError::InvalidCredentials.slug(),
            AuthError::UsernameTaken.slug(),
            AuthError::SetupAlreadyDone.slug(),
            AuthError::TooManyAttempts {
                retry_after: std::time::Duration::ZERO,
            }
            .slug(),
            AuthError::SelfGuard(SelfGuardReason::ManageSuperuserOrAdmin).slug(),
            AuthError::SelfGuard(SelfGuardReason::DisableSelf).slug(),
            AuthError::SelfGuard(SelfGuardReason::DisableLastSuperuser).slug(),
            AuthError::SelfGuard(SelfGuardReason::DeleteSelf).slug(),
            AuthError::SelfGuard(SelfGuardReason::DeleteLastSuperuser).slug(),
            AuthError::SelfGuard(SelfGuardReason::DeleteSuperuser).slug(),
            AuthError::SelfGuard(SelfGuardReason::DeleteAdmin).slug(),
            AuthError::SelfGuard(SelfGuardReason::UseChangePasswordForSelf).slug(),
            AuthError::SelfGuard(SelfGuardReason::NameChangeSuperuser).slug(),
            AuthError::SelfGuard(SelfGuardReason::RoleChangeSelf).slug(),
            AuthError::SelfGuard(SelfGuardReason::RoleChangeSuperuser).slug(),
            AuthError::SelfGuard(SelfGuardReason::RoleChangeAdmin).slug(),
            AuthError::Validation(ValidationReason::UserNotFound).slug(),
            AuthError::Validation(ValidationReason::UsernameLength).slug(),
            AuthError::Validation(ValidationReason::UsernameChars).slug(),
            AuthError::Validation(ValidationReason::PasswordLength).slug(),
            AuthError::Validation(ValidationReason::FullNameInvalid).slug(),
            AuthError::Validation(ValidationReason::InvalidRole).slug(),
            // Covers `AuthError::Storage` too — both map to `internal_error`.
            AuthError::Validation(ValidationReason::HashingFailed(String::new())).slug(),
            disk_full().slug(),
        ];
        // Slugs the route handlers pass without an `AuthError` behind them
        // (form-level checks, not service errors).
        let from_handlers = ["password_mismatch", "invalid_role"];

        for slug in from_enums.iter().chain(from_handlers.iter()) {
            let key = format!("auth.errors.{slug}");
            assert_ne!(
                crate::i18n::lookup("en", &key),
                key,
                "no i18n entry for {key} — add it to i18n/en/auth.json and i18n/cs/auth.json"
            );
        }
    }

    fn disk_full() -> AuthError {
        AuthError::Storage(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            None,
        )))
    }

    #[test]
    fn a_full_disk_is_named_on_the_login_page() {
        assert_eq!(disk_full().slug(), "storage_full");
        let busy = AuthError::Storage(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        )));
        assert_eq!(busy.slug(), "internal_error");
    }

    fn user(role: Role, is_superuser: bool) -> CurrentUser {
        CurrentUser {
            id: 1,
            username: "u".to_string(),
            full_name: None,
            user_role: role,
            is_superuser,
            monitoring_view_mode: crate::monitoring::DEFAULT_VIEW_MODE.to_string(),
        }
    }

    /// The ladder, both axes. Inherited from `perms::is_manager_or_admin`'s
    /// test when WP0.1 collapsed four copies of this predicate onto
    /// [`CurrentUser::has_role`] — the case that matters is the third block:
    /// a **superuser Viewer clears every bar**, which is exactly what a
    /// `PartialOrd` derive on `Role` would have silently dropped.
    #[test]
    fn has_role_folds_the_ladder_and_the_superuser_flag() {
        for min in [Role::Viewer, Role::Manager, Role::Admin] {
            assert!(user(Role::Admin, false).has_role(min), "admin >= {min:?}");
        }

        assert!(user(Role::Manager, false).has_role(Role::Viewer));
        assert!(user(Role::Manager, false).has_role(Role::Manager));
        assert!(!user(Role::Manager, false).has_role(Role::Admin));

        assert!(user(Role::Viewer, false).has_role(Role::Viewer));
        assert!(!user(Role::Viewer, false).has_role(Role::Manager));
        assert!(!user(Role::Viewer, false).has_role(Role::Admin));

        for min in [Role::Viewer, Role::Manager, Role::Admin] {
            assert!(
                user(Role::Viewer, true).has_role(min),
                "superuser viewer >= {min:?}"
            );
        }

        // `is_admin()` is the admin rung under its established name.
        assert_eq!(
            user(Role::Viewer, true).is_admin(),
            user(Role::Viewer, true).has_role(Role::Admin)
        );
    }

    /// `templates/layout/nav.html` renders the role badge as
    /// `("common.role." ~ role_label)|t`, so every `Role` value needs an
    /// entry. Same dynamic-key blind spot as the slugs above.
    #[test]
    fn every_role_has_a_translation() {
        for role in [Role::Viewer, Role::Manager, Role::Admin] {
            let key = format!("common.role.{}", role.as_str());
            assert_ne!(
                crate::i18n::lookup("en", &key),
                key,
                "no i18n entry for {key} — add it to i18n/en/common.json and i18n/cs/common.json"
            );
        }
    }
}
