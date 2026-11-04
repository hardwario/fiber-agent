//! In-memory portal state model — M1 subset, extended by M5-C.
//!
//! Mirrors `plan/v1/viewer/ws.md` "Data model" (`DeviceState` and
//! sub-models), scoped to what M1's handlers populate: `hostname`, `online`,
//! `version`, `first_seen`/`last_seen`, `sensors` (wired probes, 8 lines),
//! `power`, `standby`, `network`, `system` (nests `storage`), `config`.
//! M5-C adds the LoRaWAN Node (STICKER) and BLE beacon (EYE) sub-models:
//! `nodes`/`ble_sensors` plus the per-Node config/info/full-config
//! caches. M8 adds the `transport` axis (plan/viewer/p2p_nodes.md §1) so a
//! Node can be P2P as well as LoRaWAN — same map, same struct. Alarm
//! history and anything else still out of scope (design.md §2
//! phase-1 scope note; lands with M6 packages) stays deliberately absent —
//! add it in its own work package rather than stubbing fields nobody
//! populates yet.
//!
//! **Timestamp convention**: `first_seen`/`last_seen`/`*_updated_at` are
//! `i64` Unix seconds taken from the *store's own clock* when an event is
//! applied — not parsed from device-supplied timestamps. Device clocks may
//! be unsynced or absent, and parsing arbitrary ISO-8601 would pull in a
//! `chrono` dependency this crate doesn't otherwise need. Where a payload's
//! own ISO string is independently meaningful (e.g. `alarm_triggered_at`,
//! `standby.entered_at`), it's kept verbatim as a `String` rather than
//! parsed — simplest consistent choice for the M1 subset.

use std::collections::{BTreeMap, BTreeSet};

use fiber_protocol::payloads::eye::EyeBleConfig;
use fiber_protocol::payloads::system_info::{Hardware, LorawanGatewayInfo, Southbridge};
use portal_domain::{SessionState, Transport};

use super::radio::RadioGatewaysState;

/// Top-level in-memory state: one entry per device the portal has heard
/// from, plus the portal's own MQTT broker connection state.
#[derive(Debug, Clone, Default)]
pub struct PortalState {
    pub devices: BTreeMap<String, DeviceState>,
    pub mqtt: MqttConnectionState,
    /// The last `radio/gateways` report (devices.hub.md §12 A2), portal-wide rather than
    /// per device: the topic carries no hostname level and the report is cluster-wide (one
    /// broker, one report), with each row attributing itself through its own `hostname`.
    /// `None` until one arrives, which the card renders as "radio health unavailable".
    pub radio_gateways: Option<RadioGatewaysState>,
    /// The switched-off sensors, `(hostname, scope, entity, field)` with the hostname
    /// lowercase, scope `node` | `eye`, entity the node_key or the uppercase MAC (GitLab #120,
    /// `sensor_disabled`). Loaded from storage at start and kept by
    /// `StateStore::set_sensor_disabled`. Portal-wide rather than per device so it outlives a
    /// device entry. Every store write parks these sensors' values (`disabled_fields`).
    pub disabled_sensors: BTreeSet<(String, String, String, String)>,
    /// The cluster's Beacon registry and home Hubs, from the Server's retained `eye/homes`
    /// (decision #42). `None` until one arrives — a Hub whose agent predates #42, where every
    /// Beacon still belongs to the Hub that registered it.
    pub beacon_cluster: Option<BeaconCluster>,
    /// The Portal's own bounds of every wired line, `(hostname lowercase, line)` (decision
    /// #41: the Portal is the source of truth). Loaded from `field_thresholds` (scope `hub`)
    /// at start and kept by `StateStore::set_line_limits`; laid over every `config/state`
    /// echo, so the pages show what the Portal grades with even while the Hub still holds
    /// (or echoes) an older copy, is offline, or refused the command.
    pub line_limits: BTreeMap<(String, u8), SensorThresholds>,
    /// The Portal's own name and location of each Node and Beacon,
    /// `(hostname lowercase, scope node|eye, entity) -> (name, location)` (decision #41).
    /// Loaded from `device_meta` at start and kept by `StateStore::set_meta_label`; laid
    /// over every store write, so every page shows the Portal-stored text. `None` in either
    /// half inherits what the Hub reports.
    pub meta_labels: MetaLabels,
    /// What the Hub last reported under an overlaid name or location, same key, so clearing
    /// the Portal's text shows the Hub's at once instead of nothing.
    pub hub_labels: MetaLabels,
}

/// `(hostname, scope, entity) -> (name, location)`, see [`PortalState::meta_labels`].
pub type MetaLabels = BTreeMap<(String, String, String), (Option<String>, Option<String>)>;

/// The Server's `eye/homes`, normalized: hostnames lowercase, MACs uppercase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BeaconCluster {
    /// The registry: the Hub every Beacon key (`device_meta`, thresholds, readings, alarms,
    /// history jobs) and every registry command (`beacon-add`/`-remove`) belongs to.
    pub server: String,
    pub registered: std::collections::BTreeSet<String>,
    /// MAC → the Hub that hears it best, the only one that dials it.
    pub homes: BTreeMap<String, String>,
}

impl BeaconCluster {
    pub fn from_msg(msg: &fiber_protocol::payloads::eye::EyeHomesMsg) -> BeaconCluster {
        BeaconCluster {
            server: msg.server.trim().to_ascii_lowercase(),
            registered: msg
                .registered
                .iter()
                .map(|m| m.trim().to_uppercase())
                .collect(),
            homes: msg
                .homes
                .iter()
                .map(|(m, h)| (m.trim().to_uppercase(), h.trim().to_ascii_lowercase()))
                .collect(),
        }
    }

    pub fn is_registered(&self, mac: &str) -> bool {
        self.registered.contains(&mac.trim().to_uppercase())
    }

    pub fn home_of(&self, mac: &str) -> Option<&str> {
        self.homes
            .get(&mac.trim().to_uppercase())
            .map(String::as_str)
    }
}

/// Warned at most once per process, so a misconfigured pair of Hubs costs one
/// log line rather than one per request. See [`PortalState::hub_of_node`].
static DUPLICATE_NODE_WARNED: std::sync::Once = std::sync::Once::new();
static DUPLICATE_BEACON_WARNED: std::sync::Once = std::sync::Once::new();

/// Picks the owning Hub out of every Hub that claims `key`.
///
/// One match is the normal case. Several means two Hubs report the same
/// globally unique identity, which is a misconfiguration rather than a shape
/// the portal can resolve correctly. It takes the Hub with the newer
/// `last_seen`, because the fresher report is the one an operator is looking
/// at, and says so in the log once.
fn pick_owner<'a, T>(
    candidates: impl Iterator<Item = (&'a DeviceState, &'a T)>,
    warned: &std::sync::Once,
    kind: &'static str,
    key: &str,
) -> Option<(&'a DeviceState, &'a T)> {
    let mut best: Option<(&'a DeviceState, &'a T)> = None;
    let mut seen = 0usize;
    for (device, entity) in candidates {
        seen += 1;
        let better = match best {
            Some((current, _)) => device.last_seen > current.last_seen,
            None => true,
        };
        if better {
            best = Some((device, entity));
        }
    }
    if seen > 1 {
        warned.call_once(|| {
            tracing::warn!(
                kind,
                key,
                hubs = seen,
                "two Hubs report the same key; using the one with the newer last_seen (devices.hub.md §12 E)"
            );
        });
    }
    best
}

impl PortalState {
    /// The device with this hostname, whatever case the caller holds it in.
    ///
    /// `devices` is keyed by the ASCII-lowercased hostname (see
    /// [`super::store`]'s `device_entry`) while [`DeviceState::hostname`]
    /// keeps the wire spelling, and that wire spelling is what every caller
    /// carries: an MQTT topic level, a `sensor_group_members.hostname` row,
    /// the string `known_hub_in` hands back. Indexing `devices` with it
    /// directly misses, which is how a whole sensor group silently rendered
    /// empty. Go through here instead of building the key at the call site.
    pub fn device(&self, hostname: &str) -> Option<&DeviceState> {
        self.devices.get(&hostname.to_ascii_lowercase())
    }

    /// The Hub that owns `node_key`, and that Node.
    ///
    /// Every route keyed by a Node resolves its Hub through this instead of
    /// taking the first Hub in the store (devices.hub.md §12 E): `node_key`
    /// is a dev_eui and globally unique, so the owner is a lookup rather
    /// than a guess. `None` means no Hub knows the key, which is a 404 at
    /// every call site.
    pub fn hub_of_node(&self, node_key: &str) -> Option<(&DeviceState, &NodeState)> {
        let key = node_key.to_lowercase();
        pick_owner(
            self.devices
                .values()
                .filter_map(|d| d.nodes.get(&key).map(|n| (d, n))),
            &DUPLICATE_NODE_WARNED,
            "node",
            &key,
        )
    }

    /// The Hub that owns the registered Beacon `mac`, and that Beacon.
    ///
    /// Same rule as [`PortalState::hub_of_node`]; a MAC is globally unique
    /// too. Several Hubs may *hear* one Beacon, which is `render::beacon_hubs`
    /// and a display list. This answers the different question of whose
    /// `device_meta` and command topic a write for that Beacon goes to.
    ///
    /// **In a cluster (decision #42)** a registered Beacon belongs to the cluster, not to a Hub:
    /// the Hub returned is the registry (the Server), so every key and registry command lands
    /// there, and the Beacon is its home Hub's view of it — the one Hub that reads and dials
    /// it. Use [`PortalState::home_hub_of_beacon`] for the Hub a GATT command goes to.
    ///
    /// **Registered beats heard.** Outside that, a Hub that does not enroll the Beacon still
    /// overhears it and reports it `discovered: true`; that copy must never shadow the Hub that
    /// owns the registration, however fresh its report or early its hostname sorts. Only when no
    /// Hub has it registered does a heard copy answer — the caller then sees
    /// `discovered == Some(true)` and treats it as a candidate.
    pub fn hub_of_beacon(&self, mac: &str) -> Option<(&DeviceState, &BeaconState)> {
        let key = mac.trim().to_uppercase();
        if let Some(cluster) = self
            .beacon_cluster
            .as_ref()
            .filter(|c| c.is_registered(&key))
        {
            if let (Some(server), Some(beacon)) =
                (self.device(&cluster.server), self.beacon_view(&key))
            {
                return Some((server, beacon));
            }
        }
        let copies = || {
            self.devices
                .values()
                .filter_map(|d| d.ble_sensors.get(&key).map(|b| (d, b)))
        };
        let registered = copies().filter(|(_, b)| b.discovered != Some(true));
        if registered.clone().next().is_some() {
            return pick_owner(registered, &DUPLICATE_BEACON_WARNED, "beacon", &key);
        }
        pick_owner(copies(), &DUPLICATE_BEACON_WARNED, "beacon", &key)
    }

    /// The registry a Beacon command or key goes to when there is no Beacon to resolve from —
    /// registration: the cluster Server once `eye/homes` named one (decision #42).
    pub fn beacon_registry(&self) -> Option<&DeviceState> {
        self.beacon_cluster
            .as_ref()
            .and_then(|c| self.device(&c.server))
    }

    /// The Hub a GATT command for `mac` goes to (reconfigure, history download): its home Hub
    /// in a cluster, otherwise the Hub that registered it ([`PortalState::hub_of_beacon`]).
    pub fn home_hub_of_beacon(&self, mac: &str) -> Option<&DeviceState> {
        let key = mac.trim().to_uppercase();
        self.beacon_cluster
            .as_ref()
            .and_then(|c| c.home_of(&key))
            .and_then(|h| self.device(h))
            .or_else(|| self.hub_of_beacon(&key).map(|(d, _)| d))
    }

    /// One Beacon's live state in a cluster: its home Hub's, else the freshest Hub that has it.
    fn beacon_view(&self, mac: &str) -> Option<&BeaconState> {
        let home = self
            .beacon_cluster
            .as_ref()
            .and_then(|c| c.home_of(mac))
            .and_then(|h| self.device(h))
            .and_then(|d| d.ble_sensors.get(mac));
        let heard = || self.devices.values().filter_map(|d| d.ble_sensors.get(mac));
        // A Hub that still lists a registered MAC as a stranger (a Client before its first
        // view) is the weakest evidence: any Hub that knows it as registered wins.
        home.or_else(|| {
            heard()
                .filter(|b| b.discovered != Some(true))
                .max_by_key(|b| b.last_seen)
        })
        .or_else(|| heard().max_by_key(|b| b.last_seen))
    }

    /// The Beacon list of a cluster (decision #42): one entry per MAC across every Hub.
    /// Registered MACs read their [`PortalState::hub_of_beacon`] view and are marked
    /// registered; strangers heard by several Hubs fold into the freshest entry. `None`
    /// outside a cluster, where each Hub lists its own.
    pub fn cluster_beacons(&self) -> Option<Vec<BeaconState>> {
        let cluster = self.beacon_cluster.as_ref()?;
        let macs: std::collections::BTreeSet<&String> = self
            .devices
            .values()
            .flat_map(|d| d.ble_sensors.keys())
            .collect();
        Some(
            macs.into_iter()
                .filter_map(|mac| {
                    let mut b = self.beacon_view(mac)?.clone();
                    if cluster.is_registered(mac) {
                        if b.discovered == Some(true) {
                            b.discovered = None;
                        }
                    } else {
                        b.discovered = Some(true);
                    }
                    Some(b)
                })
                .collect(),
        )
    }

    /// The Hub a route acts on when it is Hub-scoped but sits outside
    /// `/hubs/{hub}` — the wired-line chart and history, the bulk threshold
    /// panel, and a registration that has no entity to resolve from yet.
    ///
    /// The rule, stated once (devices.hub.md §12 E): an explicit `?hub=`
    /// hostname the store knows; otherwise the only Hub, when there is
    /// exactly one. Several Hubs and no `?hub=` has no honest answer, so the
    /// caller 404s and logs which case it hit. Matching is case-insensitive,
    /// like `hub_routes::known_hub`.
    pub fn scoped_hub(&self, hub: Option<&str>) -> Option<&DeviceState> {
        match hub.map(str::trim).filter(|h| !h.is_empty()) {
            Some(wanted) => self
                .devices
                .values()
                .find(|d| d.hostname.eq_ignore_ascii_case(wanted)),
            None if self.devices.len() == 1 => self.devices.values().next(),
            None => None,
        }
    }
}

/// Per-device state. `sensors` always holds exactly 8 entries (lines 0..7),
/// pre-created regardless of whether the device has reported on them yet —
/// per ws.md's "always all 8 lines pre-created".
#[derive(Debug, Clone)]
pub struct DeviceState {
    pub hostname: String,
    pub online: bool,
    pub version: Option<String>,
    pub first_seen: Option<i64>,
    pub last_seen: Option<i64>,
    pub sensors: [SensorState; 8],
    pub power: PowerState,
    pub standby: StandbyState,
    pub network: NetworkState,
    pub system: SystemState,
    pub config: DeviceConfigState,
    /// Nodes (STICKERs) — LoRaWAN or LoRa P2P (plan/viewer/p2p_nodes.md §1),
    /// keyed by **lowercase** node_key (ws.md's v1 `lorawan_sensors`; the
    /// store normalizes case on every ingest path). The key is the dev_eui on
    /// both transports (GitLab #73).
    pub nodes: BTreeMap<String, NodeState>,
    /// BLE beacons (EYE tags), keyed by **uppercase** MAC (ws.md
    /// `ble_sensors`; wire case is not guaranteed, store normalizes).
    pub ble_sensors: BTreeMap<String, BeaconState>,
    /// Cached Node device-config pages (ws.md `lorawan_device_configs`
    /// values), keyed by lowercase node_key.
    pub node_configs: BTreeMap<String, NodeConfigEntry>,
    /// Raw `…/info` payloads kept verbatim (ws.md `lorawan_device_infos`:
    /// "raw MQTT payload verbatim … in-memory only"), keyed by lowercase
    /// node_key. An empty retained `info` publish is a tombstone that removes
    /// the entry (see the store's `StickerInfo` arm).
    pub node_infos: BTreeMap<String, serde_json::Value>,
    /// What each Node reported about itself in its last fPort-85 `Info` — the v2 retained
    /// `…/nodes/{id}/device_info` (node_reported_state.md §4), keyed by lowercase node_key.
    /// Deliberately **not** `node_infos`: on the v2 path that map holds the `info` inventory
    /// (the Hub's enrollment record), and every uplink replaces it wholesale. Renderers read
    /// this map first and fall back to `node_infos` for the v1 `StickerInfo` path.
    pub node_device_infos: BTreeMap<String, NodeDeviceInfoEntry>,
    /// `…/nodes/{id}/device_status` (control-radio p2p_control_commands.md §5), keyed by
    /// lowercase node_key: the Node's last DevStatusAns, verbatim.
    pub node_device_statuses: BTreeMap<String, serde_json::Value>,
    /// Cached Node full-config snapshots (ws.md `lorawan_device_full_configs`
    /// values), keyed by lowercase node_key.
    pub node_full_configs: BTreeMap<String, NodeFullConfigEntry>,
    /// The `lorawan` block of `system/info` (devices.hub.md §12 A) — gateway presence,
    /// concentratord/chirpstack health, the Hub's own gateway EUI and the cluster blob.
    /// `gateway_eui` is lowercased on ingest so it joins against the `radio/gateways` rows.
    /// Reused from `fiber-protocol` rather than mirrored: it is the wire type.
    pub lorawan: Option<LorawanGatewayInfo>,
    /// The last `eye/sensors` snapshot dropped overheard strangers (B6,
    /// `EyeSensorsMsg::discovered_capped`).
    ///
    /// Per snapshot, not sticky: it describes the array the store last
    /// reconciled against, and the scan view says so — control.ble.md Part B
    /// UI-12, "the difference between 'a quiet site' and 'we are dropping
    /// neighbours' must not be silent". Never true for `tags: []`, which is
    /// the empty snapshot rather than a cap.
    pub discovered_capped: bool,
    /// The Hub's `radio/northbridge` document (radio diagnostics §3) and when it arrived.
    pub northbridge_diag: Option<(serde_json::Value, i64)>,
    /// The Hub's `radio/lorawan` document (radio diagnostics §4a) and when it arrived.
    pub lorawan_diag: Option<(serde_json::Value, i64)>,
}

impl DeviceState {
    pub fn new(hostname: String) -> Self {
        Self {
            hostname,
            online: false,
            version: None,
            first_seen: None,
            last_seen: None,
            sensors: std::array::from_fn(|i| SensorState {
                line: i as u8,
                dominant_alarm_state: "NORMAL".to_string(),
                ..SensorState::default()
            }),
            power: PowerState::default(),
            standby: StandbyState::default(),
            network: NetworkState::default(),
            system: SystemState::default(),
            config: DeviceConfigState::default(),
            nodes: BTreeMap::new(),
            ble_sensors: BTreeMap::new(),
            node_configs: BTreeMap::new(),
            node_infos: BTreeMap::new(),
            node_device_infos: BTreeMap::new(),
            node_device_statuses: BTreeMap::new(),
            node_full_configs: BTreeMap::new(),
            lorawan: None,
            discovered_capped: false,
            northbridge_diag: None,
            lorawan_diag: None,
        }
    }
}

/// One LoRaWAN Node (STICKER) — ws.md `LoRaWANSensorState`, M5-C subset.
///
/// Update semantics differ per field (mqtt.md `lorawan/sensors` +
/// `config/state`): `fields`/`counters` are **merged, never pruned**;
/// `field_alarm_states` is **replaced wholesale** (firmware casing
/// `"NORMAL"|"WARNING"|"CRITICAL"` kept raw); `field_alarm_triggered_at` is
/// stamped from the store clock when a field *enters* WARNING/CRITICAL and
/// cleared on recovery (i64 store-clock seconds per this module's timestamp
/// convention — v1 kept an ISO string here, same information);
/// `recent_events` keeps the raw firmware event dicts tail-capped at 32.
/// `last_seen` is the exception to the store-clock convention: it is the
/// **Node uplink time string from the payload, kept verbatim** — it doubles
/// as the persistence dedup key (`lorawan_record_on_uplink_only`), so the
/// store clock must never overwrite it.
#[derive(Debug, Clone)]
pub struct NodeState {
    /// Identity key (map key duplicated here for renderers): the lowercase
    /// dev_eui, on both transports (GitLab #73 — the P2P wire carries the
    /// DevEUI too; `serial_number` is the printed identity, not the key).
    pub node_key: String,
    pub name: Option<String>,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    /// From `config/state` only (the authoritative Node inventory).
    pub enabled: Option<bool>,
    /// Radio path this Node speaks (plan/viewer/p2p_nodes.md §1,
    /// plan/decisions.md #7). Stated only by the retained `info` document's
    /// `transport` field (GitLab #73: the topic carries no transport level);
    /// defaults to `Lorawan` until that document has been seen.
    pub transport: Transport,
    /// The session the network assigned at the join (plan/decisions.md #8), from the retained
    /// `info` document: `None` until the Hub publishes one. The P2P central does; the LoRaWAN
    /// arm publishes none today.
    pub dev_addr: Option<String>,
    pub net_id: Option<String>,
    /// P2P session lifecycle (plan/viewer/p2p_nodes.md §2/§4), `None` where the `info` document
    /// carries none.
    pub session_state: Option<SessionState>,
    /// Store-clock time of the last join the `info` document reported, `None` before one.
    pub last_join_at: Option<i64>,
    pub fields: BTreeMap<String, f64>,
    /// The switched-off sensors of this Node (GitLab #120) and the last value each sent, kept
    /// out of `fields` so no page, group, chart or grade sees it. Only the sensor settings
    /// table reads the value, to show that the Node still sends it. A key is present for every
    /// switched-off sensor; the value is `None` until one arrives after a restart.
    pub disabled_fields: BTreeMap<String, Option<f64>>,
    /// The quantities the Node's latest report sent as `null`: the sensor is on but had no
    /// valid sample (sticker-firmware v2.0.0; the Hub turns each into a `no_data` event). Such a
    /// quantity has no value in `fields` or `counters`, so the page shows "–" instead of the
    /// last real value, and it still counts as reported. A real value takes it out again. An
    /// absent key is a different thing: the sensor is off or not reported (GitLab #120).
    pub no_data_fields: BTreeSet<String>,
    pub field_alarm_states: BTreeMap<String, String>,
    pub field_alarm_triggered_at: BTreeMap<String, i64>,
    /// Replaced only when a payload carries the key as a list — "present ⇒
    /// authoritative" (mqtt.md `lorawan/sensors`, `thresholds_authoritative`).
    pub field_thresholds: Option<Vec<NodeFieldThreshold>>,
    pub counters: BTreeMap<String, i64>,
    /// Raw firmware event objects, oldest first, tail-capped at 32.
    pub recent_events: Vec<serde_json::Value>,
    /// Best-gateway signal: taken from the strongest entry in `gateways`
    /// when one reports an RSSI, else merged from the payload's top level.
    pub rssi: Option<i64>,
    pub snr: Option<f64>,
    pub gateways: Vec<NodeGatewayLink>,
    pub fcnt: Option<i64>,
    /// ChirpStack DR index.
    pub dr: Option<i64>,
    pub downlink_gateway_id: Option<String>,
    /// Node uplink time **verbatim from the payload** — persistence dedup
    /// key, NOT a store-clock timestamp.
    pub last_seen: Option<String>,
    /// Firmware-cased overall alarm state, raw wire value; defaults to
    /// ws.md's `"NeverConnected"` until a payload carries one.
    pub alarm_state: String,
    /// Write-once store-clock first-seen (not a registration date).
    pub added_at: Option<i64>,
    /// This Node's latest backfill job ended `failed` — three downlinks went
    /// out and none was answered, so that window of history is not coming
    /// (devices.backend.md §6.3, §19 F12). Written by `backfill.rs` through
    /// [`crate::state::StateStore::set_history_faults`], not by any device
    /// payload: it lives here so the fault axis is readable without a
    /// database round trip, which is what lets the overview strip count it
    /// (#64).
    pub history_unavailable: bool,
}

impl NodeState {
    pub fn new(node_key: String, now: i64) -> Self {
        Self {
            node_key,
            name: None,
            serial_number: None,
            location: None,
            enabled: None,
            transport: Transport::default(),
            dev_addr: None,
            net_id: None,
            session_state: None,
            last_join_at: None,
            fields: BTreeMap::new(),
            disabled_fields: BTreeMap::new(),
            no_data_fields: BTreeSet::new(),
            field_alarm_states: BTreeMap::new(),
            field_alarm_triggered_at: BTreeMap::new(),
            field_thresholds: None,
            counters: BTreeMap::new(),
            recent_events: Vec::new(),
            rssi: None,
            snr: None,
            gateways: Vec::new(),
            fcnt: None,
            dr: None,
            downlink_gateway_id: None,
            last_seen: None,
            alarm_state: "NeverConnected".to_string(),
            added_at: Some(now),
            history_unavailable: false,
        }
    }

    /// The latest value of one of the Node's quantities: a measurement from `fields`, or a
    /// count (`motion_count`, `input_b_count`, ...) from `counters`, where the decoder puts
    /// every `*_count` (GitLab #134). `None` when the Node has not sent it.
    pub fn reading(&self, name: &str) -> Option<f64> {
        self.fields
            .get(name)
            .copied()
            .or_else(|| self.counters.get(name).map(|&c| c as f64))
    }

    /// Whether the Node has sent `name`, as a measurement or a count.
    pub fn has_reading(&self, name: &str) -> bool {
        self.fields.contains_key(name) || self.counters.contains_key(name)
    }
}

/// Per-field Node alarm thresholds — model twin of `fiber_protocol`'s
/// `lorawan::FieldThreshold` (`null` bound means "inherit").
#[derive(Debug, Clone)]
pub struct NodeFieldThreshold {
    pub field: String,
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
}

/// One gateway's reception of a Node uplink (ws.md `gateways` entries).
#[derive(Debug, Clone, Default)]
pub struct NodeGatewayLink {
    pub gateway_id: Option<String>,
    pub rssi: Option<i64>,
    pub snr: Option<f64>,
}

/// One BLE beacon (EYE tag) — ws.md `BLESensorState`, M5-C subset.
///
/// Mapping rules from `eye/sensors` (mqtt.md): `temperature_c →
/// temperature`, `stale → connected = !stale`.
/// `field_alarm_states`/`field_alarm_triggered_at` follow the same
/// replace-wholesale/stamp-on-entry semantics as [`NodeState`].
///
/// B5 (devices.backend.md §8 WP5.1, §20) removed `tag_type`, `is_en12830`
/// and the `detect_status`/`detect_at` pair: there is one kind of Beacon
/// and there never will be another (devices.beacon.md §1), so the type
/// probe had nothing left to distinguish. `tag_type` in particular was
/// never a wire attribute at all — the portal derived it from
/// `is_en12830` (§16.3).
#[derive(Debug, Clone)]
pub struct BeaconState {
    /// Uppercase MAC (map key duplicated here for renderers).
    pub mac: String,
    pub name: Option<String>,
    pub location: Option<String>,
    pub temperature: Option<f64>,
    pub humidity: Option<i64>,
    pub battery_mv: Option<i64>,
    pub low_battery: Option<bool>,
    pub magnet_present: Option<bool>,
    pub magnet_detected: Option<bool>,
    pub moving: Option<bool>,
    pub movement_count: Option<i64>,
    pub pitch: Option<i64>,
    pub roll: Option<i64>,
    pub rssi: Option<i64>,
    /// Device-reported unix seconds (`last_seen_ts`), not store clock.
    pub last_seen: Option<i64>,
    pub connected: bool,
    pub provisioning: Option<String>,
    /// `Some(true)` = auto-discovered (prunable when stale); **`None` must
    /// be read as "registered"** — the wire sends `null` for tags that are
    /// part of the device config, and registered tags are never pruned.
    pub discovered: Option<bool>,
    pub field_alarm_states: BTreeMap<String, String>,
    pub field_alarm_triggered_at: BTreeMap<String, i64>,
    /// The last value of a switched-off `temperature` / `humidity` (GitLab #120), taken out of
    /// `temperature` / `humidity` so nothing else shows or grades it; see
    /// [`NodeState::disabled_fields`].
    pub disabled_fields: BTreeMap<String, Option<f64>>,
    /// Firmware-cased overall alarm state, raw; defaults `"NeverConnected"`.
    pub alarm_state: String,
    /// Write-once store-clock first-seen.
    pub added_at: Option<i64>,
    /// **Store-clock** seconds of the last `eye/sensors` snapshot this MAC
    /// appeared in (B6).
    ///
    /// Not [`Self::last_seen`], which is `last_seen_ts` off the wire — a
    /// device-reported value the scan view cannot trust to be on the same
    /// clock as the portal, or on any clock at all (a stranger that has never
    /// been provisioned may report nothing). The 15-minute discovered window
    /// is measured against this.
    pub last_snapshot_at: Option<i64>,
    /// The last raw `eye/sensors` tag object for this MAC, as it arrived
    /// (devices.beacon.md §3.3, B6 WP6.6).
    ///
    /// One message, not a ring: the detail page renders it in a `<details>`
    /// with a copy button so an operator can paste what the Hub actually
    /// sent into a bug report. It is the *tag*, not the whole envelope —
    /// everything else in the envelope belongs to the snapshot, not to this
    /// beacon.
    pub last_payload: Option<serde_json::Value>,
    /// The configuration the Hub read back from the beacon after its last
    /// verified provisioning session — the source of the detail page's
    /// "Parametry beaconu (BLE)" rows (devices.backend.md §22.13, amended
    /// 2026-09-22). `None` = the Hub has not reported one.
    pub ble_config: Option<EyeBleConfig>,
}

impl BeaconState {
    pub fn new(mac: String, now: i64) -> Self {
        Self {
            mac,
            name: None,
            location: None,
            temperature: None,
            humidity: None,
            battery_mv: None,
            low_battery: None,
            magnet_present: None,
            magnet_detected: None,
            moving: None,
            movement_count: None,
            pitch: None,
            roll: None,
            rssi: None,
            last_seen: None,
            connected: false,
            provisioning: None,
            discovered: None,
            field_alarm_states: BTreeMap::new(),
            field_alarm_triggered_at: BTreeMap::new(),
            disabled_fields: BTreeMap::new(),
            alarm_state: "NeverConnected".to_string(),
            added_at: Some(now),
            last_snapshot_at: None,
            last_payload: None,
            ble_config: None,
        }
    }
}

/// Cached Node device-config pages — ws.md `NodeConfigEntry`. `config` is
/// the page-merged fPort-85 settable-key map; page metadata is taken as
/// sent (the plain `config` topic does no recomputation, unlike
/// [`NodeFullConfigEntry`]).
#[derive(Debug, Clone)]
pub struct NodeConfigEntry {
    pub config: serde_json::Map<String, serde_json::Value>,
    pub synced_at: Option<String>,
    pub last_ack: Option<NodeAck>,
    pub page_count: i64,
    /// `"complete" | "partial" | "not_read"`.
    pub read_status: String,
    pub missing: Vec<String>,
    pub error: Option<String>,
    pub chunks_done: Option<i64>,
    pub chunks_total: Option<i64>,
    /// The 1-Wire slot types, slot 1..4 (`empty` | `dallas` | `machine-probe` | `type<N>`),
    /// from the Node's boot `ConfigDump` (node_reported_state.md §2.2). Beside `config`, not in
    /// it: every `config` entry is an editable settable key, and a slot type is runtime state
    /// nothing can write. `None` = never reported, which is every Node on the v1 path.
    pub w1_slot_type: Option<Vec<String>>,
}

impl Default for NodeConfigEntry {
    fn default() -> Self {
        Self {
            config: serde_json::Map::new(),
            synced_at: None,
            last_ack: None,
            page_count: 0,
            read_status: "not_read".to_string(),
            missing: Vec::new(),
            error: None,
            chunks_done: None,
            chunks_total: None,
            w1_slot_type: None,
        }
    }
}

/// One Node's last fPort-85 `Info`, as the v2 `device_info` document carried it.
#[derive(Debug, Clone)]
pub struct NodeDeviceInfoEntry {
    /// RFC3339, when the Hub received the `Info` frame — the age the DeviceInfo card shows
    /// (devices.node.md UI-9: cached state, never presented as a live read).
    pub received_at: String,
    /// The decoded `Info` map verbatim; the firmware owns its shape.
    pub info: serde_json::Value,
}

/// Last firmware ack for a Node config page (`last_ack` on the wire).
#[derive(Debug, Clone, Default)]
pub struct NodeAck {
    pub seq: Option<i64>,
    pub result: Option<String>,
}

/// Cached Node full-config snapshot — ws.md `NodeFullConfigEntry`.
/// `missing`/`chunks_done`/`chunks_total` are **recomputed against the
/// merged snapshot**, not taken from the firmware's per-publish values, and
/// `"partial"` is promoted to `"complete"` once nothing is missing.
#[derive(Debug, Clone)]
pub struct NodeFullConfigEntry {
    pub config: serde_json::Map<String, serde_json::Value>,
    pub synced_at: Option<String>,
    pub page_count: i64,
    pub last_seq: Option<i64>,
    /// `"complete" | "partial" | "not_read"`.
    pub read_status: String,
    pub missing: Vec<String>,
    pub error: Option<String>,
    pub chunks_done: i64,
    pub chunks_total: i64,
}

impl Default for NodeFullConfigEntry {
    fn default() -> Self {
        Self {
            config: serde_json::Map::new(),
            synced_at: None,
            page_count: 0,
            last_seq: None,
            read_status: "not_read".to_string(),
            missing: Vec::new(),
            error: None,
            chunks_done: 0,
            chunks_total: 0,
        }
    }
}

/// Wired-probe state for one line (0..7). Field set per ws.md's
/// `SensorState`, minus `serial_number` (portal-side inventory field, not
/// populated by any M1 handler).
#[derive(Debug, Clone, Default)]
pub struct SensorState {
    pub line: u8,
    pub name: Option<String>,
    pub location: Option<String>,
    pub sample_count: i64,
    pub disconnected_count: i64,
    pub temperature_min_celsius: Option<f64>,
    pub temperature_avg_celsius: Option<f64>,
    pub temperature_max_celsius: Option<f64>,
    pub alarm_counts: SensorAlarmCounts,
    /// Firmware-cased alarm state string (`"NORMAL"`, `"WARNING"`, ...), kept
    /// as the raw wire value rather than mapped onto v1's mixed-case
    /// `AlarmState` enum — no consumer needs the mapping in M1, and adding a
    /// closed enum here would risk a hard parse failure on an unrecognized
    /// firmware string. Defaults to `"NORMAL"` (fiber-protocol's own default
    /// for an unset `dominant_alarm_state`) rather than v1's
    /// `"NeverConnected"`, since a freshly pre-created line hasn't received
    /// any payload to default from.
    pub dominant_alarm_state: String,
    pub alarm_triggered_at: Option<String>,
    pub connected: bool,
    pub updated_at: Option<i64>,
    /// `period_end_ts` of the newest `sensors/summary` or `sensors/aggregated`
    /// applied to this line: an older one (a window replayed after a
    /// reconnect) must not overwrite the live card
    /// (`plan/control/storage_intervals.md` §6).
    pub period_end_ts: Option<i64>,
    /// What the last `SensorLine` nudge for a live sample showed, so a 2 s
    /// sample that changes nothing on screen sends no nudge.
    pub live_nudge: Option<LiveNudge>,
    /// M3-C addition: per-line alarm thresholds, populated from
    /// `config/state`'s per-sensor `thresholds` (authoritative-snapshot
    /// semantics like `name`/`location` above — assigned directly on every
    /// `ConfigState` apply, including clearing back to `None` when the
    /// device stops sending a threshold block for this line).
    pub thresholds: Option<SensorThresholds>,
}

/// The on-screen part of a wired line at its last live nudge
/// ([`SensorState::live_nudge`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveNudge {
    pub at: i64,
    /// Average in tenths of a degree, the precision the pages show.
    pub temperature_dc: Option<i64>,
    pub state: String,
    pub connected: bool,
}

/// Per-line alarm thresholds shown in the history chart. Field names match
/// `GET /api/v1/chart/{line}`'s JSON `thresholds` object directly (see
/// `routes::history_routes`) — mapped in `StateStore::apply` from
/// `fiber_protocol`'s wire-cased `config_state::SensorThresholds` (see that
/// struct's field-mapping table doc-comment).
#[derive(Debug, Clone, Default)]
pub struct SensorThresholds {
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
    pub alarm_low: Option<f64>,
    pub alarm_high: Option<f64>,
    /// The hysteresis the Hub echoes for the line (decision #41); `None` from an older Hub.
    pub hysteresis: Option<f64>,
    /// The band's switch (#167); `None` (an older Hub) = on. See [`Self::is_enabled`].
    pub enabled: Option<bool>,
}

impl SensorThresholds {
    /// The band grades (#167): missing means on.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// Per ws.md `SensorState.alarm_counts` — plain ints (unlike
/// `fiber_protocol::payloads::sensors_aggregated::AlarmCounts`, which models
/// the wire shape as all-`Option` since the device may omit the container
/// entirely; here absence is normalized to 0 at the store boundary).
#[derive(Debug, Clone, Default)]
pub struct SensorAlarmCounts {
    pub normal: i64,
    pub warning: i64,
    pub critical: i64,
    pub disconnected: i64,
    pub reconnecting: i64,
}

#[derive(Debug, Clone, Default)]
pub struct PowerState {
    pub battery_voltage_mv: Option<i64>,
    pub battery_percentage: Option<i64>,
    pub battery_status: Option<String>,
    pub dc_voltage_mv: Option<i64>,
    pub dc_connected: Option<bool>,
    pub last_dc_loss: Option<String>,
    pub updated_at: Option<i64>,
}

/// Standby state — **never** touched by any handler except
/// [`crate::state::events::IngestEvent::Standby`] (retained-replay
/// protection, ws.md: "no online/last_seen").
#[derive(Debug, Clone, Default)]
pub struct StandbyState {
    pub active: bool,
    pub reason: Option<String>,
    /// Empty string (as sent) means self-initiated; `None` means the field
    /// was absent from the payload.
    pub requested_by: Option<String>,
    pub entered_at: Option<String>,
    pub vin_mv: Option<i64>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct NetworkState {
    pub wifi_connected: Option<bool>,
    pub wifi_signal_dbm: Option<i64>,
    pub wifi_ip: Option<String>,
    pub ethernet_connected: Option<bool>,
    pub ethernet_ip: Option<String>,
    pub has_internet: Option<bool>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct SystemState {
    pub version: Option<String>,
    pub uptime_seconds: Option<i64>,
    pub uptime_human: Option<String>,
    pub hostname: Option<String>,
    pub device_label: Option<String>,
    pub storage: StorageState,
    /// Whether an operator has finished setting this Hub up, and as what (GitLab #58).
    ///
    /// Mirrored from `system/info.setup`, which mirrors the device's own `setup:` config key.
    /// The portal deliberately keeps **no** copy of its own: v1 held the equivalent (`app_mode`)
    /// in the viewer database, where it went silent on exactly the Hubs it mattered for, because
    /// a client-role Hub shuts its portal down (decision #4).
    ///
    /// `None` on both fields means the Hub has not said — an agent predating the key, or one that
    /// has not published since boot — which is a different thing from `Some(false)` and is
    /// rendered as "unknown", not as "not set up".
    pub setup_provisioned: Option<bool>,
    pub setup_role: Option<String>,
    /// `system/info.hardware` — CPU serial, compute module model and revision, straight from
    /// the Hub's device tree (devices.hub.md §3.1, amendment 2026-09-28). `None` until a Hub
    /// that publishes it has reported; a field the Hub could not read is `None` on its own.
    /// The wire type, reused like `DeviceState::lorawan`.
    pub hardware: Option<Hardware>,
    /// `system/info.southbridge` — the Southbridge firmware it runs, the one the image bundles,
    /// and the last update (`plan/system/southbridge_update.md` §3.7). `None` until a Hub that
    /// publishes it has reported. The wire type, like `hardware`.
    pub southbridge: Option<Southbridge>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct StorageState {
    pub total_bytes: Option<i64>,
    pub available_bytes: Option<i64>,
    pub used_percent: Option<f64>,
}

/// ws.md's `DeviceConfigState` — M1 scalar fields plus the `eye` block's
/// beacon flags (M5-C, populated from `config/state`'s `eye` object).
#[derive(Debug, Clone, Default)]
pub struct DeviceConfigState {
    pub led_brightness: Option<i64>,
    pub screen_brightness: Option<i64>,
    pub buzzer_volume: Option<i64>,
    pub system_info_interval_s: Option<i64>,
    pub screen_timeout_secs: Option<i64>,
    pub device_label: Option<String>,
    pub beacon_enabled: Option<bool>,
    pub beacon_auto_provision: Option<bool>,
    pub beacon_auto_discover: Option<bool>,
    /// The LCD rows the Hub currently renders (display-config.md §5) —
    /// what the Unit tab's picker pre-checks against, matched by address
    /// and never by ordinal. Empty means the built-in layout, and also
    /// means "the Hub has never said" (a Hub on an agent older than the
    /// GitLab #74 fix omits `config/state.display_lines`).
    pub display_lines: Vec<fiber_protocol::DisplayLine>,
    /// `config/state.intervals`: the 1-Wire read interval and its window
    /// (config_sync.md §13 Q18 seeds and edits it).
    pub intervals: Option<fiber_protocol::payloads::config_state::Intervals>,
    /// `config/state.intervals` (ms on the wire, storage_intervals.md §3):
    /// the Hub-wide 1-Wire sample and aggregation intervals it runs with;
    /// `None` until a Hub states them. Read-only on the probe page.
    pub sample_interval_ms: Option<i64>,
    pub aggregation_interval_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct MqttConnectionState {
    pub connected: bool,
    pub last_disconnect_reason: Option<String>,
    pub reconnect_attempts: i64,
    pub last_connected_at: Option<i64>,
    /// When the link last went down, unix seconds.
    ///
    /// Needed for the **degraded** rule of plan/portal/mqtt/design.md §7:
    /// "disconnected longer than one keepalive cycle". `reconnect_attempts`
    /// cannot answer that — one attempt could be a second old or an hour old,
    /// and the difference is precisely what separates a blip an operator
    /// should ignore from an outage they should act on.
    pub last_disconnected_at: Option<i64>,
}

#[cfg(test)]
mod hub_of_beacon_tests {
    use super::*;

    const MAC: &str = "7C:D9:F4:12:50:88";

    fn hub(state: &mut PortalState, hostname: &str, last_seen: i64, discovered: Option<bool>) {
        let mut device = DeviceState::new(hostname.into());
        device.last_seen = Some(last_seen);
        let mut beacon = BeaconState::new(MAC.into(), last_seen);
        beacon.discovered = discovered;
        device.ble_sensors.insert(MAC.into(), beacon);
        state.devices.insert(hostname.into(), device);
    }

    /// Registered beats heard: a cluster Client that overhears the Beacon
    /// sorts first and reports later, and still does not own it.
    #[test]
    fn a_registered_copy_beats_a_fresher_stranger_on_another_hub() {
        let mut state = PortalState::default();
        hub(&mut state, "a-client", 200, Some(true));
        hub(&mut state, "b-owner", 100, None);
        let (device, beacon) = state.hub_of_beacon(&MAC.to_lowercase()).unwrap();
        assert_eq!(device.hostname, "b-owner");
        assert_eq!(beacon.discovered, None);
    }

    /// With no Hub registering it, a heard copy still answers, so callers
    /// can tell a candidate from an unknown MAC.
    #[test]
    fn a_heard_only_beacon_still_resolves_as_a_candidate() {
        let mut state = PortalState::default();
        hub(&mut state, "a-client", 200, Some(true));
        let (device, beacon) = state.hub_of_beacon(MAC).unwrap();
        assert_eq!(device.hostname, "a-client");
        assert_eq!(beacon.discovered, Some(true));
    }
}
