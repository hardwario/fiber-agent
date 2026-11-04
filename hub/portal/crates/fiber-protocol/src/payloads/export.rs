//! `{prefix}/{host}/export/{stream}/{key}` — firmware DB mirror
//! ("save-and-feed"). mqtt.md "export" section. Envelope is the same
//! across every stream; only `probe_1m`/`probe_1m_replay` get a typed
//! `data` here (the others are ledger-only raw DB rows in v1, with no
//! shape pinned in mqtt.md beyond prose).

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

fn default_stream_version() -> i64 {
    1
}

/// Common envelope for every `export/*` stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEnvelope<T> {
    /// Required — doubles as the ledger dedup key.
    pub message_id: String,
    pub exported_at: Option<i64>,
    pub stream: Option<String>,
    #[serde(default = "default_stream_version")]
    pub stream_version: i64,
    pub data: T,
}

/// `export/probe_1m/{line}` and (with `request_id`, see
/// [`Probe1mReplayData`]) `export/probe_1m_replay/{request_id}/{line}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe1mData {
    pub minute_ts: Option<i64>,
    pub sensor_line: Option<i64>,
    pub min_c: Option<f64>,
    pub avg_c: Option<f64>,
    pub max_c: Option<f64>,
    pub sample_count: Option<i64>,
    pub disconnect_count: Option<i64>,
    pub worst_alarm: Option<String>,
    /// Accepted as a byte array, a hex string, or `null` — stored
    /// unverified either way (mqtt.md: "`data_hmac` accepted as u8 array /
    /// hex / null, stored unverified").
    #[serde(default, deserialize_with = "deserialize_data_hmac")]
    pub data_hmac: Option<Vec<u8>>,
}

/// `export/probe_1m_replay/{request_id}/{line}` — same shape as
/// `Probe1mData` plus a top-level `request_id`; stored with
/// `source="replay"`.
///
/// `Serialize` is derived (with `#[serde(flatten)]`, which is what puts the
/// `Probe1mData` fields at the top level on the wire); `Deserialize` is hand
/// written — see the impl below for why. The two must stay symmetric: the wire
/// form is one flat object.
#[derive(Debug, Clone, Serialize)]
pub struct Probe1mReplayData {
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub probe: Probe1mData,
}

/// Hand-written to keep `#[serde(flatten)]` out of the *deserialize* path.
///
/// `control-core` enables `serde_json/arbitrary_precision`, and under Cargo's
/// resolver 2 that feature unifies into this crate for any single-invocation
/// workspace build (`cargo build`/`cargo test --workspace`). With the feature
/// on, `parse_any_number` (serde_json `de.rs`) returns a real `U64`/`I64` for
/// anything that parses as an integer, but `ParserNumber::String` for every
/// other number — a float, or an integer too large for 64 bits. `String`'s
/// `visit` hands the visitor a **one-key map** whose key is the private token
/// `$serde_json::private::Number`.
///
/// `#[serde(flatten)]` buffers the flattened half of the object into serde's
/// `Content` before replaying it into the inner struct, and serde's
/// `ContentDeserializer` knows nothing about that token: replaying the buffered
/// one-key map into `Option<f64>` fails with
/// `invalid type: map, expected f64`. So `min_c`/`avg_c`/`max_c` — every
/// temperature this stream exists to carry — could not be parsed at all under a
/// workspace build, while the integer fields next to them parsed fine.
///
/// The fix buffers through `serde_json::Value` instead. `Value`'s own
/// `Deserialize` recognises the private token and rebuilds a `Number` from it,
/// so numbers survive the detour with full precision; `request_id` is lifted
/// off the top level and the remainder is handed to `Probe1mData`. No field is
/// listed twice, so adding a field to `Probe1mData` still Just Works, and the
/// serialized form is untouched (v1 wire compatibility).
impl<'de> Deserialize<'de> for Probe1mReplayData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let request_id = match map.remove("request_id") {
            None | Some(serde_json::Value::Null) => None,
            Some(other) => Some(String::deserialize(other).map_err(de::Error::custom)?),
        };
        let probe =
            Probe1mData::deserialize(serde_json::Value::Object(map)).map_err(de::Error::custom)?;
        Ok(Probe1mReplayData { request_id, probe })
    }
}

/// `export/sticker/{dev_eui}` — ledger only, `payload` shape is
/// firmware-defined and not pinned in mqtt.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StickerExportData {
    pub dev_eui: Option<String>,
    pub provisioning_epoch: Option<i64>,
    pub ts: Option<String>,
    pub received_at: Option<String>,
    pub event_type: Option<String>,
    pub payload: Option<serde_json::Value>,
}

/// `export/eye/{MAC}` — deduped by device `message_id`, feeds
/// `eye_readings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeExportData {
    pub mac: Option<String>,
    pub ts: Option<String>,
    pub received_at: Option<String>,
    /// `"advertising"` or `"recording"`.
    pub event_type: Option<String>,
    pub payload: Option<EyeExportPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeExportPayload {
    pub temperature_c: Option<f64>,
    pub humidity_pct: Option<i64>,
    pub ts: Option<String>,
}

fn deserialize_data_hmac<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct HmacVisitor;

    impl<'de> Visitor<'de> for HmacVisitor {
        type Value = Option<Vec<u8>>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a hex string, an array of bytes, or null")
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            hex::decode(v).map(Some).map_err(E::custom)
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(v) = seq.next_element::<u8>()? {
                out.push(v);
            }
            Ok(Some(out))
        }
    }

    deserializer.deserialize_any(HmacVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_deserializes_with_typed_probe_1m_data() {
        let json = r#"{
            "message_id": "m1", "exported_at": 1736775060, "stream": "probe_1m", "stream_version": 1,
            "data": {
                "minute_ts": 1736775000, "sensor_line": 0,
                "min_c": 36.5, "avg_c": 36.8, "max_c": 37.2,
                "sample_count": 60, "disconnect_count": 0, "worst_alarm": "NORMAL",
                "data_hmac": null
            }
        }"#;
        let env: ExportEnvelope<Probe1mData> = serde_json::from_str(json).unwrap();
        assert_eq!(env.message_id, "m1");
        assert_eq!(env.data.avg_c, Some(36.8));
        assert!(env.data.data_hmac.is_none());
    }

    #[test]
    fn stream_version_defaults_to_1() {
        let json = r#"{"message_id": "m1", "data": {}}"#;
        let env: ExportEnvelope<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert_eq!(env.stream_version, 1);
    }

    #[test]
    fn data_hmac_accepts_hex_string() {
        let json = r#"{"minute_ts": 1, "sensor_line": 0, "min_c": null, "avg_c": null, "max_c": null,
            "sample_count": 0, "disconnect_count": 0, "worst_alarm": null, "data_hmac": "deadbeef"}"#;
        let data: Probe1mData = serde_json::from_str(json).unwrap();
        assert_eq!(data.data_hmac, Some(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn data_hmac_accepts_byte_array() {
        let json = r#"{"minute_ts": 1, "sensor_line": 0, "min_c": null, "avg_c": null, "max_c": null,
            "sample_count": 0, "disconnect_count": 0, "worst_alarm": null, "data_hmac": [222, 173, 190, 239]}"#;
        let data: Probe1mData = serde_json::from_str(json).unwrap();
        assert_eq!(data.data_hmac, Some(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn data_hmac_missing_is_none() {
        let json = r#"{"minute_ts": 1, "sensor_line": 0, "min_c": null, "avg_c": null, "max_c": null,
            "sample_count": 0, "disconnect_count": 0, "worst_alarm": null}"#;
        let data: Probe1mData = serde_json::from_str(json).unwrap();
        assert!(data.data_hmac.is_none());
    }

    #[test]
    fn replay_data_carries_request_id_alongside_probe_fields() {
        let json = r#"{"request_id": "r1", "minute_ts": 1, "sensor_line": 0, "min_c": null,
            "avg_c": null, "max_c": null, "sample_count": 0, "disconnect_count": 0,
            "worst_alarm": null, "data_hmac": null}"#;
        let data: Probe1mReplayData = serde_json::from_str(json).unwrap();
        assert_eq!(data.request_id.as_deref(), Some("r1"));
        assert_eq!(data.probe.sensor_line, Some(0));
    }

    /// Regression guard for `serde_json`'s `arbitrary_precision` feature.
    ///
    /// `control-core` enables `serde_json/arbitrary_precision`, and under
    /// Cargo's resolver 2 that feature unifies into *this* crate whenever the
    /// workspace is built in one invocation (`cargo build/test --workspace`).
    /// With the feature on, every JSON number is deserialized through an
    /// internal one-key map (`$serde_json::private::Number`), and
    /// `#[serde(flatten)]` buffers the flattened part as `serde::Content`
    /// before replaying it — the historical combination that made numbers
    /// inside a flattened struct fail to deserialize (serde_json #721).
    ///
    /// `replay_data_carries_request_id_alongside_probe_fields` above only
    /// covers integers with every float set to `null`, so floats through the
    /// flatten path were unproven at the wire. This pins them, from real JSON
    /// text, and pins that re-serialization does not leak the private
    /// `$serde_json::private::Number` key onto the wire. It must pass with the
    /// feature both on (workspace build) and off (`-p fiber-protocol`).
    #[test]
    fn replay_data_deserializes_floats_through_flatten_under_arbitrary_precision() {
        let json = r#"{
            "request_id": "r-2026-01-13-0001",
            "minute_ts": 1736775000, "sensor_line": 1,
            "min_c": 36.5, "avg_c": 36.8125, "max_c": -0.5,
            "sample_count": 60, "disconnect_count": 2,
            "worst_alarm": "WARNING_HIGH",
            "data_hmac": "deadbeef"
        }"#;
        let data: Probe1mReplayData = serde_json::from_str(json).unwrap();

        assert_eq!(data.request_id.as_deref(), Some("r-2026-01-13-0001"));
        assert_eq!(data.probe.minute_ts, Some(1736775000));
        assert_eq!(data.probe.sensor_line, Some(1));
        assert_eq!(data.probe.min_c, Some(36.5));
        assert_eq!(data.probe.avg_c, Some(36.8125));
        assert_eq!(data.probe.max_c, Some(-0.5));
        assert_eq!(data.probe.sample_count, Some(60));
        assert_eq!(data.probe.disconnect_count, Some(2));
        assert_eq!(data.probe.worst_alarm.as_deref(), Some("WARNING_HIGH"));
        assert_eq!(data.probe.data_hmac, Some(vec![0xde, 0xad, 0xbe, 0xef]));

        // Serialized output must stay v1-compatible: flat keys, no private
        // serde_json marker object wrapping the numbers.
        let out = serde_json::to_string(&data).unwrap();
        assert!(
            !out.contains("$serde_json::private::Number"),
            "arbitrary_precision leaked its private number marker: {out}"
        );
        let reparsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed["request_id"], "r-2026-01-13-0001");
        assert_eq!(reparsed["avg_c"].as_f64(), Some(36.8125));
        assert_eq!(reparsed["minute_ts"].as_i64(), Some(1736775000));

        // And the same payload survives a full round-trip.
        let again: Probe1mReplayData = serde_json::from_str(&out).unwrap();
        assert_eq!(again.probe.avg_c, Some(36.8125));
        assert_eq!(again.probe.max_c, Some(-0.5));
    }

    /// The hand-written `Deserialize` on [`Probe1mReplayData`] must not have
    /// moved the wire form: one flat object, `request_id` first, then the
    /// `Probe1mData` fields in declaration order. Pinned literally so a future
    /// edit to the impl cannot quietly reshape a v1-compatible payload.
    #[test]
    fn replay_data_serializes_to_one_flat_object() {
        let data = Probe1mReplayData {
            request_id: Some("r1".into()),
            probe: Probe1mData {
                minute_ts: Some(1736775000),
                sensor_line: Some(0),
                min_c: Some(36.5),
                avg_c: Some(36.8),
                max_c: Some(37.2),
                sample_count: Some(60),
                disconnect_count: Some(0),
                worst_alarm: Some("NORMAL".into()),
                data_hmac: Some(vec![0xde, 0xad]),
            },
        };
        assert_eq!(
            serde_json::to_string(&data).unwrap(),
            r#"{"request_id":"r1","minute_ts":1736775000,"sensor_line":0,"min_c":36.5,"avg_c":36.8,"max_c":37.2,"sample_count":60,"disconnect_count":0,"worst_alarm":"NORMAL","data_hmac":[222,173]}"#
        );
    }

    /// A missing or null `request_id` is a plain `None`, not an error — the
    /// hand-written impl has to reproduce what `Option` did under the derive.
    #[test]
    fn replay_data_tolerates_absent_and_null_request_id() {
        let absent: Probe1mReplayData =
            serde_json::from_str(r#"{"minute_ts": 1, "avg_c": 1.5}"#).unwrap();
        assert!(absent.request_id.is_none());
        assert_eq!(absent.probe.avg_c, Some(1.5));

        let null: Probe1mReplayData =
            serde_json::from_str(r#"{"request_id": null, "minute_ts": 1, "avg_c": 1.5}"#).unwrap();
        assert!(null.request_id.is_none());
        assert_eq!(null.probe.avg_c, Some(1.5));
    }

    /// The same guard one level up: numbers inside the flattened part of a
    /// full `export/probe_1m_replay` envelope.
    #[test]
    fn replay_envelope_round_trips_floats_under_arbitrary_precision() {
        let json = r#"{
            "message_id": "m-replay-1", "exported_at": 1736775060,
            "stream": "probe_1m_replay", "stream_version": 1,
            "data": {
                "request_id": "r1",
                "minute_ts": 1736775000, "sensor_line": 0,
                "min_c": 0.125, "avg_c": 21.5, "max_c": 41.25,
                "sample_count": 60, "disconnect_count": 0,
                "worst_alarm": "NORMAL", "data_hmac": null
            }
        }"#;
        let env: ExportEnvelope<Probe1mReplayData> = serde_json::from_str(json).unwrap();
        assert_eq!(env.message_id, "m-replay-1");
        assert_eq!(env.stream_version, 1);
        assert_eq!(env.data.request_id.as_deref(), Some("r1"));
        assert_eq!(env.data.probe.min_c, Some(0.125));
        assert_eq!(env.data.probe.avg_c, Some(21.5));
        assert_eq!(env.data.probe.max_c, Some(41.25));

        let out = serde_json::to_string(&env).unwrap();
        assert!(
            !out.contains("$serde_json::private::Number"),
            "arbitrary_precision leaked its private number marker: {out}"
        );
        let reparsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed["data"]["avg_c"].as_f64(), Some(21.5));
        assert_eq!(reparsed["data"]["request_id"], "r1");
    }
}
