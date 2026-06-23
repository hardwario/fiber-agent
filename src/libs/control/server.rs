//! Control-socket server, embedded in the `fiber_app` daemon (#79).
//!
//! Runs a blocking `UnixListener` accept loop (caller spawns it on a dedicated
//! thread, matching the daemon's thread-per-monitor style) and dispatches each
//! request to the live subsystem handles held in [`ControlContext`]. Sync
//! throughout — the handles (`LoRaWANHandle::send_command`, shared state) are
//! themselves synchronous.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Hard cap on a single request line (root-only socket, but don't allow a
/// malformed client to make us allocate without bound).
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

use serde_json::{json, Value};

use crate::libs::config::Config;
use crate::libs::config_applier::ConfigApplier;
use crate::libs::lorawan::sticker_command as sc;
use crate::libs::lorawan::sticker_response::{ConfigMismatch, ConfigValue, ResponseKind};
use crate::libs::lorawan::state::SharedLoRaWANState;
use crate::libs::lorawan::LoRaWANHandle;
use crate::libs::mqtt::{ConnectionState, SharedConnectionState};
use crate::libs::power::SharedPowerStatus;
use crate::libs::sensors::SharedSensorStateHandle;

use super::protocol::ConfigSetting;

use super::protocol::{Command, LorawanSimpleCommand, Request, Response, PROTOCOL_VERSION};

/// Live handles the control server dispatches to. Cheap to clone (handles are
/// `Arc`/channel-backed); the daemon builds one and hands clones to the server.
#[derive(Clone)]
pub struct ControlContext {
    pub app_version: String,
    pub config: Arc<Config>,
    pub lorawan: Option<LoRaWANHandle>,
    pub lorawan_state: Option<SharedLoRaWANState>,
    /// Per-command timeout for fPort-85 round-trips.
    pub command_timeout: Duration,
    /// Serializes device-mutating LoRaWAN operations so concurrent control
    /// requests don't interleave downlinks/reboots to the same STICKER.
    pub lorawan_lock: Arc<Mutex<()>>,
    pub power: Option<SharedPowerStatus>,
    pub sensors: Option<SharedSensorStateHandle>,
    pub mqtt_connection: Option<SharedConnectionState>,
    pub config_applier: Option<Arc<ConfigApplier>>,
}

impl ControlContext {
    /// Build a context with a fresh command lock. Optional subsystem handles
    /// (power/sensors/led) default to absent — attach them with the builders.
    pub fn new(
        app_version: String,
        config: Arc<Config>,
        lorawan: Option<LoRaWANHandle>,
        lorawan_state: Option<SharedLoRaWANState>,
        command_timeout: Duration,
    ) -> Self {
        ControlContext {
            app_version,
            config,
            lorawan,
            lorawan_state,
            command_timeout,
            lorawan_lock: Arc::new(Mutex::new(())),
            power: None,
            sensors: None,
            mqtt_connection: None,
            config_applier: None,
        }
    }

    pub fn with_power(mut self, p: SharedPowerStatus) -> Self {
        self.power = Some(p);
        self
    }
    pub fn with_sensors(mut self, s: SharedSensorStateHandle) -> Self {
        self.sensors = Some(s);
        self
    }
    pub fn with_mqtt_connection(mut self, c: SharedConnectionState) -> Self {
        self.mqtt_connection = Some(c);
        self
    }
    pub fn with_config_applier(mut self, a: Arc<ConfigApplier>) -> Self {
        self.config_applier = Some(a);
        self
    }
}

/// Bind the control socket and serve forever (blocking). Intended to run on its
/// own thread. Recreates the socket (removing a stale one) and locks it to
/// `0600` so only the daemon's user (root) can talk to it.
pub fn serve(ctx: ControlContext, path: &str) -> std::io::Result<()> {
    // The parent dir is the primary access control: 0700 means non-root cannot
    // even traverse to the socket, which closes the window between bind() and the
    // socket chmod below.
    if let Some(parent) = Path::new(path).parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| std::io::Error::new(e.kind(), format!("create control dir {parent:?}: {e}")))?;
        // tighten perms in case the dir pre-existed looser
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let _ = fs::remove_file(path); // clear stale socket from a previous run
    let listener = UnixListener::bind(path)?;
    // Refuse to serve if we cannot lock the socket down to owner-only.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| std::io::Error::new(e.kind(), format!("chmod control socket {path}: {e}")))?;
    eprintln!("[control] listening on {path}");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let ctx = ctx.clone();
                // One thread per connection; requests are one-shot and rare.
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, &ctx) {
                        eprintln!("[control] connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[control] accept error: {e}"),
        }
    }
    Ok(())
}

fn handle_conn(stream: UnixStream, ctx: &ControlContext) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    // Cap the request size so a malformed client can't make us allocate without bound.
    let mut reader = BufReader::new(&stream).take(MAX_REQUEST_BYTES);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // client closed without sending
    }

    let resp = match serde_json::from_str::<Request>(line.trim_end()) {
        Ok(req) if req.v > PROTOCOL_VERSION => Response::err_coded(
            "unsupported_version",
            format!("unsupported protocol version {} (server speaks {})", req.v, PROTOCOL_VERSION),
            json!(null),
        ),
        Ok(req) => dispatch(ctx, req.cmd),
        Err(e) => Response::err_coded("bad_request", format!("malformed request: {e}"), json!(null)),
    };

    let mut out = serde_json::to_string(&resp).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"encode response: {e}\"}}")
    });
    out.push('\n');
    let mut w = &stream;
    w.write_all(out.as_bytes())?;
    w.flush()?;
    Ok(())
}

/// Execute one command against the live handles. Pure-ish: all side effects go
/// through the handles, and every arm returns a [`Response`].
pub fn dispatch(ctx: &ControlContext, cmd: Command) -> Response {
    match cmd {
        Command::Status => status(ctx),
        Command::ConfigShow => match serde_json::to_value(&*ctx.config) {
            Ok(mut v) => {
                redact_secrets(&mut v);
                Response::ok(v)
            }
            Err(e) => Response::err_coded("internal", format!("serialize config: {e}"), json!(null)),
        },
        Command::ConfigGet { key } => config_get(ctx, &key),
        Command::SensorsRead => sensors_read(ctx),
        Command::PowerStatus => power_status(ctx),
        Command::MqttStatus => mqtt_status(ctx),
        Command::ConfigSet { setting, force } => config_set(ctx, setting, force),
        Command::LorawanSetParam { dev_eui, fields, save, force } => {
            lorawan_set_param(ctx, &dev_eui, fields, save, force)
        }
        Command::LorawanGetParam { dev_eui, keys, desired } => {
            lorawan_get_param(ctx, &dev_eui, keys, desired)
        }
        Command::LorawanSend { dev_eui, command, force } => {
            lorawan_send(ctx, &dev_eui, command, force)
        }
    }
}

fn status(ctx: &ControlContext) -> Response {
    let mut lorawan = json!({ "enabled": ctx.lorawan.is_some() });
    if let Some(state) = &ctx.lorawan_state {
        if let Ok(s) = state.read() {
            lorawan = json!({
                "enabled": ctx.lorawan.is_some(),
                "gateway_present": s.gateway_present,
                "concentratord_running": s.concentratord_running,
                "chirpstack_running": s.chirpstack_running,
                "device_count": s.sensors.len(),
                "devices": s.sensors.keys().cloned().collect::<Vec<_>>(),
            });
        }
    }
    Response::ok(json!({
        "app_version": ctx.app_version,
        "lorawan": lorawan,
        "power": power_json(ctx),
        "sensors": sensors_json(ctx),
    }))
}

/// Battery/DC snapshot as JSON, or `null` ONLY if the power subsystem is absent.
/// A poisoned lock is recovered (the data has no broken invariant), matching the
/// rest of the codebase — so `null` unambiguously means "not enabled".
fn power_json(ctx: &ControlContext) -> Value {
    let Some(p) = &ctx.power else { return Value::Null };
    let g = p.lock().unwrap_or_else(|e| e.into_inner());
    json!({
        "vbat_mv": g.vbat_mv,
        "battery_percent": g.battery_percent,
        "vin_mv": g.vin_mv,
        "on_dc_power": g.on_dc_power,
    })
}

/// Per-line sensor snapshot as a JSON array, or `null` ONLY if absent (poison
/// recovered, as above).
fn sensors_json(ctx: &ControlContext) -> Value {
    let Some(s) = &ctx.sensors else { return Value::Null };
    let g = s.read().unwrap_or_else(|e| e.into_inner());
    let lines: Vec<Value> = (0u8..8)
        .map(|i| {
            let name = g.get_name(i);
            let location = g.get_location(i);
            match &g.readings[i as usize] {
                Some(r) => json!({
                    "line": i, "name": name, "location": location,
                    "connected": r.is_connected, "temperature": r.temperature,
                    // Display impl = canonical SCREAMING_SNAKE (matches MQTT alarm events).
                    "alarm_state": r.alarm_state.to_string(),
                }),
                None => json!({ "line": i, "name": name, "location": location, "connected": false }),
            }
        })
        .collect();
    json!(lines)
}

fn sensors_read(ctx: &ControlContext) -> Response {
    match sensors_json(ctx) {
        Value::Null => Response::err_coded("not_enabled", "sensor subsystem not available", json!(null)),
        v => Response::ok(json!({ "sensors": v })),
    }
}

fn power_status(ctx: &ControlContext) -> Response {
    match power_json(ctx) {
        Value::Null => Response::err_coded("not_enabled", "power subsystem not available", json!(null)),
        v => Response::ok(v),
    }
}

fn connection_state_str(s: ConnectionState) -> &'static str {
    match s {
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Error => "error",
    }
}

fn mqtt_status(ctx: &ControlContext) -> Response {
    let Some(c) = &ctx.mqtt_connection else {
        return Response::err_coded("not_enabled", "MQTT is not enabled on this device", json!(null));
    };
    let st = c.lock().unwrap_or_else(|e| e.into_inner()).state();
    Response::ok(json!({
        "state": connection_state_str(st),
        "connected": matches!(st, ConnectionState::Connected),
    }))
}

/// Apply a persistent config change via `ConfigApplier` (atomic write + backup +
/// live reload; serialized + range-checked in the applier per #81). Destructive
/// → requires `force`; always audited.
fn config_set(ctx: &ControlContext, setting: ConfigSetting, force: bool) -> Response {
    if !force {
        return Response::err_coded(
            "forbidden",
            "config set is destructive (persists to disk + reloads); pass --force",
            json!(null),
        );
    }
    // Server-side range check up front — honest `validation` code, and works
    // independently of whether the applier validates.
    if let Some(reason) = setting.validate() {
        return Response::err_coded("validation", reason, json!(null));
    }
    let Some(applier) = &ctx.config_applier else {
        return Response::err_coded("not_enabled", "config applier not available", json!(null));
    };
    eprintln!("[control] AUDIT t={} config set {} force={force}", now_unix(), setting.audit_label());

    let result = match &setting {
        ConfigSetting::DeviceLabel { label } => applier.apply_device_label_change(label.clone()),
        ConfigSetting::SensorName { line, name } => applier.apply_name_change(*line, name.clone()),
        ConfigSetting::SensorLocation { line, location } => {
            applier.apply_location_change(*line, location.clone())
        }
        ConfigSetting::LedBrightness { value } => applier.apply_led_brightness_change(*value),
        ConfigSetting::ScreenBrightness { value } => applier.apply_screen_brightness_change(*value),
        ConfigSetting::BuzzerVolume { value } => applier.apply_buzzer_volume_change(*value),
        ConfigSetting::SystemInfoInterval { seconds } => {
            applier.apply_system_info_interval_change(*seconds)
        }
    };

    let data = json!({
        "setting": setting.audit_label(),
        "file_path": result.file_path,
        "backup_path": result.backup_path,
        "applied_at": result.applied_at,
    });
    if result.success {
        Response::ok(data)
    } else {
        Response::err_coded(
            "apply_failed",
            result.error_message.clone().unwrap_or_else(|| "apply failed".into()),
            data,
        )
    }
}

fn config_get(ctx: &ControlContext, key: &str) -> Response {
    let root = match serde_json::to_value(&*ctx.config) {
        Ok(v) => v,
        Err(e) => return Response::err_coded("internal", format!("serialize config: {e}"), json!(null)),
    };
    let mut cur = &root;
    let mut last = "";
    for part in key.split('.') {
        match cur.get(part) {
            Some(v) => {
                cur = v;
                last = part;
            }
            None => return Response::err_coded("not_found", format!("no such config key: {key}"), json!(null)),
        }
    }
    let mut out = cur.clone();
    // redact a secret subtree, and a secret leaf addressed directly
    redact_secrets(&mut out);
    if is_secret_key(last) && !out.is_object() && !out.is_array() && !out.is_null() {
        out = json!("***");
    }
    Response::ok(out)
}

/// True for config keys whose values must never be exposed over the control
/// plane (terminals/CI logs). Conservative substring match.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["password", "passwd", "secret", "token", "appkey", "nwkkey", "appskey", "nwkskey", "private_key"]
        .iter()
        .any(|needle| k.contains(needle))
}

/// Recursively replace scalar values held under secret-looking keys with "***".
fn redact_secrets(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if is_secret_key(k) && !child.is_object() && !child.is_array() && !child.is_null() {
                    *child = json!("***");
                } else {
                    redact_secrets(child);
                }
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(redact_secrets),
        _ => {}
    }
}

// --- LoRaWAN ---

fn lorawan_handle(ctx: &ControlContext) -> Result<&LoRaWANHandle, Response> {
    ctx.lorawan.as_ref().ok_or_else(|| {
        Response::err_coded("not_enabled", "LoRaWAN is not enabled on this device", json!(null))
    })
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cv_to_json(v: &ConfigValue) -> Value {
    match v {
        ConfigValue::Bool(b) => json!(b),
        ConfigValue::Uint(n) => json!(n),
        ConfigValue::Enum(s) | ConfigValue::Hex(s) => json!(s),
    }
}

fn lorawan_set_param(
    ctx: &ControlContext,
    dev_eui: &str,
    fields: BTreeMap<String, String>,
    save: bool,
    force: bool,
) -> Response {
    if save && !force {
        return Response::err_coded(
            "forbidden",
            "set-param with --save is destructive (persists + reboots the device); pass --force",
            json!(null),
        );
    }
    let handle = match lorawan_handle(ctx) {
        Ok(h) => h,
        Err(r) => return r,
    };

    // parse strings -> typed ConfigValue (type per field spec)
    let mut config: BTreeMap<String, ConfigValue> = BTreeMap::new();
    let mut parse_errors = Vec::new();
    for (k, raw) in &fields {
        match sc::parse_value(k, raw) {
            Ok(v) => {
                config.insert(k.clone(), v);
            }
            Err(e) => parse_errors.push(json!({ "key": e.key, "reason": e.reason })),
        }
    }
    if !parse_errors.is_empty() {
        return Response::err_coded("validation", "invalid field value(s)", json!({ "errors": parse_errors }));
    }

    let commands = match sc::build_set_param(&config, sc::DR0_COMMAND_BUDGET, save) {
        Ok(c) => c,
        Err(errs) => {
            let errors: Vec<Value> =
                errs.iter().map(|e| json!({ "key": e.key, "reason": e.reason })).collect();
            return Response::err_coded("validation", "validation failed", json!({ "errors": errors }));
        }
    };

    // Serialize device-mutating ops, and audit every attempt (staging or commit).
    let _guard = ctx.lorawan_lock.lock();
    eprintln!(
        "[control] AUDIT t={} set-param dev_eui={dev_eui} save={save} force={force} fields={:?}",
        now_unix(),
        fields.keys().collect::<Vec<_>>()
    );

    let sent_keys: Vec<&str> = config.keys().map(|s| s.as_str()).collect();
    let mut batches = Vec::new();
    let mut all_ok = true;
    let n = commands.len();
    for (i, command) in commands.into_iter().enumerate() {
        let is_last = i + 1 == n;
        match handle.send_command(dev_eui, command, ctx.command_timeout) {
            Ok(dr) => batches.push(decoded_to_json(&dr, &sent_keys)),
            Err(e) => {
                // The final (save) batch reboots the device; a missing reply there
                // is expected rather than a hard failure.
                if is_last && save {
                    batches.push(json!({ "note": "no reply after save (device reboots; expect unsolicited Info on rejoin)", "transport_error": e }));
                } else {
                    all_ok = false;
                    batches.push(json!({ "error": e }));
                }
            }
        }
    }

    let data = json!({ "batches": batches, "save": save });
    if all_ok {
        Response::ok(data)
    } else {
        Response::err_coded("transport", "one or more batches failed", data)
    }
}

fn lorawan_get_param(
    ctx: &ControlContext,
    dev_eui: &str,
    keys: Vec<String>,
    desired: Option<BTreeMap<String, String>>,
) -> Response {
    let handle = match lorawan_handle(ctx) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let _guard = ctx.lorawan_lock.lock(); // serialize with other device ops
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let command = sc::build_get_param(&key_refs);
    let dr = match handle.send_command(dev_eui, command, ctx.command_timeout) {
        Ok(dr) => dr,
        Err(e) => return Response::err_coded("transport", format!("no response from device: {e}"), json!(null)),
    };

    let ResponseKind::ConfigDump { config, page_index, page_count } = dr.kind else {
        return Response::err_coded("unexpected_response", format!("expected ConfigDump, got {:?}", dr.kind), json!(null));
    };

    let config_json: BTreeMap<String, Value> =
        config.iter().map(|(k, v)| (k.clone(), cv_to_json(v))).collect();

    let mut data = json!({
        "seq": dr.seq,
        "page_index": page_index,
        "page_count": page_count,
        "config": config_json,
    });

    if let Some(desired) = desired {
        // parse desired strings to typed values, then diff against the read-back
        let mut want: BTreeMap<String, ConfigValue> = BTreeMap::new();
        let mut perr = Vec::new();
        for (k, raw) in &desired {
            match sc::parse_value(k, raw) {
                Ok(v) => {
                    want.insert(k.clone(), v);
                }
                Err(e) => perr.push(json!({ "key": e.key, "reason": e.reason })),
            }
        }
        if !perr.is_empty() {
            return Response::err_coded("validation", "invalid desired value(s)", json!({ "errors": perr }));
        }
        let mismatches = crate::libs::lorawan::sticker_response::diff_config(&want, &config);
        data["diff"] = json!(mismatches.iter().map(mismatch_to_json).collect::<Vec<_>>());
        data["in_sync"] = json!(mismatches.is_empty());
    }

    Response::ok(data)
}

fn lorawan_send(
    ctx: &ControlContext,
    dev_eui: &str,
    command: LorawanSimpleCommand,
    force: bool,
) -> Response {
    if command.is_destructive() && !force {
        return Response::err_coded("forbidden", format!("{command:?} is destructive; pass --force"), json!(null));
    }
    let handle = match lorawan_handle(ctx) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let _guard = ctx.lorawan_lock.lock(); // serialize with other device ops
    let proto_cmd = match command {
        LorawanSimpleCommand::GetInfo => sc::build_get_info(),
        LorawanSimpleCommand::Reboot => sc::build_reboot(),
        LorawanSimpleCommand::ForceSend => sc::build_force_send(),
        LorawanSimpleCommand::ResetCounters => sc::build_reset_counters(),
        LorawanSimpleCommand::ClockSync => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
            sc::build_clock_sync(now)
        }
    };
    eprintln!(
        "[control] AUDIT t={} lorawan send dev_eui={dev_eui} command={command:?} destructive={} force={force}",
        now_unix(),
        command.is_destructive()
    );
    match handle.send_command(dev_eui, proto_cmd, ctx.command_timeout) {
        Ok(dr) => Response::ok(decoded_to_json(&dr, &[])),
        Err(e) => {
            if matches!(command, LorawanSimpleCommand::Reboot) {
                Response::ok(json!({ "note": "no reply (reboot); expect unsolicited Info on rejoin", "transport_error": e }))
            } else {
                Response::err_coded("transport", format!("no response from device: {e}"), json!(null))
            }
        }
    }
}

fn mismatch_to_json(m: &ConfigMismatch) -> Value {
    json!({
        "key": m.key,
        "desired": cv_to_json(&m.desired),
        "actual": m.actual.as_ref().map(cv_to_json),
    })
}

fn decoded_to_json(
    dr: &crate::libs::lorawan::sticker_response::DecodedResponse,
    sent_keys: &[&str],
) -> Value {
    use crate::libs::lorawan::sticker_response::ResponseKind as K;
    let kind = match &dr.kind {
        K::Ack => json!({ "kind": "ack" }),
        // claim_token is a provisioning secret — deliberately omitted from the
        // control-plane projection (would otherwise land in terminals/CI logs).
        K::Info { fw_version, build_type, serial_number, uptime_s, unix_time, debug, claim_token } => json!({
            "kind": "info", "fw_version": fw_version, "build_type": build_type,
            "serial_number": serial_number, "uptime_s": uptime_s, "unix_time": unix_time,
            "debug": debug, "has_claim_token": claim_token.is_some(),
        }),
        K::Error { code, fault_field, detail } => json!({
            "kind": "error", "code": code, "fault_field": fault_field,
            "fault_key": sc::describe_fault(*fault_field, sent_keys.iter().copied()),
            "detail": detail,
        }),
        K::ConfigDump { page_index, page_count, config } => json!({
            "kind": "config_dump", "page_index": page_index, "page_count": page_count,
            "config": config.iter().map(|(k, v)| (k.clone(), cv_to_json(v))).collect::<BTreeMap<_, _>>(),
        }),
        K::HistoryFrame { frame_index, frame_count, .. } => json!({
            "kind": "history_frame", "frame_index": frame_index, "frame_count": frame_count,
        }),
        K::W1Scan { roms } => json!({ "kind": "w1_scan", "roms": roms }),
        K::Empty => json!({ "kind": "empty" }),
    };
    json!({ "seq": dr.seq, "response": kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::control::client;
    use crate::libs::control::protocol::{Command, LorawanSimpleCommand, Request};
    use std::time::Duration;

    fn test_ctx() -> ControlContext {
        ControlContext::new(
            "9.9.9".to_string(),
            Arc::new(crate::libs::config::Config::default_config()),
            None, // no device in unit tests
            None,
            Duration::from_millis(200),
        )
    }

    /// Spawn the server on a temp socket and wait until it accepts connections.
    fn start_server() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock").to_string_lossy().to_string();
        let ctx = test_ctx();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = serve(ctx, &p);
        });
        // wait for bind
        for _ in 0..100 {
            if std::path::Path::new(&path).exists() {
                // also confirm it actually answers
                if client::send_to(&path, &Request::new(Command::Status)).is_ok() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (dir, path)
    }

    #[test]
    fn status_round_trips_over_socket() {
        let (_d, path) = start_server();
        let resp = client::send_to(&path, &Request::new(Command::Status)).unwrap();
        assert!(resp.ok, "status failed: {:?}", resp.error);
        assert_eq!(resp.data["app_version"], "9.9.9");
        assert_eq!(resp.data["lorawan"]["enabled"], false);
    }

    #[test]
    fn config_show_and_get() {
        let (_d, path) = start_server();
        let show = client::send_to(&path, &Request::new(Command::ConfigShow)).unwrap();
        assert!(show.ok);
        assert!(show.data.get("system").is_some());

        let get = client::send_to(
            &path,
            &Request::new(Command::ConfigGet { key: "system.app_version".into() }),
        )
        .unwrap();
        assert!(get.ok);
        // ConfigGet reads the config object (default_config = "0.1.0"), which is
        // distinct from status's ctx.app_version (the running binary's version).
        assert_eq!(get.data, serde_json::json!("0.1.0"));

        let missing = client::send_to(
            &path,
            &Request::new(Command::ConfigGet { key: "nope.nada".into() }),
        )
        .unwrap();
        assert!(!missing.ok);
        assert!(missing.error.unwrap().contains("no such config key"));
    }

    #[test]
    fn set_param_save_requires_force() {
        let (_d, path) = start_server();
        let mut fields = BTreeMap::new();
        fields.insert("application.interval_report".to_string(), "600".to_string());
        let resp = client::send_to(
            &path,
            &Request::new(Command::LorawanSetParam {
                dev_eui: "x".into(),
                fields,
                save: true,
                force: false,
            }),
        )
        .unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("--force"));
    }

    #[test]
    fn lorawan_without_device_reports_disabled() {
        let (_d, path) = start_server();
        let resp = client::send_to(
            &path,
            &Request::new(Command::LorawanSend {
                dev_eui: "x".into(),
                command: LorawanSimpleCommand::GetInfo,
                force: false,
            }),
        )
        .unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("LoRaWAN is not enabled"));
    }

    #[test]
    fn destructive_send_requires_force() {
        let (_d, path) = start_server();
        // reboot is destructive → gated even before the lorawan-enabled check
        let resp = client::send_to(
            &path,
            &Request::new(Command::LorawanSend {
                dev_eui: "x".into(),
                command: LorawanSimpleCommand::Reboot,
                force: false,
            }),
        )
        .unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("destructive"));
    }

    #[test]
    fn config_show_redacts_secrets() {
        // seed a config with a broker password, ensure it is not exposed
        let mut config = crate::libs::config::Config::default_config();
        if let Some(mqtt) = config.mqtt.as_mut() {
            mqtt.broker.password = Some("hunter2-supersecret".to_string());
        }
        let ctx = ControlContext::new(
            "9.9.9".into(),
            Arc::new(config),
            None,
            None,
            Duration::from_millis(200),
        );
        let resp = dispatch(&ctx, Command::ConfigShow);
        assert!(resp.ok);
        let dumped = serde_json::to_string(&resp.data).unwrap();
        assert!(!dumped.contains("hunter2-supersecret"), "secret leaked: {dumped}");
        assert!(dumped.contains("***"), "expected redaction marker");
    }

    #[test]
    fn redact_helper_masks_secret_keys_only() {
        let mut v = serde_json::json!({
            "broker": { "host": "h", "password": "p", "username": "u" },
            "lorawan": { "appkey": "deadbeef", "deveui": "0011" },
            "interval": 600,
        });
        redact_secrets(&mut v);
        assert_eq!(v["broker"]["password"], "***");
        assert_eq!(v["broker"]["host"], "h"); // non-secret untouched
        assert_eq!(v["broker"]["username"], "u");
        assert_eq!(v["lorawan"]["appkey"], "***");
        assert_eq!(v["lorawan"]["deveui"], "0011"); // identifier, not secret
        assert_eq!(v["interval"], 600);
    }

    #[test]
    fn observe_commands_not_enabled_when_absent() {
        let ctx = test_ctx(); // power/sensors all None
        for cmd in [Command::SensorsRead, Command::PowerStatus] {
            let r = dispatch(&ctx, cmd);
            assert!(!r.ok);
            assert_eq!(r.error_code.as_deref(), Some("not_enabled"));
        }
    }

    #[test]
    fn status_and_power_report_seeded_values() {
        use crate::libs::power::PowerStatus;
        let power = Arc::new(Mutex::new(PowerStatus::new(3700, 5000)));
        let ctx = test_ctx().with_power(power);
        // dedicated power command
        let p = dispatch(&ctx, Command::PowerStatus);
        assert!(p.ok);
        assert_eq!(p.data["vbat_mv"], 3700);
        assert_eq!(p.data["vin_mv"], 5000);
        // status embeds the same snapshot
        let s = dispatch(&ctx, Command::Status);
        assert_eq!(s.data["power"]["vbat_mv"], 3700);
    }

    #[test]
    fn sensors_read_returns_eight_lines() {
        use crate::libs::sensors::create_shared_sensor_state;
        let ctx = test_ctx().with_sensors(create_shared_sensor_state());
        let r = dispatch(&ctx, Command::SensorsRead);
        assert!(r.ok);
        assert_eq!(r.data["sensors"].as_array().unwrap().len(), 8);
        assert_eq!(r.data["sensors"][0]["line"], 0);
    }

    #[test]
    fn mqtt_status_present_and_absent() {
        use crate::libs::mqtt::connection::ConnectionStateHandle;
        // absent
        let absent = dispatch(&test_ctx(), Command::MqttStatus);
        assert_eq!(absent.error_code.as_deref(), Some("not_enabled"));
        // present + connected
        let cs = Arc::new(Mutex::new(ConnectionStateHandle::new()));
        cs.lock().unwrap().set_state(ConnectionState::Connected);
        let ctx = test_ctx().with_mqtt_connection(cs);
        let r = dispatch(&ctx, Command::MqttStatus);
        assert!(r.ok);
        assert_eq!(r.data["connected"], true);
        assert_eq!(r.data["state"], "connected");
    }

    #[test]
    fn config_set_gating_and_validation() {
        let ctx = test_ctx(); // no applier
        // without force -> forbidden (before applier/validation)
        let f = dispatch(&ctx, Command::ConfigSet {
            setting: ConfigSetting::DeviceLabel { label: "Lab A".into() },
            force: false,
        });
        assert_eq!(f.error_code.as_deref(), Some("forbidden"));
        // out-of-range brightness, force -> validation (server-side, no applier needed)
        let v = dispatch(&ctx, Command::ConfigSet {
            setting: ConfigSetting::LedBrightness { value: 250 },
            force: true,
        });
        assert_eq!(v.error_code.as_deref(), Some("validation"));
        // valid setting, force, but no applier wired -> not_enabled
        let n = dispatch(&ctx, Command::ConfigSet {
            setting: ConfigSetting::DeviceLabel { label: "Lab A".into() },
            force: true,
        });
        assert_eq!(n.error_code.as_deref(), Some("not_enabled"));
    }

    #[test]
    fn config_set_applies_via_real_applier() {
        use crate::libs::config_applier::ConfigApplier;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fiber.config.yaml"), "system:\n  device_label: \"OLD\"\n").unwrap();
        let ctx = test_ctx().with_config_applier(Arc::new(ConfigApplier::new(dir.path()).unwrap()));

        let r = dispatch(&ctx, Command::ConfigSet {
            setting: ConfigSetting::DeviceLabel { label: "Ward 9".into() },
            force: true,
        });
        assert!(r.ok, "config set failed: {:?}", r.error);
        let c = std::fs::read_to_string(dir.path().join("fiber.config.yaml")).unwrap();
        assert!(c.contains("Ward 9") && !c.contains("OLD"), "config not updated: {c}");
    }

    #[test]
    fn power_recovers_from_poisoned_lock() {
        use crate::libs::power::PowerStatus;
        let power = Arc::new(Mutex::new(PowerStatus::new(3700, 5000)));
        let p2 = power.clone();
        let _ = std::thread::spawn(move || {
            let _g = p2.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert!(power.is_poisoned());
        let ctx = test_ctx().with_power(power);
        let r = dispatch(&ctx, Command::PowerStatus);
        // a poisoned (but structurally valid) lock must NOT masquerade as not_enabled
        assert!(r.ok, "should recover from poison; got {:?}", r.error_code);
        assert_eq!(r.data["vbat_mv"], 3700);
    }

    #[test]
    fn errors_carry_stable_codes() {
        let ctx = test_ctx();
        let nf = dispatch(&ctx, Command::ConfigGet { key: "no.such".into() });
        assert_eq!(nf.error_code.as_deref(), Some("not_found"));
        let val = dispatch(&ctx, Command::LorawanSend {
            dev_eui: "x".into(),
            command: LorawanSimpleCommand::Reboot,
            force: false,
        });
        assert_eq!(val.error_code.as_deref(), Some("forbidden"));
    }

    #[test]
    fn malformed_json_request_rejected() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let (_d, path) = start_server();
        let stream = UnixStream::connect(&path).unwrap();
        (&stream).write_all(b"this is not json\n").unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let resp: Response = serde_json::from_str(line.trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("malformed request"));
    }

    #[test]
    fn unsupported_protocol_version_rejected() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let (_d, path) = start_server();
        let stream = UnixStream::connect(&path).unwrap();
        (&stream).write_all(b"{\"v\":999,\"cmd\":{\"type\":\"status\"}}\n").unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let resp: Response = serde_json::from_str(line.trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("unsupported protocol version"));
    }
}

