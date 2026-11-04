//! The command registry — one row per verb, and the **only** way to get a
//! verb's required permission and its [`Level`].
//!
//! v1 routed by `command_type ∈ SIGNED_COMMANDS` set membership with a
//! silent fallback to a dead topic (`commands/{command_type}`) for
//! anything missing from the set — that is exactly how
//! `reset_export_cursor` shipped a signed envelope to a topic nothing
//! subscribed to and still returned HTTP 200. [`lookup`] returns `None`
//! for an unknown verb; there is no fallback topic, and the caller must
//! treat `None` as an error, not "publish somewhere anyway".
//!
//! # The v2 shape (plan/portal/mqtt/design.md §4, §7)
//!
//! - **One dialect.** The eight v1 unsigned flat-dialect verbs
//!   (`get_sensor_config`, `get_interval`, `silence_buzzer`, the four
//!   `get_sticker_*` and `sticker_force_send`) are signed rows here like
//!   every other verb; `Routing` is gone, because there is one topic.
//! - **`level` is a property of the verb**, not of its permission string:
//!   `set_lorawan_sensor_config` alone spans `operate`
//!   (`sticker_clock_sync`), `manage` (`set_sticker_config`), `admin`
//!   (`reset_export_cursor`) and `destructive` (`sticker_device_reset`).
//!   Both ends bind it — the portal writes it into the envelope from this
//!   table (never from the caller) and the device checks the signed claim
//!   against its own table before any crypto (design.md §3.3 step 4).
//! - **`cert_preflight` is gone**: the preflight was a hand-picked quartet
//!   and is now universal (design.md §4.4 step 3), so the flag carries no
//!   information.
//! - **`permission` is no longer `Option`.** The three verbs that had none
//!   in v1 get their own string (design.md §4.2's ★): `get_sensor_config`
//!   and `get_interval` already existed as certificate permissions in
//!   `perms.rs`'s viewer set, and `silence_buzzer` had none "by design" —
//!   mapping it onto `get_status` instead would let a read-only
//!   certificate silence a medical alarm.
//!
//! # Sourcing and cross-check
//!
//! The catalogue is pinned against `vectors/catalogue.json`, which is the
//! one file the device's `command_type_to_permission`
//! (`control-core/src/libs/authorization/manager.rs`) and the mock read
//! too — see design.md §10 and [`crate::registry`]'s
//! `catalogue_matches_the_fixture` test. The device table is a **superset**
//! of this one: `get_status`, `get_info`, `set_screen`, `flush_storage` and
//! `beacon-detect` have device arms but no portal publisher, so they carry
//! no row here (design.md §4.2's "device-only" entries).
//!
//! The `*_eye_*` vocabulary is gone from the portal's rows: the device
//! speaks nine `beacon-*` verbs under `beacon.manage` and rejects the v1
//! spelling before the signature (plan/control/ble/plan.md §9.8 carries the
//! historical map). `set_eye_field_threshold`/`delete_eye_field_threshold`
//! are §9.8's named exception — fleet *config*, renamed with the rest of
//! the threshold family in plan/portal/control.md, not here.

use serde::{Deserialize, Serialize};

/// How far up the privilege ladder a verb reaches (design.md §4.1).
///
/// The ordering is the ladder itself, so a role's ceiling is one
/// comparison: Viewer stops at [`Level::Read`], Manager at
/// [`Level::Manage`], Admin/superuser reaches [`Level::Destructive`]
/// (design.md §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Returns state, changes nothing, redelivery is harmless — "would
    /// running it twice be invisible?"
    Read,
    /// Transient physical effect, nothing persisted — "does it survive a
    /// reboot? no → operate".
    Operate,
    /// Persists configuration on the Hub or a node — "does it change what
    /// the device does tomorrow?"
    Manage,
    /// Changes identity, trust, connectivity or export/audit state —
    /// "could it change who may command the device, or whether it can be
    /// reached?"
    Admin,
    /// Destroys data or availability — "is there something you cannot get
    /// back?"
    Destructive,
}

impl Level {
    /// The wire spelling. This is the string that rides inside the
    /// signature, so it is pinned by the vectors, not a `Debug` accident.
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Read => "read",
            Level::Operate => "operate",
            Level::Manage => "manage",
            Level::Admin => "admin",
            Level::Destructive => "destructive",
        }
    }

    /// Parse the wire spelling. Any other string is not a level — the
    /// device answers `InvalidCommand` (design.md §3.2).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Level::Read,
            "operate" => Level::Operate,
            "manage" => Level::Manage,
            "admin" => Level::Admin,
            "destructive" => Level::Destructive,
            _ => return None,
        })
    }

    /// `read` and `operate` execute on the request, with no challenge and
    /// no confirm (design.md §5). Everything above keeps challenge/confirm.
    pub const fn is_single_shot(self) -> bool {
        matches!(self, Level::Read | Level::Operate)
    }

    /// The five levels in ladder order — for exhaustive tests and for the
    /// `level:*` pseudo-permission set a certificate carries (design.md
    /// §4.3).
    pub const ALL: [Level; 5] = [
        Level::Read,
        Level::Operate,
        Level::Manage,
        Level::Admin,
        Level::Destructive,
    ];

    /// The `level:<name>` pseudo-permission the certificate must carry for
    /// this level (design.md §4.3). It is an ordinary entry in
    /// `permissions: [str]` with exact string match — the certificate
    /// schema does not change.
    pub fn permission(self) -> String {
        format!("level:{}", self.as_str())
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything the relay needs to gate and label one `command_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub command_type: &'static str,
    /// Required certificate permission — no longer optional: every verb
    /// has one (see the module doc's ★ note).
    pub permission: &'static str,
    /// The verb's level. The relay writes this into the envelope; the
    /// device checks the signed claim against its own copy.
    pub level: Level,
}

const fn cmd(command_type: &'static str, permission: &'static str, level: Level) -> CommandSpec {
    CommandSpec {
        command_type,
        permission,
        level,
    }
}

/// Shorthand for the deliberate "all Node/LoRaWAN mutations" collapse the
/// device's own table makes (`manager.rs`: a dedicated permission per verb
/// would force every provisioned certificate to be re-issued for no real
/// bar).
const LORAWAN: &str = "set_lorawan_sensor_config";
/// The v2 Beacon permission (plan/control/ble/plan.md §5, §8). Admin and
/// manager certificates gain it by `ensure_user_key`'s union re-mint —
/// today no portal-minted certificate carries it, which is why a pure
/// string rename of the `*_eye_*` verbs would still have failed.
const BEACON: &str = "beacon.manage";

/// The 61 rows. Grouped by [`Level`] so the table reads as design.md
/// §4.2's catalogue does; [`lookup`] is a linear scan over the whole
/// thing, so the grouping is a reading aid and nothing else.
const COMMANDS: &[CommandSpec] = &[
    // ---- read (7) — design.md §4.2. The device's `get_status` and
    // `get_info` arms are read too, but have no portal publisher and
    // therefore no row. `history_request` is not among them: P4 deleted the
    // verb from the wire outright — parser, `MqttCommand` variant and
    // dispatch arm — rather than re-homing it (handoff.md §1), so it is
    // neither a row here nor an arm there.
    cmd("get_sensor_config", "get_sensor_config", Level::Read),
    cmd("get_interval", "get_interval", Level::Read),
    cmd("get_sticker_config", "get_status", Level::Read),
    cmd("get_sticker_info", "get_status", Level::Read),
    cmd("get_sticker_full_config", "get_status", Level::Read),
    cmd("get_sticker_history", "get_status", Level::Read),
    cmd("beacon-history", BEACON, Level::Read),
    // ---- operate (6) — `set_screen` and `beacon-detect` are the
    // device-only operate arms.
    cmd("silence_buzzer", "silence_buzzer", Level::Operate),
    cmd("sticker_force_send", "get_status", Level::Operate),
    cmd("sticker_clock_sync", LORAWAN, Level::Operate),
    cmd(
        "set_screen_brightness",
        "set_screen_brightness",
        Level::Operate,
    ),
    cmd("set_led_brightness", "set_led_brightness", Level::Operate),
    // Reuses the screen-brightness permission deliberately — a dedicated
    // one would force every provisioned certificate to be reissued for no
    // real gain (`manager.rs`).
    cmd(
        "set_screen_timeout",
        "set_screen_brightness",
        Level::Operate,
    ),
    // ---- manage (30)
    cmd("set_threshold", "set_threshold", Level::Manage),
    cmd("set_sensor_name", "set_sensor_name", Level::Manage),
    cmd("set_sensor_location", "set_sensor_location", Level::Manage),
    cmd("set_alarm_pattern", "set_alarm_pattern", Level::Manage),
    cmd("set_interval", "set_interval", Level::Manage),
    // The 1-Wire read interval alone (config_sync.md §13 Q19): the only
    // interval the Portal sends a Hub. Under `set_interval`'s permission, so
    // no certificate needs reissuing.
    cmd("set_sample_interval", "set_interval", Level::Manage),
    cmd(
        "set_system_info_interval",
        "set_system_info_interval",
        Level::Manage,
    ),
    cmd("set_device_label", "set_device_label", Level::Manage),
    // Its own permission rather than a reuse of `set_device_label`, on the same
    // reasoning `power_off` is not `restart_application`: a certificate that
    // authorizes renaming a Hub must not thereby authorize declaring it
    // commissioned. GitLab #58; the BLE twin is FB17.
    cmd("setup-complete", "setup.manage", Level::Manage),
    cmd("set_buzzer_volume", "set_buzzer_volume", Level::Manage),
    // Deliberately NOT the screen-control permission: that one covers how
    // brightly and how long the panel is lit, this one decides which
    // measurements the panel lists at all — a different capability on a
    // Class IIa device (`manager.rs`).
    cmd("set_display_lines", "set_display_lines", Level::Manage),
    cmd("set_lorawan_sensor_config", LORAWAN, Level::Manage),
    cmd("add_lorawan_sticker", LORAWAN, Level::Manage),
    // The v2 enrolment verb (plan/portal/control.radio.md §7): `proximosctl
    // control.radio node-add` over MQTT. `node.manage` is the plan's one
    // permission for every Node mutation; the agent maps it in `manager.rs`
    // under the same string.
    cmd("node-add", "node.manage", Level::Manage),
    cmd("set_sticker_config", LORAWAN, Level::Manage),
    cmd("send_sticker_raw", LORAWAN, Level::Manage),
    cmd("sticker_reset_counters", LORAWAN, Level::Manage),
    cmd("add_external_gateway", LORAWAN, Level::Manage),
    cmd("remove_external_gateway", LORAWAN, Level::Manage),
    // The agent maps these two to `set_threshold`, not to the LoRaWAN
    // collapse — this registry follows the agent, the real verifier.
    cmd(
        "set_lorawan_field_threshold",
        "set_threshold",
        Level::Manage,
    ),
    cmd(
        "delete_lorawan_field_threshold",
        "set_threshold",
        Level::Manage,
    ),
    // §9.8's named exception to the `*_eye_*` rename: fleet config, not a
    // Beacon operation.
    cmd("set_eye_field_threshold", LORAWAN, Level::Manage),
    cmd("delete_eye_field_threshold", LORAWAN, Level::Manage),
    cmd("beacon-add", BEACON, Level::Manage),
    cmd("beacon-monitoring", BEACON, Level::Manage),
    cmd("beacon-recording", BEACON, Level::Manage),
    cmd("beacon-provision", BEACON, Level::Manage),
    cmd("beacon-send", BEACON, Level::Manage),
    cmd("known-beacons", BEACON, Level::Manage),
    // ---- admin (12)
    cmd("set_network_config", "set_network_config", Level::Admin),
    cmd("add_signer", "add_signer", Level::Admin),
    cmd("remove_signer", "remove_signer", Level::Admin),
    cmd("update_signer", "update_signer", Level::Admin),
    // v1 had no entry here at all (mqtt.md quirk #3): no `SIGNED_COMMANDS`
    // row, no permission mapping, a publish to a dead topic and an HTTP
    // 200. The permission is the one the agent already enforces.
    cmd("reset_export_cursor", LORAWAN, Level::Admin),
    // Admin, not manage: this decides where a site's measurement data leaves
    // the device for. GitLab #59; the BLE twin is FB18, whose warrant is
    // physical presence rather than a certificate.
    cmd("export-destination-set", "export.manage", Level::Admin),
    // The Hub's radio mode at runtime (plan/control/radio/node_registry.md
    // §11, amended R7): its own permission, because switching a carrier off
    // takes every Node on it off the air.
    cmd("radio-set-mode", "radio.manage", Level::Admin),
    // Flash the Hub's Northbridge with the firmware its image bundles
    // (plan/control/radio/northbridge_update.md §3.4, amended 2026-10-09):
    // the radio permission and tier, because it stops the P2P radio for about
    // a minute, as a mode switch stops a carrier.
    cmd("northbridge-update", "radio.manage", Level::Admin),
    // Flash the Hub's Southbridge with the firmware its image bundles
    // (plan/system/southbridge_update.md §3.7, amended 2026-10-09). The
    // Northbridge's permission and tier: the same certificates that may flash
    // one Hub coprocessor may flash the other; it cuts sensor power for about
    // fifteen seconds.
    cmd("southbridge-update", "radio.manage", Level::Admin),
    // A Node changes carrier (plan/portal/control.radio.md §7, amendment
    // 2026-09-27). Admin, not destructive: until the Node is switched too it
    // cannot be reached, and nothing is lost. The one Node permission.
    cmd("node-move", "node.manage", Level::Admin),
    // A Server Hub mints, rotates and revokes a Client Hub's broker account and
    // registers its gateway (plan/control/cluster.md §7, decision #38) — the
    // portal's Add-hub flow, admin+ (devices.hub.md §4). Admin, not manage: it
    // decides who may connect to this Hub's broker and publish under a
    // hostname. Not destructive either, removal included: a re-add restores
    // the account (with a new password). Its own permission, because no
    // existing certificate should thereby be able to hand out a broker
    // credential.
    cmd("cluster-client-add", "cluster.manage", Level::Admin),
    cmd("cluster-client-remove", "cluster.manage", Level::Admin),
    cmd("cluster-client-gateway", "cluster.manage", Level::Admin),
    // ---- destructive (8) — `flush_storage` is the device-only destructive
    // arm. (`restart` was a flat-dialect verb; P4 deleted it with the
    // dialect, and `command_type_to_spec` has no arm for it.)
    cmd("factory_reset", "factory_reset", Level::Destructive),
    // Deliberately its own permission, not a reuse of
    // `restart_application`/`power_off_device`: a certificate that
    // authorizes a reboot must not thereby authorize wiping the device.
    cmd("power_off", "power_off_device", Level::Destructive),
    // design.md §4.3 flags this row for review: `restart_application` is
    // `destructive` here, and its route is `RequireManager` today, so
    // managers lose it. The alternative is `admin`-with-exception; the
    // plan takes `destructive` and the review may flip the row
    // (design.md §13 item 3).
    cmd(
        "restart_application",
        "restart_application",
        Level::Destructive,
    ),
    cmd("remove_lorawan_sticker", LORAWAN, Level::Destructive),
    cmd("node-remove", "node.manage", Level::Destructive),
    cmd("sticker_reboot", LORAWAN, Level::Destructive),
    cmd("sticker_device_reset", LORAWAN, Level::Destructive),
    cmd("beacon-remove", BEACON, Level::Destructive),
];

/// Look up the spec for a `command_type`. `None` means the caller must
/// error — there is no fallback topic.
pub fn lookup(command_type: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|s| s.command_type == command_type)
}

/// Every row, in catalogue order. The one place the full catalogue is
/// exposed — `perms.rs` derives its permission sets from this instead of
/// keeping a second copy of the verb list.
pub fn all() -> &'static [CommandSpec] {
    COMMANDS
}
