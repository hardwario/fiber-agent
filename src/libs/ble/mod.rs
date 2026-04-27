pub mod advertising;
pub mod config;

pub use advertising::{start_ble_advertising, stop_ble_advertising};
pub use config::BleConfig;
