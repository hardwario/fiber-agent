//! The 18-byte on-device alarm rule (`alarms.alarm_0` .. `alarm_15`), sticker-firmware v2.0.0
//! (#430) — `node-firmware-v2.0.0.md` §2.
//!
//! ```text
//! [0]      flags        bit0 present, bit1 enabled
//! [1]      slot         0 = motherboard, 1..4 = s1..s4
//! [2]      channel      type-local channel number
//! [3]      sensor_type  type id the rule was written for (1 for slot 0)
//! [4]      from_state   STATE only
//! [5]      to_state     STATE only
//! [6..9]   lo           f32 LE
//! [10..13] hi           f32 LE
//! [14..17] dwell        f32 LE, seconds
//! ```
//!
//! Kind and scale are not stored: both come from the channel in the registry
//! ([`super::sensor_types`]). This is the one codec of the layout on the Hub: the radio's
//! `set-config` check, the legacy stack and the Portal's threshold push all use it.

use super::sensor_types::{self, Channel, Kind, MOTHERBOARD, W1_SLOTS};

/// Packed size of one rule.
pub const RULE_LEN: usize = 18;

const FLAG_PRESENT: u8 = 0x01;
const FLAG_ENABLED: u8 = 0x02;

/// `RULE_DWELL_MAX_S` of `app_alarm_rules.c`.
pub const DWELL_MAX_S: f32 = 3600.0;

/// One occupied rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub enabled: bool,
    pub slot: u8,
    pub channel: u8,
    pub sensor_type: u8,
    pub from_state: u8,
    pub to_state: u8,
    pub lo: f32,
    pub hi: f32,
    pub dwell: f32,
}

impl Rule {
    /// The packed bytes, flags `present` (and `enabled` when set).
    pub fn pack(&self) -> [u8; RULE_LEN] {
        let mut out = [0u8; RULE_LEN];
        out[0] = FLAG_PRESENT | if self.enabled { FLAG_ENABLED } else { 0 };
        out[1] = self.slot;
        out[2] = self.channel;
        out[3] = self.sensor_type;
        out[4] = self.from_state;
        out[5] = self.to_state;
        out[6..10].copy_from_slice(&self.lo.to_le_bytes());
        out[10..14].copy_from_slice(&self.hi.to_le_bytes());
        out[14..18].copy_from_slice(&self.dwell.to_le_bytes());
        out
    }

    /// The rule in `bytes`: `Ok(None)` for an empty slot (zero-length, or `present` clear),
    /// `Err` for a length other than 0 or [`RULE_LEN`].
    pub fn unpack(bytes: &[u8]) -> Result<Option<Rule>, String> {
        if bytes.is_empty() {
            return Ok(None);
        }
        if bytes.len() != RULE_LEN {
            return Err(format!(
                "an alarm rule is {RULE_LEN} bytes, got {}",
                bytes.len()
            ));
        }
        if bytes[0] & FLAG_PRESENT == 0 {
            return Ok(None);
        }
        let f = |i: usize| f32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        Ok(Some(Rule {
            enabled: bytes[0] & FLAG_ENABLED != 0,
            slot: bytes[1],
            channel: bytes[2],
            sensor_type: bytes[3],
            from_state: bytes[4],
            to_state: bytes[5],
            lo: f(6),
            hi: f(10),
            dwell: f(14),
        }))
    }

    /// The channel the rule targets, when the target is one a rule may have
    /// (`app_alarm_rule_valid`): slot 0..4, the motherboard type on slot 0 and a 1-Wire type
    /// on slots 1..4, a known channel that is not `kind: none`, retired or watchdog-only.
    pub fn target(&self) -> Result<&'static Channel, String> {
        if self.slot > W1_SLOTS {
            return Err(format!("slot {} is not 0..{W1_SLOTS}", self.slot));
        }
        let t = sensor_types::sensor_type(self.sensor_type)
            .ok_or_else(|| format!("sensor type {} is unknown", self.sensor_type))?;
        if (self.slot == 0) != (self.sensor_type == MOTHERBOARD) || (self.slot != 0 && !t.is_w1()) {
            return Err(format!(
                "slot {} cannot hold sensor type {} ({})",
                sensor_types::slot_label(u32::from(self.slot)),
                t.id,
                t.name
            ));
        }
        let c = t
            .channel(self.channel)
            .ok_or_else(|| format!("{} has no channel {}", t.name, self.channel))?;
        if !c.rule_target() {
            return Err(format!("{}/{} cannot carry an alarm rule", t.name, c.name));
        }
        Ok(c)
    }

    /// Every check the firmware applies before it stores a rule (`rule_valid`), so a rule the
    /// Node would sanitise away is refused before it costs a downlink. The slot's current type
    /// (the "stale" check) is not here: see [`Rule::stale`].
    pub fn validate(&self) -> Result<&'static Channel, String> {
        let c = self.target()?;
        // NaN fails both comparisons, as in C.
        if !(self.dwell >= 0.0 && self.dwell <= DWELL_MAX_S) {
            return Err(format!("dwell {} is not 0..{DWELL_MAX_S} s", self.dwell));
        }
        if c.kind == Kind::Threshold
            && self.hi.partial_cmp(&self.lo) != Some(std::cmp::Ordering::Greater)
        {
            return Err(format!(
                "{}: hi ({}) must be above lo ({})",
                c.name, self.hi, self.lo
            ));
        }
        // v2.0.0 (#485 S7, `rule_state_levels_valid`): a STATE channel is digital, a level
        // outside 0/1 never matches. A momentary rule with from == to is valid again.
        if c.kind == Kind::State && (self.from_state > 1 || self.to_state > 1) {
            return Err(format!(
                "{}: state levels must be 0 or 1 (from {}, to {})",
                c.name, self.from_state, self.to_state
            ));
        }
        // `rule_rate_limit_valid`: a RATE limit is a uint32 counter delta; NaN fails both.
        if c.kind == Kind::Rate && !(self.hi >= 0.0 && self.hi <= u32::MAX as f32) {
            return Err(format!(
                "{}: a rate limit must be 0..{} (hi {})",
                c.name,
                u32::MAX,
                self.hi
            ));
        }
        Ok(c)
    }

    /// Whether the Node keeps this rule inert: it was written for a type other than the
    /// slot's configured `sensorN_type` (`rule_stale_in`). `slot_type` is that type, 0 when
    /// unset.
    pub fn stale(&self, slot_type: u8) -> bool {
        let current = if self.slot == 0 {
            MOTHERBOARD
        } else {
            slot_type
        };
        self.sensor_type != current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// The HIL rule of #119: enabled, s1 machine-probe temperature, lo 10, hi 30, dwell 0.
    #[test]
    fn the_hil_vector_round_trips() {
        let bytes = unhex("030100030000000020410000f04100000000");
        let r = Rule::unpack(&bytes).expect("18 B").expect("present");
        assert_eq!(
            r,
            Rule {
                enabled: true,
                slot: 1,
                channel: 0,
                sensor_type: 3,
                from_state: 0,
                to_state: 0,
                lo: 10.0,
                hi: 30.0,
                dwell: 0.0,
            }
        );
        assert_eq!(hex(&r.pack()), "030100030000000020410000f04100000000");
        assert_eq!(r.validate().expect("valid").name, "temperature");
        assert!(!r.stale(3));
        assert!(r.stale(2), "written for machine-probe, slot now dallas");
        assert!(r.stale(0));
    }

    #[test]
    fn empty_slots_and_wrong_lengths() {
        assert_eq!(Rule::unpack(&[]), Ok(None));
        assert_eq!(Rule::unpack(&[0u8; RULE_LEN]), Ok(None));
        assert!(Rule::unpack(&[0u8; 17]).is_err(), "the v1.4 layout");
    }

    fn rule(slot: u8, sensor_type: u8, channel: u8) -> Rule {
        Rule {
            enabled: true,
            slot,
            channel,
            sensor_type,
            from_state: 0,
            to_state: 0,
            lo: 0.0,
            hi: 1.0,
            dwell: 0.0,
        }
    }

    #[test]
    fn targets_follow_app_alarm_rule_valid() {
        assert!(rule(0, 1, 0).validate().is_ok());
        assert!(rule(2, 2, 0).validate().is_ok(), "dallas temperature");
        assert!(
            rule(0, 2, 0).validate().is_err(),
            "slot 0 is the motherboard"
        );
        assert!(
            rule(1, 1, 0).validate().is_err(),
            "1-Wire slots take 1-Wire types"
        );
        assert!(rule(5, 2, 0).validate().is_err());
        assert!(rule(1, 2, 1).validate().is_err(), "dallas has one channel");
        assert!(rule(1, 9, 0).validate().is_err(), "unknown type");
        assert!(
            rule(0, 1, 19).validate().is_err(),
            "orientation is kind none"
        );
        assert!(
            rule(0, 1, 20).validate().is_err(),
            "battery is watchdog-only"
        );
    }

    #[test]
    fn shape_checks_follow_rule_valid() {
        let mut r = rule(0, 1, 0);
        r.hi = 0.0;
        assert!(
            r.validate().is_err(),
            "an empty band latches forever (#203)"
        );
        let mut r = rule(0, 1, 0);
        r.dwell = 3601.0;
        assert!(r.validate().is_err());
        r.dwell = f32::NAN;
        assert!(r.validate().is_err());
        // pir-motion is a momentary state channel.
        let pir = sensor_types::sensor_type(1)
            .and_then(|t| t.channel_named("pir-motion"))
            .expect("pir");
        let mut r = rule(0, 1, pir.ch);
        assert!(
            r.validate().is_ok(),
            "v2.0.0: from == to on a momentary channel is valid"
        );
        r.to_state = 1;
        assert!(r.validate().is_ok());
        r.to_state = 2;
        assert!(r.validate().is_err(), "a state level is 0 or 1 (#485 S7)");
        // hall-left-count (ch 6) is a rate channel: hi is a uint32 delta.
        let mut r = rule(0, 1, 6);
        r.hi = -1.0;
        assert!(r.validate().is_err(), "a negative rate limit");
        r.hi = f32::NAN;
        assert!(r.validate().is_err());
        r.hi = 0.0;
        assert!(r.validate().is_ok(), "0 keeps the rule off");
    }
}
