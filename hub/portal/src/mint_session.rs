//! `fiber-portal mint-session` / `fiber-portal revoke-session`: a session for an existing user,
//! issued on the Hub itself, for automated tests that must not type a password
//! (plan/portal/design.md, amendment 2026-09-25).
//!
//! Only whoever can already open the Portal's database can mint: the command must run as the
//! owner of `data_dir` (the service user, or root through `su -s /bin/sh proximos -c …`), so it adds
//! no access that root on the Hub does not already have. There is no network endpoint. The
//! token goes to a 0600 file and never to stdout, every mint and revoke is an audit row, and a
//! minted session has a hard expiry (at most [`MAX_TTL_SECS`]) that use does not slide.

use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use portal_storage::{NewAuditEntry, SessionRow, Storage};

/// A minted session outlives no working day: a forgotten token file stops being a login.
pub(crate) const MAX_TTL_SECS: i64 = 12 * 3600;
const DEFAULT_TTL_SECS: i64 = 8 * 3600;

const USAGE: &str = "usage:
  fiber-portal mint-session --user <username> --out <file> [--ttl <8h|90m|3600>]
  fiber-portal revoke-session --token-file <file>

Run as the owner of the Portal's data directory, on a Hub (BusyBox has no runuser):
  su -s /bin/sh proximos -c 'PORTAL_CONFIG=/run/proximos/portal/config.yaml \
    fiber-portal mint-session --user admin --out /tmp/portal-test-session'";

/// `Some(exit code)` when `args` name one of the commands, `None` to start the server.
pub fn dispatch(args: &[String], data_dir: &str, db_key: Option<&str>) -> Option<ExitCode> {
    let (command, rest) = args.split_first()?;
    let db = Db {
        dir: Path::new(data_dir),
        key: db_key,
    };
    let result = match command.as_str() {
        "mint-session" => parse_mint(rest).and_then(|m| mint(&db, &m)),
        "revoke-session" => parse_revoke(rest).and_then(|p| revoke(&db, &p)),
        _ => return None,
    };
    Some(match result {
        Ok(message) => {
            eprintln!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    })
}

/// Where the Portal's database is, as the service's own config names it.
pub(crate) struct Db<'a> {
    pub dir: &'a Path,
    pub key: Option<&'a str>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct MintArgs {
    pub user: String,
    pub out: PathBuf,
    pub ttl_secs: i64,
}

pub(crate) fn parse_mint(args: &[String]) -> Result<MintArgs, String> {
    let mut user = None;
    let mut out = None;
    let mut ttl_secs = DEFAULT_TTL_SECS;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--user" => user = Some(value()?.clone()),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--ttl" => ttl_secs = parse_ttl(value()?)?,
            _ => return Err(format!("unknown argument {flag}\n{USAGE}")),
        }
    }
    Ok(MintArgs {
        user: user.ok_or_else(|| format!("--user is required\n{USAGE}"))?,
        out: out.ok_or_else(|| format!("--out is required\n{USAGE}"))?,
        ttl_secs,
    })
}

fn parse_revoke(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [flag, path] if flag == "--token-file" => Ok(PathBuf::from(path)),
        _ => Err(USAGE.to_string()),
    }
}

/// `8h`, `90m`, `45s` or plain seconds, between one minute and [`MAX_TTL_SECS`].
pub(crate) fn parse_ttl(text: &str) -> Result<i64, String> {
    let (digits, unit) = match text.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (&text[..i], c),
        _ => (text, 's'),
    };
    let n: i64 = digits
        .parse()
        .map_err(|_| format!("--ttl {text}: not a duration"))?;
    let secs = match unit {
        'h' => n.saturating_mul(3600),
        'm' => n.saturating_mul(60),
        's' => n,
        _ => return Err(format!("--ttl {text}: use h, m or s")),
    };
    if !(60..=MAX_TTL_SECS).contains(&secs) {
        return Err(format!("--ttl {text}: between 1 minute and 12 hours"));
    }
    Ok(secs)
}

fn mint(db: &Db<'_>, args: &MintArgs) -> Result<String, String> {
    let storage = open_as_owner(db)?;
    let user = storage
        .get_user_by_username(&args.user)
        .map_err(|e| format!("user lookup failed: {e}"))?
        .ok_or_else(|| format!("no user {:?}", args.user))?;
    if !user.enabled {
        return Err(format!("user {:?} is disabled", args.user));
    }

    // LE-18 (local E2E 2026-09-26): the file first, then the session, and never over an
    // existing file. The other order left a valid session behind when the file could not be
    // written, and a second mint into the same file orphaned the first session, which
    // revoke-session could then no longer reach.
    if args.out.exists() {
        return Err(format!(
            "{} exists; revoke that session first (revoke-session --token-file)",
            args.out.display()
        ));
    }
    let token = crate::auth::generate_token();
    write_token_file(&args.out, &token)?;
    let now = unix_now();
    let expires_at = now + args.ttl_secs;
    // `last_seen_at` at the expiry: `AuthService::validate` slides a session only once it was
    // last seen an hour ago, which a minted session never is before it expires. So it ends at
    // `expires_at` however much it is used.
    if let Err(e) = storage.create_session(&SessionRow {
        token_hash: crate::auth::sha256_hex(&token),
        user_id: user.id,
        created_at: now,
        last_seen_at: expires_at,
        expires_at,
    }) {
        let _ = std::fs::remove_file(&args.out);
        return Err(format!("session not stored: {e}"));
    }
    audit(
        &storage,
        "session_minted",
        &user.username,
        Some(user.id),
        serde_json::json!({ "via": "cli", "ttl_s": args.ttl_secs, "expires_at": expires_at }),
    );
    Ok(format!(
        "session for {} written to {} (expires in {} min)",
        user.username,
        args.out.display(),
        args.ttl_secs / 60
    ))
}

fn revoke(db: &Db<'_>, token_file: &Path) -> Result<String, String> {
    let token = std::fs::read_to_string(token_file)
        .map_err(|e| format!("cannot read {}: {e}", token_file.display()))?;
    let storage = open_as_owner(db)?;
    let hash = crate::auth::sha256_hex(token.trim());
    let username = storage
        .get_session(&hash)
        .ok()
        .flatten()
        .map(|(_, user)| (user.username, user.id));
    storage
        .delete_session(&hash)
        .map_err(|e| format!("session not deleted: {e}"))?;
    let _ = std::fs::remove_file(token_file);
    if let Some((name, id)) = &username {
        audit(
            &storage,
            "session_revoked",
            name,
            Some(*id),
            serde_json::json!({ "via": "cli" }),
        );
    }
    Ok(match username {
        Some((name, _)) => format!("session of {name} revoked"),
        None => "no such session (already expired or revoked)".to_string(),
    })
}

/// Opens the database only as the owner of `data_dir`. A root process would create SQLite's
/// side files (`-journal`, `-wal`, `-shm`) as root, and the service could no longer write them.
fn open_as_owner(db: &Db<'_>) -> Result<Storage, String> {
    let dir = db.dir;
    let owner = std::fs::metadata(dir)
        .map_err(|e| format!("cannot stat {}: {e}", dir.display()))?
        .uid();
    // Who we are, read off a file we create: the uid a new file gets is the one SQLite's side
    // files would get.
    let probe = dir.join(format!(".mint-session-probe-{}", std::process::id()));
    let me = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
        .and_then(|f| f.metadata())
        .map_err(|e| format!("cannot write in {}: {e}", dir.display()))?
        .uid();
    let _ = std::fs::remove_file(&probe);
    if me != owner {
        return Err(format!(
            "run as the owner of {} (uid {owner}), e.g. su -s /bin/sh proximos -c 'fiber-portal …'",
            dir.display()
        ));
    }
    Storage::open(&dir.join("portal.db"), db.key)
        .map_err(|e| format!("cannot open the database: {e}"))
}

/// The token file: 0600, written whole under a temporary name and renamed into place. The
/// temporary file must be new, so a name planted in a shared directory such as `/tmp` is
/// refused rather than followed.
fn write_token_file(path: &Path, token: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    writeln!(file, "{token}").map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))
}

fn audit(
    storage: &Storage,
    event_type: &str,
    username: &str,
    user_id: Option<i64>,
    details: serde_json::Value,
) {
    let entry = NewAuditEntry {
        event_type: event_type.to_string(),
        username: Some(username.to_string()),
        user_id,
        ip: None,
        target: Some(username.to_string()),
        details: Some(details.to_string()),
        success: true,
    };
    if let Err(err) = storage.insert_audit(&entry) {
        eprintln!("warning: audit row not written: {err}");
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_ttl_takes_hours_minutes_or_seconds_within_bounds() {
        assert_eq!(parse_ttl("8h"), Ok(8 * 3600));
        assert_eq!(parse_ttl("90m"), Ok(5400));
        assert_eq!(parse_ttl("3600"), Ok(3600));
        assert!(parse_ttl("13h").is_err(), "no longer than 12 hours");
        assert!(parse_ttl("30s").is_err(), "at least a minute");
        assert!(parse_ttl("1d").is_err());
        assert!(parse_ttl("h").is_err());
    }

    #[test]
    fn mint_needs_a_user_and_a_file() {
        let parsed = parse_mint(&strings(&["--user", "admin", "--out", "/tmp/t"])).expect("ok");
        assert_eq!(
            parsed,
            MintArgs {
                user: "admin".into(),
                out: "/tmp/t".into(),
                ttl_secs: DEFAULT_TTL_SECS,
            }
        );
        assert!(parse_mint(&strings(&["--user", "admin"])).is_err());
        assert!(parse_mint(&strings(&["--out", "/tmp/t"])).is_err());
        assert!(parse_mint(&strings(&["--user"])).is_err());
        assert!(parse_mint(&strings(&["--user", "a", "--out", "b", "--x"])).is_err());
    }

    /// End to end against a real database: the minted token logs in as the user through the
    /// same `validate` every request goes through, the file is 0600, the session does not
    /// slide past its expiry, and revoking it logs out.
    #[tokio::test]
    async fn a_minted_session_logs_in_and_ends_on_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8").to_string();
        let auth =
            crate::auth::AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
                .expect("auth");
        auth.create_superuser("admin".into(), "a long test password".into(), None)
            .await
            .expect("superuser");

        let cfg = Db {
            dir: dir.path(),
            key: None,
        };
        let out = dir.path().join("session");
        let message = mint(
            &cfg,
            &MintArgs {
                user: "admin".into(),
                out: out.clone(),
                ttl_secs: 3600,
            },
        )
        .expect("minted");
        assert!(message.contains("admin"), "{message}");
        let token = std::fs::read_to_string(&out).expect("token file");
        assert!(
            !message.contains(token.trim()),
            "the token never goes to the output"
        );
        let mode = std::fs::metadata(&out).expect("stat").mode() & 0o777;
        assert_eq!(mode, 0o600);

        let user = auth.validate(token.trim()).await.expect("logged in");
        assert_eq!(user.username, "admin");
        let storage = Storage::open(&dir.path().join("portal.db"), None).expect("db");
        let (session, _) = storage
            .get_session(&crate::auth::sha256_hex(token.trim()))
            .expect("read")
            .expect("session");
        assert!(
            session.expires_at <= unix_now() + 3600,
            "no sliding past the TTL"
        );

        revoke(&cfg, &out).expect("revoked");
        assert!(auth.validate(token.trim()).await.is_none(), "logged out");
        assert!(!out.exists(), "the token file goes with it");

        // LE-18: never over an existing token file.
        std::fs::write(&out, "old").expect("plant");
        assert!(mint(
            &cfg,
            &MintArgs {
                user: "admin".into(),
                out: out.clone(),
                ttl_secs: 3600,
            },
        )
        .is_err());
        std::fs::remove_file(&out).expect("clean");

        assert!(mint(
            &cfg,
            &MintArgs {
                user: "nobody".into(),
                out: out.clone(),
                ttl_secs: 3600,
            },
        )
        .is_err());
    }
}
