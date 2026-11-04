//! The Server's IPv4 address, kept in the runtime hosts file (plan/control/cluster.md §8).
//!
//! Decided 2026-10-02 (Matej). A Client Hub reaches its Server by name — `<server>.local`
//! by default — because the Server's certificate carries DNS SANs only (§6). Two
//! processes dial that name, and neither can do it alone:
//!
//! - **`chirpstack-mqtt-forwarder`** is a static musl binary. musl has no NSS, so no
//!   nss-mdns, and `.local` never resolves; its rustls verifies the URL host, so an IP in
//!   the URL fails verification against a DNS-only certificate.
//! - **This agent** connects with rumqttc, which hands the same string to the resolver and
//!   to native-tls; on glibc a `.local` name resolves through nss-mdns, but the listener is
//!   IPv4 only (`0.0.0.0`, §5) and nothing guarantees an A record first for a DNS name.
//!
//! So this agent — root, glibc, long-running — resolves the name to an IPv4 address and
//! keeps one line, `<ipv4> <server_host>`, in the file `/etc/hosts` links to
//! ([`HOSTS_PATH`]). musl reads `/etc/hosts` before DNS on every lookup, with no cache
//! (`src/network/lookup_name.c`, `name_from_hosts`), so the forwarder connects by name and
//! verifies it, and picks a changed line up on its next connect without a restart. The
//! agent's own lookup goes `files` first (`hosts: files myhostname mdns4_minimal
//! [NOTFOUND=return] dns`) and gets the same IPv4 address, so rumqttc connects by name over
//! plain TLS, verified against that name.
//!
//! **The refresh never reads the hosts file to resolve**, or it would read its own line
//! back and never notice the Server moving: `.local` names go to avahi over D-Bus
//! (`ResolveHostName`, A records only), every other name to the nameservers in
//! `/etc/resolv.conf` as a plain DNS A query. Neither shells out (decision #6).
//!
//! The rest of the file — base-files' localhost lines, `set-hostname.sh`'s `127.0.1.1`
//! line — is preserved byte for byte; only the line carrying [`MARKER`] is ours. Writes
//! are temp-file-and-rename in the same directory, so a reader sees the old file or the
//! new one and never half of either. A failed lookup keeps the last good line. An IPv6
//! address is never written: the type is [`Ipv4Addr`] from the resolver's answer on.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::libs::config::BrokerConfig;

/// Where `/etc/hosts` points on the image (`base-files_%.bbappend`), seeded at every boot by
/// tmpfiles from [`HOSTS_BASE_PATH`]. `fiber.service` may write this directory and no
/// other under `/run/proximos`.
pub const HOSTS_PATH: &str = "/run/proximos/net/hosts";

/// base-files' own `/etc/hosts`, as the image ships it. Used only when [`HOSTS_PATH`] is
/// missing, so that writing our line never produces a hosts file without `localhost`.
pub const HOSTS_BASE_PATH: &str = "/usr/share/proximos/hosts.base";

/// Ends the one line this module owns. glibc and musl both treat `#` as a comment.
pub const MARKER: &str = "# proximos: cluster Server, kept by fiber_app";

/// How often the line is checked while everything is fine: the forwarder depends on it
/// even while the agent's own session is up.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// The reconnect path refreshes too, but not more often than this: a broker that refuses
/// the account fails fast, and the backoff starts at a few seconds.
pub const MIN_REFRESH_GAP: Duration = Duration::from_secs(10);

/// Per-query time limits. avahi's own host-name resolver gives up after about 5 s.
const AVAHI_TIMEOUT: Duration = Duration::from_secs(10);
const DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// Does this broker need the hosts entry? Only the Server's broker on a Client (the
/// rendered override, never a hand-configured one), and only when it is named by a
/// hostname: an IP literal needs no lookup, and loopback is not the Server.
pub fn wants_hosts_entry(broker: &BrokerConfig) -> bool {
    broker.from_cluster_override && is_remote_name(&broker.host)
}

fn is_remote_name(host: &str) -> bool {
    let host = host.trim();
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    !bare.is_empty() && bare.parse::<IpAddr>().is_err() && !bare.eq_ignore_ascii_case("localhost")
}

/// `name` is resolved over mDNS (avahi) rather than unicast DNS.
fn is_mdns_name(name: &str) -> bool {
    name.trim_end_matches('.')
        .to_ascii_lowercase()
        .ends_with(".local")
}

// ---------------------------------------------------------------------------
// The hosts file
// ---------------------------------------------------------------------------

/// `current` with our line for `host` set to `addr`: every line that carries [`MARKER`] is
/// dropped (there is at most one, for this host or a previous one), every other line is
/// kept as it is, and one new line is appended.
pub fn render_hosts(current: &str, host: &str, addr: Ipv4Addr) -> String {
    let mut out = String::with_capacity(current.len() + 80);
    for line in current.lines().filter(|l| !l.contains(MARKER)) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("{addr}\t{host}\t{MARKER}\n"));
    out
}

/// The address our line currently gives `host`, if there is such a line.
pub fn managed_entry(current: &str, host: &str) -> Option<Ipv4Addr> {
    current
        .lines()
        .filter(|l| l.contains(MARKER))
        .find_map(|l| {
            let mut fields = l.split_whitespace();
            let addr = fields.next()?.parse::<Ipv4Addr>().ok()?;
            fields
                .next()
                .filter(|h| h.eq_ignore_ascii_case(host))
                .map(|_| addr)
        })
}

/// Write our line for `host` into `path`, starting from `base_path` when `path` does not exist
/// yet, so the file never ends up without its localhost lines. The cluster join's step 3
/// (cluster.md §8.3): the Server's address is in the file before anything dials it.
pub fn write_entry(
    path: &Path,
    base_path: &Path,
    host: &str,
    addr: Ipv4Addr,
) -> std::io::Result<()> {
    let current = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::read_to_string(base_path).unwrap_or_default()
        }
        Err(e) => return Err(e),
    };
    let wanted = render_hosts(&current, host, addr);
    if wanted == current {
        return Ok(());
    }
    write_atomic(path, &wanted)
}

/// Drop our line from `path`, keeping every other line byte for byte. `Ok(false)` when there
/// was none (or no file). The cluster join's undo when it fails after writing the line.
pub fn remove_entry(path: &Path) -> std::io::Result<bool> {
    let current = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !current.lines().any(|l| l.contains(MARKER)) {
        return Ok(false);
    }
    let mut out = String::with_capacity(current.len());
    for line in current.lines().filter(|l| !l.contains(MARKER)) {
        out.push_str(line);
        out.push('\n');
    }
    write_atomic(path, &out)?;
    Ok(true)
}

/// Replace `path` with `content`: a temp file in the same directory, 0644, synced, then
/// renamed over it. A reader opens the old file or the new one, never a partial one.
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hosts".into());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// ---------------------------------------------------------------------------
// Resolution, bypassing the hosts file
// ---------------------------------------------------------------------------

/// The two lookups the refresh may use. Neither may consult the hosts file.
pub trait ServerResolver: Send + Sync {
    /// A `.local` name over mDNS.
    fn mdns(&self, name: &str) -> Result<Vec<IpAddr>, String>;
    /// Any other name, as a unicast DNS A query.
    fn dns(&self, name: &str) -> Result<Vec<IpAddr>, String>;
}

/// The Server's IPv4 address for `host`: mDNS for a `.local` name, DNS for anything else,
/// and the first IPv4 address of the answer. IPv6 addresses are skipped; an answer with
/// nothing else is an error, because the Server's 8883 listener is IPv4 only (§5).
pub fn resolve_ipv4(host: &str, resolver: &dyn ServerResolver) -> Result<Ipv4Addr, String> {
    let (how, addrs) = if is_mdns_name(host) {
        ("mDNS", resolver.mdns(host)?)
    } else {
        ("DNS", resolver.dns(host)?)
    };
    addrs
        .iter()
        .find_map(|a| match a {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
        .ok_or_else(|| {
            if addrs.is_empty() {
                format!("{how} has no address for {host}")
            } else {
                format!(
                    "{how} has only IPv6 addresses for {host}, and the Server's 8883 listener \
                     is IPv4 only"
                )
            }
        })
}

/// The device's resolvers: avahi over the system D-Bus, and the `resolv.conf` nameservers.
pub struct SystemResolver {
    pub resolv_conf: PathBuf,
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self {
            resolv_conf: PathBuf::from("/etc/resolv.conf"),
        }
    }
}

impl ServerResolver for SystemResolver {
    /// `org.freedesktop.Avahi.Server.ResolveHostName(interface, protocol, name,
    /// aprotocol, flags)` with `AVAHI_IF_UNSPEC` (-1), `AVAHI_PROTO_UNSPEC` (-1) for the
    /// transport and `AVAHI_PROTO_INET` (0) for the address: A records only. avahi
    /// answers from the network or its cache, never from `/etc/hosts`.
    fn mdns(&self, name: &str) -> Result<Vec<IpAddr>, String> {
        let conn = dbus::blocking::Connection::new_system()
            .map_err(|e| format!("cannot reach the system D-Bus for avahi: {e}"))?;
        let proxy = conn.with_proxy("org.freedesktop.Avahi", "/", AVAHI_TIMEOUT);
        let (_iface, _proto, _name, _aproto, address, _flags): (
            i32,
            i32,
            String,
            i32,
            String,
            u32,
        ) = proxy
            .method_call(
                "org.freedesktop.Avahi.Server",
                "ResolveHostName",
                (-1i32, -1i32, name, 0i32, 0u32),
            )
            .map_err(|e| format!("avahi cannot resolve {name}: {e}"))?;
        address
            .parse::<IpAddr>()
            .map(|a| vec![a])
            .map_err(|e| format!("avahi answered {address:?} for {name}: {e}"))
    }

    fn dns(&self, name: &str) -> Result<Vec<IpAddr>, String> {
        let text = std::fs::read_to_string(&self.resolv_conf)
            .map_err(|e| format!("cannot read {}: {e}", self.resolv_conf.display()))?;
        let servers = nameservers(&text);
        if servers.is_empty() {
            return Err(format!(
                "{} lists no nameserver to ask for {name}",
                self.resolv_conf.display()
            ));
        }
        let mut last = String::new();
        for ns in servers {
            match query_a(SocketAddr::new(ns, 53), name) {
                Ok(addrs) if !addrs.is_empty() => {
                    return Ok(addrs.into_iter().map(IpAddr::V4).collect())
                }
                Ok(_) => last = format!("{ns} has no A record for {name}"),
                Err(e) => last = format!("{ns}: {e}"),
            }
        }
        Err(last)
    }
}

/// The `nameserver` addresses of a `resolv.conf`, in order. A scoped IPv6 address
/// (`fe80::1%eth0`) does not parse and is skipped.
pub fn nameservers(resolv_conf: &str) -> Vec<IpAddr> {
    resolv_conf
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            (f.next()? == "nameserver")
                .then(|| f.next()?.parse::<IpAddr>().ok())
                .flatten()
        })
        .collect()
}

/// One DNS A query over UDP, RD set.
fn query_a(server: SocketAddr, name: &str) -> Result<Vec<Ipv4Addr>, String> {
    let id: u16 = rand::random();
    let query = build_a_query(id, name)?;
    let bind: SocketAddr = if server.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let sock = UdpSocket::bind(bind).map_err(|e| format!("bind: {e}"))?;
    sock.set_read_timeout(Some(DNS_TIMEOUT))
        .map_err(|e| format!("timeout: {e}"))?;
    sock.connect(server).map_err(|e| format!("connect: {e}"))?;
    sock.send(&query).map_err(|e| format!("send: {e}"))?;
    let mut buf = [0u8; 1500];
    // A stray datagram with another id is skipped, up to a few, within the timeout.
    for _ in 0..4 {
        let n = sock.recv(&mut buf).map_err(|e| format!("no answer: {e}"))?;
        match parse_a_answers(id, &buf[..n]) {
            Err(e) if e == WRONG_ID => continue,
            other => return other,
        }
    }
    Err("no answer with our query id".into())
}

const WRONG_ID: &str = "answer to another query";

/// A standard query for `name` IN A, recursion desired.
pub fn build_a_query(id: u16, name: &str) -> Result<Vec<u8>, String> {
    let mut q = Vec::with_capacity(18 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // RD
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // QD=1, AN=NS=AR=0
    let name = name.trim_end_matches('.');
    if name.is_empty() || name.len() > 253 {
        return Err(format!("{name:?} is not a DNS name"));
    }
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("{name:?} is not a DNS name"));
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&[0, 1, 0, 1]); // QTYPE A, QCLASS IN
    Ok(q)
}

/// The A records in the answer section of a response to query `id`. CNAMEs in front of
/// them are skipped; an error rcode is an error.
pub fn parse_a_answers(id: u16, msg: &[u8]) -> Result<Vec<Ipv4Addr>, String> {
    let short = || "truncated DNS answer".to_string();
    if msg.len() < 12 {
        return Err(short());
    }
    if u16::from_be_bytes([msg[0], msg[1]]) != id {
        return Err(WRONG_ID.into());
    }
    if msg[2] & 0x80 == 0 {
        return Err("not a DNS response".into());
    }
    match msg[3] & 0x0f {
        0 => {}
        3 => return Err("no such name (NXDOMAIN)".into()),
        rcode => return Err(format!("DNS error rcode {rcode}")),
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let an = u16::from_be_bytes([msg[6], msg[7]]);
    let mut at = 12;
    for _ in 0..qd {
        at = skip_name(msg, at).ok_or_else(short)? + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        at = skip_name(msg, at).ok_or_else(short)?;
        let rr = msg.get(at..at + 10).ok_or_else(short)?;
        let rtype = u16::from_be_bytes([rr[0], rr[1]]);
        let class = u16::from_be_bytes([rr[2], rr[3]]);
        let len = u16::from_be_bytes([rr[8], rr[9]]) as usize;
        at += 10;
        let data = msg.get(at..at + len).ok_or_else(short)?;
        if rtype == 1 && class == 1 && len == 4 {
            out.push(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
        }
        at += len;
    }
    Ok(out)
}

/// The offset just past the (possibly compressed) name at `at`.
fn skip_name(msg: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = *msg.get(at)?;
        match len {
            0 => return Some(at + 1),
            l if l & 0xc0 == 0xc0 => {
                msg.get(at + 1)?;
                return Some(at + 2);
            }
            l if l & 0xc0 == 0 => at += 1 + l as usize,
            _ => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// The keeper
// ---------------------------------------------------------------------------

/// What one refresh did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refresh {
    /// The file already says this.
    Unchanged(Ipv4Addr),
    /// The line was written (new, or the address changed).
    Written(Ipv4Addr),
    /// The lookup failed; the line from before stays.
    KeptLastGood(Ipv4Addr, String),
    /// The lookup failed and there is no line to keep.
    NoAddress(String),
    /// The lookup worked but the file could not be written.
    WriteFailed(Ipv4Addr, String),
}

struct State {
    last_good: Option<Ipv4Addr>,
    last_refresh: Option<Instant>,
}

/// Keeps the Server's line in the hosts file for one `host`.
pub struct ServerHosts {
    host: String,
    path: PathBuf,
    base_path: PathBuf,
    resolver: Box<dyn ServerResolver>,
    state: Mutex<State>,
}

impl ServerHosts {
    /// For the agent's broker on the device, or `None` when it needs no entry
    /// ([`wants_hosts_entry`]).
    pub fn for_broker(broker: &BrokerConfig) -> Option<Arc<Self>> {
        wants_hosts_entry(broker).then(|| {
            Arc::new(Self::new(
                broker.host.trim(),
                HOSTS_PATH,
                HOSTS_BASE_PATH,
                Box::new(SystemResolver::default()),
            ))
        })
    }

    /// The line already in the file (an agent restart in the same boot) counts as the
    /// last good address, so a failed first lookup does not lose it.
    pub fn new(
        host: &str,
        path: impl Into<PathBuf>,
        base_path: impl Into<PathBuf>,
        resolver: Box<dyn ServerResolver>,
    ) -> Self {
        let path = path.into();
        let last_good = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| managed_entry(&t, host));
        Self {
            host: host.to_string(),
            path,
            base_path: base_path.into(),
            resolver,
            state: Mutex::new(State {
                last_good,
                last_refresh: None,
            }),
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Resolve, and rewrite the file if it does not already say that. Logs the outcome
    /// when it is anything but "unchanged".
    pub fn refresh(&self) -> Refresh {
        let resolved = resolve_ipv4(&self.host, self.resolver.as_ref());
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.last_refresh = Some(Instant::now());
        let outcome = match resolved {
            Err(why) => match state.last_good {
                Some(ip) => Refresh::KeptLastGood(ip, why),
                None => Refresh::NoAddress(why),
            },
            Ok(ip) => {
                let current = match std::fs::read_to_string(&self.path) {
                    Ok(t) => Ok(t),
                    // Missing: tmpfiles did not seed it. Start from the image's base, so
                    // the file never ends up without its localhost lines.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Ok(std::fs::read_to_string(&self.base_path).unwrap_or_default())
                    }
                    Err(e) => Err(e),
                };
                match current {
                    Err(e) => Refresh::WriteFailed(
                        ip,
                        format!("cannot read {}: {e}", self.path.display()),
                    ),
                    Ok(current) => {
                        let wanted = render_hosts(&current, &self.host, ip);
                        if wanted == current {
                            state.last_good = Some(ip);
                            Refresh::Unchanged(ip)
                        } else {
                            match write_atomic(&self.path, &wanted) {
                                Ok(()) => {
                                    state.last_good = Some(ip);
                                    Refresh::Written(ip)
                                }
                                Err(e) => Refresh::WriteFailed(
                                    ip,
                                    format!("cannot write {}: {e}", self.path.display()),
                                ),
                            }
                        }
                    }
                }
            }
        };
        drop(state);
        match &outcome {
            Refresh::Unchanged(_) => {}
            Refresh::Written(ip) => eprintln!(
                "[Server hosts] {} is {ip}; {} updated",
                self.host,
                self.path.display()
            ),
            Refresh::KeptLastGood(ip, why) => eprintln!(
                "[Server hosts] cannot resolve {} ({why}); keeping {ip} in {}",
                self.host,
                self.path.display()
            ),
            Refresh::NoAddress(why) => eprintln!(
                "[Server hosts] cannot resolve {} ({why}); no address in {} yet, the \
                 forwarder cannot reach the Server",
                self.host,
                self.path.display()
            ),
            Refresh::WriteFailed(ip, why) => {
                eprintln!("[Server hosts] {} is {ip}, but {why}", self.host)
            }
        }
        outcome
    }

    /// [`Self::refresh`], unless one ran less than `gap` ago.
    pub fn refresh_if_older_than(&self, gap: Duration) -> Option<Refresh> {
        let recent = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_refresh
            .is_some_and(|t| t.elapsed() < gap);
        (!recent).then(|| self.refresh())
    }

    /// Refresh every `interval` on a thread of its own until `shutdown` is set. The lookups
    /// block (D-Bus, UDP), so they stay off the async runtime.
    pub fn spawn_refresher(
        self: Arc<Self>,
        interval: Duration,
        shutdown: Arc<AtomicBool>,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new()
            .name("server-hosts".into())
            .spawn(move || {
                let step = Duration::from_secs(1).min(interval);
                loop {
                    let start = Instant::now();
                    while start.elapsed() < interval {
                        if shutdown.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(step);
                    }
                    let _ = self.refresh_if_older_than(interval / 2);
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const BASE: &str = "127.0.0.1\tlocalhost.localdomain\t\tlocalhost\n\
                        # a comment the image ships\n\
                        ::1\tlocalhost ip6-localhost ip6-loopback\n\
                        127.0.1.1\tproximos-5e6f7a8b\n";
    const SERVER: &str = "proximos-1a2b3c4d.local";

    /// A resolver that answers from a script, counts its calls, and records which lookup
    /// was asked. It has no hosts file to read, which is the point.
    #[derive(Default)]
    struct Scripted {
        mdns: Mutex<Vec<Result<Vec<IpAddr>, String>>>,
        dns: Mutex<Vec<Result<Vec<IpAddr>, String>>>,
        mdns_calls: AtomicUsize,
        dns_calls: AtomicUsize,
    }

    impl Scripted {
        fn mdns(answers: Vec<Result<Vec<IpAddr>, String>>) -> Self {
            Self {
                mdns: Mutex::new(answers.into_iter().rev().collect()),
                ..Default::default()
            }
        }
        fn dns(answers: Vec<Result<Vec<IpAddr>, String>>) -> Self {
            Self {
                dns: Mutex::new(answers.into_iter().rev().collect()),
                ..Default::default()
            }
        }
    }

    impl ServerResolver for Scripted {
        fn mdns(&self, _: &str) -> Result<Vec<IpAddr>, String> {
            self.mdns_calls.fetch_add(1, Ordering::SeqCst);
            self.mdns
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err("script ran out".into()))
        }
        fn dns(&self, _: &str) -> Result<Vec<IpAddr>, String> {
            self.dns_calls.fetch_add(1, Ordering::SeqCst);
            self.dns
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err("script ran out".into()))
        }
    }

    impl ServerResolver for Arc<Scripted> {
        fn mdns(&self, n: &str) -> Result<Vec<IpAddr>, String> {
            self.as_ref().mdns(n)
        }
        fn dns(&self, n: &str) -> Result<Vec<IpAddr>, String> {
            self.as_ref().dns(n)
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn broker(host: &str, from_override: bool) -> BrokerConfig {
        BrokerConfig {
            host: host.into(),
            port: 8883,
            client_id: String::new(),
            username: Some("peer-proximos-5e6f7a8b".into()),
            password: Some("Zm9vYmFyYmF6cXV4cXV1eA".into()),
            from_cluster_override: from_override,
        }
    }

    /// The join's step 3 and its undo: the line goes in on top of the image's base when the
    /// runtime file is missing, comes out again leaving every other line as it was, and a
    /// second removal is a no-op.
    #[test]
    fn the_joins_line_is_written_from_the_base_and_removed_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts");
        let base = dir.path().join("hosts.base");
        std::fs::write(&base, BASE).unwrap();

        write_entry(&path, &base, SERVER, Ipv4Addr::new(192, 168, 1, 20)).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(BASE), "{written}");
        assert_eq!(
            managed_entry(&written, SERVER),
            Some(Ipv4Addr::new(192, 168, 1, 20))
        );

        assert!(remove_entry(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BASE);
        assert!(!remove_entry(&path).unwrap(), "nothing left to remove");
        assert!(!remove_entry(&dir.path().join("absent")).unwrap());
    }

    #[test]
    fn only_the_servers_broker_named_by_a_hostname_gets_an_entry() {
        for h in [SERVER, "mqtt.example.com", " proximos-1a2b3c4d.local "] {
            assert!(wants_hosts_entry(&broker(h, true)), "{h}");
        }
        for h in [
            "192.168.1.20",
            "fd00::1",
            "[fd00::1]",
            "localhost",
            "127.0.0.1",
            "",
        ] {
            assert!(
                !wants_hosts_entry(&broker(h, true)),
                "{h}: no lookup needed"
            );
        }
        assert!(
            !wants_hosts_entry(&broker(SERVER, false)),
            "a broker from fiber.config.yaml never touches the hosts file (a Server)"
        );
    }

    #[test]
    fn the_rewrite_keeps_the_base_and_replaces_only_our_line() {
        let one = render_hosts(BASE, SERVER, Ipv4Addr::new(192, 168, 1, 20));
        assert!(one.starts_with(BASE), "base kept byte for byte:\n{one}");
        assert_eq!(one.lines().filter(|l| l.contains(MARKER)).count(), 1);
        assert_eq!(
            managed_entry(&one, SERVER),
            Some(Ipv4Addr::new(192, 168, 1, 20))
        );

        let two = render_hosts(&one, SERVER, Ipv4Addr::new(10, 0, 0, 7));
        assert!(two.starts_with(BASE));
        assert_eq!(
            two.lines().filter(|l| l.contains(MARKER)).count(),
            1,
            "{two}"
        );
        assert_eq!(
            managed_entry(&two, SERVER),
            Some(Ipv4Addr::new(10, 0, 0, 7))
        );
        assert!(!two.contains("192.168.1.20"));
        assert_eq!(two.lines().count(), BASE.lines().count() + 1);

        // A line for a previous Server name is ours too, and goes.
        let other = render_hosts(&two, "proximos-99999999.local", Ipv4Addr::new(10, 0, 0, 8));
        assert!(!other.contains(SERVER), "{other}");
        assert_eq!(managed_entry(&other, SERVER), None);

        // Our line is `<ipv4> TAB <name> TAB # …`: glibc and musl both stop at '#'.
        let line = two.lines().last().unwrap();
        let fields: Vec<_> = line.split('#').next().unwrap().split_whitespace().collect();
        assert_eq!(fields, ["10.0.0.7", SERVER]);

        // No trailing newline on the base is mended, not glued onto our line.
        let glued = render_hosts("127.0.0.1 localhost", SERVER, Ipv4Addr::new(10, 0, 0, 7));
        assert!(
            glued.starts_with("127.0.0.1 localhost\n10.0.0.7\t"),
            "{glued:?}"
        );
    }

    #[test]
    fn the_write_is_atomic_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts");
        std::fs::write(&path, BASE).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        use std::os::unix::fs::MetadataExt;

        write_atomic(&path, "new\n").unwrap();
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert_ne!(
            before.ino(),
            after.ino(),
            "a new inode: renamed in, not rewritten in place"
        );
        assert_eq!(after.permissions().mode() & 0o777, 0o644);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["hosts"], "no temp file left behind");

        // A failed write leaves the old file as it was and nothing beside it.
        let missing_dir = dir.path().join("nope").join("hosts");
        assert!(write_atomic(&missing_dir, "x").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[test]
    fn local_names_go_to_mdns_and_other_names_to_dns() {
        let r = Scripted::mdns(vec![Ok(vec![ip("192.168.1.20")])]);
        assert_eq!(resolve_ipv4(SERVER, &r), Ok(Ipv4Addr::new(192, 168, 1, 20)));
        assert_eq!(r.mdns_calls.load(Ordering::SeqCst), 1);
        assert_eq!(r.dns_calls.load(Ordering::SeqCst), 0);

        let r = Scripted::mdns(vec![Ok(vec![ip("192.168.1.20")])]);
        assert!(
            resolve_ipv4("PROXIMOS-1A2B3C4D.LOCAL.", &r).is_ok(),
            "case and root dot"
        );

        let r = Scripted::dns(vec![Ok(vec![ip("203.0.113.5")])]);
        assert_eq!(
            resolve_ipv4("mqtt.example.com", &r),
            Ok(Ipv4Addr::new(203, 0, 113, 5))
        );
        assert_eq!(r.mdns_calls.load(Ordering::SeqCst), 0);
        assert_eq!(r.dns_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ipv6_answers_are_never_chosen() {
        let r = Scripted::dns(vec![Ok(vec![ip("2001:db8::5"), ip("203.0.113.5")])]);
        assert_eq!(
            resolve_ipv4("mqtt.example.com", &r),
            Ok(Ipv4Addr::new(203, 0, 113, 5)),
            "the IPv4 address, though IPv6 came first"
        );
        let r = Scripted::mdns(vec![Ok(vec![ip("fd00::20")])]);
        let err = resolve_ipv4(SERVER, &r).unwrap_err();
        assert!(
            err.contains("only IPv6") && err.contains("IPv4 only"),
            "{err}"
        );
    }

    fn keeper(dir: &tempfile::TempDir, r: Arc<Scripted>) -> ServerHosts {
        ServerHosts::new(
            SERVER,
            dir.path().join("hosts"),
            dir.path().join("hosts.base"),
            Box::new(r),
        )
    }

    #[test]
    fn the_keeper_writes_on_change_only_and_keeps_the_last_good_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts");
        std::fs::write(&path, BASE).unwrap();
        let r = Arc::new(Scripted::mdns(vec![
            Ok(vec![ip("192.168.1.20")]),
            Ok(vec![ip("192.168.1.20")]),
            Err("avahi timed out".into()),
            Ok(vec![ip("fd00::20")]),
            Ok(vec![ip("192.168.1.31")]),
        ]));
        let k = keeper(&dir, r.clone());
        let v4 = |s: &str| s.parse::<Ipv4Addr>().unwrap();

        assert_eq!(k.refresh(), Refresh::Written(v4("192.168.1.20")));
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.starts_with(BASE));
        use std::os::unix::fs::MetadataExt;
        let ino = std::fs::metadata(&path).unwrap().ino();

        assert_eq!(k.refresh(), Refresh::Unchanged(v4("192.168.1.20")));
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            ino,
            "no rewrite without a change"
        );

        assert!(matches!(k.refresh(), Refresh::KeptLastGood(a, _) if a == v4("192.168.1.20")));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            first,
            "a failed lookup keeps the line"
        );

        assert!(
            matches!(k.refresh(), Refresh::KeptLastGood(a, ref w) if a == v4("192.168.1.20") && w.contains("IPv6")),
            "an IPv6-only answer is a failed lookup"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);

        assert_eq!(k.refresh(), Refresh::Written(v4("192.168.1.31")));
        let moved = std::fs::read_to_string(&path).unwrap();
        assert!(moved.starts_with(BASE), "{moved}");
        assert_eq!(managed_entry(&moved, SERVER), Some(v4("192.168.1.31")));
        let ours = moved.lines().find(|l| l.contains(MARKER)).unwrap();
        assert!(
            !ours.split('#').next().unwrap().contains(':'),
            "no IPv6 address: {ours}"
        );
        assert_eq!(
            r.dns_calls.load(Ordering::SeqCst),
            0,
            ".local never goes to DNS"
        );
    }

    /// The refresh asks the resolver, never the file: a line already in the file — the one
    /// glibc would hand back first — does not stop it seeing the Server move.
    #[test]
    fn the_refresh_does_not_read_its_own_line_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts");
        std::fs::write(
            &path,
            render_hosts(BASE, SERVER, Ipv4Addr::new(192, 168, 1, 20)),
        )
        .unwrap();
        let r = Arc::new(Scripted::mdns(vec![Ok(vec![ip("192.168.1.31")])]));
        let k = keeper(&dir, r.clone());
        assert_eq!(
            k.refresh(),
            Refresh::Written(Ipv4Addr::new(192, 168, 1, 31))
        );
        assert_eq!(r.mdns_calls.load(Ordering::SeqCst), 1);
    }

    /// An agent restart in the same boot: the file's line is the last good address, so a
    /// first lookup that fails does not leave the forwarder without one.
    #[test]
    fn a_restart_keeps_the_line_already_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts");
        let text = render_hosts(BASE, SERVER, Ipv4Addr::new(192, 168, 1, 20));
        std::fs::write(&path, &text).unwrap();
        let k = keeper(
            &dir,
            Arc::new(Scripted::mdns(vec![Err("no network yet".into())])),
        );
        assert!(
            matches!(k.refresh(), Refresh::KeptLastGood(a, _) if a == Ipv4Addr::new(192, 168, 1, 20))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        let k = keeper(
            &tempfile::tempdir().unwrap(),
            Arc::new(Scripted::mdns(vec![Err("x".into())])),
        );
        assert!(matches!(k.refresh(), Refresh::NoAddress(_)));
    }

    /// A missing runtime file (tmpfiles did not run) is rebuilt from the image's base, so
    /// writing our line never produces a hosts file without localhost.
    #[test]
    fn a_missing_file_starts_from_the_base() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hosts.base"), BASE).unwrap();
        let k = keeper(
            &dir,
            Arc::new(Scripted::mdns(vec![Ok(vec![ip("192.168.1.20")])])),
        );
        assert!(matches!(k.refresh(), Refresh::Written(_)));
        let text = std::fs::read_to_string(dir.path().join("hosts")).unwrap();
        assert!(text.starts_with(BASE), "{text}");
    }

    #[test]
    fn the_reconnect_path_is_throttled() {
        let dir = tempfile::tempdir().unwrap();
        let r = Arc::new(Scripted::mdns(vec![
            Ok(vec![ip("192.168.1.20")]),
            Ok(vec![ip("192.168.1.20")]),
        ]));
        let k = keeper(&dir, r.clone());
        assert!(k.refresh_if_older_than(Duration::from_secs(60)).is_some());
        assert!(k.refresh_if_older_than(Duration::from_secs(60)).is_none());
        assert!(k.refresh_if_older_than(Duration::ZERO).is_some());
        assert_eq!(r.mdns_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn nameservers_are_read_in_order_and_scoped_ones_skipped() {
        let conf = "# Generated by NetworkManager\nsearch lan\nnameserver 192.168.1.1\n\
                    nameserver fe80::1%eth0\nnameserver 2001:db8::53\noptions edns0\n";
        assert_eq!(
            nameservers(conf),
            vec![ip("192.168.1.1"), ip("2001:db8::53")]
        );
        assert!(nameservers("search lan\n").is_empty());
    }

    #[test]
    fn a_query_is_a_standard_a_question() {
        let q = build_a_query(0xbeef, "mqtt.example.com.").unwrap();
        assert_eq!(&q[..12], &[0xbe, 0xef, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&q[12..], b"\x04mqtt\x07example\x03com\x00\x00\x01\x00\x01");
        assert!(build_a_query(1, "").is_err());
        assert!(build_a_query(1, "a..b").is_err());
        assert!(build_a_query(1, &"x".repeat(64)).is_err());
    }

    /// A response with a CNAME in front of two A records, the A owner names compressed.
    #[test]
    fn answers_parse_through_a_cname_and_compression() {
        let mut m = vec![0xbe, 0xef, 0x81, 0x80, 0, 1, 0, 3, 0, 0, 0, 0];
        m.extend_from_slice(b"\x04mqtt\x07example\x03com\x00\x00\x01\x00\x01");
        // CNAME mqtt.example.com -> b.example.com (b + pointer to "example.com" at 17)
        m.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 4, 1, b'b', 0xc0, 17]);
        let b_at = (m.len() - 4) as u8;
        m.extend_from_slice(&[0xc0, b_at, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 203, 0, 113, 5]);
        m.extend_from_slice(&[0xc0, b_at, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 203, 0, 113, 6]);
        assert_eq!(
            parse_a_answers(0xbeef, &m),
            Ok(vec![
                Ipv4Addr::new(203, 0, 113, 5),
                Ipv4Addr::new(203, 0, 113, 6)
            ])
        );
        assert_eq!(parse_a_answers(0x1234, &m), Err(WRONG_ID.to_string()));
        assert!(
            parse_a_answers(0xbeef, &m[..m.len() - 3]).is_err(),
            "truncated"
        );
        let mut nx = m[..12].to_vec();
        nx[3] = 0x83;
        assert!(parse_a_answers(0xbeef, &nx)
            .unwrap_err()
            .contains("NXDOMAIN"));
    }

    /// The real avahi over the real system bus: this machine's own `<hostname>.local`, A
    /// only. Needs a running avahi-daemon, so it is not part of the default run:
    /// `cargo test -p control-core --lib -- --ignored live_avahi`.
    #[test]
    #[ignore = "needs avahi-daemon on the system D-Bus"]
    fn live_avahi_resolves_this_host_to_ipv4() {
        let name = format!(
            "{}.local",
            std::fs::read_to_string("/etc/hostname").unwrap().trim()
        );
        let got = SystemResolver::default()
            .mdns(&name)
            .expect("avahi answers");
        assert!(
            !got.is_empty() && got.iter().all(|a| a.is_ipv4()),
            "{name}: {got:?}"
        );
    }

    /// The UDP path end to end against a stub nameserver on loopback.
    #[test]
    fn a_dns_query_round_trips_over_udp() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, peer) = server.recv_from(&mut buf).unwrap();
            let mut resp = buf[..n].to_vec();
            resp[2] = 0x81;
            resp[3] = 0x80;
            resp[7] = 1; // ANCOUNT
            resp.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 1, 2, 3]);
            server.send_to(&resp, peer).unwrap();
        });
        assert_eq!(
            query_a(addr, "mqtt.example.com"),
            Ok(vec![Ipv4Addr::new(10, 1, 2, 3)])
        );
        t.join().unwrap();
    }
}
