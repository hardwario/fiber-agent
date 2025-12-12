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
use super::messages::MqttMessage;
use super::publisher::MqttPublisher;
use super::subscriber::MqttSubscriber;
use super::topics::TopicBuilder;

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
        temperature: f32,
        is_connected: bool,
        alarm_state: crate::libs::alarms::AlarmState,
    ) {
        self.send(MqttMessage::PublishSensorReading {
            line,
            temperature,
            is_connected,
            alarm_state,
        });
    }

    /// Send alarm event
    pub fn send_alarm_event(
        &self,
        line: u8,
        from_state: crate::libs::alarms::AlarmState,
        to_state: crate::libs::alarms::AlarmState,
        temperature: f32,
    ) {
        self.send(MqttMessage::PublishAlarmEvent {
            line,
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

        // Create MQTT client options
        let mut mqttoptions = MqttOptions::new(
            client_id,
            config.broker.host.clone(),
            config.broker.port,
        );

        // Set connection parameters
        mqttoptions.set_keep_alive(Duration::from_secs(config.connection.keep_alive_sec));
        mqttoptions.set_clean_session(config.connection.clean_session);

        // Set credentials if provided
        if let (Some(username), Some(password)) = (&config.broker.username, &config.broker.password)
        {
            eprintln!("[MQTT Monitor] Setting credentials for user: {}", username);
            mqttoptions.set_credentials(username, password);
        } else {
            eprintln!("[MQTT Monitor] WARNING: No credentials configured");
            eprintln!("[MQTT Monitor]   Username: {:?}", config.broker.username.is_some());
            eprintln!("[MQTT Monitor]   Password: {:?}", config.broker.password.is_some());
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

        eprintln!("[MQTT Monitor] Connection parameters:");
        eprintln!("[MQTT Monitor]   Broker: {}:{}", config.broker.host, config.broker.port);
        eprintln!("[MQTT Monitor]   Client ID: {}",
            if config.broker.client_id.is_empty() { &hostname } else { &config.broker.client_id });
        eprintln!("[MQTT Monitor]   Keep-alive: {}s", config.connection.keep_alive_sec);
        eprintln!("[MQTT Monitor]   Clean session: {}", config.connection.clean_session);
        if config.last_will.enabled {
            eprintln!("[MQTT Monitor]   Last Will: enabled");
        }
        eprintln!(
            "[MQTT Monitor] Connecting to {}:{}...",
            config.broker.host, config.broker.port
        );

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

        runtime.block_on(async {
            // Create MQTT client and event loop
            let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

            eprintln!("[MQTT Monitor] AsyncClient created, waiting for initial connection...");

            // Wait for initial CONNACK (with timeout)
            let connection_timeout = Duration::from_secs(config.connection.connection_timeout_sec);
            let connection_start = Instant::now();
            let mut connected = false;

            while connection_start.elapsed() < connection_timeout {
                match tokio::time::timeout(Duration::from_secs(1), eventloop.poll()).await {
                    Ok(Ok(Event::Incoming(Incoming::ConnAck(connack)))) => {
                        eprintln!("[MQTT Monitor] Initial CONNACK received");
                        eprintln!("[MQTT Monitor]   Session present: {}", connack.session_present);
                        connected = true;
                        break;
                    }
                    Ok(Err(e)) => {
                        eprintln!("[MQTT Monitor] Connection error during initial connect: {}", e);
                        return Err(format!("Initial connection failed: {}", e));
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
                return Err(format!("Failed to connect within {}s timeout",
                    config.connection.connection_timeout_sec));
            }

            eprintln!("[MQTT Monitor] Initial connection successful, entering main loop");

            // Create topic builder
            let topics = TopicBuilder::new(
                config.publish.topic_prefix.clone(),
                hostname.clone(),
                config.publish.include_hostname,
            );

            // Create publisher
            let publisher = MqttPublisher::new(client.clone(), topics.clone(), &config.publish);

            // Create subscriber
            let mut subscriber = MqttSubscriber::new(
                config.subscribe.max_commands_per_second,
                config.subscribe.audit_enabled,
            );

            // Subscribe to command topics if enabled
            if config.subscribe.enabled {
                let cmd_topic = topics.commands_wildcard();
                eprintln!("[MQTT Monitor] Subscribing to commands: {}", cmd_topic);
                if let Err(e) = client.subscribe(&cmd_topic, QoS::AtLeastOnce).await {
                    eprintln!("[MQTT Monitor] Warning: Failed to subscribe to commands: {}", e);
                }
            }

            // Update connection state
            {
                if let Ok(mut state) = connection_state.lock() {
                    state.set_state(ConnectionState::Connecting);
                }
            }

            // Initialize reconnection state with exponential backoff
            let mut reconnect_state = ReconnectionState::new(
                config.connection.reconnect_delay_sec,
                config.connection.max_reconnect_delay_sec,
            );

            // Initialize network monitoring
            let mut last_network_check = Instant::now();
            let mut last_known_network = NetworkStatus::disconnected();

            // Initialize periodic status reporting
            let mut last_status_log = Instant::now();

            // Main event loop
            loop {
                // Check for shutdown signal
                if shutdown_flag.load(Ordering::Relaxed) {
                    eprintln!("[MQTT Monitor] Shutdown signal received");
                    break;
                }

                // Use tokio::select! to handle both MQTT events and channel messages
                tokio::select! {
                    // Handle MQTT broker events
                    event = eventloop.poll() => {
                        match event {
                            Ok(Event::Incoming(Incoming::ConnAck(connack))) => {
                                eprintln!("[MQTT Monitor] ✓ Connected to broker");
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
                                    state.set_state(ConnectionState::Connected);
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
                                            // TODO: Route command to appropriate handler
                                            // For now, just log it
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
                                    // Actually wait for network and check result
                                    match tokio::task::spawn_blocking(|| wait_for_network(60)).await {
                                        Ok(true) => {
                                            eprintln!("[MQTT Monitor] Network is now available");
                                        }
                                        Ok(false) => {
                                            eprintln!("[MQTT Monitor] Network still unavailable after 60s");
                                        }
                                        Err(e) => {
                                            eprintln!("[MQTT Monitor] Error waiting for network: {}", e);
                                        }
                                    }
                                } else {
                                    // Network is up, check if broker is reachable
                                    let broker_host = config.broker.host.clone();
                                    let broker_port = config.broker.port;
                                    tokio::task::spawn_blocking(move || {
                                        check_broker_reachable(&broker_host, broker_port)
                                    }).await.ok();
                                }

                                // Calculate backoff delay
                                let delay = reconnect_state.calculate_delay();
                                tokio::time::sleep(delay).await;
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
                            }
                            last_status_log = Instant::now();
                        }
                    } => {}
                }
            }

            eprintln!("[MQTT Monitor] Monitor loop exited");
            Ok(())
        })
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
