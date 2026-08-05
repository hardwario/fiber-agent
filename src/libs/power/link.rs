//! Watching the Ethernet carrier for evidence that PoE was interrupted.
//!
//! Standby decides PoE has arrived by watching VIN, but VIN is *polled*, so it
//! can only notice an interruption it happens to sample. A quick unplug/replug
//! falls entirely between two polls: the watch sees DC present before and after,
//! concludes nothing arrived, and the device stays dark. That was the reported
//! bug — it only woke if the cable was left out for longer than one poll
//! interval.
//!
//! Fixing it needs an arming signal with *memory*, and the kernel already keeps
//! one. `/sys/class/net/<iface>/carrier_down_count` is monotonic, so an unplug of
//! even 200 ms is still visible at the next poll, however late that poll is. PoE
//! power and the link share one cable, so pulling it always drops carrier.
//!
//! What this proves and what it does not:
//!
//! * It proves the **link** was interrupted. It does not prove the **power** was:
//!   a switch reboot or a renegotiation can bounce carrier while PSE power stays
//!   up. So this only ever *arms* the watch — the resume still requires VIN to
//!   show power is actually present now.
//! * The residual false wake is therefore a link bounce that never cut power. It
//!   errs towards monitoring rather than towards a device that stays off, and the
//!   arming evidence is logged so it can be told apart after the fact.
//!
//! Removing that last case needs the southbridge to latch VIN dips itself, which
//! is a firmware change and a separate decision.

use std::fs;
use std::path::{Path, PathBuf};

use crate::libs::network::status::ETH_INTERFACES;

/// Monotonic count of carrier-down transitions on one interface.
#[derive(Debug, Clone)]
pub struct CarrierWatch {
    path: PathBuf,
    baseline: u64,
}

impl CarrierWatch {
    /// Find the Ethernet interface and take a baseline reading.
    ///
    /// `None` when no candidate exposes a readable counter — a kernel or
    /// interface that does not publish one, or a device on WiFi only. Callers
    /// treat that as "carrier evidence unavailable" and fall back to the VIN-dip
    /// path, which is what the feature had before.
    ///
    /// Selection is by the counter file existing rather than by the interface
    /// being up: when PoE is unplugged the link is down but the interface is
    /// still there, and that is exactly when this has to work.
    pub fn new() -> Option<Self> {
        for iface in ETH_INTERFACES {
            let path = Self::counter_path(Path::new("/sys/class/net"), iface);
            if let Some(count) = Self::read_count(&path) {
                eprintln!("[standby] Watching {} for carrier changes (count={count})", path.display());
                return Some(Self {
                    path,
                    baseline: count,
                });
            }
        }
        eprintln!(
            "[standby] No Ethernet carrier counter found (tried {}) — \
             a brief PoE interruption may go unnoticed",
            ETH_INTERFACES.join(", ")
        );
        None
    }

    /// Build against an explicit `/sys/class/net`-shaped root, for tests.
    pub fn with_root(root: &Path, iface: &str) -> Option<Self> {
        let path = Self::counter_path(root, iface);
        Self::read_count(&path).map(|baseline| Self { path, baseline })
    }

    fn counter_path(root: &Path, iface: &str) -> PathBuf {
        root.join(iface).join("carrier_down_count")
    }

    fn read_count(path: &Path) -> Option<u64> {
        fs::read_to_string(path)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    }

    /// Current counter value, or `None` if it has become unreadable (the
    /// interface was renamed or removed under us).
    pub fn count(&self) -> Option<u64> {
        Self::read_count(&self.path)
    }

    /// Whether carrier has dropped at least once since the baseline.
    ///
    /// A counter that has gone *backwards* means the interface was recreated and
    /// its count reset, which is itself a link interruption — so that counts too.
    pub fn bounced_since_baseline(&self) -> bool {
        match self.count() {
            Some(now) => now != self.baseline,
            // Unreadable: report no bounce rather than guessing. Claiming one
            // would arm the watch on nothing and could wake a device that was
            // deliberately switched off.
            None => false,
        }
    }

    /// Move the baseline to the current value, so past bounces stop counting.
    pub fn rebaseline(&mut self) {
        if let Some(now) = self.count() {
            self.baseline = now;
        }
    }

    pub fn baseline(&self) -> u64 {
        self.baseline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a `/sys/class/net`-shaped tree with one interface's counter.
    fn sysfs_with(count: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let iface = dir.path().join("eth0");
        fs::create_dir_all(&iface).unwrap();
        fs::write(iface.join("carrier_down_count"), count).unwrap();
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn an_unchanged_counter_is_not_a_bounce() {
        let (_d, root) = sysfs_with("3\n");
        let watch = CarrierWatch::with_root(&root, "eth0").expect("counter should be readable");

        assert_eq!(watch.baseline(), 3);
        assert!(!watch.bounced_since_baseline());
    }

    #[test]
    fn an_incremented_counter_is_a_bounce() {
        // The whole point: the unplug happened between two VIN polls, and this is
        // the only thing that remembers it.
        let (dir, root) = sysfs_with("3\n");
        let watch = CarrierWatch::with_root(&root, "eth0").unwrap();

        fs::write(dir.path().join("eth0").join("carrier_down_count"), "4\n").unwrap();
        assert!(watch.bounced_since_baseline());
    }

    #[test]
    fn a_counter_that_reset_still_counts_as_a_bounce() {
        // The interface was recreated, which is itself an interruption.
        let (dir, root) = sysfs_with("7\n");
        let watch = CarrierWatch::with_root(&root, "eth0").unwrap();

        fs::write(dir.path().join("eth0").join("carrier_down_count"), "0\n").unwrap();
        assert!(watch.bounced_since_baseline());
    }

    #[test]
    fn rebaselining_forgets_earlier_bounces() {
        let (dir, root) = sysfs_with("1\n");
        let mut watch = CarrierWatch::with_root(&root, "eth0").unwrap();

        fs::write(dir.path().join("eth0").join("carrier_down_count"), "5\n").unwrap();
        assert!(watch.bounced_since_baseline());

        watch.rebaseline();
        assert_eq!(watch.baseline(), 5);
        assert!(!watch.bounced_since_baseline());
    }

    #[test]
    fn a_missing_counter_yields_no_watch() {
        let dir = TempDir::new().unwrap();
        assert!(CarrierWatch::with_root(dir.path(), "eth0").is_none());
    }

    #[test]
    fn an_unparseable_counter_yields_no_watch() {
        let (_d, root) = sysfs_with("not a number\n");
        assert!(CarrierWatch::with_root(&root, "eth0").is_none());
    }

    #[test]
    fn a_counter_that_vanishes_reports_no_bounce() {
        // Reporting one would arm the watch on nothing and could wake a device
        // that was deliberately switched off.
        let (dir, root) = sysfs_with("2\n");
        let watch = CarrierWatch::with_root(&root, "eth0").unwrap();

        fs::remove_file(dir.path().join("eth0").join("carrier_down_count")).unwrap();
        assert!(watch.count().is_none());
        assert!(!watch.bounced_since_baseline());
    }
}
