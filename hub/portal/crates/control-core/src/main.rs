// FIBER Medical Thermometer main application

use fiber_app::libs::buzzer::BuzzerPriorityManager;
use fiber_app::libs::config::LoRaWANConfig;
use fiber_app::libs::sensors::create_shared_sensor_state;
use fiber_app::libs::StorageThread;
use fiber_app::{
    new_shared_provisioning_session, AccelerometerMonitor, ButtonMonitor, BuzzerController, Config,
    DisplayMonitor, LedMonitor, LoRaWANMonitor, MqttMonitor, PairingMonitor, PowerMonitor,
    PowerStatus, SensorMonitor,
};
use rppal::gpio::Gpio;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// Query the default BLE adapter for its MAC. Falls back to a placeholder if
/// the adapter is unavailable (BLE disabled, bluetoothd down, etc.).
fn read_mac_from_bluer() -> Result<String, bluer::Error> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| bluer::Error {
            kind: bluer::ErrorKind::Failed,
            message: format!("tokio runtime: {}", e),
        })?;
    rt.block_on(async {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        Ok(adapter.address().await?.to_string())
    })
}

/// Read the hostname from /etc/hostname, lowercased.
///
/// Decision #21: a Hub's hostname is lowercase everywhere — this agent's MQTT topics, the
/// LCD, the BLE name, the pairing QR — the same spelling control-radio and the portal use.
/// It used to be uppercased here, which gave one Hub two spellings on the broker.
fn read_hostname_from_file() -> io::Result<String> {
    let hostname = fs::read_to_string("/etc/hostname")?;
    Ok(normalize_hostname(&hostname))
}

/// The one spelling of a hostname (decision #21): trimmed and lowercased.
fn normalize_hostname(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Rewrite the `app_version:` line under `system:` in the YAML config so the
/// on-disk file reflects the running binary's version (which may have been
/// updated by a RAUC bundle install). No-op if the value already matches or
/// the file/line cannot be found.
fn sync_app_version_to_yaml(path: &str, version: &str) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[main] sync_app_version_to_yaml: cannot read {}: {}",
                path, e
            );
            return;
        }
    };

    let mut in_system = false;
    let mut changed = false;
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim_start();
        // Track whether we're inside the top-level `system:` block. Top-level
        // keys start at column 0; nested keys are indented.
        if !line.starts_with(' ') && !line.starts_with('\t') && trimmed.ends_with(':') {
            in_system = trimmed == "system:";
        }

        if in_system && trimmed.starts_with("app_version:") {
            let indent_len = line.len() - trimmed.len();
            let indent = &line[..indent_len];
            let new_line = format!("{}app_version: {}", indent, version);
            if new_line != line {
                changed = true;
            }
            out.push_str(&new_line);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }

    // `lines()` drops a trailing newline; restore original file's trailing-newline state.
    if !content.ends_with('\n') {
        out.pop();
    }

    if !changed {
        return;
    }

    if let Err(e) = fs::write(path, out) {
        eprintln!(
            "[main] sync_app_version_to_yaml: cannot write {}: {}",
            path, e
        );
    } else {
        eprintln!("[main] Synced app_version={} to {}", version, path);
    }
}

fn main() -> io::Result<()> {
    #[cfg(feature = "dev-platform")]
    {
        let dev_mode_file = std::path::Path::new("/data/fiber/config/DEV_MODE_ENABLED");
        if !dev_mode_file.exists() {
            eprintln!("FATAL: dev-platform build detected but /data/fiber/config/DEV_MODE_ENABLED file not found.");
            eprintln!("This build bypasses cryptographic verification and MUST NOT be used in production.");
            eprintln!("To acknowledge dev mode, create the file: touch /data/fiber/config/DEV_MODE_ENABLED");
            std::process::exit(1);
        }
        eprintln!("WARNING: Running in DEV-PLATFORM mode. Cryptographic verification is DISABLED.");
        eprintln!("WARNING: This build is NOT suitable for medical use or EU MDR compliance.");
    }

    // `fiber_app southbridge-update ...`: the manual SB update (southbridge_update.md §3.4),
    // run with fiber.service stopped. Nothing else of the application starts.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("southbridge-update") {
        let config = Config::load_default().unwrap_or_else(|_| Config::default_config());
        std::process::exit(fiber_app::libs::southbridge_update::run_cli(
            &args[2..],
            &config.serial.port,
            config.serial.baud_rate,
        ));
    }

    eprintln!("[main] Starting FIBER application");

    // Where SQLite puts its temporary files — the VACUUM copy above all.
    //
    // `bundled-sqlcipher` compiles with `-DSQLITE_TEMP_STORE=2`, which makes a
    // temp database default to *memory*; a full VACUUM would then allocate
    // roughly the whole database in RAM, and the cap on this one is 2.5 GB on a
    // CM4. `retention::full_vacuum` asks its connection for `temp_store = FILE`,
    // and this is where that file lands: on `/data`, next to the database,
    // rather than in the default candidates (`/var/tmp`, `/tmp`) which are
    // tmpfs — i.e. RAM again.
    //
    // Set here, before any thread is spawned, so there is no concurrent reader
    // of the environment. Mirrors `portal/src/main.rs`.
    let sqlite_tmpdir = std::path::Path::new("/data/fiber/tmp");
    match std::fs::create_dir_all(sqlite_tmpdir) {
        Ok(()) => std::env::set_var("SQLITE_TMPDIR", sqlite_tmpdir),
        Err(e) => eprintln!(
            "[main] Warning: could not create {:?}: {} — a full VACUUM would fall back to RAM",
            sqlite_tmpdir, e
        ),
    }

    // Load configuration from /data/fiber/config/fiber.config.yaml
    eprintln!("[main] Loading configuration...");
    let mut config = match Config::load_default() {
        Ok(cfg) => {
            eprintln!("[main] Configuration loaded from /data/fiber/config/fiber.config.yaml");
            cfg
        }
        Err(e) => {
            eprintln!(
                "[main] Warning: Failed to load /data/fiber/config/fiber.config.yaml: {}",
                e
            );
            eprintln!("[main] Using default configuration");
            Config::default_config()
        }
    };

    // app_version is the Hub image version (/etc/os-release), falling back to the build-time
    // FIBER_VERSION and then CARGO_PKG_VERSION for local cargo builds.
    config.system.app_version = fiber_app::libs::ble_host::device_info::firmware_version();

    // Snapshot the full config for the control server (#79) before individual
    // fields get moved into subsystem monitors below (e.g. config.sensors).
    // Snapshotted here because `config` is partially moved into the monitors further down, and the
    // BLE runtime starts after MQTT (it publishes into MQTT's channel).
    let ble_section = config.ble_section_yaml();

    let control_config = std::sync::Arc::new(config.clone());

    // Persist app_version back to the YAML so external readers see the running
    // version after a RAUC bundle install. Surgical line edit preserves comments
    // and key order; only writes if the value actually changed (avoids needless
    // flash wear on every boot).
    sync_app_version_to_yaml(
        "/data/fiber/config/fiber.config.yaml",
        &config.system.app_version,
    );

    // Display configuration info
    let display_version = if cfg!(feature = "dev-platform") {
        format!("{}-dev", config.system.app_version)
    } else {
        config.system.app_version.clone()
    };
    eprintln!(
        "[main] Config: {} v{}",
        config.system.app_name, display_version
    );
    eprintln!(
        "[main] Power update interval: {}ms",
        config.power.update_interval_ms
    );
    eprintln!(
        "[main] Serial port: {} @ {} baud",
        config.serial.port, config.serial.baud_rate
    );

    // Initialize GPIO once and share with all drivers
    eprintln!("[main] Initializing GPIO...");
    let gpio = match Gpio::new() {
        Ok(g) => {
            eprintln!("[main] GPIO initialized successfully");
            Arc::new(g)
        }
        Err(e) => {
            eprintln!("[main] Failed to initialize GPIO: {}", e);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("GPIO initialization failed: {}", e),
            ));
        }
    };

    // Initialize STM32 bridge for hardware communication using config values. The
    // Southbridge is first brought in line with the firmware this image carries
    // (plan/system/southbridge_update.md §3.3); a failed update never stops the start.
    eprintln!("[main] Initializing STM32 bridge...");
    let stm = fiber_app::libs::southbridge_update::bring_up(
        &config.serial.port,
        config.serial.baud_rate,
    )?;
    eprintln!("[main] STM32 bridge initialized successfully");

    let stm_guard = Arc::new(Mutex::new(stm));

    // Decide whether this boot is a normal one or a return to standby, before
    // anything is powered up or starts measuring.
    //
    // This needs a VIN reading, and the PowerMonitor that normally produces one
    // is not started until near the end of boot — by which point the sensors,
    // BLE and MQTT are already running. So take one synchronous reading here: a
    // single UART round trip, in exchange for never bringing a device that is
    // meant to be off up as far as advertising and publishing first.
    fiber_app::libs::power::standby::init(
        fiber_app::libs::power::standby::marker_dir(&config.storage.db_path),
        config.power.standby.clone(),
    );
    let boot_vin_mv = {
        let mut stm_locked = stm_guard.lock().unwrap_or_else(|e| e.into_inner());
        match stm_locked.read_adc_data() {
            Ok((vin_opt, _)) => vin_opt.map(|adc| adc.voltage_mv as u16),
            Err(e) => {
                eprintln!("[main] Warning: could not read VIN at boot: {}", e);
                None
            }
        }
    };
    let marker_dir = fiber_app::libs::power::standby::configured_marker_dir();
    let boot_marker = fiber_app::libs::power::standby::StandbyMarker::read(&marker_dir);
    let boot_decision = fiber_app::libs::power::standby::boot_decision(
        boot_marker.is_some(),
        boot_vin_mv,
        config.power.ac_power.dc_thresholds().connect_mv,
    );
    let resuming_into_standby = match boot_decision {
        fiber_app::libs::power::BootDecision::ReenterStandby => {
            // A crash-restart (the unit is Restart=on-failure) or a cold boot
            // after the battery gave out with no PoE. Either way the operator
            // asked for this device to be off and nothing has changed that, so it
            // must not quietly resume patient monitoring.
            eprintln!(
                "[main] Standby marker present and VIN is {} — returning to standby",
                boot_vin_mv.map_or("unreadable".to_string(), |mv| format!("{} mV", mv))
            );
            fiber_app::libs::power::standby::request_standby();
            true
        }
        fiber_app::libs::power::BootDecision::Awake => {
            if boot_marker.is_some() {
                eprintln!("[main] Standby marker present but DC power is — booting normally");
                fiber_app::libs::power::standby::StandbyMarker::clear(&marker_dir);
            }
            false
        }
    };

    // Initialize sensor power lines
    {
        let mut stm_locked = stm_guard.lock().unwrap_or_else(|e| e.into_inner());
        if resuming_into_standby {
            // Not merely "don't turn them on": they boot HIGH in firmware, so
            // leaving them alone would energise all eight lines.
            eprintln!("[main] Standby: keeping sensor power off");
            stm_locked.set_sensor_power(false)?;
        } else {
            eprintln!("[main] Activating sensor power...");
            stm_locked.init_sensor_power()?;
        }
        stm_locked.init_leds_off()?;
    }
    eprintln!("[main] Sensor power settled, LEDs initialized");

    // Nothing has drawn to the panel yet, so claiming the blank now means the
    // display thread comes up dark instead of flashing the splash and an
    // overview screen at a device that is supposed to be off.
    if resuming_into_standby {
        fiber_app::libs::display::blank::request_blank();
    }

    // Create and spawn dedicated LED monitoring thread
    eprintln!("[main] Starting LED monitor...");
    let _led_monitor = LedMonitor::new(stm_guard.clone())?;
    let led_state = _led_monitor.shared_state.clone();
    eprintln!("[main] LED monitor started with 50ms update interval");

    // Create shared sensor state for temperature readings
    eprintln!("[main] Initializing shared sensor state...");
    let sensor_state = create_shared_sensor_state();
    eprintln!("[main] Sensor state initialized");

    // Read MAC address directly from the BLE adapter via bluer
    let mac_address = match read_mac_from_bluer() {
        Ok(m) => {
            eprintln!("[main] MAC address from bluer: {}", m);
            m
        }
        Err(e) => {
            eprintln!("[main] Warning: Failed to read MAC from bluer: {}", e);
            eprintln!("[main] Using placeholder MAC address");
            "00:00:00:00:00:00".to_string()
        }
    };

    // Read hostname from file
    let hostname = match read_hostname_from_file() {
        Ok(h) => {
            eprintln!("[main] Hostname read from /etc/hostname: {}", h);
            h
        }
        Err(e) => {
            eprintln!("[main] Failed to read hostname from /etc/hostname: {}", e);
            return Err(e);
        }
    };

    // Ephemeral BLE provisioning session — empty at boot, populated when
    // the user holds ENTER to enter provisioning mode. Shared between the
    // button monitor (writer), the display thread (reader, renders QR), and
    // the BLE GATT server (reader, gates pairing).
    let provisioning_session = new_shared_provisioning_session();
    eprintln!("[main] Provisioning session slot created (empty)");

    // Create shared power status for power monitoring and display
    eprintln!("[main] Initializing shared power status...");
    let power_status = Arc::new(Mutex::new(PowerStatus::default()));
    eprintln!("[main] Power status initialized");

    // Shared screen brightness for the display backlight, seeded from config like the
    // timeout below: a value set from the portal is persisted by the applier and must
    // survive a restart. It used to start at 100% whatever the config said.
    eprintln!("[main] Initializing screen brightness control...");
    let screen_brightness = Arc::new(AtomicU8::new(config.system.screen_brightness));
    eprintln!(
        "[main] Screen brightness initialized at {}%",
        config.system.screen_brightness
    );

    // Shared screen idle-timeout (seconds; 0 = always on), seeded from config.
    // Held live so it can be changed at runtime (e.g. via MQTT) without a restart.
    let screen_timeout = Arc::new(AtomicU32::new(config.system.screen_timeout_secs));
    eprintln!(
        "[main] Screen timeout initialized at {}s",
        config.system.screen_timeout_secs
    );

    // Configured overview lines, seeded from the config we already parsed.
    // Empty means the built-in layout. Shared with the MQTT monitor so a pushed
    // change lands on the next frame instead of waiting for the display thread's
    // periodic config reconcile.
    let display_lines = Arc::new(std::sync::RwLock::new(config.display.custom_lines.clone()));
    if config.display.custom_lines.is_empty() {
        eprintln!("[main] Display: built-in overview layout (no custom lines configured)");
    } else {
        eprintln!(
            "[main] Display: {} custom overview lines configured",
            config.display.custom_lines.len()
        );
    }

    // Create and spawn display monitor thread
    eprintln!("[main] Starting display monitor...");
    // Get device label from config, defaulting to hostname
    let device_label = config
        .system
        .device_label
        .clone()
        .unwrap_or_else(|| hostname.clone());
    let _display_monitor = DisplayMonitor::new(
        led_state.clone(),
        gpio.clone(),
        sensor_state.clone(),
        power_status.clone(),
        hostname.clone(),
        device_label,
        config.system.app_version.clone(),
        config.system.timezone_offset_hours,
        screen_brightness.clone(),
        screen_timeout.clone(),
        display_lines.clone(),
    )?;
    eprintln!("[main] Display monitor started with 250ms update interval");

    // Attach the provisioning-session handle to the display state so the
    // QR-config screen can pull the current session's QR each frame.
    {
        if let Ok(mut state) = _display_monitor.display_state.lock() {
            state.set_provisioning_session(provisioning_session.clone());
            eprintln!("[main] Provisioning session attached to display state");
        }
    }

    // Create and spawn button monitor thread for screen navigation (initially without pairing)
    eprintln!("[main] Starting button monitor...");
    let _button_monitor = ButtonMonitor::new(
        _display_monitor.display_state.clone(),
        None,
        None,
        sensor_state.clone(),
        provisioning_session.clone(),
        mac_address.clone(),
        hostname.clone(),
        None,
        None,
    )?;
    eprintln!("[main] Button monitor started");

    // Create shared buzzer volume (0 = muted, 1-100 = active)
    // Initialize from persisted config if available
    let buzzer_volume = Arc::new(AtomicU8::new(config.system.buzzer_volume));
    eprintln!(
        "[main] Buzzer volume initialized at {}%",
        config.system.buzzer_volume
    );

    // Create buzzer controller for power monitoring alerts (with shared volume)
    eprintln!("[main] Initializing buzzer for power management...");
    let power_buzzer = Arc::new(Mutex::new(BuzzerController::new_with_volume(
        gpio.clone(),
        buzzer_volume.clone(),
    )?));

    // Create buzzer priority manager for coordinating battery and sensor critical alarms
    eprintln!("[main] Initializing buzzer priority manager...");
    let buzzer_priority_manager = Arc::new(BuzzerPriorityManager::new(power_buzzer.clone()));
    _display_monitor.set_buzzer_priority(buzzer_priority_manager.clone());

    // The factory test (fiberctl factory-test) drives this hardware directly.
    fiber_app::libs::factory_test::register(fiber_app::libs::factory_test::Hardware {
        stm: stm_guard.clone(),
        buzzer: power_buzzer.clone(),
        power: power_status.clone(),
        accel_i2c: config.accelerometer.i2c_path.clone(),
    });

    // Initialize storage thread for medical data persistence.
    // Use the byte-precision cap (config.storage.max_size_mb takes
    // precedence over the integer-GB field) so we can sit at 2.5 GB
    // on a 4 GB partition shared with fiber-viewer.
    eprintln!("[main] Starting storage thread...");
    let storage_max_bytes = config.storage.effective_max_bytes();
    let (storage_handle, _storage_thread) = match StorageThread::spawn_with_max_bytes(
        &config.storage.db_path,
        storage_max_bytes,
        Some(&config.storage.hmac_secret_path),
        config.storage.maintenance_window.clone(),
    ) {
        Ok((handle, thread)) => {
            eprintln!(
                "[main] Storage thread started - database: {}, max size: {} MB",
                config.storage.db_path,
                storage_max_bytes / (1024 * 1024),
            );
            (handle, thread)
        }
        Err(e) => {
            eprintln!("[main] Warning: Failed to initialize storage thread: {}", e);
            eprintln!("[main] Continuing without persistent storage");
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Storage initialization failed: {}", e),
            ));
        }
    };

    // Consume a factory-reset result file left by the `fiber-factory-reset`
    // boot hook (phase 2 of the factory-reset feature), if it ran on the boot
    // that just wiped /data. This is the earliest point in startup with a
    // storage handle, which is why it sits here rather than next to the
    // standby/boot-decision block above: the audit row below needs to be the
    // first row of the fresh, post-wipe audit chain, and there is nowhere
    // earlier to durably record it. The cost is that sensor power and the
    // display have already come up by this point (a few hundred ms of
    // flicker before standby drops them again) — accepted in exchange for
    // never losing the audit row, since re-ordering around it would mean
    // standing up a second, throwaway storage handle earlier just for this.
    //
    // The decision of *what* to do lives in `decide_post_reset_boot_action`,
    // a pure function unit-tested in `factory_reset.rs`; only the actual
    // audit write and standby entry happen here.
    {
        use fiber_app::libs::factory_reset::{
            decide_post_reset_boot_action, PostResetBootAction, ResetOutcome, ResetPlan,
        };

        let result_path = ResetPlan::production().result_path();
        let outcome = ResetOutcome::read(&result_path);
        match decide_post_reset_boot_action(outcome.as_ref()) {
            PostResetBootAction::None => {
                // Three cases collapse to `None` here: no result file (the
                // overwhelmingly common case — nothing to do), a file that
                // is genuinely corrupt (unparseable JSON or an unrecognized
                // schema_version — can never become readable, safe to
                // clear), or a file that merely failed to *read* this once
                // (a transient I/O error, a permission hiccup — NOT safe to
                // clear: doing so would permanently destroy the one on-disk
                // record of this reset's outcome, silently skipping the
                // FACTORY_RESET_COMPLETED audit row and any requested
                // standby entry, with no retry possible afterward).
                // `ResetOutcome::is_corrupt` re-reads the file to tell these
                // apart; only the first, unambiguous case is cleared here.
                if ResetOutcome::is_corrupt(&result_path) {
                    eprintln!(
                        "[main] WARN: factory-reset result file at {} is corrupt (unparseable or unrecognized schema) — removing it",
                        result_path.display()
                    );
                    ResetOutcome::clear(&result_path);
                } else if result_path.exists() {
                    eprintln!(
                        "[main] WARN: factory-reset result file at {} exists but could not be read (transient I/O error?) — leaving it for a later boot to retry",
                        result_path.display()
                    );
                }
            }
            PostResetBootAction::Consume { enter_standby } => {
                // `decide_post_reset_boot_action` only ever returns `Consume`
                // when it was given `Some(outcome)`, so this always matches
                // today — but match rather than `.expect()` on that
                // invariant, so a future change to that function's `None`
                // handling cannot turn into a boot-time panic here.
                if let Some(outcome) = outcome {
                    eprintln!(
                        "[main] Factory reset result found: {} (post_action={:?})",
                        outcome.summary(),
                        outcome.post_action
                    );

                    // Genesis row of the fresh, post-wipe audit chain. The
                    // wipe destroyed the pre-wipe audit log that
                    // `request_id`/`reason`/`requested_by` originally lived
                    // in, so they are carried forward here — along with
                    // `status` and the wipe counts — so an auditor can
                    // stitch the two chains together across the reset.
                    let details = serde_json::json!({
                        "request_id": outcome.request_id,
                        "reason": outcome.reason,
                        "requested_by": outcome.requested_by,
                        "status": outcome.status,
                        "post_action": outcome.post_action,
                        "removed": outcome.removed,
                        "preserved": outcome.preserved,
                        "recreated": outcome.recreated,
                        "errors": outcome.errors,
                    })
                    .to_string();
                    if let Err(e) = storage_handle.log_audit_event(
                        "FACTORY_RESET_COMPLETED".to_string(),
                        Some("audit_log".to_string()),
                        Some(details),
                    ) {
                        eprintln!(
                            "[main] WARN: failed to queue FACTORY_RESET_COMPLETED audit row: {e}"
                        );
                    }
                    if let Err(e) = storage_handle
                        .flush_sync(fiber_app::libs::system_control::TEARDOWN_AUDIT_FLUSH_TIMEOUT)
                    {
                        eprintln!(
                            "[main] WARN: FACTORY_RESET_COMPLETED audit row may not be durable: {e}"
                        );
                    }

                    if enter_standby {
                        eprintln!(
                            "[main] Factory reset requested power-off as its post-action — entering standby"
                        );
                        // This string is not just a log line: `execute_standby`
                        // below stores it verbatim in the `StandbyMarker`,
                        // which `PowerMonitor` republishes as `reason` on the
                        // retained `power/standby` MQTT topic on the exact
                        // standby edge this boot produces (see
                        // `power::monitor`'s "no command handler sees this
                        // transition" doc), and it is what `fiberctl power`
                        // reports as `standby_reason`. There is no
                        // audit-export MQTT stream in this codebase and the
                        // result file is deleted right after this block, so
                        // this is the ONLY signal an operator or the Viewer
                        // ever receives about how the reset actually went —
                        // it must never claim success ("completed") for a
                        // `CompletedWithErrors` or `Failed` wipe. Spell out
                        // the real status and error count instead of a fixed
                        // word.
                        if let Err(e) = fiber_app::MqttMonitor::execute_standby(
                            format!(
                                "factory reset {} finished status={:?} ({} errors): {}",
                                outcome.request_id,
                                outcome.status,
                                outcome.errors.len(),
                                outcome.reason
                            ),
                            outcome.requested_by.clone(),
                            &Some(storage_handle.clone()),
                            &Some(stm_guard.clone()),
                        ) {
                            eprintln!(
                                "[main] WARN: could not enter standby after factory reset: {e} — device stays up"
                            );
                        }
                    }
                } else {
                    eprintln!(
                        "[main] WARN: factory-reset boot action was Consume with no outcome — ignoring"
                    );
                }

                // Consumed exactly once: whatever happened above, this
                // result must not be acted on again on a later, unrelated
                // boot.
                ResetOutcome::clear(&result_path);
            }
        }
    }

    // Save-and-feed: spawn the multi-destination MQTT exporter when enabled.
    // The exporter drains rows past per-(broker, stream) cursors and ships
    // them to configured destinations at QoS 1. The thread owns its own
    // tokio runtime; we hold the JoinHandle in `_export_thread` so it stays
    // alive for the rest of `main`.
    //
    // The ExportHandle is captured back out of the thread via a oneshot so
    // we can hand it to the MQTT monitor for ResetExportCursor — without
    // that wiring the orchestrator's in-memory cursor cache survives any
    // DB-side reset until the daemon restarts.
    let (export_handle_opt, _export_thread): (
        Option<fiber_app::libs::mqtt_export::ExportHandle>,
        Option<std::thread::JoinHandle<()>>,
    ) = if config
        .mqtt
        .as_ref()
        .map(|m| m.export.enabled)
        .unwrap_or(false)
    {
        // `export` lives inside the `mqtt:` block, so being here means the
        // block exists — the same `Some` the condition above just tested.
        let mqtt_cfg = config
            .mqtt
            .as_ref()
            .expect("export.enabled is only reachable through an `mqtt:` block");
        let export_cfg = mqtt_cfg.export.clone();
        let db_path = std::path::PathBuf::from(&config.storage.db_path);
        let storage_for_export = storage_handle.clone();
        let host_for_export = hostname.clone();
        // The export publishes under the same root as everything else
        // (plan/portal/mqtt/design.md §9.1); it used to hardcode `fiber/`.
        let prefix_for_export = mqtt_cfg.publish.topic_prefix.clone();
        let (handle_tx, handle_rx) =
            std::sync::mpsc::channel::<fiber_app::libs::mqtt_export::ExportHandle>();
        match std::thread::Builder::new()
            .name("fiber-mqtt-export".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("[main] mqtt_export: failed to build tokio runtime: {}", e);
                        return;
                    }
                };
                rt.block_on(async move {
                    let (handle, fut) = fiber_app::libs::mqtt_export::MqttExportThread::spawn(
                        export_cfg,
                        db_path,
                        storage_for_export,
                        host_for_export,
                        prefix_for_export,
                    );
                    // Hand the orchestrator handle back to main so MQTT
                    // command dispatch can invalidate the in-memory cursor
                    // cache on ResetExportCursor. Failure to send means main
                    // already gave up on us — keep running anyway.
                    let _ = handle_tx.send(handle);
                    // Drive the orchestrator inline on this thread's runtime.
                    // It loops until it receives Shutdown (which we never send
                    // from here) — effectively for the lifetime of the process.
                    fut.await;
                });
            }) {
            Ok(h) => {
                eprintln!("[main] mqtt_export thread spawned");
                // Wait for the orchestrator to publish its handle. Short
                // timeout so a stuck export thread doesn't gate boot.
                let handle = handle_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .ok();
                if handle.is_none() {
                    eprintln!(
                        "[main] Warning: mqtt_export handle not received within 2s — \
                         ResetExportCursor cache invalidation will be unavailable"
                    );
                }
                (handle, Some(h))
            }
            Err(e) => {
                eprintln!("[main] Warning: failed to spawn mqtt_export thread: {}", e);
                (None, None)
            }
        }
    } else {
        eprintln!("[main] mqtt.export.enabled=false — exporter not started");
        (None, None)
    };

    // Build the shared LoRa configs handle from the initial config snapshot.
    // This becomes the live source of truth: LoRaWAN monitor reads it for
    // alarm evaluation, MQTT writes it on config changes, display reads it
    // for the thresholds / location pages.
    let lorawan_configs = fiber_app::libs::lorawan::create_shared_lorawan_sensor_configs(
        config
            .lorawan
            .as_ref()
            .map(|l| l.sensors.clone())
            .unwrap_or_default(),
    );
    eprintln!(
        "[main] LoRa configs handle initialized with {} sensors",
        lorawan_configs.read().map(|v| v.len()).unwrap_or(0)
    );

    // Attach LoRa configs to display state
    {
        if let Ok(mut state) = _display_monitor.display_state.lock() {
            state.lorawan_configs = Some(lorawan_configs.clone());
            eprintln!("[main] LoRa configs attached to display state");
        }
    }

    // Create and spawn MQTT monitor if enabled
    eprintln!("[main] Checking MQTT configuration...");
    eprintln!("[main]   config.mqtt present: {}", config.mqtt.is_some());
    if let Some(ref mqtt_config) = config.mqtt {
        eprintln!("[main]   config.mqtt.enabled: {}", mqtt_config.enabled);
    }

    // Ensure the device CA key exists before anything that consumes it starts.
    //
    // Two components open /data/fiber/config/device_ca.key: the MQTT command
    // verifier (MqttMonitor::init_authorization_manager, with load_existing) and
    // the PairingMonitor (with load_or_generate). The verifier registers that key
    // as the trusted "device_ca" that every signed command is checked against; the
    // PairingMonitor is what signs the operator's certificate with it during
    // pairing. But the verifier runs from the MQTT connection thread while the
    // PairingMonitor is created only after the MQTT monitor below, so on a fresh
    // device the verifier can read the path before the key has been written. When
    // it does, it registers no device CA and every signed command is rejected
    // ("Certificate not signed by any trusted CA (issuer: device_ca)") for the
    // rest of that boot - the device only recovers on the next restart, once the
    // key finally exists when the verifier initializes. Creating it once here,
    // before either consumer starts, removes that first-boot race (and the
    // concurrent-create race between the two, which could otherwise leave the
    // verifier trusting one key while pairing signs with another).
    if config.mqtt.as_ref().map(|m| m.enabled).unwrap_or(false) {
        let ca_dir = std::path::Path::new("/data/fiber/config");
        match fiber_app::libs::pairing::ca_key::DeviceCaKey::load_or_generate(ca_dir, &hostname) {
            Ok(ca) => eprintln!("[main] Device CA ready ({})", ca.ca_id()),
            Err(e) => eprintln!("[main] Warning: could not initialize device CA: {}", e),
        }
    }

    let (mqtt_handle, mqtt_monitor) = if config.mqtt.as_ref().map(|m| m.enabled).unwrap_or(false) {
        eprintln!("[main] Starting MQTT monitor...");
        match MqttMonitor::new_with_stm(
            config.mqtt.clone().unwrap(),
            hostname.clone(),
            config.system.app_version.clone(),
            power_status.clone(),
            Some(stm_guard.clone()),
            Some(screen_brightness.clone()),
            Some(screen_timeout.clone()),
            Some(buzzer_volume.clone()),
            Some(display_lines.clone()),
            Some(buzzer_priority_manager.clone()),
            None,
            Some(lorawan_configs.clone()),
            Some(storage_handle.clone()),
        ) {
            Ok(monitor) => {
                eprintln!("[main] MQTT monitor started with STM bridge, screen brightness, and buzzer volume control");
                if let Some(ref eh) = export_handle_opt {
                    monitor.set_export_handle(eh.clone());
                }
                let handle = monitor.handle();
                (Some(handle), Some(monitor))
            }
            Err(e) => {
                eprintln!("[main] Warning: Failed to start MQTT: {}", e);
                (None, None)
            }
        }
    } else {
        eprintln!("[main] MQTT disabled in configuration");
        (None, None)
    };

    // Create and spawn pairing monitor if MQTT is enabled
    // Store monitor in _pairing_monitor to keep it alive until main loop exits
    let (_pairing_monitor, pairing_handle) = if mqtt_handle.is_some() {
        eprintln!("[main] Starting pairing monitor...");
        let config_dir = std::path::Path::new("/data/fiber/config");
        match PairingMonitor::new(
            hostname.clone(),
            config_dir,
            _display_monitor.display_state.clone(),
        ) {
            Ok(monitor) => {
                eprintln!("[main] Pairing monitor started");
                let handle = monitor.handle();
                // Set pairing handle in MQTT monitor for routing
                if let Some(ref mqtt_mon) = mqtt_monitor {
                    mqtt_mon.set_pairing_handle(handle.clone());
                }
                (Some(monitor), Some(handle))
            }
            Err(e) => {
                eprintln!("[main] Warning: Failed to start pairing monitor: {}", e);
                (None, None)
            }
        }
    } else {
        eprintln!("[main] Pairing monitor disabled (MQTT not enabled)");
        (None, None)
    };

    // Store MQTT monitor to keep it alive (after pairing handle is set)
    let _mqtt_monitor = mqtt_monitor;

    // Build a ConfigApplier for the BLE side so authenticated FB0A writes
    // can update fiber.config.yaml::system.device_label. Shares
    // /data/fiber/config with MQTT's own applier; on-disk writes are
    // atomic so racing the two is safe.
    let ble_config_applier: Option<Arc<fiber_app::ConfigApplier>> =
        match fiber_app::ConfigApplier::new_with_storage(
            std::path::Path::new("/data/fiber/config"),
            Some(storage_handle.clone()),
        ) {
            Ok(applier) => {
                eprintln!("[main] BLE-side ConfigApplier initialized");
                // A cluster join that stopped between its role marker and its setup write:
                // the Hub already runs as a Client, so record the setup it is missing.
                if let Err(e) = fiber_app::libs::cluster_join::complete_interrupted_join(&applier) {
                    eprintln!("[main] Warning: cannot complete an interrupted cluster join: {e}");
                }
                Some(Arc::new(applier))
            }
            Err(e) => {
                eprintln!(
                    "[main] Warning: BLE-side ConfigApplier failed to init: {} (FB0A writes will be rejected)",
                    e
                );
                None
            }
        };

    // Slot for the LoRaWAN shared state handed to the BLE FB0D node-add
    // path. The LoRaWAN monitor is created after the BLE monitor, so (like
    // MQTT's set_lorawan_state) the main thread fills this slot once it exists.
    let ble_lorawan_state_slot: std::sync::Arc<
        std::sync::Mutex<Option<fiber_app::libs::lorawan::SharedLoRaWANState>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));

    // Restart the button monitor with the pairing handle and the storage handle.
    // Without a pairing handle the menu's "Pairing code" says it is unavailable.
    let pairing_support = if pairing_handle.is_some() {
        "with pairing support"
    } else {
        "WITHOUT pairing support"
    };
    eprintln!("[main] Restarting button monitor {pairing_support}...");
    drop(_button_monitor);
    let _button_monitor = ButtonMonitor::new(
        _display_monitor.display_state.clone(),
        pairing_handle.clone(),
        Some(buzzer_priority_manager.clone()),
        sensor_state.clone(),
        provisioning_session.clone(),
        mac_address.clone(),
        hostname.clone(),
        Some(storage_handle.clone()),
        Some(stm_guard.clone()),
    )?;
    eprintln!("[main] Button monitor restarted {pairing_support}");

    // Derive MQTT sender for monitors that publish events
    let mqtt_sender = mqtt_handle.as_ref().map(|h| h.sender());

    // Create and spawn accelerometer monitoring thread if enabled
    let _accel_monitor = if config.accelerometer.enabled {
        eprintln!("[main] Starting accelerometer monitor...");
        match AccelerometerMonitor::new(config.accelerometer, mqtt_sender.clone()) {
            Ok(monitor) => {
                eprintln!("[main] Accelerometer monitor started");
                Some(monitor)
            }
            Err(e) => {
                eprintln!("[main] Warning: Failed to initialize accelerometer: {}", e);
                None
            }
        }
    } else {
        eprintln!("[main] Accelerometer monitoring disabled in configuration");
        None
    };

    // Create and spawn power monitoring thread with configured interval
    eprintln!("[main] Starting power monitor...");
    let _power_monitor = PowerMonitor::new(
        stm_guard.clone(),
        config.power.update_interval_ms,
        led_state.clone(),
        power_buzzer.clone(),
        buzzer_priority_manager.clone(),
        power_status.clone(),
        mqtt_sender,
        config.power.standby.clone(),
        config.power.ac_power.dc_thresholds(),
        Some(storage_handle.clone()),
    )?;
    eprintln!(
        "[main] Power monitor started (interval: {}ms)",
        config.power.update_interval_ms
    );

    // Create and spawn sensor monitoring thread (pass MQTT handle)
    eprintln!("[main] Starting sensor monitor...");
    let _sensor_monitor = match SensorMonitor::new(
        config.sensors,
        stm_guard.clone(),
        led_state.clone(),
        power_buzzer.clone(),
        sensor_state.clone(),
        buzzer_priority_manager.clone(),
        mqtt_handle.clone(),
        Some(storage_handle.clone()),
    ) {
        Ok(monitor) => {
            eprintln!("[main] Sensor monitor started");
            Some(monitor)
        }
        Err(e) => {
            eprintln!("[main] Warning: Failed to initialize sensor monitor: {}", e);
            None
        }
    };

    // Create and spawn LoRaWAN monitor if MQTT is available — unless the v2 radio stack owns
    // the radio on this Hub (decision #12, libs::radio_gate). Both stacks running would mean
    // two MQTT consumers of the same ChirpStack events and two writers of the same devices.
    let v2_radio = fiber_app::libs::radio_gate::legacy_lorawan_disabled();
    let _lorawan_monitor = if let Some(ref why) = v2_radio {
        eprintln!("[main] {}", why);
        // The LCD still shows Nodes: proximos-control's readings, mirrored by the MQTT
        // monitor (#180).
        if let Ok(mut state) = _display_monitor.display_state.lock() {
            state.lorawan_state = Some(fiber_app::libs::display_nodes::state().clone());
        }
        None
    } else if let Some(ref handle) = mqtt_handle {
        let mut lorawan_config = config.lorawan.clone().unwrap_or_else(|| {
            // Auto-enable if gateway hardware is detected
            let mut cfg = LoRaWANConfig::default();
            cfg.enabled = true;
            cfg
        });

        // If ChirpStack creds are not set and it points at the same broker as
        // the main MQTT client, reuse the main broker's credentials.
        if let Some(ref main_mqtt) = config.mqtt {
            if lorawan_config.chirpstack_mqtt_username.is_none()
                && lorawan_config.chirpstack_mqtt_password.is_none()
                && lorawan_config.chirpstack_mqtt_host == main_mqtt.broker.host
                && lorawan_config.chirpstack_mqtt_port == main_mqtt.broker.port
            {
                lorawan_config.chirpstack_mqtt_username = main_mqtt.broker.username.clone();
                lorawan_config.chirpstack_mqtt_password = main_mqtt.broker.password.clone();
            }
        }

        eprintln!("[main] Starting LoRaWAN monitor...");
        // Load YAML defaults for node per-field thresholds. These give newly
        // paired nodes automatic alarming (mirrors DS18B20 `common_alarms`).
        let lorawan_field_defaults: fiber_app::libs::lorawan::SharedFieldThresholdDefaults =
            Arc::new(
                fiber_app::libs::config::SensorFileConfig::load_default()
                    .ok()
                    .map(|c| c.common_lorawan_field_thresholds)
                    .unwrap_or_default(),
            );
        match LoRaWANMonitor::new(
            lorawan_config,
            lorawan_configs.clone(),
            lorawan_field_defaults,
            handle.sender(),
            hostname.clone(),
            Some(buzzer_priority_manager.clone()),
            storage_handle.clone(),
        ) {
            Ok(monitor) => {
                // Set LoRaWAN gateway flag and shared state in display state
                let gateway_present = monitor
                    .state
                    .read()
                    .map(|s| s.gateway_present)
                    .unwrap_or(false);
                if let Ok(mut state) = _display_monitor.display_state.lock() {
                    state.lorawan_state = Some(monitor.state.clone());
                    if gateway_present {
                        state.lorawan_gateway_present = true;
                    }
                }
                if gateway_present {
                    eprintln!("[main] LoRaWAN monitor started (gateway detected)");
                } else {
                    eprintln!("[main] LoRaWAN monitor started (no gateway detected)");
                }
                Some(monitor)
            }
            Err(e) => {
                // Not fatal — wired sensors, MQTT and the LCD are unaffected — but
                // not a warning either: on a unit with gateway hardware this means
                // the LoRaWAN bridge is down until someone acts.
                eprintln!("[main] ERROR: LoRaWAN monitor did not start: {}", e);
                None
            }
        }
    } else {
        eprintln!("[main] LoRaWAN monitor disabled (MQTT not enabled)");
        None
    };

    // Wire LoRaWAN state into MQTT monitor now that both exist.
    if let (Some(ref mqtt_mon), Some(ref lr_mon)) =
        (_mqtt_monitor.as_ref(), _lorawan_monitor.as_ref())
    {
        mqtt_mon.set_lorawan_state(lr_mon.state.clone());
        mqtt_mon.set_lorawan_handle(lr_mon.handle());
    }

    // Control socket server (#79): local CLI control plane for dev/testing,
    // driven by the `fiberctl` binary. Disable with FIBER_CONTROL_DISABLE=1.
    if std::env::var("FIBER_CONTROL_DISABLE").is_err() {
        use fiber_app::libs::control::{protocol, server};
        let ctx = server::ControlContext::new(
            control_config.system.app_version.clone(),
            control_config.clone(),
            _lorawan_monitor.as_ref().map(|m| m.handle()),
            _lorawan_monitor.as_ref().map(|m| m.state.clone()),
            std::time::Duration::from_secs(30),
        )
        .with_power(power_status.clone())
        .with_sensors(sensor_state.clone())
        .with_power_control(storage_handle.clone(), stm_guard.clone());
        let ctx = if let Some(ref m) = _mqtt_monitor {
            ctx.with_mqtt_connection(m.connection_state())
        } else {
            ctx
        };
        let ctx = if let Some(ref a) = ble_config_applier {
            ctx.with_config_applier(a.clone())
        } else {
            ctx
        };
        let path = protocol::socket_path();
        // The provisioning account's socket (plan/system/factory_test.md §4.1),
        // on an image that has the account and its directory.
        if std::path::Path::new(server::FACTORY_SOCKET_DIR).is_dir() {
            let fctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("factory-socket".into())
                .spawn(move || {
                    if let Err(e) = server::serve_factory(fctx, server::FACTORY_SOCKET_DIR) {
                        eprintln!("[main] factory socket exited: {}", e);
                    }
                });
            if let Err(e) = spawned {
                eprintln!("[main] Warning: failed to spawn the factory socket: {}", e);
            }
        }
        match std::thread::Builder::new()
            .name("control-server".into())
            .spawn(move || {
                if let Err(e) = server::serve(ctx, &path) {
                    eprintln!("[main] control server exited: {}", e);
                }
            }) {
            Ok(_) => eprintln!("[main] control server thread spawned"),
            Err(e) => eprintln!("[main] Warning: failed to spawn control server: {}", e),
        }
    } else {
        eprintln!("[main] control server disabled (FIBER_CONTROL_DISABLE set)");
    }

    // Fill the BLE FB0D node-add slot with the LoRaWAN shared state, so an
    // enrollment over BLE inserts the same optimistic stub the MQTT add does.
    // NOTE: the LoRaWAN monitor only exists when MQTT is enabled. In a
    // pure BLE-only / MQTT-disabled deployment this slot stays None, so a
    // FB0D enrollment still provisions ChirpStack + saves config, but the
    // optimistic shared-state stub (display-before-first-uplink) is skipped.
    if let Some(lr_mon) = _lorawan_monitor.as_ref() {
        if let Ok(mut slot) = ble_lorawan_state_slot.lock() {
            *slot = Some(lr_mon.state.clone());
        }
    }

    // ---------------------------------------------------------------------------------
    // Bluetooth — one component, both roles (plan/control/ble/*.md)
    //
    // v1 had two: `BleMonitor` served the FB00 GATT server to the phone, `BeaconMonitor`
    // scanned for Teltonika tags, and they shared an adapter through a set of live
    // connection addresses so neither mistook the other's connection for its own. Both are
    // gone. `control-ble` owns the adapter, runs whichever roles `ble.mode` allows, and
    // makes the shared-slot arbitration internal.
    //
    // Started here rather than beside the other monitors, deliberately: the Beacon
    // publisher writes into the MQTT channel, and starting before `mqtt_handle` exists
    // would mean the first minutes of readings go into a channel nobody is reading.
    //
    // Degraded-green throughout (CP-2): a Hub with no controller, a refused advertisement
    // or a blocked rfkill logs and keeps running. Bluetooth is not the reason a medical
    // thermometer stops measuring.
    // ---------------------------------------------------------------------------------
    // The cluster's radio mode: a Server publishes it to every Client (decision #48).
    if let (Some(h), Some(a)) = (mqtt_handle.as_ref(), ble_config_applier.as_ref()) {
        fiber_app::libs::cluster_radio::spawn(a.clone(), h.sender());
    }
    let _ble_runtime = start_bluetooth(
        ble_section,
        std::path::Path::new("/data/fiber/config"),
        ble_config_applier.clone(),
        mqtt_handle.as_ref().map(|h| h.sender()),
        storage_handle.clone(),
        hostname.clone(),
        mac_address.clone(),
        fiber_app::libs::lorawan::NodeAddDeps {
            config_applier: ble_config_applier.clone(),
            storage: Some(storage_handle.clone()),
            lorawan_configs: Some(lorawan_configs.clone()),
            lorawan_state: ble_lorawan_state_slot.lock().ok().and_then(|s| s.clone()),
        },
        _display_monitor.display_state.clone(),
    );

    // Application is now running with background monitoring
    eprintln!("[main] Application running with medical data persistence. Press Ctrl+C to exit.");

    // Install a signal handler so SIGINT/SIGTERM drives a graceful shutdown
    // path (flush export cursors, drop background tasks, run Drop impls)
    // instead of the kernel killing us mid-write. The previous busy-loop
    // had no exit path, so PowerMonitor/Storage/Export never got to run
    // their Drop cleanup on a stop signal.
    let shutdown_signal = Arc::new(AtomicBool::new(false));
    let shutdown_signal_handler = shutdown_signal.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        eprintln!("[main] Shutdown signal received");
        shutdown_signal_handler.store(true, Ordering::SeqCst);
    }) {
        eprintln!(
            "[main] WARN: failed to install signal handler: {} — shutdown will be ungraceful",
            e
        );
    }

    // Wait for signal — background monitors do all the work.
    while !shutdown_signal.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    eprintln!("[main] Shutting down …");

    // Tell the mqtt_export orchestrator to break its loop. This drops
    // in-flight publishes and final cursor advances cleanly instead of
    // the OS reaping the thread mid-write.
    if let Some(eh) = export_handle_opt.as_ref() {
        if let Err(e) = eh.shutdown() {
            eprintln!("[main] export shutdown send failed: {}", e);
        }
    }

    // Best-effort: give background threads a short window to finish.
    // Storage / sensor / display monitors run their Drop on scope exit.
    std::thread::sleep(std::time::Duration::from_millis(500));
    eprintln!("[main] Goodbye");
    Ok(())
}

/// Start `control-ble`'s runtime on its own OS thread and bridge its events to the display.
///
/// **Why a thread and not a task on someone else's runtime.** `monolith.md` §4.1 already gives BLE
/// its own thread, and it stays that way: bluer's D-Bus connection wants a multi-thread reactor, and
/// a stalled adapter must not be able to starve the sensor loop. The runtime is built by hand rather
/// than with `#[tokio::main]` for the same reason the crate does — this process has other threads
/// with other needs, and only this one should be tokio's.
///
/// **Degraded-green everywhere.** Every failure here logs and returns: no controller, an
/// unparseable `ble:` section, a refused advertisement. A Hub whose Bluetooth is broken still
/// measures temperature, which is the thing it is certified to do (CP-2).
///
/// Returns the thread's join handle so `main` can keep it alive; dropping it detaches the thread,
/// which is exactly what a shutdown wants.
#[allow(clippy::too_many_arguments)]
fn start_bluetooth(
    ble_section: Result<String, Box<dyn std::error::Error>>,
    config_dir: &std::path::Path,
    applier: Option<Arc<fiber_app::ConfigApplier>>,
    mqtt_tx: Option<crossbeam::channel::Sender<fiber_app::libs::mqtt::messages::MqttMessage>>,
    storage: fiber_app::libs::storage::StorageHandle,
    hostname: String,
    mac: String,
    node_deps: fiber_app::libs::lorawan::NodeAddDeps,
    display_state: fiber_app::libs::display::SharedDisplayStateHandle,
) -> Option<std::thread::JoinHandle<()>> {
    use control_ble::mode::Mode;

    let Some(applier) = applier else {
        eprintln!(
            "[main] Bluetooth not started: no ConfigApplier, so a phone could not change anything \
             it connected to change"
        );
        return None;
    };

    // The `ble:` section is read straight from the device config until `proximosctl render` exists
    // (system/design.md §7). A malformed section is fatal *to Bluetooth*, loudly: `BleConfig` is
    // deny_unknown_fields, so a leftover v1 key — `pin`, `enable_terminal` — is named rather than
    // ignored. Silently ignoring `enable_terminal` would leave an operator believing the BLE
    // terminal still has a switch, when CP-1 removed the terminal itself.
    let ble_config = match ble_section {
        Ok(text) => match control_ble::config::BleConfig::from_device_config_yaml(&text) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[main] Bluetooth not started: the ble: section is invalid: {e}");
                return None;
            }
        },
        Err(e) => {
            eprintln!("[main] Bluetooth not started: cannot read the ble: section: {e}");
            return None;
        }
    };

    if ble_config.mode == Mode::Off {
        eprintln!("[main] Bluetooth is off by configuration (ble.mode: off)");
        return None;
    }

    // With no MQTT there is nowhere to publish readings, but the phone surface is still worth
    // serving — that is how an installer configures a Hub that has never had a broker.
    let (mqtt_tx, publishing) = match mqtt_tx {
        Some(tx) => (tx, true),
        None => {
            let (tx, rx) = crossbeam::channel::bounded(1);
            // The receiver is dropped on purpose: `MqttPublisher` treats a closed channel as a
            // logged drop, which is the honest behaviour when there is no broker.
            drop(rx);
            (tx, false)
        }
    };
    if !publishing {
        eprintln!(
            "[main] Bluetooth running without MQTT: Beacon readings are stored, not published"
        );
    }

    let mqtt_tx_for_cluster = mqtt_tx.clone();
    let hostname_for_cluster = hostname.clone();
    let bundle = fiber_app::libs::ble_host::build(
        applier,
        config_dir,
        mqtt_tx,
        storage,
        hostname,
        mac.clone(),
        node_deps,
    );
    // Beacons belong to the cluster (decision #42): a Server computes and pushes the home Hubs.
    if publishing {
        fiber_app::libs::beacon_cluster::spawn(
            hostname_for_cluster,
            bundle.host.config.clone(),
            mqtt_tx_for_cluster,
        );
    }

    // A Hub whose own MAC will not parse cannot address a QR payload, and a phone sent to
    // 00:00:00:00:00:00 has no way to find it. The FB00 server reads the live controller address
    // anyway and overrides this; it is the pre-registration placeholder only.
    let hub_mac = match control_ble::mac::MacAddr::parse(&mac) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[main] Bluetooth not started: this Hub's MAC is unusable: {e}");
            return None;
        }
    };
    let store = control_ble::store::Store::at_default_root();

    let handle = std::thread::Builder::new()
        .name("ble".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[main] Bluetooth not started: cannot build its runtime: {e}");
                    return;
                }
            };

            let runtime = control_ble::tasks::spawn(
                rt.handle(),
                &ble_config,
                bundle.host.clone(),
                store,
                hub_mac,
            );

            // Fill the one seam that could not exist before the spawn, and publish the seams so the
            // signed-command executor on the MQTT thread can reach them.
            if let Some(central) = runtime.handles.central.clone() {
                bundle.subsystem.fill(central.clone());
            }
            if !fiber_app::libs::ble_host::install(fiber_app::libs::ble_host::BleSeams {
                host: bundle.host.clone(),
                central: runtime.handles.central.clone(),
                peripheral: runtime.handles.peripheral.clone(),
            }) {
                eprintln!("[main] WARN: the BLE seams were already installed");
            }

            // v1's `event_router.rs` lived inside the GATT server. The server moved into the crate
            // and the display did not, so the bridge is here now.
            if let Some(peripheral) = runtime.handles.peripheral.clone() {
                let mut rx = peripheral.subscribe();
                let display = display_state.clone();
                rt.spawn(async move {
                    while let Ok(event) = rx.recv().await {
                        bridge_peripheral_event(&event, &display);
                    }
                });
            }

            eprintln!(
                "[main] Bluetooth running: mode={} central={} peripheral={}",
                ble_config.mode.as_str(),
                runtime.handles.central.is_some(),
                runtime.handles.peripheral.is_some()
            );

            // Hold the runtime open for the life of the process. `main` signals shutdown by exiting,
            // which drops this thread's stack and with it the tasks.
            rt.block_on(async {
                std::future::pending::<()>().await;
            });
        });

    match handle {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("[main] Bluetooth not started: cannot spawn its thread: {e}");
            None
        }
    }
}

/// One display-worthy BLE transition → the LCD.
///
/// A port of v1's `libs/ble/event_router.rs::handle`, which lived on the server side of the split
/// and therefore could not come along. The two screen-ownership rules it encoded are preserved
/// exactly, because both were learned from the bench:
///
/// - **A connection is a notification, not a navigation.** If the operator opened a screen at the
///   panel, a phone connecting must not steal it out from under them.
/// - **Only dismiss our own screen.** Returning to the overview on disconnect is right only if the
///   screen showing is one this bridge put up.
///
/// It leaves the pairing flow alone. v1 cancelled a showing pairing code on connect and blocked the
/// menu from starting one while connected; with FB1A the app is the thing the operator types that
/// code into, over the connection it is holding, so both would refuse exactly the flow FB1A is for.
fn bridge_peripheral_event(
    event: &control_ble::peripheral::events::PeripheralEvent,
    display: &fiber_app::libs::display::SharedDisplayStateHandle,
) {
    use control_ble::peripheral::events::PeripheralEvent as E;
    match event {
        E::ClientConnected { addr } => {
            if let Ok(mut d) = display.lock() {
                if d.current_screen.is_operator_owned() {
                    eprintln!(
                        "[ble] {} — leaving the operator's screen up",
                        event.summary()
                    );
                } else {
                    d.show_ble_connected(addr);
                }
            }
        }
        E::ClientDisconnected { .. } => {
            if let Ok(mut d) = display.lock() {
                if d.current_screen.is_ble_screen() {
                    d.show_sensor_overview();
                }
            }
        }
        // Auth alone changes no screen: the BLE Connected screen is already the right one, and
        // flashing something on a failed guess would only teach an onlooker that a guess was made.
        E::AuthSucceeded { .. } | E::AuthFailed { .. } => {}
        E::WifiConnected { ssid, ip } => {
            if let Ok(mut d) = display.lock() {
                d.show_ble_wifi_ok(ssid, ip);
            }
        }
        E::WifiFailed { error, .. } => {
            if let Ok(mut d) = display.lock() {
                d.show_ble_wifi_fail(error);
            }
        }
        // LCD feedback for LAN was a follow-up in v1 and still is; the log line is the record.
        E::LanConfigured { .. } | E::LanFailed { .. } => {}
    }
    eprintln!("[ble] {}", event.summary());
}

#[cfg(test)]
mod hostname_tests {
    #[test]
    fn the_hostname_is_one_lowercase_spelling() {
        assert_eq!(
            super::normalize_hostname("FIBER-4E970F4A\n"),
            "fiber-4e970f4a"
        );
        assert_eq!(
            super::normalize_hostname("  proximos-a02ecc90 "),
            "proximos-a02ecc90"
        );
    }
}
