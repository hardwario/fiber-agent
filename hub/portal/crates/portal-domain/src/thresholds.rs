//! Which threshold applies to a Node field when two sources disagree.
//!
//! A Node's bounds arrive from two places: the firmware echo on every
//! aggregate publish (`lorawan/sensors` `field_thresholds`, authoritative
//! when present), and the row a manager last wrote locally
//! (`field_thresholds`, scope `node`). The rule, per field:
//!
//! - its **bounds** come from the stored row when there is one: the portal is
//!   the source of truth and grades every bound (decision #41), so the Hub's
//!   echo lags or may never catch up (an offline Hub, a refused command).
//!   With no stored row the echo's bounds apply. *Amended 2026-10-15:* until
//!   then an echoed field's critical bounds won outright, and a saved band
//!   kept showing the Hub's older critical pair;
//! - its **`enabled`** comes from the stored row when there is one — a
//!   manager may have switched off a threshold whose bounds the firmware
//!   still echoes — and defaults to `true`, because an echoed threshold with
//!   no local override is active by definition;
//! - a field only the store knows (set locally, not yet echoed back) keeps
//!   its row as-is;
//! - a field in neither source is absent from the result.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::types::ThresholdBounds;

/// The threshold a Node field is actually judged against.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EffectiveThreshold {
    pub bounds: ThresholdBounds,
    pub enabled: bool,
}

/// Merges the firmware echo (`echo`, `None` when the Node has not echoed a
/// list yet) with the stored rows (`stored`, `(field, bounds, enabled)`),
/// keyed by field — see the module doc for the precedence.
pub fn merge_node_thresholds<'a>(
    echo: Option<impl IntoIterator<Item = (&'a str, ThresholdBounds)>>,
    stored: impl IntoIterator<Item = (&'a str, ThresholdBounds, bool)>,
) -> BTreeMap<String, EffectiveThreshold> {
    let mut out: BTreeMap<String, EffectiveThreshold> = stored
        .into_iter()
        .map(|(field, bounds, enabled)| (field.to_string(), EffectiveThreshold { bounds, enabled }))
        .collect();

    if let Some(echo) = echo {
        for (field, mut bounds) in echo {
            let stored = out.get(field);
            let enabled = stored.map(|t| t.enabled).unwrap_or(true);
            if let Some(stored) = stored {
                bounds = stored.bounds;
            }
            out.insert(field.to_string(), EffectiveThreshold { bounds, enabled });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn high(warning_high: f64) -> ThresholdBounds {
        ThresholdBounds {
            warning_high: Some(warning_high),
            ..Default::default()
        }
    }

    #[test]
    fn stored_bounds_and_enabled_win_over_the_echo() {
        let echoed = ThresholdBounds {
            critical_high: Some(40.0),
            ..high(35.0)
        };
        let stored = ThresholdBounds {
            critical_high: Some(38.0),
            ..high(30.0)
        };
        let merged = merge_node_thresholds(
            Some([("temperature", echoed)]),
            [("temperature", stored, false)],
        );
        let t = &merged["temperature"];
        assert_eq!(
            t.bounds.critical_high,
            Some(38.0),
            "stored critical bounds win (#41)"
        );
        assert_eq!(
            t.bounds.warning_high,
            Some(30.0),
            "stored warnings win (#41)"
        );
        assert!(!t.enabled, "the stored enabled flag survives an echo");
    }

    #[test]
    fn echo_only_field_is_enabled_by_default() {
        let merged = merge_node_thresholds(Some([("humidity", high(80.0))]), []);
        let t = &merged["humidity"];
        assert_eq!(t.bounds.warning_high, Some(80.0));
        assert!(
            t.enabled,
            "no local override: an echoed threshold is active"
        );
    }

    #[test]
    fn stored_only_field_is_kept_as_is() {
        let merged = merge_node_thresholds(
            None::<[(&str, ThresholdBounds); 0]>,
            [("illuminance", high(1000.0), true)],
        );
        assert_eq!(merged["illuminance"].bounds.warning_high, Some(1000.0));
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn field_in_neither_source_is_absent() {
        let merged = merge_node_thresholds(None::<[(&str, ThresholdBounds); 0]>, []);
        assert!(merged.is_empty());
    }
}
