//! Key placement into ChirpStack's fields — `plan/control/radio/plan.md` §9.2.
//!
//! > **Key placement must be preserved byte-for-byte** — these are LoRaWAN 1.0.x conventions
//! > the tonic rewrite can silently get wrong, because the field names do not match the
//! > values:
//! >
//! > - OTAA `CreateKeys` writes `app_key` into **both** `nwk_key` and `app_key`.
//! > - ABP `Activate` writes `nwkskey` into **all three** of `nwk_s_enc_key`,
//! >   `s_nwk_s_int_key` and `f_nwk_s_int_key`, and `appskey` into `app_s_key`.
//!
//! "Silently" is the operative word: place the OTAA key in only `app_key` and every RPC
//! succeeds, the device is created, and it simply never joins. So placement is a pure
//! function here with its own tests, provable without a server, rather than a detail buried
//! in the tonic call site.

use crate::lorawan::model::{AbpKeys, OtaaKeys};

/// OTAA keys as ChirpStack stores them.
#[derive(Clone, PartialEq, Eq)]
pub struct PlacedOtaaKeys {
    /// LoRaWAN 1.0.x has one root key; ChirpStack models 1.1's two, so the same key goes in
    /// both. A device with only `app_key` set will not join.
    pub nwk_key: [u8; 16],
    pub app_key: [u8; 16],
}

/// ABP session keys as ChirpStack stores them.
#[derive(Clone, PartialEq, Eq)]
pub struct PlacedAbpSession {
    pub dev_addr: [u8; 4],
    /// All three network-session fields carry the one 1.0.x `nwkskey`.
    pub nwk_s_enc_key: [u8; 16],
    pub s_nwk_s_int_key: [u8; 16],
    pub f_nwk_s_int_key: [u8; 16],
    pub app_s_key: [u8; 16],
}

impl std::fmt::Debug for PlacedOtaaKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlacedOtaaKeys { nwk_key: <redacted>, app_key: <redacted> }")
    }
}

impl std::fmt::Debug for PlacedAbpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlacedAbpSession { <redacted> }")
    }
}

/// One OTAA root key in, two identical fields out.
pub fn place_otaa(keys: &OtaaKeys) -> PlacedOtaaKeys {
    PlacedOtaaKeys {
        nwk_key: keys.app_key,
        app_key: keys.app_key,
    }
}

/// One ABP `nwkskey` into three fields, `appskey` into one.
pub fn place_abp(keys: &AbpKeys) -> PlacedAbpSession {
    PlacedAbpSession {
        dev_addr: keys.dev_addr,
        nwk_s_enc_key: keys.nwk_s_key,
        s_nwk_s_int_key: keys.nwk_s_key,
        f_nwk_s_int_key: keys.nwk_s_key,
        app_s_key: keys.app_s_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    const NWKS: [u8; 16] = [0xa0; 16];
    const APPS: [u8; 16] = [0xb0; 16];

    /// §9.2, field by field, "since the field names do not match the values they carry"
    /// (lorawan.md §10's own wording for this test).
    #[test]
    fn otaa_app_key_lands_in_both_fields() {
        let placed = place_otaa(&OtaaKeys { app_key: APP });
        assert_eq!(placed.app_key, APP, "app_key field");
        assert_eq!(placed.nwk_key, APP, "nwk_key field must carry the SAME key");
    }

    #[test]
    fn abp_nwkskey_lands_in_all_three_network_session_fields() {
        let placed = place_abp(&AbpKeys {
            dev_addr: [1, 2, 3, 4],
            nwk_s_key: NWKS,
            app_s_key: APPS,
        });
        assert_eq!(placed.dev_addr, [1, 2, 3, 4]);
        assert_eq!(placed.nwk_s_enc_key, NWKS, "nwk_s_enc_key");
        assert_eq!(placed.s_nwk_s_int_key, NWKS, "s_nwk_s_int_key");
        assert_eq!(placed.f_nwk_s_int_key, NWKS, "f_nwk_s_int_key");
        assert_eq!(
            placed.app_s_key, APPS,
            "app_s_key carries appskey, not nwkskey"
        );
    }

    /// The mistake this whole module exists to prevent: the app-session key must never be
    /// confused with the network-session key, and vice versa.
    #[test]
    fn the_two_abp_keys_are_not_swapped() {
        let placed = place_abp(&AbpKeys {
            dev_addr: [0; 4],
            nwk_s_key: NWKS,
            app_s_key: APPS,
        });
        assert_ne!(placed.app_s_key, placed.nwk_s_enc_key);
        assert_ne!(placed.app_s_key, NWKS);
    }

    /// Key material must not be reachable through Debug — it ends up in panic messages,
    /// tracing spans and journald otherwise.
    #[test]
    fn debug_output_carries_no_key_bytes() {
        let otaa = format!("{:?}", place_otaa(&OtaaKeys { app_key: APP }));
        let abp = format!(
            "{:?}",
            place_abp(&AbpKeys {
                dev_addr: [0; 4],
                nwk_s_key: NWKS,
                app_s_key: APPS
            })
        );
        for rendered in [&otaa, &abp] {
            assert!(rendered.contains("redacted"), "{rendered}");
            assert!(!rendered.contains("170"), "raw bytes leaked: {rendered}");
            assert!(!rendered.contains("0x11"), "raw bytes leaked: {rendered}");
        }
        // And the input types too.
        assert!(format!("{:?}", OtaaKeys { app_key: APP }).contains("redacted"));
        assert!(format!(
            "{:?}",
            AbpKeys {
                dev_addr: [0; 4],
                nwk_s_key: NWKS,
                app_s_key: APPS
            }
        )
        .contains("redacted"));
    }
}
