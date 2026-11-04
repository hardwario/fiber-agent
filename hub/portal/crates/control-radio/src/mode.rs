//! The single dispatch gate — `plan/control/radio/plan.md` §1 and §5 "The `--radio`
//! parameter".
//!
//! Every Node/Gateway command routes through here, and `radio.mode` is checked **before
//! anything else** — before argument validation, before any RPC, before any file is opened.

use crate::cache::at_order_key;
use crate::error::{Component, Error, Kind, Result, Transport};

/// `radio.mode` — a factory choice, set once by the provisioning tool at flash time or by the
/// portal's first-run `/setup` (decision #37, amended 2026-09-26), never a runtime knob (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Both transports refused. The component is not in the service set at all.
    #[default]
    Off,
    Lorawan,
    P2p,
    /// Both proceed. **Phase 1 does not build or test this combination** — it is a real enum
    /// value and a real branch in every `--radio` rule so that enabling it later is a config
    /// change rather than a command-surface change.
    Dual,
}

impl Mode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Lorawan => "lorawan",
            Mode::P2p => "p2p",
            Mode::Dual => "dual",
        }
    }

    pub const fn allows(self, t: Transport) -> bool {
        matches!(
            (self, t),
            (Mode::Lorawan, Transport::Lorawan) | (Mode::P2p, Transport::P2p) | (Mode::Dual, _)
        )
    }

    /// True when the mode names exactly one transport, which is what makes `--radio`
    /// optional (§5).
    pub const fn is_unambiguous(self) -> bool {
        matches!(self, Mode::Lorawan | Mode::P2p)
    }

    /// Every mode, for the matrix tests.
    pub const ALL: [Mode; 4] = [Mode::Off, Mode::Lorawan, Mode::P2p, Mode::Dual];
}

/// Which radio hardware is populated — §1's hardware-validity table.
///
/// The STM32WL5MOC northbridge is populated on **every** Hub, so P2P capability always
/// physically exists; the RAK5146 LoRaWAN concentrator is an optional add-on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hardware {
    /// STM32WL5MOC only (no RAK5146).
    NorthbridgeOnly,
    /// STM32WL5MOC + RAK5146.
    NorthbridgeAndConcentrator,
}

impl Hardware {
    /// The valid `radio.mode` values for this hardware.
    ///
    /// **Enforcing this is the provisioning tool's job, not `control_radio`'s** (§1): this
    /// component trusts whatever `radio.mode` the rendered config says. A mismatch surfaces
    /// the way any other hardware fault would — `ServiceDown` or a gateway-discovery timeout
    /// — not as a dedicated error. The table lives here so the unit test can cross-check the
    /// dispatch matrix against it, which is exactly what §Testing asks for.
    pub fn valid_modes(self) -> &'static [Mode] {
        match self {
            Hardware::NorthbridgeOnly => &[Mode::Off, Mode::P2p],
            Hardware::NorthbridgeAndConcentrator => {
                &[Mode::Off, Mode::P2p, Mode::Lorawan, Mode::Dual]
            }
        }
    }
}

/// Whether a command accepts `--radio`, and which values it implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioArity {
    /// A unified command: takes `--radio`, both arms implemented.
    Unified,
    /// Takes `--radio` like a unified command (future-radio-ready), but only one arm is
    /// implemented today; the other returns `InvalidArgument` (§5, `node-scan-*` and
    /// `gateway-scan`).
    UnifiedOneArmed(Transport),
    /// Hardware every Hub has whatever `radio.mode` says — the Northbridge's firmware
    /// (`northbridge-update`, #158): `--radio` may only name that transport, and the mode
    /// gate does not apply.
    ModeIndependent(Transport),
}

/// Resolve the dispatch arm for one command invocation.
///
/// This is §5's `--radio` rule in one place:
///
/// - `radio.mode ∈ {lorawan, p2p}` → `--radio` optional; omitting it uses the active mode.
/// - `radio.mode == dual` → `--radio` required; omitting returns `RadioParameterRequired`.
/// - `radio.mode == off` → every command refused regardless of `--radio`.
pub fn resolve(mode: Mode, requested: Option<Transport>, arity: RadioArity) -> Result<Transport> {
    if let RadioArity::ModeIndependent(only) = arity {
        return match requested {
            Some(t) if t != only => Err(Error::new(
                Kind::InvalidArgument,
                t,
                Component::ControlSocket,
                format!(
                    "no capability for this radio: --radio {t} is not implemented for this command"
                ),
            )),
            _ => Ok(only),
        };
    }
    let target =
        match (requested, mode) {
            (Some(t), _) => t,
            // Unambiguous mode: default to the one transport it names.
            (None, Mode::Lorawan) => Transport::Lorawan,
            (None, Mode::P2p) => Transport::P2p,
            (None, Mode::Dual) => return Err(Error::new(
                Kind::RadioParameterRequired,
                None,
                Component::RadioMode,
                "--radio is required while radio.mode is dual: pass --radio lorawan or --radio p2p",
            )),
            // `off` with no `--radio`: there is no active mode to default to, and nothing was
            // asked for. `gate` refuses it as `OffMode` with no transport at all — the target
            // here is a placeholder that never reaches the error.
            (None, Mode::Off) => Transport::Lorawan,
        };

    gate(mode, target, requested)?;

    // The mode permits it; now does this command implement that arm?
    if let RadioArity::UnifiedOneArmed(implemented) = arity {
        if target != implemented {
            return Err(Error::new(
                Kind::InvalidArgument,
                target,
                Component::ControlSocket,
                format!("no capability for this radio: --radio {target} is not implemented for this command"),
            ));
        }
    }

    Ok(target)
}

/// The mode check proper: does `radio.mode` permit this transport at all?
///
/// `asked` is what the caller actually named — the `--radio` value, or the implied transport of
/// a transport-specific command — and is `None` when the flag was omitted on an unambiguous or
/// `off` Hub. It is what lands in the error's `transport` field, so a refusal never invents a
/// transport the caller did not mention.
///
/// Under `off` the refusal is `OffMode` (§6, decided 2026-08-21), not one of the `*Unavailable`
/// pair: those two are defined as "the *other* arm is the active one", which is false when no
/// arm is enabled, and the two states have different remediations. All four refusals here
/// report `component: radio-mode` — they are decided from local config, before any RPC.
fn gate(mode: Mode, target: Transport, asked: Option<Transport>) -> Result<Transport> {
    if mode.allows(target) {
        return Ok(target);
    }
    if mode == Mode::Off {
        return Err(Error::new(
            Kind::OffMode,
            asked,
            Component::RadioMode,
            "radio.mode is off: no radio arm is enabled on this Hub",
        ));
    }
    let kind = match target {
        Transport::Lorawan => Kind::LorawanUnavailable,
        Transport::P2p => Kind::P2pUnavailable,
    };
    Err(Error::new(
        kind,
        target,
        Component::RadioMode,
        format!(
            "radio.mode is {}: the {target} arm is not enabled on this Hub",
            mode.as_str()
        ),
    ))
}

/// The carrier a Node registration goes out on when the caller names none: the Hub's
/// `radio.mode`, P2P on a `dual` Hub (Hynek 2026-09-27, the carrier decision #16 prefers), and
/// LoRaWAN on a radio-off Hub and where no mode is known (`None`, a caller with no rendered
/// config).
///
/// One rule for every registration surface that carries no carrier of its own: the Portal's
/// dialog preselects it (devices.node.md, dodatek 2026-09-27) and the BLE FB0D Node add uses it
/// (peripheral.md §4, amended 2026-10-10). Kept here rather than in either caller so the two
/// cannot drift apart.
pub const fn registration_default(mode: Option<Mode>) -> Transport {
    match mode {
        Some(Mode::P2p | Mode::Dual) => Transport::P2p,
        Some(Mode::Lorawan | Mode::Off) | None => Transport::Lorawan,
    }
}

/// What one carrier holds of a DevEUI, as a `dual` Hub's routing sees it — [`live_carrier`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Heard {
    /// The carrier's registry holds the DevEUI: the P2P registry in `sessions.db`, or a
    /// LoRaWAN enrollment record in `nodes.json`.
    pub registered: bool,
    /// When the carrier last heard it, if ever: its `node-get` cache's
    /// [`crate::cache::CacheDocument::last_heard`].
    pub last: Option<String>,
}

/// The carrier a `dual` Hub sends a DevEUI's commands over and replays its retained documents
/// from — p2p_integration.md §11.
///
/// - Held by one registry only: that carrier. Held by neither: LoRaWAN, where the command then
///   fails as not enrolled, as it always has.
/// - Held by both: the carrier it was **last heard on**. A Node can switch its firmware
///   `radio-mode` and stays registered on both while it migrates, so registration alone does
///   not say where it is (bench 2026-09-27: a refresh-config queued on P2P for a Node that
///   had moved to LoRaWAN, where it never collects it).
/// - Held by both and heard on neither yet, or on both at the same instant: P2P, the
///   tie-break the rule had before it looked at recency.
///
/// Under `p2p` or `lorawan` this is not asked: the mode decides.
pub fn live_carrier(p2p: &Heard, lorawan: &Heard) -> Transport {
    match (p2p.registered, lorawan.registered) {
        (true, false) => Transport::P2p,
        (false, _) => Transport::Lorawan,
        (true, true) => match (p2p.last.as_deref(), lorawan.last.as_deref()) {
            (Some(p), Some(l)) if at_order_key(l) > at_order_key(p) => Transport::Lorawan,
            (None, Some(_)) => Transport::Lorawan,
            _ => Transport::P2p,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mode, and none: a single-carrier mode registers on its carrier, `dual` on P2P, and
    /// `off` or an unknown mode on LoRaWAN.
    #[test]
    fn a_registration_with_no_carrier_follows_the_mode_and_prefers_p2p_on_dual() {
        let cases = [
            (Some(Mode::Lorawan), Transport::Lorawan),
            (Some(Mode::P2p), Transport::P2p),
            (Some(Mode::Dual), Transport::P2p),
            (Some(Mode::Off), Transport::Lorawan),
            (None, Transport::Lorawan),
        ];
        for (mode, want) in cases {
            assert_eq!(registration_default(mode), want, "{mode:?}");
        }
        // Wherever a mode runs a carrier at all, the default is one it runs.
        for mode in [Mode::Lorawan, Mode::P2p, Mode::Dual] {
            assert!(mode.allows(registration_default(Some(mode))), "{mode:?}");
        }
    }

    /// §1's dispatch matrix, every cell.
    #[test]
    fn dispatch_matrix_matches_the_plan() {
        const LW: Transport = Transport::Lorawan;
        const PP: Transport = Transport::P2p;
        let cases: &[(Mode, Transport, Option<Kind>)] = &[
            (Mode::Lorawan, LW, None),
            (Mode::Lorawan, PP, Some(Kind::P2pUnavailable)),
            (Mode::P2p, LW, Some(Kind::LorawanUnavailable)),
            (Mode::P2p, PP, None),
            (Mode::Dual, LW, None),
            (Mode::Dual, PP, None),
            (Mode::Off, LW, Some(Kind::OffMode)),
            (Mode::Off, PP, Some(Kind::OffMode)),
        ];
        for &(mode, t, expected) in cases {
            let got = resolve(mode, Some(t), RadioArity::Unified);
            match expected {
                None => assert_eq!(got, Ok(t), "mode {mode:?} + --radio {t:?} must dispatch"),
                Some(k) => assert_eq!(
                    got.expect_err("must refuse").kind(),
                    k,
                    "mode {mode:?} + --radio {t:?}"
                ),
            }
        }
    }

    #[test]
    fn radio_flag_is_optional_only_when_the_mode_is_unambiguous() {
        assert_eq!(
            resolve(Mode::Lorawan, None, RadioArity::Unified),
            Ok(Transport::Lorawan)
        );
        assert_eq!(
            resolve(Mode::P2p, None, RadioArity::Unified),
            Ok(Transport::P2p)
        );
        assert_eq!(
            resolve(Mode::Dual, None, RadioArity::Unified)
                .expect_err("must refuse")
                .kind(),
            Kind::RadioParameterRequired
        );
    }

    /// `node-scan-start`/`gateway-scan` take `--radio` but implement one arm; the other is
    /// `InvalidArgument`, not an `*Unavailable` (§5).
    #[test]
    fn one_armed_commands_refuse_the_unimplemented_arm_as_invalid_argument() {
        let arity = RadioArity::UnifiedOneArmed(Transport::P2p);
        assert_eq!(
            resolve(Mode::Dual, Some(Transport::P2p), arity),
            Ok(Transport::P2p)
        );
        assert_eq!(
            resolve(Mode::Dual, Some(Transport::Lorawan), arity)
                .expect_err("must refuse")
                .kind(),
            Kind::InvalidArgument
        );
    }

    /// A mode refusal outranks a not-implemented refusal: the gate runs first (§1, "checks
    /// `radio.mode` before doing anything else").
    /// #158: the Northbridge is on every Hub, so its update ignores the mode.
    #[test]
    fn a_mode_independent_command_passes_every_mode() {
        let arity = RadioArity::ModeIndependent(Transport::P2p);
        for mode in Mode::ALL {
            assert_eq!(resolve(mode, None, arity).unwrap(), Transport::P2p);
            assert_eq!(
                resolve(mode, Some(Transport::P2p), arity).unwrap(),
                Transport::P2p
            );
            assert_eq!(
                resolve(mode, Some(Transport::Lorawan), arity)
                    .unwrap_err()
                    .kind(),
                Kind::InvalidArgument
            );
        }
    }

    #[test]
    fn the_mode_gate_runs_before_the_capability_check() {
        let arity = RadioArity::UnifiedOneArmed(Transport::Lorawan);
        let err = resolve(Mode::P2p, Some(Transport::Lorawan), arity).expect_err("must refuse");
        assert_eq!(err.kind(), Kind::LorawanUnavailable);
    }

    /// §1's hardware-validity table. `control_radio` does not enforce it — the provisioning
    /// tool does — but the table is asserted here so a change to either is visible.
    #[test]
    fn hardware_validity_table() {
        assert_eq!(
            Hardware::NorthbridgeOnly.valid_modes(),
            &[Mode::Off, Mode::P2p]
        );
        // Every Hub has the northbridge, so no hardware makes P2P invalid.
        for hw in [
            Hardware::NorthbridgeOnly,
            Hardware::NorthbridgeAndConcentrator,
        ] {
            assert!(hw.valid_modes().contains(&Mode::P2p));
            assert!(hw.valid_modes().contains(&Mode::Off));
        }
        // LoRaWAN and dual need the optional concentrator.
        assert!(!Hardware::NorthbridgeOnly
            .valid_modes()
            .contains(&Mode::Lorawan));
        assert!(!Hardware::NorthbridgeOnly
            .valid_modes()
            .contains(&Mode::Dual));
    }

    /// §6, decided 2026-08-21: `radio.mode: off` produces `OffMode`, never one of the
    /// `*Unavailable` pair.
    ///
    /// The old behaviour reported `LoRaWANUnavailable` for an omitted `--radio` purely because
    /// the target defaulted to LoRaWAN before the gate ran — which reads as "the p2p arm is
    /// active", the exact opposite of the truth.
    #[test]
    fn off_mode_refuses_as_off_mode_whatever_was_asked() {
        for requested in [None, Some(Transport::Lorawan), Some(Transport::P2p)] {
            let err = resolve(Mode::Off, requested, RadioArity::Unified)
                .expect_err("radio.mode: off refuses everything");
            assert_eq!(err.kind(), Kind::OffMode, "requested {requested:?}");
            assert_eq!(err.exit_code(), 35);
            assert_eq!(err.detail().component, Component::RadioMode);
            assert!(
                err.to_string().contains("radio.mode is off"),
                "the message must name the real cause: {err}"
            );
            assert_eq!(
                err.detail().transport,
                requested,
                "transport carries what was asked for, and nothing when nothing was"
            );
        }
    }

    /// §6: all four mode-gate refusals report `component: radio-mode`, so a consumer can tell
    /// a dispatch refusal from a transport failure without parsing the message.
    #[test]
    fn every_mode_gate_refusal_reports_the_radio_mode_component() {
        let cases = [
            resolve(Mode::Off, Some(Transport::Lorawan), RadioArity::Unified),
            resolve(Mode::P2p, Some(Transport::Lorawan), RadioArity::Unified),
            resolve(Mode::Lorawan, Some(Transport::P2p), RadioArity::Unified),
            resolve(Mode::Dual, None, RadioArity::Unified),
        ];
        for got in cases {
            let err = got.expect_err("all four are refusals");
            assert_eq!(
                err.detail().component,
                Component::RadioMode,
                "{} must report radio-mode",
                err.kind()
            );
        }
    }

    fn heard(registered: bool, last: Option<&str>) -> Heard {
        Heard {
            registered,
            last: last.map(str::to_string),
        }
    }

    /// p2p_integration.md §11: a Node both carriers hold follows the one it was last heard on.
    #[test]
    fn a_node_on_both_carriers_follows_the_one_it_was_last_heard_on() {
        const LW: Transport = Transport::Lorawan;
        const PP: Transport = Transport::P2p;
        let early = Some("2026-09-27T11:10:00Z");
        let late = Some("2026-09-27T11:17:15.204311Z");
        let cases: &[(Heard, Heard, Transport, &str)] = &[
            (heard(true, early), heard(true, late), LW, "LoRaWAN newer"),
            (heard(true, late), heard(true, early), PP, "P2P newer"),
            (heard(true, None), heard(true, None), PP, "heard on neither"),
            (
                heard(true, None),
                heard(true, early),
                LW,
                "heard on LoRaWAN only",
            ),
            (
                heard(true, early),
                heard(true, None),
                PP,
                "heard on P2P only",
            ),
            (
                heard(true, early),
                heard(true, early),
                PP,
                "the same instant",
            ),
            (
                heard(true, None),
                heard(false, late),
                PP,
                "P2P registry only",
            ),
            (
                heard(false, late),
                heard(true, None),
                LW,
                "LoRaWAN registry only",
            ),
            (
                heard(false, None),
                heard(false, None),
                LW,
                "neither registry",
            ),
        ];
        for (p2p, lorawan, want, why) in cases {
            assert_eq!(live_carrier(p2p, lorawan), *want, "{why}");
        }
    }

    /// A whole-second P2P stamp against a fractional LoRaWAN one in the same second: a plain
    /// string compare would call `…:05Z` the later of the two.
    #[test]
    fn the_recency_compare_orders_whole_and_fractional_seconds() {
        let p2p = heard(true, Some("2026-09-27T11:17:05Z"));
        let lorawan = heard(true, Some("2026-09-27T11:17:05.100000Z"));
        assert_eq!(live_carrier(&p2p, &lorawan), Transport::Lorawan);
        let p2p = heard(true, Some("2026-09-27T11:17:06Z"));
        assert_eq!(live_carrier(&p2p, &lorawan), Transport::P2p);
    }
}
