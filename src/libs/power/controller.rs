// Power controller for managing LED states based on power status and voltage readings

use std::io;
use std::sync::{Arc, Mutex};

use super::status::{DcDetector, DcThresholds, PowerStatus};
use crate::drivers::stm::StmBridge;
use crate::libs::logging::get_timestamp_str;

/// Controls power monitoring
/// LED control is now delegated to the dedicated LedMonitor thread
pub struct PowerController {
    stm: Arc<Mutex<StmBridge>>,
    current_status: PowerStatus,
    last_successful_vin_mv: u16,
    last_successful_vbat_mv: u16,
    /// The one DC-present decision in the process. Held here because this is
    /// where readings arrive, and it must see every reading in order to apply its
    /// hysteresis.
    dc: DcDetector,
    /// Whether the last `update` actually took a fresh reading, as opposed to
    /// reusing a cached value because the STM lock was busy or the ADC timed out.
    /// The standby resume must not confirm PoE from a stale sample.
    vin_fresh: bool,
}

impl PowerController {
    /// Initialize power controller with current voltage readings from StmBridge
    pub fn new(stm: Arc<Mutex<StmBridge>>) -> io::Result<Self> {
        Self::new_with_thresholds(stm, DcThresholds::default())
    }

    /// Initialize with an explicit DC hysteresis pair from configuration.
    pub fn new_with_thresholds(
        stm: Arc<Mutex<StmBridge>>,
        thresholds: DcThresholds,
    ) -> io::Result<Self> {
        // Read current voltage to initialize status
        let mut stm_guard = stm.lock().unwrap_or_else(|e| e.into_inner());
        let (vin_opt, vbat_opt) = stm_guard.read_adc_data()?;
        drop(stm_guard);

        let vin_fresh = vin_opt.is_some();
        let vin_mv = vin_opt.map(|adc| adc.voltage_mv as u16).unwrap_or(0);
        let vbat_mv = vbat_opt.map(|adc| adc.voltage_mv as u16).unwrap_or(0);

        // Seeded rather than fed: the first reading has no previous side to have
        // crossed from, so it adopts the state instead of counting as an edge.
        let dc = DcDetector::seeded(thresholds, vin_mv);
        let current_status = PowerStatus::with_dc(vbat_mv, vin_mv, dc.is_on_dc());

        Ok(Self {
            stm,
            current_status,
            last_successful_vin_mv: vin_mv,
            last_successful_vbat_mv: vbat_mv,
            dc,
            vin_fresh,
        })
    }

    /// Whether the VIN in [`Self::get_status`] came from a reading just taken.
    pub fn vin_fresh(&self) -> bool {
        self.vin_fresh
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
                eprintln!(
                    "[{}] [PowerController] STM lock busy, skipping ADC read this iteration",
                    crate::libs::logging::get_timestamp_str()
                );
                // Return success to keep the loop running - we'll use cached values.
                // Mark the reading stale: the status still holds the previous
                // sample, and a standby resume must not confirm PoE from it.
                self.vin_fresh = false;
                return Ok(());
            }
        };

        let (vin_opt, vbat_opt) = stm_guard.read_adc_data()?;
        drop(stm_guard);

        // Handle timeout cases: use last successful values if read returns None
        self.vin_fresh = vin_opt.is_some();
        let vin_mv = if let Some(adc) = vin_opt {
            let voltage = adc.voltage_mv as u16;
            self.last_successful_vin_mv = voltage; // Update cache on successful read
            voltage
        } else {
            eprintln!(
                "[{}] [PowerController] Warning: VIN read timed out, using cached value: {} mV",
                get_timestamp_str(),
                self.last_successful_vin_mv
            );
            self.last_successful_vin_mv
        };

        let vbat_mv = if let Some(adc) = vbat_opt {
            let voltage = adc.voltage_mv as u16;
            self.last_successful_vbat_mv = voltage; // Update cache on successful read
            voltage
        } else {
            eprintln!(
                "[{}] [PowerController] Warning: VBAT read timed out, using cached value: {} mV",
                get_timestamp_str(),
                self.last_successful_vbat_mv
            );
            self.last_successful_vbat_mv
        };

        // Only a fresh reading may move the detector. Feeding it a cached value
        // would let one stale sample drive a state change it did not observe.
        let on_dc = if self.vin_fresh {
            self.dc.update(vin_mv)
        } else {
            self.dc.is_on_dc()
        };
        self.current_status = PowerStatus::with_dc(vbat_mv, vin_mv, on_dc);

        // Note: LED state updates now happen in PowerMonitor via shared state
        // The dedicated LedMonitor thread handles actual LED control

        Ok(())
    }
}
