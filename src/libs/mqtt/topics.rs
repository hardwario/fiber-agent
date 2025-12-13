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
    pub fn sensor_temperature(&self, line: u8) -> String {
        self.build(&["sensors", &format!("line{}", line), "temperature"])
    }

    pub fn sensor_status(&self, line: u8) -> String {
        self.build(&["sensors", &format!("line{}", line), "status"])
    }

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

    pub fn power_ac_connected(&self) -> String {
        self.build(&["power", "ac", "connected"])
    }

    pub fn power_events_ac_loss(&self) -> String {
        self.build(&["power", "events", "ac_loss"])
    }

    // Network topics
    pub fn network_wifi_connected(&self) -> String {
        self.build(&["network", "wifi", "connected"])
    }

    pub fn network_wifi_signal(&self) -> String {
        self.build(&["network", "wifi", "signal"])
    }

    pub fn network_ethernet_connected(&self) -> String {
        self.build(&["network", "ethernet", "connected"])
    }

    // Alarm topics
    pub fn alarms_events(&self) -> String {
        self.build(&["alarms", "events"])
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

    pub fn commands_system_restart(&self) -> String {
        self.build(&["commands", "system", "restart"])
    }

    // Error topic
    pub fn errors(&self) -> String {
        self.build(&["errors"])
    }

    // Response topics
    pub fn responses(&self, command_type: &str) -> String {
        self.build(&["responses", command_type])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_builder_with_hostname() {
        let builder = TopicBuilder::new("fiber".to_string(), "DEVICE001".to_string(), true);

        assert_eq!(builder.status(), "fiber/DEVICE001/status");
        assert_eq!(builder.sensor_temperature(0), "fiber/DEVICE001/sensors/line0/temperature");
        assert_eq!(builder.power_battery_percentage(), "fiber/DEVICE001/power/battery/percentage");
        assert_eq!(builder.commands_wildcard(), "fiber/DEVICE001/commands/#");
    }

    #[test]
    fn test_topic_builder_without_hostname() {
        let builder = TopicBuilder::new("fiber".to_string(), "DEVICE001".to_string(), false);

        assert_eq!(builder.status(), "fiber/status");
        assert_eq!(builder.sensor_temperature(0), "fiber/sensors/line0/temperature");
        assert_eq!(builder.power_battery_percentage(), "fiber/power/battery/percentage");
        assert_eq!(builder.commands_wildcard(), "fiber/commands/#");
    }

    #[test]
    fn test_all_sensor_lines() {
        let builder = TopicBuilder::new("fiber".to_string(), "TEST".to_string(), true);

        for line in 0..8 {
            let topic = builder.sensor_temperature(line);
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
