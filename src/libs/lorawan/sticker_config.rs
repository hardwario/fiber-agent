//! Shared STICKER fPort-85 config read/write engine.
//!
//! Used by BOTH the on-device `fiberctl` control server (`control/server.rs`)
//! and the MQTT command path (`mqtt/monitor.rs`), so the validate → build →
//! send → merge logic lives in exactly one place and the two front-ends only
//! differ in how they shape their output (CLI JSON vs an MQTT publish).
//!
//! Transport-agnostic: every entry point takes a `&LoRaWANHandle` (the live
//! fPort-85 sender that stamps the seq and awaits the correlated `Response`) and
//! returns structured Rust data — no JSON, no MQTT here.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::monitor::LoRaWANHandle;
use super::sticker_command::{self as sc, ConfigError};
use super::sticker_response::{ConfigValue, DecodedResponse, HistoryRecord, ResponseKind};

/// Result of reading a STICKER's config (all ConfigDump pages merged).
#[derive(Debug, Clone)]
pub struct ConfigRead {
    /// Merged `group.field` → value across every page returned by the device.
    pub config: BTreeMap<String, ConfigValue>,
    /// Number of pages the device reported (1 for a single-page dump).
    pub page_count: u32,
    /// `seq` of the last device response received.
    pub last_seq: u32,
    /// Keys whose chunk failed, so the caller can report a partial read honestly
    /// and re-request just the gap instead of repeating the whole thing.
    pub failed_keys: Vec<String>,
}

impl ConfigRead {
    /// True when every requested chunk came back.
    pub fn is_complete(&self) -> bool {
        self.failed_keys.is_empty()
    }
}

/// Outcome of one SetParam batch within a write sequence.
#[derive(Debug, Clone)]
pub enum BatchOutcome {
    /// The device replied (Ack on success, Error on rejection, …).
    Replied(DecodedResponse),
    /// The final `save` batch persists + reboots the device, so a missing reply
    /// is expected rather than a failure.
    SavedNoReply { transport_error: String },
    /// A batch got no reply when one was expected — a transport failure.
    Failed { transport_error: String },
}

/// Result of writing a STICKER's config — one entry per SetParam batch sent.
#[derive(Debug, Clone)]
pub struct ConfigWrite {
    pub batches: Vec<BatchOutcome>,
    /// True when every batch landed (treating the post-`save` reboot as success).
    pub all_ok: bool,
    /// `seq` of the last device response received (0 if none).
    pub last_seq: u32,
}

/// Read selected `group.field` keys, following ConfigDump paging
/// (`GetParam{page}` → `ConfigDump{page_index, page_count}`) until every page is
/// collected. Empty `keys` reads the full settable set. Each page is its own
/// `send_command` (its own seq), so the one-response-per-seq correlation in the
/// LoRaWAN monitor is sufficient — no multi-frame collection needed here.
pub fn read_config(
    handle: &LoRaWANHandle,
    dev_eui: &str,
    keys: &[&str],
    timeout: Duration,
) -> Result<ConfigRead, String> {
    // Default to the SMALL core set when the caller selects nothing, deliberately
    // not the whole settable surface. The #69 work grew SETTABLE from 4 scalars to
    // 20, and a Class-A read is chunked six fields at a time with a 180 s timeout
    // per chunk — so defaulting to everything would turn every unqualified read
    // into minutes of airtime. This keeps the live-verified default path requesting
    // exactly what it always has; the wider sets are opt-in.
    let owned_all: Vec<&str>;
    let selected: &[&str] = if keys.is_empty() {
        owned_all = sc::core_settable_keys();
        &owned_all
    } else {
        keys
    };

    let mut merged: BTreeMap<String, ConfigValue> = BTreeMap::new();
    let mut last_seq: u32 = 0;
    let mut page_count_max: u32 = 0;
    // ConfigDump never spans this many DR0 pages; guards against a misbehaving
    // device looping forever.
    const MAX_PAGES: u32 = 16;
    // The sticker caps how many fields a single GetParam may request (it rejects
    // an over-long request with bad_request "array overflow"). Split large reads
    // — e.g. the 16 alarm slots — into small chunks so we never hit that cap.
    // See docs/sticker-alarm-readback-issue.md.
    const MAX_FIELDS_PER_GETPARAM: usize = 6;
    // Accumulate across chunks and record the ones that failed, rather than
    // abandoning the whole read on the first failure. With the #69 surface a full
    // read is several chunks, and one flaky Class-A round trip used to discard every
    // chunk already received — so an operator saw nothing instead of most of it.
    let mut failed_keys: Vec<String> = Vec::new();
    let mut chunks_attempted = 0usize;
    // Why the first chunk failed. Kept so a read that got nothing can say what
    // went wrong instead of only how many keys it lost — the reason was being
    // logged to the journal and then discarded, so the operator-visible error
    // pointed at the sticker even when the gateway was the one at fault.
    let mut first_failure: Option<String> = None;

    for chunk in selected.chunks(MAX_FIELDS_PER_GETPARAM) {
        chunks_attempted += 1;
        let mut page = 0u32;
        loop {
            let command = sc::build_get_param_page(chunk, page);
            let dr = match handle.send_command(dev_eui, command, timeout) {
                Ok(dr) => dr,
                Err(e) => {
                    eprintln!(
                        "[sticker] {dev_eui}: config read chunk {chunk:?} failed: {e} \
                         (keeping the chunks already read)"
                    );
                    if first_failure.is_none() {
                        first_failure = Some(e);
                    }
                    failed_keys.extend(chunk.iter().map(|k| k.to_string()));
                    break;
                }
            };
            last_seq = dr.seq;
            let ResponseKind::ConfigDump { page_index, page_count: pc, config } = dr.kind else {
                eprintln!(
                    "[sticker] {dev_eui}: expected ConfigDump for {chunk:?}, got {:?}",
                    dr.kind
                );
                if first_failure.is_none() {
                    first_failure = Some("device answered something other than a ConfigDump".into());
                }
                failed_keys.extend(chunk.iter().map(|k| k.to_string()));
                break;
            };
            for (k, v) in config {
                merged.insert(k, v);
            }
            let pc = pc.max(1);
            page_count_max = page_count_max.max(pc);
            if pc <= 1 || page_index + 1 >= pc || page + 1 >= MAX_PAGES {
                break;
            }
            page = page_index + 1;
        }
    }

    // Only a read that got nothing at all is an error. A partial read is a
    // legitimate outcome the caller reports as partial.
    if merged.is_empty() && chunks_attempted > 0 && !failed_keys.is_empty() {
        return Err(match first_failure {
            Some(why) => format!(
                "config read failed for every requested key ({} keys): {}",
                failed_keys.len(),
                why
            ),
            None => format!(
                "config read failed for every requested key ({} keys)",
                failed_keys.len()
            ),
        });
    }

    Ok(ConfigRead {
        config: merged,
        page_count: page_count_max.max(1),
        last_seq,
        failed_keys,
    })
}

/// Read a STICKER's device info: one `GetInfo` downlink, one `Info` uplink (#65).
///
/// Returns the device's `seq` alongside the decoded info so a caller can report
/// which exchange produced it.
///
/// A device with several latched alarms can answer with
/// `Error{unknown, "response too large"}`: v1.4.0's `Info` carries
/// `active_alarms` on both transports and the fPort-85 response buffer is only
/// 64 bytes (`app_cmd.c:1133-1144`). That error is returned verbatim rather than
/// retried — a retry would produce the same reply and only burn airtime.
pub fn read_info(
    handle: &LoRaWANHandle,
    dev_eui: &str,
    timeout: Duration,
) -> Result<(u32, super::sticker_response::DeviceInfo), String> {
    let dr = handle.send_command(dev_eui, sc::build_get_info(), timeout)?;
    match dr.kind {
        ResponseKind::Info(info) => Ok((dr.seq, info)),
        ResponseKind::Error { code, detail, .. } => {
            Err(format!("device error {code}: {detail}"))
        }
        other => Err(format!("expected Info, got {other:?}")),
    }
}

/// How long a device is considered busy after an action-bearing command.
///
/// The sticker does not run a deferred action immediately: it answers first and
/// schedules the action 8 s later (`app_lrw.c:738-743`). 12 s leaves margin for
/// the reply itself plus the scheduling delay.
pub const ACTION_SETTLE: Duration = Duration::from_secs(12);

/// "Busy until" per dev_eui, for [`try_action_guard`].
fn action_busy_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, Instant>> {
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Instant>>,
    > = std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Held while an action-bearing fPort-85 command settles on one device. Dropping
/// it does **not** release the device early — the 8 s deferred action is still
/// pending on the sticker regardless of what the gateway does next.
#[derive(Debug)]
pub struct ActionGuard {
    dev_eui: String,
}

impl ActionGuard {
    /// The device this guard covers.
    pub fn dev_eui(&self) -> &str {
        &self.dev_eui
    }
}

/// Serialise **action-bearing** fPort-85 commands per device.
///
/// The sticker holds a single `m_post_cmd_action` slot (`app_lrw.c:254`) and its
/// downlink queue is two deep, and `dl_request_work_handler` drains the whole
/// queue in one pass — so if two action-bearing commands arrive together, the
/// second overwrites the first and only it ever runs. The first command still
/// answers `Ack`, so the loss is completely silent.
///
/// Action-bearing commands are `SetParam{save:true}`, `reboot`, `device_reset`,
/// `reset_counters`, `settings_save`, `enter_calibration`, `lrw_reset` and
/// `lrw_join`.
///
/// This closes a race that predates the #71 commands: `control/server.rs` took
/// `ctx.lorawan_lock` but the MQTT write path did not, so an MQTT
/// `set_sticker_config{save:true}` could already collide with a `fiberctl reboot`.
///
/// Deliberately **try**-style rather than blocking: a viewer gets an immediate
/// "device busy, retry in Ns" instead of a request that hangs for 12 s.
pub fn try_action_guard(dev_eui: &str) -> Result<ActionGuard, String> {
    try_action_guard_at(dev_eui, Instant::now(), ACTION_SETTLE)
}

/// [`try_action_guard`] with an explicit clock and settle time, so the behaviour
/// is testable without sleeping for the real 12 s.
fn try_action_guard_at(
    dev_eui: &str,
    now: Instant,
    settle: Duration,
) -> Result<ActionGuard, String> {
    let mut map = action_busy_map()
        .lock()
        .map_err(|_| "action guard poisoned".to_string())?;
    // Opportunistic sweep so a long-lived process does not accumulate an entry
    // per sticker it has ever talked to.
    map.retain(|_, busy_until| *busy_until > now);
    if let Some(busy_until) = map.get(dev_eui) {
        let remaining = busy_until.saturating_duration_since(now).as_secs() + 1;
        return Err(format!(
            "device busy: another action command is still settling, retry in {remaining}s"
        ));
    }
    map.insert(dev_eui.to_string(), now + settle);
    Ok(ActionGuard { dev_eui: dev_eui.to_string() })
}

/// Minimum spacing between unsigned `force_send` triggers for one device.
pub const FORCE_SEND_COOLDOWN: Duration = Duration::from_secs(60);

/// "Next allowed at" per dev_eui, for [`check_force_send_cooldown`].
fn force_send_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, Instant>> {
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Instant>>,
    > = std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Rate-limit `force_send` per device.
///
/// `force_send` is unsigned — it changes no device state, so requiring the
/// Ed25519 handshake for it would be theatre. But that also means anything with
/// broker access can trigger uplinks, and a sticker has a finite duty cycle: a
/// tight loop would exhaust its airtime budget and starve real telemetry. The
/// global subscriber rate limit is not per-device, so it cannot prevent this.
pub fn check_force_send_cooldown(dev_eui: &str) -> Result<(), String> {
    check_force_send_cooldown_at(dev_eui, Instant::now(), FORCE_SEND_COOLDOWN)
}

fn check_force_send_cooldown_at(
    dev_eui: &str,
    now: Instant,
    cooldown: Duration,
) -> Result<(), String> {
    let mut map = force_send_map()
        .lock()
        .map_err(|_| "force_send cooldown poisoned".to_string())?;
    map.retain(|_, next_allowed| *next_allowed > now);
    if let Some(next_allowed) = map.get(dev_eui) {
        let remaining = next_allowed.saturating_duration_since(now).as_secs() + 1;
        return Err(format!(
            "force_send rate limited for this device, retry in {remaining}s \
             (a sticker's duty cycle is finite)"
        ));
    }
    map.insert(dev_eui.to_string(), now + cooldown);
    Ok(())
}

/// Validate + write a desired config. Validation failures return `Err` before
/// any airtime is spent (fail-fast). On `Ok`, every SetParam batch was sent;
/// inspect `batches`/`all_ok` for the per-batch device outcome. The final batch
/// carries `save` when `save == true` (persists + reboots the device).
pub fn write_config(
    handle: &LoRaWANHandle,
    dev_eui: &str,
    config: &BTreeMap<String, ConfigValue>,
    save: bool,
    timeout: Duration,
) -> Result<ConfigWrite, Vec<ConfigError>> {
    let commands = sc::build_set_param(config, sc::DR0_COMMAND_BUDGET, save)?;

    // save=true makes the last batch action-bearing (SETTINGS_SAVE -> persist +
    // reboot), so it must not overlap another action command on the same device.
    // Reported as a validation-style error because that is the channel this
    // signature already has for "refused before spending airtime".
    let _guard = if save {
        match try_action_guard(dev_eui) {
            Ok(g) => Some(g),
            Err(reason) => {
                return Err(vec![ConfigError { key: "save".to_string(), reason }]);
            }
        }
    } else {
        None
    };

    let n = commands.len();
    let mut batches = Vec::with_capacity(n);
    let mut all_ok = true;
    let mut last_seq = 0u32;
    for (i, command) in commands.into_iter().enumerate() {
        let is_last = i + 1 == n;
        match handle.send_command(dev_eui, command, timeout) {
            Ok(dr) => {
                last_seq = dr.seq;
                batches.push(BatchOutcome::Replied(dr));
            }
            Err(e) => {
                // The final (save) batch reboots the device; a missing reply
                // there is expected rather than a hard failure.
                if is_last && save {
                    batches.push(BatchOutcome::SavedNoReply { transport_error: e });
                } else {
                    all_ok = false;
                    batches.push(BatchOutcome::Failed { transport_error: e });
                }
            }
        }
    }

    Ok(ConfigWrite { batches, all_ok, last_seq })
}

/// Convenience: the result code for a single batch outcome — `"ok"` for an Ack
/// or post-save reboot, the stable error code for a device Error, otherwise a
/// short descriptor. Used by callers that surface a single `last_ack.result`.
pub fn batch_result(outcome: &BatchOutcome) -> String {
    match outcome {
        BatchOutcome::Replied(dr) => match &dr.kind {
            ResponseKind::Ack => "ok".to_string(),
            ResponseKind::Error { code, .. } => (*code).to_string(),
            other => format!("{other:?}").to_lowercase(),
        },
        BatchOutcome::SavedNoReply { .. } => "ok".to_string(),
        BatchOutcome::Failed { .. } => "transport_error".to_string(),
    }
}

/// Project a decoded config map into JSON values for publishing over MQTT.
pub fn config_to_json(
    config: &BTreeMap<String, ConfigValue>,
) -> BTreeMap<String, serde_json::Value> {
    config.iter().map(|(k, v)| (k.clone(), cv_to_json(v))).collect()
}

fn cv_to_json(v: &ConfigValue) -> serde_json::Value {
    match v {
        ConfigValue::Bool(b) => serde_json::json!(b),
        ConfigValue::Uint(n) => serde_json::json!(n),
        ConfigValue::Enum(s) | ConfigValue::Hex(s) => serde_json::json!(s),
    }
}

/// Project a decoded `DeviceInfo` into the JSON shape both front-ends publish, so
/// the control socket, the MQTT query reply and the unsolicited join-time Info can
/// never disagree about field names or redaction.
///
/// `source` distinguishes how the Info arrived: `"query"` (a GetInfo we sent) or
/// `"unsolicited"` (the `seq=0` Info the sticker sends on every join, and the
/// deferred answer to an empty-body clock_sync).
///
/// Two deliberate shape choices:
///   * **`claim_token` is never emitted** — it is a provisioning secret, and this
///     payload is published to a retained MQTT topic that every new subscriber
///     replays. Callers get `has_claim_token` instead.
///   * `unix_time == 0` and `battery_mv == 0` are the firmware's "unavailable"
///     sentinels, so they publish as `null` rather than as 1970 / 0 V. Absent then
///     honestly means absent, and `device_status.flags` carries `time_unsynced`
///     for the clock case.
pub fn info_to_json(
    info: &super::sticker_response::DeviceInfo,
    dev_eui: &str,
    source: &str,
    seq: u32,
    synced_at: &str,
) -> serde_json::Value {
    use super::sticker_alarm::{quantity_name, source_name};
    use super::sticker_response::{alarm_type_name, device_status_flags};

    let alarms: Vec<serde_json::Value> = info
        .active_alarms
        .iter()
        .map(|a| {
            serde_json::json!({
                "source_id": a.source,
                "source": source_name(a.source as u8),
                "quantity_id": a.quantity,
                "quantity": quantity_name(a.quantity as u8),
                "type_id": a.kind,
                "type": alarm_type_name(a.kind),
            })
        })
        .collect();

    serde_json::json!({
        "dev_eui": dev_eui,
        "source": source,
        "seq": seq,
        "synced_at": synced_at,
        "fw_version": info.fw_version,
        "build_type": info.build_type,
        "debug": info.debug,
        "serial_number": info.serial_number,
        "uptime_s": info.uptime_s,
        "unix_time": (info.unix_time != 0).then_some(info.unix_time),
        "battery_mv": (info.battery_mv != 0).then_some(info.battery_mv),
        "reset_cause": info.reset_cause,
        // Both raw and decoded: an unknown future bit still reaches the UI as
        // "bitN" while the raw value stays available for diagnosis.
        "device_status": {
            "raw": info.device_status,
            "flags": device_status_flags(info.device_status),
        },
        "active_alarms": alarms,
        // NFC-only over the wire, so normally null over LoRaWAN. Kept in the shape
        // so a consumer does not have to special-case a missing key.
        "lrw_state": info.lrw_state,
        "dev_eui_reported": info.dev_eui,
        "has_claim_token": info.claim_token.is_some(),
    })
}

/// One page of a STICKER's on-device history (an expanded fPort-85 HistoryFrame).
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPage {
    pub frame_index: u32,
    pub frame_count: u32,
    pub records: Vec<HistoryRecord>,
}

/// Outcome of a history read: the deduplicated pages plus enough accounting for
/// the caller to tell a complete replay from a truncated one.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRead {
    /// Pages sorted by frame_index, deduplicated (first page for an index wins).
    pub pages: Vec<HistoryPage>,
    /// frame_index values in `0..frame_count` that never arrived (empty ⇒ complete).
    pub missing_indices: Vec<u32>,
    /// True when every expected frame_index was received.
    pub complete: bool,
    /// True when the device reported `history_unavailable`: a successful read
    /// that legitimately carries no data (history disabled / empty window).
    pub unavailable: bool,
}

/// A history read that failed. Preserves the stable device error `code` instead
/// of collapsing it into a `{:?}` blob, so callers can tell an expected "no data"
/// outcome from a transport or protocol failure.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryError {
    /// Transport/monitor failure (channel closed, no response within the timeout).
    Transport(String),
    /// The device replied with a typed fPort-85 Error (stable `code` kept).
    Device {
        code: &'static str,
        fault_field: u32,
        detail: String,
    },
    /// A non-history, non-error reply where a HistoryFrame was expected.
    Unexpected(String),
}

impl HistoryError {
    /// Stable, machine-readable category (e.g. for an MQTT error publish).
    pub fn stable_code(&self) -> &'static str {
        match self {
            HistoryError::Transport(_) => "transport",
            HistoryError::Device { code, .. } => code,
            HistoryError::Unexpected(_) => "unexpected_response",
        }
    }
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::Transport(s) | HistoryError::Unexpected(s) => write!(f, "{s}"),
            HistoryError::Device { code, fault_field, detail } => {
                write!(f, "{code}: {detail} (fault_field={fault_field})")
            }
        }
    }
}

/// Assemble a `HistoryRead` from the raw responses collected for one ReqHistory.
/// Split out from `read_history` so the dedup / missing-index / error-typing
/// logic is unit-testable without a live `LoRaWANHandle`.
fn build_history_read(responses: Vec<DecodedResponse>) -> Result<HistoryRead, HistoryError> {
    // A BTreeMap keyed by frame_index gives dedup + sort for free.
    let mut by_index: BTreeMap<u32, HistoryPage> = BTreeMap::new();
    let mut target: u32 = 0;
    for dr in responses {
        match dr.kind {
            ResponseKind::HistoryFrame { frame_index, frame_count, records, .. } => {
                // frame_count is a device estimate that can drift; keep the max.
                target = target.max(frame_count.max(1));
                by_index
                    .entry(frame_index)
                    .or_insert(HistoryPage { frame_index, frame_count, records });
            }
            // The device may terminate the stream with an empty/no-body response.
            ResponseKind::Empty => {}
            // No history for the window is a typed error, not a failure: report it
            // as an empty-but-successful read so the caller stops waiting cleanly.
            ResponseKind::Error { code: "history_unavailable", .. } => {
                return Ok(HistoryRead {
                    pages: Vec::new(),
                    missing_indices: Vec::new(),
                    complete: true,
                    unavailable: true,
                });
            }
            ResponseKind::Error { code, fault_field, detail } => {
                return Err(HistoryError::Device { code, fault_field, detail });
            }
            other => {
                return Err(HistoryError::Unexpected(format!(
                    "expected HistoryFrame, got {other:?}"
                )))
            }
        }
    }
    let missing_indices: Vec<u32> = (0..target).filter(|i| !by_index.contains_key(i)).collect();
    let complete = missing_indices.is_empty();
    let pages: Vec<HistoryPage> = by_index.into_values().collect();
    Ok(HistoryRead { pages, missing_indices, complete, unavailable: false })
}

/// Feature D: send ONE ReqHistory and collect the resulting HistoryFrame pages
/// (they share the command seq), returning them deduplicated and sorted by
/// frame_index together with which indices are missing. A device with no history
/// for the window returns `unavailable: true` (an empty success); a transport
/// failure or unexpected reply is a typed `HistoryError`.
pub fn read_history(
    handle: &LoRaWANHandle,
    dev_eui: &str,
    from_unix: Option<u32>,
    to_unix: Option<u32>,
    frame_timeout: Duration,
) -> Result<HistoryRead, HistoryError> {
    let command = sc::build_req_history(from_unix, to_unix);
    let responses = handle
        .send_command_collect(dev_eui, command, frame_timeout)
        .map_err(HistoryError::Transport)?;
    build_history_read(responses)
}

/// Project an expanded history record into the JSON shape published over MQTT.
pub fn history_record_to_json(r: &HistoryRecord) -> serde_json::Value {
    serde_json::json!({
        "time": r.time,
        "fields": r.fields,
        "counters": r.counters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_info() -> super::super::sticker_response::DeviceInfo {
        use super::super::sticker_response::{ActiveAlarm, DeviceInfo};
        DeviceInfo {
            fw_version: "1.4.0".into(),
            build_type: "main",
            serial_number: 2_162_164_514,
            uptime_s: 1097,
            unix_time: 1_782_198_249,
            debug: false,
            claim_token: Some("158a6a5d5b54c5118e62a8f4af0de8d2".into()),
            battery_mv: 2740,
            reset_cause: 1,
            device_status: (1 << 0) | (1 << 11),
            lrw_state: None,
            dev_eui: None,
            active_alarms: vec![ActiveAlarm { source: 0, quantity: 0, kind: 2 }],
        }
    }

    #[test]
    fn info_to_json_never_leaks_the_claim_token() {
        // The claim token is a provisioning secret and this payload is published
        // RETAINED, so a single leak is replayed to every future subscriber.
        let v = info_to_json(&device_info(), "70b3d57ed80051b2", "query", 12, "2026-07-28T19:00:00Z");
        let text = v.to_string();
        assert!(!text.contains("158a6a5d"), "claim token must never be published");
        assert!(!text.contains("claim_token\":\""), "no claim_token value key");
        assert_eq!(v["has_claim_token"], serde_json::json!(true));
    }

    #[test]
    fn info_to_json_shape_and_sentinels() {
        let v = info_to_json(&device_info(), "70b3d57ed80051b2", "query", 12, "2026-07-28T19:00:00Z");
        assert_eq!(v["dev_eui"], serde_json::json!("70b3d57ed80051b2"));
        assert_eq!(v["source"], serde_json::json!("query"));
        assert_eq!(v["fw_version"], serde_json::json!("1.4.0"));
        assert_eq!(v["battery_mv"], serde_json::json!(2740));
        // Raw kept alongside decoded names so an unknown future bit is still visible.
        assert_eq!(v["device_status"]["raw"], serde_json::json!(2049));
        assert_eq!(
            v["device_status"]["flags"],
            serde_json::json!(["alarm_any", "time_unsynced"])
        );
        // Enums carry both the symbol and the id, so an unknown id still renders.
        assert_eq!(v["active_alarms"][0]["type"], serde_json::json!("high"));
        assert_eq!(v["active_alarms"][0]["type_id"], serde_json::json!(2));
        // NFC-only fields are explicitly null rather than absent, so a consumer
        // does not have to special-case a missing key.
        assert!(v["lrw_state"].is_null());
        assert!(v["dev_eui_reported"].is_null());
    }

    #[test]
    fn info_to_json_maps_zero_sentinels_to_null() {
        // The firmware uses 0 for "RTC not synced" and "battery unavailable".
        // Publishing them as 0 would render as 1970 and 0 V — both look like data.
        let mut info = device_info();
        info.unix_time = 0;
        info.battery_mv = 0;
        let v = info_to_json(&info, "aabb", "unsolicited", 0, "2026-07-28T19:00:00Z");
        assert!(v["unix_time"].is_null(), "unix_time 0 must publish as null");
        assert!(v["battery_mv"].is_null(), "battery 0 must publish as null");
        assert_eq!(v["source"], serde_json::json!("unsolicited"));
        assert_eq!(v["seq"], serde_json::json!(0));
    }

    // The guard is process-global by design (it models one physical device), so
    // each test uses its own dev_eui to stay independent of test ordering.
    #[test]
    fn action_guard_is_exclusive_per_device() {
        let now = Instant::now();
        let eui = "guard00000000001";
        let first = try_action_guard_at(eui, now, Duration::from_secs(12));
        assert!(first.is_ok());
        let second = try_action_guard_at(eui, now, Duration::from_secs(12));
        let err = second.expect_err("a second action command must be refused");
        assert!(err.contains("device busy"), "got {err:?}");
        // The caller is told how long to wait rather than just being refused.
        assert!(err.contains("retry in"), "got {err:?}");
    }

    #[test]
    fn action_guard_allows_different_devices_concurrently() {
        // The single m_post_cmd_action slot is per sticker, so one busy device must
        // never block commands to another.
        let now = Instant::now();
        assert!(try_action_guard_at("guard00000000002", now, Duration::from_secs(12)).is_ok());
        assert!(try_action_guard_at("guard00000000003", now, Duration::from_secs(12)).is_ok());
    }

    #[test]
    fn action_guard_releases_after_the_settle_window() {
        let t0 = Instant::now();
        let eui = "guard00000000004";
        assert!(try_action_guard_at(eui, t0, Duration::from_secs(12)).is_ok());
        // Still inside the window: refused.
        assert!(try_action_guard_at(eui, t0 + Duration::from_secs(11), Duration::from_secs(12))
            .is_err());
        // Past the deferred action: allowed again.
        assert!(try_action_guard_at(eui, t0 + Duration::from_secs(13), Duration::from_secs(12))
            .is_ok());
    }

    #[test]
    fn action_guard_does_not_leak_entries_for_settled_devices() {
        let t0 = Instant::now();
        assert!(try_action_guard_at("guard00000000005", t0, Duration::from_secs(12)).is_ok());
        assert!(try_action_guard_at("guard00000000006", t0, Duration::from_secs(12)).is_ok());
        // A later call sweeps every expired entry, so a long-lived process does not
        // accumulate one per sticker it has ever talked to.
        let far = t0 + Duration::from_secs(600);
        assert!(try_action_guard_at("guard00000000007", far, Duration::from_secs(12)).is_ok());
        let map = action_busy_map().lock().unwrap();
        assert!(!map.contains_key("guard00000000005"));
        assert!(!map.contains_key("guard00000000006"));
    }

    fn hist(frame_index: u32, frame_count: u32) -> DecodedResponse {
        DecodedResponse {
            seq: 1,
            kind: ResponseKind::HistoryFrame {
                frame_index,
                frame_count,
                t0_unix: 1_700_000_000,
                present: 0x3,
                interval_s: 900,
                records: Vec::new(),
            },
        }
    }

    fn err(code: &'static str) -> DecodedResponse {
        DecodedResponse {
            seq: 1,
            kind: ResponseKind::Error { code, fault_field: 0, detail: "x".to_string() },
        }
    }

    #[test]
    fn history_unavailable_yields_empty_success() {
        let hr = build_history_read(vec![err("history_unavailable")]).unwrap();
        assert!(hr.unavailable);
        assert!(hr.complete);
        assert!(hr.pages.is_empty());
        assert!(hr.missing_indices.is_empty());
    }

    #[test]
    fn device_error_preserves_stable_code() {
        let e = build_history_read(vec![err("not_ready")]).unwrap_err();
        assert_eq!(e.stable_code(), "not_ready");
        assert!(matches!(e, HistoryError::Device { code: "not_ready", .. }));
    }

    #[test]
    fn dedup_and_missing_indices() {
        // frames 0,1,3 with frame_count 4 → index 2 never arrived.
        let hr = build_history_read(vec![hist(0, 4), hist(1, 4), hist(3, 4)]).unwrap();
        assert_eq!(hr.pages.len(), 3);
        assert_eq!(hr.missing_indices, vec![2]);
        assert!(!hr.complete);
    }

    #[test]
    fn duplicate_frame_index_does_not_inflate_or_truncate() {
        // 0,0,1 with frame_count 2 → deduped to two pages, complete.
        let hr = build_history_read(vec![hist(0, 2), hist(0, 2), hist(1, 2)]).unwrap();
        assert_eq!(hr.pages.len(), 2);
        assert!(hr.missing_indices.is_empty());
        assert!(hr.complete);
    }

    #[test]
    fn pages_sorted_by_frame_index() {
        let hr = build_history_read(vec![hist(2, 3), hist(0, 3), hist(1, 3)]).unwrap();
        let idx: Vec<u32> = hr.pages.iter().map(|p| p.frame_index).collect();
        assert_eq!(idx, vec![0, 1, 2]);
        assert!(hr.complete);
    }

    #[test]
    fn empty_stream_is_complete_with_no_data() {
        let hr = build_history_read(Vec::new()).unwrap();
        assert!(hr.pages.is_empty());
        assert!(hr.complete);
        assert!(!hr.unavailable);
    }
}
