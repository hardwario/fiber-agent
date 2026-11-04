//! The verb level — the signed claim the device checks against its own
//! table before any crypto runs (plan/portal/mqtt/design.md §4).
//!
//! A level is a property of the **verb**, not of its permission string:
//! `set_lorawan_sensor_config` alone spans `operate` (`sticker_clock_sync`),
//! `manage` (`set_sticker_config`), `admin` (`reset_export_cursor`) and
//! `destructive` (`sticker_device_reset`). That is why the portal's
//! certificate permission is not enough on its own, and why the level is
//! bound into the signature and checked here.
//!
//! This is a deliberate second copy of `fiber-protocol`'s `Level`, not a
//! shared import: `control-core` has no production dependency on the
//! portal's crate, so de-vendoring the contract stays the separate decision
//! monolith.md §11.2 records. The two are held together by
//! `vectors/catalogue.json`, which both sides are tested against
//! (design.md §10) — not by convention.

/// The five rungs, in ladder order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Returns state, changes nothing, redelivery is harmless.
    Read,
    /// Transient physical effect; nothing persisted.
    Operate,
    /// Persists configuration on the Hub or a node.
    Manage,
    /// Changes identity, trust, connectivity or export/audit state.
    Admin,
    /// Destroys data or availability.
    Destructive,
}

impl Level {
    /// The wire spelling — the exact string that rides inside the
    /// signature.
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Read => "read",
            Level::Operate => "operate",
            Level::Manage => "manage",
            Level::Admin => "admin",
            Level::Destructive => "destructive",
        }
    }

    /// Parse the wire spelling. Anything else is not a level and the
    /// request is `InvalidCommand` (design.md §3.2) — including a
    /// capitalised or otherwise creative spelling.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Level::Read,
            "operate" => Level::Operate,
            "manage" => Level::Manage,
            "admin" => Level::Admin,
            "destructive" => Level::Destructive,
            _ => return None,
        })
    }

    /// `read` and `operate` execute on the request itself: verified,
    /// executed, one audit row, no challenge (design.md §5). Everything
    /// above keeps challenge/confirm.
    pub const fn is_single_shot(self) -> bool {
        matches!(self, Level::Read | Level::Operate)
    }

    /// The `level:<name>` pseudo-permission the certificate must carry
    /// alongside the verb's own permission (design.md §4.3, §3.3 step 8).
    ///
    /// It is an ordinary entry in the certificate's `permissions: [str]`
    /// list with exact string match, so the certificate **schema** does not
    /// change — which is the whole point: a manager's certificate carries
    /// `set_lorawan_sensor_config`, and without this string a compromised
    /// portal holding that key could sign `sticker_device_reset`.
    pub fn permission(self) -> String {
        format!("level:{}", self.as_str())
    }

    /// All five, in ladder order.
    pub const ALL: [Level; 5] = [
        Level::Read,
        Level::Operate,
        Level::Manage,
        Level::Admin,
        Level::Destructive,
    ];
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which envelope a command arrived in.
///
/// v2 is the contract (design.md §3), and since **P4 it is the only one the
/// wire accepts**: `parse_envelope` answers `unsupported_envelope` to
/// anything else, and the transitional `accept_v1_envelope` flag that kept
/// the tree green between P1 and P4 is deleted. "No v1 fleet migration", so
/// no dual-accept survived the branch.
///
/// [`Envelope::V1`] therefore no longer names something the device will
/// serve. It stays for the two jobs it still does: it is what the
/// `dev-platform` build falls back to when it skips envelope parsing along
/// with every other check, and it is the "claims no level" case that
/// [`Envelope::level`] has to have an answer for. Nothing on the signed path
/// can construct it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Envelope {
    /// No `v`, no `level`: the seven-key request pre-image and the
    /// five-key confirm, with a missing `reason` canonicalized as `null`.
    V1,
    /// `v == 2` and a signed `level`: the nine-key request pre-image and
    /// the seven-key confirm, with `reason` a required string.
    V2(Level),
}

impl Envelope {
    /// The level this envelope claims, if any. `None` for v1, which claims
    /// nothing — so such a request is neither level-checked nor single-shot.
    /// Unreachable from the wire since P4; see the type's note.
    pub const fn level(self) -> Option<Level> {
        match self {
            Envelope::V1 => None,
            Envelope::V2(level) => Some(level),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_levels_are_a_ladder() {
        assert!(Level::Read < Level::Operate);
        assert!(Level::Operate < Level::Manage);
        assert!(Level::Manage < Level::Admin);
        assert!(Level::Admin < Level::Destructive);
    }

    #[test]
    fn the_wire_spelling_round_trips_and_is_lowercase_only() {
        for level in Level::ALL {
            assert_eq!(Level::parse(level.as_str()), Some(level));
        }
        assert_eq!(Level::parse("Destructive"), None);
        assert_eq!(Level::parse("DESTRUCTIVE"), None);
        assert_eq!(Level::parse(""), None);
        assert_eq!(Level::parse("root"), None);
    }

    #[test]
    fn single_shot_is_exactly_read_and_operate() {
        assert!(Level::Read.is_single_shot());
        assert!(Level::Operate.is_single_shot());
        assert!(!Level::Manage.is_single_shot());
        assert!(!Level::Admin.is_single_shot());
        assert!(!Level::Destructive.is_single_shot());
    }

    #[test]
    fn the_level_pseudo_permissions_are_prefixed_strings() {
        assert_eq!(Level::Read.permission(), "level:read");
        assert_eq!(Level::Destructive.permission(), "level:destructive");
    }

    #[test]
    fn a_v1_envelope_claims_no_level() {
        assert_eq!(Envelope::V1.level(), None);
        assert_eq!(Envelope::V2(Level::Manage).level(), Some(Level::Manage));
    }
}
