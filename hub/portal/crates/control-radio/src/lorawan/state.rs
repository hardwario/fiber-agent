//! `config.json` — the converged-id record — `plan/control/radio/lorawan.md` §5.
//!
//! Lives at `/data/proximos/radio/lorawan/config.json`. Two jobs: it is what makes `create`
//! idempotent across reboots, and it is where the agent seeds `app_id` at startup — which is
//! what closes the v1 defect where downlinks silently died after every restart because
//! `app_id` was only ever learned from a live uplink.
//!
//! The shape is deliberately backward-compatible in both directions. v1's top-level keys are
//! kept, `device_profile_ids` is added alongside them for the named-map world, and v1's
//! read-side fallback (`device_profile_id` → `device_profile_id_abp`) is honoured so a
//! hand-written file from a bench device still parses.

use std::collections::BTreeMap;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::eui::Eui;

/// The conventional profile name for a fleet that defines only one per arm.
pub const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConvergedState {
    /// Resolved, never created — ChirpStack v4 ships a default tenant and `create` takes the
    /// first entry. This is also why `wipe` must not delete it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,

    /// The **`default`** OTAA profile's id specifically, kept at the top level so a
    /// single-profile fleet round-trips through v1's shape unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_profile_id_otaa: Option<String>,

    /// The `default` ABP profile's id. See `device_profile_id_otaa`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_profile_id_abp: Option<String>,

    /// v1's legacy single-profile key. Read-only compatibility: `lorawan-setup.sh` step 3b
    /// treated it as the ABP profile, so a bench file carrying it still resolves. Never
    /// written by v2 — [`ConvergedState::normalize`] folds it into `device_profile_id_abp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_profile_id: Option<String>,

    /// Every named profile's id, per arm. The only place a non-`default` profile's id lives.
    #[serde(default, skip_serializing_if = "ProfileIds::is_empty")]
    pub device_profile_ids: ProfileIds,

    /// Client-chosen, not server-assigned — which is what makes gateway converge idempotent
    /// by construction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_id: Option<Eui>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileIds {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub otaa: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub abp: BTreeMap<String, String>,
}

impl ProfileIds {
    pub fn is_empty(&self) -> bool {
        self.otaa.is_empty() && self.abp.is_empty()
    }
}

/// Which activation arm a profile belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Otaa,
    Abp,
}

impl Activation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Activation::Otaa => "otaa",
            Activation::Abp => "abp",
        }
    }
}

impl ConvergedState {
    pub fn from_json(s: &str) -> Result<Self> {
        let mut parsed: ConvergedState = serde_json::from_str(s).map_err(|e| {
            Error::new(
                Kind::StorageError,
                Transport::Lorawan,
                Component::ConfigJson,
                format!("config.json is not readable as converged state: {e}"),
            )
        })?;
        parsed.normalize();
        Ok(parsed)
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| {
            Error::new(
                Kind::StorageError,
                Transport::Lorawan,
                Component::ConfigJson,
                format!("converged state could not be serialized: {e}"),
            )
        })
    }

    /// Reconcile the legacy and named-map views so callers only ever read one of them.
    ///
    /// - v1's bare `device_profile_id` means the ABP profile (`lorawan-setup.sh` step 3b).
    /// - The two top-level keys and the `default` entries of the map are the same fact, so
    ///   whichever is present populates the other.
    pub fn normalize(&mut self) {
        if let Some(legacy) = self.device_profile_id.take() {
            self.device_profile_id_abp.get_or_insert(legacy);
        }
        sync_default(
            &mut self.device_profile_id_otaa,
            &mut self.device_profile_ids.otaa,
        );
        sync_default(
            &mut self.device_profile_id_abp,
            &mut self.device_profile_ids.abp,
        );
    }

    /// Resolve a named profile's ChirpStack id.
    pub fn profile_id(&self, activation: Activation, name: &str) -> Option<&str> {
        let map = match activation {
            Activation::Otaa => &self.device_profile_ids.otaa,
            Activation::Abp => &self.device_profile_ids.abp,
        };
        map.get(name).map(String::as_str)
    }

    /// Record a converged profile id under both views, so the file stays self-consistent.
    pub fn set_profile_id(&mut self, activation: Activation, name: &str, id: impl Into<String>) {
        let id = id.into();
        let (legacy, map) = match activation {
            Activation::Otaa => (
                &mut self.device_profile_id_otaa,
                &mut self.device_profile_ids.otaa,
            ),
            Activation::Abp => (
                &mut self.device_profile_id_abp,
                &mut self.device_profile_ids.abp,
            ),
        };
        map.insert(name.to_string(), id.clone());
        if name == DEFAULT_PROFILE {
            *legacy = Some(id);
        }
    }

    /// The `app_id` the decode path seeds at startup — protocols.md quirk 1.
    pub fn application_id(&self) -> Result<&str> {
        self.application_id.as_deref().ok_or_else(|| {
            Error::new(
                Kind::NotBootstrapped,
                Transport::Lorawan,
                Component::ConfigJson,
                "no application_id in config.json: run `create --radio lorawan` first",
            )
        })
    }
}

/// Keep a top-level legacy key and the `default` entry of its map in agreement, in whichever
/// direction has the value.
fn sync_default(legacy: &mut Option<String>, map: &mut BTreeMap<String, String>) {
    match (legacy.clone(), map.get(DEFAULT_PROFILE).cloned()) {
        (Some(l), None) => {
            map.insert(DEFAULT_PROFILE.to_string(), l);
        }
        (None, Some(m)) => *legacy = Some(m),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// lorawan.md §5's own example file.
    const PLAN_EXAMPLE: &str = r#"{
      "tenant_id": "52f14cd4-c6f1-4fbd-8f87-4025e1d49242",
      "application_id": "11111111-1111-1111-1111-111111111111",
      "device_profile_id_otaa": "22222222-2222-2222-2222-222222222222",
      "device_profile_id_abp": "33333333-3333-3333-3333-333333333333",
      "device_profile_ids": {
        "otaa": { "default": "22222222-2222-2222-2222-222222222222" },
        "abp": { "default": "33333333-3333-3333-3333-333333333333" }
      },
      "gateway_id": "0011223344556677"
    }"#;

    #[test]
    fn the_plans_example_file_round_trips() {
        let s = ConvergedState::from_json(PLAN_EXAMPLE).expect("parses");
        assert_eq!(
            s.tenant_id.as_deref(),
            Some("52f14cd4-c6f1-4fbd-8f87-4025e1d49242")
        );
        assert_eq!(
            s.profile_id(Activation::Otaa, "default"),
            Some("22222222-2222-2222-2222-222222222222")
        );
        assert_eq!(
            s.profile_id(Activation::Abp, "default"),
            Some("33333333-3333-3333-3333-333333333333")
        );
        assert_eq!(
            s.gateway_id.as_ref().map(Eui::as_str),
            Some("0011223344556677")
        );

        let again = ConvergedState::from_json(&s.to_json().expect("serializes")).expect("reparses");
        assert_eq!(s, again);
    }

    /// v1's legacy single-profile key means the ABP profile.
    #[test]
    fn the_legacy_device_profile_id_falls_back_to_abp() {
        let s = ConvergedState::from_json(
            r#"{"tenant_id":"t","application_id":"a","device_profile_id":"legacy-abp"}"#,
        )
        .expect("a hand-written bench file must still parse");
        assert_eq!(s.device_profile_id_abp.as_deref(), Some("legacy-abp"));
        assert_eq!(s.profile_id(Activation::Abp, "default"), Some("legacy-abp"));
        // Never written back out under the legacy name.
        assert!(!s
            .to_json()
            .expect("serializes")
            .contains("\"device_profile_id\":"));
    }

    /// A single-profile fleet round-trips through v1's shape unchanged: writing from the
    /// named map repopulates the top-level keys.
    #[test]
    fn the_default_profile_is_visible_under_both_views() {
        let mut s = ConvergedState::default();
        s.set_profile_id(Activation::Otaa, "default", "otaa-id");
        assert_eq!(s.device_profile_id_otaa.as_deref(), Some("otaa-id"));
        assert_eq!(s.profile_id(Activation::Otaa, "default"), Some("otaa-id"));

        // And in the other direction, for a file that only carries the map.
        let from_map = ConvergedState::from_json(
            r#"{"device_profile_ids":{"otaa":{"default":"only-in-map"}}}"#,
        )
        .expect("parses");
        assert_eq!(
            from_map.device_profile_id_otaa.as_deref(),
            Some("only-in-map")
        );
    }

    /// A non-`default` profile's id lives only in the map — that is the whole reason the map
    /// exists.
    #[test]
    fn non_default_profiles_do_not_touch_the_legacy_keys() {
        let mut s = ConvergedState::default();
        s.set_profile_id(Activation::Otaa, "cold-chain", "cc-id");
        assert_eq!(s.profile_id(Activation::Otaa, "cold-chain"), Some("cc-id"));
        assert_eq!(s.device_profile_id_otaa, None);
    }

    #[test]
    fn a_missing_application_id_is_not_bootstrapped_not_a_panic() {
        let err = ConvergedState::default()
            .application_id()
            .expect_err("must not be reachable before create");
        assert_eq!(err.kind(), Kind::NotBootstrapped);
    }

    #[test]
    fn a_malformed_gateway_eui_in_stored_state_is_a_storage_error() {
        let err = ConvergedState::from_json(r#"{"gateway_id":"not-an-eui"}"#)
            .expect_err("a bad EUI on disk must surface, not be kept");
        assert_eq!(err.kind(), Kind::StorageError);
        assert_eq!(err.component(), Component::ConfigJson);
    }
}
