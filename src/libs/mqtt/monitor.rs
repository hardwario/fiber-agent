// MQTT monitor thread - main implementation

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam::channel::{bounded, Receiver, Sender};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, TlsConfiguration, Transport};

use crate::libs::config::MqttConfig;
use crate::libs::network::status::{get_network_status, NetworkStatus};

use super::connection::{create_shared_connection_state, ConnectionState, SharedConnectionState};
use super::messages::{MqttCommand, MqttMessage};

/// Depth of rumqttc's request channel — how many publishes may be in flight to the
/// eventloop before `client.publish()` starts failing and the message is lost.
/// See the call site for the measurement that motivated raising it from 10.
const MQTT_REQUEST_CHANNEL_CAPACITY: usize = 256;
use super::publisher::MqttPublisher;
use super::subscriber::MqttSubscriber;
use super::topics::TopicBuilder;

use crate::libs::authorization::AuthorizationManager;
use crate::libs::config_applier::ConfigApplier;
use crate::libs::crypto::{CARegistry, NonceTracker, SignatureVerifier};
use crate::libs::mqtt_export::ExportHandle;
use crate::libs::pairing::PairingHandle;
use std::sync::Mutex;

/// Shared pairing handle that can be set after MQTT monitor is created
pub type SharedPairingHandle = Arc<Mutex<Option<PairingHandle>>>;

/// Shared mqtt_export handle, populated after the export thread is spawned.
/// Used by ResetExportCursor to invalidate the orchestrator's in-memory
/// cursor cache — without it, the cache out-lives a DB-side reset and the
/// drain silently keeps skipping rows the operator asked to replay.
pub type SharedExportHandle = Arc<Mutex<Option<ExportHandle>>>;

/// Shared STM bridge for hardware commands
pub type SharedStmBridge = Arc<Mutex<crate::drivers::StmBridge>>;

/// Shared screen brightness handle for display backlight control
pub type SharedScreenBrightnessHandle = std::sync::Arc<std::sync::atomic::AtomicU8>;

/// Shared screen idle-timeout handle in seconds (0 = always on)
pub type SharedScreenTimeoutHandle = std::sync::Arc<std::sync::atomic::AtomicU32>;

/// Shared buzzer volume handle (0 = muted, 1-100 = active)
pub type SharedBuzzerVolumeHandle = std::sync::Arc<std::sync::atomic::AtomicU8>;

/// Shared physical-display line config. Re-exported rather than redeclared —
/// unlike the atomics above this is a real container type and two names for it
/// would be one name too many.
pub use crate::libs::display::SharedDisplayLinesHandle;

/// Error category for diagnostics
#[derive(Debug, Clone, Copy)]
enum ErrorCategory {
    NetworkUnreachable,
    ConnectionRefused,
    Timeout,
    ConnectionReset,
    ProtocolError,
    Unknown,
}

impl std::fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorCategory::NetworkUnreachable => write!(f, "Network Unreachable"),
            ErrorCategory::ConnectionRefused => write!(f, "Connection Refused"),
            ErrorCategory::Timeout => write!(f, "Timeout"),
            ErrorCategory::ConnectionReset => write!(f, "Connection Reset"),
            ErrorCategory::ProtocolError => write!(f, "Protocol Error"),
            ErrorCategory::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Reconnection state with exponential backoff
struct ReconnectionState {
    attempt_count: u32,
    base_delay_sec: u64,
    max_delay_sec: u64,
}

impl ReconnectionState {
    fn new(base_delay_sec: u64, max_delay_sec: u64) -> Self {
        Self {
            attempt_count: 0,
            base_delay_sec,
            max_delay_sec,
        }
    }

    fn calculate_delay(&mut self) -> Duration {
        // Exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s, 60s (max)
        let delay = std::cmp::min(
            self.base_delay_sec
                .saturating_mul(2_u64.pow(self.attempt_count)),
            self.max_delay_sec,
        );

        // Add jitter ±20% to prevent thundering herd
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let jitter = rng.gen_range(-0.2..=0.2);
        let final_delay = ((delay as f64) * (1.0 + jitter)).max(1.0) as u64;

        eprintln!(
            "[MQTT Monitor] Reconnection attempt #{} - waiting {}s",
            self.attempt_count + 1,
            final_delay
        );

        self.attempt_count += 1;
        Duration::from_secs(final_delay)
    }

    fn reset(&mut self) {
        if self.attempt_count > 0 {
            eprintln!("[MQTT Monitor] Reconnection successful - resetting backoff");
        }
        self.attempt_count = 0;
    }
}

/// Categorize MQTT connection error
fn categorize_error(error: &rumqttc::ConnectionError) -> ErrorCategory {
    let error_str = format!("{:?}", error).to_lowercase();

    if error_str.contains("network") || error_str.contains("unreachable") {
        ErrorCategory::NetworkUnreachable
    } else if error_str.contains("refused") || error_str.contains("connection refused") {
        ErrorCategory::ConnectionRefused
    } else if error_str.contains("timeout") || error_str.contains("timed out") {
        ErrorCategory::Timeout
    } else if error_str.contains("reset") || error_str.contains("broken pipe") {
        ErrorCategory::ConnectionReset
    } else if error_str.contains("protocol") || error_str.contains("packet") {
        ErrorCategory::ProtocolError
    } else {
        ErrorCategory::Unknown
    }
}

/// Wait for network to become available
fn wait_for_network(timeout_sec: u64) -> bool {
    eprintln!("[MQTT Monitor] Waiting for network...");
    let start = Instant::now();

    while start.elapsed() < Duration::from_secs(timeout_sec) {
        let network = get_network_status();
        if network.wifi_connected || network.ethernet_connected {
            eprintln!(
                "[MQTT Monitor] Network available: WiFi={}, Ethernet={}",
                network.wifi_connected, network.ethernet_connected
            );
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }

    eprintln!(
        "[MQTT Monitor] Network unavailable after {}s timeout",
        timeout_sec
    );
    false
}

/// Set when something changed that `system/info` reports and an operator is
/// waiting to see it — today, a cluster arm.
///
/// `system/info` is retained and published on its own schedule (60 s by
/// default), which is the right cadence for telemetry and the wrong one for
/// feedback: the viewer's cluster card would keep showing the old role for up to
/// a minute after the change had already taken effect, which reads as a failed
/// command. A flag rather than a channel because the status block is not a
/// `select!` arm — it is a branch in the connected loop, and this is the same
/// shape as the interval check beside it.
static SYSTEM_INFO_PUBLISH_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ask the status loop to publish `system/info` on its next pass.
pub fn request_system_info_publish() {
    SYSTEM_INFO_PUBLISH_REQUESTED.store(true, Ordering::Relaxed);
}

/// Consume a pending request. Clearing it here means a burst of changes
/// collapses into one publish rather than one per change.
fn take_system_info_publish_request() -> bool {
    SYSTEM_INFO_PUBLISH_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Check if MQTT broker is reachable
fn check_broker_reachable(host: &str, port: u16) -> bool {
    let addr = format!("{}:{}", host, port);
    eprintln!("[MQTT Monitor] Checking broker reachability: {}", addr);

    // Try to resolve and connect with timeout
    match addr.to_socket_addrs() {
        Ok(mut addrs) => {
            if let Some(socket_addr) = addrs.next() {
                match TcpStream::connect_timeout(&socket_addr, Duration::from_secs(5)) {
                    Ok(_) => {
                        eprintln!("[MQTT Monitor] ✓ Broker is reachable");
                        true
                    }
                    Err(e) => {
                        eprintln!("[MQTT Monitor] ✗ Broker unreachable: {}", e);
                        false
                    }
                }
            } else {
                eprintln!("[MQTT Monitor] ✗ Failed to resolve broker address");
                false
            }
        }
        Err(e) => {
            eprintln!("[MQTT Monitor] ✗ Failed to resolve broker address: {}", e);
            false
        }
    }
}

/// Create MQTT client options with all configured parameters
fn create_mqtt_options(config: &MqttConfig, hostname: &str, client_id: &str) -> MqttOptions {
    // Start with configured port — may be overridden to 8883 if TLS succeeds
    let mut mqttoptions =
        MqttOptions::new(client_id, config.broker.host.clone(), config.broker.port);

    // Set connection parameters
    mqttoptions.set_keep_alive(Duration::from_secs(config.connection.keep_alive_sec));
    mqttoptions.set_clean_session(config.connection.clean_session);

    // Set credentials if provided
    if let (Some(username), Some(password)) = (&config.broker.username, &config.broker.password) {
        eprintln!("[MQTT Monitor] Setting credentials for user: {}", username);
        mqttoptions.set_credentials(username, password);
    }

    // Configure TLS transport when the tls config section is present and enabled.
    // Falls back to plain TCP when TLS is absent or explicitly disabled.
    // If TLS succeeds and port is default 1883, recreate options with 8883.
    if let Some(ref tls) = config.tls {
        if tls.enabled {
            match configure_tls_transport(tls) {
                Ok(transport) => {
                    if config.broker.port == 1883 {
                        // Recreate with TLS port (MqttOptions has no set_port)
                        mqttoptions = MqttOptions::new(client_id, config.broker.host.clone(), 8883);
                        mqttoptions
                            .set_keep_alive(Duration::from_secs(config.connection.keep_alive_sec));
                        mqttoptions.set_clean_session(config.connection.clean_session);
                        if let (Some(u), Some(p)) =
                            (&config.broker.username, &config.broker.password)
                        {
                            mqttoptions.set_credentials(u, p);
                        }
                        eprintln!("[MQTT Monitor] TLS enabled — port overridden 1883 -> 8883");
                    }
                    mqttoptions.set_transport(transport);
                    eprintln!("[MQTT Monitor] TLS transport configured successfully");
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("No such file") || err_str.contains("not found") {
                        // Cert not deployed yet — fall back to plaintext for local broker
                        eprintln!("[MQTT Monitor] WARNING: TLS cert not found, falling back to plaintext: {}", e);
                    } else {
                        // Real TLS error — in production, set transport to a broken state
                        // so the connection fails at TLS handshake, not plaintext fallback
                        eprintln!(
                            "[MQTT Monitor] FATAL: Failed to configure TLS transport: {}",
                            e
                        );
                        #[cfg(not(feature = "dev-platform"))]
                        {
                            eprintln!("[MQTT Monitor] Production build: TLS failure is fatal, connection will fail");
                            // Set a TLS transport with empty/invalid config — connection will
                            // fail at handshake rather than silently falling back to plaintext
                            mqttoptions.set_transport(Transport::tls_with_config(
                                TlsConfiguration::SimpleNative {
                                    ca: vec![],
                                    client_auth: None,
                                },
                            ));
                        }
                        #[cfg(feature = "dev-platform")]
                        {
                            eprintln!("[MQTT Monitor] DEV-PLATFORM: TLS failed, falling back to plaintext: {}", e);
                        }
                    }
                }
            }
        }
    }

    // Set Last Will and Testament
    if config.last_will.enabled {
        let lwt_topic = if config.publish.include_hostname {
            format!(
                "{}/{}/{}",
                config.publish.topic_prefix, hostname, config.last_will.topic
            )
        } else {
            format!("{}/{}", config.publish.topic_prefix, config.last_will.topic)
        };

        let qos = match config.last_will.qos {
            0 => QoS::AtMostOnce,
            1 => QoS::AtLeastOnce,
            2 => QoS::ExactlyOnce,
            _ => QoS::AtLeastOnce,
        };

        mqttoptions.set_last_will(rumqttc::LastWill {
            topic: lwt_topic,
            message: config.last_will.payload.as_bytes().to_vec().into(),
            qos,
            retain: config.last_will.retain,
        });
    }

    mqttoptions
}

/// Build a TLS [`Transport`] from the application's [`TlsConfig`].
///
/// Builds a native-tls `TlsConnector` directly (rather than using rumqttc's
/// `TlsConfiguration::SimpleNative` convenience path, which only accepts a
/// PKCS#12 client identity) so mutual TLS can keep using the existing
/// PEM-encoded cert + key files via `Identity::from_pkcs8`.
fn configure_tls_transport(tls: &crate::libs::config::TlsConfig) -> Result<Transport, String> {
    // Load CA certificate (PEM-encoded)
    let ca = std::fs::read(&tls.ca_cert_path).map_err(|e| {
        format!(
            "Failed to read CA certificate from '{}': {}",
            tls.ca_cert_path, e
        )
    })?;

    if ca.is_empty() {
        return Err(format!(
            "CA certificate file '{}' is empty",
            tls.ca_cert_path
        ));
    }

    eprintln!(
        "[MQTT TLS] Loaded CA certificate ({} bytes) from {}",
        ca.len(),
        tls.ca_cert_path
    );

    // Optionally load client certificate + key for mutual TLS
    let client_auth = match (&tls.client_cert_path, &tls.client_key_path) {
        (Some(cert_path), Some(key_path)) => {
            let cert = std::fs::read(cert_path).map_err(|e| {
                format!(
                    "Failed to read client certificate from '{}': {}",
                    cert_path, e
                )
            })?;
            let key = std::fs::read(key_path)
                .map_err(|e| format!("Failed to read client key from '{}': {}", key_path, e))?;

            if cert.is_empty() {
                return Err(format!("Client certificate file '{}' is empty", cert_path));
            }
            if key.is_empty() {
                return Err(format!("Client key file '{}' is empty", key_path));
            }

            eprintln!(
                "[MQTT TLS] Loaded client certificate ({} bytes) and key ({} bytes) for mutual TLS",
                cert.len(),
                key.len()
            );
            Some((cert, key))
        }
        (Some(_), None) => {
            return Err(
                "client_cert_path is set but client_key_path is missing — both are required for mutual TLS".to_string()
            );
        }
        (None, Some(_)) => {
            return Err(
                "client_key_path is set but client_cert_path is missing — both are required for mutual TLS".to_string()
            );
        }
        (None, None) => {
            eprintln!("[MQTT TLS] No client certificate configured — using server-only TLS");
            None
        }
    };

    use rumqttc::tokio_native_tls::native_tls::{Certificate, Identity, TlsConnector};

    let mut builder = TlsConnector::builder();

    // Trust only the configured CA, not the OS's built-in root store — this
    // is a private medical-device network, not a public-internet client.
    builder.disable_built_in_roots(true);
    let ca_cert =
        Certificate::from_pem(&ca).map_err(|e| format!("Invalid CA certificate: {}", e))?;
    builder.add_root_certificate(ca_cert);

    if let Some((cert_pem, key_pem)) = &client_auth {
        let identity = Identity::from_pkcs8(cert_pem, key_pem)
            .map_err(|e| format!("Invalid client certificate/key: {}", e))?;
        builder.identity(identity);
    }

    if tls.insecure_skip_verify {
        eprintln!(
            "[MQTT TLS] WARNING: insecure_skip_verify=true — skipping certificate validation"
        );
        builder.danger_accept_invalid_certs(true);
        builder.danger_accept_invalid_hostnames(true);
    }

    let connector = builder
        .build()
        .map_err(|e| format!("Failed to build TLS connector: {}", e))?;

    Ok(Transport::tls_with_config(
        TlsConfiguration::NativeConnector(connector),
    ))
}

/// MQTT monitor handle for sending messages
#[derive(Clone)]
pub struct MqttHandle {
    sender: Sender<MqttMessage>,
    /// Flag set to true when MQTT reconnects, so sensor monitor can flush immediately
    pub reconnected_flag: Arc<AtomicBool>,
}

impl MqttHandle {
    /// Get a clone of the underlying sender (for bridging from other modules like LoRaWAN)
    pub fn sender(&self) -> Sender<MqttMessage> {
        self.sender.clone()
    }

    /// Send a message to the MQTT monitor (non-blocking)
    pub fn send(&self, msg: MqttMessage) {
        // If channel is full, log warning and drop message (prevents blocking)
        if let Err(e) = self.sender.try_send(msg) {
            eprintln!("[MQTT Handle] Warning: Failed to send message: {}", e);
        }
    }

    /// Send alarm event
    pub fn send_alarm_event(
        &self,
        line: u8,
        name: &str,
        from_state: crate::libs::alarms::AlarmState,
        to_state: crate::libs::alarms::AlarmState,
        temperature: f32,
    ) {
        self.send(MqttMessage::PublishAlarmEvent {
            line,
            name: name.to_string(),
            from_state,
            to_state,
            temperature,
        });
    }

    /// Send aggregated sensor data
    pub fn send_aggregated_sensor_data(
        &self,
        period: crate::libs::sensors::aggregation::AggregationPeriod,
        names: [String; 8],
        locations: [Option<String>; 8],
    ) {
        self.send(MqttMessage::PublishAggregatedSensorData {
            period,
            names,
            locations,
        });
    }

    /// Send combined system status (power, network, storage, uptime, lorawan)
    #[allow(clippy::too_many_arguments)]
    pub fn send_system_status(
        &self,
        hostname: String,
        device_label: String,
        version: String,
        uptime_seconds: u64,
        battery_mv: u16,
        battery_percent: u8,
        vin_mv: u16,
        on_dc_power: bool,
        last_dc_loss_time: Option<u64>,
        wifi_connected: bool,
        wifi_signal_dbm: i32,
        wifi_ip: Option<String>,
        ethernet_connected: bool,
        ethernet_ip: Option<String>,
        storage_total_bytes: u64,
        storage_available_bytes: u64,
        storage_used_percent: u8,
        lorawan_gateway_present: bool,
        lorawan_concentratord_running: bool,
        lorawan_chirpstack_running: bool,
        lorawan_sensor_count: usize,
    ) {
        self.send(MqttMessage::PublishSystemStatus {
            hostname,
            device_label,
            version,
            uptime_seconds,
            battery_mv,
            battery_percent,
            vin_mv,
            on_dc_power,
            last_dc_loss_time,
            wifi_connected,
            wifi_signal_dbm,
            wifi_ip,
            ethernet_connected,
            ethernet_ip,
            storage_total_bytes,
            storage_available_bytes,
            storage_used_percent,
            lorawan_gateway_present,
            lorawan_concentratord_running,
            lorawan_chirpstack_running,
            lorawan_sensor_count,
        });
    }
}

/// MQTT monitor thread
/// Floor for an fPort-85 command round-trip. A round-trip needs at least two
/// sticker uplinks, which on a Class-A sticker is bounded by its report
/// interval, so this is deliberately longer than the fiberctl ControlContext
/// default of 30 s.
///
/// This is only the FLOOR — see `sticker_command_timeout`. As a fixed value it
/// silently broke every sticker reporting slower than ~90 s: a config read is
/// chunked six fields at a time and each chunk waits for the device's next RX
/// window, so at `interval_report = 900 s` every chunk expired at 180 s and the
/// read returned nothing at all.
const STICKER_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Cap for a derived fPort-85 timeout.
///
/// `application.interval_report` accepts up to 86400 s, so the derivation has to
/// keep scaling well past any single "expected" interval — a cap that is too low
/// silently recreates the original bug for slow stickers. Waiting is cheap here:
/// responses are correlated by seq (1..=250) rather than queued per device, so a
/// pending command holds one task and one map entry and blocks nothing else. The
/// only real bound is seq reuse, which needs 250 further commands while one is
/// outstanding — far beyond any read. Six hours covers cadences up to ~2.4 h; a
/// sticker slower than that makes a full read take days, which is a decision for
/// the operator (the UI states the estimate) rather than something a timeout
/// should paper over. Clamping is logged so it is never silent.
const STICKER_COMMAND_TIMEOUT_CAP: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Timeout used while the sticker's cadence is still unknown.
///
/// Deliberately NOT the 180 s floor. "Unknown" means we cannot rule out a slow
/// sticker, so assuming a fast one is the wrong default — it is what made the
/// first read after a restart the most likely to fail. A sticker that has never
/// uplinked at all does not wait this long: `send_command` fails immediately,
/// because without an uplink the gateway has no ChirpStack application id to
/// address a downlink to.
const STICKER_COMMAND_TIMEOUT_UNKNOWN: std::time::Duration =
    std::time::Duration::from_secs(20 * 60);

/// Multiple of the sticker's reporting cadence to allow for one round trip.
/// A request rides the RX window after an uplink and the answer comes with a
/// later uplink, so two cadences is the floor for a healthy exchange; 2.5 leaves
/// room for one retry without doubling the wait.
const STICKER_COMMAND_CADENCE_FACTOR: f64 = 2.5;

/// Timeout for one fPort-85 round trip with `dev_eui`, derived from that
/// sticker's own observed reporting cadence.
///
/// Applies to writes as well as reads: a write is also only delivered in the
/// window after an uplink, so it has exactly the same lower bound.
///
/// Falls back to the floor while the cadence is still unknown (fewer than two
/// uplinks seen since start-up), which is the previous behaviour. Reads the
/// cadence off the handle's shared state, so no call site has to thread it in.
fn sticker_command_timeout(
    handle: &crate::libs::lorawan::LoRaWANHandle,
    dev_eui: &str,
) -> std::time::Duration {
    let cadence = handle
        .state
        .read()
        .ok()
        .and_then(|s| s.cadence_secs(&dev_eui.to_lowercase()));
    let out = match cadence {
        Some(secs) if secs > 0 => {
            let want =
                std::time::Duration::from_secs_f64(secs as f64 * STICKER_COMMAND_CADENCE_FACTOR);
            let clamped = want.clamp(STICKER_COMMAND_TIMEOUT, STICKER_COMMAND_TIMEOUT_CAP);
            if want > STICKER_COMMAND_TIMEOUT_CAP {
                eprintln!(
                    "[sticker] {dev_eui}: cadence {}s wants {}s but the cap is {}s — a full \
                     config read will not complete; shorten interval_report first",
                    secs,
                    want.as_secs(),
                    STICKER_COMMAND_TIMEOUT_CAP.as_secs()
                );
            }
            clamped
        }
        _ => STICKER_COMMAND_TIMEOUT_UNKNOWN,
    };
    // Logged unconditionally, including the fallback: when a read comes back empty
    // the first question is always "how long did we actually wait, and did we know
    // the cadence?", and answering it from the journal beats guessing.
    eprintln!(
        "[sticker] {dev_eui}: fPort-85 timeout {}s (observed cadence {})",
        out.as_secs(),
        cadence.map_or_else(|| "unknown".to_string(), |s| format!("{s}s")),
    );
    out
}

/// How long to wait for the encrypted audit row of a teardown command (reboot
/// or power-off) to reach disk. Bounded on purpose: the operator's signed intent
/// outranks a perfect audit trail, so a wedged storage thread degrades to a WARN
/// instead of leaving the device up forever.
const TEARDOWN_AUDIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Grace window between returning from a teardown executor and actually going
/// down, so the queued SUCCESS ack gets on the wire. See `execute_teardown` for
/// why this cannot be replaced by waiting on the child process.
const TEARDOWN_GRACE: Duration = Duration::from_millis(1500);

/// How long to wait for the display thread to confirm the panel is dark. Fits
/// inside [`TEARDOWN_GRACE`] and is an order of magnitude above the display
/// loop's 50 ms tick, so a running display always makes it; an absent or wedged
/// one degrades to a WARN rather than holding the device up.
const DISPLAY_BLANK_TIMEOUT: Duration = Duration::from_millis(500);

pub struct MqttMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    connection_state: SharedConnectionState,
    handle: MqttHandle,
    pairing_handle: SharedPairingHandle,
    stm_bridge: Option<SharedStmBridge>,
    screen_brightness: Option<SharedScreenBrightnessHandle>,
    screen_timeout: Option<SharedScreenTimeoutHandle>,
    buzzer_volume: Option<SharedBuzzerVolumeHandle>,
    display_lines: Option<SharedDisplayLinesHandle>,
    buzzer_priority: Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
    lorawan_state_slot:
        std::sync::Arc<std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>>,
    /// fPort-85 command handle, filled after the LoRaWAN monitor exists (see
    /// set_lorawan_handle); used by the sticker config/history MQTT commands.
    lorawan_handle_slot:
        std::sync::Arc<std::sync::Mutex<Option<crate::libs::lorawan::LoRaWANHandle>>>,
    lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
    export_handle_slot: SharedExportHandle,
}

impl MqttMonitor {
    /// Create and spawn MQTT monitor thread
    pub fn new(
        config: MqttConfig,
        hostname: String,
        app_version: String,
        power_status: crate::libs::power::status::SharedPowerStatus,
    ) -> io::Result<Self> {
        Self::new_with_stm(
            config,
            hostname,
            app_version,
            power_status,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// Create and spawn MQTT monitor thread with optional STM bridge for hardware commands
    pub fn new_with_stm(
        config: MqttConfig,
        hostname: String,
        app_version: String,
        power_status: crate::libs::power::status::SharedPowerStatus,
        stm_bridge: Option<SharedStmBridge>,
        screen_brightness: Option<SharedScreenBrightnessHandle>,
        screen_timeout: Option<SharedScreenTimeoutHandle>,
        buzzer_volume: Option<SharedBuzzerVolumeHandle>,
        display_lines: Option<SharedDisplayLinesHandle>,
        buzzer_priority: Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
        lorawan_state: Option<crate::libs::lorawan::SharedLoRaWANState>,
        lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
        storage_handle: Option<crate::libs::storage::StorageHandle>,
    ) -> io::Result<Self> {
        eprintln!(
            "[MQTT Monitor] Initializing MQTT monitor for host: {}",
            hostname
        );
        eprintln!(
            "[MQTT Monitor] Broker: {}:{}",
            config.broker.host, config.broker.port
        );
        if stm_bridge.is_some() {
            eprintln!("[MQTT Monitor] STM bridge available for hardware commands");
        }
        if screen_brightness.is_some() {
            eprintln!("[MQTT Monitor] Screen brightness control available");
        }
        if screen_timeout.is_some() {
            eprintln!("[MQTT Monitor] Screen timeout control available");
        }
        if buzzer_volume.is_some() {
            eprintln!("[MQTT Monitor] Buzzer volume control available");
        }

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        // Create bounded channel for messages
        let (sender, receiver) = bounded::<MqttMessage>(config.publish.max_queue_size);

        // Create shared connection state
        let connection_state = create_shared_connection_state();
        let connection_state_clone = connection_state.clone();

        // Create shared pairing handle slot (will be set later)
        let pairing_handle: SharedPairingHandle = Arc::new(Mutex::new(None));
        let pairing_handle_clone = pairing_handle.clone();

        // Create shared reconnect flag
        let reconnected_flag = Arc::new(AtomicBool::new(false));

        // Create handle for sending messages
        let handle = MqttHandle {
            sender,
            reconnected_flag: reconnected_flag.clone(),
        };
        let handle_clone = handle.clone();

        // Clone STM bridge for monitor thread
        let stm_bridge_clone = stm_bridge.clone();

        // Clone screen brightness for monitor thread
        let screen_brightness_clone = screen_brightness.clone();

        // Clone screen timeout for monitor thread
        let screen_timeout_clone = screen_timeout.clone();

        // Clone display line config for monitor thread
        let display_lines_clone = display_lines.clone();

        // Clone buzzer volume and priority for monitor thread
        let buzzer_volume_clone = buzzer_volume.clone();
        let buzzer_priority_clone = buzzer_priority.clone();

        // Clone reconnect flag for monitor thread
        let reconnected_flag_clone = reconnected_flag.clone();

        // Create LoRaWAN state slot (filled in later via set_lorawan_state)
        let lorawan_state_slot = std::sync::Arc::new(std::sync::Mutex::new(lorawan_state));
        let lorawan_state_slot_clone = lorawan_state_slot.clone();
        // Create LoRaWAN command-handle slot (filled in later via set_lorawan_handle)
        let lorawan_handle_slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let lorawan_handle_slot_clone = lorawan_handle_slot.clone();
        let lorawan_configs_clone = lorawan_configs.clone();
        let storage_handle_clone = storage_handle.clone();

        // Slot for the mqtt_export handle. main.rs fills this in after the
        // export thread is spawned (see `set_export_handle`).
        let export_handle_slot: SharedExportHandle = Arc::new(Mutex::new(None));
        let export_handle_slot_clone = export_handle_slot.clone();

        // Spawn monitoring thread
        let thread_handle = thread::spawn(move || {
            if let Err(e) = Self::monitor_loop(
                config,
                hostname,
                app_version,
                receiver,
                shutdown_flag_clone,
                connection_state_clone,
                pairing_handle_clone,
                power_status,
                stm_bridge_clone,
                screen_brightness_clone,
                screen_timeout_clone,
                buzzer_volume_clone,
                display_lines_clone,
                buzzer_priority_clone,
                reconnected_flag_clone,
                lorawan_state_slot_clone,
                lorawan_handle_slot_clone,
                lorawan_configs_clone,
                storage_handle_clone,
                export_handle_slot_clone,
            ) {
                eprintln!("[MQTT Monitor] Error in monitor loop: {}", e);
            }
        });

        eprintln!("[MQTT Monitor] MQTT monitor thread started");

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            connection_state,
            handle: handle_clone,
            pairing_handle,
            stm_bridge,
            screen_brightness,
            screen_timeout,
            buzzer_volume,
            display_lines,
            buzzer_priority,
            lorawan_state_slot,
            lorawan_handle_slot,
            lorawan_configs,
            export_handle_slot,
        })
    }

    /// Set the pairing handle (call after PairingMonitor is created)
    pub fn set_pairing_handle(&self, handle: PairingHandle) {
        if let Ok(mut ph) = self.pairing_handle.lock() {
            *ph = Some(handle);
            eprintln!("[MQTT Monitor] Pairing handle set");
        }
    }

    /// Set the LoRaWAN state handle (call after LoRaWANMonitor is created).
    pub fn set_lorawan_state(&self, state: crate::libs::lorawan::SharedLoRaWANState) {
        if let Ok(mut g) = self.lorawan_state_slot.lock() {
            *g = Some(state);
            eprintln!("[MQTT Monitor] LoRaWAN state handle set");
        }
    }

    /// Set the LoRaWAN command handle (call after LoRaWANMonitor is created).
    /// Enables the MQTT sticker config/history commands to drive fPort-85.
    pub fn set_lorawan_handle(&self, handle: crate::libs::lorawan::LoRaWANHandle) {
        if let Ok(mut g) = self.lorawan_handle_slot.lock() {
            *g = Some(handle);
            eprintln!("[MQTT Monitor] LoRaWAN command handle set");
        }
    }

    /// Get handle for sending messages
    pub fn handle(&self) -> MqttHandle {
        self.handle.clone()
    }

    /// Set the mqtt_export handle (call after MqttExportThread is spawned).
    /// Without this, ResetExportCursor commands only reset the persisted
    /// SQLite cursor — the orchestrator's in-memory cache continues to skip
    /// the rows the operator asked to replay until the process restarts.
    pub fn set_export_handle(&self, handle: ExportHandle) {
        if let Ok(mut g) = self.export_handle_slot.lock() {
            *g = Some(handle);
            eprintln!("[MQTT Monitor] Export handle set");
        }
    }

    /// Get connection state
    pub fn connection_state(&self) -> SharedConnectionState {
        self.connection_state.clone()
    }

    /// Spawn a detached task that reads a STICKER's fPort-85 config and publishes
    /// the merged result to `lorawan/sensors/<dev_eui>/config`. Detached so the
    /// MQTT event loop stays responsive while the blocking downlink round-trips
    /// run on a blocking thread.
    /// Run one STICKER control command (#71) and publish its outcome.
    ///
    /// The three commands that cannot be confirmed at Ack time are handled
    /// explicitly rather than left to time out into a false failure:
    ///
    ///   * `sticker_force_send` sends no fPort-85 reply at all
    ///     (`app_cmd.c:699-711`) — the fPort-2 telemetry frame is the answer. It
    ///     goes out fire-and-forget, with no `seq` allocated, so it can never alias
    ///     a pending waiter.
    ///   * `sticker_clock_sync` with no `unix_time` asks the device to re-sync from
    ///     the network, which also produces no immediate reply; the deferred `Info`
    ///     arrives later and is picked up by the unsolicited-Info path from #65.
    ///   * `sticker_reboot` / `sticker_device_reset` answer `Ack` and then restart
    ///     8 s later, so a missing reply is expected rather than a failure.
    fn spawn_sticker_command(
        client: AsyncClient,
        topics: TopicBuilder,
        publish_cfg: crate::libs::config::PublishConfig,
        handle: crate::libs::lorawan::LoRaWANHandle,
        cmd: MqttCommand,
    ) {
        use crate::libs::lorawan::sticker_command as sc;
        use crate::libs::lorawan::sticker_config;
        use crate::libs::lorawan::sticker_proto::Command as ProtoCommand;
        use crate::libs::lorawan::sticker_response::ResponseKind;
        use prost::Message as _;

        tokio::spawn(async move {
            let publisher = MqttPublisher::new(client, topics, &publish_cfg);
            let name = cmd.name().to_string();

            // dev_eui + the proto command + what remains outstanding after the reply.
            let (dev_eui, proto, expect, action_bearing): (
                String,
                ProtoCommand,
                Option<&str>,
                bool,
            ) = match &cmd {
                MqttCommand::StickerReboot { dev_eui } => (
                    dev_eui.clone(),
                    sc::build_reboot(),
                    Some("unsolicited_info_on_rejoin"),
                    true,
                ),
                MqttCommand::StickerDeviceReset { dev_eui } => (
                    dev_eui.clone(),
                    sc::build_device_reset(),
                    Some("unsolicited_info_on_rejoin"),
                    true,
                ),
                MqttCommand::StickerResetCounters {
                    dev_eui,
                    hall_left,
                    hall_right,
                    input_a,
                    input_b,
                } => (
                    dev_eui.clone(),
                    sc::build_reset_counters_selective(*hall_left, *hall_right, *input_a, *input_b),
                    None,
                    true,
                ),
                MqttCommand::StickerForceSend { dev_eui } => (
                    dev_eui.clone(),
                    sc::build_force_send(),
                    Some("telemetry_uplink"),
                    false,
                ),
                MqttCommand::StickerClockSync { dev_eui, unix_time } => match unix_time {
                    Some(t) => (dev_eui.clone(), sc::build_clock_sync(*t), None, false),
                    None => (
                        dev_eui.clone(),
                        sc::build_clock_sync_from_network(),
                        Some("deferred_info"),
                        false,
                    ),
                },
                other => {
                    eprintln!(
                        "[MQTT Monitor] spawn_sticker_command: not a control command: {}",
                        other.name()
                    );
                    return;
                }
            };

            // force_send is unsigned, so broker access alone can trigger uplinks.
            // A sticker's duty cycle is finite, so space them per device.
            if matches!(cmd, MqttCommand::StickerForceSend { .. }) {
                if let Err(reason) = sticker_config::check_force_send_cooldown(&dev_eui) {
                    let msg = MqttMessage::PublishStickerCommandResult {
                        dev_eui,
                        command: name,
                        seq: 0,
                        result: "rate_limited".to_string(),
                        expect: None,
                        detail: Some(reason),
                        fault_key: None,
                    };
                    if let Err(e) = publisher.handle_message(msg).await {
                        eprintln!("[MQTT Monitor] Failed to publish command result: {}", e);
                    }
                    return;
                }
            }

            // Commands that leave a deferred action on the device's single slot must
            // not overlap. Refuse immediately rather than queueing behind a lock.
            let guard = if action_bearing {
                match sticker_config::try_action_guard(&dev_eui) {
                    Ok(g) => Some(g),
                    Err(reason) => {
                        let msg = MqttMessage::PublishStickerCommandResult {
                            dev_eui,
                            command: name,
                            seq: 0,
                            result: "device_busy".to_string(),
                            expect: None,
                            detail: Some(reason),
                            fault_key: None,
                        };
                        if let Err(e) = publisher.handle_message(msg).await {
                            eprintln!("[MQTT Monitor] Failed to publish command result: {}", e);
                        }
                        return;
                    }
                }
            } else {
                None
            };

            // No fPort-85 reply is ever coming for these two, so do not allocate a
            // seq and do not wait for one.
            let no_reply_expected =
                matches!(expect, Some("telemetry_uplink") | Some("deferred_info"));

            let dev_eui_blocking = dev_eui.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let _guard = guard; // held for the duration of the exchange
                if no_reply_expected {
                    handle
                        .send_raw(&dev_eui_blocking, proto.encode_to_vec(), 85)
                        .map(|()| None)
                } else {
                    handle
                        .send_command(
                            &dev_eui_blocking,
                            proto,
                            sticker_command_timeout(&handle, &dev_eui_blocking),
                        )
                        .map(Some)
                }
            })
            .await;

            let (seq, result, detail, fault_key) = match outcome {
                // Fire-and-forget succeeded: honestly "requested", never "ok".
                Ok(Ok(None)) => (0, "requested".to_string(), None, None),
                Ok(Ok(Some(dr))) => match dr.kind {
                    ResponseKind::Ack => (dr.seq, "ok".to_string(), None, None),
                    ResponseKind::Info(_) => (dr.seq, "ok".to_string(), None, None),
                    ResponseKind::Error {
                        code,
                        detail,
                        fault_field,
                    } => (
                        dr.seq,
                        code.to_string(),
                        Some(detail),
                        sc::describe_fault(fault_field, std::iter::empty()),
                    ),
                    other => (dr.seq, "ok".to_string(), Some(format!("{other:?}")), None),
                },
                Ok(Err(e)) => {
                    // A reboot/device_reset that never answers is the expected case:
                    // the device restarts 8 s after the Ack.
                    if expect == Some("unsolicited_info_on_rejoin") {
                        (0, "requested".to_string(), Some(e), None)
                    } else {
                        (0, "transport_error".to_string(), Some(e), None)
                    }
                }
                Err(join_err) => (
                    0,
                    "transport_error".to_string(),
                    Some(join_err.to_string()),
                    None,
                ),
            };

            let msg = MqttMessage::PublishStickerCommandResult {
                dev_eui,
                command: name,
                seq,
                result,
                expect: expect.map(|s| s.to_string()),
                detail,
                fault_key,
            };
            if let Err(e) = publisher.handle_message(msg).await {
                eprintln!(
                    "[MQTT Monitor] Failed to publish sticker command result: {}",
                    e
                );
            }
        });
    }

    /// Answer a `get_sticker_info` query (#65): one `GetInfo` round trip, then
    /// publish the decoded info on the retained `.../info` topic.
    fn spawn_sticker_info_read(
        client: AsyncClient,
        topics: TopicBuilder,
        publish_cfg: crate::libs::config::PublishConfig,
        handle: crate::libs::lorawan::LoRaWANHandle,
        dev_eui: String,
    ) {
        tokio::spawn(async move {
            let dev_eui_blocking = dev_eui.clone();
            let read = tokio::task::spawn_blocking(move || {
                crate::libs::lorawan::sticker_config::read_info(
                    &handle,
                    &dev_eui_blocking,
                    sticker_command_timeout(&handle, &dev_eui_blocking),
                )
            })
            .await;

            let publisher = MqttPublisher::new(client, topics, &publish_cfg);
            match read {
                Ok(Ok((seq, info))) => {
                    let msg = MqttMessage::PublishStickerInfo {
                        info: crate::libs::lorawan::sticker_config::info_to_json(
                            &info,
                            &dev_eui,
                            "query",
                            seq,
                            &crate::libs::mqtt::publisher::MqttPublisher::timestamp(),
                        ),
                        dev_eui,
                    };
                    if let Err(e) = publisher.handle_message(msg).await {
                        eprintln!("[MQTT Monitor] Failed to publish sticker info: {}", e);
                    }
                }
                Ok(Err(e)) => {
                    // Includes the honest 64-byte-buffer overflow case: a sticker
                    // with several latched alarms answers "response too large".
                    // Reported as-is and never retried — the reply would not change.
                    if let Err(pe) = publisher
                        .publish_error("get_sticker_info", "transport", &e)
                        .await
                    {
                        eprintln!(
                            "[MQTT Monitor] Failed to publish sticker info error: {}",
                            pe
                        );
                    }
                }
                Err(join_err) => {
                    eprintln!(
                        "[MQTT Monitor] get_sticker_info task panicked: {}",
                        join_err
                    );
                }
            }
        });
    }

    fn spawn_sticker_config_read(
        client: AsyncClient,
        topics: TopicBuilder,
        publish_cfg: crate::libs::config::PublishConfig,
        handle: crate::libs::lorawan::LoRaWANHandle,
        dev_eui: String,
        keys: Option<Vec<String>>,
    ) {
        tokio::spawn(async move {
            let dev_eui_blocking = dev_eui.clone();
            let key_strings = keys.unwrap_or_default();
            let read = tokio::task::spawn_blocking(move || {
                let key_refs: Vec<&str> = key_strings.iter().map(|s| s.as_str()).collect();
                crate::libs::lorawan::sticker_config::read_config(
                    &handle,
                    &dev_eui_blocking,
                    &key_refs,
                    sticker_command_timeout(&handle, &dev_eui_blocking),
                )
            })
            .await;

            let publisher = MqttPublisher::new(client, topics, &publish_cfg);
            match read {
                Ok(Ok(cfg)) => {
                    // A read that only partly came back is reported as partial, not
                    // as "ok" — the viewer must be able to tell "this key is absent
                    // from the device" from "we never managed to read this key".
                    let last_result = if cfg.is_complete() {
                        "ok".to_string()
                    } else {
                        eprintln!(
                            "[MQTT Monitor] partial sticker config read: {} key(s) not read",
                            cfg.failed_keys.len()
                        );
                        "partial".to_string()
                    };
                    let msg = MqttMessage::PublishStickerConfig {
                        dev_eui,
                        config: crate::libs::lorawan::sticker_config::config_to_json(&cfg.config),
                        page_index: 0,
                        page_count: cfg.page_count,
                        last_seq: cfg.last_seq,
                        last_result,
                    };
                    if let Err(e) = publisher.handle_message(msg).await {
                        eprintln!("[MQTT Monitor] Failed to publish sticker config: {}", e);
                    }
                }
                Ok(Err(e)) => {
                    if let Err(pe) = publisher
                        .publish_error("get_sticker_config", "transport", &e)
                        .await
                    {
                        eprintln!(
                            "[MQTT Monitor] Failed to publish sticker config error: {}",
                            pe
                        );
                    }
                }
                Err(join_err) => {
                    eprintln!(
                        "[MQTT Monitor] get_sticker_config task panicked: {}",
                        join_err
                    );
                }
            }
        });
    }

    /// Spawn a detached task that reads *every* readable STICKER key and
    /// publishes it to `lorawan/sensors/<dev_eui>/full-config`.
    ///
    /// Same engine as the settable read, a wider key list, a different topic. It
    /// is a lot of airtime — `all_readable_keys()` is 37 settable plus 17
    /// read-only, batched six to a chunk, one chunk per Class-A reporting cycle —
    /// so a partial result is the normal case rather than a fault, and is reported
    /// as `partial` with the missing keys named instead of being retried here. A
    /// retry would cost another full pass and produce the same answer if the
    /// device genuinely will not serve those keys.
    fn spawn_sticker_full_config_read(
        client: AsyncClient,
        topics: TopicBuilder,
        publish_cfg: crate::libs::config::PublishConfig,
        handle: crate::libs::lorawan::LoRaWANHandle,
        dev_eui: String,
    ) {
        tokio::spawn(async move {
            let dev_eui_blocking = dev_eui.clone();
            let read = tokio::task::spawn_blocking(move || {
                let keys = crate::libs::lorawan::sticker_command::all_readable_keys();
                crate::libs::lorawan::sticker_config::read_config(
                    &handle,
                    &dev_eui_blocking,
                    &keys,
                    sticker_command_timeout(&handle, &dev_eui_blocking),
                )
            })
            .await;

            let publisher = MqttPublisher::new(client, topics, &publish_cfg);
            match read {
                Ok(Ok(cfg)) => {
                    let read_status = if cfg.is_complete() {
                        "complete".to_string()
                    } else {
                        eprintln!(
                            "[MQTT Monitor] partial sticker full-config read: {} key(s) not read",
                            cfg.failed_keys.len()
                        );
                        "partial".to_string()
                    };
                    eprintln!(
                        "[MQTT Monitor] get_sticker_full_config {}: {}, {} key(s)",
                        dev_eui,
                        read_status,
                        cfg.config.len()
                    );
                    let msg = MqttMessage::PublishStickerFullConfig {
                        dev_eui,
                        config: crate::libs::lorawan::sticker_config::config_to_json(&cfg.config),
                        page_count: cfg.page_count,
                        last_seq: cfg.last_seq,
                        read_status,
                        missing: cfg.failed_keys,
                    };
                    if let Err(e) = publisher.handle_message(msg).await {
                        eprintln!(
                            "[MQTT Monitor] Failed to publish sticker full config: {}",
                            e
                        );
                    }
                }
                Ok(Err(e)) => {
                    if let Err(pe) = publisher
                        .publish_error("get_sticker_full_config", "transport", &e)
                        .await
                    {
                        eprintln!(
                            "[MQTT Monitor] Failed to publish sticker full config error: {}",
                            pe
                        );
                    }
                }
                Err(join_err) => {
                    eprintln!(
                        "[MQTT Monitor] get_sticker_full_config task panicked: {}",
                        join_err
                    );
                }
            }
        });
    }

    /// Spawn a detached task that writes a STICKER's fPort-85 config (validate →
    /// SetParam batches), reads it back (unless save+reboot), and publishes the
    /// result to `lorawan/sensors/<dev_eui>/config` with the last Ack/Error.
    fn spawn_sticker_config_write(
        client: AsyncClient,
        topics: TopicBuilder,
        publish_cfg: crate::libs::config::PublishConfig,
        handle: crate::libs::lorawan::LoRaWANHandle,
        dev_eui: String,
        fields: std::collections::BTreeMap<String, String>,
        save: bool,
    ) {
        tokio::spawn(async move {
            use crate::libs::lorawan::{sticker_command as sc, sticker_config};
            let dev_eui_blocking = dev_eui.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                // Parse string values into typed ConfigValue (fail fast).
                let mut config = std::collections::BTreeMap::new();
                for (k, raw) in &fields {
                    match sc::parse_value(k, raw) {
                        Ok(v) => {
                            config.insert(k.clone(), v);
                        }
                        Err(e) => return Err(format!("{}: {}", e.key, e.reason)),
                    }
                }
                let write = sticker_config::write_config(
                    &handle,
                    &dev_eui_blocking,
                    &config,
                    save,
                    sticker_command_timeout(&handle, &dev_eui_blocking),
                )
                .map_err(|errs| {
                    errs.iter()
                        .map(|e| format!("{}: {}", e.key, e.reason))
                        .collect::<Vec<_>>()
                        .join("; ")
                })?;
                // Read back the staged values, unless we just saved (device reboots).
                let read = if save {
                    None
                } else {
                    sticker_config::read_config(
                        &handle,
                        &dev_eui_blocking,
                        &[],
                        sticker_command_timeout(&handle, &dev_eui_blocking),
                    )
                    .ok()
                };
                Ok((write, read))
            })
            .await;

            let publisher = MqttPublisher::new(client, topics, &publish_cfg);
            match outcome {
                Ok(Ok((write, read))) => {
                    let last_result = write
                        .batches
                        .last()
                        .map(sticker_config::batch_result)
                        .unwrap_or_else(|| "ok".to_string());
                    let (config_json, page_count) = match read {
                        Some(cfg) => (sticker_config::config_to_json(&cfg.config), cfg.page_count),
                        None => (std::collections::BTreeMap::new(), 1),
                    };
                    let msg = MqttMessage::PublishStickerConfig {
                        dev_eui,
                        config: config_json,
                        page_index: 0,
                        page_count,
                        last_seq: write.last_seq,
                        last_result,
                    };
                    if let Err(e) = publisher.handle_message(msg).await {
                        eprintln!("[MQTT Monitor] Failed to publish sticker config: {}", e);
                    }
                }
                Ok(Err(e)) => {
                    if let Err(pe) = publisher
                        .publish_error("set_sticker_config", "transport", &e)
                        .await
                    {
                        eprintln!(
                            "[MQTT Monitor] Failed to publish sticker config error: {}",
                            pe
                        );
                    }
                }
                Err(join_err) => {
                    eprintln!(
                        "[MQTT Monitor] set_sticker_config task panicked: {}",
                        join_err
                    );
                }
            }
        });
    }

    /// Spawn a detached task that requests a STICKER's on-device history and
    /// publishes each returned frame to `lorawan/sensors/<dev_eui>/history`.
    fn spawn_sticker_history_read(
        client: AsyncClient,
        topics: TopicBuilder,
        publish_cfg: crate::libs::config::PublishConfig,
        handle: crate::libs::lorawan::LoRaWANHandle,
        dev_eui: String,
        from_unix: Option<u32>,
        to_unix: Option<u32>,
    ) {
        tokio::spawn(async move {
            use crate::libs::lorawan::sticker_config;
            let dev_eui_blocking = dev_eui.clone();
            let result = tokio::task::spawn_blocking(move || {
                sticker_config::read_history(
                    &handle,
                    &dev_eui_blocking,
                    from_unix,
                    to_unix,
                    sticker_command_timeout(&handle, &dev_eui_blocking),
                )
            })
            .await;

            let publisher = MqttPublisher::new(client, topics, &publish_cfg);
            match result {
                Ok(Ok(hr)) => {
                    if !hr.complete {
                        eprintln!(
                            "[MQTT Monitor] sticker history incomplete for {}: missing frames {:?}",
                            dev_eui, hr.missing_indices
                        );
                    }
                    if hr.pages.is_empty() {
                        // Signal completion-with-no-data so the viewer can stop waiting
                        // (also covers the device reporting history_unavailable).
                        let msg = MqttMessage::PublishStickerHistory {
                            dev_eui: dev_eui.clone(),
                            frame_index: 0,
                            frame_count: 0,
                            records: Vec::new(),
                        };
                        if let Err(e) = publisher.handle_message(msg).await {
                            eprintln!("[MQTT Monitor] Failed to publish sticker history: {}", e);
                        }
                    } else {
                        for page in hr.pages {
                            let records: Vec<serde_json::Value> = page
                                .records
                                .iter()
                                .map(sticker_config::history_record_to_json)
                                .collect();
                            let msg = MqttMessage::PublishStickerHistory {
                                dev_eui: dev_eui.clone(),
                                frame_index: page.frame_index,
                                frame_count: page.frame_count,
                                records,
                            };
                            if let Err(e) = publisher.handle_message(msg).await {
                                eprintln!(
                                    "[MQTT Monitor] Failed to publish sticker history: {}",
                                    e
                                );
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    if let Err(pe) = publisher
                        .publish_error("get_sticker_history", e.stable_code(), &e.to_string())
                        .await
                    {
                        eprintln!(
                            "[MQTT Monitor] Failed to publish sticker history error: {}",
                            pe
                        );
                    }
                }
                Err(join_err) => {
                    eprintln!(
                        "[MQTT Monitor] get_sticker_history task panicked: {}",
                        join_err
                    );
                }
            }
        });
    }

    /// Move the synchronous publish queue onto an awaitable channel.
    ///
    /// The event loop below is a `tokio::select!` whose other arms are periodic
    /// timers. Draining the queue with `crossbeam`'s blocking
    /// `recv_timeout(100ms)` broke every one of them: the call is synchronous
    /// and all of its outcomes fall through, so that arm returned `Ready` on its
    /// first poll every single time. `select!` short-circuits at the first ready
    /// arm and drops the rest, so each freshly-created `sleep(100ms)` was polled
    /// once at elapsed = 0, returned `Pending`, and was dropped — never
    /// re-polled after its deadline, even though the deadline passed while
    /// `recv_timeout` blocked. The periodic arms were unreachable in practice,
    /// which is why `system/info` was never published at all and the Viewer's
    /// power/network/system cards were permanently empty.
    ///
    /// `tokio::sync::mpsc::Receiver::recv` is both awaitable and cancel-safe, so
    /// the arm can now pend and cannot lose a message when another arm wins —
    /// the old code took the message off the queue before awaiting the publish,
    /// and dropped it on the floor if `select!` resolved elsewhere meanwhile.
    ///
    /// The blocking receive still happens, but on a dedicated OS thread where it
    /// costs nothing, rather than stalling a tokio worker for 100 ms per idle
    /// iteration. Bounded at the same capacity so backpressure is unchanged.
    fn spawn_publish_bridge(
        receiver: Receiver<MqttMessage>,
        capacity: usize,
    ) -> tokio::sync::mpsc::Receiver<MqttMessage> {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity.max(1));

        let spawned = thread::Builder::new()
            .name("mqtt-publish-bridge".to_string())
            .spawn(move || {
                // Ok(_) until every MqttHandle sender is dropped; Err ends the
                // thread, which in turn closes `rx` and tells the event loop to
                // shut down.
                while let Ok(msg) = receiver.recv() {
                    // Safe here (and only here): this is a plain std::thread, not
                    // a runtime worker.
                    if tx.blocking_send(msg).is_err() {
                        // Monitor task is gone; nothing left to publish to.
                        break;
                    }
                }
                eprintln!("[MQTT Monitor] Publish bridge stopped");
            });

        if let Err(e) = spawned {
            // Without the bridge nothing can be published at all, so fail loudly
            // rather than limping along with a silent queue.
            panic!("Failed to spawn MQTT publish bridge thread: {}", e);
        }

        rx
    }

    /// Main monitoring loop (runs in background thread)
    fn monitor_loop(
        config: MqttConfig,
        hostname: String,
        app_version: String,
        receiver: Receiver<MqttMessage>,
        shutdown_flag: Arc<AtomicBool>,
        connection_state: SharedConnectionState,
        pairing_handle: SharedPairingHandle,
        power_status: crate::libs::power::status::SharedPowerStatus,
        stm_bridge: Option<SharedStmBridge>,
        screen_brightness: Option<SharedScreenBrightnessHandle>,
        screen_timeout: Option<SharedScreenTimeoutHandle>,
        buzzer_volume: Option<SharedBuzzerVolumeHandle>,
        display_lines: Option<SharedDisplayLinesHandle>,
        buzzer_priority: Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
        reconnected_flag: Arc<AtomicBool>,
        lorawan_state_slot: std::sync::Arc<
            std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>,
        >,
        lorawan_handle_slot: std::sync::Arc<
            std::sync::Mutex<Option<crate::libs::lorawan::LoRaWANHandle>>,
        >,
        lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
        storage_handle: Option<crate::libs::storage::StorageHandle>,
        export_handle_slot: SharedExportHandle,
    ) -> Result<(), String> {
        // Validate and prepare client_id
        let client_id = if config.broker.client_id.is_empty() {
            eprintln!("[MQTT Monitor] Config client_id is empty, using hostname fallback");
            hostname.trim().to_string()
        } else {
            config.broker.client_id.clone()
        };

        eprintln!("[MQTT Monitor] Using client_id: '{}'", client_id);
        eprintln!("[MQTT Monitor] Client_id length: {} bytes", client_id.len());

        // Validate client_id is not empty
        if client_id.is_empty() {
            return Err("Client ID cannot be empty - check hostname configuration".to_string());
        }

        eprintln!("[MQTT Monitor] Connection parameters:");
        eprintln!(
            "[MQTT Monitor]   Broker: {}:{}",
            config.broker.host, config.broker.port
        );
        eprintln!(
            "[MQTT Monitor]   Client ID: {}",
            if config.broker.client_id.is_empty() {
                &hostname
            } else {
                &config.broker.client_id
            }
        );
        eprintln!(
            "[MQTT Monitor]   Keep-alive: {}s",
            config.connection.keep_alive_sec
        );
        eprintln!(
            "[MQTT Monitor]   Clean session: {}",
            config.connection.clean_session
        );
        if config.last_will.enabled {
            eprintln!("[MQTT Monitor]   Last Will: enabled");
        }

        // Log TLS status and warn if disabled (EU MDR Annex I, 17.2)
        match &config.tls {
            Some(tls_config) if tls_config.enabled => {
                eprintln!(
                    "[MQTT Monitor]   TLS: enabled (ca_cert: {})",
                    tls_config.ca_cert_path
                );
            }
            Some(tls_config) if !tls_config.enabled => {
                eprintln!("[MQTT Monitor] WARNING: MQTT TLS is disabled. Data transmitted in plaintext. Not recommended for EU MDR compliance.");
            }
            None => {
                eprintln!("[MQTT Monitor] WARNING: MQTT TLS is not configured. Data transmitted in plaintext. Not recommended for EU MDR compliance.");
            }
            _ => {}
        }

        // Create async runtime for rumqttc (multi-threaded required for AsyncClient)
        eprintln!("[MQTT Monitor] Creating Tokio multi-threaded runtime...");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| {
                eprintln!(
                    "[MQTT Monitor] ERROR: Failed to create tokio runtime: {}",
                    e
                );
                format!("Failed to create tokio runtime: {}", e)
            })?;
        eprintln!("[MQTT Monitor] Tokio runtime created successfully");

        // Initialize components that persist across reconnections
        #[cfg(feature = "dev-platform")]
        let auth_manager: Option<Arc<AuthorizationManager>> = {
            eprintln!("[MQTT Monitor] DEV-PLATFORM: Authorization manager DISABLED");
            None
        };
        #[cfg(not(feature = "dev-platform"))]
        let auth_manager = if config.subscribe.enabled {
            match Self::init_authorization_manager(&config) {
                Ok(manager) => {
                    eprintln!("[MQTT Monitor] Authorization manager initialized");
                    Some(Arc::new(manager))
                }
                Err(e) => {
                    eprintln!(
                        "[MQTT Monitor] Warning: Failed to initialize authorization manager: {}",
                        e
                    );
                    eprintln!("[MQTT Monitor] Signed configuration commands will not be available");
                    None
                }
            }
        } else {
            None
        };

        let config_applier = match ConfigApplier::new_with_storage(
            std::path::Path::new("/data/fiber/config"),
            storage_handle.clone(),
        ) {
            Ok(applier) => {
                eprintln!("[MQTT Monitor] Configuration applier initialized");
                Some(Arc::new(applier))
            }
            Err(e) => {
                eprintln!(
                    "[MQTT Monitor] Warning: Failed to initialize config applier: {}",
                    e
                );
                None
            }
        };

        // Track LED brightness (write-only to STM, so we track it here)
        // Initialize from persisted config if available
        let initial_led_brightness = crate::libs::config::Config::load_default()
            .map(|c| c.system.led_brightness)
            .unwrap_or(50);
        let led_brightness_tracker =
            std::sync::Arc::new(std::sync::atomic::AtomicU8::new(initial_led_brightness));

        // Track connection attempts for logging
        let mut connection_attempt: u32 = 0;

        // Move the publish queue onto an awaitable channel before entering the
        // runtime. See spawn_publish_bridge: the event loop's `select!` cannot
        // work correctly while one of its arms is a blocking crossbeam recv.
        let mut publish_rx = Self::spawn_publish_bridge(receiver, config.publish.max_queue_size);

        runtime.block_on(async {
            // Initialize reconnection state with exponential backoff
            let mut reconnect_state = ReconnectionState::new(
                config.connection.reconnect_delay_sec,
                config.connection.max_reconnect_delay_sec,
            );

            // ========== OUTER LOOP: Client Lifecycle Management ==========
            // This loop creates fresh MQTT clients when needed (on errors, network recovery, etc.)
            'connection: loop {
                // Check for shutdown signal
                if shutdown_flag.load(Ordering::Relaxed) {
                    eprintln!("[MQTT Monitor] Shutdown signal received before connection attempt");
                    break;
                }

                connection_attempt += 1;
                eprintln!("[MQTT Monitor] === Connection attempt #{} ===", connection_attempt);

                // Check if network is available before trying to connect
                let network = get_network_status();
                if !network.wifi_connected && !network.ethernet_connected {
                    eprintln!("[MQTT Monitor] No network available - waiting for network...");
                    // Wait for network with timeout
                    match tokio::task::spawn_blocking(|| wait_for_network(60)).await {
                        Ok(true) => {
                            eprintln!("[MQTT Monitor] Network is now available");
                        }
                        Ok(false) => {
                            eprintln!("[MQTT Monitor] Network still unavailable, will retry...");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue 'connection;
                        }
                        Err(e) => {
                            eprintln!("[MQTT Monitor] Error waiting for network: {}", e);
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue 'connection;
                        }
                    }
                }

                // Update connection state
                if let Ok(mut state) = connection_state.lock() {
                    state.set_state(ConnectionState::Connecting);
                }

                // Create fresh MQTT client options and client.
                //
                // The request channel used to hold 10. That is far too small for this
                // publish rate — the export streams alone put out one message per stream
                // per aggregation tick — so a burst filled it and `client.publish()`
                // returned "Failed to send mqtt requests to eventloop" and the message was
                // simply lost. Measured on fiber-ce3d59f8: 6 drops in 30 minutes, on
                // .../command, .../config and — the one that actually cost something —
                // .../history, where losing the frame stalled a backfill job until the
                // reconciler retried it.
                //
                // A deeper queue costs a little memory and turns a silent drop into a
                // short wait, which is the right trade for a device whose publishes carry
                // alarm and history data.
                let mqttoptions = create_mqtt_options(&config, &hostname, &client_id);
                let (client, mut eventloop) = AsyncClient::new(mqttoptions, MQTT_REQUEST_CHANNEL_CAPACITY);

                eprintln!("[MQTT Monitor] Fresh MQTT client created, waiting for CONNACK...");

                // Wait for initial CONNACK (with timeout)
                let connection_timeout = Duration::from_secs(config.connection.connection_timeout_sec);
                let connection_start = Instant::now();
                let mut connected = false;

                while connection_start.elapsed() < connection_timeout {
                    if shutdown_flag.load(Ordering::Relaxed) {
                        eprintln!("[MQTT Monitor] Shutdown during connection attempt");
                        break 'connection;
                    }

                    match tokio::time::timeout(Duration::from_secs(1), eventloop.poll()).await {
                        Ok(Ok(Event::Incoming(Incoming::ConnAck(connack)))) => {
                            eprintln!("[MQTT Monitor] ✓ CONNACK received - connected to broker");
                            eprintln!("[MQTT Monitor]   Session present: {}", connack.session_present);
                            connected = true;
                            reconnect_state.reset();
                            break;
                        }
                        Ok(Err(e)) => {
                            eprintln!("[MQTT Monitor] Connection error during connect: {}", e);
                            let delay = reconnect_state.calculate_delay();
                            tokio::time::sleep(delay).await;
                            continue 'connection;
                        }
                        Ok(Ok(_)) => {
                            // Other event, continue polling
                        }
                        Err(_) => {
                            // Timeout, continue waiting
                            eprintln!("[MQTT Monitor] Still waiting for CONNACK...");
                        }
                    }
                }

                if !connected {
                    eprintln!("[MQTT Monitor] Connection timeout after {}s", config.connection.connection_timeout_sec);
                    let delay = reconnect_state.calculate_delay();
                    tokio::time::sleep(delay).await;
                    continue 'connection;
                }

                // Update connection state
                if let Ok(mut state) = connection_state.lock() {
                    if connection_attempt > 1 {
                        state.record_reconnection();
                    }
                    state.set_state(ConnectionState::Connected);
                }

                // Create topic builder
                let topics = TopicBuilder::new(
                    config.publish.topic_prefix.clone(),
                    hostname.clone(),
                    config.publish.include_hostname,
                );

                // Create publisher with fresh client
                let publisher = MqttPublisher::new(client.clone(), topics.clone(), &config.publish);

                // Create subscriber
                let mut subscriber = MqttSubscriber::new(
                    config.subscribe.max_commands_per_second,
                    config.subscribe.audit_enabled,
                );

                // Subscribe to command topics
                if config.subscribe.enabled {
                    let cmd_topic = topics.commands_wildcard();
                    eprintln!("[MQTT Monitor] Subscribing to commands: {}", cmd_topic);
                    if let Err(e) = client.subscribe(&cmd_topic, QoS::AtLeastOnce).await {
                        eprintln!("[MQTT Monitor] Warning: Failed to subscribe to commands: {}", e);
                    }

                    // Subscribe to pairing request topic
                    let pair_topic = topics.pair_request();
                    eprintln!("[MQTT Monitor] Subscribing to pairing: {}", pair_topic);
                    if let Err(e) = client.subscribe(&pair_topic, QoS::ExactlyOnce).await {
                        eprintln!("[MQTT Monitor] Warning: Failed to subscribe to pairing: {}", e);
                    }
                }

                // Publish online status
                if let Err(e) = publisher.publish_online_status().await {
                    eprintln!("[MQTT Monitor] Failed to publish online status: {}", e);
                }

                // Publish current config state so viewer gets actual values
                let led_br = led_brightness_tracker.load(std::sync::atomic::Ordering::Relaxed);
                if let Some(config_msg) = Self::build_config_state_message(&screen_brightness, &screen_timeout, &buzzer_volume, led_br) {
                    if let Err(e) = publisher.handle_message(config_msg).await {
                        eprintln!("[MQTT Monitor] Failed to publish config state: {}", e);
                    } else {
                        eprintln!("[MQTT Monitor] Published initial config state");
                    }
                }

                eprintln!("[MQTT Monitor] Connection established, entering event loop");

                // Periodic jobs for this connection.
                //
                // These are real timers rather than `sleep(100ms)` polls inside
                // the select arms. The old shape could not fire at all — see
                // spawn_publish_bridge for the full explanation — which is why
                // system/info was never published and network changes were never
                // noticed. `MissedTickBehavior::Delay` keeps a stalled loop from
                // firing a catch-up burst; the first tick of each interval
                // completes immediately, so a fresh connection reports its status
                // straight away instead of one interval later.
                let interval_of = |period: Duration| {
                    let mut iv = tokio::time::interval(period);
                    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    iv
                };

                // Deliberately still 5s, not config.publish.intervals.network_sec
                // (which defaults to 30). That field was only ever echoed into
                // config/state — no network publish loop has ever read it — so
                // adopting it here would quietly make disconnect alarms six times
                // slower. Repointing it is a separate decision.
                let mut network_tick = interval_of(Duration::from_secs(5));
                // Honour the configured cadence instead of a hardcoded 60s. The
                // value was already parsed and echoed into config/state, but the
                // publish loop never read it, so set_system_info_interval could
                // not take effect.
                //
                // The tick is a fixed 100 ms poll and the cadence is enforced by
                // `last_status_publish` below, rather than being the interval's own
                // period. Two things need that: a `set_system_info_interval` change
                // has to take effect without waiting out the old period (let alone a
                // reconnect), and a cluster arm has to be able to ask for one
                // immediately — and an already-created `Interval` cannot be reset
                // from a different `select!` arm, because `status_tick.tick()` holds
                // it borrowed for the whole statement.
                let mut status_interval = Duration::from_secs(
                    config.publish.intervals.system_info_sec.max(1),
                );
                let mut status_tick = interval_of(Duration::from_millis(100));
                // Publish once as soon as the loop starts, as an immediate interval
                // used to.
                let mut last_status_publish = Instant::now() - status_interval;
                let mut challenge_cleanup_tick = interval_of(Duration::from_secs(30));
                let mut pairing_poll_tick = interval_of(Duration::from_millis(100));

                let mut last_known_network = get_network_status();
                let app_start_time = Instant::now();
                let firmware_version = app_version.clone();

                // ========== INNER LOOP: Event Processing ==========
                // This loop handles MQTT events until an error requires client recreation
                loop {
                    // Check for shutdown signal
                    if shutdown_flag.load(Ordering::Relaxed) {
                        eprintln!("[MQTT Monitor] Shutdown signal received");
                        break 'connection;
                    }

                    // Use tokio::select! to handle both MQTT events and channel messages
                    tokio::select! {
                    // Handle MQTT broker events
                    event = eventloop.poll() => {
                        match event {
                            Ok(Event::Incoming(Incoming::ConnAck(connack))) => {
                                eprintln!("[MQTT Monitor] ✓ Connected to broker (rumqttc auto-reconnect)");
                                eprintln!("[MQTT Monitor]   Session present: {}", connack.session_present);
                                eprintln!("[MQTT Monitor]   Connection time: {}",
                                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"));

                                // Reset backoff on successful connection
                                reconnect_state.reset();

                                if let Ok(mut state) = connection_state.lock() {
                                    let stats = state.stats();
                                    if stats.reconnection_count > 0 {
                                        eprintln!("[MQTT Monitor]   Reconnection #{}", stats.reconnection_count);
                                        if let Some(disconnect_time) = stats.last_disconnected_time {
                                            let now = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap()
                                                .as_secs();
                                            let disconnect_duration = now.saturating_sub(disconnect_time);
                                            eprintln!("[MQTT Monitor]   Was disconnected for: {}s", disconnect_duration);
                                        }
                                    } else {
                                        eprintln!("[MQTT Monitor]   Initial connection successful");
                                    }
                                    state.record_reconnection();
                                    state.set_state(ConnectionState::Connected);
                                }

                                // Signal sensor monitor to flush buffered data immediately
                                reconnected_flag.store(true, Ordering::Release);
                                eprintln!("[MQTT Monitor] Reconnect flag set - sensor monitor will flush buffered data");

                                // Re-subscribe to command topics (required after reconnection)
                                // Always re-subscribe as the broker may have lost our subscriptions
                                if config.subscribe.enabled {
                                    let cmd_topic = topics.commands_wildcard();
                                    eprintln!("[MQTT Monitor] Re-subscribing to commands: {}", cmd_topic);
                                    if let Err(e) = client.subscribe(&cmd_topic, QoS::AtLeastOnce).await {
                                        eprintln!("[MQTT Monitor] Warning: Failed to re-subscribe to commands: {}", e);
                                    } else {
                                        eprintln!("[MQTT Monitor] ✓ Re-subscribed to commands successfully");
                                    }

                                    // Re-subscribe to pairing topic
                                    let pair_topic = topics.pair_request();
                                    eprintln!("[MQTT Monitor] Re-subscribing to pairing: {}", pair_topic);
                                    if let Err(e) = client.subscribe(&pair_topic, QoS::ExactlyOnce).await {
                                        eprintln!("[MQTT Monitor] Warning: Failed to re-subscribe to pairing: {}", e);
                                    } else {
                                        eprintln!("[MQTT Monitor] ✓ Re-subscribed to pairing successfully");
                                    }
                                }

                                // Publish online status
                                if let Err(e) = publisher.publish_online_status().await {
                                    eprintln!("[MQTT Monitor] Failed to publish online status: {}", e);
                                }
                            }

                            Ok(Event::Incoming(Incoming::Disconnect)) => {
                                eprintln!("[MQTT Monitor] ✗ DISCONNECT received from broker");
                                eprintln!("[MQTT Monitor]   Time: {}",
                                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"));
                                if let Ok(mut state) = connection_state.lock() {
                                    state.record_disconnect("Broker sent DISCONNECT".to_string());
                                    state.set_state(ConnectionState::Disconnected);
                                }
                            }

                            Ok(Event::Incoming(Incoming::Publish(p))) => {
                                // Handle incoming messages
                                if config.subscribe.enabled {
                                    // Check if this is a pairing request (different format from commands)
                                    let pair_topic = topics.pair_request();
                                    if p.topic == pair_topic {
                                        // Parse pairing request
                                        match Self::parse_pairing_request(&p.payload) {
                                            Ok(pairing_req) => {
                                                eprintln!("[MQTT Monitor] Received pairing request: {} from {}",
                                                    pairing_req.request_id, pairing_req.admin_username);

                                                // Route to PairingMonitor
                                                if let Ok(ph_guard) = pairing_handle.lock() {
                                                    if let Some(ref ph) = *ph_guard {
                                                        ph.process_request(pairing_req);
                                                        eprintln!("[MQTT Monitor] Pairing request routed to PairingMonitor");
                                                    } else {
                                                        eprintln!("[MQTT Monitor] Pairing handle not set - cannot process request");
                                                        if let Err(publish_err) = publisher.publish_error("pairing_request", "not_available", "Pairing not initialized").await {
                                                            eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                eprintln!("[MQTT Monitor] Invalid pairing request: {}", e);
                                                if let Err(publish_err) = publisher.publish_error("pairing_request", "parse_error", &e).await {
                                                    eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                }
                                            }
                                        }
                                        continue;
                                    }

                                    // Handle regular commands
                                    match subscriber.parse_command(&p.topic, &p.payload) {
                                        Ok(cmd) => {
                                            eprintln!("[MQTT Monitor] Received command: {}", cmd.name());

                                            // Route command to appropriate handler
                                            match cmd {
                                                MqttCommand::ConfigRequest {
                                                    request_id,
                                                    command_type,
                                                    params,
                                                    reason,
                                                    signer_id,
                                                    signature,
                                                    timestamp,
                                                    nonce,
                                                    certificate,
                                                } => {
                                                    #[cfg(feature = "dev-platform")]
                                                    {
                                                        // DEV-PLATFORM: Skip signature verification,
                                                        // directly execute the command without challenge-response
                                                        eprintln!("[MQTT Monitor] DEV-PLATFORM: Bypassing auth for {} from {}",
                                                            command_type, signer_id);

                                                        // Build command directly from params
                                                        let direct_cmd = Self::build_dev_command(&command_type, &params, &reason);
                                                        match direct_cmd {
                                                            Ok(execute_cmd) => {
                                                                if let Err(e) = Self::execute_resolved_command(
                                                                    execute_cmd,
                                                                    &config_applier,
                                                                    &stm_bridge,
                                                                    &screen_brightness,
                                                                    &screen_timeout,
                                                                    &buzzer_volume,
                                                                    &display_lines,
                                                                    &buzzer_priority,
                                                                    &led_brightness_tracker,
                                                                    &lorawan_state_slot,
                                                                    &lorawan_configs,
                                                                    &storage_handle,
                                                                    &lorawan_handle_slot,
                                                                    &client,
                                                                    &topics,
                                                                    &config.publish,
                                                                    &export_handle_slot,
                                                                ) {
                                                                    eprintln!("[MQTT Monitor] DEV-PLATFORM: Command failed: {}", e);
                                                                    if let Err(publish_err) = publisher.publish_error(
                                                                        &command_type, "execution_failed", &format!("{}", e),
                                                                    ).await {
                                                                        eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                                    }
                                                                } else {
                                                                    eprintln!("[MQTT Monitor] DEV-PLATFORM: {} executed successfully", command_type);
                                                                    // Publish success response
                                                                    let applied_at = std::time::SystemTime::now()
                                                                        .duration_since(std::time::UNIX_EPOCH)
                                                                        .unwrap_or_default()
                                                                        .as_secs() as i64;
                                                                    let response = MqttMessage::PublishConfigResponse {
                                                                        challenge_id: "dev-platform".to_string(),
                                                                        request_id: request_id.clone(),
                                                                        status: "SUCCESS".to_string(),
                                                                        applied_at: Some(applied_at),
                                                                        effective_at: Some(applied_at),
                                                                        message: format!("DEV-PLATFORM: {} applied (no auth)", command_type),
                                                                    };
                                                                    if let Err(e) = publisher.handle_message(response).await {
                                                                        eprintln!("[MQTT Monitor] Failed to publish response: {}", e);
                                                                    }
                                                                    // Publish updated config state
                                                                    let led_br = led_brightness_tracker.load(std::sync::atomic::Ordering::Relaxed);
                                                                    if let Some(config_msg) = Self::build_config_state_message(&screen_brightness, &screen_timeout, &buzzer_volume, led_br) {
                                                                        if let Err(e) = publisher.handle_message(config_msg).await {
                                                                            eprintln!("[MQTT Monitor] Failed to publish config state: {}", e);
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                eprintln!("[MQTT Monitor] DEV-PLATFORM: Invalid command: {}", e);
                                                                if let Err(publish_err) = publisher.publish_error(
                                                                    &command_type, "invalid_command", &e,
                                                                ).await {
                                                                    eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                                }
                                                            }
                                                        }
                                                    }
                                                    #[cfg(not(feature = "dev-platform"))]
                                                    {
                                                    if let Some(ref auth) = auth_manager {
                                                        match auth.process_config_request(
                                                            request_id,
                                                            command_type,
                                                            params,
                                                            reason,
                                                            signer_id,
                                                            signature,
                                                            timestamp,
                                                            nonce,
                                                            &certificate,
                                                        ) {
                                                            Ok(challenge_msg) => {
                                                                // Publish challenge
                                                                if let Err(e) = publisher.handle_message(challenge_msg).await {
                                                                    eprintln!("[MQTT Monitor] Failed to publish challenge: {}", e);
                                                                }
                                                            }
                                                            Err(e) => {
                                                                eprintln!("[MQTT Monitor] ConfigRequest rejected: {}", e);
                                                                if let Err(publish_err) = publisher.publish_error(
                                                                    "config_request",
                                                                    "authorization_failed",
                                                                    &format!("{}", e),
                                                                ).await {
                                                                    eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                                }
                                                            }
                                                        }
                                                    } else {
                                                        eprintln!("[MQTT Monitor] ConfigRequest received but authorization is disabled");
                                                    }
                                                    }
                                                }

                                                MqttCommand::ConfigConfirm {
                                                    challenge_id,
                                                    confirmation,
                                                    signer_id,
                                                    signature,
                                                    timestamp,
                                                    nonce,
                                                    certificate,
                                                } => {
                                                    #[cfg(feature = "dev-platform")]
                                                    {
                                                        eprintln!("[MQTT Monitor] DEV-PLATFORM: ConfigConfirm ignored (no challenge-response needed)");
                                                    }
                                                    #[cfg(not(feature = "dev-platform"))]
                                                    {
                                                    if let Some(ref auth) = auth_manager {
                                                        match auth.process_config_confirm(
                                                            challenge_id,
                                                            confirmation,
                                                            signer_id,
                                                            signature,
                                                            timestamp,
                                                            nonce,
                                                            &certificate,
                                                        ) {
                                                            Ok((response_msg, maybe_command)) => {
                                                                // Execute FIRST, then report the real result.
                                                                // Previously SUCCESS was published before running the
                                                                // command and execution errors were only logged, so the
                                                                // client always saw SUCCESS even when the command failed.
                                                                if let Some(execute_cmd) = maybe_command {
                                                                    // Teardown commands (reboot / network reconfig) tear down
                                                                    // the process or the MQTT-bearing interface, which races the
                                                                    // response publish — so for those publish SUCCESS FIRST (M3).
                                                                    let teardown = Self::is_teardown_command(&execute_cmd);
                                                                    if teardown {
                                                                        if let Err(e) = publisher.handle_message(response_msg.clone()).await {
                                                                            eprintln!("[MQTT Monitor] Failed to publish response: {}", e);
                                                                        }
                                                                    }
                                                                    let exec = Self::execute_resolved_command(
                                                                        execute_cmd,
                                                                        &config_applier,
                                                                        &stm_bridge,
                                                                        &screen_brightness,
                                                                        &screen_timeout,
                                                                        &buzzer_volume,
                                                                        &display_lines,
                                                                        &buzzer_priority,
                                                                        &led_brightness_tracker,
                                                                        &lorawan_state_slot,
                                                                        &lorawan_configs,
                                                                        &storage_handle,
                                                                        &lorawan_handle_slot,
                                                                        &client,
                                                                        &topics,
                                                                        &config.publish,
                                                                        &export_handle_slot,
                                                                    );
                                                                    let succeeded = exec.is_ok();
                                                                    if let Err(ref e) = exec {
                                                                        eprintln!("[MQTT Monitor] Failed to execute command: {}", e);
                                                                    }
                                                                    if !teardown {
                                                                        // Execute-first: report the real SUCCESS/ERROR result.
                                                                        let response = Self::confirm_response_message(exec, response_msg);
                                                                        if let Err(e) = publisher.handle_message(response).await {
                                                                            eprintln!("[MQTT Monitor] Failed to publish response: {}", e);
                                                                        }
                                                                        if succeeded {
                                                                            // Publish updated config state after successful command
                                                                            let led_br = led_brightness_tracker.load(std::sync::atomic::Ordering::Relaxed);
                                                                            if let Some(config_msg) = Self::build_config_state_message(&screen_brightness, &screen_timeout, &buzzer_volume, led_br) {
                                                                                if let Err(e) = publisher.handle_message(config_msg).await {
                                                                                    eprintln!("[MQTT Monitor] Failed to publish config state: {}", e);
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                } else {
                                                                    // REJECTED (no command to run): publish the response as-is.
                                                                    if let Err(e) = publisher.handle_message(response_msg).await {
                                                                        eprintln!("[MQTT Monitor] Failed to publish response: {}", e);
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                eprintln!("[MQTT Monitor] ConfigConfirm rejected: {}", e);
                                                                if let Err(publish_err) = publisher.publish_error(
                                                                    "config_confirm",
                                                                    "authorization_failed",
                                                                    &format!("{}", e),
                                                                ).await {
                                                                    eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                                }
                                                            }
                                                        }
                                                    } else {
                                                        eprintln!("[MQTT Monitor] ConfigConfirm received but authorization is disabled");
                                                    }
                                                    }
                                                }

                                                MqttCommand::GetSensorConfig => {
                                                    // Load sensor configuration
                                                    match crate::libs::config::SensorFileConfig::load_default() {
                                                        Ok(sensor_config) => {
                                                            let mut sensors = Vec::new();

                                                            for line in 0..8 {
                                                                let line_config = sensor_config.lines.iter()
                                                                    .find(|l| l.line == line);

                                                                if let Some(lc) = line_config {
                                                                    let thresholds = sensor_config.get_line_thresholds(line);
                                                                    let has_override = lc.critical_low_celsius.is_some()
                                                                        || lc.low_alarm_celsius.is_some()
                                                                        || lc.warning_low_celsius.is_some()
                                                                        || lc.warning_high_celsius.is_some()
                                                                        || lc.high_alarm_celsius.is_some()
                                                                        || lc.critical_high_celsius.is_some();

                                                                    sensors.push(super::messages::SensorConfigData {
                                                                        line,
                                                                        name: lc.name.clone(),
                                                                        location: lc.location.clone(),
                                                                        enabled: lc.enabled,
                                                                        has_override,
                                                                        thresholds,
                                                                    });
                                                                }
                                                            }

                                                            let response = MqttMessage::PublishSensorConfig {
                                                                sensors,
                                                            };

                                                            if let Err(e) = publisher.handle_message(response).await {
                                                                eprintln!("[MQTT Monitor] Failed to publish sensor config: {}", e);
                                                            }
                                                        }
                                                        Err(e) => {
                                                            eprintln!("[MQTT Monitor] Failed to load sensor config: {}", e);
                                                            if let Err(publish_err) = publisher.publish_error(
                                                                "get_sensor_config",
                                                                "config_load_error",
                                                                &format!("Failed to load configuration: {}", e),
                                                            ).await {
                                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                            }
                                                        }
                                                    }
                                                }

                                                MqttCommand::GetInterval => {
                                                    // Load main configuration for intervals
                                                    match crate::libs::config::Config::load_default() {
                                                        Ok(main_config) => {
                                                            let response = MqttMessage::PublishIntervalConfig {
                                                                sample_interval_ms: main_config.sensors.sample_interval_ms,
                                                                aggregation_interval_ms: main_config.sensors.aggregation_interval_ms,
                                                                report_interval_ms: main_config.sensors.report_interval_ms,
                                                            };

                                                            if let Err(e) = publisher.handle_message(response).await {
                                                                eprintln!("[MQTT Monitor] Failed to publish interval config: {}", e);
                                                            }
                                                        }
                                                        Err(e) => {
                                                            eprintln!("[MQTT Monitor] Failed to load main config: {}", e);
                                                            if let Err(publish_err) = publisher.publish_error(
                                                                "get_interval",
                                                                "config_load_error",
                                                                &format!("Failed to load configuration: {}", e),
                                                            ).await {
                                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                            }
                                                        }
                                                    }
                                                }

                                                MqttCommand::SilenceBuzzer => {
                                                    if let Some(bp) = &buzzer_priority {
                                                        bp.silence();
                                                        eprintln!("[MQTT Monitor] ✓ Buzzer silenced by alarm ACK");
                                                    }
                                                }

                                                MqttCommand::HistoryRequest { request_id, sensor_line, from_ts, to_ts } => {
                                                    // Out-of-band replay: spawn a task that pages
                                                    // through sensor_readings_minute and publishes
                                                    // each row on export/probe_1m_replay/... — the
                                                    // natural drain cursor is NOT touched.
                                                    match crate::libs::config::Config::load_default() {
                                                        Ok(main_config) => {
                                                            let client_for_replay = client.clone();
                                                            let topics_for_replay = topics.clone();
                                                            let db_path = main_config.storage.db_path.clone();
                                                            let max_size_gb = main_config.storage.max_size_gb;
                                                            eprintln!(
                                                                "[MQTT Monitor] history_request {} [{}, {}] line={:?}",
                                                                request_id, from_ts, to_ts, sensor_line,
                                                            );
                                                            tokio::spawn(async move {
                                                                let outcome = crate::libs::mqtt_export::replay::replay_history(
                                                                    client_for_replay,
                                                                    topics_for_replay,
                                                                    db_path,
                                                                    max_size_gb,
                                                                    request_id.clone(),
                                                                    sensor_line,
                                                                    from_ts,
                                                                    to_ts,
                                                                )
                                                                .await;
                                                                if outcome.status == "complete" {
                                                                    eprintln!(
                                                                        "[MQTT Monitor] history_request {} done: {} rows",
                                                                        request_id, outcome.rows_sent,
                                                                    );
                                                                } else {
                                                                    eprintln!(
                                                                        "[MQTT Monitor] history_request {} {} after {} rows: {:?}",
                                                                        request_id, outcome.status, outcome.rows_sent, outcome.error,
                                                                    );
                                                                }
                                                            });
                                                        }
                                                        Err(e) => {
                                                            eprintln!("[MQTT Monitor] history_request: failed to load main config: {}", e);
                                                            if let Err(publish_err) = publisher.publish_error(
                                                                "history_request",
                                                                "config_load_error",
                                                                &format!("Failed to load configuration: {}", e),
                                                            ).await {
                                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                                            }
                                                        }
                                                    }
                                                }

                                                // Unsigned control command (#71).
                                                // Rate-limited per device below.
                                                MqttCommand::StickerForceSend { ref dev_eui } => {
                                                    let dev_eui = dev_eui.clone();
                                                    match lorawan_handle_slot
                                                        .lock()
                                                        .ok()
                                                        .and_then(|g| g.clone())
                                                    {
                                                        Some(lr_handle) => {
                                                            Self::spawn_sticker_command(
                                                                client.clone(),
                                                                topics.clone(),
                                                                config.publish.clone(),
                                                                lr_handle,
                                                                MqttCommand::StickerForceSend { dev_eui },
                                                            );
                                                        }
                                                        None => {
                                                            if let Err(pe) = publisher
                                                                .publish_error(
                                                                    "sticker_force_send",
                                                                    "lorawan_unavailable",
                                                                    "LoRaWAN command handle not available",
                                                                )
                                                                .await
                                                            {
                                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", pe);
                                                            }
                                                        }
                                                    }
                                                }
                                                MqttCommand::GetStickerInfo { dev_eui } => {
                                                    match lorawan_handle_slot
                                                        .lock()
                                                        .ok()
                                                        .and_then(|g| g.clone())
                                                    {
                                                        Some(lr_handle) => {
                                                            Self::spawn_sticker_info_read(
                                                                client.clone(),
                                                                topics.clone(),
                                                                config.publish.clone(),
                                                                lr_handle,
                                                                dev_eui,
                                                            );
                                                        }
                                                        None => {
                                                            if let Err(pe) = publisher
                                                                .publish_error(
                                                                    "get_sticker_info",
                                                                    "lorawan_unavailable",
                                                                    "LoRaWAN command handle not available",
                                                                )
                                                                .await
                                                            {
                                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", pe);
                                                            }
                                                        }
                                                    }
                                                }
                                                MqttCommand::GetStickerConfig { dev_eui, keys } => {
                                                    match lorawan_handle_slot
                                                        .lock()
                                                        .ok()
                                                        .and_then(|g| g.clone())
                                                    {
                                                        Some(lr_handle) => {
                                                            Self::spawn_sticker_config_read(
                                                                client.clone(),
                                                                topics.clone(),
                                                                config.publish.clone(),
                                                                lr_handle,
                                                                dev_eui,
                                                                keys,
                                                            );
                                                        }
                                                        None => {
                                                            if let Err(pe) = publisher
                                                                .publish_error(
                                                                    "get_sticker_config",
                                                                    "lorawan_unavailable",
                                                                    "LoRaWAN command handle not available",
                                                                )
                                                                .await
                                                            {
                                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", pe);
                                                            }
                                                        }
                                                    }
                                                }

                                                MqttCommand::GetStickerFullConfig { dev_eui } => {
                                                    match lorawan_handle_slot
                                                        .lock()
                                                        .ok()
                                                        .and_then(|g| g.clone())
                                                    {
                                                        Some(lr_handle) => {
                                                            Self::spawn_sticker_full_config_read(
                                                                client.clone(),
                                                                topics.clone(),
                                                                config.publish.clone(),
                                                                lr_handle,
                                                                dev_eui,
                                                            );
                                                        }
                                                        None => {
                                                            if let Err(pe) = publisher
                                                                .publish_error(
                                                                    "get_sticker_full_config",
                                                                    "lorawan_unavailable",
                                                                    "LoRaWAN command handle not available",
                                                                )
                                                                .await
                                                            {
                                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", pe);
                                                            }
                                                        }
                                                    }
                                                }

                                                MqttCommand::GetStickerHistory { dev_eui, from_unix, to_unix } => {
                                                    match lorawan_handle_slot
                                                        .lock()
                                                        .ok()
                                                        .and_then(|g| g.clone())
                                                    {
                                                        Some(lr_handle) => {
                                                            Self::spawn_sticker_history_read(
                                                                client.clone(),
                                                                topics.clone(),
                                                                config.publish.clone(),
                                                                lr_handle,
                                                                dev_eui,
                                                                from_unix,
                                                                to_unix,
                                                            );
                                                        }
                                                        None => {
                                                            if let Err(pe) = publisher
                                                                .publish_error(
                                                                    "get_sticker_history",
                                                                    "lorawan_unavailable",
                                                                    "LoRaWAN command handle not available",
                                                                )
                                                                .await
                                                            {
                                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", pe);
                                                            }
                                                        }
                                                    }
                                                }

                                                _ => {
                                                    // Other commands - TODO: route to appropriate handlers
                                                    eprintln!("[MQTT Monitor] Command received but no handler implemented yet");
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            eprintln!("[MQTT Monitor] Invalid command: {}", e);
                                            if let Err(publish_err) = publisher.publish_error("unknown", "parse_error", &e).await {
                                                eprintln!("[MQTT Monitor] Failed to publish error: {}", publish_err);
                                            }
                                        }
                                    }
                                }
                            }

                            Err(e) => {
                                let category = categorize_error(&e);
                                eprintln!("[MQTT Monitor] ✗✗ CONNECTION ERROR");
                                eprintln!("[MQTT Monitor]   Error type: {:?}", category);
                                eprintln!("[MQTT Monitor]   Error message: {}", e);
                                eprintln!("[MQTT Monitor]   Time: {}",
                                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"));

                                // Record disconnect with reason
                                if let Ok(mut state) = connection_state.lock() {
                                    state.record_disconnect(format!("{}: {}", category, e));
                                    state.set_state(ConnectionState::Error);
                                }

                                // Check if network is down
                                let network = get_network_status();
                                eprintln!("[MQTT Monitor]   Network status: WiFi={}, Ethernet={}",
                                    network.wifi_connected, network.ethernet_connected);

                                if !network.wifi_connected && !network.ethernet_connected {
                                    eprintln!("[MQTT Monitor] Network down - waiting for network...");
                                    // Wait for network to return
                                    match tokio::task::spawn_blocking(|| wait_for_network(60)).await {
                                        Ok(true) => {
                                            eprintln!("[MQTT Monitor] Network is now available - will create fresh connection");
                                            // Reset backoff since we're starting fresh after network recovery
                                            reconnect_state.reset();
                                        }
                                        Ok(false) => {
                                            eprintln!("[MQTT Monitor] Network still unavailable after 60s");
                                        }
                                        Err(e) => {
                                            eprintln!("[MQTT Monitor] Error waiting for network: {}", e);
                                        }
                                    }
                                } else {
                                    // Network is up but MQTT failed - check broker reachability for diagnostics
                                    let broker_host = config.broker.host.clone();
                                    let broker_port = config.broker.port;
                                    tokio::task::spawn_blocking(move || {
                                        check_broker_reachable(&broker_host, broker_port)
                                    }).await.ok();

                                    // Apply backoff delay before recreating client
                                    let delay = reconnect_state.calculate_delay();
                                    tokio::time::sleep(delay).await;
                                }

                                // Break to outer loop to create fresh MQTT client
                                eprintln!("[MQTT Monitor] Breaking out to recreate MQTT client...");
                                continue 'connection;
                            }

                            _ => {}
                        }
                    }

                    // Handle messages from channel. `recv` is cancel-safe, so a
                    // message is never consumed unless this arm is the one that
                    // wins the select.
                    maybe_msg = publish_rx.recv() => {
                        match maybe_msg {
                            Some(MqttMessage::Shutdown) => {
                                eprintln!("[MQTT Monitor] Shutdown message received");
                                shutdown_flag.store(true, Ordering::Relaxed);
                            }
                            Some(msg) => {
                                // Publish message
                                if let Err(e) = publisher.handle_message(msg).await {
                                    eprintln!("[MQTT Monitor] Failed to publish message: {}", e);
                                } else {
                                    // Record successful publish
                                    if let Ok(mut state) = connection_state.lock() {
                                        state.record_publish();
                                    }
                                }
                            }
                            None => {
                                eprintln!("[MQTT Monitor] Channel disconnected");
                                shutdown_flag.store(true, Ordering::Relaxed);
                            }
                        }
                    }

                    // Network status monitoring
                    _ = network_tick.tick() => {
                        {
                            let current_network = get_network_status();

                            // Detect network changes
                            let network_changed =
                                current_network.wifi_connected != last_known_network.wifi_connected ||
                                current_network.ethernet_connected != last_known_network.ethernet_connected;

                            if network_changed {
                                eprintln!("[MQTT Monitor] === NETWORK STATUS CHANGED ===");
                                eprintln!("[MQTT Monitor]   WiFi: {} -> {}",
                                    last_known_network.wifi_connected, current_network.wifi_connected);
                                eprintln!("[MQTT Monitor]   Ethernet: {} -> {}",
                                    last_known_network.ethernet_connected, current_network.ethernet_connected);

                                // Send WiFi disconnect alarm event
                                if last_known_network.wifi_connected && !current_network.wifi_connected {
                                    if let Err(e) = publisher.handle_message(MqttMessage::PublishSystemAlarmEvent {
                                        alarm_type: "WIFI_DISCONNECT".to_string(),
                                        name: "WiFi".to_string(),
                                        from_state: "NORMAL".to_string(),
                                        to_state: "WARNING".to_string(),
                                        message: "WiFi connection lost".to_string(),
                                    }).await {
                                        eprintln!("[MQTT Monitor] Failed to publish WiFi disconnect alarm: {}", e);
                                    }
                                }

                                // Send Ethernet disconnect alarm event
                                if last_known_network.ethernet_connected && !current_network.ethernet_connected {
                                    if let Err(e) = publisher.handle_message(MqttMessage::PublishSystemAlarmEvent {
                                        alarm_type: "ETHERNET_DISCONNECT".to_string(),
                                        name: "Ethernet".to_string(),
                                        from_state: "NORMAL".to_string(),
                                        to_state: "WARNING".to_string(),
                                        message: "Ethernet connection lost".to_string(),
                                    }).await {
                                        eprintln!("[MQTT Monitor] Failed to publish Ethernet disconnect alarm: {}", e);
                                    }
                                }

                                // If network came back up and we're not connected, log it
                                if (current_network.wifi_connected || current_network.ethernet_connected) &&
                                   (!last_known_network.wifi_connected && !last_known_network.ethernet_connected) {
                                    eprintln!("[MQTT Monitor] Network now available - will attempt reconnection");
                                }
                            }

                            last_known_network = current_network;
                        }
                    }

                    // Periodic status reporting — publishes system/info, on the
                    // configured cadence or on demand after a change an operator is
                    // waiting to see (a cluster arm).
                    _ = status_tick.tick() => {
                        // Not in standby: nothing is being measured, so there is no
                        // status worth reporting, and the retained power/standby
                        // topic already explains the silence. The connection itself
                        // stays up — this loop still has to carry commands and the
                        // resume event.
                        //
                        // The request is only consumed when it can actually be acted
                        // on, so one raised during standby still fires on resume
                        // instead of being swallowed by a tick nobody publishes for.
                        let standby = crate::libs::power::standby::is_standby();
                        let on_demand = !standby && take_system_info_publish_request();
                        if !standby
                            && (on_demand || last_status_publish.elapsed() >= status_interval)
                        {
                            last_status_publish = Instant::now();
                            if let Ok(state) = connection_state.lock() {
                                eprintln!("[MQTT Monitor] === STATUS REPORT ===");
                                eprintln!("[MQTT Monitor]   State: {:?}", state.state());
                                eprintln!("[MQTT Monitor]   Messages published: {}", state.stats().messages_published);
                                eprintln!("[MQTT Monitor]   Reconnections: {}", state.stats().reconnection_count);

                                if let Some(uptime) = state.uptime_seconds() {
                                    eprintln!("[MQTT Monitor]   Current connection uptime: {}s", uptime);
                                }

                                if state.stats().longest_connection_sec > 0 {
                                    eprintln!("[MQTT Monitor]   Longest connection: {}s", state.stats().longest_connection_sec);
                                }

                                if state.stats().total_uptime_sec > 0 {
                                    eprintln!("[MQTT Monitor]   Total uptime: {}s", state.stats().total_uptime_sec);
                                }

                                // Log recent disconnects
                                let history = &state.stats().disconnect_history;
                                if !history.is_empty() {
                                    eprintln!("[MQTT Monitor]   Recent disconnects ({}):", history.len());
                                    for (i, disconnect) in history.iter().rev().take(3).enumerate() {
                                        eprintln!("[MQTT Monitor]     {}: {} (lasted {}s)",
                                            i + 1, disconnect.reason, disconnect.duration_sec);
                                    }
                                }

                                // Log authorization status if available
                                if let Some(ref auth) = auth_manager {
                                    let active_challenges = auth.active_challenge_count();
                                    if active_challenges > 0 {
                                        eprintln!("[MQTT Monitor]   Active challenges: {}", active_challenges);
                                    }
                                }
                            }

                            // Publish combined system status via MQTT
                            let network = get_network_status();
                            let uptime_seconds = app_start_time.elapsed().as_secs();
                            let storage_usage = crate::libs::storage::get_partition_usage("/data");

                            // Get power data from shared state
                            let power = power_status.lock().map(|p| *p).unwrap_or_default();
                            let last_dc_loss_time = power.last_dc_loss_time
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs());

                            // Get device label and LoRaWAN sensor count from config
                            let cfg = crate::libs::config::Config::load_default().ok();
                            let device_label = cfg
                                .as_ref()
                                .and_then(|c| c.system.device_label.clone())
                                .unwrap_or_else(|| hostname.clone());
                            let lorawan_sensor_count = cfg
                                .as_ref()
                                .and_then(|c| c.lorawan.as_ref())
                                .map(|l| l.sensors.len())
                                .unwrap_or(0);
                            // Pick up a set_system_info_interval change for the next
                            // round. Reusing the config just loaded above rather than
                            // re-reading it: the command writes the file, so the value
                            // is already here, and the cadence follows on the next
                            // publish without a reconnect.
                            status_interval = cfg
                                .as_ref()
                                .and_then(|c| c.mqtt.as_ref())
                                .map(|m| Duration::from_secs(m.publish.intervals.system_info_sec.max(1)))
                                .unwrap_or(status_interval);

                            // Check LoRaWAN gateway status (checks running services, not just installed)
                            let lorawan_detection = crate::libs::lorawan::detector::detect_gateway();

                            if let Err(e) = publisher.handle_message(MqttMessage::PublishSystemStatus {
                                hostname: hostname.clone(),
                                device_label,
                                version: firmware_version.clone(),
                                uptime_seconds,
                                battery_mv: power.vbat_mv,
                                battery_percent: power.battery_percent,
                                vin_mv: power.vin_mv,
                                on_dc_power: power.on_dc_power,
                                last_dc_loss_time,
                                wifi_connected: network.wifi_connected,
                                wifi_signal_dbm: network.wifi_signal_strength,
                                wifi_ip: network.wifi_ip,
                                ethernet_connected: network.ethernet_connected,
                                ethernet_ip: network.ethernet_ip,
                                storage_total_bytes: storage_usage.total_bytes,
                                storage_available_bytes: storage_usage.available_bytes,
                                storage_used_percent: storage_usage.used_percent,
                                lorawan_gateway_present: lorawan_detection.is_present(),
                                lorawan_concentratord_running: lorawan_detection.concentratord_running,
                                lorawan_chirpstack_running: lorawan_detection.chirpstack_running,
                                lorawan_sensor_count,
                            }).await {
                                eprintln!("[MQTT Monitor] Failed to publish system status: {}", e);
                            }
                        }
                    }

                    // Periodic challenge cleanup (every 30 seconds)
                    _ = challenge_cleanup_tick.tick() => {
                        {
                            if let Some(ref auth) = auth_manager {
                                let expired_count = auth.cleanup_expired_challenges();
                                if expired_count > 0 {
                                    eprintln!("[MQTT Monitor] Cleaned up {} expired challenges", expired_count);
                                }
                            }
                        }
                    }

                    // Poll for pairing results and publish them
                    _ = pairing_poll_tick.tick() => {
                        if let Ok(ph_guard) = pairing_handle.lock() {
                            if let Some(ref ph) = *ph_guard {
                                while let Some(result) = ph.try_recv_result() {
                                    match result {
                                        crate::libs::pairing::PairingResult::Success(response) => {
                                            eprintln!("[MQTT Monitor] Publishing pairing success response for {}",
                                                response.admin_certificate.signer_id);
                                            if let Err(e) = publisher.publish_pairing_response(&response).await {
                                                eprintln!("[MQTT Monitor] Failed to publish pairing response: {}", e);
                                            }
                                        }
                                        crate::libs::pairing::PairingResult::Error(error) => {
                                            eprintln!("[MQTT Monitor] Publishing pairing error: {}", error.error);
                                            if let Err(e) = publisher.publish_pairing_error(&error).await {
                                                eprintln!("[MQTT Monitor] Failed to publish pairing error: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    }
                } // End of inner event loop
            } // End of 'connection outer loop

            eprintln!("[MQTT Monitor] Monitor loop exited");
            Ok(())
        })
    }

    /// Parse pairing request from MQTT payload
    fn parse_pairing_request(
        payload: &[u8],
    ) -> Result<crate::libs::pairing::PairingRequest, String> {
        let json_str = std::str::from_utf8(payload).map_err(|e| format!("Invalid UTF-8: {}", e))?;

        let request: crate::libs::pairing::PairingRequest =
            serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON: {}", e))?;

        // Basic validation
        if request.request_id.is_empty() {
            return Err("Missing request_id".to_string());
        }
        if request.admin_username.is_empty() {
            return Err("Missing admin_username".to_string());
        }

        Ok(request)
    }

    /// Initialize authorization manager with crypto components
    #[cfg_attr(feature = "dev-platform", allow(dead_code))]
    fn init_authorization_manager(_config: &MqttConfig) -> Result<AuthorizationManager, String> {
        use crate::libs::crypto::CertificateAuthority;
        use std::path::Path;

        // Initialize CA registry (trusted Certificate Authorities)
        let ca_file = Path::new("/data/fiber/config/authorized_signers.yaml");
        let mut registry = CARegistry::load_from_file(ca_file)
            .map_err(|e| format!("Failed to load CA registry: {:?}", e))?;

        // Try to load device CA and register it as trusted
        let device_ca_file = Path::new("/data/fiber/config/device_ca.key");
        if device_ca_file.exists() {
            match crate::libs::pairing::ca_key::DeviceCaKey::load_existing(device_ca_file) {
                Ok(device_ca) => {
                    let ca_id = device_ca.ca_id();
                    let public_key_hex = device_ca.public_key_hex();

                    // Register the device CA
                    let device_ca_entry = CertificateAuthority {
                        ca_id: ca_id.clone(),
                        ca_public_key_ed25519: public_key_hex.clone(),
                        trusted_since: chrono::Utc::now().to_rfc3339(),
                        enabled: true,
                        description: Some("Device's own CA (auto-registered)".to_string()),
                    };
                    registry.add_ca(device_ca_entry);

                    // Also register with generic "device_ca" ID for compatibility
                    let generic_ca_entry = CertificateAuthority {
                        ca_id: "device_ca".to_string(),
                        ca_public_key_ed25519: public_key_hex,
                        trusted_since: chrono::Utc::now().to_rfc3339(),
                        enabled: true,
                        description: Some("Device CA (compatibility alias)".to_string()),
                    };
                    registry.add_ca(generic_ca_entry);

                    eprintln!("[MQTT Monitor] Device CA registered as trusted: {}", ca_id);
                }
                Err(e) => {
                    eprintln!("[MQTT Monitor] Warning: Could not load device CA: {}", e);
                }
            }
        } else {
            eprintln!("[MQTT Monitor] Device CA file not found, pairing not yet performed");
        }

        let ca_registry = Arc::new(Mutex::new(registry));

        // Initialize nonce tracker
        let nonce_db = Path::new("/tmp/fiber_nonces.db");
        let nonce_tracker = Arc::new(Mutex::new(
            NonceTracker::new(nonce_db, 600, 1000)
                .map_err(|e| format!("Failed to initialize nonce tracker: {:?}", e))?,
        ));

        // Create signature verifier (with CA-based certificate chain validation)
        let verifier = Arc::new(SignatureVerifier::new(
            ca_registry,
            nonce_tracker,
            60, // ±60 seconds timestamp drift (tightened from 300s per EU MDR hardening)
        ));

        // Create authorization manager
        let audit_db = Path::new("/tmp/fiber_audit.db");
        let manager = AuthorizationManager::new(
            verifier, audit_db, 300, // 5 minute challenge timeout
            10,  // max 10 concurrent challenges
        );

        Ok(manager)
    }

    /// Build a command directly from params (dev-platform mode, no auth)
    #[cfg(feature = "dev-platform")]
    fn build_dev_command(
        command_type: &str,
        params: &serde_json::Value,
        reason: &Option<String>,
    ) -> Result<MqttCommand, String> {
        match command_type {
            "set_threshold" => {
                let line = params
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing line")? as u8;
                let thresholds = params.get("thresholds").ok_or("Missing thresholds")?;
                Ok(MqttCommand::SetSensorThreshold {
                    line,
                    critical_low: thresholds["critical_low"].as_f64().unwrap_or(0.0) as f32,
                    alarm_low: thresholds
                        .get("alarm_low")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0) as f32,
                    warning_low: thresholds["warning_low"].as_f64().unwrap_or(0.0) as f32,
                    warning_high: thresholds["warning_high"].as_f64().unwrap_or(0.0) as f32,
                    alarm_high: thresholds
                        .get("alarm_high")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(100.0) as f32,
                    critical_high: thresholds["critical_high"].as_f64().unwrap_or(0.0) as f32,
                })
            }
            "set_sensor_name" => {
                let line = params
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing line")? as u8;
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing name")?
                    .to_string();
                Ok(MqttCommand::SetSensorName { line, name })
            }
            "set_sensor_location" => {
                let line = params
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing line")? as u8;
                let location = params
                    .get("location")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing location")?
                    .to_string();
                Ok(MqttCommand::SetSensorLocation { line, location })
            }
            "restart_application" => {
                let r = reason
                    .clone()
                    .unwrap_or_else(|| "Dev platform command".to_string());
                Ok(MqttCommand::RestartApplication {
                    reason: r,
                    requested_by: "dev-platform".to_string(),
                })
            }
            "power_off" => {
                let r = reason
                    .clone()
                    .unwrap_or_else(|| "Dev platform command".to_string());
                Ok(MqttCommand::PowerOffDevice {
                    reason: r,
                    requested_by: "dev-platform".to_string(),
                })
            }
            "set_interval" => {
                let sample = params
                    .get("sample_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing sample_interval_ms")?;
                let aggregation = params
                    .get("aggregation_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing aggregation_interval_ms")?;
                let report = params
                    .get("report_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing report_interval_ms")?;
                Ok(MqttCommand::SetInterval {
                    sample_interval_ms: sample,
                    aggregation_interval_ms: aggregation,
                    report_interval_ms: report,
                })
            }
            "set_system_info_interval" => {
                let interval = params
                    .get("interval_seconds")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing interval_seconds")?;
                Ok(MqttCommand::SetSystemInfoInterval {
                    interval_seconds: interval,
                })
            }
            "set_device_label" => {
                let label = params
                    .get("label")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing label")?
                    .to_string();
                Ok(MqttCommand::SetDeviceLabel { label })
            }
            "set_led_brightness" => {
                let brightness = params
                    .get("brightness")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing brightness")? as u8;
                Ok(MqttCommand::SetLedBrightness { brightness })
            }
            "set_screen_brightness" => {
                let brightness = params
                    .get("brightness")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing brightness")? as u8;
                Ok(MqttCommand::SetScreenBrightness { brightness })
            }
            "set_screen_timeout" => {
                let raw = params
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing timeout_secs")?;
                if raw > u64::from(u32::MAX) {
                    return Err("timeout_secs out of range".to_string());
                }
                Ok(MqttCommand::SetScreenTimeout {
                    timeout_secs: raw as u32,
                })
            }
            "set_buzzer_volume" => {
                let volume = params
                    .get("volume")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing volume")? as u8;
                Ok(MqttCommand::SetBuzzerVolume { volume })
            }
            "set_display_lines" => {
                let raw = params.get("lines").ok_or("Missing lines")?;
                let lines: Vec<crate::libs::config::DisplayLine> =
                    serde_json::from_value(raw.clone())
                        .map_err(|e| format!("Invalid display lines: {}", e))?;
                crate::libs::config_applier::validation::validate_display_custom_lines(&lines)?;
                Ok(MqttCommand::SetDisplayLines { lines })
            }
            "set_network_config" => Ok(MqttCommand::SetNetworkConfig {
                interface: params
                    .get("interface")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ethernet")
                    .to_string(),
                config_type: params
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("dhcp")
                    .to_string(),
                ip_address: params
                    .get("ip_address")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                subnet_mask: params
                    .get("subnet_mask")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                gateway: params
                    .get("gateway")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                dns_primary: params
                    .get("dns_primary")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                dns_secondary: params
                    .get("dns_secondary")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            }),
            "set_sticker_config" => MqttCommand::parse_set_sticker_config(params),
            "send_sticker_raw" => MqttCommand::parse_send_sticker_raw(params),
            "set_eye_enabled" => {
                let enabled = params
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .ok_or("Missing enabled")?;
                MqttCommand::SetEyeEnabled { enabled }
            }
            "set_eye_recording" => {
                let mac = params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing mac")?
                    .to_uppercase();
                let interval_min = params
                    .get("interval_min")
                    .and_then(|v| v.as_u64())
                    .ok_or("Missing interval_min")?;
                if !matches!(interval_min, 0 | 1 | 5 | 15) {
                    return Err("interval_min must be 0 (off), 1, 5 or 15".to_string());
                }
                Ok(MqttCommand::SetEyeRecording {
                    mac,
                    interval_min: interval_min as u16,
                })
            }
            "download_eye_history" => {
                let mac = params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing mac")?
                    .to_uppercase();
                Ok(MqttCommand::DownloadEyeHistory { mac })
            }
            "add_eye_tag" => {
                let mac = params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing mac")?
                    .to_uppercase();
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                Ok(MqttCommand::AddEyeTag { mac, name })
            }
            "set_eye_known_tags" => {
                // An empty list is legal and meaningful: it means the fleet knows of
                // no tags beyond this gateway's own, so stop listening for borrowed
                // ones. Malformed MACs are dropped rather than failing the whole
                // push — one bad row must not stop the rest of the fleet's tags
                // from being audible.
                let macs: Vec<String> = params
                    .get("macs")
                    .and_then(|v| v.as_array())
                    .ok_or("Missing macs (array)")?
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_uppercase())
                    .filter(|s| crate::libs::eye::state::is_valid_mac(s))
                    .collect();
                Ok(MqttCommand::SetEyeKnownTags { macs })
            }
            "remove_eye_tag" => {
                let mac = params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing mac")?
                    .to_uppercase();
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                Ok(MqttCommand::RemoveEyeTag { mac })
            }
            "detect_eye_tag" => {
                let mac = params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing mac")?
                    .to_uppercase();
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                Ok(MqttCommand::DetectEyeTag { mac })
            }
            "set_eye_field_threshold" => {
                let mac = params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing mac")?
                    .to_uppercase();
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                let field = params
                    .get("field")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing field")?
                    .to_string();
                Ok(MqttCommand::SetEyeFieldThreshold {
                    mac,
                    field,
                    critical_low: params.get("critical_low").and_then(|v| v.as_f64()),
                    warning_low: params.get("warning_low").and_then(|v| v.as_f64()),
                    warning_high: params.get("warning_high").and_then(|v| v.as_f64()),
                    critical_high: params.get("critical_high").and_then(|v| v.as_f64()),
                })
            }
            "delete_eye_field_threshold" => {
                let mac = params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing mac")?
                    .to_uppercase();
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                let field = params
                    .get("field")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing field")?
                    .to_string();
                Ok(MqttCommand::DeleteEyeFieldThreshold { mac, field })
            }
            _ => Err(format!(
                "Unsupported dev-platform command: {}",
                command_type
            )),
        }
    }

    /// Dispatch a resolved (authorized) config command. `set_sticker_config` is
    /// an async fPort-85 write that publishes its own result, so it is spawned
    /// here; every other command delegates to the synchronous executor.
    #[allow(clippy::too_many_arguments)]
    fn execute_resolved_command(
        cmd: MqttCommand,
        config_applier: &Option<Arc<ConfigApplier>>,
        stm_bridge: &Option<SharedStmBridge>,
        screen_brightness: &Option<SharedScreenBrightnessHandle>,
        screen_timeout: &Option<SharedScreenTimeoutHandle>,
        buzzer_volume: &Option<SharedBuzzerVolumeHandle>,
        display_lines: &Option<SharedDisplayLinesHandle>,
        buzzer_priority: &Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
        led_brightness_tracker: &std::sync::Arc<std::sync::atomic::AtomicU8>,
        lorawan_state_slot: &std::sync::Arc<
            std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>,
        >,
        lorawan_configs: &Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
        storage_handle: &Option<crate::libs::storage::StorageHandle>,
        lorawan_handle_slot: &std::sync::Arc<
            std::sync::Mutex<Option<crate::libs::lorawan::LoRaWANHandle>>,
        >,
        client: &AsyncClient,
        topics: &TopicBuilder,
        publish_cfg: &crate::libs::config::PublishConfig,
        export_handle_slot: &SharedExportHandle,
    ) -> Result<(), String> {
        if let MqttCommand::SendStickerRaw {
            dev_eui,
            bytes,
            fport,
        } = cmd
        {
            // Fire-and-forget raw downlink (expert). No response is correlated;
            // its effect is confirmed by a subsequent "read from device".
            return match lorawan_handle_slot.lock().ok().and_then(|g| g.clone()) {
                Some(handle) => handle.send_raw(&dev_eui, bytes, fport),
                None => Err("LoRaWAN command handle not available".to_string()),
            };
        }
        // #71 control commands: signed, so they arrive here already authorised.
        // Handled before execute_config_command because they talk to the radio
        // rather than to on-disk config.
        if matches!(
            cmd,
            MqttCommand::StickerReboot { .. }
                | MqttCommand::StickerDeviceReset { .. }
                | MqttCommand::StickerResetCounters { .. }
                | MqttCommand::StickerClockSync { .. }
                | MqttCommand::StickerForceSend { .. }
        ) {
            return match lorawan_handle_slot.lock().ok().and_then(|g| g.clone()) {
                Some(handle) => {
                    Self::spawn_sticker_command(
                        client.clone(),
                        topics.clone(),
                        publish_cfg.clone(),
                        handle,
                        cmd,
                    );
                    Ok(())
                }
                None => Err("LoRaWAN command handle not available".to_string()),
            };
        }
        if let MqttCommand::SetStickerConfig {
            dev_eui,
            fields,
            save,
        } = cmd
        {
            return match lorawan_handle_slot.lock().ok().and_then(|g| g.clone()) {
                Some(handle) => {
                    Self::spawn_sticker_config_write(
                        client.clone(),
                        topics.clone(),
                        publish_cfg.clone(),
                        handle,
                        dev_eui,
                        fields,
                        save,
                    );
                    Ok(())
                }
                None => Err("LoRaWAN command handle not available".to_string()),
            };
        }
        // Captured before `cmd` is moved: a successful removal has to clear the
        // sticker's retained device-info topic, or the broker replays a
        // decommissioned device's info to every new subscriber indefinitely (#65).
        let removed_sticker = match &cmd {
            MqttCommand::RemoveLoRaWANSticker { dev_eui } => Some(dev_eui.clone()),
            _ => None,
        };
        let result = Self::execute_config_command(
            cmd,
            config_applier,
            stm_bridge,
            screen_brightness,
            screen_timeout,
            buzzer_volume,
            display_lines,
            buzzer_priority,
            led_brightness_tracker,
            lorawan_state_slot,
            lorawan_configs,
            storage_handle,
            export_handle_slot,
        );
        if result.is_ok() {
            if let Some(dev_eui) = removed_sticker {
                let publisher = MqttPublisher::new(client.clone(), topics.clone(), publish_cfg);
                tokio::spawn(async move {
                    if let Err(e) = publisher
                        .handle_message(MqttMessage::ClearStickerInfo { dev_eui })
                        .await
                    {
                        eprintln!(
                            "[MQTT Monitor] Failed to clear retained sticker info: {}",
                            e
                        );
                    }
                });
            }
        }
        result
    }

    /// Decide which response to publish for a confirmed command: the pre-built
    /// SUCCESS response when execution succeeded, or an ERROR response (reusing
    /// the same challenge/request ids) when it failed. Pure — unit-testable
    /// without the async MQTT/execute machinery.
    /// Commands whose execution tears down the process or the MQTT-bearing
    /// network interface. Their confirmation must be published BEFORE execution
    /// (best-effort SUCCESS), because execute-then-report would race the shutdown
    /// and the signer would never receive the response.
    ///
    /// `PowerOffDevice` is deliberately *not* one of them. It used to run
    /// `systemctl poweroff` and so had to ack first, but it now enters standby:
    /// the process and the MQTT connection both survive, and it can genuinely
    /// fail (a marker it cannot persist means it must refuse — see
    /// [`Self::execute_standby`]). Acking first would tell the signer the device
    /// is off while it carries on monitoring, which is the one outcome the
    /// preview text must never be wrong about.
    fn is_teardown_command(cmd: &MqttCommand) -> bool {
        matches!(
            cmd,
            MqttCommand::RestartApplication { .. } | MqttCommand::SetNetworkConfig { .. }
        )
    }

    fn confirm_response_message(exec: Result<(), String>, success: MqttMessage) -> MqttMessage {
        match exec {
            Ok(()) => success,
            Err(e) => match success {
                MqttMessage::PublishConfigResponse {
                    challenge_id,
                    request_id,
                    ..
                } => MqttMessage::PublishConfigResponse {
                    challenge_id,
                    request_id,
                    status: "ERROR".to_string(),
                    applied_at: None,
                    effective_at: None,
                    message: format!("Execution failed: {e}"),
                },
                other => other,
            },
        }
    }

    /// Take the device down — the shared body of reboot and power-off.
    ///
    /// Both lose the authorization record: it lives in `/tmp/fiber_audit.db` and
    /// the unit runs with `PrivateTmp=true`, so a reboot wipes it exactly as a
    /// power-off does. The row in the encrypted, hash-chained `audit_log` is
    /// therefore the only surviving evidence that the gap in monitoring was
    /// authorized and by whom, which is why this waits for it to land.
    ///
    /// Write the authorization record for a command that interrupts monitoring,
    /// and block until it is durable.
    ///
    /// Shared by reboot and standby. `label` is only used for log prefixes.
    ///
    /// Never fails the caller: an unaudited teardown is bad, but refusing to act
    /// on a signed command because the audit database is unhappy would leave a
    /// device that cannot be stopped at all.
    fn audit_and_flush(
        label: &str,
        audit_event: &'static str,
        reason: &str,
        requested_by: &str,
        storage_handle: &Option<crate::libs::storage::StorageHandle>,
    ) {
        let Some(storage) = storage_handle else {
            eprintln!("[MQTT Monitor] WARN: no storage handle — {label} will not be audited");
            return;
        };

        let details = format!(
            r#"{{"reason":{},"requested_by":{}}}"#,
            serde_json::Value::String(reason.to_string()),
            serde_json::Value::String(requested_by.to_string()),
        );
        if let Err(e) = storage.log_audit_event(
            audit_event.to_string(),
            Some("audit_log".to_string()),
            Some(details),
        ) {
            eprintln!("[MQTT Monitor] WARN: failed to queue {label} audit row: {e}");
        }
        // The storage worker is a single thread draining one FIFO channel,
        // so a FlushSync reply also proves the audit row queued above was
        // committed and checkpointed. It is also what keeps unwritten
        // temperature samples from being lost across the restart.
        if let Err(e) = storage.flush_sync(TEARDOWN_AUDIT_FLUSH_TIMEOUT) {
            eprintln!("[MQTT Monitor] WARN: {label} audit row may not be durable: {e}");
        }
    }

    /// Put the device into deep standby — what a Viewer power-off now does.
    ///
    /// It used to run `systemctl poweroff`. That halts the BCM2711 while the
    /// battery holds the rails up, and nothing on this board can wake a halted
    /// CM4 (see [`crate::libs::power::standby`]), so the device stayed
    /// unreachable until someone pulled the battery — including when PoE came
    /// back. Standby keeps the agent alive reading VIN instead, and
    /// `PowerMonitor` brings the device back when DC returns.
    ///
    /// Ordering is load-bearing:
    ///
    /// * The marker is written **first**, and a failure to write it aborts the
    ///   whole thing. The unit runs with `Restart=on-failure`, so a standby the
    ///   next boot cannot detect is a device that comes back up monitoring while
    ///   the operator has been told it is off. Better to refuse and stay running.
    /// * The audit row is flushed **before** anything is torn down, so the only
    ///   surviving evidence that this gap in monitoring was authorized is on disk
    ///   before monitoring stops.
    /// * Sensor rails go down here rather than in `PowerMonitor`, because that
    ///   loop's interval is a configured 60 s on-device and eight DS18B20 lines
    ///   must not stay energised for a minute after a power-off.
    ///
    /// Unlike the reboot path this runs inline: there is no process teardown to
    /// race, and the caller has already queued the SUCCESS ack.
    fn execute_standby(
        reason: String,
        requested_by: String,
        storage_handle: &Option<crate::libs::storage::StorageHandle>,
        stm_bridge: &Option<SharedStmBridge>,
    ) -> Result<(), String> {
        use crate::libs::power::standby;

        eprintln!(
            "[MQTT Monitor] Device standby requested by {}: {}",
            requested_by, reason
        );

        let marker_dir = standby::configured_marker_dir();
        let marker = standby::StandbyMarker::new(reason.clone(), requested_by.clone());
        marker.write(&marker_dir).map_err(|e| {
            format!(
                "cannot record standby in {}: {e} — device stays up",
                marker_dir.display()
            )
        })?;

        Self::audit_and_flush(
            "standby",
            "POWER_OFF",
            &reason,
            &requested_by,
            storage_handle,
        );

        if !standby::request_standby() {
            eprintln!("[MQTT Monitor] Device was already in standby — nothing to do");
            return Ok(());
        }

        // Rails down before the panel goes dark, so the device is genuinely not
        // measuring by the time it looks like it is not measuring.
        if let Some(stm) = stm_bridge {
            match stm.lock() {
                Ok(mut guard) => {
                    if let Err(e) = guard.set_sensor_power(false) {
                        eprintln!("[MQTT Monitor] WARN: could not drop sensor rails: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("[MQTT Monitor] WARN: STM bridge lock poisoned: {e}");
                }
            }
        }

        crate::libs::display::blank::request_blank();
        if !crate::libs::display::blank::wait_until_blank(DISPLAY_BLANK_TIMEOUT) {
            eprintln!("[standby] WARN: display did not confirm blank — continuing anyway");
        }

        standby::apply_cpu_governor(&standby::config().cpu_governor);

        eprintln!(
            "[standby] Device is in standby; waiting for PoE (resume_on_dc={})",
            standby::config().resume_on_dc
        );
        Ok(())
    }

    /// `verb` is the systemctl subcommand ("reboot"/"poweroff") and doubles as
    /// the worker-thread name and log prefix.
    fn execute_teardown(
        verb: &'static str,
        audit_event: &'static str,
        reason: String,
        requested_by: String,
        storage_handle: &Option<crate::libs::storage::StorageHandle>,
    ) -> Result<(), String> {
        eprintln!(
            "[MQTT Monitor] Device {} requested by {}: {}",
            verb, requested_by, reason
        );

        Self::audit_and_flush(verb, audit_event, &reason, &requested_by, storage_handle);

        // Spawn and return immediately; do NOT wait on the child here. The
        // SUCCESS ack for a teardown command is only *queued* at this point:
        // `AsyncClient::publish` hands the packet to rumqttc's channel and it
        // reaches the socket the next time `eventloop.poll()` runs — which
        // cannot happen until this function returns, because the whole
        // ConfigConfirm branch runs inside the poll arm of a `tokio::select!`.
        // Blocking here would mean the signer never learns the command worked.
        // `--no-block` for the same reason inside the thread: without it,
        // systemctl waits for the job to finish and may never return.
        std::thread::Builder::new()
            .name(verb.to_string())
            .spawn(move || {
                // Blank inside the grace window rather than extending it: the
                // panel goes dark while the ack is still on its way out, so the
                // device stops showing readings it is no longer taking. The
                // deadline keeps the ack's full window intact no matter how fast
                // the display confirms.
                let deadline = std::time::Instant::now() + TEARDOWN_GRACE;
                crate::libs::display::blank::request_blank();
                if !crate::libs::display::blank::wait_until_blank(DISPLAY_BLANK_TIMEOUT) {
                    eprintln!("[{verb}] WARN: display did not confirm blank — continuing anyway");
                }
                std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
                match std::process::Command::new("systemctl")
                    .args([verb, "--no-block"])
                    .status()
                {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        // A device that stays up must not stay dark.
                        crate::libs::display::blank::cancel_blank();
                        eprintln!(
                            "[{verb}] systemctl {verb} exited with {status} — device stays up"
                        );
                    }
                    Err(e) => {
                        crate::libs::display::blank::cancel_blank();
                        eprintln!(
                            "[{verb}] failed to execute systemctl {verb}: {e} — device stays up"
                        );
                    }
                }
            })
            .map_err(|e| format!("Failed to spawn {verb} thread: {e}"))?;
        Ok(())
    }

    /// Execute an approved configuration command
    /// Note: In CA-based trust model, signer management (add/remove/update) is handled by the CA platform,
    /// not directly on the device.
    fn execute_config_command(
        cmd: MqttCommand,
        config_applier: &Option<Arc<ConfigApplier>>,
        stm_bridge: &Option<SharedStmBridge>,
        screen_brightness: &Option<SharedScreenBrightnessHandle>,
        screen_timeout: &Option<SharedScreenTimeoutHandle>,
        buzzer_volume: &Option<SharedBuzzerVolumeHandle>,
        display_lines: &Option<SharedDisplayLinesHandle>,
        buzzer_priority: &Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
        led_brightness_tracker: &std::sync::Arc<std::sync::atomic::AtomicU8>,
        lorawan_state_slot: &std::sync::Arc<
            std::sync::Mutex<Option<crate::libs::lorawan::SharedLoRaWANState>>,
        >,
        lorawan_configs: &Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
        storage_handle: &Option<crate::libs::storage::StorageHandle>,
        export_handle_slot: &SharedExportHandle,
    ) -> Result<(), String> {
        match cmd {
            MqttCommand::SetSensorThreshold {
                line,
                critical_low,
                alarm_low,
                warning_low,
                warning_high,
                alarm_high,
                critical_high,
            } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_threshold_change(
                        line,
                        critical_low,
                        alarm_low,
                        warning_low,
                        warning_high,
                        alarm_high,
                        critical_high,
                    );

                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ Configuration applied successfully");
                        eprintln!("[MQTT Monitor]   File: {}", result.file_path);
                        if let Some(backup) = result.backup_path {
                            eprintln!("[MQTT Monitor]   Backup: {}", backup);
                        }
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::SetSensorName { line, name } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_name_change(line, name);

                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ Sensor name changed successfully");
                        eprintln!("[MQTT Monitor]   File: {}", result.file_path);
                        if let Some(backup) = result.backup_path {
                            eprintln!("[MQTT Monitor]   Backup: {}", backup);
                        }
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::SetSensorLocation { line, location } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_location_change(line, location);

                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ Sensor location changed successfully");
                        eprintln!("[MQTT Monitor]   File: {}", result.file_path);
                        if let Some(backup) = result.backup_path {
                            eprintln!("[MQTT Monitor]   Backup: {}", backup);
                        }
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::RestartApplication {
                reason,
                requested_by,
            } => Self::execute_teardown("reboot", "REBOOT", reason, requested_by, storage_handle),
            MqttCommand::PowerOffDevice {
                reason,
                requested_by,
            } => Self::execute_standby(reason, requested_by, storage_handle, stm_bridge),
            MqttCommand::SetInterval {
                sample_interval_ms,
                aggregation_interval_ms,
                report_interval_ms,
            } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_interval_change(
                        sample_interval_ms,
                        aggregation_interval_ms,
                        report_interval_ms,
                    );
                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ Sensor intervals updated (will apply on next hot-reload cycle)");
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::SetSystemInfoInterval { interval_seconds } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_system_info_interval_change(interval_seconds);
                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ System info interval updated to {}s (will apply on next hot-reload cycle)", interval_seconds);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::SetDeviceLabel { label } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_device_label_change(label.clone());
                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ Device label updated to \"{}\" (will apply on next hot-reload cycle)", label);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::SetLedBrightness { brightness } => {
                if let Some(stm) = stm_bridge {
                    match stm.lock() {
                        Ok(mut stm_guard) => {
                            match stm_guard.set_brightness(brightness) {
                                Ok(_) => {
                                    led_brightness_tracker.store(brightness, std::sync::atomic::Ordering::Relaxed);
                                    // Persist to config YAML
                                    if let Some(applier) = config_applier {
                                        let result = applier.apply_led_brightness_change(brightness);
                                        if !result.success {
                                            eprintln!("[MQTT Monitor] Warning: Failed to persist LED brightness: {:?}", result.error_message);
                                        }
                                    }
                                    eprintln!("[MQTT Monitor] ✓ LED brightness set to {}%", brightness);
                                    Ok(())
                                }
                                Err(e) => {
                                    Err(format!("Failed to set brightness: {}", e))
                                }
                            }
                        }
                        Err(e) => {
                            Err(format!("Failed to lock STM bridge: {}", e))
                        }
                    }
                } else {
                    Err("STM bridge not available for brightness control".to_string())
                }
            }
            MqttCommand::SetScreenBrightness { brightness } => {
                if let Some(sb) = screen_brightness {
                    sb.store(brightness, std::sync::atomic::Ordering::Relaxed);
                    // Persist to config YAML
                    if let Some(applier) = config_applier {
                        let result = applier.apply_screen_brightness_change(brightness);
                        if !result.success {
                            eprintln!("[MQTT Monitor] Warning: Failed to persist screen brightness: {:?}", result.error_message);
                        }
                    }
                    eprintln!("[MQTT Monitor] ✓ Screen brightness set to {}%", brightness);
                    Ok(())
                } else {
                    Err("Screen brightness control not available".to_string())
                }
            }
            MqttCommand::SetScreenTimeout { timeout_secs } => {
                if let Some(st) = screen_timeout {
                    st.store(timeout_secs, std::sync::atomic::Ordering::Relaxed);
                    // Persist to config YAML
                    if let Some(applier) = config_applier {
                        let result = applier.apply_screen_timeout_change(timeout_secs);
                        if !result.success {
                            eprintln!("[MQTT Monitor] Warning: Failed to persist screen timeout: {:?}", result.error_message);
                        }
                    }
                    eprintln!("[MQTT Monitor] ✓ Screen timeout set to {}s", timeout_secs);
                    Ok(())
                } else {
                    Err("Screen timeout control not available".to_string())
                }
            }
            MqttCommand::SetBuzzerVolume { volume } => {
                if let Some(bv) = buzzer_volume {
                    bv.store(volume, std::sync::atomic::Ordering::Relaxed);
                    // Persist to config YAML
                    if let Some(applier) = config_applier {
                        let result = applier.apply_buzzer_volume_change(volume);
                        if !result.success {
                            eprintln!("[MQTT Monitor] Warning: Failed to persist buzzer volume: {:?}", result.error_message);
                        }
                    }
                    eprintln!("[MQTT Monitor] ✓ Buzzer volume set to {}%", volume);
                    Ok(())
                } else {
                    Err("Buzzer volume control not available".to_string())
                }
            }
            MqttCommand::SetDisplayLines { lines } => {
                // Persist first: the on-disk config is the source of truth that
                // survives a reboot and that the display thread reconciles
                // against. Only mirror into the live handle once the write
                // succeeded, so a failed write can't leave the panel showing a
                // layout that isn't saved anywhere.
                let Some(applier) = config_applier else {
                    return Err("Config applier not initialized".to_string());
                };
                let result = applier.apply_display_custom_lines(lines.clone());
                if !result.success {
                    return Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()));
                }
                if let Some(handle) = display_lines.as_ref() {
                    // Poison-recovering: a skipped write here would silently
                    // leave the panel on the old layout until the display
                    // thread's next reconcile, or forever if it also can't
                    // write. The config on disk is already the new one.
                    *crate::libs::display::supervise::write_recover(handle) = lines.clone();
                }
                if lines.is_empty() {
                    eprintln!("[MQTT Monitor] ✓ Display lines cleared (built-in layout restored)");
                } else {
                    eprintln!("[MQTT Monitor] ✓ Display lines updated ({} lines)", lines.len());
                }
                Ok(())
            }
            MqttCommand::SilenceBuzzer => {
                if let Some(bp) = &buzzer_priority {
                    bp.silence();
                    eprintln!("[MQTT Monitor] ✓ Buzzer silenced by alarm ACK");
                    Ok(())
                } else {
                    Err("Buzzer priority manager not available".to_string())
                }
            }
            MqttCommand::SetNetworkConfig {
                interface,
                config_type,
                ip_address,
                subnet_mask,
                gateway,
                dns_primary,
                dns_secondary,
            } => {
                Self::execute_network_config(
                    &interface,
                    &config_type,
                    ip_address,
                    subnet_mask,
                    gateway,
                    dns_primary,
                    dns_secondary,
                )
            }
            MqttCommand::SetLoRaWANSensorConfig {
                dev_eui,
                name,
                serial_number,
                location,
            } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_lorawan_sensor_config(
                        dev_eui.clone(),
                        name.clone(),
                        serial_number.clone(),
                        location.clone(),
                    );
                    if result.success {
                        if let Some(cfgs) = lorawan_configs.as_ref() {
                            if let Ok(mut v) = cfgs.write() {
                                if let Some(existing) = v.iter_mut().find(|c| c.dev_eui == dev_eui) {
                                    existing.name = name.clone();
                                    existing.serial_number = serial_number.clone();
                                    if location.is_some() {
                                        existing.location = location.clone();
                                    }
                                } else {
                                    v.push(crate::libs::config::LoRaWANSensorConfig {
                                        dev_eui: dev_eui.clone(),
                                        name: name.clone(),
                                        serial_number: serial_number.clone(),
                                        location: location.clone(),
                                        enabled: true,
                                        field_thresholds: Vec::new(),
                                        disarmed_fields: Vec::new(),
                                    });
                                }
                            }
                        }
                        eprintln!("[MQTT Monitor] ✓ LoRaWAN sensor config updated for {}", dev_eui);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::SetLoRaWANFieldThreshold {
                dev_eui, field,
                critical_low, warning_low, warning_high, critical_high, enabled,
            } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_lorawan_field_threshold(
                        dev_eui.clone(), field.clone(),
                        critical_low, warning_low, warning_high, critical_high, enabled,
                    );
                    if result.success {
                        if let Some(cfgs) = lorawan_configs.as_ref() {
                            if let Ok(mut v) = cfgs.write() {
                                let entry = match v.iter_mut().find(|c| c.dev_eui == dev_eui) {
                                    Some(e) => e,
                                    None => {
                                        v.push(crate::libs::config::LoRaWANSensorConfig {
                                            dev_eui: dev_eui.clone(),
                                            name: None, serial_number: None, location: None,
                                            enabled: true, field_thresholds: Vec::new(), disarmed_fields: Vec::new(),
                                        });
                                        v.last_mut().unwrap()
                                    }
                                };
                                let new_t = crate::libs::config::FieldThreshold {
                                    field: field.clone(),
                                    critical_low, warning_low, warning_high, critical_high,
                                };
                                if let Some(t) = entry.field_thresholds.iter_mut().find(|t| t.field == field) {
                                    *t = new_t;
                                } else {
                                    entry.field_thresholds.push(new_t);
                                }
                                // Mirror the on/off flag into the in-memory config so the
                                // next alarm evaluation sees it without waiting for a
                                // config reload from disk.
                                entry.disarmed_fields.retain(|f| f != &field);
                                if !enabled {
                                    entry.disarmed_fields.push(field.clone());
                                }
                            }
                        }
                        eprintln!(
                            "[MQTT Monitor] ✓ Field threshold {}/{} applied ({})",
                            dev_eui, field, if enabled { "alarm on" } else { "alarm OFF" }
                        );
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::DeleteLoRaWANFieldThreshold { dev_eui, field } => {
                if let Some(applier) = config_applier {
                    let result = applier.delete_lorawan_field_threshold(dev_eui.clone(), field.clone());
                    if result.success {
                        if let Some(cfgs) = lorawan_configs.as_ref() {
                            if let Ok(mut v) = cfgs.write() {
                                if let Some(entry) = v.iter_mut().find(|c| c.dev_eui == dev_eui) {
                                    entry.field_thresholds.retain(|t| t.field != field);
                                }
                            }
                        }
                        eprintln!("[MQTT Monitor] ✓ Field threshold {}/{} removed", dev_eui, field);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::AddLoRaWANSticker {
                dev_eui,
                name,
                serial_number,
                activation,
            } => {
                let lorawan_state = lorawan_state_slot.lock().ok().and_then(|g| g.clone());
                let deps = crate::libs::lorawan::StickerAddDeps {
                    config_applier: config_applier.clone(),
                    storage: storage_handle.clone(),
                    lorawan_configs: lorawan_configs.clone(),
                    lorawan_state,
                };
                crate::libs::lorawan::add_lorawan_sticker(
                    &deps, dev_eui, name, serial_number, activation,
                )
            }
            MqttCommand::RemoveLoRaWANSticker { dev_eui } => {
                eprintln!("[MQTT Monitor] Removing sticker {} ...", dev_eui);
                if let Some(applier) = config_applier {
                    let result = applier.remove_lorawan_sensor_config(dev_eui.clone());
                    if result.success {
                        // Drop the in-memory sensor entry so the display stops showing it.
                        let state_opt: Option<crate::libs::lorawan::SharedLoRaWANState> =
                            lorawan_state_slot.lock().ok().and_then(|g| g.clone());
                        if let Some(state) = state_opt {
                            if let Ok(mut s) = state.write() {
                                s.sensors.remove(&dev_eui);
                            }
                        }
                        // Drop from the shared configs so the LoRa monitor stops
                        // evaluating thresholds for it (matches on-disk YAML state).
                        if let Some(cfgs) = lorawan_configs.as_ref() {
                            if let Ok(mut v) = cfgs.write() {
                                v.retain(|c| c.dev_eui != dev_eui);
                            }
                        }
                        // Best-effort: remove from ChirpStack so the device disappears
                        // from the network server too. Failure is logged but not fatal —
                        // local config is the source of truth.
                        match crate::libs::lorawan::provisioning::deprovision_sticker(&dev_eui) {
                            Ok(()) => eprintln!(
                                "[MQTT Monitor] ✓ Sticker {} removed from ChirpStack",
                                dev_eui
                            ),
                            Err(e) => eprintln!(
                                "[MQTT Monitor] ⚠ ChirpStack deprovision for {}: {}",
                                dev_eui, e
                            ),
                        }
                        eprintln!("[MQTT Monitor] ✓ LoRaWAN sticker {} removed", dev_eui);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::AddExternalGateway { gateway_eui, name } => {
                // gateway_eui was already validated/normalized in
                // build_command_from_challenge (the only construction site).
                eprintln!("[MQTT Monitor] Registering external gateway {} in ChirpStack...", gateway_eui);

                // Step 1: best-effort ChirpStack registration — ChirpStack may be
                // down; local config is the source of truth and is applied regardless.
                match crate::libs::lorawan::provisioning::provision_external_gateway(&gateway_eui, &name) {
                    Ok(()) => eprintln!("[MQTT Monitor] ✓ Gateway {} registered in ChirpStack", gateway_eui),
                    Err(e) => eprintln!("[MQTT Monitor] ⚠ ChirpStack gateway provisioning for {}: {}", gateway_eui, e),
                }

                // Step 2: persist to YAML (always, even if ChirpStack failed)
                if let Some(applier) = config_applier {
                    let result = applier.apply_external_gateway(gateway_eui.clone(), Some(name.clone()));
                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ External gateway {} config saved", gateway_eui);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::SetLorawanCluster {
                role,
                leader_host,
                leader_port,
                leader_ca,
                leader_ca_fingerprint,
                peer_username,
                peer_password,
                peer_gateway_eui,
            } => {
                // Every field was validated in build_command_from_challenge (the
                // only construction site): the host is site-local, the port is
                // not the anonymous loopback listener, the fingerprint matches
                // the CA, and the account is a dedicated peer-*.
                use crate::libs::lorawan::cluster::{ClusterArm, ClusterRole, ClusterState};

                let parsed_role = ClusterRole::parse(&role)?;
                eprintln!("[MQTT Monitor] Setting LoRaWAN cluster role to {}...", parsed_role.as_str());

                let arm = ClusterArm {
                    role: parsed_role,
                    leader_host: leader_host.clone(),
                    leader_port,
                    leader_ca_pem: leader_ca.clone(),
                    leader_ca_fingerprint: leader_ca_fingerprint.clone(),
                    peer_username: peer_username.clone(),
                    peer_password: peer_password.clone(),
                    peer_gateway_eui: peer_gateway_eui.clone(),
                };

                // What this unit registered while it was leading, read before the
                // state that records it is cleared. Withdrawing it is the other
                // half of the arm that added it: a peer radio left in ChirpStack
                // keeps a disbanded cluster's gateway in the leader's device list.
                let previously_registered_peer =
                    ClusterState::at_default().peer_gateway_eui();

                // Step 1: persist the operator's intent to /data first. This is
                // the inverse of AddExternalGateway's order, and deliberately so:
                // there the remote call is the point and the YAML is the record,
                // whereas here the persisted state *is* what the boot-time
                // renderer acts on. Writing it first means a failed activation
                // below still leaves the cluster correctly configured for the
                // next boot instead of discarding what the operator asked for.
                let state = ClusterState::at_default();
                if let Err(e) = state.write(&arm) {
                    return Err(format!("Failed to persist cluster state: {}", e));
                }
                eprintln!("[MQTT Monitor] ✓ Cluster state persisted to /data");

                // Step 2: the one-publisher invariant. A follower contributes its
                // radio, not its reports — only the leader's fiber_app may
                // publish a sticker's telemetry, or the viewer sees two sources
                // for one sticker and history double-counts.
                let want_lorawan = parsed_role != ClusterRole::Follower;
                if let Some(applier) = config_applier {
                    let result = applier.apply_lorawan_enabled(want_lorawan);
                    if !result.success {
                        return Err(result
                            .error_message
                            .unwrap_or_else(|| "Failed to set lorawan.enabled".to_string()));
                    }
                } else {
                    return Err("Config applier not initialized".to_string());
                }

                // Step 3: best-effort activation. The renderer and the peer-account
                // mint are shipped by meta-fiber and re-run on every boot anyway,
                // so a failure here delays the cluster until the next reboot
                // rather than losing it.
                match crate::libs::lorawan::cluster::activate(&arm) {
                    Ok(msg) => eprintln!("[MQTT Monitor] ✓ Cluster activation: {}", msg),
                    Err(e) => eprintln!(
                        "[MQTT Monitor] ⚠ Cluster activation deferred to next boot: {}",
                        e
                    ),
                }

                // Step 4: the peer's radio in this unit's ChirpStack. Uplinks
                // from a gateway ChirpStack does not know are discarded, so
                // without this a follower's frames arrive on the leader's broker
                // and go nowhere — the failure mode looks like a working forward
                // and a silent sticker. Idempotent both ways, and best-effort:
                // it can be redone from the UI, whereas losing the arm cannot.
                if parsed_role == ClusterRole::Leader {
                    if let Some(eui) = arm.peer_gateway_eui.as_deref() {
                        match crate::libs::lorawan::provisioning::provision_external_gateway(
                            eui,
                            &format!("FIBER cluster peer {}", eui),
                        ) {
                            Ok(()) => eprintln!(
                                "[MQTT Monitor] ✓ Peer radio {} registered in ChirpStack",
                                eui
                            ),
                            Err(e) => eprintln!(
                                "[MQTT Monitor] ⚠ Peer radio {} not registered: {}",
                                eui, e
                            ),
                        }
                    }
                } else if let Some(eui) = previously_registered_peer.as_deref() {
                    match crate::libs::lorawan::provisioning::deprovision_external_gateway(eui) {
                        Ok(()) => eprintln!(
                            "[MQTT Monitor] ✓ Peer radio {} removed from ChirpStack",
                            eui
                        ),
                        Err(e) => eprintln!(
                            "[MQTT Monitor] ⚠ Peer radio {} not removed: {}",
                            eui, e
                        ),
                    }
                }

                // The card an operator is watching reads `system/info`, which is
                // otherwise republished on its own schedule — up to a minute of
                // staring at the old role after a change that already took
                // effect. Ask for a fresh one now.
                request_system_info_publish();

                Ok(())
            }
            MqttCommand::RemoveExternalGateway { gateway_eui } => {
                eprintln!("[MQTT Monitor] Removing external gateway {} ...", gateway_eui);
                if let Some(applier) = config_applier {
                    let result = applier.remove_external_gateway(gateway_eui.clone());
                    if result.success {
                        // Best-effort: deregister from ChirpStack so it disappears from
                        // the network server too. Failure is logged but not fatal —
                        // local config is the source of truth.
                        match crate::libs::lorawan::provisioning::deprovision_external_gateway(&gateway_eui) {
                            Ok(()) => eprintln!(
                                "[MQTT Monitor] ✓ Gateway {} removed from ChirpStack",
                                gateway_eui
                            ),
                            Err(e) => eprintln!(
                                "[MQTT Monitor] ⚠ ChirpStack gateway deprovision for {}: {}",
                                gateway_eui, e
                            ),
                        }
                        eprintln!("[MQTT Monitor] ✓ External gateway {} removed", gateway_eui);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::SetEyeEnabled { enabled } => {
                let Some(applier) = config_applier else {
                    return Err("Config applier not initialized".to_string());
                };
                let result = applier.apply_eye_enabled(enabled);
                if !result.success {
                    return Err(result
                        .error_message
                        .unwrap_or_else(|| "Unknown error".to_string()));
                }
                // Mirror into the live config so config_state echoes the new
                // value immediately, even though the monitor thread itself is
                // only spawned at startup.
                if let Some(cfg) = crate::libs::eye::state::eye_config_handle() {
                    if let Ok(mut c) = cfg.write() {
                        c.enabled = enabled;
                    }
                }
                eprintln!(
                    "[MQTT Monitor] EYE subsystem {} (takes effect when the fiber service restarts)",
                    if enabled { "enabled" } else { "disabled" }
                );
                Ok(())
            }

            MqttCommand::SetEyeRecording { mac, interval_min } => {
                // Hand off to the EYE monitor, which runs recorder ops with the
                // BLE scan paused (raw L2CAP and an active scan must not overlap).
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                // Persist the recording on/off + interval FIRST, so interval 0 =
                // off survives a restart and the gap/fallback sync stops queueing
                // downloads (which would otherwise re-START_RECORD the tag) — H1.
                if let Some(applier) = config_applier {
                    let result = applier.apply_eye_recording(mac.clone(), interval_min);
                    if !result.success {
                        return Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()));
                    }
                    // Reflect in the live config so the running loop's
                    // recording_on_for() updates without a restart.
                    if let Some(cfg) = crate::libs::eye::state::eye_config_handle() {
                        if let Ok(mut c) = cfg.write() {
                            c.set_recording(&mac, interval_min);
                        }
                    }
                } else {
                    return Err("Config applier not initialized".to_string());
                }
                if crate::libs::eye::state::queue_eye_command(
                    crate::libs::eye::state::EyeCommand::SetRecording {
                        mac: mac.clone(),
                        interval_min,
                    },
                ) {
                    eprintln!("[MQTT Monitor] Queued EYE set-recording {mac} @ {interval_min} min");
                    Ok(())
                } else {
                    Err("EYE monitor not running".to_string())
                }
            }
            MqttCommand::DownloadEyeHistory { mac } => {
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                if crate::libs::eye::state::queue_eye_command(
                    crate::libs::eye::state::EyeCommand::DownloadHistory { mac: mac.clone() },
                ) {
                    eprintln!("[MQTT Monitor] Queued EYE history download {mac}");
                    Ok(())
                } else {
                    Err("EYE monitor not running".to_string())
                }
            }
            MqttCommand::SetEyeKnownTags { macs } => {
                // Held in memory only, never written to fiber.config.yaml: the
                // server re-pushes the union on every connect (the topic is
                // retained), and persisting it would blur the line between "this
                // gateway owns the tag" and "the fleet knows about it".
                let n = crate::libs::eye::state::set_eye_known_tags(macs);
                eprintln!("[MQTT Monitor] EYE fleet allowlist set: {n} MAC(s)");
                Ok(())
            }
            MqttCommand::AddEyeTag { mac, name } => {
                // Persist the tag into `eye.tags[]` so it is tracked/named
                // explicitly (auto-provisioning still discovers unknown tags).
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                if let Some(applier) = config_applier {
                    let result = applier.apply_eye_tag_config(mac.clone(), name.clone());
                    if result.success {
                        // Reflect the change in the monitor's live config so the
                        // scan loop starts tracking the new tag without a restart.
                        if let Some(cfg) = crate::libs::eye::state::eye_config_handle() {
                            if let Ok(mut c) = cfg.write() {
                                c.upsert_tag(&mac, name.as_deref());
                            }
                        }
                        // Seed in-memory state so the tag shows up before its
                        // first advertisement is parsed (uppercase key, matching
                        // the scan loop and remove path).
                        if let Some(handle) = crate::libs::eye::state::eye_state_handle() {
                            if let Ok(mut s) = handle.write() {
                                let entry = s.entry(&mac.to_uppercase(), name.clone());
                                // entry() ignores `name` for an existing tag, so a
                                // rename must be applied explicitly (only when given).
                                if let Some(n) = name {
                                    entry.name = Some(n);
                                }
                            }
                        }
                        eprintln!("[MQTT Monitor] ✓ EYE tag {mac} added to config");
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::RemoveEyeTag { mac } => {
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                if let Some(applier) = config_applier {
                    let result = applier.remove_eye_tag_config(mac.clone());
                    if result.success {
                        // Drop it from the live config too, so the scan loop stops
                        // tracking it and cannot resurrect it on the next advert.
                        if let Some(cfg) = crate::libs::eye::state::eye_config_handle() {
                            if let Ok(mut c) = cfg.write() {
                                c.remove_tag(&mac);
                            }
                        }
                        if let Some(handle) = crate::libs::eye::state::eye_state_handle() {
                            if let Ok(mut s) = handle.write() {
                                s.tags.remove(&mac.to_uppercase());
                            }
                        }
                        eprintln!("[MQTT Monitor] ✓ EYE tag {mac} removed from config");
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::DetectEyeTag { mac } => {
                // Detection runs over raw L2CAP GATT, which must not overlap the
                // active BLE scan — hand off to the EYE monitor via the queue.
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                if crate::libs::eye::state::queue_eye_command(
                    crate::libs::eye::state::EyeCommand::Detect { mac: mac.clone() },
                ) {
                    eprintln!("[MQTT Monitor] Queued EYE detect {mac}");
                    Ok(())
                } else {
                    Err("EYE monitor not running".to_string())
                }
            }
            MqttCommand::SetEyeFieldThreshold {
                mac, field, critical_low, warning_low, warning_high, critical_high,
            } => {
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                if let Some(applier) = config_applier {
                    let result = applier.apply_eye_field_threshold(
                        mac.clone(), field.clone(),
                        critical_low, warning_low, warning_high, critical_high,
                    );
                    if !result.success {
                        return Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()));
                    }
                    // Reflect in the live config so evaluate_alarms uses it next tick.
                    if let Some(cfg) = crate::libs::eye::state::eye_config_handle() {
                        if let Ok(mut c) = cfg.write() {
                            c.set_field_threshold(&mac, crate::libs::config::FieldThreshold {
                                field: field.clone(),
                                critical_low, warning_low, warning_high, critical_high,
                            });
                        }
                    }
                    eprintln!("[MQTT Monitor] ✓ EYE threshold set for {mac} field {field}");
                    Ok(())
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::DeleteEyeFieldThreshold { mac, field } => {
                if !crate::libs::eye::state::is_valid_mac(&mac) {
                    return Err(format!("Invalid MAC address: {mac}"));
                }
                if let Some(applier) = config_applier {
                    let result = applier.delete_eye_field_threshold(mac.clone(), field.clone());
                    if !result.success {
                        return Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()));
                    }
                    if let Some(cfg) = crate::libs::eye::state::eye_config_handle() {
                        if let Ok(mut c) = cfg.write() {
                            c.remove_field_threshold(&mac, &field);
                        }
                    }
                    eprintln!("[MQTT Monitor] ✓ EYE threshold deleted for {mac} field {field}");
                    Ok(())
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::ResetExportCursor { broker_id, stream } => {
                // The reset is two-phase:
                //   1. Persisted SQLite cursor → storage handle (so a restart
                //      reads cursor=0 and replays).
                //   2. In-memory cache on the export orchestrator → export
                //      handle. Without this the drain loop keeps using its
                //      cached cursor value and the operator-issued reset is
                //      silently a no-op until the daemon restarts.
                let Some(storage) = storage_handle.as_ref() else {
                    return Err("Storage handle not available for ResetExportCursor".to_string());
                };
                let export_handle = export_handle_slot.lock().ok().and_then(|g| g.clone());
                let single = [stream.as_str()];
                let streams_to_reset: &[&str] = if stream == "all" {
                    // Must stay in step with `Stream::as_str` / `default_streams()`.
                    // "eye" was missing here, so "reset all cursors" quietly
                    // skipped the one stream an operator is most likely to be
                    // resetting after a history gap.
                    &["sticker", "probe", "probe_1m", "alarm", "eye"]
                } else {
                    &single
                };
                for s in streams_to_reset {
                    if let Err(e) = storage.reset_export_cursor(broker_id.clone(), s.to_string()) {
                        eprintln!(
                            "[MQTT Monitor] reset_export_cursor({}, {}) failed: {}",
                            broker_id, s, e
                        );
                    }
                    if let Some(eh) = export_handle.as_ref() {
                        if let Err(e) = eh.reset_cursor(broker_id.clone(), s.to_string()) {
                            eprintln!(
                                "[MQTT Monitor] export_handle.reset_cursor({}, {}) failed: {}",
                                broker_id, s, e
                            );
                        }
                    }
                }
                if export_handle.is_none() {
                    eprintln!(
                        "[MQTT Monitor] WARN: ResetExportCursor without ExportHandle — \
                         orchestrator's in-memory cache will keep its stale cursor until restart"
                    );
                }
                eprintln!(
                    "[MQTT Monitor] ✓ Export cursor reset for ({}, {})",
                    broker_id, stream
                );
                Ok(())
            }
            // Signer management is handled by the CA platform in CA-based trust model
            MqttCommand::AddSigner { .. }
            | MqttCommand::RemoveSigner { .. }
            | MqttCommand::UpdateSigner { .. } => {
                Err("Signer management not available in CA-based trust model. Use your CA platform to manage user certificates.".to_string())
            }
            _ => {
                eprintln!("[MQTT Monitor] Command execution not implemented: {}", cmd.name());
                Ok(())
            }
        }
    }

    /// Build a PublishConfigState message from current config files and runtime state
    fn build_config_state_message(
        screen_brightness: &Option<SharedScreenBrightnessHandle>,
        screen_timeout: &Option<SharedScreenTimeoutHandle>,
        buzzer_volume: &Option<SharedBuzzerVolumeHandle>,
        led_brightness: u8,
    ) -> Option<MqttMessage> {
        let main_config = crate::libs::config::Config::load_default().ok()?;
        let sensor_config = crate::libs::config::SensorFileConfig::load_default().ok()?;

        let mut sensors = Vec::new();
        for line in 0..8 {
            let line_config = sensor_config.lines.iter().find(|l| l.line == line);
            if let Some(lc) = line_config {
                let thresholds = sensor_config.get_line_thresholds(line);
                let has_override = lc.critical_low_celsius.is_some()
                    || lc.low_alarm_celsius.is_some()
                    || lc.warning_low_celsius.is_some()
                    || lc.warning_high_celsius.is_some()
                    || lc.high_alarm_celsius.is_some()
                    || lc.critical_high_celsius.is_some();
                sensors.push(super::messages::SensorConfigData {
                    line,
                    name: lc.name.clone(),
                    location: lc.location.clone(),
                    enabled: lc.enabled,
                    has_override,
                    thresholds,
                });
            }
        }

        let screen_br = screen_brightness
            .as_ref()
            .map(|sb| sb.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(100);

        let screen_timeout_secs = screen_timeout
            .as_ref()
            .map(|st| st.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(main_config.system.screen_timeout_secs);

        let buzzer_vol = buzzer_volume
            .as_ref()
            .map(|bv| bv.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(main_config.system.buzzer_volume);

        let device_label = main_config.system.device_label.unwrap_or_default();

        let mqtt_config = main_config.mqtt.as_ref();
        let system_info_interval_s = mqtt_config
            .map(|m| m.publish.intervals.system_info_sec)
            .unwrap_or(60);

        // Build LoRaWAN sensor configs
        let lorawan_sensors: Vec<super::messages::LoRaWANSensorConfigData> = main_config
            .lorawan
            .as_ref()
            .map(|lw| {
                lw.sensors
                    .iter()
                    .map(|s| super::messages::LoRaWANSensorConfigData {
                        dev_eui: s.dev_eui.clone(),
                        name: s.name.clone(),
                        serial_number: s.serial_number.clone(),
                        location: s.location.clone(),
                        enabled: s.enabled,
                        field_thresholds: s.field_thresholds.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // EYE subsystem flags, so the viewer's auto-provision / auto-discover
        // toggles reflect the device's real state (None => disabled defaults).
        let eye_cfg = main_config.eye.clone().unwrap_or_default();

        Some(MqttMessage::PublishConfigState {
            led_brightness,
            screen_brightness: screen_br,
            screen_timeout_secs,
            buzzer_volume: buzzer_vol,
            system_info_interval_s,
            device_label,
            sensors,
            lorawan_sensors,
            sample_interval_ms: main_config.sensors.sample_interval_ms,
            aggregation_interval_ms: main_config.sensors.aggregation_interval_ms,
            report_interval_ms: main_config.sensors.report_interval_ms,
            eye_enabled: eye_cfg.enabled,
            eye_auto_provision: eye_cfg.auto_provision,
            // Option<bool> on this branch (it is round-tripped rather than
            // owned — see EyeConfig::auto_discover), so absent reads as off.
            eye_auto_discover: eye_cfg.auto_discover.unwrap_or(false),
        })
    }

    /// Execute network configuration using nmcli
    fn execute_network_config(
        interface: &str,
        config_type: &str,
        ip_address: Option<String>,
        subnet_mask: Option<String>,
        gateway: Option<String>,
        dns_primary: Option<String>,
        dns_secondary: Option<String>,
    ) -> Result<(), String> {
        eprintln!(
            "[MQTT Monitor] Configuring network: {} {}",
            interface, config_type
        );

        // Find connection name for the interface type
        let conn_name = Self::get_nmcli_connection_name(interface)?;
        eprintln!("[MQTT Monitor] Found connection: {}", conn_name);

        if config_type == "dhcp" {
            // Set to DHCP (automatic)
            let output = std::process::Command::new("nmcli")
                .args([
                    "con",
                    "mod",
                    &conn_name,
                    "ipv4.method",
                    "auto",
                    "ipv4.addresses",
                    "",
                    "ipv4.gateway",
                    "",
                    "ipv4.dns",
                    "",
                ])
                .output()
                .map_err(|e| format!("nmcli failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("nmcli mod failed: {}", stderr));
            }
            eprintln!("[MQTT Monitor] Set {} to DHCP", conn_name);
        } else {
            // Static IP configuration
            let ip = ip_address.ok_or("IP address required for static configuration")?;
            let gw = gateway.ok_or("Gateway required for static configuration")?;
            let mask = subnet_mask.unwrap_or_else(|| "255.255.255.0".to_string());
            let cidr = Self::subnet_to_cidr(&mask);

            let ip_with_cidr = format!("{}/{}", ip, cidr);

            // Set static IP
            let output = std::process::Command::new("nmcli")
                .args([
                    "con",
                    "mod",
                    &conn_name,
                    "ipv4.method",
                    "manual",
                    "ipv4.addresses",
                    &ip_with_cidr,
                    "ipv4.gateway",
                    &gw,
                ])
                .output()
                .map_err(|e| format!("nmcli failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("nmcli mod failed: {}", stderr));
            }
            eprintln!(
                "[MQTT Monitor] Set {} to static IP: {}",
                conn_name, ip_with_cidr
            );

            // Set DNS if provided
            if let Some(dns) = dns_primary {
                let dns_str = match dns_secondary {
                    Some(ref s) => format!("{},{}", dns, s),
                    None => dns,
                };

                let output = std::process::Command::new("nmcli")
                    .args(["con", "mod", &conn_name, "ipv4.dns", &dns_str])
                    .output()
                    .map_err(|e| format!("nmcli dns failed: {}", e))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    eprintln!("[MQTT Monitor] Warning: Failed to set DNS: {}", stderr);
                } else {
                    eprintln!("[MQTT Monitor] Set DNS: {}", dns_str);
                }
            }
        }

        // Restart connection to apply changes
        eprintln!("[MQTT Monitor] Restarting connection {}...", conn_name);
        let _ = std::process::Command::new("nmcli")
            .args(["con", "down", &conn_name])
            .output();

        let output = std::process::Command::new("nmcli")
            .args(["con", "up", &conn_name])
            .output()
            .map_err(|e| format!("Failed to restart connection: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Failed to bring up connection: {}", stderr));
        }

        eprintln!("[MQTT Monitor] ✓ Network configuration applied successfully");
        Ok(())
    }

    /// Get NetworkManager connection name for interface type
    fn get_nmcli_connection_name(interface: &str) -> Result<String, String> {
        let target_type = if interface == "ethernet" {
            "802-3-ethernet"
        } else {
            "802-11-wireless"
        };

        let output = std::process::Command::new("nmcli")
            .args(["-t", "-f", "NAME,TYPE", "con", "show"])
            .output()
            .map_err(|e| format!("nmcli failed: {}", e))?;

        if !output.status.success() {
            return Err("Failed to list connections".to_string());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.ends_with(target_type) {
                // Format is "NAME:TYPE", so split and get the name
                if let Some(name) = line.rsplit(':').nth(1) {
                    return Ok(name.to_string());
                }
                // Fallback: take everything before the last colon
                if let Some(colon_pos) = line.rfind(':') {
                    return Ok(line[..colon_pos].to_string());
                }
            }
        }

        Err(format!("No {} connection found", interface))
    }

    /// Convert subnet mask to CIDR notation
    fn subnet_to_cidr(mask: &str) -> u8 {
        mask.split('.')
            .filter_map(|p| p.parse::<u8>().ok())
            .map(|b| b.count_ones() as u8)
            .sum()
    }
}

impl Drop for MqttMonitor {
    fn drop(&mut self) {
        // Signal shutdown
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for thread to finish
        if let Some(handle) = self.thread_handle.take() {
            eprintln!("[MQTT Monitor] Waiting for MQTT thread to finish...");
            let _ = handle.join();
            eprintln!("[MQTT Monitor] MQTT thread finished");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::config::{
        BrokerConfig, ConnectionConfig, LastWillConfig, MqttConfig, PublishConfig,
        PublishIntervals, QosOverrides, SubscribeConfig, TlsConfig,
    };

    /// A real (if minimal) self-signed EC cert + PKCS#8 key, generated with
    /// `openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes
    /// -keyout client.key -out client.crt -days 3650 -subj "/CN=test-ca"`.
    /// native-tls (OpenSSL) parses certs eagerly in `configure_tls_transport`,
    /// unlike the old rustls path which only parsed lazily at connect time —
    /// so these tests need cert/key bytes that actually parse, not just
    /// readable placeholder bytes.
    const TEST_CERT_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\n\
        MIIBeDCCAR+gAwIBAgIUDqERgDyrXiAn4TmRg4BCLjplUXAwCgYIKoZIzj0EAwIw\n\
        EjEQMA4GA1UEAwwHdGVzdC1jYTAeFw0yNjA4MTAxMTQ0MTVaFw0zNjA4MDcxMTQ0\n\
        MTVaMBIxEDAOBgNVBAMMB3Rlc3QtY2EwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNC\n\
        AASstCEzKS48I9HdjALNpj15aYee/0Z2Z1Ua5cFDEUTh0thOvLdvel6VaprXndvZ\n\
        TuxZS3kuZoKQKB2Lq3XC4rAuo1MwUTAdBgNVHQ4EFgQU7+wYZ5Z2G4UIA8neLekq\n\
        QHXc+9YwHwYDVR0jBBgwFoAU7+wYZ5Z2G4UIA8neLekqQHXc+9YwDwYDVR0TAQH/\n\
        BAUwAwEB/zAKBggqhkjOPQQDAgNHADBEAiBzoNL/WWuustUfmczgH04LqyMSFSMX\n\
        ptdzt7JIzQjk5wIgaRcADc9eQR03gCtblxK+oFDjxhAzbjiiRJAQHEE6n/s=\n\
        -----END CERTIFICATE-----\n";

    /// PKCS#8 private key matching [`TEST_CERT_PEM`].
    const TEST_KEY_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\n\
        MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgalCRvP+JKO9Dds0n\n\
        FOYAHf1uxXGUF/tDP1ZNfs98KsehRANCAASstCEzKS48I9HdjALNpj15aYee/0Z2\n\
        Z1Ua5cFDEUTh0thOvLdvel6VaprXndvZTuxZS3kuZoKQKB2Lq3XC4rAu\n\
        -----END PRIVATE KEY-----\n";

    /// Build a minimal MqttConfig for testing.
    fn test_mqtt_config(tls: Option<TlsConfig>, port: u16) -> MqttConfig {
        MqttConfig {
            enabled: true,
            broker: BrokerConfig {
                host: "mqtt.example.com".to_string(),
                port,
                client_id: "test-device".to_string(),
                username: None,
                password: None,
            },
            tls,
            publish: PublishConfig {
                topic_prefix: "fiber".to_string(),
                include_hostname: true,
                default_qos: 0,
                qos_overrides: QosOverrides {
                    sensor_readings: 0,
                    power_status: 1,
                    alarm_events: 2,
                    power_events: 2,
                    network_status: 0,
                },
                intervals: PublishIntervals {
                    sensors_sec: 5,
                    power_sec: 10,
                    network_sec: 30,
                    system_info_sec: 60,
                },
                max_queue_size: 1000,
            },
            subscribe: SubscribeConfig {
                enabled: false,
                max_commands_per_second: 10,
                audit_enabled: false,
            },
            connection: ConnectionConfig {
                keep_alive_sec: 60,
                connection_timeout_sec: 30,
                max_reconnect_attempts: 0,
                reconnect_delay_sec: 1,
                max_reconnect_delay_sec: 30,
                clean_session: true,
            },
            last_will: LastWillConfig {
                enabled: false,
                topic: "status".to_string(),
                payload: r#"{"status":"offline"}"#.to_string(),
                qos: 1,
                retain: true,
            },
            export: Default::default(),
        }
    }

    #[test]
    fn test_create_mqtt_options_no_tls() {
        let config = test_mqtt_config(None, 1883);
        let opts = create_mqtt_options(&config, "testhost", "test-client");
        let (host, port) = opts.broker_address();
        assert_eq!(host, "mqtt.example.com");
        assert_eq!(
            port, 1883,
            "Port should remain 1883 when TLS is not configured"
        );
    }

    #[test]
    fn test_create_mqtt_options_tls_disabled() {
        let tls = TlsConfig {
            enabled: false,
            ca_cert_path: "/nonexistent/ca.crt".to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let config = test_mqtt_config(Some(tls), 1883);
        let opts = create_mqtt_options(&config, "testhost", "test-client");
        let (_, port) = opts.broker_address();
        assert_eq!(port, 1883, "Port should remain 1883 when TLS is disabled");
    }

    #[test]
    fn test_create_mqtt_options_tls_enabled_default_port_override() {
        // The port override only happens when configure_tls_transport()
        // succeeds, and native-tls (OpenSSL) parses the CA cert eagerly, so
        // this needs a real cert rather than a readable placeholder -- see
        // test_configure_tls_transport_valid_ca_file.
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let ca_path = dir.path().join("ca.crt");
        std::fs::write(&ca_path, TEST_CERT_PEM).expect("Failed to write CA file");

        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: ca_path.to_string_lossy().to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let config = test_mqtt_config(Some(tls), 1883);
        let opts = create_mqtt_options(&config, "testhost", "test-client");
        let (_, port) = opts.broker_address();
        assert_eq!(
            port, 8883,
            "Port should be overridden to 8883 when TLS is enabled and port was 1883"
        );
    }

    #[test]
    fn test_create_mqtt_options_tls_enabled_custom_port_preserved() {
        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: "/nonexistent/ca.crt".to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let config = test_mqtt_config(Some(tls), 9883);
        let opts = create_mqtt_options(&config, "testhost", "test-client");
        let (_, port) = opts.broker_address();
        assert_eq!(
            port, 9883,
            "Custom port should be preserved even when TLS is enabled"
        );
    }

    #[test]
    fn test_configure_tls_transport_missing_ca_file() {
        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: "/nonexistent/path/ca.crt".to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let result = configure_tls_transport(&tls);
        assert!(
            result.is_err(),
            "Should fail when CA cert file does not exist"
        );
        let err = result.err().unwrap();
        assert!(
            err.contains("Failed to read CA certificate"),
            "Error should mention CA cert: {}",
            err
        );
    }

    #[test]
    fn test_configure_tls_transport_valid_ca_file() {
        // Create a temporary PEM file with a real self-signed CA cert --
        // native-tls parses it eagerly, so it must actually be valid PEM/DER.
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let ca_path = dir.path().join("ca.crt");
        std::fs::write(&ca_path, TEST_CERT_PEM).expect("Failed to write CA file");

        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: ca_path.to_string_lossy().to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let result = configure_tls_transport(&tls);
        assert!(
            result.is_ok(),
            "Should succeed with a readable CA cert file: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_configure_tls_transport_empty_ca_file() {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let ca_path = dir.path().join("empty_ca.crt");
        std::fs::write(&ca_path, b"").expect("Failed to write empty CA file");

        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: ca_path.to_string_lossy().to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let result = configure_tls_transport(&tls);
        assert!(result.is_err(), "Should fail with empty CA cert file");
        assert!(result.err().unwrap().contains("is empty"));
    }

    #[test]
    fn test_configure_tls_transport_mismatched_client_auth() {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let ca_path = dir.path().join("ca.crt");
        let fake_pem = b"-----BEGIN CERTIFICATE-----\n\
            MIIBkTCB+wIJALRiMLAh2wG7MA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl\n\
            c3RjYTAeFw0yNDA0MjEwMDAwMDBaFw0yNTA0MjEwMDAwMDBaMBExDzANBgNVBAMM\n\
            BnRlc3RjYTBcMA0GCSqGSIb3DQEBAQUAA0sAMEgCQQC7o96Gahm8KzEGRE+HAWKL\n\
            hJJmbnRqH3UbMYvsIjmAtWBbJdU7FE4WBMhHc9cCq7YTEPHRROAKJ7mMEy0+SCCB\n\
            AgMBAAEwDQYJKoZIhvcNAQELBQADQQBR0sMEBcZykPk6DfbEbuCHuqSGgkDE\n\
            -----END CERTIFICATE-----\n";
        std::fs::write(&ca_path, fake_pem).unwrap();
        let ca_str = ca_path.to_string_lossy().to_string();

        // Only cert_path set, no key_path -> error
        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: ca_str.clone(),
            client_cert_path: Some("/some/cert.pem".to_string()),
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let result = configure_tls_transport(&tls);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(
            err.contains("client_key_path is missing"),
            "Should detect missing key when cert is present, got: {}",
            err
        );

        // Only key_path set, no cert_path -> error
        let tls2 = TlsConfig {
            enabled: true,
            ca_cert_path: ca_str,
            client_cert_path: None,
            client_key_path: Some("/some/key.pem".to_string()),
            insecure_skip_verify: false,
        };
        let result2 = configure_tls_transport(&tls2);
        assert!(result2.is_err());
        let err2 = result2.err().unwrap();
        assert!(
            err2.contains("client_cert_path is missing"),
            "Should detect missing cert when key is present, got: {}",
            err2
        );
    }

    #[test]
    fn test_configure_tls_transport_with_client_auth() {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let ca_path = dir.path().join("ca.crt");
        let cert_path = dir.path().join("client.crt");
        let key_path = dir.path().join("client.key");

        // native-tls parses both the CA and the client identity eagerly, so
        // these need to be real cert/key PEM, not placeholder content.
        std::fs::write(&ca_path, TEST_CERT_PEM).unwrap();
        std::fs::write(&cert_path, TEST_CERT_PEM).unwrap();
        std::fs::write(&key_path, TEST_KEY_PEM).unwrap();

        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: ca_path.to_string_lossy().to_string(),
            client_cert_path: Some(cert_path.to_string_lossy().to_string()),
            client_key_path: Some(key_path.to_string_lossy().to_string()),
            insecure_skip_verify: false,
        };
        let result = configure_tls_transport(&tls);
        assert!(
            result.is_ok(),
            "Should succeed loading CA, client cert, and key files: {:?}",
            result.err()
        );
    }

    #[cfg(feature = "dev-platform")]
    #[test]
    fn test_build_dev_command_eye_arms() {
        use serde_json::json;

        // add_eye_tag: MAC uppercased, name preserved
        match MqttMonitor::build_dev_command(
            "add_eye_tag",
            &json!({"mac": "aa:bb:cc:dd:ee:ff", "name": "Freezer"}),
            &None,
        )
        .unwrap()
        {
            MqttCommand::AddEyeTag { mac, name } => {
                assert_eq!(mac, "AA:BB:CC:DD:EE:FF");
                assert_eq!(name.as_deref(), Some("Freezer"));
            }
            other => panic!("expected AddEyeTag, got {other:?}"),
        }

        // add_eye_tag: empty name -> None
        assert!(matches!(
            MqttMonitor::build_dev_command(
                "add_eye_tag",
                &json!({"mac": "AA:BB:CC:DD:EE:FF", "name": ""}),
                &None,
            )
            .unwrap(),
            MqttCommand::AddEyeTag { name: None, .. }
        ));

        // remove_eye_tag / detect_eye_tag uppercase the MAC
        assert!(matches!(
            MqttMonitor::build_dev_command("remove_eye_tag", &json!({"mac": "aa:bb:cc:dd:ee:ff"}), &None).unwrap(),
            MqttCommand::RemoveEyeTag { mac } if mac == "AA:BB:CC:DD:EE:FF"
        ));
        assert!(matches!(
            MqttMonitor::build_dev_command("detect_eye_tag", &json!({"mac": "aa:bb:cc:dd:ee:ff"}), &None).unwrap(),
            MqttCommand::DetectEyeTag { mac } if mac == "AA:BB:CC:DD:EE:FF"
        ));

        // set_eye_recording: valid interval accepted, 0 = off accepted, invalid rejected
        assert!(matches!(
            MqttMonitor::build_dev_command(
                "set_eye_recording",
                &json!({"mac": "AA:BB:CC:DD:EE:FF", "interval_min": 5}),
                &None
            )
            .unwrap(),
            MqttCommand::SetEyeRecording {
                interval_min: 5,
                ..
            }
        ));
        assert!(matches!(
            MqttMonitor::build_dev_command(
                "set_eye_recording",
                &json!({"mac": "AA:BB:CC:DD:EE:FF", "interval_min": 0}),
                &None
            )
            .unwrap(),
            MqttCommand::SetEyeRecording {
                interval_min: 0,
                ..
            }
        ));
        assert!(MqttMonitor::build_dev_command(
            "set_eye_recording",
            &json!({"mac": "AA:BB:CC:DD:EE:FF", "interval_min": 7}),
            &None
        )
        .is_err());

        // download_eye_history uppercases the MAC
        assert!(matches!(
            MqttMonitor::build_dev_command("download_eye_history", &json!({"mac": "aa:bb:cc:dd:ee:ff"}), &None).unwrap(),
            MqttCommand::DownloadEyeHistory { mac } if mac == "AA:BB:CC:DD:EE:FF"
        ));

        // set/delete_eye_field_threshold: MAC uppercased, field + bounds parsed
        match MqttMonitor::build_dev_command(
            "set_eye_field_threshold",
            &json!({"mac": "aa:bb:cc:dd:ee:ff", "field": "battery", "warning_low": 2700.0, "critical_low": 2400.0}),
            &None,
        )
        .unwrap()
        {
            MqttCommand::SetEyeFieldThreshold { mac, field, warning_low, critical_low, .. } => {
                assert_eq!(mac, "AA:BB:CC:DD:EE:FF");
                assert_eq!(field, "battery");
                assert_eq!(warning_low, Some(2700.0));
                assert_eq!(critical_low, Some(2400.0));
            }
            other => panic!("expected SetEyeFieldThreshold, got {other:?}"),
        }
        assert!(matches!(
            MqttMonitor::build_dev_command("delete_eye_field_threshold", &json!({"mac": "aa:bb:cc:dd:ee:ff", "field": "movement"}), &None).unwrap(),
            MqttCommand::DeleteEyeFieldThreshold { mac, field } if mac == "AA:BB:CC:DD:EE:FF" && field == "movement"
        ));

        // malformed MAC rejected
        assert!(
            MqttMonitor::build_dev_command("add_eye_tag", &json!({"mac": "not-a-mac"}), &None)
                .is_err()
        );
    }

    #[test]
    fn power_off_is_not_a_teardown_command() {
        // Teardown classification decides whether the SUCCESS ack is published
        // BEFORE execution. A power-off used to halt the SoC, so it had to ack
        // first — the ack would otherwise queue behind a halt that never lets the
        // event loop run again. Now it enters standby: the process and the MQTT
        // connection both survive, and it can legitimately refuse (a standby it
        // cannot persist must not happen). So it must be acked from its real
        // result, or a device that stayed up monitoring would report itself off.
        assert!(!MqttMonitor::is_teardown_command(
            &MqttCommand::PowerOffDevice {
                reason: "decommissioned".to_string(),
                requested_by: "dr.jane@hospital.eu".to_string(),
            }
        ));

        // The genuine teardowns must stay classified.
        assert!(MqttMonitor::is_teardown_command(
            &MqttCommand::RestartApplication {
                reason: "r".to_string(),
                requested_by: "dr.jane@hospital.eu".to_string(),
            }
        ));

        // And an ordinary config change must NOT be, or it would be acked
        // optimistically before anyone knows whether it applied.
        assert!(!MqttMonitor::is_teardown_command(
            &MqttCommand::SetLedBrightness { brightness: 50 }
        ));
    }

    #[test]
    fn confirm_response_message_reports_execution_result() {
        let make = || MqttMessage::PublishConfigResponse {
            challenge_id: "chal-1".to_string(),
            request_id: "req-1".to_string(),
            status: "SUCCESS".to_string(),
            applied_at: Some(123),
            effective_at: Some(123),
            message: "Configuration applied: set_led_brightness".to_string(),
        };

        // Ok -> the pre-built SUCCESS response passes through unchanged.
        match MqttMonitor::confirm_response_message(Ok(()), make()) {
            MqttMessage::PublishConfigResponse {
                status, applied_at, ..
            } => {
                assert_eq!(status, "SUCCESS");
                assert_eq!(applied_at, Some(123));
            }
            _ => panic!("expected PublishConfigResponse"),
        }

        // Err -> ERROR response, same ids, nulled timestamps, error in message.
        match MqttMonitor::confirm_response_message(Err("boom".to_string()), make()) {
            MqttMessage::PublishConfigResponse {
                challenge_id,
                request_id,
                status,
                applied_at,
                effective_at,
                message,
            } => {
                assert_eq!(challenge_id, "chal-1");
                assert_eq!(request_id, "req-1");
                assert_eq!(status, "ERROR");
                assert_eq!(applied_at, None);
                assert_eq!(effective_at, None);
                assert!(message.contains("boom"), "message was: {message}");
            }
            _ => panic!("expected PublishConfigResponse"),
        }
    }

    /// The derivation under test, isolated from the handle so a unit test does not
    /// need a live monitor: mirrors `sticker_command_timeout`'s clamp.
    fn derive(cadence: Option<u64>) -> std::time::Duration {
        match cadence {
            Some(secs) if secs > 0 => {
                std::time::Duration::from_secs_f64(secs as f64 * STICKER_COMMAND_CADENCE_FACTOR)
                    .clamp(STICKER_COMMAND_TIMEOUT, STICKER_COMMAND_TIMEOUT_CAP)
            }
            _ => STICKER_COMMAND_TIMEOUT_UNKNOWN,
        }
    }

    #[test]
    fn fport85_timeout_is_patient_when_the_cadence_is_unknown() {
        // Not the floor: "unknown" cannot rule out a slow sticker, and assuming a
        // fast one is what made the first read after a restart the likeliest to fail.
        assert_eq!(derive(None), STICKER_COMMAND_TIMEOUT_UNKNOWN);
        assert_eq!(derive(Some(0)), STICKER_COMMAND_TIMEOUT_UNKNOWN);
        assert!(STICKER_COMMAND_TIMEOUT_UNKNOWN > STICKER_COMMAND_TIMEOUT);
    }

    #[test]
    fn fport85_timeout_keeps_the_floor_for_fast_stickers() {
        // A 60 s sticker needs 150 s by the factor, which is under the floor — the
        // previous behaviour must be preserved for everything that already worked.
        assert_eq!(derive(Some(60)), STICKER_COMMAND_TIMEOUT);
    }

    #[test]
    fn fport85_timeout_covers_a_900s_sticker() {
        // The reported case. Every chunk of a config read used to expire at the fixed
        // 180 s — before the sticker's next RX window — so the read returned nothing.
        let t = derive(Some(900));
        assert!(
            t.as_secs() >= 2 * 900,
            "must allow at least two reporting cycles, got {}s",
            t.as_secs()
        );
        assert_eq!(t.as_secs(), 2250);
    }

    #[test]
    fn fport85_timeout_keeps_scaling_well_past_900s() {
        // interval_report accepts up to 86400 s, so the derivation must not stop
        // being useful just past the interval someone happened to test with.
        for cadence in [900u64, 1800, 3600, 7200] {
            assert_eq!(
                derive(Some(cadence)).as_secs(),
                (cadence as f64 * STICKER_COMMAND_CADENCE_FACTOR) as u64,
                "cadence {cadence}s must derive exactly, not hit a cap"
            );
        }
    }

    #[test]
    fn fport85_timeout_is_capped_for_an_absurd_cadence() {
        // A ceiling still exists, but only where a full read is hopeless anyway —
        // and the derivation logs when it bites, so it is never silent.
        assert_eq!(derive(Some(24 * 3600)), STICKER_COMMAND_TIMEOUT_CAP);
        assert!(STICKER_COMMAND_TIMEOUT_CAP.as_secs() >= 6 * 3600);
    }
}

#[cfg(test)]
mod publish_bridge_tests {
    use super::*;

    /// The property whose absence broke every periodic publish.
    ///
    /// The event loop drained the publish queue with `crossbeam`'s blocking
    /// `recv_timeout(100ms)` inside a `tokio::select!` arm. That call is
    /// synchronous and all of its outcomes fall through, so the arm returned
    /// `Ready` on its first poll every time — after 0 ms with a message queued,
    /// after 100 ms of blocked worker thread without one. `select!`
    /// short-circuits at the first ready arm and drops the others, so each
    /// freshly built `sleep(100ms)` in the periodic arms was polled once at
    /// elapsed = 0 and thrown away before it could be re-polled past its
    /// deadline. `system/info` was therefore never published at all.
    ///
    /// An idle receive must *pend*. If this assertion ever fails, the periodic
    /// arms are starved again.
    #[tokio::test]
    async fn idle_receive_pends_instead_of_returning_ready() {
        let (_tx, receiver) = bounded::<MqttMessage>(8);
        let mut rx = MqttMonitor::spawn_publish_bridge(receiver, 8);

        let outcome = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;

        assert!(
            outcome.is_err(),
            "an empty queue must leave the select arm pending, not resolve it"
        );
    }

    #[tokio::test]
    async fn forwards_queued_messages_in_order() {
        let (tx, receiver) = bounded::<MqttMessage>(8);
        let mut rx = MqttMonitor::spawn_publish_bridge(receiver, 8);

        tx.send(MqttMessage::Shutdown).unwrap();
        tx.send(MqttMessage::Shutdown).unwrap();

        for _ in 0..2 {
            let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("bridge should forward promptly")
                .expect("channel should still be open");
            assert!(matches!(msg, MqttMessage::Shutdown));
        }
    }

    /// Dropping every sender must close the bridge, so the loop sees `None` and
    /// shuts down rather than spinning on a dead queue.
    #[tokio::test]
    async fn closes_when_all_senders_drop() {
        let (tx, receiver) = bounded::<MqttMessage>(8);
        let mut rx = MqttMonitor::spawn_publish_bridge(receiver, 8);

        drop(tx);

        let outcome = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("bridge should notice the disconnect");
        assert!(outcome.is_none(), "expected the channel to be closed");
    }

    /// A zero-capacity queue must not panic — `tokio::sync::mpsc::channel(0)`
    /// does, hence the `.max(1)` in the bridge.
    #[tokio::test]
    async fn tolerates_a_zero_capacity_queue() {
        let (tx, receiver) = bounded::<MqttMessage>(1);
        let mut rx = MqttMonitor::spawn_publish_bridge(receiver, 0);

        tx.send(MqttMessage::Shutdown).unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("bridge should forward")
            .expect("channel open");
        assert!(matches!(msg, MqttMessage::Shutdown));
    }

    /// Characterisation of the loop shape the fix relies on: a periodic arm must
    /// keep firing while the message arm is permanently ready.
    ///
    /// This exercises the concurrency pattern, not `monitor_loop` itself (which
    /// needs a broker and hardware), so treat it as documentation of *why* the
    /// arms are `interval.tick()` rather than `sleep`-and-check.
    #[tokio::test]
    async fn periodic_arm_still_fires_while_message_arm_is_saturated() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(4);

        // Keep the message arm ready for the whole test.
        let feeder = tokio::spawn(async move {
            while tx.send(1).await.is_ok() {
                tokio::task::yield_now().await;
            }
        });

        let mut tick = tokio::time::interval(Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut ticks = 0u32;
        let mut msgs = 0u32;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(600);

        while tokio::time::Instant::now() < deadline && ticks < 5 {
            tokio::select! {
                _ = tick.tick() => ticks += 1,
                Some(_) = rx.recv() => msgs += 1,
            }
        }

        feeder.abort();

        assert!(
            msgs > 0,
            "message arm never ran, test is not exercising contention"
        );
        assert!(
            ticks >= 5,
            "periodic arm was starved: only {ticks} ticks fired"
        );
    }
}

/// Executable demonstration of the bug this module was fixed for.
///
/// Kept because the diagnosis is subtle and counter-intuitive: it looks like a
/// load-dependent race, but the periodic arms could not fire *at all*. These two
/// tests contrast the old and new loop shapes side by side so the next person to
/// touch the event loop can see why it is written the way it is.
#[cfg(test)]
mod select_loop_shape_tests {
    use super::*;

    /// The old shape: a blocking `crossbeam::recv_timeout` arm beside a
    /// `sleep`-and-check arm. The periodic arm never fires — not rarely, never.
    ///
    /// `recv_timeout` is synchronous and every outcome falls through, so its arm
    /// returns `Ready` on first poll. `select!` short-circuits there and drops
    /// the other arms' futures, so the freshly built `sleep` is polled once at
    /// elapsed = 0 and discarded before it can be re-polled past its deadline.
    #[tokio::test]
    async fn old_shape_starves_the_periodic_arm() {
        let (tx, rx) = bounded::<u8>(4);

        // Feed the queue from a plain thread, as the real senders do.
        let feeder = thread::spawn(move || {
            while tx.send(1).is_ok() {
                thread::sleep(Duration::from_millis(1));
            }
        });

        let mut ticks = 0u32;
        let mut msgs = 0u32;
        let started = std::time::Instant::now();
        let mut last_periodic = Instant::now();

        while started.elapsed() < Duration::from_millis(400) {
            tokio::select! {
                _ = async {
                    // Verbatim the old channel arm.
                    let _ = rx.recv_timeout(Duration::from_millis(100));
                } => msgs += 1,

                _ = async {
                    // Verbatim the old periodic arm, with a 10ms period so a
                    // working implementation would tick ~40 times.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if last_periodic.elapsed() > Duration::from_millis(10) {
                        last_periodic = Instant::now();
                        ticks += 1;
                    }
                } => {}
            }
        }

        drop(rx);
        let _ = feeder.join();

        assert!(msgs > 0, "channel arm should have run");
        assert_eq!(
            ticks, 0,
            "this is the bug: the periodic arm fired {ticks} times, so the \
             starvation this module works around no longer reproduces — \
             re-check whether the fix is still necessary"
        );
    }

    /// The new shape: an awaitable receive beside `interval.tick()`. Both arms
    /// make progress.
    #[tokio::test]
    async fn new_shape_lets_both_arms_progress() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(4);

        let feeder = tokio::spawn(async move {
            while tx.send(1).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        let mut tick = tokio::time::interval(Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut ticks = 0u32;
        let mut msgs = 0u32;
        let started = std::time::Instant::now();

        while started.elapsed() < Duration::from_millis(400) {
            tokio::select! {
                _ = tick.tick() => ticks += 1,
                Some(_) = rx.recv() => msgs += 1,
            }
        }

        feeder.abort();

        assert!(msgs > 0, "channel arm should have run");
        assert!(
            ticks >= 5,
            "periodic arm should tick freely, got only {ticks}"
        );
    }
}
