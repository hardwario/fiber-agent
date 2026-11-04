//! Certificate permission sets per role (M4-D) — mirrors v1's
//! `role_defaults` spirit (plan/v1/viewer/rest.md "Auth / PKI",
//! `GET /api/auth/permissions` -> `{permissions, role_defaults}`).
//!
//! Certs are minted with exactly this set via `KeyService::ensure_user_key`
//! (called from `routes::hub_routes`) — a
//! user's role decides what permission strings ride on their signing
//! certificate, and the *device* is what actually enforces them on every
//! signed command (the portal's own preflight in `device_routes.rs` is a
//! same-portal, ahead-of-publish check, see `fiber_protocol::registry`'s
//! doc comment).

use std::collections::BTreeSet;

use fiber_protocol::Level;

use crate::auth::{CurrentUser, Role};

/// The highest [`Level`] a role may reach (plan/portal/mqtt/design.md
/// §4.3): Viewer stops at `read`, Manager at `manage`, Admin and superuser
/// reach `destructive`.
///
/// This is the **live** ceiling, read from the user's current role, and it
/// is not the same thing as what their certificate happens to carry:
/// `KeyService::ensure_user_key` re-mints with the *union* of stored and
/// requested permissions and never removes one, so a user demoted from
/// Admin to Viewer keeps `level:destructive` on their stored certificate.
/// That is why `relay::execute` checks this before it checks the
/// certificate (design.md §4.4 steps 2 and 3): the union is what lets a new
/// permission appear without a re-pair, and this is what stops it becoming
/// a permanent one.
pub fn level_ceiling(user: &CurrentUser) -> Level {
    if user.is_admin() {
        Level::Destructive
    } else if user.user_role == Role::Manager {
        Level::Manage
    } else {
        Level::Read
    }
}

/// The `level:*` pseudo-permissions a role's certificate carries — every
/// rung up to and including its ceiling (design.md §4.3).
///
/// They are ordinary strings in the certificate's existing `permissions`
/// list with exact string match, so the certificate **schema** does not
/// change. Step 8 of the device's verification requires the verb's own
/// permission *and* `level:<verb level>`, which is what makes the level
/// enforceable: a manager's certificate carries
/// `set_lorawan_sensor_config`, which is also `sticker_device_reset`'s
/// permission, and only the absent `level:destructive` stops that key from
/// wiping a node.
fn level_permissions(ceiling: Level) -> impl Iterator<Item = String> {
    Level::ALL
        .into_iter()
        .filter(move |level| *level <= ceiling)
        .map(Level::permission)
}

/// Fixed viewer permission set — v1 `role_defaults["viewer"]` spirit
/// (read-only device queries). Not derived from the registry: `get_status`
/// and `get_info` have device arms but no portal publisher, so they carry
/// no registry row. `get_sensor_config` and `get_interval` now do have
/// rows, requiring exactly these strings (design.md §4.2's ★) — which is
/// why this set was already the right one for a viewer to hold.
const VIEWER_PERMISSIONS: &[&str] = &[
    "get_status",
    "get_info",
    "get_sensor_config",
    "get_interval",
];

/// Every distinct permission string the registry's full command catalogue
/// can require — the admin/superuser certificate permission set.
///
/// Derived by walking `fiber_protocol::registry::all()`. This module used
/// to keep its own verbatim copy of the 52-row verb list to walk instead;
/// the registry exposes the catalogue itself now, so the copy — and the
/// drift it invited — is gone (design.md §10).
fn all_registry_permissions() -> BTreeSet<String> {
    fiber_protocol::registry::all()
        .iter()
        .map(|spec| spec.permission.to_string())
        .collect()
}

/// The certificate permission set to mint for `user` on a device — see
/// module doc. Superuser widens to the admin set regardless of `user_role`
/// (`CurrentUser::is_admin`'s own contract).
pub fn permissions_for(user: &CurrentUser) -> Vec<String> {
    let ceiling = level_ceiling(user);
    let mut set: BTreeSet<String> = if ceiling == Level::Read {
        VIEWER_PERMISSIONS.iter().map(|s| s.to_string()).collect()
    } else {
        // Manager and above get every verb permission the registry can
        // require. `MANAGER_EXCLUDED` used to withhold `power_off_device`
        // and `factory_reset` from a manager here; it is gone, because
        // `level:destructive` being absent from a manager's certificate
        // does the same job for every destructive verb rather than for two
        // hand-picked ones (design.md §4.3).
        all_registry_permissions()
    };
    set.extend(level_permissions(ceiling));
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(role: Role, is_superuser: bool) -> CurrentUser {
        CurrentUser {
            id: 1,
            username: "u".to_string(),
            full_name: None,
            user_role: role,
            is_superuser,
            monitoring_view_mode: "table-simplified".into(),
        }
    }

    /// Every verb this module derives permissions from resolves through
    /// `lookup` too — i.e. `all()` and `lookup` agree, which is what lets
    /// the relay gate on `lookup` and the certificate be minted from
    /// `all()`.
    #[test]
    fn every_registry_row_resolves_through_lookup() {
        for spec in fiber_protocol::registry::all() {
            assert_eq!(
                fiber_protocol::lookup(spec.command_type),
                Some(spec),
                "{} is in all() but not reachable through lookup",
                spec.command_type
            );
        }
    }

    #[test]
    fn admin_gets_every_distinct_registry_permission() {
        let admin = permissions_for(&user(Role::Admin, false));
        let set: BTreeSet<&str> = admin.iter().map(String::as_str).collect();

        assert_eq!(
            set.len(),
            29 + 5,
            "29 distinct permission strings in the registry (setup.manage for #58, \
             export.manage for #59, radio.manage for R7 and cluster.manage for \
             cluster.md §7 joined), plus the five level:* strings"
        );
        for expected in [
            // design.md §4.2's three ★ strings — verbs that had no
            // permission at all in v1 — plus the v2 Beacon permission.
            "get_sensor_config",
            "get_interval",
            "silence_buzzer",
            "beacon.manage",
            // The v2 `node-add`/`node-remove` permission (control.radio.md §7).
            "node.manage",
            "restart_application",
            "power_off_device",
            "factory_reset",
            "set_display_lines",
            "set_network_config",
            "set_lorawan_sensor_config",
            "set_threshold",
            "set_sensor_name",
            "set_sensor_location",
            "set_interval",
            "set_alarm_pattern",
            "add_signer",
            "remove_signer",
            "update_signer",
            "set_screen_brightness",
            "set_led_brightness",
            "set_device_label",
            "set_buzzer_volume",
            "set_system_info_interval",
            "get_status",
            // The two the count above already claims (#58's FB16/FB17 and
            // #59's FB18): named here too, so a registry row that loses its
            // permission fails on the name rather than on an arithmetic
            // coincidence with whatever else changed.
            "setup.manage",
            "export.manage",
            // `radio-set-mode` (node_registry.md §11, R7).
            "radio.manage",
            // `cluster-client-add` / `-remove` (cluster.md §7).
            "cluster.manage",
        ] {
            assert!(set.contains(expected), "admin missing {expected}");
        }
    }

    #[test]
    fn superuser_gets_admin_set_regardless_of_role() {
        let superuser_viewer = permissions_for(&user(Role::Viewer, true));
        let admin = permissions_for(&user(Role::Admin, false));
        let a: BTreeSet<&str> = superuser_viewer.iter().map(String::as_str).collect();
        let b: BTreeSet<&str> = admin.iter().map(String::as_str).collect();
        assert_eq!(a, b);
        assert_eq!(
            level_ceiling(&user(Role::Viewer, true)),
            Level::Destructive,
            "the superuser flag sits orthogonal to the role ladder"
        );
    }

    /// design.md §4.3's table, which is the whole role floor: Viewer stops
    /// at `read`, Manager at `manage`, Admin reaches `destructive`.
    #[test]
    fn each_role_mints_the_level_strings_up_to_its_ceiling() {
        for (role, superuser, expected) in [
            (Role::Viewer, false, vec!["level:read"]),
            (
                Role::Manager,
                false,
                vec!["level:manage", "level:operate", "level:read"],
            ),
            (
                Role::Admin,
                false,
                vec![
                    "level:admin",
                    "level:destructive",
                    "level:manage",
                    "level:operate",
                    "level:read",
                ],
            ),
        ] {
            let minted = permissions_for(&user(role, superuser));
            let levels: Vec<&str> = minted
                .iter()
                .map(String::as_str)
                .filter(|p| p.starts_with("level:"))
                .collect();
            // Alphabetical, not ladder order: `permissions_for` returns a
            // sorted set, and the certificate's list is a set too.
            assert_eq!(levels, expected, "{role:?}");
        }
    }

    /// `MANAGER_EXCLUDED` is gone: a manager's certificate carries
    /// `factory_reset` and `power_off_device` again, and it is the absent
    /// `level:destructive` that stops them being usable — for every
    /// destructive verb, not just the two that used to be named.
    #[test]
    fn a_manager_is_stopped_by_the_missing_level_not_by_a_withheld_permission() {
        let manager = permissions_for(&user(Role::Manager, false));
        let set: BTreeSet<&str> = manager.iter().map(String::as_str).collect();

        assert!(set.contains("factory_reset"));
        assert!(set.contains("power_off_device"));
        assert!(!set.contains("level:destructive"));
        assert!(!set.contains("level:admin"));
        assert!(set.contains("level:manage"));

        // The permission a destructive Node verb shares with a manage one —
        // the case a per-verb exclusion list could never have caught.
        assert!(set.contains("set_lorawan_sensor_config"));
        assert_eq!(
            fiber_protocol::lookup("sticker_device_reset")
                .unwrap()
                .permission,
            "set_lorawan_sensor_config"
        );
    }

    #[test]
    fn viewer_gets_the_fixed_v1_role_defaults_set_plus_level_read() {
        let viewer = permissions_for(&user(Role::Viewer, false));
        let set: BTreeSet<&str> = viewer.iter().map(String::as_str).collect();
        assert_eq!(
            set,
            BTreeSet::from([
                "get_status",
                "get_info",
                "get_sensor_config",
                "get_interval",
                "level:read",
            ])
        );
    }
}
