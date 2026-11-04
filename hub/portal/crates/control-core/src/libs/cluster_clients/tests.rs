use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use fiber_protocol::sealed_reply::{ReplySecret, Sealed as WireSealed};

const A: &str = "proximos-5e6f7a8b";
const B: &str = "proximos-9c0d1e2f";
const EUI_A: &str = "0016c001ff10a235";
const EUI_B: &str = "0016c001ff10b777";
/// The Server's own concentrator, in every fake that runs LoRaWAN.
const OWN_EUI: &str = "0016c001f11b34a6";

/// `mosquitto_passwd -c -b <file> peer-proximos-5e6f7a8b c2-test-vector-not-a-secret`, run in C1's
/// container (`system/tests/mosquitto-broker.Dockerfile`, mosquitto 2.1.2, default hash). Made-up
/// password, test only.
const VECTOR_1: (&str, &str) = (
    "c2-test-vector-not-a-secret",
    "$7$1000$GS9JVATSjWrMBIxkq0vKr5Lf70c8mjaE/E7DIacJ2ifO37Bmcm6/x1zo0bU8f6z1gJCP2BDBhVbHQDXEYFXrjw==$hh1wFUOQCSJlCuTCbfSRWq1lxMQMmnrgXZ1pofTPYXbNC8MIfvNmwmAiZ4XCk8lnDxnkmtdak4FvlXBFgtsWZw==",
);
/// The same with `-H sha512-pbkdf2` and a password shaped like ours (22 base64url characters).
const VECTOR_2: (&str, &str) = (
    "Zm9vYmFyYmF6cXV4cXV1eA",
    "$7$1000$/3HVjxlnZDhzqOdqfqbS9qajaqFxq74WhN8ivngyUsFYpevLije12bP1EpX20rqHx2YI1L/i+Z5G32fMTfbedQ==$MQPnFuGN/VoRjKfCZqEZn/ISUCE8AHdcqV0GD9z9ACrdfXxwOJmV8XCQipQaz8nzuspUn52GZ//Ie8ZHSSgV3Q==",
);

/// The request id every test add is sealed to.
const REQ: &str = "req-test";

/// The test requester's reply secret: made up, the same every time, so any test can open what an
/// add sealed (the portal's half, `fiber_protocol::sealed_reply`, does the opening).
fn secret() -> ReplySecret {
    ReplySecret::from_bytes([42u8; 32])
}

fn reply() -> ReplyTo {
    ReplyTo::new(&secret().reply_key(), REQ).unwrap()
}

/// The password an add sealed, opened as its requester would.
fn opened(r: &AddResult) -> Option<String> {
    let sealed = r.password_sealed.as_ref()?;
    let wire: WireSealed = serde_json::from_value(serde_json::to_value(sealed).unwrap()).unwrap();
    Some(
        secret()
            .open(&wire, REQ, &r.hostname)
            .expect("opens")
            .to_string(),
    )
}

// ---- the hash -------------------------------------------------------------------------------

/// The exact bytes `mosquitto_passwd` 2.1.2 wrote, reproduced from its salt: same KDF, same
/// iteration count, same base64 alphabet and padding.
#[test]
fn a_mosquitto_passwd_2_1_2_vector_verifies() {
    for (password, hash) in [VECTOR_1, VECTOR_2] {
        let salt_b64 = hash.split('$').nth(3).expect("salt field");
        let salt = base64::engine::general_purpose::STANDARD
            .decode(salt_b64)
            .expect("salt");
        assert_eq!(salt.len(), SALT_LEN, "mosquitto_passwd's salt is 64 bytes");
        assert_eq!(hash_password_with_salt(password, &salt), hash);
        assert!(verify(password, hash));
        assert!(!verify("wrong", hash));
    }
}

#[test]
fn our_hash_has_mosquitto_passwds_shape() {
    let pw = Password::generate();
    let hash = hash_password(&pw);
    let fields: Vec<&str> = hash.split('$').collect();
    assert_eq!(fields[..3], ["", "7", "1000"]);
    assert_eq!(fields.len(), 5);
    assert_eq!(fields[3].len(), 88, "64 bytes, standard base64, padded");
    assert_eq!(fields[4].len(), 88);
    assert!(fields[3].ends_with("=="));
    assert!(verify(pw.expose(), &hash));
    assert_ne!(hash, hash_password(&pw), "a fresh salt every time");
}

#[test]
fn a_password_is_128_bits_of_base64url_in_22_characters() {
    let a = Password::generate();
    let b = Password::generate();
    assert_ne!(a, b);
    for p in [&a, &b] {
        assert_eq!(p.expose().len(), 22);
        assert!(p
            .expose()
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(p.expose())
            .expect("decodes");
        assert_eq!(raw.len(), 16);
    }
    assert_eq!(format!("{a:?}"), "Password(<redacted>)");
}

// ---- validation -----------------------------------------------------------------------------

#[test]
fn hostnames_follow_decision_17() {
    assert_eq!(validate_hostname(A).unwrap(), A);
    assert_eq!(
        validate_hostname(" PROXIMOS-5E6F7A8B ").unwrap(),
        A,
        "lowercased, decision #21"
    );
    for bad in [
        "",
        "proximos-",
        "proximos-5e6f7a8",
        "proximos-5e6f7a8bc",
        "proximos-5e6f7a8g",
        "hub-5e6f7a8b",
        "proximos-5e6f7a8b/#",
        "proximos-5e6f7a8b\nuser x",
        "proximos_5e6f7a8b",
    ] {
        assert!(validate_hostname(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn gateway_euis_are_sixteen_hex_digits() {
    assert_eq!(validate_gateway_eui(EUI_A).unwrap(), EUI_A);
    assert_eq!(
        validate_gateway_eui("00:16:C0:01:FF:10:A2:35").unwrap(),
        EUI_A,
        "validate_arm's separators and case"
    );
    for bad in [
        "",
        "0016c001ff10a23",
        "0016c001ff10a2355",
        "0016c001ff10a23z",
        "#",
    ] {
        assert!(validate_gateway_eui(bad).is_err(), "{bad:?}");
    }
}

/// A prefix becomes part of the Client's whole grant, so nothing in it may widen, split or add an
/// ACL line.
#[test]
fn topic_prefixes_are_plain_segments() {
    for good in ["", "proximos", "eu868", "site-1/proximos_v2", "a/b/c"] {
        assert!(validate_topic_prefix("p", good).is_ok(), "{good:?}");
    }
    for bad in [
        "#",
        "+",
        "proximos/#",
        "proximos/+/x",
        "pro ximos",
        "proximos\ntopic readwrite #",
        "proximos\t",
        "/proximos",
        "proximos/",
        "a//b",
        "/",
        "prox.imos",
    ] {
        let err = validate_topic_prefix("the agent's topic prefix", bad).unwrap_err();
        assert!(err.contains("not a topic prefix"), "{bad:?}: {err}");
    }
}

/// An invalid agent or forwarder prefix refuses the add before either file is written.
#[test]
fn an_invalid_prefix_refuses_the_add_and_writes_no_acl() {
    let bench = Bench::new(FakeGateways::lorawan());
    let env = Env {
        agent_prefix: "proximos/#",
        ..bench.env()
    };
    let err = add(A, None, &reply(), "test", &env).unwrap_err();
    assert!(err.contains("mqtt.topic_prefix"), "{err}");
    assert_eq!(
        (bench.passwd(), bench.acl()),
        (String::new(), String::new())
    );

    let bench = Bench::new(FakeGateways {
        prefix: Ok(Some("eu868\ntopic readwrite #".to_string())),
        ..FakeGateways::lorawan()
    });
    let err = add(A, Some(EUI_A), &reply(), "test", &bench.env()).unwrap_err();
    assert!(err.contains("forwarder's topic prefix"), "{err}");
    assert_eq!(
        (bench.passwd(), bench.acl()),
        (String::new(), String::new())
    );
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), 0);
    assert!(bench.gateways.added.lock().unwrap().is_empty());
    assert!(bench.journal.audits.lock().unwrap().is_empty());
}

// ---- the two files as text ------------------------------------------------------------------

#[test]
fn passwd_upsert_replaces_and_remove_drops_one_entry() {
    let text = format!("peer-{B}:{}\n\n", VECTOR_2.1);
    let mut f = PasswdFile::parse(&text).expect("parses");
    f.upsert("peer-x", VECTOR_1.1.to_string());
    f.upsert("peer-x", VECTOR_2.1.to_string());
    assert_eq!(
        f.users().collect::<Vec<_>>(),
        [&format!("peer-{B}"), "peer-x"]
    );
    assert_eq!(f.hash_of("peer-x"), Some(VECTOR_2.1));
    assert!(f.remove("peer-x"));
    assert!(!f.remove("peer-x"));
    assert_eq!(f.render(), format!("peer-{B}:{}\n", VECTOR_2.1));
}

#[test]
fn a_passwd_line_this_module_cannot_vouch_for_refuses_the_file() {
    let good = format!("peer-{B}:{}\n", VECTOR_2.1);
    for bad in [
        "peer-a\n".to_string(),
        "peer-a:$7$1000$not-base64$nope\n".to_string(), // C1's broker-killing line
        "peer-a:$6$abc$def\n".to_string(),
        "peer-a:plaintext\n".to_string(),
        "peer-a:$7$0$AAAA$AAAA\n".to_string(),
        format!("peer-a:{}$x\n", VECTOR_1.1),
        "# a comment\n".to_string(),
        format!("{good}{good}"),
        format!("peer a:{}\n", VECTOR_1.1),
    ] {
        let text = format!("{good}{bad}");
        assert!(PasswdFile::parse(&text).is_err(), "{bad:?}");
    }
}

#[test]
fn acl_blocks_are_replaced_in_place_and_the_rest_rides_through() {
    let text = "# kept\ntopic read $SYS/#\n\nuser peer-other\ntopic readwrite proximos/other/#\n\n";
    let mut acl = AclFile::parse(text).expect("parses");
    acl.upsert(acl_block(A, "proximos", Some(("eu868", EUI_A))));
    acl.upsert(acl_block(A, "proximos", None));
    let out = acl.render();
    assert_eq!(
        out,
        "# kept\ntopic read $SYS/#\n\nuser peer-other\ntopic readwrite proximos/other/#\n\n\
         user peer-proximos-5e6f7a8b\ntopic readwrite proximos/proximos-5e6f7a8b/#\n\n"
    );
    assert_eq!(AclFile::parse(&out).expect("round trip").render(), out);
    assert!(acl.remove(&peer_username(A)));
    assert!(acl.block("peer-other").is_some());
}

#[test]
fn the_block_is_cluster_md_section_5s_and_names_its_gateway() {
    let block = acl_block(A, "proximos", Some(("eu868", EUI_A)));
    assert_eq!(block.user, "peer-proximos-5e6f7a8b");
    assert_eq!(
        block.lines,
        [
            "topic readwrite proximos/proximos-5e6f7a8b/#",
            "topic readwrite eu868/gateway/0016c001ff10a235/#"
        ]
    );
    assert_eq!(gateway_eui_of(&block).as_deref(), Some(EUI_A));
    assert_eq!(gateway_eui_of(&acl_block(A, "proximos", None)), None);
    // Upstream renders an empty forwarder prefix as none at all.
    let bare = acl_block(A, "proximos", Some(("", EUI_A)));
    assert_eq!(bare.lines[1], "topic readwrite gateway/0016c001ff10a235/#");
    assert_eq!(gateway_eui_of(&bare).as_deref(), Some(EUI_A));
}

#[test]
fn an_acl_directive_mosquitto_would_not_read_refuses_the_file() {
    for bad in [
        "user\n",
        "user a b\n",
        "user a\nuser a\n",
        "user a\ntopic\n",
        "user a\ntopic sometimes x/#\n",
        "user a\nallow x\n",
    ] {
        assert!(AclFile::parse(bad).is_err(), "{bad:?}");
    }
    assert!(AclFile::parse("").is_ok());
    assert!(AclFile::parse("user a\ntopic x/#\npattern read %u/#\n").is_ok());
}

// ---- the apply path, against a directory and fakes ------------------------------------------

#[derive(Default)]
struct FakeBroker {
    reloads: AtomicUsize,
    recovers: AtomicUsize,
    fail_reload: bool,
}

impl Broker for FakeBroker {
    fn reload(&self) -> Result<(), String> {
        self.reloads.fetch_add(1, Ordering::SeqCst);
        if self.fail_reload {
            Err("mosquitto.service is failed".to_string())
        } else {
            Ok(())
        }
    }
    fn recover(&self) -> Result<(), String> {
        self.recovers.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct FakeGateways {
    prefix: Result<Option<String>, String>,
    /// This Hub's own concentrator, as `own_eui` reads it.
    own: Result<Option<String>, String>,
    fail: Option<String>,
    added: Mutex<Vec<(String, String)>>,
    removed: Mutex<Vec<String>>,
}

impl FakeGateways {
    fn lorawan() -> FakeGateways {
        FakeGateways {
            prefix: Ok(Some("eu868".to_string())),
            own: Ok(Some(OWN_EUI.to_string())),
            fail: None,
            added: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
        }
    }
    fn no_lorawan() -> FakeGateways {
        FakeGateways {
            prefix: Ok(None),
            ..FakeGateways::lorawan()
        }
    }
}

impl Gateways for FakeGateways {
    fn lorawan_prefix(&self) -> Result<Option<String>, String> {
        self.prefix.clone()
    }
    fn add(&self, eui: &str, name: &str) -> Result<(), String> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.added
            .lock()
            .unwrap()
            .push((eui.to_string(), name.to_string()));
        Ok(())
    }
    fn remove(&self, eui: &str) -> Result<(), String> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.removed.lock().unwrap().push(eui.to_string());
        Ok(())
    }
    fn own_eui(&self) -> Result<Option<String>, String> {
        self.own.clone()
    }
}

#[derive(Default)]
struct MemJournal {
    lines: Mutex<Vec<String>>,
    audits: Mutex<Vec<(String, Value)>>,
}

impl Journal for MemJournal {
    fn log(&self, line: &str) {
        self.lines.lock().unwrap().push(line.to_string());
    }
    fn audit(&self, operation: &str, details: &Value) {
        self.audits
            .lock()
            .unwrap()
            .push((operation.to_string(), details.clone()));
    }
}

impl MemJournal {
    /// Everything that left through the journal, as one string to search.
    fn everything(&self) -> String {
        let mut s = self.lines.lock().unwrap().join("\n");
        for (op, d) in self.audits.lock().unwrap().iter() {
            s.push_str(op);
            s.push_str(&d.to_string());
        }
        s
    }
}

struct Bench {
    dir: tempfile::TempDir,
    files: BrokerFiles,
    broker: FakeBroker,
    gateways: FakeGateways,
    journal: MemJournal,
}

impl Bench {
    fn new(gateways: FakeGateways) -> Bench {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(PASSWD_FILE), "").unwrap();
        std::fs::write(dir.path().join(ACL_FILE), "").unwrap();
        let files = BrokerFiles {
            dir: dir.path().to_path_buf(),
            owner: Owner::Unchanged,
        };
        Bench {
            dir,
            files,
            broker: FakeBroker::default(),
            gateways,
            journal: MemJournal::default(),
        }
    }
    fn env(&self) -> Env<'_> {
        Env {
            files: &self.files,
            broker: &self.broker,
            gateways: &self.gateways,
            journal: &self.journal,
            agent_prefix: "proximos",
        }
    }
    fn passwd(&self) -> String {
        std::fs::read_to_string(self.files.passwd_path()).unwrap()
    }
    fn acl(&self) -> String {
        std::fs::read_to_string(self.files.acl_path()).unwrap()
    }
    fn add(&self, h: &str, eui: Option<&str>) -> AddResult {
        add(h, eui, &reply(), "test", &self.env()).expect("accepted")
    }
    fn leftovers(&self) -> Vec<String> {
        std::fs::read_dir(self.dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != PASSWD_FILE && n != ACL_FILE)
            .collect()
    }
}

#[test]
fn a_mint_writes_both_files_once_and_returns_the_password_once() {
    let bench = Bench::new(FakeGateways::lorawan());
    let r = bench.add(A, Some(EUI_A));
    assert!(r.complete(), "{r:?}");
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Ok));
    let pw = opened(&r).expect("minted");

    let passwd = PasswdFile::parse(&bench.passwd()).unwrap();
    assert!(verify(
        pw.as_str(),
        passwd.hash_of("peer-proximos-5e6f7a8b").unwrap()
    ));
    assert!(
        !bench.passwd().contains(pw.as_str()),
        "the hash, never the password"
    );
    assert_eq!(
        bench.acl(),
        "user peer-proximos-5e6f7a8b\ntopic readwrite proximos/proximos-5e6f7a8b/#\n\
         topic readwrite eu868/gateway/0016c001ff10a235/#\n\n"
    );
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), 1, "one reload");
    assert_eq!(
        *bench.gateways.added.lock().unwrap(),
        [(EUI_A.to_string(), A.to_string())]
    );
    for f in [PASSWD_FILE, ACL_FILE] {
        let mode = std::fs::metadata(bench.dir.path().join(f))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{f}");
    }
    assert!(bench.leftovers().is_empty(), "{:?}", bench.leftovers());
}

/// The contract C5's Add-hub flow reads: these keys, these spellings.
#[test]
fn the_result_document_has_the_contract_shape() {
    let bench = Bench::new(FakeGateways::lorawan());
    let r = bench.add(A, Some(EUI_A));
    let doc = document(&r);
    let obj = doc.as_object().expect("object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "account",
            "gateway",
            "gateway_eui",
            "hostname",
            "message",
            "password_sealed",
            "username"
        ]
    );
    assert_eq!(doc["account"], "ok");
    assert_eq!(doc["gateway"], "ok");
    assert_eq!(doc["hostname"], A);
    assert_eq!(doc["username"], "peer-proximos-5e6f7a8b");
    assert_eq!(doc["gateway_eui"], EUI_A);
    let sealed = doc["password_sealed"].as_object().expect("sealed");
    let mut parts: Vec<&str> = sealed.keys().map(String::as_str).collect();
    parts.sort_unstable();
    assert_eq!(parts, ["ct", "epk", "nonce"]);
    assert_eq!(doc["password_sealed"]["epk"].as_str().unwrap().len(), 43);
    assert_eq!(doc["password_sealed"]["nonce"].as_str().unwrap().len(), 16);
    // 22 bytes of password and a 16-byte tag, in base64url.
    assert_eq!(doc["password_sealed"]["ct"].as_str().unwrap().len(), 51);
    assert_eq!(opened(&r).unwrap().len(), 22);

    let no_eui = bench.add(B, None);
    let doc = document(&no_eui);
    assert_eq!(doc["gateway"], "skipped");
    assert_eq!(doc["gateway_eui"], Value::Null);

    let removed = document(&remove(A, "test", &bench.env()).unwrap());
    assert_eq!(removed["account"], "ok");
    assert_eq!(removed["gateway"], "ok");
    assert!(removed.get("password").is_none(), "{removed}");
    assert!(removed.get("password_sealed").is_none(), "{removed}");
}

#[test]
fn a_re_run_rotates_and_never_adds_a_second_entry() {
    let bench = Bench::new(FakeGateways::lorawan());
    let first = opened(&bench.add(A, Some(EUI_A))).unwrap();
    let second = opened(&bench.add(A, Some(EUI_A))).unwrap();
    assert_ne!(first, second);
    let passwd = PasswdFile::parse(&bench.passwd()).unwrap();
    assert_eq!(passwd.users().count(), 1);
    let hash = passwd.hash_of("peer-proximos-5e6f7a8b").unwrap();
    assert!(verify(second.as_str(), hash));
    assert!(
        !verify(first.as_str(), hash),
        "the old password stops working"
    );
    assert_eq!(bench.acl().matches("user ").count(), 1);
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), 2);
}

#[test]
fn other_accounts_and_their_blocks_are_preserved() {
    let bench = Bench::new(FakeGateways::lorawan());
    bench.add(A, Some(EUI_A));
    let b_pw = opened(&bench.add(B, None)).unwrap();
    bench.add(A, None);
    let r = remove(A, "test", &bench.env()).unwrap();
    assert!(r.complete());
    let passwd = PasswdFile::parse(&bench.passwd()).unwrap();
    assert_eq!(
        passwd.users().collect::<Vec<_>>(),
        ["peer-proximos-9c0d1e2f"]
    );
    assert!(verify(
        b_pw.as_str(),
        passwd.hash_of("peer-proximos-9c0d1e2f").unwrap()
    ));
    assert_eq!(
        bench.acl(),
        "user peer-proximos-9c0d1e2f\ntopic readwrite proximos/proximos-9c0d1e2f/#\n\n"
    );
}

/// C1: an ACL-only removal leaves the session connected, so the account line and its block go in
/// one write and one reload — and the gateway the block named is deregistered.
#[test]
fn remove_takes_the_line_and_the_block_together_then_the_gateway() {
    let bench = Bench::new(FakeGateways::lorawan());
    bench.add(A, Some(EUI_A));
    let reloads = bench.broker.reloads.load(Ordering::SeqCst);
    let r = remove(A, "test", &bench.env()).unwrap();
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Ok));
    assert_eq!(r.gateway_eui.as_deref(), Some(EUI_A));
    assert_eq!(bench.passwd(), "");
    assert_eq!(bench.acl(), "");
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), reloads + 1);
    assert_eq!(*bench.gateways.removed.lock().unwrap(), [EUI_A]);

    // Again: nothing there, nothing written, nothing reloaded, still success.
    let again = remove(A, "test", &bench.env()).unwrap();
    assert!(again.complete());
    assert_eq!(again.gateway, Effect::Skipped);
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), reloads + 1);
}

#[test]
fn a_malformed_file_is_refused_and_left_alone() {
    let bench = Bench::new(FakeGateways::lorawan());
    let bad = "peer-x:$7$1000$not-base64$nope\n";
    std::fs::write(bench.files.passwd_path(), bad).unwrap();
    let r = bench.add(A, Some(EUI_A));
    assert_eq!(r.account, Effect::Failed);
    assert!(
        r.password_sealed.is_none(),
        "no password for an account that was not minted"
    );
    assert!(!r.complete());
    assert!(r.message.contains("nothing changed"), "{}", r.message);
    assert_eq!(bench.passwd(), bad, "untouched");
    assert_eq!(bench.acl(), "");
    assert_eq!(
        bench.broker.reloads.load(Ordering::SeqCst),
        0,
        "no reload over it"
    );
    // No ACL line records the EUI, so registering it would orphan the registration.
    assert_eq!(r.gateway, Effect::Skipped, "{}", r.message);
    assert!(r.message.contains("account failed, so"), "{}", r.message);
    assert!(bench.gateways.added.lock().unwrap().is_empty());

    std::fs::write(bench.files.passwd_path(), "").unwrap();
    std::fs::write(bench.files.acl_path(), "user a\nfrobnicate\n").unwrap();
    assert_eq!(bench.add(A, None).account, Effect::Failed);
    assert_eq!(bench.passwd(), "");

    // A removal over an unreadable ACL cannot know the gateway: failed, not skipped.
    let r = remove(A, "test", &bench.env()).unwrap();
    assert_eq!((r.account, r.gateway), (Effect::Failed, Effect::Failed));
    assert!(bench.gateways.removed.lock().unwrap().is_empty());
}

#[test]
fn a_reload_the_broker_does_not_survive_puts_the_old_files_back() {
    let mut bench = Bench::new(FakeGateways::lorawan());
    bench.add(B, None);
    let (passwd, acl) = (bench.passwd(), bench.acl());
    bench.broker.fail_reload = true;
    let r = bench.add(A, None);
    assert_eq!(r.account, Effect::Failed);
    assert!(r.password_sealed.is_none());
    assert!(r.message.contains("the old ones are back"), "{}", r.message);
    assert_eq!((bench.passwd(), bench.acl()), (passwd, acl));
    assert_eq!(bench.broker.recovers.load(Ordering::SeqCst), 1);
    assert!(bench.leftovers().is_empty());
}

/// `acl` is renamed into place first, so a `passwd` rename that then fails must put the old
/// `acl` back: the pair never disagrees on disk. A non-empty directory where `passwd` belongs
/// makes exactly that second rename fail, after the first one took.
#[test]
fn a_failed_passwd_rename_puts_the_old_acl_back() {
    let bench = Bench::new(FakeGateways::lorawan());
    bench.add(B, None);
    let old = (bench.passwd(), bench.acl());
    std::fs::remove_file(bench.files.passwd_path()).unwrap();
    std::fs::create_dir(bench.files.passwd_path()).unwrap();
    std::fs::write(bench.files.passwd_path().join("in-the-way"), "").unwrap();

    let new_acl = format!(
        "{old_acl}user peer-{A}\ntopic readwrite proximos/{A}/#\n",
        old_acl = old.1
    );
    let err = bench
        .files
        .write("peer-x:$7$irrelevant\n", &new_acl, &old)
        .unwrap_err();
    assert!(err.contains("cannot rename into"), "{err}");
    assert!(err.contains(PASSWD_FILE), "{err}");
    assert!(!err.contains("restoring acl failed"), "{err}");
    assert_eq!(bench.acl(), old.1, "the old acl is back");
    assert!(bench.files.passwd_path().join("in-the-way").exists());
    let mut left = bench.leftovers();
    left.retain(|n| n != PASSWD_FILE);
    assert!(left.is_empty(), "no staged file left behind: {left:?}");
}

#[test]
fn the_gateway_is_skipped_without_an_eui_or_without_lorawan() {
    let bench = Bench::new(FakeGateways::no_lorawan());
    let r = bench.add(A, Some(EUI_A));
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Skipped));
    assert!(r.complete());
    assert!(r.message.contains("runs no LoRaWAN"), "{}", r.message);
    assert!(
        !bench.acl().contains("gateway"),
        "no gateway line: {}",
        bench.acl()
    );
    assert!(bench.gateways.added.lock().unwrap().is_empty());

    let bench = Bench::new(FakeGateways::lorawan());
    let r = bench.add(A, None);
    assert_eq!(r.gateway, Effect::Skipped);
    assert!(bench.gateways.added.lock().unwrap().is_empty());
}

#[test]
fn a_failed_gateway_is_an_incomplete_registration_with_a_live_password() {
    let gateways = FakeGateways {
        fail: Some("ChirpStack is down".to_string()),
        ..FakeGateways::lorawan()
    };
    let bench = Bench::new(gateways);
    let r = bench.add(A, Some(EUI_A));
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Failed));
    assert!(!r.complete());
    assert!(
        r.password_sealed.is_some(),
        "the account is live, so its one copy is handed out"
    );
    assert!(r.message.contains("ChirpStack is down"));
    // The ACL names the gateway anyway, so a re-run or a remove can find it.
    assert!(bench.acl().contains("eu868/gateway/0016c001ff10a235/#"));

    let unreadable = FakeGateways {
        prefix: Err("no rendered radio config".to_string()),
        ..FakeGateways::lorawan()
    };
    let bench = Bench::new(unreadable);
    let r = bench.add(A, Some(EUI_A));
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Failed));
}

#[test]
fn a_malformed_request_touches_nothing() {
    let bench = Bench::new(FakeGateways::lorawan());
    assert!(add("proximos-xyz", None, &reply(), "test", &bench.env()).is_err());
    assert!(add(A, Some("nope"), &reply(), "test", &bench.env()).is_err());
    assert!(remove("hub-5e6f7a8b", "test", &bench.env()).is_err());
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), 0);
    assert!(bench.journal.audits.lock().unwrap().is_empty());
}

/// The password reaches the result and nothing else: not a log line, not an audit row, not the
/// human message, not a `Debug` print — on success, on rotation and on a partial failure.
#[test]
fn the_password_never_reaches_a_log_line_or_an_audit_row() {
    let gateways = FakeGateways {
        fail: Some("ChirpStack is down".to_string()),
        ..FakeGateways::lorawan()
    };
    let bench = Bench::new(gateways);
    let mut minted = Vec::new();
    for _ in 0..3 {
        let r = bench.add(A, Some(EUI_A));
        let pw = opened(&r).expect("minted");
        assert!(!r.message.contains(pw.as_str()));
        assert!(!format!("{r:?}").contains(pw.as_str()));
        assert!(!incomplete_message(&r.message).contains(pw.as_str()));
        minted.push(pw);
    }
    remove(A, "test", &bench.env()).unwrap();
    let journal = bench.journal.everything();
    assert!(!journal.is_empty());
    for pw in &minted {
        assert!(!journal.contains(pw.as_str()), "leaked into {journal}");
    }
    let audits = bench.journal.audits.lock().unwrap();
    assert_eq!(
        audits.iter().map(|(op, _)| op.as_str()).collect::<Vec<_>>(),
        [AUDIT_ADD, AUDIT_ADD, AUDIT_ADD, AUDIT_REMOVE]
    );
    assert_eq!(audits[0].1["origin"], "test");
    assert_eq!(audits[0].1["hostname"], A);
    assert_eq!(audits[0].1["gateway"], "failed");
}

/// Both surfaces run on different threads of one process; the lock makes their rewrites queue
/// instead of losing each other's account.
#[test]
fn concurrent_adds_keep_every_account() {
    let bench = Bench::new(FakeGateways::no_lorawan());
    let hosts: Vec<String> = (0..12).map(|i| format!("proximos-{i:08x}")).collect();
    std::thread::scope(|s| {
        for h in &hosts {
            let bench = &bench;
            s.spawn(move || assert_eq!(bench.add(h, None).account, Effect::Ok));
        }
    });
    let passwd = PasswdFile::parse(&bench.passwd()).unwrap();
    assert_eq!(passwd.users().count(), hosts.len());
    assert_eq!(
        AclFile::parse(&bench.acl()).unwrap().blocks.len(),
        hosts.len()
    );
}

/// The broker's files belong to the broker's user, looked up by name; a name the system does not
/// know is a failed account and an untouched directory, never a file left to root.
#[test]
fn ownership_is_looked_up_by_name() {
    let mut bench = Bench::new(FakeGateways::no_lorawan());
    bench.files.owner = Owner::Named {
        user: "no-such-user-c2".to_string(),
        group: "no-such-group-c2".to_string(),
    };
    let r = bench.add(A, None);
    assert_eq!(r.account, Effect::Failed);
    assert!(r.message.contains("no-such-user-c2"), "{}", r.message);
    assert_eq!(bench.passwd(), "");
    assert!(bench.leftovers().is_empty());

    // The user this test runs as: the chown is a no-op, and it is taken by name.
    let me = unsafe { libc::getuid() };
    let pw = unsafe { libc::getpwuid(me) };
    assert!(!pw.is_null());
    let name = unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }
        .to_string_lossy()
        .into_owned();
    let gr = unsafe { libc::getgrgid(libc::getgid()) };
    assert!(!gr.is_null());
    let group = unsafe { std::ffi::CStr::from_ptr((*gr).gr_name) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(uid_of(&name).unwrap(), me);
    bench.files.owner = Owner::Named { user: name, group };
    assert_eq!(bench.add(A, None).account, Effect::Ok);
}

/// Writes what the agent would write for two Clients into `$PROXIMOS_C2_FIXTURE_DIR`, for
/// `system/tests/test-mosquitto-broker.sh` to install under real mosquitto 2.1.2: A with a
/// gateway, B without, and C minted then removed again so the rewrite path is what produced the
/// files. The passwords go beside them, for that test to connect with; they are made up on the
/// spot and the directory is the caller's scratch space.
#[test]
#[ignore = "fixture for system/tests/test-mosquitto-broker.sh"]
fn write_broker_fixture() {
    let Ok(dir) = std::env::var("PROXIMOS_C2_FIXTURE_DIR") else {
        panic!("set PROXIMOS_C2_FIXTURE_DIR");
    };
    let dir = PathBuf::from(dir);
    std::fs::write(dir.join(PASSWD_FILE), "").unwrap();
    std::fs::write(dir.join(ACL_FILE), "").unwrap();
    let files = BrokerFiles {
        dir: dir.clone(),
        owner: Owner::Unchanged,
    };
    let broker = FakeBroker::default();
    let gateways = FakeGateways::lorawan();
    let journal = MemJournal::default();
    let env = Env {
        files: &files,
        broker: &broker,
        gateways: &gateways,
        journal: &journal,
        agent_prefix: "proximos",
    };
    let c = "proximos-0c0c0c0c";
    add(c, None, &reply(), "fixture", &env).unwrap();
    let a = add(A, Some(EUI_A), &reply(), "fixture", &env).unwrap();
    let b = add(B, None, &reply(), "fixture", &env).unwrap();
    add(c, None, &reply(), "fixture", &env).unwrap();
    remove(c, "fixture", &env).unwrap();
    let a2 = add(A, Some(EUI_A), &reply(), "fixture", &env).unwrap(); // a rotation
    std::fs::write(dir.join("password.a.old"), opened(&a).unwrap()).unwrap();
    std::fs::write(dir.join("password.a"), opened(&a2).unwrap()).unwrap();
    std::fs::write(dir.join("password.b"), opened(&b).unwrap()).unwrap();
}

// ---- re-runs and this Hub's own gateway (decisions of 2026-10-02) ---------------------------

/// A re-run without an EUI only rotates: the line stays byte for byte, ChirpStack is not
/// touched, and the result names the EUI it kept.
#[test]
fn a_re_run_without_an_eui_keeps_the_gateway_it_has() {
    let bench = Bench::new(FakeGateways::lorawan());
    let first = bench.add(A, Some(EUI_A));
    let acl_before = bench.acl();
    let again = bench.add(A, None);
    assert!(again.complete(), "{again:?}");
    assert_eq!(
        (again.account, again.gateway),
        (Effect::Ok, Effect::Skipped)
    );
    assert_eq!(again.gateway_eui.as_deref(), Some(EUI_A), "the kept EUI");
    assert!(
        again.message.contains("kept as registered"),
        "{}",
        again.message
    );
    assert_eq!(bench.acl(), acl_before, "the gateway line is kept verbatim");
    assert_eq!(
        bench.gateways.added.lock().unwrap().len(),
        1,
        "registered once"
    );
    assert!(bench.gateways.removed.lock().unwrap().is_empty());
    let passwd = PasswdFile::parse(&bench.passwd()).unwrap();
    let hash = passwd.hash_of("peer-proximos-5e6f7a8b").unwrap();
    assert!(verify(&opened(&again).unwrap(), hash));
    assert!(!verify(&opened(&first).unwrap(), hash), "rotated");

    // An account that never had a gateway still has none.
    let r = bench.add(B, None);
    assert_eq!(
        (r.gateway, r.gateway_eui.as_deref()),
        (Effect::Skipped, None)
    );
    assert!(r.message.contains("no gateway EUI given"), "{}", r.message);
}

#[test]
fn a_re_run_with_the_same_eui_is_idempotent() {
    let bench = Bench::new(FakeGateways::lorawan());
    bench.add(A, Some(EUI_A));
    let acl_before = bench.acl();
    let r = bench.add(A, Some(EUI_A));
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Ok));
    assert_eq!(r.gateway_eui.as_deref(), Some(EUI_A));
    assert_eq!(bench.acl(), acl_before);
    assert!(
        bench.gateways.removed.lock().unwrap().is_empty(),
        "nothing deregistered"
    );
}

/// A different EUI replaces the line, registers the new gateway and deregisters the old one —
/// after the write that took the old line away.
#[test]
fn a_re_run_with_another_eui_replaces_and_deregisters_the_old_one() {
    let bench = Bench::new(FakeGateways::lorawan());
    bench.add(A, Some(EUI_A));
    let r = bench.add(A, Some(EUI_B));
    assert!(r.complete(), "{r:?}");
    assert_eq!(r.gateway_eui.as_deref(), Some(EUI_B));
    assert!(bench.acl().contains(&format!("eu868/gateway/{EUI_B}/#")));
    assert!(!bench.acl().contains(EUI_A), "{}", bench.acl());
    assert_eq!(
        bench
            .gateways
            .added
            .lock()
            .unwrap()
            .iter()
            .map(|(e, _)| e.as_str())
            .collect::<Vec<_>>(),
        [EUI_A, EUI_B]
    );
    assert_eq!(*bench.gateways.removed.lock().unwrap(), [EUI_A]);
    assert!(
        r.message
            .contains("old gateway 0016c001ff10a235 deregistered"),
        "{}",
        r.message
    );

    // A deregistration that fails makes the gateway effect fail: the old EUI is still in
    // ChirpStack with no account to use it, and the operator should hear about it.
    struct FailRemove(FakeGateways);
    impl Gateways for FailRemove {
        fn lorawan_prefix(&self) -> Result<Option<String>, String> {
            self.0.lorawan_prefix()
        }
        fn add(&self, eui: &str, name: &str) -> Result<(), String> {
            self.0.add(eui, name)
        }
        fn remove(&self, _eui: &str) -> Result<(), String> {
            Err("ChirpStack is down".to_string())
        }
        fn own_eui(&self) -> Result<Option<String>, String> {
            self.0.own_eui()
        }
    }
    let gateways = FailRemove(FakeGateways::lorawan());
    let env = Env {
        gateways: &gateways,
        ..bench.env()
    };
    let r = add(A, Some(EUI_A), &reply(), "test", &env).unwrap();
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Failed));
    assert!(
        r.message.contains("could not be deregistered"),
        "{}",
        r.message
    );
}

/// An account whose write failed keeps its old line, so its old gateway is not deregistered.
#[test]
fn a_failed_account_write_never_deregisters_the_old_gateway() {
    let mut bench = Bench::new(FakeGateways::lorawan());
    bench.add(A, Some(EUI_A));
    bench.broker.fail_reload = true;
    let r = bench.add(A, Some(EUI_B));
    assert_eq!(r.account, Effect::Failed);
    assert!(bench.acl().contains(EUI_A), "the old block is back");
    assert!(bench.gateways.removed.lock().unwrap().is_empty());
}

/// A removal the broker did not take puts the old files back, so the Client is still connected
/// with its gateway line: its gateway stays registered, and the result says why.
#[test]
fn a_failed_account_removal_never_deregisters_the_gateway() {
    let mut bench = Bench::new(FakeGateways::lorawan());
    bench.add(A, Some(EUI_A));
    bench.broker.fail_reload = true;
    let r = remove(A, "test", &bench.env()).unwrap();
    assert_eq!((r.account, r.gateway), (Effect::Failed, Effect::Skipped));
    assert!(!r.complete());
    assert!(r.message.contains("account not removed"), "{}", r.message);
    assert_eq!(r.gateway_eui.as_deref(), Some(EUI_A));
    assert!(bench.acl().contains(EUI_A), "the old block is back");
    assert!(bench.gateways.removed.lock().unwrap().is_empty());
}

/// One gateway, one account: an EUI another Client's block already grants is refused before
/// anything is written, so that Client keeps its gateway to itself and a later removal of this
/// one cannot deregister it. The holder's own re-run is still idempotent.
#[test]
fn client_add_refuses_a_gateway_another_account_holds() {
    let bench = Bench::new(FakeGateways::lorawan());
    bench.add(A, Some(EUI_A));
    let (passwd, acl) = (bench.passwd(), bench.acl());
    let reloads = bench.broker.reloads.load(Ordering::SeqCst);
    let audits = bench.journal.audits.lock().unwrap().len();

    // The separators and the case are normalised first, so a respelling does not slip past.
    let err = add(
        B,
        Some("00:16:C0:01:FF:10:A2:35"),
        &reply(),
        "test",
        &bench.env(),
    )
    .unwrap_err();
    assert!(
        err.contains(&format!("already granted to peer-{A}")),
        "{err}"
    );
    assert_eq!((bench.passwd(), bench.acl()), (passwd, acl), "untouched");
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), reloads);
    assert_eq!(bench.gateways.added.lock().unwrap().len(), 1, "only A's");
    assert_eq!(bench.journal.audits.lock().unwrap().len(), audits);
    assert!(bench.leftovers().is_empty());

    assert!(bench.add(A, Some(EUI_A)).complete());
}

/// This Hub's own concentrator is refused as a Client's gateway, before anything is touched.
#[test]
fn client_add_refuses_this_hubs_own_gateway_eui() {
    let bench = Bench::new(FakeGateways::lorawan());
    let err = add(
        A,
        Some("00:16:C0:01:F1:1B:34:A6"),
        &reply(),
        "test",
        &bench.env(),
    )
    .unwrap_err();
    assert!(err.contains("own concentrator"), "{err}");
    assert_eq!(bench.passwd(), "");
    assert_eq!(bench.acl(), "");
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), 0);
    assert!(bench.gateways.added.lock().unwrap().is_empty());
    assert!(bench.journal.audits.lock().unwrap().is_empty());
}

/// Fail-closed: an own EUI that cannot be read refuses any EUI, because it might be that one. A
/// rotation without an EUI grants no gateway and goes ahead.
#[test]
fn an_unreadable_own_eui_refuses_a_gateway_but_not_a_rotation() {
    let gateways = FakeGateways {
        own: Err("lorawan/config.json: not JSON".to_string()),
        ..FakeGateways::lorawan()
    };
    let bench = Bench::new(gateways);
    let err = add(A, Some(EUI_A), &reply(), "test", &bench.env()).unwrap_err();
    assert!(
        err.contains("cannot tell this Hub's own gateway EUI"),
        "{err}"
    );
    assert_eq!(bench.passwd(), "", "nothing minted");
    assert!(bench.gateways.added.lock().unwrap().is_empty());

    let r = bench.add(A, None);
    assert!(r.complete(), "{r:?}");

    // A Hub that registered no concentrator of its own has nothing to collide with.
    let bench = Bench::new(FakeGateways {
        own: Ok(None),
        ..FakeGateways::lorawan()
    });
    assert_eq!(bench.add(A, Some(OWN_EUI)).gateway, Effect::Ok);
}

/// `client-remove` never deregisters this Hub's own concentrator, even when an ACL block names
/// it, and never when the own EUI cannot be read. The account goes either way.
#[test]
fn client_remove_never_deregisters_this_hubs_own_gateway() {
    let bench = Bench::new(FakeGateways::lorawan());
    std::fs::write(
        bench.files.acl_path(),
        format!(
            "user peer-{A}\ntopic readwrite proximos/{A}/#\ntopic readwrite eu868/gateway/{OWN_EUI}/#\n"
        ),
    )
    .unwrap();
    let r = remove(A, "test", &bench.env()).unwrap();
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Skipped));
    assert!(r.complete());
    assert!(r.message.contains("own concentrator"), "{}", r.message);
    assert!(bench.gateways.removed.lock().unwrap().is_empty());
    assert_eq!(bench.acl(), "", "the block goes with the account");

    let bench = Bench::new(FakeGateways {
        own: Err("unreadable".to_string()),
        ..FakeGateways::lorawan()
    });
    std::fs::write(
        bench.files.acl_path(),
        format!(
            "user peer-{A}\ntopic readwrite proximos/{A}/#\ntopic readwrite eu868/gateway/{EUI_A}/#\n"
        ),
    )
    .unwrap();
    let r = remove(A, "test", &bench.env()).unwrap();
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Failed));
    assert!(r.message.contains("left registered"), "{}", r.message);
    assert!(bench.gateways.removed.lock().unwrap().is_empty());
}

// ---- the sealed reply (decision of 2026-10-02) ----------------------------------------------

/// What the signed path publishes on `config/response` holds no password, whether the
/// registration is complete or partial. The document is built by [`document`] and the payload by
/// the publisher's own `config_response_payload`, exactly as `run_and_answer` sends it, so a
/// subscriber of the loopback listener reads only the seal. Only the requester's key opens it.
#[test]
fn the_response_topic_payload_never_holds_the_password() {
    for fail in [None, Some("ChirpStack is down".to_string())] {
        let bench = Bench::new(FakeGateways {
            fail: fail.clone(),
            ..FakeGateways::lorawan()
        });
        let r = bench.add(A, Some(EUI_A));
        let pw = opened(&r).expect("minted, and it opens with the requester's key");
        let payload = crate::libs::mqtt::publisher::MqttPublisher::config_response_payload(
            "chal-1",
            REQ,
            if fail.is_none() { "SUCCESS" } else { "ERROR" },
            Some(1),
            Some(1),
            "Configuration applied: cluster-client-add",
            Some(&crate::libs::mqtt::messages::ResultDoc(document(&r))),
        )
        .to_string();
        assert!(payload.contains("password_sealed"), "{payload}");
        assert!(
            !payload.contains(&pw),
            "the clear password is on the wire: {payload}"
        );
        assert!(!payload.contains("\"password\""), "{payload}");

        // Someone else's key, or the same key on another request: nothing.
        let wire: WireSealed = serde_json::from_value(
            serde_json::to_value(r.password_sealed.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(ReplySecret::generate().open(&wire, REQ, A).is_err());
        assert!(secret().open(&wire, "req-other", A).is_err());
        assert!(secret().open(&wire, REQ, B).is_err());
    }
}

/// A key the password cannot be sealed to refuses the request before the old password is
/// rotated away.
#[test]
fn a_reply_key_that_cannot_be_sealed_to_touches_nothing() {
    let bench = Bench::new(FakeGateways::lorawan());
    bench.add(A, None);
    let (passwd, acl) = (bench.passwd(), bench.acl());
    let reloads = bench.broker.reloads.load(Ordering::SeqCst);
    let mut low_order = [0u8; 32];
    low_order[0] = 1;
    let reply = ReplyTo::new(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(low_order),
        REQ,
    )
    .unwrap();
    let err = add(A, None, &reply, "test", &bench.env()).unwrap_err();
    assert!(err.contains("reply_key"), "{err}");
    assert_eq!((bench.passwd(), bench.acl()), (passwd, acl));
    assert_eq!(bench.broker.reloads.load(Ordering::SeqCst), reloads);
}

/// #164: `cluster-client-gateway` grants an existing account its gateway and registers it,
/// without touching the password.
#[test]
fn set_gateway_grants_the_eui_and_keeps_the_password() {
    let bench = Bench::new(FakeGateways::lorawan());
    let r = bench.add(A, None);
    assert_eq!(r.gateway, Effect::Skipped);
    let passwd_before = bench.passwd();

    let r = set_gateway(A, EUI_A, "test", &bench.env()).expect("accepted");
    assert!(r.complete(), "{r:?}");
    assert_eq!((r.account, r.gateway), (Effect::Ok, Effect::Ok));
    assert!(r.password_sealed.is_none());
    assert_eq!(r.gateway_eui.as_deref(), Some(EUI_A));
    assert_eq!(
        bench.passwd(),
        passwd_before,
        "the password does not rotate"
    );
    assert!(bench
        .acl()
        .contains(&format!("topic readwrite eu868/gateway/{EUI_A}/#")));
    assert_eq!(
        bench.gateways.added.lock().unwrap().as_slice(),
        &[(EUI_A.to_string(), A.to_string())]
    );
    assert!(bench
        .journal
        .audits
        .lock()
        .unwrap()
        .iter()
        .any(|(op, _)| op == AUDIT_GATEWAY));

    // A different EUI replaces the line and deregisters the old gateway.
    let r = set_gateway(A, EUI_B, "test", &bench.env()).expect("accepted");
    assert!(r.complete(), "{r:?}");
    assert!(!bench.acl().contains(EUI_A));
    assert_eq!(
        bench.gateways.removed.lock().unwrap().as_slice(),
        &[EUI_A.to_string()]
    );
    assert_eq!(bench.passwd(), passwd_before);
}

#[test]
fn set_gateway_refuses_what_add_refuses_and_an_unknown_account() {
    let bench = Bench::new(FakeGateways::lorawan());
    assert!(set_gateway(A, EUI_A, "test", &bench.env())
        .unwrap_err()
        .contains("not registered"));
    assert_eq!(bench.acl(), "", "nothing written");

    bench.add(A, None);
    bench.add(B, Some(EUI_B));
    assert!(set_gateway(A, OWN_EUI, "test", &bench.env()).is_err());
    assert!(set_gateway(A, EUI_B, "test", &bench.env())
        .unwrap_err()
        .contains("already granted"));
    assert!(set_gateway(A, "", "test", &bench.env()).is_err());
    assert!(set_gateway(A, "0016c0", "test", &bench.env()).is_err());
}
