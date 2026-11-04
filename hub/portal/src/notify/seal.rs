//! At-rest sealing for notification secrets (SendGrid API key, SMTP
//! password, webhook header values) — same construction as the key-wrap
//! in `keys/service.rs` (`wrap`/`unwrap_seed`), with its own
//! domain-separation context so notify blobs and key blobs can never be
//! swapped for one another:
//!
//! - key: `SHA-256("fiber-viewer-notify" || db_key_utf8)`; when the config
//!   has no `db_key` (dev), the derivation runs over 16 zero bytes — a
//!   fixed, worthless key that keeps the storage format identical (see
//!   keys/service.rs module docs for the same reasoning).
//! - blob: `12-byte random nonce || AES-256-GCM ciphertext+tag`, fresh
//!   nonce per seal, no AAD. Unlike the key wrap, plaintext length is
//!   arbitrary (secrets aren't fixed 32-byte seeds).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Domain-separation prefix (the key wrap uses "fiber-viewer-keywrap").
///
/// **Keeps the pre-rename `fiber-viewer` spelling on purpose** — same reason
/// as `keys::service::WRAP_CONTEXT`: it is an input to the seal key, so
/// renaming it silently orphans every notification secret already sealed on
/// disk. A stored-data format, not a component name.
const SEAL_CONTEXT: &[u8] = b"fiber-viewer-notify";
/// AES-GCM standard 96-bit nonce, prepended to every blob.
const NONCE_LEN: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("seal failed")]
    Seal,
    /// Covers both truncated blobs and wrong-key/tampered ciphertexts —
    /// deliberately not distinguished (v1 surfaced this as a single
    /// "headers no longer decrypt" condition).
    #[error("unseal failed: blob is truncated, tampered, or sealed under a different key")]
    Unseal,
}

/// Seals/unseals notification secrets under the db-key-derived key.
#[derive(Clone)]
pub struct NotifySealer {
    key: Zeroizing<[u8; 32]>,
}

impl NotifySealer {
    /// Derives the sealing key from `db_key` (dev fallback: 16 zero bytes,
    /// same as `keys::KeyService::open`).
    pub fn new(db_key: Option<&str>) -> Self {
        let material: &[u8] = match db_key {
            Some(key) => key.as_bytes(),
            None => &[0u8; 16],
        };
        let mut hasher = Sha256::new();
        hasher.update(SEAL_CONTEXT);
        hasher.update(material);
        Self {
            key: Zeroizing::new(hasher.finalize().into()),
        }
    }

    fn cipher(&self) -> Result<Aes256Gcm, SealError> {
        Aes256Gcm::new_from_slice(&*self.key).map_err(|_| SealError::Seal)
    }

    /// `nonce || AES-256-GCM(plaintext)` with a fresh random nonce.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
        let cipher = self.cipher()?;

        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);

        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| SealError::Seal)?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Reverses [`Self::seal`]. The plaintext zeroizes on drop.
    pub fn unseal(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealError> {
        if blob.len() <= NONCE_LEN {
            return Err(SealError::Unseal);
        }
        let (nonce, ciphertext) = blob.split_at(NONCE_LEN);

        let cipher = self.cipher().map_err(|_| SealError::Unseal)?;
        cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map(Zeroizing::new)
            .map_err(|_| SealError::Unseal)
    }

    /// Convenience for string secrets (API keys, passwords, header
    /// values): unseal + UTF-8 decode.
    pub fn unseal_string(&self, blob: &[u8]) -> Result<Zeroizing<String>, SealError> {
        let bytes = self.unseal(blob)?;
        std::str::from_utf8(&bytes)
            .map(|s| Zeroizing::new(s.to_string()))
            .map_err(|_| SealError::Unseal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_dev_and_prod_keys() {
        for db_key in [None, Some("super-secret-db-key")] {
            let sealer = NotifySealer::new(db_key);
            for secret in [
                &b"SG.abc123"[..],
                b"",
                b"{\"X-Token\":\"tok\"}",
                &[0u8; 4096][..],
            ] {
                let blob = sealer.seal(secret).expect("seal");
                if !secret.is_empty() {
                    assert_ne!(&blob[NONCE_LEN..], secret, "never plaintext at rest");
                }
                assert_eq!(&*sealer.unseal(&blob).expect("unseal"), secret);
            }
        }
    }

    #[test]
    fn fresh_nonce_per_seal() {
        let sealer = NotifySealer::new(None);
        let a = sealer.seal(b"secret").expect("seal a");
        let b = sealer.seal(b"secret").expect("seal b");
        assert_ne!(a, b);
        assert_ne!(a[..NONCE_LEN], b[..NONCE_LEN]);
    }

    #[test]
    fn wrong_key_tamper_and_truncation_fail() {
        let dev = NotifySealer::new(None);
        let prod = NotifySealer::new(Some("real-key"));
        let blob = dev.seal(b"smtp-password").expect("seal");

        assert!(matches!(prod.unseal(&blob), Err(SealError::Unseal)));

        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(matches!(dev.unseal(&tampered), Err(SealError::Unseal)));

        assert!(matches!(
            dev.unseal(&blob[..NONCE_LEN]),
            Err(SealError::Unseal)
        ));
        assert!(matches!(dev.unseal(&[]), Err(SealError::Unseal)));
    }

    #[test]
    fn string_helper_roundtrip_and_utf8_guard() {
        let sealer = NotifySealer::new(Some("k"));
        let blob = sealer.seal("héslo-π".as_bytes()).expect("seal");
        assert_eq!(&**sealer.unseal_string(&blob).expect("unseal"), "héslo-π");

        let non_utf8 = sealer.seal(&[0xff, 0xfe]).expect("seal");
        assert!(matches!(
            sealer.unseal_string(&non_utf8),
            Err(SealError::Unseal)
        ));
    }
}
