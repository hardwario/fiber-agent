//! Landlock self-confinement for the portal (M7).
//!
//! Design: `plan/security/landlock.md` (§4 where it is applied, §5.2 the
//! portal's resource surface, §7 network rules, §8 failure policy) and
//! `plan/viewer/design.md` §5.
//!
//! Three properties drive everything here:
//!
//! 1. **Per-thread, not per-process.** `landlock_restrict_self(2)` restricts
//!    the calling thread only. A thread that already exists is never
//!    confined — silently, with every functional test still passing. So
//!    [`apply`] is called at the top of `main()`, while the process is still
//!    single-threaded, *before* the tokio runtime and before any library
//!    that might spawn on construction (MQTT client, DB pool, appender).
//!    `main()` deliberately does not use `#[tokio::main]` so this ordering
//!    is visible rather than hidden in a macro.
//! 2. **Irrevocable.** The ruleset must cover everything the process will
//!    ever need. Anything missing here surfaces as `EACCES` at runtime, not
//!    at startup.
//! 3. **Fails open, loudly** (CP-2, landlock.md §8). A temperature monitor
//!    that refuses to monitor because a sandbox failed to apply is the worse
//!    outcome. [`apply`] never returns an error: it returns a
//!    [`SandboxStatus`] the caller logs and republishes on `/health` (to a
//!    signed-in session), so a degraded device cannot look like a confined
//!    one.
//!
//! ## Deviations from design.md §5's five-row table — read before trimming
//!
//! The declared table covers the paths the portal *owns*. A dynamically
//! linked binary that resolves hostnames needs more, and M6's notifier needs
//! outbound ports the table (written pre-M6) does not mention. Both are
//! implemented here and flagged for the CP-8 review:
//!
//! - **`/etc` (read)** — `/etc/hosts`, `/etc/resolv.conf`, `/etc/nsswitch.conf`
//!   for resolving the broker host and operator-configured webhook/SMTP hosts.
//!   `/etc/hosts` is a symlink to `/run/proximos/net/hosts` on the image, and
//!   Landlock judges the resolved file, so that directory is a read row too.
//! - **`/usr/lib`, `/lib` (read + execute)** — glibc `dlopen`s `libnss_*.so`
//!   lazily on the first name lookup, i.e. *after* confinement. Execute is
//!   required for mapping a shared object, not for spawning anything; the
//!   portal still execs nothing (decision #6).
//! - **Outbound ports beyond 1883** — the notifier connects to operator
//!   endpoints (SendGrid 443, SMTP 25/465/587/2525, webhook 80/443). Landlock
//!   port rules are per-port, so the set is enumerated in
//!   [`NOTIFIER_CONNECT_PORTS`]; a webhook on a port outside it fails. The
//!   alternative — dropping TCP connect mediation entirely — buys nothing.
//! - **The in-process `control.radio` seam** (`plan/portal/monolith.md` §15)
//!   reads the rendered radio config under `/run/proximos/control`, reads and
//!   writes the radio state tree `/data/proximos/radio` (`nodes.json`, the
//!   tenant API token, the downlink sequence cache it allocates in by
//!   temp-and-rename within one directory), and connects to the ChirpStack
//!   gRPC API on loopback port 8080. All three are optional — a Hub with no
//!   radio loses the seam, not its confinement — and all three are rows the
//!   merged process's union ruleset carries (`landlock.md` §5.1, §7). Without
//!   them an enforced ruleset refuses the seam with `EACCES` exactly as the
//!   unit's old `User=` did.
//!
//! ## ABI floor
//!
//! landlock.md §3 leaves the floor `TBD (CP-2, blocked on S1)` because no
//! target device has reported its ABI yet. The provisional floor here is
//! derived from what our own rights require rather than guessed:
//! [`ABI_FLOOR`] = V3, because the `/data/portal` row needs `TRUNCATE`
//! (added in ABI 3). It is a **hard requirement**: below it the process
//! reports degraded confinement instead of pretending. Network mediation
//! (ABI 4) is requested best-effort on top, so a kernel between ABI 3 and 4
//! still gets filesystem confinement and says so.
//!
//! `REFER` (ABI 2, cross-directory rename) is deliberately *not* required:
//! SQLite keeps `-wal`/`-shm` beside the database in the same directory, and
//! the portal writes no file it later renames across directories. If that
//! ever changes, this is where it breaks.
//!
//! ## Testing
//!
//! Confinement is process-wide and irrevocable, so it cannot be exercised in
//! an ordinary unit test — restricting the test runner would confine the rest
//! of the suite. The tests at the bottom fork, confine the child, and assert
//! `EACCES` outside the table / success inside it, plus one case asserting the
//! restriction holds on a *worker* thread (the §4 rule). They are
//! Linux-only and have **not** been executed on the target: per
//! `plan/security/README.md`'s evidence rule, only `linux-raspberrypi`
//! 6.12.93 on real hardware counts, and spike S1 is parked until hardware
//! exists.

// On a non-Linux dev host the `linux` submodule below is not compiled, so the
// ruleset inputs and the enforced/partial/degraded states have no constructor
// in this build. They are emphatically not dead on the target — scoping the
// allow to non-Linux keeps real dead code visible where it matters.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::fmt;

/// TCP ports the notifier may connect to, on top of the MQTT broker port.
/// See the module doc: operator-configured endpoints make this a list rather
/// than a single port.
pub const NOTIFIER_CONNECT_PORTS: &[u16] = &[25, 80, 443, 465, 587, 2525];

/// Paths and ports the ruleset is built from — resolved from `PortalConfig`
/// by the caller so this module never reads configuration itself (no runtime
/// path discovery, landlock.md §2).
#[derive(Debug, Clone)]
pub struct SandboxPaths {
    /// `/data/portal` — database, exports, calibrations. Read/write.
    pub data_dir: String,
    /// `/run/proximos/portal` — rendered config. Read-only. Skipped when absent.
    pub runtime_config_dir: Option<String>,
    /// `/data/tls` — server cert/key via group perms. Read-only. Skipped when absent.
    pub tls_dir: Option<String>,
    /// `/run/fiber/control.sock` — declared, unused in phase 1. Skipped when absent.
    pub control_socket: Option<String>,
    /// `/run/proximos/control` — the rendered device config the `control.radio`
    /// seam loads its `radio:` section from. Read-only. Skipped when absent.
    pub radio_config_dir: Option<String>,
    /// `/data/proximos/radio` — the radio state tree the seam reads the Node
    /// registry and API token from and allocates downlink sequence numbers in.
    /// Read/write. Skipped when absent.
    pub radio_data_dir: Option<String>,
    /// Templates directory (rootfs, read-only).
    pub templates_dir: String,
    /// Static assets directory (rootfs, read-only).
    pub static_dir: String,
    /// The single TCP port the portal binds (loopback; nginx fronts 443).
    pub bind_port: u16,
    /// The broker port to allow outbound.
    pub mqtt_port: u16,
    /// The ChirpStack gRPC API port the seam connects to (loopback). `None`
    /// leaves it out of the connect set.
    pub chirpstack_api_port: Option<u16>,
}

/// Every TCP port the ruleset lets the process connect to: the broker, the
/// ChirpStack API when the radio seam is configured, then the notifier set.
///
/// Pure, and outside the Linux-only module on purpose, so the set is testable
/// on every platform — a port missing here is an `EACCES` on the device and
/// nothing anywhere else.
pub fn connect_ports(paths: &SandboxPaths) -> Vec<u16> {
    let mut ports = vec![paths.mqtt_port];
    ports.extend(paths.chirpstack_api_port);
    for &port in NOTIFIER_CONNECT_PORTS {
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

/// How much confinement the running kernel actually gave us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    /// Every requested restriction is in force.
    Enforced,
    /// Filesystem confinement holds, but some best-effort request (network
    /// mediation) was dropped by an older kernel.
    Partial,
    /// Landlock is present but the ruleset could not be applied, or the ABI
    /// is below [`ABI_FLOOR`]. Running unconfined — deliberately, loudly.
    Degraded,
    /// This build/platform has no Landlock at all (non-Linux dev host).
    ///
    /// Only the non-Linux [`apply`] constructs this, so a Linux build sees no
    /// constructor and calls it dead. It is not: a Linux kernel without
    /// Landlock reports [`SandboxState::Degraded`] instead, because there the
    /// absence is a packaging defect (spike S3 — `CONFIG_LSM` must name
    /// `landlock`) rather than an expected platform limitation.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Unsupported,
}

impl SandboxState {
    /// True when the process is *not* fully confined and an operator should
    /// be able to see that in the log and on `/health` (signed in).
    ///
    /// Called from the Linux-only confinement tests (the child encodes it in
    /// its exit code); a Linux non-test build therefore has no caller.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn is_degraded(self) -> bool {
        !matches!(self, SandboxState::Enforced)
    }

    /// True when the **filesystem** rules are in force.
    ///
    /// Distinct from `!is_degraded()` on purpose. [`SandboxState::Partial`] is
    /// degraded — an operator must see it — but it is *confined*: it is the
    /// documented outcome on an ABI V3 kernel, where the filesystem ruleset
    /// holds and only the best-effort network mediation was dropped (see the
    /// module doc's "ABI floor").
    ///
    /// The confinement tests key their skip on this rather than on
    /// `is_degraded`, which conflated the two and made them silently pass on
    /// exactly the kernel whose ABI is still unverified (spike S1).
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn is_confined(self) -> bool {
        matches!(self, SandboxState::Enforced | SandboxState::Partial)
    }
}

impl fmt::Display for SandboxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SandboxState::Enforced => "enforced",
            SandboxState::Partial => "partial",
            SandboxState::Degraded => "degraded",
            SandboxState::Unsupported => "unsupported",
        };
        f.write_str(s)
    }
}

/// Result of the one confinement attempt this process makes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SandboxStatus {
    pub state: SandboxState,
    /// Human-readable reason, always populated — this is what makes a
    /// degraded device distinguishable from a confined one in the log and on
    /// `/health` (signed in).
    pub detail: String,
}

impl SandboxStatus {
    fn new(state: SandboxState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
        }
    }

    /// Logs the outcome at a level matching its severity. Called once by
    /// `main()`; degraded confinement must never be a silent `debug!`.
    pub fn report(&self) {
        match self.state {
            SandboxState::Enforced => {
                tracing::info!(detail = %self.detail, "landlock: confinement enforced")
            }
            SandboxState::Partial => tracing::warn!(
                detail = %self.detail,
                "landlock: PARTIAL confinement — filesystem rules hold, some requests were dropped"
            ),
            SandboxState::Degraded => tracing::error!(
                detail = %self.detail,
                "landlock: DEGRADED — running UNCONFINED; defense-in-depth is absent on this boot"
            ),
            SandboxState::Unsupported => tracing::warn!(
                detail = %self.detail,
                "landlock: unsupported on this platform — running unconfined (dev build)"
            ),
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply(_paths: &SandboxPaths) -> SandboxStatus {
    SandboxStatus::new(
        SandboxState::Unsupported,
        "not a Linux target; Landlock unavailable",
    )
}

#[cfg(target_os = "linux")]
pub use linux::apply;

#[cfg(target_os = "linux")]
mod linux {
    use super::{connect_ports, SandboxPaths, SandboxState, SandboxStatus};

    use landlock::{
        Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
        Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
    };

    /// Provisional hard floor — see the module doc. V3 because `/data/portal`
    /// needs `TRUNCATE`.
    const ABI_FLOOR: ABI = ABI::V3;
    /// Highest ABI whose features we ask for (V4 adds TCP bind/connect).
    const ABI_TARGET: ABI = ABI::V4;
    pub const ABI_FLOOR_NAME: &str = "V3 (filesystem incl. TRUNCATE)";

    /// Read-only file/dir access at the floor ABI.
    fn read_only() -> landlock::BitFlags<AccessFs> {
        AccessFs::ReadFile | AccessFs::ReadDir
    }

    /// Read-only plus execute — required for `dlopen` of NSS modules, not for
    /// spawning processes (the portal execs nothing).
    fn read_exec() -> landlock::BitFlags<AccessFs> {
        AccessFs::ReadFile | AccessFs::ReadDir | AccessFs::Execute
    }

    /// Everything the portal's own data directory needs. `Truncate` is what
    /// pins the ABI floor at V3; `Refer` is deliberately absent (see module doc).
    fn data_rw() -> landlock::BitFlags<AccessFs> {
        AccessFs::ReadFile
            | AccessFs::WriteFile
            | AccessFs::ReadDir
            | AccessFs::MakeDir
            | AccessFs::MakeReg
            | AccessFs::RemoveDir
            | AccessFs::RemoveFile
            | AccessFs::Truncate
    }

    /// Where `/etc/hosts` points on the image.
    const HOSTS_DIR: &str = "/run/proximos/net";

    /// Builds and applies the ruleset. Never returns `Err`: every failure
    /// path becomes a [`SandboxState`] the caller reports (CP-2).
    pub fn apply(paths: &SandboxPaths) -> SandboxStatus {
        match build_and_restrict(paths) {
            Ok(status) => status,
            Err(err) => SandboxStatus::new(
                SandboxState::Degraded,
                format!("failed to apply ruleset: {err}"),
            ),
        }
    }

    fn build_and_restrict(paths: &SandboxPaths) -> Result<SandboxStatus, landlock::RulesetError> {
        // Filesystem rights are a HARD requirement: a kernel below the floor
        // must degrade visibly rather than silently drop TRUNCATE and hand us
        // a ruleset we'd wrongly call "enforced". Network mediation is
        // requested best-effort on top.
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI_FLOOR))?
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?
            .create()?;

        // --- filesystem rules (design.md §5 + the documented additions) ---
        let mut rules: Vec<(&str, landlock::BitFlags<AccessFs>, bool)> = vec![
            (paths.data_dir.as_str(), data_rw(), true),
            (paths.templates_dir.as_str(), read_only(), true),
            (paths.static_dir.as_str(), read_only(), true),
            // Name resolution for the broker and operator endpoints.
            ("/etc", read_only(), false),
            // /etc/hosts is a symlink to this directory's `hosts` (base-files'
            // bbappend, plan/control/cluster.md §8). Landlock checks the file the link
            // resolves to, not the link, so the /etc row alone no longer covers it.
            (HOSTS_DIR, read_only(), false),
            // Lazy dlopen of libnss_*.so happens after confinement.
            ("/usr/lib", read_exec(), false),
            ("/lib", read_exec(), false),
        ];
        if let Some(dir) = paths.runtime_config_dir.as_deref() {
            rules.push((dir, read_only(), false));
        }
        if let Some(dir) = paths.tls_dir.as_deref() {
            rules.push((dir, read_only(), false));
        }
        if let Some(sock) = paths.control_socket.as_deref() {
            // Declared for phase 2. Landlock does not currently mediate
            // AF_UNIX connect by pathname, so this rule documents intent more
            // than it enforces it.
            rules.push((sock, AccessFs::ReadFile | AccessFs::WriteFile, false));
        }
        // The control.radio seam (module doc). The radio tree gets the same
        // rights as our own data directory: its writers replace files by
        // temp-and-rename inside one directory, which needs MakeReg and
        // RemoveFile but never Refer.
        if let Some(dir) = paths.radio_config_dir.as_deref() {
            rules.push((dir, read_only(), false));
        }
        if let Some(dir) = paths.radio_data_dir.as_deref() {
            rules.push((dir, data_rw(), false));
        }

        let mut missing_required = Vec::new();
        let mut skipped = Vec::new();
        for (path, access, required) in rules {
            match PathFd::new(path) {
                Ok(fd) => {
                    ruleset = ruleset.add_rule(PathBeneath::new(fd, access))?;
                }
                Err(err) => {
                    if required {
                        missing_required.push(format!("{path} ({err})"));
                    } else {
                        skipped.push(path.to_string());
                    }
                }
            }
        }

        // A missing *required* path is a packaging bug (tmpfiles.d fragment
        // not shipped, per system/design.md §6.1) and would confine the
        // process out of its own database. Degrade loudly instead of
        // enforcing a ruleset that cannot work.
        if !missing_required.is_empty() {
            return Ok(SandboxStatus::new(
                SandboxState::Degraded,
                format!(
                    "required paths missing, refusing to confine: {}",
                    missing_required.join(", ")
                ),
            ));
        }

        // --- network rules (§7): bind loopback port, connect broker, the
        // ChirpStack API and the notifier set ---
        ruleset = ruleset.add_rule(NetPort::new(paths.bind_port, AccessNet::BindTcp))?;
        for port in connect_ports(paths) {
            ruleset = ruleset.add_rule(NetPort::new(port, AccessNet::ConnectTcp))?;
        }

        let status = ruleset.no_new_privs(true).restrict_self()?;

        let detail = format!(
            "abi floor {ABI_FLOOR_NAME}, target {ABI_TARGET:?}, no_new_privs={}{}",
            status.no_new_privs,
            if skipped.is_empty() {
                String::new()
            } else {
                format!(", optional paths absent: {}", skipped.join(", "))
            }
        );

        Ok(match status.ruleset {
            RulesetStatus::FullyEnforced => SandboxStatus::new(SandboxState::Enforced, detail),
            RulesetStatus::PartiallyEnforced => SandboxStatus::new(
                SandboxState::Partial,
                format!("{detail}; kernel dropped some requested access rights"),
            ),
            RulesetStatus::NotEnforced => SandboxStatus::new(
                SandboxState::Degraded,
                format!("{detail}; kernel enforced nothing (ABI below floor or Landlock disabled)"),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> SandboxPaths {
        SandboxPaths {
            data_dir: "/data/portal".into(),
            runtime_config_dir: Some("/run/proximos/portal".into()),
            tls_dir: Some("/data/tls".into()),
            control_socket: Some("/run/fiber/control.sock".into()),
            radio_config_dir: Some("/run/proximos/control".into()),
            radio_data_dir: Some("/data/proximos/radio".into()),
            templates_dir: "/usr/share/fiber-portal/templates".into(),
            static_dir: "/usr/share/fiber-portal/static".into(),
            bind_port: 8088,
            mqtt_port: 1883,
            chirpstack_api_port: Some(8080),
        }
    }

    /// The radio seam's gRPC connect is in the set exactly when it is asked
    /// for, and nothing the notifier or the broker needs is lost either way.
    #[test]
    fn the_connect_set_carries_the_chirpstack_api_only_when_asked() {
        let with = connect_ports(&paths());
        assert_eq!(with[0], 1883, "the broker first");
        assert!(
            with.contains(&8080),
            "the radio seam dials ChirpStack on 8080"
        );
        for port in NOTIFIER_CONNECT_PORTS {
            assert!(with.contains(port), "notifier port {port} dropped");
        }

        let mut no_radio = paths();
        no_radio.chirpstack_api_port = None;
        let without = connect_ports(&no_radio);
        assert!(!without.contains(&8080));
        assert_eq!(without.len(), with.len() - 1);
    }

    #[test]
    fn state_degraded_predicate() {
        assert!(!SandboxState::Enforced.is_degraded());
        assert!(SandboxState::Partial.is_degraded());
        assert!(SandboxState::Degraded.is_degraded());
        assert!(SandboxState::Unsupported.is_degraded());
    }

    /// `Partial` is the one state where the two predicates disagree, and that
    /// disagreement is the whole point: an operator must be told (degraded),
    /// while the confinement tests must still run (confined).
    #[test]
    fn confined_and_degraded_are_not_opposites() {
        assert!(SandboxState::Enforced.is_confined());
        assert!(SandboxState::Partial.is_confined());
        assert!(!SandboxState::Degraded.is_confined());
        assert!(!SandboxState::Unsupported.is_confined());

        assert!(
            SandboxState::Partial.is_degraded() && SandboxState::Partial.is_confined(),
            "Partial must be reportable as degraded yet still count as confined"
        );
    }

    #[test]
    fn status_serializes_for_health() {
        let json = serde_json::to_value(SandboxStatus::new(SandboxState::Enforced, "ok"))
            .expect("serialize");
        assert_eq!(json["state"], "enforced");
        assert_eq!(json["detail"], "ok");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_reports_unsupported() {
        let status = apply(&paths());
        assert_eq!(status.state, SandboxState::Unsupported);
    }

    /// Marks a re-exec'd child process, so the `#[ignore]`d child tests below
    /// stay inert during an ordinary `cargo test -- --ignored` sweep.
    const CHILD_MARKER: &str = "PORTAL_SANDBOX_CHILD";

    /// A child's "confined as designed" exit code. Deliberately not `0`: a
    /// filter that matched no test — one of the child names below renamed,
    /// say — exits the harness `0`, and that must fail here rather than read
    /// as a pass.
    #[cfg(target_os = "linux")]
    const CHILD_OK: i32 = 42;

    /// Run one of the `#[ignore]`d child tests below in a process of its own,
    /// and return the exit code it chose.
    ///
    /// **Not `fork()`, deliberately.** These three tests used a bare
    /// `libc::fork()` and the child then allocated — `Storage::open`, a tokio
    /// runtime, `set_var`. But `fork()` in a multithreaded process duplicates
    /// only the calling thread, so any lock a *sibling* test thread happened to
    /// hold at that instant — the allocator's, above all — is inherited already
    /// locked, by a thread that no longer exists to release it. The child then
    /// blocks on it forever and the parent blocks forever in `waitpid`. The
    /// odds scale with how busy the harness is, which is why these passed
    /// instantly on their own and hung for twenty-five minutes under
    /// `cargo test --workspace`.
    ///
    /// `exec` is the only general fix: it throws the inherited address space
    /// away, locks and all. Landlock is still applied in a process that is not
    /// the test runner, which is the property the fork was there for — that has
    /// not changed, only how the process is made.
    #[cfg(target_os = "linux")]
    fn in_a_fresh_process(child: &str, env: &[(&str, &std::path::Path)]) -> i32 {
        let mut cmd = std::process::Command::new(
            std::env::current_exe().expect("the test binary's own path"),
        );
        cmd.arg(child)
            .arg("--exact")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_MARKER, "1");
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.status()
            .expect("re-exec the test binary")
            .code()
            .unwrap_or(-1)
    }

    /// `true` when this process is a child spawned by [`in_a_fresh_process`].
    /// The child tests do nothing otherwise, so they are safe to run directly.
    #[cfg(target_os = "linux")]
    fn is_child() -> bool {
        std::env::var_os(CHILD_MARKER).is_some()
    }

    #[cfg(target_os = "linux")]
    fn child_path(key: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var_os(key).expect("path from the parent"))
    }

    /// The ruleset under test, pointed at temp directories standing in for the
    /// real ones.
    #[cfg(target_os = "linux")]
    fn confined_to(dir: &std::path::Path) -> SandboxPaths {
        let mut p = paths();
        p.data_dir = dir.to_string_lossy().into_owned();
        p.templates_dir = dir.to_string_lossy().into_owned();
        p.static_dir = dir.to_string_lossy().into_owned();
        p.runtime_config_dir = None;
        p.tls_dir = None;
        p.control_socket = None;
        p.radio_config_dir = None;
        p.radio_data_dir = None;
        p
    }

    /// Linux-only confinement harness. Runs in a process of its own so the
    /// restriction never touches the test runner (Landlock is irrevocable),
    /// applies the ruleset there against a temp directory standing in for
    /// `/data/portal`, and asserts a path inside the table is writable while
    /// one outside is not.
    ///
    /// Not executed on the target yet — see the module doc's testing note.
    #[cfg(target_os = "linux")]
    #[test]
    fn fork_child_is_confined_to_the_table() {
        let tmp = std::env::temp_dir().join(format!("portal-sandbox-{}", std::process::id()));
        let inside = tmp.join("data");
        let outside = tmp.join("elsewhere");
        std::fs::create_dir_all(&inside).expect("create inside");
        std::fs::create_dir_all(&outside).expect("create outside");

        let code = in_a_fresh_process(
            "sandbox::tests::confinement_child",
            &[
                ("PORTAL_SANDBOX_INSIDE", &inside),
                ("PORTAL_SANDBOX_OUTSIDE", &outside),
            ],
        );
        let _ = std::fs::remove_dir_all(&tmp);

        // 3 = the kernel gave us nothing, which is a skip rather than a failure.
        assert!(
            code == CHILD_OK || code == 3,
            "landlock did not confine the child (code {code})"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "child process of fork_child_is_confined_to_the_table"]
    fn confinement_child() {
        use std::io::Write;
        if !is_child() {
            return;
        }
        let inside = child_path("PORTAL_SANDBOX_INSIDE");
        let outside = child_path("PORTAL_SANDBOX_OUTSIDE");

        let status = apply(&confined_to(&inside));
        // `is_confined`, not `!is_degraded`: on an ABI V3 kernel the state is
        // Partial and the filesystem rules — the only thing this test probes —
        // are fully in force.
        let confined = status.state.is_confined();
        let can_write_inside = std::fs::File::create(inside.join("probe"))
            .and_then(|mut f| f.write_all(b"x"))
            .is_ok();
        let can_write_outside = std::fs::File::create(outside.join("probe")).is_ok();
        // CHILD_OK = confined as designed; other values encode what broke.
        let code = match (confined, can_write_inside, can_write_outside) {
            (true, true, false) => CHILD_OK,
            (true, true, true) => 1, // outside not blocked
            (true, false, _) => 2,   // own data dir blocked
            (false, _, _) => 3,      // kernel gave us nothing
        };
        std::process::exit(code);
    }

    /// The `control.radio` seam's two rows carry the rights it actually uses
    /// (`plan/portal/monolith.md` §15): the rendered control config reads and
    /// does not write, and the radio tree takes `Cache::allocate_seq`'s exact
    /// shape — create the cache directory, write a temp file, rename it over
    /// the document in the same directory, twice — while a path outside the
    /// table stays denied.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_radio_seam_paths_are_in_the_table() {
        let tmp = std::env::temp_dir().join(format!("portal-sandbox-radio-{}", std::process::id()));
        let data = tmp.join("data");
        let control = tmp.join("control");
        let radio = tmp.join("radio");
        let outside = tmp.join("elsewhere");
        for dir in [&data, &control, &radio, &outside] {
            std::fs::create_dir_all(dir).expect("create a stand-in directory");
        }
        std::fs::write(control.join("config.yaml"), "radio: {}\n").expect("seed the config");

        let code = in_a_fresh_process(
            "sandbox::tests::radio_seam_child",
            &[
                ("PORTAL_SANDBOX_INSIDE", &data),
                ("PORTAL_SANDBOX_CONTROL", &control),
                ("PORTAL_SANDBOX_RADIO", &radio),
                ("PORTAL_SANDBOX_OUTSIDE", &outside),
            ],
        );
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            code == CHILD_OK || code == 3,
            "the radio seam's rows are wrong in the confined child (code {code})"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "child process of the_radio_seam_paths_are_in_the_table"]
    fn radio_seam_child() {
        if !is_child() {
            return;
        }
        let inside = child_path("PORTAL_SANDBOX_INSIDE");
        let control = child_path("PORTAL_SANDBOX_CONTROL");
        let radio = child_path("PORTAL_SANDBOX_RADIO");
        let outside = child_path("PORTAL_SANDBOX_OUTSIDE");

        let mut p = confined_to(&inside);
        p.radio_config_dir = Some(control.to_string_lossy().into_owned());
        p.radio_data_dir = Some(radio.to_string_lossy().into_owned());
        if !apply(&p).state.is_confined() {
            std::process::exit(3);
        }

        let reads_config = std::fs::read_to_string(control.join("config.yaml")).is_ok();
        let config_stays_read_only = std::fs::File::create(control.join("probe")).is_err();
        let cache = radio.join("lorawan").join("cache");
        let (tmp, doc) = (cache.join("node.tmp"), cache.join("node.json"));
        let allocates = std::fs::create_dir_all(&cache)
            .and_then(|()| std::fs::write(&tmp, b"{\"seq\":0}"))
            .and_then(|()| std::fs::rename(&tmp, &doc))
            .and_then(|()| std::fs::write(&tmp, b"{\"seq\":1}"))
            .and_then(|()| std::fs::rename(&tmp, &doc))
            .is_ok();
        let outside_blocked = std::fs::File::create(outside.join("probe")).is_err();

        // CHILD_OK = confined as designed; other values encode what broke.
        let code = match (
            reads_config,
            config_stays_read_only,
            allocates,
            outside_blocked,
        ) {
            (true, true, true, true) => CHILD_OK,
            (false, _, _, _) => 1, // the rendered radio config is unreadable
            (_, false, _, _) => 2, // ... or writable
            (_, _, false, _) => 4, // the sequence cache cannot be written
            (_, _, _, false) => 5, // outside the table not blocked
        };
        std::process::exit(code);
    }

    /// Enforces landlock.md §4: the restriction must hold on threads created
    /// *after* it, which is the failure mode that passes every other test.
    /// Builds a runtime the way `main()` does, then probes from a worker.
    #[cfg(target_os = "linux")]
    #[test]
    fn restriction_holds_on_worker_threads() {
        let tmp = std::env::temp_dir().join(format!("portal-sandbox-thr-{}", std::process::id()));
        let inside = tmp.join("data");
        let outside = tmp.join("elsewhere");
        std::fs::create_dir_all(&inside).expect("create inside");
        std::fs::create_dir_all(&outside).expect("create outside");

        let code = in_a_fresh_process(
            "sandbox::tests::worker_thread_child",
            &[
                ("PORTAL_SANDBOX_INSIDE", &inside),
                ("PORTAL_SANDBOX_OUTSIDE", &outside),
            ],
        );
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            code == CHILD_OK || code == 3,
            "worker thread escaped confinement (code {code})"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "child process of restriction_holds_on_worker_threads"]
    fn worker_thread_child() {
        if !is_child() {
            return;
        }
        let inside = child_path("PORTAL_SANDBOX_INSIDE");
        let outside = child_path("PORTAL_SANDBOX_OUTSIDE");

        let status = apply(&confined_to(&inside));
        // Skip only when the kernel enforced *nothing*. A Partial state still
        // confines the filesystem, which is what this probes — and Partial is
        // precisely the state the old `is_degraded` check treated as
        // "unavailable", skipping the one assertion that validates
        // landlock.md §4's thread rule.
        if !status.state.is_confined() {
            std::process::exit(3);
        }
        let Ok(rt) = tokio::runtime::Builder::new_multi_thread().build() else {
            std::process::exit(5);
        };
        let blocked = rt.block_on(async {
            tokio::task::spawn_blocking(move || {
                std::fs::File::create(outside.join("probe")).is_err()
            })
            .await
            .unwrap_or(false)
        });
        std::process::exit(if blocked { CHILD_OK } else { 1 });
    }

    /// A full `VACUUM` must still work once the process is confined.
    ///
    /// This guards a dependency on a *compile flag we do not set*:
    /// `bundled-sqlcipher` builds with `-DSQLITE_TEMP_STORE=2`, so SQLite's
    /// temporary databases live in memory and never touch the filesystem —
    /// which is the only reason the ruleset needs no `/tmp` entry. Switch the
    /// recipe to a system SQLCipher built with `SQLITE_TEMP_STORE=1` and temp
    /// files become real files under `/var/tmp`, outside the table, and
    /// maintenance starts failing on the device with `SQLITE_CANTOPEN`.
    ///
    /// `main()` points `SQLITE_TMPDIR` inside the data directory so that either
    /// build works; this asserts it, confined, rather than trusting it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_confined_process_can_still_vacuum() {
        // A re-exec, not a `fork()`. libtest runs tests on a thread pool, so
        // forking here copies only the calling thread while every lock the
        // others hold — the malloc arena, the environment lock `set_var`
        // takes, an `OnceLock` mid-initialisation — stays locked forever in
        // the child, with nobody left to release it. `Storage::open`
        // allocates on its first line, so the child parked in `futex` and
        // the parent blocked in `waitpid`: `make check` **hung** rather than
        // failed, which is the worst way for a test to be wrong. `Command`
        // execs a fresh process and inherits no locks at all.
        //
        // The hang was invisible in `make check-linux` only because that
        // target passes `--test-threads=1`, which leaves no other thread to
        // hold anything. The two `fork()`s above have the same hazard and
        // have so far won the race; they are left alone here deliberately,
        // as one behaviour change at a time.
        let tmp = std::env::temp_dir().join(format!("portal-sandbox-vac-{}", std::process::id()));
        let data = tmp.join("data");
        std::fs::create_dir_all(data.join("tmp")).expect("create data/tmp");

        let code = in_a_fresh_process(
            "sandbox::tests::confined_vacuum_child",
            &[("PORTAL_SANDBOX_DATA", &data)],
        );
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            code == CHILD_OK || code == 3,
            "a confined process could not vacuum its own database (code {code})"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "child process of a_confined_process_can_still_vacuum"]
    fn confined_vacuum_child() {
        if !is_child() {
            return;
        }
        let data = child_path("PORTAL_SANDBOX_DATA");

        // Same placement as `main()`: before the ruleset, so the directory
        // exists and the variable is set while still single-threaded.
        std::env::set_var("SQLITE_TMPDIR", data.join("tmp"));

        if !apply(&confined_to(&data)).state.is_confined() {
            std::process::exit(3);
        }

        let code = match portal_storage::Storage::open(&data.join("portal.db"), None) {
            Ok(storage) => {
                if storage.set_temp_store_file().is_err() {
                    4
                } else if storage.full_vacuum().is_err() {
                    // The failure this test exists to catch.
                    1
                } else {
                    CHILD_OK
                }
            }
            Err(_) => 2,
        };
        std::process::exit(code);
    }
}
