//! Button monitoring thread for screen navigation control.
//!
//! This is the effectful half of the front-panel input path: it polls the GPIO,
//! gathers the ambient facts the gesture logic needs, hands them to
//! [`ButtonFsm::tick`], and applies whatever effects come back. All of the
//! gesture logic — holds, double-clicks, timeouts, cancellation — lives in
//! [`super::button_fsm`], where it can be tested without hardware.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::button_fsm::{ButtonFsm, Effect, Inputs, Levels};
use super::supervise::{lock_recover, read_recover};
use super::{LocalAction, Screen, SharedDisplayStateHandle};
use crate::drivers::buttons::{Button, ButtonEvent, Buttons};
use crate::libs::buzzer::BuzzerPriorityManager;
use crate::libs::mqtt::monitor::SharedStmBridge;
use crate::libs::network::{ProvisioningSession, SharedProvisioningSession};
use crate::libs::pairing::PairingHandle;
use crate::libs::storage::StorageHandle;

/// Snapshot the DS18B20 readings and their has-ever-reported flags, which the
/// overview paging and cursor maths need. Taken under one guard, and released
/// before the caller locks `display_state` (that order must not invert).
///
/// Recovers a poisoned lock via [`read_recover`] rather than substituting empty
/// readings. An all-`false` `has_reported` is not a safe default here: every
/// sensor counts as never-reported, so the overview would hide all of them and
/// the screen would go blank on a poisoning it could otherwise have ridden out.
fn ds_readings_snapshot(
    sensor_state: &crate::libs::sensors::SharedSensorStateHandle,
) -> (
    [Option<crate::libs::sensors::state::SensorReading>; 8],
    [bool; 8],
) {
    let state = read_recover(sensor_state);
    (state.readings.clone(), state.has_reported)
}

/// Everything needed to carry out an [`Effect`]. The state machine decides; this
/// does the touching of locks, BLE, storage and the system.
struct Applier {
    display_state: SharedDisplayStateHandle,
    pairing_handle: Option<PairingHandle>,
    buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
    sensor_state: crate::libs::sensors::SharedSensorStateHandle,
    provisioning_session: SharedProvisioningSession,
    mac_address: String,
    hostname: String,
    storage_handle: Option<StorageHandle>,
    stm_bridge: Option<SharedStmBridge>,
}

impl Applier {
    fn apply(&self, effect: Effect) {
        match effect {
            Effect::MarkActivity => lock_recover(&self.display_state).mark_activity(),

            Effect::SilenceBeep => {
                if let Some(ref bp) = self.buzzer_priority {
                    bp.silence_beep_30min();
                    eprintln!("[ButtonMonitor] Beep silenced by button press (30 min)");
                }
            }

            Effect::ShowSensorOverview => {
                lock_recover(&self.display_state).show_sensor_overview();
                eprintln!("[ButtonMonitor] Returning to sensor overview");
            }

            Effect::ShowSystemInfo => {
                lock_recover(&self.display_state).show_system_info();
                eprintln!("[ButtonMonitor] DOWN hold complete - showing system info");
            }

            Effect::ShowMenu => {
                lock_recover(&self.display_state).show_menu();
                eprintln!("[ButtonMonitor] UP hold complete - opening local action menu");
            }

            Effect::ShowMenuPage(page, selected) => {
                lock_recover(&self.display_state).show_menu_page(page, selected)
            }

            Effect::MenuUp => lock_recover(&self.display_state).menu_up(),
            Effect::MenuDown => lock_recover(&self.display_state).menu_down(),
            Effect::ConfirmToggle => lock_recover(&self.display_state).confirm_toggle(),

            Effect::ShowConfirm(action) => {
                lock_recover(&self.display_state).show_confirm(action);
                eprintln!(
                    "[ButtonMonitor] {:?} selected - asking for confirmation",
                    action
                );
            }

            Effect::OpenQrSession => self.open_qr_session(),
            Effect::CloseQrSession => {
                eprintln!("[ButtonMonitor] Provisioning session closed by user");
                self.tear_down_qr_session();
            }
            Effect::ExpireQrSession => {
                eprintln!("[ButtonMonitor] Provisioning session idle for 5min - tearing down");
                self.tear_down_qr_session();
            }

            Effect::StartPairing => {
                if let Some(ref ph) = self.pairing_handle {
                    ph.start_pairing();
                    eprintln!("[ButtonMonitor] Pairing triggered from menu");
                }
            }

            Effect::ShowPairingUnavailable => {
                lock_recover(&self.display_state).show_pairing_unavailable();
                eprintln!("[ButtonMonitor] Pairing code chosen, but pairing is unavailable");
            }

            Effect::CancelPairing => {
                if let Some(ref ph) = self.pairing_handle {
                    ph.cancel_pairing();
                    eprintln!("[ButtonMonitor] Pairing cancelled by button");
                }
            }

            Effect::NextPage => {
                let (readings, reported) = ds_readings_snapshot(&self.sensor_state);
                lock_recover(&self.display_state).next_page(&readings, &reported);
            }

            Effect::SelectionUp => {
                let (readings, reported) = ds_readings_snapshot(&self.sensor_state);
                lock_recover(&self.display_state).selection_up(&readings, &reported);
            }

            Effect::SelectionDown => {
                let (readings, reported) = ds_readings_snapshot(&self.sensor_state);
                lock_recover(&self.display_state).selection_down(&readings, &reported);
            }

            Effect::EnterSelectionMode => {
                let (readings, reported) = ds_readings_snapshot(&self.sensor_state);
                lock_recover(&self.display_state).enter_selection_mode(&readings, &reported);
                eprintln!("[ButtonMonitor] Double-click - entering selection mode");
            }

            Effect::EnterDetailView => {
                lock_recover(&self.display_state).enter_detail_view();
                eprintln!("[ButtonMonitor] Entering sensor detail view");
            }

            Effect::ExitDetailView => {
                let (readings, reported) = ds_readings_snapshot(&self.sensor_state);
                lock_recover(&self.display_state).exit_detail_view(&readings, &reported);
            }

            Effect::LoraDetailPrev => lock_recover(&self.display_state).lorawan_detail_prev(),
            Effect::LoraDetailNext => lock_recover(&self.display_state).lorawan_detail_next(),

            Effect::ExecuteTeardown(action) => self.execute_teardown(action),

            Effect::RequestStandbyWake => {
                eprintln!("[ButtonMonitor] Standby wake: button held");
                crate::libs::power::standby::request_wake(
                    crate::libs::power::standby::WakeReason::Button,
                );
            }

            Effect::WarnStaleMenuSelection => eprintln!(
                "[ButtonMonitor] ENTER released against a screen that is no longer the menu \
                 — ignored (another thread replaced it)"
            ),
        }
    }

    /// Mint a fresh ephemeral provisioning session BEFORE starting BLE
    /// advertising — the GATT auth path reads this on every pairing attempt, so
    /// it must be live by the time the phone connects.
    fn open_qr_session(&self) {
        // EDIT (control-ble integration). The token is minted by the crate, not here, because the
        // crate is what checks it at FB01. Two mints would mean the screen showed a code the Hub
        // would refuse.
        //
        // Note what is no longer logged. v1 printed the token to stderr on every ENTER hold, which
        // put a live credential into journald and into every support bundle taken afterwards.
        let mac = match control_ble::mac::MacAddr::parse(&self.mac_address) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[ButtonMonitor] Cannot open a phone session: {e}");
                return;
            }
        };
        let payload = match crate::libs::ble_host::begin_phone_session(
            control_ble::peripheral::session::DEFAULT_IDLE_TIMEOUT_S,
            &mac,
            &self.hostname,
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[ButtonMonitor] Cannot open a phone session: {e}");
                return;
            }
        };

        match ProvisioningSession::from_payload(&payload) {
            Ok(session) => {
                if let Ok(mut slot) = self.provisioning_session.write() {
                    eprintln!(
                        "[ButtonMonitor] Provisioning session opened (expires_at={})",
                        payload.exp
                    );
                    *slot = Some(session);
                } else {
                    eprintln!("[ButtonMonitor] Failed to lock provisioning session for write");
                }
            }
            Err(e) => eprintln!(
                "[ButtonMonitor] Failed to render the provisioning QR: {}",
                e
            ),
        }

        lock_recover(&self.display_state).show_qr_code();
        eprintln!("[ButtonMonitor] Countdown complete - transitioning to QR code screen");
    }

    /// End the provisioning session before tearing down BLE — invalidates the
    /// token first so a racing pairing attempt fails closed.
    fn tear_down_qr_session(&self) {
        if let Ok(mut slot) = self.provisioning_session.write() {
            *slot = None;
        }
        // The crate stops the session before it stops advertising, so a pairing attempt racing this
        // teardown fails closed.
        if let Err(e) = crate::libs::ble_host::end_phone_session() {
            eprintln!("[ButtonMonitor] Failed to end the phone session: {}", e);
        }
        lock_recover(&self.display_state).show_sensor_overview();
    }

    fn execute_teardown(&self, action: LocalAction) {
        eprintln!("[ButtonMonitor] {:?} confirmed via front panel", action);
        let reason = format!("Local {:?} via front-panel menu", action);
        let by = crate::libs::system_control::LOCAL_BUTTON_REQUESTER.to_string();
        let result = match action {
            LocalAction::Reboot => crate::libs::system_control::execute_teardown(
                "reboot",
                "REBOOT",
                reason,
                by,
                &self.storage_handle,
            ),
            // Sleep mode is standby, as from the portal: a halted CM4 on this
            // board cannot be woken by PoE or the front-panel buttons.
            LocalAction::Sleep => crate::libs::mqtt::monitor::MqttMonitor::execute_standby(
                reason,
                by,
                &self.storage_handle,
                &self.stm_bridge,
            ),
            // The same power-off as `fiberctl shutdown`; the system-shutdown
            // hook darkens the board once the services have stopped.
            LocalAction::Shutdown => crate::libs::system_control::execute_teardown(
                "poweroff",
                "POWER_OFF_LOCAL",
                reason,
                by,
                &self.storage_handle,
            ),
        };
        if let Err(e) = result {
            eprintln!("[ButtonMonitor] Failed to execute {:?}: {}", action, e);
        }
        // Both paths blank the panel themselves — nothing more to render here.
    }
}

/// Button monitor thread for controlling display navigation
pub struct ButtonMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl ButtonMonitor {
    /// Create and spawn background button monitoring thread
    ///
    /// The thread continuously monitors button input and drives the display.
    /// If a pairing_handle is provided, UP held for 2 seconds opens the local
    /// action menu (Pairing mode / Power / Back; Power holds Reboot / Sleep / Shutdown / Back). `storage_handle` audits
    /// the latter two, tagged as locally-triggered; pass `None` before the
    /// storage thread exists (Reboot/Sleep/Shutdown will simply go unaudited until
    /// the monitor is recreated with it, matching how `pairing_handle` works).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        display_state: SharedDisplayStateHandle,
        pairing_handle: Option<PairingHandle>,
        buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
        sensor_state: crate::libs::sensors::SharedSensorStateHandle,
        provisioning_session: SharedProvisioningSession,
        mac_address: String,
        hostname: String,
        storage_handle: Option<StorageHandle>,
        stm_bridge: Option<SharedStmBridge>,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();

        let thread_handle = thread::spawn(move || {
            // Contain panics: an unhandled one here would silently and
            // permanently deafen the front-panel buttons. See
            // `display::supervise` for why restart alone isn't sufficient.
            crate::libs::display::supervise::supervise("buttons", &shutdown_flag_clone, || {
                Self::button_loop(
                    shutdown_flag_clone.clone(),
                    Applier {
                        display_state: display_state.clone(),
                        pairing_handle: pairing_handle.clone(),
                        buzzer_priority: buzzer_priority.clone(),
                        sensor_state: sensor_state.clone(),
                        provisioning_session: provisioning_session.clone(),
                        mac_address: mac_address.clone(),
                        hostname: hostname.clone(),
                        storage_handle: storage_handle.clone(),
                        stm_bridge: stm_bridge.clone(),
                    },
                )
            });
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
        })
    }

    /// Background button monitoring loop: poll, decide, apply.
    ///
    /// Returns `Err` if the buttons could not be claimed, which
    /// [`supervise`](super::supervise::supervise) retries with a backoff. A bare
    /// `return` here used to read as a clean shutdown, so a single failed init —
    /// a transient GPIO claim, or losing the race with the previous monitor's
    /// pins during the startup hand-off — left the only local UI permanently deaf
    /// with one line in the journal.
    fn button_loop(shutdown_flag: Arc<AtomicBool>, applier: Applier) -> Result<(), String> {
        let mut buttons = match Buttons::new() {
            Ok(btn) => {
                eprintln!("[ButtonMonitor] Buttons initialized successfully");
                btn
            }
            Err(e) => return Err(format!("failed to initialize buttons: {}", e)),
        };

        let poll_interval = Duration::from_millis(50);
        eprintln!("[ButtonMonitor] Started button monitoring with 50ms poll interval");

        let mut fsm = ButtonFsm::new();

        /// How long a button may read pressed before it is reported as a fault.
        /// Longer than any legitimate gesture (the longest is a 2s hold) with room
        /// to spare, so a deliberate lean on the panel is not flagged.
        const STUCK_WARN_AFTER: Duration = Duration::from_secs(30);
        // Per button, indexed Up/Down/Enter.
        let mut held_since: [Option<Instant>; 3] = [None; 3];
        let mut stuck_warned = [false; 3];

        loop {
            if shutdown_flag.load(Ordering::Relaxed) {
                eprintln!("[ButtonMonitor] Shutdown signal received, exiting button thread");
                break;
            }

            // One clock reading for the whole tick: the debouncer, the hold
            // deadlines and the progress bar then all agree about "now".
            let now = Instant::now();
            let events = buttons.poll(now);
            // Read the levels after polling, so they reflect the same debounced
            // samples the events were derived from.
            let levels = Levels {
                up: buttons.is_pressed(Button::Up),
                down: buttons.is_pressed(Button::Down),
                enter: buttons.is_pressed(Button::Enter),
            };

            // Sample the ambient facts the gesture logic needs. Read once per
            // poll rather than per event: two events landing in the same 50ms
            // window then see one consistent view of the world.
            let screen: Screen = lock_recover(&applier.display_state).current_screen.clone();
            let any_beeping = applier
                .buzzer_priority
                .as_ref()
                .is_some_and(|bp| bp.is_sensor_beeping() || bp.is_battery_beeping());
            // A missing session counts as expired: if the QR screen is up with
            // nothing behind it, there is nothing to keep alive.
            let provisioning_expired = applier
                .provisioning_session
                .read()
                .ok()
                .map(|g| g.as_ref().map(|s| s.is_expired()).unwrap_or(true))
                .unwrap_or(false);

            // One line per edge, with the levels and the state/screen it landed on.
            // The pre-refactor loop logged every press; keeping that is what makes a
            // field report ("the buttons did nothing") diagnosable at all, and the
            // levels distinguish a button that was never seen from one the machine
            // believes is still held.
            for event in &events {
                eprintln!(
                    "[ButtonMonitor] {:?} levels=up:{} down:{} enter:{} state={:?} screen={:?}",
                    event,
                    levels.up as u8,
                    levels.down as u8,
                    levels.enter as u8,
                    fsm.state(),
                    screen,
                );
            }

            // A line that reads pressed for this long is a fault, not a gesture: a
            // stuck switch, a broken panel harness, or a line left floating. It is
            // also invisible from the outside — no edge is ever emitted again, so
            // the button simply stops working. Say so, once per episode.
            for (idx, button) in [Button::Up, Button::Down, Button::Enter]
                .into_iter()
                .enumerate()
            {
                if levels.get(button) {
                    let since = *held_since[idx].get_or_insert(now);
                    if !stuck_warned[idx]
                        && now.saturating_duration_since(since) >= STUCK_WARN_AFTER
                    {
                        stuck_warned[idx] = true;
                        eprintln!(
                            "[ButtonMonitor] WARN: {:?} has read pressed for {:?} — stuck switch \
                             or floating line? No further edges will be reported for it.",
                            button, STUCK_WARN_AFTER,
                        );
                    }
                } else {
                    held_since[idx] = None;
                    stuck_warned[idx] = false;
                }
            }

            // The factory test takes the presses while it runs; the menu and
            // the hold gestures stay out of it.
            if crate::libs::factory_test::is_active() {
                for event in &events {
                    if let ButtonEvent::Press(button) = event {
                        crate::libs::factory_test::on_press(match button {
                            Button::Up => crate::libs::factory_test::Btn::Up,
                            Button::Enter => crate::libs::factory_test::Btn::Middle,
                            Button::Down => crate::libs::factory_test::Btn::Down,
                        });
                    }
                }
                thread::sleep(poll_interval);
                continue;
            }

            let outcome = fsm.tick(Inputs {
                now,
                events: &events,
                screen: &screen,
                levels,
                in_standby: crate::libs::power::standby::is_standby(),
                any_beeping,
                pairing_available: applier.pairing_handle.is_some(),
                provisioning_expired,
            });

            for effect in outcome.effects {
                applier.apply(effect);
            }

            // Publish the current hold progress (0..=127) so the display monitor
            // can render a 1-px progress bar under the header divider.
            if let Some(pixels) = outcome.hold_bar_pixels {
                let mut ds = lock_recover(&applier.display_state);
                if ds.hold_bar_pixels != pixels {
                    ds.hold_bar_pixels = pixels;
                }
            }

            thread::sleep(poll_interval);
        }

        eprintln!("[ButtonMonitor] Button monitor thread exited cleanly");
        Ok(())
    }
}

impl Drop for ButtonMonitor {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Wait for the thread to finish, then join it so its `Buttons` — and with
        // it the claim on GPIO 23/24/25 — is definitely released. main.rs drops
        // this monitor and immediately builds another one with the pairing and
        // storage handles, and rppal tracks pin ownership process-wide, so an
        // un-reaped thread means the replacement cannot claim the pins.
        //
        // Bounded rather than a bare join: a wedged thread should delay startup
        // and say so, not hang the process. `button_loop` polls the flag every
        // 50ms and supervise's backoff is interruptible, so this normally returns
        // in well under a tick.
        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(2);
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                eprintln!(
                    "[ButtonMonitor] Thread still running after {:?}; \
                     leaving it detached (the GPIO pins may not be free yet)",
                    timeout
                );
            }
        }
    }
}
