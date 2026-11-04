//! A whole device's worth of paths, in a temp directory.
//!
//! Every test drives the shipped templates — `portal/packaging/templates/`, the exact
//! tree WP-G installs to `/usr/share/proximos/templates/` — against an injected `/data`
//! and `/run`. Nothing here copies or paraphrases a template: a golden file that passed
//! against a paraphrase would prove nothing about the device.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use proximos_render::{Mode, RenderPaths};

/// The shipped templates tree, as installed to `/usr/share/proximos/templates/`.
pub fn templates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/templates")
}

pub struct Device {
    pub dir: tempfile::TempDir,
    pub paths: RenderPaths,
    /// What `proximosctl render` would have probed: the mode this device's hardware suggests
    /// (node_registry.md §12). A RAK5146 is fitted unless a test says otherwise.
    pub default_mode: Mode,
}

impl Device {
    /// A virgin device: `/data` present but empty, `/run` empty, no overlay, no secrets.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let data = dir.path().join("data");
        let run = dir.path().join("run/proximos");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        Self {
            paths: RenderPaths {
                templates: templates_dir(),
                data,
                run,
                control_config: None,
            },
            dir,
            default_mode: Mode::Lorawan,
        }
    }

    /// A Hub with no RAK5146: the hardware default is `p2p`.
    pub fn without_card(mut self) -> Self {
        self.default_mode = Mode::P2p;
        self
    }

    /// A device that has run migration `005-chirpstack-seed.sh` — i.e. one whose LoRaWAN
    /// chain can actually be configured. The value is the 64 hex characters that
    /// migration writes, without a trailing newline.
    pub fn with_api_secret(self) -> Self {
        self.write_data(
            "chirpstack/api-secret",
            "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
        )
    }

    /// The db.key a device that has already booted once carries. Fixed, so golden files
    /// can contain it.
    pub const SHIM_DB_KEY: &'static str =
        "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

    pub fn with_db_key(self) -> Self {
        self.write_data("portal/db.key", Self::SHIM_DB_KEY)
    }

    /// Write the per-device overlay, `/data/proximos/config/proximos.config.yaml`.
    pub fn with_overlay(self, yaml: &str) -> Self {
        self.write_data("proximos/config/proximos.config.yaml", yaml)
    }

    /// A Hub that has joined a Server (`plan/control/cluster.md` §3): the four value
    /// files, the CA, and `role` written last, each without a trailing newline, the way
    /// `ClusterState` writes them. Fixed values, so golden files can contain them.
    pub fn joined_as_client(self) -> Self {
        self.write_data("proximos/cluster/server_host", Self::CLIENT_SERVER_HOST)
            .write_data("proximos/cluster/server_port", "8883")
            .write_data("proximos/cluster/peer_username", Self::CLIENT_PEER_USERNAME)
            .write_data("proximos/cluster/peer_password", Self::CLIENT_PEER_PASSWORD)
            .write_data("proximos/cluster/ca.crt", "-----BEGIN CERTIFICATE-----\n")
            .write_data("proximos/cluster/role", "client")
    }

    pub const CLIENT_SERVER_HOST: &'static str = "proximos-1a2b3c4d.local";
    pub const CLIENT_PEER_USERNAME: &'static str = "peer-proximos-5e6f7a8b";
    pub const CLIENT_PEER_PASSWORD: &'static str = "Zm9vYmFyYmF6cXV4cXV1eA";

    pub fn write_data(self, rel: &str, contents: &str) -> Self {
        let p = self.paths.data.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
        self
    }

    pub fn render(&self) -> proximos_render::Result<proximos_render::Report> {
        proximos_render::render_all(&self.paths, self.default_mode)
    }

    pub fn run_file(&self, rel: &str) -> PathBuf {
        self.paths.run.join(rel)
    }

    pub fn read_run(&self, rel: &str) -> String {
        std::fs::read_to_string(self.run_file(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.run_file(rel).exists()
    }

    /// Every file currently under `/run`, relative and sorted. Used to assert that a
    /// failed render left no `.tmp.<pid>` debris behind.
    pub fn run_tree(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(&self.paths.run, &self.paths.run, &mut out);
        out.sort();
        out
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out);
        } else {
            out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
        }
    }
}

/// Compare against a golden file, with a diff-friendly failure and an escape hatch.
///
/// `UPDATE_GOLDEN=1 cargo test -p proximos-render` rewrites them. That is a convenience
/// for a deliberate template change, not a way to make a surprise go away — a golden diff
/// in a review is the point of having them.
pub fn assert_golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "golden {} is missing ({e}); UPDATE_GOLDEN=1 to create it",
            path.display()
        )
    });
    if expected != actual {
        panic!(
            "rendered output differs from {}\n--- expected ---\n{expected}\n--- actual ---\n{actual}",
            path.display()
        );
    }
}
