// Power controller for managing LED states based on power status and voltage readings

use std::io;
use std::sync::{Arc, Mutex};

use crate::drivers::stm::StmBridge;
use crate::libs::logging::get_timestamp_str;
use super::status::PowerStatus;

/// Controls power monitoring
/// LED control is now delegated to the dedicated LedMonitor thread
pub struct PowerController {
    stm: Arc<Mutex<StmBridge>>,
    current_status: PowerStatus,
    last_successful_vin_mv: u16,
    last_successful_vbat_mv: u16,
}

impl PowerController {
    /// Initialize power controller with current voltage readings from StmBridge
    pub fn new(stm: Arc<Mutex<StmBridge>>) -> io::Result<Self> {
        // Read current voltage to initialize status
        let mut stm_guard = stm.lock().unwrap_or_else(|e| e.into_inner());
        let (vin_opt, vbat_opt) = stm_guard.read_adc_data()?;
        drop(stm_guard);

        let vin_mv = vin_opt.map(|adc| adc.voltage_mv as u16).unwrap_or(0);
        let vbat_mv = vbat_opt.map(|adc| adc.voltage_mv as u16).unwrap_or(0);

        let current_status = PowerStatus::new(vbat_mv, vin_mv);

        Ok(Self {
            stm,
            current_status,
            last_successful_vin_mv: vin_mv,
            last_successful_vbat_mv: vbat_mv,
        })
    }

    /// Get current power status
    pub fn get_status(&self) -> PowerStatus {
        self.current_status
    }

    /// Update power status (call periodically, e.g., every 100-500ms)
    /// LED control is now handled by the dedicated LedMonitor thread via shared state
    /// If ADC reads timeout (return None), uses last successful values to keep the update loop running
    /// Uses try_lock to avoid blocking while LedMonitor updates LEDs
    pub fn update(&mut self) -> io::Result<()> {
        // Try to acquire lock without blocking - if LedMonitor is using it, skip this update
        let mut stm_guard = match self.stm.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                eprintln!("[{}] [PowerController] STM lock busy, skipping ADC read this iteration", crate::libs::logging::get_timestamp_str());
                // Return success to keep the loop running - we'll use cached values
                return Ok(());
            }
        };

        let (vin_opt, vbat_opt) = stm_guard.read_adc_data()?;
        drop(stm_guard);

        // Handle timeout cases: use last successful values if read returns None
        let vin_mv = if let Some(adc) = vin_opt {
            let voltage = adc.voltage_mv as u16;
            self.last_successful_vin_mv = voltage;  // Update cache on successful read
            voltage
        } else {
            eprintln!("[{}] [PowerController] Warning: VIN read timed out, using cached value: {} mV", get_timestamp_str(), self.last_successful_vin_mv);
            self.last_successful_vin_mv
        };

        let vbat_mv = if let Some(adc) = vbat_opt {
            let voltage = adc.voltage_mv as u16;
            self.last_successful_vbat_mv = voltage;  // Update cache on successful read
            voltage
        } else {
            eprintln!("[{}] [PowerController] Warning: VBAT read timed out, using cached value: {} mV", get_timestamp_str(), self.last_successful_vbat_mv);
            self.last_successful_vbat_mv
        };

        self.current_status = PowerStatus::new(vbat_mv, vin_mv);

        // Note: LED state updates now happen in PowerMonitor via shared state
        // The dedicated LedMonitor thread handles actual LED control

        Ok(())
    }

}
