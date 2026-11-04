//! fPort-85 `HistoryFrame` — one frame of the record stream a `ReqHistory` replay answers with
//! (`p2p_history_replay.md` §5.2).
//!
//! Written from the firmware's `app_config.proto` `Response.HistoryFrame` and the v1 module as
//! provenance (`control-core` `node_response::expand_history_frame`), not shared with it: the
//! two engines stay separate (p2p.md §6.2), and the golden vectors below are what keeps them in
//! agreement.
//!
//! A frame is a header plus `samples`, a run of fixed-size records that carry values only;
//! record `j` was taken at `t0_unix + j * interval_s`.
//!
//! *Amended 2026-10-09 (v2.0.0, #430, `node-firmware-v2.0.0.md` §6):* a frame that carries
//! `channels` (field 10, one `slot << 5 | ch` byte per column) and `w1_types` (11) lays its
//! records out by the sensor type registry instead: each column's width, scale and sentinel
//! come from the channel's `history` encoding, and its name from `sensor_types::hub_field`.
//!
//! *Amended 2026-10-14 (v2.0.0 minimum):* the pre-#430 layout (the fixed `present` bit order)
//! and the body's own frame numbering (fields 1/2, before #425) are no longer read. A frame
//! without `channels` yields no records.

use std::collections::BTreeMap;

use serde::Serialize;

use super::pb::{read_tag, read_varint, skip};
use super::sensor_types;

/// A decoded `HistoryFrame` header and its record bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryFrame {
    pub t0_unix: u32,
    pub samples: Vec<u8>,
    pub interval_s: u32,
    /// Field 7. `Some(false)` means the RTC had not synced when the records were taken, so
    /// `t0_unix` is uptime-relative. Absent reads as synced, as the proto says.
    pub time_synced: Option<bool>,
    /// Field 10 (v2.0.0): the record's columns, `slot << 5 | ch` each.
    pub channels: Vec<u8>,
    /// Field 11 (v2.0.0): the sensor type of s1..s4 the layout was built with; absent when no
    /// column is a 1-Wire one.
    pub w1_types: Option<Vec<u8>>,
}

/// One column of a v2.0.0 record.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// The Hub field (`temperature`, `w1_2_temperature_aux`).
    pub field: String,
    pub enc: sensor_types::Hist,
    pub scale: f64,
    /// A `kind: rate` channel: a whole-number counter.
    pub counter: bool,
}

impl HistoryFrame {
    /// The v2.0.0 column layout: `None` for a frame without `channels`, and
    /// `Some(None)` when a column names a channel the registry does not know, which makes the
    /// record size, and so every record, unknowable (`ttn.js` gives up the same way).
    pub fn columns(&self) -> Option<Option<Vec<Column>>> {
        if self.channels.is_empty() {
            return None;
        }
        Some(
            self.channels
                .iter()
                .map(|&b| {
                    let (slot, ch) = (b >> 5, b & 0x1f);
                    let w1_type = if slot == 0 {
                        None
                    } else {
                        Some(
                            self.w1_types
                                .as_ref()
                                .and_then(|t| t.get(usize::from(slot) - 1).copied())
                                .unwrap_or(0),
                        )
                    };
                    let c = sensor_types::channel(slot, w1_type, ch)?;
                    Some(Column {
                        field: sensor_types::hub_field_of(slot, c),
                        enc: c.hist,
                        scale: c.hist_scale,
                        counter: c.kind == sensor_types::Kind::Rate,
                    })
                })
                .collect(),
        )
    }
}

/// One record of a frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryRecord {
    /// Unix seconds, or `None` for a frame taken before the RTC synced: an uptime-relative
    /// `t0_unix` read as absolute would file the record under 1970.
    pub time: Option<u32>,
    /// Measurements. A channel the firmware wrote its sentinel for is absent.
    pub fields: BTreeMap<String, f64>,
    /// Pulse counters.
    pub counters: BTreeMap<String, u64>,
}

/// The `history_channels` column (`slot << 5 | ch`, v2.0.0) a Hub field is recorded under;
/// `w1_type(slot)` is the type of a 1-Wire slot. `None` for a field no channel carries.
pub fn history_column(field: &str, w1_type: impl Fn(u8) -> Option<u8>) -> Option<u8> {
    sensor_types::field_channel(field, w1_type).map(|(slot, _, c)| slot << 5 | c.ch)
}

/// Read a `HistoryFrame` submessage. Never fails: a truncated or corrupt frame yields what
/// could be read, like every other decoder in this crate.
pub fn decode_frame(bytes: &[u8]) -> HistoryFrame {
    let mut frame = HistoryFrame::default();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        match (tag.field, tag.wire) {
            (4 | 10 | 11, 2) => {
                let len = read_varint(bytes, pos);
                let start = len.next.min(bytes.len());
                let end = (start + len.value as usize).min(bytes.len());
                let body = bytes[start..end].to_vec();
                match tag.field {
                    4 => frame.samples = body,
                    10 => frame.channels = body,
                    _ => frame.w1_types = Some(body),
                }
                pos = end;
            }
            (field, 0) => {
                let v = read_varint(bytes, pos);
                pos = v.next;
                match field {
                    3 => frame.t0_unix = v.value,
                    6 => frame.interval_s = v.value,
                    7 => frame.time_synced = Some(v.value != 0),
                    _ => {}
                }
            }
            (_, wire) => match skip(bytes, wire, pos) {
                Some(next) => pos = next,
                None => break,
            },
        }
    }
    frame
}

/// Expand a frame's `samples` into timestamped records. A trailing partial record is dropped,
/// and a frame without `channels`, or whose layout the registry cannot resolve, yields none.
pub fn expand(frame: &HistoryFrame) -> Vec<HistoryRecord> {
    match frame.columns() {
        Some(Some(cols)) => expand_columns(frame, &cols),
        _ => Vec::new(),
    }
}

fn record_time(frame: &HistoryFrame, j: usize) -> Option<u32> {
    frame.time_synced.unwrap_or(true).then(|| {
        frame
            .t0_unix
            .wrapping_add((j as u32).wrapping_mul(frame.interval_s))
    })
}

/// The v2.0.0 path: columns by the registry (`ttn.js` `_decodeHistoryColumns`).
fn expand_columns(frame: &HistoryFrame, cols: &[Column]) -> Vec<HistoryRecord> {
    use sensor_types::Hist;
    let size: usize = cols.iter().map(|c| c.enc.size()).sum();
    if size == 0 {
        return Vec::new();
    }
    frame
        .samples
        .chunks_exact(size)
        .enumerate()
        .map(|(j, rec)| {
            let mut out = HistoryRecord {
                time: record_time(frame, j),
                fields: BTreeMap::new(),
                counters: BTreeMap::new(),
            };
            let mut p = 0usize;
            for c in cols {
                let n = c.enc.size();
                let mut le = [0u8; 4];
                le[..n].copy_from_slice(&rec[p..p + n]);
                p += n;
                let raw = u32::from_le_bytes(le);
                if raw == c.enc.sentinel() {
                    continue;
                }
                let value: i64 = match c.enc {
                    Hist::I16 => i64::from(raw as u16 as i16),
                    Hist::I32 => i64::from(raw as i32),
                    _ => i64::from(raw),
                };
                if c.counter {
                    if let Ok(v) = u64::try_from(value) {
                        out.counters.insert(c.field.clone(), v);
                        continue;
                    }
                }
                out.fields.insert(c.field.clone(), value as f64 / c.scale);
            }
            out
        })
        .collect()
}

/// Whether a queued downlink body is a `ReqHistory` command.
///
/// The queue head's *answer* usually identifies it — nothing but a `req_history` is replied to
/// with a `HistoryFrame`. But an empty window is answered with a typed `history_unavailable`
/// Error and no frames at all, and that case still has to publish the "done, no data" sentinel.
/// So the head is identified from the bytes we sent: `APP_PROTO_VERSION` then a `Command` whose
/// body oneof is field 11.
///
/// A raw `--hex` payload the operator wrote by hand is read the same way — if it really is a
/// ReqHistory it should behave like one.
///
/// Note the asymmetry: **uplinks carry `APP_PROTO_VERSION`, downlinks do not**
/// (`command_encode::Downlink::encode`). So this parses from offset 0 and must *not* call
/// `strip_version_prefix`, which drops the first byte unconditionally and would eat the `seq`
/// tag. The mirror-image mistake on the uplink side was the 2026-09-08 bench failure.
pub fn is_req_history(body: &[u8]) -> bool {
    let cmd = body;
    let mut pos = 0usize;
    while pos < cmd.len() {
        let tag = read_tag(cmd, pos);
        pos = tag.next;
        if tag.wire == 0 {
            // `skip` does not handle varints — `Command.seq` is one, and it comes first.
            pos = read_varint(cmd, pos).next;
            continue;
        }
        if tag.wire == 2 {
            let len = read_varint(cmd, pos);
            pos = (len.next + len.value as usize).min(cmd.len());
            if tag.field == crate::decode::command::CommandTag::ReqHistory as u32 {
                return true;
            }
            continue;
        }
        match skip(cmd, tag.wire, pos) {
            Some(next) => pos = next,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// A pre-#430 frame from STICKER 0413 (2026-09-25, `node-send cmd history-replay`): 11
    /// records in the fixed `present` order and no `channels`. v2.0.0 is the minimum firmware,
    /// so its records are not decoded.
    #[test]
    fn a_frame_without_channels_yields_no_records() {
        let body = unhex(
            "18f7bad8d5062221930a6a900a6a8c0a6a8b0a6a840a69810a69810a697c0a68780a69790a6979\
             0a692803303c3801",
        );
        let frame = decode_frame(&body);
        assert_eq!(frame.interval_s, 60);
        assert_eq!(frame.columns(), None);
        assert!(expand(&frame).is_empty());
    }

    /// A v2.0.0 column frame: mb temperature, mb humidity, s1 machine-probe temperature-aux,
    /// mb hall-left-count, mb illuminance; w1_types [3, 0, 0, 0]. Two records, the second with
    /// every sentinel.
    #[test]
    fn a_column_frame_names_its_columns_by_the_registry() {
        let mut body = vec![0x18, 0x64, 0x30, 0x3c, 0x38, 0x01]; // t0 100, interval 60, synced
        body.extend([0x52, 0x05, 0x00, 0x01, 0x22, 0x06, 0x03]); // channels
        body.extend([0x5a, 0x04, 0x03, 0x00, 0x00, 0x00]); // w1_types
        let rec1 = [
            0x93, 0x0a, // 27.07
            0x6a, // 53.0
            0x0c, 0xfe, // -5.00
            0x07, 0x00, 0x00, 0x00, // 7 pulses
            0x64, 0x00, // 100 * 0.5 lx
        ];
        let rec2 = [
            0xff, 0x7f, 0xff, 0xff, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        body.extend([0x22, 22]);
        body.extend(rec1);
        body.extend(rec2);
        let frame = decode_frame(&body);
        assert_eq!(frame.channels, [0x00, 0x01, 0x22, 0x06, 0x03]);
        assert_eq!(frame.w1_types.as_deref(), Some(&[3u8, 0, 0, 0][..]));
        let cols = frame.columns().expect("v2.0.0").expect("known");
        let names: Vec<_> = cols.iter().map(|c| c.field.as_str()).collect();
        assert_eq!(
            names,
            [
                "temperature",
                "humidity",
                "w1_1_temperature_aux",
                "hall_left_count",
                "illuminance"
            ]
        );

        let records = expand(&frame);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].time, Some(100));
        assert_eq!(records[0].fields["temperature"], 27.07);
        assert_eq!(records[0].fields["humidity"], 53.0);
        assert_eq!(records[0].fields["w1_1_temperature_aux"], -5.0);
        assert_eq!(records[0].fields["illuminance"], 200.0);
        assert_eq!(records[0].counters["hall_left_count"], 7);
        assert_eq!(records[1].time, Some(160));
        assert!(records[1].fields.is_empty() && records[1].counters.is_empty());
    }

    /// A column the registry does not know makes the record size unknowable.
    #[test]
    fn an_unknown_column_leaves_the_frame_undecoded() {
        // s2 with w1_types saying dallas, channel 3: dallas has one channel.
        let body = [
            0x52, 0x01, 0x43, 0x5a, 0x04, 0x00, 0x02, 0x00, 0x00, 0x22, 0x02, 0x01, 0x02,
        ];
        let frame = decode_frame(&body);
        assert_eq!(frame.columns(), Some(None));
        assert!(expand(&frame).is_empty());
    }

    #[test]
    fn history_columns_follow_the_hub_field_names() {
        assert_eq!(history_column("temperature", |_| None), Some(0x00));
        assert_eq!(history_column("humidity", |_| None), Some(0x01));
        assert_eq!(history_column("w1_1_temperature", |_| Some(2)), Some(0x20));
        assert_eq!(history_column("w1_3_tilt", |_| Some(3)), Some(0x65));
        assert_eq!(history_column("w1_3_tilt", |_| Some(2)), None);
        assert_eq!(history_column("motion_count", |_| None), Some(16));
    }

    /// mb temperature + humidity columns.
    fn th_frame(samples: Vec<u8>, time_synced: Option<bool>) -> HistoryFrame {
        HistoryFrame {
            t0_unix: 1_780_000_000,
            interval_s: 900,
            samples,
            time_synced,
            channels: vec![0x00, 0x01],
            ..HistoryFrame::default()
        }
    }

    #[test]
    fn a_sentinel_leaves_its_channel_out() {
        // 23.50 °C / 50 %, then -5.50 °C / no humidity.
        let records = expand(&th_frame(vec![0x2e, 0x09, 0x64, 0xda, 0xfd, 0xff], None));
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].time, Some(1_780_000_900));
        assert_eq!(records[1].fields["temperature"], -5.5);
        assert!(!records[1].fields.contains_key("humidity"));
    }

    #[test]
    fn an_unsynced_frame_has_no_times_but_keeps_its_values() {
        let records = expand(&th_frame(vec![0x2e, 0x09, 0x64], Some(false)));
        assert_eq!(records[0].time, None);
        assert_eq!(records[0].fields["temperature"], 23.5);
    }

    #[test]
    fn a_counter_channel_and_a_torn_record() {
        // hall_left_count (mb ch 6): 123, then two stray bytes of a record that never finished.
        let frame = HistoryFrame {
            t0_unix: 1000,
            interval_s: 60,
            samples: vec![0x7b, 0x00, 0x00, 0x00, 0x01, 0x02],
            channels: vec![0x06],
            ..HistoryFrame::default()
        };
        let records = expand(&frame);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].counters["hall_left_count"], 123);
    }

    #[test]
    fn a_truncated_frame_does_not_panic() {
        let body = unhex("18f7bad8d5062221930a6a");
        let frame = decode_frame(&body);
        assert_eq!(frame.samples.len(), 3);
        assert_eq!(expand(&frame).len(), 0, "no channels, no records");
    }

    #[test]
    fn a_queued_req_history_is_recognised_from_its_own_bytes() {
        use crate::decode::command_encode::Downlink;
        use crate::Transport;

        for (from, to) in [
            (None, None),
            (Some(1_600_000_000i64), Some(1_700_000_000i64)),
        ] {
            let body = Downlink::ReqHistory {
                from_unix: from,
                to_unix: to,
            }
            .encode(9, Transport::P2p)
            .expect("encodes");
            assert!(is_req_history(&body), "from={from:?} to={to:?}");
        }

        // Every other command must not be mistaken for one.
        let info = Downlink::GetInfo
            .encode(9, Transport::P2p)
            .expect("encodes");
        assert!(!is_req_history(&info), "GetInfo is not a history request");
        assert!(
            !is_req_history(&[]),
            "an empty body is not a history request"
        );
    }
}
