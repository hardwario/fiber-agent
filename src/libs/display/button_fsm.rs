//! Pure decision half of the front-panel button state machine.
//!
//! [`ButtonFsm::tick`] takes the button events for one poll, the current screen
//! and a handful of ambient facts, and returns the [`Effect`]s to apply. It owns
//! no locks, no GPIO and no clock: `now` is passed in. That is what makes the
//! gesture logic testable — see the tests at the bottom of this file, and
//! [`super::buttons`] for the thread that supplies the inputs and applies the
//! effects.
//!
//! Splitting it this way is deliberate. The logic previously lived inline in a
//! ~670-line loop body that constructed real GPIO on entry and read
//! `Instant::now()` throughout, so none of it could be exercised without the
//! hardware, and a fix could only ever be argued for rather than demonstrated.

use std::time::{Duration, Instant};

use super::{LocalAction, Screen};
use crate::drivers::buttons::{Button, ButtonEvent};

/// How long a button must be held for its action to fire.
pub const COUNTDOWN_DURATION: Duration = Duration::from_millis(2000);

/// Idle timeout for the navigation screens that have one. Safety-relevant for
/// Menu/Confirm: an abandoned confirm dialog must fall back to the overview
/// rather than sit there waiting to be answered "Yes".
pub const SELECTION_TIMEOUT: Duration = Duration::from_secs(15);

/// Two ENTER clicks closer together than this count as a double-click.
pub const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(400);

/// One flag per button.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ButtonFlags {
    pub up: bool,
    pub down: bool,
    pub enter: bool,
}

impl ButtonFlags {
    pub fn get(self, button: Button) -> bool {
        match button {
            Button::Up => self.up,
            Button::Down => self.down,
            Button::Enter => self.enter,
        }
    }

    pub fn set(&mut self, button: Button, value: bool) {
        match button {
            Button::Up => self.up = value,
            Button::Down => self.down = value,
            Button::Enter => self.enter = value,
        }
    }
}

/// Which buttons are held down right now, debounced. `true` means pressed.
///
/// This is what makes a hold trustworthy: the gesture is defined by the button
/// still being down, not by a tally of edges that a neighbouring button or a
/// dropped sample could interfere with.
pub type Levels = ButtonFlags;

/// Where the front-panel navigation currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonMonitorState {
    /// Normal operation - waiting for button input
    Idle,
    /// ENTER button pressed, counting down for 2 seconds
    CountdownActive,
    /// QR code screen is displayed
    ShowingQr,
    /// DOWN button being held, counting down for 2 seconds
    DownHoldActive,
    /// System info screen is displayed
    ShowingSystem,
    /// UP button being held, counting down for 2 seconds to open the local action menu
    UpHoldActive,
    /// Pairing screen is displayed
    ShowingPairing,
    /// Sensor selection mode - cursor navigation active
    SelectionMode,
    /// Viewing sensor detail screen
    ShowingDetail,
    /// Local action menu (Pairing code / Reboot / Shutdown) is displayed.
    MenuActive,
    /// Yes/No confirmation for a pending local Reboot/Shutdown is displayed.
    ConfirmActive,
}

/// Everything the machine needs to decide, for one poll.
pub struct Inputs<'a> {
    /// The poll's timestamp. Every deadline in the machine is measured against
    /// this rather than against a fresh `Instant::now()`, so a tick is a pure
    /// function of its inputs.
    pub now: Instant,
    pub events: &'a [ButtonEvent],
    /// What the panel is showing. Read-only here: the machine asks the screen
    /// what it is, and never assumes.
    pub screen: &'a Screen,
    /// Debounced level of each button, as of this poll.
    pub levels: Levels,
    pub in_standby: bool,
    pub sensor_beeping: bool,
    /// A BLE client is connected, which blocks the MQTT pairing flow.
    pub ble_active: bool,
    /// Whether a pairing handle exists at all (MQTT may be disabled).
    pub pairing_available: bool,
    /// The BLE provisioning session behind the QR screen has gone idle.
    pub provisioning_expired: bool,
}

/// A side effect for the caller to apply, in the order emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reset the backlight idle timeout.
    MarkActivity,
    /// Mute the sensor buzzer for 30 minutes.
    SilenceSensorBeep,
    ShowSensorOverview,
    ShowSystemInfo,
    ShowMenu,
    MenuUp,
    MenuDown,
    ShowConfirm(LocalAction),
    ConfirmToggle,
    /// Mint a provisioning session, start BLE advertising, show the QR screen.
    OpenQrSession,
    /// User dismissed the QR screen: drop the session, stop advertising, return
    /// to the overview.
    CloseQrSession,
    /// Same teardown, but triggered by the session going idle rather than by the
    /// user, so it is logged differently.
    ExpireQrSession,
    StartPairing,
    CancelPairing,
    NextPage,
    SelectionUp,
    SelectionDown,
    EnterSelectionMode,
    EnterDetailView,
    ExitDetailView,
    LoraDetailPrev,
    LoraDetailNext,
    /// Reboot or power off, audited as locally triggered.
    ExecuteTeardown(LocalAction),
    /// Ask PowerMonitor to leave standby. This thread owns no hardware.
    RequestStandbyWake,
    /// ENTER was released believing the menu was up, but the screen says
    /// otherwise. Nothing is actuated; worth a log line because it means two
    /// threads disagreed about the panel.
    WarnStaleMenuSelection,
}

/// The result of one tick.
pub struct Outcome {
    pub effects: Vec<Effect>,
    /// Width of the hold-progress bar (0..=127), or `None` to leave it alone —
    /// which is what a standby tick does, since the panel is dark anyway.
    pub hold_bar_pixels: Option<u8>,
}

/// Map elapsed/total to a 1-pixel-tall progress bar width in the 0..=127 range.
/// Saturates at 127 once the hold completes.
fn progress_pixels(elapsed: Duration, total: Duration) -> u8 {
    if total.is_zero() {
        return 0;
    }
    let ratio = (elapsed.as_millis() as f64 / total.as_millis() as f64).clamp(0.0, 1.0);
    (ratio * 127.0).round() as u8
}

pub struct ButtonFsm {
    state: ButtonMonitorState,
    countdown_start: Instant,
    down_hold_start: Instant,
    up_hold_start: Instant,
    /// Last activity in selection/detail/menu/confirm mode.
    selection_activity: Instant,
    /// Last completed ENTER click, for double-click detection.
    last_enter_click: Option<Instant>,
    /// When a button was first pressed while in standby, for the wake hold.
    standby_hold_start: Option<Instant>,
    /// Buttons whose press was consumed by the beep silencer. Their release has
    /// to be consumed too, or half the gesture would still act.
    swallowed: ButtonFlags,
}

/// The state implied by what is on screen.
///
/// Every navigation state except the holds is really a property of the screen, so
/// this is the authority when the two disagree — see the reconcile step in
/// [`ButtonFsm::tick`].
fn state_for(screen: &Screen) -> ButtonMonitorState {
    match screen {
        Screen::Menu { .. } => ButtonMonitorState::MenuActive,
        Screen::Confirm { .. } => ButtonMonitorState::ConfirmActive,
        Screen::SystemInfo { .. } => ButtonMonitorState::ShowingSystem,
        Screen::QrCodeConfig => ButtonMonitorState::ShowingQr,
        Screen::Pairing { .. } => ButtonMonitorState::ShowingPairing,
        Screen::SensorDetail { .. } | Screen::LoRaWANSensorDetail { .. } => {
            ButtonMonitorState::ShowingDetail
        }
        Screen::SensorOverview {
            selected_sensor: Some(_),
            ..
        } => ButtonMonitorState::SelectionMode,
        Screen::SensorOverview {
            selected_sensor: None,
            ..
        } => ButtonMonitorState::Idle,
        // The BLE notification screens carry no navigation of their own. The
        // machine is idle behind them, and a hold may start — a passing client
        // must not make the panel deaf.
        Screen::BleConnected { .. }
        | Screen::BleProvisioning { .. }
        | Screen::BleWifiOk { .. }
        | Screen::BleWifiFail { .. } => ButtonMonitorState::Idle,
    }
}

impl ButtonFsm {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            state: ButtonMonitorState::Idle,
            countdown_start: now,
            down_hold_start: now,
            up_hold_start: now,
            selection_activity: now,
            last_enter_click: None,
            standby_hold_start: None,
            swallowed: ButtonFlags::default(),
        }
    }

    /// A hold or countdown is in flight, so the screen has not caught up with the
    /// machine yet and must not be reconciled against.
    fn holding(&self) -> bool {
        matches!(
            self.state,
            ButtonMonitorState::CountdownActive
                | ButtonMonitorState::UpHoldActive
                | ButtonMonitorState::DownHoldActive
        )
    }

    /// Current state, for logging.
    pub fn state(&self) -> ButtonMonitorState {
        self.state
    }

    fn since(&self, start: Instant, now: Instant) -> Duration {
        let _ = self;
        now.saturating_duration_since(start)
    }

    fn is_double_click(&self, now: Instant) -> bool {
        self.last_enter_click
            .map(|last| now.saturating_duration_since(last) < DOUBLE_CLICK_THRESHOLD)
            .unwrap_or(false)
    }

    pub fn tick(&mut self, input: Inputs<'_>) -> Outcome {
        let mut effects = Vec::new();
        let now = input.now;

        // A device in standby is dark and is meant to look off. Buttons must not
        // light the panel, walk the menus, or start BLE advertising and make it
        // pairable.
        //
        // The one exception is a deliberate long hold, which wakes the device.
        // That is the escape hatch: PoE detection can fail (a wedged STM, a
        // supply the ADC cannot see), and a monitoring device must never be
        // strandable in a state only a battery-pull can leave. A hold rather than
        // a press so a knock or something resting on the unit cannot do it.
        //
        // The wake is a request, not an action: this machine owns no hardware,
        // and every standby hardware transition belongs to PowerMonitor.
        if input.in_standby {
            for event in input.events {
                match event {
                    ButtonEvent::Press(_) => {
                        if self.standby_hold_start.is_none() {
                            self.standby_hold_start = Some(now);
                        }
                    }
                    // Any release abandons the hold — this must be a deliberate,
                    // sustained press.
                    ButtonEvent::Release(_) => self.standby_hold_start = None,
                }
            }
            if let Some(started) = self.standby_hold_start {
                if self.since(started, now) >= COUNTDOWN_DURATION {
                    effects.push(Effect::RequestStandbyWake);
                    self.standby_hold_start = None;
                }
            }
            // Menu state must not carry across a standby, or the panel would come
            // back mid-navigation.
            self.state = ButtonMonitorState::Idle;
            return Outcome {
                effects,
                hold_bar_pixels: None,
            };
        }
        self.standby_hold_start = None;

        // Reconcile against the screen before acting on anything.
        //
        // `state` is this machine's private idea of where the navigation is, but
        // the screen is shared: standby resets the state while leaving the panel
        // as it was, a supervisor restart starts over at Idle, and other threads
        // can replace the screen outright. When the two disagree the screen wins —
        // otherwise the panel goes deaf (a menu on screen whose buttons do
        // nothing) or, worse, a keypress actuates a menu that is no longer there.
        //
        // Holds are exempt: they run while the screen is still the overview, so
        // there is nothing to reconcile against yet. So is ShowingPairing, whose
        // screen is set asynchronously by the pairing thread a moment later.
        if !self.holding() && self.state != ButtonMonitorState::ShowingPairing {
            let implied = state_for(input.screen);
            if implied != self.state {
                self.state = implied;
                // Give the adopted screen a full timeout rather than a deadline
                // inherited from whatever the machine was doing before.
                self.selection_activity = now;
            }
        }

        // Whether the QR screen was already up before this tick's events, which is
        // what the session-expiry check below is allowed to act on.
        let was_showing_qr = self.state == ButtonMonitorState::ShowingQr;

        for event in input.events {
            match event {
                // Any button PRESS counts as user activity: wake the backlight
                // and reset the idle timeout. Done before the beep silence below
                // so the silencing press still keeps the screen lit.
                ButtonEvent::Press(button) => {
                    effects.push(Effect::MarkActivity);

                    // Any button PRESS silences the sensor beep, and that is all
                    // it does: the gesture is spent. Remember it so the matching
                    // release is spent too — otherwise a tap meaning "quiet
                    // please" would still actuate whatever the release does.
                    if input.sensor_beeping {
                        effects.push(Effect::SilenceSensorBeep);
                        self.swallowed.set(*button, true);
                        continue;
                    }
                }
                ButtonEvent::Release(button) => {
                    if self.swallowed.get(*button) {
                        self.swallowed.set(*button, false);
                        continue;
                    }
                }
            }

            match event {
                ButtonEvent::Press(Button::Up) => {
                    if self.cancel_pairing_screen(&input, &mut effects) {
                        continue;
                    }
                    match self.state {
                        ButtonMonitorState::Idle => {
                            self.state = ButtonMonitorState::UpHoldActive;
                            self.up_hold_start = now;
                        }
                        ButtonMonitorState::ShowingSystem => effects.push(Effect::NextPage),
                        ButtonMonitorState::SelectionMode => {
                            effects.push(Effect::SelectionUp);
                            self.selection_activity = now;
                        }
                        // A hold in flight owns the gesture: neither this button
                        // nor any other may end it. Brushing a neighbouring panel
                        // button used to abort the hold silently, which is one of
                        // the reported defects — and a real contact lasts far
                        // longer than any debounce window, so filtering the edge
                        // was never going to help.
                        ButtonMonitorState::UpHoldActive
                        | ButtonMonitorState::CountdownActive
                        | ButtonMonitorState::DownHoldActive => {}
                        ButtonMonitorState::ShowingDetail => effects.push(Effect::LoraDetailPrev),
                        ButtonMonitorState::MenuActive => {
                            effects.push(Effect::MenuUp);
                            self.selection_activity = now;
                        }
                        ButtonMonitorState::ConfirmActive => {
                            effects.push(Effect::ConfirmToggle);
                            self.selection_activity = now;
                        }
                        // ShowingQr / ShowingPairing - ignore UP.
                        _ => {}
                    }
                }
                ButtonEvent::Release(Button::Up) => {
                    if self.state == ButtonMonitorState::UpHoldActive
                        && self.since(self.up_hold_start, now) < COUNTDOWN_DURATION
                    {
                        // Released early - cancel the hold and page instead.
                        if input.screen.is_navigable() {
                            effects.push(Effect::NextPage);
                        }
                        self.state = ButtonMonitorState::Idle;
                    }
                    // At or past the countdown the hold has already fired.
                }
                ButtonEvent::Press(Button::Down) => {
                    if self.cancel_pairing_screen(&input, &mut effects) {
                        continue;
                    }
                    match self.state {
                        ButtonMonitorState::Idle => {
                            self.state = ButtonMonitorState::DownHoldActive;
                            self.down_hold_start = now;
                        }
                        ButtonMonitorState::ShowingSystem => effects.push(Effect::NextPage),
                        // A hold in flight owns the gesture — see the UP arm.
                        ButtonMonitorState::DownHoldActive
                        | ButtonMonitorState::CountdownActive
                        | ButtonMonitorState::UpHoldActive => {}
                        ButtonMonitorState::SelectionMode => {
                            effects.push(Effect::SelectionDown);
                            self.selection_activity = now;
                        }
                        ButtonMonitorState::ShowingDetail => effects.push(Effect::LoraDetailNext),
                        ButtonMonitorState::MenuActive => {
                            effects.push(Effect::MenuDown);
                            self.selection_activity = now;
                        }
                        ButtonMonitorState::ConfirmActive => {
                            effects.push(Effect::ConfirmToggle);
                            self.selection_activity = now;
                        }
                        // ShowingQr / ShowingPairing - ignore DOWN.
                        _ => {}
                    }
                }
                ButtonEvent::Release(Button::Down) => {
                    if self.state == ButtonMonitorState::DownHoldActive
                        && self.since(self.down_hold_start, now) < COUNTDOWN_DURATION
                    {
                        effects.push(Effect::NextPage);
                        self.state = ButtonMonitorState::Idle;
                    }
                }
                ButtonEvent::Press(Button::Enter) => {
                    if self.cancel_pairing_screen(&input, &mut effects) {
                        continue;
                    }
                    match self.state {
                        ButtonMonitorState::Idle => {
                            self.state = ButtonMonitorState::CountdownActive;
                            self.countdown_start = now;
                        }
                        // ENTER pressed again during the countdown - restart it.
                        ButtonMonitorState::CountdownActive => self.countdown_start = now,
                        ButtonMonitorState::ShowingQr => {
                            effects.push(Effect::CloseQrSession);
                            self.state = ButtonMonitorState::Idle;
                        }
                        ButtonMonitorState::ShowingSystem => {
                            effects.push(Effect::ShowSensorOverview);
                            self.state = ButtonMonitorState::Idle;
                        }
                        // A hold in flight owns the gesture — see the UP arm.
                        ButtonMonitorState::DownHoldActive | ButtonMonitorState::UpHoldActive => {}
                        // Already handled above.
                        ButtonMonitorState::ShowingPairing => {}
                        // Actions happen on release, for double-click detection.
                        ButtonMonitorState::SelectionMode
                        | ButtonMonitorState::ShowingDetail
                        | ButtonMonitorState::MenuActive => self.selection_activity = now,
                        ButtonMonitorState::ConfirmActive => {
                            // Acts immediately on press, unlike Menu/SelectionMode:
                            // "No" is the pre-selected default, so there is no
                            // double-click ambiguity to resolve on release.
                            let (action, yes_selected) = match input.screen {
                                Screen::Confirm {
                                    action,
                                    yes_selected,
                                } => (*action, *yes_selected),
                                // Unreachable: ConfirmActive only holds while the
                                // screen is Screen::Confirm.
                                _ => (LocalAction::Reboot, false),
                            };
                            if yes_selected {
                                effects.push(Effect::ExecuteTeardown(action));
                            } else {
                                effects.push(Effect::ShowSensorOverview);
                            }
                            self.state = ButtonMonitorState::Idle;
                        }
                    }
                }
                ButtonEvent::Release(Button::Enter) => match self.state {
                    ButtonMonitorState::CountdownActive => {
                        if self.since(self.countdown_start, now) < COUNTDOWN_DURATION {
                            if input.screen.is_sensor_overview() {
                                if self.is_double_click(now) {
                                    effects.push(Effect::EnterSelectionMode);
                                    self.state = ButtonMonitorState::SelectionMode;
                                    self.selection_activity = now;
                                    self.last_enter_click = None;
                                } else {
                                    // First click - wait for a second one.
                                    self.last_enter_click = Some(now);
                                    self.state = ButtonMonitorState::Idle;
                                }
                            } else {
                                self.state = ButtonMonitorState::Idle;
                            }
                        }
                    }
                    ButtonMonitorState::SelectionMode => {
                        if self.is_double_click(now) {
                            effects.push(Effect::ShowSensorOverview);
                            self.state = ButtonMonitorState::Idle;
                            self.last_enter_click = None;
                        } else {
                            effects.push(Effect::EnterDetailView);
                            self.state = ButtonMonitorState::ShowingDetail;
                            self.selection_activity = now;
                            self.last_enter_click = Some(now);
                        }
                    }
                    ButtonMonitorState::ShowingDetail => {
                        if self.is_double_click(now) {
                            effects.push(Effect::ShowSensorOverview);
                            self.state = ButtonMonitorState::Idle;
                            self.last_enter_click = None;
                        } else {
                            effects.push(Effect::ExitDetailView);
                            self.state = ButtonMonitorState::SelectionMode;
                            self.selection_activity = now;
                            self.last_enter_click = Some(now);
                        }
                    }
                    ButtonMonitorState::MenuActive => {
                        // Double-click cancels the menu, same idiom as
                        // SelectionMode's exit; single-click acts on the
                        // highlighted item.
                        if self.is_double_click(now) {
                            effects.push(Effect::ShowSensorOverview);
                            self.state = ButtonMonitorState::Idle;
                            self.last_enter_click = None;
                        } else {
                            // No default selection: this used to fall back to item
                            // 0, which is "Pairing code", so an ENTER released
                            // against a screen that was no longer the menu could
                            // start BLE pairing on its own.
                            let Screen::Menu { selected } = input.screen else {
                                effects.push(Effect::WarnStaleMenuSelection);
                                self.state = ButtonMonitorState::Idle;
                                self.last_enter_click = None;
                                continue;
                            };
                            match *selected {
                                0 => {
                                    // "Pairing code" - identical gating to the old
                                    // UP-hold-triggers-pairing path.
                                    if input.ble_active || !input.pairing_available {
                                        effects.push(Effect::ShowSensorOverview);
                                        self.state = ButtonMonitorState::Idle;
                                    } else {
                                        effects.push(Effect::StartPairing);
                                        self.state = ButtonMonitorState::ShowingPairing;
                                    }
                                }
                                1 => {
                                    effects.push(Effect::ShowConfirm(LocalAction::Reboot));
                                    self.state = ButtonMonitorState::ConfirmActive;
                                }
                                _ => {
                                    effects.push(Effect::ShowConfirm(LocalAction::Shutdown));
                                    self.state = ButtonMonitorState::ConfirmActive;
                                }
                            }
                            self.selection_activity = now;
                            self.last_enter_click = Some(now);
                        }
                    }
                    _ => {}
                },
            }
        }

        // A hold lives and dies by its button's level, not by edge bookkeeping. The
        // Release arms above have already handled the ordinary case and left the
        // state Idle; this catches a release whose edge never arrived, which would
        // otherwise strand the machine mid-hold until something else reset it.
        match self.state {
            ButtonMonitorState::UpHoldActive if !input.levels.get(Button::Up) => {
                self.state = ButtonMonitorState::Idle
            }
            ButtonMonitorState::DownHoldActive if !input.levels.get(Button::Down) => {
                self.state = ButtonMonitorState::Idle
            }
            ButtonMonitorState::CountdownActive if !input.levels.get(Button::Enter) => {
                self.state = ButtonMonitorState::Idle
            }
            _ => {}
        }

        // Countdown completion for the ENTER hold. Requires the button to still be
        // down: a hold must never fire for a button the operator has let go of.
        if self.state == ButtonMonitorState::CountdownActive
            && input.levels.get(Button::Enter)
            && self.since(self.countdown_start, now) >= COUNTDOWN_DURATION
        {
            effects.push(Effect::OpenQrSession);
            self.state = ButtonMonitorState::ShowingQr;
        }

        // Expire the provisioning session once it has sat idle (no BLE GATT
        // activity from the phone), dropping the user back to the overview.
        //
        // Only for a QR screen that was already up when this tick began.
        // `provisioning_expired` is sampled by the caller before the tick runs, so
        // on the tick that emits `OpenQrSession` it still describes the empty slot
        // from *before* the session was minted — and an absent session counts as
        // expired. Acting on that would tear the screen down in the same tick that
        // opened it, which is exactly what it did on hardware.
        if was_showing_qr
            && self.state == ButtonMonitorState::ShowingQr
            && input.provisioning_expired
        {
            effects.push(Effect::ExpireQrSession);
            self.state = ButtonMonitorState::Idle;
        }

        // Countdown completion for the DOWN hold.
        if self.state == ButtonMonitorState::DownHoldActive
            && input.levels.get(Button::Down)
            && self.since(self.down_hold_start, now) >= COUNTDOWN_DURATION
        {
            effects.push(Effect::ShowSystemInfo);
            self.state = ButtonMonitorState::ShowingSystem;
        }

        // Countdown completion for the UP hold - opens the local action menu
        // unconditionally. The BLE-active gate runs when "Pairing code" is
        // actually selected from the menu, not here.
        if self.state == ButtonMonitorState::UpHoldActive
            && input.levels.get(Button::Up)
            && self.since(self.up_hold_start, now) >= COUNTDOWN_DURATION
        {
            effects.push(Effect::ShowMenu);
            self.state = ButtonMonitorState::MenuActive;
            // Reset here, not just on the menu's own inputs: a stale timestamp
            // left over from an earlier SelectionMode session would otherwise
            // make the freshly-opened menu look like it had already timed out.
            self.selection_activity = now;
        }

        // Inactivity timeout for the selection/detail/menu/confirm screens.
        if matches!(
            self.state,
            ButtonMonitorState::SelectionMode
                | ButtonMonitorState::ShowingDetail
                | ButtonMonitorState::MenuActive
                | ButtonMonitorState::ConfirmActive
        ) && self.since(self.selection_activity, now) >= SELECTION_TIMEOUT
        {
            effects.push(Effect::ShowSensorOverview);
            self.state = ButtonMonitorState::Idle;
        }

        let hold_bar_pixels = match self.state {
            ButtonMonitorState::CountdownActive => {
                progress_pixels(self.since(self.countdown_start, now), COUNTDOWN_DURATION)
            }
            ButtonMonitorState::DownHoldActive => {
                progress_pixels(self.since(self.down_hold_start, now), COUNTDOWN_DURATION)
            }
            ButtonMonitorState::UpHoldActive => {
                progress_pixels(self.since(self.up_hold_start, now), COUNTDOWN_DURATION)
            }
            _ => 0,
        };

        Outcome {
            effects,
            hold_bar_pixels: Some(hold_bar_pixels),
        }
    }

    /// While the pairing screen is up, any button cancels it. Returns whether the
    /// event was consumed by that.
    fn cancel_pairing_screen(&mut self, input: &Inputs<'_>, effects: &mut Vec<Effect>) -> bool {
        if self.state != ButtonMonitorState::ShowingPairing {
            return false;
        }
        if input.pairing_available {
            effects.push(Effect::CancelPairing);
        }
        effects.push(Effect::ShowSensorOverview);
        self.state = ButtonMonitorState::Idle;
        true
    }
}

impl Default for ButtonFsm {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ButtonFsm, Effect, Inputs, Levels, Outcome, COUNTDOWN_DURATION, SELECTION_TIMEOUT,
    };
    use crate::drivers::buttons::{Button, ButtonEvent};
    use crate::libs::display::{LocalAction, Screen};
    use std::time::{Duration, Instant};

    fn overview() -> Screen {
        Screen::SensorOverview {
            page: 0,
            selected_sensor: None,
        }
    }

    /// Drives the machine the way the button thread does: button levels persist
    /// between polls, and the screen changes as the applier would change it.
    ///
    /// Modelling the levels is the point — a hold is governed by the button still
    /// being down, so a test that only replayed edges could not tell a real hold
    /// from a stranded one.
    struct Harness {
        fsm: ButtonFsm,
        levels: Levels,
        screen: Screen,
        in_standby: bool,
        sensor_beeping: bool,
        ble_active: bool,
        pairing_available: bool,
        provisioning_expired: bool,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                fsm: ButtonFsm::new(),
                levels: Levels::default(),
                screen: overview(),
                in_standby: false,
                sensor_beeping: false,
                ble_active: false,
                pairing_available: true,
                provisioning_expired: false,
            }
        }

        fn tick(&mut self, now: Instant, events: &[ButtonEvent]) -> Outcome {
            let outcome = self.fsm.tick(Inputs {
                now,
                events,
                screen: &self.screen,
                levels: self.levels,
                in_standby: self.in_standby,
                sensor_beeping: self.sensor_beeping,
                ble_active: self.ble_active,
                pairing_available: self.pairing_available,
                provisioning_expired: self.provisioning_expired,
            });
            self.mirror(&outcome.effects);
            outcome
        }

        /// The half of `Applier` that matters here: what each effect does to the
        /// screen. Keeps the state/screen relationship in tests as it is on the
        /// device, so the reconciliation logic is exercised for real.
        fn mirror(&mut self, effects: &[Effect]) {
            for effect in effects {
                match effect {
                    Effect::ShowSensorOverview
                    | Effect::CloseQrSession
                    | Effect::ExpireQrSession => self.screen = overview(),
                    Effect::ShowSystemInfo => self.screen = Screen::SystemInfo { page: 0 },
                    Effect::ShowMenu => self.screen = Screen::Menu { selected: 0 },
                    Effect::MenuDown => {
                        if let Screen::Menu { selected } = self.screen {
                            self.screen = Screen::Menu {
                                selected: (selected + 1) % 3,
                            };
                        }
                    }
                    Effect::MenuUp => {
                        if let Screen::Menu { selected } = self.screen {
                            self.screen = Screen::Menu {
                                selected: (selected + 2) % 3,
                            };
                        }
                    }
                    Effect::ShowConfirm(action) => {
                        self.screen = Screen::Confirm {
                            action: *action,
                            yes_selected: false,
                        }
                    }
                    Effect::ConfirmToggle => {
                        if let Screen::Confirm {
                            action,
                            yes_selected,
                        } = self.screen
                        {
                            self.screen = Screen::Confirm {
                                action,
                                yes_selected: !yes_selected,
                            };
                        }
                    }
                    Effect::OpenQrSession => self.screen = Screen::QrCodeConfig,
                    Effect::StartPairing => {
                        self.screen = Screen::Pairing {
                            code: "123456".to_string(),
                        }
                    }
                    Effect::EnterSelectionMode | Effect::ExitDetailView => {
                        self.screen = Screen::SensorOverview {
                            page: 0,
                            selected_sensor: Some(0),
                        }
                    }
                    Effect::EnterDetailView => self.screen = Screen::SensorDetail { sensor_idx: 0 },
                    _ => {}
                }
            }
        }

        fn press(&mut self, now: Instant, b: Button) -> Outcome {
            self.levels.set(b, true);
            self.tick(now, &[ButtonEvent::Press(b)])
        }

        fn release(&mut self, now: Instant, b: Button) -> Outcome {
            self.levels.set(b, false);
            self.tick(now, &[ButtonEvent::Release(b)])
        }

        /// The button comes up but its edge never arrives — a dropped release.
        fn drop_level(&mut self, b: Button) {
            self.levels.set(b, false);
        }

        fn idle(&mut self, now: Instant) -> Outcome {
            self.tick(now, &[])
        }

        /// Hold UP to completion, leaving the menu open.
        fn with_menu_open(t0: Instant) -> (Self, Instant) {
            let mut h = Self::new();
            h.press(t0, Button::Up);
            let opened = t0 + COUNTDOWN_DURATION;
            let out = h.idle(opened);
            assert_eq!(out.effects, vec![Effect::ShowMenu]);
            h.release(opened, Button::Up);
            (h, opened)
        }
    }

    // ---- behaviour that must not regress -------------------------------------

    #[test]
    fn an_up_press_alone_changes_nothing_on_screen() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        let out = h.press(t0, Button::Up);
        assert_eq!(out.effects, vec![Effect::MarkActivity]);
    }

    #[test]
    fn holding_up_for_two_seconds_opens_the_local_action_menu() {
        Harness::with_menu_open(Instant::now());
    }

    #[test]
    fn releasing_up_early_pages_the_overview_instead() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Up);

        let out = h.release(t0 + Duration::from_millis(300), Button::Up);

        assert_eq!(out.effects, vec![Effect::NextPage]);
    }

    #[test]
    fn holding_down_for_two_seconds_opens_system_info() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Down);

        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert_eq!(out.effects, vec![Effect::ShowSystemInfo]);
    }

    #[test]
    fn holding_enter_for_two_seconds_opens_the_qr_provisioning_screen() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Enter);

        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert_eq!(out.effects, vec![Effect::OpenQrSession]);
    }

    #[test]
    fn the_hold_bar_tracks_progress_and_clears_when_the_hold_ends() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Up);

        let half = h.idle(t0 + Duration::from_millis(1000));
        assert_eq!(half.hold_bar_pixels, Some(64));

        let done = h.idle(t0 + COUNTDOWN_DURATION);
        // The hold completed on this tick, so the bar is already cleared.
        assert_eq!(done.hold_bar_pixels, Some(0));
    }

    #[test]
    fn up_and_down_move_the_menu_cursor() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);

        let down = h.press(t, Button::Down);
        assert_eq!(down.effects, vec![Effect::MarkActivity, Effect::MenuDown]);
        h.release(t, Button::Down);

        let up = h.press(t, Button::Up);
        assert_eq!(up.effects, vec![Effect::MarkActivity, Effect::MenuUp]);
    }

    #[test]
    fn selecting_reboot_from_the_menu_asks_for_confirmation_first() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);
        // Move the cursor to "Reboot" (item 1).
        h.press(t, Button::Down);
        h.release(t, Button::Down);

        h.press(t, Button::Enter);
        let out = h.release(t + Duration::from_millis(50), Button::Enter);

        assert_eq!(
            out.effects,
            vec![Effect::ShowConfirm(LocalAction::Reboot)],
            "a destructive action must never fire straight off the menu"
        );
    }

    #[test]
    fn confirming_yes_executes_the_teardown() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);
        h.press(t, Button::Down);
        h.release(t, Button::Down);
        h.press(t, Button::Enter);
        h.release(t + Duration::from_millis(50), Button::Enter);

        // Deliberately move the cursor off the safe default onto "Yes".
        let t = t + Duration::from_millis(100);
        h.press(t, Button::Up);
        h.release(t, Button::Up);

        let out = h.press(t + Duration::from_millis(50), Button::Enter);

        assert_eq!(
            out.effects,
            vec![
                Effect::MarkActivity,
                Effect::ExecuteTeardown(LocalAction::Reboot)
            ]
        );
    }

    #[test]
    fn confirming_no_returns_to_the_overview_without_acting() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);
        h.press(t, Button::Down);
        h.release(t, Button::Down);
        h.press(t, Button::Enter);
        h.release(t + Duration::from_millis(50), Button::Enter);

        // "No" is pre-selected; press ENTER without moving the cursor.
        let out = h.press(t + Duration::from_millis(100), Button::Enter);

        assert_eq!(
            out.effects,
            vec![Effect::MarkActivity, Effect::ShowSensorOverview]
        );
    }

    #[test]
    fn an_abandoned_menu_times_out_to_the_overview() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);

        let out = h.idle(t + SELECTION_TIMEOUT);

        assert_eq!(out.effects, vec![Effect::ShowSensorOverview]);
    }

    #[test]
    fn an_abandoned_confirm_dialog_times_out_rather_than_waiting() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);
        h.press(t, Button::Down);
        h.release(t, Button::Down);
        h.press(t, Button::Enter);
        h.release(t + Duration::from_millis(50), Button::Enter);

        let out = h.idle(t + Duration::from_millis(50) + SELECTION_TIMEOUT);

        assert_eq!(out.effects, vec![Effect::ShowSensorOverview]);
    }

    #[test]
    fn a_double_click_on_the_overview_enters_selection_mode() {
        let t0 = Instant::now();
        let mut h = Harness::new();

        h.press(t0, Button::Enter);
        h.release(t0 + Duration::from_millis(80), Button::Enter);
        h.press(t0 + Duration::from_millis(160), Button::Enter);
        let out = h.release(t0 + Duration::from_millis(240), Button::Enter);

        assert_eq!(out.effects, vec![Effect::EnterSelectionMode]);
    }

    #[test]
    fn any_button_cancels_the_pairing_screen() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);
        h.press(t, Button::Enter);
        let out = h.release(t + Duration::from_millis(50), Button::Enter);
        assert_eq!(out.effects, vec![Effect::StartPairing]);

        let out = h.press(t + Duration::from_secs(3), Button::Down);

        assert_eq!(
            out.effects,
            vec![
                Effect::MarkActivity,
                Effect::CancelPairing,
                Effect::ShowSensorOverview
            ]
        );
    }

    #[test]
    fn pairing_from_the_menu_is_refused_while_a_ble_client_is_connected() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);
        h.ble_active = true;

        h.press(t, Button::Enter);
        let out = h.release(t + Duration::from_millis(50), Button::Enter);

        assert_eq!(out.effects, vec![Effect::ShowSensorOverview]);
    }

    #[test]
    fn an_expired_provisioning_session_tears_down_the_qr_screen() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Enter);
        h.idle(t0 + COUNTDOWN_DURATION);
        h.release(t0 + COUNTDOWN_DURATION, Button::Enter);

        h.provisioning_expired = true;
        let out = h.idle(t0 + Duration::from_secs(400));

        assert_eq!(out.effects, vec![Effect::ExpireQrSession]);
    }

    #[test]
    fn a_sustained_hold_in_standby_requests_a_wake() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.in_standby = true;

        let out = h.press(t0, Button::Enter);
        assert!(
            out.effects.is_empty(),
            "a press in standby must not light the panel"
        );

        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert_eq!(out.effects, vec![Effect::RequestStandbyWake]);
    }

    #[test]
    fn releasing_in_standby_abandons_the_wake_hold() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.in_standby = true;
        h.press(t0, Button::Enter);
        h.release(t0 + Duration::from_millis(500), Button::Enter);

        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert!(out.effects.is_empty(), "the hold was abandoned");
    }

    // ---- the reported defects ------------------------------------------------

    #[test]
    fn a_press_on_another_button_does_not_cancel_a_hold() {
        // The reported "hold dies before it completes": brushing a neighbouring
        // panel button used to abort the gesture silently. A real contact lasts
        // far longer than any debounce window, so no amount of filtering helps.
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Up);

        h.press(t0 + Duration::from_millis(500), Button::Down);
        h.release(t0 + Duration::from_millis(700), Button::Down);
        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert_eq!(
            out.effects,
            vec![Effect::ShowMenu],
            "UP was held throughout, so the menu must still open"
        );
    }

    #[test]
    fn pressing_another_button_during_an_enter_countdown_is_ignored() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Enter);

        h.press(t0 + Duration::from_millis(400), Button::Up);
        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert_eq!(
            out.effects,
            vec![Effect::OpenQrSession],
            "the ENTER hold owns the gesture until it ends"
        );
    }

    #[test]
    fn a_hold_cannot_complete_once_the_button_has_come_up() {
        // Belt to the level check's braces: even if the release edge is lost, the
        // hold must not fire two seconds later on its own.
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Up);

        h.drop_level(Button::Up);
        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert!(
            !out.effects.contains(&Effect::ShowMenu),
            "the button was no longer held, so the hold must not complete"
        );
    }

    #[test]
    fn a_dropped_release_edge_does_not_strand_the_machine() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Up);
        h.drop_level(Button::Up);
        h.idle(t0 + Duration::from_millis(100));

        // A fresh, complete gesture must still work afterwards.
        let t1 = t0 + Duration::from_millis(200);
        h.press(t1, Button::Up);
        let out = h.idle(t1 + COUNTDOWN_DURATION);

        assert_eq!(out.effects, vec![Effect::ShowMenu]);
    }

    #[test]
    fn the_press_that_silenced_a_beep_takes_its_release_with_it() {
        // The silencer consumed the press but not the release, so a tap that only
        // meant "quiet please" could still actuate whatever the release does.
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);
        h.sensor_beeping = true;

        let press = h.press(t, Button::Enter);
        assert_eq!(
            press.effects,
            vec![Effect::MarkActivity, Effect::SilenceSensorBeep]
        );

        let release = h.release(t + Duration::from_millis(80), Button::Enter);

        assert!(
            release.effects.is_empty(),
            "the whole gesture was spent silencing the buzzer, so it must not \
             also select a menu item"
        );
    }

    #[test]
    fn a_stale_idle_state_adopts_the_menu_that_is_on_screen() {
        // Standby forces the machine to Idle but leaves the screen alone, and a
        // supervisor restart does the same. The panel then showed a menu whose
        // UP/DOWN did nothing at all, until a reboot.
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);

        h.in_standby = true;
        h.idle(t + Duration::from_millis(50));
        h.in_standby = false;

        let out = h.press(t + Duration::from_millis(100), Button::Down);

        assert_eq!(
            out.effects,
            vec![Effect::MarkActivity, Effect::MenuDown],
            "the screen is the source of truth once no hold is in flight"
        );
    }

    #[test]
    fn an_adopted_screen_gets_a_full_inactivity_timeout() {
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);

        h.in_standby = true;
        h.idle(t + Duration::from_millis(50));
        h.in_standby = false;
        let adopted = t + Duration::from_millis(100);
        h.idle(adopted);

        let out = h.idle(adopted + SELECTION_TIMEOUT - Duration::from_millis(100));
        assert!(
            out.effects.is_empty(),
            "the adopted menu must not inherit a stale deadline"
        );

        let out = h.idle(adopted + SELECTION_TIMEOUT);
        assert_eq!(out.effects, vec![Effect::ShowSensorOverview]);
    }

    #[test]
    fn a_menu_taken_by_another_thread_cannot_be_actuated_by_a_pending_enter() {
        // The old code defaulted an unreadable menu selection to item 0, which is
        // "Pairing code" — so an ENTER released while the machine still believed
        // the menu was up could start BLE pairing against a screen that had been
        // replaced. Reconciliation is what closes this: the menu is gone, so there
        // is nothing to actuate.
        let t0 = Instant::now();
        let (mut h, t) = Harness::with_menu_open(t0);

        // Something else took the screen, exactly as a BLE notification used to.
        h.screen = Screen::BleConnected {
            addr: "AA:BB:CC:DD:EE:01".to_string(),
        };

        h.press(t, Button::Enter);
        let out = h.release(t + Duration::from_millis(50), Button::Enter);

        assert!(
            !out.effects.contains(&Effect::StartPairing)
                && !out
                    .effects
                    .contains(&Effect::ShowConfirm(LocalAction::Reboot))
                && !out
                    .effects
                    .contains(&Effect::ShowConfirm(LocalAction::Shutdown)),
            "no menu item may be actuated once the menu is off screen, got {:?}",
            out.effects
        );
    }

    #[test]
    fn a_hold_can_still_start_while_a_ble_screen_is_up() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.screen = Screen::BleConnected {
            addr: "AA:BB:CC:DD:EE:01".to_string(),
        };

        h.press(t0, Button::Up);
        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert_eq!(
            out.effects,
            vec![Effect::ShowMenu],
            "a notification screen must not make the panel deaf"
        );
    }

    #[test]
    fn a_second_hold_works_after_the_first_menu_times_out() {
        // Observed on hardware: the first UP hold opened the menu, and every hold
        // after it did nothing at all.
        let t0 = Instant::now();
        let (mut h, opened) = Harness::with_menu_open(t0);

        let timed_out = opened + SELECTION_TIMEOUT;
        let out = h.idle(timed_out);
        assert_eq!(
            out.effects,
            vec![Effect::ShowSensorOverview],
            "menu timed out"
        );

        let t1 = timed_out + Duration::from_secs(1);
        h.press(t1, Button::Up);
        let out = h.idle(t1 + COUNTDOWN_DURATION);

        assert_eq!(
            out.effects,
            vec![Effect::ShowMenu],
            "a second hold must behave exactly like the first"
        );
    }

    #[test]
    fn opening_the_qr_screen_does_not_immediately_expire_it() {
        // Observed on hardware: "session opened" / "transitioning to QR" /
        // "session idle for 5min - tearing down" all in one tick. The caller reads
        // the session slot before the tick, so on the tick that opens the screen the
        // slot is still empty — and an absent session counts as expired.
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.provisioning_expired = true; // no session exists yet, so: "expired"
        h.press(t0, Button::Enter);

        let out = h.idle(t0 + COUNTDOWN_DURATION);

        assert_eq!(
            out.effects,
            vec![Effect::OpenQrSession],
            "the QR screen must not be torn down on the tick that opened it"
        );
    }

    #[test]
    fn up_does_nothing_on_the_qr_screen() {
        let t0 = Instant::now();
        let mut h = Harness::new();
        h.press(t0, Button::Enter);
        h.idle(t0 + COUNTDOWN_DURATION);
        h.release(t0 + COUNTDOWN_DURATION, Button::Enter);

        let out = h.press(t0 + Duration::from_secs(3), Button::Up);

        assert_eq!(
            out.effects,
            vec![Effect::MarkActivity],
            "the QR screen is modal: only ENTER leaves it"
        );
    }
}
