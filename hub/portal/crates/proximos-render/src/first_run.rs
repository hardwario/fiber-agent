//! `proximosctl first-run-apply` — the one writer of `radio.mode` into the device overlay
//! (decision #37, `plan/system/design.md` §6.1).
//!
//! The portal's first-run `/setup` page cannot write the overlay: it runs sandboxed as
//! `proximos` with `/data/portal` as its only writable tree. So it writes a **request**,
//! `/data/portal/requested-radio-mode`, and `proximos-first-run.path` starts
//! `proximos-first-run.service`, which runs this as root. Everything that makes the
//! request safe to honour is decided here, not in the portal:
//!
//! * Only `p2p` and `lorawan` are accepted. `off` and `dual` stay provisioning-only.
//! * **Once.** An overlay that already carries `radio.mode` is final. A later request is
//!   refused. Switching transport after first run is a factory reset (devices.hub.md
//!   §3.4), and a reset wipes the overlay, which is what reopens this door.
//! * The overlay about to be written is validated by the renderer's own loader first, so
//!   this can never leave behind an overlay the next boot's render would refuse.
//!
//! * The request itself is untrusted: the portal owns the directory it sits in. It is
//!   read only as a regular file of at most [`MAX_REQUEST_BYTES`] of UTF-8, opened without
//!   following a symlink and without blocking. Anything else (a symlink, a FIFO, a
//!   directory, an oversized file) is refused unread, so a compromised portal cannot make
//!   root read another file into the journal, fill memory, or hang this unit, which the
//!   renderer and so the whole boot wait for.
//!
//! The request is removed on every outcome except an I/O failure, so the `.path` unit
//! does not re-trigger on a request that has already been answered.
//!
//! This module never reboots. It reports whether the new mode differs from the rendered
//! one ([`Outcome::Applied::reboot`]), and the unit acts on the exit code.

use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

use crate::config::{self, Effective};
use crate::{data_paths, fsutil, read_optional, run_paths, Error, Mode, RenderPaths, Result};

/// The longest request read. `lorawan\n` is 8 bytes; anything near this is not a request.
pub const MAX_REQUEST_BYTES: u64 = 64;

/// What one `first-run-apply` run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No request file. The ordinary case at every boot.
    NoRequest,
    /// `radio.mode` was written into the overlay.
    Applied {
        mode: Mode,
        /// `true` when a rendered control config exists and names a different mode, so
        /// the running services are on the old one. `false` at early boot (nothing
        /// rendered yet: the render that follows picks the new mode up) and when the mode
        /// did not change.
        reboot: bool,
    },
    /// The request was refused and removed. `reason` is for the journal.
    Refused { requested: String, reason: String },
}

/// The overlay's own path.
pub fn overlay_path(paths: &RenderPaths) -> PathBuf {
    paths.data.join(data_paths::OVERLAY)
}

/// The portal's request file.
pub fn request_path(paths: &RenderPaths) -> PathBuf {
    paths.data.join(data_paths::RADIO_MODE_REQUEST)
}

/// Read the request, answer it, remove it.
pub fn apply(paths: &RenderPaths) -> Result<Outcome> {
    let request = request_path(paths);
    let outcome = match read_request(&request)? {
        None => return Ok(Outcome::NoRequest),
        Some(Ok(raw)) => decide(paths, raw.trim())?,
        Some(Err(reason)) => Outcome::Refused {
            requested: "<unread>".to_string(),
            reason,
        },
    };
    remove_request(&request)?;
    Ok(outcome)
}

/// Read the request the way an untrusted file has to be read by root: `None` when there
/// is none, `Some(Err(reason))` when what is there is not a small regular file, which is
/// then refused without reading it. `O_NOFOLLOW` fails a symlink with `ELOOP`;
/// `O_NONBLOCK` makes opening a FIFO return at once instead of waiting for a writer.
fn read_request(path: &Path) -> Result<Option<std::result::Result<String, String>>> {
    let read_err = |source| Error::Read {
        path: path.to_path_buf(),
        source,
    };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Ok(Some(Err("the request is a symlink".to_string())))
        }
        Err(e) => return Err(read_err(e)),
    };
    if !file.metadata().map_err(read_err)?.is_file() {
        return Ok(Some(Err("the request is not a regular file".to_string())));
    }
    let mut bytes = Vec::new();
    file.take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(read_err)?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Ok(Some(Err(format!(
            "the request is longer than {MAX_REQUEST_BYTES} bytes"
        ))));
    }
    Ok(Some(
        String::from_utf8(bytes).map_err(|_| "the request is not UTF-8".to_string()),
    ))
}

fn decide(paths: &RenderPaths, requested: &str) -> Result<Outcome> {
    let refuse = |reason: String| {
        Ok(Outcome::Refused {
            requested: requested.to_string(),
            reason,
        })
    };

    let mode = match requested {
        "p2p" => Mode::P2p,
        "lorawan" => Mode::Lorawan,
        _ => return refuse("only `p2p` and `lorawan` can be chosen at first run".to_string()),
    };

    let overlay_file = overlay_path(paths);
    let mut overlay = match read_optional(&overlay_file)? {
        Some(text) => config::parse(&overlay_file, &text)?,
        None => Value::Mapping(Mapping::new()),
    };
    if let Some(existing) = overlay.get("radio").and_then(|r| r.get("mode")) {
        let existing = existing.as_str().unwrap_or("<not a string>");
        return refuse(format!(
            "{} already sets radio.mode: {existing}; after first run the transport changes \
             only by factory reset",
            overlay_file.display()
        ));
    }

    set_mode(&mut overlay, mode, &overlay_file)?;
    // Validate the candidate as the next render will see it, before it touches the disk.
    // The overlay now names a mode, so the hardware default passed here is never reached.
    Effective::load_with_overlay(paths, Some(overlay.clone()), mode)?;

    let text = serde_yaml::to_string(&overlay).map_err(|source| Error::Config {
        path: overlay_file.clone(),
        source,
    })?;
    fsutil::write_atomic(&overlay_file, &format!("{OVERLAY_HEADER}{text}"), "root")?;

    let reboot = rendered_mode(paths).is_some_and(|rendered| rendered != mode.as_str());
    Ok(Outcome::Applied { mode, reboot })
}

/// Written at the top of the overlay every time this rewrites it. `serde_yaml` does not
/// keep comments, so an overlay edited by hand before first run loses its own; nothing
/// writes one today, and this says where the file came from.
const OVERLAY_HEADER: &str = "\
# ProXimos device overlay, merged over /usr/share/proximos/templates/defaults.yaml by
# `proximosctl render` (plan/system/design.md §7). radio.mode was set by first-run setup
# (decision #37). It changes only by factory reset.
";

fn set_mode(overlay: &mut Value, mode: Mode, path: &Path) -> Result<()> {
    let invalid = |message: &str| Error::Invalid {
        path: path.to_path_buf(),
        message: message.to_string(),
    };
    let root = overlay
        .as_mapping_mut()
        .ok_or_else(|| invalid("the overlay is not a mapping"))?;
    let radio = root
        .entry(Value::String("radio".to_string()))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if radio.is_null() {
        *radio = Value::Mapping(Mapping::new());
    }
    radio
        .as_mapping_mut()
        .ok_or_else(|| invalid("`radio:` in the overlay is not a mapping"))?
        .insert(
            Value::String("mode".to_string()),
            Value::String(mode.as_str().to_string()),
        );
    Ok(())
}

/// The mode the running services were rendered with, from the control config this
/// boot's render wrote. `None` when there is no rendered config yet, which is the
/// early-boot case: the request is then applied before the render, and nothing needs a
/// reboot. An unreadable or odd file answers `Some("<unknown>")`, which never equals a
/// real mode and so asks for the reboot that makes the rendered state consistent again.
fn rendered_mode(paths: &RenderPaths) -> Option<String> {
    let file = paths
        .control_config
        .clone()
        .unwrap_or_else(|| paths.run.join(run_paths::CONTROL.0));
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some("<unknown>".to_string()),
    };
    let mode = serde_yaml::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("radio")?.get("mode")?.as_str().map(str::to_string));
    Some(mode.unwrap_or_else(|| "<unknown>".to_string()))
}

/// Remove the request, whatever it turned out to be. A symlink or FIFO goes with
/// `remove_file` (the link, never its target); a directory the portal put there needs
/// `remove_dir_all`, which does not follow symlinks inside it.
fn remove_request(path: &Path) -> Result<()> {
    let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let removed = if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Write {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp `/data` + `/run` with the shipped templates, as the render tests use them.
    struct Roots {
        _dir: tempfile::TempDir,
        paths: RenderPaths,
    }

    fn roots() -> Roots {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let run = dir.path().join("run");
        std::fs::create_dir_all(data.join("portal")).unwrap();
        // So a lorawan render really writes chirpstack.toml, and a p2p one really has to
        // remove it — without the secret both would just skip it.
        std::fs::create_dir_all(data.join("chirpstack")).unwrap();
        std::fs::write(data.join(data_paths::API_SECRET), "test-secret\n").unwrap();
        std::fs::create_dir_all(&run).unwrap();
        let paths = RenderPaths {
            templates: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packaging/templates"),
            data,
            run,
            control_config: None,
        };
        Roots { _dir: dir, paths }
    }

    fn request(r: &Roots, body: &str) {
        std::fs::write(request_path(&r.paths), body).unwrap();
    }

    /// The hardware default for these tests: a Hub with the concentrator, which runs
    /// LoRaWAN until a mode is chosen.
    const DEFAULT_MODE: Mode = Mode::Lorawan;

    fn render(r: &Roots) {
        crate::render_all(&r.paths, DEFAULT_MODE).unwrap();
    }

    fn overlay(r: &Roots) -> Option<String> {
        std::fs::read_to_string(overlay_path(&r.paths)).ok()
    }

    fn effective_mode(r: &Roots) -> Mode {
        Effective::load(&r.paths, DEFAULT_MODE).unwrap().mode
    }

    #[test]
    fn no_request_is_a_no_op() {
        let r = roots();
        assert_eq!(apply(&r.paths).unwrap(), Outcome::NoRequest);
        assert!(overlay(&r).is_none());
    }

    #[test]
    fn a_request_before_the_render_is_applied_without_a_reboot() {
        // The boot-time path: a request left behind by a power cut, nothing rendered yet.
        let r = roots();
        request(&r, "p2p\n");
        assert_eq!(
            apply(&r.paths).unwrap(),
            Outcome::Applied {
                mode: Mode::P2p,
                reboot: false
            }
        );
        assert!(!request_path(&r.paths).exists(), "the request is consumed");
        assert_eq!(effective_mode(&r), Mode::P2p);
    }

    #[test]
    fn a_changed_mode_after_the_render_asks_for_a_reboot() {
        let r = roots();
        render(&r); // the hardware default, lorawan
        assert!(r.paths.run.join(run_paths::CHIRPSTACK_TOML.0).exists());
        request(&r, "p2p\n");
        assert_eq!(
            apply(&r.paths).unwrap(),
            Outcome::Applied {
                mode: Mode::P2p,
                reboot: true
            }
        );
        // And the next boot's render really is P2P: no ChirpStack config.
        render(&r);
        assert!(!r.paths.run.join(run_paths::CHIRPSTACK_TOML.0).exists());
        let control = std::fs::read_to_string(r.paths.run.join(run_paths::CONTROL.0)).unwrap();
        assert!(control.contains("mode: p2p"), "{control}");
    }

    #[test]
    fn choosing_the_rendered_mode_pins_it_without_a_reboot() {
        let r = roots();
        render(&r);
        request(&r, "lorawan");
        assert_eq!(
            apply(&r.paths).unwrap(),
            Outcome::Applied {
                mode: Mode::Lorawan,
                reboot: false
            }
        );
        assert!(overlay(&r).unwrap().contains("mode: lorawan"));
    }

    #[test]
    fn a_second_request_is_refused_once_the_mode_is_set() {
        let r = roots();
        request(&r, "p2p");
        apply(&r.paths).unwrap();
        request(&r, "lorawan");
        let out = apply(&r.paths).unwrap();
        assert!(
            matches!(&out, Outcome::Refused { requested, reason }
                if requested == "lorawan" && reason.contains("factory reset")),
            "{out:?}"
        );
        assert!(
            !request_path(&r.paths).exists(),
            "a refused request is removed too"
        );
        assert_eq!(effective_mode(&r), Mode::P2p);
    }

    #[test]
    fn off_dual_and_garbage_are_refused_and_removed() {
        for value in ["off", "dual", "LoRaWAN", "", "p2p lorawan"] {
            let r = roots();
            request(&r, value);
            let out = apply(&r.paths).unwrap();
            assert!(matches!(out, Outcome::Refused { .. }), "{value:?}: {out:?}");
            assert!(!request_path(&r.paths).exists());
            assert!(overlay(&r).is_none(), "{value:?} wrote an overlay");
        }
    }

    #[test]
    fn an_existing_overlay_keeps_its_other_keys() {
        let r = roots();
        let file = overlay_path(&r.paths);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "radio:\n  region: eu868\nportal:\n  cookie_secure: false\n",
        )
        .unwrap();
        request(&r, "p2p");
        apply(&r.paths).unwrap();
        let eff = Effective::load(&r.paths, DEFAULT_MODE).unwrap();
        assert_eq!(eff.mode, Mode::P2p);
        assert!(!eff.portal.cookie_secure);
    }

    #[test]
    fn an_overlay_pinned_by_the_migration_refuses_the_request() {
        // What 040-config-init.sh writes on a Hub that was set up before decision #37.
        let r = roots();
        let file = overlay_path(&r.paths);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "radio:\n  mode: lorawan\n").unwrap();
        request(&r, "p2p");
        assert!(matches!(apply(&r.paths).unwrap(), Outcome::Refused { .. }));
        assert_eq!(effective_mode(&r), Mode::Lorawan);
    }

    #[test]
    fn a_broken_overlay_fails_and_keeps_the_request() {
        // Not refused: the operator's choice has not been answered, and the overlay needs
        // a human. The unit fails visibly, and the request stays for the next attempt.
        let r = roots();
        let file = overlay_path(&r.paths);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "radio: [unclosed\n").unwrap();
        request(&r, "p2p");
        assert!(apply(&r.paths).is_err());
        assert!(request_path(&r.paths).exists());
    }

    /// Every hostile shape of the request is refused unread, removed, and writes nothing.
    fn assert_refused_unread(r: &Roots, what: &str) {
        let out = apply(&r.paths).unwrap();
        assert!(
            matches!(&out, Outcome::Refused { requested, .. } if requested == "<unread>"),
            "{what}: {out:?}"
        );
        assert!(
            std::fs::symlink_metadata(request_path(&r.paths)).is_err(),
            "{what}: the request is removed"
        );
        assert!(overlay(r).is_none(), "{what}: wrote an overlay");
    }

    #[test]
    fn a_symlinked_request_is_refused_and_its_target_is_left_alone() {
        // Root must not read, and so print to the journal, a file the portal points at.
        let r = roots();
        let target = r.paths.data.join("secret");
        std::fs::write(&target, "p2p\n").unwrap();
        std::os::unix::fs::symlink(&target, request_path(&r.paths)).unwrap();
        assert_refused_unread(&r, "symlink");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "p2p\n");
    }

    #[test]
    fn a_fifo_request_is_refused_without_blocking() {
        // Blocking here would hold proximos-render, and with it the portal, on every boot.
        let r = roots();
        let path =
            std::ffi::CString::new(request_path(&r.paths).as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);
        assert_refused_unread(&r, "fifo");
    }

    #[test]
    fn a_directory_request_is_refused_and_removed() {
        let r = roots();
        std::fs::create_dir_all(request_path(&r.paths).join("sub")).unwrap();
        assert_refused_unread(&r, "directory");
    }

    #[test]
    fn an_oversized_or_non_utf8_request_is_refused() {
        let r = roots();
        std::fs::write(request_path(&r.paths), vec![b'p'; 4096]).unwrap();
        assert_refused_unread(&r, "oversized");

        let r = roots();
        std::fs::write(request_path(&r.paths), [0xff, 0xfe, b'\n']).unwrap();
        assert_refused_unread(&r, "non-UTF-8");
    }

    #[test]
    fn a_request_of_exactly_the_limit_is_read() {
        // Read, then refused on its value: the limit is not off by one.
        let r = roots();
        let body = format!("p2p{}", " ".repeat(MAX_REQUEST_BYTES as usize - 3));
        request(&r, &body);
        assert_eq!(
            apply(&r.paths).unwrap(),
            Outcome::Applied {
                mode: Mode::P2p,
                reboot: false
            }
        );
    }
}
