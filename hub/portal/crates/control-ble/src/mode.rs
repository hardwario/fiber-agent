//! The `ble.mode` dispatch gate — `plan/control/ble/plan.md` §1. Checked **before anything else**:
//! before argument validation, before any file is opened, before the adapter is touched.
//!
//! Unlike `radio.mode` this is a **policy** gate, not a capability one — every CM4 has a controller.
//! So `dual` is the normal case rather than a deferred corner, and the client-Hub validity rule
//! (decision #5: `peripheral` or `dual` only) is the provisioning tool's to enforce. This component
//! trusts the rendered config and pins the table in `valid_for_client_hub` so it stays testable.

use crate::error::{Component, Error, Kind, Result, Role};
use std::fmt;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    PartialOrd,
    Ord,
)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Off,
    Central,
    Peripheral,
    Dual,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Off, Mode::Central, Mode::Peripheral, Mode::Dual];

    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Central => "central",
            Mode::Peripheral => "peripheral",
            Mode::Dual => "dual",
        }
    }

    /// Does this mode enable `role` at all?
    pub const fn allows(self, role: Role) -> bool {
        match (self, role) {
            (Mode::Off, _) => false,
            (Mode::Dual, _) => true,
            (Mode::Central, Role::Central) => true,
            (Mode::Central, Role::Peripheral) => false,
            (Mode::Peripheral, Role::Peripheral) => true,
            (Mode::Peripheral, Role::Central) => false,
        }
    }

    /// The roles this mode enables, in dispatch order — what `create` under `dual` iterates.
    pub const fn roles(self) -> &'static [Role] {
        match self {
            Mode::Off => &[],
            Mode::Central => &[Role::Central],
            Mode::Peripheral => &[Role::Peripheral],
            Mode::Dual => &[Role::Central, Role::Peripheral],
        }
    }

    /// `plan.md` §1's validity table, client column. A headless client Hub (decision #5) has BLE as
    /// its **only** local surface, so `off` and `central` leave it with no operator surface at all.
    /// Enforced by the provisioning tool, pinned here so a future reader finds the fact testable.
    pub const fn valid_for_client_hub(self) -> bool {
        matches!(self, Mode::Peripheral | Mode::Dual)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How many roles a command can run as. `Both` is a two-role command that takes `--role`; `Fixed` is
/// a `beacon-*`/`phone-*` command whose role is in its name and which therefore has no flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleArity {
    Both,
    Fixed(Role),
}

/// Pick the role this invocation runs as, or refuse. The **only** entry point; `dispatch` calls it
/// before it looks at anything else.
pub fn resolve(mode: Mode, requested: Option<Role>, arity: RoleArity) -> Result<Role> {
    match arity {
        // The role is in the command name: an explicit request cannot disagree with it, so the
        // asked-for role is the target and the refusal names it.
        RoleArity::Fixed(r) => gate(mode, r, Some(r)),
        RoleArity::Both => match (requested, mode) {
            (Some(r), _) => gate(mode, r, Some(r)),
            (None, Mode::Central) => gate(mode, Role::Central, None),
            (None, Mode::Peripheral) => gate(mode, Role::Peripheral, None),
            (None, Mode::Dual) => Err(Error::new(
                Kind::RoleParameterRequired,
                None,
                Component::BleMode,
                "--role is required while ble.mode is dual: pass --role central or --role peripheral",
            )),
            // Off refuses whatever is asked; the target is a placeholder the gate never returns.
            (None, Mode::Off) => gate(mode, Role::Central, None),
        },
    }
}

/// Which roles this invocation acts on.
///
/// `create` and `status` **span** their scope: under `dual` with no `--role` they act on both roles
/// rather than demanding a flag. That is not a softening of [`resolve`] — it is what the design
/// requires, from two independent directions:
///
/// - §5's golden `status --json` shows `"ble_mode": "dual"` with **both** the `central` and
///   `peripheral` blocks present in one document, which is only producible without a role flag;
/// - `ble-setup.service` runs `ExecStart=/usr/bin/proximosctl control.ble create` with **no
///   `--role`**, and `dual` is the shipped default — so requiring the flag would fail the boot unit
///   on every shipped Hub.
///
/// [`resolve`] keeps its pinned single-role matrix and is what every `beacon-*`/`phone-*` command
/// goes through; `RoleArity::Both` with an explicit `--role` still narrows to one role here.
///
/// > **Noted for the design set:** the implementation plan's Verification bullet and its A4 matrix
/// > disagree on this point. The two sources above decide it, and `RoleParameterRequired` (exit 38)
/// > consequently has no phase-1 caller — it stays in the table, and in [`resolve`], for a future
/// > two-role command that genuinely cannot span.
pub fn resolve_scope(mode: Mode, requested: Option<Role>, arity: RoleArity) -> Result<Vec<Role>> {
    match arity {
        RoleArity::Fixed(r) => Ok(vec![gate(mode, r, Some(r))?]),
        // An explicit role narrows a spanning command to exactly that role.
        RoleArity::Both => match requested {
            Some(r) => Ok(vec![gate(mode, r, Some(r))?]),
            None => {
                let roles = mode.roles();
                if roles.is_empty() {
                    // `off`: refuse, and do not invent a role in the refusal.
                    return Err(Error::new(
                        Kind::OffMode,
                        None,
                        Component::BleMode,
                        "ble.mode is off: no BLE role is enabled on this Hub",
                    ));
                }
                Ok(roles.to_vec())
            }
        },
    }
}

/// `target` is the role the command would run as; `asked` is what the operator actually named, and
/// it is what a refusal reports — an `OffMode` on a bare `status` must not invent a role.
fn gate(mode: Mode, target: Role, asked: Option<Role>) -> Result<Role> {
    if mode.allows(target) {
        return Ok(target);
    }
    if mode == Mode::Off {
        return Err(Error::new(
            Kind::OffMode,
            asked,
            Component::BleMode,
            "ble.mode is off: no BLE role is enabled on this Hub",
        ));
    }
    let kind = match target {
        Role::Central => Kind::CentralUnavailable,
        Role::Peripheral => Kind::PeripheralUnavailable,
    };
    Err(Error::new(
        kind,
        target,
        Component::BleMode,
        format!("ble.mode is {mode}: the {target} role is not enabled on this Hub"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_matrix_matches_the_plan() {
        // off refuses everything, and reports back exactly the role that was asked for.
        for arity in [
            RoleArity::Both,
            RoleArity::Fixed(Role::Central),
            RoleArity::Fixed(Role::Peripheral),
        ] {
            for asked in [None, Some(Role::Central), Some(Role::Peripheral)] {
                let err = resolve(Mode::Off, asked, arity).unwrap_err();
                assert_eq!(err.kind(), Kind::OffMode, "{arity:?} {asked:?}");
                assert_eq!(err.component(), Component::BleMode);
                let expected_role = match arity {
                    RoleArity::Fixed(r) => Some(r),
                    RoleArity::Both => asked,
                };
                assert_eq!(err.role(), expected_role, "{arity:?} {asked:?}");
            }
        }

        assert_eq!(
            resolve(Mode::Central, None, RoleArity::Both).unwrap(),
            Role::Central
        );

        let err = resolve(Mode::Central, Some(Role::Peripheral), RoleArity::Both).unwrap_err();
        assert_eq!(err.kind(), Kind::PeripheralUnavailable);
        assert_eq!(err.role(), Some(Role::Peripheral));

        let err = resolve(Mode::Central, None, RoleArity::Fixed(Role::Peripheral)).unwrap_err();
        assert_eq!(err.kind(), Kind::PeripheralUnavailable);

        let err = resolve(Mode::Peripheral, None, RoleArity::Fixed(Role::Central)).unwrap_err();
        assert_eq!(err.kind(), Kind::CentralUnavailable);

        let err = resolve(Mode::Dual, None, RoleArity::Both).unwrap_err();
        assert_eq!(err.kind(), Kind::RoleParameterRequired);
        assert_eq!(err.role(), None, "no role was asked for");
        assert_eq!(err.component(), Component::BleMode);

        assert_eq!(
            resolve(Mode::Dual, Some(Role::Central), RoleArity::Both).unwrap(),
            Role::Central
        );
        assert_eq!(
            resolve(Mode::Dual, None, RoleArity::Fixed(Role::Peripheral)).unwrap(),
            Role::Peripheral
        );
        assert_eq!(
            resolve(Mode::Peripheral, None, RoleArity::Both).unwrap(),
            Role::Peripheral
        );
    }

    #[test]
    fn every_mode_gate_refusal_reports_the_ble_mode_component() {
        let mut refusals = 0;
        for mode in Mode::ALL {
            for arity in [
                RoleArity::Both,
                RoleArity::Fixed(Role::Central),
                RoleArity::Fixed(Role::Peripheral),
            ] {
                for asked in [None, Some(Role::Central), Some(Role::Peripheral)] {
                    if let Err(e) = resolve(mode, asked, arity) {
                        assert_eq!(
                            e.component(),
                            Component::BleMode,
                            "{mode} {arity:?} {asked:?}"
                        );
                        assert!(
                            matches!(
                                e.kind(),
                                Kind::OffMode
                                    | Kind::CentralUnavailable
                                    | Kind::PeripheralUnavailable
                                    | Kind::RoleParameterRequired
                            ),
                            "{mode} {arity:?} {asked:?} produced {}",
                            e.kind().as_str()
                        );
                        refusals += 1;
                    }
                }
            }
        }
        assert!(refusals > 0, "the matrix must exercise refusals");
    }

    #[test]
    fn role_validity_table() {
        // Server role: every mode is valid — `off` is a legitimate choice for a Hub with no BLE duty.
        for mode in Mode::ALL {
            assert!(matches!(
                mode,
                Mode::Off | Mode::Central | Mode::Peripheral | Mode::Dual
            ));
        }
        // Client role (headless, decision #5): only the two modes that keep a local surface.
        assert!(!Mode::Off.valid_for_client_hub());
        assert!(!Mode::Central.valid_for_client_hub());
        assert!(Mode::Peripheral.valid_for_client_hub());
        assert!(Mode::Dual.valid_for_client_hub());
    }

    #[test]
    fn allows_and_roles_agree() {
        for mode in Mode::ALL {
            for role in [Role::Central, Role::Peripheral] {
                assert_eq!(
                    mode.allows(role),
                    mode.roles().contains(&role),
                    "{mode} / {role}"
                );
            }
        }
        assert_eq!(Mode::Dual.roles(), &[Role::Central, Role::Peripheral]);
        assert!(Mode::Off.roles().is_empty());
    }

    #[test]
    fn a_spanning_command_covers_every_role_the_mode_enables() {
        // §5's golden `status --json` carries both blocks under `dual`, and `ble-setup.service`
        // runs `create` with no `--role`. Both require this.
        assert_eq!(
            resolve_scope(Mode::Dual, None, RoleArity::Both).expect("spans"),
            vec![Role::Central, Role::Peripheral]
        );
        assert_eq!(
            resolve_scope(Mode::Central, None, RoleArity::Both).expect("spans"),
            vec![Role::Central]
        );
        assert_eq!(
            resolve_scope(Mode::Peripheral, None, RoleArity::Both).expect("spans"),
            vec![Role::Peripheral]
        );
    }

    #[test]
    fn an_explicit_role_narrows_a_spanning_command_to_one() {
        assert_eq!(
            resolve_scope(Mode::Dual, Some(Role::Peripheral), RoleArity::Both).expect("narrows"),
            vec![Role::Peripheral]
        );
        // …and a role the mode excludes is still refused, with its own code.
        let err = resolve_scope(Mode::Central, Some(Role::Peripheral), RoleArity::Both)
            .expect_err("not enabled");
        assert_eq!(err.kind(), Kind::PeripheralUnavailable);
    }

    #[test]
    fn a_spanning_command_on_an_off_hub_is_off_mode_and_invents_no_role() {
        let err = resolve_scope(Mode::Off, None, RoleArity::Both).expect_err("refused");
        assert_eq!(err.kind(), Kind::OffMode);
        assert_eq!(err.component(), Component::BleMode);
        assert_eq!(err.role(), None, "a bare `status` must not invent a role");
    }

    #[test]
    fn a_fixed_command_still_resolves_to_exactly_its_own_role() {
        assert_eq!(
            resolve_scope(Mode::Dual, None, RoleArity::Fixed(Role::Central)).expect("fixed"),
            vec![Role::Central]
        );
        assert_eq!(
            resolve_scope(Mode::Central, None, RoleArity::Fixed(Role::Peripheral))
                .expect_err("not enabled")
                .kind(),
            Kind::PeripheralUnavailable
        );
    }

    #[test]
    fn resolve_keeps_its_pinned_single_role_matrix_even_though_no_command_uses_both_now() {
        // `RoleParameterRequired` has no phase-1 caller, but the gate it comes from is still the
        // one every Fixed command goes through — so the row stays live and tested here rather than
        // being quietly deleted.
        let err = resolve(Mode::Dual, None, RoleArity::Both).expect_err("still refuses");
        assert_eq!(err.kind(), Kind::RoleParameterRequired);
        assert_eq!(err.exit_code(), 38);
    }

    #[test]
    fn mode_is_lowercase_on_the_wire_and_defaults_to_off() {
        assert_eq!(Mode::default(), Mode::Off);
        for mode in Mode::ALL {
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::Value::String(mode.as_str().to_string())
            );
        }
        assert!(serde_yaml::from_str::<Mode>("invented").is_err());
    }
}
