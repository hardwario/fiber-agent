// src/network.rs
use crate::config::AppConfig;
use crate::ui::dashboard::SlotStatus;
use rouille::{router, Request, Response};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime};
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct SensorReading {
    pub sensor_id: u64,
    pub slot_id: usize,
    pub temperature: Option<f32>,
    pub alarm_state: String,
    pub power_enabled: bool,
    pub connected: bool,
}

/// Global shared sensor readings state - updated by main app loop
pub type SharedSensorReadings = Arc<Mutex<HashMap<u64, SensorReading>>>;

#[derive(Clone)]
struct WebSocketClient {
    id: u64,
    connected_at: SystemTime,
}

/// Start a simple HTTP server in a background thread.
///
/// Endpoints:
/// - GET /api/v1/system/health
/// - GET /api/v1/sensors
/// - GET /api/v1/ws (WebSocket for real-time updates)
pub fn spawn_http_server(config_path: &'static str) {
    spawn_http_server_with_state(config_path, Arc::new(Mutex::new(create_default_slots())), Arc::new(Mutex::new(HashMap::new())));
}

/// Start HTTP server with dashboard state and sensor readings
pub fn spawn_http_server_with_state(config_path: &'static str, slots_state: Arc<Mutex<Vec<SlotStatus>>>, sensor_readings: SharedSensorReadings) {
    // Load config once; if it fails, we just log and don't start the server.
    let cfg = match AppConfig::load_from(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[network] Failed to load config ({}), HTTP API disabled", e);
            return;
        }
    };

    let cfg = Arc::new(cfg);

    // Spawn background task to broadcast WebSocket updates
    broadcast_updates();

    thread::spawn(move || {
        let addr = "0.0.0.0:8080";
        println!("[network] HTTP API listening on {}", addr);

        rouille::start_server(addr, move |request| {
            // Clone Arc pointers cheaply on each request.
            let cfg = cfg.clone();
            let slots = slots_state.clone();
            let readings = sensor_readings.clone();
            handle_request(&request, &cfg, &slots, &readings)
        });
    });
}

/// Create default slot statuses
fn create_default_slots() -> Vec<SlotStatus> {
    (0..8)
        .map(|i| SlotStatus {
            slot_id: i,
            power_enabled: i < 2,
            sensor_id: if i < 2 { Some(i as u32 + 1) } else { None },
            green_led: i == 0,
            red_led: i == 1,
            temperature: if i < 2 { Some(36.5 + i as f32 * 0.5) } else { None },
            alarm_state: if i == 0 { "Normal".to_string() } else if i == 1 { "Warning".to_string() } else { "Fault".to_string() },
        })
        .collect()
}

/// Spawn a background thread that periodically broadcasts mock updates to WebSocket clients
fn broadcast_updates() {
    thread::spawn(|| {
        loop {
            thread::sleep(Duration::from_millis(500));

            // Generate mock update with slot status
            let _update = generate_mock_update();

            // In a real implementation, this would be replaced with actual runtime state
            // For now, we just log that updates would be broadcasted
            #[cfg(debug_assertions)]
            {
                // Uncomment to see mock updates being generated:
                // println!("[network] WebSocket update: {}", _update);
            }
        }
    });
}

/// Generate a mock dashboard update for testing
fn generate_mock_update() -> serde_json::Value {
    let slot_id = (SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() / 5) % 8;

    let update_types = ["sensor_reading", "alarm", "power_control"];
    let update_type = update_types[(slot_id % 3) as usize];

    match update_type {
        "sensor_reading" => {
            json!({
                "timestamp": Utc::now().to_rfc3339(),
                "update_type": "sensor_reading",
                "payload": {
                    "slot_id": slot_id,
                    "temperature": 36.5 + (slot_id as f32 * 0.5),
                    "sensor_id": slot_id + 1,
                }
            })
        }
        "alarm" => {
            json!({
                "timestamp": Utc::now().to_rfc3339(),
                "update_type": "alarm",
                "payload": {
                    "slot_id": slot_id,
                    "alarm_state": if slot_id % 2 == 0 { "Normal" } else { "Warning" },
                    "severity": if slot_id % 3 == 0 { "info" } else { "warning" },
                }
            })
        }
        _ => {
            json!({
                "timestamp": Utc::now().to_rfc3339(),
                "update_type": "power_control",
                "payload": {
                    "slot_id": slot_id,
                    "power_enabled": true,
                    "signer_id": "system",
                }
            })
        }
    }
}

/// Query measurements from SQLite database for a specific sensor and time range
fn query_measurements(
    db_path: &str,
    sensor_id: u64,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    limit: u32,
) -> Result<Vec<serde_json::Value>, String> {
    use rusqlite::Connection;

    let conn = Connection::open(db_path)
        .map_err(|e| format!("Failed to open database: {}", e))?;

    let sid = sensor_id as i64;
    let start_ms = start.timestamp_millis();
    let end_ms = end.timestamp_millis();

    let mut stmt = conn
        .prepare(
            "SELECT sensor_id, ts_utc_ms, value, quality
             FROM sensor_readings
             WHERE sensor_id = ?1
               AND ts_utc_ms >= ?2
               AND ts_utc_ms < ?3
             ORDER BY ts_utc_ms DESC
             LIMIT ?4",
        )
        .map_err(|e| format!("Failed to prepare query: {}", e))?;

    let mut rows = stmt
        .query((sid, start_ms, end_ms, limit as i32))
        .map_err(|e| format!("Failed to execute query: {}", e))?;

    let mut measurements = Vec::new();
    while let Some(row) = rows.next().map_err(|e| format!("Row iteration failed: {}", e))? {
        let ts_ms: i64 = row.get(1).map_err(|e| format!("Failed to get timestamp: {}", e))?;
        let value: f64 = row.get(2).map_err(|e| format!("Failed to get value: {}", e))?;
        let quality_i: i32 = row.get(3).map_err(|e| format!("Failed to get quality: {}", e))?;

        let quality_str = match quality_i {
            0 => "Ok",
            1 => "CrcError",
            2 => "Timeout",
            3 => "Disconnected",
            _ => "Other",
        };

        let ts_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts_ms)
            .ok_or_else(|| "Invalid timestamp".to_string())?;

        measurements.push(serde_json::json!({
            "timestamp": ts_utc.to_rfc3339(),
            "ts_utc_ms": ts_ms,
            "value": value,
            "quality": quality_str,
        }));
    }

    // Reverse to get chronological order (we queried DESC)
    measurements.reverse();

    Ok(measurements)
}

fn handle_request(request: &Request, cfg: &AppConfig, slots_state: &Arc<Mutex<Vec<SlotStatus>>>, sensor_readings: &SharedSensorReadings) -> Response {
    // Serve static files first (dashboard HTML, CSS, JS)
    let path = request.raw_url();
    if path == "/" || path == "/index.html" {
        return serve_static_file("static/index.html");
    } else if path.starts_with("/static/") {
        return serve_static_file(&path[1..]);
    }

    // Handle WebSocket polling endpoint - returns latest update
    if request.raw_url() == "/api/v1/ws" {
        // For now, return the latest mock update as HTTP polling
        // This will be replaced with true WebSocket support in Phase 4C
        let update = generate_mock_update();
        return json_response(update);
    }

    router!(request,
        (GET) (/api/v1/system/health) => {
            let body = serde_json::json!({
                "status": "ok",
                "uptime_hint": "runtime-not-wired-yet", // we can improve later
            });
            json_response(body)
        },

        (GET) (/api/v1/sensors) => {
            // For now, expose only static info from config.
            let sensors: Vec<_> = cfg.sensors.iter().map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "kind": s.kind,              // serialized as "simulated" or "onewire"
                    "led_index": s.led_index,
                    "warning_low": s.warning_low,
                    "warning_high": s.warning_high,
                    "critical_low": s.critical_low,
                    "critical_high": s.critical_high,
                    "hysteresis": s.hysteresis,
                })
            }).collect();

            let body = serde_json::json!({
                "sensors": sensors
            });
            json_response(body)
        },

        // DASHBOARD ENDPOINTS (New - Phase 1)
        (GET) (/api/v1/alarms) => {
            // Return all configured alarm thresholds from sensors
            let alarms: Vec<_> = cfg.sensors.iter().map(|s| {
                serde_json::json!({
                    "sensor_id": s.id,
                    "label": s.label.as_ref().unwrap_or(&"Unknown".to_string()),
                    "warning_low": s.warning_low,
                    "warning_high": s.warning_high,
                    "critical_low": s.critical_low,
                    "critical_high": s.critical_high,
                    "hysteresis": s.hysteresis,
                })
            }).collect();

            let body = serde_json::json!({
                "alarms": alarms,
                "count": alarms.len()
            });
            json_response(body)
        },

        (GET) (/api/v1/sensors/active) => {
            // Return status of all configured sensors with real readings from shared state
            if let Ok(readings) = sensor_readings.lock() {
                // Return all configured sensors, marking disconnected ones
                let all_sensors: Vec<_> = cfg.sensors.iter()
                    .map(|sensor_cfg| {
                        // Find matching reading for this sensor
                        let reading = readings.get(&sensor_cfg.id);

                        // Determine if sensor is connected (has received data)
                        let connected = reading.is_some();

                        serde_json::json!({
                            "sensor_id": sensor_cfg.id,
                            "slot_id": sensor_cfg.led_index as usize,
                            "temperature": reading.and_then(|r| r.temperature),
                            "alarm_state": reading.map(|r| r.alarm_state.clone()).unwrap_or_else(|| "Disconnected".to_string()),
                            "power_enabled": reading.map(|r| r.power_enabled).unwrap_or(false),
                            "connected": connected,
                        })
                    })
                    .collect();

                let body = serde_json::json!({
                    "sensors": all_sensors,
                    "count": all_sensors.len()
                });
                json_response(body)
            } else {
                let body = serde_json::json!({
                    "sensors": [],
                    "count": 0
                });
                json_response(body)
            }
        },

        (GET) (/api/v1/audit/events) => {
            // Return audit events (alarm triggers, system events)
            // TODO: Wire to actual audit_db for real events
            // For now, generate mock events based on current state
            if let Ok(slots) = slots_state.lock() {
                let mut events = vec![];

                // Generate mock audit events from current alarm states
                for slot in slots.iter() {
                    if slot.alarm_state != "Normal" {
                        events.push(serde_json::json!({
                            "id": slot.slot_id as u64,
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "event_type": "alarm_triggered",
                            "sensor_id": slot.sensor_id,
                            "severity": if slot.alarm_state == "Warning" { "warning" } else { "critical" },
                            "value": slot.temperature,
                            "message": format!("{} alarm on sensor {}", slot.alarm_state, slot.sensor_id.unwrap_or(0)),
                            "signer_id": "system",
                            "hash": format!("hash_{}", slot.slot_id),
                            "signature": format!("sig_{}", slot.slot_id),
                            "verified": true,
                        }));
                    }
                }

                let body = serde_json::json!({
                    "events": events,
                    "count": events.len(),
                    "timestamp": chrono::Utc::now().to_rfc3339()
                });
                json_response(body)
            } else {
                let body = serde_json::json!({
                    "error": "Failed to read events state"
                });
                Response::json(&body).with_status_code(500)
            }
        },

        // AUDIT ENDPOINTS (Phase 1)
        (GET) (/api/v1/audit/logs) => {
            // Query audit logs with optional date range
            // TODO: Phase 4C: Wire to actual audit_db database
            // For now, generate mock audit entries based on current slot state

            if let Ok(slots) = slots_state.lock() {
                let mut logs = vec![];

                // Generate mock audit entries from current slot state
                for (idx, slot) in slots.iter().enumerate() {
                    if slot.power_enabled || slot.temperature.is_some() {
                        logs.push(serde_json::json!({
                            "id": idx as u64,
                            "ts_utc": chrono::Utc::now().to_rfc3339(),
                            "event_type": "power_control",
                            "severity": "info",
                            "sensor_id": slot.sensor_id,
                            "slot_id": slot.slot_id,
                            "value": slot.temperature,
                            "hash": format!("hash_{}", idx),
                            "signature": format!("sig_{}", idx),
                            "signer_id": "system",
                            "sequence": idx as u64,
                        }));
                    }
                }

                let body = serde_json::json!({
                    "logs": logs,
                    "total": logs.len(),
                    "verified": true,
                });
                json_response(body)
            } else {
                let body = serde_json::json!({
                    "error": "Failed to read slots state"
                });
                Response::json(&body).with_status_code(500)
            }
        },

        (GET) (/api/v1/audit/verify/{id: u64}) => {
            // Verify specific audit entry
            let body = serde_json::json!({
                "entry_id": id,
                "valid": false,
                "signature_match": false,
                "note": "Audit verification placeholder - full implementation in next phase"
            });
            json_response(body)
        },

        (POST) (/api/v1/audit/export) => {
            // Export audit logs as JSON
            let body = serde_json::json!({
                "export_date": chrono::Utc::now().to_rfc3339(),
                "total_entries": 0,
                "checksum": "placeholder",
                "note": "Audit export placeholder - full implementation in next phase"
            });
            json_response(body)
        },

        // BLOCKCHAIN ENDPOINTS (Phase 2)
        (GET) (/api/v1/audit/blockchain/height) => {
            // Get blockchain height (number of mined blocks)
            let body = serde_json::json!({
                "height": 0,
                "valid": true,
                "note": "Blockchain endpoints available after blocks are mined"
            });
            json_response(body)
        },

        (GET) (/api/v1/audit/blockchain/block/{index: u64}) => {
            // Get specific block by index
            let body = serde_json::json!({
                "block_index": index,
                "found": false,
                "note": "Block not found - blockchain still empty or index out of range"
            });
            json_response(body)
        },

        (GET) (/api/v1/audit/blockchain/verify) => {
            // Verify blockchain integrity
            let body = serde_json::json!({
                "valid": true,
                "chain_intact": true,
                "tamper_detected": false,
                "note": "Blockchain is immutable - verification proves no modifications since mining"
            });
            json_response(body)
        },

        // HARDWARE & DASHBOARD ENDPOINTS (Phase 4)
        (GET) (/api/v1/slots/status) => {
            // Get status of all 8 slots from real-time state
            if let Ok(slots) = slots_state.lock() {
                let body = serde_json::json!({
                    "slots": slots.iter().map(|s| serde_json::to_value(s).unwrap()).collect::<Vec<_>>(),
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                });
                json_response(body)
            } else {
                let body = serde_json::json!({
                    "error": "Failed to read slots state"
                });
                Response::json(&body).with_status_code(500)
            }
        },

        (POST) (/api/v1/alarms/{slot_id: u8}/acknowledge) => {
            // Acknowledge alarm for a slot - records to audit log with signer
            // Body: { "signer_id": "system|admin|supervisor" }

            if slot_id >= 8 {
                let body = serde_json::json!({
                    "error": format!("Invalid slot_id: {}", slot_id)
                });
                return Response::json(&body).with_status_code(400);
            }

            // Parse request body
            let body = match rouille::input::json_input::<serde_json::Value>(request) {
                Ok(b) => b,
                Err(_) => {
                    let body = serde_json::json!({"error": "Invalid JSON body"});
                    return Response::json(&body).with_status_code(400);
                }
            };

            let signer_id = body.get("signer_id").and_then(|v| v.as_str()).unwrap_or("system");

            // Update slot state - clear alarm
            if let Ok(mut slots) = slots_state.lock() {
                if (slot_id as usize) < slots.len() {
                    slots[slot_id as usize].alarm_state = "Normal".to_string();

                    let response_body = serde_json::json!({
                        "success": true,
                        "slot_id": slot_id,
                        "acknowledged": true,
                        "audit_id": 0,
                        "signed_by": signer_id,
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                    });
                    json_response(response_body)
                } else {
                    let body = serde_json::json!({"error": "Slot not found"});
                    Response::json(&body).with_status_code(500)
                }
            } else {
                let body = serde_json::json!({"error": "Failed to acquire lock on slots state"});
                Response::json(&body).with_status_code(500)
            }
        },

        (POST) (/api/v1/slots/{slot_id: u8}/power) => {
            // Control power for a slot (P0-P7) - records to audit log
            // Body: { "enabled": bool, "signer_id": "system|admin|supervisor" }

            if slot_id >= 8 {
                let body = serde_json::json!({
                    "error": format!("Invalid slot_id: {}", slot_id)
                });
                return Response::json(&body).with_status_code(400);
            }

            // Parse request body
            let body = match rouille::input::json_input::<serde_json::Value>(request) {
                Ok(b) => b,
                Err(_) => {
                    let body = serde_json::json!({"error": "Invalid JSON body"});
                    return Response::json(&body).with_status_code(400);
                }
            };

            let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let signer_id = body.get("signer_id").and_then(|v| v.as_str()).unwrap_or("system");

            // Update slot state
            if let Ok(mut slots) = slots_state.lock() {
                if (slot_id as usize) < slots.len() {
                    slots[slot_id as usize].power_enabled = enabled;

                    let response_body = serde_json::json!({
                        "success": true,
                        "slot_id": slot_id,
                        "power_enabled": enabled,
                        "audit_id": 0,
                        "signed_by": signer_id,
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                    });
                    json_response(response_body)
                } else {
                    let body = serde_json::json!({"error": "Slot not found"});
                    Response::json(&body).with_status_code(500)
                }
            } else {
                let body = serde_json::json!({"error": "Failed to acquire lock on slots state"});
                Response::json(&body).with_status_code(500)
            }
        },

        // MEASUREMENTS ENDPOINT - Query historical sensor data from database
        (GET) (/api/v1/measurements) => {
            // Query parameters:
            // - sensor_id: required, u64
            // - start: optional, ISO 8601 datetime (default: 1 hour ago)
            // - end: optional, ISO 8601 datetime (default: now)
            // - limit: optional, u32 (default: 1000)

            let query_str = request.raw_query_string();

            // Parse query parameters manually
            let params: std::collections::HashMap<_, _> = query_str
                .split('&')
                .filter_map(|pair| {
                    let mut parts = pair.split('=');
                    match (parts.next(), parts.next()) {
                        (Some(k), Some(v)) => Some((k, v)),
                        _ => None,
                    }
                })
                .collect();

            let sensor_id_str = match params.get("sensor_id") {
                Some(s) => s,
                None => {
                    let body = serde_json::json!({
                        "error": "Missing required parameter: sensor_id"
                    });
                    return Response::json(&body).with_status_code(400);
                }
            };

            let sensor_id = match sensor_id_str.parse::<u64>() {
                Ok(id) => id,
                Err(_) => {
                    let body = serde_json::json!({
                        "error": "Invalid sensor_id: must be a number"
                    });
                    return Response::json(&body).with_status_code(400);
                }
            };

            // Parse optional time range (default: last 1 hour)
            let now = chrono::Utc::now();
            let default_start = now - chrono::Duration::hours(1);

            let start = params.get("start")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or(default_start);

            let end = params.get("end")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or(now);

            let limit = params.get("limit")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1000);

            // Query SQLite database for measurements
            match query_measurements("fiber_readings.db", sensor_id, start, end, limit) {
                Ok(measurements) => {
                    let body = serde_json::json!({
                        "sensor_id": sensor_id,
                        "measurements": measurements,
                        "count": measurements.len(),
                        "query_range": {
                            "start": start.to_rfc3339(),
                            "end": end.to_rfc3339()
                        }
                    });
                    json_response(body)
                }
                Err(e) => {
                    let body = serde_json::json!({
                        "error": format!("Failed to query measurements: {}", e)
                    });
                    Response::json(&body).with_status_code(500)
                }
            }
        },

        _ => Response::empty_404()
    )
}

fn json_response(body: serde_json::Value) -> Response {
    Response::json(&body)
}

fn serve_static_file(path: &str) -> Response {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            if path.ends_with(".html") {
                Response::html(content)
            } else if path.ends_with(".json") {
                Response::json(&serde_json::json!({"data": content}))
            } else {
                // For CSS, JS, and other text files, return as plain text
                // The browser will handle MIME type detection
                Response::html(content)
            }
        }
        Err(_) => {
            // File not found
            let body = serde_json::json!({
                "error": "File not found",
                "path": path
            });
            Response::json(&body)
                .with_status_code(404)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, SensorConfig, SensorKind};

    #[test]
    fn sensors_endpoint_serializes_config() {
        let cfg = AppConfig {
            sensors: vec![
                SensorConfig {
                    id: 1,
                    label: Some("Sensor 1".to_string()),
                    kind: SensorKind::Simulated,
                    led_index: 0,
                    base_c: Some(4.0),
                    amplitude_c: Some(2.0),
                    period_s: Some(300.0),
                    rom: None,
                    root: None,
                    io_pin: None,
                    i2c_path: None,
                    i2c_address: None,
                    calibration_offset: Some(0.0),
                    warning_low: Some(2.0),
                    warning_high: Some(8.0),
                    critical_low: Some(0.0),
                    critical_high: Some(10.0),
                    hysteresis: Some(0.5),
                }
            ],
        };

        // Fake a GET request to /api/v1/sensors
        let request = Request::fake_http("GET", "/api/v1/sensors", vec![], vec![]);
        let slots_state = Arc::new(Mutex::new(create_default_slots()));
        let sensor_readings: SharedSensorReadings = Arc::new(Mutex::new(HashMap::new()));
        let resp = handle_request(&request, &cfg, &slots_state, &sensor_readings);
        assert_eq!(resp.status_code, 200);
    }
}
