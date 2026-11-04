//! `{prefix}/{host}/config/challenge` — QoS 2, not retained. mqtt.md
//! "config/challenge" section: the portal only ever reads `challenge_id`
//! and `request_id` (see `handle_config_challenge`,
//! `handlers.py:1312-1364`); `preview` is opaque here (untyped
//! `serde_json::Value`) since no consumer needs its shape yet.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigChallengeMsg {
    pub challenge_id: String,
    pub request_id: Option<String>,
    pub signer_id: Option<String>,
    pub expires_at: Option<i64>,
    pub preview: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_spec_example() {
        let json = r#"{
            "challenge_id": "chal-1", "request_id": "req-1", "signer_id": "admin",
            "expires_at": 1736775060, "preview": {"command_type": "restart_application"}
        }"#;
        let msg: ConfigChallengeMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.challenge_id, "chal-1");
        assert_eq!(msg.request_id.as_deref(), Some("req-1"));
    }
}
