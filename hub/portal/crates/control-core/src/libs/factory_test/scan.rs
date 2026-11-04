//! Step 0 and step 5 of the factory test: the radios listen while the operator
//! works through the front panel, then the report says what they heard, with
//! the battery and a few automatic board checks.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use super::radios;
use crate::libs::display::supervise::lock_recover;
use crate::libs::power::SharedPowerStatus;

/// How long the BLE discovery listens.
const BLE_LISTEN: Duration = Duration::from_secs(20);
/// How long BlueZ may take on top of the listening before the scan fails.
const BLE_SLACK: Duration = Duration::from_secs(8);

/// One radio's result.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RadioResult {
    /// `pending`, `pass`, `fail` or `n/a` (not testable on this Hub).
    pub status: String,
    /// Networks, devices or frames heard.
    pub count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl RadioResult {
    fn pending() -> Self {
        RadioResult {
            status: "pending".into(),
            ..Default::default()
        }
    }

    fn counted(count: u64, note: Option<String>) -> Self {
        let status = if count > 0 { "pass" } else { "fail" };
        RadioResult {
            status: status.into(),
            count,
            note,
        }
    }

    fn failed(note: String) -> Self {
        RadioResult {
            status: "fail".into(),
            count: 0,
            note: Some(note),
        }
    }

    /// For the 128 px wide LCD.
    pub fn short(&self) -> String {
        match self.status.as_str() {
            "pass" | "fail" => self.count.to_string(),
            "pending" => "wait".into(),
            other => other.to_string(),
        }
    }

    fn waiting(count: u64, note: String) -> Self {
        RadioResult {
            status: "pending".into(),
            count,
            note: Some(note),
        }
    }

    fn ok(&self) -> bool {
        self.status == "pass" || self.status == "n/a"
    }
}

/// What the background scans found so far.
#[derive(Debug, Clone, Serialize)]
pub struct ScanResults {
    pub wifi: RadioResult,
    pub ble: RadioResult,
    /// Northbridge counters when the test started; `None` when unreadable.
    #[serde(skip)]
    pub p2p_start: Option<P2pSnapshot>,
    /// When the P2P check started: `p2p_start` read, or the own listener started.
    #[serde(skip)]
    p2p_start_at: Option<std::time::Instant>,
    /// Packets the RAK5146 received, summed over the concentrator's stats
    /// reports that arrived during the test (each covers its own interval).
    pub lora_rx: u64,
    #[serde(skip)]
    lora_last_report: Option<String>,
    #[serde(skip)]
    watching: bool,
    /// The Northbridge as the test listens on it itself, when no P2P
    /// central owns its UART (a Hub not provisioned yet).
    #[serde(skip)]
    nb_direct: Option<radios::Shared>,
    /// The RAK5146 through the test's own concentratord, when LoRaWAN is not
    /// running yet.
    #[serde(skip)]
    lora_direct: Option<radios::Shared>,
    #[serde(skip)]
    stop: radios::Stop,
}

impl Default for ScanResults {
    fn default() -> Self {
        ScanResults {
            wifi: RadioResult::pending(),
            ble: RadioResult::pending(),
            p2p_start: None,
            p2p_start_at: None,
            lora_rx: 0,
            lora_last_report: None,
            watching: true,
            nb_direct: None,
            lora_direct: None,
            stop: radios::Stop::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Battery {
    pub vbat_mv: u16,
    pub percent: u8,
    pub vin_mv: u16,
    pub on_dc: bool,
}

/// One automatic board check.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub wifi: RadioResult,
    pub ble: RadioResult,
    pub p2p: RadioResult,
    /// The RAK5146 LoRaWAN concentrator; `n/a` on a Hub without one.
    pub lorawan: RadioResult,
    /// How the Northbridge was set up and what its counters did, so a P2P
    /// fail can be told apart: wrong channel, receiver off, nothing on air.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub p2p_radio: Value,
    pub battery: Battery,
    pub checks: Vec<Check>,
}

impl Report {
    /// False while the P2P check still waits for frames; the rest is final.
    pub fn complete(&self) -> bool {
        self.p2p.status != "pending"
    }

    pub fn passed(&self) -> bool {
        self.wifi.ok()
            && self.ble.ok()
            && self.p2p.ok()
            && self.lorawan.ok()
            && self.checks.iter().all(|c| c.ok)
    }
}

/// Starts the listening: a WiFi scan, a BLE discovery and the Northbridge
/// counter baseline.
pub fn start(results: Arc<Mutex<ScanResults>>) {
    let stop = lock_recover(&results).stop.clone();

    // P2P: the running central's counters, or the Northbridge itself.
    let p2p = read_p2p().ok();
    if p2p.as_ref().is_some_and(|p| p.state != "disabled") {
        let mut r = lock_recover(&results);
        r.p2p_start = p2p;
        r.p2p_start_at = Some(std::time::Instant::now());
    } else {
        let nb = radios::Shared::default();
        {
            let mut r = lock_recover(&results);
            r.nb_direct = Some(nb.clone());
            r.p2p_start_at = Some(std::time::Instant::now());
        }
        let stop = stop.clone();
        thread::spawn(move || radios::listen_northbridge(nb, stop));
    }

    // LoRaWAN: the running concentrator's stats, or a concentratord of our own.
    let lorawan = read_lorawan();
    let running = lorawan
        .as_ref()
        .and_then(|d| d.get("state"))
        .and_then(Value::as_str)
        .is_some_and(|s| s != "disabled");
    if running {
        // The report already in the file predates the test.
        lock_recover(&results).lora_last_report = lorawan.as_ref().and_then(lora_report_at);
        let w = results.clone();
        thread::spawn(move || watch_lorawan(w));
    } else if rak_on_usb() {
        let lora = radios::Shared::default();
        lock_recover(&results).lora_direct = Some(lora.clone());
        thread::spawn(move || radios::listen_concentrator(lora, stop));
    }

    let r = results.clone();
    thread::spawn(move || {
        let wifi = scan_wifi();
        lock_recover(&r).wifi = wifi;
    });

    thread::spawn(move || {
        let ble = scan_ble();
        lock_recover(&results).ble = ble;
    });
}

fn scan_wifi() -> RadioResult {
    // nmcli rescans when its list is older than 30 s, so one call is enough.
    let networks = crate::libs::network::wifi::scan_wifi();
    let unique: HashSet<_> = networks.iter().map(|n| n.ssid.clone()).collect();
    RadioResult::counted(unique.len() as u64, Some("networks (unique SSIDs)".into()))
}

fn scan_ble() -> RadioResult {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return RadioResult::failed(format!("tokio runtime: {e}")),
    };
    let scan = async {
        use futures::StreamExt;
        let session = bluer::Session::new().await.map_err(|e| e.to_string())?;
        let adapter = session.default_adapter().await.map_err(|e| e.to_string())?;
        // A discovery session of our own beside the Beacon scanner: BlueZ
        // merges them, and devices it already knows count as well.
        let mut seen: HashSet<bluer::Address> = HashSet::new();
        let events = adapter
            .discover_devices()
            .await
            .map_err(|e| e.to_string())?;
        tokio::pin!(events);
        let deadline = tokio::time::sleep(BLE_LISTEN);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                ev = events.next() => match ev {
                    Some(bluer::AdapterEvent::DeviceAdded(addr)) => { seen.insert(addr); }
                    Some(_) => {}
                    None => break,
                },
            }
        }
        for addr in adapter
            .device_addresses()
            .await
            .map_err(|e| e.to_string())?
        {
            if let Ok(dev) = adapter.device(addr) {
                if matches!(dev.rssi().await, Ok(Some(_))) {
                    seen.insert(addr);
                }
            }
        }
        Ok::<_, String>(seen.len() as u64)
    };
    rt.block_on(async {
        tokio::time::timeout(BLE_LISTEN + BLE_SLACK, scan)
            .await
            .unwrap_or_else(|_| {
                Err("BlueZ did not answer (discovery never started or stalled)".into())
            })
    })
    .map(|n| RadioResult::counted(n, Some("devices".into())))
    .unwrap_or_else(|e| RadioResult::failed(format!("BLE: {e}")))
}

/// The Northbridge counters that prove the receiver heard something.
#[derive(Debug, Clone, PartialEq)]
pub struct P2pSnapshot {
    pub state: String,
    /// Frames of any kind: for us, for others, and corrupted ones.
    pub heard: u64,
    /// The radio's state and counters, for the report (`p2p_radio`).
    pub detail: Value,
}

pub fn parse_p2p(status: &Value) -> Result<P2pSnapshot, String> {
    let nb = status
        .get("northbridge")
        .ok_or("no northbridge status yet")?;
    let state = nb
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let c = nb.get("counters");
    let n = |k: &str| {
        c.and_then(|c| c.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let heard = n("rx") + n("crc_err") + n("radio_hdr_err");
    let counters: serde_json::Map<String, Value> = [
        "rx",
        "crc_err",
        "radio_hdr_err",
        "rx_not_for_us",
        "rx_fresh",
        "tx",
    ]
    .iter()
    .map(|k| (k.to_string(), json!(n(k))))
    .collect();
    let detail = json!({
        "state": state,
        "fw_version": nb.get("fw_version"),
        "radio": nb.get("radio"),
        "net_id": nb.pointer("/gateway/net_id"),
        "peers": nb.pointer("/gateway/peers"),
        "last_error": nb.get("last_error"),
        "err_flags": nb.get("err_flags"),
        "counters": counters,
    });
    Ok(P2pSnapshot {
        state,
        heard,
        detail,
    })
}

/// Where `proximos-control` mirrors its radio status documents
/// (`plan/control/radio/northbridge_diagnostics.md` §3, §4a).
const RUN_DIR: &str = "/run/proximos/radio";

fn read_run_document(name: &str) -> Option<Value> {
    let raw = std::fs::read(Path::new(RUN_DIR).join(name)).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn read_p2p() -> Result<P2pSnapshot, String> {
    let doc = read_run_document("northbridge.json")
        .ok_or("no Northbridge status (is proximos-control running?)")?;
    parse_p2p(&json!({ "northbridge": doc }))
}

fn read_lorawan() -> Option<Value> {
    read_run_document(crate::libs::radio_gate::LORAWAN_RUN_DOCUMENT)
}

fn lora_report_at(doc: &Value) -> Option<String> {
    doc.pointer("/concentratord/last_stats")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Adds each new concentrator stats report to the running total until the
/// report is taken. The reports come every 30 s; polling at 5 s misses none.
fn watch_lorawan(results: Arc<Mutex<ScanResults>>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30 * 60);
    while std::time::Instant::now() < deadline {
        if let Some(doc) = read_lorawan() {
            let mut r = lock_recover(&results);
            if !r.watching {
                return;
            }
            let at = lora_report_at(&doc);
            if at.is_some() && at != r.lora_last_report {
                r.lora_rx += doc
                    .pointer("/concentratord/stats/rx_received")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                r.lora_last_report = at;
            }
        } else if !lock_recover(&results).watching {
            return;
        }
        thread::sleep(Duration::from_secs(5));
    }
}

/// The RAK5146-USB's bridge as the concentratord udev rule matches it.
fn rak_on_usb() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return false;
    };
    dir.flatten().any(|d| {
        let id = |f: &str| read_trim(&d.path().join(f).to_string_lossy());
        id("idVendor").as_deref() == Some("0483") && id("idProduct").as_deref() == Some("5740")
    })
}

pub fn lorawan_result(doc: Option<&Value>, on_usb: bool, rx: u64) -> RadioResult {
    let state = doc.and_then(|d| d.get("state")).and_then(Value::as_str);
    match state {
        Some("disabled") | None if !on_usb => RadioResult {
            status: "n/a".into(),
            count: 0,
            note: Some("no RAK5146 on USB".into()),
        },
        None => RadioResult::failed("RAK5146 on USB but no LoRaWAN status".into()),
        Some("disabled") => {
            RadioResult::failed("RAK5146 on USB but the radio mode runs no LoRaWAN".into())
        }
        Some(state) => {
            let d = doc.expect("state came from it");
            if d.pointer("/card/present").and_then(Value::as_bool) != Some(true) {
                return RadioResult::failed(format!("concentrator card missing ({state})"));
            }
            if d.pointer("/concentratord/last_stats").is_none() {
                return RadioResult::failed(format!("no concentrator stats yet ({state})"));
            }
            RadioResult::counted(rx, Some(format!("packets, concentrator {state}")))
        }
    }
}

/// How long the P2P check waits for a frame, from the start of the test.
/// `proximos-control` reads the Northbridge's counters every 30 s on a quiet
/// link (`IDLE_PROBE_AFTER_S`), so this is four readings; the station keeps
/// a transmitter near the Hub, which sends more often than that.
pub const P2P_WAIT: Duration = Duration::from_secs(120);
/// How often the P2P check looks at the status document while it waits.
const P2P_POLL: Duration = Duration::from_secs(2);

pub fn p2p_result(start: Option<&P2pSnapshot>, now: Result<P2pSnapshot, String>) -> RadioResult {
    match now {
        Err(e) => RadioResult::failed(e),
        // Every Hub has a Northbridge, but the lorawan mode leaves it off.
        Ok(now) if now.state == "disabled" => RadioResult {
            status: "n/a".into(),
            count: 0,
            note: Some("Northbridge off: the radio mode runs no P2P".into()),
        },
        Ok(now) => {
            // Counters restart with the Northbridge; then the whole count is new.
            let base = start
                .map(|s| s.heard)
                .filter(|b| *b <= now.heard)
                .unwrap_or(0);
            RadioResult::counted(
                now.heard - base,
                Some(format!("frames, Northbridge {}", now.state)),
            )
        }
    }
}

/// A listener of the test's own: it must have been listening, and heard one.
pub fn direct_result(l: &radios::Listen, unit: &str) -> RadioResult {
    let ident = l
        .ident
        .as_deref()
        .map(|i| format!(" ({i})"))
        .unwrap_or_default();
    match l.state.as_str() {
        "listening" | "stopped" => RadioResult::counted(l.heard, Some(format!("{unit}{ident}"))),
        "error" => RadioResult::failed(l.error.clone().unwrap_or_else(|| "error".into())),
        _ => RadioResult::failed("the receiver never started listening".into()),
    }
}

/// Stops the test's own listeners (the test ended before its report).
pub fn stop(results: &Arc<Mutex<ScanResults>>) {
    lock_recover(results).stop.set();
}

/// Waits for the scans still running (at most the BLE window), then reads
/// the rest. Everything but P2P goes to `publish` at once, with P2P
/// `pending`; P2P then waits until a frame was heard, or [`P2P_WAIT`] from
/// the start of the test, publishing its count at every poll.
pub fn report(
    results: &Arc<Mutex<ScanResults>>,
    power: &SharedPowerStatus,
    accel_i2c: &str,
    publish: impl Fn(Report),
) -> Report {
    for _ in 0..(BLE_LISTEN.as_secs() + 10) {
        let r = lock_recover(results);
        if r.wifi.status != "pending" && r.ble.status != "pending" {
            break;
        }
        drop(r);
        thread::sleep(Duration::from_secs(1));
    }
    let scan = {
        let mut r = lock_recover(results);
        r.watching = false;
        let r = &mut *r;
        for (name, res) in [("WiFi", &mut r.wifi), ("BLE", &mut r.ble)] {
            if res.status == "pending" {
                *res = RadioResult::failed(format!("{name} scan did not finish"));
            }
        }
        r.clone()
    };
    let lorawan = match &scan.lora_direct {
        Some(lora) => direct_result(&lock_recover(lora), "packets"),
        None => lorawan_result(read_lorawan().as_ref(), rak_on_usb(), scan.lora_rx),
    };
    let battery = {
        let p = lock_recover(power);
        Battery {
            vbat_mv: p.vbat_mv,
            percent: p.battery_percent,
            vin_mv: p.vin_mv,
            on_dc: p.on_dc_power,
        }
    };
    let mut report = Report {
        wifi: scan.wifi.clone(),
        ble: scan.ble.clone(),
        p2p: RadioResult::waiting(0, "waiting for P2P frames".into()),
        lorawan,
        p2p_radio: Value::Null,
        battery,
        checks: board_checks(accel_i2c),
    };
    publish(report.clone());
    let deadline = scan.p2p_start_at.unwrap_or_else(std::time::Instant::now) + P2P_WAIT;
    loop {
        let (p2p, radio, may_still_hear) = p2p_now(&scan);
        let last = std::time::Instant::now() + P2P_POLL > deadline;
        let done = p2p.status != "fail" || !may_still_hear || last;
        report.p2p = if done {
            p2p
        } else {
            RadioResult::waiting(p2p.count, "waiting for P2P frames".into())
        };
        report.p2p_radio = radio;
        if done {
            break;
        }
        publish(report.clone());
        thread::sleep(P2P_POLL);
    }
    // The listeners stop on their next poll and hand the radios back.
    scan.stop.set();
    report
}

/// The P2P result and radio detail as they stand now, and whether a frame
/// may still come (the receiver runs); a `fail` that may still turn is what
/// the waiting in [`report`] shows as `pending`.
fn p2p_now(scan: &ScanResults) -> (RadioResult, Value, bool) {
    match &scan.nb_direct {
        Some(nb) => {
            let l = lock_recover(nb).clone();
            let radio = json!({
                "source": "factory listener",
                "channel": radios::FACTORY_CHANNEL,
                "ident": l.ident,
                "state": l.state,
                "heard": l.heard,
                "error": l.error,
            });
            let alive = l.state != "error";
            (direct_result(&l, "frames"), radio, alive)
        }
        None => {
            let now = read_p2p();
            let alive = now.is_ok();
            let radio = json!({
                "source": "proximos-control",
                "start": scan.p2p_start.as_ref().map(|s| s.detail.clone()),
                "end": now.as_ref().map(|s| s.detail.clone()).unwrap_or_else(|e| json!({ "error": e })),
            });
            (p2p_result(scan.p2p_start.as_ref(), now), radio, alive)
        }
    }
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Cheap checks that need no operator.
fn board_checks(accel_i2c: &str) -> Vec<Check> {
    let mut checks = Vec::new();

    let carrier = read_trim("/sys/class/net/eth0/carrier").as_deref() == Some("1");
    let speed = read_trim("/sys/class/net/eth0/speed").unwrap_or_default();
    checks.push(Check {
        name: "ethernet",
        ok: carrier,
        detail: if carrier {
            format!("link up, {speed} Mb/s")
        } else {
            "no link".into()
        },
    });

    let rtc = read_trim("/sys/class/rtc/rtc0/since_epoch").and_then(|s| s.parse::<u64>().ok());
    checks.push(Check {
        name: "rtc",
        ok: rtc.is_some(),
        detail: match rtc {
            Some(t) => format!("reads {t} s since epoch"),
            None => "/dev/rtc0 not readable".into(),
        },
    });

    let accel = crate::drivers::lis2dh12::who_am_i(accel_i2c);
    checks.push(Check {
        name: "accelerometer",
        ok: accel == Ok(0x33),
        detail: match accel {
            Ok(id) => format!("WHO_AM_I 0x{id:02X}"),
            Err(e) => e,
        },
    });

    let probes = std::fs::read_dir("/sys/bus/w1/devices")
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("28-"))
                .count()
        })
        .unwrap_or(0);
    // Information only: a bare board has no probes plugged in.
    checks.push(Check {
        name: "ds18b20",
        ok: true,
        detail: format!("{probes} probes found"),
    });

    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &str, rx: u64, crc: u64) -> Value {
        json!({"northbridge": {"state": state, "counters": {"rx": rx, "crc_err": crc}}})
    }

    #[test]
    fn the_p2p_detail_carries_channel_and_counters() {
        let doc = json!({"northbridge": {"state": "alive", "fw_version": "0.4.0",
            "radio": {"freq_hz": 869525000, "sf": 7, "state": "gateway"},
            "gateway": {"net_id": 5, "peers": 0},
            "counters": {"rx": 3, "crc_err": 1, "rx_not_for_us": 2}}});
        let d = parse_p2p(&doc).unwrap().detail;
        assert_eq!(d["radio"]["freq_hz"], 869525000);
        assert_eq!(d["net_id"], 5);
        assert_eq!(d["counters"]["rx_not_for_us"], 2);
        assert_eq!(d["counters"]["radio_hdr_err"], 0);
    }

    #[test]
    fn p2p_counts_frames_heard_since_the_start() {
        let start = parse_p2p(&status("alive", 10, 2)).unwrap();
        let r = p2p_result(Some(&start), parse_p2p(&status("alive", 13, 4)));
        assert_eq!((r.status.as_str(), r.count), ("pass", 5));
        let r = p2p_result(Some(&start), parse_p2p(&status("alive", 10, 2)));
        assert_eq!(r.status, "fail");
    }

    #[test]
    fn p2p_after_a_northbridge_restart_counts_everything() {
        let start = parse_p2p(&status("alive", 100, 0)).unwrap();
        let r = p2p_result(Some(&start), parse_p2p(&status("alive", 3, 0)));
        assert_eq!(r.count, 3);
    }

    #[test]
    fn p2p_is_not_applicable_in_lorawan_mode() {
        let r = p2p_result(None, parse_p2p(&status("disabled", 0, 0)));
        assert_eq!(r.status, "n/a");
        assert!(r.ok());
    }

    #[test]
    fn lorawan_needs_the_card_its_stats_and_a_packet() {
        let doc = |present: bool, stats: bool| {
            let mut d = json!({"state": "alive", "card": {"present": present},
                               "concentratord": {}});
            if stats {
                d["concentratord"]["last_stats"] = json!("2026-10-12T10:00:00Z");
            }
            d
        };
        assert_eq!(
            lorawan_result(Some(&doc(true, true)), true, 4).status,
            "pass"
        );
        assert_eq!(
            lorawan_result(Some(&doc(true, true)), true, 0).status,
            "fail"
        );
        assert_eq!(
            lorawan_result(Some(&doc(true, false)), true, 0).status,
            "fail"
        );
        assert_eq!(
            lorawan_result(Some(&doc(false, true)), false, 9).status,
            "fail"
        );
    }

    #[test]
    fn lorawan_without_a_card_is_not_applicable_but_a_card_left_off_fails() {
        let off = json!({"state": "disabled"});
        assert_eq!(lorawan_result(Some(&off), false, 0).status, "n/a");
        assert_eq!(lorawan_result(None, false, 0).status, "n/a");
        assert_eq!(lorawan_result(Some(&off), true, 0).status, "fail");
        assert_eq!(lorawan_result(None, true, 0).status, "fail");
    }

    #[test]
    fn an_own_listener_passes_only_once_it_listened_and_heard() {
        let l = |state: &str, heard| radios::Listen {
            state: state.into(),
            heard,
            ..Default::default()
        };
        assert_eq!(direct_result(&l("listening", 2), "frames").status, "pass");
        assert_eq!(direct_result(&l("listening", 0), "frames").status, "fail");
        assert_eq!(direct_result(&l("starting", 5), "frames").status, "fail");
        assert_eq!(direct_result(&l("error", 5), "frames").status, "fail");
    }

    #[test]
    fn a_report_waiting_for_p2p_is_not_complete() {
        let w = RadioResult::waiting(0, "waiting for P2P frames".into());
        assert_eq!(w.status, "pending");
        assert_eq!(w.short(), "wait");
        let mut r = Report {
            wifi: RadioResult::counted(3, None),
            ble: RadioResult::counted(2, None),
            p2p: w,
            lorawan: RadioResult::counted(1, None),
            p2p_radio: Value::Null,
            battery: Battery {
                vbat_mv: 4000,
                percent: 90,
                vin_mv: 0,
                on_dc: false,
            },
            checks: vec![],
        };
        assert!(!r.complete());
        assert!(!r.passed());
        r.p2p = RadioResult::counted(5, None);
        assert!(r.complete() && r.passed());
    }

    #[test]
    fn p2p_without_status_fails() {
        assert!(parse_p2p(&json!({})).is_err());
        assert_eq!(p2p_result(None, Err("x".into())).status, "fail");
    }
}
