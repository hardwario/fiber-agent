//! Advertisement decode — `plan/control/ble/plan.md` §7.
//!
//! A **registry**, not a match arm: variant proliferation is the normal case for this hardware
//! (`beacon-btsmp1.md` §4 — the recorder SKU already sits beside the standard beacon, with an
//! intrinsically-safe variant beside them), so a two-variant world is not hardcoded anywhere.
//!
//! The unknown-version rule (§7 rule 2, decided in the direction **opposite** to `control/radio`
//! finding 1.5): **the remainder is discarded, not decoded.** Unlike a Node fPort payload — a
//! fixed-layout frame with a reference decoder — a BTSMP1 frame's field offsets depend on the flags
//! byte, so a version bump plausibly changes the layout. Decoding the remainder would publish
//! plausible garbage. The frame is counted (`status.central.unparseable_frames`) and dropped whole.

pub mod btsmp1;
pub mod registry;

/// Teltonika's Bluetooth SIG company identifier — the manufacturer-data key.
pub const TELTONIKA_COMPANY_ID: u16 = 0x089A;

/// One decoded advertisement.
///
/// Every value field is `Option` and every flag defaults to `false`, because **absent must never
/// surface as zero**: a beacon with no humidity sensor rendering as `0 %` on the portal card is
/// exactly the failure UI-10 exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reading {
    pub protocol_version: u8,
    pub flags: u8,
    pub temperature_c: Option<f64>,
    pub humidity_pct: Option<u8>,
    pub magnet_present: bool,
    pub magnet_detected: bool,
    pub moving: Option<bool>,
    pub movement_count: Option<u16>,
    pub pitch_deg: Option<i8>,
    pub roll_deg: Option<i16>,
    pub low_battery: bool,
    pub battery_mv: Option<u16>,
}

impl Reading {
    /// An empty decode of `version`/`flags`, before any value field is filled in.
    pub const fn empty(protocol_version: u8, flags: u8) -> Reading {
        Reading {
            protocol_version,
            flags,
            temperature_c: None,
            humidity_pct: None,
            magnet_present: false,
            magnet_detected: false,
            moving: None,
            movement_count: None,
            pitch_deg: None,
            roll_deg: None,
            low_battery: false,
            battery_mv: None,
        }
    }
}

/// The decodable field names, for tests and for the payload projection. `protocol_version` and
/// `flags` are frame metadata, not readings, so they are deliberately not here.
pub const DECODE_FIELDS: [&str; 10] = [
    "temperature_c",
    "humidity_pct",
    "magnet_present",
    "magnet_detected",
    "moving",
    "movement_count",
    "pitch_deg",
    "roll_deg",
    "low_battery",
    "battery_mv",
];

/// Why a frame was dropped. Never an error to the scan loop — each of these is counted and the
/// frame is discarded, because a beacon in range emitting frames we cannot read is a fact about the
/// estate, not a fault in this Hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Shorter than the mandatory version + flags pair.
    TooShort,
    /// A protocol version this registry has no parser for.
    UnknownVersion(u8),
    /// The flags claim a field the frame does not carry.
    Truncated,
}

impl ParseError {
    /// The stable wire spelling, so a counter or a log line can name the reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            ParseError::TooShort => "too_short",
            ParseError::UnknownVersion(_) => "unknown_version",
            ParseError::Truncated => "truncated",
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::TooShort => f.write_str("frame shorter than version + flags"),
            ParseError::UnknownVersion(v) => {
                write!(f, "unknown protocol version 0x{v:02X}: frame dropped whole")
            }
            ParseError::Truncated => {
                f.write_str("flags claim a field the frame does not carry: frame dropped whole")
            }
        }
    }
}

/// A parser takes the manufacturer-data **value** — the bytes *after* the company id.
pub type ParseFn = fn(&[u8]) -> std::result::Result<Reading, ParseError>;

/// A decoded frame together with the parser that produced it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decoded {
    pub parser: &'static str,
    pub reading: Reading,
}

/// Decode with a named parser from the registry. `None` means no such parser — deliberately
/// distinct from `Some(Err(..))`, which means the parser ran and rejected the frame.
pub fn decode_with(parser: &str, value: &[u8]) -> Option<std::result::Result<Decoded, ParseError>> {
    let (name, f) = registry::entry(parser)?;
    Some(f(value).map(|reading| Decoded {
        parser: name,
        reading,
    }))
}
