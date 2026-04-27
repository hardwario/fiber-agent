pub mod auth;
pub mod device_info;
pub mod service;
pub mod state;
pub mod terminal;
pub mod wifi;

// Forward-declared event enum. The full BleMonitor + BleHandle are added in Task 12.
#[derive(Debug, Clone)]
pub enum BleEvent {
    ClientConnected { addr: String },
    ClientDisconnected,
    AuthSuccess,
    AuthFailed,
    WifiConnecting { ssid: String },
    WifiConnected { ssid: String, ip: String },
    WifiFailed { error: String },
}
