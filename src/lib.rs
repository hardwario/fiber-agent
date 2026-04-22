// FIBER Medical Thermometer application library
// Provides hardware drivers and application logic for medical temperature monitoring

pub mod drivers;
pub mod libs;

// Re-export key types for convenience
pub use drivers::{StmBridge, St7920, Buttons, Button, ButtonEvent, Buzzer, Lis2dh12};
pub use libs::power::{PowerStatus, SharedPowerStatus, PowerController, PowerMonitor};
pub use libs::config::Config;
pub use libs::accelerometer::AccelerometerMonitor;
pub use libs::sensors::{SensorMonitor, SharedSensorStateHandle};
pub use libs::leds::{LedMonitor, SharedLedState};
pub use libs::buzzer::BuzzerController;
pub use libs::display::{DisplayMonitor, ButtonMonitor};
pub use libs::network::QrCodeGenerator;
pub use libs::mqtt::{MqttMonitor, MqttHandle};
pub use libs::pairing::{PairingMonitor, PairingHandle};
pub use libs::lorawan::{LoRaWANMonitor, LoRaWANHandle};
