//! Pure functions turning `StateStore` models into template contexts.
//!
//! Each function here targets exactly one template contract documented in
//! `templates/pages/dashboard.html` / `templates/partials/*.html` (those
//! doc-comments are binding — see plan/viewer/design.md §4). This module
//! does no I/O and holds no state: given a `&PortalState`/`&DeviceState`/
//! `&SensorState`/`&MqttConnectionState`, it returns the matching context.
//!
//! The builders are split by the surface they render, one submodule each,
//! and every one of them is re-exported here: `crate::render::X` names the
//! same items it always did, wherever X now lives.
//!
//! - [`hub`] — the Hub (FIBER) surfaces: wired sensor cards, the hub list
//!   row, the detail page's radio/power/network/system cards, unit display
//!   config.
//! - [`monitoring`] — the monitoring page (`pages/dashboard.html`).
//! - [`stats`] — `partials/page_stats.html`: the connection pill and the
//!   per-page metric tiles.
//! - [`node`] — Node (LoRaWAN STICKER) cards, the detail page, thresholds
//!   and the pending-configuration draft.
//! - [`beacon`] — Beacon (BLE EYE) cards, the detail page and the
//!   passive-scan panel.
//! - [`alarm`] — the alarm list/detail contexts and their duration
//!   formatting.
//!
//! What stays here is what every surface shares: the template
//! [`Environment`] wrapper, the filters it registers, the time helpers and
//! the two contexts no single page owns (`page_ctx`, `CommandOutcomeCtx`,
//! `DeviceMetaCtx`).
//!
//! **Normalization note (store vs. template casing).** `SensorState`
//! carries `dominant_alarm_state` verbatim from the wire — firmware-cased
//! (`"NORMAL"`, `"WARNING"`, `"CRITICAL"`, `"DISCONNECTED"`,
//! `"RECONNECTING"`, `"NEVERCONNECTED"`; see `state::models` doc-comment).
//! The templates match mixed-case (`"Normal"`, `"Warning"`, ...). This is
//! the mapping boundary — `hub::normalize_alarm_state` does it, plus a
//! fallback the store doesn't encode: a pre-created line that has never
//! actually received a payload defaults to `"NORMAL"` in the store (see
//! `SensorState` doc-comment), but the dashboard should show
//! `"NeverConnected"` for it, not `"Normal"`.
//!
//! **Timestamp note.** Every displayed instant goes through [`short_dt`]
//! (or [`format_time`] for a bare unix-seconds clock) and is rendered in the
//! installation's display zone **with the zone written out**
//! ([`crate::display_tz`], plan/decisions.md #25). Contexts pass instants raw
//! (RFC 3339 or SQLite's `datetime('now')`, both UTC); templates format them.

use std::time::{SystemTime, UNIX_EPOCH};

use minijinja::value::Value;
use std::sync::Arc;

use minijinja::{context, path_loader, Environment, State};
use serde::Serialize;
use sha2::{Digest, Sha256};

use std::collections::{BTreeMap, HashMap};

use portal_domain::{EffectiveThreshold, ThresholdBounds};
use portal_storage::FieldThreshold;

use crate::auth::{CurrentUser, Role};
use crate::keys::DeviceIdentityInfo;
use crate::relay::CommandOutcome;
use crate::state::{
    BeaconState, DeviceConfigState, DeviceState, MqttConnectionState, NodeConfigEntry,
    NodeDeviceInfoEntry, NodeFieldThreshold, NodeGatewayLink, NodeState, PortalState,
    RadioGatewaysState, SensorState, SensorThresholds,
};

mod alarm;
mod beacon;
mod calibration;
mod device_info;
mod diagnostics;
mod hub;
mod monitoring;
mod node;
mod stats;

pub use alarm::*;
pub use beacon::*;
pub use calibration::*;
pub use device_info::*;
pub use diagnostics::*;
pub use hub::*;
pub use monitoring::*;
pub use node::*;
pub use stats::*;

/// How a person recorded by username (an alarm acknowledgement, a note, an
/// audit row) is shown to a reader: `"Jan Novák (admin)"` when the user has a
/// full name, else the bare username. The stored username stays the audit
/// identity and stays visible, not tucked into a hover tooltip: the audit
/// page filters by it, a touch screen has no hover, and two people may share
/// a full name. A deleted user or a system actor (`system:backfill`) has no
/// entry in `full_names` and shows exactly as stored.
pub fn person_label(username: &str, full_names: &HashMap<String, String>) -> String {
    match full_names.get(username) {
        Some(name) => format!("{name} ({username})"),
        None => username.to_string(),
    }
}

/// `partials/command_status.html`'s `cmd` context, built from a
/// [`CommandOutcome`] — that struct's own doc comment notes it's
/// deliberately shaped to match this template modulo `ts`, which the route
/// layer (`routes::render_command_outcome`) stamps here at render time.
/// Same shape as `sse.rs`'s private `CommandStatusCtx` — that one renders
/// live `CommandStatus` transitions pushed over SSE, this one renders the
/// synchronous outcome of the direct htmx POST that kicked a command off.
#[derive(Debug, Clone, Serialize)]
pub struct CommandOutcomeCtx {
    pub request_id: String,
    pub command_type: String,
    pub status: &'static str,
    pub message: Option<String>,
    /// `monitoring.command_error.<slug>` names the sentence the template
    /// leads with; `message` then renders as the technical detail under it.
    pub error_code: Option<&'static str>,
    /// The device's name, the banner's first line (GitLab #121). `None` from `From`; the
    /// route layer (`routes::command::Cmd::done`) fills it.
    pub target: Option<String>,
    pub ts: String,
}

impl From<CommandOutcome> for CommandOutcomeCtx {
    fn from(outcome: CommandOutcome) -> Self {
        CommandOutcomeCtx {
            // `None` only for the no-request-ever-sent error paths
            // (`CommandOutcome`'s own doc comment) — the template has no
            // special-case for it, an empty string renders harmlessly.
            request_id: outcome.request_id.unwrap_or_default(),
            command_type: outcome.command_type,
            status: outcome.status,
            message: outcome.message,
            error_code: outcome.error_code,
            target: None,
            ts: format_time(now_unix()),
        }
    }
}

/// Unix-seconds "now" for stamping a freshly rendered [`CommandOutcomeCtx`].
/// Duplicated rather than shared with `relay.rs`'s identical helper — same
/// small-helper-per-module convention that function's own module already
/// follows (`sse.rs` duplicates `format_time` for the same reason).
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Renders a unix-seconds timestamp as `"HH:MM:SS CEST"` in the display
/// zone — see module doc.
pub fn format_time(unix_secs: i64) -> String {
    format_time_in(unix_secs, &crate::display_tz::current())
}

fn format_time_in(unix_secs: i64, tz: &jiff::tz::TimeZone) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(unix_secs) else {
        return String::new();
    };
    let z = ts.to_zoned(tz.clone());
    format!(
        "{:02}:{:02}:{:02} {}",
        z.hour(),
        z.minute(),
        z.second(),
        z.strftime("%Z")
    )
}

/// Beacon `last_seen` is device-reported unix seconds (unlike a Node's,
/// kept verbatim as an RFC 3339 string from the payload — see
/// `BeaconState::last_seen`'s doc comment) — this turns it into the same
/// RFC 3339 shape so `partials/beacon_card.html` can render it through
/// `short_dt` too, same as Nodes, instead of the bare "HH:MM:SS" P1 used
/// (which loses the date and drifted from legacy's shared Last Activity
/// column format). `None` on a range error (should not happen for a real
/// device timestamp) degrades to no value, same as a missing `last_seen`.
pub fn unix_to_rfc3339(unix_secs: i64) -> Option<String> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

/// The template environment every render path uses (`AppState.templates`,
/// `sse::run`). Release builds hold one cached [`Environment`]. Debug builds
/// rebuild it on every [`Templates::env`] call, so an edited template shows
/// up on the next request without restarting the server (minijinja parses
/// lazily, so this costs parsing the handful of templates one page touches).
///
/// It also carries `asset_v`, the cache-busting stamp the templates append
/// to every script and stylesheet URL — see [`Templates::asset_version`].
#[derive(Clone)]
pub struct Templates {
    dir: Arc<str>,
    static_dir: Arc<str>,
    cached: Arc<Environment<'static>>,
    cached_asset_v: Arc<str>,
}

impl Templates {
    pub fn new(dir: &str, static_dir: &str) -> Self {
        let asset_v = Self::asset_version(static_dir);
        Templates {
            dir: dir.into(),
            static_dir: static_dir.into(),
            cached: Arc::new(Self::build(dir, &asset_v)),
            cached_asset_v: asset_v.into(),
        }
    }

    pub fn env(&self) -> Arc<Environment<'static>> {
        if cfg!(debug_assertions) {
            Arc::new(Self::build(&self.dir, &self.asset_v()))
        } else {
            Arc::clone(&self.cached)
        }
    }

    /// The `asset_v` stamp the templates emit right now: the startup hash in
    /// release, a fresh one in debug (as [`Self::env`]). `?v=` with any
    /// other value names another build's file, which
    /// `http::static_cache_control` does not let a browser keep.
    pub fn asset_v(&self) -> String {
        if cfg!(debug_assertions) {
            Self::asset_version(&self.static_dir)
        } else {
            self.cached_asset_v.to_string()
        }
    }

    /// Stamp for every `/static/{css,js}/…?v=…` URL the templates emit: a
    /// hash of the contents of `static/css/` and `static/js/`.
    ///
    /// Without it a browser keeps running the previous build's scripts and
    /// stylesheet after an upgrade (UX-030: an 11-day-old `portal-charts.js`
    /// drew UTC axes against a server that had moved to the display zone).
    /// A new stamp is a new URL, so the first page after an upgrade fetches
    /// everything fresh; `http::static_cache_control` lets the browser keep
    /// a versioned URL for good in exchange.
    ///
    /// Contents rather than mtimes: the Yocto recipe copies `static/` as-is
    /// and reproducible builds clamp mtimes to one epoch, so two images can
    /// ship different files with identical timestamps. Hashing ~1.5 MB costs
    /// a few milliseconds, once at startup in release and per render in
    /// debug (where the CSS is rebuilt without a restart), the trade-off
    /// `env` already makes for templates. A file that cannot be read is
    /// skipped, which only costs the busting, not the page.
    fn asset_version(static_dir: &str) -> String {
        use std::hash::{Hash, Hasher};
        let root = std::path::Path::new(static_dir);
        let mut files: Vec<std::path::PathBuf> = ["css", "js"]
            .iter()
            .filter_map(|sub| std::fs::read_dir(root.join(sub)).ok())
            .flatten()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for path in &files {
            if let Ok(bytes) = std::fs::read(path) {
                path.file_name().hash(&mut hasher);
                bytes.hash(&mut hasher);
            }
        }
        // The logo is drawn on every page under the same stamp, so a new
        // logo must be a new URL too. It is the brand's, compiled in and
        // served by `http::brand_asset` (plan/portal/branding.md §3), and
        // hashed exactly as the file `static/logo.svg` was — last, since
        // it sorted after css/ and js/ — so the stamp did not move with it.
        Some(std::ffi::OsStr::new("logo.svg")).hash(&mut hasher);
        proximos_brand::LOGO_SVG.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    /// The portal's own version and its copyright line, for the sidebar foot
    /// (`layout/nav.html`). Globals rather than page context because every
    /// authenticated page renders them and none of them has an opinion.
    ///
    /// The year is read once, at startup. A Hub runs for months, so a process
    /// alive across New Year prints last year until it restarts — which it
    /// will, on the next update. Reading the clock per render to fix that
    /// would cost a syscall on every page for a character.
    fn build(dir: &str, asset_v: &str) -> Environment<'static> {
        let mut env = Environment::new();
        env.set_loader(path_loader(dir));
        env.add_global("asset_v", asset_v.to_string());
        env.add_global("app_version", crate::version());
        env.add_global(
            "build_year",
            time::OffsetDateTime::now_utc().year().to_string(),
        );
        crate::i18n::register(&mut env);
        register(&mut env);
        env
    }
}

/// Registers render.rs's own template filters. Every place that builds a
/// template `Environment` must call this alongside `i18n::register`
/// (`http::app` and `routes::test_support::test_state`) or `short_dt`
/// fails templates with an "unknown filter" error.
pub fn register(env: &mut Environment<'_>) {
    env.add_global("brand", brand());
    env.add_filter("short_dt", short_dt);
    // The display zone's IANA name, for `<body data-tz>` — the chart script
    // formats its labels in it (decisions.md #25).
    env.add_function("display_tz", crate::display_tz::name);
    env.add_filter("node_serial", node_serial);
    env.add_function(
        "calibration_second_warning_days",
        crate::calibration::label_second_warning_days,
    );
}

/// The `brand` template global: the build's brand.toml (plan/portal/branding.md §4). With the
/// default brand every field is the literal the templates carried before they read it here.
pub fn brand() -> Value {
    use proximos_brand as b;
    context! {
        id => b::ID,
        product => b::PRODUCT,
        hub => b::HUB,
        node => b::NODE,
        beacon => b::BEACON,
        monitor => b::MONITOR,
        mobile_app => b::MOBILE_APP,
        hostname_prefix => b::HOSTNAME_PREFIX,
        hostname_pattern => b::HOSTNAME_PATTERN,
        theme => b::THEME,
        support_url => b::SUPPORT_URL,
        support_email => b::SUPPORT_EMAIL,
        logo_width => b::LOGO_WIDTH,
        logo_height => b::LOGO_HEIGHT,
    }
}

/// The `{{ serial|node_serial }}` filter: a STICKER serial in its canonical form, 8 lowercase
/// hex, with the decimal in brackets — `80e007da (2162165722)`. The Hub's registry keeps the hex
/// and the device label prints the decimal, so the Portal shows both (node_registry.md §2).
///
/// A string is read the way the registry reads it (`0x…`, bare 8 hex, or decimal); a number,
/// which is how a Node's own DeviceInfo carries it, is decimal. Anything that is not a u32 — a
/// migrated record flagged `serial_invalid` — is shown as it is, and none as nothing.
pub fn node_serial(value: Value) -> String {
    if value.is_undefined() || value.is_none() {
        return String::new();
    }
    let n = match value.as_str() {
        Some(s) => control_radio::p2p::session::canonical_serial(s)
            .and_then(|hex| u32::from_str_radix(&hex, 16).ok()),
        None => value.to_string().parse::<u32>().ok(),
    };
    match (n, value.as_str()) {
        (Some(n), _) => format!("{n:08x} ({n})"),
        (None, Some(s)) => s.to_string(),
        (None, None) => value.to_string(),
    }
}

const MONTH_ABBR_EN: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The `{{ value|short_dt(mode) }}` filter — the one place the portal turns an
/// instant into display text, so every page shows time the same way.
///
/// Input is RFC 3339 (node/beacon `last_seen`, alarm rows) or SQLite's
/// `datetime('now')` shape `YYYY-MM-DD HH:MM:SS` (audit, notes, report
/// history), which is UTC by definition. Output is in the installation's
/// display zone ([`crate::display_tz`], decisions.md #25) and names it —
/// here for Europe/Prague in summer:
///
/// | mode | cs | en |
/// |---|---|---|
/// | default | `25. 8. 10:43:39 CEST` | `Aug 25, 10:43:39 CEST` |
/// | `"full"` | `25. 8. 2026 10:43:39 CEST` | `Aug 25, 2026, 10:43:39 CEST` |
/// | `"compact"` | `25. 8. 10:43 CEST` | `Aug 25, 10:43 CEST` |
/// | `"time"` | `10:43:39 CEST` | `10:43:39 CEST` |
/// | `"date"` | `25. 8. 2026` | `Aug 25, 2026` |
///
/// A value that isn't a string (missing/`none`) renders as `""` — so the
/// existing `|default(..., true)` idiom still works when piped after this
/// filter. A string that parses as neither shape passes through unchanged,
/// never blanked or guessed at.
pub fn short_dt(state: &State, value: Value, mode: Option<&str>) -> String {
    let Some(s) = value.as_str() else {
        return String::new();
    };
    let Some(t) = parse_instant(s) else {
        return s.to_string();
    };
    let lang = state
        .lookup("lang")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| crate::i18n::DEFAULT_LANG.to_string());
    display_dt(
        t,
        &lang,
        mode.unwrap_or("default"),
        &crate::display_tz::current(),
    )
}

/// RFC 3339, or SQLite's `YYYY-MM-DD HH:MM:SS` read as UTC.
fn parse_instant(s: &str) -> Option<time::OffsetDateTime> {
    use time::format_description::well_known::Rfc3339;
    if let Ok(t) = time::OffsetDateTime::parse(s, &Rfc3339) {
        return Some(t);
    }
    let sqlite =
        time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]").ok()?;
    time::PrimitiveDateTime::parse(s, &sqlite)
        .ok()
        .map(|t| t.assume_utc())
}

/// See [`short_dt`]'s table. Also the e-mail's timestamp, which has no
/// template to run the filter in.
pub fn display_dt(
    t: time::OffsetDateTime,
    lang: &str,
    mode: &str,
    tz: &jiff::tz::TimeZone,
) -> String {
    let Ok(ts) = jiff::Timestamp::new(t.unix_timestamp(), t.nanosecond() as i32) else {
        return String::new();
    };
    let z = ts.to_zoned(tz.clone());
    let (d, mo, y) = (z.day(), z.month(), z.year());
    let hm = format!("{:02}:{:02}", z.hour(), z.minute());
    let hms = format!("{hm}:{:02}", z.second());
    let cs = lang == "cs";
    let mon = MONTH_ABBR_EN[(mo - 1) as usize];
    // A calendar date with its year and no time: the calibration card's expiry date
    // (calibration.cs.md §2, "DD. MM. RRRR" in Czech). The instant is a UTC midnight, so
    // the day is read in UTC whatever the display zone, and no zone is named.
    if mode == "date" {
        let (d, mo, y) = (t.day(), u8::from(t.month()), t.year());
        let mon = MONTH_ABBR_EN[(mo - 1) as usize];
        return if cs {
            format!("{d}. {mo}. {y}")
        } else {
            format!("{mon} {d}, {y}")
        };
    }
    let out = match (mode, cs) {
        ("time", _) => hms,
        ("compact", true) => format!("{d}. {mo}. {hm}"),
        ("compact", false) => format!("{mon} {d}, {hm}"),
        ("full", true) => format!("{d}. {mo}. {y} {hms}"),
        ("full", false) => format!("{mon} {d}, {y}, {hms}"),
        (_, true) => format!("{d}. {mo}. {hms}"),
        (_, false) => format!("{mon} {d}, {hms}"),
    };
    format!("{out} {}", z.strftime("%Z"))
}

/// One device's `device_meta` row in template shape (devices.backend.md §3,
/// WP0.2). `None` — the operator has never touched this device — renders as
/// every default rather than as an absence, because that is exactly what the
/// missing row means.
///
/// The two override fields stay `Option`: blank is "inherit the YAML
/// default" and `0` is "off for this device", and a template that could not
/// tell them apart would show an inherited setting as a disabled one.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceMetaCtx {
    /// Operator-typed display name (V16). Used by the Hub, whose name is
    /// held portal-side because `device_label` rides the device config and
    /// a factory reset wipes it (devices.hub.md §3.1).
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub added_by: Option<String>,
    pub offline_k_override: Option<i64>,
    pub backfill_auto: bool,
    /// Beacon only (V14, B6). Typed at registration and never on the wire —
    /// see `portal_storage::DeviceMeta::serial`.
    pub serial: Option<String>,
    /// Portal-side location (V39), a Hub's only one.
    pub location: Option<String>,
}

/// One `serde_json::Value` as a TEMPLATE value, with its numbers converted
/// by hand.
///
/// serde_json is compiled with `arbitrary_precision` whenever this binary is
/// built alongside `control-core` — Cargo unifies features per build, and
/// control-core enables it to verify MDR envelopes against the wire's own
/// number text (`fiber-protocol::envelope`). Under that feature a number
/// *serializes* as a one-key map (`$serde_json::private::Number`), so raw
/// JSON reaching a template through `Serialize` arrives as a map: an
/// arithmetic expression fails outright (the `/docs` image boxes:
/// `{{ box.x * 100 }}`) and a plain `{{ info.uptime_s }}` prints the map.
/// Walking the value once, here, is what makes a page render the same with
/// the feature and without it.
///
/// Every context field that carries JSON the portal did not shape itself —
/// a Node's DeviceInfo, its config entries, one raw uplink, the `/docs`
/// content — goes through this.
pub fn json_for_template(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::from(()),
        serde_json::Value::Bool(b) => Value::from(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::from)
            .or_else(|| n.as_u64().map(Value::from))
            .or_else(|| n.as_f64().map(Value::from))
            // A number no float can hold keeps its own text rather than
            // quietly becoming a different number.
            .unwrap_or_else(|| Value::from(n.to_string())),
        serde_json::Value::String(s) => Value::from(s.clone()),
        serde_json::Value::Array(items) => {
            Value::from(items.iter().map(json_for_template).collect::<Vec<_>>())
        }
        serde_json::Value::Object(fields) => Value::from(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), json_for_template(v)))
                .collect::<std::collections::BTreeMap<_, _>>(),
        ),
    }
}

pub fn device_meta_ctx(meta: Option<&portal_storage::DeviceMeta>) -> DeviceMetaCtx {
    match meta {
        Some(m) => DeviceMetaCtx {
            name: m.name.clone(),
            description: m.description.clone(),
            created_at: Some(m.created_at.clone()),
            added_by: m.added_by.clone(),
            offline_k_override: m.offline_k_override,
            backfill_auto: m.backfill_auto,
            serial: m.serial.clone(),
            location: m.location.clone(),
        },
        None => DeviceMetaCtx {
            name: None,
            description: None,
            created_at: None,
            added_by: None,
            offline_k_override: None,
            backfill_auto: true,
            serial: None,
            location: None,
        },
    }
}

/// `true` for `manager`/`admin`/superuser — the M4-D role gate shared by the
/// dashboard's threshold-edit pencil (`sensor_card.html`'s `can_edit`),
/// `POST /hubs/{hub}/system/restart`, and `POST /hubs/{hub}/sensors/
/// {line}/threshold`
/// (mirrors `CurrentUser::has_role`, which the `RequireManager` extractor
/// gates routes with; this is the same check exposed for template context,
/// where no route is being gated at all — kept here, not in `perms`, since both `http.rs`'s
/// initial page render and every `sse.rs` re-render need it purely for
/// template context, with no certificate involved).
pub fn can_edit_thresholds(user: &CurrentUser) -> bool {
    user.has_role(Role::Manager)
}

/// Merges `current_user` into a page's own context — every render call in
/// `http.rs`/`routes/*` goes through this once, rather than threading
/// `current_user` through each page's `*_ctx` builder. `None` for the
/// anonymous surface (login/setup), matching `layout/nav.html`'s contract
/// (a `none`-valued `current_user` renders bare-brand chrome, no nav).
/// `CurrentUser` derives `Serialize` with exactly the shape that template
/// expects (plus an unused `id` field, harmless).
pub fn page_ctx(base: Value, current_user: Option<&CurrentUser>) -> Value {
    context! { current_user => current_user, ..base }
}

/// Fixtures shared by the submodules' own test modules — the store and
/// database rows every context builder is exercised against.
#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    pub(super) fn sensor(line: u8) -> SensorState {
        SensorState {
            line,
            ..SensorState::default()
        }
    }

    /// `short_dt` reads `state` only for `lang`, same as `i18n::t` — driven
    /// through an actual template render (rather than hand-building a
    /// `State`) since that's the only public way to get one.
    pub(super) fn render_short_dt(template: &str, value: Value, lang: &str) -> String {
        let mut env = Environment::new();
        register(&mut env);
        env.render_str(template, context! { value => value, lang => lang })
            .unwrap()
    }

    pub(super) fn node(node_key: &str) -> NodeState {
        NodeState::new(node_key.to_string(), 1000)
    }

    /// The banner-builder tests' clock. The fixture rows are stamped
    /// `updated_at: 0`, so "now" is also 0 and no row is ever accidentally
    /// aged out — the one test that cares about ageing passes its own
    /// timestamps.
    pub(super) const NOW: i64 = 0;

    /// One draft row, for the banner-builder tests.
    pub(super) fn pending_row(
        param_key: &str,
        kind: &str,
        value: &str,
    ) -> portal_storage::PendingConfigRow {
        portal_storage::PendingConfigRow {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            param_key: param_key.into(),
            kind: kind.into(),
            desired_value: Some(value.into()),
            author: "anna".into(),
            created_at: 0,
            state: portal_storage::PENDING_STATE_PENDING.into(),
            attempts: 0,
            last_error: None,
            updated_at: 0,
            previous_value: None,
            previous_band: None,
            previous_order: None,
        }
    }

    pub(super) fn beacon(mac: &str) -> BeaconState {
        BeaconState::new(mac.to_string(), 1000)
    }

    /// A `BeaconCardInputs` with nothing loaded — the Beacon twin of
    /// `bare`, for the tests that are about the state mapping and not
    /// about what the bulk pass found.
    pub(super) fn bare_beacon(b: &BeaconState) -> BeaconCardInputs<'_> {
        BeaconCardInputs {
            beacon: b,
            meta: None,
            thresholds: &[],
            offline: None,
            signal_bands: BARE_BANDS.get_or_init(Default::default),
            hubs: Vec::new(),
            calibration: None,
        }
    }

    /// A Node row with no extras — no metadata, no stored thresholds, and
    /// the detector saying "online". What `node_card_ctx` sees for a Node
    /// nobody has configured, which is every Node until an operator touches
    /// one.
    pub(super) fn bare(n: &NodeState) -> NodeCardInputs<'_> {
        NodeCardInputs {
            node: n,
            info: None,
            device_info: None,
            meta: None,
            thresholds: &[],
            offline: Some(false),
            pending_count: 0,
            has_readings: false,
            signal_bands: BARE_BANDS.get_or_init(Default::default),
            calibration: None,
        }
    }

    pub(super) static BARE_BANDS: std::sync::OnceLock<crate::config::SignalQualityConfig> =
        std::sync::OnceLock::new();

    pub(super) fn raw(id: i64, received_at: i64, payload: &str) -> portal_storage::NodeRawMessage {
        portal_storage::NodeRawMessage {
            id,
            hostname: "h1".into(),
            node_key: "aabbccdd00000001".into(),
            received_at,
            payload: payload.to_string(),
        }
    }

    pub(super) fn event(
        ts: i64,
        event_type: &str,
        payload: Option<&str>,
    ) -> portal_storage::NodeEvent {
        portal_storage::NodeEvent {
            hostname: "h1".into(),
            node_key: "aabbccdd00000001".into(),
            event_type: event_type.to_string(),
            ts,
            seq: 0,
            payload: payload.map(str::to_string),
        }
    }

    pub(super) fn db_threshold(
        field: &str,
        warning_high: Option<f64>,
        enabled: bool,
    ) -> FieldThreshold {
        FieldThreshold {
            hostname: "h1".to_string(),
            scope: "node".to_string(),
            entity: "aabbccdd00000001".to_string(),
            field: field.to_string(),
            critical_low: None,
            warning_low: None,
            warning_high,
            critical_high: None,
            enabled,
            updated_at: None,
            hysteresis: None,
        }
    }

    /// All four bounds set, which is what a gauge needs before it draws
    /// anything — [`db_threshold`] sets one, for the tests about precedence.
    pub(super) fn db_threshold_with_bounds(field: &str, enabled: bool) -> FieldThreshold {
        FieldThreshold {
            critical_low: Some(-30.0),
            warning_low: Some(-20.0),
            warning_high: Some(30.0),
            critical_high: Some(40.0),
            ..db_threshold(field, Some(30.0), enabled)
        }
    }

    /// One Hub holding the three cases the panel has to tell apart.
    pub(super) fn scan_device(now: i64) -> crate::state::DeviceState {
        let mut device = crate::state::DeviceState::new("h1".into());
        device.online = true;

        // A registered Beacon, the wire's `null` form. It is on `/beacons`,
        // never on the scan table.
        let mut registered = beacon("AA:BB:CC:DD:EE:01");
        registered.discovered = None;
        registered.last_snapshot_at = Some(now);

        // A stranger heard a moment ago.
        let mut fresh = beacon("AA:BB:CC:DD:EE:02");
        fresh.discovered = Some(true);
        fresh.rssi = Some(-55);
        fresh.last_snapshot_at = Some(now - 30);

        // ...and one last heard beyond the window.
        let mut old = beacon("AA:BB:CC:DD:EE:03");
        old.discovered = Some(true);
        old.rssi = Some(-40); // stronger, so ordering cannot hide the filter
        old.last_snapshot_at = Some(now - DISCOVERED_WINDOW_S - 1);

        for b in [registered, fresh, old] {
            device.ble_sensors.insert(b.mac.clone(), b);
        }
        device
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::render_short_dt;
    use super::*;

    #[test]
    fn short_dt_formats_every_mode_in_utc_and_says_so() {
        // 10:43:39 at +02:00 is 08:43:39 UTC.
        let ts = Value::from("2026-08-25T10:43:39.572686+02:00");
        for (tpl, cs, en) in [
            (
                "{{ value|short_dt }}",
                "25. 8. 08:43:39 UTC",
                "Aug 25, 08:43:39 UTC",
            ),
            (
                "{{ value|short_dt(\"full\") }}",
                "25. 8. 2026 08:43:39 UTC",
                "Aug 25, 2026, 08:43:39 UTC",
            ),
            (
                "{{ value|short_dt(\"compact\") }}",
                "25. 8. 08:43 UTC",
                "Aug 25, 08:43 UTC",
            ),
            (
                "{{ value|short_dt(\"time\") }}",
                "08:43:39 UTC",
                "08:43:39 UTC",
            ),
        ] {
            assert_eq!(render_short_dt(tpl, ts.clone(), "cs"), cs, "{tpl}");
            assert_eq!(render_short_dt(tpl, ts.clone(), "en"), en, "{tpl}");
        }
    }

    /// decisions.md #25: the display zone moves the clock and names the zone
    /// as it stood on that day — CEST in summer, CET in winter, which is why
    /// a fixed hour offset would not do.
    #[test]
    fn display_dt_follows_the_zone_across_daylight_saving() {
        let prague = crate::display_tz::parse("Europe/Prague").expect("bundled zone");
        let at = |iso: &str| {
            time::OffsetDateTime::parse(iso, &time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        assert_eq!(
            display_dt(at("2026-08-25T08:43:39Z"), "cs", "full", &prague),
            "25. 8. 2026 10:43:39 CEST"
        );
        assert_eq!(
            display_dt(at("2026-01-05T08:43:39Z"), "en", "default", &prague),
            "Jan 5, 09:43:39 CET"
        );
        assert_eq!(format_time_in(0, &prague), "01:00:00 CET");
        // `date` is a calendar day held as a UTC midnight: no clock, no zone, and the
        // day is not moved by the display zone.
        assert_eq!(
            display_dt(at("2026-08-25T00:00:00Z"), "cs", "date", &prague),
            "25. 8. 2026"
        );
        assert_eq!(
            display_dt(at("2026-08-25T00:00:00Z"), "en", "date", &prague),
            "Aug 25, 2026"
        );
    }

    /// Audit, notes and report history are stamped by SQLite's
    /// `datetime('now')`, which is UTC without saying so.
    #[test]
    fn short_dt_reads_sqlite_datetime_as_utc() {
        assert_eq!(
            render_short_dt(
                "{{ value|short_dt(\"full\") }}",
                Value::from("2026-09-25 16:02:21"),
                "cs"
            ),
            "25. 9. 2026 16:02:21 UTC"
        );
    }

    /// node_registry.md §2: the canonical 8 hex and the decimal, whichever form came in.
    #[test]
    fn node_serial_shows_the_hex_and_the_decimal() {
        for input in [
            Value::from("80e007da"),
            Value::from("0x80E007DA"),
            Value::from("2162165722"),
            Value::from(2_162_165_722_u32),
        ] {
            assert_eq!(
                node_serial(input.clone()),
                "80e007da (2162165722)",
                "{input}"
            );
        }
        // A number is decimal even when it has eight digits; a string of eight is hex, as the
        // registry reads it.
        assert_eq!(node_serial(Value::from(12_345_678)), "00bc614e (12345678)");
        assert_eq!(node_serial(Value::from(33_333)), "00008235 (33333)");
        // Not a u32: as it is (a migrated `serial_invalid` record), and none as nothing.
        assert_eq!(node_serial(Value::from("SN-1")), "SN-1");
        assert_eq!(node_serial(Value::from("4294967296")), "4294967296");
        assert_eq!(node_serial(Value::from(())), "");
    }

    #[test]
    fn short_dt_passes_through_unparseable_and_blanks_none() {
        assert_eq!(
            render_short_dt("{{ value|short_dt }}", Value::from("garbage"), "en"),
            "garbage"
        );
        assert_eq!(
            render_short_dt("{{ value|short_dt }}", Value::from(()), "en"),
            ""
        );
    }

    #[test]
    fn format_time_renders_hh_mm_ss_utc() {
        // 1970-01-01T12:34:56Z
        assert_eq!(format_time(12 * 3600 + 34 * 60 + 56), "12:34:56 UTC");
        // Wraps within a day regardless of which day the timestamp falls on.
        assert_eq!(format_time(86_400 + 5), "00:00:05 UTC");
    }

    #[test]
    fn can_edit_thresholds_by_role() {
        fn user(role: Role, is_superuser: bool) -> CurrentUser {
            CurrentUser {
                id: 1,
                username: "u".into(),
                full_name: None,
                user_role: role,
                is_superuser,
                monitoring_view_mode: "table-simplified".into(),
            }
        }

        assert!(can_edit_thresholds(&user(Role::Admin, false)));
        assert!(can_edit_thresholds(&user(Role::Manager, false)));
        assert!(can_edit_thresholds(&user(Role::Viewer, true)));
        assert!(!can_edit_thresholds(&user(Role::Viewer, false)));
    }

    #[test]
    fn command_outcome_ctx_maps_missing_request_id_to_empty_string() {
        let outcome = CommandOutcome {
            request_id: None,
            command_type: "not_a_real_command".to_string(),
            status: "error",
            message: Some("unknown command_type".to_string()),
            error_code: None,
            result: None,
        };
        let ctx = CommandOutcomeCtx::from(outcome);
        assert_eq!(ctx.request_id, "");
        assert_eq!(ctx.status, "error");
        assert_eq!(ctx.command_type, "not_a_real_command");
    }

    fn render_command_status(outcome: CommandOutcome, lang: &str) -> String {
        Templates::new("templates", "static")
            .env()
            .get_template("partials/command_status.html")
            .expect("template loads")
            .render(context! { lang => lang, cmd => CommandOutcomeCtx::from(outcome) })
            .expect("renders")
    }

    /// GitLab #121: the device's name is the banner's first line, above the action and its
    /// state, in every state.
    #[test]
    fn the_command_banner_leads_with_the_device_name() {
        for status in ["sent", "success", "error", "timeout"] {
            let outcome = CommandOutcome {
                request_id: Some("req-1".into()),
                command_type: "reboot".into(),
                status,
                message: None,
                error_code: None,
                result: None,
            };
            let mut ctx = CommandOutcomeCtx::from(outcome);
            ctx.target = Some("Beacon 3DA0".into());
            let html = Templates::new("templates", "static")
                .env()
                .get_template("partials/command_status.html")
                .unwrap()
                .render(context! { lang => "cs", cmd => ctx })
                .unwrap();
            let name = html.find("Beacon 3DA0").expect("device name");
            let action = html.find(" — ").expect("action line");
            let ts = html.find("req-1").expect("request id");
            assert!(name < action && action < ts, "{status}: {html}");
            assert!(html.contains("data-command-target"), "{html}");
        }
    }

    /// Hardware test 2026-10-09: a history request's ack means the Hub took
    /// the download on, not that it finished, so it must not say "Dokončeno".
    /// Any other command's success still does.
    #[test]
    fn an_accepted_history_request_does_not_claim_it_is_done() {
        let ok = |command_type: &str| CommandOutcome {
            request_id: Some("r1".to_string()),
            command_type: command_type.to_string(),
            status: "success",
            message: None,
            error_code: None,
            result: None,
        };
        for command_type in ["beacon-history", "get_sticker_history"] {
            let html = render_command_status(ok(command_type), "cs");
            assert!(!html.contains("Dokončeno"), "{html}");
            assert!(html.contains("Přijato Hubem"), "{html}");
            assert!(html.contains("Průběh ukazuje tabulka níže."), "{html}");
            assert!(html.contains("alert-info"), "{html}");
        }
        // Its ack is the request written (`--no-wait`); the radio card follows the flash.
        let html = render_command_status(ok("northbridge-update"), "cs");
        assert!(!html.contains("Dokončeno"), "{html}");
        assert!(html.contains("Aktualizace firmwaru Northbridge"), "{html}");
        assert!(html.contains("karta Firmware Northbridge"), "{html}");
        assert!(html.contains("alert-info"), "{html}");
        let html = render_command_status(ok("get_sticker_config"), "cs");
        assert!(html.contains("Dokončeno"), "{html}");
        assert!(html.contains("alert-success"), "{html}");
    }

    /// UX-005: an inert radio seam leads with a sentence the operator can
    /// act on, and the seam's English text survives only as the detail.
    #[test]
    fn a_radio_not_configured_failure_explains_itself_in_czech() {
        let err = crate::radio::RadioError::NotConfigured("no rendered radio config".into());
        let html = render_command_status(
            CommandOutcome {
                request_id: None,
                command_type: "get_sticker_config".to_string(),
                status: "error",
                message: Some(err.to_string()),
                error_code: err.code(),
                result: None,
            },
            "cs",
        );
        assert!(html.contains("Načtení konfigurace Nodu"));
        assert!(html.contains("Tento portál nemá nastavené ovládání rádia"));
        assert!(html.contains("Technický detail"));
        assert!(html.contains("radio is not configured on this Hub: no rendered radio config"));
    }

    /// An unclassified failure renders as it always did: the detail alone.
    #[test]
    fn an_unclassified_failure_keeps_only_the_detail() {
        let html = render_command_status(
            CommandOutcome {
                request_id: None,
                command_type: "get_sticker_config".to_string(),
                status: "error",
                message: Some("something odd".to_string()),
                error_code: None,
                result: None,
            },
            "en",
        );
        assert!(html.contains("Technical detail"));
        assert!(html.contains("something odd"));
        assert!(!html.contains("monitoring.command_error"));
    }
}
