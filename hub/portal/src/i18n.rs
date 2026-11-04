//! UI translation catalogue (M9, plan/viewer/design.md §8.2) plus the
//! machinery to get a string in front of a template: the `t` minijinja
//! filter, the cookie/`Accept-Language` locale rule, and the `lang`
//! task-local that carries the resolved language from the request into
//! the three render paths without threading it through every handler.
//!
//! **Catalogue.** `i18n/<lang>/<group>.json` for `lang` in [`LANGS`] and
//! `group` in [`GROUPS`], embedded with `include_str!` so lookup is
//! infallible and needs no runtime path (plan/security/landlock.md §2 bans
//! paths assembled at runtime; an `include_str!`'d catalogue can't be
//! missing at startup either way). Each file is a flat JSON object whose
//! keys are already the full dotted, group-prefixed catalogue key (e.g.
//! `"auth.login.submit"`) — storing the prefix in the key itself is what
//! makes cross-group collisions a structural property the `no_duplicate_
//! keys_across_groups` test can check mechanically, rather than something a
//! reviewer has to remember to look for.
//!
//! **Lookup order** (requested language -> English -> the key itself) and
//! the `t` filter live here; see [`t`] and [`lookup`].
//!
//! **Locale is not stored per user.** A `lang` cookie is enough — it's a
//! display preference, not account state — so M9 needed no DB migration.
//! [`resolve_lang`] is the cookie-vs-`Accept-Language` rule; [`LANG_COOKIE`]
//! /[`lang_cookie`] build the cookie the `/lang` route sets
//! (`routes::lang_routes`).

use std::borrow::Cow;
use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::sync::Arc;
#[cfg(not(debug_assertions))]
use std::sync::OnceLock;

use axum::extract::Request;
use axum::http::header::ACCEPT_LANGUAGE;
use axum::middleware::Next;
use axum::response::Response;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use cookie::time::Duration;
use minijinja::{Environment, State};

/// Supported UI languages. Adding a third is a JSON file per group plus a
/// new entry here — design.md §8.2's whole point in building this now.
pub const LANGS: [&str; 2] = ["en", "cs"];
const GROUPS: [&str; 4] = ["common", "auth", "monitoring", "alarms"];
pub const DEFAULT_LANG: &str = "en";

/// Cookie carrying the language preference (`en`|`cs`). Separate from
/// [`crate::auth::SESSION_COOKIE`] — this is set (and readable) even for an
/// anonymous visitor on `/login`/`/setup`.
pub const LANG_COOKIE: &str = "lang";
/// ~400 days: the de-facto browser cap on `Max-Age` (Chrome refuses longer),
/// used here as "as long-lived as a preference cookie can usefully be".
const LANG_TTL_SECS: i64 = 400 * 24 * 3600;

/// Debug builds read the catalogue file from the source tree first, so an
/// edited translation shows up on the next render without a rebuild; the
/// embedded copy is the fallback and the only source in release.
fn catalogue_json(lang: &str, group: &str) -> Cow<'static, str> {
    if cfg!(debug_assertions) {
        let path = format!("{}/i18n/{lang}/{group}.json", env!("CARGO_MANIFEST_DIR"));
        if let Ok(json) = std::fs::read_to_string(path) {
            return Cow::Owned(json);
        }
    }
    Cow::Borrowed(match (lang, group) {
        ("en", "common") => include_str!("../i18n/en/common.json"),
        ("en", "auth") => include_str!("../i18n/en/auth.json"),
        ("en", "monitoring") => include_str!("../i18n/en/monitoring.json"),
        ("en", "alarms") => include_str!("../i18n/en/alarms.json"),
        ("cs", "common") => include_str!("../i18n/cs/common.json"),
        ("cs", "auth") => include_str!("../i18n/cs/auth.json"),
        ("cs", "monitoring") => include_str!("../i18n/cs/monitoring.json"),
        ("cs", "alarms") => include_str!("../i18n/cs/alarms.json"),
        _ => "{}",
    })
}

/// `lang -> (key -> value)`. Built once in release; rebuilt periodically in
/// debug builds so `catalogue_json`'s on-disk read is actually seen.
type Catalogue = HashMap<&'static str, HashMap<String, String>>;

#[cfg(not(debug_assertions))]
static CATALOGUE: OnceLock<Catalogue> = OnceLock::new();

/// How stale a debug build's hot-reloaded catalogue may be.
///
/// It used to rebuild on **every** lookup, which is eight file reads and
/// eight JSON parses per translated string. That is invisible on a page with
/// a dozen of them and ruinous on a list: `partials/node_card.html` carries
/// three, so a 500-Node list did twelve thousand file reads per render and
/// took about five seconds — and the SSE path re-renders those same rows on
/// every batch. It is what devices.backend.md §4's 500-row performance gate
/// caught first, before any of the code that gate was written for.
///
/// Half a second is under the time it takes to alt-tab to the browser, so
/// hot reload still feels immediate, and a render now does at most one
/// rebuild instead of one per string.
#[cfg(debug_assertions)]
const HOT_RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[cfg(debug_assertions)]
static HOT_CATALOGUE: std::sync::Mutex<Option<(std::time::Instant, Arc<Catalogue>)>> =
    std::sync::Mutex::new(None);

#[cfg(debug_assertions)]
fn catalogue() -> Arc<Catalogue> {
    let mut cached = HOT_CATALOGUE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let fresh = cached
        .as_ref()
        .is_some_and(|(built, _)| built.elapsed() < HOT_RELOAD_INTERVAL);
    if !fresh {
        *cached = Some((std::time::Instant::now(), Arc::new(build_catalogue())));
    }
    Arc::clone(&cached.as_ref().expect("just populated").1)
}

#[cfg(not(debug_assertions))]
fn catalogue() -> &'static Catalogue {
    CATALOGUE.get_or_init(build_catalogue)
}

fn build_catalogue() -> Catalogue {
    {
        let mut out = Catalogue::new();
        for lang in LANGS {
            let mut keys = HashMap::new();
            for group in GROUPS {
                // A malformed catalogue file contributes no keys for that
                // group/lang rather than panicking at startup — `t`'s
                // fallback chain (requested -> English -> the key itself)
                // means a gap here is visible in the UI, never a crash.
                // `cargo test`'s `catalogue_files_are_valid_json` is what
                // actually guards this file being well-formed.
                let parsed: HashMap<String, String> =
                    serde_json::from_str(&catalogue_json(lang, group)).unwrap_or_default();
                keys.extend(parsed);
            }
            brand(lang, &mut keys);
            out.insert(lang, keys);
        }
        out
    }
}

/// The build's brand, applied to one language of the catalogue (plan/portal/branding.md §3):
/// `{brand}`, `{mobile_app}`, `{hostname_prefix}` … filled in, then the brand's own overrides
/// (`branding/<brand>/i18n/<lang>.json`, flat like the catalogue) replace whole strings. The
/// default brand has no overrides and its placeholders expand to the literals they replaced,
/// so its catalogue is the one it always was.
fn brand(lang: &str, keys: &mut HashMap<String, String>) {
    for value in keys.values_mut() {
        if let Cow::Owned(expanded) = proximos_brand::expand(value) {
            *value = expanded;
        }
    }
    for (_, json) in proximos_brand::I18N_OVERRIDES
        .iter()
        .filter(|(l, _)| *l == lang)
    {
        let parsed: HashMap<String, String> = serde_json::from_str(json).unwrap_or_default();
        for (k, v) in parsed {
            keys.insert(k, proximos_brand::expand(&v).into_owned());
        }
    }
}

/// Registers the `t` filter on `env` — every place that builds a template
/// [`Environment`] must call this (`http::app` and `routes::test_support::
/// test_state`, currently the only two) or every template touched by this
/// milestone fails to render with an "unknown filter" error.
pub fn register(env: &mut Environment<'_>) {
    env.add_filter("t", t);
    env.add_test("translated", translated);
    env.add_filter("field_label", field_label_filter);
    env.add_filter("alarm_message", alarm_message);
    // P-3: the threshold form presets a Node's critical band with its open sides.
    // The Quantity filter's options on lists spanning every device kind.
    env.add_function("quantity_options", || {
        minijinja::Value::from_serialize(crate::sensor_picker::quantity_options())
    });
    env.add_function("critical_open", |field: String| {
        crate::radio::alarm::open_band(&field).map(|(low, high)| vec![low, high])
    });
    // threshold_defaults.md D5: a field with no stored band pre-fills the configured default
    // of its quantity (`"node"` or `"eye"` scope), and the slider seeds a unit's blank band
    // from the same numbers rather than from a spread of its own.
    env.add_function("threshold_default", |scope: String, field: String| {
        let scope = match scope.as_str() {
            "node" => portal_storage::THRESHOLD_SCOPE_NODE,
            "eye" => portal_storage::THRESHOLD_SCOPE_EYE,
            _ => return None,
        };
        let defaults = crate::threshold_defaults::configured();
        // Built as a template value directly, not through `serde_json`: CI's workspace build
        // turns on its `arbitrary_precision`, which serializes a number as a private map.
        crate::threshold_defaults::default_band(&defaults, scope, &field).map(|b| {
            minijinja::context! {
                critical_low => b.critical_low,
                warning_low => b.warning_low,
                warning_high => b.warning_high,
                critical_high => b.critical_high,
            }
        })
    });
    // alarm_hysteresis.md §5: the hysteresis a form pre-fills when the field has none of its
    // own — the default of the field's quantity (`"node"`, `"eye"` or `"hub"` scope), or of a
    // unit for the bulk panel, whose bands are per field name across kinds.
    env.add_function("threshold_hysteresis", |scope: String, field: String| {
        let scope = match scope.as_str() {
            "node" => portal_storage::THRESHOLD_SCOPE_NODE,
            "eye" => portal_storage::THRESHOLD_SCOPE_EYE,
            "hub" => portal_storage::THRESHOLD_SCOPE_HUB,
            _ => return 0.0,
        };
        crate::threshold_defaults::hysteresis(
            &crate::threshold_defaults::configured(),
            scope,
            &field,
            None,
        )
    });
    env.add_function("threshold_hysteresis_of_unit", |unit: String| {
        fiber_protocol::Quantity::of_unit(Some(&unit)).map_or(0.0, |q| {
            crate::threshold_defaults::configured().hysteresis(q)
        })
    });
    // limits.cs.md §9.4: what the one limits editor opens on for a field, decided in one place
    // (`limits_routes::editor_band`). `t` is the stored band (`{critical_low, …, enabled,
    // hysteresis}`) or none.
    env.add_function(
        "limits_band",
        |scope: String, field: String, t: Option<minijinja::Value>| {
            let num = |v: &minijinja::Value, k: &str| {
                v.get_attr(k)
                    .ok()
                    .filter(|x| !x.is_none() && !x.is_undefined())
                    .and_then(|x| f64::try_from(x).ok())
            };
            let stored = t.filter(|v| !v.is_none() && !v.is_undefined()).map(|v| {
                let enabled = v
                    .get_attr("enabled")
                    .ok()
                    .filter(|x| !x.is_undefined() && !x.is_none())
                    .is_none_or(|x| x.is_true());
                (
                    (
                        num(&v, "critical_low"),
                        num(&v, "warning_low"),
                        num(&v, "warning_high"),
                        num(&v, "critical_high"),
                    ),
                    enabled,
                    num(&v, "hysteresis"),
                )
            });
            let b = crate::routes::limits_routes::editor_band(&scope, &field, stored);
            minijinja::context! {
                critical_low => b.critical_low,
                warning_low => b.warning_low,
                warning_high => b.warning_high,
                critical_high => b.critical_high,
                enabled => b.enabled,
                hysteresis => b.hysteresis,
            }
        },
    );
    env.add_function("threshold_seed", |unit: String| {
        let quantity = fiber_protocol::Quantity::of_unit(Some(&unit))?;
        let b = crate::threshold_defaults::configured().band(quantity);
        Some(vec![
            b.critical_low,
            b.warning_low,
            b.warning_high,
            b.critical_high,
        ])
    });
}

/// `{{ f.label|field_label(f.name) }}`: the `common.field.<name>`
/// translation of a registry field, or the registry label it was given when
/// the catalogue has none. Without `name` (a wired probe, a stored alarm
/// name, a composed slot name) the label is translated part by part
/// ([`translate_field_label`]).
fn field_label_filter(state: &State, label: String, name: Option<String>) -> String {
    let lang = state
        .lookup("lang")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_LANG.to_string());
    match name {
        Some(name) => field_label(&lang, &name, &translate_field_label(&lang, &label)),
        None => translate_field_label(&lang, &label),
    }
}

/// The Rust side of the `field_label` filter, for labels a handler bakes
/// into JSON or a composed string. Unlike [`lookup`] a miss is expected (a
/// field added to the registry before its translation) and does not warn.
pub(crate) fn field_label(lang: &str, name: &str, fallback: &str) -> String {
    let cat = catalogue();
    let key = format!("common.field.{name}");
    [lang, DEFAULT_LANG]
        .iter()
        .find_map(|l| cat.get(l).and_then(|m| m.get(&key)))
        .cloned()
        // A 1-Wire slot's field has no catalogue name; its label is drawn in the
        // unified "1W slot N - … - quantity" shape (#122) wherever it is shown.
        .unwrap_or_else(|| {
            match translate_w1_label(cat.get(lang).unwrap_or(&HashMap::new()), fallback) {
                Some(label) => label,
                None => fallback.to_string(),
            }
        })
}

/// The `{{ a.message|alarm_message }}` filter (HS-13): an alarm's stored sentence is English,
/// written when the alarm was raised (`offline::offline_message`, the liveness rows'
/// `offline`). The shapes the Portal writes are recognised and drawn in the page's language,
/// rows already stored included; anything else is drawn as it is.
pub fn alarm_message(state: &State, message: &str) -> String {
    let lang = state
        .lookup("lang")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_LANG.to_string());
    translate_alarm_message(&lang, message)
}

/// `YYYY-MM-DD` as a day and month without the year — "31. 10." in Czech, "Oct 31" in English,
/// the calibration alarm's spelling of its date. Anything else is returned as it is.
fn day_month(lang: &str, iso: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = iso.splitn(3, '-');
    let (Some(_), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        return iso.to_string();
    };
    match (month.parse::<usize>(), day.parse::<u32>()) {
        (Ok(m @ 1..=12), Ok(d @ 1..=31)) if lang == "cs" => format!("{d}. {m}."),
        (Ok(m @ 1..=12), Ok(d @ 1..=31)) => format!("{} {d}", MONTHS[m - 1]),
        _ => iso.to_string(),
    }
}

pub(crate) fn translate_alarm_message(lang: &str, message: &str) -> String {
    if message == "offline" {
        return lookup(lang, "alarms.message.offline");
    }
    // The `sync:<hub>` alarm's two fixed sentences (config_sync.md §10).
    if message == crate::sync::MSG_NO_REQUEST {
        return lookup(lang, "alarms.message.sync_no_request");
    }
    if message == crate::sync::MSG_ITEM_FAILED {
        return lookup(lang, "alarms.message.sync_item_failed");
    }
    // The calibration reminder's three shapes (`calibration::alarm_message`, amendment
    // 2026-10-13): "Kalibrace vyprší 31. 10.", "Kalibrace vyprší do 7 dní", "Kalibrace prošla
    // 31. 10.".
    use crate::calibration::{EXPIRED_PREFIX, SECOND_WARNING_PREFIX, WARNING_PREFIX};
    if let Some(date) = message.strip_prefix(EXPIRED_PREFIX) {
        return lookup(lang, "alarms.message.calibration_expired")
            .replace("{date}", &day_month(lang, date));
    }
    if let Some(days) = message
        .strip_prefix(SECOND_WARNING_PREFIX)
        .and_then(|rest| rest.split_once(", on "))
        .and_then(|(days, _)| days.strip_suffix(" days").or(days.strip_suffix(" day")))
        .and_then(|days| days.parse::<u32>().ok())
    {
        let key = if days == 1 {
            "alarms.message.calibration_second_warning_one"
        } else {
            "alarms.message.calibration_second_warning"
        };
        return lookup(lang, key).replace("{days}", &days.to_string());
    }
    if let Some(date) = message.strip_prefix(WARNING_PREFIX) {
        return lookup(lang, "alarms.message.calibration_due")
            .replace("{date}", &day_month(lang, date));
    }
    let not_reported = message
        .strip_suffix(" minutes")
        .and_then(|m| m.split_once(" has not reported for "))
        .and_then(|(who, minutes)| {
            let (kind, entity) = who.split_once(' ')?;
            let kind = match kind {
                "Node" => "node",
                "Beacon" => "beacon",
                _ => return None,
            };
            minutes.parse::<u64>().ok()?;
            Some((kind, entity, minutes))
        });
    match not_reported {
        Some((kind, entity, minutes)) => lookup(lang, "alarms.message.not_reported")
            .replace(
                "{kind}",
                &lookup(lang, &format!("monitoring.groups.kind_{kind}")),
            )
            .replace("{entity}", entity)
            .replace("{minutes}", minutes),
        None => message.to_string(),
    }
}

pub(crate) fn translate_field_label(lang: &str, label: &str) -> String {
    const SEPARATORS: [&str; 2] = [" \u{b7} ", " \u{2014} "];
    let cat = catalogue();
    let Some(keys) = cat.get(lang) else {
        return label.to_string();
    };
    if let Some(out) = translate_w1_label(keys, label) {
        return out;
    }
    let mut out = String::with_capacity(label.len());
    let mut rest = label;
    loop {
        let next = SEPARATORS
            .iter()
            .filter_map(|sep| rest.find(sep).map(|at| (at, *sep)))
            .min_by_key(|(at, _)| *at);
        let part = next.map_or(rest, |(at, _)| &rest[..at]);
        out.push_str(&translate_label_part(keys, part));
        match next {
            Some((at, sep)) => {
                out.push_str(sep);
                rest = &rest[at + sep.len()..];
            }
            None => return out,
        }
    }
}

/// The registry name a search typed in `lang` means (N25): the alarms search runs in SQL over
/// the stored English names, and a Czech page shows them translated. `None` for English, for a
/// query under three letters and for one no translated name contains, which is then searched
/// as typed. Several matches (`tepl` is in "Teplota" and "Tepl.") give the shortest English
/// name, which the others contain ("Temp" in "Temperature").
pub(crate) fn field_label_source(lang: &str, query: &str) -> Option<String> {
    let q = query.trim().to_lowercase();
    if lang == DEFAULT_LANG || q.chars().count() < 3 {
        return None;
    }
    let cat = catalogue();
    let (local, english) = (cat.get(lang)?, cat.get(DEFAULT_LANG)?);
    local
        .iter()
        .filter(|(key, name)| {
            key.starts_with("monitoring.field_label.") && name.to_lowercase().contains(&q)
        })
        .filter_map(|(key, _)| english.get(key).cloned())
        .min_by_key(|name| (name.len(), name.clone()))
}

/// A 1-Wire channel label (#122): "1W slot N - <type> - <quantity>", or the pre-v2
/// "machine probe N - <quantity>". Only the quantity is translated, and it stays lowercase in
/// every language; the type is a product name (`Dallas`, `machine probe`, `type<N>`) and is
/// kept as written. A label stored before #122 ("1W slot 1 · Dallas — Temperature") is
/// redrawn in the new shape. `None` for any other label.
fn translate_w1_label(keys: &HashMap<String, String>, label: &str) -> Option<String> {
    let normal = label
        .replace(" \u{b7} ", " - ")
        .replace(" \u{2014} ", " - ")
        .replace(" - Machine probe - ", " - machine probe - ");
    let normal = match normal.strip_prefix("Machine probe ") {
        Some(rest) => format!("machine probe {rest}"),
        None => normal,
    };
    if !(normal.starts_with("1W slot ") || normal.starts_with("machine probe ")) {
        return None;
    }
    let (head, quantity) = normal.rsplit_once(" - ")?;
    // The pre-v2 registry's short names ("Machine probe 1 — Temp").
    let quantity = match quantity {
        "Temp" => "temperature",
        "Hum" => "humidity",
        q => q,
    };
    let translated = translate_label_part(keys, quantity);
    let mut chars = translated.chars();
    let lower: String = match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    };
    Some(format!("{head} - {lower}"))
}

fn translate_label_part(keys: &HashMap<String, String>, part: &str) -> String {
    // A trailing number or single capital is an index, kept as it is.
    let is_index = |tail: &str| {
        tail.chars().all(|c| c.is_ascii_digit())
            || (tail.len() == 1 && tail.chars().all(|c| c.is_ascii_uppercase()))
    };
    let (base, index) = match part.rsplit_once(' ') {
        Some((base, tail)) if !base.is_empty() && is_index(tail) => (base, &part[base.len()..]),
        _ => (part, ""),
    };
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    match keys.get(&format!("monitoring.field_label.{slug}")) {
        Some(name) => format!("{name}{index}"),
        None => part.to_string(),
    }
}

/// The `{{ "auth.login.submit"|t }}` filter (registered via [`register`]).
/// `state.lookup("lang")` reads the `lang` context key
/// every render path merges in (see module doc); no key at all (a fragment
/// rendered without going through one of those three paths) defaults to
/// English, same as an unknown language would.
pub fn t(state: &State, key: &str) -> String {
    let lang = state
        .lookup("lang")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_LANG.to_string());
    lookup(&lang, key)
}

/// The `{% if key is translated %}` test: whether `key` has a text in the request's language or
/// in English. For a template that probes an optional key (a per-command label), so the probe
/// does not log a missing key.
pub fn translated(state: &State, key: &str) -> bool {
    let lang = state
        .lookup("lang")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_LANG.to_string());
    let cat = catalogue();
    [lang.as_str(), DEFAULT_LANG]
        .iter()
        .any(|l| cat.get(*l).is_some_and(|m| m.contains_key(key)))
}

/// Requested language -> English -> the key itself. Only warns on the final
/// fallback (missing from English too) — a `cs`-only gap is what
/// `translation_keys_match_across_languages` exists to catch, so warning on
/// every miss there too would just be log spam for a known, tested state.
pub(crate) fn lookup(lang: &str, key: &str) -> String {
    let cat = catalogue();
    if let Some(value) = cat.get(lang).and_then(|m| m.get(key)) {
        return value.clone();
    }
    if lang != DEFAULT_LANG {
        if let Some(value) = cat.get(DEFAULT_LANG).and_then(|m| m.get(key)) {
            return value.clone();
        }
    }
    tracing::warn!(key, "missing i18n key in English catalogue");
    key.to_string()
}

/// `en`/`cs` (case-sensitive, no normalization) or `None` for anything else
/// — used both for the `lang` cookie value and the `/lang` route's form
/// input, so an unknown value falls back the same way in both places.
fn normalize_lang(value: &str) -> Option<&'static str> {
    LANGS.iter().find(|&&l| l == value).copied()
}

/// Cookie wins; otherwise a deliberately dumb `Accept-Language` check (no
/// q-value parsing — "mentions cs" is enough per design.md §8.2); otherwise
/// English. An unknown cookie value falls through to the header rather than
/// short-circuiting to English, same as a missing cookie would.
pub fn resolve_lang(cookie: Option<&str>, accept_language: Option<&str>) -> &'static str {
    if let Some(lang) = cookie.and_then(normalize_lang) {
        return lang;
    }
    if accept_language.is_some_and(|header| header.to_ascii_lowercase().contains("cs")) {
        return "cs";
    }
    DEFAULT_LANG
}

tokio::task_local! {
    /// Per-request resolved language, scoped by [`lang_middleware`] around
    /// the whole request. `routes::render()` and `http::dashboard_handler`
    /// read it via [`current_lang`] instead of taking a `lang` parameter —
    /// that's what keeps M9's "plumb lang into the context" down to editing
    /// those two functions (plus `sse::send_rendered()`) instead of every
    /// one of their call sites.
    ///
    /// This does NOT reach `sse::events_handler`'s spawned render task: a
    /// `tokio::spawn`ed future starts a new task and does not inherit the
    /// spawning task's task-locals. That path resolves `lang` once (this
    /// same task-local is still valid at the point `/events` is handled,
    /// before the spawn) and carries the plain value into the connection's
    /// state instead, next to `can_edit` — see `sse.rs`'s module doc.
    static CURRENT_LANG: &'static str;
}

/// Reads the per-request language set by [`lang_middleware`]; `en` if
/// called outside a request that went through it (e.g. a route-module unit
/// test that builds a bare `Router` without the app-level middleware stack
/// — same simplification those tests already make for `setup_gate`/
/// `origin_guard`).
pub fn current_lang() -> &'static str {
    CURRENT_LANG.try_with(|lang| *lang).unwrap_or(DEFAULT_LANG)
}

/// App-wide middleware (`http::app`, alongside `setup_gate`/`origin_guard`):
/// resolves [`resolve_lang`] once per request from the `lang` cookie and
/// `Accept-Language`, then scopes [`CURRENT_LANG`] around the rest of the
/// request's handling so [`current_lang`] can read it from wherever a
/// template actually gets rendered.
pub async fn lang_middleware(request: Request, next: Next) -> Response {
    let jar = CookieJar::from_headers(request.headers());
    let cookie = jar.get(LANG_COOKIE).map(|c| c.value().to_string());
    let accept_language = request
        .headers()
        .get(ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let lang = resolve_lang(cookie.as_deref(), accept_language.as_deref());
    CURRENT_LANG.scope(lang, next.run(request)).await
}

/// Builds the `Set-Cookie` value for the language preference (`routes::
/// lang_routes::set_lang`). Same flag convention as `auth::cookies::
/// session_cookie` — `secure` mirrors `PortalConfig::cookie_secure` — but a
/// much longer `Max-Age`: this is a low-stakes display preference, not a
/// session, so there's no reason to expire it on any particular schedule.
/// `HttpOnly` even though nothing needs to keep it from JS, for consistency
/// with the session cookie.
pub fn lang_cookie(lang: &str, secure: bool) -> Cookie<'static> {
    Cookie::build((LANG_COOKIE, lang.to_string()))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .secure(secure)
        .max_age(Duration::seconds(LANG_TTL_SECS))
        .build()
}

/// `form.lang` from the `/lang` POST -> a valid language, defaulting to
/// English for anything unrecognized (same posture as an unknown cookie).
pub fn lang_or_default(value: &str) -> &'static str {
    normalize_lang(value).unwrap_or(DEFAULT_LANG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_label_translates_and_falls_back_to_the_registry_label() {
        assert_eq!(field_label("cs", "temperature", "Temperature"), "Teplota");
        assert_eq!(field_label("en", "no_such_field", "Raw"), "Raw");
    }

    /// Decision #33: a Node's or Beacon's name is owned by the Hub, and
    /// renaming is not available until that design ships, so the register
    /// dialogs must not promise it.
    #[test]
    fn the_register_hint_does_not_promise_renaming() {
        for (lang, promise) in [("cs", "později"), ("en", "later")] {
            let hint = lookup(lang, "monitoring.shared.name_hint_serial_fallback");
            assert!(!hint.contains(promise), "{lang}: {hint}");
            assert!(hint.len() > 20, "the rest of the hint stays: {hint}");
        }
    }

    /// HS-13: the stored English sentence is drawn in the page's language, and anything the
    /// Portal did not write is left alone.
    #[test]
    fn stored_alarm_messages_are_translated_when_drawn() {
        assert_eq!(
            translate_alarm_message(
                "cs",
                "Node 5876070000000413 has not reported for 60 minutes"
            ),
            "Node 5876070000000413 nehlásí už 60 min"
        );
        assert_eq!(
            translate_alarm_message("en", "Beacon AA:BB has not reported for 15 minutes"),
            "Beacon AA:BB has not reported for 15 minutes"
        );
        assert_eq!(translate_alarm_message("cs", "offline"), "offline");
        assert_eq!(
            translate_alarm_message("cs", "Calibration expires on 2026-10-31"),
            "Kalibrace vyprší 31. 10."
        );
        assert_eq!(
            translate_alarm_message("en", "Calibration expires on 2026-10-01"),
            "Calibration expires on Oct 1"
        );
        assert_eq!(
            translate_alarm_message("cs", "Calibration expires within 7 days, on 2026-10-31"),
            "Kalibrace vyprší do 7 dní"
        );
        assert_eq!(
            translate_alarm_message("cs", "Calibration expires within 1 day, on 2026-10-31"),
            "Kalibrace vyprší do 1 dne"
        );
        assert_eq!(
            translate_alarm_message("cs", "Calibration expired on 2026-11-01"),
            "Kalibrace prošla 1. 11."
        );
        assert_eq!(
            translate_alarm_message("cs", "Sensor 3 has not reported for many minutes"),
            "Sensor 3 has not reported for many minutes"
        );
    }

    /// The brand's placeholders are filled once per build, so none may survive into the
    /// catalogue, and a brand override may only replace a string the catalogue has (a typo'd
    /// key would add a string nothing reads and leave the original showing).
    #[test]
    fn the_brand_is_applied_to_the_whole_catalogue() {
        let cat = build_catalogue();
        for lang in LANGS {
            for (key, value) in &cat[lang] {
                for (name, _) in proximos_brand::PLACEHOLDERS {
                    assert!(
                        !value.contains(name),
                        "{lang} {key}: {name} left in {value:?}"
                    );
                }
                if proximos_brand::ID != "proximos" {
                    assert!(
                        !value.contains("ProXimos") && !value.contains("PROXIMOS"),
                        "{lang} {key} still says ProXimos in a {} build: {value:?}",
                        proximos_brand::PRODUCT
                    );
                }
            }
        }
        for (lang, json) in proximos_brand::I18N_OVERRIDES {
            let parsed: HashMap<String, String> = serde_json::from_str(json).unwrap();
            for key in parsed.keys() {
                assert!(
                    cat[*lang].contains_key(key),
                    "branding override {lang} {key} names no catalogue key"
                );
            }
        }
    }

    /// Every group file the catalogue reads must actually parse — a
    /// hand-edited JSON typo would otherwise silently drop that group's
    /// keys for that language (see `catalogue()`'s `unwrap_or_default`).
    #[test]
    fn catalogue_files_are_valid_json() {
        for lang in LANGS {
            for group in GROUPS {
                let raw = catalogue_json(lang, group);
                let parsed: Result<HashMap<String, String>, _> = serde_json::from_str(&raw);
                assert!(
                    parsed.is_ok(),
                    "i18n/{lang}/{group}.json is not valid JSON: {:?}",
                    parsed.err()
                );
            }
        }
    }

    /// Every key must actually belong to the group whose file it's stored
    /// in — the "group prefix in the key" convention is only a structural
    /// safeguard if this holds.
    #[test]
    fn keys_are_prefixed_by_their_own_group() {
        let mut offenders = Vec::new();
        for lang in LANGS {
            for group in GROUPS {
                let raw = catalogue_json(lang, group);
                let parsed: HashMap<String, String> =
                    serde_json::from_str(&raw).unwrap_or_default();
                for key in parsed.keys() {
                    if !key.starts_with(&format!("{group}.")) {
                        offenders.push(format!("i18n/{lang}/{group}.json: {key}"));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "keys stored under the wrong group's file: {offenders:?}"
        );
    }

    /// The completeness guarantee M9 exists for: `en` and `cs` must define
    /// exactly the same key set, group by group, so a missing translation
    /// is caught here rather than in the rendered page.
    #[test]
    fn translation_keys_match_across_languages() {
        for group in GROUPS {
            let en: HashMap<String, String> =
                serde_json::from_str(&catalogue_json("en", group)).unwrap_or_default();
            let cs: HashMap<String, String> =
                serde_json::from_str(&catalogue_json("cs", group)).unwrap_or_default();

            let missing_in_cs: Vec<&String> =
                en.keys().filter(|k| !cs.contains_key(k.as_str())).collect();
            let missing_in_en: Vec<&String> =
                cs.keys().filter(|k| !en.contains_key(k.as_str())).collect();

            assert!(
                missing_in_cs.is_empty() && missing_in_en.is_empty(),
                "group {group:?}: missing in cs: {missing_in_cs:?}; missing in en: {missing_in_en:?}"
            );
        }
    }

    /// No key may appear in more than one group's file, for either
    /// language — the group prefix is meant to make this structurally
    /// impossible in practice, this is the check that would catch it if it
    /// ever weren't.
    #[test]
    fn no_duplicate_keys_across_groups() {
        for lang in LANGS {
            let mut seen: HashMap<String, &str> = HashMap::new();
            for group in GROUPS {
                let parsed: HashMap<String, String> =
                    serde_json::from_str(&catalogue_json(lang, group)).unwrap_or_default();
                for key in parsed.keys() {
                    if let Some(first_group) = seen.insert(key.clone(), group) {
                        panic!(
                            "key {key:?} appears in both {first_group:?} and {group:?} ({lang})"
                        );
                    }
                }
            }
        }
    }

    /// Walks every template and asserts that each *static* key handed to the
    /// `t` filter exists in the catalogue. The parity test above proves `cs`
    /// and `en` agree; it cannot prove a key exists at all, and a template
    /// typo would put the raw key on screen instead of failing anything.
    /// Dynamic keys (`("common.nav." ~ id)|t`) are invisible here by
    /// construction — their value sets are covered by the tests next to the
    /// enums that produce them.
    #[test]
    fn every_static_template_key_exists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let mut missing = Vec::new();
        let mut checked = 0usize;

        for path in template_files(&root) {
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            for key in filtered_keys(&body) {
                checked += 1;
                if lookup("en", &key) == key {
                    missing.push(format!("{}: {key}", path.display()));
                }
            }
        }

        assert!(checked > 0, "found no `|t` keys at all — parser is broken");
        assert!(
            missing.is_empty(),
            "template keys with no catalogue entry: {missing:#?}"
        );
    }

    fn template_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(template_files(&path));
            } else if path.extension().is_some_and(|e| e == "html") {
                out.push(path);
            }
        }
        out
    }

    /// Every `"some.key"|t` in `body`, ignoring dynamic keys and other
    /// filters (`|title`, `|trim`).
    fn filtered_keys(body: &str) -> Vec<String> {
        let bytes: Vec<char> = body.chars().collect();
        let mut keys = Vec::new();
        let mut i = 0;

        while i < bytes.len() {
            if bytes[i] != '"' {
                i += 1;
                continue;
            }
            let start = i + 1;
            let Some(len) = bytes[start..].iter().position(|c| *c == '"') else {
                break;
            };
            let literal: String = bytes[start..start + len].iter().collect();
            let mut j = start + len + 1;

            while bytes.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            if bytes.get(j) == Some(&'|') {
                j += 1;
                while bytes.get(j).is_some_and(|c| c.is_whitespace()) {
                    j += 1;
                }
                let is_t_filter = bytes.get(j) == Some(&'t')
                    && !bytes
                        .get(j + 1)
                        .is_some_and(|c| c.is_alphanumeric() || *c == '_');
                if is_t_filter && literal.contains('.') {
                    keys.push(literal);
                }
            }
            i = start + len + 1;
        }

        keys
    }

    #[test]
    fn field_labels_translate_part_by_part_and_keep_what_they_do_not_know() {
        assert_eq!(translate_field_label("cs", "Temperature"), "Teplota");
        assert_eq!(
            translate_field_label("cs", "Node \u{b7} Temperature"),
            "Node \u{b7} Teplota"
        );
        assert_eq!(
            translate_field_label("cs", "External probe 1"),
            "Externí sonda 1"
        );
        assert_eq!(translate_field_label("cs", "Input A"), "Vstup A");
        assert_eq!(
            translate_field_label("cs", "1W slot 1 - Dallas - temperature"),
            "1W slot 1 - Dallas - teplota"
        );
        assert_eq!(
            translate_field_label("cs", "1W slot 2 - machine probe - magnetic field"),
            "1W slot 2 - machine probe - magnetické pole",
            "the type is a product name and stays untranslated"
        );
        assert_eq!(
            translate_field_label("en", "1W slot 3 - type7 - humidity"),
            "1W slot 3 - type7 - humidity"
        );
        assert_eq!(
            translate_field_label("cs", "1W slot 1 \u{b7} Machine probe \u{2014} Temperature"),
            "1W slot 1 - machine probe - teplota",
            "a label stored before #122 takes the new separators"
        );
        assert_eq!(
            translate_field_label("cs", "machine probe 1 - humidity"),
            "machine probe 1 - vlhkost"
        );
        assert_eq!(
            translate_field_label("en", "Machine probe 1 \u{2014} Hum"),
            "machine probe 1 - humidity"
        );
        assert_eq!(
            translate_field_label("cs", "Temperature \u{b7} critical_high"),
            "Teplota \u{b7} critical_high"
        );
        assert_eq!(
            translate_field_label("cs", "Freezer 2"),
            "Freezer 2",
            "a name, not a field"
        );
        assert_eq!(translate_field_label("en", "Temperature"), "Temperature");
        assert_eq!(translate_field_label("xx", "Temperature"), "Temperature");
    }

    /// N25: a Czech search for a quantity reaches the English name storage holds.
    #[test]
    fn a_translated_search_finds_the_stored_name() {
        assert_eq!(
            field_label_source("cs", "Teplota").as_deref(),
            Some("Temperature")
        );
        assert_eq!(field_label_source("cs", "tepl").as_deref(), Some("Temp"));
        assert_eq!(
            field_label_source("cs", "vlhkost").as_deref(),
            Some("Humidity")
        );
        assert_eq!(field_label_source("cs", "Freezer"), None, "not a quantity");
        assert_eq!(
            field_label_source("cs", "te"),
            None,
            "too short to mean one"
        );
        assert_eq!(field_label_source("en", "Temperature"), None);
    }

    /// Every part of every name the registries hold has an entry, so no field is drawn half
    /// in English on a Czech page.
    #[test]
    fn every_registry_label_has_a_czech_name() {
        let labels = fiber_protocol::fields::lorawan::REGISTRY
            .iter()
            .map(|f| f.label)
            .chain(
                fiber_protocol::fields::eye::REGISTRY
                    .iter()
                    .map(|f| f.label),
            );
        let mut missing = Vec::new();
        for label in labels {
            let cs = translate_field_label("cs", label);
            if cs == label {
                missing.push(label);
            }
        }
        assert!(missing.is_empty(), "{missing:#?}");
    }

    #[test]
    fn t_falls_back_to_english_then_to_the_key() {
        assert_eq!(lookup("en", "auth.login.submit"), "Sign in");
        // cs has a real translation for this key.
        assert_ne!(lookup("cs", "auth.login.submit"), "auth.login.submit");
        // A language with no catalogue entry at all falls back to English.
        assert_eq!(lookup("de", "auth.login.submit"), "Sign in");
        // A key present in neither falls back to itself.
        assert_eq!(lookup("en", "nope.does.not.exist"), "nope.does.not.exist");
        assert_eq!(lookup("cs", "nope.does.not.exist"), "nope.does.not.exist");
    }

    #[test]
    fn resolve_lang_cookie_beats_header() {
        assert_eq!(resolve_lang(Some("cs"), Some("en-US,en;q=0.9")), "cs");
    }

    #[test]
    fn resolve_lang_unknown_cookie_falls_through_to_header() {
        assert_eq!(resolve_lang(Some("de"), Some("cs-CZ,cs;q=0.9")), "cs");
    }

    #[test]
    fn resolve_lang_header_mentions_cs() {
        assert_eq!(resolve_lang(None, Some("cs-CZ,cs;q=0.9,en;q=0.8")), "cs");
    }

    #[test]
    fn resolve_lang_defaults_to_english() {
        assert_eq!(resolve_lang(None, None), "en");
        assert_eq!(resolve_lang(None, Some("fr-FR,fr;q=0.9")), "en");
    }
}
