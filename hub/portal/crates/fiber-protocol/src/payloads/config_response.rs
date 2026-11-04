//! `{prefix}/{host}/config/response` — QoS 2, not retained. mqtt.md
//! "config/response" section: resolves orchestrated commands and settles
//! the display-lines record.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigResponseMsg {
    pub challenge_id: String,
    pub request_id: Option<String>,
    pub status: String,
    pub applied_at: Option<i64>,
    pub effective_at: Option<i64>,
    pub message: Option<String>,
    /// A verb's structured answer, for the few that have one: today
    /// `cluster-client-add` / `cluster-client-remove` (plan/control/cluster.md
    /// §7). Absent on every other response, and from devices older than the
    /// field. Present on an `ERROR` too when the verb did part of its work —
    /// a minted account whose gateway failed still carries its password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResponseResult>,
}

/// The `result` document. Its own type rather than a bare `Value` so that
/// `Debug` — and so every `{:?}` of a response, or of an event carrying one —
/// prints the keys and never the values: `cluster-client-add`'s holds a
/// broker password that must reach the operator once and no log line.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResponseResult(pub serde_json::Value);

impl std::fmt::Debug for ResponseResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self
            .0
            .as_object()
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        write!(f, "ResponseResult {{ keys: {keys:?}, values: <redacted> }}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_spec_example() {
        let json = r#"{
            "challenge_id": "chal-1", "request_id": "req-1", "status": "SUCCESS",
            "applied_at": 1736775060, "effective_at": null, "message": "ok"
        }"#;
        let msg: ConfigResponseMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.status, "SUCCESS");
        assert_eq!(msg.effective_at, None);
        assert!(msg.result.is_none(), "absent on every ordinary response");
    }

    #[test]
    fn a_result_round_trips_and_never_prints_its_values() {
        let json = r#"{
            "challenge_id": "chal-1", "request_id": "req-1", "status": "SUCCESS",
            "applied_at": 1, "effective_at": 1, "message": "Configuration applied",
            "result": {"account": "ok", "gateway": "skipped", "password": "s3cr3t-s3cr3t-s3cr3t-x"}
        }"#;
        let msg: ConfigResponseMsg = serde_json::from_str(json).unwrap();
        let result = msg.result.as_ref().expect("result");
        assert_eq!(result.0["password"], "s3cr3t-s3cr3t-s3cr3t-x");
        let printed = format!("{msg:?}");
        assert!(!printed.contains("s3cr3t"), "{printed}");
        assert!(
            printed.contains("password"),
            "the keys are shown: {printed}"
        );
        let back = serde_json::to_value(&msg).unwrap();
        assert_eq!(back["result"]["account"], "ok");
        let plain: ConfigResponseMsg = serde_json::from_str(
            r#"{"challenge_id":"c","request_id":null,"status":"SUCCESS",
                "applied_at":null,"effective_at":null,"message":null}"#,
        )
        .unwrap();
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("result")
            .is_none());
    }
}
