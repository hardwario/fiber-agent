//! `ExportDestination` — FB18's write, over `mqtt.export.destinations[]`.
//!
//! `plan/control/ble/peripheral.md` §6.4, GitLab #59. Both the BLE write and the signed
//! `export-destination-set` command land in `ConfigApplier::apply_export_destination`, so there is
//! no second way to write the section.
//!
//! **What this can honestly promise today.** A destination written here is *configuration*, not a
//! working export, and the result message says so rather than leaving the app to discover it:
//!
//! - `mqtt.export` has **no live reload** — the export thread takes a copy of the config at boot —
//!   so a saved destination applies at the next restart.
//! - TLS on 8883 needs the server Hub's CA at [`ConfigApplier::EXPORT_CA_PATH`] on *this* Hub,
//!   and nothing puts it there: the server Hub makes its own `/data/tls/ca.crt` at first boot
//!   (plan/control/cluster.md §6) and its mosquitto listens on 8883 with per-account logins
//!   (§5), but no account is minted (§7, milestone C2) and no join copies the CA here (§8, C4).
//!   That is `plan/portal/devices.hub.md` §12 row **H**, tracked there. Until it lands, this
//!   seam reports the CA as missing instead of pretending the destination will connect.

use std::path::PathBuf;
use std::sync::Arc;

use control_ble::host::{ExportDestination, ExportDestinationRequest, ExportOutcome, Origin};

use crate::libs::config_applier::ConfigApplier;

pub struct ApplierExportDestination {
    applier: Arc<ConfigApplier>,
    /// The path [`ApplierExportDestination::ca_present`] looks at — always
    /// [`ConfigApplier::EXPORT_CA_PATH`] in production, and the constructor is the only place that
    /// says so. Held rather than hard-coded because an absolute path under `/data` is not one a
    /// test can create, which left the "CA present" half of the §6.4 message untested: the only
    /// branch a dev machine could ever take was the missing one.
    ca_path: PathBuf,
}

impl ApplierExportDestination {
    pub fn new(applier: Arc<ConfigApplier>) -> ApplierExportDestination {
        ApplierExportDestination {
            applier,
            ca_path: PathBuf::from(ConfigApplier::EXPORT_CA_PATH),
        }
    }

    #[cfg(test)]
    fn with_ca_path(applier: Arc<ConfigApplier>, ca_path: PathBuf) -> ApplierExportDestination {
        ApplierExportDestination { applier, ca_path }
    }

    /// Is there a CA at the path the written destination points `ca_cert_path` at?
    ///
    /// Separate from the write because it is the one part of §6.4 that is about the *world* rather
    /// than about this Hub's config: the destination is written either way, and this only decides
    /// which half of the message the app reads. Empty counts as absent — a zero-byte PEM is a
    /// failed fetch, not a trust anchor.
    fn ca_present(&self) -> bool {
        self.ca_path
            .metadata()
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    }
}

impl ExportDestination for ApplierExportDestination {
    fn set(&self, origin: Origin, req: &ExportDestinationRequest) -> ExportOutcome {
        let result = self.applier.apply_export_destination(
            &req.host,
            &req.username,
            &req.password,
            req.enabled,
        );
        if !result.success {
            return ExportOutcome::refused(
                result
                    .error_message
                    .unwrap_or_else(|| "the export destination was refused".to_string()),
            );
        }

        let origin = match origin {
            Origin::Cli => "cli",
            Origin::Ble => "ble",
            Origin::SignedMqtt => "signed-mqtt",
        };
        eprintln!(
            "[export] destination set via {origin}: {} (enabled={})",
            req.host, req.enabled
        );

        // Two facts, both about timing rather than about the write, and both things the app would
        // otherwise have to find out by watching an export that never starts.
        if self.ca_present() {
            ExportOutcome::ok("destination saved; applies at the next restart")
        } else {
            ExportOutcome::ok(
                "destination saved; applies at the next restart. No CA is installed yet, so the \
                 TLS connection will not come up until one is",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "ble-export-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&base).expect("temp dir");
            std::fs::write(
                base.join("fiber.config.yaml"),
                "system:\n  device_label: bench\nmqtt:\n  export:\n    enabled: false\n    destinations:\n      - broker_id: local\n        enabled: true\n        host: localhost\n        port: 1883\n        username: ''\n        password: ''\n",
            )
            .expect("seed");
            TempDir(base)
        }
        fn seam(&self) -> ApplierExportDestination {
            ApplierExportDestination::new(Arc::new(ConfigApplier::new(&self.0).expect("applier")))
        }
        /// The same seam, but looking for the CA at `ca_path` instead of at `/data`.
        fn seam_with_ca(&self, ca_path: std::path::PathBuf) -> ApplierExportDestination {
            ApplierExportDestination::with_ca_path(
                Arc::new(ConfigApplier::new(&self.0).expect("applier")),
                ca_path,
            )
        }
        /// Every `.bak` the applier has written next to this config.
        fn backups(&self) -> Vec<std::path::PathBuf> {
            let Ok(entries) = std::fs::read_dir(self.0.join(".backups")) else {
                return Vec::new();
            };
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("bak"))
                .collect()
        }
        fn config(&self) -> serde_yaml::Value {
            serde_yaml::from_str(
                &std::fs::read_to_string(self.0.join("fiber.config.yaml")).expect("read back"),
            )
            .expect("parse")
        }
        fn destinations(&self) -> Vec<serde_yaml::Value> {
            self.config()["mqtt"]["export"]["destinations"]
                .as_sequence()
                .expect("a list")
                .clone()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn req(host: &str, username: &str, password: &str) -> ExportDestinationRequest {
        serde_json::from_value(serde_json::json!({
            "host": host, "username": username, "password": password,
        }))
        .expect("request")
    }

    #[test]
    fn the_destination_is_written_with_the_settled_contract_and_the_image_one_is_untouched() {
        let d = TempDir::new("write");
        assert!(
            d.seam()
                .set(Origin::Ble, &req("hub-1.local", "fiber", "s3cret"))
                .success
        );

        let dests = d.destinations();
        assert_eq!(dests.len(), 2, "the shipped `local` destination survives");
        assert_eq!(dests[0]["broker_id"].as_str(), Some("local"));
        assert_eq!(dests[0]["port"].as_u64(), Some(1883), "untouched");

        let remote = &dests[1];
        assert_eq!(remote["broker_id"].as_str(), Some("remote"));
        assert_eq!(remote["host"].as_str(), Some("hub-1.local"));
        assert_eq!(remote["port"].as_u64(), Some(8883), "not a parameter");
        assert_eq!(remote["username"].as_str(), Some("fiber"));
        assert_eq!(remote["tls"]["enabled"].as_bool(), Some(true));
        assert_eq!(remote["tls"]["insecure_skip_verify"].as_bool(), Some(false));
        assert!(!remote["tls"]["ca_cert_path"]
            .as_str()
            .expect("a path")
            .is_empty());
    }

    #[test]
    fn enabling_a_destination_enables_the_export_block_in_the_same_write() {
        // The `eye.enabled` lesson: a destination enabled inside a disabled export is a setting
        // that looks applied and does nothing.
        let d = TempDir::new("enable");
        assert_eq!(
            d.config()["mqtt"]["export"]["enabled"].as_bool(),
            Some(false)
        );
        assert!(
            d.seam()
                .set(Origin::Ble, &req("hub-1.local", "", ""))
                .success
        );
        assert_eq!(
            d.config()["mqtt"]["export"]["enabled"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn a_second_write_replaces_the_entry_in_place_rather_than_appending() {
        let d = TempDir::new("replace");
        let seam = d.seam();
        assert!(seam.set(Origin::Ble, &req("hub-1.local", "", "")).success);
        assert!(seam.set(Origin::Ble, &req("hub-2.local", "", "")).success);
        let dests = d.destinations();
        assert_eq!(dests.len(), 2, "one `local`, one `remote` — not three");
        assert_eq!(dests[1]["broker_id"].as_str(), Some("remote"));
        assert_eq!(dests[1]["host"].as_str(), Some("hub-2.local"));
    }

    #[test]
    fn a_password_without_a_username_is_refused_and_nothing_is_written() {
        let d = TempDir::new("anon");
        let before = d.destinations();
        assert!(d.backups().is_empty(), "the fixture starts clean");

        let outcome = d.seam().set(Origin::Ble, &req("hub-1.local", "", "s3cret"));
        assert!(!outcome.success);
        assert!(outcome.message.contains("anonymous"), "{}", outcome.message);
        assert_eq!(d.destinations(), before);

        // "Nothing is written" includes the backup. FB18 has no rate limit, so a refusal that
        // costs a `fiber.config.yaml.<secs>.bak` is a way to fill /data one file per second —
        // which is what `edit_config_yaml` did until it was reordered to validate first.
        assert!(
            d.backups().is_empty(),
            "a refused write must leave no backup: {:?}",
            d.backups()
        );
    }

    #[test]
    fn an_installed_ca_changes_the_message_and_nothing_else() {
        // The other half of §6.4's message. On a dev machine — and on a stock v2 Hub — the real
        // path is always absent, so without an injectable path this branch could not be reached.
        let d = TempDir::new("ca");
        let ca = d.0.join("export-ca.crt");
        std::fs::write(&ca, "-----BEGIN CERTIFICATE-----\n").expect("a CA to find");

        let outcome = d
            .seam_with_ca(ca)
            .set(Origin::Ble, &req("hub-1.local", "fiber", "s3cret"));
        assert!(outcome.success);
        assert!(outcome.message.contains("restart"), "{}", outcome.message);
        assert!(
            !outcome.message.contains("No CA"),
            "with a CA installed the message must not warn about a missing one: {}",
            outcome.message
        );
    }

    #[test]
    fn an_empty_ca_file_counts_as_no_ca_at_all() {
        // A zero-byte PEM is a failed fetch, not a trust anchor, and must read as the latter.
        let d = TempDir::new("emptyca");
        let ca = d.0.join("export-ca.crt");
        std::fs::write(&ca, "").expect("an empty file");

        let outcome = d
            .seam_with_ca(ca)
            .set(Origin::Ble, &req("hub-1.local", "", ""));
        assert!(outcome.success);
        assert!(outcome.message.contains("No CA"), "{}", outcome.message);
    }

    #[test]
    fn an_empty_host_is_refused() {
        let d = TempDir::new("nohost");
        let outcome = d.seam().set(Origin::Ble, &req("   ", "", ""));
        assert!(!outcome.success);
        assert!(outcome.message.contains("host"), "{}", outcome.message);
    }

    #[test]
    fn the_message_never_implies_a_running_export() {
        // No live reload exists, so "saved" must not read as "exporting".
        let d = TempDir::new("msg");
        let outcome = d.seam().set(Origin::Ble, &req("hub-1.local", "", ""));
        assert!(outcome.success);
        assert!(outcome.message.contains("restart"), "{}", outcome.message);
    }
}
