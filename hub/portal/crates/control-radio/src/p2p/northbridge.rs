//! The northbridge status document — `plan/control/radio/northbridge_diagnostics.md` §3.
//!
//! The hosted P2P central (PI1) feeds a [`NorthbridgeStatus`] from everything the modem tells
//! it — `GET_INFO`, `EVT_BOOT`, `EVT_LOG` faults, refused commands, a dead link — and the host
//! publishes it retained on `proximos/{hostname}/radio/northbridge`. One handle is shared by
//! the central and the host, like [`crate::p2p::runtime::Liveness`], and outlives the
//! supervisor's restarts of the central.
//!
//! The Northbridge runs the TOWER gateway image and ships together with the Hub software
//! (`p2p_tower_gateway.md` §1, flag day), so its `GET_INFO` is one frozen layout
//! ([`ModemInfo`]). Before the first answer the document carries only what the host knows.

use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::p2p::modem::{BootEvent, FaultEvent, RadioConfig};
use crate::p2p::modem_io::{build_flags, gateway_state, ModemInfo, COUNTER_NAMES};

/// The document's topic for one Hub.
pub fn topic(hostname: &str) -> String {
    format!("{}/{hostname}/radio/northbridge", crate::northbound::ROOT)
}

/// §3's state vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The task is up and the modem has not answered its first `GET_INFO`.
    Starting,
    /// The modem answers and reports nothing wrong.
    Alive,
    /// The modem answers but reports a fault.
    Error,
    /// The UART cannot be opened or the modem does not answer.
    Unreachable,
    /// `radio.mode` does not allow P2P.
    Disabled,
}

impl State {
    pub const fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Alive => "alive",
            State::Error => "error",
            State::Unreachable => "unreachable",
            State::Disabled => "disabled",
        }
    }
}

/// Where an error came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The modem said so: an `EVT_LOG` fault, a `RADIO_ERR`, a refused command.
    Modem,
    /// The link to it: the UART, a command with no answer.
    Link,
}

impl Source {
    const fn as_str(self) -> &'static str {
        match self {
            Source::Modem => "modem",
            Source::Link => "link",
        }
    }
}

/// The most recent error, kept until the northbridge is healthy again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastError {
    pub at: i64,
    pub source: Source,
    /// The §2.2 code, when the modem sent one.
    pub code: Option<u8>,
    pub message: String,
}

/// Decode `EVT_BOOT`'s `reset_cause` — Zephyr's `RESET_*` bitmask truncated to 8 bits. More
/// than one bit can be set (power-on plus pin on a first power-up).
pub fn reset_causes(mask: u8) -> Vec<&'static str> {
    const NAMES: [&str; 8] = [
        "pin",
        "software",
        "brownout",
        "por",
        "watchdog",
        "debug",
        "security",
        "low_power_wake",
    ];
    NAMES
        .iter()
        .enumerate()
        .filter(|(bit, _)| mask & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// §2.1's sticky `err_flags`, by bit.
pub fn err_flags(mask: u32) -> Vec<&'static str> {
    const NAMES: [&str; 6] = [
        "radio_init_failed",
        "rx_rearm_failed",
        "tx_radio_error",
        "link_frame_error",
        "uart_overrun",
        "tx_internal_error",
    ];
    NAMES
        .iter()
        .enumerate()
        .filter(|(bit, _)| mask & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// `GET_INFO`'s `build_flags`, by name.
pub fn build_flag_names(flags: u8) -> Vec<&'static str> {
    let mut v = Vec::new();
    if flags & build_flags::BENCH_RTT_BRIDGE != 0 {
        v.push("bench_rtt_bridge");
    }
    if flags & build_flags::DEBUG != 0 {
        v.push("debug");
    }
    v
}

/// The counters the document has always carried under a shorter name: `rx`, `tx`, `crc_err`.
const COUNTER_ALIASES: [(&str, &str); 3] = [
    ("rx_total", "rx"),
    ("tx_total", "tx"),
    ("radio_crc_err", "crc_err"),
];

/// A counter's key in the document: its `GET_INFO` name, or the shorter one it always had.
fn counter_key(name: &'static str) -> &'static str {
    COUNTER_ALIASES
        .iter()
        .find(|(full, _)| *full == name)
        .map_or(name, |(_, short)| *short)
}

/// §2.2's error codes, for the document's message when the modem sent no text of its own.
pub fn fault_code_name(code: u8) -> &'static str {
    match code {
        1 => "radio init failed",
        2 => "RX re-arm failed",
        3 => "TX radio error",
        4 => "link framing errors",
        5 => "UART overrun",
        6 => "stack/heap near limit",
        _ => "unknown fault",
    }
}

/// The firmware update's part of the document (`northbridge_update.md` §3.5), from the P2P
/// task's `update-state.json` ([`crate::p2p::nb_update`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateSummary {
    /// `idle`, `flashing` or `failed`; `failed` is a Hub alarm in the Portal.
    pub state: &'static str,
    /// The image in the Hub's bundle, `x.y.z+sha8`; `None` when the image carries none.
    pub bundled: Option<String>,
    /// Attempts spent on the current (image, boot) pair.
    pub attempts: u32,
    pub last_error: Option<String>,
    /// Something the operator should know that is not a failure: an image that needs the
    /// one-time J-Link migration (§4), a hotfix kept over the bundled image.
    pub note: Option<String>,
}

/// What the document holds. Built up from the modem's reports; see [`NorthbridgeStatus`].
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: Option<State>,
    pub uart: Option<String>,
    pub info: Option<ModemInfo>,
    /// When the current `info` was read, Unix seconds.
    pub info_at: Option<i64>,
    pub boot: Option<(u8, i64)>,
    pub radio: Option<RadioConfig>,
    pub last_seen: Option<i64>,
    pub tx_refused: u32,
    /// The central's ring receiver (`p2p_tower_gateway.md` §8.6): gap relearns, the `seq`s
    /// they skipped, and how many of those had arrived and been dropped as out of order.
    pub evt_relearn: u64,
    pub evt_seq_gap: u64,
    pub evt_seq_gap_seen: u64,
    /// The northbound outbox: messages waiting for the broker, and events refused (not
    /// acked) because it was full.
    pub outbox_pending: u64,
    pub outbox_overflow: u64,
    /// Uplinks stamped with the time the central handled them, not the time the gateway heard
    /// them: no clock anchor, or one that maps ahead of the host clock (§8.8 D).
    pub rx_time_fallback: u64,
    /// `EVT_TWR_UPLINK`s with the reserved `ack` 5, acked and dropped unpublished.
    pub evt_reserved_ack: u64,
    /// `LinkCheckReq` / `TimeReq` uplinks left to the Northbridge's ACK (§10.4).
    pub ack_ctrl_req: u64,
    /// `TimeReq` uplinks whose ACK carried no `TimeAns` (`ack` bit 6 clear): the Northbridge
    /// had no `TWR_TIME_SYNC` anchor (§13.5).
    pub ack_time_missed: u64,
    /// `TWR_TIME_SYNC`s the Northbridge took, and those refused or unanswered (§13.5).
    pub time_sync_sent: u64,
    pub time_sync_failed: u64,
    pub last_error: Option<LastError>,
    /// When the last modem-reported fault arrived, for §3's "error in the last interval".
    pub last_fault_at: Option<i64>,
    /// The firmware update; `None` when the image carries no Northbridge firmware.
    pub update: Option<UpdateSummary>,
    /// `radio.mode` runs no P2P task, but the mode-independent Northbridge task reads the
    /// firmware and answers `northbridge-update` (#158, `northbridge_update.md` amendment
    /// 2026-10-28): the `disabled` document then carries the version, `update` and
    /// `nb_task: "idle"`.
    pub idle_task: bool,
}

/// How long a modem-reported fault keeps the state at `error` — one health interval.
const FAULT_HOLD_S: i64 = 60;

impl Status {
    /// §3's state, from what is known at `now`. `link_alive` is the central's `Liveness`.
    fn derived_state(&self, now: i64, link_alive: bool) -> State {
        match self.state {
            Some(State::Disabled) => return State::Disabled,
            Some(State::Unreachable) => return State::Unreachable,
            None | Some(State::Starting) if self.info.is_none() => return State::Starting,
            _ => {}
        }
        if !link_alive {
            return State::Unreachable;
        }
        // The modem's own most recent error, aged by the time since it said so.
        let info_age = self.info_at.map_or(0, |t| now.saturating_sub(t).max(0));
        let modem_error = self.info.is_some_and(|i| {
            i.last_err_code != 0
                && i64::from(i.last_err_age_s).saturating_add(info_age) <= FAULT_HOLD_S
        });
        let recent_fault = self
            .last_fault_at
            .is_some_and(|t| now.saturating_sub(t) <= FAULT_HOLD_S);
        if modem_error || recent_fault || self.state == Some(State::Error) {
            State::Error
        } else {
            State::Alive
        }
    }

    /// The §3 JSON document at `now`.
    pub fn document(&self, now: i64, link_alive: bool) -> Value {
        let state = self.derived_state(now, link_alive);
        let mut doc = Map::new();
        doc.insert("timestamp".into(), json!(rfc3339(now)));
        doc.insert("state".into(), json!(state.as_str()));
        // The keys the Portal's Hub radio card reads are always present, with a defined value
        // (planner 2026-09-28, `northbridge_diagnostics.md` §3): `null` for what is not known
        // yet, `0` for a counter before the first `GET_INFO`. `proto_version` is `0`: the flag
        // day has no link version (p2p_tower_gateway.md §6.1).
        // A disabled radio claims nothing: every card key, but at `null` / `0` — unless the
        // idle Northbridge task (#158) read the firmware, which is then reported.
        let known = self
            .info
            .filter(|_| state != State::Disabled || self.idle_task);
        doc.insert("proto_version".into(), json!(0));
        doc.insert(
            "fw_version".into(),
            known.map_or(Value::Null, |i| {
                json!(format!("{}.{}.{}", i.fw[0], i.fw[1], i.fw[2]))
            }),
        );
        // The uptime ages with the clock between reads, rather than freezing at the last one.
        doc.insert(
            "uptime_s".into(),
            known.map_or(Value::Null, |i| {
                let since = self
                    .info_at
                    .map_or(0, |t| u64::try_from(now.saturating_sub(t)).unwrap_or(0));
                json!(i.now_ms / 1000 + since)
            }),
        );
        let counters_now = known.map_or([0; 25], |i| i.counters);
        let mut counters = Map::new();
        for (name, value) in COUNTER_NAMES.iter().zip(counters_now) {
            counters.insert(counter_key(name).into(), json!(value));
        }
        // The central's own count of refused `TX_SCHEDULE`s (the JoinAccept), not a modem one.
        counters.insert("tx_refused".into(), json!(self.tx_refused));
        // The central's own delivery counters (p2p_tower_gateway.md §8.6), cumulative per
        // process, and the outbox gauge.
        counters.insert("evt_relearn".into(), json!(self.evt_relearn));
        counters.insert("evt_seq_gap".into(), json!(self.evt_seq_gap));
        counters.insert("evt_seq_gap_seen".into(), json!(self.evt_seq_gap_seen));
        counters.insert("outbox_pending".into(), json!(self.outbox_pending));
        counters.insert("outbox_overflow".into(), json!(self.outbox_overflow));
        counters.insert("rx_time_fallback".into(), json!(self.rx_time_fallback));
        counters.insert("evt_reserved_ack".into(), json!(self.evt_reserved_ack));
        // The Northbridge's ACK-tail counters (GET_INFO 180 B, §3): `null` when not reported.
        let ack_ctrl = known.and_then(|i| i.ack_ctrl);
        counters.insert(
            "ack_ctrl_sent".into(),
            ack_ctrl.map_or(Value::Null, |a| json!(a.sent)),
        );
        counters.insert(
            "ack_time_no_anchor".into(),
            ack_ctrl.map_or(Value::Null, |a| json!(a.time_no_anchor)),
        );
        counters.insert("ack_ctrl_req".into(), json!(self.ack_ctrl_req));
        counters.insert("ack_time_missed".into(), json!(self.ack_time_missed));
        counters.insert("time_sync_sent".into(), json!(self.time_sync_sent));
        counters.insert("time_sync_failed".into(), json!(self.time_sync_failed));
        doc.insert("counters".into(), Value::Object(counters));
        // What the modem holds beats what the host last pushed; before the first
        // `SET_RADIO_CONFIG` (state `boot`) the modem holds nothing worth showing.
        let held = self
            .info
            .filter(|i| i.state != gateway_state::BOOT)
            .map(|i| i.radio);
        let shown = held.or(self.radio.filter(|_| state != State::Disabled));
        let mut radio = Map::new();
        radio.insert(
            "state".into(),
            known
                .filter(|_| state != State::Disabled)
                .map_or(Value::Null, |i| json!(gateway_state::name(i.state))),
        );
        radio.insert(
            "freq_hz".into(),
            shown.map_or(Value::Null, |r| json!(r.freq_hz)),
        );
        radio.insert("sf".into(), shown.map_or(Value::Null, |r| json!(r.sf)));
        radio.insert(
            "tx_power_dbm".into(),
            shown.map_or(Value::Null, |r| json!(r.tx_power_dbm)),
        );
        doc.insert("radio".into(), Value::Object(radio));
        // The firmware update (`northbridge_update.md` §3.5): always present, `null` when the
        // image carries no Northbridge firmware and nothing was asked for.
        doc.insert(
            "update".into(),
            self.update.as_ref().map_or(Value::Null, |u| {
                json!({
                    "state": u.state,
                    "bundled": u.bundled,
                    "attempts": u.attempts,
                    "last_error": u.last_error,
                    "note": u.note,
                })
            }),
        );
        if state == State::Disabled {
            if self.idle_task {
                doc.insert("nb_task".into(), json!("idle"));
            }
            return Value::Object(doc);
        }
        if let Some(uart) = &self.uart {
            doc.insert("uart".into(), json!(uart));
        }
        if let Some(info) = self.info {
            doc.insert("git_sha".into(), json!(hex(&info.git_sha)));
            doc.insert(
                "build_flags".into(),
                json!(build_flag_names(info.build_flags)),
            );
            doc.insert("err_flags".into(), json!(err_flags(info.err_flags)));
            doc.insert("gateway".into(), gateway_document(&info));
        }
        if let Some(t) = self.last_seen {
            doc.insert("last_seen".into(), json!(rfc3339(t)));
        }
        // A boot the host saw beats the cause the modem kept for the current boot. A missed
        // `EVT_BOOT` (the host was between runs, e.g. retrying a dead UART) still has a time:
        // the modem had been up `now_ms` when it answered the `GET_INFO` at `info_at`.
        let boot = self.boot.or_else(|| {
            let info = self.info?;
            let at = self.info_at.map_or(i64::MIN, |t| {
                t.saturating_sub(i64::try_from(info.now_ms / 1000).unwrap_or(0))
            });
            Some((info.reset_cause, at))
        });
        if let Some((mask, at)) = boot {
            let mut b = Map::new();
            b.insert("reset_cause".into(), json!(reset_causes(mask)));
            if at != i64::MIN {
                b.insert("at".into(), json!(rfc3339(at)));
            }
            doc.insert("boot".into(), Value::Object(b));
        }
        doc.insert(
            "last_error".into(),
            match &self.last_error {
                Some(e) => {
                    let mut m = Map::new();
                    m.insert("at".into(), json!(rfc3339(e.at)));
                    m.insert("source".into(), json!(e.source.as_str()));
                    if let Some(c) = e.code {
                        m.insert("code".into(), json!(c));
                    }
                    m.insert("message".into(), json!(e.message));
                    Value::Object(m)
                }
                None => Value::Null,
            },
        );
        Value::Object(doc)
    }
}

/// The shared handle. Cheap to clone; every clone sees the same status.
#[derive(Clone, Debug, Default)]
pub struct NorthbridgeStatus {
    inner: Arc<Mutex<Status>>,
    /// Bumped on every change, so the host publishes on change rather than on a timer alone.
    version: Arc<std::sync::atomic::AtomicU64>,
}

impl NorthbridgeStatus {
    fn update(&self, f: impl FnOnce(&mut Status)) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard);
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// A counter that changes whenever the status does.
    pub fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A copy of the current status.
    pub fn snapshot(&self) -> Status {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The task is (re)starting on `uart`.
    pub fn starting(&self, uart: &str) {
        self.update(|s| {
            s.state = Some(State::Starting);
            s.uart = Some(uart.to_string());
        });
    }

    /// The modem is being flashed and will start over: `starting`, what a local Northbridge
    /// shows meanwhile, and not `unreachable`.
    pub fn restarting(&self) {
        self.update(|s| s.state = Some(State::Starting));
    }

    /// `radio.mode` does not allow P2P.
    pub fn disabled(&self) {
        self.update(|s| {
            *s = Status {
                state: Some(State::Disabled),
                ..Status::default()
            }
        });
    }

    /// `radio.mode` does not allow P2P, and the idle Northbridge task (#158) holds the port
    /// between probes: `disabled`, keeping what it read and the `update` object.
    pub fn idle(&self) {
        self.update(|s| {
            s.state = Some(State::Disabled);
            s.idle_task = true;
        });
    }

    /// The link is gone: the UART would not open or a command went unanswered.
    pub fn unreachable(&self, now: i64, message: impl Into<String>) {
        self.update(|s| {
            s.state = Some(State::Unreachable);
            s.last_error = Some(LastError {
                at: now,
                source: Source::Link,
                code: None,
                message: message.into(),
            });
        });
    }

    /// A `GET_INFO` answer. Clears a link error: the modem is talking again.
    pub fn on_info(&self, info: ModemInfo, now: i64) {
        self.update(|s| {
            s.info = Some(info);
            s.info_at = Some(now);
            s.last_seen = Some(now);
            if matches!(s.state, Some(State::Starting | State::Unreachable) | None) {
                s.state = Some(State::Alive);
            }
            if s.last_error
                .as_ref()
                .is_some_and(|e| e.source == Source::Link)
            {
                s.last_error = None;
            }
            // A modem that reports a code of its own, and how long ago, is believed over the
            // host's record: it may have happened while nobody was listening.
            if info.last_err_code != 0 {
                let at = now.saturating_sub(i64::from(info.last_err_age_s));
                let newer = s.last_error.as_ref().is_none_or(|e| e.at < at);
                if newer {
                    s.last_error = Some(LastError {
                        at,
                        source: Source::Modem,
                        code: Some(info.last_err_code),
                        message: fault_code_name(info.last_err_code).to_string(),
                    });
                }
            }
        });
    }

    /// Any event from the modem — it is talking.
    pub fn on_event(&self, now: i64) {
        self.update(|s| s.last_seen = Some(now));
    }

    /// `EVT_BOOT`: the modem restarted and lost its state.
    pub fn on_boot(&self, boot: &BootEvent, now: i64) {
        self.update(|s| {
            s.boot = Some((boot.reset_cause, now));
            s.last_seen = Some(now);
        });
    }

    /// An `EVT_LOG` fault (§2.2).
    pub fn on_fault(&self, fault: &FaultEvent, now: i64) {
        self.update(|s| {
            let message = if fault.text.trim().is_empty() {
                fault_code_name(fault.code).to_string()
            } else {
                format!("{}: {}", fault_code_name(fault.code), fault.text.trim())
            };
            s.last_error = Some(LastError {
                at: now,
                source: Source::Modem,
                code: Some(fault.code),
                message,
            });
            if fault.level == crate::p2p::modem::FAULT_LEVEL_ERROR {
                s.last_fault_at = Some(now);
            }
            s.last_seen = Some(now);
        });
    }

    /// A command the modem refused, or a frame it could not send.
    pub fn on_modem_error(&self, now: i64, message: impl Into<String>, is_fault: bool) {
        self.update(|s| {
            s.last_error = Some(LastError {
                at: now,
                source: Source::Modem,
                code: None,
                message: message.into(),
            });
            if is_fault {
                s.last_fault_at = Some(now);
            }
        });
    }

    /// A `TX_SCHEDULE` the modem refused (`BUSY`, `LATE`, …): counted, not an error state.
    pub fn on_tx_refused(&self) {
        self.update(|s| s.tx_refused = s.tx_refused.saturating_add(1));
    }

    /// An uplink's `received_at` fell back to the time it was handled (§8.8 D).
    pub fn on_rx_time_fallback(&self) {
        self.update(|s| s.rx_time_fallback = s.rx_time_fallback.saturating_add(1));
    }

    /// An `EVT_TWR_UPLINK` carried the reserved `ack` 5 and was dropped.
    pub fn on_reserved_ack(&self) {
        self.update(|s| s.evt_reserved_ack = s.evt_reserved_ack.saturating_add(1));
    }

    /// A `LinkCheckReq` / `TimeReq` uplink the Northbridge answers in its ACK.
    pub fn on_ack_ctrl_req(&self) {
        self.update(|s| s.ack_ctrl_req = s.ack_ctrl_req.saturating_add(1));
    }

    /// A `TimeReq` whose ACK carried no `TimeAns`: the Northbridge had no anchor.
    pub fn on_ack_time_missed(&self) {
        self.update(|s| s.ack_time_missed = s.ack_time_missed.saturating_add(1));
    }

    /// A `TWR_TIME_SYNC` taken (`ok`), or refused / unanswered.
    pub fn on_time_sync(&self, ok: bool) {
        self.update(|s| {
            if ok {
                s.time_sync_sent = s.time_sync_sent.saturating_add(1);
            } else {
                s.time_sync_failed = s.time_sync_failed.saturating_add(1);
            }
        });
    }

    /// What the ring receiver skipped since the last call: added to the cumulative counters.
    pub fn add_delivery(&self, relearn: u64, seq_gap: u64, seq_gap_seen: u64) {
        if relearn == 0 && seq_gap == 0 && seq_gap_seen == 0 {
            return;
        }
        self.update(|s| {
            s.evt_relearn = s.evt_relearn.saturating_add(relearn);
            s.evt_seq_gap = s.evt_seq_gap.saturating_add(seq_gap);
            s.evt_seq_gap_seen = s.evt_seq_gap_seen.saturating_add(seq_gap_seen);
        });
    }

    /// The outbox as it is now. A new overflow, or a backlog appearing or clearing, is a change
    /// worth publishing at once; the count moving within a backlog waits for the next tick.
    pub fn set_outbox(&self, pending: u64, overflow: u64) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let notable =
            overflow != guard.outbox_overflow || (pending == 0) != (guard.outbox_pending == 0);
        guard.outbox_pending = pending;
        guard.outbox_overflow = overflow;
        drop(guard);
        if notable {
            self.version
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The firmware update's state changed (`northbridge_update.md` §3.5).
    pub fn set_update(&self, update: Option<UpdateSummary>) {
        self.update(|s| s.update = update);
    }

    /// The radio parameters just pushed to the modem.
    pub fn on_radio_config(&self, radio: RadioConfig) {
        self.update(|s| s.radio = Some(radio));
    }

    /// The §3 document at `now`.
    pub fn document(&self, now: i64, link_alive: bool) -> Value {
        self.snapshot().document(now, link_alive)
    }
}

/// The document's `gateway` object: the TOWER gateway's own state (`p2p_tower_gateway.md` §2).
fn gateway_document(info: &ModemInfo) -> Value {
    json!({
        "net_id": info.net_id,
        "turnaround_ms": info.turnaround_ms,
        "dl_gap_ms": info.dl_gap_ms,
        "peers": info.peers,
        "peers_max": info.peers_max,
        "queue_used": info.queue_used,
        "queue_max": info.queue_max,
        "evt_ring_used": info.evt_ring_used,
        "evt_ring_max": info.evt_ring_max,
        "ctr_next": info.ctr_next,
        "ctr_last": info.ctr_last,
        "ack_turnaround_us": {
            "last": info.ack_turnaround_last_us,
            "max": info.ack_turnaround_max_us,
        },
        "airtime_ms_last_hour": info.airtime_ms_last_hour,
    })
}

/// Lowercase hex, in byte order.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Unix seconds → RFC 3339 UTC, the timestamps §3 uses.
fn rfc3339(t: i64) -> String {
    crate::lorawan::gateway::format_rfc3339(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_400_000;

    fn info(fw: [u8; 3]) -> ModemInfo {
        let mut i = ModemInfo {
            fw,
            git_sha: [0xc0, 0x5b, 0x8d, 0x3e],
            now_ms: 3_600_000,
            state: gateway_state::GATEWAY,
            net_id: 0x6d24_8a62,
            turnaround_ms: 20,
            dl_gap_ms: 20,
            radio: RadioConfig {
                freq_hz: 869_525_000,
                sf: 7,
                bw: 0,
                cr: 1,
                preamble: 8,
                tx_power_dbm: 14,
                flags: 0,
            },
            peers: 2,
            peers_max: 64,
            queue_max: 32,
            evt_ring_max: 16,
            ctr_next: 5,
            ctr_last: 4096,
            ..ModemInfo::default()
        };
        i.counters[0] = 10; // rx_total
        i.counters[13] = 9; // tx_total
        i.counters[9] = 1; // radio_crc_err
        i
    }

    #[test]
    fn reset_causes_decode_every_latched_bit() {
        assert_eq!(reset_causes(0b0000_1001), vec!["pin", "por"]);
        assert_eq!(reset_causes(1 << 4), vec!["watchdog"]);
        assert!(reset_causes(0).is_empty());
    }

    #[test]
    fn a_fresh_status_is_starting_until_the_modem_answers() {
        let s = NorthbridgeStatus::default();
        s.starting("/dev/ttyAMA3");
        assert_eq!(s.document(NOW, false)["state"], "starting");
        s.on_info(info([0, 1, 1]), NOW);
        let doc = s.document(NOW, true);
        assert_eq!(doc["state"], "alive");
        assert_eq!(doc["fw_version"], "0.1.1");
        assert_eq!(doc["git_sha"], "c05b8d3e");
        assert_eq!(doc["proto_version"], 0, "the flag day has no link version");
        assert_eq!(doc["uptime_s"], 3600);
        assert_eq!(doc["uart"], "/dev/ttyAMA3");
        assert_eq!(doc["last_error"], Value::Null);
    }

    #[test]
    fn every_get_info_counter_is_in_the_document_under_its_name() {
        let s = NorthbridgeStatus::default();
        let mut i = info([0, 3, 0]);
        i.counters[12] = 4; // rx_rearm_fail
        i.counters[24] = 7; // evt_resent
        i.build_flags = build_flags::BENCH_RTT_BRIDGE | build_flags::DEBUG;
        s.on_info(i, NOW);
        s.on_tx_refused();
        let doc = s.document(NOW, true);
        let c = &doc["counters"];
        assert_eq!(
            (c["rx"].clone(), c["tx"].clone(), c["crc_err"].clone()),
            (json!(10), json!(9), json!(1))
        );
        assert!(c.get("rx_total").is_none(), "the old short names stay");
        assert_eq!(c["rx_rearm_fail"], 4);
        assert_eq!(c["evt_resent"], 7);
        assert_eq!(c["tx_refused"], 1);
        assert_eq!(
            c.as_object().map(Map::len),
            Some(39),
            "25 counters + tx_refused + the central's ten + the two ACK-tail ones"
        );
        assert_eq!(doc["err_flags"], json!([]));
        assert_eq!(doc["build_flags"], json!(["bench_rtt_bridge", "debug"]));
        let g = &doc["gateway"];
        assert_eq!(g["net_id"], 0x6d24_8a62u32);
        assert_eq!(g["peers"], 2);
        assert_eq!(g["ctr_last"], 4096);
        assert_eq!(g["ack_turnaround_us"]["max"], 0);
        assert_eq!(doc["radio"]["state"], "gateway");
        assert_eq!(doc["radio"]["freq_hz"], 869_525_000);
    }

    /// A 180 B `GET_INFO` (#470 §13.5): the Northbridge's ACK-tail counters by name, beside the
    /// central's own time-sync and ACK-request counts.
    #[test]
    fn the_ack_tail_and_time_sync_counters_are_in_the_document() {
        let s = NorthbridgeStatus::default();
        let mut i = info([0, 3, 2]);
        i.evt_head_seq = Some(1);
        i.ack_ctrl = Some(crate::p2p::modem_io::AckCtrlCounters {
            sent: 12,
            time_no_anchor: 3,
        });
        s.on_info(i, NOW);
        s.on_ack_ctrl_req();
        s.on_ack_ctrl_req();
        s.on_ack_time_missed();
        s.on_time_sync(true);
        s.on_time_sync(false);
        s.on_time_sync(true);
        let c = &s.document(NOW, true)["counters"];
        assert_eq!(c["ack_ctrl_sent"], 12);
        assert_eq!(c["ack_time_no_anchor"], 3);
        assert_eq!(c["ack_ctrl_req"], 2);
        assert_eq!(c["ack_time_missed"], 1);
        assert_eq!(c["time_sync_sent"], 2);
        assert_eq!(c["time_sync_failed"], 1);
        // Before the first GET_INFO nothing is reported: null.
        let fresh = NorthbridgeStatus::default().document(NOW, true);
        assert_eq!(fresh["counters"]["ack_ctrl_sent"], Value::Null);
        assert_eq!(fresh["counters"]["time_sync_sent"], 0);
    }

    #[test]
    fn a_booted_modem_shows_the_pushed_radio_config_rather_than_its_empty_one() {
        let s = NorthbridgeStatus::default();
        let mut i = info([0, 3, 0]);
        i.state = gateway_state::BOOT;
        i.radio = RadioConfig::default();
        s.on_info(i, NOW);
        let radio = &s.document(NOW, true)["radio"];
        assert_eq!(radio["state"], "boot");
        assert_eq!(
            (radio["freq_hz"].clone(), radio["sf"].clone()),
            (Value::Null, Value::Null)
        );
        s.on_radio_config(info([0, 3, 0]).radio);
        let doc = s.document(NOW, true);
        assert_eq!(doc["radio"]["state"], "boot");
        assert_eq!(doc["radio"]["sf"], 7);
    }

    #[test]
    fn a_silent_link_is_unreachable_and_says_why() {
        let s = NorthbridgeStatus::default();
        s.on_info(info([0, 1, 1]), NOW);
        assert_eq!(s.document(NOW + 600, false)["state"], "unreachable");
        s.unreachable(NOW + 600, "GET_INFO: no RSP");
        let doc = s.document(NOW + 600, false);
        assert_eq!(doc["state"], "unreachable");
        assert_eq!(doc["last_error"]["source"], "link");
        assert_eq!(doc["last_error"]["message"], "GET_INFO: no RSP");
        // The modem answers again: alive, and the link error is gone.
        s.on_info(info([0, 1, 1]), NOW + 700);
        let doc = s.document(NOW + 700, true);
        assert_eq!(doc["state"], "alive");
        assert_eq!(doc["last_error"], Value::Null);
    }

    #[test]
    fn a_modem_fault_is_error_for_one_interval_and_stays_as_last_error() {
        let s = NorthbridgeStatus::default();
        s.on_info(info([0, 2, 0]), NOW);
        s.on_fault(
            &FaultEvent {
                level: crate::p2p::modem::FAULT_LEVEL_ERROR,
                code: 2,
                text: "rx_start -5".into(),
            },
            NOW,
        );
        let doc = s.document(NOW + 10, true);
        assert_eq!(doc["state"], "error");
        assert_eq!(doc["last_error"]["code"], 2);
        assert_eq!(
            doc["last_error"]["message"],
            "RX re-arm failed: rx_start -5"
        );
        let later = s.document(NOW + 61, true);
        assert_eq!(later["state"], "alive");
        assert_eq!(
            later["last_error"]["code"], 2,
            "the record stays for the operator"
        );
    }

    #[test]
    fn the_modems_own_recent_error_is_error_state_and_last_error() {
        let s = NorthbridgeStatus::default();
        let mut i = info([0, 3, 0]);
        i.reset_cause = 1 << 4;
        i.err_flags = 0b10;
        i.last_err_code = 2;
        i.last_err_age_s = 30;
        s.on_info(i, NOW);
        let doc = s.document(NOW, true);
        assert_eq!(
            doc["state"], "error",
            "an error 30 s ago is within the interval"
        );
        assert_eq!(doc["err_flags"], json!(["rx_rearm_failed"]));
        assert_eq!(doc["boot"]["reset_cause"], json!(["watchdog"]));
        assert_eq!(
            doc["boot"]["at"],
            rfc3339(NOW - 3600),
            "a missed EVT_BOOT is dated from the modem's uptime"
        );
        assert_eq!(doc["last_error"]["code"], 2);
        assert_eq!(doc["last_error"]["at"], rfc3339(NOW - 30));
        // The same answer, read 31 s later: the error has aged past one interval.
        assert_eq!(s.document(NOW + 31, true)["state"], "alive");
    }

    #[test]
    fn a_disabled_document_carries_the_card_keys_and_claims_nothing() {
        let s = NorthbridgeStatus::default();
        s.on_info(info([0, 1, 1]), NOW);
        s.disabled();
        let doc = s.document(NOW, true);
        assert_eq!(doc["state"], "disabled");
        assert_eq!(doc["fw_version"], Value::Null);
        assert_eq!(doc["counters"]["rx"], 0);
        assert_eq!(doc["radio"]["freq_hz"], Value::Null);
        assert!(doc.get("gateway").is_none() && doc.get("uart").is_none());
    }

    /// #158: on a Hub without P2P the idle task's probe still reports the firmware and the
    /// update object, and says it is there to answer an update.
    #[test]
    fn an_idle_document_reports_the_firmware_while_disabled() {
        let s = NorthbridgeStatus::default();
        s.disabled();
        s.idle();
        s.on_info(info([0, 4, 0]), NOW);
        s.set_update(Some(UpdateSummary {
            state: "ok",
            bundled: Some("0.4.1+1a2b3c4d".into()),
            attempts: 0,
            last_error: None,
            note: None,
        }));
        let doc = s.document(NOW, false);
        assert_eq!(doc["state"], "disabled");
        assert_eq!(doc["fw_version"], "0.4.0");
        assert_eq!(doc["nb_task"], "idle");
        assert_eq!(doc["update"]["bundled"], "0.4.1+1a2b3c4d");
        assert_eq!(doc["radio"]["state"], Value::Null);
    }

    #[test]
    fn the_update_object_is_null_without_a_bundle_and_filled_with_one() {
        let s = NorthbridgeStatus::default();
        s.on_info(info([0, 4, 0]), NOW);
        assert_eq!(s.document(NOW, true)["update"], Value::Null);
        s.set_update(Some(UpdateSummary {
            state: "failed",
            bundled: Some("0.4.1+1a2b3c4d".into()),
            attempts: 3,
            last_error: Some("erase: NACK".into()),
            note: None,
        }));
        let doc = s.document(NOW, true);
        assert_eq!(
            doc["update"],
            json!({
                "state": "failed",
                "bundled": "0.4.1+1a2b3c4d",
                "attempts": 3,
                "last_error": "erase: NACK",
                "note": null,
            })
        );
        // The rest of the document is unchanged by it.
        assert_eq!(doc["fw_version"], "0.4.0");
        s.disabled();
        assert_eq!(s.document(NOW, true)["update"], Value::Null);
    }

    #[test]
    fn the_topic_is_per_hub() {
        assert_eq!(
            topic("fiber-4e970f4a"),
            "proximos/fiber-4e970f4a/radio/northbridge"
        );
    }

    /// The keys the Portal's Hub radio card reads (`portal/src/render/hub.rs`,
    /// `hub_radio_card.html`), present in every state with a defined value.
    const CARD_KEYS: [&str; 6] = [
        "state",
        "fw_version",
        "proto_version",
        "uptime_s",
        "radio",
        "update",
    ];
    const CARD_RADIO_KEYS: [&str; 3] = ["freq_hz", "sf", "tx_power_dbm"];
    const CARD_COUNTERS: [&str; 7] = [
        "crc_err",
        "link_err",
        "rx",
        "rx_rearm_fail",
        "tx",
        "tx_radio_err",
        "tx_refused",
    ];

    #[test]
    fn the_card_keys_are_there_in_every_state() {
        let starting = NorthbridgeStatus::default();
        starting.starting("/dev/ttyAMA3");
        let alive = NorthbridgeStatus::default();
        alive.on_info(info([0, 3, 0]), NOW);
        let unreachable = NorthbridgeStatus::default();
        unreachable.on_info(info([0, 3, 0]), NOW);
        unreachable.unreachable(NOW + 600, "GET_INFO: no RSP");
        let disabled = NorthbridgeStatus::default();
        disabled.disabled();
        for (want, s, now, link) in [
            ("starting", &starting, NOW, false),
            ("alive", &alive, NOW, true),
            ("unreachable", &unreachable, NOW + 600, false),
            ("disabled", &disabled, NOW, false),
        ] {
            let doc = s.document(now, link);
            assert_eq!(doc["state"], want);
            for k in CARD_KEYS {
                assert!(doc.get(k).is_some(), "{want}: {k} missing");
            }
            assert_eq!(doc["proto_version"], 0, "{want}");
            for k in CARD_RADIO_KEYS {
                assert!(doc["radio"].get(k).is_some(), "{want}: radio.{k} missing");
            }
            for k in CARD_COUNTERS {
                assert!(doc["counters"][k].is_u64(), "{want}: counters.{k} missing");
            }
        }
    }

    /// A retained document as the Portal receives it, from the one serializer: `GET_INFO` of a
    /// gateway in `GATEWAY` with every counter non-zero. The committed file is what the Portal's
    /// radio card is built against; `UPDATE_FIXTURES=1` rewrites it.
    #[test]
    fn the_retained_sample_is_what_the_serializer_writes() {
        let s = NorthbridgeStatus::default();
        s.starting("/dev/ttyAMA3");
        let mut i = info([0, 3, 0]);
        for (n, c) in i.counters.iter_mut().enumerate() {
            *c = 100 + n as u32;
        }
        i.reset_cause = 0x01;
        i.ack_turnaround_last_us = 20_150;
        i.ack_turnaround_max_us = 21_400;
        i.airtime_ms_last_hour = 5_210;
        i.queue_used = 1;
        i.evt_ring_used = 0;
        i.evt_ring_max = 0;
        s.on_info(i, NOW);
        // The bundled image is the one running: nothing to do (`northbridge_update.md` §3.5).
        s.set_update(Some(UpdateSummary {
            state: "idle",
            bundled: Some("0.3.0+c05b8d3e".into()),
            ..UpdateSummary::default()
        }));
        let doc = s.document(NOW, true);
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/northbridge_retained_sample.json"
        );
        let text = serde_json::to_string_pretty(&doc).expect("json") + "\n";
        if std::env::var_os("UPDATE_FIXTURES").is_some() {
            std::fs::write(path, &text).expect("write the sample");
        }
        let committed = std::fs::read_to_string(path).expect("the committed sample");
        assert_eq!(
            committed, text,
            "rerun with UPDATE_FIXTURES=1 and commit the file"
        );
        let c = doc["counters"].as_object().expect("counters");
        for name in COUNTER_NAMES {
            assert!(c.contains_key(counter_key(name)), "{name}");
        }
        assert_eq!(
            c.len(),
            39,
            "25 counters + tx_refused + the central's ten + the two ACK-tail ones"
        );
        // A 172 B Northbridge reports no ACK-tail counters: null, not 0.
        assert_eq!(c["ack_ctrl_sent"], Value::Null);
        assert_eq!(c["ack_time_no_anchor"], Value::Null);
        assert_eq!(
            (doc["proto_version"].clone(), c["tx_refused"].clone()),
            (json!(0), json!(0))
        );
        assert!(doc["gateway"].is_object());
    }
}
