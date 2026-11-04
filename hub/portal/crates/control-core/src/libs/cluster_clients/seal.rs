//! The agent's half of `cluster-client-add`'s sealed password (plan/control/cluster.md §7,
//! amended 2026-10-02).
//!
//! The answer to `cluster-client-add` travels on `proximos/<server>/config/response`, which every
//! subscriber of the anonymous loopback listener can read. The new password is therefore never
//! put in it in clear. It is sealed to the `reply_key` the requester signed into its request:
//!
//! ```text
//! esk, epk = a fresh X25519 key pair, for this one answer
//! shared   = X25519(esk, reply_key)               -- refused if all zero
//! info     = "proximos/cluster-client-add/password-seal/v1" 0x00 request_id 0x00 hostname
//! key      = HKDF-SHA256(salt = epk || reply_key, ikm = shared, info, 32 bytes)
//! ct       = AES-256-GCM(key, 12 random nonce bytes, the password, aad = empty)
//! password_sealed = {"epk", "nonce", "ct"}, each base64url without padding
//! ```
//!
//! The requester's half is `fiber_protocol::sealed_reply`. This crate shares no code with it,
//! as with pairing: `fiber-protocol/vectors/sealed_reply.json` pins both, and was generated with
//! a third implementation. The primitives are the ones already in the lock: `curve25519-dalek`'s
//! Montgomery ladder for X25519, HKDF over `hmac`, and `aes-gcm` as `pairing::crypto` uses it.
//! Every secret, from the ephemeral scalar to the derived key, lives in a `Zeroizing` buffer.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use curve25519_dalek::montgomery::MontgomeryPoint;
use hmac::{Hmac, Mac};
use rand::RngCore as _;
use serde::Serialize;
use sha2::Sha256;
use zeroize::Zeroizing;

/// The HKDF `info` label, version 1, `fiber_protocol::sealed_reply::LABEL`'s twin.
pub const LABEL: &str = "proximos/cluster-client-add/password-seal/v1";

/// The requester's X25519 public key, as it arrives in `params.reply_key`. Public material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyKey([u8; 32]);

impl ReplyKey {
    /// base64url without padding, exactly 32 bytes, and not all zero. A key of small order
    /// passes this check; [`seal`] refuses it, because the shared secret comes out all zero.
    pub fn parse(text: &str) -> Result<ReplyKey, String> {
        let bytes = B64
            .decode(text.trim())
            .ok()
            .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
            .ok_or("reply_key is not an X25519 public key: expected 32 bytes of base64url")?;
        if bytes.iter().all(|b| *b == 0) {
            return Err("reply_key is not an X25519 public key: it is all zero".to_string());
        }
        Ok(ReplyKey(bytes))
    }

    pub fn as_base64(&self) -> String {
        B64.encode(self.0)
    }
}

/// The requester's key and the id of the request it signed, which the seal is bound to. The
/// hostname is the third binding, and [`super::add`] has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTo {
    pub key: ReplyKey,
    pub request_id: String,
}

impl ReplyTo {
    /// `request_id` must be what the requester will look the answer up by: not empty, at most
    /// 128 characters, printable ASCII. The signed path's is the signed request's own id, and
    /// the control socket's is one `proximosctl` makes up.
    pub fn new(reply_key: &str, request_id: &str) -> Result<ReplyTo, String> {
        let key = ReplyKey::parse(reply_key)?;
        if request_id.is_empty()
            || request_id.len() > 128
            || !request_id.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(
                "request_id must be 1 to 128 printable ASCII characters, for the sealed reply"
                    .to_string(),
            );
        }
        Ok(ReplyTo {
            key,
            request_id: request_id.to_string(),
        })
    }
}

/// `result.password_sealed`. Public material only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sealed {
    pub epk: String,
    pub nonce: String,
    pub ct: String,
}

/// Seal `password` to `reply`, for the Client `hostname`, with a fresh ephemeral key and nonce.
pub fn seal(password: &str, reply: &ReplyTo, hostname: &str) -> Result<Sealed, String> {
    let mut esk = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(esk.as_mut());
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    seal_with(password, reply, hostname, &esk, nonce)
}

/// [`seal`] with a given ephemeral secret and nonce, for the test vector.
pub(crate) fn seal_with(
    password: &str,
    reply: &ReplyTo,
    hostname: &str,
    esk: &[u8; 32],
    nonce: [u8; 12],
) -> Result<Sealed, String> {
    let epk = MontgomeryPoint::mul_base_clamped(*esk).to_bytes();
    let shared = Zeroizing::new(MontgomeryPoint(reply.key.0).mul_clamped(*esk).to_bytes());
    if shared.iter().all(|b| *b == 0) {
        return Err("reply_key is not a usable X25519 public key (it has small order)".to_string());
    }
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(&epk);
    salt[32..].copy_from_slice(&reply.key.0);
    let key = hkdf_sha256_32(&salt, shared.as_ref(), &info(&reply.request_id, hostname));
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())
        .map_err(|_| "cannot build the AES-256-GCM key".to_string())?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), password.as_bytes())
        .map_err(|_| "cannot seal the password".to_string())?;
    Ok(Sealed {
        epk: B64.encode(epk),
        nonce: B64.encode(nonce),
        ct: B64.encode(ct),
    })
}

fn info(request_id: &str, hostname: &str) -> Vec<u8> {
    let mut info = Vec::with_capacity(LABEL.len() + request_id.len() + hostname.len() + 2);
    info.extend_from_slice(LABEL.as_bytes());
    info.push(0);
    info.extend_from_slice(request_id.as_bytes());
    info.push(0);
    info.extend_from_slice(hostname.as_bytes());
    info
}

/// RFC 5869 for L = 32: `PRK = HMAC(salt, ikm)`, `OKM = HMAC(PRK, info || 0x01)`.
fn hkdf_sha256_32(salt: &[u8], ikm: &[u8], info: &[u8]) -> Zeroizing<[u8; 32]> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_protocol::sealed_reply::{ReplySecret, Sealed as WireSealed};
    use serde_json::Value;

    fn vector() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../fiber-protocol/vectors/sealed_reply.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn hex_bytes<const N: usize>(v: &Value) -> [u8; N] {
        hex::decode(v.as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn wire(s: &Sealed) -> WireSealed {
        serde_json::from_value(serde_json::to_value(s).unwrap()).unwrap()
    }

    /// Given the vector's ephemeral secret and nonce, this seal is byte for byte what Python
    /// `cryptography` produced.
    #[test]
    fn the_seal_reproduces_the_vector() {
        let v = vector();
        assert_eq!(v["label"].as_str().unwrap(), LABEL);
        let reply = ReplyTo::new(
            v["reply_key"].as_str().unwrap(),
            v["request_id"].as_str().unwrap(),
        )
        .unwrap();
        let sealed = seal_with(
            v["password"].as_str().unwrap(),
            &reply,
            v["hostname"].as_str().unwrap(),
            &hex_bytes::<32>(&v["agent_secret_hex"]),
            hex_bytes::<12>(&v["nonce_hex"]),
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&sealed).unwrap(), v["sealed"]);
    }

    /// The portal's half opens a live seal, and only with the key, request and Client it was
    /// made for.
    #[test]
    fn the_requesters_half_opens_a_fresh_seal_and_nothing_else() {
        let secret = || ReplySecret::from_bytes([7u8; 32]);
        let reply = ReplyTo::new(&secret().reply_key(), "req-1").unwrap();
        let sealed = seal("Zq3-Xr9_Lm2Kp7Wn4Vb8Tc", &reply, "proximos-1a2b3c4d").unwrap();
        assert_ne!(
            sealed,
            seal("Zq3-Xr9_Lm2Kp7Wn4Vb8Tc", &reply, "proximos-1a2b3c4d").unwrap()
        );
        assert!(!serde_json::to_string(&sealed)
            .unwrap()
            .contains("Zq3-Xr9_Lm2Kp7Wn4Vb8Tc"));
        let opened = secret()
            .open(&wire(&sealed), "req-1", "proximos-1a2b3c4d")
            .unwrap();
        assert_eq!(opened.as_str(), "Zq3-Xr9_Lm2Kp7Wn4Vb8Tc");
        assert!(ReplySecret::from_bytes([8u8; 32])
            .open(&wire(&sealed), "req-1", "proximos-1a2b3c4d")
            .is_err());
        assert!(secret()
            .open(&wire(&sealed), "req-2", "proximos-1a2b3c4d")
            .is_err());
        assert!(secret()
            .open(&wire(&sealed), "req-1", "proximos-00000000")
            .is_err());
    }

    #[test]
    fn a_reply_key_or_request_id_that_cannot_be_used_is_refused() {
        for bad in [
            "",
            "not base64!",
            &B64.encode([1u8; 31]),
            &B64.encode([1u8; 33]),
            &B64.encode([0u8; 32]),
        ] {
            assert!(ReplyKey::parse(bad).is_err(), "{bad:?}");
        }
        let key = B64.encode([9u8; 32]);
        for bad in ["", "a b", &"x".repeat(129), "é"] {
            assert!(ReplyTo::new(&key, bad).is_err(), "{bad:?}");
        }
        // A point of small order passes the parse and is refused at the seal: the order-1 point
        // (u = 1) gives an all-zero shared secret whatever the scalar.
        let mut one = [0u8; 32];
        one[0] = 1;
        let reply = ReplyTo::new(&B64.encode(one), "req").unwrap();
        assert!(seal("pw", &reply, "proximos-1a2b3c4d").is_err());
    }
}
