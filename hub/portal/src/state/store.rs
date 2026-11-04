//! The in-memory state store: single source of truth for live device state,
//! written by the MQTT ingest layer via [`StateStore::apply`], read by HTTP
//! handlers via clone-based snapshot accessors (no lock held across an
//! `await` — see design.md §4).

use std::time::{SystemTime, UNIX_EPOCH};

use fiber_protocol::payloads::status::DeviceStatus;
use parking_lot::RwLock;
use tokio::sync::broadcast;

use std::collections::{BTreeMap, BTreeSet};

use super::events::{ChangeEvent, IngestEvent};
use super::models::{
    BeaconState, DeviceState, LiveNudge, MqttConnectionState, NodeAck, NodeDeviceInfoEntry,
    NodeFieldThreshold, NodeGatewayLink, NodeState, PortalState, SensorAlarmCounts,
    SensorThresholds,
};
use super::radio::RadioGatewaysState;

/// Broadcast channel capacity: generous enough that a lagging SSE-hub
/// subscriber doesn't need lossless delivery (a missed `ChangeEvent` just
/// means that fragment re-renders on the next one, or on browser refetch).
const CHANGE_CHANNEL_CAPACITY: usize = 256;

/// ws.md `LoRaWANSensorState.recent_events`: "raw firmware dicts,
/// tail-capped 32".
const RECENT_EVENTS_CAP: usize = 32;

/// Capacity of the command-result broadcast. Small on purpose: it carries
/// only operator-initiated answers, and one waiter reads it per in-flight
/// send walk.
const COMMAND_RESULT_CHANNEL_CAPACITY: usize = 64;

pub struct StateStore {
    inner: RwLock<PortalState>,
    tx: broadcast::Sender<ChangeEvent>,
    /// A second, narrow broadcast carrying **only**
    /// [`ChangeEvent::NodeCommandResult`] and [`ChangeEvent::NodeJoined`].
    ///
    /// `pending`'s send walk waits up to the Node's `downlink_answer_budget`
    /// (at least 180 s) for one device's answer. On the firehose above it was competing with
    /// every reading from every device — `NodeAdded` + `NodeBatch` per
    /// uplink, plus `DeviceSeen`, `DeviceStatus` and the liveness repaints —
    /// so on a fleet emitting a couple of events a second the 256-slot
    /// channel could lap it, and a `Lagged` drops the very message the walk
    /// was waiting for. The row then failed with "device did not confirm"
    /// for a config the device had in fact applied, and the threshold
    /// write-through was skipped for bounds the Hub had accepted.
    ///
    /// Command results are operator-initiated, so this channel sees a
    /// trickle where the other sees telemetry; lapping it would take a
    /// deliberate flood rather than a busy Tuesday.
    results_tx: broadcast::Sender<ChangeEvent>,
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateStore {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        let (results_tx, _results_rx) = broadcast::channel(COMMAND_RESULT_CHANNEL_CAPACITY);
        Self {
            inner: RwLock::new(PortalState::default()),
            tx,
            results_tx,
        }
    }

    /// Applies a parsed inbound MQTT message, mutating state and emitting
    /// the matching [`ChangeEvent`]s. Infallible by design: malformed or
    /// out-of-range data (e.g. a sensor line outside 0..7) is logged and
    /// dropped rather than surfaced as an error — the caller has no
    /// meaningful recovery action for a bad device payload.
    pub fn apply(&self, ev: IngestEvent) {
        let mut changes = Vec::new();
        {
            let mut state = self.inner.write();
            apply_locked(&mut state, ev, &mut changes);
            park_disabled_sensors(&mut state);
            overlay_line_limits(&mut state);
            overlay_meta_labels(&mut state);
        }
        for change in changes {
            // No subscribers is a normal state (e.g. no browser tab open
            // yet) — a send error here just means nobody was listening.
            self.fan_out(change);
        }
    }

    /// Broadcasts a [`ChangeEvent`] that no inbound message produced.
    ///
    /// [`Self::apply`] is the only mutator *device payloads* reach, and stays
    /// so. This is for the case B2 introduces: a verdict that changed because
    /// time passed rather than because a message arrived (a Node going
    /// quiet), which still has to repaint its row — SSE is the only push
    /// channel to the templates, and it is driven entirely off this
    /// broadcast.
    pub fn emit(&self, change: ChangeEvent) {
        self.fan_out(change);
    }

    /// Writes §6.3's dead-history fault onto one Hub's Nodes: `failed` is
    /// every node_key whose *latest* backfill job ended `failed`, so a Node
    /// absent from it has the flag cleared. Emits [`ChangeEvent::Node`] for
    /// each Node that actually changed, so the row repaints exactly as it
    /// does for every other fault.
    ///
    /// The one writer outside [`Self::apply`], because the fact is
    /// `backfill.rs`'s rather than a device's — see
    /// [`NodeState::history_unavailable`] for why it is stored at all
    /// (devices.backend.md §19 F12, #64). A Node the store has not seen yet
    /// is skipped; the next sync pass picks it up once it appears.
    /// Loads the switched-off sensors at start (`Storage::list_disabled_sensors`), replacing
    /// whatever the store held, and parks their values.
    pub fn load_disabled_sensors(
        &self,
        sensors: impl IntoIterator<Item = (String, String, String, String)>,
    ) {
        let mut state = self.inner.write();
        state.disabled_sensors = sensors
            .into_iter()
            .map(|(h, scope, entity, field)| (h.to_ascii_lowercase(), scope, entity, field))
            .collect();
        park_disabled_sensors(&mut state);
    }

    /// Loads the Portal's wired-line bounds at start (decision #41), replacing what the store
    /// held. `(hostname, line, bounds)`.
    pub fn load_line_limits(
        &self,
        limits: impl IntoIterator<Item = (String, u8, SensorThresholds)>,
    ) {
        let mut state = self.inner.write();
        state.line_limits = limits
            .into_iter()
            .map(|(h, line, t)| ((h.to_ascii_lowercase(), line), t))
            .collect();
        overlay_line_limits(&mut state);
    }

    /// Records the bounds the Portal just stored for a wired line and repaints the line at
    /// once, whatever the Hub later answers (decision #41).
    pub fn set_line_limits(&self, hostname: &str, line: u8, limits: SensorThresholds) {
        let hostname = hostname.to_ascii_lowercase();
        {
            let mut state = self.inner.write();
            state.line_limits.insert((hostname.clone(), line), limits);
            overlay_line_limits(&mut state);
        }
        self.fan_out(ChangeEvent::SensorLine { hostname, line });
    }

    /// Loads the Portal's Node and Beacon names and locations at start (decision #41),
    /// `(hostname, scope, entity, name, location)`, replacing what the store held.
    pub fn load_meta_labels(
        &self,
        rows: impl IntoIterator<Item = (String, String, String, Option<String>, Option<String>)>,
    ) {
        let mut state = self.inner.write();
        state.meta_labels = rows
            .into_iter()
            .map(|(h, scope, entity, n, l)| ((h.to_ascii_lowercase(), scope, entity), (n, l)))
            .collect();
        overlay_meta_labels(&mut state);
    }

    /// Records the name and location the Portal just stored for a Node (`node`) or Beacon
    /// (`eye`) and repaints it at once (decision #41). `None` hands that half back to the Hub.
    pub fn set_meta_label(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        name: Option<String>,
        location: Option<String>,
    ) {
        let hostname = hostname.to_ascii_lowercase();
        let key = (hostname.clone(), scope.to_string(), entity.to_string());
        {
            let mut state = self.inner.write();
            // Hand a cleared half back to the Hub's text before the overlay runs again.
            let hub = state.hub_labels.get(&key).cloned().unwrap_or_default();
            let prev = state.meta_labels.get(&key).cloned().unwrap_or_default();
            let restore_name = (prev.0.is_some() && name.is_none()).then(|| hub.0.clone());
            let restore_loc = (prev.1.is_some() && location.is_none()).then(|| hub.1.clone());
            if let Some((n, l)) = label_slot(&mut state, &key) {
                if let Some(h) = restore_name {
                    *n = h;
                }
                if let Some(h) = restore_loc {
                    *l = h;
                }
            }
            if name.is_none() && location.is_none() {
                state.meta_labels.remove(&key);
                state.hub_labels.remove(&key);
            } else {
                state.meta_labels.insert(key, (name, location));
            }
            overlay_meta_labels(&mut state);
        }
        self.fan_out(label_change(hostname, scope, entity));
    }

    /// Drops a removed Node's or Beacon's Portal name and location, so the next device
    /// registered under the same key does not inherit them.
    pub fn forget_meta_label(&self, hostname: &str, scope: &str, entity: &str) {
        let key = (
            hostname.to_ascii_lowercase(),
            scope.to_string(),
            entity.to_string(),
        );
        let mut state = self.inner.write();
        state.meta_labels.remove(&key);
        state.hub_labels.remove(&key);
    }

    /// Drops a removed Client Hub's state (cluster.md amendment 2026-10-28, #168), so the Hub
    /// leaves the list at once instead of lingering until the portal restarts.
    pub fn forget_device(&self, hostname: &str) {
        self.inner
            .write()
            .devices
            .remove(&hostname.to_ascii_lowercase());
    }

    /// Drops a removed Beacon from the cluster registry at once. The Server's next `eye/homes`
    /// document says the same, but until it arrives a re-registration of the MAC would be
    /// refused as a duplicate.
    pub fn forget_registered_beacon(&self, mac: &str) {
        let mac = mac.trim().to_uppercase();
        if let Some(cluster) = self.inner.write().beacon_cluster.as_mut() {
            cluster.registered.remove(&mac);
            cluster.homes.remove(&mac);
        }
    }

    /// Whether one sensor is switched off (GitLab #120).
    pub fn sensor_disabled(&self, hostname: &str, scope: &str, entity: &str, field: &str) -> bool {
        self.inner.read().disabled_sensors.contains(&(
            hostname.to_ascii_lowercase(),
            scope.to_string(),
            entity.to_string(),
            field.to_string(),
        ))
    }

    /// Switches one sensor off or on in memory, after storage has (GitLab #120). Off parks its
    /// last value in `disabled_fields` and drops its alarm state; on moves the last value back,
    /// so the pages show it at once rather than after the next uplink. Emits the entity's
    /// change event.
    pub fn set_sensor_disabled(
        &self,
        hostname: &str,
        scope: &str,
        entity: &str,
        field: &str,
        disabled: bool,
    ) {
        let hostname = hostname.to_ascii_lowercase();
        {
            let mut state = self.inner.write();
            let key = (
                hostname.clone(),
                scope.to_string(),
                entity.to_string(),
                field.to_string(),
            );
            if disabled {
                state.disabled_sensors.insert(key);
                park_disabled_sensors(&mut state);
            } else {
                state.disabled_sensors.remove(&key);
                if let Some(device) = state.devices.get_mut(&hostname) {
                    unpark_sensor(device, scope, entity, field);
                }
            }
        }
        self.fan_out(if scope == "eye" {
            ChangeEvent::Beacon {
                hostname,
                mac: entity.to_string(),
            }
        } else {
            ChangeEvent::Node {
                hostname,
                node_key: entity.to_string(),
            }
        });
    }

    /// Drops every switched-off sensor of a removed Node or Beacon, so the next device
    /// registered under the same key starts with all of them on.
    pub fn forget_disabled_sensors(&self, hostname: &str, scope: &str, entity: &str) {
        let hostname = hostname.to_ascii_lowercase();
        self.inner
            .write()
            .disabled_sensors
            .retain(|(h, s, e, _)| !(*h == hostname && s == scope && e == entity));
    }

    pub fn set_history_faults(&self, hostname: &str, failed: &std::collections::HashSet<String>) {
        let mut changes = Vec::new();
        {
            let mut state = self.inner.write();
            let Some(device) = state.devices.get_mut(&hostname.to_ascii_lowercase()) else {
                return;
            };
            for (node_key, node) in device.nodes.iter_mut() {
                let now = failed.contains(node_key);
                if node.history_unavailable != now {
                    node.history_unavailable = now;
                    changes.push(ChangeEvent::Node {
                        hostname: hostname.to_string(),
                        node_key: node_key.clone(),
                    });
                }
            }
        }
        for change in changes {
            self.fan_out(change);
        }
    }

    /// A v2 Node's value axis (nodes_phase2.cs.md, P-9 / N32): the per-field states the persist
    /// thread graded its latest telemetry to, against the bounds the portal holds. The v2
    /// envelope carries no state, so without this every list, filter, tile and header read the
    /// Node as `NORMAL` while its alarm was open. Fields the reading did not carry keep their
    /// state; `alarm_state` becomes the worst field state. Emits [`ChangeEvent::Node`] when
    /// anything changed. A store write outside [`Self::apply`], like
    /// [`Self::set_history_faults`]: the grade is the persist thread's, not a device's.
    pub fn set_node_field_states(
        &self,
        hostname: &str,
        node_key: &str,
        states: &BTreeMap<String, String>,
    ) -> bool {
        let changed = {
            let mut state = self.inner.write();
            let Some(node) = state
                .devices
                .get_mut(&hostname.to_ascii_lowercase())
                .and_then(|d| d.nodes.get_mut(node_key))
            else {
                return false;
            };
            let mut next = node.field_alarm_states.clone();
            next.extend(states.iter().map(|(k, v)| (k.clone(), v.clone())));
            let worst = worst_field_state(&next);
            let changed = next != node.field_alarm_states || node.alarm_state != worst;
            if changed {
                node.field_alarm_triggered_at = restamp_field_alarms(
                    &node.field_alarm_states,
                    &node.field_alarm_triggered_at,
                    &next,
                    now_unix(),
                );
                node.field_alarm_states = next;
                node.alarm_state = worst.to_string();
            }
            park_disabled_sensors(&mut state);
            changed
        };
        if changed {
            self.fan_out(ChangeEvent::Node {
                hostname: hostname.to_string(),
                node_key: node_key.to_string(),
            });
        }
        changed
    }

    /// Refills one Node's last values after a restart (N14, combined23): the in-memory
    /// `fields` map starts empty and only an uplink filled it, so the detail page showed a
    /// dash for up to a report interval while the chart had the data. `fields` comes from
    /// storage (`latest_node_fields`), and it is written only into a Node whose map is still
    /// empty, so a live reading that got there first always wins. Emits
    /// [`ChangeEvent::Node`] when it wrote anything.
    ///
    /// A store write outside [`Self::apply`], like [`Self::set_history_faults`]: no ingest
    /// event, so nothing is persisted again and no alarm is evaluated on an old value.
    pub fn seed_node_fields(
        &self,
        hostname: &str,
        node_key: &str,
        fields: &[(String, f64)],
    ) -> bool {
        if fields.is_empty() {
            return false;
        }
        {
            let mut state = self.inner.write();
            let Some(node) = state
                .devices
                .get_mut(&hostname.to_ascii_lowercase())
                .and_then(|d| d.nodes.get_mut(node_key))
            else {
                return false;
            };
            // A live reading got there first, even one whose every quantity was `null`.
            if !node.fields.is_empty() || !node.no_data_fields.is_empty() {
                return false;
            }
            node.fields.extend(fields.iter().cloned());
            park_disabled_sensors(&mut state);
        }
        self.fan_out(ChangeEvent::Node {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
        });
        true
    }

    /// Refills one Node's last sensor counts after a restart, the twin of
    /// [`Self::seed_node_fields`] for `counters` (GitLab #134): a count row would read "–"
    /// until the next uplink. Only counts the Node does not hold yet are written, so a live
    /// value always wins. Emits [`ChangeEvent::Node`] when it wrote anything.
    pub fn seed_node_counters(
        &self,
        hostname: &str,
        node_key: &str,
        counters: &[(String, i64)],
    ) -> bool {
        let mut wrote = false;
        {
            let mut state = self.inner.write();
            let Some(node) = state
                .devices
                .get_mut(&hostname.to_ascii_lowercase())
                .and_then(|d| d.nodes.get_mut(node_key))
            else {
                return false;
            };
            for (name, value) in counters {
                // A switched-off count is seeded too; the park below moves it aside.
                let parked = matches!(node.disabled_fields.get(name), Some(Some(_)));
                if !node.counters.contains_key(name) && !parked {
                    node.counters.insert(name.clone(), *value);
                    wrote = true;
                }
            }
            if wrote {
                park_disabled_sensors(&mut state);
            }
        }
        if wrote {
            self.fan_out(ChangeEvent::Node {
                hostname: hostname.to_string(),
                node_key: node_key.to_string(),
            });
        }
        wrote
    }

    /// Refills which quantities the Node last sent as `null` after a restart, the twin of
    /// [`Self::seed_node_fields`] for `no_data_fields` (storage `nulled_node_fields`). The
    /// row then reads "–" like before the restart. A quantity the Node holds a value for, or
    /// one that is switched off, is left alone, so a live reading always wins. Emits
    /// [`ChangeEvent::Node`] when it wrote anything.
    pub fn seed_node_no_data(&self, hostname: &str, node_key: &str, names: &[String]) -> bool {
        let mut wrote = false;
        {
            let mut state = self.inner.write();
            let Some(node) = state
                .devices
                .get_mut(&hostname.to_ascii_lowercase())
                .and_then(|d| d.nodes.get_mut(node_key))
            else {
                return false;
            };
            for name in names {
                let held = node.fields.contains_key(name) || node.counters.contains_key(name);
                if !held && !node.disabled_fields.contains_key(name) {
                    node.no_data_fields.insert(name.clone());
                    wrote = true;
                }
            }
        }
        if wrote {
            self.fan_out(ChangeEvent::Node {
                hostname: hostname.to_string(),
                node_key: node_key.to_string(),
            });
        }
        wrote
    }

    /// Refills one Node's last signal (RSSI, SNR, the gateways that heard it) after a
    /// restart, the twin of [`Self::seed_node_fields`]: the signal is in-memory and only an
    /// uplink filled it, so the signal bars read "unknown" until the next reading. The values
    /// come from the newest stored data frame, and they are written only into a Node that
    /// holds no signal at all, so a live reading that got there first always wins.
    /// `last_seen` is left alone. Emits [`ChangeEvent::Node`] when it wrote anything.
    pub fn seed_node_signal(
        &self,
        hostname: &str,
        node_key: &str,
        rssi: Option<i64>,
        snr: Option<f64>,
        gateways: Vec<NodeGatewayLink>,
    ) -> bool {
        if rssi.is_none() && snr.is_none() && gateways.is_empty() {
            return false;
        }
        {
            let mut state = self.inner.write();
            let Some(node) = state
                .devices
                .get_mut(&hostname.to_ascii_lowercase())
                .and_then(|d| d.nodes.get_mut(node_key))
            else {
                return false;
            };
            if node.rssi.is_some() || node.snr.is_some() || !node.gateways.is_empty() {
                return false;
            }
            apply_signal(node, rssi, snr, gateways);
        }
        self.fan_out(ChangeEvent::Node {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
        });
        true
    }

    /// Every event goes to the main broadcast; a command result *also* goes
    /// to the narrow one. Mirrored rather than moved, because SSE renders
    /// `command-status` frames off the main stream and must keep seeing it.
    fn fan_out(&self, change: ChangeEvent) {
        if matches!(
            change,
            ChangeEvent::NodeCommandResult { .. } | ChangeEvent::NodeJoined { .. }
        ) {
            let _ = self.results_tx.send(change.clone());
        }
        let _ = self.tx.send(change);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ChangeEvent> {
        self.tx.subscribe()
    }

    /// Subscribe to [`Self::results_tx`] — command results only. What a
    /// caller correlating one device's answer should use; `subscribe` is
    /// for anything that renders the whole fleet.
    pub fn subscribe_command_results(&self) -> broadcast::Receiver<ChangeEvent> {
        self.results_tx.subscribe()
    }

    /// Full-state clone. Cheap enough for M1's scale (single-digit devices);
    /// revisit if the dashboard page ever needs to avoid cloning every
    /// device on every render.
    pub fn snapshot(&self) -> PortalState {
        self.inner.read().clone()
    }

    pub fn snapshot_device(&self, hostname: &str) -> Option<DeviceState> {
        self.inner
            .read()
            .devices
            .get(&hostname.to_ascii_lowercase())
            .cloned()
    }

    /// The canonical spelling of a Hub's hostname — `DeviceState::hostname`, the one every
    /// page renders ids and `sse-swap` names with — for a hostname in either case, or `None`
    /// for a Hub the store does not know. Since decision #21 that is the lowercase spelling;
    /// the lookup stays so an event from an older agent still names the Hub the page drew
    /// (#87). Reads one
    /// `String`, not a device clone: it runs per event per open connection.
    pub fn display_hostname(&self, hostname: &str) -> Option<String> {
        self.inner
            .read()
            .devices
            .get(&hostname.to_ascii_lowercase())
            .map(|d| d.hostname.clone())
    }

    /// The archive interval a Beacon last reported (`ble_config.logging_interval_min`,
    /// beacon_history.md §3), in minutes; `None` until the Hub has read the record info.
    /// Reads one number, not a device clone: the History tab's table asks on every nudge.
    pub fn beacon_logging_interval_min(&self, hostname: &str, mac: &str) -> Option<i64> {
        self.inner
            .read()
            .devices
            .get(&hostname.to_ascii_lowercase())?
            .ble_sensors
            .get(&mac.to_uppercase())?
            .ble_config
            .as_ref()?
            .logging_interval_min
            .filter(|min| *min > 0)
    }

    /// The cluster's Beacon registry and home Hubs (decision #42), without cloning the rest.
    pub fn snapshot_beacon_cluster(&self) -> Option<crate::state::models::BeaconCluster> {
        self.inner.read().beacon_cluster.clone()
    }

    pub fn snapshot_mqtt(&self) -> MqttConnectionState {
        self.inner.read().mqtt.clone()
    }

    /// The last `radio/gateways` report, cloned (devices.hub.md §12 A2). Separate from
    /// [`StateStore::snapshot`] so the card can read it without cloning every device.
    pub fn snapshot_radio_gateways(&self) -> Option<RadioGatewaysState> {
        self.inner.read().radio_gateways.clone()
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The least time between two value-only nudges of one wired line. A live
/// sample arrives every `sensors.sample_interval_s` (2 s); every nudge makes
/// each open tab refetch the row, so a value change waits this long while a
/// state or connection change goes at once.
const LIVE_NUDGE_MIN_S: i64 = 10;

/// Whether a live sample showing `shown` moves the row enough to nudge it.
fn live_nudge_due(last: Option<&LiveNudge>, shown: &LiveNudge) -> bool {
    let Some(last) = last else {
        return true;
    };
    if last.state != shown.state || last.connected != shown.connected {
        return true;
    }
    last.temperature_dc != shown.temperature_dc && shown.at - last.at >= LIVE_NUDGE_MIN_S
}

/// `sensors/aggregated` (one storage window) and `sensors/summary` (one
/// sample, `live`) update the same wired-line state
/// (`plan/control/storage_intervals.md` §6). Only the nudges differ: a
/// window always nudges, a live sample only when the row would change.
fn apply_sensor_lines(
    state: &mut PortalState,
    hostname: String,
    payload: fiber_protocol::payloads::sensors_aggregated::SensorsAggregatedMsg,
    live: bool,
    now: i64,
    changes: &mut Vec<ChangeEvent>,
) {
    let device = device_entry(&mut state.devices, &hostname);
    mark_seen(device, &hostname, now, changes);

    for sensor in payload.sensors {
        let Some(line) = validate_line(&hostname, sensor.line) else {
            continue;
        };
        let s = &mut device.sensors[line as usize];
        if let (Some(seen), Some(end)) = (s.period_end_ts, payload.period_end_ts) {
            if end < seen {
                continue; // an older window replayed after a reconnect
            }
        }
        s.period_end_ts = payload.period_end_ts.or(s.period_end_ts);
        // Every window repeats the line's name and location. config/state
        // stays authoritative (it can clear them); a window only fills them
        // in, so a probe whose config/state has not arrived yet (a restart,
        // a missed retained message) still shows its current location.
        merge(&mut s.name, sensor.name);
        merge(&mut s.location, sensor.location);
        s.sample_count = sensor.sample_count;
        s.disconnected_count = sensor.disconnected_count;

        let temp = sensor.temperature;
        s.temperature_min_celsius = temp.as_ref().and_then(|t| t.min_celsius);
        s.temperature_avg_celsius = temp.as_ref().and_then(|t| t.avg_celsius);
        s.temperature_max_celsius = temp.and_then(|t| t.max_celsius);

        let counts = sensor.alarm_counts;
        s.alarm_counts = SensorAlarmCounts {
            normal: counts.as_ref().and_then(|c| c.normal).unwrap_or(0),
            warning: counts.as_ref().and_then(|c| c.warning).unwrap_or(0),
            critical: counts.as_ref().and_then(|c| c.critical).unwrap_or(0),
            disconnected: counts.as_ref().and_then(|c| c.disconnected).unwrap_or(0),
            reconnecting: counts.and_then(|c| c.reconnecting).unwrap_or(0),
        };
        // A live sample carries no onset time: keep the one the window or
        // the alarm event gave while the line stays in the same state.
        let same_state = s.dominant_alarm_state == sensor.dominant_alarm_state;
        if !(live && same_state && sensor.alarm_triggered_at.is_none()) {
            s.alarm_triggered_at = sensor.alarm_triggered_at;
        }
        s.dominant_alarm_state = sensor.dominant_alarm_state;
        // The Hub publishes all 8 lines every window whether or not a
        // probe is attached, so appearing in this payload is no
        // evidence of one. `sample_count > 0` is the actual "a probe
        // answered during this window" signal, and it is the same rule
        // `persist.rs`'s raw-reading path already applies -- keep the
        // two in step, or the stat tiles and the Device page's
        // `{online}/{total} Active` pill count phantom probes forever
        // (nothing else ever clears this flag).
        s.connected = s.sample_count > 0;
        s.updated_at = Some(now);

        if live {
            let shown = LiveNudge {
                at: now,
                temperature_dc: s.temperature_avg_celsius.map(|t| (t * 10.0).round() as i64),
                state: s.dominant_alarm_state.clone(),
                connected: s.connected,
            };
            if !live_nudge_due(s.live_nudge.as_ref(), &shown) {
                continue;
            }
            s.live_nudge = Some(shown);
        }
        changes.push(ChangeEvent::SensorLine {
            hostname: hostname.clone(),
            line,
        });
    }
}

/// Validates a device-supplied sensor line against the 8 pre-created lines
/// (0..7); logs and returns `None` on anything else so the caller can
/// `continue`/`return` and drop just that piece of data.
fn validate_line(hostname: &str, line: i64) -> Option<u8> {
    if (0..=7).contains(&line) {
        Some(line as u8)
    } else {
        tracing::warn!(hostname, line, "sensor line out of range 0..7 — dropping");
        None
    }
}

/// Sets `dst` only when `src` is present — the "merged state, null means
/// unknown" semantics ws.md documents for telemetry updates (system/power/
/// network), as opposed to `config/state`'s authoritative-snapshot
/// semantics (assign unconditionally, see the `ConfigState` arm below).
fn merge<T>(dst: &mut Option<T>, src: Option<T>) {
    if src.is_some() {
        *dst = src;
    }
}

/// A Node's signal from one frame: the best gateway wins; the envelope's own scalars are the
/// fallback for a transport that reports no per-gateway rows. `gateways` is replaced, not
/// merged. Shared by a live reading and the restart seed, so both pick the same values.
fn apply_signal(
    node: &mut NodeState,
    rssi: Option<i64>,
    snr: Option<f64>,
    gateways: Vec<NodeGatewayLink>,
) {
    match gateways
        .iter()
        .filter(|g| g.rssi.is_some())
        .max_by_key(|g| g.rssi)
    {
        Some(best) => {
            node.rssi = best.rssi;
            node.snr = best.snr;
        }
        None => {
            merge(&mut node.rssi, rssi);
            merge(&mut node.snr, snr);
        }
    }
    node.gateways = gateways;
}

/// Flips `online` true and bumps `last_seen`/`first_seen` — the behavior
/// ws.md/mqtt.md describe as shared by "most setters". Emits
/// `ChangeEvent::DeviceStatus` only on an actual offline->online transition.
/// **Never call this for `Standby`** (mqtt.md: `update_standby()` never
/// touches `online`/`last_seen` — retained-replay protection).
fn mark_seen(device: &mut DeviceState, hostname: &str, now: i64, changes: &mut Vec<ChangeEvent>) {
    if !device.online {
        device.online = true;
        changes.push(ChangeEvent::DeviceStatus {
            hostname: hostname.to_string(),
            online: true,
        });
    }
    if device.first_seen.is_none() {
        device.first_seen = Some(now);
    }
    device.last_seen = Some(now);
}

/// Look up — or create — the [`DeviceState`] for a Hub, keyed **case-insensitively**.
///
/// A hostname is case-insensitive (RFC 1035 §2.3.3), and before decision #21 the producers on
/// one physical Hub did not agree on its spelling: v1's `fiber_app` uppercased the topic
/// hostname (`FIBER-…`) while `control-radio` kept `/etc/hostname`'s lowercase (`fiber-…`).
/// Keying `devices` by the raw string split one Hub into two entries.
///
/// Decision #21 makes the hostname lowercase everywhere, so both the key and the stored
/// [`DeviceState::hostname`] are the lowercase spelling, whatever the event carried: the
/// relay publishes to it and the agent now subscribes on it.
fn device_entry<'a>(
    devices: &'a mut BTreeMap<String, DeviceState>,
    hostname: &str,
) -> &'a mut DeviceState {
    // One spelling (decision #21): the key and the stored hostname are both lowercase, so
    // no later event can re-spell the Hub.
    let hostname = hostname.to_ascii_lowercase();
    devices
        .entry(hostname.clone())
        .or_insert_with(|| DeviceState::new(hostname))
}

/// A control-radio `command` document as the `NodeCommandResult` payload the portal renders and
/// the pending walk confirms on: `{transport, command, result, seq, detail}`.
///
/// Only a frame that **answers a command** becomes one. `seq` 0 is the Node's own unsolicited
/// report (the boot `Info` and settings-info), and a page of a paged dump is not an answer
/// until the last one completes it. Anything else (history frames, samples, an unknown arm) is
/// not a command result either. `result` uses the vocabulary `sse::NODE_COMMAND_OK_RESULTS`
/// knows, so a refusal renders as an error.
fn v2_command_result(doc: &serde_json::Value) -> Option<serde_json::Value> {
    let seq = doc
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if seq == 0 {
        return None;
    }
    let kind = doc.get("kind").and_then(serde_json::Value::as_str)?;
    // The firmware's numeric error code, carried as it came: a consumer that acts on one
    // (the backfill watcher, on `HISTORY_UNAVAILABLE`) should not parse it out of `detail`.
    let code = doc
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(serde_json::Value::as_u64);
    let reason = (kind == control_radio::p2p::session::UNDELIVERED)
        .then(|| doc.get("reason").and_then(serde_json::Value::as_str))
        .flatten()
        .filter(|r| !r.trim().is_empty())
        .map(str::to_string);
    let (command, result, detail) = match kind {
        // A queued frame a newer identical ask took the place of (control.radio Q-N1, plan.md
        // §5 amended 2026-09-28): the Node answers the newer `seq`, never this one. No result,
        // so it closes quietly, neither an error nor a "not delivered". Nothing here waits on
        // the old `seq` of such an ask: the Portal's coalescing ones (`get-info`, `force-send`)
        // are confirmed by what the Node reports, not by a `seq`.
        control_radio::coalesce::SUPERSEDED => return None,
        // The Hub gave a queued downlink up after its unacked attempts (p2p_tower_gateway.md
        // §10.5, amended 2026-10-07): not the Node's answer, but the end of the wait for one.
        // Its own `result`, so a waiter can tell it from a refusal and the toast says what
        // happened.
        // A frame the network server discarded before sending it carries the reason (#129, a
        // LoRaWAN item too large for the Node's data rate); that is the detail, not attempts.
        control_radio::p2p::session::UNDELIVERED if reason.is_some() => (
            control_radio::p2p::session::UNDELIVERED,
            control_radio::p2p::session::UNDELIVERED,
            reason.clone(),
        ),
        control_radio::p2p::session::UNDELIVERED => {
            let attempts = doc
                .get("attempts")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::from(control_radio::p2p::session::UNDELIVERED_LIMIT));
            let last = doc
                .get("last_outcome")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            (
                control_radio::p2p::session::UNDELIVERED,
                control_radio::p2p::session::UNDELIVERED,
                Some(format!("{attempts} attempts, last {last}")),
            )
        }
        "ack" => ("ack", "ok", None),
        "info" => ("info", "ok", None),
        "config_dump" => {
            if doc.get("complete").and_then(serde_json::Value::as_bool) != Some(true) {
                return None;
            }
            ("config", "ok", None)
        }
        "w1_scan" => {
            let roms = doc
                .get("roms")
                .and_then(serde_json::Value::as_array)
                .map(|r| {
                    r.iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            ("w1_scan", "ok", Some(format!("ROMs: {roms}")))
        }
        "error" => {
            let e = doc.get("error");
            let code = e
                .and_then(|e| e.get("code"))
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string());
            let text = e
                .and_then(|e| e.get("detail"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            (
                "error",
                "error",
                Some(format!("code {code} {text}").trim().to_string()),
            )
        }
        _ => return None,
    };
    let mut out = serde_json::json!({
        "transport": "control.radio",
        "command": command,
        "result": result,
        "seq": seq,
    });
    if let Some(d) = detail {
        out["detail"] = serde_json::Value::from(d);
    }
    if let Some(r) = reason {
        out["reason"] = serde_json::Value::from(r);
    }
    if let (Some(code), "error") = (code, kind) {
        out["code"] = serde_json::Value::from(code);
    }
    Some(out)
}

/// The `transport` field of a retained `…/nodes/{dev_eui}/info` document, when it names one.
///
/// Since GitLab #73 this field is the only place the wire states which radio a Node speaks:
/// the topic family is transport-agnostic, and `reading`/`alarm` envelopes carry no transport
/// at all. A missing or unknown value is `None`, and the caller leaves the Node's transport
/// alone rather than guessing.
fn transport_from_info(info: &serde_json::Value) -> Option<portal_domain::Transport> {
    serde_json::from_value(info.get("transport")?.clone()).ok()
}

/// What a Node held about the carrier it left (node_registry.md §4). [`apply_node_info`] keeps
/// a value the new document does not name, so without this the old carrier's session (a P2P
/// DevAddr, NetID and join time) and its frame counter read on as the new carrier's.
fn forget_carrier_session(node: &mut NodeState) {
    node.dev_addr = None;
    node.net_id = None;
    node.session_state = None;
    node.last_join_at = None;
    // The frame counter of each carrier; sensor counters stay.
    node.counters.remove("counter");
    node.counters.remove("fCnt");
}

/// What a retained `…/nodes/{dev_eui}/info` document states about a Node, written onto its
/// `NodeState` (p2p_integration.md §4.0: `transport`, `serial_number`, `dev_addr`, `net_id`,
/// `session_state`, `joined_at`).
///
/// Only keys the document carries are written. A `registered` document has no session yet
/// and a LoRaWAN one never will, so an absent key must not erase what an earlier document
/// said; `session_state` is the exception the other way round — the central rewrites the
/// whole document on every transition, so a value it names replaces the last one.
///
/// `dev_addr`/`net_id` are numbers on the P2P wire (`control-radio` `p2p/runtime.rs`) and hex
/// strings on LoRaWAN; both render as text, so a number is kept as its decimal spelling.
fn apply_node_info(node: &mut NodeState, info: &serde_json::Value) {
    if let Some(transport) = transport_from_info(info) {
        node.transport = transport;
    }
    let text = |key: &str| match info.get(key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    merge(&mut node.serial_number, text("serial_number"));
    merge(&mut node.name, text("name"));
    merge(&mut node.dev_addr, text("dev_addr"));
    merge(&mut node.net_id, text("net_id"));
    if let Some(state) = info.get("session_state") {
        node.session_state = serde_json::from_value(state.clone()).ok();
    }
    let joined_at = text("joined_at").and_then(|s| {
        time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
            .ok()
            .map(|t| t.unix_timestamp())
    });
    merge(&mut node.last_join_at, joined_at);
    // After a portal restart the in-memory `last_seen` is gone and the header read "never ·
    // NeverConnected" until the Node's next reading, up to a report interval later. The retained
    // `info` carries the Hub's last uplink time, replayed on subscribe: it stands in until a
    // reading replaces it. Only when nothing better is held, so it never rolls a reading back.
    if node.last_seen.is_none() {
        if let Some(seen) = text("last_seen") {
            node.last_seen = Some(seen);
            if node.alarm_state == "NeverConnected" {
                node.alarm_state = "NORMAL".to_string();
            }
        }
    }
}

/// Lays the Portal's wired-line bounds ([`PortalState::line_limits`]) over whatever the Hub
/// echoed, after every store write (decision #41). A line with no Portal row keeps the echo.
fn overlay_line_limits(state: &mut PortalState) {
    let PortalState {
        devices,
        line_limits,
        ..
    } = state;
    for ((hostname, line), limits) in line_limits.iter() {
        if let Some(s) = devices
            .get_mut(hostname)
            .and_then(|d| d.sensors.get_mut(*line as usize))
        {
            s.thresholds = Some(limits.clone());
        }
    }
}

/// The `(name, location)` fields of one Node (`node`) or Beacon (`eye`) in the store.
fn label_slot<'a>(
    state: &'a mut PortalState,
    key: &(String, String, String),
) -> Option<(&'a mut Option<String>, &'a mut Option<String>)> {
    let (hostname, scope, entity) = key;
    let device = state.devices.get_mut(hostname)?;
    match scope.as_str() {
        "node" => device
            .nodes
            .get_mut(entity)
            .map(|n| (&mut n.name, &mut n.location)),
        "eye" => device
            .ble_sensors
            .get_mut(entity)
            .map(|b| (&mut b.name, &mut b.location)),
        _ => None,
    }
}

fn label_change(hostname: String, scope: &str, entity: &str) -> ChangeEvent {
    if scope == "eye" {
        ChangeEvent::Beacon {
            hostname,
            mac: entity.to_string(),
        }
    } else {
        ChangeEvent::Node {
            hostname,
            node_key: entity.to_string(),
        }
    }
}

/// Lays the Portal's Node and Beacon names and locations ([`PortalState::meta_labels`]) over
/// whatever the Hub reported, after every store write (decision #41). What the Hub had under
/// an overlaid half is kept in `hub_labels`, for when the Portal's text is cleared.
fn overlay_meta_labels(state: &mut PortalState) {
    if state.meta_labels.is_empty() {
        return;
    }
    let labels: Vec<_> = state
        .meta_labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (key, (name, location)) in labels {
        let mut hub = state.hub_labels.get(&key).cloned().unwrap_or_default();
        let Some((n, l)) = label_slot(state, &key) else {
            continue;
        };
        if let Some(want) = name {
            if n.as_deref() != Some(want.as_str()) {
                hub.0 = n.take();
                *n = Some(want);
            }
        }
        if let Some(want) = location {
            if l.as_deref() != Some(want.as_str()) {
                hub.1 = l.take();
                *l = Some(want);
            }
        }
        state.hub_labels.insert(key, hub);
    }
}

/// Moves the value of every switched-off sensor out of the place the pages read it from and
/// into the entity's `disabled_fields`, and drops its alarm state (GitLab #120). Run after every
/// store write that can bring a value in, so no reader ever sees a switched-off sensor. Cheap:
/// it walks the switched-off set, which is empty on most sites.
fn park_disabled_sensors(state: &mut PortalState) {
    if state.disabled_sensors.is_empty() {
        return;
    }
    let PortalState {
        devices,
        disabled_sensors,
        ..
    } = state;
    for (hostname, scope, entity, field) in disabled_sensors.iter() {
        let Some(device) = devices.get_mut(hostname) else {
            continue;
        };
        if scope == "node" {
            let Some(node) = device.nodes.get_mut(entity) else {
                continue;
            };
            let parked = node.disabled_fields.entry(field.clone()).or_default();
            if let Some(v) = node.fields.remove(field) {
                *parked = Some(v);
            }
            // A count sensor's value lives in `counters` (GitLab #134).
            if let Some(c) = node.counters.remove(field) {
                *parked = Some(c as f64);
            }
            // Off is not "on without a sample".
            node.no_data_fields.remove(field);
            if node.field_alarm_states.remove(field).is_some() {
                node.field_alarm_triggered_at.remove(field);
                if node.alarm_state != "NeverConnected" {
                    node.alarm_state = worst_field_state(&node.field_alarm_states).to_string();
                }
            }
        } else if scope == "eye" {
            let Some(beacon) = device.ble_sensors.get_mut(entity) else {
                continue;
            };
            let value = match field.as_str() {
                "temperature" => beacon.temperature.take(),
                "humidity" => beacon.humidity.take().map(|v| v as f64),
                _ => None,
            };
            let parked = beacon.disabled_fields.entry(field.clone()).or_default();
            if value.is_some() {
                *parked = value;
            }
            if beacon.field_alarm_states.remove(field).is_some() {
                beacon.field_alarm_triggered_at.remove(field);
            }
        }
    }
}

/// Whether a Node quantity is a count, kept in `NodeState::counters` (GitLab #134).
fn is_count_field(field: &str) -> bool {
    fiber_protocol::lorawan_field(field)
        .is_some_and(|f| f.kind == fiber_protocol::LorawanFieldKind::Counter)
}

/// The reverse of [`park_disabled_sensors`] for one sensor being switched back on.
fn unpark_sensor(device: &mut DeviceState, scope: &str, entity: &str, field: &str) {
    if scope == "node" {
        if let Some(node) = device.nodes.get_mut(entity) {
            if let Some(Some(v)) = node.disabled_fields.remove(field) {
                if is_count_field(field) {
                    node.counters
                        .entry(field.to_string())
                        .or_insert(v.round() as i64);
                } else {
                    node.fields.entry(field.to_string()).or_insert(v);
                }
            }
        }
    } else if scope == "eye" {
        if let Some(beacon) = device.ble_sensors.get_mut(entity) {
            if let Some(Some(v)) = beacon.disabled_fields.remove(field) {
                match field {
                    "temperature" => {
                        beacon.temperature.get_or_insert(v);
                    }
                    "humidity" => {
                        beacon.humidity.get_or_insert(v.round() as i64);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// One `proximos/…/nodes/{id}/{reading|alarm}` document into `NodeState`.
///
/// The v1 `lorawan/sensors` arm above is the model for what lands where; the differences are
/// all consequences of the envelope carrying **one** node and of `NodeReading`'s field set:
///
/// - No "drop a sensor with no fields" rule. That existed because v1's array carried a row per
///   Node whether or not it had reported; here a document *is* an uplink, and an alarm batch
///   legitimately carries events and no measurements.
/// - `last_seen` is the envelope's `received_at` — verbatim, because it is the persistence
///   dedup key on the other side too.
/// - No `field_alarm_states`/`field_thresholds`: the v2 envelope carries neither. Threshold
///   evaluation is the portal's own (`portal_domain::on_field_states` over stored thresholds),
///   not something the Hub echoes back.
fn apply_node_reading(
    state: &mut PortalState,
    changes: &mut Vec<ChangeEvent>,
    hostname: String,
    node_key: String,
    payload: super::events::NodeReadingMsg,
    now: i64,
) {
    let device = device_entry(&mut state.devices, &hostname);
    mark_seen(device, &hostname, now, changes);

    // B4: a true insert, distinguished from the upsert every reading does.
    let inserted = !device.nodes.contains_key(&node_key);
    // The envelope says nothing about the radio; the retained `info` document does, and it is
    // normally already here (retained, so replayed on subscribe before any live reading). A
    // first reading that beats its `info` keeps the model default until the document lands.
    let transport = device
        .node_infos
        .get(&node_key)
        .and_then(transport_from_info);
    let node = device
        .nodes
        .entry(node_key.clone())
        .or_insert_with(|| NodeState::new(node_key.clone(), now));
    if let Some(transport) = transport {
        node.transport = transport;
    }
    merge(&mut node.serial_number, payload.serial.clone());

    // fields/counters: merged, never pruned — same rule as the v1 arm, so a sensor that goes
    // quiet keeps its last value rather than disappearing from the rendered list.
    node.fields
        .extend(payload.fields.iter().map(|(k, v)| (k.clone(), *v)));
    node.counters.extend(
        payload
            .counters
            .iter()
            // `u64` on the wire, `i64` in the model. A count past `i64::MAX` is not a count,
            // so it is dropped rather than wrapped into a negative one.
            .filter_map(|(k, v)| i64::try_from(*v).ok().map(|v| (k.clone(), v))),
    );
    // A real value ends a quantity's `null` state (`no_data_fields`, below).
    for name in payload.fields.keys().chain(payload.counters.keys()) {
        node.no_data_fields.remove(name);
    }

    // A field the frame names as null (the decoder's `no_data` event, sticker-firmware NA
    // sentinel) is an explicit "no valid sample now": drop the held value so the row reads
    // '–', as it does after a restart. A field merely absent from the frame keeps its value
    // (paged reports carry a slice). Nothing is persisted for it: no sentinel, no null row.
    // The field goes into `no_data_fields`, so it still counts as reported and its row
    // stays, reading '–', instead of vanishing like a sensor that is off (#120).
    for field in no_data_names(&payload.events) {
        if payload.fields.contains_key(field) || payload.counters.contains_key(field) {
            continue;
        }
        node.fields.remove(field);
        node.counters.remove(field);
        node.no_data_fields.insert(field.to_string());
    }

    for event in &payload.events {
        if let Ok(raw) = serde_json::to_value(event) {
            node.recent_events.push(raw);
        }
    }
    let excess = node.recent_events.len().saturating_sub(RECENT_EVENTS_CAP);
    if excess > 0 {
        node.recent_events.drain(..excess);
    }

    let gateways = payload
        .gateways
        .iter()
        .map(|g| NodeGatewayLink {
            // `Option` in the model (v1's wire allowed a row with no id); the v2 envelope
            // always names the gateway that heard the frame.
            gateway_id: Some(g.gateway_id.clone()),
            rssi: g.rssi.map(i64::from),
            snr: g.snr,
        })
        .collect();
    apply_signal(node, payload.rssi.map(i64::from), payload.snr, gateways);

    merge(&mut node.dr, payload.dr.map(i64::from));
    node.last_seen = Some(payload.received_at.clone());
    // The v2 envelope carries no aggregate alarm state, so the model default stuck: a Node
    // reporting every 15 minutes read "NeverConnected" for good (WP4 on 0413, N3). A reading
    // is the proof it connected. The value axis stays Normal until the portal evaluates its
    // own bounds on v2 telemetry (phase 2); liveness is the offline detector's either way.
    if node.alarm_state == "NeverConnected" {
        node.alarm_state = "NORMAL".to_string();
    }

    // Before the batch, so a connection that prepends the row then repaints
    // it rather than the other way round.
    if inserted {
        changes.push(ChangeEvent::NodeAdded {
            hostname: hostname.clone(),
            node_key,
        });
    }
    changes.push(ChangeEvent::NodeBatch { hostname });
}

/// The quantities a reading sent as `null`: the `field` of each `no_data` event the Hub's
/// decoder raises for one (control-radio `decode::reading`). An alarm's `no_data` side is an
/// `alarm` event, not this one.
fn no_data_names(
    events: &[control_radio::decode::reading::NodeEvent],
) -> impl Iterator<Item = &str> {
    events
        .iter()
        .filter(|e| e.event_type == "no_data")
        .filter_map(|e| e.extra.get("field").and_then(serde_json::Value::as_str))
}

/// Recomputes a `field_alarm_triggered_at` map against a wholesale-replaced
/// `field_alarm_states` map (shared by Nodes and beacons, ws.md): a field
/// keeps its stamp while it stays in WARNING/CRITICAL, gets stamped `now`
/// when it *enters* that zone, and loses the entry on recovery (or when the
/// field vanishes from the replaced states map).
/// The overall value state of a set of field states, in the firmware casing the v1 Hub sends:
/// `CRITICAL` over `WARNING` over `NORMAL`.
fn worst_field_state(states: &BTreeMap<String, String>) -> &'static str {
    let has = |s: &str| states.values().any(|v| v.eq_ignore_ascii_case(s));
    if has("CRITICAL") {
        "CRITICAL"
    } else if has("WARNING") {
        "WARNING"
    } else {
        "NORMAL"
    }
}

fn restamp_field_alarms(
    old_states: &BTreeMap<String, String>,
    old_triggered: &BTreeMap<String, i64>,
    new_states: &BTreeMap<String, String>,
    now: i64,
) -> BTreeMap<String, i64> {
    fn in_alarm(s: &str) -> bool {
        s.eq_ignore_ascii_case("WARNING") || s.eq_ignore_ascii_case("CRITICAL")
    }
    let mut out = BTreeMap::new();
    for (field, state) in new_states {
        if !in_alarm(state) {
            continue;
        }
        let was_alarm = old_states.get(field).is_some_and(|s| in_alarm(s));
        let stamp = if was_alarm {
            old_triggered.get(field).copied().unwrap_or(now)
        } else {
            now
        };
        out.insert(field.clone(), stamp);
    }
    out
}

/// Wire `FieldThreshold` list → model list (1:1 fields).
fn map_field_thresholds(
    thresholds: Vec<fiber_protocol::payloads::lorawan::FieldThreshold>,
) -> Vec<NodeFieldThreshold> {
    thresholds
        .into_iter()
        .map(|t| NodeFieldThreshold {
            field: t.field,
            critical_low: t.critical_low,
            warning_low: t.warning_low,
            warning_high: t.warning_high,
            critical_high: t.critical_high,
        })
        .collect()
}

fn apply_locked(state: &mut PortalState, ev: IngestEvent, changes: &mut Vec<ChangeEvent>) {
    let now = now_unix();
    match ev {
        IngestEvent::Status { hostname, status } => {
            let online = matches!(status, DeviceStatus::Online);
            let device = device_entry(&mut state.devices, &hostname);
            if device.online != online {
                device.online = online;
                if online {
                    if device.first_seen.is_none() {
                        device.first_seen = Some(now);
                    }
                    device.last_seen = Some(now);
                }
                changes.push(ChangeEvent::DeviceStatus { hostname, online });
            }
        }

        IngestEvent::SensorsAggregated { hostname, payload } => {
            apply_sensor_lines(state, hostname, payload, false, now, changes);
        }
        IngestEvent::SensorsLive { hostname, payload } => {
            apply_sensor_lines(state, hostname, payload, true, now, changes);
        }

        IngestEvent::SystemInfo { hostname, payload } => {
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);

            merge(&mut device.version, payload.firmware_version.clone());

            device.system.hostname = Some(hostname.clone());
            merge(&mut device.system.version, payload.firmware_version);
            merge(&mut device.system.uptime_seconds, payload.uptime_seconds);
            merge(&mut device.system.uptime_human, payload.uptime_human);
            merge(&mut device.system.device_label, payload.device_label);
            if let Some(partition) = payload.storage.and_then(|s| s.data_partition) {
                merge(
                    &mut device.system.storage.total_bytes,
                    partition.total_bytes,
                );
                merge(
                    &mut device.system.storage.available_bytes,
                    partition.available_bytes,
                );
                merge(
                    &mut device.system.storage.used_percent,
                    partition.used_percent,
                );
            }
            // Assigned, not merged — the one block in this arm that follows its source all the
            // way down to absence (GitLab #58). Everything around it is telemetry, where "null
            // means unknown" and a gap must never overwrite a reading. `setup` is not a reading:
            // it mirrors the Hub's own `setup:` config key, and the block going away *is* an
            // answer — a factory reset, or a firmware downgrade to an agent that no longer
            // publishes it. Merged, the page would keep showing "provisioned, server" for a Hub
            // that is back out of the box, with nothing to say how old the claim is.
            let setup = payload.setup;
            device.system.setup_provisioned = setup.as_ref().and_then(|s| s.provisioned);
            device.system.setup_role = setup.and_then(|s| s.role);
            // Assigned for the same reason: it is the unit's identity as the Hub reads it, not
            // a reading. A Hub that stops sending it (an agent downgrade) has stopped vouching
            // for it, and the card goes back to "—" rather than showing what an older agent said.
            device.system.hardware = payload.hardware;
            // Assigned like `hardware`: the update's state must follow the Hub down to `idle`
            // (southbridge_update.md §3.7), and an agent that stops sending the block has
            // stopped vouching for the versions in it.
            device.system.southbridge = payload.southbridge;
            device.system.updated_at = Some(now);

            if let Some(power) = payload.power {
                if let Some(battery) = power.battery {
                    merge(&mut device.power.battery_voltage_mv, battery.voltage_mv);
                    merge(&mut device.power.battery_percentage, battery.percentage);
                    merge(&mut device.power.battery_status, battery.status);
                }
                if let Some(dc) = power.dc {
                    merge(&mut device.power.dc_voltage_mv, dc.voltage_mv);
                    merge(&mut device.power.dc_connected, dc.connected);
                }
                merge(&mut device.power.last_dc_loss, power.last_dc_loss);
            }
            device.power.updated_at = Some(now);

            if let Some(network) = payload.network {
                if let Some(wifi) = network.wifi {
                    merge(&mut device.network.wifi_connected, wifi.connected);
                    merge(&mut device.network.wifi_signal_dbm, wifi.signal_dbm);
                    merge(&mut device.network.wifi_ip, wifi.ip);
                }
                if let Some(ethernet) = network.ethernet {
                    merge(&mut device.network.ethernet_connected, ethernet.connected);
                    merge(&mut device.network.ethernet_ip, ethernet.ip);
                }
                merge(&mut device.network.has_internet, network.has_internet);
            }
            device.network.updated_at = Some(now);

            // devices.hub.md §12 A: the radio block feeds the Hub page's "Radio & gateways"
            // card. EUIs are stored lowercase, the portal's convention everywhere else
            // (`node_key` in `mqtt::routing`), so the local gateway joins against the
            // `radio/gateways` rows. Emitted only on a real change: `system/info` repeats
            // every interval and this block is near-static between them.
            if let Some(mut lorawan) = payload.lorawan {
                if let Some(eui) = lorawan.gateway_eui.as_mut() {
                    eui.make_ascii_lowercase();
                }
                if let Some(eui) = lorawan.join_eui.as_mut() {
                    eui.make_ascii_lowercase();
                }
                if device.lorawan.as_ref() != Some(&lorawan) {
                    device.lorawan = Some(lorawan);
                    changes.push(ChangeEvent::Radio {
                        hostname: hostname.clone(),
                    });
                }
            }

            changes.push(ChangeEvent::System {
                hostname: hostname.clone(),
            });
            changes.push(ChangeEvent::Power {
                hostname: hostname.clone(),
            });
            changes.push(ChangeEvent::Network {
                hostname: hostname.clone(),
            });
            changes.push(ChangeEvent::Storage { hostname });
        }

        IngestEvent::AlarmEvent { hostname, payload } => {
            let Some(line) = validate_line(&hostname, payload.line) else {
                return;
            };
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);

            let s = &mut device.sensors[line as usize];
            if payload.to_state.eq_ignore_ascii_case("normal") {
                s.alarm_triggered_at = None;
            } else {
                s.alarm_triggered_at = payload.timestamp;
            }
            s.dominant_alarm_state = payload.to_state;
            s.updated_at = Some(now);

            changes.push(ChangeEvent::Alarm {
                hostname: hostname.clone(),
            });
            changes.push(ChangeEvent::SensorLine { hostname, line });
        }

        IngestEvent::Standby { hostname, payload } => {
            // No mark_seen: standby never touches online/last_seen.
            let device = device_entry(&mut state.devices, &hostname);
            device.standby.active = payload.standby;
            device.standby.reason = payload.reason;
            device.standby.requested_by = payload.requested_by;
            device.standby.entered_at = payload.entered_at;
            device.standby.vin_mv = payload.vin_mv;
            device.standby.updated_at = Some(now);

            changes.push(ChangeEvent::Standby { hostname });
        }

        IngestEvent::ConfigState { hostname, payload } => {
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);

            merge(&mut device.config.led_brightness, payload.led_brightness);
            merge(
                &mut device.config.screen_brightness,
                payload.screen_brightness,
            );
            merge(&mut device.config.buzzer_volume, payload.buzzer_volume);
            merge(
                &mut device.config.system_info_interval_s,
                payload.system_info_interval_s,
            );
            merge(
                &mut device.config.screen_timeout_secs,
                payload.screen_timeout_secs,
            );
            merge(&mut device.config.device_label, payload.device_label);
            if let Some(iv) = &payload.intervals {
                merge(&mut device.config.sample_interval_ms, iv.sample_interval_ms);
                merge(
                    &mut device.config.aggregation_interval_ms,
                    iv.aggregation_interval_ms,
                );
            }
            // Authoritative when stated, like the per-sensor block below:
            // `Some([])` is the Hub saying it renders the built-in layout,
            // an absent key makes no statement and leaves the last list.
            if let Some(lines) = payload.display_lines {
                device.config.display_lines = lines;
            }
            if payload.intervals.is_some() {
                device.config.intervals = payload.intervals;
            }

            for entry in payload.sensors {
                let Some(line) = validate_line(&hostname, entry.line) else {
                    continue;
                };
                let s = &mut device.sensors[line as usize];
                // config/state is the authoritative device snapshot (unlike
                // the telemetry merges above): assign directly, including
                // clearing to `None`, rather than keep-old-on-null.
                s.name = entry.name;
                s.location = entry.location;
                s.thresholds = entry.thresholds.map(|t| SensorThresholds {
                    critical_low: t.critical_low,
                    warning_low: t.warning_low,
                    warning_high: t.warning_high,
                    critical_high: t.critical_high,
                    alarm_low: t.alarm_low,
                    alarm_high: t.alarm_high,
                    hysteresis: t.hysteresis,
                    enabled: t.enabled,
                });

                changes.push(ChangeEvent::SensorLine {
                    hostname: hostname.clone(),
                    line,
                });
            }

            // M5-C: `config/state` is THE authoritative Node inventory
            // (mqtt.md): a Node absent from `lorawan_sensors` here is
            // removed. Runs unconditionally — the wire key defaults to []
            // when absent, exactly like v1's handler.
            //
            // #89: authoritative for the **v1** inventory only. A Node with a
            // v2 inventory document (the retained `nodes/{eui}/info`, which
            // states a `transport` — v1's `lorawan/sensors/{eui}/info` never
            // does) is governed by that document and removed by its
            // tombstone. On a v2 Hub `fiber_app`'s legacy stack is gated off,
            // so `lorawan_sensors` is always `[]`, and without this every
            // `config/state` wiped every v2 Node — a registered one that had
            // not reported yet vanished from `/nodes` for good.
            let active: BTreeSet<String> = payload
                .lorawan_sensors
                .iter()
                .map(|e| e.dev_eui.to_lowercase())
                .collect();
            let v2_inventory: BTreeSet<String> = device
                .node_infos
                .iter()
                .filter(|(_, doc)| transport_from_info(doc).is_some())
                .map(|(node_key, _)| node_key.clone())
                .collect();
            let mut removed = Vec::new();
            device.nodes.retain(|node_key, _| {
                let keep = active.contains(node_key) || v2_inventory.contains(node_key);
                if !keep {
                    removed.push(node_key.clone());
                }
                keep
            });
            for node_key in removed {
                changes.push(ChangeEvent::NodeRemoved {
                    hostname: hostname.clone(),
                    node_key,
                });
            }
            for entry in payload.lorawan_sensors {
                let node_key = entry.dev_eui.to_lowercase();
                // B4: `/nodes` prepends a row on a true insert, so the
                // insert has to be distinguishable from the upsert that
                // runs on every inventory publish.
                let inserted = !device.nodes.contains_key(&node_key);
                let node = device
                    .nodes
                    .entry(node_key.clone())
                    .or_insert_with(|| NodeState::new(node_key.clone(), now));
                if v2_inventory.contains(&node_key) {
                    // #89 again, for identity (Nodes test 2026-09-23, F22): a Node with a v2
                    // inventory document is named by that document. The agent still lists it
                    // for its thresholds, with no name or serial, and assigning those cleared
                    // the Node's identity on every `config/state` until the next uplink
                    // re-sent `info`.
                    merge(&mut node.name, entry.name);
                    merge(&mut node.serial_number, entry.serial_number);
                    merge(&mut node.location, entry.location);
                } else {
                    // Authoritative snapshot: assign directly (may clear), same
                    // convention as the wired-sensor block above.
                    node.name = entry.name;
                    node.serial_number = entry.serial_number;
                    node.location = entry.location;
                }
                node.enabled = entry.enabled;
                // Present as a list ⇒ authoritative; absent keeps the cache.
                if let Some(thresholds) = entry.field_thresholds {
                    node.field_thresholds = Some(map_field_thresholds(thresholds));
                }
                if inserted {
                    changes.push(ChangeEvent::NodeAdded {
                        hostname: hostname.clone(),
                        node_key: node_key.clone(),
                    });
                }
                changes.push(ChangeEvent::Node {
                    hostname: hostname.clone(),
                    node_key,
                });
            }

            // `config/state.eye_tags` is THE authoritative **enrolled**
            // inventory, exactly as `lorawan_sensors` is for Nodes — so a
            // beacon that is no longer in it has been removed from the fleet
            // and goes (B6; before that this arm was upsert-only and a
            // removed Beacon's row survived until the process restarted,
            // which is what devices.beacon.md §5's "beacon zmizí ze seznamu
            // registrovaných" is about).
            //
            // Two things this is careful about, and neither has a Node twin:
            //
            // * **Only the enrolled half.** A discovered-only tag is absent
            //   from `eye.tags` *by definition* — it is a stranger the radio
            //   overheard. Its membership question is `eye/sensors`', not
            //   this topic's (the `EyeSensors` arm's reconcile-by-absence).
            // * **`None` is not `[]`.** `lorawan_sensors` defaults to an
            //   empty vector when the key is absent, so absence prunes; here
            //   the field is `Option`, and a document that simply does not
            //   mention beacons must not empty the fleet.
            if let Some(listed) = payload.eye_tags.as_ref() {
                let enrolled: BTreeSet<String> =
                    listed.iter().map(|t| t.mac.to_uppercase()).collect();
                let mut removed = Vec::new();
                device.ble_sensors.retain(|mac, beacon| {
                    let keep = beacon.discovered == Some(true) || enrolled.contains(mac);
                    if !keep {
                        removed.push(mac.clone());
                    }
                    keep
                });
                for mac in removed {
                    tracing::debug!(
                        hostname = %hostname,
                        mac = %mac,
                        reason = "absent from config/state.eye_tags",
                        "removing an enrolled beacon"
                    );
                    changes.push(ChangeEvent::BeaconRemoved {
                        hostname: hostname.clone(),
                        mac,
                        // The `retain` above keeps every `discovered` beacon,
                        // so anything this arm removes was enrolled.
                        discovered: false,
                    });
                }
            }

            // A config-listed tag is by definition registered: `discovered`
            // resets to `None` ("registered", ws.md).
            for tag in payload.eye_tags.unwrap_or_default() {
                let mac = tag.mac.to_uppercase();
                let newly_enrolled = device
                    .ble_sensors
                    .get(&mac)
                    .is_none_or(|b| b.discovered == Some(true));
                if newly_enrolled {
                    changes.push(ChangeEvent::BeaconAdded {
                        hostname: hostname.clone(),
                        mac: mac.clone(),
                    });
                }
                let beacon = device
                    .ble_sensors
                    .entry(mac.clone())
                    .or_insert_with(|| BeaconState::new(mac.clone(), now));
                beacon.name = tag.name;
                beacon.location = tag.location;
                beacon.discovered = None;
                changes.push(ChangeEvent::Beacon {
                    hostname: hostname.clone(),
                    mac,
                });
            }

            if let Some(eye) = payload.eye {
                merge(&mut device.config.beacon_enabled, eye.enabled);
                merge(&mut device.config.beacon_auto_provision, eye.auto_provision);
                merge(&mut device.config.beacon_auto_discover, eye.auto_discover);
            }

            changes.push(ChangeEvent::Config { hostname });
        }

        IngestEvent::Probe1m {
            hostname,
            line,
            payload: _,
        } => {
            // M1's StateStore holds no per-minute aggregates in memory —
            // that's portal-storage's job (design.md §4). This arm only
            // proves liveness and validates the line; no rendered fragment
            // depends on it, so no ChangeEvent is emitted.
            if validate_line(&hostname, line as i64).is_none() {
                return;
            }
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);
        }

        IngestEvent::DeviceError {
            hostname,
            payload: _,
        } => {
            // No dedicated state field for device errors in the M1 subset
            // (v1 doesn't cache device_error either, ws.md line 13); this
            // only proves liveness.
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);
        }

        IngestEvent::LorawanSensors { hostname, payload } => {
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);

            let mut upserted = false;
            let mut inserted: Vec<String> = Vec::new();
            for sensor in payload.sensors {
                // v1 drops sensors whose payload lacks `fields` entirely;
                // serde defaults a missing key to an empty map, so empty ==
                // absent here.
                if sensor.fields.is_empty() {
                    tracing::debug!(
                        hostname,
                        dev_eui = %sensor.dev_eui,
                        "lorawan sensor without fields — dropping (v1 parity)"
                    );
                    continue;
                }
                let node_key = sensor.dev_eui.to_lowercase();
                // B4: the third insert site, and it announces itself like
                // the other two. A Node can reach the store through
                // telemetry before its inventory publish arrives, and
                // `/nodes` prepends its row on `NodeAdded` whichever door
                // it came through.
                if !device.nodes.contains_key(&node_key) {
                    inserted.push(node_key.clone());
                }
                let node = device
                    .nodes
                    .entry(node_key.clone())
                    .or_insert_with(|| NodeState::new(node_key, now));

                merge(&mut node.name, sensor.name);
                merge(&mut node.serial_number, sensor.serial_number);
                merge(&mut node.location, sensor.location);

                // fields/counters: merged, never pruned.
                node.fields.extend(sensor.fields);
                node.counters.extend(sensor.counters);

                // field_alarm_states: replaced wholesale (firmware casing
                // kept raw); triggered_at restamped against the transition.
                let new_states: BTreeMap<String, String> =
                    sensor.field_alarm_states.into_iter().collect();
                node.field_alarm_triggered_at = restamp_field_alarms(
                    &node.field_alarm_states,
                    &node.field_alarm_triggered_at,
                    &new_states,
                    now,
                );
                node.field_alarm_states = new_states;

                if let Some(thresholds) = sensor.field_thresholds {
                    node.field_thresholds = Some(map_field_thresholds(thresholds));
                }

                // recent_events: raw firmware dicts, tail-capped.
                for event in sensor.events {
                    // to_value on a Serialize struct can only fail on a
                    // non-string map key — impossible here, but stay
                    // panic-free per crate rules.
                    if let Ok(raw) = serde_json::to_value(event) {
                        node.recent_events.push(raw);
                    }
                }
                let excess = node.recent_events.len().saturating_sub(RECENT_EVENTS_CAP);
                if excess > 0 {
                    node.recent_events.drain(..excess);
                }

                // rssi/snr: best gateway wins; top-level values are only a
                // fallback when no gateway reports an RSSI.
                match sensor
                    .gateways
                    .iter()
                    .filter(|g| g.rssi.is_some())
                    .max_by_key(|g| g.rssi)
                {
                    Some(best) => {
                        node.rssi = best.rssi;
                        node.snr = best.snr;
                    }
                    None => {
                        merge(&mut node.rssi, sensor.rssi);
                        merge(&mut node.snr, sensor.snr);
                    }
                }
                node.gateways = sensor
                    .gateways
                    .into_iter()
                    .map(|g| NodeGatewayLink {
                        gateway_id: g.gateway_id,
                        rssi: g.rssi,
                        snr: g.snr,
                    })
                    .collect();

                merge(&mut node.fcnt, sensor.fcnt);
                merge(&mut node.dr, sensor.dr);
                merge(&mut node.downlink_gateway_id, sensor.downlink_gateway_id);
                // Node uplink time string, verbatim — persistence dedup key.
                merge(&mut node.last_seen, sensor.last_seen);
                if let Some(alarm_state) = sensor.alarm_state {
                    node.alarm_state = alarm_state;
                }
                upserted = true;
            }
            // Inserts first, so a connection prepends the row and then
            // repaints it rather than the other way round.
            for node_key in inserted {
                changes.push(ChangeEvent::NodeAdded {
                    hostname: hostname.clone(),
                    node_key,
                });
            }
            // One batch per publish, skipped when nothing survived (v1
            // parity; per-Node events are suppressed in favor of the batch).
            if upserted {
                changes.push(ChangeEvent::NodeBatch { hostname });
            }
        }

        IngestEvent::EyeSensors { hostname, payload } => {
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);

            // B6 (devices.backend.md §9, §22.2): the snapshot's **membership**
            // is authoritative for discovered-only beacons. A stranger the Hub
            // can still hear is in `tags`; one it cannot is gone, whether or
            // not it was ever flagged `stale`. `stale` no longer prunes
            // anything — it means "advertising has stopped", which is exactly
            // the `connected` flag.
            let capped_changed = device.discovered_capped != payload.discovered_capped;
            device.discovered_capped = payload.discovered_capped;
            let mut upserted = false;
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for tag in payload.tags {
                let mac = tag.mac.to_uppercase();
                seen.insert(mac.clone());
                // Absent `stale` reads as "not stale" (v1's falsy check).
                let stale = tag.stale.unwrap_or(false);
                // The raw tag, kept for the detail page's §3.3 preview —
                // serialized back from the parsed struct rather than sliced
                // out of the incoming bytes, because ingest is typed and the
                // envelope is already gone by here. Fields the Hub omitted
                // therefore show as `null`, which is what the store saw.
                let raw = serde_json::to_value(&tag).ok();
                let beacon = device
                    .ble_sensors
                    .entry(mac.clone())
                    .or_insert_with(|| BeaconState::new(mac, now));

                merge(&mut beacon.name, tag.name);
                merge(&mut beacon.temperature, tag.temperature_c);
                merge(&mut beacon.humidity, tag.humidity_pct);
                merge(&mut beacon.battery_mv, tag.battery_mv);
                merge(&mut beacon.low_battery, tag.low_battery);
                merge(&mut beacon.magnet_present, tag.magnet_present);
                merge(&mut beacon.magnet_detected, tag.magnet_detected);
                merge(&mut beacon.moving, tag.moving);
                merge(&mut beacon.movement_count, tag.movement_count);
                merge(&mut beacon.pitch, tag.pitch_deg);
                merge(&mut beacon.roll, tag.roll_deg);
                merge(&mut beacon.rssi, tag.rssi);
                merge(&mut beacon.last_seen, tag.last_seen_ts);
                beacon.connected = !stale;
                // Status flags where null is meaningful (None = registered
                // for `discovered`): assigned verbatim, not merged.
                beacon.provisioning = tag.provisioning;
                beacon.discovered = tag.discovered;
                // Tri-state: an absent key is no news (the Hub's trimmed
                // payload rungs omit it), `null` clears, a value replaces.
                if let Some(ble_config) = tag.ble_config {
                    beacon.ble_config = ble_config;
                }

                let new_states: BTreeMap<String, String> =
                    tag.field_alarm_states.into_iter().collect();
                beacon.field_alarm_triggered_at = restamp_field_alarms(
                    &beacon.field_alarm_states,
                    &beacon.field_alarm_triggered_at,
                    &new_states,
                    now,
                );
                beacon.field_alarm_states = new_states;
                if let Some(alarm_state) = tag.alarm_state {
                    beacon.alarm_state = alarm_state;
                }
                beacon.last_snapshot_at = Some(now);
                beacon.last_payload = raw;
                upserted = true;
            }

            // Reconcile by absence — **outside** the `upserted` guard, because
            // `tags: []` is the empty snapshot and must prune, and by
            // definition it upserts nothing. Enrolled beacons are never
            // pruned: they belong to `eye.tags` on the Hub, and a quiet one is
            // an outage the offline detector reports, not a device that
            // stopped existing.
            //
            // A **capped** snapshot prunes nothing. `discovered_capped` says
            // the Hub (or the payload ladder) dropped strangers to fit, so
            // absence no longer means "gone" and one oversized publish would
            // otherwise empty the whole scan table. This is the reason the
            // flag had to reach the wire before this rule could ship
            // (devices.backend.md §22.1).
            let pruned: Vec<String> = if payload.discovered_capped {
                Vec::new()
            } else {
                device
                    .ble_sensors
                    .iter()
                    .filter(|(mac, b)| b.discovered == Some(true) && !seen.contains(*mac))
                    .map(|(mac, _)| mac.clone())
                    .collect()
            };
            if !pruned.is_empty() {
                tracing::debug!(
                    hostname = %hostname,
                    count = pruned.len(),
                    reason = "absent from the eye/sensors snapshot",
                    "pruning discovered-only beacons"
                );
            }
            for mac in pruned {
                device.ble_sensors.remove(&mac);
                changes.push(ChangeEvent::BeaconRemoved {
                    hostname: hostname.clone(),
                    mac,
                    // Reconcile-by-absence only ever touches strangers.
                    discovered: true,
                });
                upserted = true;
            }

            // The panel repaints on a **cap change** too, even when the
            // snapshot upserted and pruned nothing: `discovered_capped` is
            // drawn as a notice above the table (§22.1), and a Hub that has
            // just come back under its own limit clears that notice without
            // any row changing (B6 review, item 11).
            if upserted || capped_changed {
                changes.push(ChangeEvent::BeaconBatch { hostname });
            }
        }

        IngestEvent::StickerConfig {
            hostname,
            node_key,
            payload,
        } => {
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);

            let node_key = node_key.to_lowercase();
            let entry = device.node_configs.entry(node_key.clone()).or_default();
            // Config keys merge into the cached page set; metadata is taken
            // as sent (no recomputation on the plain config topic).
            entry.config.extend(payload.config);
            merge(&mut entry.synced_at, payload.synced_at);
            if let Some(ack) = payload.last_ack {
                entry.last_ack = Some(NodeAck {
                    seq: ack.seq,
                    result: ack.result,
                });
            }
            if let Some(page_count) = payload.page_count {
                entry.page_count = page_count;
            }
            if let Some(read_status) = payload.read_status {
                entry.read_status = read_status;
            }
            if let Some(missing) = payload.missing {
                entry.missing = missing;
            }
            // Assigned verbatim so a clean page clears a previous error.
            entry.error = payload.error;
            merge(&mut entry.chunks_done, payload.chunks_done);
            merge(&mut entry.chunks_total, payload.chunks_total);

            changes.push(ChangeEvent::NodeConfig { hostname, node_key });
        }

        IngestEvent::StickerFullConfig {
            hostname,
            node_key,
            payload,
        } => {
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);

            let node_key = node_key.to_lowercase();
            let entry = device
                .node_full_configs
                .entry(node_key.clone())
                .or_default();
            entry.config.extend(payload.config);
            merge(&mut entry.synced_at, payload.synced_at);
            merge(&mut entry.last_seq, payload.last_seq);
            if let Some(page_count) = payload.page_count {
                entry.page_count = page_count;
            }
            entry.error = payload.error;

            // `missing`/chunk counters are recomputed against the merged
            // snapshot, not taken from the firmware's per-publish values
            // (ws.md NodeFullConfigEntry).
            let missing_src = payload.missing.unwrap_or_else(|| entry.missing.clone());
            let still_missing: Vec<String> = missing_src
                .into_iter()
                .filter(|key| !entry.config.contains_key(key))
                .collect();
            entry.missing = still_missing;
            if let Some(chunks_total) = payload.chunks_total {
                entry.chunks_total = chunks_total;
            }
            entry.chunks_done = (entry.chunks_total - entry.missing.len() as i64).max(0);

            if let Some(read_status) = payload.read_status {
                entry.read_status = read_status;
            }
            // partial → complete promotion once nothing is missing.
            if entry.missing.is_empty() && entry.read_status == "partial" {
                entry.read_status = "complete".to_string();
            }

            changes.push(ChangeEvent::NodeConfig { hostname, node_key });
        }

        IngestEvent::StickerInfo {
            hostname,
            node_key,
            payload,
        } => {
            let node_key = node_key.to_lowercase();
            if payload.is_null() {
                // Empty retained payload = tombstone for a removed Node
                // (routing contract: empty arrives as Value::Null). No
                // liveness bump and no device creation — a replayed
                // tombstone proves nothing about the device being alive.
                if let Some(device) = state.devices.get_mut(&hostname.to_ascii_lowercase()) {
                    if device.node_infos.remove(&node_key).is_some() {
                        changes.push(ChangeEvent::NodeConfig { hostname, node_key });
                    }
                }
                return;
            }
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);
            // Replaced wholesale, raw payload verbatim (ws.md device-info).
            device.node_infos.insert(node_key.clone(), payload);
            changes.push(ChangeEvent::NodeConfig { hostname, node_key });
        }

        IngestEvent::StickerCommandResult {
            hostname,
            node_key,
            payload,
        } => {
            // Transient, v1 parity: command results are never cached — the
            // raw payload rides on the ChangeEvent for SSE only. No state
            // mutation, no liveness bump.
            changes.push(ChangeEvent::NodeCommandResult {
                hostname,
                node_key: node_key.to_lowercase(),
                payload,
            });
        }

        // History only (storage_intervals.md §6): the persistence sink stores the window, and the
        // live view is the `eye/sensors` snapshot's. No state mutation, no fragment.
        IngestEvent::EyeAggregated { .. } => {}

        IngestEvent::StickerHistory {
            hostname,
            node_key,
            payload,
        } => {
            // Transient: row persistence is the persistence sink's job;
            // the store only reports replay progress. No state mutation.
            changes.push(ChangeEvent::NodeHistoryProgress {
                hostname,
                node_key: node_key.to_lowercase(),
                frame_index: payload.frame_index.unwrap_or(0),
                frame_count: payload.frame_count.unwrap_or(0),
                points: payload.records.len(),
            });
        }

        // ---- WP-E: the v2 radio northbound family -----------------------
        //
        // One node per document (plan.md §9.4 bans the full-array shape), so unlike
        // `LorawanSensors` above there is no loop and no "nothing survived" case. `reading`
        // and `alarm` share one applier because they share one envelope: an alarm batch is a
        // `NodeReading` whose measurements happen to be events.
        IngestEvent::NodeReading {
            hostname,
            node_key,
            payload,
        }
        | IngestEvent::NodeAlarm {
            hostname,
            node_key,
            payload,
        } => {
            apply_node_reading(state, changes, hostname, node_key, *payload, now);
        }

        IngestEvent::NodeInventory {
            hostname,
            node_key,
            payload,
        } => {
            if payload.is_null() {
                // The empty retained payload `node-remove` publishes. No liveness bump and no
                // device creation — a replayed tombstone proves nothing about the Hub being
                // alive. Same rule as `StickerInfo` above.
                if let Some(device) = state.devices.get_mut(&hostname.to_ascii_lowercase()) {
                    let dropped = device.node_infos.remove(&node_key).is_some();
                    // The node itself goes too: the inventory document is what says this Hub
                    // still has it, and leaving the telemetry entry behind would keep a
                    // removed Node on every rendered list forever.
                    let removed = device.nodes.remove(&node_key).is_some();
                    // B4: the row has to go from every open list, and only
                    // `NodeRemoved` says so. This arm pushed `NodeConfig`
                    // alone, which repainted a config card for a Node that
                    // no longer existed and left its row on `/nodes` until
                    // somebody reloaded — a removal that looked like it had
                    // not happened.
                    if removed {
                        changes.push(ChangeEvent::NodeRemoved {
                            hostname: hostname.clone(),
                            node_key: node_key.clone(),
                        });
                    }
                    if dropped || removed {
                        changes.push(ChangeEvent::NodeConfig { hostname, node_key });
                    }
                }
                return;
            }
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);
            // The document is the one statement of the Node's radio path (GitLab #73) and of
            // its P2P identity (p2p_integration.md §2.3/§4.0). It also CREATES the row: a Node
            // `node-add` has registered has an inventory document and no telemetry, and
            // devices.node.md §5 wants it listed as "registered — waiting for join" before
            // its first frame — the same way the v1 `config/state` arm inserts a silent
            // `NodeState` for every Node it names.
            let inserted = !device.nodes.contains_key(&node_key);
            // A Node moved to the other carrier (node_registry.md §4) when its document names
            // a transport other than the last one did.
            let moved = device
                .node_infos
                .get(&node_key)
                .and_then(transport_from_info)
                .zip(transport_from_info(&payload))
                .is_some_and(|(was, now)| was != now);
            let node = device
                .nodes
                .entry(node_key.clone())
                .or_insert_with(|| NodeState::new(node_key.clone(), now));
            if moved {
                forget_carrier_session(node);
            }
            // Whether an earlier document of this Node reached this store. The first one after a
            // portal start is the retained replay on subscribe, which only restates the last join.
            let seen_before = device.node_infos.contains_key(&node_key);
            let joined_before = node.last_join_at;
            apply_node_info(node, &payload);
            // The Node joined (again) when `joined_at` (ChirpStack's `event/join`) moves forward
            // (node-firmware-v2.0.0.md §5), or appears on a Node a document already named: the
            // first join of a `registered` Node, and the first join after a carrier move
            // (`forget_carrier_session` cleared the old one). Not on an `Info` with seq 0:
            // v2.0.0 re-sends that every `interval_announce` hours without a join (#445). The
            // first document after a portal restart only replays the last join, which no
            // waiting send queued behind.
            let joined = match (joined_before, node.last_join_at) {
                (Some(before), Some(after)) => after > before,
                (None, Some(_)) => seen_before,
                _ => false,
            };
            device.node_infos.insert(node_key.clone(), payload);
            if inserted {
                changes.push(ChangeEvent::NodeAdded {
                    hostname: hostname.clone(),
                    node_key: node_key.clone(),
                });
            }
            if joined {
                changes.push(ChangeEvent::NodeJoined {
                    hostname: hostname.clone(),
                    node_key: node_key.to_lowercase(),
                    at: now,
                });
            }
            changes.push(ChangeEvent::NodeConfig { hostname, node_key });
        }

        // The Node's own reports (node_reported_state.md §4). Retained on the Hub and replayed
        // on every connect, so these arms are also what rebuilds the Node detail after a
        // restart of either side.
        IngestEvent::NodeDeviceInfo {
            hostname,
            node_key,
            payload,
        } => {
            let Some(doc) = payload else {
                // The removal tombstone. No liveness bump and no device creation — a replayed
                // tombstone proves nothing about the Hub being alive. Same rule as `info`.
                if let Some(device) = state.devices.get_mut(&hostname.to_ascii_lowercase()) {
                    if device.node_device_infos.remove(&node_key).is_some() {
                        changes.push(ChangeEvent::NodeConfig { hostname, node_key });
                    }
                }
                return;
            };
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);
            // Replaced wholesale: an `Info` is a complete statement, not a page.
            let doc = *doc;
            device.node_device_infos.insert(
                node_key.clone(),
                NodeDeviceInfoEntry {
                    received_at: doc.received_at,
                    info: serde_json::Value::Object(doc.device_info),
                },
            );
            changes.push(ChangeEvent::NodeConfig { hostname, node_key });
        }

        IngestEvent::NodeDeviceStatus {
            hostname,
            node_key,
            payload,
        } => {
            let Some(doc) = payload else {
                if let Some(device) = state.devices.get_mut(&hostname.to_ascii_lowercase()) {
                    if device.node_device_statuses.remove(&node_key).is_some() {
                        changes.push(ChangeEvent::NodeConfig { hostname, node_key });
                    }
                }
                return;
            };
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);
            device.node_device_statuses.insert(node_key.clone(), doc);
            changes.push(ChangeEvent::NodeConfig { hostname, node_key });
        }

        IngestEvent::NodeDeviceConfig {
            hostname,
            node_key,
            payload,
        } => {
            let Some(doc) = payload else {
                if let Some(device) = state.devices.get_mut(&hostname.to_ascii_lowercase()) {
                    if device.node_configs.remove(&node_key).is_some() {
                        changes.push(ChangeEvent::NodeConfig { hostname, node_key });
                    }
                }
                return;
            };
            let device = device_entry(&mut state.devices, &hostname);
            mark_seen(device, &hostname, now, changes);
            let doc = *doc;
            let entry = device.node_configs.entry(node_key.clone()).or_default();
            // Taken whole, not merged: the Hub's document is already the accumulated
            // configuration (node_reported_state.md §2.2) — merged over earlier dumps, or
            // replaced by a complete `get-config`. Merging again here would keep what the Hub
            // deliberately dropped, such as an alarm rule the device has cleared (bench,
            // 2026-09-23).
            entry.config = doc.config;
            if !doc.received_at.is_empty() {
                entry.synced_at = Some(doc.received_at);
            }
            entry.page_count = i64::from(doc.page_count);
            // Only a completed dump is ever published, so the read it reports is whole.
            entry.read_status = "complete".to_string();
            entry.missing.clear();
            entry.error = None;
            // The Hub carries the boot dump's slots forward in every document.
            entry.w1_slot_type = doc.w1_slot_type;
            changes.push(ChangeEvent::NodeConfig { hostname, node_key });
        }

        IngestEvent::NodeCommand {
            hostname,
            node_key,
            payload,
        } => {
            // Transient, like the v1 `StickerCommandResult`: no state, SSE and the pending
            // walk only. Named after the Hub the store already knows, so it correlates with
            // the walk that sent the command under the canonical spelling.
            let hostname = state
                .devices
                .get(&hostname.to_ascii_lowercase())
                .map(|d| d.hostname.clone())
                .unwrap_or(hostname);
            // An unsolicited Info with seq 0 is no longer read as a join: v2.0.0 re-sends it
            // every `interval_announce` hours without one (#445, node-firmware-v2.0.0.md §5).
            // The join is the `info` document's `joined_at` moving (the `NodeInfo` arm).
            let Some(result) = v2_command_result(&payload) else {
                return;
            };
            changes.push(ChangeEvent::NodeCommandResult {
                hostname,
                node_key: node_key.to_lowercase(),
                payload: result,
            });
        }

        IngestEvent::NodeHistory {
            hostname,
            node_key,
            payload,
        } => {
            // Transient, like the v1 `StickerHistory`: the persist sink stores and credits the
            // rows; the store only reports progress, under the Hub's canonical spelling so the
            // backfill table it nudges is the job's.
            let hostname = state
                .devices
                .get(&hostname.to_ascii_lowercase())
                .map(|d| d.hostname.clone())
                .unwrap_or(hostname);
            changes.push(ChangeEvent::NodeHistoryProgress {
                hostname,
                node_key: node_key.to_lowercase(),
                frame_index: payload.frame_index.unwrap_or(0),
                frame_count: payload.frame_count.unwrap_or(0),
                points: payload.records.len(),
            });
        }

        // The archive download is the persist sink's and the job service's;
        // the in-memory state has nothing to say about it.
        IngestEvent::BeaconHistory { .. } => {}

        // Decision #42: only the Server's own document names the cluster. A Client never
        // publishes one; a stray copy under another Hub's name is ignored rather than trusted.
        IngestEvent::EyeHomes { hostname, payload } => {
            if payload.server.trim().eq_ignore_ascii_case(&hostname) {
                state.beacon_cluster =
                    Some(crate::state::models::BeaconCluster::from_msg(&payload));
            }
        }

        IngestEvent::ConfigChallenge { .. }
        | IngestEvent::ConfigResponse { .. }
        | IngestEvent::PairResponse { .. }
        | IngestEvent::PairInvite { .. } => {
            // Relay-only events (M4-C): `mqtt::dispatch` routes these to
            // `CommandRelay`'s third sink, never here. No state mutation,
            // no `ChangeEvent`, and — unlike `Probe1m`/`DeviceError` above —
            // no liveness bump either, since these resolve in-flight
            // commands/pairing rather than reporting device telemetry.
        }

        IngestEvent::MqttConnection { connected, reason } => {
            state.mqtt.connected = connected;
            if connected {
                state.mqtt.last_connected_at = Some(now);
                state.mqtt.reconnect_attempts = 0;
                state.mqtt.last_disconnect_reason = None;
                state.mqtt.last_disconnected_at = None;
            } else {
                state.mqtt.last_disconnect_reason = reason;
                state.mqtt.reconnect_attempts += 1;
                // Only the *first* transition down stamps the clock: the
                // degraded rule asks how long we have been off, not how long
                // since the latest retry, and every retry that fails calls
                // through here again.
                state.mqtt.last_disconnected_at.get_or_insert(now);
            }
            changes.push(ChangeEvent::MqttConnection { connected });
        }

        // devices.hub.md §12 A2. Portal-wide, not per device: one broker carries one
        // cluster-wide report and the rows attribute themselves through their own
        // `hostname`. `received_at` is the store clock, per this module's convention.
        IngestEvent::RadioGateways { report } => {
            let next = report.map(|mut report| {
                for gateway in &mut report.gateways {
                    gateway.gateway_eui.make_ascii_lowercase();
                }
                RadioGatewaysState {
                    report,
                    received_at: now,
                }
            });
            // The arrival time is refreshed either way, the repaint only on new content:
            // the producer republishes every 60 s and the broker replays the retained copy
            // on every reconnect, and the report's own `timestamp` already differs whenever
            // anything really did.
            let changed = next.as_ref().map(|s| &s.report)
                != state.radio_gateways.as_ref().map(|s| &s.report);
            state.radio_gateways = next;
            if changed {
                changes.push(ChangeEvent::RadioGateways);
            }
        }
        IngestEvent::RadioDiagnostics {
            hostname,
            kind,
            doc,
        } => {
            // `device_entry`, not `agent_device_entry`: the document's hostname is the
            // producer's (`proximos-control`), which must not re-spell the Hub's display name.
            let device = device_entry(&mut state.devices, &hostname);
            let slot = match kind {
                super::events::RadioDiagKind::Northbridge => &mut device.northbridge_diag,
                super::events::RadioDiagKind::Lorawan => &mut device.lorawan_diag,
            };
            // The producer republishes every health tick with a fresh `timestamp`, and the
            // counters, uptime and stats move with every frame; repaint only when something
            // an operator acts on changed (state, versions, errors, units), or the card
            // would refetch every few seconds for nothing. A reload still shows the rest.
            let significant = |v: &serde_json::Value| {
                let mut v = v.clone();
                if let Some(o) = v.as_object_mut() {
                    for k in ["timestamp", "last_seen", "uptime_s", "counters"] {
                        o.remove(k);
                    }
                    if let Some(c) = o.get_mut("concentratord").and_then(|c| c.as_object_mut()) {
                        c.remove("last_stats");
                        c.remove("stats");
                    }
                }
                v
            };
            let changed =
                slot.as_ref().map(|(v, _)| significant(v)) != doc.as_ref().map(significant);
            *slot = doc.map(|v| (v, now));
            if changed {
                changes.push(ChangeEvent::Radio {
                    hostname: device.hostname.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::events::NodeReadingMsg;
    use super::*;
    use fiber_protocol::payloads::alarm_event::AlarmEventMsg;
    use fiber_protocol::payloads::config_state::{
        ConfigStateMsg, EyeConfig, EyeTagConfigEntry, LorawanSensorConfigEntry, SensorConfigEntry,
    };
    use fiber_protocol::payloads::eye::{EyeSensorsMsg, EyeTag};
    use fiber_protocol::payloads::lorawan::{
        LorawanEvent, LorawanGatewayRssi, LorawanSensor, LorawanSensorsMsg, NodeConfigMsg,
        NodeFullConfigMsg, NodeHistoryMsg, NodeHistoryRecord,
    };
    use fiber_protocol::payloads::sensors_aggregated::SensorsAggregatedMsg;
    use fiber_protocol::payloads::standby::StandbyMsg;
    use serde_json::json;
    use std::collections::HashMap;

    fn drain(rx: &mut broadcast::Receiver<ChangeEvent>) -> Vec<ChangeEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn eight_lines_pre_created() {
        let store = StateStore::new();
        store.apply(IngestEvent::Status {
            hostname: "h1".into(),
            status: DeviceStatus::Online,
        });
        let device = store.snapshot_device("h1").unwrap();
        assert_eq!(device.sensors.len(), 8);
        for (i, s) in device.sensors.iter().enumerate() {
            assert_eq!(s.line, i as u8);
        }
    }

    /// One physical Hub, two producers spelling its hostname differently — an older v1 agent's
    /// uppercased `FIBER-…` and control-radio's lowercase `fiber-…` — must land on ONE device,
    /// and the stored `hostname` is the lowercase spelling (decision #21), not the one first
    /// seen.
    #[test]
    fn hub_hostname_is_case_insensitive_and_stored_lowercase() {
        let store = StateStore::new();
        // v1 sensor telemetry arrives first, uppercased on the wire.
        store.apply(IngestEvent::Status {
            hostname: "FIBER-4E970F4A".into(),
            status: DeviceStatus::Online,
        });
        // control-radio's traffic for the same Hub arrives under the lowercase hostname.
        store.apply(IngestEvent::Status {
            hostname: "fiber-4e970f4a".into(),
            status: DeviceStatus::Online,
        });

        assert_eq!(
            store.inner.read().devices.len(),
            1,
            "the two spellings fold to one Hub, not two"
        );
        let by_upper = store
            .snapshot_device("FIBER-4E970F4A")
            .expect("reachable by the uppercase spelling");
        let by_lower = store
            .snapshot_device("fiber-4e970f4a")
            .expect("reachable by the lowercase spelling");
        assert_eq!(
            by_upper.hostname, "fiber-4e970f4a",
            "decision #21: one lowercase spelling, even when the uppercase one came first"
        );
        assert_eq!(by_lower.hostname, "fiber-4e970f4a");
    }

    #[test]
    fn status_transition_dedup() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        store.apply(IngestEvent::Status {
            hostname: "h1".into(),
            status: DeviceStatus::Online,
        });
        let changes = drain(&mut rx);
        assert_eq!(
            changes,
            vec![ChangeEvent::DeviceStatus {
                hostname: "h1".into(),
                online: true
            }]
        );

        // Same status again: no transition, no event.
        store.apply(IngestEvent::Status {
            hostname: "h1".into(),
            status: DeviceStatus::Online,
        });
        assert!(drain(&mut rx).is_empty());

        // Offline: a transition, one event.
        store.apply(IngestEvent::Status {
            hostname: "h1".into(),
            status: DeviceStatus::Offline,
        });
        assert_eq!(
            drain(&mut rx),
            vec![ChangeEvent::DeviceStatus {
                hostname: "h1".into(),
                online: false
            }]
        );
    }

    #[test]
    fn standby_never_touches_online_or_last_seen() {
        let store = StateStore::new();
        // Device has never been seen online.
        store.apply(IngestEvent::Standby {
            hostname: "h1".into(),
            payload: StandbyMsg {
                timestamp: None,
                standby: true,
                reason: Some("operator power_off".into()),
                requested_by: Some("dr.jane".into()),
                entered_at: Some("2026-08-05T12:00:00Z".into()),
                vin_mv: Some(0),
            },
        });
        let device = store.snapshot_device("h1").unwrap();
        assert!(!device.online);
        assert!(device.last_seen.is_none());
        assert!(device.first_seen.is_none());
        assert!(device.standby.active);
        assert_eq!(device.standby.requested_by.as_deref(), Some("dr.jane"));

        // Even once online, a later standby message must not bump last_seen.
        store.apply(IngestEvent::Status {
            hostname: "h1".into(),
            status: DeviceStatus::Online,
        });
        let seen_at = store.snapshot_device("h1").unwrap().last_seen;
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.apply(IngestEvent::Standby {
            hostname: "h1".into(),
            payload: StandbyMsg {
                timestamp: None,
                standby: false,
                reason: None,
                requested_by: None,
                entered_at: None,
                vin_mv: None,
            },
        });
        let device = store.snapshot_device("h1").unwrap();
        assert!(device.online);
        assert_eq!(device.last_seen, seen_at);
    }

    #[test]
    fn system_info_fans_out_to_four_events() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(fiber_protocol::payloads::system_info::SystemInfoMsg::default()),
        });

        let changes = drain(&mut rx);
        assert_eq!(
            changes,
            vec![
                ChangeEvent::DeviceStatus {
                    hostname: "h1".into(),
                    online: true
                },
                ChangeEvent::System {
                    hostname: "h1".into()
                },
                ChangeEvent::Power {
                    hostname: "h1".into()
                },
                ChangeEvent::Network {
                    hostname: "h1".into()
                },
                ChangeEvent::Storage {
                    hostname: "h1".into()
                },
            ]
        );
    }

    /// devices.hub.md §12 A: the `lorawan` block lands lowercased and repaints once, not on
    /// every `system/info` interval.
    #[test]
    fn system_info_lorawan_lands_once_and_repeats_are_silent() {
        use fiber_protocol::payloads::system_info::{LorawanGatewayInfo, SystemInfoMsg};

        let store = StateStore::new();
        let mut rx = store.subscribe();

        let payload = || SystemInfoMsg {
            lorawan: Some(LorawanGatewayInfo {
                mode: None,
                gateway_present: Some(true),
                concentratord_running: Some(true),
                chirpstack_running: Some(false),
                sensor_count: Some(3),
                gateway_eui: Some("70B3D54900AA0001".into()),
                cluster: Some(serde_json::json!({ "role": "server" })),
                join_eui: Some("58760700000000AB".into()),
            }),
            ..SystemInfoMsg::default()
        };

        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(payload()),
        });

        let lorawan = store
            .snapshot_device("h1")
            .unwrap()
            .lorawan
            .expect("stored");
        assert_eq!(lorawan.gateway_eui.as_deref(), Some("70b3d54900aa0001"));
        // #86: the registration dialog's prefill, lowercased like every EUI.
        assert_eq!(lorawan.join_eui.as_deref(), Some("58760700000000ab"));
        assert_eq!(lorawan.chirpstack_running, Some(false));
        assert_eq!(lorawan.sensor_count, Some(3));
        assert_eq!(
            drain(&mut rx)
                .iter()
                .filter(|c| **c
                    == ChangeEvent::Radio {
                        hostname: "h1".into()
                    })
                .count(),
            1,
            "one repaint for the first block"
        );

        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(payload()),
        });
        assert!(
            !drain(&mut rx).contains(&ChangeEvent::Radio {
                hostname: "h1".into()
            }),
            "an unchanged block repaints nothing"
        );
    }

    /// GitLab #58. The portal shows the Hub's setup state and keeps no copy of it: the value
    /// arrives in `system/info` and is stored as reported, including the difference between
    /// "not set up" and "has not said" — and including the block disappearing again, which is
    /// what a factory reset or a firmware downgrade looks like from here.
    #[test]
    fn the_setup_block_mirrors_the_last_system_info_and_absence_is_neither_a_false_nor_sticky() {
        use fiber_protocol::payloads::system_info::{SetupInfo, SystemInfoMsg};

        let store = StateStore::new();

        // A Hub that predates the key, or has not published since boot.
        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(SystemInfoMsg::default()),
        });
        let system = store.snapshot_device("h1").unwrap().system;
        assert_eq!(
            system.setup_provisioned, None,
            "absent must not collapse into Some(false) — the page would tell an operator a \
             configured Hub is out of the box"
        );
        assert_eq!(system.setup_role, None);

        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(SystemInfoMsg {
                setup: Some(SetupInfo {
                    provisioned: Some(true),
                    role: Some("client".into()),
                }),
                ..SystemInfoMsg::default()
            }),
        });
        let system = store.snapshot_device("h1").unwrap().system;
        assert_eq!(system.setup_provisioned, Some(true));
        assert_eq!(system.setup_role.as_deref(), Some("client"));

        // And the Hub stops saying it — downgraded agent, or a factory reset that emptied the
        // config key. The last report is the whole truth, so the stale claim goes with it.
        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(SystemInfoMsg::default()),
        });
        let system = store.snapshot_device("h1").unwrap().system;
        assert_eq!(
            system.setup_provisioned, None,
            "a Hub that has stopped reporting setup is not still provisioned"
        );
        assert_eq!(system.setup_role, None);
    }

    /// devices.hub.md §3.1 (amendment 2026-09-28): `hardware` is stored as the last
    /// `system/info` carried it, and a report without it takes it away again.
    #[test]
    fn the_hardware_block_follows_the_last_system_info() {
        use fiber_protocol::payloads::system_info::{Hardware, SystemInfoMsg};

        let store = StateStore::new();
        let hardware = Hardware {
            model: Some("Raspberry Pi Compute Module 4".into()),
            revision: Some("1.1".into()),
            serial: Some("100000004e970f4a".into()),
        };
        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(SystemInfoMsg {
                hardware: Some(hardware.clone()),
                ..SystemInfoMsg::default()
            }),
        });
        assert_eq!(
            store.snapshot_device("h1").unwrap().system.hardware,
            Some(hardware)
        );

        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(SystemInfoMsg::default()),
        });
        assert_eq!(store.snapshot_device("h1").unwrap().system.hardware, None);
    }

    /// southbridge_update.md §3.7: `southbridge` is stored as the last `system/info` carried
    /// it, the update's state included, and a report without it takes it away again.
    #[test]
    fn the_southbridge_block_follows_the_last_system_info() {
        use fiber_protocol::payloads::system_info::{
            Southbridge, SouthbridgeUpdate, SystemInfoMsg,
        };

        let store = StateStore::new();
        let sb = |state: &str| Southbridge {
            version: Some("south-zephyr 1.0.0 g11111111".into()),
            bundled: Some("south-zephyr 1.1.0 g73f39ed5".into()),
            update: Some(SouthbridgeUpdate {
                state: Some(state.into()),
                attempts: Some(0),
                ..SouthbridgeUpdate::default()
            }),
        };
        for state in ["running", "idle"] {
            store.apply(IngestEvent::SystemInfo {
                hostname: "h1".into(),
                payload: Box::new(SystemInfoMsg {
                    southbridge: Some(sb(state)),
                    ..SystemInfoMsg::default()
                }),
            });
            assert_eq!(
                store.snapshot_device("h1").unwrap().system.southbridge,
                Some(sb(state))
            );
        }

        store.apply(IngestEvent::SystemInfo {
            hostname: "h1".into(),
            payload: Box::new(SystemInfoMsg::default()),
        });
        assert_eq!(
            store.snapshot_device("h1").unwrap().system.southbridge,
            None
        );
    }

    /// devices.hub.md §12 A2: the cluster-wide report is portal-wide, deduplicated, and the
    /// empty retained payload clears it.
    #[test]
    fn radio_gateways_report_lands_deduplicates_and_clears() {
        use crate::state::radio::{ApiStatus, GatewayStatus, GatewaysReport};

        let store = StateStore::new();
        let mut rx = store.subscribe();

        let report = || GatewaysReport {
            timestamp: "2026-09-15T10:00:00Z".into(),
            api_status: ApiStatus::Ok,
            gateways: vec![GatewayStatus {
                gateway_eui: "70B3D54900AA0001".into(),
                name: "Local".into(),
                hostname: Some("h1".into()),
                ip: None,
                status: "online".into(),
                last_seen: Some("2026-09-15T09:59:41Z".into()),
                rssi: Some(-67),
                snr: Some(9.5),
                radio: "lorawan".into(),
            }],
        };

        store.apply(IngestEvent::RadioGateways {
            report: Some(report()),
        });

        let stored = store.snapshot_radio_gateways().expect("stored");
        assert_eq!(stored.report.gateways[0].gateway_eui, "70b3d54900aa0001");
        assert!(stored.received_at > 0, "stamped from the store clock");
        assert_eq!(drain(&mut rx), vec![ChangeEvent::RadioGateways]);

        // A retained replay on reconnect carries the same document.
        store.apply(IngestEvent::RadioGateways {
            report: Some(report()),
        });
        assert!(
            drain(&mut rx).is_empty(),
            "an identical report repaints nothing"
        );

        store.apply(IngestEvent::RadioGateways { report: None });
        assert!(
            store.snapshot_radio_gateways().is_none(),
            "the empty retained payload clears the report"
        );
        assert_eq!(drain(&mut rx), vec![ChangeEvent::RadioGateways]);
    }

    #[test]
    fn config_state_propagates_sensor_name_and_location() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        let mut msg = ConfigStateMsg::default();
        msg.sensors.push(SensorConfigEntry {
            line: 3,
            name: Some("Freezer probe".into()),
            location: Some("Shelf A".into()),
            enabled: None,
            has_override: None,
            thresholds: None,
        });

        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: msg,
        });

        let device = store.snapshot_device("h1").unwrap();
        assert_eq!(device.sensors[3].name.as_deref(), Some("Freezer probe"));
        assert_eq!(device.sensors[3].location.as_deref(), Some("Shelf A"));

        let changes = drain(&mut rx);
        assert!(changes.contains(&ChangeEvent::SensorLine {
            hostname: "h1".into(),
            line: 3
        }));
        assert!(changes.contains(&ChangeEvent::Config {
            hostname: "h1".into()
        }));
    }

    #[test]
    fn config_state_maps_and_clears_thresholds() {
        use fiber_protocol::payloads::config_state::SensorThresholds as WireThresholds;

        let store = StateStore::new();

        let mut msg = ConfigStateMsg::default();
        msg.sensors.push(SensorConfigEntry {
            line: 2,
            name: None,
            location: None,
            enabled: None,
            has_override: None,
            thresholds: Some(WireThresholds {
                critical_low: Some(-20.0),
                critical_high: Some(10.0),
                alarm_low: Some(-15.0),
                alarm_high: Some(8.0),
                hysteresis: None,
                enabled: None,
                warning_low: Some(-10.0),
                warning_high: Some(5.0),
            }),
        });
        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: msg,
        });

        let device = store.snapshot_device("h1").unwrap();
        let thresholds = device.sensors[2]
            .thresholds
            .as_ref()
            .expect("thresholds set");
        assert_eq!(thresholds.critical_low, Some(-20.0));
        assert_eq!(thresholds.warning_low, Some(-10.0));
        assert_eq!(thresholds.warning_high, Some(5.0));
        assert_eq!(thresholds.critical_high, Some(10.0));
        assert_eq!(thresholds.alarm_low, Some(-15.0));
        assert_eq!(thresholds.alarm_high, Some(8.0));

        // A later config/state without a thresholds block for this line
        // clears it back to None — authoritative-snapshot semantics, not a
        // keep-old-on-null merge.
        let mut msg2 = ConfigStateMsg::default();
        msg2.sensors.push(SensorConfigEntry {
            line: 2,
            name: None,
            location: None,
            enabled: None,
            has_override: None,
            thresholds: None,
        });
        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: msg2,
        });

        let device = store.snapshot_device("h1").unwrap();
        assert!(device.sensors[2].thresholds.is_none());
    }

    fn line_msg(end: i64, avg: f64, state: &str, since: Option<&str>) -> SensorsAggregatedMsg {
        SensorsAggregatedMsg {
            timestamp: None,
            period_start_ts: Some(end - 2),
            period_end_ts: Some(end),
            duration_sec: Some(2),
            sensors: vec![
                fiber_protocol::payloads::sensors_aggregated::AggregatedSensor {
                    line: 0,
                    name: None,
                    location: None,
                    sample_count: 1,
                    disconnected_count: 0,
                    temperature: Some(
                        fiber_protocol::payloads::sensors_aggregated::TemperatureStats {
                            min_celsius: Some(avg),
                            max_celsius: Some(avg),
                            avg_celsius: Some(avg),
                        },
                    ),
                    alarm_counts: None,
                    dominant_alarm_state: state.into(),
                    alarm_triggered_at: since.map(Into::into),
                },
            ],
        }
    }

    fn line_nudges(changes: &[ChangeEvent]) -> usize {
        changes
            .iter()
            .filter(|c| matches!(c, ChangeEvent::SensorLine { .. }))
            .count()
    }

    #[test]
    fn a_window_fills_the_probe_name_and_location_without_config_state() {
        let store = StateStore::new();
        let mut m = line_msg(100, 4.0, "NORMAL", None);
        m.sensors[0].name = Some("Probe 1".into());
        m.sensors[0].location = Some("Zone A".into());
        store.apply(IngestEvent::SensorsAggregated {
            hostname: "h1".into(),
            payload: m,
        });
        let s = &store.snapshot_device("h1").unwrap().sensors[0];
        assert_eq!(s.location.as_deref(), Some("Zone A"));
        assert_eq!(s.name.as_deref(), Some("Probe 1"));
        // A window without them leaves what is known.
        store.apply(IngestEvent::SensorsAggregated {
            hostname: "h1".into(),
            payload: line_msg(102, 4.0, "NORMAL", None),
        });
        let s = &store.snapshot_device("h1").unwrap().sensors[0];
        assert_eq!(s.location.as_deref(), Some("Zone A"));
    }

    #[test]
    fn a_live_sample_nudges_only_when_the_row_changes() {
        let store = StateStore::new();
        let mut rx = store.subscribe();
        let live = |m| IngestEvent::SensorsLive {
            hostname: "h1".into(),
            payload: m,
        };

        store.apply(live(line_msg(100, 4.21, "NORMAL", None)));
        assert_eq!(line_nudges(&drain(&mut rx)), 1, "first sight nudges");

        // Same tenth of a degree, same state: nothing on screen moves.
        store.apply(live(line_msg(102, 4.24, "NORMAL", None)));
        assert_eq!(line_nudges(&drain(&mut rx)), 0);
        // The value still lands in the state the next render reads.
        let s = &store.snapshot_device("h1").unwrap().sensors[0];
        assert_eq!(s.temperature_avg_celsius, Some(4.24));

        // A state change goes at once.
        store.apply(live(line_msg(104, 9.0, "WARNING", None)));
        assert_eq!(line_nudges(&drain(&mut rx)), 1);

        // A storage window always nudges.
        store.apply(IngestEvent::SensorsAggregated {
            hostname: "h1".into(),
            payload: line_msg(106, 9.0, "WARNING", None),
        });
        assert_eq!(line_nudges(&drain(&mut rx)), 1);
    }

    #[test]
    fn a_value_only_change_waits_for_the_nudge_interval() {
        let at = |at, t| LiveNudge {
            at,
            temperature_dc: Some(t),
            state: "NORMAL".into(),
            connected: true,
        };
        assert!(live_nudge_due(None, &at(0, 42)));
        assert!(!live_nudge_due(Some(&at(0, 42)), &at(4, 43)));
        assert!(live_nudge_due(Some(&at(0, 42)), &at(LIVE_NUDGE_MIN_S, 43)));
        assert!(
            !live_nudge_due(Some(&at(0, 42)), &at(600, 42)),
            "no change, no nudge however long"
        );
        let mut gone = at(1, 42);
        gone.connected = false;
        assert!(live_nudge_due(Some(&at(0, 42)), &gone));
    }

    #[test]
    fn an_older_window_does_not_overwrite_the_live_line() {
        let store = StateStore::new();
        store.apply(IngestEvent::SensorsLive {
            hostname: "h1".into(),
            payload: line_msg(1_000, 5.0, "WARNING", Some("2026-10-12T10:00:00Z")),
        });
        // A live sample carries no onset; the line keeps the one it had.
        store.apply(IngestEvent::SensorsLive {
            hostname: "h1".into(),
            payload: line_msg(1_002, 5.1, "WARNING", None),
        });
        // A window replayed after a reconnect ends before the live sample.
        store.apply(IngestEvent::SensorsAggregated {
            hostname: "h1".into(),
            payload: line_msg(400, -2.0, "NORMAL", None),
        });
        let s = &store.snapshot_device("h1").unwrap().sensors[0];
        assert_eq!(s.temperature_avg_celsius, Some(5.1));
        assert_eq!(s.dominant_alarm_state, "WARNING");
        assert_eq!(
            s.alarm_triggered_at.as_deref(),
            Some("2026-10-12T10:00:00Z")
        );
        assert_eq!(s.period_end_ts, Some(1_002));
    }

    #[test]
    fn out_of_range_sensor_line_is_dropped() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        let mut msg = SensorsAggregatedMsg {
            timestamp: None,
            period_start_ts: None,
            period_end_ts: None,
            duration_sec: None,
            sensors: Vec::new(),
        };
        msg.sensors.push(
            fiber_protocol::payloads::sensors_aggregated::AggregatedSensor {
                line: 9,
                name: None,
                location: None,
                sample_count: 1,
                disconnected_count: 0,
                temperature: None,
                alarm_counts: None,
                dominant_alarm_state: "NORMAL".into(),
                alarm_triggered_at: None,
            },
        );

        store.apply(IngestEvent::SensorsAggregated {
            hostname: "h1".into(),
            payload: msg,
        });

        // Device still gets created (liveness bump) but no SensorLine event
        // for the invalid line.
        let changes = drain(&mut rx);
        assert!(!changes
            .iter()
            .any(|c| matches!(c, ChangeEvent::SensorLine { .. })));
        assert!(changes.contains(&ChangeEvent::DeviceStatus {
            hostname: "h1".into(),
            online: true
        }));
    }

    #[test]
    fn alarm_event_out_of_range_line_drops_without_creating_transition() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        store.apply(IngestEvent::AlarmEvent {
            hostname: "h1".into(),
            payload: AlarmEventMsg {
                timestamp: None,
                line: -1,
                name: None,
                from_state: "NORMAL".into(),
                to_state: "CRITICAL".into(),
                temperature_celsius: None,
                event_type: None,
                message: None,
                alarm_type: None,
            },
        });

        assert!(drain(&mut rx).is_empty());
        assert!(store.snapshot_device("h1").is_none());
    }

    #[test]
    fn relay_only_events_are_ignored_by_the_store() {
        use fiber_protocol::payloads::config_challenge::ConfigChallengeMsg;
        use fiber_protocol::payloads::config_response::ConfigResponseMsg;
        use fiber_protocol::payloads::pair_response::PairResponseMsg;

        let store = StateStore::new();
        let mut rx = store.subscribe();

        store.apply(IngestEvent::ConfigChallenge {
            hostname: "h1".into(),
            payload: ConfigChallengeMsg {
                challenge_id: "chal-1".into(),
                request_id: Some("req-1".into()),
                signer_id: None,
                expires_at: None,
                preview: None,
            },
        });
        store.apply(IngestEvent::ConfigResponse {
            hostname: "h1".into(),
            payload: ConfigResponseMsg {
                challenge_id: "chal-1".into(),
                request_id: None,
                status: "SUCCESS".into(),
                applied_at: None,
                effective_at: None,
                message: None,
                result: None,
            },
        });
        store.apply(IngestEvent::PairResponse {
            hostname: "h1".into(),
            payload: PairResponseMsg {
                request_id: "req-2".into(),
                success: true,
                encrypted_private_key: None,
                ca_public_key: None,
                ca_id: None,
                admin_certificate: None,
                error: None,
            },
        });

        assert!(
            drain(&mut rx).is_empty(),
            "no ChangeEvent for relay-only events"
        );
        assert!(
            store.snapshot_device("h1").is_none(),
            "no device created, unlike Probe1m/DeviceError's liveness bump"
        );
    }

    #[test]
    fn mqtt_connection_tracks_reconnect_attempts() {
        let store = StateStore::new();
        store.apply(IngestEvent::MqttConnection {
            connected: false,
            reason: Some("broker unreachable".into()),
        });
        store.apply(IngestEvent::MqttConnection {
            connected: false,
            reason: Some("broker unreachable".into()),
        });
        let mqtt = store.snapshot_mqtt();
        assert!(!mqtt.connected);
        assert_eq!(mqtt.reconnect_attempts, 2);
        assert_eq!(
            mqtt.last_disconnect_reason.as_deref(),
            Some("broker unreachable")
        );

        store.apply(IngestEvent::MqttConnection {
            connected: true,
            reason: None,
        });
        let mqtt = store.snapshot_mqtt();
        assert!(mqtt.connected);
        assert_eq!(mqtt.reconnect_attempts, 0);
        assert!(mqtt.last_disconnect_reason.is_none());
    }

    // ---- M5-C: Node (STICKER) + Beacon (EYE) ----

    /// Minimal Node payload entry — one field so it survives the
    /// "no fields ⇒ dropped" gate.
    fn node_sensor(dev_eui: &str) -> LorawanSensor {
        LorawanSensor {
            dev_eui: dev_eui.into(),
            name: None,
            serial_number: None,
            location: None,
            fields: HashMap::from([("temperature".to_string(), 21.5)]),
            field_alarm_states: HashMap::new(),
            field_thresholds: None,
            counters: HashMap::new(),
            events: Vec::new(),
            rssi: None,
            snr: None,
            gateways: Vec::new(),
            fcnt: None,
            dr: None,
            downlink_gateway_id: None,
            last_seen: None,
            alarm_state: None,
        }
    }

    fn lorawan_msg(sensors: Vec<LorawanSensor>) -> IngestEvent {
        IngestEvent::LorawanSensors {
            hostname: "h1".into(),
            payload: LorawanSensorsMsg {
                timestamp: None,
                sensors,
            },
        }
    }

    fn eye_tag(mac: &str) -> EyeTag {
        EyeTag {
            gateway: None,
            mac: mac.into(),
            name: None,
            temperature_c: None,
            humidity_pct: None,
            battery_mv: None,
            low_battery: None,
            magnet_present: None,
            magnet_detected: None,
            moving: None,
            movement_count: None,
            pitch_deg: None,
            roll_deg: None,
            rssi: None,
            last_seen_ts: None,
            stale: None,
            provisioning: None,
            discovered: None,
            field_alarm_states: HashMap::new(),
            alarm_state: None,
            ble_config: None,
        }
    }

    fn eye_msg(tags: Vec<EyeTag>) -> IngestEvent {
        IngestEvent::EyeSensors {
            hostname: "h1".into(),
            payload: EyeSensorsMsg {
                timestamp: None,
                discovered_capped: false,
                tags,
            },
        }
    }

    fn node_config_msg() -> NodeConfigMsg {
        NodeConfigMsg {
            dev_eui: None,
            config: serde_json::Map::new(),
            page_index: None,
            page_count: None,
            synced_at: None,
            last_ack: None,
            read_status: None,
            missing: None,
            error: None,
            chunks_done: None,
            chunks_total: None,
        }
    }

    fn node_full_config_msg() -> NodeFullConfigMsg {
        NodeFullConfigMsg {
            dev_eui: None,
            config: serde_json::Map::new(),
            page_index: None,
            page_count: None,
            synced_at: None,
            last_ack: None,
            read_status: None,
            missing: None,
            error: None,
            chunks_done: None,
            chunks_total: None,
            last_seq: None,
        }
    }

    fn node_cfg_entry(dev_eui: &str) -> LorawanSensorConfigEntry {
        LorawanSensorConfigEntry {
            dev_eui: dev_eui.into(),
            name: None,
            serial_number: None,
            location: None,
            enabled: None,
            temp_critical_low: None,
            temp_warning_low: None,
            temp_warning_high: None,
            temp_critical_high: None,
            humidity_critical_low: None,
            humidity_warning_low: None,
            humidity_warning_high: None,
            humidity_critical_high: None,
            field_thresholds: None,
        }
    }

    #[test]
    fn lorawan_sensors_merge_and_replace_semantics() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        let mut first = node_sensor("AA11"); // wire casing — store lowercases
        first.name = Some("Node 1".into());
        first.fields = HashMap::from([("temperature".into(), 21.5), ("humidity".into(), 40.0)]);
        first.field_alarm_states = HashMap::from([("temperature".into(), "NORMAL".into())]);
        first.counters = HashMap::from([("fCnt".into(), 1)]);
        first.last_seen = Some("2026-01-13T14:30:00Z".into());
        first.alarm_state = Some("NORMAL".into());
        store.apply(lorawan_msg(vec![first]));

        assert!(drain(&mut rx).contains(&ChangeEvent::NodeBatch {
            hostname: "h1".into()
        }));
        let added_at = store.snapshot_device("h1").unwrap().nodes["aa11"].added_at;
        assert!(added_at.is_some());

        let mut second = node_sensor("aa11");
        second.fields = HashMap::from([("temperature".into(), 22.0)]);
        second.field_alarm_states = HashMap::from([("humidity".into(), "WARNING".into())]);
        second.counters = HashMap::from([("fCnt".into(), 2), ("hall_left".into(), 1)]);
        second.last_seen = Some("2026-01-13T14:31:00Z".into());
        store.apply(lorawan_msg(vec![second]));

        let device = store.snapshot_device("h1").unwrap();
        let node = &device.nodes["aa11"];
        // fields merged, never pruned; counters merged.
        assert_eq!(node.fields.get("temperature"), Some(&22.0));
        assert_eq!(node.fields.get("humidity"), Some(&40.0));
        assert_eq!(node.counters.get("fCnt"), Some(&2));
        assert_eq!(node.counters.get("hall_left"), Some(&1));
        // field_alarm_states replaced wholesale (temperature gone).
        assert_eq!(node.field_alarm_states.len(), 1);
        assert_eq!(
            node.field_alarm_states.get("humidity").map(String::as_str),
            Some("WARNING")
        );
        // WARNING entry got a triggered_at stamp; nothing else has one.
        assert_eq!(node.field_alarm_triggered_at.len(), 1);
        assert!(node.field_alarm_triggered_at.contains_key("humidity"));
        // name kept (merge), last_seen is the payload string, added_at
        // write-once, alarm_state kept from the first publish.
        assert_eq!(node.name.as_deref(), Some("Node 1"));
        assert_eq!(node.last_seen.as_deref(), Some("2026-01-13T14:31:00Z"));
        assert_eq!(node.added_at, added_at);
        assert_eq!(node.alarm_state, "NORMAL");
    }

    #[test]
    fn lorawan_sensor_without_fields_is_dropped() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        let mut sensor = node_sensor("aa11");
        sensor.fields = HashMap::new(); // v1: no `fields` ⇒ dropped
        store.apply(lorawan_msg(vec![sensor]));

        let changes = drain(&mut rx);
        // Liveness bump still happens, but no batch and no Node state.
        assert!(changes.contains(&ChangeEvent::DeviceStatus {
            hostname: "h1".into(),
            online: true
        }));
        assert!(!changes
            .iter()
            .any(|c| matches!(c, ChangeEvent::NodeBatch { .. })));
        assert!(store.snapshot_device("h1").unwrap().nodes.is_empty());
    }

    #[test]
    fn restamp_field_alarms_stamps_carries_and_clears() {
        let warning = BTreeMap::from([("t".to_string(), "WARNING".to_string())]);
        let critical = BTreeMap::from([("t".to_string(), "CRITICAL".to_string())]);
        let normal = BTreeMap::from([("t".to_string(), "NORMAL".to_string())]);
        let empty_states = BTreeMap::new();
        let empty_stamps = BTreeMap::new();

        // Entering WARNING stamps `now`.
        let stamped = restamp_field_alarms(&empty_states, &empty_stamps, &warning, 100);
        assert_eq!(stamped.get("t"), Some(&100));
        // WARNING → CRITICAL stays in-alarm: original stamp carried.
        let carried = restamp_field_alarms(&warning, &stamped, &critical, 200);
        assert_eq!(carried.get("t"), Some(&100));
        // Recovery clears the entry.
        assert!(restamp_field_alarms(&critical, &carried, &normal, 300).is_empty());
        // Vanishing from the wholesale-replaced map also clears it.
        assert!(restamp_field_alarms(&critical, &carried, &empty_states, 300).is_empty());
    }

    #[test]
    fn lorawan_recent_events_tail_capped_at_32() {
        let store = StateStore::new();
        let mut sensor = node_sensor("aa11");
        sensor.events = (0..40)
            .map(|i| LorawanEvent {
                event_type: format!("e{i}"),
                ts: None,
                extra: serde_json::Map::new(),
            })
            .collect();
        store.apply(lorawan_msg(vec![sensor]));

        let device = store.snapshot_device("h1").unwrap();
        let events = &device.nodes["aa11"].recent_events;
        assert_eq!(events.len(), 32);
        // Tail kept: the first 8 were dropped.
        assert_eq!(events[0].get("type").and_then(|v| v.as_str()), Some("e8"));
        assert_eq!(events[31].get("type").and_then(|v| v.as_str()), Some("e39"));
    }

    #[test]
    fn lorawan_rssi_snr_prefer_best_gateway() {
        let store = StateStore::new();
        let mut sensor = node_sensor("aa11");
        sensor.rssi = Some(-100); // top-level: only a fallback
        sensor.snr = Some(1.0);
        sensor.gateways = vec![
            LorawanGatewayRssi {
                gateway_id: Some("gw1".into()),
                rssi: Some(-80),
                snr: Some(7.5),
            },
            LorawanGatewayRssi {
                gateway_id: Some("gw2".into()),
                rssi: Some(-70),
                snr: Some(5.0),
            },
        ];
        store.apply(lorawan_msg(vec![sensor]));
        let node = &store.snapshot_device("h1").unwrap().nodes["aa11"];
        assert_eq!(node.rssi, Some(-70));
        assert_eq!(node.snr, Some(5.0));
        assert_eq!(node.gateways.len(), 2);

        // No gateway RSSI ⇒ top-level merges in.
        let mut sensor = node_sensor("aa11");
        sensor.rssi = Some(-90);
        store.apply(lorawan_msg(vec![sensor]));
        let node = &store.snapshot_device("h1").unwrap().nodes["aa11"];
        assert_eq!(node.rssi, Some(-90));
    }

    #[test]
    fn config_state_is_authoritative_node_inventory() {
        let store = StateStore::new();
        store.apply(lorawan_msg(vec![node_sensor("aa11"), node_sensor("bb22")]));
        let mut rx = store.subscribe();

        let mut kept = node_cfg_entry("AA11"); // wire casing — lowercased
        kept.name = Some("Renamed".into());
        kept.serial_number = Some("SN9".into());
        kept.location = Some("Room X".into());
        kept.enabled = Some(true);
        let msg = ConfigStateMsg {
            lorawan_sensors: vec![kept, node_cfg_entry("cc33")],
            ..ConfigStateMsg::default()
        };
        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: msg,
        });

        let device = store.snapshot_device("h1").unwrap();
        // bb22 absent from the authoritative list ⇒ removed.
        assert!(!device.nodes.contains_key("bb22"));
        // aa11 upserted, cc33 created (write-once added_at set).
        let aa = &device.nodes["aa11"];
        assert_eq!(aa.name.as_deref(), Some("Renamed"));
        assert_eq!(aa.serial_number.as_deref(), Some("SN9"));
        assert_eq!(aa.location.as_deref(), Some("Room X"));
        assert_eq!(aa.enabled, Some(true));
        assert!(device.nodes["cc33"].added_at.is_some());

        let changes = drain(&mut rx);
        assert!(changes.contains(&ChangeEvent::NodeRemoved {
            hostname: "h1".into(),
            node_key: "bb22".into()
        }));
        assert!(changes.contains(&ChangeEvent::Node {
            hostname: "h1".into(),
            node_key: "aa11".into()
        }));
    }

    /// After a restart the retained `info` replays before any reading; its `last_seen` stands
    /// in for the lost in-memory one, and never overrides a reading's.
    #[test]
    fn the_inventory_restores_last_seen_after_a_restart() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: serde_json::json!({
                "dev_eui": "dd44", "transport": "lorawan", "pairing_status": "joined",
                "last_seen": "2026-09-24T16:34:07.1Z",
            }),
        });
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.last_seen.as_deref(), Some("2026-09-24T16:34:07.1Z"));
        assert_eq!(node.alarm_state, "NORMAL", "it has connected");

        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: Box::new(v2_reading("m-dd44", "2026-09-24T16:49:10Z", 28.1)),
        });
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: serde_json::json!({
                "dev_eui": "dd44", "transport": "lorawan", "last_seen": "2026-09-24T16:34:07.1Z",
            }),
        });
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(
            node.last_seen.as_deref(),
            Some("2026-09-24T16:49:10Z"),
            "a replayed info never rolls a reading back"
        );

        // Never heard from: no last_seen in the document, still NeverConnected.
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "ee55".into(),
            payload: serde_json::json!({"dev_eui": "ee55", "pairing_status": "never", "last_seen": null}),
        });
        let node = store.snapshot_device("h1").unwrap().nodes["ee55"].clone();
        assert!(node.last_seen.is_none());
        assert_eq!(node.alarm_state, "NeverConnected");
    }

    /// N3 (WP4 on 0413): a v2 reading carries no aggregate alarm state; it is the proof the
    /// Node connected, so the model default gives way to Normal, and only the default does.
    #[test]
    fn a_v2_reading_ends_never_connected() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: Box::new(v2_reading("m-dd44", "2026-09-24T13:23:28Z", 28.1)),
        });
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.alarm_state, "NORMAL");

        // A state something else set is left alone.
        let mut sensor = node_sensor("aa11");
        sensor.alarm_state = Some("WARNING".into());
        store.apply(lorawan_msg(vec![sensor]));
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-aa11", "2026-09-24T13:24:00Z", 21.0)),
        });
        let after = store.snapshot_device("h1").unwrap().nodes["aa11"]
            .alarm_state
            .clone();
        assert_eq!(after, "WARNING", "not overwritten");
    }

    /// STICKER 2.0.0 NA sentinel (HIL on 182): a field the next frame reports as null drops
    /// the held value, live as after a restart; a field merely absent keeps it.
    #[test]
    fn a_null_field_clears_the_held_value_and_an_absent_one_keeps_it() {
        let store = StateStore::new();
        let mut first = v2_reading("m-1", "2026-10-14T10:00:00Z", 21.5);
        first.fields.insert("pressure".into(), 98_100.0);
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: Box::new(first),
        });

        let mut second = v2_reading("m-2", "2026-10-14T10:15:00Z", 21.6);
        let mut extra = serde_json::Map::new();
        extra.insert("field".into(), serde_json::json!("pressure"));
        second
            .events
            .push(control_radio::decode::reading::NodeEvent::new(
                "no_data",
                "2026-10-14T10:15:00Z",
                extra,
            ));
        second.counters.clear();
        let mut rx = store.subscribe();
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: Box::new(second),
        });
        let changes = drain(&mut rx);
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert!(!node.fields.contains_key("pressure"), "{:?}", node.fields);
        assert_eq!(node.fields.get("temperature"), Some(&21.6));
        assert_eq!(
            node.counters.get("hall_left_count"),
            Some(&7),
            "absent keeps"
        );
        assert!(changes.contains(&ChangeEvent::NodeBatch {
            hostname: "h1".into()
        }));
        assert!(
            node.no_data_fields.contains("pressure"),
            "row stays, reads '–'"
        );
        assert!(!node.no_data_fields.contains("hall_left_count"));
    }

    fn no_data(at: &str, field: &str) -> control_radio::decode::reading::NodeEvent {
        let mut extra = serde_json::Map::new();
        extra.insert("field".into(), serde_json::json!(field));
        control_radio::decode::reading::NodeEvent::new("no_data", at, extra)
    }

    /// A real value after a `null` ends the `null` state; a frame that carries a value and
    /// a `null` for the same quantity keeps the value.
    #[test]
    fn a_value_after_null_restores_the_field() {
        let store = StateStore::new();
        let apply = |reading: NodeReadingMsg| {
            store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "dd44".into(),
                payload: Box::new(reading),
            });
        };
        let mut first = v2_reading("m-1", "2026-10-14T10:00:00Z", 21.5);
        first.fields.insert("input_a_voltage".into(), 28.0);
        apply(first);
        let mut second = v2_reading("m-2", "2026-10-14T10:15:00Z", 21.6);
        second
            .events
            .push(no_data("2026-10-14T10:15:00Z", "input_a_voltage"));
        apply(second);
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.fields.get("input_a_voltage"), None);
        assert!(node.no_data_fields.contains("input_a_voltage"));

        let mut third = v2_reading("m-3", "2026-10-14T10:30:00Z", 21.7);
        third.fields.insert("input_a_voltage".into(), 27.5);
        third
            .events
            .push(no_data("2026-10-14T10:30:00Z", "input_a_voltage"));
        apply(third);
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.fields.get("input_a_voltage"), Some(&27.5));
        assert!(node.no_data_fields.is_empty());
    }

    /// The restart refill does not bring back a value the Node later sent as `null`: a
    /// seeded `null` mark keeps the row at '–', and a live value wins over the mark.
    #[test]
    fn seeded_no_data_marks_and_never_overrides_a_live_value() {
        let store = StateStore::new();
        let mut live = v2_reading("m-1", "2026-10-14T10:00:00Z", 21.5);
        live.counters.clear();
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: Box::new(live),
        });
        assert!(store.seed_node_no_data(
            "h1",
            "dd44",
            &["temperature".to_string(), "humidity".to_string()]
        ));
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.fields.get("temperature"), Some(&21.5), "live wins");
        assert!(node.no_data_fields.contains("humidity"));
        assert!(!node.no_data_fields.contains("temperature"));
        // A Node already marked takes no stale values from the field seed.
        assert!(!store.seed_node_fields("h1", "dd44", &[("humidity".to_string(), 40.0)]));
    }

    /// #89: on a v2 Hub `config/state.lorawan_sensors` is `[]` (the legacy stack is gated
    /// off), and it must not take the v2 Nodes with it — their inventory is the retained
    /// `info` document, removed only by its own tombstone. A v1 Node absent from the list
    /// still goes, exactly as before.
    #[test]
    fn config_state_leaves_v2_inventory_nodes_alone() {
        let store = StateStore::new();
        store.apply(lorawan_msg(vec![node_sensor("aa11")]));
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: serde_json::json!({
                "dev_eui": "dd44",
                "transport": "lorawan",
                "pairing_status": "joined",
            }),
        });
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: Box::new(v2_reading("m-dd44", "2026-09-22T22:35:00Z", 21.0)),
        });
        assert!(store
            .snapshot_device("h1")
            .unwrap()
            .nodes
            .contains_key("dd44"));

        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: ConfigStateMsg::default(),
        });

        let device = store.snapshot_device("h1").unwrap();
        assert!(
            device.nodes.contains_key("dd44"),
            "a v2 Node survives v1 config/state"
        );
        assert!(
            !device.nodes.contains_key("aa11"),
            "a v1 Node absent from the list is still removed"
        );

        // Its own tombstone is what removes it.
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: serde_json::Value::Null,
        });
        assert!(!store
            .snapshot_device("h1")
            .unwrap()
            .nodes
            .contains_key("dd44"));
    }

    /// F22 (Nodes test 2026-09-23): the agent lists a v2 Node in `config/state` for its
    /// thresholds, with no name or serial. That entry must not clear the identity the v2
    /// `info` document gave the Node, while a v1-only Node keeps the authoritative clear.
    #[test]
    fn config_state_keeps_a_v2_nodes_identity() {
        let store = StateStore::new();
        store.apply(lorawan_msg(vec![node_sensor("aa11")]));
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: serde_json::json!({
                "dev_eui": "dd44",
                "transport": "lorawan",
                "pairing_status": "joined",
                "serial_number": "33333",
                "name": "33333",
            }),
        });
        let mut v1_named = node_cfg_entry("aa11");
        v1_named.name = Some("Old name".into());
        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: ConfigStateMsg {
                lorawan_sensors: vec![v1_named],
                ..ConfigStateMsg::default()
            },
        });

        // The agent's view: both Nodes, thresholds only.
        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: ConfigStateMsg {
                lorawan_sensors: vec![node_cfg_entry("aa11"), node_cfg_entry("dd44")],
                ..ConfigStateMsg::default()
            },
        });

        let device = store.snapshot_device("h1").unwrap();
        let dd = &device.nodes["dd44"];
        assert_eq!(dd.serial_number.as_deref(), Some("33333"), "v2 serial kept");
        assert_eq!(dd.name.as_deref(), Some("33333"), "v2 name kept");
        assert_eq!(
            device.nodes["aa11"].name, None,
            "a v1 Node's name still follows config/state"
        );
    }

    /// The name is B5's; B6 narrowed the rule it holds. `config/state` IS
    /// authoritative for the **enrolled** inventory now (a removed Beacon
    /// has to leave the list, devices.beacon.md §5) — what it must never
    /// remove is a **discovered** tag, which is absent from `eye.tags` by
    /// definition. See `an_enrolled_beacon_absent_from_config_state_is_removed`
    /// for the other half.
    #[test]
    fn config_state_eye_tags_never_remove_a_discovered_tag() {
        let store = StateStore::new();
        let mut discovered = eye_tag("11:22:33:44:55:66");
        discovered.discovered = Some(true);
        store.apply(eye_msg(vec![discovered]));

        let msg = ConfigStateMsg {
            eye_tags: Some(vec![EyeTagConfigEntry {
                mac: "aa:bb:cc:dd:ee:ff".into(), // wire casing — uppercased
                name: Some("Tag 1".into()),
                location: Some("Room B".into()),
            }]),
            eye: Some(EyeConfig {
                enabled: Some(true),
                auto_provision: Some(false),
                auto_discover: Some(true),
            }),
            ..ConfigStateMsg::default()
        };
        let mut rx = store.subscribe();
        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: msg,
        });

        let device = store.snapshot_device("h1").unwrap();
        // Config-listed tag upserted as registered (discovered = None).
        let tag = &device.ble_sensors["AA:BB:CC:DD:EE:FF"];
        assert_eq!(tag.name.as_deref(), Some("Tag 1"));
        assert_eq!(tag.location.as_deref(), Some("Room B"));
        assert!(tag.discovered.is_none());
        // The unlisted DISCOVERED beacon is untouched: it is absent from
        // `eye.tags` because it is a stranger, not because it was removed.
        assert!(device.ble_sensors.contains_key("11:22:33:44:55:66"));
        assert!(!drain(&mut rx)
            .iter()
            .any(|c| matches!(c, ChangeEvent::BeaconRemoved { .. })));
        // eye block landed on device config.
        assert_eq!(device.config.beacon_enabled, Some(true));
        assert_eq!(device.config.beacon_auto_provision, Some(false));
        assert_eq!(device.config.beacon_auto_discover, Some(true));
    }

    /// B6: `remove_eye_tag` succeeds on the Hub and the Beacon leaves
    /// `config/state.eye_tags` — at which point the portal's own row has to
    /// go too, or `/beacons` keeps showing a device nobody owns until the
    /// process restarts. This is the Node rule
    /// (`config_state_is_authoritative_node_inventory`) applied to the
    /// enrolled half of `ble_sensors`.
    #[test]
    fn an_enrolled_beacon_absent_from_config_state_is_removed() {
        let store = StateStore::new();
        let enrolled = |mac: &str| IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: ConfigStateMsg {
                eye_tags: Some(vec![EyeTagConfigEntry {
                    mac: mac.into(),
                    name: None,
                    location: None,
                }]),
                ..ConfigStateMsg::default()
            },
        };
        store.apply(enrolled("AA:BB:CC:DD:EE:01"));
        assert!(store
            .snapshot_device("h1")
            .unwrap()
            .ble_sensors
            .contains_key("AA:BB:CC:DD:EE:01"));

        // A second document that no longer lists it: removed from the fleet.
        let mut rx = store.subscribe();
        store.apply(enrolled("AA:BB:CC:DD:EE:02"));
        let device = store.snapshot_device("h1").unwrap();
        assert!(!device.ble_sensors.contains_key("AA:BB:CC:DD:EE:01"));
        assert!(device.ble_sensors.contains_key("AA:BB:CC:DD:EE:02"));
        assert!(drain(&mut rx).contains(&ChangeEvent::BeaconRemoved {
            hostname: "h1".into(),
            mac: "AA:BB:CC:DD:EE:01".into(),
            // Enrolled: this half of the fleet still sends `refresh`.
            discovered: false,
        }));

        // The newly listed one is announced as added; the one that stayed
        // listed is not.
        let mut rx = store.subscribe();
        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: ConfigStateMsg {
                eye_tags: Some(vec![
                    EyeTagConfigEntry {
                        mac: "AA:BB:CC:DD:EE:02".into(),
                        name: None,
                        location: None,
                    },
                    EyeTagConfigEntry {
                        mac: "AA:BB:CC:DD:EE:03".into(),
                        name: None,
                        location: None,
                    },
                ]),
                ..ConfigStateMsg::default()
            },
        });
        let added: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|c| match c {
                ChangeEvent::BeaconAdded { mac, .. } => Some(mac),
                _ => None,
            })
            .collect();
        assert_eq!(added, vec!["AA:BB:CC:DD:EE:03".to_string()]);

        // ...but a document that does not mention beacons at all is not a
        // statement about them. `eye_tags` is `Option`, unlike the Node
        // list's plain vector, and `None` must not empty the fleet.
        store.apply(IngestEvent::ConfigState {
            hostname: "h1".into(),
            payload: ConfigStateMsg::default(),
        });
        assert!(store
            .snapshot_device("h1")
            .unwrap()
            .ble_sensors
            .contains_key("AA:BB:CC:DD:EE:02"));
    }

    #[test]
    fn eye_sensors_mapping_rules() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        let mut tag = eye_tag("aa:bb:cc:dd:ee:ff"); // wire casing — uppercased
        tag.temperature_c = Some(21.5);
        tag.humidity_pct = Some(45);
        tag.stale = Some(false);
        tag.field_alarm_states = HashMap::from([("temperature".into(), "CRITICAL".into())]);
        tag.alarm_state = Some("CRITICAL".into());
        store.apply(eye_msg(vec![tag]));

        assert!(drain(&mut rx).contains(&ChangeEvent::BeaconBatch {
            hostname: "h1".into()
        }));
        let device = store.snapshot_device("h1").unwrap();
        let beacon = &device.ble_sensors["AA:BB:CC:DD:EE:FF"];
        assert_eq!(beacon.temperature, Some(21.5)); // temperature_c → temperature
        assert_eq!(beacon.humidity, Some(45));
        assert!(beacon.connected); // stale → connected = !stale
        assert!(beacon.field_alarm_triggered_at.contains_key("temperature"));
        assert_eq!(beacon.alarm_state, "CRITICAL");

        // Registered tag going stale: disconnected but never pruned.
        let mut tag = eye_tag("AA:BB:CC:DD:EE:FF");
        tag.stale = Some(true);
        store.apply(eye_msg(vec![tag]));
        let device = store.snapshot_device("h1").unwrap();
        let beacon = &device.ble_sensors["AA:BB:CC:DD:EE:FF"];
        assert!(!beacon.connected);
    }

    // ---- B6: reconcile by snapshot membership -----------------------
    //
    // `eye_stale_discovered_only_beacon_is_pruned` used to live here. It
    // pinned the rule these six replace: prune on the per-tag `stale` flag.
    // devices.backend.md §9 moves the decision to **membership** — a
    // stranger the Hub can still hear is in `tags`, one it cannot is gone —
    // and `stale` goes back to meaning only "advertising has stopped",
    // which is the `connected` flag. Deleted in the same commit as the
    // behaviour, as §16.4 said it would be.

    /// The rule itself, with the case the old flag-based one could not see:
    /// a stranger that simply stops being mentioned.
    #[test]
    fn a_discovered_beacon_absent_from_the_next_snapshot_is_pruned() {
        let store = StateStore::new();
        let mut stranger = eye_tag("AA:BB:CC:DD:EE:FF");
        stranger.discovered = Some(true);
        let mut other = eye_tag("AA:BB:CC:DD:EE:02");
        other.discovered = Some(true);
        store.apply(eye_msg(vec![stranger, other.clone()]));
        assert_eq!(store.snapshot_device("h1").unwrap().ble_sensors.len(), 2);

        let mut rx = store.subscribe();
        // The next snapshot mentions only the second one — no `stale` flag
        // anywhere, which is exactly what the Hub sends.
        store.apply(eye_msg(vec![other]));

        let device = store.snapshot_device("h1").unwrap();
        assert_eq!(
            device.ble_sensors.keys().collect::<Vec<_>>(),
            vec!["AA:BB:CC:DD:EE:02"]
        );
        let changes = drain(&mut rx);
        assert!(changes.contains(&ChangeEvent::BeaconRemoved {
            hostname: "h1".into(),
            mac: "AA:BB:CC:DD:EE:FF".into(),
            discovered: true,
        }));
    }

    /// `tags: []` is the empty snapshot, not a signal to ignore — and it
    /// upserts nothing, which is why the reconcile pass sits outside the
    /// `upserted` guard.
    #[test]
    fn an_empty_tags_array_prunes_every_discovered_beacon() {
        let store = StateStore::new();
        let mut stranger = eye_tag("AA:BB:CC:DD:EE:FF");
        stranger.discovered = Some(true);
        store.apply(eye_msg(vec![stranger]));

        let mut rx = store.subscribe();
        store.apply(eye_msg(vec![]));

        assert!(store.snapshot_device("h1").unwrap().ble_sensors.is_empty());
        let changes = drain(&mut rx);
        assert!(changes.contains(&ChangeEvent::BeaconRemoved {
            hostname: "h1".into(),
            mac: "AA:BB:CC:DD:EE:FF".into(),
            discovered: true,
        }));
        // A prune is a change, so the table repaints.
        assert!(changes
            .iter()
            .any(|c| matches!(c, ChangeEvent::BeaconBatch { .. })));
    }

    /// An enrolled beacon belongs to the Hub's `eye.tags`, not to what the
    /// radio happens to hear. A quiet one is an outage the offline detector
    /// reports — never a row that disappears.
    #[test]
    fn an_enrolled_beacon_survives_absence_and_staleness() {
        let store = StateStore::new();
        // `discovered: None` is the wire's "registered" (the stated trap),
        // and `Some(false)` is the explicit form. Both are enrolled.
        let mut null_discovered = eye_tag("AA:BB:CC:DD:EE:01");
        null_discovered.discovered = None;
        let mut explicit = eye_tag("AA:BB:CC:DD:EE:02");
        explicit.discovered = Some(false);
        explicit.stale = Some(true);
        store.apply(eye_msg(vec![null_discovered, explicit]));
        assert_eq!(store.snapshot_device("h1").unwrap().ble_sensors.len(), 2);

        store.apply(eye_msg(vec![]));
        let device = store.snapshot_device("h1").unwrap();
        assert_eq!(
            device.ble_sensors.len(),
            2,
            "an empty snapshot must not remove enrolled beacons"
        );
    }

    /// `stale` no longer prunes anything. A stranger the Hub still lists but
    /// has stopped hearing from stays on the scan table, disconnected — the
    /// operator can see it was there a moment ago.
    #[test]
    fn a_stale_discovered_beacon_that_is_still_present_is_kept() {
        let store = StateStore::new();
        let mut tag = eye_tag("AA:BB:CC:DD:EE:FF");
        tag.discovered = Some(true);
        store.apply(eye_msg(vec![tag]));

        let mut tag = eye_tag("AA:BB:CC:DD:EE:FF");
        tag.discovered = Some(true);
        tag.stale = Some(true);
        store.apply(eye_msg(vec![tag]));

        let device = store.snapshot_device("h1").unwrap();
        let beacon = &device.ble_sensors["AA:BB:CC:DD:EE:FF"];
        assert!(!beacon.connected, "stale sets connected, and only that");
        assert_eq!(beacon.discovered, Some(true));
    }

    /// The flag is per snapshot and rides onto `DeviceState`, and while it
    /// is set **nothing is pruned**: a capped snapshot dropped strangers to
    /// fit, so absence no longer means gone (devices.backend.md §22.1).
    #[test]
    fn discovered_capped_rides_the_snapshot_onto_device_state() {
        let store = StateStore::new();
        let mut stranger = eye_tag("AA:BB:CC:DD:EE:FF");
        stranger.discovered = Some(true);
        store.apply(eye_msg(vec![stranger]));
        assert!(!store.snapshot_device("h1").unwrap().discovered_capped);

        store.apply(IngestEvent::EyeSensors {
            hostname: "h1".into(),
            payload: EyeSensorsMsg {
                timestamp: None,
                discovered_capped: true,
                tags: vec![],
            },
        });
        let device = store.snapshot_device("h1").unwrap();
        assert!(device.discovered_capped);
        assert!(
            device.ble_sensors.contains_key("AA:BB:CC:DD:EE:FF"),
            "a capped snapshot is not evidence of absence"
        );

        // ...and the flag is not sticky: the next honest snapshot clears it
        // and prunes.
        store.apply(eye_msg(vec![]));
        let device = store.snapshot_device("h1").unwrap();
        assert!(!device.discovered_capped);
        assert!(device.ble_sensors.is_empty());
    }

    /// B6 review, item 11. The cap notice is drawn above the scan table, so
    /// a Hub coming back under its own limit changes the panel without
    /// changing a single row — and an `upserted`-only guard would leave the
    /// warning on screen until something else happened to move.
    #[test]
    fn a_cap_that_changes_repaints_the_panel_even_with_nothing_to_upsert() {
        let store = StateStore::new();
        let capped = |capped: bool| IngestEvent::EyeSensors {
            hostname: "h1".into(),
            payload: EyeSensorsMsg {
                timestamp: None,
                discovered_capped: capped,
                tags: vec![],
            },
        };
        store.apply(capped(true));

        // Nothing to upsert, nothing to prune — only the flag goes back down.
        let mut rx = store.subscribe();
        store.apply(capped(false));
        let changes = drain(&mut rx);
        assert!(
            changes.iter().any(|c| matches!(
                c,
                ChangeEvent::BeaconBatch { hostname } if hostname == "h1"
            )),
            "a cleared cap left the notice on screen: {changes:?}"
        );

        // An unchanged flag on an empty snapshot still says nothing: this is
        // a repaint on the edge, not on every publish.
        let mut rx = store.subscribe();
        store.apply(capped(false));
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|c| matches!(c, ChangeEvent::BeaconBatch { .. })),
            "an unchanged snapshot repainted the panel anyway"
        );
    }

    /// The 15-minute discovered window is measured on the **store** clock,
    /// not on the device-reported `last_seen_ts` a stranger may never send.
    #[test]
    fn last_snapshot_at_advances_on_every_snapshot_carrying_the_mac() {
        let store = StateStore::new();
        let mut tag = eye_tag("AA:BB:CC:DD:EE:FF");
        tag.discovered = Some(true);
        // No `last_seen_ts` at all — the wire field the window must not use.
        tag.last_seen_ts = None;
        store.apply(eye_msg(vec![tag.clone()]));

        let device = store.snapshot_device("h1").unwrap();
        let beacon = &device.ble_sensors["AA:BB:CC:DD:EE:FF"];
        let first = beacon
            .last_snapshot_at
            .expect("stamped on the first snapshot");
        assert!(first > 0);
        assert!(beacon.last_seen.is_none());
        // The raw tag is kept for the detail page's §3.3 preview.
        assert_eq!(
            beacon.last_payload.as_ref().and_then(|v| v["mac"].as_str()),
            Some("AA:BB:CC:DD:EE:FF")
        );

        store.apply(eye_msg(vec![tag]));
        let device = store.snapshot_device("h1").unwrap();
        assert!(
            device.ble_sensors["AA:BB:CC:DD:EE:FF"]
                .last_snapshot_at
                .expect("still stamped")
                >= first
        );
    }

    #[test]
    fn sticker_config_pages_merge_into_cache() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        let mut page1 = node_config_msg();
        page1.config.insert("k1".into(), json!(1));
        page1.page_count = Some(2);
        page1.read_status = Some("partial".into());
        page1.missing = Some(vec!["k2".into()]);
        page1.error = Some("timeout".into());
        store.apply(IngestEvent::StickerConfig {
            hostname: "h1".into(),
            node_key: "AA11".into(), // wire casing — lowercased
            payload: page1,
        });

        let mut page2 = node_config_msg();
        page2.config.insert("k2".into(), json!("v"));
        page2.read_status = Some("complete".into());
        page2.missing = Some(vec![]);
        store.apply(IngestEvent::StickerConfig {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: page2,
        });

        let device = store.snapshot_device("h1").unwrap();
        let entry = &device.node_configs["aa11"];
        // Pages merged into one config map.
        assert_eq!(entry.config.get("k1"), Some(&json!(1)));
        assert_eq!(entry.config.get("k2"), Some(&json!("v")));
        assert_eq!(entry.page_count, 2);
        assert_eq!(entry.read_status, "complete");
        assert!(entry.missing.is_empty());
        // Error is verbatim per publish: the clean page cleared it.
        assert!(entry.error.is_none());
        assert!(drain(&mut rx).contains(&ChangeEvent::NodeConfig {
            hostname: "h1".into(),
            node_key: "aa11".into()
        }));
    }

    #[test]
    fn sticker_full_config_recomputes_and_promotes() {
        let store = StateStore::new();

        let mut page1 = node_full_config_msg();
        page1.config.insert("k1".into(), json!(1));
        page1.missing = Some(vec!["k2".into()]);
        page1.chunks_total = Some(2);
        page1.read_status = Some("partial".into());
        page1.last_seq = Some(7);
        store.apply(IngestEvent::StickerFullConfig {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: page1,
        });

        let entry = store.snapshot_device("h1").unwrap().node_full_configs["aa11"].clone();
        assert_eq!(entry.missing, vec!["k2".to_string()]);
        assert_eq!(entry.chunks_total, 2);
        assert_eq!(entry.chunks_done, 1); // recomputed vs merged snapshot
        assert_eq!(entry.read_status, "partial");
        assert_eq!(entry.last_seq, Some(7));

        // Second frame delivers k2; firmware still reports it missing and
        // partial — the merged snapshot says otherwise.
        let mut page2 = node_full_config_msg();
        page2.config.insert("k2".into(), json!(2));
        page2.missing = Some(vec!["k2".into()]);
        page2.read_status = Some("partial".into());
        store.apply(IngestEvent::StickerFullConfig {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: page2,
        });

        let entry = store.snapshot_device("h1").unwrap().node_full_configs["aa11"].clone();
        assert!(entry.missing.is_empty());
        assert_eq!(entry.chunks_done, 2);
        assert_eq!(entry.read_status, "complete"); // partial → complete
    }

    #[test]
    fn sticker_info_replaced_wholesale_and_tombstoned() {
        let store = StateStore::new();
        store.apply(IngestEvent::StickerInfo {
            hostname: "h1".into(),
            node_key: "AA11".into(),
            payload: json!({"seq": 1, "fw_major": 1}),
        });
        store.apply(IngestEvent::StickerInfo {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: json!({"seq": 2}),
        });
        let device = store.snapshot_device("h1").unwrap();
        // Replaced wholesale — fw_major from the first publish is gone.
        assert_eq!(device.node_infos["aa11"], json!({"seq": 2}));

        // Empty payload (routed as Null) is the removal tombstone.
        let mut rx = store.subscribe();
        store.apply(IngestEvent::StickerInfo {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::Value::Null,
        });
        let device = store.snapshot_device("h1").unwrap();
        assert!(device.node_infos.is_empty());
        assert!(drain(&mut rx).contains(&ChangeEvent::NodeConfig {
            hostname: "h1".into(),
            node_key: "aa11".into()
        }));

        // A replayed tombstone for an unknown host creates nothing.
        store.apply(IngestEvent::StickerInfo {
            hostname: "ghost".into(),
            node_key: "aa11".into(),
            payload: serde_json::Value::Null,
        });
        assert!(store.snapshot_device("ghost").is_none());
    }

    #[test]
    fn sticker_command_result_is_transient_only() {
        let store = StateStore::new();
        let mut rx = store.subscribe();
        let payload = json!({"command": "reboot", "result": "requested"});
        store.apply(IngestEvent::StickerCommandResult {
            hostname: "h1".into(),
            node_key: "AA11".into(),
            payload: payload.clone(),
        });
        assert_eq!(
            drain(&mut rx),
            vec![ChangeEvent::NodeCommandResult {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload
            }]
        );
        // Not cached — no state was touched, not even device creation.
        assert!(store.snapshot_device("h1").is_none());
    }

    #[test]
    fn sticker_history_emits_transient_progress() {
        let store = StateStore::new();
        let mut rx = store.subscribe();
        store.apply(IngestEvent::StickerHistory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: NodeHistoryMsg {
                dev_eui: Some("aa11".into()),
                frame_index: Some(1),
                frame_count: Some(3),
                interval_s: None,
                records: vec![
                    NodeHistoryRecord {
                        time: Some(1_736_775_000),
                        fields: HashMap::new(),
                        counters: HashMap::new(),
                    },
                    NodeHistoryRecord {
                        time: Some(1_736_775_060),
                        fields: HashMap::new(),
                        counters: HashMap::new(),
                    },
                ],
            },
        });
        assert_eq!(
            drain(&mut rx),
            vec![ChangeEvent::NodeHistoryProgress {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                frame_index: 1,
                frame_count: 3,
                points: 2
            }]
        );
        assert!(store.snapshot_device("h1").is_none());
    }

    /// The v2 replay frame reports progress under the Hub's canonical spelling, which is
    /// what the backfill table it re-renders is keyed by.
    #[test]
    fn a_v2_history_frame_reports_progress_under_the_canonical_spelling() {
        let store = StateStore::new();
        store.apply(IngestEvent::Status {
            hostname: "FIBER-4E970F4A".into(),
            status: DeviceStatus::Online,
        });
        let mut rx = store.subscribe();
        store.apply(IngestEvent::NodeHistory {
            hostname: "fiber-4e970f4a".into(),
            node_key: "5876070000000413".into(),
            payload: NodeHistoryMsg {
                dev_eui: Some("5876070000000413".into()),
                frame_index: Some(0),
                frame_count: Some(1),
                interval_s: None,
                records: vec![NodeHistoryRecord {
                    time: Some(1_790_319_991),
                    fields: HashMap::new(),
                    counters: HashMap::new(),
                }],
            },
        });
        assert_eq!(
            drain(&mut rx),
            vec![ChangeEvent::NodeHistoryProgress {
                // Decision #21: one lowercase hostname on ingest, whatever the Hub sent.
                hostname: "fiber-4e970f4a".into(),
                node_key: "5876070000000413".into(),
                frame_index: 0,
                frame_count: 1,
                points: 1
            }]
        );
    }

    // ---- WP-E: the v2 radio northbound family ----------------------------

    fn v2_reading(message_id: &str, received_at: &str, temperature: f64) -> NodeReadingMsg {
        NodeReadingMsg {
            message_id: message_id.to_string(),
            dev_eui: "58760700c0668fc7".into(),
            serial: Some("SN-1".into()),
            fields: [("temperature".to_string(), temperature)]
                .into_iter()
                .collect(),
            counters: [("hall_left_count".to_string(), 7u64)]
                .into_iter()
                .collect(),
            events: Vec::new(),
            gateways: vec![
                control_radio::decode::reading::GatewayRx {
                    gateway_id: "quiet".into(),
                    rssi: Some(-87),
                    snr: Some(4.25),
                },
                control_radio::decode::reading::GatewayRx {
                    gateway_id: "loud".into(),
                    rssi: Some(-41),
                    snr: Some(13.5),
                },
            ],
            dr: Some(5),
            rssi: Some(-99),
            snr: Some(0.0),
            received_at: received_at.to_string(),
            fport: Some(2),
            page_index: None,
            page_count: None,
        }
    }

    /// One `proximos/…/nodes/{id}/reading` upserts one Node — no array, and no "dropped
    /// because it carried no fields" rule (plan.md §9.4 bans the full-array shape the v1 arm
    /// was written for).
    #[test]
    fn a_radio_reading_upserts_one_node_and_takes_the_loudest_gateway() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-1", "2026-08-27T09:59:19Z", 21.5)),
        });

        let device = store.snapshot_device("h1").expect("device created");
        let node = device.nodes.get("aa11").expect("node upserted");
        assert_eq!(node.serial_number.as_deref(), Some("SN-1"));
        assert_eq!(node.fields.get("temperature"), Some(&21.5));
        assert_eq!(node.counters.get("hall_left_count"), Some(&7));
        assert_eq!(node.transport, portal_domain::Transport::Lorawan);
        assert_eq!(node.dr, Some(5));
        assert_eq!(
            node.last_seen.as_deref(),
            Some("2026-08-27T09:59:19Z"),
            "received_at verbatim — it is the persistence dedup key too"
        );
        // Best gateway wins over the envelope's own scalar fallback.
        assert_eq!(node.rssi, Some(-41));
        assert_eq!(node.snr, Some(13.5));
        assert_eq!(node.gateways.len(), 2);

        let changes = drain(&mut rx);
        assert!(changes.contains(&ChangeEvent::NodeBatch {
            hostname: "h1".into()
        }));
    }

    /// The restart seed only fills a Node with no signal: a live reading that landed first
    /// is newer than anything in storage and is kept.
    #[test]
    fn a_live_reading_beats_the_signal_seed() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-1", "2026-08-27T09:59:19Z", 21.5)),
        });

        let stale = vec![NodeGatewayLink {
            gateway_id: Some("old".into()),
            rssi: Some(-120),
            snr: Some(-15.0),
        }];
        assert!(!store.seed_node_signal("h1", "aa11", Some(-120), Some(-15.0), stale));

        let node = store.snapshot_device("h1").unwrap().nodes["aa11"].clone();
        assert_eq!(node.rssi, Some(-41));
        assert_eq!(node.snr, Some(13.5));
        assert_eq!(node.gateways.len(), 2);
    }

    /// An `alarm` document carries events and no measurements, and must still land: the v1
    /// arm's "no fields, drop it" rule would have thrown the whole batch away.
    #[test]
    fn a_radio_alarm_with_no_fields_still_upserts_and_keeps_its_events() {
        let store = StateStore::new();
        let mut payload = v2_reading("m-2", "2026-08-27T10:00:00Z", 0.0);
        payload.fields.clear();
        payload.counters.clear();
        payload
            .events
            .push(control_radio::decode::reading::NodeEvent::new(
                "alarm",
                "2026-08-27T10:00:00Z",
                serde_json::Map::from_iter([("slot".to_string(), serde_json::Value::from(0))]),
            ));

        store.apply(IngestEvent::NodeAlarm {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(payload),
        });

        let node = store
            .snapshot_device("h1")
            .expect("device")
            .nodes
            .get("aa11")
            .cloned()
            .expect("node upserted from an alarm document");
        assert_eq!(node.recent_events.len(), 1);
        assert_eq!(node.recent_events[0]["type"], "alarm");
    }

    /// The retained inventory document and its tombstone: an empty payload arrives as `Null`
    /// and removes the Node from the inventory *and* from the telemetry map, so a removed
    /// device cannot linger on a rendered list.
    #[test]
    fn radio_info_upserts_the_inventory_and_an_empty_payload_removes_the_node() {
        let store = StateStore::new();

        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-1", "2026-08-27T09:59:19Z", 21.5)),
        });
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::json!({"serial_number": "SN-1", "dev_eui": "aa11"}),
        });

        let device = store.snapshot_device("h1").expect("device");
        assert_eq!(device.node_infos["aa11"]["serial_number"], "SN-1");
        assert!(device.nodes.contains_key("aa11"));

        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::Value::Null,
        });

        let device = store.snapshot_device("h1").expect("device");
        assert!(!device.node_infos.contains_key("aa11"), "inventory cleared");
        assert!(
            !device.nodes.contains_key("aa11"),
            "and the telemetry entry goes with it"
        );
    }

    fn device_info_doc(
        received_at: &str,
        info: serde_json::Value,
    ) -> crate::state::NodeDeviceInfoMsg {
        crate::state::NodeDeviceInfoMsg {
            dev_eui: "aa11".into(),
            received_at: received_at.into(),
            device_info: info.as_object().cloned().expect("an object"),
        }
    }

    fn config_doc(
        received_at: &str,
        config: serde_json::Value,
        w1: Option<Vec<&str>>,
    ) -> crate::state::NodeConfigDumpMsg {
        crate::state::NodeConfigDumpMsg {
            dev_eui: "aa11".into(),
            received_at: received_at.into(),
            seq: 0,
            page_count: 1,
            config: config.as_object().cloned().expect("an object"),
            w1_slot_type: w1.map(|v| v.into_iter().map(str::to_string).collect()),
            body_hex: String::new(),
        }
    }

    /// A control-radio `command` document answering a command becomes a `NodeCommandResult`
    /// under the Hub's canonical spelling; the Node's own reports and half a dump do not.
    #[test]
    fn a_v2_command_answer_becomes_a_command_result_under_the_canonical_spelling() {
        let store = StateStore::new();
        let mut rx = store.subscribe_command_results();
        // The Hub known under the agent's spelling first.
        store.apply(IngestEvent::Status {
            hostname: "FIBER-4E970F4A".into(),
            status: DeviceStatus::Online,
        });
        for doc in [
            serde_json::json!({"kind": "info", "seq": 0}),
            serde_json::json!({"kind": "config_dump", "seq": 21, "complete": false}),
            serde_json::json!({"kind": "history_frame", "seq": 9}),
            serde_json::json!({"kind": "ack", "seq": 22}),
            serde_json::json!({"kind": "error", "seq": 12,
                               "error": {"code": 7, "detail": "not supported"}}),
        ] {
            store.apply(IngestEvent::NodeCommand {
                hostname: "fiber-4e970f4a".into(),
                node_key: "5876070000000413".into(),
                payload: doc,
            });
        }
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let ChangeEvent::NodeCommandResult {
                hostname, payload, ..
            } = ev
            {
                got.push((hostname, payload));
            }
        }
        assert_eq!(
            got.len(),
            2,
            "only the ack and the refusal answer a command: {got:?}"
        );
        assert_eq!(got[0].0, "fiber-4e970f4a", "the canonical spelling (#21)");
        assert_eq!(got[0].1["result"], "ok");
        assert_eq!(got[0].1["seq"], 22);
        assert_eq!(got[0].1["transport"], "control.radio");
        assert_eq!(got[1].1["result"], "error");
        assert_eq!(got[1].1["detail"], "code 7 not supported");
        assert_eq!(got[1].1["code"], 7, "the code as the firmware sent it");
    }

    /// Q-N1: the `superseded` document control-radio publishes for a queued frame a newer ask
    /// replaced is no answer: no `NodeCommandResult` (no toast, no error), and no join.
    #[test]
    fn a_superseded_command_document_closes_quietly() {
        let store = StateStore::new();
        let mut results = store.subscribe_command_results();
        let mut changes = store.subscribe();
        store.apply(IngestEvent::NodeCommand {
            hostname: "fiber-4e970f4a".into(),
            node_key: "5876070000000413".into(),
            payload: control_radio::coalesce::superseded_document(
                "5876070000000413",
                Some("0413"),
                7,
                9,
                "2026-09-28T08:00:00Z",
            ),
        });
        assert!(results.try_recv().is_err(), "no command result");
        while let Ok(ev) = changes.try_recv() {
            assert!(
                !matches!(
                    ev,
                    ChangeEvent::NodeCommandResult { .. } | ChangeEvent::NodeJoined { .. }
                ),
                "{ev:?}"
            );
        }
    }

    /// #470 H4 / §10.5: the Hub's `undelivered` document ends the wait for its `seq` as a
    /// command result of its own, not as an ok and not as the Node's refusal.
    #[test]
    fn an_undelivered_command_document_is_a_result_of_its_own() {
        let store = StateStore::new();
        let mut results = store.subscribe_command_results();
        store.apply(IngestEvent::NodeCommand {
            hostname: "fiber-4e970f4a".into(),
            node_key: "5876070000005722".into(),
            payload: serde_json::json!({
                "dev_eui": "5876070000005722", "serial": "5722", "seq": 31,
                "kind": "undelivered", "status": "undelivered", "attempts": 5,
                "last_outcome": "NotDelivered", "at": "2026-10-07T11:00:00Z",
            }),
        });
        let Ok(ChangeEvent::NodeCommandResult { payload, .. }) = results.try_recv() else {
            panic!("a command result");
        };
        assert_eq!(payload["transport"], "control.radio");
        assert_eq!(payload["result"], "undelivered");
        assert_eq!(payload["command"], "undelivered");
        assert_eq!(payload["seq"], 31);
        assert_eq!(payload["detail"], "5 attempts, last NotDelivered");
        assert!(!crate::sse::NODE_COMMAND_OK_RESULTS.contains(&"undelivered"));
        assert!(
            payload.get("reason").is_none(),
            "a P2P give-up has no reason"
        );
    }

    /// #129: a LoRaWAN frame the network server discarded is `undelivered` too, but its
    /// detail is the reason it was not sent, not an attempt count it never spent.
    #[test]
    fn a_discarded_command_document_carries_its_reason() {
        let store = StateStore::new();
        let mut results = store.subscribe_command_results();
        let reason = "discarded by the network server: 88 B exceeds 51 B at the current data rate";
        store.apply(IngestEvent::NodeCommand {
            hostname: "fiber-4e970f4a".into(),
            node_key: "5876070000005132".into(),
            payload: serde_json::json!({
                "dev_eui": "5876070000005132", "serial": "5132", "seq": 13,
                "kind": "undelivered", "status": "undelivered", "attempts": 0,
                "last_outcome": reason, "reason": reason,
                "code": "DOWNLINK_PAYLOAD_SIZE", "at": "2026-10-13T12:08:18Z",
            }),
        });
        let Ok(ChangeEvent::NodeCommandResult { payload, .. }) = results.try_recv() else {
            panic!("a command result");
        };
        assert_eq!(payload["result"], "undelivered");
        assert_eq!(payload["seq"], 13);
        assert_eq!(payload["detail"], reason);
        assert_eq!(payload["reason"], reason);
    }

    /// P-9 / N32: the persist thread's grade becomes the Node's value axis — the per-field
    /// states and the worst of them as `alarm_state` — and a field the next reading does not
    /// carry keeps its state. An unchanged grade emits nothing.
    #[test]
    fn a_graded_reading_sets_the_nodes_value_axis() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeReading {
            hostname: "FIBER-4E970F4A".into(),
            node_key: "5876070000000413".into(),
            payload: Box::new(NodeReadingMsg {
                message_id: "m-1".into(),
                dev_eui: "5876070000000413".into(),
                serial: None,
                fields: [
                    ("temperature".to_string(), 31.0),
                    ("humidity".to_string(), 50.0),
                ]
                .into_iter()
                .collect(),
                counters: Default::default(),
                events: Vec::new(),
                gateways: Vec::new(),
                rssi: None,
                snr: None,
                fport: None,
                page_index: None,
                page_count: None,
                dr: None,
                received_at: "2026-09-26T00:00:00Z".into(),
            }),
        });
        let mut rx = store.subscribe();
        let states = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert!(store.set_node_field_states(
            "fiber-4e970f4a",
            "5876070000000413",
            &states(&[("temperature", "CRITICAL"), ("humidity", "NORMAL")]),
        ));
        let node = |store: &StateStore| {
            store
                .snapshot_device("fiber-4e970f4a")
                .and_then(|d| d.nodes.get("5876070000000413").cloned())
                .expect("node")
        };
        let n = node(&store);
        assert_eq!(n.alarm_state, "CRITICAL");
        assert_eq!(
            n.field_alarm_states.get("temperature").map(String::as_str),
            Some("CRITICAL")
        );
        assert!(n.field_alarm_triggered_at.contains_key("temperature"));
        assert!(matches!(rx.try_recv(), Ok(ChangeEvent::Node { .. })));

        // Same grade: nothing to repaint.
        assert!(!store.set_node_field_states(
            "fiber-4e970f4a",
            "5876070000000413",
            &states(&[("temperature", "CRITICAL")]),
        ));
        // Back inside, humidity not in this reading: it keeps NORMAL, the Node is NORMAL.
        assert!(store.set_node_field_states(
            "fiber-4e970f4a",
            "5876070000000413",
            &states(&[("temperature", "NORMAL")]),
        ));
        let n = node(&store);
        assert_eq!(n.alarm_state, "NORMAL");
        assert_eq!(n.field_alarm_states.len(), 2);
    }

    /// A join is the inventory's `joined_at` moving forward (node-firmware-v2.0.0.md §5): the
    /// store says so once, on the narrow channel the send walk listens to, under the canonical
    /// spelling (#21). The first document only replays the last join, and an `Info` with seq 0
    /// is v2.0.0's periodic announce (#445), not a join.
    #[test]
    fn a_join_is_joined_at_moving_forward_not_an_info_with_seq_0() {
        let store = StateStore::new();
        let mut rx = store.subscribe_command_results();
        store.apply(IngestEvent::Status {
            hostname: "FIBER-4E970F4A".into(),
            status: DeviceStatus::Online,
        });
        let inventory = |joined_at: &str| IngestEvent::NodeInventory {
            hostname: "fiber-4e970f4a".into(),
            node_key: "5876070000000413".into(),
            payload: serde_json::json!({
                "dev_eui": "5876070000000413",
                "transport": "lorawan",
                "joined_at": joined_at,
            }),
        };
        store.apply(inventory("2026-10-09T08:00:00Z"));
        store.apply(IngestEvent::NodeCommand {
            hostname: "fiber-4e970f4a".into(),
            node_key: "5876070000000413".into(),
            payload: serde_json::json!({"kind": "info", "seq": 0}),
        });
        store.apply(inventory("2026-10-09T08:00:00Z"));
        store.apply(inventory("2026-10-09T09:30:00Z"));
        let mut joins = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let ChangeEvent::NodeJoined {
                hostname, node_key, ..
            } = ev
            {
                joins.push((hostname, node_key));
            }
        }
        assert_eq!(
            joins,
            vec![("fiber-4e970f4a".to_string(), "5876070000000413".to_string())],
            "one join: the second joined_at"
        );
    }

    /// MR !129 review: `joined_at` going from absent to present is a join on a Node an earlier
    /// document named (a `registered` Node's first join, the first join after a carrier move),
    /// and not on the first document the store sees (the retained replay after subscribe).
    #[test]
    fn a_first_join_is_reported_but_the_replayed_one_is_not() {
        let store = StateStore::new();
        let mut rx = store.subscribe_command_results();
        let doc = |node: &str, transport: &str, joined_at: Option<&str>| {
            let mut payload = serde_json::json!({ "dev_eui": node, "transport": transport });
            if let Some(at) = joined_at {
                payload["joined_at"] = serde_json::json!(at);
            }
            IngestEvent::NodeInventory {
                hostname: "h1".into(),
                node_key: node.into(),
                payload,
            }
        };
        let joins = |rx: &mut broadcast::Receiver<ChangeEvent>| {
            let mut out = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                if let ChangeEvent::NodeJoined { node_key, .. } = ev {
                    out.push(node_key);
                }
            }
            out
        };

        // The replay after subscribe: a joined Node's first document is no join.
        store.apply(doc("aa11", "lorawan", Some("2026-10-09T08:00:00Z")));
        assert!(
            joins(&mut rx).is_empty(),
            "the replay restates the last join"
        );

        // A registered Node, then its first join.
        store.apply(doc("bb22", "lorawan", None));
        assert!(joins(&mut rx).is_empty());
        store.apply(doc("bb22", "lorawan", Some("2026-10-09T08:05:00Z")));
        assert_eq!(joins(&mut rx), ["bb22"], "None -> Some on a known Node");

        // aa11 moves to P2P: the session is forgotten, and its first P2P join is a join.
        store.apply(doc("aa11", "p2p", None));
        assert!(joins(&mut rx).is_empty());
        store.apply(doc("aa11", "p2p", Some("2026-10-09T08:00:00Z")));
        assert_eq!(
            joins(&mut rx),
            ["aa11"],
            "first join after the carrier move"
        );
    }

    /// The Node's own `Info` lands in its own map, never in `node_infos`, which on the v2
    /// path is the inventory every uplink replaces (node_reported_state.md §0 finding 2).
    #[test]
    fn a_device_info_document_is_kept_apart_from_the_inventory_and_tombstoned() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::json!({"dev_eui": "aa11", "pairing_status": "joined"}),
        });
        store.apply(IngestEvent::NodeDeviceInfo {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Some(Box::new(device_info_doc(
                "2026-09-22T19:09:04Z",
                serde_json::json!({"fw_version": "1.4.0", "battery": 3458}),
            ))),
        });
        // A later uplink's inventory refresh must not touch it.
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::json!({"dev_eui": "aa11", "pairing_status": "joined",
                                        "last_seen": "2026-09-22T19:20:00Z"}),
        });

        let device = store.snapshot_device("h1").expect("device");
        let entry = &device.node_device_infos["aa11"];
        assert_eq!(entry.received_at, "2026-09-22T19:09:04Z");
        assert_eq!(entry.info["fw_version"], "1.4.0");
        assert!(device.node_infos["aa11"].get("fw_version").is_none());

        store.apply(IngestEvent::NodeDeviceInfo {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: None,
        });
        let device = store.snapshot_device("h1").expect("device");
        assert!(!device.node_device_infos.contains_key("aa11"), "tombstoned");
    }

    /// The Hub's document is the accumulated configuration, so the portal takes it whole: a
    /// key the Hub dropped (a cleared alarm rule after a complete `get-config`) goes here too.
    #[test]
    fn a_config_document_replaces_the_node_config_it_updates() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeDeviceConfig {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Some(Box::new(config_doc(
                "2026-09-22T19:09:09Z",
                serde_json::json!({"application.interval_report": 900,
                                   "alarms.alarm_0": "0300"}),
                Some(vec!["dallas", "empty", "empty", "empty"]),
            ))),
        });
        // The Hub's next document: the cleared rule dropped, one key changed, slots carried.
        store.apply(IngestEvent::NodeDeviceConfig {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Some(Box::new(config_doc(
                "2026-09-22T20:00:00Z",
                serde_json::json!({"application.interval_report": 600}),
                Some(vec!["dallas", "empty", "empty", "empty"]),
            ))),
        });

        let device = store.snapshot_device("h1").expect("device");
        let entry = &device.node_configs["aa11"];
        assert_eq!(entry.config["application.interval_report"], 600);
        assert!(
            !entry.config.contains_key("alarms.alarm_0"),
            "what the Hub dropped goes here too"
        );
        assert_eq!(entry.config.len(), 1);
        assert_eq!(entry.synced_at.as_deref(), Some("2026-09-22T20:00:00Z"));
        assert_eq!(entry.read_status, "complete");
        assert_eq!(entry.page_count, 1);
        assert_eq!(
            entry.w1_slot_type.as_deref(),
            Some(
                &[
                    "dallas".to_string(),
                    "empty".into(),
                    "empty".into(),
                    "empty".into()
                ][..]
            ),
        );

        store.apply(IngestEvent::NodeDeviceConfig {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: None,
        });
        let device = store.snapshot_device("h1").expect("device");
        assert!(!device.node_configs.contains_key("aa11"), "tombstoned");
    }

    /// A replayed tombstone proves nothing about the Hub, so it creates no device.
    #[test]
    fn a_node_reported_state_tombstone_for_an_unknown_hub_creates_nothing() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeDeviceInfo {
            hostname: "h9".into(),
            node_key: "aa11".into(),
            payload: None,
        });
        store.apply(IngestEvent::NodeDeviceConfig {
            hostname: "h9".into(),
            node_key: "aa11".into(),
            payload: None,
        });
        assert!(store.snapshot_device("h9").is_none());
    }

    /// The topic no longer says which radio a Node speaks (GitLab #73); the retained `info`
    /// payload does, and it has to win in either arrival order.
    #[test]
    fn radio_info_sets_the_transport_whichever_document_arrives_first() {
        let store = StateStore::new();
        let p2p_info = serde_json::json!({"dev_eui": "aa11", "transport": "p2p"});

        // Reading first, then info: the existing Node is updated in place.
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-1", "2026-08-27T09:59:19Z", 21.5)),
        });
        assert_eq!(
            store.snapshot_device("h1").expect("device").nodes["aa11"].transport,
            portal_domain::Transport::Lorawan,
            "no info yet: the model default stands"
        );
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: p2p_info.clone(),
        });
        assert_eq!(
            store.snapshot_device("h1").expect("device").nodes["aa11"].transport,
            portal_domain::Transport::P2p
        );

        // Info first (the retained replay on subscribe), then the first reading.
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "bb22".into(),
            payload: p2p_info,
        });
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "bb22".into(),
            payload: Box::new(v2_reading("m-2", "2026-08-27T09:59:20Z", 21.5)),
        });
        assert_eq!(
            store.snapshot_device("h1").expect("device").nodes["bb22"].transport,
            portal_domain::Transport::P2p
        );

        // A document without the field leaves the transport alone rather than resetting it.
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "bb22".into(),
            payload: serde_json::json!({"dev_eui": "bb22"}),
        });
        assert_eq!(
            store.snapshot_device("h1").expect("device").nodes["bb22"].transport,
            portal_domain::Transport::P2p
        );
    }

    /// p2p_integration.md §4.0: the retained `info` document is the single source of a Node's
    /// transport and P2P identity. A `registered` document (no session yet) creates the row so
    /// devices.node.md §5's "registered — waiting for join" has something to render; the
    /// joined document that follows fills the session fields in place.
    #[test]
    fn an_info_document_sets_transport_and_the_p2p_identity_fields() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::json!({
                "transport": "p2p", "dev_eui": "aa11", "serial_number": "SN-1",
                "profile": "default", "pairing_status": "registered"
            }),
        });
        let changes = drain(&mut rx);
        assert!(
            changes.iter().any(
                |c| matches!(c, ChangeEvent::NodeAdded { node_key, .. } if node_key == "aa11")
            ),
            "a registered Node is a row before its first frame: {changes:?}"
        );
        let node = store.snapshot_device("h1").expect("device").nodes["aa11"].clone();
        assert_eq!(node.transport, portal_domain::Transport::P2p);
        assert_eq!(node.serial_number.as_deref(), Some("SN-1"));
        assert!(node.last_seen.is_none(), "no frame has arrived");
        assert!(node.session_state.is_none() && node.dev_addr.is_none());

        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::json!({
                "transport": "p2p", "dev_eui": "aa11", "serial_number": "SN-1",
                "pairing_status": "joined", "dev_addr": 1, "net_id": 7,
                "session_state": "joined", "joined_at": "2026-09-15T10:00:00Z"
            }),
        });
        let changes = drain(&mut rx);
        assert!(
            !changes
                .iter()
                .any(|c| matches!(c, ChangeEvent::NodeAdded { .. })),
            "the second document updates the row, it does not add one"
        );
        let node = store.snapshot_device("h1").expect("device").nodes["aa11"].clone();
        assert_eq!(node.dev_addr.as_deref(), Some("1"));
        assert_eq!(node.net_id.as_deref(), Some("7"));
        assert_eq!(
            node.session_state,
            Some(portal_domain::SessionState::Joined)
        );
        assert_eq!(node.last_join_at, Some(1_789_466_400));

        // A transition the central spells that the portal has a name for.
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::json!({"transport": "p2p", "session_state": "detach_pending"}),
        });
        let node = store.snapshot_device("h1").expect("device").nodes["aa11"].clone();
        assert_eq!(
            node.session_state,
            Some(portal_domain::SessionState::DetachPending)
        );
        assert_eq!(
            node.dev_addr.as_deref(),
            Some("1"),
            "absent keys leave earlier facts alone"
        );
    }

    /// p2p_integration.md §2.3: a document without `transport` (pre-#73) reads as LoRaWAN.
    /// Bench c57, 2026-09-27: 0413 moved from P2P to LoRaWAN and its diagnostics still showed
    /// the P2P session (DevAddr 1, the NetID, the P2P join time) and the P2P frame counter
    /// beside the LoRaWAN one. The LoRaWAN document names none of those, and absent keys are
    /// kept, so the move itself has to let them go.
    #[test]
    fn a_move_to_the_other_carrier_forgets_the_old_session() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::json!({
                "transport": "p2p", "dev_eui": "aa11", "serial_number": "SN-1",
                "dev_addr": 1, "net_id": 7, "session_state": "joined",
                "joined_at": "2026-09-27T20:40:58Z"
            }),
        });
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-1", "2026-09-27T23:10:00Z", 21.5)),
        });
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::json!({
                "transport": "lorawan", "dev_eui": "aa11", "serial_number": "SN-1",
                "activation": "otaa", "pairing_status": "joined"
            }),
        });
        let node = store.snapshot_device("h1").expect("device").nodes["aa11"].clone();
        assert_eq!(node.transport, portal_domain::Transport::Lorawan);
        assert_eq!(
            node.serial_number.as_deref(),
            Some("SN-1"),
            "identity stays"
        );
        assert!(
            node.dev_addr.is_none()
                && node.net_id.is_none()
                && node.session_state.is_none()
                && node.last_join_at.is_none(),
            "the P2P session is gone: {node:?}"
        );
        assert!(
            !node.counters.contains_key("counter"),
            "the P2P frame counter is gone: {:?}",
            node.counters
        );
    }

    /// node_registry.md §5 (amended 2026-09-28, Q-N2): the LoRaWAN `info` carries the join's
    /// `dev_addr` in ChirpStack's hex spelling and `joined_at` with ChirpStack's microseconds.
    /// Both land in the fields the P2P session fills, as text and as the join second.
    #[test]
    fn a_lorawan_info_carries_the_hex_dev_addr_and_the_join_time() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "bb22".into(),
            payload: serde_json::json!({
                "transport": "lorawan", "dev_eui": "bb22", "serial_number": "SN-2",
                "activation": "otaa", "pairing_status": "joined",
                "dev_addr": "01ab23cd", "joined_at": "2026-09-27T21:04:11.482161Z"
            }),
        });
        let node = store.snapshot_device("h1").expect("device").nodes["bb22"].clone();
        assert_eq!(node.transport, portal_domain::Transport::Lorawan);
        assert_eq!(node.dev_addr.as_deref(), Some("01ab23cd"));
        assert_eq!(node.last_join_at, Some(1_790_543_051));
    }

    #[test]
    fn an_info_without_transport_reads_as_lorawan() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "cc33".into(),
            payload: serde_json::json!({"dev_eui": "cc33", "serial_number": "SN-3", "name": "Hall"}),
        });
        let node = store.snapshot_device("h1").expect("device").nodes["cc33"].clone();
        assert_eq!(node.transport, portal_domain::Transport::Lorawan);
        assert_eq!(node.name.as_deref(), Some("Hall"));
    }

    // ---- B4 (devices.backend.md §7) --------------------------------------

    /// `/nodes` prepends a row on `NodeAdded`, so the event has to fire on
    /// a true insert and only on one. Every telemetry frame runs the same
    /// `.entry().or_insert_with()`; if that were the trigger the table
    /// would grow a row per reading.
    #[test]
    fn a_new_node_emits_node_added_once_and_a_repaint_thereafter() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        store.apply(lorawan_msg(vec![node_sensor("AA11")]));
        let first = drain(&mut rx);
        assert!(
            first.contains(&ChangeEvent::NodeAdded {
                hostname: "h1".into(),
                node_key: "aa11".into()
            }),
            "the first sight of a Node is an insert: {first:?}"
        );

        // The very same Node reports again.
        store.apply(lorawan_msg(vec![node_sensor("AA11")]));
        let second = drain(&mut rx);
        assert!(
            !second
                .iter()
                .any(|c| matches!(c, ChangeEvent::NodeAdded { .. })),
            "an upsert is not an insert: {second:?}"
        );
        assert!(
            second
                .iter()
                .any(|c| matches!(c, ChangeEvent::NodeBatch { .. })),
            "it still repaints"
        );
    }

    /// The same rule on the v2 arm, which reaches the store through a
    /// different function entirely.
    #[test]
    fn the_v2_arm_also_announces_only_a_true_insert() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-1", "2026-08-27T09:59:19Z", 21.5)),
        });
        assert!(drain(&mut rx).contains(&ChangeEvent::NodeAdded {
            hostname: "h1".into(),
            node_key: "aa11".into()
        }));

        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-2", "2026-08-27T10:09:19Z", 22.0)),
        });
        assert!(!drain(&mut rx)
            .iter()
            .any(|c| matches!(c, ChangeEvent::NodeAdded { .. })));
    }

    /// The bug B4 found: the v2 tombstone pushed `NodeConfig` alone, so a
    /// removal on that arm repainted a config card for a Node that no
    /// longer existed and left its row on `/nodes` until somebody
    /// reloaded — a removal that looked like it had not happened.
    #[test]
    fn a_v2_tombstone_removes_the_row_and_not_only_the_config() {
        let store = StateStore::new();
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(v2_reading("m-1", "2026-08-27T09:59:19Z", 21.5)),
        });

        let mut rx = store.subscribe();
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: serde_json::Value::Null,
        });

        let changes = drain(&mut rx);
        assert!(
            changes.contains(&ChangeEvent::NodeRemoved {
                hostname: "h1".into(),
                node_key: "aa11".into()
            }),
            "the row has to go from every open list: {changes:?}"
        );
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, ChangeEvent::NodeConfig { .. })),
            "and the config card still clears"
        );
    }

    /// A tombstone for a Node this Hub does not have is not a removal, so
    /// it must not tell every open tab to delete a row.
    #[test]
    fn a_tombstone_for_an_unknown_node_announces_no_removal() {
        let store = StateStore::new();
        store.apply(IngestEvent::Status {
            hostname: "h1".into(),
            status: DeviceStatus::Online,
        });
        let mut rx = store.subscribe();
        store.apply(IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "nosuch".into(),
            payload: serde_json::Value::Null,
        });
        assert!(!drain(&mut rx)
            .iter()
            .any(|c| matches!(c, ChangeEvent::NodeRemoved { .. })));
    }

    /// A replayed tombstone for a Hub this portal has never heard from must not create the
    /// device — it proves nothing about anything being alive.
    #[test]
    fn a_replayed_radio_tombstone_creates_nothing() {
        let store = StateStore::new();
        let mut rx = store.subscribe();
        store.apply(IngestEvent::NodeInventory {
            hostname: "ghost".into(),
            node_key: "aa11".into(),
            payload: serde_json::Value::Null,
        });
        assert!(store.snapshot_device("ghost").is_none());
        assert!(drain(&mut rx).is_empty());
    }

    /// Decision #41: the Portal's bounds of a line win over the Hub's echo, before and after
    /// it arrives, so a save shows at once whatever the Hub holds.
    #[test]
    fn portal_line_limits_win_over_the_config_state_echo() {
        use fiber_protocol::payloads::config_state::SensorThresholds as WireThresholds;
        let store = StateStore::new();
        let echo = |high: f64| {
            let mut msg = ConfigStateMsg::default();
            msg.sensors.push(SensorConfigEntry {
                line: 1,
                name: None,
                location: None,
                enabled: None,
                has_override: None,
                thresholds: Some(WireThresholds {
                    critical_low: None,
                    critical_high: Some(high),
                    alarm_low: None,
                    alarm_high: Some(high),
                    hysteresis: None,
                    enabled: None,
                    warning_low: None,
                    warning_high: None,
                }),
            });
            IngestEvent::ConfigState {
                hostname: "H1".into(),
                payload: msg,
            }
        };
        store.apply(echo(10.0));
        let portal = SensorThresholds {
            critical_high: Some(12.5),
            warning_high: Some(7.5),
            ..Default::default()
        };
        store.set_line_limits("H1", 1, portal);
        let shown = |store: &StateStore| {
            store.snapshot_device("h1").unwrap().sensors[1]
                .thresholds
                .as_ref()
                .and_then(|t| t.critical_high)
        };
        assert_eq!(shown(&store), Some(12.5));
        // The Hub re-publishes its older copy: the Portal's stays.
        store.apply(echo(10.0));
        assert_eq!(shown(&store), Some(12.5));
    }

    /// Decision #41: the Portal's Node name and location win over every Hub echo, and
    /// clearing them shows the Hub's text again at once.
    #[test]
    fn portal_node_labels_win_and_clear_back_to_the_hub() {
        let store = StateStore::new();
        let hub_named = || {
            let mut s = node_sensor("AA11");
            s.name = Some("Hub name".into());
            lorawan_msg(vec![s])
        };
        store.apply(hub_named());
        let shown = |store: &StateStore| {
            let n = store.snapshot_device("h1").unwrap().nodes["aa11"].clone();
            (n.name, n.location)
        };
        let mut rx = store.subscribe();
        store.set_meta_label(
            "H1",
            "node",
            "aa11",
            Some("Mraznice".into()),
            Some("Sklad".into()),
        );
        assert!(drain(&mut rx).contains(&ChangeEvent::Node {
            hostname: "h1".into(),
            node_key: "aa11".into()
        }));
        assert_eq!(
            shown(&store),
            (Some("Mraznice".into()), Some("Sklad".into()))
        );
        store.apply(hub_named());
        assert_eq!(
            shown(&store).0.as_deref(),
            Some("Mraznice"),
            "the echo does not win"
        );
        store.set_meta_label("h1", "node", "aa11", None, Some("Sklad".into()));
        assert_eq!(
            shown(&store),
            (Some("Hub name".into()), Some("Sklad".into()))
        );
    }
}
