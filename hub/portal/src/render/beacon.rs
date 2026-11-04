use super::*;

/// Which operational fault a Beacon is in, if any — the Beacon half of
/// [`NodeFault`] (devices.beacon.md §3.7, §2.3 "indikátor Poruchy").
///
/// Two terms, not the Node's three: a Beacon has no backfill job to run out
/// of retries (that is B6) and no self-contradicting reading to detect
/// (its field set is fixed at temperature + humidity, so "reports an alarm
/// state for a field it carries no value for" cannot arise the way it does
/// across a 25-field LoRaWAN registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeaconFault {
    /// B2's per-Beacon detector says this Beacon has gone quiet for longer
    /// than K x the effective store interval.
    Offline,
    /// The tag's own `low_battery` flag. The wire flag, not a portal-side
    /// voltage threshold: the Beacon asserts this itself, which is also why
    /// there is no Node equivalent (F15, §19.10).
    LowBattery,
}

/// The one operational fault, in precedence order. Silence first, for the
/// same reason it leads [`NodeFault`]'s: a Beacon nobody can hear is the
/// fault that explains any other symptom, and a low battery is very often
/// *why* it went quiet.
pub(super) fn beacon_fault(beacon: &BeaconState, offline: bool) -> Option<BeaconFault> {
    if offline {
        return Some(BeaconFault::Offline);
    }
    (beacon.low_battery == Some(true)).then_some(BeaconFault::LowBattery)
}

/// One Hub that currently hears this Beacon (devices.beacon.md §3.2).
///
/// A Beacon is a BLE **broadcast**: every Hub in range hears the same
/// advertisement, so several rows here is the normal state rather than a
/// forward-looking single-element list. The portal's store is already
/// per-Hub (`PortalState::devices[host].ble_sensors`), so this is a
/// projection of what it holds rather than anything new — the dev mock runs
/// one Hub, so expect one row there.
#[derive(Debug, Clone, Serialize)]
pub struct BeaconHubCtx {
    pub hostname: String,
    pub rssi: Option<i64>,
    /// RFC 3339, same convention as `BeaconCtx::last_seen`.
    pub last_seen: Option<String>,
    /// This Hub's own `connected` flag for this MAC — whether *it* still
    /// hears the Beacon, as opposed to having heard it once.
    ///
    /// `ble_sensors` is an accumulating map: a Hub that heard one
    /// advertisement keeps its entry forever, so before the B5 review the
    /// table said "currently hears it" about every Hub that ever had. The
    /// entry is not removed (the RSSI and the last-seen stamp are the record
    /// of when it was last heard) — it is marked, so the reader can tell a
    /// live link from a historical one.
    pub connected: bool,
    /// The strongest **connected** Hub — the row the detail page highlights.
    /// No connected Hub means no `best` row at all rather than promoting a
    /// Hub that has stopped hearing the Beacon.
    pub best: bool,
    /// The Beacon's home Hub (decision #42): in a cluster the one the Server named in
    /// `eye/homes`, the only Hub that dials it; outside one the Hub that registered it.
    pub home: bool,
}

/// Every Hub whose `ble_sensors` holds `mac`, live Hubs first and strongest
/// first inside each half.
///
/// A Hub with no RSSI for the Beacon sorts last within its half: "heard it,
/// cannot say how well" is worse evidence than any number, and floating it
/// to the top would push a measured link off the highlighted row.
pub fn beacon_hubs(state: &crate::state::PortalState, mac: &str) -> Vec<BeaconHubCtx> {
    let cluster_home = state
        .beacon_cluster
        .as_ref()
        .map(|c| c.home_of(mac).map(str::to_string));
    let mut hubs: Vec<BeaconHubCtx> = state
        .devices
        .values()
        .filter_map(|device| {
            let beacon = device.ble_sensors.get(mac)?;
            let home = match &cluster_home {
                Some(home) => home
                    .as_deref()
                    .is_some_and(|h| device.hostname.eq_ignore_ascii_case(h)),
                None => beacon.discovered != Some(true),
            };
            Some(BeaconHubCtx {
                hostname: device.hostname.clone(),
                rssi: beacon.rssi,
                last_seen: beacon.last_seen.and_then(unix_to_rfc3339),
                connected: beacon.connected,
                best: false,
                home,
            })
        })
        .collect();
    hubs.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| match (a.rssi, b.rssi) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.hostname.cmp(&b.hostname),
            })
    });
    if let Some(first) = hubs.first_mut() {
        first.best = first.connected;
    }
    hubs
}

/// `pages/beacon_detail.html` / `partials/beacon_card.html`'s `b.fields[]`
/// entry — the fixed temperature + humidity pair (devices.beacon.md §3.4:
/// "vzdy jen dve veliciny"), driven off the EYE registry so the labels and
/// units come from one place.
#[derive(Debug, Clone, Serialize)]
pub struct BeaconFieldCtx {
    pub name: String,
    pub label: String,
    pub unit: String,
    pub value: Option<f64>,
    /// Normalized ("Normal"/"Warning"/"Critical"), so the three-state "out
    /// of range" filter reads one vocabulary across Nodes and Beacons.
    pub alarm_state: String,
    /// `false` = the sensor is switched off (GitLab #120): `value` is its last value, kept in
    /// memory only, and nothing but the detail's "Nastavení čidel" table shows it.
    pub enabled: bool,
}

/// Everything the Beacon list needs from outside the store, loaded once per
/// render pass — the Beacon mirror of [`NodeRowData`], and the fix for the
/// N+1 §16.3 found in four places (`beacon_ctxs`, `beacon_threshold_map`
/// and both SSE arms each awaited one `list_field_thresholds` per Beacon).
///
/// Same two bulk reads as the Node side, on the same already-scope-
/// parameterised functions: `device_meta_map` and `field_thresholds_by_entity`
/// with `THRESHOLD_SCOPE_EYE`. No new repo function, and no per-row query at
/// any of the four sites.
///
/// **Three of the four go through this builder, not all four** (B5 review,
/// 2026-09-08; §20.3 overstated it): `beacon_threshold_map` wants a
/// `MAC -> thresholds` map rather than a whole `BeaconCtx`, so it makes the
/// same single bulk call directly. The N+1 is gone at all four sites; the
/// builder is shared by three.
#[derive(Debug, Default)]
pub struct BeaconRowData {
    pub meta: std::collections::HashMap<String, portal_storage::DeviceMeta>,
    pub thresholds: std::collections::HashMap<String, Vec<FieldThreshold>>,
    pub liveness: crate::notify::offline::LivenessVerdicts,
    pub signal_bands: crate::config::SignalQualityConfig,
    /// Calibration level per MAC holding a certificate — the Node side's
    /// `NodeRowData::calibration`, from the same in-memory cache.
    pub calibration: std::collections::HashMap<String, crate::calibration::CalibrationLevel>,
}

impl BeaconRowData {
    /// Assembles one Beacon's inputs. `hubs` comes from the caller's own
    /// snapshot ([`beacon_hubs`]) rather than being resolved here, exactly
    /// as `NodeRowData::inputs` takes `info` from the caller's.
    pub fn inputs<'a>(
        &'a self,
        beacon: &'a BeaconState,
        hubs: Vec<BeaconHubCtx>,
    ) -> BeaconCardInputs<'a> {
        BeaconCardInputs {
            beacon,
            meta: self.meta.get(&beacon.mac),
            thresholds: self
                .thresholds
                .get(&beacon.mac)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            offline: self
                .liveness
                .verdict(crate::notify::offline::DeviceScope::Beacon, &beacon.mac),
            signal_bands: &self.signal_bands,
            hubs,
            calibration: self.calibration.get(&beacon.mac).copied(),
        }
    }
}

/// Everything one Beacon row needs that is not on `BeaconState` — the
/// Beacon mirror of [`NodeCardInputs`], and for the same reason: keeping
/// `beacon_ctx` pure while the resolution happens once per pass.
pub struct BeaconCardInputs<'a> {
    pub beacon: &'a BeaconState,
    pub meta: Option<&'a portal_storage::DeviceMeta>,
    /// The stored rows for this Beacon. Unlike Nodes there is nothing to
    /// merge them with — `eye/sensors` carries no firmware-echoed threshold
    /// block, so these are the thresholds (this module's header).
    pub thresholds: &'a [FieldThreshold],
    /// B2's verdict for this Beacon: `Some(true)` offline, `Some(false)`
    /// online, `None` no verdict yet — see [`node_connected`], which is the
    /// same rule and the same function.
    pub offline: Option<bool>,
    pub signal_bands: &'a crate::config::SignalQualityConfig,
    /// Every Hub that hears it, strongest first ([`beacon_hubs`]).
    pub hubs: Vec<BeaconHubCtx>,
    /// This Beacon's calibration level; `None` = no certificate uploaded.
    pub calibration: Option<crate::calibration::CalibrationLevel>,
}

/// `partials/beacon_card.html` / `pages/beacons.html`'s `b`/list-entry
/// context — shared by both the registered-card partial and the flat
/// `beacons[]` list (identical shape; the page splits on `discovered`), and
/// by `pages/beacon_detail.html`.
/// `thresholds` is only ever populated for registered beacons (the page's
/// header: candidates never carry it, read defensively by the card
/// partial).
#[derive(Debug, Clone, Serialize)]
pub struct BeaconCtx {
    /// The Hub this Beacon is registered on, stamped by the list builder so
    /// one list can hold rows from several Hubs (devices.hub.md §12 E).
    /// `None` elsewhere, where the template falls back to the page-level (or
    /// SSE-supplied) `hostname`. Not the same thing as `hubs`, which is every
    /// Hub that *hears* the broadcast.
    pub hostname: Option<String>,
    /// The `hubs` entry flagged `home` — the list's "Domovský Hub" column (decision #42).
    pub home_hub: Option<String>,
    pub mac: String,
    pub name: Option<String>,
    pub location: Option<String>,
    pub connected: bool,
    pub discovered: bool,
    pub temperature: Option<f64>,
    pub humidity: Option<i64>,
    pub battery_mv: Option<i64>,
    pub low_battery: Option<bool>,
    pub rssi: Option<i64>,
    /// RFC 3339, kept verbatim like a Node's — see `unix_to_rfc3339` (the
    /// wire value is unix seconds; this converts it once here so every
    /// template renders it through the `short_dt` filter).
    pub last_seen: Option<String>,
    pub alarm_state: String,
    /// Raw ingest string (mqtt.md `eye/sensors.provisioning`), per-beacon
    /// only — never rolled up into page/Hub-level status (UI-15).
    pub provisioning: Option<String>,

    // ---- B6 (devices.backend.md §9 WP6.3/WP6.4, §22) -------------------
    /// The auto-provisioning state as the operator reads it, or `None` when
    /// there is nothing to say — see [`provisioning_state`].
    pub provisioning_state: Option<&'static str>,
    /// What the Beacon reported the last time the Hub read its configuration
    /// — the "Parametry beaconu (BLE)" rows. `None`: the Hub has not reported
    /// one, and the rows say exactly that.
    pub ble_config: Option<BleConfigCtx>,
    /// On a `differs` Beacon with a read-back: the parameters that differ
    /// from the Hub's default fleet profile, see [`profile_differences`].
    /// `None` in any other state.
    pub profile_diffs: Option<Vec<&'static str>>,
    /// The Beacon has never sent a message: devices.beacon.md §2.5's
    /// "zaregistrováno — čeká na první zprávu", the Beacon twin of a Node's
    /// "waiting for join". Not a fault and not a countdown — a registered
    /// Beacon that nobody has switched on yet is a perfectly ordinary state,
    /// and §21.6 settled that it needs no storage.
    pub waiting_for_first_message: bool,
    /// The serial number off the device's label, from `device_meta` (V14).
    /// **Not on the wire** — the BTSMP1 advertisement carries none — so
    /// `None` means "nobody typed it", never "the Beacon did not report it".
    pub serial: Option<String>,
    /// The last raw `eye/sensors` tag object, pretty-printed for §3.3's
    /// expandable JSON preview. `None` before the first message — **and on
    /// every surface except the detail page.**
    ///
    /// `beacon_ctx` deliberately leaves this `None`: exactly one template
    /// reads it, and pretty-printing there meant re-serializing every
    /// Beacon's whole last tag for every list render and every SSE card
    /// frame, to throw all of it away (B6 review, item 10). The detail route
    /// fills it in with [`beacon_last_payload_json`].
    pub last_payload_json: Option<String>,
    pub thresholds: Option<BTreeMap<String, ThresholdEntryCtx>>,

    // ---- B5 (devices.backend.md §8 WP5.2, §20) ---------------------------
    /// `None` = operationally healthy. Independent of `alarm_state`, which
    /// is the value axis — devices.node.md §3.1's two indicators, which
    /// devices.beacon.md §1 adopts by reference.
    pub fault: Option<BeaconFault>,
    /// Since when the fault has stood, for the detail page's banner. Set
    /// for an offline fault only, same honesty rule as `NodeDetailCtx`:
    /// a low battery has no moment the portal knows about.
    pub fault_since: Option<String>,
    /// Every Hub that hears this broadcast, strongest first.
    pub hubs: Vec<BeaconHubCtx>,
    /// Operator-supplied, from `device_meta`. On the row and not only on
    /// the detail page because it is part of the fulltext filter
    /// (devices.beacon.md §2.2).
    pub description: Option<String>,
    /// When the operator registered this Beacon — the list's default sort,
    /// newest first. From `device_meta`, *not* `BeaconState::added_at`:
    /// that one is a store-clock first-seen that resets with the process.
    pub created_at: Option<String>,
    /// The full `device_meta` row, for the detail page's system-configuration
    /// half (WP5.4). The list ignores it.
    pub meta: DeviceMetaCtx,
    /// The fixed temperature + humidity pair with their per-field value
    /// class, so the three-state "out of range" filter evaluates
    /// client-side without a round trip per row.
    pub fields: Vec<BeaconFieldCtx>,
    /// Derived from RSSI **only**: BLE advertisements carry no SNR, so the
    /// SNR half of the ladder would reject every Beacon as `Unknown`.
    pub signal: SignalLevel,
    /// `x.yy V` from `battery_mv` (devices.beacon.md §2.3), formatted by
    /// [`beacon_battery_volts`] so the card, the detail page and the
    /// monitoring group row agree — the group row used to print raw
    /// millivolts, which is the B5 review defect this helper closes.
    pub battery: Option<String>,
    /// The list's calibration icon — `NodeCardCtx::calibration`'s rule.
    pub calibration: Option<&'static str>,
}

/// The wire's `provisioning` string as one of the **four labels the operator
/// sees**, or `None` for the two cases that show nothing at all.
///
/// `control-ble`'s host publishes one of `pending | provisioning |
/// provisioned | differs | failed | read_only` (`crates/control-ble/src/host.rs`).
/// devices.beacon.md §3.5 (addendum 2026-09-23) names each one, so:
///
/// * `pending` → **waiting**: registered, the configuration not read yet.
/// * `provisioning` → **running**: a configuration session is queued or in
///   flight.
/// * `provisioned` → **configured**: the last read holds the fleet profile.
///   Drawn since 2026-09-23 — the operator asked to see it, where it used to
///   be the absence of a pill.
/// * `differs` → **differs**: the last plain read does not hold the fleet
///   profile, so a reconfiguration is what it wants.
/// * `failed` → **failed**, which is the one that also offers "Zkusit
///   znovu". **No reason travels with it** — the wire carries a state and
///   nothing else — so the page states the failure and points at the
///   retry (`monitoring.beacon.prov_no_reason`, recorded in §22.5).
/// * `read_only` → **read_only**: the Beacon refuses configuration writes,
///   so the reconfigure button is hidden rather than offered and rejected.
/// * anything else, or nothing at all → `None`. An unknown state is not a
///   fault, and a Beacon heard before B6's Hub says nothing here.
///
/// Per-beacon only, never rolled up (UI-15): one Beacon that will not take
/// its profile is not a Hub-level fault.
fn provisioning_state(raw: Option<&str>) -> Option<&'static str> {
    match raw?.trim() {
        "pending" => Some("waiting"),
        "provisioning" => Some("running"),
        "provisioned" => Some("configured"),
        "differs" => Some("differs"),
        "failed" => Some("failed"),
        "read_only" => Some("read_only"),
        _ => None,
    }
}

/// §3.3's raw-message preview: the last `eye/sensors` tag object as
/// indented JSON.
///
/// Separate from [`beacon_ctx`] and called by the detail route alone. The
/// store keeps the raw `Value` for **every** tag, discovered ones included —
/// the detail page serves any MAC the store holds, so a candidate's preview
/// has to work too — but the pretty-printing is the part that used to run
/// once per Beacon per list render and per SSE frame for a string only this
/// one template reads (B6 review, item 10).
pub fn beacon_last_payload_json(beacon: &BeaconState) -> Option<String> {
    beacon
        .last_payload
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
}

/// What a BTSMP1 broadcasts, from its `e61c0001` protocol-type byte
/// (Teltonika wiki, "EYE SENSOR / BTSMP1": 0 iBeacon, 1 Eddystone, 2 EYE
/// Sensor, 3 iBeacon + EYE Sensor, 4 Eddystone + EYE Sensor; 3 and 4 need
/// firmware 01.02.10). Only the EYE Sensor frame carries the readings the
/// decoder reads. Serialized snake_case so the template can branch on it and
/// keep every label a literal `"…"|t` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeaconProtocol {
    Ibeacon,
    Eddystone,
    EyeSensor,
    IbeaconEyeSensor,
    EddystoneEyeSensor,
    /// A code the table above does not know; the row prints the number.
    Unknown,
}

impl BeaconProtocol {
    pub fn from_code(code: i64) -> Self {
        match code {
            0 => Self::Ibeacon,
            1 => Self::Eddystone,
            2 => Self::EyeSensor,
            3 => Self::IbeaconEyeSensor,
            4 => Self::EddystoneEyeSensor,
            _ => Self::Unknown,
        }
    }
}

/// The four sensors of the `e61c0021` active-sensor mask, in bit order —
/// the same codes control-ble puts on the wire.
const BLE_SENSORS: [&str; 4] = ["temperature", "humidity", "magnet", "movement"];

/// One row of the sensor list: every sensor the mask can switch, on or off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BleSensorCtx {
    pub code: String,
    pub active: bool,
}

/// The "Parametry beaconu (BLE)" card's rows, from the Hub's read-back
/// (`eye/sensors` tag `ble_config`). Values only — the template words them,
/// so every label stays a literal `"…"|t` key.
#[derive(Debug, Clone, Serialize)]
pub struct BleConfigCtx {
    /// Whole seconds when the interval is one (every fleet profile is);
    /// otherwise `None` and the row falls back to `interval_ms`.
    pub interval_s: Option<i64>,
    pub interval_ms: Option<i64>,
    pub tx_power_dbm: Option<i64>,
    pub protocol: Option<BeaconProtocol>,
    pub protocol_type: Option<i64>,
    /// Every sensor of the mask with whether it is on, in bit order; a name
    /// the Hub reported that the table does not know follows, as on. `None`
    /// on a recorder, which has no active-sensor characteristic — not the
    /// same thing as every sensor off.
    pub sensors: Option<Vec<BleSensorCtx>>,
    /// `sensors` came from what the Beacon broadcasts, not from a mask it was
    /// read for: a Beacon without the active-sensor characteristic (older
    /// than firmware 01.02.10) cannot be asked, but its advertisement says
    /// which values it carries. See [`with_broadcast_sensors`].
    pub sensors_from_broadcast: bool,
    /// When the Hub read it — RFC 3339, rendered through `short_dt` like
    /// `last_seen`.
    pub read_at: Option<String>,
    /// The read closed a provisioning session and matched what it wrote.
    /// `false` for the plain read that follows an enrollment: it says what
    /// the Beacon holds, and vouches for nothing.
    pub verified: bool,
    /// Whether the archive is recording (beacon_history.md §8); `None` until
    /// the Hub has read the record info. The History tab warns on `false`.
    pub recording: Option<bool>,
    pub logging_interval_min: Option<i64>,
}

impl From<&fiber_protocol::payloads::eye::EyeBleConfig> for BleConfigCtx {
    fn from(c: &fiber_protocol::payloads::eye::EyeBleConfig) -> Self {
        BleConfigCtx {
            interval_s: c
                .advertising_interval_ms
                .filter(|ms| ms % 1000 == 0)
                .map(|ms| ms / 1000),
            interval_ms: c.advertising_interval_ms,
            tx_power_dbm: c.tx_power_dbm,
            protocol: c.protocol_type.map(BeaconProtocol::from_code),
            protocol_type: c.protocol_type,
            // `sensors` (every bit, on or off) from a current Hub; the older list of the bits
            // that were on from one that predates it.
            sensors: match (&c.sensors, &c.active_sensors) {
                (Some(map), _) => Some(ble_sensor_rows_from_map(map)),
                (None, Some(active)) => Some(ble_sensor_rows(active)),
                (None, None) => None,
            },
            sensors_from_broadcast: false,
            read_at: c.read_at.and_then(unix_to_rfc3339),
            verified: c.verified == Some(true),
            recording: c.recording,
            logging_interval_min: c.logging_interval_min,
        }
    }
}

/// The fleet profile a Hub writes when its `eye:` section sets nothing:
/// control-ble's `BeaconProfile` defaults and `eye.recording_interval_min`
/// (beacon_history.md §3). The Hub does not publish the profile it actually
/// runs, so this is what the Portal can compare a `differs` read-back with,
/// and the page says it is the default.
pub const DEFAULT_PROFILE_INTERVAL_MS: i64 = 10_000;
pub const DEFAULT_PROFILE_TX_POWER_DBM: i64 = 8;
pub const DEFAULT_PROFILE_SENSORS: [&str; 2] = ["temperature", "humidity"];
pub const DEFAULT_PROFILE_RECORDING_MIN: i64 =
    crate::beacon_history::DEFAULT_RECORDING_INTERVAL_S / 60;
/// The protocol type every provisioning writes (BTSMP1 `e61c0001`, EYE
/// Sensor). Not configurable, so a different one always differs.
pub const PROFILE_PROTOCOL_TYPE: i64 = 0x02;

/// Which read-back parameters differ from the default fleet profile, by
/// code: `advertising_interval`, `tx_power`, `protocol`, `sensors`,
/// `recording`. Only what was read is judged: a recorder has no sensor mask,
/// sensors taken from the broadcast were not read, and the recording is
/// unknown until the Hub has read the record info. The Hub judges the same
/// parameters (control-ble `holds_profile`, `judge_recording`) against the
/// profile in its own config, which may not be the default.
pub fn profile_differences(ble: &BleConfigCtx) -> Vec<&'static str> {
    let mut diffs = Vec::new();
    if ble
        .interval_ms
        .is_some_and(|ms| ms != DEFAULT_PROFILE_INTERVAL_MS)
    {
        diffs.push("advertising_interval");
    }
    if ble
        .tx_power_dbm
        .is_some_and(|dbm| dbm != DEFAULT_PROFILE_TX_POWER_DBM)
    {
        diffs.push("tx_power");
    }
    if ble
        .protocol_type
        .is_some_and(|code| code != PROFILE_PROTOCOL_TYPE)
    {
        diffs.push("protocol");
    }
    if let Some(rows) = ble.sensors.as_ref().filter(|_| !ble.sensors_from_broadcast) {
        let mut on: Vec<&str> = rows
            .iter()
            .filter(|r| r.active)
            .map(|r| r.code.as_str())
            .collect();
        on.sort_unstable();
        let mut want = DEFAULT_PROFILE_SENSORS.to_vec();
        want.sort_unstable();
        if on != want {
            diffs.push("sensors");
        }
    }
    match (ble.recording, ble.logging_interval_min) {
        (Some(false), _) => diffs.push("recording"),
        (Some(true), Some(min)) if min != DEFAULT_PROFILE_RECORDING_MIN => diffs.push("recording"),
        _ => {}
    }
    diffs
}

/// A Beacon read back without an active-sensor mask gets its sensor rows
/// from its last advertisement instead: temperature and humidity when it
/// carries them, magnet when the magnet field is present, movement when the
/// movement word is. Nothing heard yet leaves `sensors` empty, and the card
/// keeps "senzory dané typem beaconu".
fn with_broadcast_sensors(mut ctx: BleConfigCtx, beacon: &BeaconState) -> BleConfigCtx {
    if ctx.sensors.is_some() {
        return ctx;
    }
    let heard = [
        ("temperature", beacon.temperature.is_some()),
        ("humidity", beacon.humidity.is_some()),
        ("magnet", beacon.magnet_present == Some(true)),
        (
            "movement",
            beacon.moving.is_some() || beacon.movement_count.is_some(),
        ),
    ];
    if heard.iter().any(|(_, on)| *on) {
        ctx.sensors = Some(
            heard
                .into_iter()
                .map(|(code, active)| BleSensorCtx {
                    code: code.to_string(),
                    active,
                })
                .collect(),
        );
        ctx.sensors_from_broadcast = true;
    }
    ctx
}

fn ble_sensor_rows_from_map(map: &std::collections::BTreeMap<String, bool>) -> Vec<BleSensorCtx> {
    let known = BLE_SENSORS.iter().map(|code| BleSensorCtx {
        code: (*code).to_string(),
        active: map.get(*code).copied().unwrap_or(false),
    });
    let unknown = map
        .iter()
        .filter(|(code, _)| !BLE_SENSORS.contains(&code.as_str()))
        .map(|(code, on)| BleSensorCtx {
            code: code.clone(),
            active: *on,
        });
    known.chain(unknown).collect()
}

fn ble_sensor_rows(active: &[String]) -> Vec<BleSensorCtx> {
    let known = BLE_SENSORS.iter().map(|code| BleSensorCtx {
        code: (*code).to_string(),
        active: active.iter().any(|a| a == code),
    });
    let unknown = active
        .iter()
        .filter(|a| !BLE_SENSORS.contains(&a.as_str()))
        .map(|a| BleSensorCtx {
            code: a.clone(),
            active: true,
        });
    known.chain(unknown).collect()
}

/// Builds one Beacon's context from the bulk-loaded inputs
/// ([`BeaconRowData::inputs`]).
pub fn beacon_ctx(input: BeaconCardInputs<'_>) -> BeaconCtx {
    let BeaconCardInputs {
        beacon,
        meta,
        thresholds,
        offline,
        signal_bands,
        hubs,
        calibration,
    } = input;

    // Bands the operator switched off (GitLab #80). `enabled` is the portal's
    // own flag, so the portal is the only place that can honour it: the
    // per-field states below are the DEVICE's, and it goes on echoing a band
    // it has not been told to clear yet.
    let switched_off: std::collections::BTreeSet<&str> = thresholds
        .iter()
        .filter(|t| !t.enabled)
        .map(|t| t.field.as_str())
        .collect();

    let alarm_state = normalize_radio_alarm_state(&beacon.alarm_state);
    // A switched-off band must not hold the row's value state up either. Only
    // the VALUE states are recomputed — `Disconnected`, `Reconnecting` and
    // `NeverConnected` are the connectivity axis and are not ours to restate —
    // and only from what the device echoed, so a field the portal knows
    // nothing about still counts.
    let alarm_state = match alarm_state.as_str() {
        "Normal" | "Warning" | "Critical" if !beacon.field_alarm_states.is_empty() => beacon
            .field_alarm_states
            .iter()
            .filter(|(field, _)| !switched_off.contains(field.as_str()))
            .map(|(_, state)| normalize_radio_alarm_state(state))
            .max_by_key(|state| match state.as_str() {
                "Critical" => 2,
                "Warning" => 1,
                _ => 0,
            })
            .unwrap_or_else(|| "Normal".to_string()),
        _ => alarm_state,
    };
    // The same one answer the Node list gives (§19.1): the detector wins,
    // the Hub's echoed enum is only the fallback where it holds no verdict.
    let connected = node_connected(&alarm_state, offline);
    let fault = beacon_fault(beacon, offline == Some(true));
    let last_seen = beacon.last_seen.and_then(unix_to_rfc3339);

    let fields: Vec<BeaconFieldCtx> = fiber_protocol::eye_registry()
        .iter()
        .filter(|f| f.thresholdable)
        .map(|f| BeaconFieldCtx {
            name: f.name.to_string(),
            label: f.label.to_string(),
            unit: f.unit.unwrap_or("").to_string(),
            value: match (beacon.disabled_fields.get(f.name), f.name) {
                (Some(last), _) => *last,
                (None, "temperature") => beacon.temperature,
                (None, "humidity") => beacon.humidity.map(|h| h as f64),
                _ => None,
            },
            enabled: !beacon.disabled_fields.contains_key(f.name),
            alarm_state: match switched_off.contains(f.name) {
                true => "Normal".to_string(),
                false => beacon
                    .field_alarm_states
                    .get(f.name)
                    .map(|s| normalize_radio_alarm_state(s))
                    .unwrap_or_else(|| "Normal".to_string()),
            },
        })
        .collect();

    let thresholds = if thresholds.is_empty() {
        None
    } else {
        Some(
            thresholds
                .iter()
                .map(|t| (t.field.clone(), ThresholdEntryCtx::from(t)))
                .collect(),
        )
    };

    let provisioning_state = provisioning_state(beacon.provisioning.as_deref());
    let ble_config = beacon
        .ble_config
        .as_ref()
        .map(|c| with_broadcast_sensors(BleConfigCtx::from(c), beacon));
    BeaconCtx {
        // Stamped by the caller when the list spans several Hubs; see the
        // field's own comment.
        hostname: None,
        home_hub: hubs.iter().find(|h| h.home).map(|h| h.hostname.clone()),
        mac: beacon.mac.clone(),
        name: beacon.name.clone(),
        location: beacon.location.clone(),
        connected,
        discovered: beacon.discovered.unwrap_or(false),
        temperature: beacon.temperature,
        humidity: beacon.humidity,
        battery_mv: beacon.battery_mv,
        low_battery: beacon.low_battery,
        rssi: beacon.rssi,
        provisioning_state,
        profile_diffs: ble_config
            .as_ref()
            .filter(|_| provisioning_state == Some("differs"))
            .map(profile_differences),
        ble_config,
        waiting_for_first_message: alarm_state == "NeverConnected" && beacon.last_seen.is_none(),
        serial: meta.and_then(|m| m.serial.clone()),
        // Filled in by the detail route alone — see the field's doc.
        last_payload_json: None,
        alarm_state,
        provisioning: beacon.provisioning.clone(),
        thresholds,
        fault,
        fault_since: matches!(fault, Some(BeaconFault::Offline))
            .then(|| last_seen.clone())
            .flatten(),
        hubs,
        description: meta.and_then(|m| m.description.clone()),
        created_at: meta.map(|m| m.created_at.clone()),
        meta: device_meta_ctx(meta),
        fields,
        signal: beacon_signal_level(beacon.rssi, signal_bands),
        battery: beacon_battery_volts(beacon.battery_mv),
        last_seen,
        calibration: super::node::calibration_flag(calibration),
    }
}

/// A Beacon's battery as `x.yy V`, from the wire's millivolts.
///
/// devices.beacon.md §2.3 asks for volts everywhere a Beacon battery is
/// shown. Two builders show one — [`beacon_ctx`] for the Beacon card and the
/// detail page, and `monitoring::beacon_group_row` for the dashboard's group
/// row — and until the B5 review the second printed `"2904 mV"` while the
/// first printed `"2.90 V"`. One helper, so the unit cannot drift again.
pub fn beacon_battery_volts(battery_mv: Option<i64>) -> Option<String> {
    battery_mv.map(|mv| format!("{:.2} V", mv as f64 / 1000.0))
}

/// The link-quality rung for a Beacon, from RSSI alone.
///
/// [`signal_level`] needs both RSSI and SNR and answers `Unknown` when
/// either is missing — correct for LoRaWAN, useless here: a BLE
/// advertisement has no SNR at all, so every Beacon would read `Unknown`
/// forever. devices.beacon.md §2.3 says so explicitly ("u BLE bez SNR —
/// mapovani jen z RSSI, prahy v temze YAML"), which is why this reads the
/// RSSI half of the same [`crate::config::SignalQualityConfig`] ladder
/// rather than introducing a second set of floors to keep in step.
///
/// The `weak` rung is a plain floor here, not the Node ladder's `or` of two
/// measurements — with one measurement there is nothing to `or`.
pub fn beacon_signal_level(
    rssi: Option<i64>,
    bands: &crate::config::SignalQualityConfig,
) -> SignalLevel {
    let Some(rssi) = rssi else {
        return SignalLevel::Unknown;
    };
    if rssi >= bands.excellent.rssi {
        SignalLevel::Excellent
    } else if rssi >= bands.good.rssi {
        SignalLevel::Good
    } else if rssi >= bands.weak.rssi {
        SignalLevel::Weak
    } else {
        SignalLevel::Critical
    }
}

// ---------------------------------------------------------------------
// B6 (devices.backend.md §9, spec devices.beacon.md §4): the passive-scan
// view — "zachycené beacony (posledních 15 minut)".
// ---------------------------------------------------------------------

/// How far back the scan table looks, in seconds.
///
/// A **render-time filter**, and a constant rather than a config read:
/// devices.beacon.md §4 states the window as fifteen minutes of the
/// registration flow, which is a different thing from `beacons.store_interval`
/// (also fifteen minutes by default, but the offline detector's expected
/// interval, not how long a stranger stays on screen).
///
/// It rarely bites. The Hub expires a stranger from its own `discovered` set
/// after `ble.central.beacon_timeout_s` = 600 s, and reconcile-by-absence
/// prunes it from the store on the next snapshot — so in practice a row
/// disappears at ten minutes, not fifteen. The filter is the portal's own
/// floor for the case where the Hub's timeout is raised above it.
pub const DISCOVERED_WINDOW_S: i64 = 15 * 60;

/// One row of `partials/discovered_table.html` — a beacon the Hub overheard
/// and nobody has registered.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredCtx {
    pub mac: String,
    /// `:` → `-`, the DOM-id mangling every Beacon target uses.
    pub mac_id: String,
    /// Only ever the advertised name; a stranger has no `device_meta` row.
    pub name: Option<String>,
    pub temperature: Option<f64>,
    pub humidity: Option<i64>,
    pub rssi: Option<i64>,
    pub signal: SignalLevel,
    /// Which Hub heard it. Single-Hub in phase 1, but the column is in the
    /// spec's wireframe because a Beacon is a broadcast and several Hubs
    /// hearing it is the normal state (§3.2).
    pub hub: String,
    /// RFC3339 of the **store-clock** stamp, not the device-reported
    /// `last_seen_ts`: a stranger that has never been provisioned may report
    /// no time at all, and the window is measured on our clock.
    pub last_seen: Option<String>,
    /// Seconds since that stamp — the spec's "před 8 s".
    pub age_s: Option<i64>,
}

/// `pages/beacon_add.html`'s lower half.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredPanelCtx {
    pub rows: Vec<DiscoveredCtx>,
    /// The last snapshot dropped strangers to fit — "we are dropping
    /// neighbours", which control.ble.md Part B UI-12 requires be visible
    /// rather than passing for a quiet site.
    pub capped: bool,
    /// 15, for the heading. Rendered rather than hard-coded in the template
    /// so the constant above is the only place the number lives.
    pub window_minutes: i64,
    /// The Hub is online and therefore listening. A stranger cannot appear
    /// while it is not, and an empty table means two different things
    /// depending on this.
    pub scanning: bool,
}

/// The scan panel for one Hub's snapshot.
///
/// Discovered-only beacons (`discovered == Some(true)`; `None` is
/// **registered** — the wire's stated trap) inside [`DISCOVERED_WINDOW_S`],
/// strongest signal first so the beacon in the operator's hand is at the top.
pub fn discovered_panel(
    device: &crate::state::DeviceState,
    signal_bands: &crate::config::SignalQualityConfig,
    now: i64,
) -> DiscoveredPanelCtx {
    let mut rows: Vec<DiscoveredCtx> = device
        .ble_sensors
        .values()
        .filter(|b| b.discovered == Some(true))
        .filter(|b| in_scan_window(b, now))
        .map(|b| discovered_row(device, b, signal_bands, now))
        .collect();
    sort_discovered(&mut rows);

    DiscoveredPanelCtx {
        rows,
        capped: device.discovered_capped,
        window_minutes: DISCOVERED_WINDOW_S / 60,
        scanning: device.online,
    }
}

/// The cluster's one scan panel (decision #42): every Hub's strangers, one row per MAC —
/// the Hub that hears it strongest names the row. A MAC the Server registered is never a
/// stranger here, even on a Hub that has not received its view yet.
pub fn cluster_discovered_panel(
    state: &crate::state::PortalState,
    signal_bands: &crate::config::SignalQualityConfig,
    now: i64,
) -> DiscoveredPanelCtx {
    let registered = |mac: &str| {
        state
            .beacon_cluster
            .as_ref()
            .is_some_and(|c| c.is_registered(mac))
    };
    let mut best: BTreeMap<&str, DiscoveredCtx> = BTreeMap::new();
    for device in state.devices.values() {
        for b in device.ble_sensors.values() {
            if b.discovered != Some(true) || registered(&b.mac) || !in_scan_window(b, now) {
                continue;
            }
            let row = discovered_row(device, b, signal_bands, now);
            let stronger = match best.get(b.mac.as_str()) {
                None => true,
                Some(held) => row.rssi.unwrap_or(i64::MIN) > held.rssi.unwrap_or(i64::MIN),
            };
            if stronger {
                best.insert(&b.mac, row);
            }
        }
    }
    let mut rows: Vec<DiscoveredCtx> = best.into_values().collect();
    sort_discovered(&mut rows);

    DiscoveredPanelCtx {
        rows,
        capped: state.devices.values().any(|d| d.discovered_capped),
        window_minutes: DISCOVERED_WINDOW_S / 60,
        scanning: state.devices.values().any(|d| d.online),
    }
}

fn in_scan_window(b: &crate::state::BeaconState, now: i64) -> bool {
    match b.last_snapshot_at {
        Some(at) => now - at <= DISCOVERED_WINDOW_S,
        // Never seen in a snapshot this process handled — a row restored
        // from nothing. It cannot be inside the window.
        None => false,
    }
}

fn discovered_row(
    device: &crate::state::DeviceState,
    b: &crate::state::BeaconState,
    signal_bands: &crate::config::SignalQualityConfig,
    now: i64,
) -> DiscoveredCtx {
    DiscoveredCtx {
        mac: b.mac.clone(),
        mac_id: b.mac.replace(':', "-"),
        name: b.name.clone(),
        temperature: b.temperature,
        humidity: b.humidity,
        rssi: b.rssi,
        signal: beacon_signal_level(b.rssi, signal_bands),
        hub: device
            .system
            .device_label
            .clone()
            .unwrap_or_else(|| device.hostname.clone()),
        last_seen: b.last_snapshot_at.and_then(unix_to_rfc3339),
        age_s: b.last_snapshot_at.map(|at| (now - at).max(0)),
    }
}

/// Strongest signal first, so the beacon in the operator's hand is at the top.
fn sort_discovered(rows: &mut [DiscoveredCtx]) {
    rows.sort_by(|a, b| match (a.rssi, b.rssi) {
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.mac.cmp(&b.mac)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.mac.cmp(&b.mac),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::test_support::{
        bare_beacon, beacon, db_threshold, db_threshold_with_bounds, scan_device,
    };

    /// GitLab #80 on the Beacon list, the twin of the Node row's test: a band
    /// the operator switched off reads as "no bounds set" in the Meze column.
    /// Both quantities are asserted, because a Beacon's row carries two
    /// gauges and the template sets them up one line apart.
    #[test]
    fn a_switched_off_band_draws_no_bands_in_the_beacon_row() {
        let mut b = beacon("AA:BB:CC:DD:EE:01");
        b.alarm_state = "NORMAL".to_string();
        b.temperature = Some(4.0);
        b.humidity = Some(41);

        let render = |enabled: bool| {
            let mut rows = vec![
                db_threshold_with_bounds("temperature", enabled),
                db_threshold_with_bounds("humidity", enabled),
            ];
            for r in &mut rows {
                r.scope = "eye".to_string();
                r.entity = b.mac.clone();
            }
            let ctx = beacon_ctx(BeaconCardInputs {
                thresholds: &rows,
                ..bare_beacon(&b)
            });
            let templates = Templates::new("templates", "static");
            let html = templates
                .env()
                .get_template("pages/beacons.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    current_user => None::<CurrentUser>,
                    hostname => "h1",
                    beacons => vec![ctx],
                    can_edit => false,
                })
                .expect("beacons page renders");
            let row_at = html.find(r#"<tr id="beacon-"#).expect("the row");
            let row = &html[row_at..][..html[row_at..].find("</tr>").expect("row ends")];
            let cell_at = row.find(r#"data-column="bounds""#).expect("the Meze cell");
            row[cell_at..][..row[cell_at..].find("</td>").expect("cell ends")].to_string()
        };

        let on = render(true);
        assert_eq!(on.matches("bg-critical").count(), 4, "two gauges: {on}");

        let off = render(false);
        assert!(!off.contains("bg-critical"), "no bands: {off}");
        assert_eq!(
            off.matches("bg-muted").count(),
            2,
            "an empty bar for each quantity: {off}"
        );
    }

    /// The protocol byte is named from the BTSMP1 table, and a code outside
    /// it is kept as a code rather than guessed at.
    #[test]
    fn the_protocol_byte_is_named_from_the_btsmp1_table() {
        let named: Vec<_> = (0..=5).map(BeaconProtocol::from_code).collect();
        assert_eq!(
            named,
            [
                BeaconProtocol::Ibeacon,
                BeaconProtocol::Eddystone,
                BeaconProtocol::EyeSensor,
                BeaconProtocol::IbeaconEyeSensor,
                BeaconProtocol::EddystoneEyeSensor,
                BeaconProtocol::Unknown,
            ]
        );
        assert_eq!(
            serde_json::to_value(BeaconProtocol::IbeaconEyeSensor).expect("serialize"),
            "ibeacon_eye_sensor",
            "the template branches on the snake_case name"
        );
    }

    /// Every sensor of the mask gets a row, on or off, in bit order; a name
    /// the table does not know still shows, and a recorder (no mask at all)
    /// is not turned into four "off" rows.
    /// A Beacon that has no active-sensor mask (older firmware) still gets
    /// its four rows — from what its advertisement carries — and says so;
    /// one nobody has heard yet keeps "senzory dané typem beaconu".
    #[test]
    fn a_beacon_without_a_mask_lists_the_sensors_it_broadcasts() {
        let read = fiber_protocol::payloads::eye::EyeBleConfig {
            recording: None,
            logging_interval_min: None,
            advertising_interval_ms: Some(10_000),
            tx_power_dbm: Some(8),
            protocol_type: Some(2),
            protocol: Some("eye_sensor".into()),
            sensors: None,
            active_sensors: None,
            read_at: None,
            verified: Some(true),
        };
        let mut b = beacon("7C:D9:F4:12:50:88");
        b.temperature = Some(27.4);
        b.humidity = Some(92);
        b.magnet_present = Some(true);
        b.moving = Some(false);
        let ctx = with_broadcast_sensors(BleConfigCtx::from(&read), &b);
        assert!(ctx.sensors_from_broadcast);
        let rows: Vec<_> = ctx
            .sensors
            .expect("rows from the broadcast")
            .into_iter()
            .map(|r| (r.code, r.active))
            .collect();
        assert_eq!(
            rows,
            [
                ("temperature".to_string(), true),
                ("humidity".to_string(), true),
                ("magnet".to_string(), true),
                ("movement".to_string(), true),
            ]
        );

        let silent =
            with_broadcast_sensors(BleConfigCtx::from(&read), &beacon("7C:D9:F4:12:50:89"));
        assert!(silent.sensors.is_none() && !silent.sensors_from_broadcast);

        // A read mask always wins over the broadcast.
        let masked = fiber_protocol::payloads::eye::EyeBleConfig {
            recording: None,
            logging_interval_min: None,
            active_sensors: Some(vec!["temperature".into()]),
            ..read
        };
        let ctx = with_broadcast_sensors(BleConfigCtx::from(&masked), &b);
        assert!(!ctx.sensors_from_broadcast);
        assert_eq!(ctx.sensors.map(|r| r[1].active), Some(false));
    }

    #[test]
    fn the_sensor_rows_list_every_sensor_of_the_mask_on_or_off() {
        let ble = |active_sensors| {
            BleConfigCtx::from(&fiber_protocol::payloads::eye::EyeBleConfig {
                recording: None,
                logging_interval_min: None,
                advertising_interval_ms: Some(10_000),
                tx_power_dbm: Some(4),
                protocol_type: Some(2),
                protocol: None,
                sensors: None,
                active_sensors,
                read_at: None,
                verified: None,
            })
        };
        let rows = ble(Some(vec![
            "humidity".into(),
            "temperature".into(),
            "light".into(),
        ]))
        .sensors
        .expect("a mask was read");
        let rows: Vec<_> = rows.iter().map(|r| (r.code.as_str(), r.active)).collect();
        assert_eq!(
            rows,
            [
                ("temperature", true),
                ("humidity", true),
                ("magnet", false),
                ("movement", false),
                ("light", true),
            ]
        );
        let none = ble(Some(vec![])).sensors.expect("an empty mask is a mask");
        assert!(none.iter().all(|r| !r.active) && none.len() == 4);
        assert!(ble(None).sensors.is_none(), "a recorder has no mask");

        // A current Hub sends every bit as a map; the rows come out the same way.
        let from_map = BleConfigCtx::from(&fiber_protocol::payloads::eye::EyeBleConfig {
            recording: None,
            logging_interval_min: None,
            advertising_interval_ms: Some(10_000),
            tx_power_dbm: Some(4),
            protocol_type: Some(2),
            protocol: Some("eye_sensor".into()),
            sensors: Some(
                [
                    ("humidity", true),
                    ("magnet", false),
                    ("movement", true),
                    ("temperature", true),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            ),
            active_sensors: None,
            read_at: None,
            verified: None,
        });
        let rows: Vec<_> = from_map
            .sensors
            .expect("a map was read")
            .into_iter()
            .map(|r| (r.code, r.active))
            .collect();
        assert_eq!(
            rows,
            [
                ("temperature".to_string(), true),
                ("humidity".to_string(), true),
                ("magnet".to_string(), false),
                ("movement".to_string(), true),
            ]
        );
    }

    /// The Beacon list runs the same toolbar off the same script, so it has
    /// the same two ways to break — a missing attribute, and a head that has
    /// drifted from the row. The column sets differ (no Protocol, no SNR, no
    /// firmware; a Hub checklist where the Node has gateways), which is
    /// exactly why the width is asserted per page rather than once.
    #[test]
    fn the_beacons_page_and_its_rows_agree_on_the_toolbar_contract() {
        let mut b = beacon("AA:BB:CC:DD:EE:01");
        b.alarm_state = "NORMAL".to_string();
        b.temperature = Some(4.0);
        b.humidity = Some(41);
        b.location = Some("Sklad B".to_string());
        let ctx = beacon_ctx(bare_beacon(&b));

        let templates = Templates::new("templates", "static");
        let html = templates
            .env()
            .get_template("pages/beacons.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                current_user => None::<CurrentUser>,
                hostname => "h1",
                beacons => vec![ctx],
                can_edit => false,
            })
            .expect("beacons page renders");

        assert!(html.contains(r#"data-list-key="beacons""#), "the wrapper");
        assert!(html.contains("data-list-search"), "the fulltext box");
        assert!(
            !html.contains(r#"data-list-filter="proto""#),
            "every Beacon is BLE — no Protocol control (§2.2)"
        );

        // No `state`: §2.3 has no Stav column — the value class is the
        // strip and the coloured values, as on /nodes — and the lifecycle
        // labels it used to carry sit under the identity now.
        for col in [
            "loc", "overview", "values", "bounds", "hub", "seen", "rssi", "snr", "batt", "fw",
            "proto",
        ] {
            assert!(
                html.contains(&format!(r#"data-col="{col}""#)),
                "the {col} checkbox"
            );
            assert!(
                html.matches(&format!(r#"data-column="{col}""#)).count() >= 2,
                "the {col} head cell and its body cell"
            );
        }
        assert_eq!(
            html.matches(r#"data-col-default="1""#).count(),
            5,
            "the unified table's five columns default to visible, the home Hub \
             (decision #42) among them; the diagnostic tail does not"
        );

        let row_start = html.find(r#"id="beacon-h1-"#).expect("the row is rendered");
        let row = &html[row_start..][..html[row_start..].find("</tr>").expect("row ends")];
        assert!(row.contains(r#"data-oor="normal""#), "the value class");
        assert!(
            row.contains(r#"data-location="Sklad B""#),
            "the location key"
        );

        let head_start = html.find("<thead").expect("a head");
        let head = &html[head_start..][..html[head_start..].find("</thead>").expect("head ends")];
        assert_eq!(
            head.matches("<th ").count(),
            row.matches("<td").count(),
            "the head and the row must declare the same number of columns"
        );
    }

    /// GitLab #80: a band the operator switched off cannot colour its field
    /// and cannot hold the row's value state up, whatever the device echoes.
    /// The device keeps evaluating until the `delete_eye_field_threshold`
    /// reaches it (and a Hub that never answers keeps evaluating for good),
    /// so honouring the flag is the portal's own job.
    #[test]
    fn a_switched_off_band_neither_colours_its_field_nor_raises_the_row() {
        let mut b = beacon("AA:BB:CC:DD:EE:FF");
        b.alarm_state = "WARNING".to_string();
        b.humidity = Some(88);
        b.temperature = Some(4.2);
        b.field_alarm_states
            .insert("humidity".to_string(), "WARNING".to_string());

        let mut off = db_threshold("humidity", Some(80.0), false);
        off.scope = "eye".to_string();

        let ctx = beacon_ctx(BeaconCardInputs {
            thresholds: std::slice::from_ref(&off),
            ..bare_beacon(&b)
        });
        let humidity = ctx
            .fields
            .iter()
            .find(|f| f.name == "humidity")
            .expect("the field");
        assert_eq!(humidity.alarm_state, "Normal", "the field reads normal");
        assert_eq!(
            ctx.alarm_state, "Normal",
            "and it is not the row's state either"
        );

        // Ticked back on, the same echo counts again.
        let mut on = off.clone();
        on.enabled = true;
        let ctx = beacon_ctx(BeaconCardInputs {
            thresholds: std::slice::from_ref(&on),
            ..bare_beacon(&b)
        });
        assert_eq!(ctx.alarm_state, "Warning");
    }

    #[test]
    fn beacon_ctx_maps_state_and_wraps_db_thresholds() {
        let mut b = beacon("AA:BB:CC:DD:EE:FF");
        b.alarm_state = "WARNING".to_string();
        b.temperature = Some(4.2);
        let db = vec![db_threshold("temperature", Some(10.0), true)];

        let ctx = beacon_ctx(BeaconCardInputs {
            thresholds: &db,
            ..bare_beacon(&b)
        });
        assert_eq!(ctx.alarm_state, "Warning");
        assert!(ctx.connected);
        assert!(!ctx.discovered);
        let thresholds = ctx.thresholds.expect("thresholds present");
        assert_eq!(thresholds["temperature"].warning_high, Some(10.0));
    }

    #[test]
    fn beacon_ctx_no_db_rows_is_none_not_empty_map() {
        let b = beacon("AA:BB:CC:DD:EE:FF");
        let ctx = beacon_ctx(bare_beacon(&b));
        assert!(ctx.thresholds.is_none());
    }

    #[test]
    fn beacon_ctx_discovered_none_reads_as_registered() {
        let b = beacon("AA:BB:CC:DD:EE:FF"); // discovered: None by default
        let ctx = beacon_ctx(bare_beacon(&b));
        assert!(!ctx.discovered);
    }

    // ---- B6 (§9 WP6.3): the auto-provisioning states ------------------

    /// devices.beacon.md §3.5's visible arc, and the two states that show
    /// nothing. The wire vocabulary is `control-ble`'s
    /// (`host.rs`: pending | provisioning | provisioned | failed |
    /// read_only), so this is the one place the two vocabularies meet.
    #[test]
    fn provisioning_states_map_to_the_six_visible_labels() {
        assert_eq!(provisioning_state(Some("pending")), Some("waiting"));
        assert_eq!(provisioning_state(Some("provisioning")), Some("running"));
        assert_eq!(provisioning_state(Some("provisioned")), Some("configured"));
        assert_eq!(provisioning_state(Some("differs")), Some("differs"));
        assert_eq!(provisioning_state(Some("failed")), Some("failed"));
        assert_eq!(provisioning_state(Some("read_only")), Some("read_only"));
        // A state this portal has never heard of is not a fault, and a Hub
        // older than B6 says nothing at all.
        assert_eq!(provisioning_state(Some("something_new")), None);
        assert_eq!(provisioning_state(None), None);
    }

    /// Both templates build `monitoring.beacon.prov_{state}` dynamically, so
    /// `every_static_template_key_exists` cannot see any of them — the same
    /// hole F9 found on the monitoring side, and the same fix: enumerate the
    /// closed set the code itself declares.
    #[test]
    fn every_beacon_provisioning_key_exists_in_both_catalogues() {
        let mut keys: Vec<String> = [
            "pending",
            "provisioning",
            "provisioned",
            "differs",
            "failed",
            "read_only",
        ]
        .iter()
        .filter_map(|raw| provisioning_state(Some(raw)))
        .map(|state| format!("monitoring.beacon.prov_{state}"))
        .collect();
        keys.push("monitoring.beacon.prov_no_reason".to_string());
        keys.push("monitoring.beacon.waiting_first_message".to_string());
        assert_eq!(keys.len(), 8);

        for key in keys {
            for lang in crate::i18n::LANGS {
                assert_ne!(
                    crate::i18n::lookup(lang, &key),
                    key,
                    "{key} is missing from the {lang} catalogue"
                );
            }
        }

        // The Czech wording the spec uses (§3.5 addendum 2026-09-23, §2.5), verbatim.
        for (key, cs) in [
            ("monitoring.beacon.prov_running", "konfigurace probíhá"),
            ("monitoring.beacon.prov_configured", "nakonfigurovaný"),
            ("monitoring.beacon.prov_differs", "odlišná konfigurace"),
            ("monitoring.beacon.prov_failed", "konfigurace selhala"),
        ] {
            assert_eq!(crate::i18n::lookup("cs", key), cs);
        }
        assert_eq!(
            crate::i18n::lookup("cs", "monitoring.beacon.waiting_first_message"),
            "zaregistrováno — čeká na první zprávu"
        );
    }

    /// §2.5: a registered Beacon nobody has switched on yet is an ordinary
    /// state, not a fault and not a countdown. It has to be distinguishable
    /// from one that has reported and then gone quiet.
    #[test]
    fn a_beacon_that_has_never_reported_says_it_is_waiting() {
        let never = beacon("AA:BB:CC:DD:EE:FF");
        assert!(beacon_ctx(bare_beacon(&never)).waiting_for_first_message);

        let mut heard = beacon("AA:BB:CC:DD:EE:FF");
        heard.last_seen = Some(1_768_314_600);
        heard.alarm_state = "NORMAL".to_string();
        assert!(!beacon_ctx(bare_beacon(&heard)).waiting_for_first_message);
    }

    // ---- B6 (§9 WP6.1): the passive-scan panel ------------------------

    /// `discovered: null` is **registered** — the wire's stated trap, and
    /// the one that would put a customer's whole fleet on the scan table.
    #[test]
    fn a_null_discovered_reads_as_registered() {
        let now = 1_768_314_600;
        let panel = discovered_panel(&scan_device(now), &Default::default(), now);
        assert!(
            panel.rows.iter().all(|r| r.mac != "AA:BB:CC:DD:EE:01"),
            "a registered Beacon must not appear as a scan candidate"
        );
    }

    /// The 15-minute window of devices.beacon.md §4, applied at render time
    /// against the **store** clock.
    #[test]
    fn a_discovered_beacon_older_than_the_window_is_not_listed() {
        let now = 1_768_314_600;
        let panel = discovered_panel(&scan_device(now), &Default::default(), now);
        assert_eq!(
            panel
                .rows
                .iter()
                .map(|r| r.mac.as_str())
                .collect::<Vec<_>>(),
            vec!["AA:BB:CC:DD:EE:02"],
            "only the stranger inside the window belongs on the table"
        );
        assert_eq!(panel.window_minutes, 15);
        assert!(panel.scanning, "the Hub is online, so it is listening");

        // A beacon this process has never seen in a snapshot has no
        // store-clock stamp and cannot be inside any window.
        let mut device = scan_device(now);
        device
            .ble_sensors
            .get_mut("AA:BB:CC:DD:EE:02")
            .expect("the stranger")
            .last_snapshot_at = None;
        assert!(discovered_panel(&device, &Default::default(), now)
            .rows
            .is_empty());
    }

    /// UI-12: "the difference between 'a quiet site' and 'we are dropping
    /// neighbours' must not be silent."
    #[test]
    fn discovered_capped_reaches_the_panel() {
        let now = 1_768_314_600;
        let mut device = scan_device(now);
        assert!(!discovered_panel(&device, &Default::default(), now).capped);
        device.discovered_capped = true;
        assert!(discovered_panel(&device, &Default::default(), now).capped);
    }

    // ---- B5 (§8 WP5.2, §20): the row data the Beacon list was missing ----

    /// devices.beacon.md §2.2 puts the operator's description in the
    /// fulltext filter beside the MAC. It is a `device_meta` column, so it
    /// only reaches the row through the bulk `meta` map — which is the
    /// §15.7 lesson this test exists to hold.
    #[test]
    fn beacon_ctx_carries_description_and_created_at_from_device_meta() {
        let b = beacon("AA:BB:CC:DD:EE:FF");
        let meta = portal_storage::DeviceMeta {
            hostname: "h1".into(),
            scope: portal_storage::THRESHOLD_SCOPE_EYE.into(),
            entity: "AA:BB:CC:DD:EE:FF".into(),
            name: None,
            description: Some("mrazak sklad".into()),
            created_at: "2026-09-07T10:00:00Z".into(),
            added_by: Some("manager1".into()),
            offline_k_override: None,
            backfill_auto: true,
            store_interval_override: Some(5),
            serial: None,
            location: None,
        };
        let ctx = beacon_ctx(BeaconCardInputs {
            meta: Some(&meta),
            ..bare_beacon(&b)
        });
        assert_eq!(ctx.description.as_deref(), Some("mrazak sklad"));
        assert_eq!(ctx.created_at.as_deref(), Some("2026-09-07T10:00:00Z"));
        // The whole row travels to the detail page's system-configuration
        // half, not just the two fields the list reads.
        assert_eq!(ctx.meta.added_by.as_deref(), Some("manager1"));
    }

    /// The Beacon fault axis, which before B5 had no way of reaching a
    /// Beacon at all: `beacon_ctx` took no liveness input, so B2's Beacon
    /// offline alert (§5) was raised into the notifier and rendered
    /// nowhere. Precedence is offline first — see `beacon_fault`.
    #[test]
    fn beacon_ctx_fault_prefers_offline_over_low_battery() {
        let mut b = beacon("AA:BB:CC:DD:EE:FF");
        b.low_battery = Some(true);
        b.last_seen = Some(1_736_775_000);
        b.alarm_state = "NORMAL".to_string();

        let healthy = beacon_ctx(BeaconCardInputs {
            offline: Some(false),
            ..bare_beacon(&b)
        });
        assert_eq!(healthy.fault, Some(BeaconFault::LowBattery));
        assert!(healthy.connected);
        assert!(
            healthy.fault_since.is_none(),
            "a low battery has no moment the portal knows"
        );

        let silent = beacon_ctx(BeaconCardInputs {
            offline: Some(true),
            ..bare_beacon(&b)
        });
        assert_eq!(silent.fault, Some(BeaconFault::Offline));
        assert!(!silent.connected, "the detector overrides the Hub echo");
        assert!(silent.fault_since.is_some());
    }

    /// devices.beacon.md §2.3: "u BLE bez SNR — mapovani jen z RSSI".
    /// `signal_level` would answer `Unknown` for every Beacon forever,
    /// because a BLE advertisement carries no SNR to clear the floor with.
    #[test]
    fn beacon_signal_reads_the_rssi_half_of_the_ladder_alone() {
        let bands = crate::config::SignalQualityConfig::default();
        assert_eq!(
            beacon_signal_level(Some(-60), &bands),
            SignalLevel::Excellent
        );
        assert_eq!(beacon_signal_level(Some(-90), &bands), SignalLevel::Good);
        assert_eq!(beacon_signal_level(Some(-100), &bands), SignalLevel::Weak);
        assert_eq!(
            beacon_signal_level(Some(-120), &bands),
            SignalLevel::Critical
        );
        assert_eq!(beacon_signal_level(None, &bands), SignalLevel::Unknown);
        // The Node ladder on the same RSSI is Unknown, which is the whole
        // reason this function exists.
        assert_eq!(signal_level(Some(-60), None, &bands), SignalLevel::Unknown);
    }

    // ---- B4 (devices.backend.md §7) --------------------------------------

    /// devices.beacon.md §3.2: a Beacon is a broadcast, so several Hubs
    /// hearing it at once is the normal state. Strongest first; a Hub with
    /// no RSSI sorts last rather than displacing a measured link.
    #[test]
    fn beacon_hubs_are_sorted_strongest_first_with_unmeasured_last() {
        let mut state = crate::state::PortalState::default();
        for (host, rssi) in [("hub-c", None), ("hub-a", Some(-80)), ("hub-b", Some(-60))] {
            let device = state
                .devices
                .entry(host.to_string())
                .or_insert_with(|| crate::state::DeviceState::new(host.to_string()));
            let mut b = beacon("AA:BB:CC:DD:EE:FF");
            b.rssi = rssi;
            b.connected = true;
            device.ble_sensors.insert(b.mac.clone(), b);
        }

        let hubs = beacon_hubs(&state, "AA:BB:CC:DD:EE:FF");
        let order: Vec<&str> = hubs.iter().map(|h| h.hostname.as_str()).collect();
        assert_eq!(order, ["hub-b", "hub-a", "hub-c"]);
        assert!(hubs[0].best);
        assert!(!hubs[1].best);
        assert!(hubs.iter().all(|h| h.connected));
        assert!(beacon_hubs(&state, "FF:FF:FF:FF:FF:FF").is_empty());
    }

    /// Decision #42: in a cluster the home is the Hub the Server named, not the strongest one
    /// (hysteresis keeps it while another is only slightly better); the row's `home_hub` and
    /// the detail's badge read it.
    #[test]
    fn the_home_hub_is_the_one_the_server_named() {
        let mut state = crate::state::PortalState::default();
        for (host, rssi) in [("c1", -70), ("s1", -66)] {
            let device = state
                .devices
                .entry(host.to_string())
                .or_insert_with(|| crate::state::DeviceState::new(host.to_string()));
            let mut b = beacon("AA:BB:CC:DD:EE:FF");
            b.rssi = Some(rssi);
            b.connected = true;
            device.ble_sensors.insert(b.mac.clone(), b);
        }
        // Outside a cluster every Hub that registered it is "home" (the single-Hub case).
        assert!(beacon_hubs(&state, "AA:BB:CC:DD:EE:FF")
            .iter()
            .all(|h| h.home));

        state.beacon_cluster = Some(crate::state::models::BeaconCluster::from_msg(
            &fiber_protocol::payloads::eye::EyeHomesMsg {
                v: 1,
                server: "s1".into(),
                registered: vec!["AA:BB:CC:DD:EE:FF".into()],
                homes: [("AA:BB:CC:DD:EE:FF".to_string(), "c1".to_string())].into(),
            },
        ));
        let hubs = beacon_hubs(&state, "AA:BB:CC:DD:EE:FF");
        assert_eq!(hubs[0].hostname, "s1");
        assert!(hubs[0].best && !hubs[0].home, "strongest, not home");
        assert!(hubs[1].home, "c1 is home");
        let snapshot = state.devices["s1"].ble_sensors["AA:BB:CC:DD:EE:FF"].clone();
        let ctx = beacon_ctx(BeaconCardInputs {
            hubs,
            ..bare_beacon(&snapshot)
        });
        assert_eq!(ctx.home_hub.as_deref(), Some("c1"));
    }

    /// `ble_sensors` accumulates: a Hub that heard one advertisement keeps
    /// its entry forever, so before the B5 review the strongest such entry
    /// was labelled "strongest" indefinitely. The row stays — its RSSI and
    /// last-seen stamp are the record of when the Beacon was last heard —
    /// but it sorts below every live Hub and can never be `best`.
    #[test]
    fn a_hub_that_no_longer_hears_the_beacon_sorts_last_and_is_never_best() {
        let mut state = crate::state::PortalState::default();
        // The stale Hub has the *better* RSSI, so RSSI order alone would
        // put it first.
        for (host, rssi, connected) in [
            ("hub-stale", Some(-50), false),
            ("hub-live", Some(-85), true),
        ] {
            let device = state
                .devices
                .entry(host.to_string())
                .or_insert_with(|| crate::state::DeviceState::new(host.to_string()));
            let mut b = beacon("AA:BB:CC:DD:EE:FF");
            b.rssi = rssi;
            b.connected = connected;
            device.ble_sensors.insert(b.mac.clone(), b);
        }

        let hubs = beacon_hubs(&state, "AA:BB:CC:DD:EE:FF");
        let order: Vec<&str> = hubs.iter().map(|h| h.hostname.as_str()).collect();
        assert_eq!(order, ["hub-live", "hub-stale"]);
        assert!(hubs[0].best, "the live Hub is the best one");
        assert!(!hubs[1].best);
        assert!(!hubs[1].connected);

        // Nobody hears it any more: the rows survive, and none is `best`.
        for device in state.devices.values_mut() {
            device
                .ble_sensors
                .get_mut("AA:BB:CC:DD:EE:FF")
                .unwrap()
                .connected = false;
        }
        let hubs = beacon_hubs(&state, "AA:BB:CC:DD:EE:FF");
        assert_eq!(hubs.len(), 2);
        assert!(hubs.iter().all(|h| !h.best));
    }

    /// The fixed T + H pair (devices.beacon.md §3.4), with the per-field
    /// value class the three-state "out of range" filter reads client-side.
    /// A field the Beacon has not reported keeps its row with a `None`
    /// value rather than vanishing — the table has exactly two rows.
    #[test]
    fn beacon_ctx_fields_are_the_fixed_pair_with_their_value_class() {
        let mut b = beacon("AA:BB:CC:DD:EE:FF");
        b.temperature = Some(21.5);
        b.field_alarm_states
            .insert("temperature".into(), "CRITICAL".into());

        let ctx = beacon_ctx(bare_beacon(&b));
        let names: Vec<&str> = ctx.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["temperature", "humidity"]);
        assert_eq!(ctx.fields[0].alarm_state, "Critical");
        assert_eq!(ctx.fields[0].value, Some(21.5));
        assert!(ctx.fields[1].value.is_none());
        assert_eq!(
            ctx.fields[1].alarm_state, "Normal",
            "no state reported is not a warning"
        );
    }

    /// devices.beacon.md §2.3 asks for `x.yy V`, the same shape the group
    /// row already formats a Node's voltage into.
    #[test]
    fn beacon_ctx_battery_is_volts_not_millivolts() {
        let mut b = beacon("AA:BB:CC:DD:EE:FF");
        b.battery_mv = Some(2904);
        assert_eq!(
            beacon_ctx(bare_beacon(&b)).battery.as_deref(),
            Some("2.90 V")
        );
        let b = beacon("AA:BB:CC:DD:EE:FF");
        assert!(beacon_ctx(bare_beacon(&b)).battery.is_none());
    }

    // --- M6-F alarm ctx helpers ---

    /// The Beacon twin of `the_node_row_draws_a_fault_triangle_with_a
    /// _translated_tooltip`. Three things at once, because all three are
    /// row markup the list had no way to produce before B5: the fault
    /// triangle (§5's Beacon offline alert reaching a page at last), the
    /// link into the detail page, and the operator's description in the
    /// fulltext haystack.
    #[test]
    fn the_beacon_row_links_to_its_detail_page_and_draws_its_fault() {
        let mut b = beacon("AA:BB:CC:DD:EE:FF");
        b.alarm_state = "NORMAL".to_string();
        let meta = portal_storage::DeviceMeta {
            hostname: "h1".into(),
            scope: portal_storage::THRESHOLD_SCOPE_EYE.into(),
            entity: "AA:BB:CC:DD:EE:FF".into(),
            name: None,
            description: Some("mrazak sklad".into()),
            created_at: "2026-09-07T10:00:00Z".into(),
            added_by: None,
            offline_k_override: None,
            backfill_auto: true,
            store_interval_override: None,
            serial: None,
            location: None,
        };
        let templates = Templates::new("templates", "static");
        let render = |ctx: BeaconCtx| {
            templates
                .env()
                .get_template("partials/beacon_card.html")
                .expect("template loads")
                .render(context! { lang => "en", hostname => "h1", can_edit => true, b => ctx })
                .expect("row renders")
                .replace("&#x2f;", "/")
        };

        let healthy = render(beacon_ctx(BeaconCardInputs {
            meta: Some(&meta),
            ..bare_beacon(&b)
        }));
        assert!(!healthy.contains("data-fault"), "no fault, no indicator");
        assert!(
            healthy.contains(r#"href="/beacons/AA:BB:CC:DD:EE:FF/overview""#),
            "the row is the way into the detail page: {healthy}"
        );
        assert!(
            healthy.contains("mrazak sklad"),
            "the description is in data-search (§2.2's fulltext): {healthy}"
        );
        // devices.beacon.md §1: no tag-type column, no capability badge.
        assert!(!healthy.contains("EN 12830"));

        let silent = render(beacon_ctx(BeaconCardInputs {
            offline: Some(true),
            ..bare_beacon(&b)
        }));
        assert!(silent.contains(r#"data-fault="offline""#));
        assert!(
            silent.contains("Beacon is not broadcasting"),
            "the tooltip is translated, not the raw key: {silent}"
        );
        assert!(!silent.contains("monitoring.beacons.fault_"));

        let mut flat = b.clone();
        flat.low_battery = Some(true);
        let flat = render(beacon_ctx(BeaconCardInputs {
            offline: Some(false),
            ..bare_beacon(&flat)
        }));
        assert!(flat.contains(r#"data-fault="low_battery""#));
        assert!(flat.contains("Low battery"));
    }

    /// The tooltip and the signal rung both go through dynamic keys, which
    /// `every_static_template_key_exists` cannot see — the hole F9 found on
    /// the monitoring side and §19.2 hit again on the Node fault.
    #[test]
    fn every_beacon_fault_and_signal_key_exists_in_both_catalogues() {
        let mut keys = Vec::new();
        for fault in [BeaconFault::Offline, BeaconFault::LowBattery] {
            let slug = serde_json::to_value(fault)
                .expect("serialises")
                .as_str()
                .expect("as a string")
                .to_string();
            keys.push(format!("monitoring.beacons.fault_{slug}"));
        }
        for level in [
            SignalLevel::Excellent,
            SignalLevel::Good,
            SignalLevel::Weak,
            SignalLevel::Critical,
            SignalLevel::Unknown,
        ] {
            let slug = serde_json::to_value(level)
                .expect("serialises")
                .as_str()
                .expect("as a string")
                .to_string();
            keys.push(format!("monitoring.signal.{slug}"));
        }
        for key in keys {
            for lang in crate::i18n::LANGS {
                assert_ne!(
                    crate::i18n::lookup(lang, &key),
                    key,
                    "{key} is missing from the {lang} catalogue"
                );
            }
        }
    }

    /// The detail page renders from its context alone (§8's *Accept:*), and
    /// the two halves of §3.5 are both on it — the editable system half and
    /// the read-only BLE half, the latter with no reconfiguration button
    /// (that request flow is B6).
    #[test]
    fn the_beacon_detail_page_renders_both_configuration_halves() {
        let mut b = beacon("AA:BB:CC:DD:EE:FF");
        b.alarm_state = "NORMAL".to_string();
        b.temperature = Some(4.2);
        b.rssi = Some(-70);
        b.connected = true;
        let templates = Templates::new("templates", "static");

        let mut state = crate::state::PortalState::default();
        let device = state
            .devices
            .entry("h1".to_string())
            .or_insert_with(|| crate::state::DeviceState::new("h1".to_string()));
        device.ble_sensors.insert(b.mac.clone(), b.clone());

        let ctx = beacon_ctx(BeaconCardInputs {
            hubs: beacon_hubs(&state, &b.mac),
            ..bare_beacon(&b)
        });
        // The page is tabbed since 2026-09-15, so each half is asked for on
        // the tab that holds it.
        let render_tab = |tab: &str| {
            templates
                .env()
                .get_template("pages/beacon_detail.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    hostname => "h1",
                    nav_active => "beacons",
                    can_edit => true,
                    offline_k_default => 1,
                    active_tab => tab,
                    b => ctx.clone(),
                })
                .expect("page renders")
                .replace("&#x2f;", "/")
        };
        let overview = render_tab("overview");
        let settings = render_tab("settings");
        let diagnostics = render_tab("diagnostics");

        // §3.1: the MAC identifies the device in the header, on every tab.
        assert!(overview.contains("AA:BB:CC:DD:EE:FF"));
        // §3.2's Hub table is a real table even with the dev fleet's one Hub.
        assert!(diagnostics.contains("Heard by Hubs"));
        assert!(diagnostics.contains("Strongest first"));
        // Half 1, editable and immediate.
        assert!(settings.contains(r#"hx-post="/beacons/AA:BB:CC:DD:EE:FF/device-config""#));
        assert!(
            !settings.contains("storage interval"),
            "no Portal store-interval override any more (storage_intervals.md S4)"
        );
        // Half 2, read-only placeholders.
        assert!(settings.contains("Settings in device"));
        assert!(settings.contains("not reported by the Hub yet"));
        // ...and, since B6, the one action §3.5 allows; in the locked
        // Service functions card since 2026-10-15 (ui_components.md §7).
        assert!(
            settings.contains(r#"hx-post="/beacons/AA:BB:CC:DD:EE:FF/reconfigure""#)
                && settings.contains("data-service-card"),
            "the reconfiguration request sits in the Service functions card"
        );
        // History has its own tab since beacon_history.md §7.1, the Node's
        // card: quick choices and the window form post to the history route,
        // and the table is the shared partial under this Beacon's swap id.
        let history = render_tab("history");
        assert!(history.contains(r#"hx-post="/beacons/AA:BB:CC:DD:EE:FF/history""#));
        assert!(history.contains(r#"id="beacon-history-AA-BB-CC-DD-EE-FF""#));
        assert!(history.contains(r#"sse-swap="beacon-history-AA-BB-CC-DD-EE-FF""#));
        assert!(!diagnostics.contains(r#"hx-post="/beacons/AA:BB:CC:DD:EE:FF/history""#));
        // The automatic download sits in the Device configuration card.
        assert!(settings.contains(r#"name="history""#));
        // And nothing of the removed concept survives.
        assert!(!overview.contains("EN 12830"));
    }

    fn read_back(
        tx_power_dbm: i64,
        sensors: Option<Vec<String>>,
        recording: Option<(bool, i64)>,
    ) -> fiber_protocol::payloads::eye::EyeBleConfig {
        fiber_protocol::payloads::eye::EyeBleConfig {
            recording: recording.map(|(on, _)| on),
            logging_interval_min: recording.map(|(_, min)| min),
            advertising_interval_ms: Some(DEFAULT_PROFILE_INTERVAL_MS),
            tx_power_dbm: Some(tx_power_dbm),
            protocol_type: Some(PROFILE_PROTOCOL_TYPE),
            protocol: None,
            sensors: None,
            active_sensors: sensors,
            read_at: Some(1_760_000_000),
            verified: Some(false),
        }
    }

    /// Hardware test 2026-10-09: `differs` must say which parameter differs.
    /// Only what was read is judged, against the default fleet profile.
    #[test]
    fn profile_differences_name_only_what_was_read_and_differs() {
        let th = Some(vec!["temperature".to_string(), "humidity".to_string()]);
        let diffs = |c| profile_differences(&BleConfigCtx::from(&c));
        assert!(diffs(read_back(8, th.clone(), Some((true, 15)))).is_empty());
        assert_eq!(
            diffs(read_back(4, th.clone(), Some((true, 5)))),
            ["tx_power", "recording"]
        );
        assert_eq!(
            diffs(read_back(8, th.clone(), Some((false, 15)))),
            ["recording"]
        );
        // A recorder has no mask and an unread recording is not judged.
        assert!(diffs(read_back(8, None, None)).is_empty());
        assert_eq!(
            diffs(read_back(8, Some(vec!["temperature".to_string()]), None)),
            ["sensors"]
        );
        let mut other = read_back(8, None, None);
        other.protocol_type = Some(0x01);
        other.advertising_interval_ms = Some(5_000);
        assert_eq!(diffs(other), ["advertising_interval", "protocol"]);
    }

    /// The BLE half shows the archive recording, and on `differs` the list of
    /// what differs; in no other state is the list drawn.
    #[test]
    fn the_ble_half_shows_recording_and_what_differs() {
        let templates = Templates::new("templates", "static");
        let render = |b: &BeaconState| {
            let ctx = beacon_ctx(bare_beacon(b));
            templates
                .env()
                .get_template("pages/beacon_detail.html")
                .expect("template loads")
                .render(context! {
                    lang => "cs",
                    hostname => "h1",
                    nav_active => "beacons",
                    can_edit => true,
                    store_interval_default => 15,
                    offline_k_default => 1,
                    active_tab => "settings",
                    b => ctx,
                })
                .expect("page renders")
        };
        let th = Some(vec!["temperature".to_string(), "humidity".to_string()]);

        let mut b = beacon("AA:BB:CC:DD:EE:FF");
        b.provisioning = Some("differs".to_string());
        b.ble_config = Some(read_back(4, th.clone(), Some((true, 5))));
        let page = render(&b);
        assert!(page.contains("Záznam do archivu"), "{page}");
        assert!(page.contains("zapnutý, 5 min"));
        assert!(page.contains("Od výchozího profilu flotily se liší:"));
        assert!(page.contains("vysílací síla (výchozí 8 dBm)"));
        assert!(page.contains("záznam do archivu (výchozí: zapnutý, 15 min)"));
        assert!(!page.contains("interval vysílání (výchozí 10 s)"));

        b.provisioning = Some("provisioned".to_string());
        b.ble_config = Some(read_back(8, th.clone(), Some((false, 15))));
        let page = render(&b);
        assert!(page.contains(">vypnutý</dd>"), "{page}");
        assert!(!page.contains("Od výchozího profilu flotily se liší:"));

        b.ble_config = Some(read_back(8, th, None));
        assert!(render(&b).contains("neznámo"));
    }
}
