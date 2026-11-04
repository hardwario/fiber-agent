//! Atomic writes, modes, and name-based ownership.
//!
//! Every rendered file goes through [`write_atomic`]. Nothing in this crate ever opens a
//! destination path for writing directly.

use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::{Error, Ownership, Result};

/// Mode of every rendered file: readable by root and by the consuming service's group,
/// and by nobody else.
///
/// Not the directory's mode — the directories under `/run/proximos` are 0755, created by
/// each unit's `RuntimeDirectory=`, because world traversal of a directory reveals a
/// filename and nothing more. The file is where the secrets are: `db_key` in the portal's
/// config, the JWT signing `secret` in ChirpStack's.
const FILE_MODE: u32 = 0o640;

/// Mode of the generated `db.key` itself: 0600, and owned by the portal account rather
/// than root, because the portal reads it directly.
const KEY_MODE: u32 = 0o600;

/// Removes the temp file unless [`Written::keep`] has defused it.
///
/// A guard rather than a `match` on the error path, so a panic between the write and the
/// rename cannot leave a 0640 file containing a database passphrase under a name nothing
/// cleans up.
struct TempGuard(Option<PathBuf>);

impl TempGuard {
    fn keep(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_file(p);
        }
    }
}

/// Is this process root, and therefore able to `chown`?
///
/// Reported rather than assumed, and never fatal: a test suite and a hand-run render must
/// produce the same bytes as the device does, differing only in ownership.
pub(crate) fn ownership_state() -> Ownership {
    // SAFETY: `geteuid` takes no arguments, cannot fail, and touches no memory we own.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        Ownership::Applied
    } else {
        Ownership::Skipped(format!(
            "running as uid {euid}, not root: file modes were set, ownership was not"
        ))
    }
}

fn is_root() -> bool {
    matches!(ownership_state(), Ownership::Applied)
}

/// Resolve a group name to a gid.
///
/// `getgrnam` is not thread-safe (it returns a pointer into a static buffer). This crate
/// runs on one thread, from a `Type=oneshot` unit and from a sync `proximosctl`
/// subcommand — never from inside the tokio runtime, which is the reason the `render`
/// arm is deliberately placed before the runtime is built.
fn gid_for(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    // SAFETY: `c` outlives the call; the returned pointer is only dereferenced while it is
    // non-null and the gid is copied out before any other libc call can reuse the buffer.
    unsafe {
        let p = libc::getgrnam(c.as_ptr());
        if p.is_null() {
            None
        } else {
            Some((*p).gr_gid)
        }
    }
}

fn uid_for(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    // SAFETY: as above, for `getpwnam`.
    unsafe {
        let p = libc::getpwnam(c.as_ptr());
        if p.is_null() {
            None
        } else {
            Some((*p).pw_uid)
        }
    }
}

/// Write `contents` to `dest`, atomically, 0640 `root:<group>`.
///
/// Temp file in the **destination directory**, never `/tmp`: a rename is only atomic
/// within one filesystem, and `/run/proximos/<service>` is tmpfs while `/tmp` under
/// `PrivateTmp=` is somewhere else entirely. The temp name carries the pid, so a manual
/// `systemctl start` racing a boot-time run cannot clobber the other's half-written file.
///
/// A group that does not exist on this device is not an error. It means the consuming
/// package is not installed — a Hub built without the portal, say — and refusing to write
/// the file would turn a packaging choice into a failed boot unit.
pub(crate) fn write_atomic(dest: &Path, contents: &str, group: &str) -> Result<()> {
    let dir = dest.parent().ok_or_else(|| Error::Invalid {
        path: dest.to_path_buf(),
        message: "destination has no parent directory".to_string(),
    })?;
    create_dir_all(dir)?;

    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Invalid {
            path: dest.to_path_buf(),
            message: "destination has no file name".to_string(),
        })?;
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    let mut guard = TempGuard(Some(tmp.clone()));

    let write = |path: &Path| -> std::io::Result<()> {
        let mut f = fs::File::create(path)?;
        // Mode before content, not after: this file carries a database passphrase and a
        // JWT signing key, and a secret that is briefly world-readable on disk has
        // already leaked. `File::create` honours the umask, so it is set explicitly
        // rather than inherited from whatever started the process.
        f.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
        f.write_all(contents.as_bytes())?;
        // Durability is not the point on a tmpfs; ordering is. Flushing before the
        // rename keeps "the file exists" and "the file has content" from being two
        // separate observations for a consumer started in between.
        f.flush()?;
        f.sync_all()
    };
    write(&tmp).map_err(|source| Error::Write {
        path: tmp.clone(),
        source,
    })?;

    if is_root() {
        if let Some(gid) = gid_for(group) {
            std::os::unix::fs::chown(&tmp, Some(0), Some(gid)).map_err(|source| Error::Write {
                path: tmp.clone(),
                source,
            })?;
        }
    }

    fs::rename(&tmp, dest).map_err(|source| Error::Write {
        path: dest.to_path_buf(),
        source,
    })?;
    guard.keep();
    Ok(())
}

/// Write a generated secret to `dest`, atomically, 0600 `<user>:<user>`.
///
/// Separate from [`write_atomic`] because the mode and the owner are both different: this
/// file is read by the service account directly rather than through a group grant.
pub(crate) fn write_secret(dest: &Path, contents: &str, owner: &str) -> Result<()> {
    let dir = dest.parent().ok_or_else(|| Error::Invalid {
        path: dest.to_path_buf(),
        message: "destination has no parent directory".to_string(),
    })?;
    create_dir_all(dir)?;

    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Invalid {
            path: dest.to_path_buf(),
            message: "destination has no file name".to_string(),
        })?;
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    let mut guard = TempGuard(Some(tmp.clone()));

    let write = |path: &Path| -> std::io::Result<()> {
        let mut f = fs::File::create(path)?;
        f.set_permissions(fs::Permissions::from_mode(KEY_MODE))?;
        f.write_all(contents.as_bytes())?;
        f.flush()?;
        f.sync_all()
    };
    write(&tmp).map_err(|source| Error::Write {
        path: tmp.clone(),
        source,
    })?;

    if is_root() {
        if let Some(id) = uid_for(owner) {
            let gid = gid_for(owner).unwrap_or(id);
            std::os::unix::fs::chown(&tmp, Some(id), Some(gid)).map_err(|source| Error::Write {
                path: tmp.clone(),
                source,
            })?;
        }
    }

    fs::rename(&tmp, dest).map_err(|source| Error::Write {
        path: dest.to_path_buf(),
        source,
    })?;
    guard.keep();
    Ok(())
}

pub(crate) fn create_dir_all(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|source| Error::Write {
        path: dir.to_path_buf(),
        source,
    })
}

/// `mkdir -p` a `/data` directory and assert its mode and owner, as the shim did.
///
/// Both halves matter and neither is redundant. The `mkdir` defends against
/// `systemd-tmpfiles-setup.service` losing its race with the `nofail` `data.mount` — the
/// failure mode there is a `db.key` created inside the rootfs mount point that
/// "disappears" the moment the real `/data` lands on top of it. The `chown`/`chmod`
/// afterwards makes the ownership right regardless of which of the two won.
pub(crate) fn ensure_data_dir(dir: &Path, owner: &str, mode: u32) -> Result<()> {
    create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode)).map_err(|source| Error::Write {
        path: dir.to_path_buf(),
        source,
    })?;
    if is_root() {
        if let Some(id) = uid_for(owner) {
            let gid = gid_for(owner).unwrap_or(id);
            std::os::unix::fs::chown(dir, Some(id), Some(gid)).map_err(|source| Error::Write {
                path: dir.to_path_buf(),
                source,
            })?;
        }
    }
    Ok(())
}

/// Remove a rendered file that the current configuration no longer asks for.
///
/// Returns whether anything was there. Absence is the normal case and not an error: this
/// runs on every boot, and `/run` is a tmpfs that starts empty.
pub(crate) fn remove_stale(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Write {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Every `region_*.toml` currently sitting in `dir`, sorted.
///
/// ChirpStack is started with `-c <DIR>` and loads **every** `region_*.toml` it finds
/// there, so a file left behind by an earlier render with a different `radio.region` is
/// not inert — it is a second region definition with a different `topic_prefix`, which is
/// the exact silent-failure pairing this renderer exists to make impossible.
pub(crate) fn stale_region_files(dir: &Path, keep: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("region_") && name.ends_with(".toml") && name != keep {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}
