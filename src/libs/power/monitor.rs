// Background monitoring thread for continuous power monitoring

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::controller::PowerController;
use super::link::CarrierWatch;
use super::standby::{self, ResumeWatch, StandbyMarker, WakeReason};
use super::status::{DcThresholds, SharedPowerStatus};
use crate::drivers::stm::StmBridge;
use crate::libs::buzzer::pattern::BuzzerPattern;
use crate::libs::buzzer::{BuzzerController, BuzzerPriorityManager};
use crate::libs::config::{BuzzerTiming, StandbyConfig};
use crate::libs::leds::state::PowerLedColor;
use crate::libs::leds::SharedLedStateHandle;
use crate::libs::logging::get_timestamp_str;
use crate::libs::mqtt::messages::MqttMessage;
use crate::libs::storage::StorageHandle;
use crossbeam::channel::Sender;

/// Longest single sleep between shutdown / standby-state checks.
///
/// The loop's own interval is a configured 60 s on-device, so sleeping it in one
/// go would leave the device up to a minute late noticing that standby had been
/// entered — a minute of a "powered off" device still showing a lit power LED —
/// and would delay process shutdown by the same amount.
const TICK_SLICE: Duration = Duration::from_millis(250);

/// Background power monitoring thread
pub struct PowerMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl PowerMonitor {
    /// Create and spawn background power monitoring thread
    ///
    /// The thread will continuously monitor power status and update the shared LED state
    /// at the specified update interval. Actual LED control happens in the dedicated LedMonitor thread.
    /// Also manages buzzer alerts for power events.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stm: Arc<Mutex<StmBridge>>,
        update_interval_ms: u64,
        led_state: SharedLedStateHandle,
        buzzer: Arc<Mutex<BuzzerController>>,
        priority_manager: Arc<BuzzerPriorityManager>,
        power_status: SharedPowerStatus,
        mqtt_sender: Option<Sender<MqttMessage>>,
        standby_cfg: StandbyConfig,
        dc_thresholds: DcThresholds,
        storage_handle: Option<StorageHandle>,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let thread_handle = thread::spawn(move || {
            Self::monitor_loop(
                stm,
                shutdown_flag_clone,
                update_interval_ms,
                led_state,
                buzzer,
                priority_manager,
                power_status,
                mqtt_sender,
                standby_cfg,
                dc_thresholds,
                storage_handle,
            );
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
        })
    }

    /// Bring the device back from standby. Runs on the poll that confirmed PoE.
    ///
    /// Idempotent through [`standby::resume`]: the state flip is a
    /// compare-exchange, so only the caller that actually changed it does the
    /// work. Everything after that is best-effort — once the device has decided
    /// it is awake, a failure to restore any one thing must not leave it stuck
    /// half-asleep.
    #[allow(clippy::too_many_arguments)]
    fn resume_from_standby(
        stm: &Arc<Mutex<StmBridge>>,
        buzzer: &Arc<Mutex<BuzzerController>>,
        mqtt_sender: &Option<Sender<MqttMessage>>,
        storage_handle: &Option<StorageHandle>,
        vin_mv: u16,
        reason: WakeReason,
    ) {
        if !standby::resume() {
            return;
        }

        eprintln!(
            "[{}] [PowerMonitor] Resuming from standby (trigger={}, vin={} mV)",
            get_timestamp_str(),
            reason.as_str(),
            vin_mv
        );

        StandbyMarker::clear(&standby::configured_marker_dir());
        standby::restore_cpu_governor();

        // Sensor rails first: the display coming back must not show eight
        // disconnected lines because their power is still off.
        match stm.lock() {
            Ok(mut guard) => {
                if let Err(e) = guard.set_sensor_power(true) {
                    eprintln!("[PowerMonitor] WARN: could not restore sensor rails: {e}");
                }
            }
            Err(e) => eprintln!("[PowerMonitor] WARN: STM bridge lock poisoned on resume: {e}"),
        }

        crate::libs::display::blank::cancel_blank();

        // A device that starts monitoring on its own says so out loud.
        if let Ok(bz) = buzzer.lock() {
            bz.play_once(BuzzerPattern::ReconnectionHappy { frequency_hz: 150 });
        }

        if let Some(storage) = storage_handle {
            let details = format!(r#"{{"vin_mv":{},"trigger":"{}"}}"#, vin_mv, reason.as_str());
            if let Err(e) = storage.log_audit_event(
                "POWER_RESUME".to_string(),
                Some("audit_log".to_string()),
                Some(details),
            ) {
                eprintln!("[PowerMonitor] WARN: failed to queue POWER_RESUME audit row: {e}");
            }
        }

        if let Some(sender) = mqtt_sender {
            let _ = sender.try_send(MqttMessage::PublishStandbyState {
                standby: false,
                reason: match reason {
                    WakeReason::DcPresent => "PoE reconnected".to_string(),
                    WakeReason::Button => "Woken with the local button".to_string(),
                },
                requested_by: String::new(),
                entered_at: None,
                vin_mv,
            });
        }
    }

    /// Sleep up to `interval`, waking early on shutdown or a standby-state change.
    ///
    /// Returns false if the process is shutting down.
    fn sleep_watching(shutdown_flag: &AtomicBool, interval: Duration, standby_was: bool) -> bool {
        let deadline = Instant::now() + interval;
        loop {
            if shutdown_flag.load(Ordering::Relaxed) {
                return false;
            }
            if standby::is_standby() != standby_was {
                // The state changed under us — go round again immediately so the
                // power LED and the poll interval both follow it.
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            thread::sleep(remaining.min(TICK_SLICE));
        }
    }

    /// Background monitoring loop
    #[allow(clippy::too_many_arguments)]
    fn monitor_loop(
        stm: Arc<Mutex<StmBridge>>,
        shutdown_flag: Arc<AtomicBool>,
        update_interval_ms: u64,
        led_state: SharedLedStateHandle,
        buzzer: Arc<Mutex<BuzzerController>>,
        priority_manager: Arc<BuzzerPriorityManager>,
        power_status: SharedPowerStatus,
        mqtt_sender: Option<Sender<MqttMessage>>,
        standby_cfg: StandbyConfig,
        dc_thresholds: DcThresholds,
        storage_handle: Option<StorageHandle>,
    ) {
        // The controller takes the bridge, but standby needs it too — to drop the
        // sensor rails on the way down and raise them again on the way back.
        let stm_for_standby = stm.clone();

        // Create power controller
        let mut controller = match PowerController::new_with_thresholds(stm, dc_thresholds) {
            Ok(ctrl) => ctrl,
            Err(e) => {
                eprintln!("[PowerMonitor] Failed to initialize controller: {}", e);
                return;
            }
        };

        // Set update interval from configuration
        let update_interval = Duration::from_millis(update_interval_ms);

        // State tracking for buzzer alerts
        let mut previous_vin_status = false; // Was on AC power?

        // The first reading has nothing real to compare against — without this,
        // a device that was on DC power the whole time looks like it "just
        // reconnected" on every process start (e.g. the EYE watchdog's restart),
        // firing a false Power Supply CRITICAL->NORMAL alarm and buzzer chime.
        let mut first_vin_reading = true;

        let mut previous_critical_status = false; // Was battery critical?
        let mut last_battery_beep = Instant::now(); // When was the last battery mode beep?
        let battery_beep_interval = Duration::from_secs(10); // Beep every 10 seconds in battery mode

        // Armed on the tick that first sees standby, from the VIN reading at that
        // moment, so "PoE arrived" means an edge rather than a level. None while
        // awake.
        let mut resume_watch: Option<ResumeWatch> = None;
        // Baselined alongside it. This is what makes an unplug shorter than one
        // poll interval detectable at all — see link::CarrierWatch.
        let mut carrier_watch: Option<CarrierWatch> = None;
        let mut previous_standby = false;
        // Throttle for the standby heartbeat, and the last state we reported, so
        // the journal shows every change without a line per poll.
        let mut last_standby_log = Instant::now();
        let mut last_logged_standby: Option<(bool, bool, u32)> = None;
        const STANDBY_HEARTBEAT: Duration = Duration::from_secs(60);

        eprintln!(
            "[{}] [PowerMonitor] Started power monitoring with {}ms interval",
            get_timestamp_str(),
            update_interval_ms
        );

        // Main monitoring loop
        loop {
            // Check for shutdown signal
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!(
                    "[{}] [PowerMonitor] Shutdown signal received, exiting monitor thread",
                    get_timestamp_str()
                );
                break;
            }

            let in_standby = standby::is_standby();
            if !in_standby && previous_standby {
                // Woken by something other than this loop (the boot path, or a
                // future local control). Drop the watch so a later entry re-arms
                // from a fresh reading.
                resume_watch = None;
                carrier_watch = None;
                last_logged_standby = None;
            }
            previous_standby = in_standby;

            // Perform power status update
            //eprintln!("[{}] [PowerMonitor] Attempting ADC read...", get_timestamp_str());
            let update_start = Instant::now();
            match controller.update() {
                Ok(()) => {
                    let update_duration = update_start.elapsed();
                    // eprintln!("[{}] [PowerMonitor] ADC read completed in {}ms", get_timestamp_str(), update_duration.as_millis());
                    let status = controller.get_status();
                    let current_vin_status = status.is_on_dc_power();

                    /*                     eprintln!(
                        "[{}] [PowerMonitor] Battery: {} mV, VIN: {} mV, AC: {}, Low: {}, Critical: {}",
                        get_timestamp_str(),
                        status.vbat_mv,
                        status.vin_mv,
                        status.is_on_dc_power(),
                        status.is_low(),
                        status.is_critical()
                    ); */

                    // Update shared power status
                    if let Ok(mut ps) = power_status.lock() {
                        *ps = status;
                    }

                    // Update shared LED state
                    // The set_power_leds() method automatically notifies the LED monitor of changes
                    if in_standby {
                        // Standby owns the power LED, or the status-derived state
                        // below would repaint it green/yellow every tick and the
                        // device would look as though it were still running.
                        //
                        // Lime is the one colour the firmware accepts that
                        // get_pwr_led_state() never returns, so it cannot be
                        // confused with any running state — and yellow-blinking
                        // already means two different things (battery OK and
                        // battery critical). Slow blink rather than fast: this is
                        // a device resting, not a device in trouble.
                        led_state.set_power_leds(PowerLedColor::Lime, true);
                    } else {
                        let (color, blink) = status.get_pwr_led_state();
                        led_state.set_power_leds(color, blink);
                    }

                    // Arm the resume watch on the tick that first sees standby,
                    // using the reading we just took: if DC is present right now,
                    // it has to go away before coming back counts as an arrival.
                    if in_standby && resume_watch.is_none() {
                        let watch = ResumeWatch::new(standby_cfg.confirm_polls, current_vin_status);
                        // Baseline the carrier counter now, so only interruptions
                        // from this point on count. Without it a brief unplug is
                        // invisible: the VIN poll simply never samples the gap.
                        carrier_watch = CarrierWatch::new();
                        // A press held as the device went down must not wake it
                        // again immediately.
                        standby::clear_wake_request();
                        eprintln!(
                            "[{}] [PowerMonitor] Standby: watching VIN every {}ms (armed={}, evidence={:?}, carrier={}, resume_on_dc={})",
                            get_timestamp_str(),
                            standby_cfg.poll_interval_ms,
                            watch.is_armed(),
                            watch.evidence(),
                            carrier_watch
                                .as_ref()
                                .map_or("unavailable".to_string(), |c| c.baseline().to_string()),
                            standby_cfg.resume_on_dc
                        );
                        // One short beep so a bystander knows monitoring stopped.
                        if let Ok(bz) = buzzer.lock() {
                            bz.play_once(BuzzerPattern::BatteryModeBeep(BuzzerTiming {
                                on_ms: 150,
                                off_ms: 0,
                            }));
                        }

                        // Announce it from here rather than from the command
                        // handler, so entry and resume are published by the same
                        // code on the same edges — including the entry that
                        // happens at boot, which no command handler sees. The
                        // reason and signer come from the marker the handler wrote.
                        if let Some(sender) = mqtt_sender.as_ref() {
                            let marker = StandbyMarker::read(&standby::configured_marker_dir());
                            let _ = sender.try_send(MqttMessage::PublishStandbyState {
                                standby: true,
                                reason: marker
                                    .as_ref()
                                    .map(|m| m.reason.clone())
                                    .unwrap_or_else(|| "unknown".to_string()),
                                requested_by: marker
                                    .as_ref()
                                    .map(|m| m.requested_by.clone())
                                    .unwrap_or_default(),
                                entered_at: marker.as_ref().map(|m| m.entered_at_rfc3339()),
                                vin_mv: status.vin_mv,
                            });
                        }

                        resume_watch = Some(watch);
                    }

                    // Handle VIN connection/disconnection transitions
                    if first_vin_reading {
                        // Establish the baseline silently — there is no prior
                        // state yet, so this can't be a real edge.
                        first_vin_reading = false;
                        priority_manager.set_battery_reminder(!current_vin_status);
                    } else if current_vin_status && !previous_vin_status {
                        // VIN just connected (DC power detected)
                        eprintln!(
                            "[{}] [PowerMonitor] DC power detected - VIN connected",
                            get_timestamp_str()
                        );
                        priority_manager.set_battery_reminder(false);

                        // In standby the resume plays this same pattern once the
                        // connection has been confirmed. Beeping here too would
                        // sound twice for one cable, and the first beep would be
                        // for a connection that may still fail the debounce.
                        if !in_standby {
                            if let Ok(bz) = buzzer.lock() {
                                bz.play_once(BuzzerPattern::ReconnectionHappy {
                                    frequency_hz: 150,
                                });
                            }
                        }

                        // Send power restored alarm event (clears active alarm)
                        if let Some(ref sender) = mqtt_sender {
                            let _ = sender.try_send(MqttMessage::PublishSystemAlarmEvent {
                                alarm_type: "POWER_DISCONNECT".to_string(),
                                name: "Power Supply".to_string(),
                                from_state: "CRITICAL".to_string(),
                                to_state: "NORMAL".to_string(),
                                message: "DC power restored".to_string(),
                            });
                        }
                    } else if !current_vin_status && previous_vin_status {
                        // VIN just disconnected (lost AC power, switched to battery)
                        eprintln!(
                            "[{}] [PowerMonitor] DC power lost - switched to battery",
                            get_timestamp_str()
                        );
                        priority_manager.set_battery_reminder(true);

                        // Record DC loss timestamp in shared power status
                        if let Ok(mut ps) = power_status.lock() {
                            ps.record_dc_loss();
                            eprintln!(
                                "[{}] [PowerMonitor] DC loss timestamp recorded",
                                get_timestamp_str()
                            );
                        }

                        // A device the operator switched off must not announce
                        // losing a supply it is not using.
                        if !in_standby {
                            if let Ok(bz) = buzzer.lock() {
                                let vin_disconnect_timing = BuzzerTiming {
                                    on_ms: 2000, // 2 second long beep
                                    off_ms: 0,
                                };
                                bz.play_once(BuzzerPattern::VinDisconnectBeep(
                                    vin_disconnect_timing,
                                ));
                            }
                        }

                        // Send power disconnect alarm event
                        if let Some(ref sender) = mqtt_sender {
                            let _ = sender.try_send(MqttMessage::PublishSystemAlarmEvent {
                                alarm_type: "POWER_DISCONNECT".to_string(),
                                name: "Power Supply".to_string(),
                                from_state: "NORMAL".to_string(),
                                to_state: "CRITICAL".to_string(),
                                message: "DC power disconnected".to_string(),
                            });
                        }
                    }

                    // Handle battery mode reminder beeps (every 10 seconds)
                    // Never in standby: a device that looks off and is meant to be
                    // off must not beep at the room every ten seconds, and running
                    // on battery is the expected state there rather than a fault.
                    if !in_standby
                        && status.is_on_battery()
                        && last_battery_beep.elapsed() >= battery_beep_interval
                    {
                        if priority_manager.should_play_battery_reminder() {
                            eprintln!(
                                "[{}] [PowerMonitor] Battery mode reminder beep",
                                get_timestamp_str()
                            );
                            if let Ok(bz) = buzzer.lock() {
                                let battery_mode_timing = BuzzerTiming {
                                    on_ms: 100, // 100ms beep
                                    off_ms: 100,
                                };
                                bz.play_once(BuzzerPattern::BatteryModeBeep(battery_mode_timing));
                            }
                        }
                        last_battery_beep = Instant::now();
                    }

                    // Handle critical battery alert (repeating)
                    // Use BuzzerPriorityManager to coordinate with sensor critical alarms
                    let current_critical_status = status.is_critical();
                    if in_standby {
                        // The repeating critical-battery alert is the loudest thing
                        // this device does. In standby there are no readings to
                        // protect and nobody is expected to intervene, so it must
                        // stay clear — the flag is cleared rather than merely not
                        // set, in case standby was entered while already critical.
                        if previous_critical_status {
                            priority_manager.set_battery_critical(false);
                        }
                    } else if current_critical_status && !previous_critical_status {
                        // Just entered critical state - notify priority manager
                        eprintln!(
                            "[{}] [PowerMonitor] Battery critical - notifying priority manager",
                            get_timestamp_str()
                        );
                        priority_manager.set_battery_critical(true);
                    } else if !current_critical_status && previous_critical_status {
                        // Just left critical state - notify priority manager
                        eprintln!(
                            "[{}] [PowerMonitor] Battery recovered - clearing critical flag",
                            get_timestamp_str()
                        );
                        priority_manager.set_battery_critical(false);
                    }

                    // Update previous states for next iteration
                    previous_vin_status = current_vin_status;
                    previous_critical_status = current_critical_status;

                    // Standby: has PoE newly arrived, or has someone asked us up?
                    if in_standby {
                        // The button escape hatch outranks everything: it exists so
                        // a device whose supply cannot be detected can never be
                        // stranded dark.
                        if let Some(reason) = standby::take_wake_request() {
                            Self::resume_from_standby(
                                &stm_for_standby,
                                &buzzer,
                                &mqtt_sender,
                                &storage_handle,
                                status.vin_mv,
                                reason,
                            );
                            resume_watch = None;
                            carrier_watch = None;
                        } else if standby_cfg.resume_on_dc {
                            // The single hysteretic DC signal, the same one the LED,
                            // the LCD, MQTT and the alarm edge use. It must not be a
                            // separate comparison: the first version tested
                            // `vin_mv >= 12000` here, which the southbridge's VIN
                            // maths makes unreachable at a nominal 12 V.
                            let dc_present = status.is_on_dc_power();
                            // A cached reading is evidence of nothing. Confirming on
                            // one could wake a device from a sample taken before the
                            // supply was pulled.
                            let fresh = controller.vin_fresh();
                            let link_bounced = carrier_watch
                                .as_ref()
                                .is_some_and(|c| c.bounced_since_baseline());

                            if let Some(watch) = resume_watch.as_mut() {
                                let fired = if fresh {
                                    watch.observe(dc_present, link_bounced)
                                } else {
                                    watch.break_run();
                                    false
                                };

                                // Log on any change, plus a heartbeat, so a field
                                // failure is diagnosable from the journal alone —
                                // the first version logged nothing per poll, which
                                // is why this bug needed a bench session to find.
                                let snapshot =
                                    (watch.is_armed(), dc_present, watch.confirm_progress());
                                if last_logged_standby != Some(snapshot)
                                    || last_standby_log.elapsed() >= STANDBY_HEARTBEAT
                                {
                                    eprintln!(
                                        "[{}] [PowerMonitor] Standby: vin={}mV fresh={} dc={} armed={} evidence={:?} link_bounced={} confirm={}/{}",
                                        get_timestamp_str(),
                                        status.vin_mv,
                                        fresh,
                                        dc_present,
                                        watch.is_armed(),
                                        watch.evidence(),
                                        link_bounced,
                                        watch.confirm_progress(),
                                        standby_cfg.confirm_polls.max(1),
                                    );
                                    last_logged_standby = Some(snapshot);
                                    last_standby_log = Instant::now();
                                }

                                if fired {
                                    Self::resume_from_standby(
                                        &stm_for_standby,
                                        &buzzer,
                                        &mqtt_sender,
                                        &storage_handle,
                                        status.vin_mv,
                                        WakeReason::DcPresent,
                                    );
                                    resume_watch = None;
                                    carrier_watch = None;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[{}] [PowerMonitor] Error during update: {}",
                        get_timestamp_str(),
                        e
                    );
                    // Continue on error - don't crash the monitor thread.
                    //
                    // In standby a failed read is not evidence of DC: break the
                    // confirmation run rather than letting a stale cached VIN carry
                    // it to completion and wake a device on a supply that is not
                    // actually there. Deliberately break_run rather than
                    // observe(false) — see its doc comment.
                    if let Some(watch) = resume_watch.as_mut() {
                        watch.break_run();
                    }
                }
            }

            // Sleep before the next update, waking early on shutdown or a change
            // of standby state. Standby has its own, much shorter interval: this
            // one is the wake latency, and update_interval_ms ships as 60 s.
            let interval = if in_standby {
                Duration::from_millis(standby_cfg.poll_interval_ms)
            } else {
                update_interval
            };
            if !Self::sleep_watching(&shutdown_flag, interval, in_standby) {
                eprintln!(
                    "[{}] [PowerMonitor] Shutdown signal received, exiting monitor thread",
                    get_timestamp_str()
                );
                break;
            }
        }

        eprintln!(
            "[{}] [PowerMonitor] Monitor thread exited cleanly",
            get_timestamp_str()
        );
    }

    /// Gracefully shutdown the monitoring thread
    pub fn shutdown(mut self) -> io::Result<()> {
        // Signal the thread to shutdown
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for thread to finish
        if let Some(handle) = self.thread_handle.take() {
            handle.join().ok();
        }

        Ok(())
    }
}

impl Drop for PowerMonitor {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for thread with a timeout
        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(2);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
