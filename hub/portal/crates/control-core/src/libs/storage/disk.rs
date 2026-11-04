//! Disk usage utilities for monitoring storage capacity

/// Disk partition usage statistics
#[derive(Debug, Clone, Copy)]
pub struct PartitionUsage {
    /// Total space in bytes
    pub total_bytes: u64,
    /// Available space in bytes
    pub available_bytes: u64,
    /// Used space percentage (0-100)
    pub used_percent: u8,
}

impl PartitionUsage {
    /// Create empty/unavailable partition usage
    pub fn unavailable() -> Self {
        Self {
            total_bytes: 0,
            available_bytes: 0,
            used_percent: 0,
        }
    }
}

/// Get disk usage for a given path via `statvfs`.
///
/// This used to shell out to `df -B1`, which silently produced
/// `PartitionUsage::unavailable()` on every Hub: the image ships no
/// `coreutils`, so `df` is BusyBox built without `CONFIG_FEATURE_DF_FANCY`,
/// where `-B SIZE` does not exist. It exited 1 with the usage text on stderr,
/// this function did not check the status, and the empty stdout parsed to
/// nothing — which the portal's Device tab rendered as "0.0 GB (100%)" and the
/// `StorageLow` alert read as 0% forever. `statvfs` is the same syscall `df`
/// itself calls, with no argv dialect to get wrong and no fork per poll.
///
/// Mirrors `portal/src/diskspace.rs`, which does this for the portal's own data
/// partition. That helper lives in the `portal` package rather than a shared
/// crate, so it cannot be called from here.
pub fn get_partition_usage(path: &str) -> PartitionUsage {
    let Ok(c_path) = std::ffi::CString::new(path) else {
        // Interior NUL — not a path any caller of ours has.
        return PartitionUsage::unavailable();
    };

    // SAFETY: `stat` is a plain POD out-parameter and `c_path` is a valid
    // NUL-terminated string that outlives the call.
    let stat = unsafe {
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return PartitionUsage::unavailable();
        }
        stat.assume_init()
    };

    let block_size = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    let total_bytes = block_size.saturating_mul(stat.f_blocks as u64);
    // `f_bavail` rather than `f_bfree`: the reserved-for-root blocks are not
    // ours to spend, so they are not "available" to report.
    let available_bytes = block_size
        .saturating_mul(stat.f_bavail as u64)
        .min(total_bytes);

    // Deliberately measured against the whole partition, which is NOT what
    // `df`'s Use% column reports: df divides by used+available, excluding
    // ext4's root reserve (5% by default), so it reads a few points lower.
    // This basis is the one that agrees with `available_bytes` above — the
    // Device tab shows both, as "N GB (100-used_percent %)", and the two
    // halves have to describe the same partition. Do not "fix" this back to
    // match a shell `df`.
    //
    // The u128 widening keeps `* 100` from overflowing on a filesystem larger
    // than ~1.8 EiB. Truncating rather than rounding means the >=90%
    // StorageLow threshold trips on a true 90.0%, not on 89.5%.
    let used_percent = if total_bytes > 0 {
        ((total_bytes - available_bytes) as u128 * 100 / total_bytes as u128) as u8
    } else {
        0
    };

    PartitionUsage {
        total_bytes,
        available_bytes,
        used_percent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_partition_usage_root() {
        // Test with root filesystem (should always exist)
        let usage = get_partition_usage("/");
        assert!(usage.total_bytes > 0);
        assert!(usage.used_percent <= 100);
    }

    #[test]
    fn test_get_partition_usage_invalid() {
        // `statvfs` fails with ENOENT, unlike the old `df`, which resolved a
        // missing path to the filesystem holding its nearest existing parent.
        //
        // Note what the caller gets: the sentinel is indistinguishable from a
        // genuine reading of an empty filesystem, so "measurement failed" and
        // "0 bytes free" look identical downstream. That is the known cost of
        // `PartitionUsage` using plain integers instead of `Option`.
        let usage = get_partition_usage("/nonexistent/path/xyz");
        assert_eq!(usage.total_bytes, 0);
        assert_eq!(usage.available_bytes, 0);
        assert_eq!(usage.used_percent, 0);
    }

    #[test]
    fn used_percent_is_measured_against_the_whole_partition() {
        // Regression guard for the percent basis: recompute it independently
        // from the two byte counts. A revert to df's used/(used+available)
        // would disagree on any ext4 filesystem with a root reserve.
        let usage = get_partition_usage("/");
        assert!(usage.available_bytes <= usage.total_bytes);

        let expected = ((usage.total_bytes - usage.available_bytes) as u128 * 100
            / usage.total_bytes as u128) as u8;
        assert_eq!(usage.used_percent, expected);
    }
}
