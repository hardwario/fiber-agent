//! A cluster Client registers no Nodes — `plan/portal/control.radio.md`, amendment 2026-10-13.
//!
//! Nodes belong to the Server (decisions #8, #16, #40): it holds the AppKeys, the registry and
//! the P2P central. A Client that took a `node-add` would keep an AppKey nobody reads — on P2P
//! the Server then drops the Node's JoinReq and the registration "succeeds" with no Node ever
//! appearing. So `node-add`, `node-remove` and `node-move` are refused on a Client, from the CLI
//! ([`crate::dispatch::run`]) and from the agent's signed commands (control-core
//! `radio_nodes::run`). Nothing is relayed to the Server.
//!
//! The role is read the way the renderer and control-core's `cluster_state::ClusterDir` read
//! it: `/data/proximos/cluster/role` holding exactly `client` (trimmed). Decision #12 keeps this
//! crate free of workspace dependencies, so the two file names are repeated here.

use std::path::Path;

use crate::cli::Command;
use crate::error::{Component, Error, Kind, Result};

/// control-core's `cluster_state::CLUSTER_DIR`.
pub const CLUSTER_DIR: &str = "/data/proximos/cluster";
/// The role marker; `client` makes this Hub a Client, absence a Server.
const ROLE: &str = "role";
/// The Server this Client joined, as the join bundle named it.
const SERVER_HOST: &str = "server_host";
/// The marker's one Client value.
const ROLE_CLIENT: &str = "client";

/// Whether `command` is one of the verbs a Client refuses.
pub fn is_node_registration(command: &Command) -> bool {
    matches!(
        command,
        Command::NodeAdd(_) | Command::NodeRemove(_) | Command::NodeMove(_)
    )
}

/// Refuses a Node registration verb on a Client Hub, reading the cluster directory at `dir`.
///
/// `Ok(())` on a Server (no marker, or a marker that is not `client`). An unreadable marker is
/// refused too: a Hub that cannot tell whether it is a Client must not act as if it were sure it
/// is not (control-core `ClusterDir::role_marker`).
pub fn refuse_on_client_at(dir: &Path) -> Result<()> {
    let role = match std::fs::read_to_string(dir.join(ROLE)) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(Error::new(
                Kind::PermissionDenied,
                None,
                Component::NodeRegistry,
                format!(
                    "cannot read {}: {e}; Nodes are registered on the Server only",
                    dir.join(ROLE).display()
                ),
            ))
        }
    };
    if role.trim() != ROLE_CLIENT {
        return Ok(());
    }
    let server = std::fs::read_to_string(dir.join(SERVER_HOST))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this Client joined".to_string());
    Err(Error::new(
        Kind::PermissionDenied,
        None,
        Component::NodeRegistry,
        format!("this Hub is a cluster Client; Nodes are registered on the Server {server}"),
    ))
}

/// [`refuse_on_client_at`] on the shipped [`CLUSTER_DIR`].
pub fn refuse_on_client() -> Result<()> {
    refuse_on_client_at(Path::new(CLUSTER_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir() -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "control-radio-cluster-role-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        TempDir(dir)
    }

    #[test]
    fn a_server_is_not_refused() {
        let d = dir();
        assert!(refuse_on_client_at(d.path()).is_ok(), "no marker = Server");
        std::fs::write(d.path().join(ROLE), "server").unwrap();
        assert!(
            refuse_on_client_at(d.path()).is_ok(),
            "any other token = Server"
        );
        assert!(refuse_on_client_at(&d.path().join("absent")).is_ok());
    }

    #[test]
    fn a_client_is_refused_naming_its_server() {
        let d = dir();
        std::fs::write(d.path().join(ROLE), "client\n").unwrap();
        std::fs::write(d.path().join(SERVER_HOST), "proximos-4e970f4a.local").unwrap();
        let err = refuse_on_client_at(d.path()).expect_err("a Client refuses");
        assert_eq!(err.kind(), Kind::PermissionDenied);
        let text = err.to_string();
        assert!(
            text.contains("this Hub is a cluster Client; Nodes are registered on the Server proximos-4e970f4a.local"),
            "{text}"
        );
    }

    #[test]
    fn a_client_without_its_server_name_is_still_refused() {
        let d = dir();
        std::fs::write(d.path().join(ROLE), "client").unwrap();
        let err = refuse_on_client_at(d.path()).expect_err("a Client refuses");
        assert!(err.to_string().contains("cluster Client"), "{err}");
    }

    #[test]
    fn an_unreadable_marker_is_refused() {
        let d = dir();
        // A directory where the marker file should be: reading it fails with something other
        // than NotFound.
        std::fs::create_dir(d.path().join(ROLE)).unwrap();
        assert!(refuse_on_client_at(d.path()).is_err());
    }
}
