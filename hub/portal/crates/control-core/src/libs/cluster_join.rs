//! The cluster join — a Hub becomes a **Client** of a Server Hub (`plan/control/cluster.md` §8,
//! decision #38; milestone C4).
//!
//! **One apply path, two surfaces**, as FB1B and `radio-set-mode` share
//! ([`crate::libs::radio_mode`]): [`run`] is what BLE FB1C (`ble_host::cluster_join`) and the
//! control socket behind `proximosctl control.radio create --role client` (`control::server`)
//! both call. Only the audit row's `origin` differs.
//!
//! The input is the join bundle the Server's portal shows once, as a QR code:
//! `{"v":1,"h":"<server>.local","p":"<22-char password>","f":"<43-char fingerprint>"}`. There is
//! no username: this Hub derives `peer-<own hostname>`, so a bundle minted for another Hub fails
//! at the broker.
//!
//! The steps, in order (§8.3):
//!
//! 1. **Refuse** if `setup.provisioned` is true or a role marker exists, so a set-up Hub is
//!    never turned into a Client without a factory reset. The radio mode is not checked: a
//!    Client runs the Server's, which the Server's portal sets after the first report
//!    (decision #47).
//! 2. **Validate** the bundle: `v` 1; `h` a hostname — an IPv4 or IPv6 literal is refused,
//!    because the Server's certificate carries DNS SANs only and its 8883 listener is IPv4
//!    only (§5, §6, §10 Q2); `p` 22 characters of base64url; `f` 43 characters of base64url.
//! 3. **Resolve `h` to an IPv4 address** — `.local` over avahi, anything else as a DNS A query,
//!    never through the hosts file (`mqtt::server_hosts`) — and write the hosts line before
//!    anything dials it. Both connections below then dial that address, and verify `h`.
//! 4. **Fetch the Server's CA** from `https://<h>/connection/ca` without verifying the server
//!    certificate — the CA is what is being fetched, and port 80 is closed — and refuse unless
//!    SHA-256 of the body **exactly as received**, base64url, equals `f` (§6).
//! 5. **Test-connect** to `<h>:8883` over TLS against that CA, verifying the name, as
//!    `peer-<own hostname>` / `p`. A refusal carries the broker's reason.
//! 6. **Write** the connection files (`cluster_state::ClusterDir`), then the role marker, then
//!    `setup: {provisioned: true, role: client}` with the `CLUSTER_JOIN` audit row, **last**
//!    (decided 2026-10-02).
//! 7. **Answer** `joined <h>; restarting`, and reboot after [`REBOOT_DELAY`] — a reboot, not a
//!    restart list, because the role flips half the service set and the renderer only runs at
//!    boot (§8, the CLI twin's amendment).
//!
//! A failure before step 6 leaves nothing written but the hosts line, and that is removed
//! again. A failure inside step 6 removes what step 6 wrote — the marker first, then the
//! connection files — and the hosts line. A crash inside step 6 leaves one of two Hubs, and
//! neither needs a factory reset:
//!
//! - connection files without a marker: the renderer reads a Server, and a retried join
//!   overwrites the files;
//! - a marker without `setup`: the renderer reads a Client and the Hub boots as one, a retried
//!   join is refused (the marker exists), and [`complete_interrupted_join`], which the agent
//!   runs at start, writes the missing `setup` with its audit row.
//!
//! The other order (setup first) could leave a Hub provisioned as `client` with no marker: it
//! renders a Server that FB17 cannot set up as one, which only a factory reset recovers.
//!
//! **The password** is held in a [`Secret`], which has no `Display` and a redacting `Debug`. It
//! goes to exactly two places: the broker's CONNECT packet and the 0600 `peer_password` file.
//! No log line, audit row, error or result names it; the tests hold all of those to it.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::libs::cluster_state::{self, ClusterDir, Connection};
use crate::libs::config_applier::ConfigApplier;
use crate::libs::mqtt::server_hosts::{self, ServerResolver};

/// The audit operation of a join: the setup write that completes it.
pub const AUDIT_JOIN: &str = "CLUSTER_JOIN";
/// The audit `origin` of a setup write [`complete_interrupted_join`] makes at the agent's start.
pub const ORIGIN_RESUMED: &str = "resumed-at-start";

/// The bundle version this build reads.
pub const BUNDLE_VERSION: u32 = 1;
/// `h` is at most this long (`plan/control/ble/peripheral.md` §6.7).
pub const MAX_HOST_LEN: usize = 63;
/// `p`: 128 random bits, base64url without padding.
pub const PASSWORD_LEN: usize = 22;
/// `f`: SHA-256, base64url without padding.
pub const FINGERPRINT_LEN: usize = 43;

/// Where the Server serves its CA (cluster.md §6), over HTTPS: port 80 is closed.
pub const CA_ROUTE: &str = "/connection/ca";
pub const HTTPS_PORT: u16 = 443;

/// The whole network half of a join, resolution included, is bounded by this (§6.7: "bounded
/// at 20"). avahi's own resolver gives up after about 5 s.
pub const JOIN_DEADLINE: Duration = Duration::from_secs(20);
/// Any one connect, read or write.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// A CA is a few hundred bytes; a body this large is not one.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// How long after the answer the Hub reboots: long enough for the phone to read the result
/// (§6.7: "about 5 seconds after the result is readable"), plus the teardown's own grace.
pub const REBOOT_DELAY: Duration = Duration::from_secs(5);

/// One join at a time, process-wide: the two surfaces run on different threads.
static JOINING: Mutex<()> = Mutex::new(());

/// Tests in this crate that join hold this first: joins are exclusive ([`JOINING`]) and the
/// harness runs tests in parallel, so without it one test's join is another's `Busy`.
#[cfg(test)]
pub(crate) fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------------------------
// The bundle
// ---------------------------------------------------------------------------------------------

/// The broker password. Serialises as the plain string — it has to cross the root-only control
/// socket from `proximosctl` — and has no `Display`; `Debug` prints nothing of it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// The join bundle, as the QR code carries it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub v: u32,
    pub h: String,
    pub p: Secret,
    pub f: String,
}

impl Bundle {
    /// The bundle's JSON, strictly: the four keys and no other.
    pub fn parse(json: &[u8]) -> Result<Bundle, String> {
        serde_json::from_slice(json).map_err(|e| {
            // serde's message can quote a value; never a password's. Keep the category only.
            if e.is_syntax() || e.is_eof() {
                "the join bundle is not JSON".to_string()
            } else {
                "the join bundle must be exactly {\"v\",\"h\",\"p\",\"f\"}".to_string()
            }
        })
    }
}

/// Why a join was refused. The surfaces show the message; the CLI also maps the kind onto its
/// exit-code table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailKind {
    /// Setup is complete, or the Hub already has a role marker.
    NotJoinable,
    /// Another join is running.
    Busy,
    /// The bundle, or this Hub's own hostname, does not pass validation.
    Invalid,
    /// `h` does not resolve to an IPv4 address, or does not answer in time.
    Unreachable,
    /// The served CA does not match `f`, or the broker's certificate does not verify against it
    /// for `h`: the machine that answered is not the Server the bundle names.
    CaMismatch,
    /// The broker refused the account.
    BrokerRefused,
    /// Something on this Hub could not be written.
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinError {
    pub kind: FailKind,
    pub message: String,
}

impl JoinError {
    fn new(kind: FailKind, message: impl Into<String>) -> JoinError {
        JoinError {
            kind,
            message: message.into(),
        }
    }
}

/// A join that completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joined {
    pub server_host: String,
    /// What step 6 wrote, in order — the connection files, `role`, then `setup`.
    pub written: Vec<&'static str>,
}

impl Joined {
    /// The answer both surfaces show.
    pub fn message(&self) -> String {
        format!("joined {}; restarting", self.server_host)
    }
}

/// A bundle that passed step 2.
#[derive(Debug)]
pub struct Validated {
    pub host: String,
    pub password: Secret,
    pub fingerprint: String,
    pub username: String,
}

fn is_base64url(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `h` as the join accepts it: a DNS hostname of at most [`MAX_HOST_LEN`] characters,
/// lowercased, without a trailing dot. **No IP literal**, IPv4 or IPv6, until cluster.md §10
/// Q2 is decided: the Server's certificate names it by DNS only, so neither TLS client could
/// verify an address, and its listener is IPv4 only. A last label of digits alone is refused
/// for the same reason — a resolver may read `10.1.2` as an address.
pub fn validate_server_host(raw: &str) -> Result<String, String> {
    let h = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return Err("the join bundle names no Server (h is empty)".to_string());
    }
    let bare = h
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(&h);
    if bare.parse::<IpAddr>().is_ok() {
        return Err(format!(
            "the Server must be named by its hostname (<hostname>.local), not the address \
             {raw:?}: its certificate carries DNS names only"
        ));
    }
    if h.len() > MAX_HOST_LEN {
        return Err(format!(
            "the Server's name is longer than {MAX_HOST_LEN} characters"
        ));
    }
    let labels_ok = h.split('.').all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    });
    if !labels_ok {
        return Err(format!("{raw:?} is not a hostname"));
    }
    if h.rsplit('.')
        .next()
        .is_some_and(|last| last.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!(
            "the Server must be named by its hostname (<hostname>.local), not the address \
             {raw:?}"
        ));
    }
    Ok(h)
}

/// Step 2. `own_hostname` is this Hub's, as `/etc/hostname` holds it (decision #21: lowercase).
pub fn validate(bundle: &Bundle, own_hostname: &str) -> Result<Validated, JoinError> {
    let invalid = |m: String| JoinError::new(FailKind::Invalid, m);
    if bundle.v != BUNDLE_VERSION {
        return Err(invalid(format!(
            "join bundle version {} is not one this Hub reads (it reads version {BUNDLE_VERSION})",
            bundle.v
        )));
    }
    let host = validate_server_host(&bundle.h).map_err(invalid)?;

    let p = bundle.p.expose();
    if p.len() != PASSWORD_LEN || !is_base64url(p) {
        return Err(invalid(format!(
            "the join bundle's password must be {PASSWORD_LEN} characters of base64url"
        )));
    }
    cluster_state::validate_peer_password(p).map_err(invalid)?;

    let f = bundle.f.trim();
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(f);
    if f.len() != FINGERPRINT_LEN || !is_base64url(f) || decoded.map(|d| d.len()) != Ok(32) {
        return Err(invalid(format!(
            "the join bundle's CA fingerprint must be {FINGERPRINT_LEN} characters of base64url \
             (SHA-256)"
        )));
    }

    let own = crate::libs::cluster_clients::validate_hostname(own_hostname).map_err(|e| {
        invalid(format!(
            "this Hub's own hostname cannot name a broker account: {e}"
        ))
    })?;
    if host == own || host.strip_suffix(".local") == Some(own.as_str()) {
        return Err(invalid(format!(
            "{host} is this Hub: it cannot join itself"
        )));
    }
    let username = crate::libs::cluster_clients::peer_username(&own);
    cluster_state::validate_peer_username(&username).map_err(invalid)?;

    Ok(Validated {
        host,
        password: bundle.p.clone(),
        fingerprint: f.to_string(),
        username,
    })
}

// ---------------------------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------------------------

/// Everything a join reads from and does to the machine it runs on. [`run`] builds the real
/// one; a test points every path into a temp directory, the resolver at a script and
/// the ports at local servers, and records the reboot instead of doing it.
pub struct JoinEnv<'a> {
    pub applier: &'a ConfigApplier,
    pub cluster: ClusterDir,
    /// The file `/etc/hosts` links to, and the image's base it starts from.
    pub hosts_path: PathBuf,
    pub hosts_base_path: PathBuf,
    pub resolver: &'a dyn ServerResolver,
    pub https_port: u16,
    pub mqtt_port: u16,
    pub own_hostname: String,
    /// Step 7. Called once, after the answer is final; it must return at once.
    pub reboot: &'a (dyn Fn() -> Result<(), String> + Sync),
}

/// This Hub's hostname, as `/etc/hostname` holds it, lowercased (decision #21).
pub fn this_hub_hostname() -> Result<String, String> {
    std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_ascii_lowercase())
        .map_err(|e| format!("cannot read /etc/hostname: {e}"))
}

/// Step 7 on this Hub: a thread that waits [`REBOOT_DELAY`] and then takes the ordinary audited
/// reboot path (`system_control::spawn_teardown`), the one FB1B falls back to.
pub fn schedule_reboot() -> Result<(), String> {
    std::thread::Builder::new()
        .name("cluster-join-reboot".to_string())
        .spawn(|| {
            std::thread::sleep(REBOOT_DELAY);
            eprintln!("[cluster_join] rebooting into the client role");
            if let Err(e) = crate::libs::system_control::spawn_teardown("reboot") {
                eprintln!(
                    "[cluster_join] the reboot did not start ({e}); the join applies at the next \
                     boot"
                );
            }
        })
        .map(|_| ())
        .map_err(|e| format!("cannot start the reboot: {e}"))
}

/// The join on this Hub — what FB1C and the control socket call. `origin` names the surface in
/// the audit row (`ble`, `cli`). The machine is this one: the applier's cluster directory, the
/// runtime hosts file, avahi and the nameservers, ports 443 and 8883, `/etc/hostname`, and a
/// reboot [`REBOOT_DELAY`] after the answer.
pub fn run(
    bundle: &Bundle,
    origin: &str,
    applier: &Arc<ConfigApplier>,
) -> Result<Joined, JoinError> {
    let own_hostname = this_hub_hostname().map_err(|e| JoinError::new(FailKind::Invalid, e))?;
    let resolver = server_hosts::SystemResolver::default();
    let env = JoinEnv {
        applier,
        cluster: ClusterDir::new(applier.cluster_dir()),
        hosts_path: PathBuf::from(server_hosts::HOSTS_PATH),
        hosts_base_path: PathBuf::from(server_hosts::HOSTS_BASE_PATH),
        resolver: &resolver,
        https_port: HTTPS_PORT,
        mqtt_port: cluster_state::DEFAULT_SERVER_PORT,
        own_hostname,
        reboot: &schedule_reboot,
    };
    join(bundle, origin, &env)
}

// ---------------------------------------------------------------------------------------------
// The apply path
// ---------------------------------------------------------------------------------------------

/// What FB1C serves before any write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The marker says `client`.
    pub client: bool,
    /// A join can succeed: setup is not complete and there is no marker.
    pub joinable: bool,
    /// On a Client, the Server it joined.
    pub server: Option<String>,
}

pub fn status(applier: &ConfigApplier, cluster: &ClusterDir) -> Result<Status, String> {
    let (provisioned, _) = applier.setup_status()?;
    let marker = cluster
        .role_marker()
        .map_err(|e| format!("cannot read the cluster role marker: {e}"))?;
    let client = marker.as_deref() == Some(cluster_state::ROLE_CLIENT);
    Ok(Status {
        client,
        joinable: !provisioned && marker.is_none(),
        server: client.then(|| cluster.server_host()).flatten(),
    })
}

/// Step 1.
fn joinable(env: &JoinEnv<'_>) -> Result<(), JoinError> {
    let (provisioned, role) = env
        .applier
        .setup_status()
        .map_err(|e| JoinError::new(FailKind::Storage, e))?;
    if provisioned {
        return Err(JoinError::new(
            FailKind::NotJoinable,
            format!(
                "setup is already complete on this Hub (as {}); a factory reset is the only way \
                 to make it a client",
                role.as_deref().unwrap_or("no role")
            ),
        ));
    }
    match env.cluster.role_marker() {
        Ok(None) => Ok(()),
        Ok(Some(m)) if m == cluster_state::ROLE_CLIENT => Err(JoinError::new(
            FailKind::NotJoinable,
            format!(
                "this Hub has already joined {}",
                env.cluster
                    .server_host()
                    .unwrap_or_else(|| "a Server".to_string())
            ),
        )),
        Ok(Some(m)) => Err(JoinError::new(
            FailKind::NotJoinable,
            format!(
                "this Hub already has a cluster role marker ({m:?}); a factory reset is the only \
                 way to make it a client"
            ),
        )),
        Err(e) => Err(JoinError::new(
            FailKind::Storage,
            format!("cannot read the cluster role marker: {e}"),
        )),
    }
}

/// The whole join against `env`. See the module documentation for the steps.
pub fn join(bundle: &Bundle, origin: &str, env: &JoinEnv<'_>) -> Result<Joined, JoinError> {
    let _one = match JOINING.try_lock() {
        Ok(guard) => guard,
        // A join that panicked holds nothing a later one depends on.
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(JoinError::new(
                FailKind::Busy,
                "a cluster join is already running on this Hub",
            ))
        }
    };
    let result = join_locked(bundle, origin, env);
    match &result {
        Ok(j) => eprintln!("[cluster_join] via {origin}: {}", j.message()),
        Err(e) => eprintln!("[cluster_join] via {origin}: refused: {}", e.message),
    }
    result
}

fn join_locked(bundle: &Bundle, origin: &str, env: &JoinEnv<'_>) -> Result<Joined, JoinError> {
    let started = Instant::now();

    // 1. Not set up, no marker. The radio mode is not checked: a Client takes the Server's
    //    after its first report (decision #47).
    joinable(env)?;

    // 2. The bundle.
    let v = validate(bundle, &env.own_hostname)?;
    eprintln!(
        "[cluster_join] via {origin}: joining {} as {}",
        v.host, v.username
    );

    // 3. The Server's IPv4 address, in the hosts file before anything dials it.
    let ip = server_hosts::resolve_ipv4(&v.host, env.resolver).map_err(|e| {
        JoinError::new(
            FailKind::Unreachable,
            format!("the Server cannot be reached at {}: {e}", v.host),
        )
    })?;
    server_hosts::write_entry(&env.hosts_path, &env.hosts_base_path, &v.host, ip).map_err(|e| {
        JoinError::new(
            FailKind::Storage,
            format!(
                "cannot write the Server's address to {}: {e}",
                env.hosts_path.display()
            ),
        )
    })?;

    let undo_hosts = || {
        if let Err(e) = server_hosts::remove_entry(&env.hosts_path) {
            eprintln!(
                "[cluster_join] cannot remove the Server's line from {}: {e}",
                env.hosts_path.display()
            );
        }
    };

    match connect_and_write(&v, ip, origin, env, started) {
        Ok(written) => {
            let joined = Joined {
                server_host: v.host.clone(),
                written,
            };
            // 7. The answer is final; the reboot comes after the grace window.
            if let Err(e) = (env.reboot)() {
                eprintln!("[cluster_join] {e}; the join is saved and applies at the next boot");
            }
            Ok(joined)
        }
        Err(e) => {
            undo_hosts();
            Err(e)
        }
    }
}

/// Steps 4–6. Everything here either completes or leaves step 6's files unwritten.
fn connect_and_write(
    v: &Validated,
    ip: Ipv4Addr,
    origin: &str,
    env: &JoinEnv<'_>,
    started: Instant,
) -> Result<Vec<&'static str>, JoinError> {
    let deadline = Deadline(started + JOIN_DEADLINE);

    // 4. The CA, exactly as served, against `f`.
    let ca = fetch_ca(
        SocketAddr::new(IpAddr::V4(ip), env.https_port),
        &v.host,
        &deadline,
    )?;
    if cluster_state::bundle_fingerprint(&ca) != v.fingerprint {
        return Err(JoinError::new(
            FailKind::CaMismatch,
            format!(
                "the Server's CA does not match the bundle: a wrong QR code, or a different \
                 machine answered at {}",
                v.host
            ),
        ));
    }

    // 5. The broker, against that CA, as this Hub's account.
    broker_login(
        SocketAddr::new(IpAddr::V4(ip), env.mqtt_port),
        &v.host,
        &ca,
        &v.username,
        &v.password,
        &deadline,
    )?;

    // 6. The files, the marker, and `setup` last (decided 2026-10-02): a crash between the
    // last two leaves a Hub that boots as a Client and is completed at start, never one
    // provisioned as `client` with no marker.
    let storage = |what: &str, e: String| {
        JoinError::new(FailKind::Storage, format!("cannot save the {what}: {e}"))
    };
    // The undo: `clear` takes the marker first, then the connection files. The caller removes
    // the hosts line.
    let undo = || {
        if let Err(e) = env.cluster.clear() {
            eprintln!(
                "[cluster_join] cannot remove {}: {e}",
                env.cluster.dir().display()
            );
        }
    };
    let mut written: Vec<&'static str> = Vec::new();
    if let Err(e) = env.cluster.write_connection(&Connection {
        server_host: &v.host,
        server_port: env.mqtt_port,
        peer_username: &v.username,
        peer_password: v.password.expose(),
        ca_pem: &ca,
    }) {
        undo();
        return Err(storage("connection", e.to_string()));
    }
    written.extend(cluster_state::CONNECTION_FILES);

    if let Err(e) = env.cluster.write_role_client() {
        undo();
        return Err(storage("role marker", e.to_string()));
    }
    written.push(cluster_state::ROLE);

    // The setup write does not consult the marker it now follows, so FB17's setup-role lock
    // (`apply_setup_complete`) does not stand in its way; its own re-check of `provisioned`
    // keeps two racing surfaces from both landing.
    let setup = env
        .applier
        .apply_cluster_join_setup(origin, &v.host, &v.username);
    if !setup.success {
        undo();
        let message = setup
            .error_message
            .unwrap_or_else(|| "the setup write was refused".to_string());
        let kind = if message.contains("already complete") {
            FailKind::NotJoinable
        } else {
            FailKind::Storage
        };
        return Err(JoinError::new(kind, message));
    }
    written.push("setup");
    Ok(written)
}

/// Finish a join a crash interrupted between its marker and its `setup` write (step 6): the
/// marker says `client` and setup is not complete. The Hub already boots as a Client — the
/// renderer reads the marker — so what is missing is only the record: `setup: {provisioned:
/// true, role: client}` and its `CLUSTER_JOIN` audit row, with `origin` [`ORIGIN_RESUMED`].
/// Without it FB16 and `system/info` would report an unprovisioned Hub forever on a Client the
/// CLI joined, which has no wizard to finish it (FB17 `client` would also be accepted).
///
/// The agent calls this once at start. `Ok(true)` when it wrote; `Ok(false)` when there was
/// nothing to finish — no marker, a marker that is not `client`, or setup already complete.
pub fn complete_interrupted_join(applier: &ConfigApplier) -> Result<bool, String> {
    let cluster = ClusterDir::new(applier.cluster_dir());
    let client = cluster
        .is_client()
        .map_err(|e| format!("cannot read the cluster role marker: {e}"))?;
    if !client || applier.setup_status()?.0 {
        return Ok(false);
    }
    let host = cluster.server_host().unwrap_or_default();
    let username = cluster
        .value(cluster_state::PEER_USERNAME)
        .unwrap_or_default();
    let setup = applier.apply_cluster_join_setup(ORIGIN_RESUMED, &host, &username);
    if setup.success {
        eprintln!(
            "[cluster_join] the join of {host} was interrupted before its setup write; setup \
             completed as a client"
        );
        Ok(true)
    } else {
        Err(setup
            .error_message
            .unwrap_or_else(|| "the setup write was refused".to_string()))
    }
}

// ---------------------------------------------------------------------------------------------
// The network
// ---------------------------------------------------------------------------------------------

struct Deadline(Instant);

impl Deadline {
    /// The time left for one operation, capped at [`IO_TIMEOUT`].
    fn budget(&self, what: &str, host: &str) -> Result<Duration, JoinError> {
        let left = self.0.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(50) {
            return Err(JoinError::new(
                FailKind::Unreachable,
                format!("the Server at {host} did not answer in time ({what})"),
            ));
        }
        Ok(left.min(IO_TIMEOUT))
    }
}

fn tcp(
    addr: SocketAddr,
    host: &str,
    what: &str,
    deadline: &Deadline,
) -> Result<TcpStream, JoinError> {
    let t = deadline.budget(what, host)?;
    let s = TcpStream::connect_timeout(&addr, t).map_err(|e| {
        JoinError::new(
            FailKind::Unreachable,
            format!("the Server cannot be reached at {host} ({addr}, {what}): {e}"),
        )
    })?;
    let t = deadline.budget(what, host)?;
    let _ = s.set_read_timeout(Some(t));
    let _ = s.set_write_timeout(Some(t));
    Ok(s)
}

fn ssl_err(kind: FailKind, what: String) -> impl FnOnce(openssl::error::ErrorStack) -> JoinError {
    move |e| JoinError::new(kind, format!("{what}: {e}"))
}

/// Step 4: `GET /connection/ca` over HTTPS, the server certificate **not** verified, the body
/// returned byte for byte. HTTP/1.0 with `Connection: close`, so the body is not chunked and
/// ends where the connection does.
fn fetch_ca(addr: SocketAddr, host: &str, deadline: &Deadline) -> Result<Vec<u8>, JoinError> {
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
    let what = format!("the Server's CA from https://{host}{CA_ROUTE}");
    let stream = tcp(addr, host, "fetching its CA", deadline)?;

    let mut builder = SslConnector::builder(SslMethod::tls_client())
        .map_err(ssl_err(FailKind::Storage, "cannot set up TLS".into()))?;
    // Trust on nothing here: the bytes are checked against `f` instead.
    builder.set_verify(SslVerifyMode::NONE);
    let mut config = builder
        .build()
        .configure()
        .map_err(ssl_err(FailKind::Storage, "cannot set up TLS".into()))?;
    config.set_verify_hostname(false);
    let mut tls = config.connect(host, stream).map_err(|e| {
        JoinError::new(
            FailKind::Unreachable,
            format!("cannot fetch {what}: TLS failed: {e}"),
        )
    })?;

    let request = format!(
        "GET {CA_ROUTE} HTTP/1.0\r\nHost: {host}\r\nAccept: application/x-pem-file\r\n\
         User-Agent: proximos-cluster-join\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(request.as_bytes())
        .and_then(|()| tls.flush())
        .map_err(|e| JoinError::new(FailKind::Unreachable, format!("cannot fetch {what}: {e}")))?;

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if response.len() > MAX_RESPONSE_BYTES {
                    return Err(JoinError::new(
                        FailKind::CaMismatch,
                        format!("{what} is larger than a CA certificate can be"),
                    ));
                }
                if http_complete(&response) {
                    break;
                }
            }
            // A peer that closes without close_notify: what arrived is the answer if it is
            // complete by its own Content-Length, and a failure otherwise.
            Err(e) => {
                if http_complete(&response) {
                    break;
                }
                return Err(JoinError::new(
                    FailKind::Unreachable,
                    format!("cannot fetch {what}: {e}"),
                ));
            }
        }
    }
    let _ = tls.shutdown();
    let body = http_body(&response)
        .map_err(|e| JoinError::new(FailKind::Unreachable, format!("cannot fetch {what}: {e}")))?;
    if openssl::x509::X509::stack_from_pem(&body)
        .map(|certs| certs.is_empty())
        .unwrap_or(true)
    {
        return Err(JoinError::new(
            FailKind::CaMismatch,
            format!("{what} is not a PEM certificate"),
        ));
    }
    Ok(body)
}

/// The header block's end, if it has arrived.
fn header_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse().ok())
            .flatten()
    })
}

/// A response is complete when its headers are in and, if it declared a length, that many body
/// bytes are too. Without a length, only the end of the stream completes it.
fn http_complete(response: &[u8]) -> bool {
    let Some(end) = header_end(response) else {
        return false;
    };
    let head = String::from_utf8_lossy(&response[..end]);
    content_length(&head).is_some_and(|n| response.len() - end >= n)
}

/// The body of a `200` response, exactly as sent.
fn http_body(response: &[u8]) -> Result<Vec<u8>, String> {
    let end = header_end(response).ok_or("the answer is not HTTP")?;
    let head = String::from_utf8_lossy(&response[..end]);
    let status_line = head.lines().next().unwrap_or_default();
    let mut parts = status_line.split_whitespace();
    let (Some(proto), Some(code)) = (parts.next(), parts.next()) else {
        return Err("the answer is not HTTP".to_string());
    };
    if !proto.starts_with("HTTP/1.") {
        return Err("the answer is not HTTP".to_string());
    }
    match code {
        "200" => {}
        "404" => return Err("the Server serves no CA (404)".to_string()),
        other => return Err(format!("the Server answered HTTP {other}")),
    }
    let chunked = head.lines().any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && !v.trim().eq_ignore_ascii_case("identity")
        })
    });
    if chunked {
        return Err("the Server sent an encoded body to an HTTP/1.0 request".to_string());
    }
    let body = &response[end..];
    match content_length(&head) {
        Some(n) if body.len() < n => Err("the answer was cut short".to_string()),
        Some(n) => Ok(body[..n].to_vec()),
        None => Ok(body.to_vec()),
    }
}

/// Step 5: TLS to the broker, verified against `ca` **for `host`**, then one MQTT 3.1.1
/// CONNECT as `username` / `password`, the CONNACK read, and a DISCONNECT.
fn broker_login(
    addr: SocketAddr,
    host: &str,
    ca: &[u8],
    username: &str,
    password: &Secret,
    deadline: &Deadline,
) -> Result<(), JoinError> {
    use openssl::ssl::{HandshakeError, SslConnector, SslMethod, SslVerifyMode};
    use openssl::x509::store::X509StoreBuilder;
    use openssl::x509::{X509VerifyResult, X509};

    let certs = X509::stack_from_pem(ca).map_err(|_| {
        JoinError::new(
            FailKind::CaMismatch,
            "the Server's CA is not a PEM certificate",
        )
    })?;
    let mut store =
        X509StoreBuilder::new().map_err(ssl_err(FailKind::Storage, "cannot set up TLS".into()))?;
    for c in certs {
        store.add_cert(c).map_err(ssl_err(
            FailKind::CaMismatch,
            "the Server's CA cannot be used".into(),
        ))?;
    }
    let mut builder = SslConnector::builder(SslMethod::tls_client())
        .map_err(ssl_err(FailKind::Storage, "cannot set up TLS".into()))?;
    // Only the fetched CA: never the image's public roots.
    builder.set_cert_store(store.build());
    builder.set_verify(SslVerifyMode::PEER);
    let config = builder
        .build()
        .configure()
        .map_err(ssl_err(FailKind::Storage, "cannot set up TLS".into()))?;

    let stream = tcp(addr, host, "its broker", deadline)?;
    let mut tls = match config.connect(host, stream) {
        Ok(s) => s,
        Err(HandshakeError::Failure(mid)) => {
            let verify = mid.ssl().verify_result();
            return Err(if verify != X509VerifyResult::OK {
                JoinError::new(
                    FailKind::CaMismatch,
                    format!(
                        "the broker at {host}:{} does not prove it is that Server: {}",
                        addr.port(),
                        verify.error_string()
                    ),
                )
            } else {
                JoinError::new(
                    FailKind::Unreachable,
                    format!(
                        "TLS to the broker at {host}:{} failed: {}",
                        addr.port(),
                        mid.error()
                    ),
                )
            });
        }
        Err(e) => {
            return Err(JoinError::new(
                FailKind::Unreachable,
                format!("TLS to the broker at {host}:{} failed: {e}", addr.port()),
            ))
        }
    };

    let client_id = format!("{username}-join");
    let packet = connect_packet(&client_id, username, password.expose());
    let io = |e: std::io::Error| {
        JoinError::new(
            FailKind::Unreachable,
            format!(
                "the broker at {host}:{} stopped answering: {e}",
                addr.port()
            ),
        )
    };
    tls.write_all(&packet)
        .and_then(|()| tls.flush())
        .map_err(io)?;
    let mut connack = [0u8; 4];
    tls.read_exact(&mut connack).map_err(io)?;
    let _ = tls.write_all(&[0xE0, 0x00]);
    let _ = tls.shutdown();
    if connack[0] != 0x20 || connack[1] != 0x02 {
        return Err(JoinError::new(
            FailKind::BrokerRefused,
            format!("{host}:{} did not answer as an MQTT broker", addr.port()),
        ));
    }
    match connack[3] {
        0 => Ok(()),
        rc => Err(JoinError::new(
            FailKind::BrokerRefused,
            format!(
                "the broker refused {username}: {} (the bundle was re-minted on the Server \
                 since, or was minted for another Hub)",
                connack_reason(rc)
            ),
        )),
    }
}

/// MQTT 3.1.1 CONNACK return codes (§3.2.2.3).
fn connack_reason(rc: u8) -> String {
    match rc {
        1 => "unacceptable protocol version".to_string(),
        2 => "identifier rejected".to_string(),
        3 => "server unavailable".to_string(),
        4 => "bad user name or password".to_string(),
        5 => "not authorized".to_string(),
        other => format!("return code {other}"),
    }
}

/// An MQTT 3.1.1 CONNECT: clean session, 30 s keep-alive, user name and password.
fn connect_packet(client_id: &str, username: &str, password: &str) -> Vec<u8> {
    fn string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut body = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0xC2, 0x00, 30];
    string(&mut body, client_id);
    string(&mut body, username);
    string(&mut body, password);
    let mut packet = vec![0x10];
    let mut len = body.len();
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        packet.push(byte);
        if len == 0 {
            break;
        }
    }
    packet.extend_from_slice(&body);
    packet
}

#[cfg(test)]
mod tests;
