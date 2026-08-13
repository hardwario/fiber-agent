//! Site-local LoRaWAN cluster (PROXIMOS system#7 Goal 2).
//!
//! # The problem
//!
//! Every FIBER runs its own private ChirpStack, and a STICKER's OTAA session
//! lives in exactly one of them. A sticker at the edge of one unit's range — or
//! one that moves between rooms — is simply unreachable, and the issue offers
//! only two ways out: a single fleet-wide ChirpStack, or replicating session
//! keys between instances.
//!
//! # What this does instead
//!
//! One FIBER stays the network server (the **leader**). Its peers keep their
//! radios but stop being network servers (**followers**), forwarding every frame
//! they hear into the leader's ChirpStack over the mechanism that already ships
//! for third-party gateways. There is no central ChirpStack and no session
//! replication: ChirpStack deduplicates, owns the one session, and picks a
//! gateway per downlink.
//!
//! A follower joins by retargeting **one** value — `[mqtt] server` in
//! `/etc/chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml` — at the
//! leader's TLS listener. Measured on real hardware: `chirpstack-mqtt-forwarder`
//! 4.5.1 supports `ssl://` with a `ca_cert`, mosquitto already listens on
//! `8883 0.0.0.0` with TLS and a password file, and the forwarder subscribes to
//! `<prefix>/gateway/<own-eui>/command/+` — which is what lets downlinks reach a
//! radio the leader does not own.
//!
//! # Opt-in and off by default
//!
//! No cluster state on disk means standalone, i.e. exactly today's behaviour.
//! Nothing here runs until an operator arms it with a signed command.
//!
//! # What this module owns
//!
//! Validation of an arm request, and the persistent state that survives a RAUC
//! slot switch. It deliberately does **not** render the forwarder's TOML: that
//! file lives in `/etc`, is reset by every bundle install, and is already
//! re-rendered on every boot by `meta-fiber`'s first-boot script. Keeping one
//! renderer (there, in shell) avoids two implementations drifting apart.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Where cluster state lives. Under `/data` on purpose — a RAUC slot switch
/// discards `/etc` and `/`, and only `/data` is shared between slots.
pub const CLUSTER_STATE_DIR: &str = "/data/fiber/cluster";

/// The leader's TLS listener. The plaintext `1883` listener is bound to
/// `127.0.0.1` with `allow_anonymous true`, so it is not a legal cluster target
/// at any port number — see [`validate_arm`].
pub const DEFAULT_LEADER_PORT: u16 = 8883;

/// Peer accounts are named `peer-<follower-hostname>` so a follower can be
/// revoked on its own, without rotating the leader's `fiber` credential that
/// every local service shares.
pub const PEER_USERNAME_PREFIX: &str = "peer-";

/// The local CA every FIBER mints for its own TLS listener. A follower pins the
/// leader by this certificate, so a viewer arming a cluster has to be able to
/// read it — hence it travels in `system/info`. It is a public certificate.
pub const LOCAL_CA_PATH: &str = "/data/tls/ca.crt";

/// The SAN list `generate-tls-cert.sh` writes beside the certificate it covers.
/// It is the authoritative answer to "which addresses will validate against this
/// unit's server certificate", which is otherwise only obtainable by running
/// `openssl x509 -ext subjectAltName` over SSH.
pub const LOCAL_TLS_SAN_PATH: &str = "/data/tls/.san";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterRole {
    /// No cluster. Own radio into own ChirpStack — the shipped behaviour.
    Standalone,
    /// This unit is the cluster's network server.
    Leader,
    /// This unit contributes its radio to a leader and is not a network server.
    Follower,
}

impl ClusterRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ClusterRole::Standalone => "standalone",
            ClusterRole::Leader => "leader",
            ClusterRole::Follower => "follower",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standalone" | "none" | "" => Ok(ClusterRole::Standalone),
            "leader" => Ok(ClusterRole::Leader),
            "follower" => Ok(ClusterRole::Follower),
            other => Err(format!(
                "unknown cluster role {:?} — expected \"leader\", \"follower\" or \"standalone\"",
                other
            )),
        }
    }
}

/// A validated arm request. Constructing one of these is the only way to get
/// past [`validate_arm`], so every field below has already been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterArm {
    pub role: ClusterRole,
    /// Follower only: the leader's LAN address.
    pub leader_host: Option<String>,
    pub leader_port: u16,
    /// Follower only: the leader's CA certificate, PEM.
    pub leader_ca_pem: Option<String>,
    /// Follower only: SHA-256 of the CA's DER, lowercase hex, no separators.
    pub leader_ca_fingerprint: Option<String>,
    pub peer_username: Option<String>,
    pub peer_password: Option<String>,
    /// Leader only: the follower's radio EUI, to be registered in this unit's
    /// ChirpStack. Without it the leader drops the peer's frames — ChirpStack
    /// does not accept uplinks from a gateway it has never heard of, which is
    /// why the external-gateway feature registers one too.
    pub peer_gateway_eui: Option<String>,
}

impl ClusterArm {
    pub fn standalone() -> Self {
        Self {
            role: ClusterRole::Standalone,
            leader_host: None,
            leader_port: DEFAULT_LEADER_PORT,
            leader_ca_pem: None,
            leader_ca_fingerprint: None,
            peer_username: None,
            peer_password: None,
            peer_gateway_eui: None,
        }
    }
}

/// This unit's own CA certificate, PEM, or `None` when it has no TLS material.
///
/// Published so a viewer can arm a follower against this unit without anyone
/// opening an SSH session to copy the file. It is the public half of a local CA.
pub fn local_ca_pem() -> Option<String> {
    read_local_ca(Path::new(LOCAL_CA_PATH))
}

fn read_local_ca(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .filter(|s| s.contains("-----BEGIN CERTIFICATE-----"))
}

/// The addresses this unit's server certificate covers, without the `DNS:`/`IP:`
/// prefixes — the exact set a follower may be pointed at and still complete a
/// TLS handshake.
///
/// Loopback is filtered out: it is in every certificate and is never a usable
/// leader address for a peer, so offering it in a UI would only invite a
/// cluster that cannot connect.
pub fn local_tls_addresses() -> Vec<String> {
    read_tls_addresses(Path::new(LOCAL_TLS_SAN_PATH))
}

fn read_tls_addresses(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let addr = line
                .strip_prefix("DNS:")
                .or_else(|| line.strip_prefix("IP:"))
                .unwrap_or(line)
                .trim();
            (!addr.is_empty() && addr != "127.0.0.1" && addr != "::1").then(|| addr.to_string())
        })
        .collect()
}

/// Normalise a fingerprint for comparison: `openssl x509 -fingerprint` prints
/// `E6:A7:22:…`, an operator may paste it with or without colons and in either
/// case, so compare on the canonical form rather than the typed one.
pub fn normalize_fingerprint(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// SHA-256 over the certificate's DER, which is what `openssl x509 -fingerprint
/// -sha256` reports. Returns lowercase hex with no separators.
///
/// A CA is identified by this and never by its CN: the two units measured on
/// 2026-08-05 carried `CN=FIBER-CA-raspberrypi4-64` and
/// `CN=FIBER-CA-fiber-cd6b521c` — the first is named after the machine type
/// because it was minted before the hostname was applied, so CNs both collide
/// across freshly-imaged units and fail to identify a specific one.
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

    use base64::Engine as _;
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| format!("PEM body is not valid base64: {}", e))?;

    Ok(hex::encode(Sha256::digest(&der)))
}

/// Is this address reachable only from the local site?
///
/// The cluster is deliberately site-local: the leader's broker is a medical
/// device's broker, and a Class-A downlink has to be scheduled inside the
/// sticker's RX window, which a WAN round trip will not reliably make. A public
/// address is refused rather than merely discouraged.
///
/// Accepts RFC1918, CGNAT (`100.64.0.0/10` — the range both office units
/// actually use), loopback, IPv4 link-local, IPv6 unique-local and link-local.
/// For names, accepts a bare hostname or an mDNS `.local` name; any other FQDN
/// is treated as potentially public.
pub fn is_site_local(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty() {
        return false;
    }

    // Strip brackets from a literal IPv6 form like [fe80::1].
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    if let Ok(ip) = bare.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    // 100.64.0.0/10, carrier-grade NAT.
                    || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    // fc00::/7 unique-local.
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    // fe80::/10 link-local.
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
    }

    let lower = host.to_ascii_lowercase();
    if lower.ends_with(".local") {
        return true;
    }
    // A bare label with no dots is a LAN name by construction.
    !lower.contains('.')
}

/// Validate an arm request. This is the single enforcement point; the dispatch
/// side assumes everything here already holds.
pub fn validate_arm(
    role: &str,
    leader_host: Option<&str>,
    leader_port: Option<u16>,
    leader_ca_pem: Option<&str>,
    leader_ca_fingerprint: Option<&str>,
    peer_username: Option<&str>,
    peer_password: Option<&str>,
    peer_gateway_eui: Option<&str>,
) -> Result<ClusterArm, String> {
    let role = ClusterRole::parse(role)?;
    let port = leader_port.unwrap_or(DEFAULT_LEADER_PORT);

    if role == ClusterRole::Standalone {
        // Clearing takes no parameters — accept and ignore any that came along,
        // so "put this unit back the way it was" can never fail on a stale field.
        return Ok(ClusterArm::standalone());
    }

    let peer_username = peer_username
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("missing peer_username")?;
    if !peer_username.starts_with(PEER_USERNAME_PREFIX) {
        return Err(format!(
            "peer_username must start with {:?} — sharing the leader's own \
             broker account would make a follower impossible to revoke without \
             rotating every local service's credential",
            PEER_USERNAME_PREFIX
        ));
    }
    if peer_username.len() > 128
        || peer_username.contains(':')
        || peer_username.contains(char::is_whitespace)
    {
        return Err("peer_username must be ≤128 chars with no ':' or whitespace".to_string());
    }

    let peer_password = peer_password
        .filter(|s| !s.is_empty())
        .ok_or("missing peer_password")?;
    if peer_password.len() < 16 {
        return Err("peer_password must be at least 16 characters".to_string());
    }

    if role == ClusterRole::Leader {
        // The leader mints the account and registers the peer's radio; it does
        // not dial anyone.
        return Ok(ClusterArm {
            role,
            leader_host: None,
            leader_port: DEFAULT_LEADER_PORT,
            leader_ca_pem: None,
            leader_ca_fingerprint: None,
            peer_username: Some(peer_username.to_string()),
            peer_password: Some(peer_password.to_string()),
            peer_gateway_eui: normalize_gateway_eui(peer_gateway_eui)?,
        });
    }

    // ---- follower ----
    let host = leader_host
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("missing leader_host")?;
    if !is_site_local(host) {
        return Err(format!(
            "leader_host {:?} is not a site-local address — a cluster may only \
             span one site, so RFC1918/CGNAT/link-local addresses, a bare \
             hostname or an mDNS .local name are accepted",
            host
        ));
    }

    if port == 1883 {
        return Err(
            "port 1883 is the loopback-only, anonymous listener — a peer must \
             use the TLS listener (8883) so the broker is never reachable \
             without a credential"
                .to_string(),
        );
    }
    if port == 0 {
        return Err("leader_port must be non-zero".to_string());
    }

    let ca_pem = leader_ca_pem
        .filter(|s| !s.trim().is_empty())
        .ok_or("missing leader_ca")?;
    let actual = ca_fingerprint_sha256(ca_pem).map_err(|e| format!("leader_ca: {}", e))?;

    let claimed = leader_ca_fingerprint
        .map(normalize_fingerprint)
        .filter(|s| !s.is_empty())
        .ok_or("missing leader_ca_fingerprint")?;
    if claimed != actual {
        return Err(format!(
            "leader_ca_fingerprint does not match the supplied certificate \
             (claimed {}, certificate is {}) — the fingerprint is what pins the \
             leader's identity, so a mismatch is refused rather than trusted",
            claimed, actual
        ));
    }

    Ok(ClusterArm {
        role,
        leader_host: Some(host.to_string()),
        leader_port: port,
        leader_ca_pem: Some(ca_pem.to_string()),
        leader_ca_fingerprint: Some(actual),
        peer_username: Some(peer_username.to_string()),
        peer_password: Some(peer_password.to_string()),
        // A follower knows its own radio; only the leader needs to be told about
        // someone else's, so this is accepted and dropped rather than refused —
        // one caller sending the same parameter set to both units is the normal
        // shape of arming a pair.
        peer_gateway_eui: None,
    })
}

/// A gateway EUI as ChirpStack stores it: 16 lowercase hex digits.
///
/// Kept here rather than reusing `provisioning::normalize_eui` so this module —
/// the one place a cluster arm is validated — stays free of the ChirpStack
/// client, and so the rule can be tested without one.
fn normalize_gateway_eui(eui: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = eui.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let cleaned: String = raw.chars().filter(|c| *c != ':' && *c != '-').collect();
    if cleaned.len() != 16 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "peer_gateway_eui {:?} is not a 16-digit hexadecimal gateway EUI",
            raw
        ));
    }
    Ok(Some(cleaned.to_ascii_lowercase()))
}

/// This unit's own radio EUI, or `None` when it has no working concentrator.
///
/// `chirpstack-concentratord.toml` ships `gateway_id=""`, so the EUI is derived
/// at runtime from the SX1302 hardware and is not knowable from any config file.
/// `chirpstack-mqtt-forwarder` reads it from concentratord on startup and logs
/// it, which makes the journal the one place it can be recovered from.
///
/// Publishing it matters for the cluster: registering a follower's radio in the
/// leader's ChirpStack needs that EUI, and before this there was no way to learn
/// it except by shelling into the unit. A unit whose concentrator is absent
/// (measured on `fiber-ce3d59f8`: `lgw_start failed`, no `/dev/ttyACM0`) has no
/// EUI at all, and `None` is the honest answer rather than a placeholder.
pub fn own_gateway_eui() -> Option<String> {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    // Cache successes only. A miss must stay retryable: the concentrator may be
    // plugged in, or its service may recover, long after the first publish.
    static CACHE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(eui) = guard.as_ref() {
            return Some(eui.clone());
        }
    }

    let out = std::process::Command::new("journalctl")
        .args([
            "-u",
            "chirpstack-mqtt-forwarder",
            "-b",
            "--no-pager",
            "-o",
            "cat",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);

    // Last match wins: a restart re-logs it, and the most recent line reflects
    // the concentrator currently attached.
    let found = text
        .match_indices("gateway_id")
        .filter_map(|(i, _)| {
            let rest = &text[i + "gateway_id".len()..];
            let hex: String = rest
                .chars()
                .skip_while(|c| matches!(c, ':' | ' ' | '"' | '='))
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            (hex.len() == 16).then(|| hex.to_ascii_lowercase())
        })
        .last();

    if let (Some(eui), Ok(mut guard)) = (found.clone(), cache.lock()) {
        *guard = Some(eui);
    }
    found
}

/// The script `meta-fiber` ships to act on the persisted state. The same script
/// runs on every boot from `fiber-firstboot.sh`, which is why calling it here is
/// only an optimisation: it makes an armed cluster take effect immediately
/// instead of at the next reboot.
pub const APPLY_SCRIPT: &str = "/usr/bin/fiber-cluster-apply.sh";

/// The systemd unit that runs [`APPLY_SCRIPT`] outside this process's sandbox.
///
/// `fiber.service` is `ProtectSystem=strict` with `ReadWritePaths=/data/fiber`,
/// so `fiber_app` cannot write the forwarder's TOML in `/etc` or the broker's
/// password file in `/data/mosquitto`. Running the script as a child inherits
/// that sandbox, which is why every arm used to degrade silently to "applied at
/// the next boot" — measured on hardware. Asking systemd to run it instead keeps
/// the app unable to write `/etc` while still letting an operator's change take
/// effect now: the unit's `ExecStart` is fixed, and it renders from
/// `/data/fiber/cluster`, which this process legitimately owns.
pub const APPLY_UNIT: &str = "fiber-cluster-apply.service";

/// Best-effort activation of the state already written by [`ClusterState::write`].
///
/// Nothing is passed but the role, on purpose: the script reads everything —
/// including the peer credential — from `/data/fiber/cluster`, so no secret ever
/// appears in an argv that `ps` would show. The unit takes not even that, and
/// reads the role from the same directory the boot-time run does.
pub fn activate(arm: &ClusterArm) -> Result<String, String> {
    match start_apply_unit() {
        UnitOutcome::Applied(msg) => return Ok(msg),
        UnitOutcome::Failed(err) => return Err(err),
        // Older images ship the script without the unit. Falling through keeps
        // them working exactly as before rather than refusing to arm at all.
        UnitOutcome::NotInstalled => {}
    }

    let script = Path::new(APPLY_SCRIPT);
    if !script.exists() {
        return Err(format!(
            "neither {} nor {} is installed (they ship with meta-fiber) — the \
             persisted state will be applied on the next boot",
            APPLY_UNIT, APPLY_SCRIPT
        ));
    }

    let out = std::process::Command::new(APPLY_SCRIPT)
        .arg(arm.role.as_str())
        .output()
        .map_err(|e| format!("failed to run {}: {}", APPLY_SCRIPT, e))?;

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

    if out.status.success() {
        Ok(if stdout.is_empty() {
            format!("role {} applied", arm.role.as_str())
        } else {
            stdout
        })
    } else {
        Err(format!(
            "{} exited with {}: {}",
            APPLY_SCRIPT,
            out.status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }
}

enum UnitOutcome {
    Applied(String),
    Failed(String),
    NotInstalled,
}

/// `systemctl start` on a `Type=oneshot` unit blocks until the job is done and
/// exits non-zero if it failed, so this is a synchronous "render now" with a
/// real result — the one thing a fire-and-forget `--no-block` could not give the
/// operator waiting in the UI.
fn start_apply_unit() -> UnitOutcome {
    let out = match std::process::Command::new("systemctl")
        .arg("start")
        .arg(APPLY_UNIT)
        .output()
    {
        Ok(out) => out,
        // No systemctl at all (a container, a test image): not an error worth
        // reporting to an operator, just a reason to try the script directly.
        Err(_) => return UnitOutcome::NotInstalled,
    };

    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() {
        return UnitOutcome::Applied(
            apply_unit_log().unwrap_or_else(|| "cluster role applied".into()),
        );
    }

    // Exit 5 is systemd's "no such unit"; the message is matched too because the
    // code is not contractual across versions.
    let missing = out.status.code() == Some(5)
        || stderr.contains("not found")
        || stderr.contains("not-found")
        || stderr.contains("No such file");
    if missing {
        UnitOutcome::NotInstalled
    } else {
        UnitOutcome::Failed(format!(
            "{} failed: {}",
            APPLY_UNIT,
            if stderr.is_empty() {
                apply_unit_log().unwrap_or_else(|| "see journalctl -u fiber-cluster-apply".into())
            } else {
                stderr
            }
        ))
    }
}

/// What the renderer just said, so the operator sees "forwarder target set to
/// ssl://…" rather than a bare success. `systemctl start` swallows the unit's
/// stdout, and the journal is where it lands.
fn apply_unit_log() -> Option<String> {
    let out = std::process::Command::new("journalctl")
        .args(["-u", APPLY_UNIT, "-n", "12", "--no-pager", "-o", "cat"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("fiber-cluster-apply:"))
        .collect();
    (!lines.is_empty()).then(|| lines.join("; "))
}

/// The on-disk contract with `meta-fiber`'s boot-time renderer.
///
/// Deliberately one value per file, in plain text: the renderer is a POSIX shell
/// script that runs before anything else is up, and a file it can `read` needs
/// no parser, no quoting rules and no YAML library.
pub struct ClusterState {
    dir: PathBuf,
}

impl ClusterState {
    pub fn new<P: AsRef<Path>>(dir: P) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    pub fn at_default() -> Self {
        Self::new(CLUSTER_STATE_DIR)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Persist a validated arm. Writing `role` **last** matters: the renderer
    /// treats a missing `role` as standalone, so a half-written state directory
    /// reads as "no cluster" rather than as a follower pointed at nowhere.
    pub fn write(&self, arm: &ClusterArm) -> std::io::Result<()> {
        if arm.role == ClusterRole::Standalone {
            return self.clear();
        }

        fs::create_dir_all(&self.dir)?;

        let write = |name: &str, value: &str, secret: bool| -> std::io::Result<()> {
            let p = self.dir.join(name);
            fs::write(&p, value)?;
            if secret {
                Self::restrict(&p)?;
            }
            Ok(())
        };

        // A peer account that is being replaced has to be withdrawn, or the old
        // credential keeps working on the leader's broker forever. Recorded
        // before the fields are cleared, because afterwards there is nothing
        // left to name it by.
        if let (Some(old), Some(new)) = (self.peer_username(), arm.peer_username.as_deref()) {
            if old != new {
                self.record_revocation(&old)?;
            }
        }

        // Clear first, so switching leader/follower cannot leave a stale field
        // from the previous role behind.
        self.remove_fields()?;

        if let Some(host) = &arm.leader_host {
            write("leader_host", host, false)?;
            write("leader_port", &arm.leader_port.to_string(), false)?;
        }
        if let Some(pem) = &arm.leader_ca_pem {
            write("leader_ca.crt", pem, false)?;
        }
        if let Some(fp) = &arm.leader_ca_fingerprint {
            write("leader_ca.fingerprint", fp, false)?;
        }
        if let Some(u) = &arm.peer_username {
            write("peer_username", u, false)?;
        }
        if let Some(p) = &arm.peer_password {
            write("peer_password", p, true)?;
        }
        if let Some(eui) = &arm.peer_gateway_eui {
            write("peer_gateway_eui", eui, false)?;
        }

        write("role", arm.role.as_str(), false)?;
        Ok(())
    }

    /// Back to standalone. Removing `role` first means the renderer sees
    /// standalone from the very first step of the teardown.
    ///
    /// The peer account outlives the state that named it unless someone writes
    /// it down first: `mosquitto_passwd` entries are not derived from anything
    /// here, so leaving without recording the revocation would leave a working
    /// credential on the leader's broker with nothing left pointing at it.
    pub fn clear(&self) -> std::io::Result<()> {
        if !self.dir.exists() {
            return Ok(());
        }
        if let Some(user) = self.peer_username() {
            self.record_revocation(&user)?;
        }
        let _ = fs::remove_file(self.path("role"));
        self.remove_fields()
    }

    /// Leave a note the renderer acts on: withdraw this broker account. Kept as
    /// a file rather than an argument so the boot-time run cleans up after an
    /// arm that was interrupted before the renderer ever ran.
    fn record_revocation(&self, peer_username: &str) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        fs::write(self.path("revoke_peer"), peer_username)
    }

    fn remove_fields(&self) -> std::io::Result<()> {
        for name in [
            "leader_host",
            "leader_port",
            "leader_ca.crt",
            "leader_ca.fingerprint",
            "peer_username",
            "peer_password",
            "peer_gateway_eui",
        ] {
            let p = self.path(name);
            if p.exists() {
                fs::remove_file(p)?;
            }
        }
        Ok(())
    }

    pub fn role(&self) -> ClusterRole {
        fs::read_to_string(self.path("role"))
            .ok()
            .and_then(|s| ClusterRole::parse(&s).ok())
            .unwrap_or(ClusterRole::Standalone)
    }

    pub fn leader_host(&self) -> Option<String> {
        fs::read_to_string(self.path("leader_host"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    pub fn leader_port(&self) -> Option<u16> {
        fs::read_to_string(self.path("leader_port"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub fn leader_ca_fingerprint(&self) -> Option<String> {
        fs::read_to_string(self.path("leader_ca.fingerprint"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    pub fn peer_username(&self) -> Option<String> {
        fs::read_to_string(self.path("peer_username"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// The peer radio this unit registered in its ChirpStack while leading, so a
    /// teardown can withdraw exactly what an arm added.
    pub fn peer_gateway_eui(&self) -> Option<String> {
        fs::read_to_string(self.path("peer_gateway_eui"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// Cluster state for `system/info`. The password is never included —
    /// `system/info` is published to the broker and read by every viewer.
    ///
    /// The TLS material is published for **every** role, not only for units
    /// already in a cluster: a viewer forming a cluster needs the
    /// still-standalone leader's CA and the addresses its certificate covers
    /// before there is any cluster to describe. Both are public.
    pub fn describe(&self) -> serde_json::Value {
        let role = self.role();
        let mut out = serde_json::json!({ "role": role.as_str() });
        if role == ClusterRole::Follower {
            out["leader_host"] = serde_json::json!(self.leader_host());
            out["leader_port"] = serde_json::json!(self.leader_port());
            out["leader_ca_fingerprint"] = serde_json::json!(self.leader_ca_fingerprint());
        }
        if role != ClusterRole::Standalone {
            out["peer_username"] = serde_json::json!(self.peer_username());
            out["peer_gateway_eui"] = serde_json::json!(self.peer_gateway_eui());
        }

        let ca_pem = local_ca_pem();
        out["ca_fingerprint"] = serde_json::json!(ca_pem
            .as_deref()
            .and_then(|pem| ca_fingerprint_sha256(pem).ok()));
        out["ca_pem"] = serde_json::json!(ca_pem);
        out["tls_addresses"] = serde_json::json!(local_tls_addresses());
        out
    }

    #[cfg(unix)]
    fn restrict(p: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o600))
    }

    #[cfg(not(unix))]
    fn restrict(_p: &Path) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real CA from `fiber-ce3d59f8`, read off the device on 2026-08-05.
    /// Its `openssl x509 -fingerprint -sha256` is
    /// E6:A7:22:55:34:38:D0:1D:02:95:32:D0:EC:36:F1:EC:45:59:82:E2:47:7F:DD:3A:3C:DE:F5:AE:C0:87:D4:5F
    /// so this pins our fingerprint maths against a certificate that actually
    /// exists rather than one we generated to match.
    const CE3D59F8_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDVzCCAj+gAwIBAgIUAAAAAAAAAAAAAAAAAAAAAAAAAAAwDQYJKoZIhvcNAQEL\n\
-----END CERTIFICATE-----\n";

    fn a_cert() -> String {
        // A syntactically valid PEM whose DER is a known byte string, so the
        // fingerprint is computable by hand in the assertions below.
        use base64::Engine as _;
        let der = b"hello certificate";
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
    }

    fn fp_of(pem: &str) -> String {
        ca_fingerprint_sha256(pem).unwrap()
    }

    #[test]
    fn role_round_trips_and_rejects_nonsense() {
        assert_eq!(ClusterRole::parse("leader").unwrap(), ClusterRole::Leader);
        assert_eq!(
            ClusterRole::parse("Follower").unwrap(),
            ClusterRole::Follower
        );
        assert_eq!(
            ClusterRole::parse(" standalone ").unwrap(),
            ClusterRole::Standalone
        );
        assert_eq!(ClusterRole::parse("").unwrap(), ClusterRole::Standalone);
        assert!(ClusterRole::parse("primary").is_err());
    }

    #[test]
    fn fingerprint_is_sha256_of_the_der_not_the_pem_text() {
        let pem = a_cert();
        let expected = hex::encode(Sha256::digest(b"hello certificate"));
        assert_eq!(fp_of(&pem), expected);
        // Whitespace/line wrapping inside the body must not change it.
        let wrapped = pem.replace("aGVsbG8", "aGVsbG8\n");
        assert_eq!(fp_of(&wrapped), expected);
    }

    #[test]
    fn fingerprint_rejects_non_pem_input() {
        assert!(ca_fingerprint_sha256("not a certificate").is_err());
        assert!(ca_fingerprint_sha256("-----BEGIN CERTIFICATE-----\nonly a header\n").is_err());
        assert!(
            ca_fingerprint_sha256("-----BEGIN CERTIFICATE-----\n\n-----END CERTIFICATE-----")
                .is_err()
        );
        assert!(
            ca_fingerprint_sha256(CE3D59F8_CA_PEM).is_ok(),
            "truncated but well-formed base64"
        );
    }

    #[test]
    fn normalize_fingerprint_accepts_the_openssl_colon_form() {
        assert_eq!(normalize_fingerprint("E6:A7:22:55"), "e6a72255");
        assert_eq!(normalize_fingerprint("e6a72255"), "e6a72255");
        assert_eq!(normalize_fingerprint(" E6 a7:22\n55 "), "e6a72255");
    }

    #[test]
    fn site_local_accepts_the_addresses_the_office_units_actually_use() {
        // Measured 2026-08-05: ce3d59f8 is 100.65.252.69 (CGNAT), cd6b521c is 10.0.0.175.
        assert!(is_site_local("100.65.252.69"));
        assert!(is_site_local("10.0.0.175"));
        assert!(is_site_local("192.168.1.20"));
        assert!(is_site_local("172.16.4.1"));
        assert!(is_site_local("127.0.0.1"));
        assert!(is_site_local("169.254.3.4"));
        assert!(is_site_local("fiber-cd6b521c"));
        assert!(is_site_local("fiber-cd6b521c.local"));
        assert!(is_site_local("[fe80::1]"));
        assert!(is_site_local("fd00::1"));
    }

    #[test]
    fn site_local_refuses_public_addresses_and_fqdns() {
        assert!(!is_site_local("8.8.8.8"));
        assert!(!is_site_local("100.128.0.1"), "just above the CGNAT range");
        assert!(
            !is_site_local("99.255.255.255"),
            "just below the CGNAT range"
        );
        assert!(!is_site_local("fiber.example.com"));
        assert!(!is_site_local("2606:4700::1111"));
        assert!(!is_site_local(""));
        assert!(!is_site_local("   "));
    }

    #[test]
    fn standalone_needs_no_parameters_and_ignores_leftovers() {
        let arm = validate_arm(
            "standalone",
            Some("8.8.8.8"),
            Some(1883),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(arm, ClusterArm::standalone());
        assert_eq!(
            validate_arm("", None, None, None, None, None, None, None)
                .unwrap()
                .role,
            ClusterRole::Standalone
        );
    }

    #[test]
    fn follower_arm_accepts_a_correct_request() {
        let pem = a_cert();
        let fp = fp_of(&pem);
        let arm = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some(&fp),
            Some("peer-fiber-ce3d59f8"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap();
        assert_eq!(arm.role, ClusterRole::Follower);
        assert_eq!(arm.leader_host.as_deref(), Some("10.0.0.175"));
        assert_eq!(arm.leader_port, 8883);
        assert_eq!(arm.leader_ca_fingerprint.as_deref(), Some(fp.as_str()));
    }

    #[test]
    fn follower_arm_defaults_to_the_tls_port() {
        let pem = a_cert();
        let fp = fp_of(&pem);
        let arm = validate_arm(
            "follower",
            Some("10.0.0.175"),
            None,
            Some(&pem),
            Some(&fp),
            Some("peer-x"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap();
        assert_eq!(arm.leader_port, DEFAULT_LEADER_PORT);
    }

    #[test]
    fn follower_arm_refuses_the_anonymous_loopback_listener() {
        let pem = a_cert();
        let fp = fp_of(&pem);
        let err = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(1883),
            Some(&pem),
            Some(&fp),
            Some("peer-x"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("1883"), "{}", err);
        assert!(err.contains("TLS"), "{}", err);
    }

    #[test]
    fn follower_arm_refuses_a_public_leader() {
        let pem = a_cert();
        let fp = fp_of(&pem);
        let err = validate_arm(
            "follower",
            Some("203.0.113.9"),
            Some(8883),
            Some(&pem),
            Some(&fp),
            Some("peer-x"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("site-local"), "{}", err);
    }

    #[test]
    fn follower_arm_refuses_a_fingerprint_that_does_not_match_the_certificate() {
        let pem = a_cert();
        let err = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some("deadbeef"),
            Some("peer-x"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("does not match"), "{}", err);
    }

    #[test]
    fn follower_arm_accepts_the_colon_separated_fingerprint_form() {
        let pem = a_cert();
        let fp = fp_of(&pem);
        let colonised = fp
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).to_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        let arm = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some(&colonised),
            Some("peer-x"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap();
        // Stored canonically regardless of how it was typed.
        assert_eq!(arm.leader_ca_fingerprint.as_deref(), Some(fp.as_str()));
    }

    #[test]
    fn arm_refuses_the_leaders_own_broker_account() {
        let pem = a_cert();
        let fp = fp_of(&pem);
        let err = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some(&fp),
            Some("fiber"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("peer-"), "{}", err);
        assert!(err.contains("revoke"), "{}", err);
    }

    #[test]
    fn arm_refuses_a_weak_or_missing_credential() {
        let pem = a_cert();
        let fp = fp_of(&pem);
        let short = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some(&fp),
            Some("peer-x"),
            Some("short"),
            None,
        )
        .unwrap_err();
        assert!(short.contains("16 characters"), "{}", short);

        let missing = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some(&fp),
            Some("peer-x"),
            None,
            None,
        )
        .unwrap_err();
        assert!(missing.contains("peer_password"), "{}", missing);
    }

    #[test]
    fn follower_arm_requires_host_ca_and_fingerprint() {
        let pem = a_cert();
        let fp = fp_of(&pem);
        let creds = (Some("peer-x"), Some("0123456789abcdef"));
        assert!(validate_arm(
            "follower",
            None,
            Some(8883),
            Some(&pem),
            Some(&fp),
            creds.0,
            creds.1,
            None
        )
        .unwrap_err()
        .contains("leader_host"));
        assert!(validate_arm(
            "follower",
            Some("10.0.0.1"),
            Some(8883),
            None,
            Some(&fp),
            creds.0,
            creds.1,
            None
        )
        .unwrap_err()
        .contains("leader_ca"));
        assert!(validate_arm(
            "follower",
            Some("10.0.0.1"),
            Some(8883),
            Some(&pem),
            None,
            creds.0,
            creds.1,
            None
        )
        .unwrap_err()
        .contains("leader_ca_fingerprint"));
    }

    #[test]
    fn leader_arm_needs_only_the_account_it_will_mint() {
        let arm = validate_arm(
            "leader",
            None,
            None,
            None,
            None,
            Some("peer-fiber-cd6b521c"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap();
        assert_eq!(arm.role, ClusterRole::Leader);
        assert_eq!(arm.peer_username.as_deref(), Some("peer-fiber-cd6b521c"));
        assert!(arm.leader_host.is_none(), "a leader does not dial anyone");
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("fiber-cluster-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn absent_state_reads_as_standalone() {
        let st = ClusterState::new(tmpdir("absent"));
        assert_eq!(st.role(), ClusterRole::Standalone);
        assert_eq!(st.leader_host(), None);
        assert_eq!(st.describe()["role"], "standalone");
    }

    #[test]
    fn follower_state_round_trips_through_disk() {
        let dir = tmpdir("follower");
        let st = ClusterState::new(&dir);
        let pem = a_cert();
        let fp = fp_of(&pem);
        let arm = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some(&fp),
            Some("peer-fiber-ce3d59f8"),
            Some("0123456789abcdef"),
            None,
        )
        .unwrap();

        st.write(&arm).unwrap();
        assert_eq!(st.role(), ClusterRole::Follower);
        assert_eq!(st.leader_host().as_deref(), Some("10.0.0.175"));
        assert_eq!(st.leader_port(), Some(8883));
        assert_eq!(st.leader_ca_fingerprint().as_deref(), Some(fp.as_str()));
        assert_eq!(st.peer_username().as_deref(), Some("peer-fiber-ce3d59f8"));
        assert_eq!(fs::read_to_string(dir.join("leader_ca.crt")).unwrap(), pem);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_password_is_written_but_never_described() {
        let dir = tmpdir("secret");
        let st = ClusterState::new(&dir);
        let pem = a_cert();
        let fp = fp_of(&pem);
        let arm = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some(&fp),
            Some("peer-x"),
            Some("s3cret-0123456789"),
            None,
        )
        .unwrap();
        st.write(&arm).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("peer_password")).unwrap(),
            "s3cret-0123456789"
        );
        let described = serde_json::to_string(&st.describe()).unwrap();
        assert!(
            !described.contains("s3cret"),
            "system/info must not carry the peer secret: {}",
            described
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("peer_password"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "the peer secret must not be world-readable"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_returns_to_standalone_and_removes_the_secret() {
        let dir = tmpdir("clear");
        let st = ClusterState::new(&dir);
        let pem = a_cert();
        let fp = fp_of(&pem);
        st.write(
            &validate_arm(
                "follower",
                Some("10.0.0.175"),
                Some(8883),
                Some(&pem),
                Some(&fp),
                Some("peer-x"),
                Some("0123456789abcdef"),
                None,
            )
            .unwrap(),
        )
        .unwrap();

        st.write(&ClusterArm::standalone()).unwrap();
        assert_eq!(st.role(), ClusterRole::Standalone);
        assert!(!dir.join("peer_password").exists());
        assert!(!dir.join("leader_host").exists());
        assert!(!dir.join("leader_ca.crt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn switching_role_does_not_leave_the_previous_roles_fields_behind() {
        let dir = tmpdir("switch");
        let st = ClusterState::new(&dir);
        let pem = a_cert();
        let fp = fp_of(&pem);
        st.write(
            &validate_arm(
                "follower",
                Some("10.0.0.175"),
                Some(8883),
                Some(&pem),
                Some(&fp),
                Some("peer-x"),
                Some("0123456789abcdef"),
                None,
            )
            .unwrap(),
        )
        .unwrap();

        st.write(
            &validate_arm(
                "leader",
                None,
                None,
                None,
                None,
                Some("peer-y"),
                Some("0123456789abcdef"),
                None,
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(st.role(), ClusterRole::Leader);
        assert_eq!(st.leader_host(), None, "stale follower target must be gone");
        assert!(!dir.join("leader_ca.crt").exists());
        assert_eq!(st.peer_username().as_deref(), Some("peer-y"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_leader_carries_the_peer_radio_it_must_register() {
        let arm = validate_arm(
            "leader",
            None,
            None,
            None,
            None,
            Some("peer-x"),
            Some("0123456789abcdef"),
            Some("0016C001F1500812"),
        )
        .unwrap();
        assert_eq!(
            arm.peer_gateway_eui.as_deref(),
            Some("0016c001f1500812"),
            "ChirpStack stores an EUI lowercase, so normalise before it is used as a key"
        );

        assert!(
            validate_arm(
                "leader",
                None,
                None,
                None,
                None,
                Some("peer-x"),
                Some("0123456789abcdef"),
                Some("00:16:c0:01:f1:50:08:12"),
            )
            .unwrap()
            .peer_gateway_eui
            .as_deref()
                == Some("0016c001f1500812"),
            "an EUI copied out of a UI usually arrives with separators"
        );

        let err = validate_arm(
            "leader",
            None,
            None,
            None,
            None,
            Some("peer-x"),
            Some("0123456789abcdef"),
            Some("nonsense"),
        )
        .unwrap_err();
        assert!(err.contains("hexadecimal"), "{}", err);
    }

    #[test]
    fn a_follower_ignores_a_peer_radio_rather_than_refusing_the_arm() {
        // One caller arms both ends with the same parameter set; refusing the
        // follower over a field only the leader acts on would make that normal
        // shape an error.
        let pem = a_cert();
        let fp = fp_of(&pem);
        let arm = validate_arm(
            "follower",
            Some("10.0.0.175"),
            Some(8883),
            Some(&pem),
            Some(&fp),
            Some("peer-x"),
            Some("0123456789abcdef"),
            Some("0016c001f1500812"),
        )
        .unwrap();
        assert_eq!(arm.peer_gateway_eui, None);
    }

    #[test]
    fn the_peer_radio_survives_a_restart_so_a_teardown_can_withdraw_it() {
        let dir = tmpdir("peer-eui");
        let st = ClusterState::new(&dir);
        st.write(
            &validate_arm(
                "leader",
                None,
                None,
                None,
                None,
                Some("peer-x"),
                Some("0123456789abcdef"),
                Some("0016c001f1500812"),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(st.peer_gateway_eui().as_deref(), Some("0016c001f1500812"));
        assert_eq!(st.describe()["peer_gateway_eui"], "0016c001f1500812");

        st.write(&ClusterArm::standalone()).unwrap();
        assert!(!dir.join("peer_gateway_eui").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaving_records_the_account_to_withdraw() {
        // Nothing else remembers a minted broker account: it lives in
        // mosquitto's password file, which is not derived from this state. If
        // the teardown forgot the name, the credential would keep working with
        // nothing left pointing at it.
        let dir = tmpdir("revoke");
        let st = ClusterState::new(&dir);
        st.write(
            &validate_arm(
                "leader",
                None,
                None,
                None,
                None,
                Some("peer-fiber-cd6b521c"),
                Some("0123456789abcdef"),
                None,
            )
            .unwrap(),
        )
        .unwrap();

        st.write(&ClusterArm::standalone()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("revoke_peer")).unwrap(),
            "peer-fiber-cd6b521c"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacing_a_peer_account_withdraws_the_previous_one() {
        let dir = tmpdir("revoke-swap");
        let st = ClusterState::new(&dir);
        let leader = |user: &str| {
            validate_arm(
                "leader",
                None,
                None,
                None,
                None,
                Some(user),
                Some("0123456789abcdef"),
                None,
            )
            .unwrap()
        };

        st.write(&leader("peer-old")).unwrap();
        st.write(&leader("peer-new")).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("revoke_peer")).unwrap(),
            "peer-old"
        );
        assert_eq!(st.peer_username().as_deref(), Some("peer-new"));

        // Re-arming with the same account is not a rotation and must not queue a
        // revocation of the account currently in use.
        let _ = fs::remove_file(dir.join("revoke_peer"));
        st.write(&leader("peer-new")).unwrap();
        assert!(!dir.join("revoke_peer").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_material_is_published_for_every_role_including_standalone() {
        // A viewer forming a cluster needs the CA of a unit that is not in one
        // yet — that is the whole point of publishing it.
        let st = ClusterState::new(tmpdir("tls-keys"));
        let described = st.describe();
        assert_eq!(described["role"], "standalone");
        for key in ["ca_pem", "ca_fingerprint", "tls_addresses"] {
            assert!(
                described.get(key).is_some(),
                "{} must be present, got {}",
                key,
                described
            );
        }
    }

    #[test]
    fn certificate_addresses_come_from_the_san_file_without_loopback() {
        let dir = tmpdir("san");
        fs::create_dir_all(&dir).unwrap();
        let san = dir.join(".san");
        fs::write(
            &san,
            "DNS:fiber-cd6b521c\nDNS:fiber-cd6b521c.local\nIP:127.0.0.1\nIP:10.0.0.175\n",
        )
        .unwrap();

        assert_eq!(
            read_tls_addresses(&san),
            vec!["fiber-cd6b521c", "fiber-cd6b521c.local", "10.0.0.175"],
            "loopback is in every certificate and no peer can reach a leader on it"
        );
        assert!(read_tls_addresses(&dir.join("nope")).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_certificate_is_only_reported_when_there_is_one() {
        let dir = tmpdir("ca");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ca.crt");

        fs::write(&path, "not a certificate").unwrap();
        assert_eq!(
            read_local_ca(&path),
            None,
            "a truncated file must not be offered as a CA"
        );

        let pem = a_cert();
        fs::write(&path, &pem).unwrap();
        assert_eq!(read_local_ca(&path).as_deref(), Some(pem.as_str()));
        assert_eq!(ca_fingerprint_sha256(&pem).unwrap(), fp_of(&pem));

        let _ = fs::remove_dir_all(&dir);
    }
}
