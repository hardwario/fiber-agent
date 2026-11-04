//! `ClusterJoin` — BLE FB1C's read and write, over the agent's one join path.
//!
//! `plan/control/ble/peripheral.md` §6.7, `plan/control/cluster.md` §8: the Manager app writes
//! the join bundle the Server's portal showed, last in the wizard. The write is **the same apply
//! path as `proximosctl control.radio create --role client`** ([`cluster_join::run`]): the
//! precondition, the bundle's validation, the hosts line, the CA fetch against `f`, the broker
//! test-connect, the writes with their `CLUSTER_JOIN` audit row (`origin: ble`), and the reboot.
//! This module only translates between `control-ble`'s wire types and the join's.

use std::sync::Arc;

use control_ble::host::{
    ClusterJoin, ClusterJoinOutcome, ClusterJoinRequest, ClusterJoinStatus, HubRole, Origin,
};

use crate::libs::cluster_join::{self, Bundle, JoinError, Joined, Secret};
use crate::libs::cluster_state::ClusterDir;
use crate::libs::config_applier::ConfigApplier;

/// What a write runs: the bundle and the surface's name, to the join's answer.
type Joiner = dyn Fn(&Bundle, &str) -> Result<Joined, JoinError> + Send + Sync;

pub struct ApplierClusterJoin {
    applier: Arc<ConfigApplier>,
    joiner: Arc<Joiner>,
}

impl ApplierClusterJoin {
    /// The binding the agent runs: this Hub's join, reboot included.
    pub fn new(applier: Arc<ConfigApplier>) -> ApplierClusterJoin {
        let for_join = applier.clone();
        ApplierClusterJoin::with_joiner(
            applier,
            Arc::new(move |bundle: &Bundle, origin: &str| {
                cluster_join::run(bundle, origin, &for_join)
            }),
        )
    }

    /// The same binding with another join behind it — a test's, which dials nothing.
    pub fn with_joiner(applier: Arc<ConfigApplier>, joiner: Arc<Joiner>) -> ApplierClusterJoin {
        ApplierClusterJoin { applier, joiner }
    }
}

fn origin_label(origin: Origin) -> &'static str {
    match origin {
        Origin::Cli => "cli",
        Origin::Ble => "ble",
        Origin::SignedMqtt => "signed-mqtt",
    }
}

impl ClusterJoin for ApplierClusterJoin {
    fn read(&self) -> Result<ClusterJoinStatus, String> {
        let s = cluster_join::status(&self.applier, &ClusterDir::new(self.applier.cluster_dir()))?;
        Ok(ClusterJoinStatus {
            role: s.client.then_some(HubRole::Client),
            joinable: s.joinable,
            server: s.server,
        })
    }

    fn join(&self, origin: Origin, req: &ClusterJoinRequest) -> ClusterJoinOutcome {
        let bundle = Bundle {
            v: req.v,
            h: req.h.clone(),
            p: Secret::new(req.p.clone()),
            f: req.f.clone(),
        };
        match (self.joiner)(&bundle, origin_label(origin)) {
            Ok(joined) => ClusterJoinOutcome::ok(joined.message()),
            Err(e) => ClusterJoinOutcome::refused(e.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::cluster_join::FailKind;
    use std::sync::Mutex;

    const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eA";

    fn hub(config: &str) -> (tempfile::TempDir, Arc<ConfigApplier>) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("fiber.config.yaml"), config).expect("seed");
        let applier = Arc::new(
            ConfigApplier::new(dir.path())
                .expect("applier")
                .with_cluster_dir(dir.path().join("cluster")),
        );
        (dir, applier)
    }

    fn request() -> ClusterJoinRequest {
        ClusterJoinRequest {
            v: 1,
            h: "proximos-1a2b3c4d.local".into(),
            p: PASSWORD.into(),
            f: "a".repeat(43),
        }
    }

    #[test]
    fn the_state_is_the_joins_status_on_the_wire() {
        let (dir, applier) = hub("system:\n  device_label: x\n");
        let seam = ApplierClusterJoin::new(applier.clone());
        assert_eq!(
            seam.read().unwrap(),
            ClusterJoinStatus {
                role: None,
                joinable: true,
                server: None
            }
        );

        // Setup complete as a Server: not joinable, still no role on FB1C.
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "setup:\n  provisioned: true\n  role: server\n",
        )
        .unwrap();
        assert_eq!(
            seam.read().unwrap(),
            ClusterJoinStatus {
                role: None,
                joinable: false,
                server: None
            }
        );

        // A joined Hub: the marker and the Server it names.
        let cluster = dir.path().join("cluster");
        std::fs::create_dir_all(&cluster).unwrap();
        std::fs::write(cluster.join("server_host"), "proximos-1a2b3c4d.local").unwrap();
        std::fs::write(cluster.join("role"), "client").unwrap();
        let wire = serde_json::to_string(&seam.read().unwrap()).unwrap();
        assert_eq!(
            wire, r#"{"role":"client","joinable":false,"server":"proximos-1a2b3c4d.local"}"#,
            "§6.7's second example"
        );

        // An unreadable config is "cannot answer", never "joinable".
        std::fs::write(dir.path().join("fiber.config.yaml"), "setup: [").unwrap();
        assert!(seam.read().is_err());
    }

    /// The write reaches the join with the bundle as written and `ble` as the origin, and the
    /// answer comes back as the outcome — the refusal's reason included, and no password.
    #[test]
    fn a_write_runs_the_one_join_path_with_the_ble_origin() {
        let (_dir, applier) = hub("{}\n");
        let seen: Arc<Mutex<Vec<(Bundle, String)>>> = Arc::default();
        let log = seen.clone();
        let seam = ApplierClusterJoin::with_joiner(
            applier,
            Arc::new(move |b: &Bundle, origin: &str| {
                log.lock().unwrap().push((b.clone(), origin.to_string()));
                if b.f.starts_with('a') {
                    Ok(Joined {
                        server_host: b.h.clone(),
                        written: vec![],
                    })
                } else {
                    Err(JoinError {
                        kind: FailKind::CaMismatch,
                        message: "the Server's CA does not match the bundle".into(),
                    })
                }
            }),
        );

        let ok = seam.join(Origin::Ble, &request());
        assert_eq!(
            ok,
            ClusterJoinOutcome::ok("joined proximos-1a2b3c4d.local; restarting")
        );
        let refused = seam.join(
            Origin::Ble,
            &ClusterJoinRequest {
                f: "b".repeat(43),
                ..request()
            },
        );
        assert!(!refused.success);
        assert_eq!(refused.message, "the Server's CA does not match the bundle");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let (bundle, origin) = &seen[0];
        assert_eq!(origin, "ble");
        assert_eq!(bundle.v, 1);
        assert_eq!(bundle.h, "proximos-1a2b3c4d.local");
        assert_eq!(bundle.p.expose(), PASSWORD);
        assert!(!format!("{bundle:?}").contains(PASSWORD));
    }

    /// Through the real join: FB1C does not check the radio mode (decision #47). Every mode,
    /// `off` included, passes to the bundle check — the version is what stops them here, before
    /// anything is dialled.
    #[test]
    fn the_real_join_does_not_check_the_radio_mode() {
        let _serial = cluster_join::test_serial();
        for mode in ["off", "lorawan", "p2p", "dual"] {
            let (dir, applier) = hub(&format!("radio:\n  mode: {mode}\n"));
            let out = ApplierClusterJoin::new(applier)
                .join(Origin::Ble, &ClusterJoinRequest { v: 2, ..request() });
            assert!(!out.success, "{mode}");
            assert!(
                out.message.contains("version 2"),
                "{mode} is not refused for its mode: {}",
                out.message
            );
            assert!(!dir.path().join("cluster").exists(), "{mode}");
        }
    }

    /// Through the real join: a Hub whose setup is complete is refused before anything is dialled
    /// or written, and the reason reaches the outcome.
    #[test]
    fn the_real_join_refuses_a_set_up_hub_and_writes_nothing() {
        let _serial = cluster_join::test_serial();
        let config = "setup:\n  provisioned: true\n  role: server\n";
        let (dir, applier) = hub(config);
        let out = ApplierClusterJoin::new(applier).join(Origin::Ble, &request());
        assert!(!out.success);
        assert!(out.message.contains("already complete"), "{}", out.message);
        assert!(!out.message.contains(PASSWORD));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("fiber.config.yaml")).unwrap(),
            config
        );
        assert!(!dir.path().join("cluster").exists());
    }
}
