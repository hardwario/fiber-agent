// FIBER Medical Thermometer application library
// Provides hardware drivers and application logic for medical temperature monitoring

pub mod drivers;
pub mod libs;

// Re-export key types for convenience
pub use drivers::{Button, ButtonEvent, Buttons, Buzzer, Lis2dh12, St7920, StmBridge};
pub use libs::accelerometer::AccelerometerMonitor;
pub use libs::ble::{spawn_ble_event_router, BleConfig, BleEvent, BleHandle, BleMonitor};
pub use libs::buzzer::BuzzerController;
pub use libs::config::Config;
pub use libs::config_applier::ConfigApplier;
pub use libs::display::{ButtonMonitor, DisplayMonitor};
pub use libs::leds::{LedMonitor, SharedLedState};
pub use libs::lorawan::{LoRaWANHandle, LoRaWANMonitor};
pub use libs::mqtt::{MqttHandle, MqttMonitor};
pub use libs::network::{
    new_shared_provisioning_session, touch_shared, ProvisioningSession, QrCodeGenerator,
    SharedProvisioningSession, DEFAULT_SESSION_DURATION, IDLE_TIMEOUT,
};
pub use libs::pairing::{PairingHandle, PairingMonitor};
pub use libs::power::{PowerController, PowerMonitor, PowerStatus, SharedPowerStatus};
pub use libs::sensors::{SensorMonitor, SharedSensorStateHandle};
