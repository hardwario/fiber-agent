//! A Server Hub's Client accounts — `cluster client-add` / `client-remove` and their signed twins
//! `cluster-client-add` / `cluster-client-remove` (`plan/control/cluster.md` §5, §7; milestone C2).
//!
//! **One re-runnable operation, two idempotent effects** (decision #15's first prerequisite):
//!
//! 1. **the account** — `peer-<hostname>` in mosquitto's `password_file`, hashed here, in this
//!    process, to the `$7$` PBKDF2-SHA512 form `mosquitto_passwd` 2.1.2 writes, and one ACL block
//!    in `acl_file` that confines it to `<agent prefix>/<hostname>/#` and, when the Client brings
//!    a LoRaWAN gateway, `<forwarder prefix>/gateway/<eui>/#`. Then one SIGHUP. A re-run
//!    **rotates**: the entry and the block are replaced, never duplicated, and the old password
//!    stops working at the reload. A re-run without an EUI keeps the gateway line the account
//!    has; one with another EUI replaces it (see [`add`]);
//! 2. **the gateway** — the Client's gateway EUI registered in this Hub's ChirpStack, skipped when
//!    no EUI was given or `radio.mode` runs no LoRaWAN. Never this Hub's own concentrator: `add`
//!    refuses its EUI, and `remove` never deregisters it ([`Gateways::own_eui`]).
//!
//! Each is reported on its own (`account: ok|failed`, `gateway: ok|skipped|failed`), so a partial
//! result is a visible "registration incomplete" and never a silent half. The gateway effect
//! runs only once the account effect is `ok`: the ACL block is the one record of which gateway a
//! Client brought, so a gateway is never registered without its line, nor deregistered while
//! its line still stands.
//!
//! **One apply path, two surfaces**, as `radio-set-mode` and FB1B share
//! ([`crate::libs::radio_mode`]): [`add`] and [`remove`] are what the signed verbs
//! (`mqtt::monitor`) and the control socket behind `proximosctl cluster` (`control::server`)
//! both call; only the audit row's `origin` differs.
//!
//! # What the broker forgives, and what it does not (C1, `system/tests/test-mosquitto-broker.sh`)
//!
//! mosquitto 2.1.2 re-reads both files on SIGHUP and re-checks every session, so a removed or
//! rotated account's sessions are dropped at the reload and no client-id kick is needed. But a
//! SIGHUP over a file it cannot read or parse makes the **whole broker exit**, the anonymous
//! loopback listener every local service uses included. So, in this order and without
//! exceptions:
//!
//! - the existing files are parsed strictly, and an entry this module cannot vouch for refuses the
//!   operation before anything is written: it is never carried into a new file;
//! - the new content is rendered, then parsed back with the same parser, before it is written;
//! - each file is written to a temporary file in the same directory, `chown`ed to the broker's
//!   own user and `chmod 0600` (looked up by name at run time, never a hard-coded uid), `fsync`ed
//!   and only then renamed over the old one; the directory is `fsync`ed after the renames;
//! - the account line and its ACL block always change in the **same** write and reload: an
//!   ACL-only removal leaves the session connected (C1);
//! - after the reload, `mosquitto.service` must still be active with the same main PID. If it is
//!   not, the old files go back and the broker is restarted, and the account is `failed`.
//!
//! Concurrent calls are serialised by one process-wide lock, because the two surfaces run on
//! different threads of this one process.
//!
//! # The password
//!
//! 128 random bits, base64url without padding: 22 characters. It is stored nowhere but as the
//! hash, and it leaves this module only **sealed** to the requester ([`seal`], decision of
//! 2026-10-02): the signed `cluster-client-add` carries the requester's X25519 `reply_key`, and
//! the result holds `password_sealed` instead of the password. The answer is published on
//! `config/response`, which every subscriber of the anonymous loopback listener can read, so the
//! password must not be in it in clear. The control socket takes the same `reply_key`, and
//! `proximosctl` opens the seal itself. [`Password`] has no `Display`, no `Serialize` and a
//! redacting `Debug`, and is wiped when dropped. Nothing in this module formats it into a log
//! line, an audit row, an error or a result document; the tests below hold all four to that.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use rand::RngCore as _;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha512;

use crate::libs::cluster_state::PEER_USERNAME_PREFIX;
use crate::libs::config_applier::ConfigApplier;

pub mod seal;
pub use seal::{ReplyTo, Sealed};

/// The directory C1 creates for the 8883 listener's files (`proximos-config`'s `data-dirs.conf`:
/// `0750 root:mosquitto`).
pub const BROKER_DIR: &str = "/data/mosquitto";
/// `password_file` of the 8883 listener (`proximos.conf`).
pub const PASSWD_FILE: &str = "passwd";
/// `acl_file` of the 8883 listener (`proximos.conf`).
pub const ACL_FILE: &str = "acl";
/// The unit whose `ExecReload=` is the SIGHUP.
pub const MOSQUITTO_UNIT: &str = "mosquitto.service";
/// The user and group mosquitto runs as, and so must own both files (mosquitto 2.1 warns about,
/// and announces it will refuse, a file another user owns).
pub const MOSQUITTO_USER: &str = "mosquitto";

/// `mosquitto_passwd` 2.1.2's PBKDF2-SHA512 parameters, as measured in C1's test and pinned by
/// [`tests::a_mosquitto_passwd_2_1_2_vector_verifies`].
pub const PBKDF2_ITERATIONS: u32 = 1000;
pub const SALT_LEN: usize = 64;
pub const HASH_LEN: usize = 64;

/// The audit operations this module writes.
pub const AUDIT_ADD: &str = "CLUSTER_CLIENT_ADD";
pub const AUDIT_REMOVE: &str = "CLUSTER_CLIENT_REMOVE";
pub const AUDIT_GATEWAY: &str = "CLUSTER_CLIENT_GATEWAY";

/// The one lock both surfaces take. Process-wide because the signed verbs run on the MQTT
/// monitor's blocking pool and `proximosctl`'s on the control socket's thread.
static ACCOUNTS: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// A Client Hub's hostname, decision #17's `^proximos-[0-9a-f]{8}$`, with the build's brand
/// prefix (`fiber-` in a FIBER build, decision #50). Lowercased first, because a Hub's hostname
/// is lowercase everywhere (decision #21) and an operator may type the LCD's spelling; anything
/// else is refused, since the name becomes a topic segment and an ACL line.
pub fn validate_hostname(raw: &str) -> Result<String, String> {
    let h = raw.trim().to_ascii_lowercase();
    if proximos_brand::hostname_serial(&h).is_some() {
        Ok(h)
    } else {
        let p = proximos_brand::HOSTNAME_PREFIX;
        Err(format!(
            "hostname {raw:?} is not a Hub hostname: expected {p} and eight hex digits \
             ({p}1a2b3c4d)"
        ))
    }
}

/// A Client's gateway EUI: 16 hex digits, normalised to lowercase. The rule `validate_arm`
/// applies to `peer_gateway_eui` (`lorawan::cluster`), so `:`/`-` separators are accepted as they
/// are there.
pub fn validate_gateway_eui(raw: &str) -> Result<String, String> {
    match crate::libs::cluster_state::normalize_gateway_eui(Some(raw)) {
        Ok(Some(eui)) => Ok(eui),
        _ => Err(format!(
            "gateway_eui {raw:?} is not a gateway EUI: expected 16 hex digits"
        )),
    }
}

/// A topic prefix an ACL line is written under — the agent's (`mqtt.topic_prefix`) or the
/// forwarder's: one or more `/`-separated segments of `[A-Za-z0-9_-]`. The line is the Client's
/// whole grant, so a `+` or `#` would widen it to other Clients' topics, whitespace would split
/// it into another directive and a newline would add one; an empty segment is a topic no Hub
/// publishes under. The empty prefix is no prefix at all (`under`), as upstream renders the
/// forwarder's, and is accepted.
pub fn validate_topic_prefix(what: &str, raw: &str) -> Result<(), String> {
    let ok = raw.is_empty()
        || raw.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{what} {raw:?} is not a topic prefix: expected segments of letters, digits, _ and - \
             separated by /; nothing changed"
        ))
    }
}

/// `peer-<hostname>` — the account a Client derives for itself (cluster.md §8), so a join bundle
/// minted for one Client cannot be used on another.
pub fn peer_username(hostname: &str) -> String {
    format!("{PEER_USERNAME_PREFIX}{hostname}")
}

// ---------------------------------------------------------------------------------------------
// The password and its hash
// ---------------------------------------------------------------------------------------------

/// A minted broker password. No `Display` and no `Serialize`: it leaves this module only through
/// [`seal::seal`]. `Debug` prints nothing of it, and the buffer is wiped when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(zeroize::Zeroizing<String>);

impl Password {
    /// 128 random bits from the OS, base64url without padding: 22 characters, and no `+`, `/` or
    /// `=` to trip a QR code, a shell or a URL.
    pub fn generate() -> Password {
        let mut bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let password = Password(zeroize::Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        ));
        zeroize::Zeroize::zeroize(&mut bytes);
        password
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

fn pbkdf2_sha512(password: &[u8], salt: &[u8], iterations: u32) -> [u8; HASH_LEN] {
    let mut out = [0u8; HASH_LEN];
    pbkdf2::pbkdf2_hmac::<Sha512>(password, salt, iterations, &mut out);
    out
}

/// mosquitto 2's `$7$<iterations>$<salt>$<hash>`, both in standard base64 with padding — the
/// alphabet `mosquitto_passwd` 2.1.2 writes (`+`, `/`, `=`), checked against its output.
pub fn hash_password_with_salt(password: &str, salt: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD;
    let hash = pbkdf2_sha512(password.as_bytes(), salt, PBKDF2_ITERATIONS);
    format!(
        "$7${PBKDF2_ITERATIONS}${}${}",
        b64.encode(salt),
        b64.encode(hash)
    )
}

/// [`hash_password_with_salt`] with a fresh 64-byte salt.
pub fn hash_password(password: &Password) -> String {
    let mut salt = [0u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    hash_password_with_salt(password.expose(), &salt)
}

/// A `$7$` hash this module can vouch for: four `$` fields, a positive iteration count, a salt
/// and a 64-byte hash that both decode as standard base64. Returns the parts for [`verify`].
fn parse_hash(hash: &str) -> Result<(u32, Vec<u8>, Vec<u8>), String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let rest = hash
        .strip_prefix("$7$")
        .ok_or("not a $7$ (PBKDF2-SHA512) hash")?;
    let mut parts = rest.split('$');
    let (Some(iterations), Some(salt), Some(digest), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("a $7$ hash has exactly iterations, salt and hash".to_string());
    };
    let iterations: u32 = iterations
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or("the iteration count is not a positive number")?;
    let salt = b64
        .decode(salt)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or("the salt is not base64")?;
    let digest = b64
        .decode(digest)
        .ok()
        .filter(|d| d.len() == HASH_LEN)
        .ok_or("the hash is not 64 bytes of base64")?;
    Ok((iterations, salt, digest))
}

/// Does `password` match a `$7$` hash? What mosquitto does at a CONNECT; here for the tests and
/// for nothing else.
pub fn verify(password: &str, hash: &str) -> bool {
    match parse_hash(hash) {
        Ok((iterations, salt, digest)) => {
            pbkdf2_sha512(password.as_bytes(), &salt, iterations).as_slice() == digest.as_slice()
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// The two files, as text
// ---------------------------------------------------------------------------------------------

/// mosquitto's `password_file`: `username:hash` lines. Blank lines are dropped on the rewrite;
/// anything else that is not a well-formed `$7$` entry refuses the whole file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PasswdFile {
    entries: Vec<(String, String)>,
}

impl PasswdFile {
    pub fn parse(text: &str) -> Result<PasswdFile, String> {
        let mut entries: Vec<(String, String)> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.trim().is_empty() {
                continue;
            }
            let at = |why: &str| format!("passwd line {}: {why}", n + 1);
            let (user, hash) = line
                .split_once(':')
                .ok_or_else(|| at("not a username:hash entry"))?;
            if user.is_empty() || user.contains(char::is_whitespace) {
                return Err(at("the username is empty or contains whitespace"));
            }
            parse_hash(hash).map_err(|e| at(&e))?;
            if entries.iter().any(|(u, _)| u == user) {
                return Err(at("the username appears twice"));
            }
            entries.push((user.to_string(), hash.to_string()));
        }
        Ok(PasswdFile { entries })
    }

    pub fn hash_of(&self, user: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(u, _)| u == user)
            .map(|(_, h)| h.as_str())
    }

    pub fn users(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(u, _)| u.as_str())
    }

    /// Replace `user`'s entry in place, or append it: one entry per account, always.
    pub fn upsert(&mut self, user: &str, hash: String) {
        match self.entries.iter_mut().find(|(u, _)| u == user) {
            Some(entry) => entry.1 = hash,
            None => self.entries.push((user.to_string(), hash)),
        }
    }

    /// Drop `user`'s entry. `true` when there was one.
    pub fn remove(&mut self, user: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(u, _)| u != user);
        self.entries.len() != before
    }

    pub fn render(&self) -> String {
        self.entries
            .iter()
            .map(|(u, h)| format!("{u}:{h}\n"))
            .collect()
    }
}

/// mosquitto's `acl_file`: optional lines before the first `user`, then one block per account.
/// Comments and blank lines are kept where they are; a directive mosquitto would not read
/// refuses the whole file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AclFile {
    preamble: Vec<String>,
    blocks: Vec<AclBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclBlock {
    pub user: String,
    pub lines: Vec<String>,
}

/// `topic|pattern [read|write|readwrite|deny] <topic>`, the two rule directives of mosquitto's ACL
/// file. The topic is the rest of the line and may not be empty.
fn is_rule(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let Some(directive) = words.next() else {
        return false;
    };
    if directive != "topic" && directive != "pattern" {
        return false;
    }
    let rest: Vec<&str> = words.collect();
    match rest.as_slice() {
        [_topic] => true,
        [access, _topic, ..] if ["read", "write", "readwrite", "deny"].contains(access) => true,
        [_topic, ..] => false,
        [] => false,
    }
}

fn is_inert(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

impl AclFile {
    pub fn parse(text: &str) -> Result<AclFile, String> {
        let mut acl = AclFile::default();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let at = |why: &str| format!("acl line {}: {why}", n + 1);
            if let Some(user) = line.trim_start().strip_prefix("user ") {
                let user = user.trim();
                if user.is_empty() || user.contains(char::is_whitespace) {
                    return Err(at("a user line names no single username"));
                }
                if acl.blocks.iter().any(|b| b.user == user) {
                    return Err(at("the user has two blocks"));
                }
                acl.blocks.push(AclBlock {
                    user: user.to_string(),
                    lines: Vec::new(),
                });
                continue;
            }
            if !is_inert(line) && !is_rule(line.trim()) {
                return Err(at("not a user, topic or pattern line"));
            }
            match acl.blocks.last_mut() {
                Some(block) => block.lines.push(line.to_string()),
                None => acl.preamble.push(line.to_string()),
            }
        }
        Ok(acl)
    }

    pub fn block(&self, user: &str) -> Option<&AclBlock> {
        self.blocks.iter().find(|b| b.user == user)
    }

    /// The account, other than `except`, whose block already grants gateway `eui`.
    pub fn gateway_holder(&self, eui: &str, except: &str) -> Option<&str> {
        self.blocks
            .iter()
            .filter(|b| b.user != except)
            .find(|b| {
                b.lines
                    .iter()
                    .any(|line| gateway_eui_of_line(line).as_deref() == Some(eui))
            })
            .map(|b| b.user.as_str())
    }

    /// Replace `block.user`'s block in place, or append it.
    pub fn upsert(&mut self, block: AclBlock) {
        match self.blocks.iter_mut().find(|b| b.user == block.user) {
            Some(existing) => *existing = block,
            None => self.blocks.push(block),
        }
    }

    pub fn remove(&mut self, user: &str) -> bool {
        let before = self.blocks.len();
        self.blocks.retain(|b| b.user != user);
        self.blocks.len() != before
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.preamble {
            out.push_str(line);
            out.push('\n');
        }
        for block in &self.blocks {
            out.push_str("user ");
            out.push_str(&block.user);
            out.push('\n');
            let mut lines = block.lines.as_slice();
            while let Some((last, head)) = lines.split_last() {
                if last.trim().is_empty() {
                    lines = head;
                } else {
                    break;
                }
            }
            for line in lines {
                out.push_str(line);
                out.push('\n');
            }
            // One blank line between blocks, for whoever reads the file over ssh.
            out.push('\n');
        }
        out
    }
}

/// `<prefix>/<rest>`, where an empty prefix is no prefix at all — not a leading slash, the way
/// upstream renders the forwarder's.
fn under(prefix: &str, rest: &str) -> String {
    let p = prefix.trim_end_matches('/');
    if p.is_empty() {
        rest.to_string()
    } else {
        format!("{p}/{rest}")
    }
}

/// cluster.md §5's block: the Client's own topic family and, when it brings one, its gateway's.
pub fn acl_block(hostname: &str, agent_prefix: &str, gateway: Option<(&str, &str)>) -> AclBlock {
    let mut lines = vec![format!(
        "topic readwrite {}",
        under(agent_prefix, &format!("{hostname}/#"))
    )];
    if let Some((forwarder_prefix, eui)) = gateway {
        lines.push(format!(
            "topic readwrite {}",
            under(forwarder_prefix, &format!("gateway/{eui}/#"))
        ));
    }
    AclBlock {
        user: peer_username(hostname),
        lines,
    }
}

/// The gateway EUI a block grants, read back for `client-remove` and for a re-run: the ACL is the
/// one record on this Hub of which gateway a Client brought (the portal's registry is a copy).
pub fn gateway_eui_of(block: &AclBlock) -> Option<String> {
    block
        .lines
        .iter()
        .find_map(|line| gateway_eui_of_line(line))
}

/// One `topic … <prefix>/gateway/<eui>/#` line's EUI.
fn gateway_eui_of_line(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    if words.next() != Some("topic") {
        return None;
    }
    let topic = words.last()?;
    let segments: Vec<&str> = topic.split('/').collect();
    let n = segments.len();
    if n < 3 || segments[n - 1] != "#" || segments[n - 3] != "gateway" {
        return None;
    }
    validate_gateway_eui(segments[n - 2]).ok()
}

// ---------------------------------------------------------------------------------------------
// The two files, on disk
// ---------------------------------------------------------------------------------------------

/// Who the written files belong to.
#[derive(Debug, Clone)]
pub enum Owner {
    /// Looked up by name at write time — the Hub: `mosquitto:mosquitto`.
    Named { user: String, group: String },
    /// Leave them to the writing process. Tests, and the fixture C1's broker test installs itself.
    Unchanged,
}

/// The directory holding `passwd` and `acl`, and who owns what is written there.
#[derive(Debug, Clone)]
pub struct BrokerFiles {
    pub dir: PathBuf,
    pub owner: Owner,
}

impl BrokerFiles {
    /// The Hub's: [`BROKER_DIR`], owned by [`MOSQUITTO_USER`].
    pub fn this_hub() -> BrokerFiles {
        BrokerFiles {
            dir: PathBuf::from(BROKER_DIR),
            owner: Owner::Named {
                user: MOSQUITTO_USER.to_string(),
                group: MOSQUITTO_USER.to_string(),
            },
        }
    }

    pub fn passwd_path(&self) -> PathBuf {
        self.dir.join(PASSWD_FILE)
    }

    pub fn acl_path(&self) -> PathBuf {
        self.dir.join(ACL_FILE)
    }

    /// Both files as they are. A missing file reads as empty: tmpfiles creates both, but an
    /// absent one is a state this module can fix, not one it must refuse.
    fn read(&self) -> Result<(String, String), String> {
        let read = |path: PathBuf| match fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        };
        Ok((read(self.passwd_path())?, read(self.acl_path())?))
    }

    fn ids(&self) -> Result<Option<(u32, u32)>, String> {
        match &self.owner {
            Owner::Unchanged => Ok(None),
            Owner::Named { user, group } => Ok(Some((uid_of(user)?, gid_of(group)?))),
        }
    }

    /// Write both files, each atomically, `acl` first; on a failed second rename the first is put
    /// back, so the pair never disagrees on disk. Both texts must already have been checked.
    fn write(&self, passwd: &str, acl: &str, old: &(String, String)) -> Result<(), String> {
        let ids = self.ids()?;
        let acl_tmp = self.stage(ACL_FILE, acl, ids)?;
        let passwd_tmp = match self.stage(PASSWD_FILE, passwd, ids) {
            Ok(t) => t,
            Err(e) => {
                let _ = fs::remove_file(&acl_tmp);
                return Err(e);
            }
        };
        if let Err(e) = fs::rename(&acl_tmp, self.acl_path()) {
            let _ = fs::remove_file(&acl_tmp);
            let _ = fs::remove_file(&passwd_tmp);
            return Err(format!(
                "cannot rename into {}: {e}",
                self.acl_path().display()
            ));
        }
        if let Err(e) = fs::rename(&passwd_tmp, self.passwd_path()) {
            let _ = fs::remove_file(&passwd_tmp);
            let undo = self
                .stage(ACL_FILE, &old.1, ids)
                .and_then(|t| fs::rename(&t, self.acl_path()).map_err(|e| e.to_string()));
            return Err(format!(
                "cannot rename into {}: {e}{}",
                self.passwd_path().display(),
                match undo {
                    Ok(()) => String::new(),
                    Err(u) => format!("; restoring acl failed too: {u}"),
                }
            ));
        }
        sync_dir(&self.dir);
        Ok(())
    }

    /// A temporary file in the same directory, with the content, owner and mode the final file
    /// must have, flushed to disk. Only the rename is left.
    fn stage(&self, name: &str, content: &str, ids: Option<(u32, u32)>) -> Result<PathBuf, String> {
        let mut suffix = [0u8; 6];
        rand::rngs::OsRng.fill_bytes(&mut suffix);
        let path = self
            .dir
            .join(format!(".{name}.tmp-{}", hex::encode(suffix)));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(content.as_bytes())?;
            if let Some((uid, gid)) = ids {
                std::os::unix::fs::fchown(&file, Some(uid), Some(gid))?;
            }
            // Explicitly, after the chown: the umask must not decide, and a chown may clear bits.
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.sync_all()
        })();
        result.map_err(|e| {
            let _ = fs::remove_file(&path);
            format!("cannot write {}: {e}", path.display())
        })?;
        Ok(path)
    }
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// `getpwnam_r(3)`: this process is multi-threaded, so not `getpwnam`.
fn uid_of(user: &str) -> Result<u32, String> {
    let name = std::ffi::CString::new(user).map_err(|_| format!("bad user name {user:?}"))?;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live local of the right type and `buf.len()` is its size.
    let rc = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut out,
        )
    };
    if rc != 0 || out.is_null() {
        return Err(format!(
            "no user {user:?} on this Hub, so the broker's files cannot be given to it"
        ));
    }
    Ok(pwd.pw_uid)
}

/// `getgrnam_r(3)`, for the same reason.
fn gid_of(group: &str) -> Result<u32, String> {
    let name = std::ffi::CString::new(group).map_err(|_| format!("bad group name {group:?}"))?;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::group = std::ptr::null_mut();
    // SAFETY: as in `uid_of`.
    let rc = unsafe {
        libc::getgrnam_r(
            name.as_ptr(),
            &mut grp,
            buf.as_mut_ptr(),
            buf.len(),
            &mut out,
        )
    };
    if rc != 0 || out.is_null() {
        return Err(format!(
            "no group {group:?} on this Hub, so the broker's files cannot be given to it"
        ));
    }
    Ok(grp.gr_gid)
}

// ---------------------------------------------------------------------------------------------
// What the operation reads from the machine and does to it
// ---------------------------------------------------------------------------------------------

/// The broker. A trait so the apply path runs in a test without a mosquitto to signal.
pub trait Broker: Send + Sync {
    /// Make the broker re-read `passwd` and `acl`, and confirm it still serves afterwards. `Err`
    /// is "it did not, or it is not running".
    fn reload(&self) -> Result<(), String>;
    /// Bring a broker the reload took down back up, once the old files are back.
    fn recover(&self) -> Result<(), String>;
}

/// This Hub's ChirpStack, as far as a Client's gateway is concerned.
pub trait Gateways: Send + Sync {
    /// The forwarder's topic prefix when `radio.mode` runs LoRaWAN; `None` when it runs none.
    fn lorawan_prefix(&self) -> Result<Option<String>, String>;
    /// Register the gateway. Already registered is success.
    fn add(&self, eui: &str, name: &str) -> Result<(), String>;
    /// Deregister it. Already absent is success.
    fn remove(&self, eui: &str) -> Result<(), String>;
    /// This Hub's own concentrator EUI, as `control.radio create` registered it; `None` when it
    /// registered none (no concentrator, or LoRaWAN never created). `Err` when that record
    /// cannot be read, which every caller treats as "might be any EUI".
    fn own_eui(&self) -> Result<Option<String>, String>;
}

/// Where this module's log lines and audit rows go.
pub trait Journal: Send + Sync {
    fn log(&self, line: &str);
    fn audit(&self, operation: &str, details: &Value);
}

/// Everything [`add`] and [`remove`] touch.
pub struct Env<'a> {
    pub files: &'a BrokerFiles,
    pub broker: &'a dyn Broker,
    pub gateways: &'a dyn Gateways,
    pub journal: &'a dyn Journal,
    /// The agent's topic prefix (`mqtt.topic_prefix`, rendered from `portal.topic_prefix`): the
    /// Client publishes its family under the same one, or the Server's portal would not see it.
    pub agent_prefix: &'a str,
}

// ---------------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------------

/// How one effect went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Ok,
    Skipped,
    Failed,
}

/// `cluster client-add`'s result: cluster.md §7's `{account, gateway, password}`, with the
/// password sealed to the requester, the names it is about, and one human line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AddResult {
    pub hostname: String,
    pub username: String,
    pub gateway_eui: Option<String>,
    pub account: Effect,
    pub gateway: Effect,
    /// Present exactly when `account` is `ok`: the one copy there is, sealed to the requester's
    /// `reply_key` (see [`seal`]).
    pub password_sealed: Option<Sealed>,
    pub message: String,
}

impl AddResult {
    /// Both effects good: the portal's `pending` row (cluster.md §7). Anything else is its
    /// `failed` — "registration incomplete".
    pub fn complete(&self) -> bool {
        self.account == Effect::Ok && self.gateway != Effect::Failed
    }
}

/// `cluster client-remove`'s result. No password: there is none to hand out.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RemoveResult {
    pub hostname: String,
    pub username: String,
    /// The gateway the account's ACL block named, which is what was deregistered.
    pub gateway_eui: Option<String>,
    pub account: Effect,
    pub gateway: Effect,
    pub message: String,
}

impl RemoveResult {
    pub fn complete(&self) -> bool {
        self.account == Effect::Ok && self.gateway != Effect::Failed
    }
}

// ---------------------------------------------------------------------------------------------
// The apply path
// ---------------------------------------------------------------------------------------------

/// What a run does about the account's gateway line, decided before the account write because
/// the line belongs in the block that write puts there.
enum GatewayPlan {
    /// Leave whatever gateway line the account already has exactly as it is, and do nothing in
    /// ChirpStack. `failed` carries the reason when the gateway effect could not be evaluated.
    Keep { note: String, failed: bool },
    /// Grant (and register) this EUI under the forwarder's prefix, replacing any other.
    Grant { prefix: String, eui: String },
}

/// Mint or rotate `peer-<hostname>`, then register its gateway. `Err` only for a request that is
/// refused before anything is touched (a malformed hostname or EUI, this Hub's own gateway EUI,
/// an own EUI that cannot be read, an EUI another account's block already grants, an agent or
/// forwarder prefix that is not a plain topic prefix ([`validate_topic_prefix`]), or a reply key
/// the password cannot be sealed to); every other outcome is an [`AddResult`] whose effects say
/// what happened.
///
/// **A re-run** (decision of 2026-10-02):
///
/// - without an EUI it only rotates the password. The account keeps the gateway line it has and
///   ChirpStack keeps the registration; `gateway` is `skipped` and `gateway_eui` names the kept
///   EUI;
/// - with the same EUI it is idempotent: the same line, and a registration ChirpStack already
///   has, which is success;
/// - with a different EUI the line is replaced, the new gateway is registered and the old one is
///   deregistered.
///
/// `gateway_eui` in the result is always the gateway the account is granted once this run is
/// over (or was asked to be, when the account write itself failed).
pub fn add(
    hostname: &str,
    gateway_eui: Option<&str>,
    reply: &ReplyTo,
    origin: &str,
    env: &Env<'_>,
) -> Result<AddResult, String> {
    apply(hostname, gateway_eui, Some(reply), origin, env)
}

/// Grant an existing account the gateway `gateway_eui` and register it — `cluster-client-gateway`
/// (cluster.md amendment 2026-10-28, #164): [`add`] with the account half reduced to rewriting
/// its ACL block. The `passwd` line is not touched, so the password does not rotate and the
/// Client stays connected. An account that does not exist is refused, as is everything [`add`]
/// refuses about an EUI. `account` is `ok` when the block was rewritten; `password_sealed` is
/// always `None`.
pub fn set_gateway(
    hostname: &str,
    gateway_eui: &str,
    origin: &str,
    env: &Env<'_>,
) -> Result<AddResult, String> {
    if gateway_eui.trim().is_empty() {
        return Err("gateway_eui is required".to_string());
    }
    apply(hostname, Some(gateway_eui), None, origin, env)
}

/// [`add`] (`reply` set: mint or rotate the password) and [`set_gateway`] (`reply` none: keep it).
fn apply(
    hostname: &str,
    gateway_eui: Option<&str>,
    reply: Option<&ReplyTo>,
    origin: &str,
    env: &Env<'_>,
) -> Result<AddResult, String> {
    let operation = if reply.is_some() {
        AUDIT_ADD
    } else {
        AUDIT_GATEWAY
    };
    let hostname = validate_hostname(hostname)?;
    let eui = gateway_eui
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(validate_gateway_eui)
        .transpose()?;
    validate_topic_prefix(
        "the agent's topic prefix (mqtt.topic_prefix)",
        env.agent_prefix,
    )?;
    let username = peer_username(&hostname);
    let _guard = ACCOUNTS.lock().unwrap_or_else(|e| e.into_inner());

    // This Hub's own concentrator is never a Client's gateway: the ACL line would let the Client
    // publish downlinks and fake uplinks as this Server's radio, and a later removal would
    // deregister it. When the own EUI cannot be read the request is refused (fail-closed).
    let own = match &eui {
        Some(eui) => match env.gateways.own_eui() {
            Ok(own) => {
                if own.as_deref() == Some(eui.as_str()) {
                    return Err(format!(
                        "gateway_eui {eui} is this Hub's own concentrator; a Client brings its \
                         own gateway"
                    ));
                }
                own
            }
            Err(e) => {
                return Err(format!(
                    "cannot tell this Hub's own gateway EUI ({e}), so no gateway is granted to a \
                     Client; nothing changed"
                ))
            }
        },
        None => None,
    };

    let plan = match &eui {
        None => GatewayPlan::Keep {
            note: String::new(),
            failed: false,
        },
        Some(eui) => match env.gateways.lorawan_prefix() {
            Ok(Some(prefix)) => {
                validate_topic_prefix("the forwarder's topic prefix", &prefix)?;
                GatewayPlan::Grant {
                    prefix,
                    eui: eui.clone(),
                }
            }
            Ok(None) => GatewayPlan::Keep {
                note: "gateway skipped: this Hub's radio.mode runs no LoRaWAN".to_string(),
                failed: false,
            },
            Err(e) => GatewayPlan::Keep {
                note: format!("gateway failed: cannot read this Hub's radio config ({e})"),
                failed: true,
            },
        },
    };

    // 1. The account. The password is sealed before anything is written: a reply key it cannot
    // be sealed to refuses the request while the old password still works.
    let password = match reply {
        Some(reply) => {
            let password = Password::generate();
            let sealed = seal::seal(password.expose(), reply, &hostname)?;
            Some((password, sealed))
        }
        None => None,
    };
    let mut existing_eui: Option<String> = None;
    // Set when the files show the request must be refused; the edit then changes nothing, so
    // nothing is written or reloaded.
    let mut refused: Option<String> = None;
    let account = rewrite(env, |passwd, acl| {
        // One gateway, one account. Granting an EUI another account's block already names would
        // give this Client readwrite on that Client's gateway, and a later removal of this one
        // would deregister a gateway the other still uses. Read under the lock, from the file
        // the broker has, so two adds cannot both pass it.
        if let GatewayPlan::Grant { eui, .. } = &plan {
            if let Some(holder) = acl.gateway_holder(eui, &username) {
                refused = Some(format!(
                    "gateway_eui {eui} is already granted to {holder}; remove that Client \
                     first. Nothing changed"
                ));
                return false;
            }
        }
        let existing = acl.block(&username);
        if existing.is_none() && password.is_none() {
            refused = Some(format!(
                "{username} is not registered on this Hub; add the Client first. Nothing changed"
            ));
            return false;
        }
        existing_eui = existing.and_then(gateway_eui_of);
        let block = match &plan {
            GatewayPlan::Grant { prefix, eui } => acl_block(
                &hostname,
                env.agent_prefix,
                Some((prefix.as_str(), eui.as_str())),
            ),
            GatewayPlan::Keep { .. } => {
                // The kept gateway line goes into the new block verbatim, never re-rendered.
                let mut block = acl_block(&hostname, env.agent_prefix, None);
                if let Some(existing) = existing {
                    block.lines.extend(
                        existing
                            .lines
                            .iter()
                            .filter(|line| gateway_eui_of_line(line).is_some())
                            .cloned(),
                    );
                }
                block
            }
        };
        if let Some((password, _)) = &password {
            passwd.upsert(&username, hash_password(password));
        }
        acl.upsert(block);
        true
    });
    if let Some(why) = refused {
        return Err(why);
    }
    let mut notes: Vec<String> = Vec::new();
    let sealed = password.map(|(password, sealed)| {
        drop(password);
        sealed
    });
    let (account, password_sealed) = match account {
        Ok(_) => {
            notes.push(match &sealed {
                Some(_) => format!("account {username} minted"),
                None => format!("account {username} kept, its password unchanged"),
            });
            (Effect::Ok, sealed)
        }
        Err(e) => {
            notes.push(format!("account failed: {e}"));
            (Effect::Failed, None)
        }
    };

    // 2. The gateway — idempotent like the account.
    let (gateway, granted) = match plan {
        GatewayPlan::Keep { note, failed } => {
            if note.is_empty() {
                // No EUI asked for: whatever the account had stays, line and registration.
                notes.push(match &existing_eui {
                    Some(kept) => format!("gateway {kept} kept as registered"),
                    None => "gateway skipped: no gateway EUI given".to_string(),
                });
            } else {
                notes.push(note);
                if let Some(kept) = &existing_eui {
                    notes.push(format!("the account keeps gateway {kept}"));
                }
            }
            (
                if failed {
                    Effect::Failed
                } else {
                    Effect::Skipped
                },
                existing_eui.clone(),
            )
        }
        // The account write failed, so no ACL line records this EUI: a registration now would be
        // one nothing on this Hub knows of, and no `client-remove` would ever take it away.
        GatewayPlan::Grant { eui, .. } if account != Effect::Ok => {
            notes.push(format!(
                "gateway skipped: account failed, so {eui} is not registered"
            ));
            (Effect::Skipped, Some(eui))
        }
        GatewayPlan::Grant { eui, .. } => {
            let mut effect = match env.gateways.add(&eui, &hostname) {
                Ok(()) => {
                    notes.push(format!("gateway {eui} registered"));
                    Effect::Ok
                }
                Err(e) => {
                    notes.push(format!("gateway failed: {e}"));
                    Effect::Failed
                }
            };
            // A different EUI replaced the old one: its line is gone with the write above, so
            // its registration goes too. Only once that write took — until then the old line
            // still grants it — and never this Hub's own (the account could only have named it
            // from a file written before the own-EUI rule).
            if let (Some(old), Effect::Ok) = (&existing_eui, account) {
                if *old != eui {
                    if own.as_deref() == Some(old.as_str()) {
                        notes.push(format!(
                            "old gateway {old} is this Hub's own concentrator; left registered"
                        ));
                    } else {
                        match env.gateways.remove(old) {
                            Ok(()) => notes.push(format!("old gateway {old} deregistered")),
                            Err(e) => {
                                notes.push(format!(
                                    "gateway failed: old gateway {old} could not be \
                                     deregistered ({e})"
                                ));
                                effect = Effect::Failed;
                            }
                        }
                    }
                }
            }
            (effect, Some(eui))
        }
    };

    let result = AddResult {
        hostname,
        username,
        gateway_eui: granted,
        account,
        gateway,
        password_sealed,
        message: notes.join("; "),
    };
    report(
        env.journal,
        operation,
        origin,
        &result.hostname,
        result.gateway_eui.as_deref(),
        result.account,
        result.gateway,
        &result.message,
    );
    Ok(result)
}

/// Remove `peer-<hostname>` — the account line and its ACL block in one write and one reload —
/// then deregister the gateway its block named, but only once the account is gone: a removal
/// whose write or reload failed leaves the gateway registered (`skipped`). Removing an account
/// that is not there is success: the desired state already holds.
pub fn remove(hostname: &str, origin: &str, env: &Env<'_>) -> Result<RemoveResult, String> {
    let hostname = validate_hostname(hostname)?;
    let username = peer_username(&hostname);
    let _guard = ACCOUNTS.lock().unwrap_or_else(|e| e.into_inner());

    let mut notes: Vec<String> = Vec::new();
    let mut eui: Option<String> = None;
    // Whether the files were read at all: if not, which gateway the account named is unknown,
    // and that is a failure to look, not "it named none".
    let mut examined = false;
    let account = rewrite(env, |passwd, acl| {
        examined = true;
        eui = acl.block(&username).and_then(gateway_eui_of);
        let had_entry = passwd.remove(&username);
        let had_block = acl.remove(&username);
        had_entry || had_block
    });
    let account = match account {
        Ok(true) => {
            notes.push(format!("account {username} removed"));
            Effect::Ok
        }
        Ok(false) => {
            notes.push(format!("account {username} was not there"));
            Effect::Ok
        }
        Err(e) => {
            notes.push(format!("account failed: {e}"));
            Effect::Failed
        }
    };

    let gateway = match &eui {
        None if !examined => {
            notes.push(
                "gateway failed: the account's ACL block could not be read, so its gateway is \
                 unknown"
                    .to_string(),
            );
            Effect::Failed
        }
        None => {
            notes.push("gateway skipped: the account named no gateway".to_string());
            Effect::Skipped
        }
        // The account is still there: the write or the reload failed and the old files are
        // back, so the Client stays connected with its gateway line. Deregistering the gateway
        // now would cut its radio off behind an account that still works — the same gate `add`
        // puts on deregistering a replaced EUI.
        Some(eui) if account != Effect::Ok => {
            notes.push(format!(
                "gateway skipped: account not removed, so {eui} stays registered"
            ));
            Effect::Skipped
        }
        Some(eui) => match env.gateways.lorawan_prefix() {
            Ok(None) => {
                notes.push("gateway skipped: this Hub's radio.mode runs no LoRaWAN".to_string());
                Effect::Skipped
            }
            Err(e) => {
                notes.push(format!(
                    "gateway failed: cannot read this Hub's radio config ({e})"
                ));
                Effect::Failed
            }
            // Never this Hub's own concentrator, whatever the block says (a file written by hand
            // or before the own-EUI rule), and never when that cannot be told (fail-closed: a
            // gateway left registered is a nuisance, this Hub's own radio deregistered is an
            // outage).
            Ok(Some(_)) => match env.gateways.own_eui() {
                Err(e) => {
                    notes.push(format!(
                        "gateway failed: cannot tell this Hub's own gateway EUI ({e}), so {eui} \
                         is left registered"
                    ));
                    Effect::Failed
                }
                Ok(Some(own)) if own == *eui => {
                    notes.push(format!(
                        "gateway skipped: {eui} is this Hub's own concentrator and stays registered"
                    ));
                    Effect::Skipped
                }
                Ok(_) => match env.gateways.remove(eui) {
                    Ok(()) => {
                        notes.push(format!("gateway {eui} removed"));
                        Effect::Ok
                    }
                    Err(e) => {
                        notes.push(format!("gateway failed: {e}"));
                        Effect::Failed
                    }
                },
            },
        },
    };

    let result = RemoveResult {
        hostname,
        username,
        gateway_eui: eui,
        account,
        gateway,
        message: notes.join("; "),
    };
    report(
        env.journal,
        AUDIT_REMOVE,
        origin,
        &result.hostname,
        result.gateway_eui.as_deref(),
        result.account,
        result.gateway,
        &result.message,
    );
    Ok(result)
}

/// Read both files, let `edit` change the models, and when it reports a change write both and
/// reload once. `Ok(changed)`; `Err` leaves the broker on the files it had.
fn rewrite(
    env: &Env<'_>,
    edit: impl FnOnce(&mut PasswdFile, &mut AclFile) -> bool,
) -> Result<bool, String> {
    let old = env.files.read()?;
    let mut passwd = PasswdFile::parse(&old.0).map_err(|e| {
        format!(
            "{} is not a file this Hub wrote ({e}); nothing changed",
            env.files.passwd_path().display()
        )
    })?;
    let mut acl = AclFile::parse(&old.1).map_err(|e| {
        format!(
            "{} is not a file this Hub wrote ({e}); nothing changed",
            env.files.acl_path().display()
        )
    })?;
    if !edit(&mut passwd, &mut acl) {
        return Ok(false);
    }
    let (passwd_text, acl_text) = (passwd.render(), acl.render());
    // What is about to be renamed into place is parsed back first: a reload over a file the broker
    // cannot parse takes the whole broker down, loopback included.
    PasswdFile::parse(&passwd_text).map_err(|e| format!("refusing to write passwd: {e}"))?;
    AclFile::parse(&acl_text).map_err(|e| format!("refusing to write acl: {e}"))?;

    env.files.write(&passwd_text, &acl_text, &old)?;
    if let Err(e) = env.broker.reload() {
        // Put back what the broker last loaded and get it serving again; the account is not
        // changed, and the caller says so.
        let restored = env
            .files
            .write(&old.0, &old.1, &(passwd_text, acl_text))
            .and_then(|()| env.broker.recover());
        return Err(match restored {
            Ok(()) => format!("the broker did not take the new files ({e}); the old ones are back"),
            Err(r) => format!(
                "the broker did not take the new files ({e}), and restoring the old ones failed \
                 ({r})"
            ),
        });
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn report(
    journal: &dyn Journal,
    operation: &str,
    origin: &str,
    hostname: &str,
    gateway_eui: Option<&str>,
    account: Effect,
    gateway: Effect,
    message: &str,
) {
    journal.log(&format!(
        "[cluster_clients] {operation} {hostname} (origin {origin}): {message}"
    ));
    journal.audit(
        operation,
        &json!({
            "hostname": hostname,
            "gateway_eui": gateway_eui,
            "account": account,
            "gateway": gateway,
            "origin": origin,
            "message": message,
        }),
    );
}

// ---------------------------------------------------------------------------------------------
// This Hub
// ---------------------------------------------------------------------------------------------

/// mosquitto under systemd. `systemctl reload` runs the unit's `ExecReload=kill -HUP $MAINPID`,
/// so the signal goes where systemd says the broker is — the same `systemctl` the agent already
/// restarts units and reboots the Hub with (decision #6's surviving exec set), and no new binary.
pub struct SystemdMosquitto;

impl SystemdMosquitto {
    fn show() -> Result<(String, u32), String> {
        let out = std::process::Command::new("systemctl")
            .args(["show", "-p", "ActiveState", "-p", "MainPID", MOSQUITTO_UNIT])
            .output()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut state = None;
        let mut pid = None;
        for line in text.lines() {
            match line.split_once('=') {
                Some(("ActiveState", v)) => state = Some(v.trim().to_string()),
                Some(("MainPID", v)) => pid = v.trim().parse().ok(),
                _ => {}
            }
        }
        match (state, pid) {
            (Some(s), Some(p)) => Ok((s, p)),
            _ => Err(format!(
                "systemctl show {MOSQUITTO_UNIT} said nothing usable"
            )),
        }
    }

    fn systemctl(args: &[&str]) -> Result<(), String> {
        let out = std::process::Command::new("systemctl")
            .args(args)
            .output()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemctl {} exited with {}: {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

/// How long the broker gets to choke on a reload before it counts as having taken it. mosquitto
/// handles the SIGHUP in its main loop, and C1 saw a bad file end the process within that loop's
/// first turn; a second covers it with margin, and the caller is a person waiting on one form.
const RELOAD_SETTLE: Duration = Duration::from_millis(1000);
const RELOAD_POLL: Duration = Duration::from_millis(200);

impl Broker for SystemdMosquitto {
    fn reload(&self) -> Result<(), String> {
        let (state, before) = Self::show()?;
        if state != "active" || before == 0 {
            return Err(format!("{MOSQUITTO_UNIT} is {state}, not running"));
        }
        Self::systemctl(&["reload", MOSQUITTO_UNIT])?;
        let mut waited = Duration::ZERO;
        while waited < RELOAD_SETTLE {
            std::thread::sleep(RELOAD_POLL);
            waited += RELOAD_POLL;
            let (state, pid) = Self::show()?;
            if state != "active" || pid != before {
                return Err(format!(
                    "{MOSQUITTO_UNIT} is {state} (main PID {before} → {pid}) after the reload"
                ));
            }
        }
        Ok(())
    }

    fn recover(&self) -> Result<(), String> {
        match Self::show() {
            Ok((state, pid)) if state == "active" && pid != 0 => {
                Self::systemctl(&["reload", MOSQUITTO_UNIT])
            }
            _ => Self::systemctl(&["restart", MOSQUITTO_UNIT]),
        }
    }
}

/// This Hub's ChirpStack through `control_radio`, in process, the way the signed `node-add` reaches
/// it (`radio_nodes`): the rendered radio config, the tenant token in the radio store, and the
/// gateway functions `proximosctl control.radio gateway-add` / `gateway-remove` run.
///
/// **One difference from `gateway-add`, on purpose.** The CLI refuses unless
/// `PROXIMOS_GATEWAY_BRIDGE_ENABLED` is set, because an *external* gateway reaches ChirpStack over
/// the UDP gateway bridge, which is closed by default and would never deliver. A Client Hub's
/// gateway does not use the bridge: its `chirpstack-mqtt-forwarder` publishes to this Hub's
/// broker on 8883 (cluster.md §1, §5), under the topics the account's ACL block grants. So the
/// bridge gate does not apply and is not consulted here.
pub struct RadioGateways;

impl RadioGateways {
    /// Run one ChirpStack operation on a thread and runtime of its own: the signed path runs
    /// inside the MQTT monitor's runtime, where `block_on` would panic (`radio_nodes`, same
    /// reason). The client is built from the tenant token in the radio store, as `proximosctl`
    /// builds it.
    fn chirpstack<F>(op: F) -> Result<(), String>
    where
        F: FnOnce(
                control_radio::config::RadioConfig,
                control_radio::lorawan::tonic_client::TonicChirpStack,
                control_radio::store::Store,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = control_radio::Result<()>>>,
            > + Send,
    {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let config =
                        control_radio::dispatch::load_config().map_err(|e| e.to_string())?;
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
                    rt.block_on(async move {
                        let store =
                            control_radio::store::Store::new(control_radio::dispatch::store_root());
                        let client =
                            control_radio::lorawan::tonic_client::TonicChirpStack::connect(
                                control_radio::lorawan::Tokens::new(store.clone()),
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        op(config, client, store).await.map_err(|e| e.to_string())
                    })
                })
                .join()
                .map_err(|_| "the radio thread panicked".to_string())?
        })
    }
}

impl Gateways for RadioGateways {
    fn lorawan_prefix(&self) -> Result<Option<String>, String> {
        let config = control_radio::dispatch::load_config().map_err(|e| e.to_string())?;
        Ok(config
            .mode
            .allows(control_radio::Transport::Lorawan)
            .then(|| control_radio::dispatch::forwarder_topic_prefix(&config)))
    }

    fn add(&self, eui: &str, name: &str) -> Result<(), String> {
        let eui = control_radio::lorawan::Eui::parse(eui).map_err(|e| e.to_string())?;
        let name = name.to_string();
        Self::chirpstack(move |config, client, store| {
            Box::pin(async move {
                let tokens = control_radio::lorawan::Tokens::new(store.clone());
                let conv = control_radio::lorawan::converge::Converger {
                    client: &client,
                    config: &config,
                    tokens: &tokens,
                    store: &store,
                };
                let tenant = conv.resolve_tenant_id().await?;
                control_radio::lorawan::gateway::add_external(&client, &tenant, &eui, &name, true)
                    .await
                    .map(|_| ())
            })
        })
    }

    /// The converged-id record `create` writes when it registers the concentrator it discovered
    /// (`/data/proximos/radio/lorawan/config.json`, `gateway_id`; control/radio/lorawan.md §5).
    /// Not the retained `gateway/+/state/conn` topics `create` discovers it from: on a Server
    /// every Client's forwarder publishes there too. Not a journal scrape either (decision #6).
    fn own_eui(&self) -> Result<Option<String>, String> {
        let store = control_radio::store::Store::new(control_radio::dispatch::store_root());
        match store
            .read(control_radio::store::Slot::LorawanConfig)
            .map_err(|e| e.to_string())?
        {
            None => Ok(None),
            Some(raw) => control_radio::lorawan::ConvergedState::from_json(&raw)
                .map(|state| state.gateway_id.map(|eui| eui.as_str().to_string()))
                .map_err(|e| e.to_string()),
        }
    }

    fn remove(&self, eui: &str) -> Result<(), String> {
        use control_radio::lorawan::client::ChirpStack as _;
        let eui = control_radio::lorawan::Eui::parse(eui).map_err(|e| e.to_string())?;
        Self::chirpstack(move |config, client, store| {
            Box::pin(async move {
                // `gateway-remove`'s own order: absent is already the desired state.
                match client.delete_gateway(&eui).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == control_radio::Kind::NotFound => {}
                    Err(e) => return Err(e),
                }
                let tokens = control_radio::lorawan::Tokens::new(store.clone());
                control_radio::lorawan::converge::Converger {
                    client: &client,
                    config: &config,
                    tokens: &tokens,
                    store: &store,
                }
                .forget_gateway(&eui)
            })
        })
    }
}

/// The agent's journal: stderr, and the Hub audit trail through the config applier when there is
/// one (the same rows every other Hub-side change writes).
pub struct AgentJournal {
    pub applier: Option<Arc<ConfigApplier>>,
}

impl Journal for AgentJournal {
    fn log(&self, line: &str) {
        eprintln!("{line}");
    }
    fn audit(&self, operation: &str, details: &Value) {
        match &self.applier {
            Some(a) => a.log_audit_public(operation, details.to_string()),
            None => eprintln!("[cluster_clients] no audit trail for {operation}: {details}"),
        }
    }
}

/// [`add`] on this Hub, for either surface.
pub fn run_add(
    hostname: &str,
    gateway_eui: Option<&str>,
    reply: &ReplyTo,
    origin: &str,
    agent_prefix: &str,
    applier: Option<Arc<ConfigApplier>>,
) -> Result<AddResult, String> {
    let files = BrokerFiles::this_hub();
    let journal = AgentJournal { applier };
    add(
        hostname,
        gateway_eui,
        reply,
        origin,
        &Env {
            files: &files,
            broker: &SystemdMosquitto,
            gateways: &RadioGateways,
            journal: &journal,
            agent_prefix,
        },
    )
}

/// [`set_gateway`] on this Hub.
pub fn run_set_gateway(
    hostname: &str,
    gateway_eui: &str,
    origin: &str,
    agent_prefix: &str,
    applier: Option<Arc<ConfigApplier>>,
) -> Result<AddResult, String> {
    let files = BrokerFiles::this_hub();
    let journal = AgentJournal { applier };
    set_gateway(
        hostname,
        gateway_eui,
        origin,
        &Env {
            files: &files,
            broker: &SystemdMosquitto,
            gateways: &RadioGateways,
            journal: &journal,
            agent_prefix,
        },
    )
}

/// [`remove`] on this Hub, for either surface.
pub fn run_remove(
    hostname: &str,
    origin: &str,
    agent_prefix: &str,
    applier: Option<Arc<ConfigApplier>>,
) -> Result<RemoveResult, String> {
    let files = BrokerFiles::this_hub();
    let journal = AgentJournal { applier };
    let mut result = remove(
        hostname,
        origin,
        &Env {
            files: &files,
            broker: &SystemdMosquitto,
            gateways: &RadioGateways,
            journal: &journal,
            agent_prefix,
        },
    )?;
    // cluster.md amendment 2026-10-28 (#168): the removed Client's retained documents go too,
    // or a restarted portal reads the Hub back in. A note, never a failed removal.
    if result.complete() {
        let note =
            match crate::libs::cluster_retained::clear_loopback(agent_prefix, &result.hostname) {
                Ok(n) => format!("retained: {n} cleared"),
                Err(e) => format!("retained: not cleared: {e}"),
            };
        eprintln!("[cluster_clients] {} {note}", result.hostname);
        result.message = format!("{}; {note}", result.message);
    }
    Ok(result)
}

/// One line for a command response's `message`/`error_message`. Never the password, which is
/// nowhere but sealed in the result document.
pub fn incomplete_message(message: &str) -> String {
    format!("registration incomplete: {message}")
}

/// A result as the JSON document both surfaces return. It holds no password, only
/// `password_sealed`.
pub fn document<T: Serialize>(result: &T) -> Value {
    serde_json::to_value(result).unwrap_or(Value::Null)
}

/// The agent's topic prefix when no config names one — `defaults.yaml`'s `portal.topic_prefix`.
pub const DEFAULT_AGENT_PREFIX: &str = "proximos";

/// What the signed verbs answer: the command status, and the result document for the
/// response's `result`.
///
/// SUCCESS exactly when the registration is complete (account `ok`, gateway `ok` or `skipped`).
/// Anything partial is an ERROR whose `Execution failed: …` message names what failed, and it
/// still carries the document, because a minted account's (sealed) password exists nowhere else. A request the
/// apply path refuses outright (it was validated at construction, so this is a guard) is an ERROR
/// with no document. The `Err` text reaches the audit row and the log; it never holds the
/// password.
pub fn run_signed(
    cmd: crate::libs::mqtt::messages::MqttCommand,
    agent_prefix: &str,
    applier: Option<Arc<ConfigApplier>>,
) -> (Result<(), String>, Option<Value>) {
    use crate::libs::mqtt::messages::MqttCommand;
    let prefix = if agent_prefix.is_empty() {
        DEFAULT_AGENT_PREFIX
    } else {
        agent_prefix
    };
    match cmd {
        MqttCommand::ClusterClientAdd {
            hostname,
            gateway_eui,
            reply,
        } => match run_add(
            &hostname,
            gateway_eui.as_deref(),
            &reply,
            "signed-mqtt",
            prefix,
            applier,
        ) {
            Ok(r) if r.complete() => (Ok(()), Some(document(&r))),
            Ok(r) => (Err(incomplete_message(&r.message)), Some(document(&r))),
            Err(e) => (Err(e), None),
        },
        MqttCommand::ClusterClientGateway {
            hostname,
            gateway_eui,
        } => match run_set_gateway(&hostname, &gateway_eui, "signed-mqtt", prefix, applier) {
            Ok(r) if r.complete() => (Ok(()), Some(document(&r))),
            Ok(r) => (Err(incomplete_message(&r.message)), Some(document(&r))),
            Err(e) => (Err(e), None),
        },
        MqttCommand::ClusterClientRemove { hostname } => {
            match run_remove(&hostname, "signed-mqtt", prefix, applier) {
                Ok(r) if r.complete() => (Ok(()), Some(document(&r))),
                Ok(r) => (Err(incomplete_message(&r.message)), Some(document(&r))),
                Err(e) => (Err(e), None),
            }
        }
        other => (
            Err(format!("{} is not a cluster-client verb", other.name())),
            None,
        ),
    }
}

#[cfg(test)]
mod tests;
