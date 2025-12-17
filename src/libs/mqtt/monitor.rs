// MQTT monitor thread - main implementation

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam::channel::{bounded, Receiver, Sender};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};

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
use std::sync::Mutex;

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

/// MQTT monitor handle for sending messages
#[derive(Clone)]
pub struct MqttHandle {
    sender: Sender<MqttMessage>,
}

impl MqttHandle {
    /// Send a message to the MQTT monitor (non-blocking)
    pub fn send(&self, msg: MqttMessage) {
        // If channel is full, log warning and drop message (prevents blocking)
        if let Err(e) = self.sender.try_send(msg) {
            eprintln!("[MQTT Handle] Warning: Failed to send message: {}", e);
        }
    }

    /// Send sensor reading
    pub fn send_sensor_reading(
        &self,
        line: u8,
        name: &str,
        temperature: f32,
        is_connected: bool,
        alarm_state: crate::libs::alarms::AlarmState,
    ) {
        self.send(MqttMessage::PublishSensorReading {
            line,
            name: name.to_string(),
            temperature,
            is_connected,
            alarm_state,
        });
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

    /// Send power status
    pub fn send_power_status(
        &self,
        battery_mv: u16,
        battery_percent: u8,
        vin_mv: u16,
        on_ac_power: bool,
        last_ac_loss_time: Option<u64>,
    ) {
        self.send(MqttMessage::PublishPowerStatus {
            battery_mv,
            battery_percent,
            vin_mv,
            on_ac_power,
            last_ac_loss_time,
        });
    }

    /// Send AC power loss event
    pub fn send_ac_loss_event(&self) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.send(MqttMessage::PublishAcLossEvent { timestamp });
    }

    /// Send AC power reconnect event
    pub fn send_ac_reconnect_event(&self) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.send(MqttMessage::PublishAcReconnectEvent { timestamp });
    }

    /// Send aggregated sensor data
    pub fn send_aggregated_sensor_data(&self, period: crate::libs::sensors::aggregation::AggregationPeriod, names: [String; 8]) {
        self.send(MqttMessage::PublishAggregatedSensorData { period, names });
    }

    /// Send network status
    pub fn send_network_status(
        &self,
        wifi_connected: bool,
        wifi_signal_dbm: i32,
        ethernet_connected: bool,
    ) {
        self.send(MqttMessage::PublishNetworkStatus {
            wifi_connected,
            wifi_signal_dbm,
            ethernet_connected,
        });
    }

    /// Send system information
    pub fn send_system_info(
        &self,
        version: String,
        uptime_seconds: u64,
        hostname: String,
    ) {
        self.send(MqttMessage::PublishSystemInfo {
            version,
            uptime_seconds,
            hostname,
        });
    }
}

/// MQTT monitor thread
pub struct MqttMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    connection_state: SharedConnectionState,
    handle: MqttHandle,
}

impl MqttMonitor {
    /// Create and spawn MQTT monitor thread
    pub fn new(config: MqttConfig, hostname: String) -> io::Result<Self> {
        eprintln!("[MQTT Monitor] Initializing MQTT monitor for host: {}", hostname);
        eprintln!("[MQTT Monitor] Broker: {}:{}", config.broker.host, config.broker.port);

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        // Create bounded channel for messages
        let (sender, receiver) = bounded::<MqttMessage>(config.publish.max_queue_size);

        // Create shared connection state
        let connection_state = create_shared_connection_state();
        let connection_state_clone = connection_state.clone();

        // Create handle for sending messages
        let handle = MqttHandle { sender };
        let handle_clone = handle.clone();

        // Spawn monitoring thread
        let thread_handle = thread::spawn(move || {
            if let Err(e) = Self::monitor_loop(
                config,
                hostname,
                receiver,
                shutdown_flag_clone,
                connection_state_clone,
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
        })
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
                }

                // Publish online status
                if let Err(e) = publisher.publish_online_status().await {
                    eprintln!("[MQTT Monitor] Failed to publish online status: {}", e);
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
                                // Handle incoming command
                                if config.subscribe.enabled {
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

                                                MqttCommand::ConfigConfirm {
                                                    challenge_id,
                                                    confirmation,
                                                    signer_id,
                                                    signature,
                                                    timestamp,
                                                    nonce,
                                                    certificate,
                                                } => {
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
                                                                    ) {
                                                                        eprintln!("[MQTT Monitor] Failed to execute command: {}", e);
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

                            // Publish network status via MQTT
                            let network = get_network_status();
                            if let Err(e) = publisher.handle_message(MqttMessage::PublishNetworkStatus {
                                wifi_connected: network.wifi_connected,
                                wifi_signal_dbm: network.wifi_signal_strength,
                                ethernet_connected: network.ethernet_connected,
                            }).await {
                                eprintln!("[MQTT Monitor] Failed to publish network status: {}", e);
                            }

                            // Publish system info via MQTT
                            let uptime_seconds = app_start_time.elapsed().as_secs();
                            if let Err(e) = publisher.handle_message(MqttMessage::PublishSystemInfo {
                                version: firmware_version.clone(),
                                uptime_seconds,
                                hostname: hostname.clone(),
                            }).await {
                                eprintln!("[MQTT Monitor] Failed to publish system info: {}", e);
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
                    }
                } // End of inner event loop
            } // End of 'connection outer loop

            eprintln!("[MQTT Monitor] Monitor loop exited");
            Ok(())
        })
    }

    /// Initialize authorization manager with crypto components
    fn init_authorization_manager(_config: &MqttConfig) -> Result<AuthorizationManager, String> {
        use std::path::Path;

        // Initialize CA registry (trusted Certificate Authorities)
        let ca_file = Path::new("/data/fiber/config/authorized_signers.yaml");
        let ca_registry = Arc::new(Mutex::new(
            CARegistry::load_from_file(ca_file)
                .map_err(|e| format!("Failed to load CA registry: {:?}", e))?,
        ));

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
            300, // ±5 minutes timestamp drift
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

    /// Execute an approved configuration command
    /// Note: In CA-based trust model, signer management (add/remove/update) is handled by the CA platform,
    /// not directly on the device.
    fn execute_config_command(
        cmd: MqttCommand,
        config_applier: &Option<Arc<ConfigApplier>>,
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
