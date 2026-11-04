//! Key-material input — `plan/control/radio/plan.md` §5, `system/design.md` §10, ledger #22.
//!
//! Key material enters through `--keys-file <path>` or `--keys-stdin`, **never argv**. The
//! plan fixes the flags and what is inside them (OTAA `app_key`; ABP `devaddr` + `nwkskey` +
//! `appskey`; P2P `app_key`) but does not specify the file *syntax*.
//!
//! **Chosen here: a flat JSON object, keys named exactly as the plan names them.** JSON
//! because every other file this component writes under `/data` is JSON, so an operator
//! learns one format; flat because the shape is already discriminated by `--otaa`/`--abp` and
//! nesting would invite a second discriminator that could disagree with the flag. Carried to
//! the plan-feedback set for confirmation.
//!
//! ```json
//! { "app_key": "00112233445566778899aabbccddeeff" }
//! { "devaddr": "01020304", "nwkskey": "…32 hex…", "appskey": "…32 hex…" }
//! ```
//!
//! The **mismatch rule** is enforced here, before any RPC: `--otaa` with an ABP-shaped file,
//! or `--abp` with an OTAA-shaped one, is `InvalidArgument` locally.

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::model::{AbpKeys, OtaaKeys};

/// What a keys file turned out to contain. The shape is the discriminator, so a file cannot
/// silently claim to be something it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMaterial {
    Otaa(OtaaKeys),
    Abp(AbpKeys),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeyFile {
    #[serde(default)]
    app_key: Option<String>,
    #[serde(default)]
    devaddr: Option<String>,
    #[serde(default)]
    nwkskey: Option<String>,
    #[serde(default)]
    appskey: Option<String>,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(
        Kind::InvalidArgument,
        Transport::Lorawan,
        Component::EnrollmentRecord,
        msg.into(),
    )
}

/// Parse key material from the file's contents.
///
/// The error messages deliberately never quote the file's content — a malformed key is still
/// a key, and an error string reaches the journal.
pub fn parse(contents: &str) -> Result<KeyMaterial> {
    let raw: RawKeyFile = serde_json::from_str(contents).map_err(|e| {
        invalid(format!(
            "key material must be a flat JSON object with \"app_key\", or \
             \"devaddr\"+\"nwkskey\"+\"appskey\": {e}"
        ))
    })?;

    let has_abp = raw.devaddr.is_some() || raw.nwkskey.is_some() || raw.appskey.is_some();

    match (raw.app_key.as_deref(), has_abp) {
        (Some(_), true) => Err(invalid(
            "key material carries both OTAA and ABP fields; it must be one or the other",
        )),
        (Some(app_key), false) => Ok(KeyMaterial::Otaa(OtaaKeys {
            app_key: hex16(app_key, "app_key")?,
        })),
        (None, true) => {
            let devaddr = raw
                .devaddr
                .ok_or_else(|| invalid("ABP key material is missing \"devaddr\""))?;
            let nwkskey = raw
                .nwkskey
                .ok_or_else(|| invalid("ABP key material is missing \"nwkskey\""))?;
            let appskey = raw
                .appskey
                .ok_or_else(|| invalid("ABP key material is missing \"appskey\""))?;
            Ok(KeyMaterial::Abp(AbpKeys {
                dev_addr: hex4(&devaddr, "devaddr")?,
                nwk_s_key: hex16(&nwkskey, "nwkskey")?,
                app_s_key: hex16(&appskey, "appskey")?,
            }))
        }
        (None, false) => Err(invalid(
            "key material is empty: expected \"app_key\", or \"devaddr\"+\"nwkskey\"+\"appskey\"",
        )),
    }
}

/// The §5 mismatch rule, checked locally before any RPC.
pub fn check_matches_activation(material: &KeyMaterial, otaa_requested: bool) -> Result<()> {
    match (material, otaa_requested) {
        (KeyMaterial::Otaa(_), true) | (KeyMaterial::Abp(_), false) => Ok(()),
        (KeyMaterial::Abp(_), true) => Err(invalid(
            "--otaa was given but the key material is ABP-shaped \
             (devaddr/nwkskey/appskey); refused before any RPC",
        )),
        (KeyMaterial::Otaa(_), false) => Err(invalid(
            "--abp was given but the key material is OTAA-shaped (app_key alone); \
             refused before any RPC",
        )),
    }
}

fn hex16(s: &str, field: &str) -> Result<[u8; 16]> {
    let bytes = hex_bytes(s, field, 16)?;
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex4(s: &str, field: &str) -> Result<[u8; 4]> {
    let bytes = hex_bytes(s, field, 4)?;
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex_bytes(s: &str, field: &str, want: usize) -> Result<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| *c != ':' && *c != '-').collect();
    if !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid(format!("{field} is not hex")));
    }
    if cleaned.len() != want * 2 {
        // Length is safe to report; the value is not.
        return Err(invalid(format!(
            "{field} must be {} hex digits ({want} bytes), got {}",
            want * 2,
            cleaned.len()
        )));
    }
    Ok((0..want)
        .map(|i| {
            let hi = cleaned.as_bytes()[i * 2];
            let lo = cleaned.as_bytes()[i * 2 + 1];
            (hex_val(hi) << 4) | hex_val(lo)
        })
        .collect())
}

const fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP_HEX: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn an_otaa_file_parses() {
        let m = parse(&format!("{{\"app_key\": \"{APP_HEX}\"}}")).expect("parses");
        match m {
            KeyMaterial::Otaa(k) => assert_eq!(k.app_key[0], 0x00),
            other => panic!("expected OTAA, got {other:?}"),
        }
    }

    #[test]
    fn an_abp_file_parses() {
        let json = format!(
            "{{\"devaddr\":\"01020304\",\"nwkskey\":\"{APP_HEX}\",\"appskey\":\"{APP_HEX}\"}}"
        );
        match parse(&json).expect("parses") {
            KeyMaterial::Abp(k) => assert_eq!(k.dev_addr, [1, 2, 3, 4]),
            other => panic!("expected ABP, got {other:?}"),
        }
    }

    /// §5's mismatch rule: refused locally, before any RPC.
    #[test]
    fn activation_and_key_shape_must_agree() {
        let otaa = parse(&format!("{{\"app_key\":\"{APP_HEX}\"}}")).expect("parses");
        let abp = parse(&format!(
            "{{\"devaddr\":\"01020304\",\"nwkskey\":\"{APP_HEX}\",\"appskey\":\"{APP_HEX}\"}}"
        ))
        .expect("parses");

        assert!(check_matches_activation(&otaa, true).is_ok());
        assert!(check_matches_activation(&abp, false).is_ok());

        let e = check_matches_activation(&abp, true).expect_err("--otaa with an ABP file");
        assert_eq!(e.kind(), Kind::InvalidArgument);
        assert!(e.message().contains("before any RPC"));

        let e = check_matches_activation(&otaa, false).expect_err("--abp with an OTAA file");
        assert_eq!(e.kind(), Kind::InvalidArgument);
    }

    #[test]
    fn a_mixed_file_is_refused() {
        let json = format!("{{\"app_key\":\"{APP_HEX}\",\"nwkskey\":\"{APP_HEX}\"}}");
        let e = parse(&json).expect_err("both shapes at once");
        assert!(e.message().contains("one or the other"));
    }

    #[test]
    fn wrong_length_and_non_hex_are_refused() {
        for bad in ["\"00\"", "\"zz112233445566778899aabbccddeeff\"", "\"\""] {
            let e = parse(&format!("{{\"app_key\": {bad}}}")).expect_err("must reject");
            assert_eq!(e.kind(), Kind::InvalidArgument);
        }
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let e = parse(&format!(
            "{{\"app_key\":\"{APP_HEX}\",\"appkey\":\"{APP_HEX}\"}}"
        ))
        .expect_err("a typo must not be silently dropped");
        assert_eq!(e.kind(), Kind::InvalidArgument);
    }

    /// An error message reaches the journal; the key must not go with it.
    #[test]
    fn error_messages_never_quote_the_key_material() {
        let e = parse("{\"app_key\": \"00112233\"}").expect_err("too short");
        assert!(!e.message().contains("00112233"), "leaked: {}", e.message());
        assert!(e.message().contains("32 hex digits"));

        let e = parse("{\"app_key\": \"zz112233445566778899aabbccddeeff\"}").expect_err("non-hex");
        assert!(!e.message().contains("zz11"), "leaked: {}", e.message());
    }
}
