//! `/data/portal/db.key` — generate once, reuse forever.
//!
//! Ported from `proximos-render.sh`, including the guard that script learned the hard
//! way. Nothing about the file's shape changes at the rename: 64 lowercase hex
//! characters, no trailing newline, 0600 and owned by the portal unit's user. A device
//! that already has a shim-generated key keeps using it, and its database keeps opening.
//!
//! That user was `fiber-portal` and is `proximos` since plan/portal/monolith.md §15
//! (#88). An existing key is never rewritten, so on an upgraded unit it is migration
//! `030-portal-owner.sh`, not this module, that moves the file to the new owner.

use std::path::Path;

use crate::{data_paths, fsutil, run_paths, Error, RenderPaths, Result};

/// Who owns `/data/portal` and `db.key`: the portal unit's `User=`, which is also the
/// group its rendered config is chowned to. One account, so one constant, and the unit
/// contract tests in `lib.rs` check it against `fiber-portal.service` and its tmpfiles
/// fragment.
pub(crate) const OWNER: &str = run_paths::PORTAL.1;

/// The minimum a key file must weigh to be believed, in bytes. The shim's own number.
const MIN_BYTES: usize = 32;

/// 32 bytes of entropy, hex-encoded.
const KEY_BYTES: usize = 32;
const KEY_CHARS: usize = KEY_BYTES * 2;

/// Read the portal's SQLCipher passphrase, generating it if it is absent or unusable.
///
/// # Why the file matters at all
///
/// Every field in `portal/src/config.rs` is `#[serde(default)]`, `db_key` included, so
/// nothing *forces* a renderer to set it and its absence is not a parse error. It is a
/// correctness-and-safety hole anyway: without it the portal opens a plain, unencrypted
/// SQLite database on a device whose database holds patient-adjacent temperature history
/// and a signed-command audit trail. An unencrypted default is not acceptable here, so
/// key generation is not deferred to some later milestone.
///
/// # Why the guard is `is not usable`, not `does not exist`
///
/// The shim's first shipped version wrote a zero-byte `db.key` — it used
/// `od -An -N32 -tx1 /dev/urandom`, and this image's busybox `od` has none of those
/// flags. An existence-only test would have reused that empty file forever, silently
/// encrypting the database with an empty passphrase. A key file that exists but is
/// unusable must be treated as absent, so the check is "non-empty **and** at least
/// [`MIN_BYTES`] bytes".
///
/// # Why the entropy source changed and the format did not
///
/// The shim concatenated two `/proc/sys/kernel/random/uuid` reads and stripped the
/// dashes, because on this image busybox's `od` is crippled and `base64`, `xxd` and
/// `head -c` are absent outright. A Rust process has no such problem and reads
/// `getrandom(2)` directly — the same CSPRNG, 256 bits of it, without the UUID
/// version/variant bits that made the shell version's 244 effective. The *format* is
/// unchanged deliberately: 64 lowercase hex characters with no trailing newline, so the
/// two implementations are interchangeable on a device that has already booted once.
pub(crate) fn load_or_generate(paths: &RenderPaths) -> Result<String> {
    // Same reasoning as the shim's `mkdir -p`: fiber-portal.tmpfiles.conf declares this
    // directory, but the unit that applies it is not ordered after the nofail data.mount.
    let dir = paths.data.join(data_paths::PORTAL_DIR);
    fsutil::ensure_data_dir(&dir, OWNER, 0o750)?;

    let path = paths.data.join(data_paths::DB_KEY);
    if let Some(existing) = read_usable(&path)? {
        return Ok(existing);
    }

    let generated = generate()?;
    fsutil::write_secret(&path, &generated, OWNER)?;
    Ok(generated)
}

/// The existing key, if there is one worth keeping.
fn read_usable(path: &Path) -> Result<Option<String>> {
    let Some(raw) = crate::read_optional(path)? else {
        return Ok(None);
    };
    if raw.len() < MIN_BYTES {
        return Ok(None);
    }
    // The same `tr -d ' \t\n\r'` the shim applied when reading the file back: a key
    // written by hand, or by a future tool that appends a newline, must not silently
    // become a different passphrase than the one on disk.
    let trimmed: String = raw
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\r'))
        .collect();
    if trimmed.len() < MIN_BYTES {
        return Ok(None);
    }
    Ok(Some(trimmed))
}

fn generate() -> Result<String> {
    let mut bytes = [0u8; KEY_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| Error::Invalid {
        path: std::path::PathBuf::from("<getrandom>"),
        message: format!("cannot draw {KEY_BYTES} bytes of entropy for db.key: {e}"),
    })?;

    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    // Loud, not quietly weaker — the shim's `refusing to write a N-char db_key` check.
    // If the encoding above ever changes shape this must fail the render rather than
    // seed a short passphrase, because a short passphrase is indistinguishable from a
    // good one afterwards.
    if hex.len() != KEY_CHARS {
        return Err(Error::Invalid {
            path: std::path::PathBuf::from("<generated>"),
            message: format!(
                "refusing to write a {}-char db_key (expected {KEY_CHARS})",
                hex.len()
            ),
        });
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_64_lowercase_hex_characters() {
        let k = generate().unwrap();
        assert_eq!(k.len(), KEY_CHARS);
        assert!(k
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn two_generated_keys_differ() {
        assert_ne!(generate().unwrap(), generate().unwrap());
    }

    #[test]
    fn a_shim_written_key_is_read_back_unchanged() {
        // 64 hex chars, no trailing newline — exactly what proximos-render.sh's
        // `printf '%s'` produced from two stripped UUIDs.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.key");
        let shim_key = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
        std::fs::write(&path, shim_key).unwrap();
        assert_eq!(read_usable(&path).unwrap().as_deref(), Some(shim_key));
    }

    #[test]
    fn a_trailing_newline_is_stripped_rather_than_becoming_part_of_the_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.key");
        let key = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
        std::fs::write(&path, format!("{key}\n")).unwrap();
        assert_eq!(read_usable(&path).unwrap().as_deref(), Some(key));
    }

    #[test]
    fn a_zero_byte_key_is_treated_as_absent() {
        // The exact file the shim's first shipped version left behind.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.key");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_usable(&path).unwrap(), None);
    }

    #[test]
    fn a_short_key_is_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.key");
        std::fs::write(&path, "cafebabe").unwrap();
        assert_eq!(read_usable(&path).unwrap(), None);
    }

    #[test]
    fn a_whitespace_only_file_that_is_long_enough_is_still_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.key");
        std::fs::write(&path, " ".repeat(64)).unwrap();
        assert_eq!(read_usable(&path).unwrap(), None);
    }
}
