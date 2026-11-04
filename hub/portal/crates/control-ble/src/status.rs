//! `status` and the one command-result envelope — `plan/control/ble/plan.md` §5.
//!
//! `status --json` and `beacon-list --json` are consumed over SSH by the provisioning wizard and the
//! fleet updater, so they are **contract**: additive-only evolution, exactly like the exit codes.
//!
//! `status` **never fails for a reason it exists to report.** An absent adapter, a dead scan loop, a
//! disabled capability — all of those are things the operator asked about, so they are fields, not
//! errors. The one-stop view being unavailable precisely when something is wrong would make it
//! useless.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdapterReport {
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub powered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct En12830Report {
    pub enabled: bool,
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CentralReport {
    pub provisioned: bool,
    pub scanning: bool,
    /// `eye.enabled` — the runtime flag, distinct from whether a loop is hosted at all.
    pub enabled: bool,
    pub beacons: u32,
    pub discovered: u32,
    /// The wire also carries this on every snapshot; both must agree.
    pub discovered_capped: bool,
    /// §7's counter: "we are seeing frames we do not understand" made visible.
    pub unparseable_frames: u64,
    pub en12830: En12830Report,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionReport {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub authenticated: bool,
    /// The authenticated peer's address. **Never the token** — that appears only on the LCD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PeripheralReport {
    pub provisioned: bool,
    pub advertising: bool,
    pub session: SessionReport,
}

/// Produce-only: `ble_mode` is a `&'static str` from the mode table, so deriving `Deserialize`
/// would only ever yield a `Deserialize<'static>` impl. The consumers (the provisioning wizard and
/// the fleet updater) parse this over SSH with their own types; this crate's job is to emit it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StatusReport {
    pub ble_mode: &'static str,
    pub adapter: AdapterReport,
    /// Absent when the mode excludes the role — an empty block would read as "central exists and is
    /// idle", which is a different fact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub central: Option<CentralReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peripheral: Option<PeripheralReport>,
}

/// One result envelope for every mutating command.
///
/// `success` is always `true` here: a failure is the §6 error envelope, not a result with a flag.
/// The two shapes are deliberately different so a caller cannot mistake one for the other.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CommandResult {
    pub success: bool,
    pub command: &'static str,
    pub role: &'static str,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

impl CommandResult {
    pub fn new(command: &'static str, role: &'static str) -> CommandResult {
        CommandResult {
            success: true,
            command,
            role,
            fields: Map::new(),
        }
    }

    /// Add a field. Chained, so a dispatch arm reads as the document it produces.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> CommandResult {
        self.fields.insert(key.to_string(), value.into());
        self
    }

    /// Add a field from anything serializable.
    pub fn with_json(mut self, key: &str, value: &impl serde::Serialize) -> CommandResult {
        if let Ok(v) = serde_json::to_value(value) {
            self.fields.insert(key.to_string(), v);
        }
        self
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_envelope_carries_success_command_and_role_plus_its_own_fields() {
        let r = CommandResult::new("beacon-add", "central")
            .with("mac", "AA:BB:CC:DD:EE:FF")
            .with("monitoring", "enabled_by_this_call")
            .with("already_enrolled", false);
        let v = r.to_value();
        assert_eq!(v["success"], true);
        assert_eq!(v["command"], "beacon-add");
        assert_eq!(v["role"], "central");
        // Flattened, so there is no nested wrapper level for a caller to unwrap.
        assert_eq!(v["mac"], "AA:BB:CC:DD:EE:FF");
        assert_eq!(v["monitoring"], "enabled_by_this_call");
        assert_eq!(v["already_enrolled"], false);
        assert!(v.get("fields").is_none(), "flattened, not nested");
    }

    #[test]
    fn a_status_report_omits_a_role_the_mode_excludes() {
        let s = StatusReport {
            ble_mode: "central",
            adapter: AdapterReport {
                present: true,
                address: Some("B8:27:EB:11:22:33".to_string()),
                powered: true,
            },
            central: Some(CentralReport {
                provisioned: true,
                scanning: true,
                enabled: true,
                beacons: 12,
                discovered: 4,
                discovered_capped: false,
                unparseable_frames: 0,
                en12830: En12830Report {
                    enabled: true,
                    consecutive_failures: 0,
                },
            }),
            peripheral: None,
        };
        let v = serde_json::to_value(&s).expect("serialize");
        assert_eq!(v["ble_mode"], "central");
        assert_eq!(v["central"]["beacons"], 12);
        assert_eq!(v["central"]["unparseable_frames"], 0);
        assert!(
            v.get("peripheral").is_none(),
            "absent, not an empty block: an empty one would read as `exists and is idle`"
        );
        // The blocks a consumer parses do round-trip on their own.
        let back: CentralReport = serde_json::from_value(v["central"].clone()).expect("round-trip");
        assert_eq!(Some(back), s.central);
    }

    #[test]
    fn a_status_report_with_no_adapter_still_carries_both_role_blocks() {
        // The degraded-green case: `status` must answer, and the answer is that there is no
        // controller — not an error.
        let s = StatusReport {
            ble_mode: "dual",
            adapter: AdapterReport {
                present: false,
                address: None,
                powered: false,
            },
            central: Some(CentralReport {
                provisioned: false,
                scanning: false,
                enabled: true,
                beacons: 0,
                discovered: 0,
                discovered_capped: false,
                unparseable_frames: 0,
                en12830: En12830Report {
                    enabled: true,
                    consecutive_failures: 0,
                },
            }),
            peripheral: Some(PeripheralReport {
                provisioned: false,
                advertising: false,
                session: SessionReport {
                    active: false,
                    expires_at: None,
                    authenticated: false,
                    peer: None,
                },
            }),
        };
        let v = serde_json::to_value(&s).expect("serialize");
        assert_eq!(v["adapter"]["present"], false);
        assert!(v["adapter"].get("address").is_none());
        assert!(v.get("central").is_some());
        assert!(v.get("peripheral").is_some());
    }

    #[test]
    fn the_session_block_never_carries_a_token() {
        let s = SessionReport {
            active: true,
            expires_at: Some(1_700_000_300),
            authenticated: true,
            peer: Some("11:22:33:44:55:66".to_string()),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        for forbidden in ["tok", "token", "pin", "credential"] {
            assert!(
                !json.contains(forbidden),
                "{forbidden} must never appear in a status document: {json}"
            );
        }
        assert!(json.contains("expires_at"));
        assert!(json.contains("11:22:33:44:55:66"));
    }
}
