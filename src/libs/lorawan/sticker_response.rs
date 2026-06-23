//! In-app decoding of STICKER fPort-85 `Response` messages (command/response
//! protocol, #34). A `Response` carries the original command `seq` plus one of
//! Ack / Info (#65) / ConfigDump (#70) / HistoryFrame (#39) / Error / W1Scan.
//!
//! Like fPort 2/3, the wire payload is prefixed with the 1-byte
//! `APP_PROTO_VERSION`; the caller strips it before calling `decode_response`.

use prost::Message;

use super::sticker_proto::{response, Response};

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedResponse {
    /// Echoed command sequence number (correlates the reply to its request).
    pub seq: u32,
    pub kind: ResponseKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseKind {
    Ack,
    Info {
        fw_version: String,
        build_type: &'static str,
        serial_number: u32,
        uptime_s: u32,
        unix_time: u32,
        debug: bool,
        claim_token: Option<String>,
    },
    ConfigDump {
        page_index: u32,
        page_count: u32,
    },
    HistoryFrame {
        frame_index: u32,
        frame_count: u32,
        t0_unix: u32,
        present: u32,
        interval_s: u32,
        samples_len: usize,
    },
    Error {
        code: &'static str,
        fault_field: u32,
        detail: String,
    },
    W1Scan {
        roms: Vec<String>,
    },
    /// Response with no body set (forward-compat / unknown variant).
    Empty,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn build_type_name(v: i32) -> &'static str {
    match v {
        0 => "main",
        1 => "dev",
        2 => "custom",
        _ => "unknown",
    }
}

fn error_code_name(v: i32) -> &'static str {
    match v {
        0 => "unknown",
        1 => "bad_request",
        2 => "out_of_range",
        3 => "not_ready",
        4 => "history_unavailable",
        5 => "unsupported_field",
        6 => "persist_failed",
        _ => "unknown",
    }
}

/// Decode an fPort-85 `Response` (proto-version byte already stripped).
pub fn decode_response(bytes: &[u8]) -> Result<DecodedResponse, String> {
    let r = Response::decode(bytes).map_err(|e| format!("Response protobuf decode failed: {e}"))?;
    let kind = match r.body {
        Some(response::Body::Ack(_)) => ResponseKind::Ack,
        Some(response::Body::Info(i)) => ResponseKind::Info {
            fw_version: format!("{}.{}.{}", i.fw_major, i.fw_minor, i.fw_patch),
            build_type: build_type_name(i.build_type),
            serial_number: i.serial_number,
            uptime_s: i.uptime_s,
            unix_time: i.unix_time,
            debug: i.debug,
            claim_token: i.claim_token.as_deref().map(hex),
        },
        Some(response::Body::ConfigDump(c)) => ResponseKind::ConfigDump {
            page_index: c.page_index,
            page_count: c.page_count,
        },
        Some(response::Body::HistoryFrame(h)) => ResponseKind::HistoryFrame {
            frame_index: h.frame_index,
            frame_count: h.frame_count,
            t0_unix: h.t0_unix,
            present: h.present,
            interval_s: h.interval_s,
            samples_len: h.samples.len(),
        },
        Some(response::Body::Error(e)) => ResponseKind::Error {
            code: error_code_name(e.code),
            fault_field: e.fault_field,
            detail: e.detail,
        },
        Some(response::Body::W1Scan(w)) => ResponseKind::W1Scan {
            roms: w.rom.iter().map(|r| hex(r)).collect(),
        },
        None => ResponseKind::Empty,
    };
    Ok(DecodedResponse { seq: r.seq, kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::sticker_proto::{response, Response};

    #[test]
    fn info_round_trip() {
        let resp = Response {
            seq: 7,
            body: Some(response::Body::Info(response::Info {
                fw_major: 1,
                fw_minor: 4,
                fw_patch: 0,
                build_type: 2,
                serial_number: 12345,
                uptime_s: 100,
                unix_time: 1_780_000_000,
                debug: true,
                claim_token: Some(vec![0xaa; 16]),
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 7);
        match d.kind {
            ResponseKind::Info { fw_version, build_type, serial_number, debug, claim_token, .. } => {
                assert_eq!(fw_version, "1.4.0");
                assert_eq!(build_type, "custom");
                assert_eq!(serial_number, 12345);
                assert!(debug);
                assert_eq!(claim_token.as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn error_round_trip() {
        let resp = Response {
            seq: 3,
            body: Some(response::Body::Error(response::Error {
                code: 2,
                fault_field: 4,
                detail: "out of range".into(),
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 3);
        assert_eq!(
            d.kind,
            ResponseKind::Error { code: "out_of_range", fault_field: 4, detail: "out of range".into() }
        );
    }

    #[test]
    fn real_e2e_fport85_info_decodes() {
        // GOLDEN VECTOR: live fPort-85 `Response{Info}` captured from a STICKER
        // (get_info reply), with the 0x01 proto-version byte stripped. Confirms
        // the in-app Response decode against real firmware output.
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let raw = B64
            .decode("AQgDGikIARAEIAIoooaAhwgwigQ467vo0QZAAUoQFYpqXVtUxRGOYqj0rw3o0g==")
            .unwrap();
        let bytes = &raw[1..]; // strip APP_PROTO_VERSION
        let d = decode_response(bytes).unwrap();
        assert_eq!(d.seq, 3);
        match d.kind {
            ResponseKind::Info { fw_version, build_type, serial_number, debug, claim_token, .. } => {
                assert_eq!(fw_version, "1.4.0");
                assert_eq!(build_type, "custom");
                assert_eq!(serial_number, 2162164514);
                assert!(debug);
                assert_eq!(claim_token.as_deref(), Some("158a6a5d5b54c5118e62a8f4af0de8d2"));
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }
}
