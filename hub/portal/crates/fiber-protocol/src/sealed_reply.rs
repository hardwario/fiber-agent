//! The requester's half of `cluster-client-add`'s sealed password (plan/control/cluster.md §7,
//! amended 2026-10-02).
//!
//! A Server Hub's agent answers `cluster-client-add` on `proximos/<server>/config/response`,
//! which every subscriber of the anonymous loopback listener can read. So the agent never
//! publishes the Client's new broker password in clear. The requester, which is the portal's
//! Add-hub flow or `proximosctl cluster client-add`, generates an **ephemeral X25519 key pair
//! for that one request**. It sends the public half as `params.reply_key`, inside the signed
//! request, so the signature authenticates it. The agent then seals the password to it:
//!
//! ```text
//! reply_key = base64url(X25519 public key of the requester), 32 bytes, no padding
//! esk, epk  = a fresh X25519 key pair of the agent, for this one answer
//! shared    = X25519(esk, reply_key)              -- refused if all zero (a low-order key)
//! info      = "proximos/cluster-client-add/password-seal/v1" 0x00 request_id 0x00 hostname
//! key       = HKDF-SHA256(salt = epk || reply_key, ikm = shared, info, 32 bytes)
//! nonce     = 12 random bytes
//! ct        = AES-256-GCM(key, nonce, plaintext = the password's UTF-8, aad = empty)
//! result.password_sealed = {"epk": b64url(epk), "nonce": b64url(nonce), "ct": b64url(ct || tag)}
//! ```
//!
//! `request_id` is the signed request's own id, and `hostname` is the Client's, in the
//! lowercase form the agent registered. Both are in `info`, so a sealed password cannot be
//! replayed into the answer to another request or for another Client. Every base64 here is
//! base64url without padding.
//!
//! The agent's half is `control-core/src/libs/cluster_clients/seal.rs`. The two share no code.
//! `vectors/sealed_reply.json` pins the construction for both of them, and for the dev mock. It
//! was generated with Python `cryptography`, a third implementation.
//!
//! The primitives are the ones the lock file already carries. X25519 is `curve25519-dalek`'s
//! Montgomery ladder, which is what `x25519-dalek` wraps; it arrives with `ed25519-dalek`.
//! HKDF-SHA256 is RFC 5869 over `hmac`, which arrives with `pbkdf2`. AES-256-GCM is `aes-gcm`,
//! which [`crate::pairing`] already uses. Secrets live in `Zeroizing` buffers and are wiped
//! when dropped.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use curve25519_dalek::montgomery::MontgomeryPoint;
use hmac::{Hmac, Mac};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

/// The HKDF `info` label, version 1. A change of construction is a new label.
pub const LABEL: &str = "proximos/cluster-client-add/password-seal/v1";
/// Length of an X25519 key in bytes.
pub const KEY_LEN: usize = 32;
/// AES-GCM nonce length.
pub const NONCE_LEN: usize = 12;

/// `result.password_sealed`. Public material only: the agent's ephemeral public key, the
/// nonce and the ciphertext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed {
    pub epk: String,
    pub nonce: String,
    pub ct: String,
}

/// Why a sealed password did not open. Deliberately coarse: a wrong key and a tampered
/// ciphertext look the same, as an AEAD makes them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    #[error("password_sealed is malformed: {0}")]
    Malformed(&'static str),
    #[error("password_sealed does not open with this request's key")]
    Rejected,
}

/// The requester's ephemeral secret for one request. No `Clone`, no `Serialize`, and a `Debug`
/// that prints nothing. [`ReplySecret::open`] consumes it, so one key opens one answer. The
/// bytes are wiped when it is dropped.
pub struct ReplySecret(Zeroizing<[u8; KEY_LEN]>);

impl std::fmt::Debug for ReplySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplySecret(<redacted>)")
    }
}

impl ReplySecret {
    /// 32 bytes from the OS. X25519 clamps them at every use.
    pub fn generate() -> Self {
        let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
        rand::rngs::OsRng.fill_bytes(bytes.as_mut());
        ReplySecret(bytes)
    }

    /// A given secret, for the test vectors.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        ReplySecret(Zeroizing::new(bytes))
    }

    /// `params.reply_key`: the public half, in base64url without padding (43 characters).
    pub fn reply_key(&self) -> String {
        B64.encode(MontgomeryPoint::mul_base_clamped(*self.0).to_bytes())
    }

    /// Open `sealed`, the answer to `request_id` for `hostname`. The password comes back in a
    /// buffer that wipes itself when dropped.
    pub fn open(
        self,
        sealed: &Sealed,
        request_id: &str,
        hostname: &str,
    ) -> Result<Zeroizing<String>, OpenError> {
        let epk: [u8; KEY_LEN] = decode_exact(&sealed.epk, "epk is not 32 bytes of base64url")?;
        let nonce: [u8; NONCE_LEN] =
            decode_exact(&sealed.nonce, "nonce is not 12 bytes of base64url")?;
        let ct = B64
            .decode(&sealed.ct)
            .map_err(|_| OpenError::Malformed("ct is not base64url"))?;
        let reply_key = MontgomeryPoint::mul_base_clamped(*self.0).to_bytes();
        let shared = Zeroizing::new(MontgomeryPoint(epk).mul_clamped(*self.0).to_bytes());
        if shared.iter().all(|b| *b == 0) {
            return Err(OpenError::Rejected);
        }
        let key = derive_key(&epk, &reply_key, &shared, &info(request_id, hostname));
        let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| OpenError::Rejected)?;
        let plain = Zeroizing::new(
            cipher
                .decrypt(Nonce::from_slice(&nonce), ct.as_slice())
                .map_err(|_| OpenError::Rejected)?,
        );
        let text = std::str::from_utf8(&plain).map_err(|_| OpenError::Rejected)?;
        Ok(Zeroizing::new(text.to_string()))
    }
}

/// `info` for one request: the label, then the request id and the Client's hostname, each
/// after one zero byte.
pub fn info(request_id: &str, hostname: &str) -> Vec<u8> {
    let mut info = Vec::with_capacity(LABEL.len() + request_id.len() + hostname.len() + 2);
    info.extend_from_slice(LABEL.as_bytes());
    info.push(0);
    info.extend_from_slice(request_id.as_bytes());
    info.push(0);
    info.extend_from_slice(hostname.as_bytes());
    info
}

/// HKDF-SHA256 (RFC 5869) with one 32-byte output block: `PRK = HMAC(salt, ikm)`, then
/// `OKM = HMAC(PRK, info || 0x01)`. The salt is `epk || reply_key`.
fn derive_key(
    epk: &[u8; KEY_LEN],
    reply_key: &[u8; KEY_LEN],
    shared: &[u8; KEY_LEN],
    info: &[u8],
) -> Zeroizing<[u8; 32]> {
    let mut salt = [0u8; 2 * KEY_LEN];
    salt[..KEY_LEN].copy_from_slice(epk);
    salt[KEY_LEN..].copy_from_slice(reply_key);
    hkdf_sha256_32(&salt, shared, info)
}

/// RFC 5869 extract-and-expand for L = 32, which is one block of SHA-256.
pub(crate) fn hkdf_sha256_32(salt: &[u8], ikm: &[u8], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut extract =
        <Hmac<Sha256> as Mac>::new_from_slice(salt).expect("HMAC takes a key of any length");
    extract.update(ikm);
    let prk = Zeroizing::new(<[u8; 32]>::from(extract.finalize().into_bytes()));
    let mut expand = <Hmac<Sha256> as Mac>::new_from_slice(prk.as_ref())
        .expect("HMAC takes a key of any length");
    expand.update(info);
    expand.update(&[1u8]);
    Zeroizing::new(<[u8; 32]>::from(expand.finalize().into_bytes()))
}

/// The **agent's** half, for the portal-side code that plays the agent: the portal's route tests
/// answer `cluster-client-add` with it. The real agent does not use this function; its own copy
/// is `control-core/src/libs/cluster_clients/seal.rs`. Both are pinned by the same vector.
/// `reply_key` is the request's `params.reply_key`. The ephemeral key and the nonce are fresh
/// for every call.
pub fn seal(
    password: &str,
    reply_key: &str,
    request_id: &str,
    hostname: &str,
) -> Result<Sealed, OpenError> {
    let mut esk = Zeroizing::new([0u8; KEY_LEN]);
    rand::rngs::OsRng.fill_bytes(esk.as_mut());
    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    seal_with(password, reply_key, request_id, hostname, &esk, nonce)
}

fn seal_with(
    password: &str,
    reply_key: &str,
    request_id: &str,
    hostname: &str,
    esk: &[u8; KEY_LEN],
    nonce: [u8; NONCE_LEN],
) -> Result<Sealed, OpenError> {
    let reply_key: [u8; KEY_LEN] =
        decode_exact(reply_key, "reply_key is not 32 bytes of base64url")?;
    let epk = MontgomeryPoint::mul_base_clamped(*esk).to_bytes();
    let shared = Zeroizing::new(MontgomeryPoint(reply_key).mul_clamped(*esk).to_bytes());
    if shared.iter().all(|b| *b == 0) {
        return Err(OpenError::Rejected);
    }
    let key = derive_key(&epk, &reply_key, &shared, &info(request_id, hostname));
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| OpenError::Rejected)?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), password.as_bytes())
        .map_err(|_| OpenError::Rejected)?;
    Ok(Sealed {
        epk: B64.encode(epk),
        nonce: B64.encode(nonce),
        ct: B64.encode(ct),
    })
}

fn decode_exact<const N: usize>(text: &str, why: &'static str) -> Result<[u8; N], OpenError> {
    B64.decode(text)
        .ok()
        .and_then(|v| <[u8; N]>::try_from(v.as_slice()).ok())
        .ok_or(OpenError::Malformed(why))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn vector() -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/vectors/sealed_reply.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn hex32(v: &Value) -> [u8; 32] {
        hex::decode(v.as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn sealed(v: &Value) -> Sealed {
        serde_json::from_value(v["sealed"].clone()).unwrap()
    }

    /// RFC 7748 §6.1: the curve is X25519, the clamping included.
    #[test]
    fn x25519_is_rfc_7748() {
        let alice: [u8; 32] =
            hex::decode("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a")
                .unwrap()
                .try_into()
                .unwrap();
        let bob_pub: [u8; 32] =
            hex::decode("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
                .unwrap()
                .try_into()
                .unwrap();
        assert_eq!(
            hex::encode(MontgomeryPoint::mul_base_clamped(alice).to_bytes()),
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
        );
        assert_eq!(
            hex::encode(MontgomeryPoint(bob_pub).mul_clamped(alice).to_bytes()),
            "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742"
        );
    }

    /// RFC 5869 appendix A.1.
    #[test]
    fn hkdf_is_rfc_5869() {
        let ikm = [0x0bu8; 22];
        let salt = hex::decode("000102030405060708090a0b0c").unwrap();
        let info = hex::decode("f0f1f2f3f4f5f6f7f8f9").unwrap();
        assert_eq!(
            hex::encode(hkdf_sha256_32(&salt, &ikm, &info).as_ref()),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf"
        );
    }

    /// The checked-in vector, made by Python `cryptography`: every step agrees, and it opens.
    #[test]
    fn the_vector_opens() {
        let v = vector();
        let secret = ReplySecret::from_bytes(hex32(&v["reply_secret_hex"]));
        assert_eq!(secret.reply_key(), v["reply_key"].as_str().unwrap());
        let request_id = v["request_id"].as_str().unwrap();
        let hostname = v["hostname"].as_str().unwrap();
        assert_eq!(
            hex::encode(info(request_id, hostname)),
            v["info_hex"].as_str().unwrap()
        );
        assert_eq!(v["label"].as_str().unwrap(), LABEL);
        let opened = secret.open(&sealed(&v), request_id, hostname).unwrap();
        assert_eq!(opened.as_str(), v["password"].as_str().unwrap());
    }

    /// The agent-side copy here reproduces the vector too, and round-trips through `open`.
    #[test]
    fn the_test_seal_reproduces_the_vector() {
        let v = vector();
        let esk = hex32(&v["agent_secret_hex"]);
        let nonce: [u8; 12] = hex::decode(v["nonce_hex"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let (rid, host) = (
            v["request_id"].as_str().unwrap(),
            v["hostname"].as_str().unwrap(),
        );
        let made = seal_with(
            v["password"].as_str().unwrap(),
            v["reply_key"].as_str().unwrap(),
            rid,
            host,
            &esk,
            nonce,
        )
        .unwrap();
        assert_eq!(made, sealed(&v));
        let secret = ReplySecret::generate();
        let fresh = seal("pw", &secret.reply_key(), "r", "h").unwrap();
        assert_eq!(secret.open(&fresh, "r", "h").unwrap().as_str(), "pw");
    }

    /// A wrong key, a wrong request id, a wrong hostname or one flipped bit anywhere: it does not
    /// open, and the error says nothing more.
    #[test]
    fn a_wrong_key_or_context_or_a_tampered_field_does_not_open() {
        let v = vector();
        let good = || ReplySecret::from_bytes(hex32(&v["reply_secret_hex"]));
        let (rid, host) = (
            v["request_id"].as_str().unwrap(),
            v["hostname"].as_str().unwrap(),
        );
        let s = sealed(&v);

        assert_eq!(
            ReplySecret::generate().open(&s, rid, host).unwrap_err(),
            OpenError::Rejected
        );
        assert_eq!(
            good().open(&s, "another-request", host).unwrap_err(),
            OpenError::Rejected
        );
        assert_eq!(
            good().open(&s, rid, "proximos-00000000").unwrap_err(),
            OpenError::Rejected
        );
        let flip = |field: &str, bit: usize| {
            let mut bytes = B64.decode(field).unwrap();
            bytes[bit / 8] ^= 1 << (bit % 8);
            B64.encode(bytes)
        };
        for n in [0, 7, 100, 22 * 8 + 3] {
            let t = Sealed {
                ct: flip(&s.ct, n),
                ..s.clone()
            };
            assert_eq!(
                good().open(&t, rid, host).unwrap_err(),
                OpenError::Rejected,
                "ct bit {n}"
            );
        }
        for n in [0, 95] {
            let t = Sealed {
                nonce: flip(&s.nonce, n),
                ..s.clone()
            };
            assert_eq!(good().open(&t, rid, host).unwrap_err(), OpenError::Rejected);
        }
        let t = Sealed {
            epk: flip(&s.epk, 9),
            ..s.clone()
        };
        assert_eq!(good().open(&t, rid, host).unwrap_err(), OpenError::Rejected);
        let short = Sealed {
            epk: B64.encode([1u8; 31]),
            ..s.clone()
        };
        assert!(matches!(
            good().open(&short, rid, host),
            Err(OpenError::Malformed(_))
        ));
        // A low-order agent key gives an all-zero shared secret, which is refused outright.
        let zero = Sealed {
            epk: B64.encode([0u8; 32]),
            ..s
        };
        assert_eq!(
            good().open(&zero, rid, host).unwrap_err(),
            OpenError::Rejected
        );
    }

    #[test]
    fn a_reply_key_is_43_characters_and_the_secret_never_prints() {
        let secret = ReplySecret::generate();
        let key = secret.reply_key();
        assert_eq!(key.len(), 43);
        assert_eq!(B64.decode(&key).unwrap().len(), 32);
        assert_eq!(format!("{secret:?}"), "ReplySecret(<redacted>)");
        assert_ne!(key, ReplySecret::generate().reply_key());
    }
}
