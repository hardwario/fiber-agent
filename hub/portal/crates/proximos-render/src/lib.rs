//! `proximosctl render` — the configuration renderer of `plan/system/design.md` §7.
//!
//! One process, run once per boot by `proximos-render.service`, before every consumer.
//! It reads immutable templates from the rootfs and state and secrets from `/data`, and
//! writes rendered configuration into `/run/proximos/<service>/`. It **never starts,
//! stops or reloads a unit**, and it writes no systemd drop-in — the v1
//! firstboot ↔ cluster-apply deadlock class (a oneshot blocking on `systemctl restart`
//! of a unit ordered `After=` itself) cannot recur because no boot-path unit here ever
//! calls systemctl. Ordering is static and lives in the units.
//!
//! # What it replaces
//!
//! `system/meta-proximos/recipes-core/proximos-render/files/proximos-render.sh`, a POSIX
//! shell script that wrote one of these four files. Its output for
//! `/run/proximos/portal/config.yaml` is reproduced key for key, in the same order, with
//! the same quoting — `tests/shim_parity.rs` holds that script's exact output as a
//! fixture and diffs against it. WP-G deletes the shim only after that parity is proven.
//!
//! Three things the shim could not do, and this does:
//!
//! * **A typed merge instead of concatenation.** The shim appended the override file
//!   after the block it had just written and relied on the vendored serde_yaml resolving
//!   a repeated mapping key last-wins — true of that version, guaranteed by neither the
//!   YAML spec nor a future upgrade. Here both documents are parsed, deep-merged, and
//!   deserialized into `deny_unknown_fields` structs, so a misspelled override key is a
//!   render failure naming the key rather than a setting that silently does nothing.
//! * **The other three consumers.** `/run/proximos/control/config.yaml` had no producer
//!   at all; nothing on any device has ever rendered it, and radio-setup.service,
//!   `proximosctl control.radio` and (WP-D) proximos-control.service all read it.
//! * **The mode gate.** ChirpStack and the MQTT forwarder are configured only when
//!   `radio.mode ∈ {lorawan, dual}`; otherwise their files are absent, and a previously
//!   rendered pair is removed. Both units carry `ConditionPathExists=` on exactly those
//!   paths, so absence is how a Hub says "not asked for" instead of "broken".
//! * **The role gate** (`plan/control/cluster.md` §4). A client-role Hub gets no portal
//!   config and no ChirpStack pair (and so no redis, whose drop-in is gated on the same
//!   file); its forwarder is retargeted at the Server's TLS listener, and the control
//!   config carries the agent's `mqtt.broker` + `mqtt.tls` override, so the seeded
//!   `fiber.config.yaml` is never edited. Only a Server gets mosquitto's 8883 listener
//!   (`/run/proximos/mosquitto/8883.conf`), and with it the open firewall port; a Client
//!   broker is loopback-only.
//!
//! # Failure policy
//!
//! Ordered by what it costs to be wrong, and shaped by a bug the shim shipped: when this
//! process exits non-zero, systemd marks `proximos-render.service` failed, tears down its
//! `RuntimeDirectory=`, and **deletes the configuration this process had just written**.
//! The observed symptom of that was a 502 from nginx with a journal line saying the
//! render had succeeded.
//!
//! So:
//!
//! * A missing or unreadable **input on `/data`** (no api-secret yet, no overlay) is
//!   never fatal. The affected output is skipped, recorded in the [`Report`], and every
//!   other output is still written. A skipped ChirpStack config means a condition-skipped
//!   unit, which is diagnosable; a failed render means no portal, which is not.
//! * A broken **template or overlay** — unparseable YAML, an unknown key, an unknown
//!   `radio.mode` — is fatal, before anything is written. There is no sensible partial
//!   output for "the configuration is wrong", and rendering a guess would put a wrong
//!   config in front of a medical-grade device.
//!
//! # Ownership and atomicity
//!
//! Every output is written to a temp file in its destination directory, `chmod 0640`,
//! `chown root:<consuming service's group>`, then renamed. A consumer never observes a
//! partially written file, and a failure part-way through leaves no debris (the temp file
//! is removed by a drop guard, on the error path and on a panic alike).
//!
//! Ownership is best-effort and skipped when the process is not root, so the tests — and
//! a developer running `proximosctl render` by hand with the roots pointed at a temp
//! directory — behave the same as the device does, minus the `chown`. [`Report::ownership`]
//! says which happened; it is never silently assumed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod config;
mod dbkey;
pub mod first_run;
mod fsutil;
mod outputs;

pub use config::{ChirpstackSection, ForwarderSection, Mode, PortalSection, Region};

/// The roots every path in this crate is resolved against.
///
/// Injectable rather than compiled in, so a test never touches `/data` or `/run` and a
/// developer can drive a whole render into a temp directory. The defaults are the device's.
#[derive(Debug, Clone)]
pub struct RenderPaths {
    /// `/usr/share/proximos/templates` — the immutable half of §7's input. `defaults.yaml`
    /// and the per-service `*.j2` files live here.
    pub templates: PathBuf,
    /// `/data` — the persistent half: the device overlay, `db.key`, the ChirpStack
    /// api-secret, the cluster role marker.
    pub data: PathBuf,
    /// `/run/proximos` — tmpfs, wiped every boot, where the rendered files go.
    pub run: PathBuf,
    /// Overrides the *destination* of the control config, honouring `PROXIMOS_CONFIG`
    /// exactly as control-radio's `dispatch::config_path()` reads it: one variable, one
    /// file, so `PROXIMOS_CONFIG=… proximosctl render` and
    /// `PROXIMOS_CONFIG=… proximosctl control.radio status` agree about which file they
    /// mean. `None` on a device, where both sides use the compiled-in default.
    pub control_config: Option<PathBuf>,
}

/// The device's paths, as `proximos-render.service` invokes the renderer.
pub const DEFAULT_TEMPLATES_DIR: &str = "/usr/share/proximos/templates";
pub const DEFAULT_DATA_DIR: &str = "/data";
pub const DEFAULT_RUN_DIR: &str = "/run/proximos";

impl Default for RenderPaths {
    fn default() -> Self {
        Self {
            templates: PathBuf::from(DEFAULT_TEMPLATES_DIR),
            data: PathBuf::from(DEFAULT_DATA_DIR),
            run: PathBuf::from(DEFAULT_RUN_DIR),
            control_config: None,
        }
    }
}

impl RenderPaths {
    /// The device's paths, with the documented environment overrides applied.
    ///
    /// | variable | overrides | who else reads it |
    /// |---|---|---|
    /// | `PROXIMOS_TEMPLATES` | the templates directory | nobody — this crate's own |
    /// | `PROXIMOS_DATA_ROOT` | `/data` | nobody — this crate's own |
    /// | `PROXIMOS_RUN_ROOT` | `/run/proximos` | nobody — this crate's own |
    /// | `PROXIMOS_CONFIG` | the rendered control config's path | `control-radio`, as the file it *reads* |
    ///
    /// The last one is the only shared name, and sharing it is the point: it is the same
    /// file seen from the two ends.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            templates: var("PROXIMOS_TEMPLATES")
                .map_or_else(|| PathBuf::from(DEFAULT_TEMPLATES_DIR), PathBuf::from),
            data: var("PROXIMOS_DATA_ROOT")
                .map_or_else(|| PathBuf::from(DEFAULT_DATA_DIR), PathBuf::from),
            run: var("PROXIMOS_RUN_ROOT")
                .map_or_else(|| PathBuf::from(DEFAULT_RUN_DIR), PathBuf::from),
            control_config: var("PROXIMOS_CONFIG").map(PathBuf::from),
        }
    }
}

/// Whether the `chown` half of the write actually happened.
///
/// Reported rather than assumed: on the device the group of every rendered file is a
/// cross-file contract with a unit's `User=`, and "the render succeeded but nothing is
/// group-readable" is a failure mode worth being able to name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Running as root; owner and group were set on every file written.
    Applied,
    /// Not root (a test, or a hand-run). Modes were still set; ownership was left alone.
    Skipped(String),
}

/// One output the renderer decided not to write, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// What one render did. Every path this process touched appears exactly once.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub written: Vec<PathBuf>,
    /// Stale outputs removed because the current configuration does not ask for them —
    /// a `radio.mode` flip away from lorawan, or a Hub that became client-role.
    pub removed: Vec<PathBuf>,
    pub skipped: Vec<Skipped>,
    /// Outputs that **were** written (they are in `written` too), but without a section
    /// their inputs asked for — today only the control config's `mqtt.broker` override on
    /// a client-role Hub whose `/data/proximos/cluster/` payload is incomplete. Kept apart
    /// from `skipped`, which means "this file is not on the tmpfs".
    pub degraded: Vec<Skipped>,
    pub ownership: Option<Ownership>,
}

impl Report {
    fn wrote(&mut self, p: impl Into<PathBuf>) {
        self.written.push(p.into());
    }
    fn removed(&mut self, p: impl Into<PathBuf>) {
        self.removed.push(p.into());
    }
    fn skip(&mut self, p: impl Into<PathBuf>, reason: impl Into<String>) {
        self.skipped.push(Skipped {
            path: p.into(),
            reason: reason.into(),
        });
    }
    fn degraded(&mut self, p: impl Into<PathBuf>, reason: impl Into<String>) {
        self.degraded.push(Skipped {
            path: p.into(),
            reason: reason.into(),
        });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A YAML input (`defaults.yaml`, an overlay) does not parse, or does not match the
    /// schema. Fatal by design — see the module docs' failure policy.
    #[error("{path} is not a valid configuration document: {source}")]
    Config {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },

    #[error("template {name} failed to render: {source}")]
    Template {
        name: String,
        #[source]
        source: minijinja::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Render every configuration this device's state asks for.
///
/// The whole entry point; `proximosctl render` is a thin wrapper over it. Idempotent —
/// running it twice against unchanged inputs produces byte-identical files, including the
/// generated `db.key`, which is created once and then reused forever.
///
/// Order matters and is not alphabetical. The portal and control configs are written
/// first because they are unconditional and because the portal is the output whose loss
/// is most visible; the ChirpStack pair comes last, so that a Hub missing its api-secret
/// still ends the run with a portal.
///
/// `default_mode` is the `radio.mode` this Hub's hardware suggests when no layer names one:
/// `lorawan` with the RAK5146, `p2p` without (`plan/control/radio/node_registry.md` §12). It
/// is handed in rather than probed here: the one card detection is control-radio's
/// (`setmode::probe_hardware`), and this crate does not depend on control-radio (see
/// [`Mode`]). `proximosctl render`, which depends on both, joins them.
pub fn render_all(paths: &RenderPaths, default_mode: Mode) -> Result<Report> {
    let mut report = Report {
        // Resolved once, up front, so a report always carries the answer even when the
        // run writes nothing at all.
        ownership: Some(fsutil::ownership_state()),
        ..Default::default()
    };

    // Everything is parsed and validated before a single file is written. A broken
    // overlay must not produce three good files and one missing one.
    let effective = config::Effective::load(paths, default_mode)?;

    outputs::portal(paths, &effective, &mut report)?;
    outputs::control(paths, &effective, &mut report)?;
    outputs::chirpstack(paths, &effective, &mut report)?;
    outputs::forwarder(paths, &effective, &mut report)?;
    outputs::mosquitto_listener(paths, &effective, &mut report)?;

    Ok(report)
}

/// Relative locations under [`RenderPaths::data`]. Grouped here because these paths
/// are the whole of this crate's contract with the rest of `/data`, and each of them is
/// owned by something else: a migration, a bootstrap unit, or a cluster feature that does
/// not exist yet.
pub(crate) mod data_paths {
    /// The per-device overlay — the file an operator or the provisioning tool edits.
    ///
    /// **Chosen, not inherited.** `plan/control/radio/plan.md` §4 names the v2 device
    /// config `proximos.config.yaml` (v1: `fiber.config.yaml`) and
    /// `plan/security/landlock.md` §5.1 gives the merged component a read rule on
    /// `/data/proximos/config/`; this is those two facts spelled as one path. It is the
    /// same document `radio:` is "one section of" — `power`, `sensors`, `mqtt`, `ble`,
    /// `eye` join it as the rest of the sweep lands.
    ///
    /// *2026-09-26:* no longer unwritten. Migration `040-config-init.sh` creates the
    /// directory, and [`crate::first_run`] writes `radio.mode` into this file on the first
    /// run (decision #37). Everything else in it is still unwritten.
    ///
    /// ⚠ **Nothing creates this file or its directory today.** control-core still reads
    /// the v1-spelled `/data/fiber/config/fiber.config.yaml`, seeded by
    /// `packaging/fiber-config-seed.sh` from the rootfs. Absence here is not an error —
    /// it simply means the shipped defaults apply unmodified, which is the correct
    /// behaviour for a device nobody has provisioned. Creating the directory belongs to a
    /// migration (`NNN-config-init.sh`), and pointing control-core at the v2 path belongs
    /// to the §11.3 rename sweep. Flagged for both.
    pub const OVERLAY: &str = "proximos/config/proximos.config.yaml";

    /// `fiber.service`'s own config, the home of the Hub-side settings its `ConfigApplier`
    /// writes. The renderer reads exactly one key of it, `radio.mode` — the Hub's runtime
    /// radio mode (`plan/control/radio/node_registry.md` §11, amended 2026-09-27 R7) — and
    /// that key wins over the defaults and the overlay.
    ///
    /// The v1 spelling of the path, because it is where the agent keeps the file today; it
    /// moves with the §11.3 rename sweep, and this line with it.
    pub const HUB_CONFIG: &str = "fiber/config/fiber.config.yaml";

    /// The legacy portal-only override the shim appended verbatim. Still honoured, now as
    /// a typed merge into the `portal:` section. Kept because it is the path that exists
    /// on devices in the field and the one `proximos-render.sh`'s rendered header tells
    /// operators to use.
    pub const PORTAL_OVERRIDE: &str = "portal/config.yaml";

    /// SQLCipher passphrase for the portal database. Generated once, here, and then
    /// reused forever.
    pub const DB_KEY: &str = "portal/db.key";

    /// The directory `db.key` lives in. Created by this crate if absent for the same
    /// reason the shim created it: the tmpfiles fragment that declares it runs from
    /// `systemd-tmpfiles-setup.service`, which is not ordered after the `nofail`
    /// `data.mount`, so losing that race would put the key inside the rootfs mount point
    /// where the real `/data` then hides it.
    pub const PORTAL_DIR: &str = "portal";

    /// ChirpStack's JWT signing key, generated 0600 root:root by migration
    /// `005-chirpstack-seed.sh`. Read here, injected into `chirpstack.toml`, never logged.
    pub const API_SECRET: &str = "chirpstack/api-secret";

    /// The client/server role marker of decision #5. Nothing writes it yet — the join
    /// that does is `plan/control/cluster.md` §8 (milestone C4) — and an absent marker
    /// means **server**, which is what makes a single bench Hub serve its own portal.
    /// Written **last** by the join and removed first on teardown (cluster.md §3).
    pub const CLUSTER_ROLE: &str = "proximos/cluster/role";

    /// The rest of cluster.md §3's directory: one value per file, read only when
    /// [`CLUSTER_ROLE`] says `client`. `server_host` is a name or an IP literal,
    /// `server_port` the Server's TLS listener (8883), `peer_username`
    /// `peer-<own hostname>`, `peer_password` 0600.
    pub const CLUSTER_SERVER_HOST: &str = "proximos/cluster/server_host";
    pub const CLUSTER_SERVER_PORT: &str = "proximos/cluster/server_port";
    pub const CLUSTER_PEER_USERNAME: &str = "proximos/cluster/peer_username";
    pub const CLUSTER_PEER_PASSWORD: &str = "proximos/cluster/peer_password";
    /// The Server's CA. **Never opened by this crate** — its path is rendered into the
    /// forwarder TOML and the agent's broker override, and those two read it.
    pub const CLUSTER_CA: &str = "proximos/cluster/ca.crt";

    /// The portal's first-run transport request (decision #37): `p2p` or `lorawan`,
    /// written by `/setup`, consumed by `first-run-apply`. Inside `/data/portal` because
    /// that is the only tree the portal can write. `proximos-first-run.path` watches the
    /// same path, and the portal names the file `requested-radio-mode` in its data dir;
    /// `unit_contract_tests` holds all three together.
    pub const RADIO_MODE_REQUEST: &str = "portal/requested-radio-mode";
}

/// Relative locations under [`RenderPaths::run`], each with the group that must be able to
/// read it.
///
/// The group is the `User=` of the unit that consumes the file, and it is a **cross-file
/// contract**: `chirpstack.service`'s own comment says so in as many words. Change one
/// side and the daemon stops being able to read its own configuration. The tests at the
/// bottom of this file read the unit files and hold the two sides together.
pub(crate) mod run_paths {
    /// `proximos`, not `fiber-portal`: `fiber-portal.service` runs as the merged
    /// component's one account ahead of the merge (plan/portal/monolith.md §15, #88).
    pub const PORTAL: (&str, &str) = ("portal/config.yaml", "proximos");
    pub const CONTROL: (&str, &str) = ("control/config.yaml", "proximos");
    pub const CHIRPSTACK_DIR: &str = "chirpstack";
    pub const CHIRPSTACK_TOML: (&str, &str) = ("chirpstack/chirpstack.toml", "chirpstack");
    /// The forwarder unit has no `User=` and therefore runs as root; `root` is its group,
    /// not a placeholder. If WP-G gives that unit a service account, this is the line that
    /// changes with it.
    pub const FORWARDER: (&str, &str) = (
        "chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml",
        "root",
    );
    /// The Server's TLS listener (plan/control/cluster.md §5), included by
    /// `mosquitto.conf`'s second `include_dir`. mosquitto.service runs as `mosquitto`
    /// and reads its configuration — at start and on every SIGHUP — as that user. The
    /// directory is not a `RuntimeDirectory=` of proximos-render.service but a tmpfiles
    /// line of the mosquitto recipe, so it outlives a failed render.
    pub const MOSQUITTO_LISTENER: (&str, &str) = ("mosquitto/8883.conf", "mosquitto");
}

/// Load one template and render it with `ctx`.
///
/// `trim_blocks` + `lstrip_blocks` are both on, the Jinja convention, so a `{% if %}` may
/// sit on its own line without contributing a blank one — which is what makes the
/// portal template's optional keys byte-compatible with the shim's fixed block.
///
/// Auto-escaping is turned **off explicitly**, and that is not a formality. minijinja's
/// default callback strips a `.j2` suffix and then looks at what is left, so
/// `control/config.yaml.j2` resolves to `.yaml` and gets `AutoEscape::Json` — which
/// re-encodes every substituted value as a JSON literal. The observed result was a
/// control config whose entire `radio:` section was one quoted string with `\n` in it: a
/// file that parses as YAML, passes any "is it valid YAML" check, and means nothing.
/// There is no correct auto-escaping for YAML or TOML; the templates quote what needs
/// quoting themselves.
fn render_template(
    paths: &RenderPaths,
    name: &str,
    ctx: BTreeMap<&str, minijinja::Value>,
) -> Result<String> {
    let path = paths.templates.join(name);
    let source = std::fs::read_to_string(&path).map_err(|source| Error::Read {
        path: path.clone(),
        source,
    })?;

    let mut env = minijinja::Environment::new();
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    // Jinja2 — and minijinja after it — strips one trailing newline from a template
    // source by default, which is right for HTML fragments and wrong for a Unix
    // configuration file. Without this the rendered TOML ends mid-line, and every
    // `diff`, `tail` and `cat` of it thereafter reports a "\ No newline at end of file".
    env.set_keep_trailing_newline(true);
    env.add_template(name, &source)
        .and_then(|()| env.get_template(name)?.render(ctx))
        .map_err(|source| Error::Template {
            name: name.to_string(),
            source,
        })
}

/// Read a `/data` file that may legitimately not exist yet.
fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// The other side of every group in [`run_paths`], read from the files that declare it.
///
/// A test cannot `chown`, so the render tests prove the bytes and never the group; the
/// group only fails on a device, as a unit that cannot read its own configuration. That
/// already happened once in intent — `fiber-portal.service` moved to `User=proximos`
/// (plan/portal/monolith.md §15) and `run_paths::PORTAL` had to move with it. These read
/// the unit files, the portal's tmpfiles fragment and its sysusers fragment straight out
/// of the tree, so the next such move breaks here instead.
#[cfg(test)]
mod unit_contract_tests {
    use super::{data_paths, dbkey, run_paths, DEFAULT_DATA_DIR};
    use std::path::PathBuf;

    const PORTAL_UNIT: &str = "portal/packaging/fiber-portal.service";
    const CONTROL_UNIT: &str = "portal/packaging/proximos-control.service";
    const RADIO_SETUP_UNIT: &str = "portal/packaging/radio-setup.service";
    const CHIRPSTACK_UNIT: &str =
        "system/meta-proximos/recipes-connectivity/chirpstack/files/chirpstack.service";
    const FORWARDER_UNIT: &str = "system/meta-proximos/recipes-connectivity/\
                                  chirpstack-mqtt-forwarder/files/chirpstack-mqtt-forwarder.service";
    const AGENT_UNIT: &str = "portal/packaging/fiber.service";
    const MOSQUITTO_BBAPPEND: &str =
        "system/meta-proximos/recipes-connectivity/mosquitto/mosquitto_%.bbappend";
    const MOSQUITTO_DROPIN: &str =
        "system/meta-proximos/recipes-connectivity/mosquitto/files/10-proximos.conf";
    const MOSQUITTO_TMPFILES: &str = "system/meta-proximos/recipes-connectivity/mosquitto/\
                                      files/proximos-mosquitto.tmpfiles.conf";
    const FIREWALL_NFT: &str = "system/meta-proximos/recipes-core/firewall/files/firewall.nft";
    const FIREWALL_SERVER_UNIT: &str =
        "system/meta-proximos/recipes-core/firewall/files/proximos-firewall-server.service";
    const FIREWALL_SERVER_NFT: &str =
        "system/meta-proximos/recipes-core/firewall/files/firewall-server.nft";
    const REDIS_DROPIN: &str =
        "system/meta-proximos/recipes-connectivity/redis/files/10-proximos.conf";
    const BOOTSTRAP_UNIT: &str =
        "system/meta-proximos/recipes-connectivity/chirpstack/files/chirpstack-bootstrap.service";
    const CONCENTRATORD_UNIT: &str = "system/meta-proximos/recipes-connectivity/\
                                      chirpstack-concentratord/files/chirpstack-concentratord.service";
    const PORTAL_TMPFILES: &str = "portal/packaging/fiber-portal.tmpfiles.conf";
    const PORTAL_SYSUSERS: &str = "portal/packaging/fiber-portal.sysusers.conf";
    const FIRST_RUN_PATH_UNIT: &str = "portal/packaging/proximos-first-run.path";
    const FIRST_RUN_SERVICE_UNIT: &str = "portal/packaging/proximos-first-run.service";
    const CONFIG_INIT_MIGRATION: &str = "portal/packaging/migrations/040-config-init.sh";

    /// The monorepo root: this crate is `portal/crates/proximos-render`.
    fn repo_file(rel: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
    }

    /// The `(User=, Group=)` a unit's `[Service]` section runs with, resolved the way
    /// systemd does it for these units: no `User=` is root, and no `Group=` is the user's
    /// own group (every account here is created with `--user-group`).
    fn unit_identity(rel: &str) -> (String, String) {
        let text = repo_file(rel);
        let (mut user, mut group) = (None, None);
        let mut in_service = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_service = line == "[Service]";
            } else if in_service {
                if let Some(v) = line.strip_prefix("User=") {
                    user = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("Group=") {
                    group = Some(v.trim().to_string());
                }
            }
        }
        let user = user.unwrap_or_else(|| "root".to_string());
        let group = group.unwrap_or_else(|| user.clone());
        (user, group)
    }

    #[test]
    fn every_rendered_file_is_readable_by_the_group_of_the_unit_that_reads_it() {
        let cases = [
            (run_paths::PORTAL, PORTAL_UNIT),
            (run_paths::CONTROL, CONTROL_UNIT),
            (run_paths::CONTROL, RADIO_SETUP_UNIT),
            // The portal reads the control config too: its in-process control.radio seam
            // loads the rendered `radio:` section (monolith.md §15).
            (run_paths::CONTROL, PORTAL_UNIT),
            (run_paths::CHIRPSTACK_TOML, CHIRPSTACK_UNIT),
            (run_paths::FORWARDER, FORWARDER_UNIT),
        ];
        for ((file, group), unit) in cases {
            let (_, unit_group) = unit_identity(unit);
            assert_eq!(
                unit_group, group,
                "{file} is chowned root:{group}, but {unit} runs with group {unit_group} \
                 and could not read it"
            );
        }
    }

    /// The concentrator is LoRaWAN's radio (P2P runs over the northbridge alone), and its
    /// unit is gated on the forwarder config this renderer writes only for `lorawan`/`dual`.
    /// The unit names that path literally, so a move of `run_paths::FORWARDER` has to move
    /// the condition too — or a `p2p` Hub with a card fitted runs the concentrator again.
    #[test]
    fn the_concentrator_runs_only_where_the_forwarder_config_is_rendered() {
        let (rel, _) = run_paths::FORWARDER;
        let text = repo_file(CONCENTRATORD_UNIT);
        let condition = format!("ConditionPathExists=/run/proximos/{rel}");
        assert!(
            text.lines().any(|l| l.trim() == condition),
            "{CONCENTRATORD_UNIT} must carry `{condition}`"
        );
        assert!(
            text.lines()
                .any(|l| l.trim() == "After=proximos-render.service"),
            "the condition must be judged after the renderer has run"
        );
    }

    /// cluster.md §2 / decision #8: a Client runs no ChirpStack **and no redis**. Nothing
    /// renders a redis config, so the gate is the ChirpStack config itself — which is
    /// absent on a Client and on an off/p2p Hub. All three units must name that one path,
    /// and redis must be judged after the renderer has written it.
    #[test]
    fn redis_and_chirpstack_run_only_where_chirpstack_toml_is_rendered() {
        let (rel, _) = run_paths::CHIRPSTACK_TOML;
        let condition = format!("ConditionPathExists=/run/proximos/{rel}");
        for unit in [REDIS_DROPIN, CHIRPSTACK_UNIT, BOOTSTRAP_UNIT] {
            assert!(
                repo_file(unit).lines().any(|l| l.trim() == condition),
                "{unit} must carry `{condition}`"
            );
        }
        assert!(
            repo_file(REDIS_DROPIN)
                .lines()
                .any(|l| l.trim() == "After=proximos-render.service"),
            "redis's condition must be judged after the renderer has run"
        );
    }

    /// The agent reads the control config once, at startup, for its topic root and — on a
    /// Client — the Server's broker (cluster.md §4.4). It runs as root, so the group is
    /// not the contract here; the ordering is.
    /// Decided 2026-10-02: the 8883 listener is rendered on a Server only, into a directory
    /// mosquitto.conf includes. Three things outside this crate must name that directory:
    /// the main config's second `include_dir` (mosquitto 2.1.2 allows it nowhere else), the
    /// tmpfiles line that makes it exist before the broker — a missing include_dir stops
    /// mosquitto, loopback included — and the drop-in that orders the broker after the
    /// renderer, so a Server's first start already sees the file.
    #[test]
    fn mosquitto_includes_the_rendered_listener_directory_and_waits_for_it() {
        let (rel, _) = run_paths::MOSQUITTO_LISTENER;
        let dir = format!(
            "/run/proximos/{}",
            std::path::Path::new(rel).parent().unwrap().display()
        );

        let bb = repo_file(MOSQUITTO_BBAPPEND);
        let includes: Vec<&str> = bb
            .lines()
            .filter_map(|l| l.trim().strip_prefix("include_dir "))
            .collect();
        assert_eq!(
            includes,
            vec!["/etc/mosquitto/conf.d", dir.as_str()],
            "mosquitto.conf must include the static half first, then {dir}"
        );

        let tmpfiles = repo_file(MOSQUITTO_TMPFILES);
        assert!(
            tmpfiles.lines().any(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.first() == Some(&"d") && f.get(1) == Some(&dir.as_str())
            }),
            "{MOSQUITTO_TMPFILES} must create {dir}"
        );

        let dropin = repo_file(MOSQUITTO_DROPIN);
        for want in [
            "After=proximos-render.service",
            "Wants=proximos-render.service",
        ] {
            assert!(
                dropin.lines().any(|l| l.trim() == want),
                "{MOSQUITTO_DROPIN} must carry {want}"
            );
        }
        assert!(
            !dropin
                .lines()
                .any(|l| l.trim() == "Requires=proximos-render.service"),
            "a failed render must not take the loopback broker down"
        );
    }

    /// The firewall half of the same decision: 8883 is not in the static rule set, the unit that
    /// opens it is gated on the very file that opens the listener, and what that unit loads
    /// really opens it — the drop-in adds 8883 to the set the base rule set accepts.
    #[test]
    fn the_firewall_opens_8883_exactly_where_the_listener_is_rendered() {
        let (rel, _) = run_paths::MOSQUITTO_LISTENER;
        let condition = format!("ConditionPathExists=/run/proximos/{rel}");
        let unit = repo_file(FIREWALL_SERVER_UNIT);
        assert!(
            unit.lines().any(|l| l.trim() == condition),
            "{FIREWALL_SERVER_UNIT} must carry `{condition}`"
        );
        assert!(unit
            .lines()
            .any(|l| l.trim() == "After=proximos-render.service"));

        let nft = repo_file(FIREWALL_NFT);
        let open_tcp = nft
            .lines()
            .find_map(|l| l.trim().strip_prefix("define open_tcp = "))
            .expect("firewall.nft defines open_tcp");
        assert!(
            !open_tcp.contains("8883"),
            "8883 must not be open on every Hub: {open_tcp}"
        );
        assert!(
            nft.lines().any(|l| l.trim() == "set server_tcp {"),
            "{FIREWALL_NFT} must declare the set the drop-in fills"
        );
        assert!(
            nft.lines()
                .any(|l| l.trim() == "tcp dport @server_tcp accept"),
            "{FIREWALL_NFT} must accept what is in @server_tcp"
        );

        let dropin = "/usr/share/proximos/firewall-server.nft";
        assert!(
            unit.lines()
                .filter_map(|l| l.trim().strip_prefix("ExecStart="))
                .any(|cmd| cmd.split_whitespace().any(|w| w == dropin)),
            "{FIREWALL_SERVER_UNIT} must load {dropin}"
        );
        let server = repo_file(FIREWALL_SERVER_NFT);
        assert!(
            server
                .lines()
                .any(|l| l.trim() == "add element inet proximos server_tcp { 8883 }"),
            "{FIREWALL_SERVER_NFT} must add 8883 to @server_tcp"
        );
    }

    #[test]
    fn the_agent_starts_after_the_renderer_it_reads() {
        let text = repo_file(AGENT_UNIT);
        assert!(
            text.lines()
                .filter_map(|l| l.trim().strip_prefix("After="))
                .flat_map(str::split_whitespace)
                .any(|u| u == "proximos-render.service"),
            "{AGENT_UNIT} must be ordered After=proximos-render.service"
        );
    }

    #[test]
    fn the_portal_unit_runs_as_the_account_that_owns_its_data() {
        let (user, group) = unit_identity(PORTAL_UNIT);
        assert_eq!(
            user,
            dbkey::OWNER,
            "db.key and /data/portal must belong to User="
        );
        assert_eq!(group, dbkey::OWNER);
        assert_eq!(
            dbkey::OWNER,
            run_paths::PORTAL.1,
            "one account: the portal config's group is the owner of the portal's data"
        );
    }

    #[test]
    fn the_portal_tmpfiles_fragment_creates_its_directories_for_that_account() {
        let text = repo_file(PORTAL_TMPFILES);
        let lines: Vec<Vec<&str>> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.split_whitespace().collect())
            .collect();
        assert!(!lines.is_empty(), "{PORTAL_TMPFILES} declares nothing");
        for fields in lines {
            // type path mode user group age
            assert!(fields.len() >= 5, "short tmpfiles line: {fields:?}");
            assert_eq!(
                (fields[3], fields[4]),
                (dbkey::OWNER, dbkey::OWNER),
                "{PORTAL_TMPFILES}: {} is not owned by the portal's account",
                fields[1]
            );
        }
    }

    #[test]
    fn the_portal_package_declares_the_account_it_runs_as() {
        let declared: Vec<String> = repo_file(PORTAL_SYSUSERS)
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("u "))
            .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
            .collect();
        let (user, _) = unit_identity(PORTAL_UNIT);
        assert_eq!(
            declared,
            vec![user.clone()],
            "{PORTAL_SYSUSERS} must declare exactly the account {PORTAL_UNIT} runs as"
        );
    }

    /// Decision #37's three-way contract: the portal writes the request, the path unit
    /// watches it, `first-run-apply` reads it. The portal side is pinned against the same
    /// unit file in `portal/src/first_run.rs`.
    #[test]
    fn the_first_run_path_unit_watches_the_request_the_applier_reads() {
        let expected = format!("{DEFAULT_DATA_DIR}/{}", data_paths::RADIO_MODE_REQUEST);
        let text = repo_file(FIRST_RUN_PATH_UNIT);
        let watched: Vec<&str> = text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("PathExists="))
            .collect();
        assert_eq!(watched, vec![expected.as_str()]);
        let service = repo_file(FIRST_RUN_SERVICE_UNIT);
        assert!(
            service.contains(&format!("ConditionPathExists={expected}")),
            "{FIRST_RUN_SERVICE_UNIT} must be gated on the same request file"
        );
    }

    /// The migration that pins upgraded Hubs writes the overlay this crate reads, and
    /// creates the directory the applier's unit names in ReadWritePaths=.
    #[test]
    fn the_config_init_migration_writes_the_overlay_this_crate_reads() {
        let overlay = format!("{DEFAULT_DATA_DIR}/{}", data_paths::OVERLAY);
        let (dir, file) = overlay.rsplit_once('/').unwrap();
        let script = repo_file(CONFIG_INIT_MIGRATION);
        assert!(
            script.contains(&format!("dir={dir}\n")),
            "040 creates {dir}"
        );
        assert!(script.contains(&format!("overlay=\"$dir/{file}\"")));
        assert!(repo_file(FIRST_RUN_SERVICE_UNIT).contains(&format!("-{dir} ")));
    }
}
