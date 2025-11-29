// src/ui/dashboard.rs
//
// Web-based dashboard for monitoring 8 sensor slots with:
// - Real-time sensor readings and alarm states
// - Slot power control (P0-P7)
// - LED visualization (GG/YY/RR/--)
// - Multi-signer audit log with filtering
// - Alarm acknowledgement and threshold management

use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::sync::Mutex;

/// Global dashboard state that can be updated by app and read by network handler
pub struct DashboardGlobalState {
    pub slots: Arc<Mutex<Vec<SlotStatus>>>,
}

impl DashboardGlobalState {
    pub fn new() -> Self {
        // Initialize with 8 slots
        let mut slots = Vec::with_capacity(8);
        for i in 0..8 {
            slots.push(SlotStatus {
                slot_id: i as u8,
                power_enabled: false,
                sensor_id: None,
                green_led: false,
                red_led: false,
                temperature: None,
                alarm_state: "Unknown".to_string(),
            });
        }
        Self {
            slots: Arc::new(Mutex::new(slots)),
        }
    }

    /// Update a single slot's status
    pub fn update_slot(&self, status: SlotStatus) -> Result<(), String> {
        if status.slot_id >= 8 {
            return Err(format!("Invalid slot_id: {}", status.slot_id));
        }
        if let Ok(mut slots) = self.slots.lock() {
            slots[status.slot_id as usize] = status.clone();
            Ok(())
        } else {
            Err("Failed to acquire lock".to_string())
        }
    }

    /// Get status of all 8 slots
    pub fn get_all_slots(&self) -> Result<Vec<SlotStatus>, String> {
        if let Ok(slots) = self.slots.lock() {
            Ok(slots.to_vec())
        } else {
            Err("Failed to acquire lock".to_string())
        }
    }

    /// Get status of a single slot
    pub fn get_slot(&self, slot_id: u8) -> Result<SlotStatus, String> {
        if slot_id >= 8 {
            return Err(format!("Invalid slot_id: {}", slot_id));
        }
        if let Ok(slots) = self.slots.lock() {
            Ok(slots[slot_id as usize].clone())
        } else {
            Err("Failed to acquire lock".to_string())
        }
    }
}

impl Default for DashboardGlobalState {
    fn default() -> Self {
        Self::new()
    }
}

/// Status of a single physical slot (0-7)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotStatus {
    pub slot_id: u8,                    // 0-7
    pub power_enabled: bool,            // Power pin state (P0-P7)
    pub sensor_id: Option<u32>,         // Which sensor in this slot (if any)
    pub green_led: bool,                // Green LED state
    pub red_led: bool,                  // Red LED state
    pub temperature: Option<f32>,       // Current reading
    pub alarm_state: String,            // Normal/Warning/Critical/Fault
}

impl Default for SlotStatus {
    fn default() -> Self {
        Self {
            slot_id: 0,
            power_enabled: false,
            sensor_id: None,
            green_led: false,
            red_led: false,
            temperature: None,
            alarm_state: "Unknown".to_string(),
        }
    }
}

/// Dashboard client connection
#[derive(Debug, Clone)]
pub struct DashboardClient {
    pub id: String,
    pub connected_at: DateTime<Utc>,
}

/// WebSocket message types for dashboard updates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardUpdate {
    pub timestamp: DateTime<Utc>,
    pub update_type: String, // "sensor_reading", "alarm", "power_control", "config_change"
    pub payload: serde_json::Value,
}

/// Dashboard server configuration
pub struct DashboardServer {
    pub port: u16,
    pub db_path: String,
    pub subscribers: Arc<Mutex<Vec<DashboardClient>>>,
}

impl DashboardServer {
    pub fn new(port: u16, db_path: String) -> Self {
        Self {
            port,
            db_path,
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Get current status of all 8 slots
    pub fn get_slots_status(&self) -> Vec<SlotStatus> {
        // This will be implemented in Phase 4B to query real hardware/runtime state
        vec![
            SlotStatus {
                slot_id: 0,
                power_enabled: true,
                sensor_id: Some(1),
                green_led: true,
                red_led: false,
                temperature: Some(36.5),
                alarm_state: "Normal".to_string(),
            },
            SlotStatus {
                slot_id: 1,
                power_enabled: true,
                sensor_id: Some(2),
                green_led: false,
                red_led: false,
                temperature: Some(37.2),
                alarm_state: "Warning".to_string(),
            },
        ]
    }

    /// Control power for a slot (records to audit log)
    pub fn set_slot_power(
        &self,
        slot_id: u8,
        enabled: bool,
        signer_id: String,
    ) -> Result<u64, String> {
        if slot_id >= 8 {
            return Err(format!("Invalid slot_id: {}", slot_id));
        }

        // TODO: Phase 4B implementation
        // 1. Control pin P{slot_id} via STM bridge
        // 2. Record action to audit log with signature
        // 3. Broadcast update to all WebSocket clients
        // 4. Return audit entry ID

        Ok(0)
    }

    /// Register a new WebSocket client
    pub fn add_subscriber(&self, client: DashboardClient) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(client);
        }
    }

    /// Remove a WebSocket client
    pub fn remove_subscriber(&self, client_id: &str) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.retain(|c| c.id != client_id);
        }
    }

    /// Broadcast update to all connected WebSocket clients
    pub fn broadcast_update(&self, update: DashboardUpdate) {
        // TODO: Phase 4B implementation
        // 1. Serialize update to JSON
        // 2. Send to all connected WebSocket clients
        // 3. Log any send failures but continue
    }
}

/// Query audit logs with optional filters
#[derive(Debug, Deserialize)]
pub struct AuditQueryParams {
    pub from: Option<String>,      // ISO8601 datetime
    pub to: Option<String>,        // ISO8601 datetime
    pub signer_id: Option<String>, // Filter by signer
    pub slot_id: Option<u8>,       // Filter by slot
    pub limit: Option<usize>,      // Max results (default 100)
}

/// Audit log entry response
#[derive(Debug, Serialize)]
pub struct AuditLogEntry {
    pub id: u64,
    pub ts_utc: String,
    pub event_type: String,
    pub sensor_id: Option<u32>,
    pub slot_id: Option<u8>,
    pub severity: String,
    pub value: Option<f32>,
    pub hash: String,
    pub signature: String,
    pub signer_id: String,
    pub sequence: u64,
}

/// Alarm acknowledgement request
#[derive(Debug, Deserialize)]
pub struct AckAlarmRequest {
    pub slot_id: u8,
    pub signer_id: String, // system, admin, or supervisor
}

/// Threshold update request
#[derive(Debug, Deserialize)]
pub struct UpdateThresholdRequest {
    pub slot_id: u8,
    pub warning_high: Option<f32>,
    pub warning_low: Option<f32>,
    pub critical_high: Option<f32>,
    pub critical_low: Option<f32>,
    pub signer_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dashboard_slot_creation() {
        let slot = SlotStatus {
            slot_id: 0,
            power_enabled: true,
            sensor_id: Some(1),
            green_led: true,
            red_led: false,
            temperature: Some(36.5),
            alarm_state: "Normal".to_string(),
        };

        assert_eq!(slot.slot_id, 0);
        assert!(slot.power_enabled);
        assert_eq!(slot.sensor_id, Some(1));
    }

    #[test]
    fn test_dashboard_server_creation() {
        let server = DashboardServer::new(8080, "/tmp/test.db".to_string());
        assert_eq!(server.port, 8080);
        assert_eq!(server.db_path, "/tmp/test.db");
    }

    #[test]
    fn test_dashboard_get_slots_status() {
        let server = DashboardServer::new(8080, "/tmp/test.db".to_string());
        let slots = server.get_slots_status();
        assert!(!slots.is_empty());
        assert_eq!(slots[0].slot_id, 0);
    }

    #[test]
    fn test_dashboard_invalid_slot_power() {
        let server = DashboardServer::new(8080, "/tmp/test.db".to_string());
        let result = server.set_slot_power(8, true, "system".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_dashboard_client_subscription() {
        let server = DashboardServer::new(8080, "/tmp/test.db".to_string());
        let client = DashboardClient {
            id: "test_client".to_string(),
            connected_at: Utc::now(),
        };

        server.add_subscriber(client.clone());
        let subs = server.subscribers.lock().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, "test_client");
    }
}
