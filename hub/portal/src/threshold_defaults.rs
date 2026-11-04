//! Default thresholds for temperature and humidity, and the battery that is not a sensor
//! (plan/portal/threshold_defaults.md).
//!
//! **The defaults (§1–§3).** Every temperature and humidity sensor gets the configured band
//! of its quantity (`thresholds.defaults`, [`crate::config::ThresholdDefaults`]) once, when the
//! portal first registers it, as a `field_thresholds` row **switched off** and written by
//! [`DEFAULTS_ACTOR`]. A field that has a row, in any state, is left alone, so an operator's
//! band and a threshold they switched off both win. A switched-off row grades nothing and
//! goes nowhere — no draft, no set-config, no signed command — until an operator switches it
//! on, which then takes the usual way (a Node's draft and Send, a Beacon's signed command).
//!
//! Where a sensor is registered, and so where [`seed`] runs:
//! - a Node: the portal's `node-add` (`node_routes::add_node`); the Hub's retained inventory
//!   document, which is how the portal learns of a Node registered from the CLI; its config
//!   info, which declares its 1-Wire probes; and its readings, for a probe that reports before
//!   it is declared (`persist`, the v2 radio family only — a v1 Hub's Nodes carry the Hub's
//!   own bounds);
//! - a Beacon: `beacon_routes::add_beacon`, and the Hub naming it registered
//!   (`config/state` eye tags, `eye/sensors`).
//!
//! Sensors that exist already get theirs the same way at the first start of this build: the
//! Hub replays every retained document on connect, and inserting only what is missing makes
//! every later start a no-op. A change of the YAML therefore reaches only fields that had no
//! row yet — sensors registered after it (§3).
//!
//! **The battery (§5).** A Node's `voltage` is its battery: shown as the battery, never a
//! sensor with a band, because the Node raises its own low-battery alarm on fPort 3. The
//! field registry says it is not thresholdable, the routes refuse a band on it, the engine
//! grades none, and [`forget_battery_thresholds`] deletes the rows an earlier build stored,
//! with the drafts that would put one back, once at startup.

use std::sync::OnceLock;

use portal_storage::{
    NewAuditEntry, NewFieldThreshold, Storage, StorageError, THRESHOLD_SCOPE_EYE,
    THRESHOLD_SCOPE_HUB, THRESHOLD_SCOPE_NODE,
};

use crate::config::{DefaultBand, ThresholdDefaults};

/// Who the audit trail names for what this module writes on the portal's own initiative.
pub const DEFAULTS_ACTOR: &str = "system:defaults";

static CONFIGURED: OnceLock<ThresholdDefaults> = OnceLock::new();

/// Sets the configured defaults once at startup (`main`), before anything registers a
/// sensor. The same process-wide pattern as `auth::set_trusted_proxies`.
pub fn set_configured(defaults: ThresholdDefaults) {
    let _ = CONFIGURED.set(defaults);
}

/// The configured defaults, or the shipped ones where nothing set them (tests, a dev
/// portal with no config).
pub fn configured() -> ThresholdDefaults {
    CONFIGURED.get().copied().unwrap_or_default()
}

/// The default band of one field in one scope: its quantity's band, read off the field
/// registry (unit and kind), or `None` for a field that gets none.
///
/// A Node's critical band on a quantity the Node watches itself is always a range (P-3,
/// nodes_phase2.cs.md dodatek 2026-09-25), so a default that leaves one critical side out is
/// completed with that side's open bound, as the form does. A band that ends up out of order
/// is not written: no Send could deliver it.
pub fn default_band(defaults: &ThresholdDefaults, scope: &str, field: &str) -> Option<DefaultBand> {
    let quantity = match scope {
        THRESHOLD_SCOPE_NODE => fiber_protocol::lorawan_field(field)?.default_quantity()?,
        THRESHOLD_SCOPE_EYE => fiber_protocol::eye_field(field)?.default_quantity()?,
        _ => return None,
    };
    let mut band = defaults.band(quantity);
    if band.is_empty() {
        return None;
    }
    if scope == THRESHOLD_SCOPE_NODE {
        if let Some((open_low, open_high)) = crate::radio::alarm::open_band(field) {
            match (band.critical_low, band.critical_high) {
                (Some(_), None) => band.critical_high = Some(open_high),
                (None, Some(_)) => band.critical_low = Some(open_low),
                _ => {}
            }
        }
    }
    band.rises().then_some(band)
}

/// The quantity of one field in one scope, read off the field registry. A Hub's 1-Wire line
/// measures temperature.
pub fn quantity(scope: &str, field: &str) -> Option<fiber_protocol::Quantity> {
    match scope {
        THRESHOLD_SCOPE_NODE => fiber_protocol::lorawan_field(field)?.default_quantity(),
        THRESHOLD_SCOPE_EYE => fiber_protocol::eye_field(field)?.default_quantity(),
        THRESHOLD_SCOPE_HUB => {
            (field == "temperature").then_some(fiber_protocol::Quantity::Temperature)
        }
        _ => None,
    }
}

/// The hysteresis a field is graded with (decision #41): its own when set, else the default
/// of its quantity, else 0 (strict).
pub fn hysteresis(defaults: &ThresholdDefaults, scope: &str, field: &str, own: Option<f64>) -> f64 {
    own.filter(|h| h.is_finite() && *h >= 0.0)
        .unwrap_or_else(|| quantity(scope, field).map_or(0.0, |q| defaults.hysteresis(q)))
}

/// The rows [`seed`] offers for one sensor: one switched-off row per field of `fields` that
/// has a default band, in the order given, each field once.
pub fn default_rows<'a>(
    defaults: &ThresholdDefaults,
    hostname: &str,
    scope: &str,
    entity: &str,
    fields: impl IntoIterator<Item = &'a str>,
) -> Vec<NewFieldThreshold> {
    let mut out: Vec<NewFieldThreshold> = Vec::new();
    for field in fields {
        if out.iter().any(|r| r.field == field) {
            continue;
        }
        let Some(band) = default_band(defaults, scope, field) else {
            continue;
        };
        out.push(NewFieldThreshold {
            hostname: hostname.to_string(),
            scope: scope.to_string(),
            entity: entity.to_string(),
            field: field.to_string(),
            critical_low: band.critical_low,
            warning_low: band.warning_low,
            warning_high: band.warning_high,
            critical_high: band.critical_high,
            enabled: false,
        });
    }
    out
}

/// Writes the default bands of one sensor's `fields` that have no row yet (§3) and returns
/// the fields it wrote. Never touches a row that exists.
pub fn seed<'a>(
    storage: &Storage,
    defaults: &ThresholdDefaults,
    hostname: &str,
    scope: &str,
    entity: &str,
    fields: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<String>, StorageError> {
    let rows = default_rows(defaults, hostname, scope, entity, fields);
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let inserted = storage.insert_missing_field_thresholds(&rows, DEFAULTS_ACTOR)?;
    if !inserted.is_empty() {
        tracing::info!(
            hostname,
            scope,
            entity,
            fields = ?inserted,
            "default thresholds written, switched off"
        );
    }
    Ok(inserted)
}

/// The fields of a Beacon that get a default band.
pub fn beacon_fields() -> impl Iterator<Item = &'static str> {
    fiber_protocol::eye_registry()
        .iter()
        .filter(|f| f.default_quantity().is_some())
        .map(|f| f.name)
}

/// The Node field that is its battery: the registry's one `Battery` field.
fn battery_fields() -> impl Iterator<Item = &'static str> {
    fiber_protocol::lorawan_registry()
        .iter()
        .filter(|f| f.group == fiber_protocol::LorawanFieldGroup::Battery)
        .map(|f| f.name)
}

/// Deletes every stored band and draft of the Node's battery (§5), one audit row per Node
/// that had a band. Idempotent: after the first start there is nothing left, and nothing is
/// audited. Run by the persist thread before it takes any event.
pub fn forget_battery_thresholds(storage: &Storage) -> Result<usize, StorageError> {
    let mut removed = 0;
    for field in battery_fields() {
        let rows = storage.delete_field_thresholds_named(THRESHOLD_SCOPE_NODE, field)?;
        let drafts = storage.delete_pending_threshold_field(field)?;
        for (hostname, node_key) in &rows {
            storage.insert_audit(&NewAuditEntry {
                event_type: "node_threshold_removed".to_string(),
                username: Some(DEFAULTS_ACTOR.to_string()),
                user_id: None,
                ip: None,
                target: Some(node_key.clone()),
                details: Some(
                    serde_json::json!({
                        "hostname": hostname,
                        "field": field,
                        "reason": "the battery is not a sensor; the Node alarms on it itself",
                    })
                    .to_string(),
                ),
                success: true,
            })?;
        }
        if !rows.is_empty() || drafts > 0 {
            tracing::info!(
                field,
                bands = rows.len(),
                drafts,
                "deleted the battery's thresholds: the Node alarms on it itself"
            );
        }
        removed += rows.len();
    }
    Ok(removed)
}

/// Whether a Node field is its battery, which takes no band (§5).
pub fn is_battery(field: &str) -> bool {
    battery_fields().any(|f| f == field)
}

/// Whether a stored Node band is graded at all: not on a field the registry says takes
/// none (the battery, §5). A field the registry does not know is graded — more likely a
/// newer field than a withdrawn one.
pub fn node_field_is_graded(field: &str) -> bool {
    fiber_protocol::lorawan_field(field).is_none_or(|f| f.thresholdable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_storage::{NewFieldThreshold, NewPendingConfig, PENDING_KIND_THRESHOLD};

    fn row(entity: &str, field: &str) -> NewFieldThreshold {
        NewFieldThreshold {
            hostname: "hub-1".into(),
            scope: THRESHOLD_SCOPE_NODE.into(),
            entity: entity.into(),
            field: field.into(),
            critical_low: Some(2.8),
            warning_low: None,
            warning_high: None,
            critical_high: Some(4.2),
            enabled: true,
        }
    }

    #[test]
    fn the_battery_takes_no_band_and_its_old_ones_go_once_with_an_audit_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");
        for (entity, field) in [
            ("aa11", "voltage"),
            ("aa11", "temperature"),
            ("bb22", "voltage"),
        ] {
            storage
                .upsert_field_threshold(&row(entity, field), Some("admin"))
                .expect("upsert");
        }
        storage
            .upsert_pending(&NewPendingConfig {
                hostname: "hub-1".into(),
                node_key: "aa11".into(),
                param_key: "threshold.voltage.critical_low".into(),
                kind: PENDING_KIND_THRESHOLD.into(),
                desired_value: Some("2.9".into()),
                author: "admin".into(),
            })
            .expect("draft");

        assert_eq!(forget_battery_thresholds(&storage).expect("forget"), 2);
        let left = storage
            .list_field_thresholds("hub-1", THRESHOLD_SCOPE_NODE, "aa11")
            .expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].field, "temperature");
        assert!(storage
            .list_pending("hub-1", "aa11")
            .expect("drafts")
            .is_empty());
        let (audit, total) = storage
            .list_audit(&portal_storage::AuditFilter {
                event_type: Some("node_threshold_removed".into()),
                limit: 50,
                ..Default::default()
            })
            .expect("audit");
        assert_eq!(total, 2);
        assert!(audit
            .iter()
            .all(|a| a.username.as_deref() == Some(DEFAULTS_ACTOR)));

        assert_eq!(forget_battery_thresholds(&storage).expect("again"), 0);
        assert_eq!(
            storage
                .list_audit(&portal_storage::AuditFilter {
                    event_type: Some("node_threshold_removed".into()),
                    ..Default::default()
                })
                .expect("audit")
                .1,
            2,
            "a second start audits nothing"
        );
    }

    fn temp_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");
        (dir, storage)
    }

    /// §1: the band follows the quantity the registry gives the field, on a Node and on a
    /// Beacon; the battery, a counter and an unknown field get none.
    #[test]
    fn a_field_gets_the_band_of_its_quantity() {
        let d = ThresholdDefaults::default();
        for field in ["temperature", "w1_2_temperature", "ext_temperature_1"] {
            assert_eq!(
                default_band(&d, THRESHOLD_SCOPE_NODE, field),
                Some(d.temperature),
                "{field}"
            );
        }
        for field in ["humidity", "w1_4_humidity", "machine_probe_humidity_1"] {
            assert_eq!(
                default_band(&d, THRESHOLD_SCOPE_NODE, field),
                Some(d.humidity),
                "{field}"
            );
        }
        assert_eq!(
            default_band(&d, THRESHOLD_SCOPE_EYE, "temperature"),
            Some(d.temperature)
        );
        for field in ["voltage", "pressure", "input_a_count", "no_such_field"] {
            assert_eq!(
                default_band(&d, THRESHOLD_SCOPE_NODE, field),
                None,
                "{field}"
            );
        }
        assert_eq!(default_band(&d, THRESHOLD_SCOPE_EYE, "battery"), None);
        assert_eq!(
            beacon_fields().collect::<Vec<_>>(),
            ["temperature", "humidity"]
        );
    }

    /// §2: a bound left out of the block is not written — except a Node's critical side,
    /// which P-3 always completes — and a quantity with no bound gets no row at all.
    #[test]
    fn a_partial_default_band_is_written_as_configured() {
        let d = ThresholdDefaults {
            temperature: DefaultBand {
                critical_low: Some(-20.0),
                warning_low: None,
                warning_high: Some(30.0),
                critical_high: None,
                hysteresis: None,
            },
            humidity: DefaultBand::default(),
            illuminance: DefaultBand::default(),
        };
        let node = default_band(&d, THRESHOLD_SCOPE_NODE, "temperature").expect("band");
        assert_eq!(
            node.critical_high,
            Some(199.0),
            "completed for the Node's rule"
        );
        assert_eq!(node.warning_low, None);
        let beacon = default_band(&d, THRESHOLD_SCOPE_EYE, "temperature").expect("band");
        assert_eq!(
            beacon.critical_high, None,
            "a Beacon's band may be one-sided"
        );
        assert_eq!(default_band(&d, THRESHOLD_SCOPE_NODE, "humidity"), None);
    }

    /// §3, §6: registering writes a switched-off row per temperature and humidity field and
    /// none for the battery; a second registration writes nothing more, and a field with a
    /// row — enabled or switched off — is left alone.
    #[test]
    fn seeding_writes_switched_off_rows_once_and_never_over_a_row() {
        let (_dir, storage) = temp_storage();
        let d = ThresholdDefaults::default();
        let fields = [
            "temperature",
            "humidity",
            "voltage",
            "w1_1_temperature",
            "illuminance",
            "temperature",
        ];
        let wrote =
            seed(&storage, &d, "hub-1", THRESHOLD_SCOPE_NODE, "aa11", fields).expect("seed");
        assert_eq!(
            wrote,
            ["temperature", "humidity", "w1_1_temperature", "illuminance"]
        );
        let rows = storage
            .list_field_thresholds("hub-1", THRESHOLD_SCOPE_NODE, "aa11")
            .expect("list");
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| !r.enabled), "stored switched off");
        let l = rows.iter().find(|r| r.field == "illuminance").expect("row");
        assert_eq!(
            (
                l.critical_low,
                l.warning_low,
                l.warning_high,
                l.critical_high
            ),
            (Some(0.0), None, None, Some(10_000.0))
        );
        let t = rows.iter().find(|r| r.field == "temperature").expect("row");
        assert_eq!(
            (
                t.critical_low,
                t.warning_low,
                t.warning_high,
                t.critical_high
            ),
            (Some(-10.0), Some(0.0), Some(40.0), Some(50.0))
        );
        let h = rows.iter().find(|r| r.field == "humidity").expect("row");
        assert_eq!(
            (
                h.critical_low,
                h.warning_low,
                h.warning_high,
                h.critical_high
            ),
            (Some(0.0), Some(10.0), Some(80.0), Some(90.0))
        );
        assert!(
            seed(&storage, &d, "hub-1", THRESHOLD_SCOPE_NODE, "aa11", fields)
                .expect("again")
                .is_empty()
        );

        // An operator's own rows on another Node, one on, one off: both win.
        for (field, enabled) in [("temperature", true), ("humidity", false)] {
            storage
                .upsert_field_threshold(
                    &NewFieldThreshold {
                        hostname: "hub-1".into(),
                        scope: THRESHOLD_SCOPE_NODE.into(),
                        entity: "bb22".into(),
                        field: field.into(),
                        critical_low: Some(2.0),
                        warning_low: None,
                        warning_high: None,
                        critical_high: Some(8.0),
                        enabled,
                    },
                    Some("manager1"),
                )
                .expect("operator row");
        }
        let wrote = seed(
            &storage,
            &d,
            "hub-1",
            THRESHOLD_SCOPE_NODE,
            "bb22",
            ["temperature", "humidity", "w1_2_humidity"],
        )
        .expect("seed");
        assert_eq!(wrote, ["w1_2_humidity"], "only the field without a row");
        let rows = storage
            .list_field_thresholds("hub-1", THRESHOLD_SCOPE_NODE, "bb22")
            .expect("list");
        let t = rows.iter().find(|r| r.field == "temperature").expect("row");
        assert!(t.enabled && t.critical_low == Some(2.0), "{t:?}");
        let h = rows.iter().find(|r| r.field == "humidity").expect("row");
        assert!(!h.enabled && h.critical_high == Some(8.0), "{h:?}");
    }

    #[test]
    fn the_battery_is_not_graded_and_everything_else_is() {
        assert!(!node_field_is_graded("voltage"));
        assert!(node_field_is_graded("temperature"));
        assert!(node_field_is_graded("w1_3_humidity"));
        assert!(node_field_is_graded("a_field_from_a_newer_node"));
        assert!(is_battery("voltage"));
        assert!(!is_battery("temperature"));
    }
}
