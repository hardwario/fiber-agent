//! Encoding side of the STICKER fPort-85 protocol: build `Command` downlinks
//! and the remote-configuration engine (#68).
//!
//! The config-write API takes the same flat `group.field` key space that
//! `sticker_response::decode_config` produces (#70), so a desired config can be
//! written here and verified with `diff_config` against a `GetParam` read-back.
//!
//! `Command`s are emitted with `seq = 0`; the live sender
//! (`LoRaWANHandle::send_command`) stamps the real sequence number and awaits
//! the correlated `Response` (Ack on success, Error on validation failure).
//!
//! Scope: the `Application` group (sampling/reporting/history) plus the two
//! scalar `Alarms` fields. The `Lorawan` group is intentionally excluded —
//! changing region/activation/keys can permanently disconnect the device and
//! needs a separate guarded flow (#35).

use std::collections::BTreeMap;

use prost::Message;

use super::sticker_proto::app_config_message::{Alarms, Application};
use super::sticker_proto::{command, Command};
use super::sticker_response::ConfigValue;

/// EU868 DR0 (SF12) maximum downlink application payload, in bytes. The STICKER
/// receives the raw `Command` protobuf on fPort 85 (no proto-version prefix on
/// downlinks), so the whole encoded `Command` must fit this budget at the
/// worst-case data rate.
pub const DR0_COMMAND_BUDGET: usize = 51;

/// A field that failed server-side validation before anything was sent
/// (fail-fast — never burn airtime on a value the firmware would reject).
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigError {
    pub key: String,
    pub reason: String,
}

#[derive(Clone, Copy)]
enum Kind {
    Bool,
    /// inclusive `[min, max]`; `zero_ok` allows the sentinel 0 outside the range.
    Uint { min: u64, max: u64, zero_ok: bool },
    /// free uint32 (e.g. bitmask) — only range-checked to u32.
    Bitmask,
}

/// Settable `group.field` → (proto field number within its group, validation).
/// Ranges mirror `app_config.yml` @ the live `origin/v1.4.0` branch (HEAD
/// 6eb111b), which is the single source of truth: `configen` generates
/// `app_config.proto` from it, so a parameter's yml `proto_id` IS its protobuf
/// wire field number within its group submessage (e.g. interval_report = 3).
/// NOTE: the frozen `v1.4.0` *tag* (daec406) predates the #166/#174 field
/// recompaction and uses different numbers (interval_report = 4, history_enable
/// = 49, …); our proto tracks the branch, matching the bench device.
const SETTABLE: &[(&str, u32, Kind)] = &[
    ("application.calibration", 1, Kind::Bool),
    ("application.interval_sample", 2, Kind::Uint { min: 5, max: 3600, zero_ok: true }),
    ("application.interval_report", 3, Kind::Uint { min: 60, max: 86400, zero_ok: false }),
    ("application.history_enable", 4, Kind::Bool),
    ("application.history_sensors", 5, Kind::Bitmask),
    ("alarms.alarm_limit", 1, Kind::Uint { min: 0, max: 3600, zero_ok: false }),
    ("alarms.alarm_notif_time", 2, Kind::Uint { min: 1, max: 60, zero_ok: false }),
];

fn spec(key: &str) -> Option<(u32, Kind)> {
    SETTABLE.iter().find(|(k, _, _)| *k == key).map(|(_, f, k)| (*f, *k))
}

/// Map an `Error.fault_field` (proto field number) back to the key the engine
/// sent, so the UI/API can name the offending parameter. `fault_field` is
/// ambiguous across groups, so it is resolved only among `sent_keys`.
pub fn describe_fault<'a>(fault_field: u32, sent_keys: impl IntoIterator<Item = &'a str>) -> Option<String> {
    sent_keys
        .into_iter()
        .find(|k| spec(k).map(|(f, _)| f) == Some(fault_field))
        .map(|k| k.to_string())
}

/// Validate one value against its field spec.
fn validate_one(key: &str, v: &ConfigValue) -> Result<(), ConfigError> {
    let err = |reason: String| ConfigError { key: key.to_string(), reason };
    let (_, kind) = spec(key).ok_or_else(|| err("not a remotely settable parameter".into()))?;
    match (kind, v) {
        (Kind::Bool, ConfigValue::Bool(_)) => Ok(()),
        (Kind::Bool, _) => Err(err("expected a boolean".into())),
        (Kind::Bitmask, ConfigValue::Uint(n)) if *n <= u32::MAX as u64 => Ok(()),
        (Kind::Bitmask, ConfigValue::Uint(_)) => Err(err("exceeds uint32".into())),
        (Kind::Bitmask, _) => Err(err("expected an unsigned integer".into())),
        (Kind::Uint { min, max, zero_ok }, ConfigValue::Uint(n)) => {
            if (zero_ok && *n == 0) || (*n >= min && *n <= max) {
                Ok(())
            } else {
                let z = if zero_ok { " (or 0)" } else { "" };
                Err(err(format!("out of range {min}..={max}{z}, got {n}")))
            }
        }
        (Kind::Uint { .. }, _) => Err(err("expected an unsigned integer".into())),
    }
}

/// Validate every key in a desired config. Returns all errors at once (so the
/// UI can show them together) — or the ordered list of validated settable
/// fields, in `SETTABLE` order for deterministic batching.
pub fn validate(config: &BTreeMap<String, ConfigValue>) -> Result<Vec<(String, ConfigValue)>, Vec<ConfigError>> {
    let mut errors = Vec::new();
    for (k, v) in config {
        if let Err(e) = validate_one(k, v) {
            errors.push(e);
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    // emit in canonical SETTABLE order
    let mut out = Vec::new();
    for (k, _, _) in SETTABLE {
        if let Some(v) = config.get(*k) {
            out.push((k.to_string(), v.clone()));
        }
    }
    Ok(out)
}

/// Set one validated `group.field` value onto a `SetParam`, creating the group
/// submessage on demand.
fn apply(sp: &mut command::SetParam, key: &str, v: &ConfigValue) {
    let group = key.split('.').next().unwrap_or("");
    match (group, key, v) {
        ("application", "application.calibration", ConfigValue::Bool(b)) => {
            sp.application.get_or_insert_with(Application::default).calibration = Some(*b);
        }
        ("application", "application.interval_sample", ConfigValue::Uint(n)) => {
            sp.application.get_or_insert_with(Application::default).interval_sample = Some(*n as u32);
        }
        ("application", "application.interval_report", ConfigValue::Uint(n)) => {
            sp.application.get_or_insert_with(Application::default).interval_report = Some(*n as u32);
        }
        ("application", "application.history_enable", ConfigValue::Bool(b)) => {
            sp.application.get_or_insert_with(Application::default).history_enable = Some(*b);
        }
        ("application", "application.history_sensors", ConfigValue::Uint(n)) => {
            sp.application.get_or_insert_with(Application::default).history_sensors = Some(*n as u32);
        }
        ("alarms", "alarms.alarm_limit", ConfigValue::Uint(n)) => {
            sp.alarms.get_or_insert_with(Alarms::default).alarm_limit = Some(*n as u32);
        }
        ("alarms", "alarms.alarm_notif_time", ConfigValue::Uint(n)) => {
            sp.alarms.get_or_insert_with(Alarms::default).alarm_notif_time = Some(*n as u32);
        }
        _ => {} // unreachable after validate(); ignore defensively
    }
}

fn set_param_command(sp: command::SetParam) -> Command {
    Command { seq: 0, body: Some(command::Body::SetParam(sp)) }
}

/// Build the `SetParam` downlink(s) for a desired config. Validates first
/// (fail-fast). Fields are greedily packed so each encoded `Command` stays
/// within `max_command_len`.
///
/// `save` controls the COMMIT semantics, and therefore what the device does:
/// - `save = true`  → the **last** batch carries `save=true`, so the device
///   stages every batch and then persists + **reboots** once (destructive).
/// - `save = false` → no batch carries `save`, so the values are only staged in
///   the device's RAM and are reverted on the next reboot (non-destructive dry
///   run / inspect-before-commit).
///
/// Returns `Command`s with `seq = 0` — the sender stamps the real seq.
pub fn build_set_param(
    config: &BTreeMap<String, ConfigValue>,
    max_command_len: usize,
    save: bool,
) -> Result<Vec<Command>, Vec<ConfigError>> {
    let fields = validate(config)?;

    let mut commands: Vec<Command> = Vec::new();
    let mut current = command::SetParam::default();
    let mut current_has = false;

    for (key, value) in &fields {
        // tentatively add the field, then check the encoded size
        let mut trial = current.clone();
        apply(&mut trial, key, value);
        let fits = set_param_command(trial.clone()).encoded_len() <= max_command_len;

        if !fits && current_has {
            // flush the current batch (never the last → save stays unset)
            commands.push(set_param_command(std::mem::take(&mut current)));
            current_has = false;
            apply(&mut current, key, value);
            current_has = true;
        } else {
            current = trial;
            current_has = true;
        }
    }

    // Only the final batch commits, and only when the caller asked to save.
    if save {
        current.save = Some(true);
    }
    commands.push(set_param_command(current));
    Ok(commands)
}

// --- simple no-arg / read command builders (used by read-back + #71) ---

fn cmd(body: command::Body) -> Command {
    Command { seq: 0, body: Some(body) }
}

/// `GetParam` reading back the given `group.field` keys — the read-side partner
/// of `build_set_param`, used to verify a write landed (decode → `diff_config`).
/// Unknown keys are skipped. The full-dump `GetConfig` is avoided on purpose
/// (it overflows the device stack in fw v1.4.0, hardware/sticker-firmware#176).
pub fn build_get_param(keys: &[&str]) -> Command {
    build_get_param_page(keys, 0)
}

/// Like [`build_get_param`] but requests a specific ConfigDump `page`. The
/// device pages the response: the host reads page 0, learns `page_count` from
/// the `ConfigDump`, then fetches the rest. `page == 0` encodes no page field,
/// so it is wire-identical to a plain [`build_get_param`].
pub fn build_get_param_page(keys: &[&str], page: u32) -> Command {
    let mut gp = command::GetParam::default();
    for k in keys {
        match spec(k).map(|(f, _)| f) {
            Some(f) if k.starts_with("application.") => gp.application_field.push(f),
            Some(f) if k.starts_with("alarms.") => gp.alarms_field.push(f),
            _ => {}
        }
    }
    if page > 0 {
        gp.page = Some(page);
    }
    cmd(command::Body::GetParam(gp))
}

/// Every remotely-settable `group.field` key, in canonical [`SETTABLE`] order.
/// Used to read back the full settable set when no specific keys are requested.
pub fn all_settable_keys() -> Vec<&'static str> {
    SETTABLE.iter().map(|(k, _, _)| *k).collect()
}

pub fn build_get_info() -> Command {
    cmd(command::Body::GetInfo(command::GetInfo::default()))
}

pub fn build_reboot() -> Command {
    cmd(command::Body::Reboot(command::Reboot::default()))
}

pub fn build_force_send() -> Command {
    cmd(command::Body::ForceSend(command::ForceSend::default()))
}

/// Empty `ResetCounters` = reset every pulse counter channel.
pub fn build_reset_counters() -> Command {
    cmd(command::Body::ResetCounters(command::ResetCounters::default()))
}

/// `ClockSync` carrying an explicit wall-clock (Unix seconds) to push to the device.
pub fn build_clock_sync(unix_time: u32) -> Command {
    cmd(command::Body::ClockSync(command::ClockSync { unix_time: Some(unix_time) }))
}

/// Parse a raw `key=value` string into a [`ConfigValue`] of the type the field
/// expects (bool vs unsigned), per the [`SETTABLE`] spec. Range validation
/// happens later in [`build_set_param`]/[`validate`]; this only fixes the type.
pub fn parse_value(key: &str, raw: &str) -> Result<ConfigValue, ConfigError> {
    let err = |reason: String| ConfigError { key: key.to_string(), reason };
    let (_, kind) = spec(key).ok_or_else(|| err("not a remotely settable parameter".into()))?;
    match kind {
        Kind::Bool => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Ok(ConfigValue::Bool(true)),
            "false" | "0" | "off" | "no" => Ok(ConfigValue::Bool(false)),
            _ => Err(err(format!("expected a boolean (true/false), got {raw:?}"))),
        },
        Kind::Uint { .. } | Kind::Bitmask => raw
            .trim()
            .parse::<u64>()
            .map(ConfigValue::Uint)
            .map_err(|_| err(format!("expected an unsigned integer, got {raw:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::sticker_proto::{command, Command};

    fn cfg(pairs: &[(&str, ConfigValue)]) -> BTreeMap<String, ConfigValue> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn decode_set_param(c: &Command) -> command::SetParam {
        match &c.body {
            Some(command::Body::SetParam(sp)) => sp.clone(),
            other => panic!("expected SetParam, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_out_of_range_and_wrong_type() {
        let errs = validate(&cfg(&[
            ("application.interval_report", ConfigValue::Uint(10)), // < 60
            ("alarms.alarm_notif_time", ConfigValue::Uint(99)),     // > 60
            ("application.calibration", ConfigValue::Uint(1)),      // wrong type
            ("application.bogus", ConfigValue::Uint(1)),            // unknown
        ]))
        .unwrap_err();
        assert_eq!(errs.len(), 4);
        assert!(errs.iter().any(|e| e.key == "application.interval_report"));
        assert!(errs.iter().any(|e| e.key == "application.bogus"));
    }

    #[test]
    fn validate_allows_zero_sentinel_for_interval_sample() {
        // interval_sample has zero_allowed (0 = precede report), range 5..3600
        assert!(validate(&cfg(&[("application.interval_sample", ConfigValue::Uint(0))])).is_ok());
        assert!(validate(&cfg(&[("application.interval_sample", ConfigValue::Uint(4))])).is_err());
    }

    #[test]
    fn single_batch_sets_fields_and_save() {
        let cmds = build_set_param(
            &cfg(&[
                ("application.interval_report", ConfigValue::Uint(1200)),
                ("application.history_enable", ConfigValue::Bool(true)),
                ("alarms.alarm_limit", ConfigValue::Uint(300)),
            ]),
            DR0_COMMAND_BUDGET,
            true,
        )
        .unwrap();
        assert_eq!(cmds.len(), 1); // small config → one downlink
        let sp = decode_set_param(&cmds[0]);
        assert_eq!(sp.save, Some(true));
        let app = sp.application.unwrap();
        assert_eq!(app.interval_report, Some(1200));
        assert_eq!(app.history_enable, Some(true));
        assert_eq!(sp.alarms.unwrap().alarm_limit, Some(300));
    }

    #[test]
    fn save_false_does_not_commit() {
        // save=false → values staged only, no batch carries save (no reboot).
        let cmds = build_set_param(
            &cfg(&[("application.interval_report", ConfigValue::Uint(600))]),
            DR0_COMMAND_BUDGET,
            false,
        )
        .unwrap();
        assert_eq!(cmds.len(), 1);
        let sp = decode_set_param(&cmds[0]);
        assert_eq!(sp.save, None, "save=false must not set the commit flag");
        assert_eq!(sp.application.unwrap().interval_report, Some(600));
    }

    #[test]
    fn batching_puts_save_on_last_only() {
        // Tiny budget so single fields fit alone but pairs don't → forced split.
        let budget = 8;
        let cmds = build_set_param(
            &cfg(&[
                ("application.interval_report", ConfigValue::Uint(1200)),
                ("application.interval_sample", ConfigValue::Uint(60)),
                ("alarms.alarm_limit", ConfigValue::Uint(300)),
            ]),
            budget,
            true,
        )
        .unwrap();
        assert!(cmds.len() >= 2, "expected multiple batches, got {}", cmds.len());
        for (i, c) in cmds.iter().enumerate() {
            let sp = decode_set_param(c);
            let is_last = i == cmds.len() - 1;
            assert_eq!(sp.save, if is_last { Some(true) } else { None }, "save placement at #{i}");
            // non-final batches must respect the budget; the final one may exceed
            // it only by the 2-byte save flag (unavoidable on the commit message).
            if !is_last {
                assert!(c.encoded_len() <= budget, "batch #{i} = {} B > {budget}", c.encoded_len());
            }
        }
        // round-trip: every requested field is present across the batches
        let mut report = None;
        let mut sample = None;
        let mut limit = None;
        for c in &cmds {
            let sp = decode_set_param(c);
            if let Some(a) = &sp.application {
                report = report.or(a.interval_report);
                sample = sample.or(a.interval_sample);
            }
            if let Some(al) = &sp.alarms {
                limit = limit.or(al.alarm_limit);
            }
        }
        assert_eq!((report, sample, limit), (Some(1200), Some(60), Some(300)));
    }

    #[test]
    fn describe_fault_maps_field_number_to_sent_key() {
        // application.interval_report is proto field 3.
        let sent = ["application.interval_report", "application.history_enable"];
        assert_eq!(describe_fault(3, sent), Some("application.interval_report".to_string()));
        assert_eq!(describe_fault(4, sent), Some("application.history_enable".to_string()));
        assert_eq!(describe_fault(99, sent), None);
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }

    #[test]
    fn real_hw_set_param_accepted_with_ack() {
        // GOLDEN: the STICKER (fw v1.4.0) accepted exactly these bytes over the
        // shell-inject path and replied Response{seq, Ack} (action 1=save was
        // recognised, just not executed from a shell inject). Ties our builder
        // output to bytes real firmware parses, plus the decode of its Ack.
        use crate::libs::lorawan::sticker_response::{decode_response, DecodedResponse, ResponseKind};
        let config = cfg(&[
            ("application.interval_report", ConfigValue::Uint(1200)),
            ("application.history_enable", ConfigValue::Bool(true)),
        ]);
        let mut cmds = build_set_param(&config, DR0_COMMAND_BUDGET, true).unwrap();
        assert_eq!(cmds.len(), 1);
        cmds[0].seq = 9; // sender stamps the seq
        assert_eq!(hex(&cmds[0].encode_to_vec()), "08091209120518b00920011801");

        // the real Ack the device returned (on-wire, incl. 0x01 version byte)
        let resp = [0x01u8, 0x08, 0x09, 0x12, 0x00];
        let d = decode_response(&resp[1..]).unwrap();
        assert_eq!(d, DecodedResponse { seq: 9, kind: ResponseKind::Ack });
    }

    #[test]
    fn get_param_selects_proto_fields_per_group() {
        let c = build_get_param(&["application.interval_report", "alarms.alarm_limit", "bogus"]);
        match c.body {
            Some(command::Body::GetParam(gp)) => {
                assert_eq!(gp.application_field, vec![3]);
                assert_eq!(gp.alarms_field, vec![1]);
            }
            other => panic!("expected GetParam, got {other:?}"),
        }
    }
}
