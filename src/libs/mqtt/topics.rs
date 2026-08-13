// MQTT topic hierarchy builder

/// Topic builder for MQTT messages
#[derive(Clone)]
pub struct TopicBuilder {
    prefix: String,
    hostname: String,
    include_hostname: bool,
}

impl TopicBuilder {
    /// Create a new topic builder
    pub fn new(prefix: String, hostname: String, include_hostname: bool) -> Self {
        Self {
            prefix,
            hostname,
            include_hostname,
        }
    }

    /// Build a topic path
    fn build(&self, parts: &[&str]) -> String {
        let mut topic = self.prefix.clone();

        if self.include_hostname {
            topic.push('/');
            topic.push_str(&self.hostname);
        }

        for part in parts {
            topic.push('/');
            topic.push_str(part);
        }

        topic
    }

    // Device status topics
    pub fn status(&self) -> String {
        self.build(&["status"])
    }

    pub fn info_version(&self) -> String {
        self.build(&["info", "version"])
    }

    pub fn info_uptime(&self) -> String {
        self.build(&["info", "uptime"])
    }

    pub fn info_hostname(&self) -> String {
        self.build(&["info", "hostname"])
    }

    // Sensor topics
    pub fn sensor_alarm(&self, line: u8) -> String {
        self.build(&["sensors", &format!("line{}", line), "alarm"])
    }

    pub fn sensors_summary(&self) -> String {
        self.build(&["sensors", "summary"])
    }

    pub fn sensors_aggregated(&self) -> String {
        self.build(&["sensors", "aggregated"])
    }

    // Power topics
    pub fn power_battery_percentage(&self) -> String {
        self.build(&["power", "battery", "percentage"])
    }

    pub fn power_battery_voltage(&self) -> String {
        self.build(&["power", "battery", "voltage"])
    }

    pub fn power_battery_status(&self) -> String {
        self.build(&["power", "battery", "status"])
    }

    pub fn power_dc_connected(&self) -> String {
        self.build(&["power", "ac", "connected"])
    }

    pub fn power_events_dc_loss(&self) -> String {
        self.build(&["power", "events", "dc_loss"])
    }

    /// Retained standby state — see
    /// [`super::publisher::MqttPublisher::publish_standby_state`].
    pub fn power_standby(&self) -> String {
        self.build(&["power", "standby"])
    }

    /// One-off standby/resume transition.
    pub fn power_events_standby(&self) -> String {
        self.build(&["power", "events", "standby"])
    }

    // Network topics
    pub fn network_status(&self) -> String {
        self.build(&["network", "status"])
    }

    pub fn network_wifi_connected(&self) -> String {
        self.build(&["network", "wifi", "connected"])
    }

    pub fn network_wifi_signal(&self) -> String {
        self.build(&["network", "wifi", "signal"])
    }

    pub fn network_ethernet_connected(&self) -> String {
        self.build(&["network", "ethernet", "connected"])
    }

    // System info topics
    pub fn system_info(&self) -> String {
        self.build(&["system", "info"])
    }

    // Alarm topics
    pub fn alarms_events(&self) -> String {
        self.build(&["alarms", "events"])
    }

    // Accelerometer topics
    pub fn accelerometer_events(&self) -> String {
        self.build(&["accelerometer", "events"])
    }

    // Command topics (for subscription)
    pub fn commands_wildcard(&self) -> String {
        self.build(&["commands", "#"])
    }

    pub fn commands_sensor_set_threshold(&self) -> String {
        self.build(&["commands", "sensor", "set_threshold"])
    }

    pub fn commands_display_set_screen(&self) -> String {
        self.build(&["commands", "display", "set_screen"])
    }

    pub fn commands_system_flush_storage(&self) -> String {
        self.build(&["commands", "system", "flush_storage"])
    }

    pub fn commands_system_get_info(&self) -> String {
        self.build(&["commands", "system", "get_info"])
    }

    pub fn commands_sensor_get_config(&self) -> String {
        self.build(&["commands", "sensor", "get_config"])
    }

    pub fn commands_system_restart(&self) -> String {
        self.build(&["commands", "system", "restart"])
    }

    // Configuration management topics (EU MDR-compliant signed commands)
    pub fn commands_config_request(&self) -> String {
        self.build(&["commands", "config", "request"])
    }

    pub fn commands_config_confirm(&self) -> String {
        self.build(&["commands", "config", "confirm"])
    }

    pub fn config_challenge(&self) -> String {
        self.build(&["config", "challenge"])
    }

    pub fn config_response(&self) -> String {
        self.build(&["config", "response"])
    }

    pub fn config_state(&self) -> String {
        self.build(&["config", "state"])
    }

    // Pairing topics
    pub fn pair_request(&self) -> String {
        self.build(&["pair", "request"])
    }

    pub fn pair_response(&self) -> String {
        self.build(&["pair", "response"])
    }

    // LoRaWAN topics
    pub fn lorawan_gateways(&self) -> String {
        self.build(&["lorawan", "gateways"])
    }

    pub fn lorawan_sensors(&self) -> String {
        self.build(&["lorawan", "sensors"])
    }

    /// Per-sticker fPort-85 config read-back result (Feature C).
    pub fn lorawan_sensor_config(&self, dev_eui: &str) -> String {
        self.build(&["lorawan", "sensors", dev_eui, "config"])
    }

    /// Per-sticker full non-secret config read-back — every readable key, not
    /// just the settable ones. Deliberately a separate topic from `config`: a
    /// Feature-C write publishes to `config`, and if the wide read shared that
    /// topic each write would clobber the read-only snapshot the drawer renders.
    /// Not retained, for the same reason `config` is not.
    pub fn lorawan_sensor_full_config(&self, dev_eui: &str) -> String {
        self.build(&["lorawan", "sensors", dev_eui, "full-config"])
    }

    /// Per-sticker fPort-85 history page (Feature D).
    pub fn lorawan_sensor_history(&self, dev_eui: &str) -> String {
        self.build(&["lorawan", "sensors", dev_eui, "history"])
    }

    /// Per-sticker fPort-85 device info (`GetInfo`, #65). Published **retained**:
    /// it is the device's identity and last-known health, and a sticker only
    /// speaks every `interval_report` (900 s by default), so a reconnecting
    /// viewer must be able to render a firmware version without waiting for an
    /// operator to re-query. Contrast `lorawan_sensor_config`, which is the answer
    /// to one specific query and is deliberately not retained.
    pub fn lorawan_sensor_info(&self, dev_eui: &str) -> String {
        self.build(&["lorawan", "sensors", dev_eui, "info"])
    }

    /// Per-sticker control-command outcome (#71). **Not** retained: it is the
    /// result of one operator action, and a replayed copy would read as a fresh
    /// command to a reconnecting viewer.
    pub fn lorawan_sensor_command(&self, dev_eui: &str) -> String {
        self.build(&["lorawan", "sensors", dev_eui, "command"])
    }

    // EYE BLE tag topics
    pub fn beacon_sensors(&self) -> String {
        self.build(&["eye", "sensors"])
    }

    pub fn beacon_detect(&self) -> String {
        self.build(&["eye", "detect"])
    }

    // Error topic
    pub fn errors(&self) -> String {
        self.build(&["errors"])
    }

    // Response topics
    pub fn responses(&self, command_type: &str) -> String {
        self.build(&["responses", command_type])
    }

    pub fn responses_sensor_config(&self) -> String {
        self.build(&["responses", "sensor_config"])
    }

    pub fn responses_interval_config(&self) -> String {
        self.build(&["responses", "interval_config"])
    }

    // Save-and-feed: on-demand history replay topics
    pub fn export_probe_1m_replay(&self, request_id: &str, sensor_line: u8) -> String {
        let line_s = sensor_line.to_string();
        self.build(&["export", "probe_1m_replay", request_id, &line_s])
    }

    pub fn responses_history(&self) -> String {
        self.build(&["responses", "history"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_builder_with_hostname() {
        let builder = TopicBuilder::new("fiber".to_string(), "DEVICE001".to_string(), true);

        assert_eq!(builder.status(), "fiber/DEVICE001/status");
        assert_eq!(
            builder.sensors_aggregated(),
            "fiber/DEVICE001/sensors/aggregated"
        );
        assert_eq!(
            builder.power_battery_percentage(),
            "fiber/DEVICE001/power/battery/percentage"
        );
        assert_eq!(builder.commands_wildcard(), "fiber/DEVICE001/commands/#");
    }

    #[test]
    fn test_sticker_subtopics() {
        let builder = TopicBuilder::new("fiber".to_string(), "DEVICE001".to_string(), true);
        assert_eq!(
            builder.lorawan_sensor_config("0102030405060708"),
            "fiber/DEVICE001/lorawan/sensors/0102030405060708/config"
        );
        assert_eq!(
            builder.lorawan_sensor_history("0102030405060708"),
            "fiber/DEVICE001/lorawan/sensors/0102030405060708/history"
        );
        // The viewer distinguishes these by suffix, so "/info" must not collide
        // with "/config" or "/history".
        assert_eq!(
            builder.lorawan_sensor_info("0102030405060708"),
            "fiber/DEVICE001/lorawan/sensors/0102030405060708/info"
        );
        // The wide read gets its own topic so a Feature-C write to "/config"
        // cannot clobber the read-only snapshot. Note the hyphen: the viewer
        // routes on `topic.endswith("/full-config")`, and an underscore here
        // would silently fall through to the generic sensors branch.
        assert_eq!(
            builder.lorawan_sensor_full_config("0102030405060708"),
            "fiber/DEVICE001/lorawan/sensors/0102030405060708/full-config"
        );
    }

    #[test]
    fn test_topic_builder_without_hostname() {
        let builder = TopicBuilder::new("fiber".to_string(), "DEVICE001".to_string(), false);

        assert_eq!(builder.status(), "fiber/status");
        assert_eq!(builder.sensors_aggregated(), "fiber/sensors/aggregated");
        assert_eq!(
            builder.power_battery_percentage(),
            "fiber/power/battery/percentage"
        );
        assert_eq!(builder.commands_wildcard(), "fiber/commands/#");
    }

    #[test]
    fn test_all_sensor_lines() {
        let builder = TopicBuilder::new("fiber".to_string(), "TEST".to_string(), true);

        for line in 0..8 {
            let topic = builder.sensor_alarm(line);
            assert!(topic.contains(&format!("line{}", line)));
        }
    }

    #[test]
    fn test_command_topics() {
        let builder = TopicBuilder::new("fiber".to_string(), "TEST".to_string(), true);

        assert_eq!(
            builder.commands_sensor_set_threshold(),
            "fiber/TEST/commands/sensor/set_threshold"
        );
        assert_eq!(
            builder.commands_system_flush_storage(),
            "fiber/TEST/commands/system/flush_storage"
        );
    }
}
