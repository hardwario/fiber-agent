//! `SetupState` — FB16's read and FB17's write, over the device's own config.
//!
//! The state lives in `setup:` in `fiber.config.yaml` and nowhere else
//! ([decision #4](../../../../../plan/decisions.md), `plan/control/ble/peripheral.md` §6.3). Two
//! consequences that are the point of putting it there rather than in the portal's database:
//!
//! - **A client-role Hub shuts its portal down.** v1 kept the equivalent (`app_mode`) in the
//!   viewer's database, so on exactly the Hubs the app most needs to ask about, the component that
//!   held the answer was not running.
//! - **A factory reset clears it by construction.** The wipe takes everything under `/data`, this
//!   file included, so a refurbished Hub cannot silently claim to be configured — no extra clearing
//!   step, and therefore no step that can be forgotten.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use control_ble::host::{HubRole, Origin, SetupOutcome, SetupState, SetupStatus};

use crate::libs::config::SetupRole;
use crate::libs::config_applier::ConfigApplier;

pub struct ApplierSetupState {
    applier: Arc<ConfigApplier>,
    config_path: PathBuf,
}

impl ApplierSetupState {
    pub fn new(applier: Arc<ConfigApplier>, config_dir: &Path) -> ApplierSetupState {
        ApplierSetupState {
            applier,
            config_path: config_dir.join("fiber.config.yaml"),
        }
    }
}

/// `control-ble`'s role type and this crate's are deliberately separate — the crate owns its wire
/// shape and this crate owns the config schema — so the one place they meet is here.
fn to_config_role(role: HubRole) -> SetupRole {
    match role {
        HubRole::Server => SetupRole::Server,
        HubRole::Client => SetupRole::Client,
    }
}

impl SetupState for ApplierSetupState {
    /// Read through the file rather than a cached `Config`, for the same reason FB0A's label does:
    /// the signed `setup-complete` command can write it while this process runs, and FB16 must not
    /// serve a value from boot.
    ///
    /// An unreadable or unparseable file is `Err` — "this Hub cannot answer" — never a defaulted
    /// `provisioned: false`, which would be a claim about the site made out of a failure to look.
    /// An **absent** `setup:` section is not that: it is a genuine "nobody has set this Hub up",
    /// which is what every config file written before 2026-09-16 says.
    fn read(&self) -> Result<SetupStatus, String> {
        let text = std::fs::read_to_string(&self.config_path)
            .map_err(|e| format!("cannot read the config file: {e}"))?;
        let doc: serde_yaml::Value = serde_yaml::from_str(&text)
            .map_err(|e| format!("cannot parse the config file: {e}"))?;

        let Some(section) = doc.get("setup") else {
            return Ok(SetupStatus::default());
        };
        if section.is_null() {
            return Ok(SetupStatus::default());
        }

        let provisioned = section
            .get("provisioned")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let role = match section.get("role").and_then(|v| v.as_str()) {
            Some("server") => Some(HubRole::Server),
            Some("client") => Some(HubRole::Client),
            // A role this build does not know is not a reason to refuse the whole read: the
            // `provisioned` half is still true and still useful. Since the 2026-09-16 review
            // `config::SetupSection` is lenient about the same value for the same reason, so
            // `system/info` and FB16 cannot disagree about a Hub whose role string is from
            // another build.
            _ => None,
        };
        Ok(SetupStatus { provisioned, role })
    }

    /// The refusal text is the applier's, unchanged: `apply_setup_complete` owns the role lock
    /// (`peripheral.md` §6.3 — fixed once provisioned, factory reset is the way back), and FB17's
    /// `{success:false, message}` envelope is only useful if the reason survives the trip.
    fn mark_complete(&self, origin: Origin, role: HubRole) -> SetupOutcome {
        let r = self.applier.apply_setup_complete(to_config_role(role));
        if r.success {
            tracing_setup_complete(origin, role);
            SetupOutcome::ok(format!("setup complete, role {}", role.as_str()))
        } else {
            SetupOutcome::refused(
                r.error_message
                    .unwrap_or_else(|| "the setup-complete write was refused".to_string()),
            )
        }
    }
}

/// One line, on the agent's own log, naming which surface did it. The audit row the applier writes
/// records *what* changed; this records *where it came from*, which is the question a service
/// engineer asks about a Hub that claims to be provisioned and nobody remembers setting up.
fn tracing_setup_complete(origin: Origin, role: HubRole) {
    let origin = match origin {
        Origin::Cli => "cli",
        Origin::Ble => "ble",
        Origin::SignedMqtt => "signed-mqtt",
    };
    eprintln!(
        "[setup] marked complete via {origin}, role {}",
        role.as_str()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str, body: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "ble-setup-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&base).expect("temp dir");
            std::fs::write(base.join("fiber.config.yaml"), body).expect("seed");
            TempDir(base)
        }
        fn state(&self) -> ApplierSetupState {
            let applier = Arc::new(
                ConfigApplier::new(&self.0)
                    .expect("applier")
                    .with_cluster_dir(self.0.join("cluster")),
            );
            ApplierSetupState::new(applier, &self.0)
        }
        /// The role marker a cluster join leaves (`plan/control/cluster.md` §3).
        fn joined(&self) -> &Self {
            std::fs::create_dir_all(self.0.join("cluster")).expect("cluster dir");
            std::fs::write(self.0.join("cluster/role"), "client").expect("marker");
            self
        }
        fn config(&self) -> String {
            std::fs::read_to_string(self.0.join("fiber.config.yaml")).expect("read back")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_config_with_no_setup_section_reads_as_nobody_has_set_this_hub_up() {
        // Every config file written before this section existed. It is a genuine answer, not a
        // failure, so it must not become an ATT error on FB16.
        let d = TempDir::new("absent", "system:\n  device_label: bench\n");
        assert_eq!(d.state().read().expect("readable"), SetupStatus::default());
    }

    #[test]
    fn an_unreadable_config_is_an_error_not_an_unprovisioned_hub() {
        let d = TempDir::new("gone", "system:\n  device_label: bench\n");
        std::fs::remove_file(d.0.join("fiber.config.yaml")).expect("remove");
        d.state().read().expect_err("cannot answer");
    }

    #[test]
    fn a_malformed_config_is_an_error_too() {
        let d = TempDir::new("bad", "system:\n  device_label: [unclosed\n");
        d.state().read().expect_err("cannot answer");
    }

    #[test]
    fn mark_complete_writes_both_fields_and_the_read_sees_them() {
        let d = TempDir::new("write", "system:\n  device_label: bench\n");
        let s = d.joined().state();
        assert!(s.mark_complete(Origin::Ble, HubRole::Client).success);
        assert_eq!(
            s.read().expect("readable"),
            SetupStatus {
                provisioned: true,
                role: Some(HubRole::Client),
            }
        );
        assert!(d.config().contains("provisioned: true"), "{}", d.config());
        assert!(d.config().contains("role: client"), "{}", d.config());
    }

    #[test]
    fn re_writing_the_same_role_is_idempotent() {
        // Re-running the wizard's last step is ordinary — a retried write, a second phone — and
        // must not need a clearing step first.
        let d = TempDir::new("idem", "setup:\n  provisioned: false\n");
        let s = d.state();
        assert!(s.mark_complete(Origin::Ble, HubRole::Server).success);
        assert!(s.mark_complete(Origin::Ble, HubRole::Server).success);
        assert!(s.mark_complete(Origin::SignedMqtt, HubRole::Server).success);
        assert_eq!(
            s.read().expect("readable"),
            SetupStatus {
                provisioned: true,
                role: Some(HubRole::Server),
            }
        );
    }

    #[test]
    fn a_hub_nobody_has_set_up_yet_may_pick_either_role() {
        // The lock below is about *changing* a choice, not about making one. Both roles must still
        // be reachable out of the box, or the wizard has nothing to offer.
        for role in [HubRole::Server, HubRole::Client] {
            let d = TempDir::new("pick", "system:\n  device_label: bench\n");
            if role == HubRole::Client {
                d.joined();
            }
            let s = d.state();
            assert!(s.mark_complete(Origin::Ble, role).success);
            assert_eq!(s.read().expect("readable").role, Some(role));
        }
    }

    #[test]
    fn the_role_cannot_be_changed_once_the_hub_is_provisioned() {
        // Re-roling used to be "last write wins", which made FB19's client-role gate a formality:
        // the same authenticated peer wrote FB17 `server` and then read the credential it had just
        // been refused. The refusal must reach the FB17 envelope with the way back in it.
        let d = TempDir::new("locked", "setup:\n  provisioned: false\n");
        let s = d.joined().state();
        assert!(s.mark_complete(Origin::Ble, HubRole::Client).success);

        let refused = s.mark_complete(Origin::SignedMqtt, HubRole::Server);
        assert!(!refused.success, "{}", refused.message);
        assert!(
            refused.message.contains("factory reset"),
            "the refusal names the documented way back (peripheral.md §6.3): {}",
            refused.message
        );
        assert_eq!(
            s.read().expect("readable").role,
            Some(HubRole::Client),
            "a refused re-role leaves the file alone"
        );
    }

    #[test]
    fn a_role_this_build_does_not_know_still_counts_as_a_role_for_the_lock() {
        // Otherwise a firmware downgrade would be the one way to unlock re-roling: the stale value
        // reads back as `None` and a naive check would call that "no role chosen yet".
        let d = TempDir::new("stale", "setup:\n  provisioned: true\n  role: arbiter\n");
        let s = d.state();
        let refused = s.mark_complete(Origin::Ble, HubRole::Server);
        assert!(!refused.success, "{}", refused.message);
        assert!(
            refused.message.contains("arbiter"),
            "the refusal quotes what the file actually says: {}",
            refused.message
        );
    }

    #[test]
    fn a_hub_provisioned_without_a_role_may_still_choose_one() {
        // A hand-edited file, or one written before the role existed. There is no earlier choice
        // to contradict, so there is nothing to protect.
        let d = TempDir::new("noroleyet", "setup:\n  provisioned: true\n");
        let s = d.joined().state();
        assert!(s.mark_complete(Origin::Ble, HubRole::Client).success);
        assert_eq!(s.read().expect("readable").role, Some(HubRole::Client));
    }

    /// FB17 `client` on a Hub that has not joined a Server is refused (cluster.md §3), and the
    /// applier's reason reaches the envelope unchanged.
    #[test]
    fn the_client_role_is_refused_without_the_join_marker() {
        let d = TempDir::new("nomarker", "system:\n  device_label: bench\n");
        let s = d.state();
        let refused = s.mark_complete(Origin::Ble, HubRole::Client);
        assert!(!refused.success);
        assert!(
            refused.message.contains("has not joined a Server"),
            "{}",
            refused.message
        );
        assert_eq!(s.read().expect("readable"), SetupStatus::default());
    }

    #[test]
    fn a_null_setup_section_is_treated_as_absent_rather_than_refused() {
        // What an operator gets by deleting the two lines under the key but leaving the key.
        let d = TempDir::new("null", "setup:\n");
        let s = d.state();
        assert_eq!(s.read().expect("readable"), SetupStatus::default());
        assert!(s.mark_complete(Origin::Ble, HubRole::Server).success);
        assert!(s.read().expect("readable").provisioned);
    }

    #[test]
    fn a_role_this_build_does_not_know_still_reports_provisioned() {
        // Half an answer beats none: the flag is the half the app gates its wizard on.
        let d = TempDir::new("future", "setup:\n  provisioned: true\n  role: arbiter\n");
        assert_eq!(
            d.state().read().expect("readable"),
            SetupStatus {
                provisioned: true,
                role: None,
            }
        );
    }

    #[test]
    fn the_write_leaves_the_rest_of_the_config_alone() {
        let d = TempDir::new(
            "rest",
            "system:\n  device_label: bench\neye:\n  enabled: true\n",
        );
        assert!(
            d.state()
                .mark_complete(Origin::Ble, HubRole::Server)
                .success
        );
        let after = d.config();
        assert!(after.contains("device_label: bench"), "{after}");
        assert!(after.contains("enabled: true"), "{after}");
    }
}
