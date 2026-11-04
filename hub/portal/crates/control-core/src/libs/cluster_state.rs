//! One home for a Hub's cluster role and a Client's connection to its Server
//! (`plan/control/cluster.md` §3, decision #38).
//!
//! ```text
//! /data/proximos/cluster/
//!   server_host        # the Server's name, as the join bundle's `h` gave it
//!   server_port        # 8883
//!   peer_username      # peer-<own hostname>
//!   peer_password      # 0600
//!   ca.crt             # the Server's CA, exactly the bytes whose SHA-256 the bundle carried
//!   role               # "client" — written LAST, removed FIRST
//! ```
//!
//! The renderer's input (`proximos-render`'s `read_cluster_role` / `read_cluster_peer`) and,
//! since milestone C4, the join's output (`libs::cluster_join`). A Server needs no marker:
//! absence means server (decisions #5, #11).
//!
//! **Write discipline**, carried over from the v1 port's `ClusterState` (retired with its
//! `/data/fiber/cluster`, its `leader | follower | standalone` roles and its
//! `fiber-cluster-apply` unit): one value per file, plain text, no trailing newline, so the
//! renderer needs no parser; every file is written to a temporary file in the same directory
//! and renamed into place; `role` is written **last**, so a crash part-way through leaves a
//! Hub that is still a Server; and it is removed **first**, so a teardown is a Server from its
//! first step on.
//!
//! This module also holds the account rules both halves of the cluster share — the Server's
//! mint (`libs::cluster_clients`) and the Client's join — so that neither can accept an
//! account the other would refuse.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// Where the role marker and the Client's connection live. Under `/data` because only `/data`
/// survives a RAUC slot switch; wiped by a factory reset with the rest of it.
pub const CLUSTER_DIR: &str = "/data/proximos/cluster";

/// The Server's TLS listener (cluster.md §5). Its plaintext `1883` listener is bound to
/// `127.0.0.1` with `allow_anonymous true`, so it is never a legal cluster target at any port
/// number — see [`validate_server_port`].
pub const DEFAULT_SERVER_PORT: u16 = 8883;

/// Peer accounts are `peer-<client hostname>` (decision #38), so one Client can be revoked on
/// its own and no Client can ever be handed an account a local service uses.
pub const PEER_USERNAME_PREFIX: &str = "peer-";

/// The one value the role marker has. Anything else — or no file at all — is a Server, the
/// way the renderer reads it (an exact token after trimming).
pub const ROLE_CLIENT: &str = "client";

/// The file names of the directory, in the order a join writes them. `role` is last.
pub const SERVER_HOST: &str = "server_host";
pub const SERVER_PORT: &str = "server_port";
pub const PEER_USERNAME: &str = "peer_username";
pub const PEER_PASSWORD: &str = "peer_password";
pub const CA_CERT: &str = "ca.crt";
pub const ROLE: &str = "role";

/// Every file a join writes before the marker, in write order.
pub const CONNECTION_FILES: [&str; 5] = [
    SERVER_HOST,
    SERVER_PORT,
    PEER_USERNAME,
    PEER_PASSWORD,
    CA_CERT,
];

// ---------------------------------------------------------------------------------------------
// The account rules
// ---------------------------------------------------------------------------------------------

/// `peer-<hostname>`: the prefix, at most 128 characters, no `:` (mosquitto's passwd separator)
/// and no whitespace. The rule `validate_arm` enforced in the v1 port, and the one
/// `proximos-render` mirrors.
pub fn validate_peer_username(username: &str) -> Result<(), String> {
    if !username.starts_with(PEER_USERNAME_PREFIX) {
        return Err(format!(
            "the broker account must start with {PEER_USERNAME_PREFIX:?}: a Client never shares \
             an account a local service uses, so it can be revoked on its own"
        ));
    }
    if username.len() > 128
        || username.contains(':')
        || username
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(
            "the broker account must be at most 128 characters, with no ':' or \
                    whitespace"
                .to_string(),
        );
    }
    Ok(())
}

/// At least 16 characters, one token: no whitespace, no control characters, and none of the
/// characters the renderer refuses in a value file (`"`, `\`, `'`, `#`).
pub fn validate_peer_password(password: &str) -> Result<(), String> {
    if password.len() < 16 {
        return Err("the broker password must be at least 16 characters".to_string());
    }
    if password
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\' | '\'' | '#'))
    {
        return Err(
            "the broker password must be one token, with no quotes, backslash or '#'".to_string(),
        );
    }
    Ok(())
}

/// The Server's TLS port: never 0, never the anonymous loopback listener's 1883.
pub fn validate_server_port(port: u16) -> Result<(), String> {
    match port {
        0 => Err("the Server's port must be non-zero".to_string()),
        1883 => Err(
            "port 1883 is the Server's loopback-only, anonymous listener; a Client uses the TLS \
             listener (8883), so the broker is never reachable without a credential"
                .to_string(),
        ),
        _ => Ok(()),
    }
}

/// A gateway EUI as ChirpStack stores it: 16 lowercase hex digits. `:` and `-` separators are
/// accepted, because an EUI copied out of a UI usually arrives with them.
pub(crate) fn normalize_gateway_eui(eui: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = eui.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let cleaned: String = raw.chars().filter(|c| *c != ':' && *c != '-').collect();
    if cleaned.len() != 16 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "peer_gateway_eui {raw:?} is not a 16-digit hexadecimal gateway EUI"
        ));
    }
    Ok(Some(cleaned.to_ascii_lowercase()))
}

/// SHA-256 over a PEM certificate's DER, lowercase hex with no separators — what
/// `openssl x509 -fingerprint -sha256` reports. A CA is identified by this, never by its CN.
///
/// Not the join bundle's `f`: that one is [`bundle_fingerprint`], over the bytes
/// `GET /connection/ca` serves (cluster.md §6).
pub fn ca_fingerprint_sha256(pem: &str) -> Result<String, String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let start = pem
        .find(BEGIN)
        .ok_or("not a PEM certificate (no BEGIN line)")?;
    let body_start = start + BEGIN.len();
    let end = pem[body_start..]
        .find(END)
        .ok_or("not a PEM certificate (no END line)")?
        + body_start;

    let b64: String = pem[body_start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if b64.is_empty() {
        return Err("PEM certificate body is empty".to_string());
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| format!("PEM body is not valid base64: {e}"))?;
    Ok(hex::encode(Sha256::digest(&der)))
}

/// The join bundle's `f`: SHA-256 of `bytes`, base64url without padding, 43 characters. Taken
/// over the CA **exactly as `GET /connection/ca` serves it** (cluster.md §6) — the Server's
/// portal computes it over the same bytes, so no re-encoding happens on either side.
pub fn bundle_fingerprint(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------------------------
// The directory
// ---------------------------------------------------------------------------------------------

/// What a join writes before the marker. Every value has been validated by the caller.
pub struct Connection<'a> {
    pub server_host: &'a str,
    pub server_port: u16,
    pub peer_username: &'a str,
    pub peer_password: &'a str,
    pub ca_pem: &'a [u8],
}

impl std::fmt::Debug for Connection<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("server_host", &self.server_host)
            .field("server_port", &self.server_port)
            .field("peer_username", &self.peer_username)
            .field("peer_password", &"<redacted>")
            .field("ca_pem", &format_args!("{} bytes", self.ca_pem.len()))
            .finish()
    }
}

/// `/data/proximos/cluster/`, or a test's stand-in for it.
#[derive(Debug, Clone)]
pub struct ClusterDir {
    dir: PathBuf,
}

impl ClusterDir {
    pub fn new(dir: impl Into<PathBuf>) -> ClusterDir {
        ClusterDir { dir: dir.into() }
    }

    pub fn at_default() -> ClusterDir {
        ClusterDir::new(CLUSTER_DIR)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The marker as written, trimmed; `None` when there is no file. An unreadable marker is an
    /// error, never "no marker": a Hub that cannot tell whether it is a Client must not act as
    /// if it were sure it is not.
    pub fn role_marker(&self) -> std::io::Result<Option<String>> {
        match fs::read_to_string(self.path(ROLE)) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Does any marker exist, whatever it says?
    pub fn marker_exists(&self) -> std::io::Result<bool> {
        Ok(self.role_marker()?.is_some())
    }

    /// Is this Hub a Client, the way the renderer decides it?
    pub fn is_client(&self) -> std::io::Result<bool> {
        Ok(self.role_marker()?.as_deref() == Some(ROLE_CLIENT))
    }

    /// One value file, trimmed; `None` when absent or unreadable.
    pub fn value(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.path(name))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// The Server this Hub joined, if it is a Client.
    pub fn server_host(&self) -> Option<String> {
        self.value(SERVER_HOST)
    }

    /// Write everything but the marker, in [`CONNECTION_FILES`] order. The directory is created
    /// `0700` if it is missing; `peer_password` is created `0600` and never exists with a wider
    /// mode, not even for a moment.
    pub fn write_connection(&self, c: &Connection<'_>) -> std::io::Result<()> {
        self.ensure_dir()?;
        self.write_file(SERVER_HOST, c.server_host.as_bytes(), 0o644)?;
        self.write_file(SERVER_PORT, c.server_port.to_string().as_bytes(), 0o644)?;
        self.write_file(PEER_USERNAME, c.peer_username.as_bytes(), 0o644)?;
        self.write_file(PEER_PASSWORD, c.peer_password.as_bytes(), 0o600)?;
        self.write_file(CA_CERT, c.ca_pem, 0o644)?;
        self.sync_dir()
    }

    /// The marker, last. From this rename on, the next boot renders a Client.
    pub fn write_role_client(&self) -> std::io::Result<()> {
        self.ensure_dir()?;
        self.write_file(ROLE, ROLE_CLIENT.as_bytes(), 0o644)?;
        self.sync_dir()
    }

    /// Back to a Server: the marker first, then every connection file. Missing files are not an
    /// error, so a teardown of a half-written join finishes the job.
    pub fn clear(&self) -> std::io::Result<()> {
        remove_if_present(&self.path(ROLE))?;
        for name in CONNECTION_FILES {
            remove_if_present(&self.path(name))?;
        }
        if self.dir.exists() {
            self.sync_dir()?;
        }
        Ok(())
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        if self.dir.is_dir() {
            return Ok(());
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
    }

    /// Temp file in the same directory, created with its final mode, synced, renamed.
    fn write_file(&self, name: &str, bytes: &[u8], mode: u32) -> std::io::Result<()> {
        let tmp = self.dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let result = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, self.path(name))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn sync_dir(&self) -> std::io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }
}

fn remove_if_present(p: &Path) -> std::io::Result<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn a_cert() -> String {
        let der = b"hello certificate";
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
    }

    const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eA";

    fn connection<'a>(ca: &'a [u8]) -> Connection<'a> {
        Connection {
            server_host: "proximos-1a2b3c4d.local",
            server_port: 8883,
            peer_username: "peer-proximos-5e6f7a8b",
            peer_password: PASSWORD,
            ca_pem: ca,
        }
    }

    #[test]
    fn the_der_fingerprint_is_sha256_of_the_der_not_the_pem_text() {
        let pem = a_cert();
        let expected = hex::encode(Sha256::digest(b"hello certificate"));
        assert_eq!(ca_fingerprint_sha256(&pem).unwrap(), expected);
        let wrapped = pem.replace("aGVsbG8", "aGVsbG8\n");
        assert_eq!(ca_fingerprint_sha256(&wrapped).unwrap(), expected);
        assert!(ca_fingerprint_sha256("not a certificate").is_err());
        assert!(
            ca_fingerprint_sha256("-----BEGIN CERTIFICATE-----\n\n-----END CERTIFICATE-----")
                .is_err()
        );
    }

    /// The bundle's `f` is over the served bytes, so a different line ending is a different
    /// fingerprint — which is why neither side may re-encode.
    #[test]
    fn the_bundle_fingerprint_is_over_the_bytes_as_served() {
        let pem = a_cert();
        let f = bundle_fingerprint(pem.as_bytes());
        assert_eq!(f.len(), 43);
        assert!(!f.contains('=') && !f.contains('+') && !f.contains('/'));
        assert_ne!(f, bundle_fingerprint(pem.replace('\n', "\r\n").as_bytes()));
        assert_eq!(
            f,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(pem.as_bytes()))
        );
    }

    #[test]
    fn the_account_rules_are_the_v1_ports() {
        validate_peer_username("peer-proximos-5e6f7a8b").unwrap();
        assert!(validate_peer_username("fiber")
            .unwrap_err()
            .contains("peer-"));
        assert!(validate_peer_username("peer-a:b").is_err());
        assert!(validate_peer_username("peer-a b").is_err());
        assert!(validate_peer_username(&format!("peer-{}", "a".repeat(124))).is_err());

        validate_peer_password(PASSWORD).unwrap();
        assert!(validate_peer_password("short")
            .unwrap_err()
            .contains("16 characters"));
        assert!(validate_peer_password("0123456789abcdef ").is_err());
        assert!(validate_peer_password("0123456789abcdef#").is_err());

        validate_server_port(8883).unwrap();
        assert!(validate_server_port(1883).unwrap_err().contains("TLS"));
        assert!(validate_server_port(0).is_err());
    }

    #[test]
    fn a_gateway_eui_is_normalised_or_refused() {
        assert_eq!(
            normalize_gateway_eui(Some("00:16:C0:01:F1:50:08:12")).unwrap(),
            Some("0016c001f1500812".to_string())
        );
        assert_eq!(normalize_gateway_eui(Some("  ")).unwrap(), None);
        assert_eq!(normalize_gateway_eui(None).unwrap(), None);
        assert!(normalize_gateway_eui(Some("nonsense"))
            .unwrap_err()
            .contains("hexadecimal"));
    }

    #[test]
    fn no_marker_is_a_server_and_only_client_is_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let st = ClusterDir::new(dir.path().join("cluster"));
        assert_eq!(st.role_marker().unwrap(), None);
        assert!(!st.is_client().unwrap());
        assert!(!st.marker_exists().unwrap());

        std::fs::create_dir_all(st.dir()).unwrap();
        std::fs::write(st.path(ROLE), "client\n").unwrap();
        assert!(
            st.is_client().unwrap(),
            "a hand-written newline is tolerated"
        );
        std::fs::write(st.path(ROLE), "clients").unwrap();
        assert!(!st.is_client().unwrap(), "an exact token, not a prefix");
        assert!(st.marker_exists().unwrap());
    }

    #[test]
    fn the_connection_files_are_one_token_each_with_the_secret_0600_and_no_marker() {
        let dir = tempfile::tempdir().unwrap();
        let st = ClusterDir::new(dir.path().join("cluster"));
        let ca = a_cert();
        st.write_connection(&connection(ca.as_bytes())).unwrap();

        assert_eq!(
            st.value(SERVER_HOST).as_deref(),
            Some("proximos-1a2b3c4d.local")
        );
        assert_eq!(
            std::fs::read_to_string(st.path(SERVER_PORT)).unwrap(),
            "8883"
        );
        assert_eq!(
            std::fs::read_to_string(st.path(PEER_PASSWORD)).unwrap(),
            PASSWORD,
            "no trailing newline"
        );
        assert_eq!(std::fs::read(st.path(CA_CERT)).unwrap(), ca.as_bytes());
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(st.path(PEER_PASSWORD)), 0o600);
        assert_eq!(mode(st.dir().to_path_buf()), 0o700);
        assert!(
            !st.path(ROLE).exists(),
            "the marker is its own, later, step"
        );
        assert!(!st.is_client().unwrap());

        st.write_role_client().unwrap();
        assert!(st.is_client().unwrap());
        assert_eq!(std::fs::read_to_string(st.path(ROLE)).unwrap(), "client");
        let leftovers: Vec<_> = std::fs::read_dir(st.dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn clearing_takes_the_marker_first_and_tolerates_a_half_written_join() {
        let dir = tempfile::tempdir().unwrap();
        let st = ClusterDir::new(dir.path().join("cluster"));
        let ca = a_cert();
        st.write_connection(&connection(ca.as_bytes())).unwrap();
        std::fs::remove_file(st.path(CA_CERT)).unwrap();
        st.clear().unwrap();
        for name in CONNECTION_FILES.iter().chain([ROLE].iter()) {
            assert!(!st.path(name).exists(), "{name}");
        }
        // Nothing there at all is not an error either.
        ClusterDir::new(dir.path().join("never")).clear().unwrap();
    }

    #[test]
    fn the_debug_form_never_carries_the_secret() {
        let ca = a_cert();
        let shown = format!("{:?}", connection(ca.as_bytes()));
        assert!(!shown.contains(PASSWORD), "{shown}");
        assert!(shown.contains("redacted"));
    }
}
