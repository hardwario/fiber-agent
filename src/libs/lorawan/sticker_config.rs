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
    // Assigned on every loop iteration before any break reads them.
    let mut last_seq: u32;
    let mut page_count: u32;
    let mut page = 0u32;
    // ConfigDump never spans this many DR0 pages; guards against a misbehaving
    // device looping forever.
    const MAX_PAGES: u32 = 16;
    loop {
        let command = sc::build_get_param_page(selected, page);
        let dr = handle.send_command(dev_eui, command, timeout)?;
        last_seq = dr.seq;
        let ResponseKind::ConfigDump { page_index, page_count: pc, config } = dr.kind else {
            return Err(format!("expected ConfigDump, got {:?}", dr.kind));
        };
        for (k, v) in config {
            merged.insert(k, v);
        }
        page_count = pc.max(1);
        if page_count <= 1 || page_index + 1 >= page_count || page + 1 >= MAX_PAGES {
            break;
        }
        page = page_index + 1;
    }

    Ok(ConfigRead { config: merged, page_count, last_seq })
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
pub struct HistoryPage {
    pub frame_index: u32,
    pub frame_count: u32,
    pub records: Vec<HistoryRecord>,
}

/// Feature D: send ONE ReqHistory and collect the resulting HistoryFrame pages
/// (they share the command seq), returning the per-frame expanded records sorted
/// by frame_index. A non-history reply yields an error.
pub fn read_history(
    handle: &LoRaWANHandle,
    dev_eui: &str,
    from_unix: Option<u32>,
    to_unix: Option<u32>,
    frame_timeout: Duration,
) -> Result<Vec<HistoryPage>, String> {
    let command = sc::build_req_history(from_unix, to_unix);
    let responses = handle.send_command_collect(dev_eui, command, frame_timeout)?;
    let mut pages = Vec::new();
    for dr in responses {
        match dr.kind {
            ResponseKind::HistoryFrame { frame_index, frame_count, records, .. } => {
                pages.push(HistoryPage { frame_index, frame_count, records });
            }
            // The device may terminate the stream with an empty/no-body response.
            ResponseKind::Empty => {}
            other => return Err(format!("expected HistoryFrame, got {:?}", other)),
        }
    }
    pages.sort_by_key(|p| p.frame_index);
    Ok(pages)
}

/// Project an expanded history record into the JSON shape published over MQTT.
pub fn history_record_to_json(r: &HistoryRecord) -> serde_json::Value {
    serde_json::json!({
        "time": r.time,
        "fields": r.fields,
        "counters": r.counters,
    })
}
