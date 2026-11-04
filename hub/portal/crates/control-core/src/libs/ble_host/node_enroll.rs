//! `NodeEnroll` — FB0D/FB11/FB12, the Node half of the phone's enrolment surface.
//!
//! A Beacon crate that also enrols Nodes looks odd until you remember what FB0D is: the wizard's
//! "scan the Node's QR code" step, served over the same GATT connection as everything else in the
//! provisioning flow. The crate owns the characteristics and the response buffers; the actual
//! enrolment is the radio stack's, unchanged, and this file is the calls between them.
//!
//! **Two stacks, one seam (GitLab #118).** Which stack serves the three verbs is decided per call
//! by the same rule that gates the legacy stack, `radio_gate::disabled_reason`:
//!
//! - **`radio.mode` names a transport** (`lorawan`, `p2p`, `dual`): `proximos-control` owns the
//!   radio, and the Node registry (`plan/control/radio/node_registry.md`) is the one list of
//!   Nodes. FB0D, FB11 and FB12 go where the Portal's signed `node-add`/`node-remove` go —
//!   [`crate::libs::radio_nodes::run`] for the two mutations, `node_registry.json` read-only for
//!   the list. A Node added in the Portal is listed here, and one added here is in the Portal.
//! - **No rendered radio config, or `radio.mode: off`**: the legacy `libs::lorawan` path, exactly
//!   as before — `lorawan.sensors[]` in `fiber.config.yaml` and the v1 ChirpStack client.
//!
//! Until #118 the three verbs took the legacy path on every Hub. On a radio-mode Hub that path is
//! gated off, so FB0D was refused, FB11 read an empty `sensors[]` and FB12 found nothing.
//!
//! **Only OTAA reaches here.** `NodeAddRequest` carries `appkey` + `joineui` + an optional
//! `profile_id`, which is the QR label's content. ABP has no QR path and no phone flow; a Hub that
//! needs it uses the portal.
//!
//! **`pending` is preserved.** The add is synchronous from the handler's point of view — on the
//! legacy path the ChirpStack half is best-effort, on the registry path it is the signed command's
//! own round trip — so the phone sees an interim `pending: true` and then the final result out of
//! the crate's response buffer. This returns the final one; the buffer supplies the interim.
//!
//! **The legacy list is read from disk, not from the in-memory mirror.** `deps.lorawan_configs` is
//! an `Option`, and on a Hub whose LoRaWAN monitor never started it is `None` — but the phone is
//! standing in front of that Hub asking what it holds, and "nothing" would be a lie. So `list`
//! reads `lorawan.sensors[]` out of the persisted `fiber.config.yaml`, for the same reason
//! `node_add`'s path reads its ChirpStack credentials from disk on every call. The mirror is a
//! cache of that file; the file is the answer. The registry path reads its file the same way, on
//! every call, and never migrates it: `proximos-control` does that at its start.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use control_ble::host::{
    NodeAddRequest, NodeAddResponse, NodeEnroll, NodeEntry, NodeRemoveOutcome, NodeRemoveTarget,
};
use control_radio::config::RadioConfig;
use control_radio::p2p::session::canonical_serial;
use control_radio::registry::{NodeRecord, Registry};
use control_radio::Transport;

use crate::libs::config::LoRaWANSensorConfig;
use crate::libs::lorawan::{add_lorawan_node, remove_lorawan_node, NodeAddDeps};
use crate::libs::mqtt::messages::ActivationMode;
use crate::libs::radio_nodes::Op;

/// The v2 radio stack as the three Node verbs reach it: the rendered config that decides which
/// stack serves them, the Node registry they list and resolve against, and the signed commands'
/// `node-add`/`node-remove` they run.
///
/// A seam so a test can stand a Hub's radio up in a temporary directory — the shipped P2P arm
/// included — instead of `/run/proximos` and `/data/proximos/radio`.
pub trait RadioStack: Send + Sync {
    /// The rendered `radio:` config, or why there is none.
    fn config(&self) -> control_radio::Result<RadioConfig>;

    /// The Node registry as it stands, **read only**: `Ok(None)` when it was never written.
    /// Never `registry::current`, which migrates and writes on first use.
    fn registry(&self) -> control_radio::Result<Option<Registry>>;

    /// One `node-add`/`node-remove`, as the Portal's signed command runs it. The `Err` text is
    /// control-radio's own sentence.
    fn run(&self, op: Op) -> Result<(), String>;
}

/// The Hub's own radio stack: `/run/proximos/control/config.yaml` (`PROXIMOS_CONFIG`), the
/// registry under `/data/proximos/radio` (`PROXIMOS_RADIO_DATA`) and
/// [`crate::libs::radio_nodes::run`].
pub struct HubRadioStack;

impl RadioStack for HubRadioStack {
    fn config(&self) -> control_radio::Result<RadioConfig> {
        control_radio::dispatch::load_config()
    }

    fn registry(&self) -> control_radio::Result<Option<Registry>> {
        Registry::load(&control_radio::store::Store::new(
            control_radio::dispatch::store_root(),
        ))
    }

    fn run(&self, op: Op) -> Result<(), String> {
        crate::libs::radio_nodes::run(op)
    }
}

pub struct LorawanNodeEnroll {
    deps: NodeAddDeps,
    /// The persisted `fiber.config.yaml`, which the legacy `list` and `remove` resolve against.
    config_path: PathBuf,
    /// The v2 stack, which serves all three verbs on a radio-mode Hub.
    radio: Arc<dyn RadioStack>,
}

impl LorawanNodeEnroll {
    pub fn new(deps: NodeAddDeps, config_path: impl Into<PathBuf>) -> LorawanNodeEnroll {
        LorawanNodeEnroll {
            deps,
            config_path: config_path.into(),
            radio: Arc::new(HubRadioStack),
        }
    }

    /// The same, over another radio stack than the Hub's own.
    pub fn with_radio(mut self, radio: Arc<dyn RadioStack>) -> LorawanNodeEnroll {
        self.radio = radio;
        self
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// The rendered radio config when the v2 stack owns the radio, `None` when the legacy path
    /// serves this Hub.
    ///
    /// Exactly the rule `radio_gate` disables the legacy stack by, so the two cannot disagree: a
    /// verb the gate would refuse on the legacy path takes the registry path instead, and a Hub
    /// the gate leaves alone — no rendered config, or `radio.mode: off` — keeps the legacy path.
    /// Read on every call, like the gate: a mode re-rendered since the last call counts.
    fn v2_config(&self) -> Option<RadioConfig> {
        let loaded = self.radio.config();
        crate::libs::radio_gate::disabled_reason(&loaded)?;
        loaded.ok()
    }

    /// The registry, or the sentence an unreadable one is refused with.
    fn registry(&self) -> Result<Option<Registry>, String> {
        self.radio
            .registry()
            .map_err(|e| format!("cannot read the Node registry: {e}"))
    }

    /// `lorawan.sensors[]` as persisted.
    ///
    /// A config with no `lorawan:` section, or one whose `lorawan:` carries no `sensors:`, is an
    /// **empty list rather than an error** — that is a Hub with no Nodes enrolled, which is the
    /// ordinary state of a Beacon-only site. Unreadable or invalid YAML *is* an error, because then
    /// the Hub genuinely cannot answer and the phone must not be told "none".
    fn read_sensors(&self) -> Result<Vec<LoRaWANSensorConfig>, String> {
        let text = std::fs::read_to_string(&self.config_path)
            .map_err(|e| format!("cannot read {}: {e}", self.config_path.display()))?;
        let doc: serde_yaml::Value = serde_yaml::from_str(&text)
            .map_err(|e| format!("the device config is not valid YAML: {e}"))?;
        let Some(sensors) = doc.get("lorawan").and_then(|l| l.get("sensors")) else {
            return Ok(Vec::new());
        };
        // `null` is how an empty YAML sequence is often spelled by hand (`sensors:`).
        if sensors.is_null() {
            return Ok(Vec::new());
        }
        serde_yaml::from_value(sensors.clone())
            .map_err(|e| format!("the lorawan.sensors: section is invalid: {e}"))
    }

    /// Which enrolled `dev_eui`s a target names.
    ///
    /// Returned as a list rather than an `Option` so the ambiguous case stays visible: a serial that
    /// matches two rows must refuse, not pick one. Serial matching is **trim + exact** — a fuzzy
    /// match here would delete the wrong device.
    fn resolve(&self, target: &NodeRemoveTarget, sensors: &[LoRaWANSensorConfig]) -> Vec<String> {
        match target {
            NodeRemoveTarget::DevEui(v) => {
                let want = v.trim().to_lowercase();
                sensors
                    .iter()
                    .filter(|s| s.dev_eui.trim().to_lowercase() == want)
                    .map(|s| s.dev_eui.clone())
                    .collect()
            }
            NodeRemoveTarget::Serial(v) => {
                let want = v.trim();
                sensors
                    .iter()
                    .filter(|s| s.serial_number.as_deref().map(str::trim) == Some(want))
                    .map(|s| s.dev_eui.clone())
                    .collect()
            }
        }
    }

    /// FB0D on a radio-mode Hub: the Portal's registration, with what FB0D does not carry
    /// defaulted the way the Portal's dialog defaults it.
    ///
    /// - **Carrier:** [`control_radio::mode::registration_default`] — the Hub's mode, P2P on a
    ///   `dual` Hub. FB0D has no `radio` key.
    /// - **Profile:** `control_radio::lorawan::DEFAULT_PROFILE`. The v1 `profile_id` names a
    ///   legacy ChirpStack profile number that the v2 config has no use for, so it is ignored.
    /// - **JoinEUI:** all zeros means "not given", so the Hub's `radio.join_eui` applies, as for
    ///   a blank Portal field. The v1 contract makes the key mandatory, and the app sends zeros
    ///   when the Node's tag carries none. A P2P registration never carries one.
    /// - **Serial:** a STICKER serial (decimal, `0x`-hex or 8 hex digits), the Portal's rule.
    /// - **One carrier per Node:** a DevEUI the registry holds on the other carrier is refused
    ///   before anything runs. The LoRaWAN arm would otherwise move it there silently.
    fn add_v2(&self, req: &NodeAddRequest, config: &RadioConfig) -> NodeAddResponse {
        let refuse = |message: String| NodeAddResponse {
            pending: false,
            success: false,
            message,
            deveui: req.deveui.trim().to_lowercase(),
        };

        let prepared = match prepare(req) {
            Ok(p) => p,
            Err(e) => return refuse(e),
        };
        if canonical_serial(&prepared.serial_number).is_none() {
            return refuse(
                "serial_number must be the Node's numeric serial: decimal (2162165722), 0x-hex or \
                 8 hex digits (80e007da)"
                    .to_string(),
            );
        }
        let carrier = control_radio::mode::registration_default(Some(config.mode));
        // Unreachable while FB0D names no carrier — the default is always one the mode runs —
        // but kept with the Portal's check so a carrier key added later cannot skip it.
        if !config.mode.allows(carrier) {
            return refuse(format!(
                "{} is switched off on this Hub (radio.mode={}), so a Node cannot be registered \
                 on it",
                carrier_label(carrier),
                config.mode.as_str()
            ));
        }
        let held = match self.registry() {
            Ok(registry) => registry.and_then(|r| r.get(&prepared.dev_eui).map(|n| n.radio)),
            Err(e) => return refuse(e),
        };
        if let Some(on) = held.filter(|on| *on != carrier) {
            return refuse(format!(
                "deveui {} is already registered on {}. A Node is registered on one radio only; \
                 change its radio on its Portal page (Change radio). Nothing was sent.",
                prepared.dev_eui,
                carrier_label(on)
            ));
        }
        let join_eui = Some(prepared.join_eui)
            .filter(|_| carrier == Transport::Lorawan)
            .filter(|j| !j.bytes().all(|b| b == b'0'));

        let op = Op::Add {
            radio: carrier.as_str().to_string(),
            profile: control_radio::lorawan::DEFAULT_PROFILE.to_string(),
            serial: prepared.serial_number,
            dev_eui: prepared.dev_eui.clone(),
            name: Some(prepared.name),
            app_key: prepared.app_key,
            join_eui,
        };
        match self.radio.run(op) {
            Ok(()) => NodeAddResponse {
                pending: false,
                success: true,
                message: format!("node enrolled on {}", carrier_label(carrier)),
                deveui: prepared.dev_eui,
            },
            Err(e) => refuse(e),
        }
    }

    /// FB11 on a radio-mode Hub: every Node of the registry, on either carrier — a Node on a
    /// carrier this Hub has switched off is still held by it (node_registry.md §4).
    fn list_v2(&self) -> Result<Vec<NodeEntry>, String> {
        Ok(self
            .registry()?
            .map(|r| {
                r.nodes
                    .into_values()
                    .map(|n| NodeEntry {
                        deveui: n.dev_eui,
                        serial: Some(n.serial).filter(|v| !v.trim().is_empty()),
                        name: n.name.filter(|v| !v.trim().is_empty()),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// FB12 on a radio-mode Hub: resolve against the registry, then the Portal's `node-remove`
    /// on the carrier the registry holds the Node on.
    fn remove_v2(&self, target: &NodeRemoveTarget) -> NodeRemoveOutcome {
        let registry = match self.registry() {
            Ok(r) => r.unwrap_or_default(),
            Err(e) => return NodeRemoveOutcome::refused(e),
        };
        let node = match exactly_one(target, resolve_in_registry(target, &registry)) {
            Ok(node) => node,
            Err(refusal) => return refusal,
        };
        match self.radio.run(Op::Remove {
            radio: node.radio.as_str().to_string(),
            dev_eui: node.dev_eui.clone(),
        }) {
            Ok(()) => NodeRemoveOutcome::ok(format!("{} removed", node.dev_eui)),
            Err(e) => NodeRemoveOutcome::refused(e),
        }
    }
}

/// The registry records a target names, every one of them.
///
/// A DevEUI is the registry key. A serial is **trim + exact** as on the legacy path, compared in
/// the registry's own spelling: the 8-hex [`canonical_serial`] of what was typed, so the decimal
/// on the label and the hex FB11 lists name the same Node, or the serial as typed for a record
/// migrated with one that is not a STICKER serial (`serial_invalid`).
fn resolve_in_registry<'r>(
    target: &NodeRemoveTarget,
    registry: &'r Registry,
) -> Vec<&'r NodeRecord> {
    match target {
        NodeRemoveTarget::DevEui(v) => registry.get(v.trim()).into_iter().collect(),
        NodeRemoveTarget::Serial(v) => {
            let want = v.trim();
            let canonical = canonical_serial(want);
            registry
                .nodes
                .values()
                .filter(|n| {
                    if n.serial_invalid {
                        n.serial == want
                    } else {
                        canonical.as_deref() == Some(n.serial.as_str())
                    }
                })
                .collect()
        }
    }
}

/// The one match, or the refusal: none, or more than one.
fn exactly_one<T>(target: &NodeRemoveTarget, found: Vec<T>) -> Result<T, NodeRemoveOutcome> {
    let count = found.len();
    let mut found = found.into_iter();
    match (found.next(), count) {
        (None, _) => Err(NodeRemoveOutcome::refused(format!(
            "no node is enrolled with {} {}",
            target.label(),
            target.value()
        ))),
        (Some(one), 1) => Ok(one),
        // Guessing which of two devices sharing a serial the installer meant would turn a data
        // defect into a deleted device.
        (Some(_), many) => Err(NodeRemoveOutcome::refused(format!(
            "{} {} matches {many} enrolled nodes; nothing was removed — remove by deveui",
            target.label(),
            target.value(),
        ))),
    }
}

/// A carrier as an installer reads it.
fn carrier_label(t: Transport) -> &'static str {
    match t {
        Transport::Lorawan => "LoRaWAN",
        Transport::P2p => "P2P",
    }
}

/// The longest a Node name or serial number may be.
pub const MAX_STR_CHARS: usize = 64;

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn check_label(s: &str, field: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if s.chars().count() > MAX_STR_CHARS {
        return Err(format!("{field} too long (>{MAX_STR_CHARS} chars)"));
    }
    // Control characters would break YAML quoting and inject into log lines.
    if s.chars().any(|c| c.is_control()) {
        return Err(format!("{field} must not contain control characters"));
    }
    Ok(s.to_string())
}

/// The validated, normalised form of a request.
struct Prepared {
    dev_eui: String,
    join_eui: String,
    app_key: String,
    name: String,
    serial_number: String,
}

/// Validate and normalise, before anything is provisioned or written.
///
/// **This lives on the seam, not in the caller.** In v1 it lived inside the FB0D GATT handler, so
/// it protected exactly one path; when that handler moved into `control-ble` the check did not
/// travel with it and every non-BLE caller silently lost it. Putting it here means a malformed
/// AppKey is refused whoever asks — and the refusal names the field, because the installer reading
/// it on a phone screen has to know whether to re-scan the Node or the Hub. Both stacks run it.
fn prepare(req: &NodeAddRequest) -> Result<Prepared, String> {
    let dev_eui = req.deveui.trim().to_lowercase();
    let join_eui = req.joineui.trim().to_lowercase();
    let app_key = req.appkey.trim().to_lowercase();

    if !is_hex(&dev_eui, 16) {
        return Err("deveui must be 16 hex chars".to_string());
    }
    // An all-zero DevEUI is not a real device. An all-zero JoinEUI, by contrast, is the legitimate
    // default and is allowed.
    if dev_eui.bytes().all(|b| b == b'0') {
        return Err("deveui must not be all zeros".to_string());
    }
    if !is_hex(&join_eui, 16) {
        return Err("joineui must be 16 hex chars".to_string());
    }
    if !is_hex(&app_key, 32) {
        return Err("appkey must be 32 hex chars".to_string());
    }
    Ok(Prepared {
        dev_eui,
        join_eui,
        app_key,
        name: check_label(req.name.trim(), "name")?,
        serial_number: check_label(req.serial_number.trim(), "serial_number")?,
    })
}

impl NodeEnroll for LorawanNodeEnroll {
    fn add(&self, req: &NodeAddRequest) -> NodeAddResponse {
        if let Some(config) = self.v2_config() {
            return self.add_v2(req, &config);
        }

        let refuse = |message: String| NodeAddResponse {
            pending: false,
            success: false,
            message,
            deveui: req.deveui.trim().to_lowercase(),
        };

        // Refuse before calling: an absent applier is v1's own hard error, and saying so by name
        // beats a generic failure the installer cannot act on.
        if self.deps.config_applier.is_none() {
            return refuse("no config applier: this Hub cannot enrol Nodes".to_string());
        }

        let prepared = match prepare(req) {
            Ok(p) => p,
            Err(e) => return refuse(e),
        };

        let activation = ActivationMode::Otaa {
            app_key: prepared.app_key,
            join_eui: prepared.join_eui,
            profile_id: req.profile_id,
        };

        match add_lorawan_node(
            &self.deps,
            prepared.dev_eui.clone(),
            prepared.name,
            prepared.serial_number,
            activation,
        ) {
            Ok(()) => NodeAddResponse {
                pending: false,
                success: true,
                message: "node enrolled".to_string(),
                deveui: prepared.dev_eui,
            },
            Err(e) => refuse(e),
        }
    }

    fn list(&self) -> Result<Vec<NodeEntry>, String> {
        if self.v2_config().is_some() {
            return self.list_v2();
        }
        Ok(self
            .read_sensors()?
            .into_iter()
            .map(|s| NodeEntry {
                // The wire identity is lowercase everywhere else this crate touches it — `prepare`
                // normalises it on the way in — so the list does not invent a second spelling for
                // a row written before that normalisation existed.
                deveui: s.dev_eui.trim().to_lowercase(),
                serial: s.serial_number.filter(|v| !v.trim().is_empty()),
                name: s.name.filter(|v| !v.trim().is_empty()),
            })
            .collect())
    }

    fn remove(&self, target: &NodeRemoveTarget) -> NodeRemoveOutcome {
        // The same shape check the add path applies, so a mistyped DevEUI is refused by name rather
        // than reaching either stack as a "no such sensor".
        if let NodeRemoveTarget::DevEui(v) = target {
            let dev_eui = v.trim().to_lowercase();
            if !is_hex(&dev_eui, 16) {
                return NodeRemoveOutcome::refused("deveui must be 16 hex chars");
            }
            if dev_eui.bytes().all(|b| b == b'0') {
                return NodeRemoveOutcome::refused("deveui must not be all zeros");
            }
        }

        if self.v2_config().is_some() {
            return self.remove_v2(target);
        }

        let sensors = match self.read_sensors() {
            Ok(s) => s,
            Err(e) => return NodeRemoveOutcome::refused(e),
        };

        // Resolve first, refuse before writing. An unknown identifier must leave the config
        // untouched — FB10's contract for an unknown MAC, kept here.
        let dev_eui = match exactly_one(target, self.resolve(target, &sensors)) {
            Ok(one) => one,
            Err(refusal) => return refusal,
        };

        // From here it is the shared path, byte-for-byte what the signed MQTT command does.
        match remove_lorawan_node(&self.deps, dev_eui.clone()) {
            Ok(()) => NodeRemoveOutcome::ok(format!("{dev_eui} removed")),
            Err(e) => NodeRemoveOutcome::refused(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn request() -> NodeAddRequest {
        NodeAddRequest {
            deveui: "0011223344556677".into(),
            profile_id: Some(1),
            joineui: "0000000000000000".into(),
            appkey: "00112233445566778899AABBCCDDEEFF".into(),
            name: "probe 1".into(),
            serial_number: "SN-1".into(),
        }
    }

    /// A radio stack that holds no config at all: the legacy path's Hub, whatever the test
    /// machine has under `/run/proximos`.
    struct NoRadio;
    impl RadioStack for NoRadio {
        fn config(&self) -> control_radio::Result<RadioConfig> {
            RadioConfig::from_device_config_yaml("system: {}")
        }
        fn registry(&self) -> control_radio::Result<Option<Registry>> {
            panic!("the legacy path never reads the registry")
        }
        fn run(&self, _: Op) -> Result<(), String> {
            panic!("the legacy path never runs a radio command")
        }
    }

    fn legacy(deps: NodeAddDeps) -> LorawanNodeEnroll {
        LorawanNodeEnroll::new(deps, "/tmp/fiber.config.yaml").with_radio(Arc::new(NoRadio))
    }

    #[test]
    fn a_malformed_request_is_refused_before_anything_is_provisioned() {
        // The regression this guards: the checks used to live in v1's FB0D handler, which is now
        // in another crate. Without them here an installer's typo would reach ChirpStack.
        let e = legacy(NodeAddDeps {
            config_applier: Some(Arc::new(
                crate::libs::config_applier::ConfigApplier::new(std::path::Path::new("/tmp"))
                    .expect("applier"),
            )),
            ..NodeAddDeps::default()
        });
        let cases: [(&str, &str, fn(&mut NodeAddRequest)); 7] = [
            ("bad appkey", "appkey", |r| r.appkey = "deadbeef".into()),
            ("short deveui", "deveui", |r| r.deveui = "0011".into()),
            ("all-zero deveui", "deveui", |r| r.deveui = "0".repeat(16)),
            ("bad joineui", "joineui", |r| r.joineui = "nope".into()),
            ("empty name", "name", |r| r.name = String::new()),
            ("newline in name", "control characters", |r| {
                r.name = "probe\n1".into()
            }),
            ("long serial", "too long", |r| {
                r.serial_number = "S".repeat(MAX_STR_CHARS + 1)
            }),
        ];
        for (label, expect, mutate) in cases {
            let mut r = request();
            mutate(&mut r);
            let resp = e.add(&r);
            assert!(!resp.success, "{label} was accepted");
            assert!(
                resp.message.contains(expect),
                "{label}: the refusal must name the field, got {:?}",
                resp.message
            );
        }
    }

    #[test]
    fn node_enroll_forwards_to_the_lorawan_node_add_path() {
        // With no dependencies wired the add cannot proceed, and the refusal must name the reason:
        // "failed" alone would send an installer to re-scan a QR code that was never the problem.
        let e = legacy(NodeAddDeps::default());
        let r = e.add(&request());
        assert!(!r.success);
        assert!(!r.pending, "a refusal is final, not pending");
        assert!(
            r.message.contains("config applier"),
            "the message names what is missing: {}",
            r.message
        );
        assert_eq!(r.deveui, "0011223344556677", "and echoes the device");
    }

    /// The registry path's own refusals, each before anything runs: what a radio-mode Hub
    /// answers for a request the Portal would refuse too.
    struct Recording {
        mode: &'static str,
        registry: control_radio::Result<Option<Registry>>,
        ran: Mutex<usize>,
    }
    impl RadioStack for Recording {
        fn config(&self) -> control_radio::Result<RadioConfig> {
            let yaml = include_str!("../../../../control-radio/tests/fixtures/radio-section.yaml")
                .replacen("mode: lorawan", &format!("mode: {}", self.mode), 1);
            RadioConfig::from_device_config_yaml(&yaml)
        }
        fn registry(&self) -> control_radio::Result<Option<Registry>> {
            self.registry.clone()
        }
        fn run(&self, _: Op) -> Result<(), String> {
            *self.ran.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn a_radio_mode_hub_refuses_what_the_portal_refuses_and_runs_nothing() {
        // "SN-1" is a fine legacy label and not a STICKER serial.
        let radio = Arc::new(Recording {
            mode: "p2p",
            registry: Ok(None),
            ran: Mutex::new(0),
        });
        let e = LorawanNodeEnroll::new(NodeAddDeps::default(), "/nonexistent/fiber.config.yaml")
            .with_radio(radio.clone());
        let r = e.add(&request());
        assert!(!r.success && !r.pending, "{r:?}");
        assert!(r.message.contains("serial_number"), "{}", r.message);

        // The v1 shape checks still run first, and name the field.
        let mut bad = request();
        bad.appkey = "deadbeef".into();
        let r = e.add(&bad);
        assert!(r.message.contains("appkey"), "{}", r.message);
        assert_eq!(*radio.ran.lock().unwrap(), 0, "nothing was registered");
    }

    #[test]
    fn an_unreadable_registry_is_an_error_on_every_verb_never_an_empty_answer() {
        let radio = Arc::new(Recording {
            mode: "lorawan",
            registry: Err(control_radio::Error::new(
                control_radio::Kind::StorageError,
                None,
                control_radio::Component::NodeRegistry,
                "node_registry.json is not readable: expected value",
            )),
            ran: Mutex::new(0),
        });
        let e = LorawanNodeEnroll::new(NodeAddDeps::default(), "/nonexistent/fiber.config.yaml")
            .with_radio(radio.clone());
        let err = e.list().expect_err("FB11 must fail, not answer total: 0");
        assert!(err.contains("Node registry"), "{err}");

        let mut req = request();
        req.serial_number = "2162165722".into();
        let r = e.add(&req);
        assert!(!r.success && r.message.contains("Node registry"), "{r:?}");
        let out = e.remove(&NodeRemoveTarget::Serial("2162165722".into()));
        assert!(
            !out.success && out.message.contains("Node registry"),
            "{out:?}"
        );
        assert_eq!(*radio.ran.lock().unwrap(), 0);
    }
}
