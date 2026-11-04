//! The typed merge: shipped defaults ← device overlay ← legacy portal override, then the
//! hardware's default `radio.mode` where none of them named one, and last the Hub's runtime
//! `radio.mode` from `fiber.service`'s own config.
//!
//! Three documents, one fact and one key in, one validated [`Effective`] out, and nothing is
//! written until that conversion has succeeded. The shim's own header called plain concatenation "the honest
//! limitation" of doing this in shell; this is the replacement it asked for.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::{data_paths, read_optional, Error, RenderPaths, Result};

/// Deep-merge `over` into `base`.
///
/// Mappings merge key by key, recursively. **Everything else replaces**, sequences
/// included: an overlay that lists one redis server replaces the default list rather than
/// appending to it. The asymmetry is deliberate — an appending list merge gives no way to
/// express removal, so every list would become write-only.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Mapping(b), Value::Mapping(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

pub(crate) fn parse(path: &Path, text: &str) -> Result<Value> {
    let v: Value = serde_yaml::from_str(text).map_err(|source| Error::Config {
        path: path.to_path_buf(),
        source,
    })?;
    // An empty file parses as Null, and merging Null over a mapping would wipe it. Treat
    // "the file exists but says nothing" as "no overrides", which is what it means.
    Ok(match v {
        Value::Null => Value::Mapping(Default::default()),
        other => other,
    })
}

/// The portal's rendered keys.
///
/// `deny_unknown_fields`, which is a deliberate tightening over the shim: the shim
/// appended an override file verbatim, so a typo in it produced a config the portal
/// silently ignored. Here it names the key and fails the render.
///
/// Mirrors `portal/src/config.rs`'s `RawConfig`. `db_key` is **absent on purpose** and
/// therefore unsettable from any config file: `/data/portal/db.key` is the single
/// authority for it, and a passphrase pasted into a config file is a passphrase in the
/// next backup, support bundle and journal line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PortalSection {
    pub bind_addr: String,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub topic_prefix: String,
    /// The broker client id the portal connects with (plan/portal/mqtt/design.md
    /// §6.2, §12). Rendered rather than derived: 3.1.1 makes a same-id connect a
    /// takeover, so the portal and the agent's radio tasks must not be able to
    /// arrive at one string by accident. Transitional — the monolith merge
    /// collapses it into `proximos-control`.
    pub mqtt_client_id: String,
    pub data_dir: String,
    pub templates_dir: String,
    pub static_dir: String,
    pub cookie_secure: bool,
    /// When a scheduled full `VACUUM` may start. Set by `defaults.yaml`, so every
    /// rendered Hub gets one; an overlay may move it, and removing the block turns
    /// scheduled maintenance off (`portal/src/config.rs` treats absent as "never").
    ///
    /// Not `Option`-and-skipped like the three below: this is a key `defaults.yaml`
    /// *does* set, so it renders unconditionally.
    pub maintenance_window: MaintenanceWindowSection,

    /// The three keys `portal/src/config.rs` accepts that `defaults.yaml` does not set.
    /// Rendered only when an overlay sets one, so the emitted document stays exactly the
    /// shim's nine lines on an unconfigured device.
    ///
    /// Serialized as `null` rather than skipped, deliberately: the template asks
    /// `is not none`, and a key that had been *omitted* from the context would be
    /// `undefined` instead — which is not `none` in Jinja semantics, so every optional
    /// key would render on every device. That is a real bug this crate shipped for
    /// exactly one test run.
    #[serde(default)]
    pub mqtt_username: Option<String>,
    #[serde(default)]
    pub mqtt_password: Option<String>,
    #[serde(default)]
    pub experimental_features_enabled: Option<bool>,

    /// The three device-tuning sections (`portal/src/config.rs`'s
    /// `nodes:` / `beacons:` / `signal_quality:`, devices.backend.md §3
    /// WP0.3). Optional for the same reason the three keys above are: an
    /// unconfigured device must still render exactly the shim's nine lines,
    /// which `tests/shim_parity.rs` asserts byte-for-byte.
    ///
    /// Carried as opaque `serde_yaml::Value` rather than mirrored structs.
    /// This crate does not *use* these values — it moves an operator's
    /// overlay into the file the portal reads — and the portal's own
    /// sections are `deny_unknown_fields`, so it is already the side that
    /// rejects a typo, with a better error and no second definition to keep
    /// in step. The one thing this crate must still enforce is that the
    /// section names themselves are real, which `deny_unknown_fields` on
    /// this struct does.
    #[serde(default)]
    pub nodes: Option<Value>,
    #[serde(default)]
    pub beacons: Option<Value>,
    #[serde(default)]
    pub signal_quality: Option<Value>,

    /// `thresholds:` — the default bands a new temperature or humidity sensor gets
    /// (`portal/src/config.rs`'s `ThresholdDefaults`, plan/portal/threshold_defaults.md §2).
    /// Opaque for the reason the three sections above are: the portal's own block is
    /// `deny_unknown_fields` and says what a typo is. Unlike them, `defaults.yaml` sets it,
    /// so every Hub renders it; it is one of `tests/shim_parity.rs`'s post-shim keys.
    #[serde(default)]
    pub thresholds: Option<Value>,
    /// `calibration:` (plan/portal/calibration.cs.md §3) — the reminder
    /// windows, carried exactly like the three sections above and for the
    /// same reasons: optional so an unconfigured device keeps the shim's
    /// output, opaque because the portal's own `deny_unknown_fields`
    /// section is the one that validates it.
    #[serde(default)]
    pub calibration: Option<Value>,
}

/// Mirrors `portal/src/config.rs`'s `RawMaintenanceWindow`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceWindowSection {
    /// `"HH:MM"`, UTC. Validated on the reading side, which refuses to start on a
    /// malformed value rather than silently running no maintenance at all.
    pub start_utc: String,
    pub duration_minutes: u32,
}

/// `radio.mode` — the service gate.
///
/// A local copy of `control_radio::mode::Mode`, spelled identically, because
/// `control-radio` may depend on no other crate in this workspace (decision #12,
/// monolith.md §11.7) and inverting that — this crate depending on it — would put the
/// whole tonic/gRPC client and `chirpstack_api` into a boot-path renderer that needs
/// none of it. The shape is pinned from the other side instead:
/// `tests/control_schema.rs` deserializes this crate's rendered output with a
/// `deny_unknown_fields` mirror of `control-radio`'s `RadioConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Lorawan,
    P2p,
    Dual,
}

impl Mode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Lorawan => "lorawan",
            Mode::P2p => "p2p",
            Mode::Dual => "dual",
        }
    }

    /// Whether a ChirpStack network server is part of this Hub's service set at all.
    /// The single question the ChirpStack and forwarder outputs are gated on.
    pub const fn wants_lorawan_server(self) -> bool {
        matches!(self, Mode::Lorawan | Mode::Dual)
    }
}

/// `radio.region`. One variant, because v2 physically ships and supports only EU868.
///
/// It appears in four rendered places — `enabled_regions`, the region file's name, that
/// file's `[[regions]] name`, and the topic prefix on both sides of the gateway backend
/// — which is exactly why it is one config value and not four constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    Eu868,
}

impl Region {
    pub const fn as_str(self) -> &'static str {
        match self {
            Region::Eu868 => "eu868",
        }
    }

    /// ChirpStack's `[[regions]] common_name`, which is the upper-case spelling of the
    /// same thing. Verified against `tests/integration/region_eu868.toml`.
    pub const fn common_name(self) -> &'static str {
        match self {
            Region::Eu868 => "EU868",
        }
    }
}

/// The two fields of `radio:` this renderer itself reads.
///
/// **Not** `deny_unknown_fields`, and that is the point: the rest of the section is
/// control-radio's schema, is passed through untouched, and is validated by
/// control-radio. Modelling it here would be the hand-mirroring of a schema that
/// `plan/portal/monolith.md` §12.1 counts as a defect class.
#[derive(Debug, Clone, Copy, Deserialize)]
struct RadioView {
    mode: Mode,
    region: Region,
}

/// The renderer-only knobs for `chirpstack.toml`.
///
/// Under `render:` rather than `radio:` for two independent reasons: `radio:` is
/// LNS-agnostic by design (plan.md §4) and would be the wrong home for a redis URL, and
/// `radio:` is handed to control-radio verbatim, whose `deny_unknown_fields` structs
/// would reject every key here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChirpstackSection {
    pub log_level: String,
    pub api_bind: String,
    pub sqlite_path: String,
    pub redis_servers: Vec<String>,
    pub net_id: String,
    pub mqtt_server: String,
    pub mqtt_json: bool,
}

/// The renderer-only knobs for `chirpstack-mqtt-forwarder.toml`. `topic_prefix` is
/// absent on purpose — it is `radio.region` and nothing else, which is what stops it
/// drifting from the region file ChirpStack reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwarderSection {
    pub log_level: String,
    pub log_to_syslog: bool,
    pub concentratord_event_url: String,
    pub concentratord_command_url: String,
    pub mqtt_server: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderSection {
    chirpstack: ChirpstackSection,
    forwarder: ForwarderSection,
}

/// The merged, validated configuration one render works from.
#[derive(Debug, Clone)]
pub(crate) struct Effective {
    pub portal: PortalSection,
    pub mode: Mode,
    pub region: Region,
    pub chirpstack: ChirpstackSection,
    pub forwarder: ForwarderSection,
    /// `radio:` serialized back to YAML, exactly as it goes into the control config —
    /// the merged tree, not a re-modelled one. No trailing newline; the template supplies
    /// it.
    pub radio_yaml: String,
    /// What `/data/proximos/cluster/` says this Hub is (`plan/control/cluster.md` §3, §4).
    pub role: Role,
}

/// The Hub's cluster role, as the renderer reads it.
///
/// `role` is the gate and the four value files are the payload. The join writes `role`
/// after the payload (cluster.md §3), so a client marker with an incomplete payload is a
/// damaged `/data`, not a join in progress — and per this crate's failure policy that is never
/// fatal: [`Role::Client`] carries the reason instead, and the outputs that need the
/// payload skip it. `setup:` is never consulted: the join writes it after `role`, and a Hub
/// whose join stopped between the two is a Client (the agent completes its setup at start).
#[derive(Debug, Clone)]
pub(crate) enum Role {
    /// No marker, or one that does not say `client`. Absence means server (decision #5).
    Server,
    /// `role` says `client`. The payload, or why it could not be read.
    Client(std::result::Result<ClusterPeer, String>),
}

impl Role {
    pub(crate) fn is_client(&self) -> bool {
        matches!(self, Role::Client(_))
    }
}

/// The Server a Client Hub joins: its broker address and this Hub's account on it.
///
/// One read feeds two templates — the forwarder TOML and the control config's
/// `mqtt.broker` override — so they cannot disagree. Only the forwarder gets the password;
/// the control config gets the path of the file that holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ClusterPeer {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// The secret itself, for the **forwarder TOML only** (0640 root:root — the forwarder
    /// runs as root and has no way to read a password from a file of its own). Never
    /// serialized: the control config is 0640 root:proximos, readable by
    /// `proximos-control`, so it names [`Self::password_file`] instead and the agent
    /// (root) reads the file itself. Skipping it here means no template can emit it from
    /// this struct by accident.
    #[serde(skip_serializing)]
    pub password: String,
    /// `/data/proximos/cluster/peer_password` as the agent opens it (0600 root). A device
    /// path, like [`Self::ca_cert_path`].
    pub password_file: String,
    /// The CA the Server's 8883 certificate chains to, as the **consumers** spell it
    /// (`/data/proximos/cluster/ca.crt`). A device path, not `paths.data`-relative: the
    /// renderer never opens the file, the agent and the forwarder do. Its absence is the
    /// agent's to refuse, loudly (cluster.md §4.5), not a reason to skip a render.
    pub ca_cert_path: String,
}

impl ClusterPeer {
    /// `ssl://<host>:<port>`, the forwarder's `[mqtt] server`. No brackets: an IPv6
    /// literal never gets this far ([`read_cluster_peer`] refuses it).
    pub(crate) fn ssl_url(&self) -> String {
        format!("ssl://{}:{}", self.host, self.port)
    }
}

impl Effective {
    /// `default_mode` is the hardware's answer (`plan/control/radio/node_registry.md` §12),
    /// used only when neither the defaults, the overlay nor the Hub's own config name a mode.
    pub(crate) fn load(paths: &RenderPaths, default_mode: Mode) -> Result<Self> {
        let overlay_path = paths.data.join(data_paths::OVERLAY);
        let overlay = match read_optional(&overlay_path)? {
            Some(text) => Some(parse(&overlay_path, &text)?),
            None => None,
        };
        Self::load_with_overlay(paths, overlay, default_mode)
    }

    /// [`Effective::load`] with the overlay supplied by the caller instead of read from
    /// `/data`. `first-run-apply` validates the overlay it is about to write through this,
    /// so an overlay the renderer would refuse is never put on disk.
    pub(crate) fn load_with_overlay(
        paths: &RenderPaths,
        overlay: Option<Value>,
        default_mode: Mode,
    ) -> Result<Self> {
        let defaults_path = paths.templates.join("defaults.yaml");
        let defaults_text =
            std::fs::read_to_string(&defaults_path).map_err(|source| Error::Read {
                path: defaults_path.clone(),
                source,
            })?;
        let mut doc = parse(&defaults_path, &defaults_text)?;

        // 2. the whole-device overlay.
        if let Some(overlay) = overlay {
            merge(&mut doc, overlay);
        }

        // 3. the legacy portal-only override — a FLAT document (bind_addr at top level),
        //    which is why it merges into the `portal:` subtree rather than the root.
        let portal_override_path = paths.data.join(data_paths::PORTAL_OVERRIDE);
        if let Some(text) = read_optional(&portal_override_path)? {
            let over = parse(&portal_override_path, &text)?;
            let root = doc.as_mapping_mut().ok_or_else(|| Error::Invalid {
                path: defaults_path.clone(),
                message: "the defaults document is not a mapping".to_string(),
            })?;
            let key = Value::String("portal".to_string());
            match root.get_mut(&key) {
                Some(existing) => merge(existing, over),
                None => {
                    root.insert(key, over);
                }
            }
        }

        // 4. the hardware's default mode (node_registry.md §12): `defaults.yaml` names no mode,
        //    so a Hub whose overlay names none either runs what its hardware suggests.
        default_radio_mode(&mut doc, default_mode, &defaults_path)?;

        // 5. the Hub's own runtime radio mode (node_registry.md §11, amended 2026-09-27 R7):
        //    one key of `fiber.service`'s config, last, so a signed `radio-set-mode` or the
        //    provisioning choice (BLE FB1B) wins over the overlay and the hardware default.
        if let Some(mode) = read_hub_radio_mode(paths)? {
            set_radio_mode(&mut doc, mode, &defaults_path)?;
        }

        // Everything below is the "typed" half of the typed merge: the merged Value is
        // turned into structs, and a key nobody models is a named failure here rather
        // than a setting that does nothing on the device.
        let section = |name: &str| -> Result<Value> {
            doc.get(name).cloned().ok_or_else(|| Error::Invalid {
                path: defaults_path.clone(),
                message: format!("no `{name}:` section after the merge"),
            })
        };
        let portal: PortalSection = typed("portal", section("portal")?)?;
        let render: RenderSection = typed("render", section("render")?)?;
        let radio_value = section("radio")?;
        let radio: RadioView = typed("radio", radio_value.clone())?;

        // Re-serialized from the merged tree, so an overlay that adds a fourth OTAA
        // profile reaches control-radio unmodelled and unmangled.
        let mut wrapper = serde_yaml::Mapping::new();
        wrapper.insert(Value::String("radio".to_string()), radio_value);
        let radio_yaml = serde_yaml::to_string(&Value::Mapping(wrapper))
            .map_err(|source| Error::Config {
                path: PathBuf::from("<merged configuration>:radio"),
                source,
            })?
            .trim_end_matches('\n')
            .to_string();

        Ok(Self {
            portal,
            mode: radio.mode,
            region: radio.region,
            chirpstack: render.chirpstack,
            forwarder: render.forwarder,
            radio_yaml,
            role: read_cluster_role(paths)?,
        })
    }
}

/// Deserialize one merged section into its typed shape.
///
/// The error names the **section**, not one of the three input files: after a merge the
/// offending key may have come from any of them, and picking one would be a guess printed
/// as a fact.
fn typed<T: serde::de::DeserializeOwned>(name: &str, v: Value) -> Result<T> {
    serde_yaml::from_value(v).map_err(|source| Error::Config {
        path: PathBuf::from(format!("<merged configuration>:{name}")),
        source,
    })
}

/// `radio.mode` as the Hub's own config holds it, or `None` when it holds none.
///
/// The file is `fiber.service`'s, not this crate's, so only the one key is read and the rest
/// of the document is never modelled here. Absent file, no `radio:` section or no `mode:` in
/// it all mean "no runtime setting": the overlay's mode or the hardware default applies, which
/// is also what a factory reset — it takes the whole file — returns a Hub to.
///
/// The failure policy is the overlay's: a document that does not parse, or a `radio.mode` that
/// is not one of the four, is fatal. Rendering the factory mode instead would be a guess about
/// which radio the operator asked for.
fn read_hub_radio_mode(paths: &RenderPaths) -> Result<Option<Mode>> {
    let path = paths.data.join(data_paths::HUB_CONFIG);
    let Some(text) = read_optional(&path)? else {
        return Ok(None);
    };
    let doc = parse(&path, &text)?;
    let Some(radio) = doc.get("radio").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(mode) = radio.get("mode").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    serde_yaml::from_value::<Mode>(mode.clone())
        .map(Some)
        .map_err(|_| Error::Invalid {
            path,
            message: format!(
                "radio.mode {} is not a radio mode: expected off, lorawan, p2p or dual",
                serde_yaml::to_string(mode)
                    .unwrap_or_default()
                    .trim_end_matches('\n')
            ),
        })
}

/// Give the merged `radio:` section `mode` when it names none (absent, or `null`).
///
/// The key goes right after `region`, where `defaults.yaml` carried it until §12 took it out,
/// so the rendered control config keeps its key order whichever layer named the mode.
fn default_radio_mode(doc: &mut Value, mode: Mode, defaults_path: &Path) -> Result<()> {
    let radio = doc
        .get_mut("radio")
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| Error::Invalid {
            path: defaults_path.to_path_buf(),
            message: "no `radio:` mapping to give a default radio mode".to_string(),
        })?;
    let key = Value::String("mode".to_string());
    if radio.get(&key).is_some_and(|v| !v.is_null()) {
        return Ok(());
    }
    let value = Value::String(mode.as_str().to_string());
    let mut placed = false;
    for (k, v) in std::mem::take(radio) {
        if k == key {
            continue;
        }
        let after = k.as_str() == Some("region");
        radio.insert(k, v);
        if after {
            radio.insert(key.clone(), value.clone());
            placed = true;
        }
    }
    if !placed {
        radio.insert(key, value);
    }
    Ok(())
}

/// Put `mode` into the merged document's `radio:` section, which the defaults always carry.
fn set_radio_mode(doc: &mut Value, mode: Mode, defaults_path: &Path) -> Result<()> {
    let radio = doc
        .get_mut("radio")
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| Error::Invalid {
            path: defaults_path.to_path_buf(),
            message: "no `radio:` mapping to set the Hub's radio mode in".to_string(),
        })?;
    radio.insert(
        Value::String("mode".to_string()),
        Value::String(mode.as_str().to_string()),
    );
    Ok(())
}

/// The client/server gate, ported from the shim's own role check, and the Client's payload.
///
/// An **absent** marker means server, deliberately: it is what makes a single bench Hub
/// with no cluster configuration at all serve its own portal, which is the only topology
/// that exists today. Once role assignment is real, an explicit `client` is the only
/// thing that turns the portal off.
///
/// Compared against an exact token after stripping surrounding whitespace, not a prefix —
/// a hand-written marker must not match by accident in either direction.
fn read_cluster_role(paths: &RenderPaths) -> Result<Role> {
    let path = paths.data.join(data_paths::CLUSTER_ROLE);
    let client = read_optional(&path)?
        .map(|s| s.trim() == "client")
        .unwrap_or(false);
    if !client {
        return Ok(Role::Server);
    }
    Ok(Role::Client(read_cluster_peer(paths)))
}

/// The four value files of cluster.md §3, one value per file, surrounding whitespace
/// stripped (the writer is `ClusterState`, which writes no newline, but a hand-written file
/// will have one).
///
/// Validated only as far as **this crate's** job needs: every value is substituted into a
/// TOML and a YAML document, so nothing that could change the document's shape passes,
/// whatever the templates' own quoting does. The credential rules proper —
/// `validate_arm`'s `peer-` prefix and 16-character floor, mirrored here so a hand-edited
/// marker cannot render a config the join itself would have refused — are the join's
/// (C4). Every failure is a reason string, never an `Error`: see [`Role`].
fn read_cluster_peer(paths: &RenderPaths) -> std::result::Result<ClusterPeer, String> {
    let read = |rel: &str| -> std::result::Result<String, String> {
        let p = paths.data.join(rel);
        match std::fs::read_to_string(&p) {
            Ok(s) => {
                let v = s.trim().to_string();
                if v.is_empty() {
                    Err(format!("{} is empty", p.display()))
                } else if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    Err(format!("{} holds more than one token", p.display()))
                } else if v.contains(['"', '\\', '\'', '#']) {
                    Err(format!(
                        "{} contains a quote, backslash or '#'",
                        p.display()
                    ))
                } else {
                    Ok(v)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(format!("{} does not exist", p.display()))
            }
            Err(e) => Err(format!("cannot read {}: {e}", p.display())),
        }
    };

    let host = read(data_paths::CLUSTER_SERVER_HOST)?;
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&host)
        .to_string();
    // The Server's 8883 listener is `0.0.0.0`, IPv4 only (cluster.md §5, decided
    // 2026-10-02), so an IPv6 literal names an address nothing listens on. Refused here
    // rather than rendered into a forwarder and an agent that would retry it forever.
    if bare.parse::<std::net::Ipv6Addr>().is_ok() {
        return Err(format!(
            "{}: {host:?} is an IPv6 literal, and the Server's 8883 listener is IPv4 only; \
             name the Server (`<hostname>.local`) or give its IPv4 address",
            paths.data.join(data_paths::CLUSTER_SERVER_HOST).display()
        ));
    }
    let is_ip = bare.parse::<std::net::IpAddr>().is_ok();
    let is_name = !bare.is_empty()
        && bare.len() <= 253
        && bare
            .split('.')
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    if !is_ip && !is_name {
        return Err(format!(
            "{}: {host:?} is neither a hostname nor an IP literal",
            paths.data.join(data_paths::CLUSTER_SERVER_HOST).display()
        ));
    }

    let port_text = read(data_paths::CLUSTER_SERVER_PORT)?;
    let port = match port_text.parse::<u16>() {
        Ok(0) | Err(_) => {
            return Err(format!(
                "{}: {port_text:?} is not a TCP port",
                paths.data.join(data_paths::CLUSTER_SERVER_PORT).display()
            ))
        }
        // The Server's plaintext loopback listener; validate_arm refuses it for the same
        // reason — a Client's broker is the TLS one, always.
        Ok(1883) => {
            return Err(format!(
                "{}: 1883 is the Server's plaintext loopback listener, not its TLS one",
                paths.data.join(data_paths::CLUSTER_SERVER_PORT).display()
            ))
        }
        Ok(p) => p,
    };

    let username = read(data_paths::CLUSTER_PEER_USERNAME)?;
    if !username.starts_with("peer-") || username.contains(':') || username.len() > 128 {
        return Err(format!(
            "{}: a peer account is `peer-<hostname>`, at most 128 characters, no ':'",
            paths.data.join(data_paths::CLUSTER_PEER_USERNAME).display()
        ));
    }

    let password = read(data_paths::CLUSTER_PEER_PASSWORD)?;
    if password.len() < 16 {
        return Err(format!(
            "{} is shorter than 16 characters",
            paths.data.join(data_paths::CLUSTER_PEER_PASSWORD).display()
        ));
    }

    Ok(ClusterPeer {
        host: bare,
        port,
        username,
        password,
        password_file: format!(
            "{}/{}",
            crate::DEFAULT_DATA_DIR,
            data_paths::CLUSTER_PEER_PASSWORD
        ),
        ca_cert_path: format!("{}/{}", crate::DEFAULT_DATA_DIR, data_paths::CLUSTER_CA),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn mappings_merge_recursively() {
        let mut base = yaml("a:\n  b: 1\n  c: 2\n");
        merge(&mut base, yaml("a:\n  c: 3\n  d: 4\n"));
        assert_eq!(base, yaml("a:\n  b: 1\n  c: 3\n  d: 4\n"));
    }

    #[test]
    fn sequences_replace_rather_than_append() {
        let mut base = yaml("s: [1, 2, 3]\n");
        merge(&mut base, yaml("s: [9]\n"));
        assert_eq!(base, yaml("s: [9]\n"));
    }

    #[test]
    fn a_scalar_replaces_a_mapping() {
        let mut base = yaml("a:\n  b: 1\n");
        merge(&mut base, yaml("a: plain\n"));
        assert_eq!(base, yaml("a: plain\n"));
    }

    #[test]
    fn merge_preserves_base_key_order_and_appends_new_keys() {
        // The control config is re-serialized from the merged tree, so key order is
        // observable in a golden file. It must follow the defaults, not the overlay.
        let mut base = yaml("z: 1\na: 2\n");
        merge(&mut base, yaml("a: 3\nm: 4\n"));
        assert_eq!(serde_yaml::to_string(&base).unwrap(), "z: 1\na: 3\nm: 4\n");
    }

    #[test]
    fn mode_spellings_match_control_radios() {
        for (text, want) in [
            ("off", Mode::Off),
            ("lorawan", Mode::Lorawan),
            ("p2p", Mode::P2p),
            ("dual", Mode::Dual),
        ] {
            let got: Mode = serde_yaml::from_str(text).unwrap();
            assert_eq!(got, want);
            assert_eq!(got.as_str(), text);
        }
        assert!(serde_yaml::from_str::<Mode>("lorawan-and-p2p").is_err());
    }

    #[test]
    fn only_lorawan_and_dual_want_a_network_server() {
        assert!(!Mode::Off.wants_lorawan_server());
        assert!(!Mode::P2p.wants_lorawan_server());
        assert!(Mode::Lorawan.wants_lorawan_server());
        assert!(Mode::Dual.wants_lorawan_server());
    }

    #[test]
    fn db_key_is_not_settable_from_a_config_file() {
        // The whole point of the deny_unknown_fields on PortalSection: an operator who
        // writes db_key into an override gets a named error, not a silently ignored key
        // and not a second authority for the passphrase.
        let err = serde_yaml::from_str::<PortalSection>(
            "bind_addr: \"127.0.0.1:8088\"\nmqtt_host: localhost\nmqtt_port: 1883\n\
             topic_prefix: fiber\nmqtt_client_id: proximos-portal\ndata_dir: /data/portal\n\
             templates_dir: t\nstatic_dir: s\n\
             cookie_secure: true\ndb_key: \"deadbeef\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("db_key"), "{err}");
    }
}
