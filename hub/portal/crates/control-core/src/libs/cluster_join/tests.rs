//! The join against a real HTTPS server and a real TLS broker stand-in on 127.0.0.1, with a test
//! CA whose leaf names the Server the way C0 issues it (DNS SANs only), a scripted resolver, and
//! every path in a temp directory.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::*;
use crate::libs::storage::{db::Database, StorageHandle, StorageThread};

const SERVER: &str = "proximos-1a2b3c4d.local";
const OWN: &str = "proximos-5e6f7a8b";
const USER: &str = "peer-proximos-5e6f7a8b";
const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eA";
const CONFIG: &str = "system:\n  device_label: bench\n";
const HOSTS_BASE: &str = "127.0.0.1\tlocalhost\n127.0.1.1\tproximos-5e6f7a8b\n";

fn serial() -> std::sync::MutexGuard<'static, ()> {
    super::test_serial()
}

// ---------------------------------------------------------------------------------------------
// A test CA and two servers
// ---------------------------------------------------------------------------------------------

/// A CA and a leaf for `san`, signed by it, as PEM: (ca, leaf cert, leaf key). The shape C0
/// issues: P-256, a DNS SAN and no IP SAN. (The helper `mqtt::monitor`'s tests use, again.)
fn ca_and_leaf(san: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::pkey::PKey;
    use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
    use openssl::x509::{X509NameBuilder, X509};

    let key = || {
        let g = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        PKey::from_ec_key(EcKey::generate(&g).unwrap()).unwrap()
    };
    let name = |cn: &str| {
        let mut b = X509NameBuilder::new().unwrap();
        b.append_entry_by_text("CN", cn).unwrap();
        b.build()
    };
    let base = |serial: u32| {
        let mut b = X509::builder().unwrap();
        b.set_version(2).unwrap();
        b.set_serial_number(&BigNum::from_u32(serial).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        b.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        b.set_not_after(&Asn1Time::days_from_now(30).unwrap())
            .unwrap();
        b
    };

    let ca_key = key();
    let mut ca = base(1);
    ca.set_subject_name(&name("ProXimos Hub CA test")).unwrap();
    ca.set_issuer_name(&name("ProXimos Hub CA test")).unwrap();
    ca.set_pubkey(&ca_key).unwrap();
    ca.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    ca.sign(&ca_key, MessageDigest::sha256()).unwrap();
    let ca = ca.build();

    let leaf_key = key();
    let mut leaf = base(2);
    leaf.set_subject_name(&name(san)).unwrap();
    leaf.set_issuer_name(ca.subject_name()).unwrap();
    leaf.set_pubkey(&leaf_key).unwrap();
    let ctx = leaf.x509v3_context(Some(&ca), None);
    let ext = SubjectAlternativeName::new().dns(san).build(&ctx).unwrap();
    leaf.append_extension(ext).unwrap();
    leaf.sign(&ca_key, MessageDigest::sha256()).unwrap();
    let leaf = leaf.build();

    (
        ca.to_pem().unwrap(),
        leaf.to_pem().unwrap(),
        leaf_key.private_key_to_pem_pkcs8().unwrap(),
    )
}

fn acceptor(leaf: &[u8], key: &[u8]) -> openssl::ssl::SslAcceptor {
    use openssl::pkey::PKey;
    use openssl::ssl::{SslAcceptor, SslMethod};
    use openssl::x509::X509;
    let mut b = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).unwrap();
    b.set_certificate(&X509::from_pem(leaf).unwrap()).unwrap();
    b.set_private_key(&PKey::private_key_from_pem(key).unwrap())
        .unwrap();
    b.build()
}

/// `GET /connection/ca` over TLS, answering `status` with `body`. Records each request head.
fn https_server(
    leaf: &[u8],
    key: &[u8],
    status: &'static str,
    body: Vec<u8>,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let acceptor = acceptor(leaf, key);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(tcp) = tcp else { return };
            let Ok(mut s) = acceptor.accept(tcp) else {
                continue;
            };
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&head).into_owned());
            let mut out = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/x-pem-file\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            out.extend_from_slice(&body);
            let _ = s.write_all(&out);
            let _ = s.shutdown();
        }
    });
    (port, seen)
}

/// A broker stand-in: reads one CONNECT, records it, answers CONNACK with `rc`.
fn tls_broker(leaf: &[u8], key: &[u8], rc: u8) -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let acceptor = acceptor(leaf, key);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(tcp) = tcp else { return };
            let Ok(mut s) = acceptor.accept(tcp) else {
                continue;
            };
            let mut buf = [0u8; 512];
            let n = s.read(&mut buf).unwrap_or(0);
            log.lock().unwrap().push(buf[..n].to_vec());
            if n > 0 {
                let _ = s.write_all(&[0x20, 0x02, 0x00, rc]);
            }
            let _ = s.read(&mut buf);
        }
    });
    (port, seen)
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Answers every lookup with `answer`, and counts them.
struct Resolver {
    answer: Result<Vec<IpAddr>, String>,
    calls: AtomicUsize,
}

impl ServerResolver for Resolver {
    fn mdns(&self, _: &str) -> Result<Vec<IpAddr>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answer.clone()
    }
    fn dns(&self, _: &str) -> Result<Vec<IpAddr>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answer.clone()
    }
}

fn loopback() -> Resolver {
    Resolver {
        answer: Ok(vec!["127.0.0.1".parse().unwrap()]),
        calls: AtomicUsize::new(0),
    }
}

// ---------------------------------------------------------------------------------------------
// The Hub
// ---------------------------------------------------------------------------------------------

/// A Hub out of the box, with an audit database, a Server that serves `served` as its CA and
/// presents a leaf for `san`, and a broker answering `rc`.
struct Bench {
    dir: tempfile::TempDir,
    applier: ConfigApplier,
    storage: StorageHandle,
    db_path: String,
    ca: Vec<u8>,
    https_port: u16,
    mqtt_port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    connects: Arc<Mutex<Vec<Vec<u8>>>>,
    reboots: AtomicUsize,
}

impl Bench {
    fn new() -> Bench {
        Bench::with(SERVER, None, "200 OK", 0)
    }

    /// `served`: the CA bytes the HTTPS server sends, when not the broker's own CA.
    fn with(san: &str, served: Option<Vec<u8>>, status: &'static str, rc: u8) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fiber.config.yaml"), CONFIG).unwrap();
        std::fs::create_dir_all(dir.path().join("net")).unwrap();
        std::fs::write(dir.path().join("net/hosts.base"), HOSTS_BASE).unwrap();
        std::fs::create_dir_all(dir.path().join("db")).unwrap();
        let db_path = dir
            .path()
            .join("db/audit.db")
            .to_string_lossy()
            .into_owned();
        let (storage, _join) = StorageThread::spawn(&db_path, 1).unwrap();
        let applier = ConfigApplier::new_with_storage(dir.path(), Some(storage.clone()))
            .unwrap()
            .with_cluster_dir(dir.path().join("cluster"));
        let (ca, leaf, key) = ca_and_leaf(san);
        let (https_port, requests) =
            https_server(&leaf, &key, status, served.unwrap_or_else(|| ca.clone()));
        let (mqtt_port, connects) = tls_broker(&leaf, &key, rc);
        Bench {
            dir,
            applier,
            storage,
            db_path,
            ca,
            https_port,
            mqtt_port,
            requests,
            connects,
            reboots: AtomicUsize::new(0),
        }
    }

    fn bundle(&self) -> Bundle {
        Bundle {
            v: 1,
            h: SERVER.to_string(),
            p: Secret::new(PASSWORD),
            f: cluster_state::bundle_fingerprint(&self.ca),
        }
    }

    fn join_with(
        &self,
        bundle: &Bundle,
        resolver: &dyn ServerResolver,
    ) -> Result<Joined, JoinError> {
        let reboot = || {
            self.reboots.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let env = JoinEnv {
            applier: &self.applier,
            cluster: ClusterDir::new(self.dir.path().join("cluster")),
            hosts_path: self.dir.path().join("net/hosts"),
            hosts_base_path: self.dir.path().join("net/hosts.base"),
            resolver,
            https_port: self.https_port,
            mqtt_port: self.mqtt_port,
            own_hostname: OWN.to_string(),
            reboot: &reboot,
        };
        join(bundle, "cli", &env)
    }

    fn join(&self, bundle: &Bundle) -> Result<Joined, JoinError> {
        self.join_with(bundle, &loopback())
    }

    fn cluster(&self) -> ClusterDir {
        ClusterDir::new(self.dir.path().join("cluster"))
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("fiber.config.yaml")).unwrap()
    }

    fn hosts(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join("net/hosts")).ok()
    }

    /// Every audit row, `(operation, details)`, durable.
    fn audit(&self) -> Vec<(String, String)> {
        self.storage
            .flush_sync(Duration::from_secs(5))
            .expect("flush");
        let conn = Database::new(&self.db_path, 1).unwrap().connect().unwrap();
        let mut stmt = conn
            .prepare("SELECT operation, COALESCE(details, '') FROM audit_log ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    }

    /// The refusal left nothing behind: no cluster file, no setup, no hosts line, no reboot, no
    /// join audit row.
    fn assert_untouched(&self, why: &str) {
        let cluster = self.dir.path().join("cluster");
        let files: Vec<_> = std::fs::read_dir(&cluster)
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(files.is_empty(), "{why}: cluster files left: {files:?}");
        assert_eq!(self.config(), CONFIG, "{why}: setup was written");
        if let Some(hosts) = self.hosts() {
            assert!(
                !hosts.contains(server_hosts::MARKER),
                "{why}: the hosts line stayed: {hosts}"
            );
        }
        assert_eq!(self.reboots.load(Ordering::SeqCst), 0, "{why}: rebooted");
        assert!(
            !self.audit().iter().any(|(op, _)| op == AUDIT_JOIN),
            "{why}: a join audit row"
        );
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = self.storage.shutdown();
    }
}

fn assert_no_password(what: &str, text: &str) {
    assert!(
        !text.contains(PASSWORD),
        "{what} carries the password: {text}"
    );
}

// ---------------------------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------------------------

#[test]
fn a_join_writes_the_connection_then_the_marker_then_setup_and_reboots() {
    let _s = serial();
    let b = Bench::new();
    let joined = b.join(&b.bundle()).expect("joined");

    assert_eq!(joined.server_host, SERVER);
    assert_eq!(joined.message(), format!("joined {SERVER}; restarting"));
    assert_eq!(
        joined.written,
        vec![
            "server_host",
            "server_port",
            "peer_username",
            "peer_password",
            "ca.crt",
            "role",
            "setup"
        ],
        "the marker before setup, setup last (decided 2026-10-02)"
    );

    let c = b.cluster();
    assert!(c.is_client().unwrap());
    assert_eq!(c.server_host().as_deref(), Some(SERVER));
    assert_eq!(
        std::fs::read_to_string(c.path("server_port")).unwrap(),
        b.mqtt_port.to_string()
    );
    assert_eq!(c.value("peer_username").as_deref(), Some(USER));
    assert_eq!(
        std::fs::read_to_string(c.path("peer_password")).unwrap(),
        PASSWORD
    );
    assert_eq!(
        std::fs::read(c.path("ca.crt")).unwrap(),
        b.ca,
        "the CA exactly as served"
    );

    let doc: serde_yaml::Value = serde_yaml::from_str(&b.config()).unwrap();
    assert_eq!(doc["setup"]["provisioned"], true);
    assert_eq!(doc["setup"]["role"], "client");
    assert_eq!(
        doc["system"]["device_label"], "bench",
        "the rest rides through"
    );

    let hosts = b.hosts().expect("the hosts line");
    assert!(hosts.starts_with(HOSTS_BASE), "{hosts}");
    assert_eq!(
        server_hosts::managed_entry(&hosts, SERVER),
        Some(Ipv4Addr::LOCALHOST)
    );
    assert_eq!(b.reboots.load(Ordering::SeqCst), 1);

    // What reached the Server: the CA route by name, and one CONNECT as this Hub's account.
    let requests = b.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("GET /connection/ca HTTP/1.0\r\n"),
        "{}",
        requests[0]
    );
    assert!(requests[0].contains(&format!("Host: {SERVER}\r\n")));
    let connects = b.connects.lock().unwrap().clone();
    assert_eq!(connects.len(), 1);
    let connect = &connects[0];
    assert_eq!(connect[0], 0x10, "CONNECT");
    let has = |needle: &[u8]| connect.windows(needle.len()).any(|w| w == needle);
    assert!(has(USER.as_bytes()));
    assert!(has(PASSWORD.as_bytes()), "the password goes to the broker");

    // One audit row, with the origin and the Server, without the password.
    let rows: Vec<_> = b
        .audit()
        .into_iter()
        .filter(|(op, _)| op == AUDIT_JOIN)
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let details: serde_json::Value = serde_json::from_str(&rows[0].1).unwrap();
    assert_eq!(details["origin"], "cli");
    assert_eq!(details["server_host"], SERVER);
    assert_eq!(details["peer_username"], USER);
    for (op, d) in b.audit() {
        assert_no_password(&format!("audit row {op}"), &d);
    }

    // FB1C's state after it.
    assert_eq!(
        status(&b.applier, &c).unwrap(),
        Status {
            client: true,
            joinable: false,
            server: Some(SERVER.to_string())
        }
    );
}

/// The files the join writes are the ones `proximos-render` renders a Client from, without a
/// degraded output: the round trip through the renderer's own reader.
#[test]
fn the_files_written_render_a_client_without_degradation() {
    let _s = serial();
    let b = Bench::new();
    b.join(&b.bundle()).expect("joined");

    let data = b.dir.path().join("data");
    std::fs::create_dir_all(data.join("proximos")).unwrap();
    std::fs::rename(b.dir.path().join("cluster"), data.join("proximos/cluster")).unwrap();
    std::fs::create_dir_all(data.join("chirpstack")).unwrap();
    std::fs::write(
        data.join("chirpstack/api-secret"),
        "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
    )
    .unwrap();
    let run = b.dir.path().join("run/proximos");
    std::fs::create_dir_all(&run).unwrap();
    let paths = proximos_render::RenderPaths {
        templates: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/templates"),
        data,
        run: run.clone(),
        control_config: None,
    };
    let report =
        proximos_render::render_all(&paths, proximos_render::Mode::Lorawan).expect("renders");
    assert!(
        report.degraded.is_empty(),
        "the join's files must not degrade the render: {:?}",
        report.degraded
    );
    let control = std::fs::read_to_string(run.join("control/config.yaml")).unwrap();
    assert!(control.contains(SERVER), "{control}");
    assert!(control.contains(USER), "{control}");
    assert_no_password("the control config", &control);
    let forwarder = std::fs::read_to_string(
        run.join("chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml"),
    )
    .unwrap();
    assert!(
        forwarder.contains(&format!("ssl://{SERVER}:{}", b.mqtt_port)),
        "{forwarder}"
    );
    assert!(
        !run.join("portal/config.yaml").exists(),
        "a Client serves no portal"
    );
}

// ---------------------------------------------------------------------------------------------
// Refusals, each leaving nothing written
// ---------------------------------------------------------------------------------------------

#[test]
fn a_hub_whose_setup_is_complete_is_refused() {
    let _s = serial();
    let b = Bench::new();
    let provisioned = "setup:\n  provisioned: true\n  role: server\n";
    std::fs::write(b.dir.path().join("fiber.config.yaml"), provisioned).unwrap();
    let e = b.join(&b.bundle()).expect_err("refused");
    assert_eq!(e.kind, FailKind::NotJoinable);
    assert!(e.message.contains("already complete"), "{}", e.message);
    assert!(e.message.contains("factory reset"), "{}", e.message);
    assert_eq!(b.config(), provisioned);
    assert!(b.hosts().is_none(), "refused before any lookup");
    assert!(b.requests.lock().unwrap().is_empty());
}

#[test]
fn a_hub_with_a_role_marker_is_refused() {
    let _s = serial();
    for marker in ["client", "server"] {
        let b = Bench::new();
        std::fs::create_dir_all(b.dir.path().join("cluster")).unwrap();
        std::fs::write(b.dir.path().join("cluster/role"), marker).unwrap();
        let e = b.join(&b.bundle()).expect_err("refused");
        assert_eq!(e.kind, FailKind::NotJoinable, "{marker}");
        assert_eq!(b.config(), CONFIG);
        assert_eq!(
            std::fs::read_to_string(b.dir.path().join("cluster/role")).unwrap(),
            marker
        );
        assert!(b.requests.lock().unwrap().is_empty());
    }
}

/// The join does not look at this Hub's radio mode (decision #47): any mode, `off` included,
/// and even one that does not parse, joins. The Server converges the mode afterwards.
#[test]
fn the_join_ignores_this_hubs_radio_mode() {
    let _s = serial();
    for mode in ["off", "lorawan", "p2p", "dual", "banana"] {
        let b = Bench::new();
        let config = format!("{CONFIG}radio:\n  mode: {mode}\n");
        std::fs::write(b.dir.path().join("fiber.config.yaml"), &config).unwrap();
        b.join(&b.bundle())
            .unwrap_or_else(|e| panic!("{mode}: {}", e.message));
        assert!(b.cluster().is_client().unwrap(), "{mode}");
    }
}

#[test]
fn a_bundle_that_does_not_validate_is_refused_before_any_lookup() {
    let _s = serial();
    let b = Bench::new();
    let good = b.bundle();
    let cases: Vec<(&str, Bundle)> = vec![
        (
            "version 2",
            Bundle {
                v: 2,
                ..good.clone()
            },
        ),
        (
            "short password",
            Bundle {
                p: Secret::new("Zm9vYmFy"),
                ..good.clone()
            },
        ),
        (
            "password not base64url",
            Bundle {
                p: Secret::new("Zm9vYmFyYmF6cXV4cXV1e+"),
                ..good.clone()
            },
        ),
        (
            "short fingerprint",
            Bundle {
                f: "abc".into(),
                ..good.clone()
            },
        ),
        (
            "fingerprint not base64url",
            Bundle {
                f: format!("{}=", &good.f[..42]),
                ..good.clone()
            },
        ),
        (
            "empty host",
            Bundle {
                h: " ".into(),
                ..good.clone()
            },
        ),
        (
            "host with a space",
            Bundle {
                h: "proximos 1a2b.local".into(),
                ..good.clone()
            },
        ),
        (
            "host with a quote",
            Bundle {
                h: "proximos\"x.local".into(),
                ..good.clone()
            },
        ),
        (
            "host too long",
            Bundle {
                h: format!("{}.local", "a".repeat(60)),
                ..good.clone()
            },
        ),
        (
            "this Hub",
            Bundle {
                h: format!("{OWN}.local"),
                ..good.clone()
            },
        ),
    ];
    let resolver = loopback();
    for (why, bundle) in cases {
        let e = b.join_with(&bundle, &resolver).expect_err(why);
        assert_eq!(e.kind, FailKind::Invalid, "{why}: {}", e.message);
        assert_no_password(why, &e.message);
        b.assert_untouched(why);
    }
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        0,
        "nothing was looked up"
    );
    assert!(b.requests.lock().unwrap().is_empty(), "nothing was dialled");
}

/// cluster.md §10 Q2 is open: until it is decided, `h` is a name. Both TLS clients verify DNS
/// SANs only, and the listener is IPv4 only.
#[test]
fn an_ip_literal_is_refused() {
    let _s = serial();
    let b = Bench::new();
    for h in [
        "192.168.1.20",
        "127.0.0.1",
        "fd00::1",
        "[fd00::1]",
        "::ffff:10.0.0.1",
        "10.1.2",
    ] {
        let bundle = Bundle {
            h: h.into(),
            ..b.bundle()
        };
        let e = b.join(&bundle).expect_err(h);
        assert_eq!(e.kind, FailKind::Invalid, "{h}");
        assert!(e.message.contains("hostname"), "{h}: {}", e.message);
        b.assert_untouched(h);
    }
    // And the validator on its own, the ones that pass.
    assert_eq!(
        validate_server_host(" Proximos-1A2B3C4D.local. ").unwrap(),
        SERVER
    );
    validate_server_host("server-1.site.example").unwrap();
}

#[test]
fn a_name_that_does_not_resolve_to_ipv4_is_refused() {
    let _s = serial();
    let b = Bench::new();
    for answer in [
        Err("avahi cannot resolve it".to_string()),
        Ok(vec!["fd00::1".parse().unwrap()]),
        Ok(vec![]),
    ] {
        let r = Resolver {
            answer,
            calls: AtomicUsize::new(0),
        };
        let e = b.join_with(&b.bundle(), &r).expect_err("refused");
        assert_eq!(e.kind, FailKind::Unreachable, "{}", e.message);
        assert!(e.message.contains("cannot be reached"), "{}", e.message);
        b.assert_untouched("unresolved");
    }
}

#[test]
fn a_server_that_does_not_answer_is_refused_and_the_hosts_line_removed() {
    let _s = serial();
    let mut b = Bench::new();
    b.https_port = closed_port();
    let e = b.join(&b.bundle()).expect_err("refused");
    assert_eq!(e.kind, FailKind::Unreachable, "{}", e.message);
    assert!(b.hosts().is_some(), "step 3 wrote the hosts file");
    b.assert_untouched("no HTTPS");
}

/// The bundle's `f` against a CA that hashes differently: a wrong QR code, or another machine.
#[test]
fn a_ca_that_does_not_match_the_fingerprint_is_refused() {
    let _s = serial();
    let b = Bench::new();
    let other = ca_and_leaf(SERVER).0;
    let wrong = Bundle {
        f: cluster_state::bundle_fingerprint(&other),
        ..b.bundle()
    };
    let e = b.join(&wrong).expect_err("refused");
    assert_eq!(e.kind, FailKind::CaMismatch);
    assert!(e.message.contains("does not match"), "{}", e.message);
    assert!(
        b.connects.lock().unwrap().is_empty(),
        "never dialled the broker with the password"
    );
    b.assert_untouched("CA mismatch");
}

/// Hashed exactly as received: the same certificate with CRLF line ends is not the same bytes.
#[test]
fn the_fingerprint_is_over_the_bytes_as_served_not_a_re_encoding() {
    let _s = serial();
    let (ca, _, _) = ca_and_leaf(SERVER);
    let crlf = String::from_utf8(ca.clone()).unwrap().replace('\n', "\r\n");
    let b = Bench::with(SERVER, Some(crlf.into_bytes()), "200 OK", 0);
    // `f` of the LF form — what a Server hashing re-encoded bytes would show.
    let lf = Bundle {
        f: cluster_state::bundle_fingerprint(&ca),
        ..b.bundle()
    };
    let e = b.join(&lf).expect_err("refused");
    assert_eq!(e.kind, FailKind::CaMismatch);
    b.assert_untouched("re-encoded");
}

#[test]
fn a_server_without_a_ca_is_refused() {
    let _s = serial();
    let b = Bench::with(SERVER, Some(b"not found".to_vec()), "404 Not Found", 0);
    let e = b.join(&b.bundle()).expect_err("refused");
    assert!(e.message.contains("404"), "{}", e.message);
    b.assert_untouched("404");

    let b = Bench::with(SERVER, Some(b"hello".to_vec()), "200 OK", 0);
    let bundle = Bundle {
        f: cluster_state::bundle_fingerprint(b"hello"),
        ..b.bundle()
    };
    let e = b.join(&bundle).expect_err("refused");
    assert_eq!(e.kind, FailKind::CaMismatch);
    assert!(e.message.contains("not a PEM certificate"), "{}", e.message);
    b.assert_untouched("not PEM");
}

/// The broker's leaf names another Hub: its CA matches `f`, but it does not prove it is `h`.
#[test]
fn a_broker_certificate_for_another_name_is_refused() {
    let _s = serial();
    let b = Bench::with("proximos-99999999.local", None, "200 OK", 0);
    let e = b.join(&b.bundle()).expect_err("refused");
    assert_eq!(e.kind, FailKind::CaMismatch, "{}", e.message);
    assert!(e.message.contains("does not prove"), "{}", e.message);
    assert!(b.connects.lock().unwrap().is_empty(), "no CONNECT sent");
    b.assert_untouched("wrong SAN");
}

#[test]
fn a_broker_that_refuses_the_account_is_refused_with_its_reason() {
    let _s = serial();
    for (rc, reason) in [(5u8, "not authorized"), (4, "bad user name or password")] {
        let b = Bench::with(SERVER, None, "200 OK", rc);
        let e = b.join(&b.bundle()).expect_err("refused");
        assert_eq!(e.kind, FailKind::BrokerRefused);
        assert!(e.message.contains(reason), "{}", e.message);
        assert!(e.message.contains(USER), "{}", e.message);
        assert_no_password("the refusal", &e.message);
        b.assert_untouched("broker refusal");
    }
}

#[test]
fn a_broker_that_does_not_listen_is_refused() {
    let _s = serial();
    let mut b = Bench::new();
    b.mqtt_port = closed_port();
    let e = b.join(&b.bundle()).expect_err("refused");
    assert_eq!(e.kind, FailKind::Unreachable);
    b.assert_untouched("no broker");
}

/// A setup write that fails — the last write, after the marker — takes the marker back out,
/// then the connection files, and the hosts line.
#[test]
fn a_failed_setup_write_leaves_no_connection_files() {
    use std::os::unix::fs::PermissionsExt as _;
    let _s = serial();
    let b = Bench::new();
    // The cluster directory exists and is writable; the config directory is not, so the
    // applier cannot stage its temp file. Step 1 still reads the config.
    std::fs::create_dir_all(b.dir.path().join("cluster")).unwrap();
    std::fs::set_permissions(b.dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = b.join(&b.bundle());
    std::fs::set_permissions(b.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        // Root ignores directory modes, so the write went through; nothing to assert here.
        return;
    }
    let e = result.expect_err("refused");
    assert_eq!(e.kind, FailKind::Storage, "{}", e.message);
    b.assert_untouched("setup write failed");
}

/// Two joins at once: the second is `Busy`, and writes nothing.
#[test]
fn a_second_join_while_one_runs_is_busy() {
    let _s = serial();
    let b = Bench::new();
    let _held = JOINING.lock().unwrap_or_else(|p| p.into_inner());
    let e = b.join(&b.bundle()).expect_err("busy");
    assert_eq!(e.kind, FailKind::Busy);
    b.assert_untouched("busy");
}

#[test]
fn a_joined_hub_refuses_a_second_join() {
    let _s = serial();
    let b = Bench::new();
    b.join(&b.bundle()).expect("first");
    let e = b.join(&b.bundle()).expect_err("second");
    assert_eq!(e.kind, FailKind::NotJoinable);
    assert!(
        e.message.contains("already complete") || e.message.contains("already joined"),
        "{}",
        e.message
    );
    assert_eq!(
        b.reboots.load(Ordering::SeqCst),
        1,
        "one reboot, for the first"
    );
}

/// A crash between the marker and the setup write (step 6) leaves a Hub that is marked but not
/// provisioned. It boots as a Client — the renderer reads the marker and never `setup:` — a
/// retried join is refused because the marker exists, and the agent's start completes the
/// missing setup write, once, with its audit row.
#[test]
fn a_join_interrupted_before_setup_is_completed_at_start() {
    let _s = serial();
    let b = Bench::new();
    b.join(&b.bundle()).expect("joined");
    // The crash: everything but the setup write landed.
    std::fs::write(b.dir.path().join("fiber.config.yaml"), CONFIG).unwrap();
    assert!(b.cluster().is_client().unwrap());

    let e = b.join(&b.bundle()).expect_err("a retried join");
    assert_eq!(e.kind, FailKind::NotJoinable);
    assert!(e.message.contains("already joined"), "{}", e.message);
    assert_eq!(b.config(), CONFIG, "the refusal writes nothing");
    assert_eq!(
        status(&b.applier, &b.cluster()).unwrap(),
        Status {
            client: true,
            joinable: false,
            server: Some(SERVER.to_string())
        }
    );

    assert_eq!(complete_interrupted_join(&b.applier), Ok(true));
    let doc: serde_yaml::Value = serde_yaml::from_str(&b.config()).unwrap();
    assert_eq!(doc["setup"]["provisioned"], true);
    assert_eq!(doc["setup"]["role"], "client");
    assert_eq!(doc["system"]["device_label"], "bench");
    let rows: Vec<_> = b
        .audit()
        .into_iter()
        .filter(|(op, _)| op == AUDIT_JOIN)
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "the join's row and the completion's: {rows:?}"
    );
    let details: serde_json::Value = serde_json::from_str(&rows[1].1).unwrap();
    assert_eq!(details["origin"], ORIGIN_RESUMED);
    assert_eq!(details["server_host"], SERVER);
    assert_eq!(details["peer_username"], USER);
    for (op, d) in b.audit() {
        assert_no_password(&format!("audit row {op}"), &d);
    }

    // Done once: the next start finds nothing to finish.
    let before = b.config();
    assert_eq!(complete_interrupted_join(&b.applier), Ok(false));
    assert_eq!(b.config(), before);
}

/// The start-time completion touches nothing but an interrupted join: a fresh Hub, a Server
/// set up as one, a Client whose join completed, and a marker that does not say `client`.
#[test]
fn completing_at_start_leaves_every_other_hub_alone() {
    let _s = serial();
    for (config, marker) in [
        (CONFIG, None),
        ("setup:\n  provisioned: true\n  role: server\n", None),
        (
            "setup:\n  provisioned: true\n  role: client\n",
            Some("client"),
        ),
        (CONFIG, Some("server")),
    ] {
        let b = Bench::new();
        std::fs::write(b.dir.path().join("fiber.config.yaml"), config).unwrap();
        if let Some(m) = marker {
            std::fs::create_dir_all(b.dir.path().join("cluster")).unwrap();
            std::fs::write(b.dir.path().join("cluster/role"), m).unwrap();
        }
        assert_eq!(
            complete_interrupted_join(&b.applier),
            Ok(false),
            "{config:?} {marker:?}"
        );
        assert_eq!(b.config(), config, "{config:?} {marker:?}");
    }
}

/// After a join, FB17 `client` is the idempotent same-role write, and `server` is refused: the
/// setup lock and the join agree.
#[test]
fn after_a_join_the_setup_lock_accepts_client_only() {
    let _s = serial();
    let b = Bench::new();
    b.join(&b.bundle()).expect("joined");
    let again = b
        .applier
        .apply_setup_complete(crate::libs::config::SetupRole::Client);
    assert!(again.success, "{:?}", again.error_message);
    let refused = b
        .applier
        .apply_setup_complete(crate::libs::config::SetupRole::Server);
    assert!(!refused.success);
}

// ---------------------------------------------------------------------------------------------
// Smaller pieces
// ---------------------------------------------------------------------------------------------

#[test]
fn the_bundle_parses_strictly_and_never_echoes_the_password() {
    let json = format!(
        r#"{{"v":1,"h":"{SERVER}","p":"{PASSWORD}","f":"{}"}}"#,
        "a".repeat(43)
    );
    let b = Bundle::parse(json.as_bytes()).unwrap();
    assert_eq!(b.h, SERVER);
    assert_eq!(b.p.expose(), PASSWORD);
    assert_no_password("Debug", &format!("{b:?}"));

    let extra = format!(r#"{{"v":1,"h":"{SERVER}","p":"{PASSWORD}","f":"x","u":"peer-x"}}"#);
    assert_no_password("parse error", &Bundle::parse(extra.as_bytes()).unwrap_err());
    let missing = format!(r#"{{"v":1,"h":"{SERVER}","p":"{PASSWORD}"}}"#);
    assert_no_password(
        "parse error",
        &Bundle::parse(missing.as_bytes()).unwrap_err(),
    );
    let wrong_type = format!(r#"{{"v":"1","h":"{SERVER}","p":"{PASSWORD}","f":"x"}}"#);
    assert_no_password(
        "parse error",
        &Bundle::parse(wrong_type.as_bytes()).unwrap_err(),
    );
    assert!(Bundle::parse(b"not json").is_err());
}

#[test]
fn the_username_is_derived_from_this_hubs_own_hostname() {
    let bundle = Bundle {
        v: 1,
        h: SERVER.into(),
        p: Secret::new(PASSWORD),
        f: cluster_state::bundle_fingerprint(b"a CA"),
    };
    let v = validate(&bundle, "Proximos-5E6F7A8B").unwrap();
    assert_eq!(v.username, USER, "decision #21: lowercase");
    assert_no_password("Validated's Debug", &format!("{v:?}"));
    let e = validate(&bundle, "raspberrypi4-64").unwrap_err();
    assert_eq!(e.kind, FailKind::Invalid);
    assert!(e.message.contains("own hostname"), "{}", e.message);
}

#[test]
fn the_connect_packet_is_mqtt_3_1_1_with_credentials() {
    let p = connect_packet("c", "u", "pw");
    assert_eq!(
        p,
        vec![
            0x10, 20, 0, 4, b'M', b'Q', b'T', b'T', 4, 0xC2, 0, 30, 0, 1, b'c', 0, 1, b'u', 0, 2,
            b'p', b'w'
        ]
    );
    // A remaining length past 127 takes two bytes.
    let long = connect_packet(&"c".repeat(200), "u", "pw");
    assert_eq!(&long[1..3], &[0xDB, 0x01], "219 = 0x5B + 1 * 128");
}

#[test]
fn the_http_body_is_returned_exactly_and_odd_answers_are_refused() {
    let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab\r\ncTRAILING";
    assert_eq!(http_body(ok).unwrap(), b"ab\r\nc");
    assert!(http_complete(ok));
    assert!(!http_complete(
        b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab"
    ));
    assert!(http_body(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab").is_err());
    assert_eq!(
        http_body(b"HTTP/1.0 200 OK\r\n\r\nto the end").unwrap(),
        b"to the end"
    );
    assert!(http_body(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab").is_err());
    assert!(http_body(b"HTTP/1.1 302 Found\r\nLocation: /setup\r\n\r\n")
        .unwrap_err()
        .contains("302"));
    assert!(http_body(b"SSH-2.0-OpenSSH\r\n\r\n").is_err());
}
