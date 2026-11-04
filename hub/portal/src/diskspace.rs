//! Free space on the filesystem holding a given path.
//!
//! Exists for one caller: the preflight before a full `VACUUM`, which needs
//! roughly twice the database's size in scratch space and must refuse rather
//! than fill `/data` half way. There is no free-space API in `std`, so this is
//! a thin `statvfs` wrapper.
//!
//! Landlock does not mediate `statvfs`, and the path this is called on is
//! inside the ruleset regardless, so confinement does not affect it.

/// Bytes available to an unprivileged process on the filesystem containing
/// `path`, or `None` when it cannot be determined.
///
/// `None` means "unknown", not "zero" — the caller proceeds, because refusing
/// maintenance on a platform we cannot measure would be worse than attempting
/// it. On a dev host (non-Linux) that is always the answer.
#[cfg(target_os = "linux")]
pub fn available_bytes(path: &std::path::Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `stat` is a plain POD out-parameter and `c_path` is a valid
    // NUL-terminated string that outlives the call.
    let stat = unsafe {
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return None;
        }
        stat.assume_init()
    };

    // `f_bavail` rather than `f_bfree`: the reserved-for-root blocks are not
    // ours to spend, and this process is deliberately not root.
    let block_size = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    (block_size as u64).checked_mul(stat.f_bavail as u64)
}

#[cfg(not(target_os = "linux"))]
pub fn available_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

/// How full the filesystem holding `path` is, in percent, as `df` counts it: used blocks over
/// used plus available to this process. `None` when it cannot be determined. HN-18 (Hub test
/// 2026-09-26): the "Portal storage low" system alert is evaluated on this, and nothing
/// measured it, so it could never fire.
#[cfg(target_os = "linux")]
pub fn used_pct(path: &std::path::Path) -> Option<f64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: as in `available_bytes`.
    let stat = unsafe {
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return None;
        }
        stat.assume_init()
    };
    let used = (stat.f_blocks as f64) - (stat.f_bfree as f64);
    let usable = used + stat.f_bavail as f64;
    (usable > 0.0).then(|| used / usable * 100.0)
}

#[cfg(not(target_os = "linux"))]
pub fn used_pct(_path: &std::path::Path) -> Option<f64> {
    None
}

// Both tests call statvfs, so the whole module is Linux-only; a plain
// `#[cfg(test)]` leaves `use super::*` unused on macOS and fails `-D warnings`.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// HN-18: the fill level of a real filesystem is a percentage; an unknown path is None.
    #[test]
    fn the_fill_level_is_a_percentage() {
        let pct = used_pct(std::path::Path::new("/")).expect("statvfs on /");
        assert!((0.0..=100.0).contains(&pct), "{pct}");
        assert_eq!(used_pct(std::path::Path::new("/no/such/path")), None);
    }

    #[test]
    fn reports_something_plausible_for_an_existing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let available = available_bytes(dir.path()).expect("statvfs on a real directory");
        assert!(available > 0, "a writable tempdir reported no free space");
    }

    #[test]
    fn missing_path_is_unknown_rather_than_zero() {
        assert_eq!(
            available_bytes(std::path::Path::new("/nonexistent-xyzzy")),
            None
        );
    }
}
