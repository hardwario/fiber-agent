// Logging utilities with timestamps

use std::time::{SystemTime, UNIX_EPOCH};

/// Get current time formatted as HH:MM:SS.mmm
pub fn get_timestamp_str() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    let total_secs = duration.as_secs();
    let millis = duration.subsec_millis();

    let hours = (total_secs / 3600) % 24;
    let minutes = (total_secs / 60) % 60;
    let seconds = total_secs % 60;

    format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, seconds, millis)
}

/// Log with timestamp
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[{}] {}", $crate::libs::logging::get_timestamp_str(), format!($($arg)*))
    };
}
