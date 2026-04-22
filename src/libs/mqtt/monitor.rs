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
use super::publisher::MqttPublisher;
use super::subscriber::MqttSubscriber;
use super::topics::TopicBuilder;

use crate::libs::authorization::AuthorizationManager;
use crate::libs::config_applier::ConfigApplier;
use crate::libs::crypto::{CARegistry, NonceTracker, SignatureVerifier};
use crate::libs::pairing::PairingHandle;
use std::sync::Mutex;

/// Shared pairing handle that can be set after MQTT monitor is created
pub type SharedPairingHandle = Arc<Mutex<Option<PairingHandle>>>;

/// Shared STM bridge for hardware commands
pub type SharedStmBridge = Arc<Mutex<crate::drivers::StmBridge>>;

/// Shared screen brightness handle for display backlight control
pub type SharedScreenBrightnessHandle = std::sync::Arc<std::sync::atomic::AtomicU8>;

/// Shared buzzer volume handle (0 = muted, 1-100 = active)
pub type SharedBuzzerVolumeHandle = std::sync::Arc<std::sync::atomic::AtomicU8>;

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
            self.base_delay_sec.saturating_mul(2_u64.pow(self.attempt_count)),
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
    let mut mqttoptions = MqttOptions::new(
        client_id,
        config.broker.host.clone(),
        config.broker.port,
    );

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
                        mqttoptions.set_keep_alive(Duration::from_secs(config.connection.keep_alive_sec));
                        mqttoptions.set_clean_session(config.connection.clean_session);
                        if let (Some(u), Some(p)) = (&config.broker.username, &config.broker.password) {
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
                        eprintln!("[MQTT Monitor] WARNING: TLS cert not found, falling back to plaintext: {}", e);
                    } else {
                        eprintln!("[MQTT Monitor] FATAL: Failed to configure TLS transport: {}", e);
                        eprintln!("[MQTT Monitor] Refusing to connect over plaintext when TLS is enabled");
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
/// Uses `TlsConfiguration::Simple` which accepts PEM-encoded CA cert bytes
/// and optional PEM-encoded client cert + key for mutual TLS.
fn configure_tls_transport(
    tls: &crate::libs::config::TlsConfig,
) -> Result<Transport, String> {
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
                format!("Failed to read client certificate from '{}': {}", cert_path, e)
            })?;
            let key = std::fs::read(key_path).map_err(|e| {
                format!("Failed to read client key from '{}': {}", key_path, e)
            })?;

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

    let transport = if tls.insecure_skip_verify {
        eprintln!("[MQTT TLS] WARNING: insecure_skip_verify=true — skipping certificate validation");
        // Build a rustls ClientConfig that skips cert verification
        use rumqttc::tokio_rustls::rustls;

        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
            .with_no_client_auth();

        transport_from_rustls_config(config)
    } else {
        Transport::tls_with_config(TlsConfiguration::Simple {
            ca,
            alpn: None,
            client_auth,
        })
    };

    Ok(transport)
}

/// Certificate verifier that accepts any certificate (insecure_skip_verify mode)
/// Used for device-to-device TLS on local medical networks with self-signed certs
#[derive(Debug)]
struct NoCertVerifier;

impl rumqttc::tokio_rustls::rustls::client::danger::ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rumqttc::tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rumqttc::tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &rumqttc::tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rumqttc::tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<rumqttc::tokio_rustls::rustls::client::danger::ServerCertVerified, rumqttc::tokio_rustls::rustls::Error> {
        Ok(rumqttc::tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rumqttc::tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &rumqttc::tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<rumqttc::tokio_rustls::rustls::client::danger::HandshakeSignatureValid, rumqttc::tokio_rustls::rustls::Error> {
        Ok(rumqttc::tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rumqttc::tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &rumqttc::tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<rumqttc::tokio_rustls::rustls::client::danger::HandshakeSignatureValid, rumqttc::tokio_rustls::rustls::Error> {
        Ok(rumqttc::tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rumqttc::tokio_rustls::rustls::SignatureScheme> {
        use rumqttc::tokio_rustls::rustls::SignatureScheme;
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
        ]
    }
}

fn transport_from_rustls_config(config: rumqttc::tokio_rustls::rustls::ClientConfig) -> Transport {
    Transport::tls_with_config(TlsConfiguration::Rustls(Arc::new(config)))
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
    pub fn send_aggregated_sensor_data(&self, period: crate::libs::sensors::aggregation::AggregationPeriod, names: [String; 8], locations: [Option<String>; 8]) {
        self.send(MqttMessage::PublishAggregatedSensorData { period, names, locations });
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
pub struct MqttMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    connection_state: SharedConnectionState,
    handle: MqttHandle,
    pairing_handle: SharedPairingHandle,
    stm_bridge: Option<SharedStmBridge>,
    screen_brightness: Option<SharedScreenBrightnessHandle>,
    buzzer_volume: Option<SharedBuzzerVolumeHandle>,
    buzzer_priority: Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
}

impl MqttMonitor {
    /// Create and spawn MQTT monitor thread
    pub fn new(config: MqttConfig, hostname: String, power_status: crate::libs::power::status::SharedPowerStatus) -> io::Result<Self> {
        Self::new_with_stm(config, hostname, power_status, None, None, None, None)
    }

    /// Create and spawn MQTT monitor thread with optional STM bridge for hardware commands
    pub fn new_with_stm(
        config: MqttConfig,
        hostname: String,
        power_status: crate::libs::power::status::SharedPowerStatus,
        stm_bridge: Option<SharedStmBridge>,
        screen_brightness: Option<SharedScreenBrightnessHandle>,
        buzzer_volume: Option<SharedBuzzerVolumeHandle>,
        buzzer_priority: Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
    ) -> io::Result<Self> {
        eprintln!("[MQTT Monitor] Initializing MQTT monitor for host: {}", hostname);
        eprintln!("[MQTT Monitor] Broker: {}:{}", config.broker.host, config.broker.port);
        if stm_bridge.is_some() {
            eprintln!("[MQTT Monitor] STM bridge available for hardware commands");
        }
        if screen_brightness.is_some() {
            eprintln!("[MQTT Monitor] Screen brightness control available");
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
        let handle = MqttHandle { sender, reconnected_flag: reconnected_flag.clone() };
        let handle_clone = handle.clone();

        // Clone STM bridge for monitor thread
        let stm_bridge_clone = stm_bridge.clone();

        // Clone screen brightness for monitor thread
        let screen_brightness_clone = screen_brightness.clone();

        // Clone buzzer volume and priority for monitor thread
        let buzzer_volume_clone = buzzer_volume.clone();
        let buzzer_priority_clone = buzzer_priority.clone();

        // Clone reconnect flag for monitor thread
        let reconnected_flag_clone = reconnected_flag.clone();

        // Spawn monitoring thread
        let thread_handle = thread::spawn(move || {
            if let Err(e) = Self::monitor_loop(
                config,
                hostname,
                receiver,
                shutdown_flag_clone,
                connection_state_clone,
                pairing_handle_clone,
                power_status,
                stm_bridge_clone,
                screen_brightness_clone,
                buzzer_volume_clone,
                buzzer_priority_clone,
                reconnected_flag_clone,
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
            buzzer_volume,
            buzzer_priority,
        })
    }

    /// Set the pairing handle (call after PairingMonitor is created)
    pub fn set_pairing_handle(&self, handle: PairingHandle) {
        if let Ok(mut ph) = self.pairing_handle.lock() {
            *ph = Some(handle);
            eprintln!("[MQTT Monitor] Pairing handle set");
        }
    }

    /// Get handle for sending messages
    pub fn handle(&self) -> MqttHandle {
        self.handle.clone()
    }

    /// Get connection state
    pub fn connection_state(&self) -> SharedConnectionState {
        self.connection_state.clone()
    }

    /// Main monitoring loop (runs in background thread)
    fn monitor_loop(
        config: MqttConfig,
        hostname: String,
        receiver: Receiver<MqttMessage>,
        shutdown_flag: Arc<AtomicBool>,
        connection_state: SharedConnectionState,
        pairing_handle: SharedPairingHandle,
        power_status: crate::libs::power::status::SharedPowerStatus,
        stm_bridge: Option<SharedStmBridge>,
        screen_brightness: Option<SharedScreenBrightnessHandle>,
        buzzer_volume: Option<SharedBuzzerVolumeHandle>,
        buzzer_priority: Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
        reconnected_flag: Arc<AtomicBool>,
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
        eprintln!("[MQTT Monitor]   Broker: {}:{}", config.broker.host, config.broker.port);
        eprintln!("[MQTT Monitor]   Client ID: {}",
            if config.broker.client_id.is_empty() { &hostname } else { &config.broker.client_id });
        eprintln!("[MQTT Monitor]   Keep-alive: {}s", config.connection.keep_alive_sec);
        eprintln!("[MQTT Monitor]   Clean session: {}", config.connection.clean_session);
        if config.last_will.enabled {
            eprintln!("[MQTT Monitor]   Last Will: enabled");
        }

        // Log TLS status and warn if disabled (EU MDR Annex I, 17.2)
        match &config.tls {
            Some(tls_config) if tls_config.enabled => {
                eprintln!("[MQTT Monitor]   TLS: enabled (ca_cert: {})", tls_config.ca_cert_path);
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
                eprintln!("[MQTT Monitor] ERROR: Failed to create tokio runtime: {}", e);
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
                    eprintln!("[MQTT Monitor] Warning: Failed to initialize authorization manager: {}", e);
                    eprintln!("[MQTT Monitor] Signed configuration commands will not be available");
                    None
                }
            }
        } else {
            None
        };

        let config_applier = match ConfigApplier::new(std::path::Path::new("/data/fiber/config")) {
            Ok(applier) => {
                eprintln!("[MQTT Monitor] Configuration applier initialized");
                Some(Arc::new(applier))
            }
            Err(e) => {
                eprintln!("[MQTT Monitor] Warning: Failed to initialize config applier: {}", e);
                None
            }
        };

        // Track LED brightness (write-only to STM, so we track it here)
        // Initialize from persisted config if available
        let initial_led_brightness = crate::libs::config::Config::load_default()
            .map(|c| c.system.led_brightness)
            .unwrap_or(50);
        let led_brightness_tracker = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(initial_led_brightness));

        // Track connection attempts for logging
        let mut connection_attempt: u32 = 0;

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

                // Create fresh MQTT client options and client
                let mqttoptions = create_mqtt_options(&config, &hostname, &client_id);
                let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

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
                if let Some(config_msg) = Self::build_config_state_message(&screen_brightness, &buzzer_volume, led_br) {
                    if let Err(e) = publisher.handle_message(config_msg).await {
                        eprintln!("[MQTT Monitor] Failed to publish config state: {}", e);
                    } else {
                        eprintln!("[MQTT Monitor] Published initial config state");
                    }
                }

                eprintln!("[MQTT Monitor] Connection established, entering event loop");

                // Initialize network monitoring for this connection
                let mut last_network_check = Instant::now();
                let mut last_known_network = get_network_status();

                // Initialize periodic status reporting
                let mut last_status_log = Instant::now();
                let app_start_time = Instant::now();
                let firmware_version = env!("CARGO_PKG_VERSION").to_string();

                // Initialize periodic challenge cleanup
                let mut last_challenge_cleanup = Instant::now();

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
                                                                if let Err(e) = Self::execute_config_command(
                                                                    execute_cmd,
                                                                    &config_applier,
                                                                    &stm_bridge,
                                                                    &screen_brightness,
                                                                    &buzzer_volume,
                                                                    &buzzer_priority,
                                                                    &led_brightness_tracker,
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
                                                                    if let Some(config_msg) = Self::build_config_state_message(&screen_brightness, &buzzer_volume, led_br) {
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
                                                                // Publish response
                                                                if let Err(e) = publisher.handle_message(response_msg).await {
                                                                    eprintln!("[MQTT Monitor] Failed to publish response: {}", e);
                                                                }

                                                                // Execute command if approved
                                                                if let Some(execute_cmd) = maybe_command {
                                                                    if let Err(e) = Self::execute_config_command(
                                                                        execute_cmd,
                                                                        &config_applier,
                                                                        &stm_bridge,
                                                                        &screen_brightness,
                                                                        &buzzer_volume,
                                                                        &buzzer_priority,
                                                                        &led_brightness_tracker,
                                                                    ) {
                                                                        eprintln!("[MQTT Monitor] Failed to execute command: {}", e);
                                                                    } else {
                                                                        // Publish updated config state after successful command
                                                                        let led_br = led_brightness_tracker.load(std::sync::atomic::Ordering::Relaxed);
                                                                        if let Some(config_msg) = Self::build_config_state_message(&screen_brightness, &buzzer_volume, led_br) {
                                                                            if let Err(e) = publisher.handle_message(config_msg).await {
                                                                                eprintln!("[MQTT Monitor] Failed to publish config state: {}", e);
                                                                            }
                                                                        }
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

                    // Handle messages from channel
                    _ = async {
                        // Check for messages with timeout
                        match receiver.recv_timeout(Duration::from_millis(100)) {
                            Ok(msg) => {
                                match msg {
                                    MqttMessage::Shutdown => {
                                        eprintln!("[MQTT Monitor] Shutdown message received");
                                        shutdown_flag.store(true, Ordering::Relaxed);
                                    }
                                    _ => {
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
                                }
                            }
                            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                                // Timeout is expected, continue
                            }
                            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                                eprintln!("[MQTT Monitor] Channel disconnected");
                                shutdown_flag.store(true, Ordering::Relaxed);
                            }
                        }
                    } => {}

                    // Network status monitoring (every 5 seconds)
                    _ = async {
                        if last_network_check.elapsed() > Duration::from_secs(5) {
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
                            last_network_check = Instant::now();
                        }
                    } => {}

                    // Periodic status reporting (every 60 seconds)
                    _ = async {
                        if last_status_log.elapsed() > Duration::from_secs(60) {
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

                            // Get device label from config, defaulting to hostname
                            let device_label = crate::libs::config::Config::load_default()
                                .ok()
                                .and_then(|cfg| cfg.system.device_label)
                                .unwrap_or_else(|| hostname.clone());

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
                                lorawan_sensor_count: 0, // Sensor count updated by LoRaWAN monitor
                            }).await {
                                eprintln!("[MQTT Monitor] Failed to publish system status: {}", e);
                            }

                            last_status_log = Instant::now();
                        }
                    } => {}

                    // Periodic challenge cleanup (every 30 seconds)
                    _ = async {
                        if last_challenge_cleanup.elapsed() > Duration::from_secs(30) {
                            if let Some(ref auth) = auth_manager {
                                let expired_count = auth.cleanup_expired_challenges();
                                if expired_count > 0 {
                                    eprintln!("[MQTT Monitor] Cleaned up {} expired challenges", expired_count);
                                }
                            }
                            last_challenge_cleanup = Instant::now();
                        }
                    } => {}

                    // Poll for pairing results and publish them
                    _ = async {
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
                    } => {}
                    }
                } // End of inner event loop
            } // End of 'connection outer loop

            eprintln!("[MQTT Monitor] Monitor loop exited");
            Ok(())
        })
    }

    /// Parse pairing request from MQTT payload
    fn parse_pairing_request(payload: &[u8]) -> Result<crate::libs::pairing::PairingRequest, String> {
        let json_str = std::str::from_utf8(payload)
            .map_err(|e| format!("Invalid UTF-8: {}", e))?;

        let request: crate::libs::pairing::PairingRequest = serde_json::from_str(json_str)
            .map_err(|e| format!("Invalid JSON: {}", e))?;

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
        use std::path::Path;
        use crate::libs::crypto::CertificateAuthority;

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
            verifier,
            audit_db,
            300,  // 5 minute challenge timeout
            10,   // max 10 concurrent challenges
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
                let line = params.get("line").and_then(|v| v.as_u64())
                    .ok_or("Missing line")? as u8;
                let thresholds = params.get("thresholds").ok_or("Missing thresholds")?;
                Ok(MqttCommand::SetSensorThreshold {
                    line,
                    critical_low: thresholds["critical_low"].as_f64().unwrap_or(0.0) as f32,
                    alarm_low: thresholds.get("alarm_low").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                    warning_low: thresholds["warning_low"].as_f64().unwrap_or(0.0) as f32,
                    warning_high: thresholds["warning_high"].as_f64().unwrap_or(0.0) as f32,
                    alarm_high: thresholds.get("alarm_high").and_then(|v| v.as_f64()).unwrap_or(100.0) as f32,
                    critical_high: thresholds["critical_high"].as_f64().unwrap_or(0.0) as f32,
                })
            }
            "set_sensor_name" => {
                let line = params.get("line").and_then(|v| v.as_u64()).ok_or("Missing line")? as u8;
                let name = params.get("name").and_then(|v| v.as_str()).ok_or("Missing name")?.to_string();
                Ok(MqttCommand::SetSensorName { line, name })
            }
            "set_sensor_location" => {
                let line = params.get("line").and_then(|v| v.as_u64()).ok_or("Missing line")? as u8;
                let location = params.get("location").and_then(|v| v.as_str()).ok_or("Missing location")?.to_string();
                Ok(MqttCommand::SetSensorLocation { line, location })
            }
            "restart_application" => {
                let r = reason.clone().unwrap_or_else(|| "Dev platform command".to_string());
                Ok(MqttCommand::RestartApplication { reason: r })
            }
            "set_interval" => {
                let sample = params.get("sample_interval_ms").and_then(|v| v.as_u64()).ok_or("Missing sample_interval_ms")?;
                let aggregation = params.get("aggregation_interval_ms").and_then(|v| v.as_u64()).ok_or("Missing aggregation_interval_ms")?;
                let report = params.get("report_interval_ms").and_then(|v| v.as_u64()).ok_or("Missing report_interval_ms")?;
                Ok(MqttCommand::SetInterval { sample_interval_ms: sample, aggregation_interval_ms: aggregation, report_interval_ms: report })
            }
            "set_system_info_interval" => {
                let interval = params.get("interval_seconds").and_then(|v| v.as_u64()).ok_or("Missing interval_seconds")?;
                Ok(MqttCommand::SetSystemInfoInterval { interval_seconds: interval })
            }
            "set_device_label" => {
                let label = params.get("label").and_then(|v| v.as_str()).ok_or("Missing label")?.to_string();
                Ok(MqttCommand::SetDeviceLabel { label })
            }
            "set_led_brightness" => {
                let brightness = params.get("brightness").and_then(|v| v.as_u64()).ok_or("Missing brightness")? as u8;
                Ok(MqttCommand::SetLedBrightness { brightness })
            }
            "set_screen_brightness" => {
                let brightness = params.get("brightness").and_then(|v| v.as_u64()).ok_or("Missing brightness")? as u8;
                Ok(MqttCommand::SetScreenBrightness { brightness })
            }
            "set_buzzer_volume" => {
                let volume = params.get("volume").and_then(|v| v.as_u64()).ok_or("Missing volume")? as u8;
                Ok(MqttCommand::SetBuzzerVolume { volume })
            }
            "set_network_config" => {
                Ok(MqttCommand::SetNetworkConfig {
                    interface: params.get("interface").and_then(|v| v.as_str()).unwrap_or("ethernet").to_string(),
                    config_type: params.get("type").and_then(|v| v.as_str()).unwrap_or("dhcp").to_string(),
                    ip_address: params.get("ip_address").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    subnet_mask: params.get("subnet_mask").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    gateway: params.get("gateway").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    dns_primary: params.get("dns_primary").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    dns_secondary: params.get("dns_secondary").and_then(|v| v.as_str()).map(|s| s.to_string()),
                })
            }
            _ => Err(format!("Unsupported dev-platform command: {}", command_type)),
        }
    }

    /// Execute an approved configuration command
    /// Note: In CA-based trust model, signer management (add/remove/update) is handled by the CA platform,
    /// not directly on the device.
    fn execute_config_command(
        cmd: MqttCommand,
        config_applier: &Option<Arc<ConfigApplier>>,
        stm_bridge: &Option<SharedStmBridge>,
        screen_brightness: &Option<SharedScreenBrightnessHandle>,
        buzzer_volume: &Option<SharedBuzzerVolumeHandle>,
        buzzer_priority: &Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
        led_brightness_tracker: &std::sync::Arc<std::sync::atomic::AtomicU8>,
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
            MqttCommand::RestartApplication { reason } => {
                eprintln!("[MQTT Monitor] Device reboot requested: {}", reason);
                std::process::Command::new("reboot")
                    .spawn()
                    .map_err(|e| format!("Failed to execute reboot: {}", e))?;
                Ok(())
            }
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
                temp_critical_low,
                temp_warning_low,
                temp_warning_high,
                temp_critical_high,
                humidity_critical_low,
                humidity_warning_low,
                humidity_warning_high,
                humidity_critical_high,
            } => {
                if let Some(applier) = config_applier {
                    let result = applier.apply_lorawan_sensor_config(
                        dev_eui.clone(),
                        name,
                        serial_number,
                        temp_critical_low,
                        temp_warning_low,
                        temp_warning_high,
                        temp_critical_high,
                        humidity_critical_low,
                        humidity_warning_low,
                        humidity_warning_high,
                        humidity_critical_high,
                    );
                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ LoRaWAN sensor config updated for {}", dev_eui);
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
                devaddr,
                nwkskey,
                appskey,
            } => {
                // Step 1: Provision in ChirpStack (create device + ABP activate)
                eprintln!("[MQTT Monitor] Provisioning sticker {} in ChirpStack...", dev_eui);
                match crate::libs::lorawan::provisioning::provision_sticker(
                    &dev_eui, &name, &serial_number, &devaddr, &nwkskey, &appskey,
                ) {
                    Ok(()) => {
                        eprintln!("[MQTT Monitor] ✓ Sticker {} provisioned in ChirpStack", dev_eui);
                    }
                    Err(e) => {
                        // Log but continue - ChirpStack may be down or device may already exist
                        eprintln!("[MQTT Monitor] ⚠ ChirpStack provisioning for {}: {}", dev_eui, e);
                    }
                }

                // Step 2: Save sensor config to YAML (always, even if ChirpStack failed)
                if let Some(applier) = config_applier {
                    let result = applier.apply_lorawan_sensor_config(
                        dev_eui.clone(),
                        Some(name),
                        Some(serial_number),
                        None, None, None, None,  // temp thresholds: use defaults
                        None, None, None, None,  // humidity thresholds: use defaults
                    );
                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ LoRaWAN sticker {} config saved", dev_eui);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
            }
            MqttCommand::RemoveLoRaWANSticker { dev_eui } => {
                eprintln!("[MQTT Monitor] Removing sticker {} ...", dev_eui);
                if let Some(applier) = config_applier {
                    let result = applier.remove_lorawan_sensor_config(dev_eui.clone());
                    if result.success {
                        eprintln!("[MQTT Monitor] ✓ LoRaWAN sticker {} removed", dev_eui);
                        Ok(())
                    } else {
                        Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
                    }
                } else {
                    Err("Config applier not initialized".to_string())
                }
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

        let buzzer_vol = buzzer_volume
            .as_ref()
            .map(|bv| bv.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(main_config.system.buzzer_volume);

        let device_label = main_config.system.device_label
            .unwrap_or_default();

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
                        enabled: s.enabled,
                        temp_critical_low: s.temp_critical_low,
                        temp_warning_low: s.temp_warning_low,
                        temp_warning_high: s.temp_warning_high,
                        temp_critical_high: s.temp_critical_high,
                        humidity_critical_low: s.humidity_critical_low,
                        humidity_warning_low: s.humidity_warning_low,
                        humidity_warning_high: s.humidity_warning_high,
                        humidity_critical_high: s.humidity_critical_high,
                    })
                    .collect()
            })
            .unwrap_or_default();

        Some(MqttMessage::PublishConfigState {
            led_brightness,
            screen_brightness: screen_br,
            buzzer_volume: buzzer_vol,
            system_info_interval_s,
            device_label,
            sensors,
            lorawan_sensors,
            sample_interval_ms: main_config.sensors.sample_interval_ms,
            aggregation_interval_ms: main_config.sensors.aggregation_interval_ms,
            report_interval_ms: main_config.sensors.report_interval_ms,
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
        eprintln!("[MQTT Monitor] Configuring network: {} {}", interface, config_type);

        // Find connection name for the interface type
        let conn_name = Self::get_nmcli_connection_name(interface)?;
        eprintln!("[MQTT Monitor] Found connection: {}", conn_name);

        if config_type == "dhcp" {
            // Set to DHCP (automatic)
            let output = std::process::Command::new("nmcli")
                .args(["con", "mod", &conn_name, "ipv4.method", "auto", "ipv4.addresses", "", "ipv4.gateway", "", "ipv4.dns", ""])
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
                    "con", "mod", &conn_name,
                    "ipv4.method", "manual",
                    "ipv4.addresses", &ip_with_cidr,
                    "ipv4.gateway", &gw,
                ])
                .output()
                .map_err(|e| format!("nmcli failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("nmcli mod failed: {}", stderr));
            }
            eprintln!("[MQTT Monitor] Set {} to static IP: {}", conn_name, ip_with_cidr);

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
        }
    }

    #[test]
    fn test_create_mqtt_options_no_tls() {
        let config = test_mqtt_config(None, 1883);
        let opts = create_mqtt_options(&config, "testhost", "test-client");
        let (host, port) = opts.broker_address();
        assert_eq!(host, "mqtt.example.com");
        assert_eq!(port, 1883, "Port should remain 1883 when TLS is not configured");
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
        // TLS enabled but CA cert won't exist -- that's fine for this test,
        // we're only testing port override logic. The configure_tls_transport
        // will log an error but the function still returns options.
        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: "/nonexistent/ca.crt".to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let config = test_mqtt_config(Some(tls), 1883);
        let opts = create_mqtt_options(&config, "testhost", "test-client");
        let (_, port) = opts.broker_address();
        assert_eq!(port, 8883, "Port should be overridden to 8883 when TLS is enabled and port was 1883");
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
        assert_eq!(port, 9883, "Custom port should be preserved even when TLS is enabled");
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
        assert!(result.is_err(), "Should fail when CA cert file does not exist");
        let err = result.err().unwrap();
        assert!(err.contains("Failed to read CA certificate"), "Error should mention CA cert: {}", err);
    }

    #[test]
    fn test_configure_tls_transport_valid_ca_file() {
        // Create a temporary PEM file with a self-signed CA cert
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let ca_path = dir.path().join("ca.crt");

        // Write a minimal PEM-encoded certificate (not a real cert, but enough
        // to test the file-loading path -- the actual TLS handshake will fail at
        // runtime, but we just want to verify the Transport gets configured).
        let fake_pem = b"-----BEGIN CERTIFICATE-----\n\
            MIIBkTCB+wIJALRiMLAh2wG7MA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl\n\
            c3RjYTAeFw0yNDA0MjEwMDAwMDBaFw0yNTA0MjEwMDAwMDBaMBExDzANBgNVBAMM\n\
            BnRlc3RjYTBcMA0GCSqGSIb3DQEBAQUAA0sAMEgCQQC7o96Gahm8KzEGRE+HAWKL\n\
            hJJmbnRqH3UbMYvsIjmAtWBbJdU7FE4WBMhHc9cCq7YTEPHRROAKJ7mMEy0+SCCB\n\
            AgMBAAEwDQYJKoZIhvcNAQELBQADQQBR0sMEBcZykPk6DfbEbuCHuqSGgkDE\n\
            -----END CERTIFICATE-----\n";
        std::fs::write(&ca_path, fake_pem).expect("Failed to write CA file");

        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: ca_path.to_string_lossy().to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        };
        let result = configure_tls_transport(&tls);
        assert!(result.is_ok(), "Should succeed with a readable CA cert file: {:?}", result.err());
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
            "Should detect missing key when cert is present, got: {}", err
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
            "Should detect missing cert when key is present, got: {}", err2
        );
    }

    #[test]
    fn test_configure_tls_transport_with_client_auth() {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let ca_path = dir.path().join("ca.crt");
        let cert_path = dir.path().join("client.crt");
        let key_path = dir.path().join("client.key");

        // Write minimal PEM content (not real certs, but file-reading will succeed)
        let fake_pem = b"-----BEGIN CERTIFICATE-----\n\
            MIIBkTCB+wIJALRiMLAh2wG7MA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl\n\
            c3RjYTAeFw0yNDA0MjEwMDAwMDBaFw0yNTA0MjEwMDAwMDBaMBExDzANBgNVBAMM\n\
            BnRlc3RjYTBcMA0GCSqGSIb3DQEBAQUAA0sAMEgCQQC7o96Gahm8KzEGRE+HAWKL\n\
            hJJmbnRqH3UbMYvsIjmAtWBbJdU7FE4WBMhHc9cCq7YTEPHRROAKJ7mMEy0+SCCB\n\
            AgMBAAEwDQYJKoZIhvcNAQELBQADQQBR0sMEBcZykPk6DfbEbuCHuqSGgkDE\n\
            -----END CERTIFICATE-----\n";
        let fake_key = b"-----BEGIN PRIVATE KEY-----\n\
            MIIEvAIBADANBgkqhkiG9w0BAQEFAASC\n\
            -----END PRIVATE KEY-----\n";

        std::fs::write(&ca_path, fake_pem).unwrap();
        std::fs::write(&cert_path, fake_pem).unwrap();
        std::fs::write(&key_path, fake_key).unwrap();

        let tls = TlsConfig {
            enabled: true,
            ca_cert_path: ca_path.to_string_lossy().to_string(),
            client_cert_path: Some(cert_path.to_string_lossy().to_string()),
            client_key_path: Some(key_path.to_string_lossy().to_string()),
            insecure_skip_verify: false,
        };
        let result = configure_tls_transport(&tls);
        assert!(result.is_ok(), "Should succeed loading CA, client cert, and key files: {:?}", result.err());
    }
}
