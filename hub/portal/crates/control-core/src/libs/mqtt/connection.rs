// MQTT connection state management

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// MQTT connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// Disconnect reason tracking
#[derive(Debug, Clone)]
pub struct DisconnectReason {
    /// Timestamp when disconnected
    pub timestamp: u64,
    /// Reason for disconnect (error message or category)
    pub reason: String,
    /// How long the connection lasted before disconnecting (seconds)
    pub duration_sec: u64,
}

/// Connection statistics
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    /// Number of messages published
    pub messages_published: u64,
    /// Number of messages received
    pub messages_received: u64,
    /// Number of successful reconnections
    pub reconnection_count: u32,
    /// Timestamp of last successful connection
    pub last_connected_time: Option<u64>,
    /// Timestamp of last disconnection
    pub last_disconnected_time: Option<u64>,
    /// Last 10 disconnect reasons
    pub disconnect_history: Vec<DisconnectReason>,
    /// Longest connection duration in seconds
    pub longest_connection_sec: u64,
    /// Total uptime across all connections
    pub total_uptime_sec: u64,
}

impl Default for ConnectionStats {
    fn default() -> Self {
        Self {
            messages_published: 0,
            messages_received: 0,
            reconnection_count: 0,
            last_connected_time: None,
            last_disconnected_time: None,
            disconnect_history: Vec::new(),
            longest_connection_sec: 0,
            total_uptime_sec: 0,
        }
    }
}

/// Shared connection state handle
pub type SharedConnectionState = Arc<Mutex<ConnectionStateHandle>>;

/// Connection state handle with statistics
pub struct ConnectionStateHandle {
    state: ConnectionState,
    stats: ConnectionStats,
}

impl ConnectionStateHandle {
    /// Create a new connection state handle
    pub fn new() -> Self {
        Self {
            state: ConnectionState::Disconnected,
            stats: ConnectionStats::default(),
        }
    }

    /// Get current connection state
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Set connection state
    pub fn set_state(&mut self, state: ConnectionState) {
        self.state = state;

        // Update timestamps based on state transition
        match state {
            ConnectionState::Connected => {
                self.stats.last_connected_time = Some(Self::current_timestamp());
                if self.stats.reconnection_count > 0 {
                    eprintln!("[MQTT Connection] Reconnected successfully");
                } else {
                    eprintln!("[MQTT Connection] Initial connection successful");
                }
            }
            ConnectionState::Disconnected | ConnectionState::Error => {
                self.stats.last_disconnected_time = Some(Self::current_timestamp());
                eprintln!("[MQTT Connection] Disconnected");
            }
            ConnectionState::Connecting => {
                eprintln!("[MQTT Connection] Attempting connection...");
            }
        }
    }

    /// Check if currently connected
    pub fn is_connected(&self) -> bool {
        self.state == ConnectionState::Connected
    }

    /// Record a successful reconnection
    pub fn record_reconnection(&mut self) {
        self.stats.reconnection_count += 1;
        self.set_state(ConnectionState::Connected);
    }

    /// Increment published message count
    pub fn record_publish(&mut self) {
        self.stats.messages_published += 1;
    }

    /// Increment received message count
    pub fn record_receive(&mut self) {
        self.stats.messages_received += 1;
    }

    /// Get connection statistics
    pub fn stats(&self) -> &ConnectionStats {
        &self.stats
    }

    /// Get connection uptime in seconds (if currently connected)
    pub fn uptime_seconds(&self) -> Option<u64> {
        if self.is_connected() {
            if let Some(connected_time) = self.stats.last_connected_time {
                let now = Self::current_timestamp();
                return Some(now.saturating_sub(connected_time));
            }
        }
        None
    }

    /// Get current Unix timestamp
    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Record a disconnect event with reason
    pub fn record_disconnect(&mut self, reason: String) {
        let now = Self::current_timestamp();
        self.stats.last_disconnected_time = Some(now);

        // Calculate connection duration if we were connected
        if let Some(connected_time) = self.stats.last_connected_time {
            let duration = now.saturating_sub(connected_time);

            // Update total uptime
            self.stats.total_uptime_sec += duration;

            // Update longest connection if this was longer
            if duration > self.stats.longest_connection_sec {
                self.stats.longest_connection_sec = duration;
            }

            // Add to disconnect history (keep last 10)
            self.stats.disconnect_history.push(DisconnectReason {
                timestamp: now,
                reason: reason.clone(),
                duration_sec: duration,
            });

            // Keep only last 10 disconnects
            if self.stats.disconnect_history.len() > 10 {
                self.stats.disconnect_history.remove(0);
            }

            eprintln!(
                "[MQTT Connection] Disconnected after {}s - Reason: {}",
                duration, reason
            );
        }
    }
}

impl Default for ConnectionStateHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a new shared connection state
pub fn create_shared_connection_state() -> SharedConnectionState {
    Arc::new(Mutex::new(ConnectionStateHandle::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_state_transitions() {
        let mut handle = ConnectionStateHandle::new();
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(!handle.is_connected());

        handle.set_state(ConnectionState::Connecting);
        assert_eq!(handle.state(), ConnectionState::Connecting);
        assert!(!handle.is_connected());

        handle.set_state(ConnectionState::Connected);
        assert_eq!(handle.state(), ConnectionState::Connected);
        assert!(handle.is_connected());

        handle.set_state(ConnectionState::Disconnected);
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(!handle.is_connected());
    }

    #[test]
    fn test_connection_stats() {
        let mut handle = ConnectionStateHandle::new();

        assert_eq!(handle.stats().messages_published, 0);
        assert_eq!(handle.stats().messages_received, 0);
        assert_eq!(handle.stats().reconnection_count, 0);

        handle.record_publish();
        handle.record_publish();
        assert_eq!(handle.stats().messages_published, 2);

        handle.record_receive();
        assert_eq!(handle.stats().messages_received, 1);

        handle.record_reconnection();
        assert_eq!(handle.stats().reconnection_count, 1);
        assert!(handle.is_connected());
    }

    #[test]
    fn test_uptime_calculation() {
        let mut handle = ConnectionStateHandle::new();

        // Not connected, no uptime
        assert_eq!(handle.uptime_seconds(), None);

        // Connect
        handle.set_state(ConnectionState::Connected);

        // Should have uptime now (may be 0 or 1 depending on timing)
        assert!(handle.uptime_seconds().is_some());
    }
}
