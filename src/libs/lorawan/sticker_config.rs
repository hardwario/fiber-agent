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
use std::time::Duration;

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
    // Default to the full settable set when the caller selects nothing.
    let owned_all: Vec<&str>;
    let selected: &[&str] = if keys.is_empty() {
        owned_all = sc::all_settable_keys();
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
    for chunk in selected.chunks(MAX_FIELDS_PER_GETPARAM) {
        let mut page = 0u32;
        loop {
            let command = sc::build_get_param_page(chunk, page);
            let dr = handle.send_command(dev_eui, command, timeout)?;
            last_seq = dr.seq;
            let ResponseKind::ConfigDump { page_index, page_count: pc, config } = dr.kind else {
                return Err(format!("expected ConfigDump, got {:?}", dr.kind));
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

    Ok(ConfigRead { config: merged, page_count: page_count_max.max(1), last_seq })
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
