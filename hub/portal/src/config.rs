//! Loads `PortalConfig` from the first existing config file, falling back to
//! built-in defaults. Search order: `$PORTAL_CONFIG`, the rendered runtime
//! path, then the dev-loop path — see plan/viewer/design.md §4.

use std::env;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const RUNTIME_CONFIG_PATH: &str = "/run/proximos/portal/config.yaml";
const DEV_CONFIG_PATH: &str = "./dev/config.yaml";

/// Node-scoped operational tuning (`nodes:`), devices.backend.md §3 WP0.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodesConfig {
    /// Silence multiplier K: a Node is offline once it has said nothing for
    /// longer than `K × interval_report`. **`0` disables detection fleet-wide**
    /// — a real setting, not "unset"; a single device is switched off with
    /// `device_meta.offline_k_override = 0` instead.
    pub offline_detector: u32,
    /// The most automatic history requests the portal sends to one Hub in any 10 minutes
    /// (devices.node.md §3.9, amendment 2026-09-25, item 6). Every downlink shares the
    /// gateway's duty cycle, and the per-Node spacing does not stop fifty Nodes that return
    /// together from asking at once. Manual requests count toward it but are never held back.
    /// `0` sends no automatic request at all.
    pub history_auto_requests_per_10min: u32,
}

impl Default for NodesConfig {
    fn default() -> Self {
        // 4 report intervals of silence before alarming: long enough that a
        // single missed uplink (routine on LoRaWAN) is not an incident,
        // short enough to notice within an hour at the default cadence.
        NodesConfig {
            offline_detector: 4,
            history_auto_requests_per_10min: 6,
        }
    }
}

/// Beacon-scoped operational tuning (`beacons:`), devices.backend.md §3 WP0.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeaconsConfig {
    /// The offline detector's expected Beacon interval, in minutes — and
    /// nothing else since storage_intervals.md §6 (S4): the Portal no longer
    /// throttles its own Beacon history, it stores the Hub's storage windows
    /// (`eye/aggregated`). It should match the Hub's
    /// `eye.aggregation_interval_s` (600 s by default).
    pub store_interval: u32,
    /// Silence multiplier K against `store_interval`. Default
    /// 1, not the Node's 4: a Beacon advertises ~90x per store interval, so
    /// one whole interval of total silence is already unambiguous. `0`
    /// disables detection fleet-wide.
    pub offline_detector: u32,
}

impl Default for BeaconsConfig {
    fn default() -> Self {
        BeaconsConfig {
            store_interval: 15,
            offline_detector: 1,
        }
    }
}

/// One rung of the derived link-quality ladder — the RSSI and SNR floors a
/// link must clear to count as this level.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalBand {
    pub rssi: i64,
    pub snr: f64,
}

/// Thresholds behind the single derived signal-quality indicator
/// (devices.node.md §2.5). In the file rather than the code by explicit
/// decision (2026-08-31): the table below is the *default* content of that
/// YAML, and it is tuned against real deployments by editing data.
///
/// Note the ladder is not uniform, and that is from the spec, not an
/// oversight: `excellent` and `good` require **both** floors, `weak`
/// requires **either**. Anything clearing neither `weak` floor is critical;
/// a link missing RSSI or SNR entirely is *unknown*, which is not a rung.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalQualityConfig {
    pub excellent: SignalBand,
    pub good: SignalBand,
    pub weak: SignalBand,
}

impl Default for SignalQualityConfig {
    fn default() -> Self {
        SignalQualityConfig {
            excellent: SignalBand {
                rssi: -80,
                snr: 7.0,
            },
            good: SignalBand {
                rssi: -95,
                snr: 0.0,
            },
            weak: SignalBand {
                rssi: -110,
                snr: -7.0,
            },
        }
    }
}

/// One quantity's default band (`thresholds.defaults.<quantity>`,
/// plan/portal/threshold_defaults.md §1–§2). A bound left out is not written: the seeded
/// row leaves it unset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultBand {
    #[serde(default)]
    pub critical_low: Option<f64>,
    #[serde(default)]
    pub warning_low: Option<f64>,
    #[serde(default)]
    pub warning_high: Option<f64>,
    #[serde(default)]
    pub critical_high: Option<f64>,
    /// How far back inside a bound a value has to come before its alarm steps down
    /// (decision #41). Left out = the shared default of the quantity
    /// (`fiber_protocol::grading`, 0.5 °C / 3 % RH).
    #[serde(default)]
    pub hysteresis: Option<f64>,
}

impl DefaultBand {
    const fn full(
        critical_low: f64,
        warning_low: f64,
        warning_high: f64,
        critical_high: f64,
    ) -> Self {
        DefaultBand {
            critical_low: Some(critical_low),
            warning_low: Some(warning_low),
            warning_high: Some(warning_high),
            critical_high: Some(critical_high),
            hysteresis: None,
        }
    }

    /// No bound set at all: nothing to pre-fill or seed.
    pub fn is_empty(&self) -> bool {
        self.critical_low.is_none()
            && self.warning_low.is_none()
            && self.warning_high.is_none()
            && self.critical_high.is_none()
    }

    /// The bounds rise along the axis wherever two are set, the order a threshold form
    /// accepts (`node_routes::check_bound_order`). A default that breaks it would seed rows
    /// no Send could deliver.
    pub fn rises(&self) -> bool {
        let below = |a: Option<f64>, b: Option<f64>, strict: bool| match (a, b) {
            (Some(a), Some(b)) if strict => a < b,
            (Some(a), Some(b)) => a <= b,
            _ => true,
        };
        let (cl, wl, wh, ch) = (
            self.critical_low,
            self.warning_low,
            self.warning_high,
            self.critical_high,
        );
        below(cl, ch, true)
            && below(cl, wl, false)
            && below(wl, wh, false)
            && below(wh, ch, false)
            && below(cl, wh, false)
            && below(wl, ch, false)
    }
}

/// The default bands every temperature and humidity sensor gets when the portal registers
/// it, stored disabled (`thresholds.defaults`, plan/portal/threshold_defaults.md).
///
/// The same numbers are the Hub's `common_alarms` for its wired lines
/// (`control-core/fiber.sensors.config.yaml`); a test reads both shipped files and fails when
/// they differ.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThresholdDefaults {
    pub temperature: DefaultBand,
    pub humidity: DefaultBand,
    pub illuminance: DefaultBand,
}

impl Default for ThresholdDefaults {
    fn default() -> Self {
        // Hynek, 2026-09-28: −10/0/40/50 °C and 0/10/80/90 %. A humidity of 0 % cannot be
        // undercut, so its critical low never fires; it is kept because the form shows four.
        ThresholdDefaults {
            // Decision #41: 0.5 °C and 3 % RH.
            temperature: DefaultBand {
                hysteresis: Some(fiber_protocol::grading::DEFAULT_HYSTERESIS_TEMPERATURE),
                ..DefaultBand::full(-10.0, 0.0, 40.0, 50.0)
            },
            humidity: DefaultBand {
                hysteresis: Some(fiber_protocol::grading::DEFAULT_HYSTERESIS_HUMIDITY),
                ..DefaultBand::full(0.0, 10.0, 80.0, 90.0)
            },
            // Hynek, 2026-10-28 (#165): 0–10 000 lx, critical bounds only; the operator
            // narrows it per sensor.
            illuminance: DefaultBand {
                critical_low: Some(0.0),
                critical_high: Some(10_000.0),
                hysteresis: Some(fiber_protocol::grading::DEFAULT_HYSTERESIS_ILLUMINANCE),
                ..DefaultBand::default()
            },
        }
    }
}

impl ThresholdDefaults {
    /// The default band of a quantity.
    pub fn band(&self, quantity: fiber_protocol::Quantity) -> DefaultBand {
        match quantity {
            fiber_protocol::Quantity::Temperature => self.temperature,
            fiber_protocol::Quantity::Humidity => self.humidity,
            fiber_protocol::Quantity::Illuminance => self.illuminance,
        }
    }

    /// The default hysteresis of a quantity: the configured one, else the shared default.
    pub fn hysteresis(&self, quantity: fiber_protocol::Quantity) -> f64 {
        self.band(quantity)
            .hysteresis
            .unwrap_or_else(|| fiber_protocol::grading::default_hysteresis(Some(quantity)))
    }
}

/// Calibration reminders (`calibration:`), plan/portal/calibration.cs.md
/// §3 as amended 2026-10-13.
///
/// Both windows count whole days before a certificate's expiry date (the
/// first day it is no longer valid). `second_warning_days` must sit strictly
/// inside `warning_days`, or the first warning would never be seen before
/// the second replaces it — [`validate`] refuses such a file by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalibrationConfig {
    /// From this many days before the expiry date the device carries the
    /// first warning ("Brzy vyprší"), the calibration alarm opens at warning
    /// severity and the first e-mail goes out.
    pub warning_days: u32,
    /// From this many days before the expiry date the device carries the
    /// second warning ("Vyprší do N dní"): the alarm stays at warning
    /// severity and the second e-mail goes out. Only the expiry date itself
    /// turns the alarm critical.
    pub second_warning_days: u32,
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        // A month to book the lab, a week's second reminder before it
        // lapses — the two numbers of the original request
        // (calibration.cs.md, header).
        CalibrationConfig {
            warning_days: 30,
            second_warning_days: 7,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PortalConfig {
    pub bind_addr: String,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub mqtt_username: Option<String>,
    pub mqtt_password: Option<String>,
    /// The MQTT client id this portal connects with (design.md §6.2).
    ///
    /// Transitional: the monolith merge collapses it into
    /// `proximos-control` (monolith.md §5). Until then the portal and the
    /// radio tasks must **not** share an id — MQTT 3.1.1 makes a second
    /// connection with the same id take the first one over, so two halves
    /// sharing one would knock each other off the broker in a loop.
    pub mqtt_client_id: String,
    pub topic_prefix: String,
    pub data_dir: String,
    /// SQLCipher key for the portal DB; `None` = plain SQLite (dev only).
    pub db_key: Option<String>,
    pub templates_dir: String,
    pub static_dir: String,
    /// Whether session cookies get the `Secure` flag. `true` by default
    /// (production is HTTPS-only); `dev/config.yaml` overrides to `false`
    /// so the dev loop's plain-HTTP `cargo run` can still keep a session.
    pub cookie_secure: bool,
    /// The hostname this portal's own machine answers to, used to decide
    /// whether a device on the bus *is* this machine — in which case pairing
    /// skips the typed code (`routes::hub_routes::is_local_device`).
    ///
    /// Resolved once at startup in `http::app`: this value if set, else
    /// `gethostname(2)`. Setting it is an override, needed by the dev mock
    /// (whose device hostname has nothing to do with the developer's laptop)
    /// and available as an operational escape hatch if the hub's MQTT
    /// hostname and its system hostname ever diverge. Getting it wrong is
    /// not dangerous: a mismatch just falls back to the code flow.
    ///
    /// It is **not** the portal's MQTT identity and never has been — that
    /// is [`PortalConfig::mqtt_client_id`]. Nothing derives a broker
    /// identity from this value, and nothing should: it names the machine
    /// the portal runs on, not the connection it opens.
    pub local_hostname: Option<String>,
    /// Gates everything outside the phase-1 scope in `plan/portal/design.md`
    /// §2 (e.g. the M10 LoRaWAN cluster / LoRa P2P UI) — `false` by default,
    /// so a build reflects tested core scope unless a deployment opts in.
    /// `http::app` merges non-core route modules only when this is `true`;
    /// `routes::render()` threads it to every template so nav entries for
    /// those routes stay hidden too (`layout/nav_items.html`'s `"experimental"`
    /// gate).
    pub experimental_features_enabled: bool,
    /// Reverse proxies whose `X-Real-IP` names the client, as IP addresses or CIDR ranges
    /// (`172.16.0.0/12`). A loopback peer (the Hub's nginx) is always trusted; the list is for
    /// a proxy on another address, such as the public demo's Caddy container (R2-4, review 2
    /// of !102). Empty by default.
    pub trusted_proxies: Vec<String>,
    /// `nodes:` — see [`NodesConfig`].
    pub nodes: NodesConfig,
    /// `beacons:` — see [`BeaconsConfig`].
    pub beacons: BeaconsConfig,
    /// `signal_quality:` — see [`SignalQualityConfig`].
    pub signal_quality: SignalQualityConfig,
    /// `thresholds.defaults:` — see [`ThresholdDefaults`].
    pub thresholds: ThresholdDefaults,
    /// `calibration:` — see [`CalibrationConfig`].
    pub calibration: CalibrationConfig,
    /// When a scheduled full `VACUUM` may start. `None` — the default, and what
    /// an absent `maintenance_window:` block means — keeps the 2026-08-24
    /// behaviour exactly: rewrites happen only when an operator asks.
    pub maintenance_window: Option<MaintenanceWindow>,
}

/// When a scheduled full `VACUUM` is allowed to *start*.
///
/// **UTC, and the key says so.** Neither binary carries a timezone database
/// (`proximos-render` emits UTC everywhere), and the agent's
/// `system.timezone_offset_hours` is a display affordance, not a scheduling
/// authority — quietly reusing it would make the same YAML mean two different
/// times in the two halves. If deployments ever need local-time windows, that
/// is a decision for the whole product, not for this feature.
///
/// Absent from the config = no scheduled maintenance at all. The manual button
/// on `/settings` always works regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceWindow {
    /// Minutes past midnight UTC at which the window opens.
    pub start_minute_utc: u32,
    /// How long it stays open. This bounds when a rewrite may *start*, never
    /// when it may finish — SQLite cannot be paused, and killing a `VACUUM`
    /// wastes the work rather than saving anything.
    pub duration_minutes: u32,
}

impl MaintenanceWindow {
    /// Whether `minute_of_day_utc` falls inside the window, wrapping across
    /// midnight (a 23:30 window with a 90-minute duration runs to 01:00).
    pub fn contains(&self, minute_of_day_utc: u32) -> bool {
        let offset =
            (minute_of_day_utc + MINUTES_PER_DAY - self.start_minute_utc) % MINUTES_PER_DAY;
        offset < self.duration_minutes
    }
}

/// Minutes in a day, for the wrap arithmetic above.
const MINUTES_PER_DAY: u32 = 24 * 60;

/// Parse `"HH:MM"` into minutes past midnight.
///
/// Strict on purpose. A window that fails to parse must not silently degrade to
/// "never run maintenance" — that is the failure this whole feature exists to
/// end, and it would be invisible until `/data` filled up months later.
#[cfg(test)]
pub fn parse_hh_mm_for_tests(value: &str) -> Option<u32> {
    parse_hh_mm(value)
}

fn parse_hh_mm(value: &str) -> Option<u32> {
    let (h, m) = value.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "maintenance_window is invalid: {reason}. Refusing to start rather than \
         silently running no scheduled maintenance"
    )]
    InvalidMaintenanceWindow { reason: String },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    /// A value that parsed as the right *type* but cannot mean anything.
    /// `key` is the dotted YAML path so the message names the line to edit —
    /// B0's *Accept:* "reject nonsense values" (devices.backend.md §19, F14).
    #[error("invalid config value for `{key}` in {path}: {reason}")]
    Invalid {
        path: PathBuf,
        key: String,
        reason: String,
    },
}

/// Where a `RawConfig` came from — decides the `data_dir` default (dev
/// fallback / absent get a repo-local dir instead of the production path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// Path came from `PORTAL_CONFIG` or the rendered runtime location.
    Production,
    /// Path came from the dev-loop fallback, or no config file exists at all.
    Dev,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
struct RawConfig {
    bind_addr: Option<String>,
    mqtt_host: Option<String>,
    mqtt_port: Option<u16>,
    mqtt_username: Option<String>,
    mqtt_password: Option<String>,
    mqtt_client_id: Option<String>,
    topic_prefix: Option<String>,
    data_dir: Option<String>,
    db_key: Option<String>,
    templates_dir: Option<String>,
    static_dir: Option<String>,
    cookie_secure: Option<bool>,
    local_hostname: Option<String>,
    experimental_features_enabled: Option<bool>,
    trusted_proxies: Option<Vec<String>>,
    nodes: Option<RawNodes>,
    beacons: Option<RawBeacons>,
    signal_quality: Option<RawSignalQuality>,
    thresholds: Option<RawThresholds>,
    calibration: Option<RawCalibration>,
    maintenance_window: Option<RawMaintenanceWindow>,
}

/// `deny_unknown_fields` on the nested sections but not on [`RawConfig`]:
/// the top level has to tolerate keys this binary does not know (the
/// rendered file is shared ground and `db_key` arrives from elsewhere),
/// while a typo *inside* a section this binary owns is worth failing on —
/// silently ignoring `offline_detektor` would leave detection at its default
/// with nothing in the log to say so.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawNodes {
    offline_detector: Option<u32>,
    history_auto_requests_per_10min: Option<u32>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawBeacons {
    store_interval: Option<u32>,
    offline_detector: Option<u32>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawSignalQuality {
    excellent: Option<SignalBand>,
    good: Option<SignalBand>,
    weak: Option<SignalBand>,
}

/// `thresholds:`. One key today; a section rather than a bare `threshold_defaults` so the
/// block reads the way plan/portal/threshold_defaults.md §2 writes it.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawThresholds {
    defaults: Option<RawThresholdDefaults>,
}

/// `thresholds.defaults:`, keyed by quantity. A quantity left out keeps the shipped band; a
/// quantity given is taken as written, so a bound left out of it is not written either.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawThresholdDefaults {
    temperature: Option<DefaultBand>,
    humidity: Option<DefaultBand>,
    illuminance: Option<DefaultBand>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawCalibration {
    warning_days: Option<u32>,
    second_warning_days: Option<u32>,
}

/// The YAML shape of [`MaintenanceWindow`]. `deny_unknown_fields` because a
/// typo'd key here means the window silently keeps its default, and this is the
/// one block where "silently the default" is the bug.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct RawMaintenanceWindow {
    start_utc: Option<String>,
    duration_minutes: Option<u32>,
}

/// Loads config from the real candidate paths (env var, runtime, dev).
pub fn load() -> Result<PortalConfig, ConfigError> {
    let mut candidates = Vec::new();
    if let Ok(path) = env::var("PORTAL_CONFIG") {
        candidates.push((PathBuf::from(path), Source::Production));
    }
    candidates.push((PathBuf::from(RUNTIME_CONFIG_PATH), Source::Production));
    candidates.push((PathBuf::from(DEV_CONFIG_PATH), Source::Dev));

    load_from(&candidates)
}

pub(crate) fn load_from(candidates: &[(PathBuf, Source)]) -> Result<PortalConfig, ConfigError> {
    for (path, source) in candidates {
        if path.exists() {
            let raw = read_raw(path)?;
            let cfg = finalize(raw, *source)?;
            validate(&cfg).map_err(|(key, reason)| ConfigError::Invalid {
                path: path.clone(),
                key,
                reason,
            })?;
            tracing::info!(path = %path.display(), "loaded portal config");
            return Ok(cfg);
        }
    }

    tracing::warn!("no portal config file found at any candidate path; using built-in defaults");
    finalize(RawConfig::default(), Source::Dev)
}

/// Semantic validation, run after [`finalize`] so it sees the values the
/// process will actually use — a key left unset inherits a default that is
/// valid by construction, and only what the file supplied can be wrong.
///
/// Type-level parsing was the whole of B0's validation, which let two
/// nonsense files boot clean (§19, F14):
///
/// - `beacons.store_interval: 0` — a plausible way to write "turn it off" —
///   was coerced by `notify::offline::beacon_liveness`'s `minutes.max(1)`
///   into a **one-minute** liveness expectation, alarming the whole Beacon
///   fleet within minutes of start-up. Zero has no meaning here; "off" is
///   `offline_detector: 0`, which the detector really does honour.
/// - a non-monotonic signal-quality ladder made rungs unreachable, so the
///   derived indicator (devices.node.md §2.5) silently stopped matching the
///   file it is configured by.
///
/// Returns `(key, reason)` — the caller adds the path, so every message
/// names the exact line to edit.
fn validate(cfg: &PortalConfig) -> std::result::Result<(), (String, String)> {
    for entry in &cfg.trusted_proxies {
        if crate::auth::IpRange::parse(entry).is_none() {
            return Err((
                "trusted_proxies".to_string(),
                format!("`{entry}` is not an IP address or a CIDR range"),
            ));
        }
    }
    if cfg.beacons.store_interval == 0 {
        return Err((
            "beacons.store_interval".to_string(),
            "must be at least 1 minute; use `beacons.offline_detector: 0` to switch \
             liveness detection off"
                .to_string(),
        ));
    }

    // Zero days is not "off": the window would open on the expiry day
    // itself, when the alarm is already critical, and never warn. Nothing asks for an off switch — a device
    // without an uploaded certificate simply has no calibration alarm.
    let cal = &cfg.calibration;
    for (key, days) in [
        ("calibration.warning_days", cal.warning_days),
        ("calibration.second_warning_days", cal.second_warning_days),
    ] {
        if days == 0 {
            return Err((key.to_string(), "must be at least 1 day".to_string()));
        }
    }
    if cal.second_warning_days >= cal.warning_days {
        return Err((
            "calibration.second_warning_days".to_string(),
            format!(
                "must be smaller than calibration.warning_days ({}), or the first \
                 warning is never shown before the second replaces it",
                cal.warning_days
            ),
        ));
    }

    // Each rung must be no stricter than the one above it, on both floors —
    // otherwise the rung below can never be reached.
    let sq = &cfg.signal_quality;
    for (upper_name, upper, lower_name, lower) in [
        ("excellent", &sq.excellent, "good", &sq.good),
        ("good", &sq.good, "weak", &sq.weak),
    ] {
        if lower.rssi > upper.rssi {
            return Err((
                format!("signal_quality.{lower_name}.rssi"),
                format!(
                    "the ladder must not rise: {lower_name} rssi {} is above {upper_name} rssi {}",
                    lower.rssi, upper.rssi
                ),
            ));
        }
        if lower.snr > upper.snr {
            return Err((
                format!("signal_quality.{lower_name}.snr"),
                format!(
                    "the ladder must not rise: {lower_name} snr {} is above {upper_name} snr {}",
                    lower.snr, upper.snr
                ),
            ));
        }
    }

    for (name, band) in [
        ("temperature", &cfg.thresholds.temperature),
        ("humidity", &cfg.thresholds.humidity),
        ("illuminance", &cfg.thresholds.illuminance),
    ] {
        if band.hysteresis.is_some_and(|h| !h.is_finite() || h < 0.0) {
            return Err((
                format!("thresholds.defaults.{name}.hysteresis"),
                "the hysteresis must be a number >= 0".to_string(),
            ));
        }
        if !band.rises() {
            return Err((
                format!("thresholds.defaults.{name}"),
                "the bounds must rise: critical_low < critical_high, and \
                 critical_low <= warning_low <= warning_high <= critical_high"
                    .to_string(),
            ));
        }
    }

    Ok(())
}

fn read_raw(path: &Path) -> Result<RawConfig, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_yaml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn finalize(raw: RawConfig, source: Source) -> Result<PortalConfig, ConfigError> {
    let default_data_dir = match source {
        Source::Production => "/data/portal",
        Source::Dev => "./dev-data",
    };

    let maintenance_window = raw.maintenance_window.map(finalize_window).transpose()?;

    Ok(PortalConfig {
        // 8088, not 8080: v1's ChirpStack gRPC API binds 127.0.0.1:8080 and that
        // address is deliberately non-configurable (plan/control/radio/lorawan.md
        // §3), while both run on a server-role Hub — so this side moves
        // (plan/portal/monolith.md §9, open question 3).
        bind_addr: raw
            .bind_addr
            .unwrap_or_else(|| "127.0.0.1:8088".to_string()),
        mqtt_host: raw.mqtt_host.unwrap_or_else(|| "localhost".to_string()),
        mqtt_port: raw.mqtt_port.unwrap_or(1883),
        mqtt_username: raw.mqtt_username,
        mqtt_password: raw.mqtt_password,
        mqtt_client_id: raw
            .mqtt_client_id
            .unwrap_or_else(|| "proximos-portal".to_string()),
        topic_prefix: raw.topic_prefix.unwrap_or_else(|| "proximos".to_string()),
        data_dir: raw.data_dir.unwrap_or_else(|| default_data_dir.to_string()),
        db_key: raw.db_key,
        templates_dir: raw.templates_dir.unwrap_or_else(|| "templates".to_string()),
        static_dir: raw.static_dir.unwrap_or_else(|| "static".to_string()),
        cookie_secure: raw.cookie_secure.unwrap_or(true),
        local_hostname: raw.local_hostname,
        experimental_features_enabled: raw.experimental_features_enabled.unwrap_or(false),
        trusted_proxies: raw.trusted_proxies.unwrap_or_default(),
        nodes: finalize_nodes(raw.nodes),
        beacons: finalize_beacons(raw.beacons),
        signal_quality: finalize_signal_quality(raw.signal_quality),
        thresholds: finalize_thresholds(raw.thresholds),
        calibration: finalize_calibration(raw.calibration),
        maintenance_window,
    })
}

/// Each nested section fills its unset keys from the section default, so a
/// file that sets one key of a section still gets documented values for the
/// rest — an absent section and an empty one mean the same thing.
fn finalize_nodes(raw: Option<RawNodes>) -> NodesConfig {
    let d = NodesConfig::default();
    match raw {
        Some(raw) => NodesConfig {
            offline_detector: raw.offline_detector.unwrap_or(d.offline_detector),
            history_auto_requests_per_10min: raw
                .history_auto_requests_per_10min
                .unwrap_or(d.history_auto_requests_per_10min),
        },
        None => d,
    }
}

fn finalize_beacons(raw: Option<RawBeacons>) -> BeaconsConfig {
    let d = BeaconsConfig::default();
    match raw {
        Some(raw) => BeaconsConfig {
            store_interval: raw.store_interval.unwrap_or(d.store_interval),
            offline_detector: raw.offline_detector.unwrap_or(d.offline_detector),
        },
        None => d,
    }
}

fn finalize_signal_quality(raw: Option<RawSignalQuality>) -> SignalQualityConfig {
    let d = SignalQualityConfig::default();
    match raw {
        Some(raw) => SignalQualityConfig {
            excellent: raw.excellent.unwrap_or(d.excellent),
            good: raw.good.unwrap_or(d.good),
            weak: raw.weak.unwrap_or(d.weak),
        },
        None => d,
    }
}

fn finalize_thresholds(raw: Option<RawThresholds>) -> ThresholdDefaults {
    let d = ThresholdDefaults::default();
    match raw.and_then(|t| t.defaults) {
        Some(raw) => ThresholdDefaults {
            temperature: raw.temperature.unwrap_or(d.temperature),
            humidity: raw.humidity.unwrap_or(d.humidity),
            illuminance: raw.illuminance.unwrap_or(d.illuminance),
        },
        None => d,
    }
}

fn finalize_calibration(raw: Option<RawCalibration>) -> CalibrationConfig {
    let d = CalibrationConfig::default();
    match raw {
        Some(raw) => CalibrationConfig {
            warning_days: raw.warning_days.unwrap_or(d.warning_days),
            second_warning_days: raw.second_warning_days.unwrap_or(d.second_warning_days),
        },
        None => d,
    }
}

/// Defaults for a `maintenance_window:` block that is present but partial.
///
/// 02:30 UTC because it is the quietest hour on the ward this device is
/// deployed to; 90 minutes because it must comfortably contain a worst-case
/// rewrite (still unmeasured on the CM4's eMMC — plan/portal/sqlite-vacuum.md
/// §8). Both are per-deployment, which is why they live in YAML at all; the
/// freelist thresholds that decide *whether* to run do not.
const DEFAULT_WINDOW_START_UTC: &str = "02:30";
const DEFAULT_WINDOW_DURATION_MINUTES: u32 = 90;

fn finalize_window(raw: RawMaintenanceWindow) -> Result<MaintenanceWindow, ConfigError> {
    let start = raw
        .start_utc
        .unwrap_or_else(|| DEFAULT_WINDOW_START_UTC.to_string());
    let start_minute_utc =
        parse_hh_mm(&start).ok_or_else(|| ConfigError::InvalidMaintenanceWindow {
            reason: format!("start_utc {start:?} is not a 24-hour HH:MM time"),
        })?;

    let duration_minutes = raw
        .duration_minutes
        .unwrap_or(DEFAULT_WINDOW_DURATION_MINUTES);
    if duration_minutes == 0 || duration_minutes > MINUTES_PER_DAY {
        return Err(ConfigError::InvalidMaintenanceWindow {
            reason: format!(
                "duration_minutes must be between 1 and {MINUTES_PER_DAY}, got {duration_minutes}"
            ),
        });
    }

    Ok(MaintenanceWindow {
        start_minute_utc,
        duration_minutes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "portal-config-test-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str, contents: &str) -> Self {
            let path = temp_path(name);
            std::fs::write(&path, contents).expect("write temp config file");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn defaults_when_no_candidate_exists() {
        let missing = PathBuf::from("/definitely/does/not/exist/portal-config.yaml");
        let config = load_from(&[(missing, Source::Production)]).expect("load defaults");

        assert_eq!(config.bind_addr, "127.0.0.1:8088");
        assert_eq!(config.mqtt_host, "localhost");
        assert_eq!(config.mqtt_port, 1883);
        assert_eq!(config.mqtt_username, None);
        assert_eq!(config.mqtt_password, None);
        assert_eq!(config.mqtt_client_id, "proximos-portal");
        assert_eq!(config.topic_prefix, "proximos");
        assert_eq!(config.data_dir, "./dev-data");
        assert_eq!(config.templates_dir, "templates");
        assert_eq!(config.static_dir, "static");
        assert!(config.cookie_secure);
        assert_eq!(config.local_hostname, None);
        assert!(!config.experimental_features_enabled);
        assert_eq!(config.nodes, NodesConfig::default());
        assert_eq!(config.beacons, BeaconsConfig::default());
        assert_eq!(config.signal_quality, SignalQualityConfig::default());
        assert_eq!(
            config.calibration,
            CalibrationConfig {
                warning_days: 30,
                second_warning_days: 7,
            }
        );
        assert_eq!(config.thresholds, ThresholdDefaults::default());
    }

    #[test]
    fn empty_file_yields_defaults_with_production_data_dir() {
        let file = TempFile::new("empty", "");
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load empty file");

        assert_eq!(config.bind_addr, "127.0.0.1:8088");
        assert_eq!(config.data_dir, "/data/portal");
    }

    #[test]
    fn dev_fallback_defaults_data_dir_to_repo_local() {
        let file = TempFile::new("dev", "bind_addr: \"0.0.0.0:9090\"\n");
        let config = load_from(&[(file.0.clone(), Source::Dev)]).expect("load dev file");

        assert_eq!(config.bind_addr, "0.0.0.0:9090");
        assert_eq!(config.data_dir, "./dev-data");
    }

    /// The demo's config parses, including its trusted proxy range (R2-4).
    #[test]
    fn the_demo_config_loads() {
        let config = load_from(&[(
            std::path::PathBuf::from("packaging/demo/config.yaml"),
            Source::Production,
        )])
        .expect("demo config");
        assert_eq!(config.trusted_proxies, ["172.16.0.0/12"]);
    }

    #[test]
    fn a_trusted_proxy_that_is_not_an_address_is_refused() {
        let file = TempFile::new("proxies", "trusted_proxies: [\"caddy\"]\n");
        let err = load_from(&[(file.0.clone(), Source::Production)]).unwrap_err();
        assert!(
            matches!(&err, ConfigError::Invalid { key, .. } if key == "trusted_proxies"),
            "{err:?}"
        );
    }

    #[test]
    fn parses_full_overrides() {
        let yaml = r#"
bind_addr: "0.0.0.0:8081"
mqtt_host: "mqtt.local"
mqtt_port: 8883
mqtt_username: "hub"
mqtt_password: "secret"
topic_prefix: "custom"
data_dir: "/srv/portal"
templates_dir: "/opt/portal/templates"
static_dir: "/opt/portal/static"
cookie_secure: false
local_hostname: "FIBER-MOCK-001"
experimental_features_enabled: true
trusted_proxies: ["172.16.0.0/12"]
nodes:
  offline_detector: 6
beacons:
  store_interval: 30
  offline_detector: 2
signal_quality:
  excellent: { rssi: -70, snr: 9 }
  good: { rssi: -90, snr: 1 }
  weak: { rssi: -105, snr: -5 }
calibration:
  warning_days: 45
  second_warning_days: 10
"#;
        let file = TempFile::new("full", yaml);
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load full file");

        assert_eq!(
            config,
            PortalConfig {
                bind_addr: "0.0.0.0:8081".to_string(),
                mqtt_host: "mqtt.local".to_string(),
                mqtt_port: 8883,
                mqtt_username: Some("hub".to_string()),
                mqtt_password: Some("secret".to_string()),
                mqtt_client_id: "proximos-portal".to_string(),
                topic_prefix: "custom".to_string(),
                data_dir: "/srv/portal".to_string(),
                db_key: None,
                templates_dir: "/opt/portal/templates".to_string(),
                static_dir: "/opt/portal/static".to_string(),
                cookie_secure: false,
                local_hostname: Some("FIBER-MOCK-001".to_string()),
                experimental_features_enabled: true,
                trusted_proxies: vec!["172.16.0.0/12".to_string()],
                nodes: NodesConfig {
                    offline_detector: 6,
                    ..NodesConfig::default()
                },
                beacons: BeaconsConfig {
                    store_interval: 30,
                    offline_detector: 2,
                },
                signal_quality: SignalQualityConfig {
                    excellent: SignalBand {
                        rssi: -70,
                        snr: 9.0
                    },
                    good: SignalBand {
                        rssi: -90,
                        snr: 1.0
                    },
                    weak: SignalBand {
                        rssi: -105,
                        snr: -5.0
                    },
                },
                thresholds: ThresholdDefaults::default(),
                calibration: CalibrationConfig {
                    warning_days: 45,
                    second_warning_days: 10,
                },
                maintenance_window: None,
            }
        );
    }

    /// threshold_defaults.md §2: a quantity given is taken as written — a bound left out
    /// (or `null`, how an overlay removes one) is not written — and a quantity left out
    /// keeps the shipped band.
    #[test]
    fn threshold_defaults_are_read_per_quantity() {
        let yaml = "thresholds:\n  defaults:\n    temperature: { critical_low: -20, \
                    warning_low: -5, warning_high: null, critical_high: 30 }\n";
        let file = TempFile::new("thr-defaults", yaml);
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
        assert_eq!(
            config.thresholds.temperature,
            DefaultBand {
                critical_low: Some(-20.0),
                warning_low: Some(-5.0),
                warning_high: None,
                critical_high: Some(30.0),
                hysteresis: None,
            }
        );
        assert_eq!(
            config.thresholds.humidity,
            ThresholdDefaults::default().humidity
        );
        // No hysteresis given: the shared default of the quantity.
        assert_eq!(
            config
                .thresholds
                .hysteresis(fiber_protocol::Quantity::Temperature),
            0.5
        );
        assert_eq!(
            config
                .thresholds
                .hysteresis(fiber_protocol::Quantity::Humidity),
            3.0
        );
    }

    /// #165: illuminance has a shipped band, 0–10 000 lx, and the YAML overrides it.
    #[test]
    fn the_illuminance_default_is_shipped_and_overridable() {
        let d = ThresholdDefaults::default();
        assert_eq!(
            (d.illuminance.critical_low, d.illuminance.critical_high),
            (Some(0.0), Some(10_000.0))
        );
        let yaml = "thresholds:\n  defaults:\n    illuminance: { critical_low: 50, \
                    critical_high: 2000 }\n";
        let file = TempFile::new("thr-lux", yaml);
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
        assert_eq!(config.thresholds.illuminance.critical_high, Some(2000.0));
        assert_eq!(
            config
                .thresholds
                .hysteresis(fiber_protocol::Quantity::Illuminance),
            10.0
        );
        let yaml = "thresholds:\n  defaults:\n    illuminance: { critical_low: 2000, \
                    critical_high: 50 }\n";
        let file = TempFile::new("thr-lux-bad", yaml);
        assert!(load_from(&[(file.0.clone(), Source::Production)]).is_err());
    }

    /// decision #41: `thresholds.defaults.<quantity>.hysteresis` sets the default, and a
    /// negative one is refused by name.
    #[test]
    fn a_default_hysteresis_is_read_and_checked() {
        let yaml = "thresholds:\n  defaults:\n    humidity: { critical_low: 0, warning_low: 10, \
                    warning_high: 80, critical_high: 90, hysteresis: 5 }\n";
        let file = TempFile::new("thr-hyst", yaml);
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
        assert_eq!(
            config
                .thresholds
                .hysteresis(fiber_protocol::Quantity::Humidity),
            5.0
        );
        let yaml = "thresholds:\n  defaults:\n    temperature: { critical_low: -10, \
                    critical_high: 50, hysteresis: -1 }\n";
        let file = TempFile::new("thr-hyst-neg", yaml);
        match load_from(&[(file.0.clone(), Source::Production)]).unwrap_err() {
            ConfigError::Invalid { key, .. } => {
                assert_eq!(key, "thresholds.defaults.temperature.hysteresis")
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// A default band whose bounds do not rise is refused by name, and a typo inside the
    /// block is a parse error like in every section this binary owns.
    #[test]
    fn a_default_band_out_of_order_or_misspelled_is_refused() {
        let yaml =
            "thresholds:\n  defaults:\n    humidity: { critical_low: 90, critical_high: 10 }\n";
        let file = TempFile::new("thr-order", yaml);
        match load_from(&[(file.0.clone(), Source::Production)]).unwrap_err() {
            ConfigError::Invalid { key, .. } => assert_eq!(key, "thresholds.defaults.humidity"),
            other => panic!("expected Invalid, got {other:?}"),
        }
        for typo in [
            "thresholds:\n  defaults:\n    temprature: { critical_low: 1 }\n",
            "thresholds:\n  defaults:\n    humidity: { critical_lo: 1 }\n",
            "thresholds:\n  default: {}\n",
        ] {
            let file = TempFile::new("thr-typo", typo);
            assert!(
                matches!(
                    load_from(&[(file.0.clone(), Source::Production)]),
                    Err(ConfigError::Parse { .. })
                ),
                "{typo:?}"
            );
        }
    }

    /// threshold_defaults.md §2, §6: the portal's default bands and the Hub's are one set of
    /// numbers. `defaults.yaml`'s temperature band is the Hub's `common_alarms` for its wired
    /// lines, and both bands are its legacy `common_lorawan_field_thresholds`.
    #[test]
    fn the_portal_and_the_hub_ship_the_same_default_bands() {
        let portal: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../packaging/templates/defaults.yaml"))
                .expect("defaults.yaml parses");
        let hub: serde_yaml::Value = serde_yaml::from_str(include_str!(
            "../crates/control-core/fiber.sensors.config.yaml"
        ))
        .expect("fiber.sensors.config.yaml parses");
        let bounds = [
            "critical_low",
            "warning_low",
            "warning_high",
            "critical_high",
        ];
        let band = |v: &serde_yaml::Value, suffix: &str| -> Vec<Option<f64>> {
            bounds
                .iter()
                .map(|b| v[format!("{b}{suffix}").as_str()].as_f64())
                .collect()
        };
        let defaults = &portal["portal"]["thresholds"]["defaults"];
        let temperature = band(&defaults["temperature"], "");
        let humidity = band(&defaults["humidity"], "");
        assert!(temperature.iter().chain(&humidity).all(Option::is_some));

        assert_eq!(band(&hub["common_alarms"], "_celsius"), temperature);
        let lorawan = &hub["common_lorawan_field_thresholds"];
        for (field, want) in [
            ("temperature", &temperature),
            ("ext_temperature_1", &temperature),
            ("machine_probe_temperature_2", &temperature),
            ("humidity", &humidity),
            ("machine_probe_humidity_1", &humidity),
        ] {
            assert_eq!(&band(&lorawan[field], ""), want, "{field}");
        }
    }

    /// threshold_defaults.md §6: the block in `defaults.yaml` parses and is the built-in
    /// band, so a Hub rendered from the image and a dev portal with no config agree.
    #[test]
    fn the_shipped_defaults_yaml_carries_the_built_in_bands() {
        let doc: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../packaging/templates/defaults.yaml"))
                .expect("defaults.yaml parses");
        let block = serde_yaml::to_string(&doc["portal"]["thresholds"]).expect("serialize");
        let raw: RawThresholds = serde_yaml::from_str(&block).expect("the block parses");
        assert_eq!(finalize_thresholds(Some(raw)), ThresholdDefaults::default());
    }

    /// A section that sets one key keeps documented defaults for the rest —
    /// the property `finalize_*`'s two-level match exists for.
    #[test]
    fn a_partial_section_fills_the_rest_from_defaults() {
        let file = TempFile::new("partial", "beacons:\n  store_interval: 30\n");
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");

        assert_eq!(config.beacons.store_interval, 30);
        assert_eq!(
            config.beacons.offline_detector,
            BeaconsConfig::default().offline_detector
        );
        // An untouched sibling section is untouched.
        assert_eq!(config.nodes, NodesConfig::default());
    }

    /// `0` is a real value — detection off — and must survive `unwrap_or`,
    /// which is exactly the bug an `unwrap_or_default()` on a `u32` would
    /// introduce.
    #[test]
    fn zero_disables_rather_than_falling_back_to_the_default() {
        let file = TempFile::new(
            "zero",
            "nodes:\n  offline_detector: 0\nbeacons:\n  offline_detector: 0\n",
        );
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");

        assert_eq!(config.nodes.offline_detector, 0);
        assert_eq!(config.beacons.offline_detector, 0);
    }

    /// devices.node.md §3.9, amendment 2026-09-25, item 6: the per-Hub limit on automatic
    /// history requests is set in the file, 6 by default, and 0 is a real value.
    #[test]
    fn the_automatic_history_request_limit_is_read_from_the_file() {
        assert_eq!(NodesConfig::default().history_auto_requests_per_10min, 6);
        for (yaml, want) in [
            ("nodes:\n  history_auto_requests_per_10min: 2\n", 2),
            ("nodes:\n  history_auto_requests_per_10min: 0\n", 0),
            ("nodes:\n  offline_detector: 3\n", 6),
        ] {
            let file = TempFile::new("limit", yaml);
            let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
            assert_eq!(config.nodes.history_auto_requests_per_10min, want, "{yaml}");
        }
    }

    /// The nested sections are `deny_unknown_fields`, unlike the top level:
    /// a typo in a key this binary owns is a startup failure, not a silent
    /// default.
    #[test]
    fn an_unknown_key_inside_a_known_section_is_an_error() {
        let file = TempFile::new("typo", "nodes:\n  offline_detektor: 4\n");
        let err = load_from(&[(file.0.clone(), Source::Production)]).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    /// ...while an unknown key at the TOP level still passes, because the
    /// rendered file is shared ground (`db_key` and friends).
    #[test]
    fn an_unknown_top_level_key_is_ignored() {
        let file = TempFile::new("extra", "some_future_key: 1\nmqtt_port: 8885\n");
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");

        assert_eq!(config.mqtt_port, 8885);
    }

    #[test]
    fn skips_missing_candidates_before_a_hit() {
        let missing = PathBuf::from("/definitely/does/not/exist/portal-config.yaml");
        let file = TempFile::new("second", "mqtt_port: 8884\n");
        let config = load_from(&[
            (missing, Source::Production),
            (file.0.clone(), Source::Production),
        ])
        .expect("load with fallthrough");

        assert_eq!(config.mqtt_port, 8884);
    }

    #[test]
    fn local_hostname_is_absent_unless_configured() {
        // The production shape: no override, so `http::app` falls back to
        // `gethostname(2)`.
        let file = TempFile::new("no-local-host", "bind_addr: \"0.0.0.0:9090\"\n");
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");

        assert_eq!(config.local_hostname, None);
    }

    #[test]
    fn invalid_yaml_is_an_error() {
        let file = TempFile::new("invalid", "mqtt_port: [this is not a port]\n");
        let err = load_from(&[(file.0.clone(), Source::Production)]).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    // ---- §19 (F14): semantic validation, not just types ------------------

    /// `beacons.store_interval: 0` — a plausible way to write "turn it off"
    /// — parsed fine as a `u32` and was then coerced by `beacon_liveness`'s
    /// `minutes.max(1)` into a **one-minute** liveness expectation, alarming
    /// the whole Beacon fleet within minutes of start-up.
    #[test]
    fn a_zero_store_interval_is_refused_by_name() {
        let file = TempFile::new("zero-interval", "beacons:\n  store_interval: 0\n");
        let err = load_from(&[(file.0.clone(), Source::Production)]).unwrap_err();
        match err {
            ConfigError::Invalid { key, reason, .. } => {
                assert_eq!(key, "beacons.store_interval");
                assert!(
                    reason.contains("offline_detector"),
                    "the message points at the key that really does switch it off: {reason}"
                );
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// A ladder whose lower rung sits above the one over it makes that rung
    /// unreachable, so the derived signal indicator silently stops matching
    /// the file it is configured by. Checked on both floors.
    #[test]
    fn a_signal_ladder_that_rises_is_refused_by_name() {
        for (yaml, want_key) in [
            (
                "signal_quality:\n  good:\n    rssi: -50\n    snr: 0.0\n",
                "signal_quality.good.rssi",
            ),
            (
                "signal_quality:\n  weak:\n    rssi: -110\n    snr: 3.0\n",
                "signal_quality.weak.snr",
            ),
        ] {
            let file = TempFile::new("ladder", yaml);
            let err = load_from(&[(file.0.clone(), Source::Production)]).unwrap_err();
            match err {
                ConfigError::Invalid { key, .. } => assert_eq!(key, want_key),
                other => panic!("expected Invalid for {yaml:?}, got {other:?}"),
            }
        }
    }

    /// The shipped defaults, and a file that sets only unrelated keys, must
    /// still load — validation that rejects its own defaults is worse than
    /// none.
    #[test]
    fn the_shipped_defaults_and_an_ordinary_file_still_validate() {
        assert!(
            validate(&finalize(RawConfig::default(), Source::Dev).expect("defaults finalize"))
                .is_ok()
        );

        let file = TempFile::new(
            "ordinary",
            "beacons:\n  store_interval: 15\n  offline_detector: 0\n",
        );
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("loads");
        assert_eq!(config.beacons.store_interval, 15);
        assert_eq!(
            config.beacons.offline_detector, 0,
            "0 here is the documented way to switch detection off, and stays legal"
        );
    }

    /// calibration.cs.md §3: both windows come out of the file, and a block
    /// that names one key keeps the documented default for the other.
    #[test]
    fn the_calibration_windows_are_read_from_the_file() {
        let file = TempFile::new(
            "calibration",
            "calibration:\n  warning_days: 60\n  second_warning_days: 14\n",
        );
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
        assert_eq!(
            config.calibration,
            CalibrationConfig {
                warning_days: 60,
                second_warning_days: 14,
            }
        );

        let file = TempFile::new("calibration-partial", "calibration:\n  warning_days: 40\n");
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
        assert_eq!(config.calibration.warning_days, 40);
        assert_eq!(
            config.calibration.second_warning_days,
            CalibrationConfig::default().second_warning_days
        );
    }

    /// A second-warning window that does not sit inside the first one makes
    /// the first warning unreachable, and a zero-day window is not an off switch —
    /// both are refused by name rather than started with.
    #[test]
    fn a_calibration_window_that_cannot_warn_is_refused_by_name() {
        for (yaml, want_key) in [
            (
                "calibration:\n  warning_days: 7\n  second_warning_days: 7\n",
                "calibration.second_warning_days",
            ),
            (
                "calibration:\n  warning_days: 10\n  second_warning_days: 20\n",
                "calibration.second_warning_days",
            ),
            (
                "calibration:\n  warning_days: 0\n",
                "calibration.warning_days",
            ),
            (
                "calibration:\n  second_warning_days: 0\n",
                "calibration.second_warning_days",
            ),
        ] {
            let file = TempFile::new("calibration-bad", yaml);
            let err = load_from(&[(file.0.clone(), Source::Production)]).unwrap_err();
            match err {
                ConfigError::Invalid { key, .. } => assert_eq!(key, want_key, "{yaml:?}"),
                other => panic!("expected Invalid for {yaml:?}, got {other:?}"),
            }
        }

        // A typo inside the section is a parse failure, like every other
        // section this binary owns.
        let file = TempFile::new("calibration-typo", "calibration:\n  warn_days: 30\n");
        assert!(matches!(
            load_from(&[(file.0.clone(), Source::Production)]),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn an_absent_window_means_no_scheduled_maintenance() {
        let file = TempFile::new("no-window", "bind_addr: \"0.0.0.0:1\"\n");
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
        assert_eq!(
            config.maintenance_window, None,
            "the recorded 2026-08-24 behaviour stays the default: rewrites only when asked for"
        );
    }

    #[test]
    fn a_window_is_read_as_minutes_past_midnight_utc() {
        let yaml = "maintenance_window:\n  start_utc: \"02:30\"\n  duration_minutes: 90\n";
        let file = TempFile::new("window", yaml);
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
        assert_eq!(
            config.maintenance_window,
            Some(MaintenanceWindow {
                start_minute_utc: 150,
                duration_minutes: 90,
            })
        );
    }

    #[test]
    fn a_partial_window_block_takes_the_documented_defaults() {
        let file = TempFile::new("partial", "maintenance_window:\n  duration_minutes: 30\n");
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load");
        assert_eq!(
            config.maintenance_window,
            Some(MaintenanceWindow {
                start_minute_utc: 150,
                duration_minutes: 30,
            })
        );
    }

    /// A misparsed window must not silently mean "never run maintenance" — that
    /// is invisible until `/data` fills up months later, which is the failure
    /// this whole feature exists to end.
    #[test]
    fn a_malformed_window_refuses_to_start_rather_than_quietly_doing_nothing() {
        for bad in ["25:00", "02:60", "0230", "half past two", ""] {
            let yaml = format!("maintenance_window:\n  start_utc: \"{bad}\"\n");
            let file = TempFile::new("bad-window", &yaml);
            let err = load_from(&[(file.0.clone(), Source::Production)])
                .expect_err(&format!("{bad:?} must be refused"));
            assert!(
                matches!(err, ConfigError::InvalidMaintenanceWindow { .. }),
                "{bad:?} gave {err:?}"
            );
        }
    }

    #[test]
    fn a_zero_length_window_is_refused_because_it_can_never_open() {
        let yaml = "maintenance_window:\n  start_utc: \"02:30\"\n  duration_minutes: 0\n";
        let file = TempFile::new("zero-window", yaml);
        assert!(matches!(
            load_from(&[(file.0.clone(), Source::Production)]),
            Err(ConfigError::InvalidMaintenanceWindow { .. })
        ));
    }

    /// A typo'd key inside the block would otherwise leave the window at its
    /// default with no complaint — the one place "silently the default" is the
    /// bug rather than the convenience.
    #[test]
    fn an_unknown_key_inside_the_window_block_is_refused() {
        let yaml = "maintenance_window:\n  start_time: \"02:30\"\n";
        let file = TempFile::new("typo-window", yaml);
        assert!(matches!(
            load_from(&[(file.0.clone(), Source::Production)]),
            Err(ConfigError::Parse { .. })
        ));
    }

    /// The renderer writes this file; this crate reads it. Nothing else checks
    /// that the two agree.
    ///
    /// `proximos-render` cannot depend on the portal binary (and must not — it
    /// runs on the boot path), so its golden output is pulled in here instead,
    /// the same way `control-core`'s `tests/shipped_config.rs` pins the shipped
    /// `fiber.config.yaml`. A key added on one side and misspelled on the other
    /// would otherwise surface as a silently-defaulted value on a Hub.
    const RENDERED: &str =
        include_str!("../crates/proximos-render/tests/golden/lorawan/portal-config.yaml");

    #[test]
    fn the_rendered_config_loads_with_every_key_this_crate_expects() {
        let file = TempFile::new("rendered", RENDERED);
        let config = load_from(&[(file.0.clone(), Source::Production)]).expect("load rendered");

        assert_eq!(config.bind_addr, "127.0.0.1:8088");
        assert_eq!(config.data_dir, "/data/portal");
        assert!(config.cookie_secure);
        assert_eq!(
            config.maintenance_window,
            Some(MaintenanceWindow {
                start_minute_utc: 150,
                duration_minutes: 90,
            }),
            "every rendered Hub schedules maintenance; defaults.yaml is what turns it on"
        );
        assert_eq!(
            config.thresholds,
            ThresholdDefaults::default(),
            "every rendered Hub carries the default bands, as defaults.yaml sets them"
        );
    }
}
