//! Display/UI system for FIBER Medical Thermometer
//!
//! Provides a dedicated display monitor thread that renders sensor information
//! to the ST7920 graphical LCD display. The display shows sensor temperatures,
//! alarm states, and status indicators in a multi-page format.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rppal::gpio::Gpio;

use crate::libs::buzzer::BuzzerPriorityManager;
use crate::libs::leds::SharedLedStateHandle;
use crate::libs::lorawan::SharedLoRaWANState;
use crate::libs::network::{NetworkStatus, SharedProvisioningSession};
use crate::libs::sensors::SharedSensorStateHandle;

/// Type alias for shared screen brightness handle (0-100%)
pub type SharedScreenBrightnessHandle = Arc<AtomicU8>;

/// Type alias for shared screen idle-timeout handle (seconds; 0 = always on).
/// Read live by the display loop so the timeout can be changed at runtime
/// (e.g. via MQTT) without restarting.
pub type SharedScreenTimeoutHandle = Arc<AtomicU32>;

/// Type alias for the live overview-line configuration.
///
/// Read by the display loop each frame and written by the MQTT executor after a
/// successful config write, so a pushed change takes effect on the next frame
/// rather than waiting for the periodic config reconcile. Mirrors the existing
/// [`crate::libs::lorawan::SharedLoRaWANSensorConfigs`] pattern — a `Vec` can't
/// live in an atomic.
pub type SharedDisplayLinesHandle = Arc<std::sync::RwLock<Vec<crate::libs::config::DisplayLine>>>;

/// What the sensor overview is currently showing.
///
/// Derived in exactly one place ([`DisplayState::overview_mode`]) so the page
/// count, the paging arithmetic and the renderer dispatch cannot disagree about
/// which row set is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewMode {
    /// Built-in active-first layout: 8 DS18B20 slots plus N stickers.
    Default { rows: usize },
    /// User-configured display lines.
    ///
    /// `NonZeroUsize` is load-bearing: an empty custom list *is* [`Self::Default`]
    /// (that's the documented fallback), so "custom mode with zero rows" is a
    /// state that cannot be constructed, and no downstream arithmetic has to
    /// defend against it.
    Custom { rows: std::num::NonZeroUsize },
}

impl OverviewMode {
    /// Number of rows to page through.
    pub fn rows(&self) -> usize {
        match self {
            Self::Default { rows } => *rows,
            Self::Custom { rows } => rows.get(),
        }
    }

    /// Total number of pages. Always at least 1, for every possible input —
    /// the overview screen exists even with nothing to show on it.
    pub fn total_pages(&self) -> usize {
        self.rows().max(1).div_ceil(screens::ROWS_PER_PAGE)
    }

    /// True when the user's configured lines are being rendered.
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }
}

/// Number of pages on the system info screen.
pub const SYSTEM_INFO_PAGES: usize = 3;

/// Fixed labels for the UP-hold local action menu, in display order. A plain
/// index into this array is enough — unlike the sensor-selection cursor,
/// this list's length and membership never change at runtime.
pub const MENU_ITEMS: [&str; 3] = ["Pairing code", "Reboot", "Shutdown"];

/// Which destructive local action a [`Screen::Confirm`] is guarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAction {
    Reboot,
    Shutdown,
}

pub mod blank;
pub mod buttons;
pub mod font;
pub mod icons;
pub mod monitor;
pub mod overview;
pub mod screens;
pub mod splash;
pub mod supervise;

pub use buttons::ButtonMonitor;

/// Enum representing different display screens
#[derive(Clone, Debug)]
pub enum Screen {
    /// Sensor overview showing temperature readings
    /// page: 0 or 1 (4 sensors per page)
    /// selected_sensor: Some(0-7) when in selection mode, None when in page mode
    SensorOverview {
        page: usize,
        selected_sensor: Option<usize>,
    },
    /// Sensor detail view showing thresholds for a specific sensor
    SensorDetail { sensor_idx: usize },
    /// LoRaWAN sensor detail view
    LoRaWANSensorDetail { dev_eui: String },
    /// QR code configuration screen for Bluetooth/WiFi setup
    QrCodeConfig,
    /// System information screen with pagination
    SystemInfo { page: usize },
    /// Pairing mode - displays pairing code
    Pairing { code: String },
    /// BLE client connected — shows abbreviated client address.
    BleConnected { addr: String },
    /// BLE provisioning a WiFi connection.
    BleProvisioning { ssid: String },
    /// WiFi provisioning succeeded — auto-reverts at `until`.
    BleWifiOk {
        ssid: String,
        ip: String,
        until: std::time::Instant,
    },
    /// WiFi provisioning failed — auto-reverts at `until`.
    BleWifiFail {
        error: String,
        until: std::time::Instant,
    },
    /// UP-hold local action menu. `selected` indexes [`MENU_ITEMS`] (0..3).
    Menu { selected: usize },
    /// Yes/No confirmation before a local Reboot/Shutdown. `yes_selected`
    /// defaults to `false` (No) on entry — an accidental extra press can
    /// never itself confirm a destructive action; the operator must
    /// deliberately move the cursor onto "Yes".
    Confirm {
        action: LocalAction,
        yes_selected: bool,
    },
}

impl Screen {
    /// Get the current page if this is a paginated screen
    pub fn get_page(&self) -> Option<usize> {
        match self {
            Screen::SensorOverview { page, .. } => Some(*page),
            Screen::QrCodeConfig => None,
            Screen::SystemInfo { page } => Some(*page),
            Screen::Pairing { .. } => None,
            Screen::SensorDetail { .. } => None,
            Screen::LoRaWANSensorDetail { .. } => None,
            Screen::BleConnected { .. } => None,
            Screen::BleProvisioning { .. } => None,
            Screen::BleWifiOk { .. } => None,
            Screen::BleWifiFail { .. } => None,
            Screen::Menu { .. } => None,
            Screen::Confirm { .. } => None,
        }
    }

    /// Check if this is a QR code screen
    pub fn is_qr_code(&self) -> bool {
        matches!(self, Screen::QrCodeConfig)
    }

    /// Check if this is a special screen (QR code or Pairing - System info allows navigation)
    pub fn is_special_screen(&self) -> bool {
        matches!(
            self,
            Screen::QrCodeConfig
                | Screen::Pairing { .. }
                | Screen::BleConnected { .. }
                | Screen::BleProvisioning { .. }
                | Screen::BleWifiOk { .. }
                | Screen::BleWifiFail { .. }
                | Screen::Menu { .. }
                | Screen::Confirm { .. }
        )
    }

    /// Check if this is a pairing screen
    pub fn is_pairing(&self) -> bool {
        matches!(self, Screen::Pairing { .. })
    }

    /// Check if this is a system info screen
    pub fn is_system_info(&self) -> bool {
        matches!(self, Screen::SystemInfo { .. })
    }

    /// Check if this is a navigable screen (Sensor Overview or System Info)
    pub fn is_navigable(&self) -> bool {
        matches!(
            self,
            Screen::SensorOverview { .. } | Screen::SystemInfo { .. }
        )
    }

    /// Check if this is a sensor overview screen
    pub fn is_sensor_overview(&self) -> bool {
        matches!(self, Screen::SensorOverview { .. })
    }

    /// Check if sensor selection mode is active
    pub fn is_selection_mode(&self) -> bool {
        matches!(
            self,
            Screen::SensorOverview {
                selected_sensor: Some(_),
                ..
            }
        )
    }

    /// Check if this is a sensor detail screen (DS18B20 or LoRaWAN)
    pub fn is_sensor_detail(&self) -> bool {
        matches!(
            self,
            Screen::SensorDetail { .. } | Screen::LoRaWANSensorDetail { .. }
        )
    }

    /// Get selected sensor index if in selection mode
    pub fn get_selected_sensor(&self) -> Option<usize> {
        match self {
            Screen::SensorOverview {
                selected_sensor, ..
            } => *selected_sensor,
            _ => None,
        }
    }
}

/// Shared display state for managing screen navigation and display control
pub struct DisplayState {
    /// Current screen being displayed
    pub current_screen: Screen,
    /// Whether the display should be updated
    pub should_update: bool,
    /// Live provisioning session handle. None until [`set_provisioning_session`]
    /// is called from main. The QR-config screen pulls the current session's
    /// `QrCodeGenerator` on every frame, so rotating the session updates the
    /// rendered QR without any further plumbing.
    pub provisioning_session: Option<SharedProvisioningSession>,
    /// Current network connection status
    pub network_status: NetworkStatus,
    /// Whether a LoRaWAN gateway is present (set from main after detection)
    pub lorawan_gateway_present: bool,
    /// Shared LoRaWAN state for display rendering
    pub lorawan_state: Option<SharedLoRaWANState>,
    /// Buzzer priority manager for checking mute state
    pub buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
    /// Current page within the LoRa detail view (0..3). Reset to 0 on each entry.
    pub lorawan_detail_page: u8,
    /// Shared LoRa configs handle (for rendering thresholds + location pages).
    pub lorawan_configs: Option<crate::libs::lorawan::SharedLoRaWANSensorConfigs>,
    /// Button-hold progress in pixels (0..=127). 0 = no hold active; otherwise the
    /// width of the 1-px progress bar drawn under the header divider on the sensor
    /// overview. Written by the button monitor thread each poll and read by the
    /// display monitor.
    pub hold_bar_pixels: u8,
    /// Instant of the last user activity (button press). Drives the backlight
    /// idle timeout: the display monitor turns the backlight off once the
    /// elapsed time since this exceeds the configured timeout. Written by the
    /// button monitor via [`DisplayState::mark_activity`] and read by the
    /// display monitor each frame.
    pub last_activity: Instant,
    /// Number of configured custom overview lines, or 0 for the built-in
    /// layout. Refreshed by the display monitor's config reconcile; consumed by
    /// [`DisplayState::overview_mode`] so the button thread pages over the same
    /// row count the renderer is drawing.
    pub custom_line_count: usize,
}

impl DisplayState {
    pub fn new() -> Self {
        Self {
            current_screen: Screen::SensorOverview {
                page: 0,
                selected_sensor: None,
            },
            should_update: true,
            provisioning_session: None,
            network_status: NetworkStatus::disconnected(),
            lorawan_gateway_present: false,
            lorawan_state: None,
            buzzer_priority: None,
            lorawan_detail_page: 0,
            lorawan_configs: None,
            hold_bar_pixels: 0,
            last_activity: Instant::now(),
            custom_line_count: 0,
        }
    }

    /// Record user activity (called on any button press). Resets the backlight
    /// idle timeout so the display stays lit for another full timeout period.
    pub fn mark_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Which row set the overview is showing right now.
    ///
    /// Selection mode always falls back to the canonical sensor list: custom
    /// lines may repeat a source (two rows on one sticker is the headline use
    /// case), and the selection cursor is keyed on a sensor's global index, so a
    /// custom-derived list would make the second row on a sensor unreachable.
    ///
    /// That canonical list is the *filtered* one — a sensor that has never
    /// reported is hidden here too, so selection paging matches what the built-in
    /// overview draws. Custom mode is deliberately exempt: a configured line is
    /// an explicit request, and [`overview`] renders `?` for a source that has
    /// never reported rather than dropping the row.
    ///
    /// Total function: no `unwrap`, no panicking path.
    pub fn overview_mode(
        &self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) -> OverviewMode {
        if self.current_screen.is_selection_mode() {
            OverviewMode::Default {
                rows: self.visible_sensor_count(ds_readings, ds_has_reported),
            }
        } else {
            self.page_mode(ds_readings, ds_has_reported)
        }
    }

    /// The mode page mode would use, independent of the current screen.
    ///
    /// Needed by [`Self::exit_selection_mode`], which has to know the page count
    /// it's about to switch *to* while still in selection mode.
    fn page_mode(
        &self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) -> OverviewMode {
        match std::num::NonZeroUsize::new(self.custom_line_count) {
            Some(rows) => OverviewMode::Custom { rows },
            None => OverviewMode::Default {
                rows: self.visible_sensor_count(ds_readings, ds_has_reported),
            },
        }
    }

    /// How many sensors the built-in overview actually lists.
    ///
    /// The *filtered* count, not [`Self::total_sensor_count`] — this is what
    /// makes hide-never-reported apply to the page count, and it comes from the
    /// same entry list the renderer uses so paging can't run past the last
    /// populated row.
    fn visible_sensor_count(
        &self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) -> usize {
        self.ordered_entries(ds_readings, ds_has_reported).len()
    }

    /// Total number of overview pages for the current mode. Always >= 1, so the
    /// header always reads a valid `n/m` and the paging arithmetic stays safe
    /// when no sensor has ever reported.
    pub fn total_pages(
        &self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) -> usize {
        self.overview_mode(ds_readings, ds_has_reported)
            .total_pages()
    }

    /// Pull the overview page and the selection cursor back into range after
    /// the visible sensor list shrinks (a LoRa sticker can leave the map), so
    /// the screen self-corrects instead of sitting on a blank page — or in
    /// selection mode with no cursor drawn — until the next button press.
    /// No-op on any other screen.
    pub fn clamp_overview(&mut self, entries: &[crate::libs::display::screens::OverviewEntry]) {
        let Screen::SensorOverview {
            page,
            selected_sensor,
        } = &self.current_screen
        else {
            return;
        };
        let (page, selected_sensor) = (*page, *selected_sensor);

        // A cursor on a sensor that is no longer listed can't be drawn — move
        // it to the first surviving entry, or drop out of selection mode if
        // nothing is left.
        let (new_page, new_selected) = match selected_sensor {
            Some(idx) if !entries.iter().any(|e| e.global_idx == idx) => {
                (0, entries.first().map(|e| e.global_idx))
            }
            _ => (
                page.min(crate::libs::display::screens::page_count(entries) - 1),
                selected_sensor,
            ),
        };

        if (new_page, new_selected) != (page, selected_sensor) {
            self.current_screen = Screen::SensorOverview {
                page: new_page,
                selected_sensor: new_selected,
            };
            self.should_update = true;
        }
    }

    /// Snapshot the current ordered list of overview entries.
    /// Returns an empty list if shared state isn't ready.
    pub fn ordered_entries(
        &self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) -> Vec<crate::libs::display::screens::OverviewEntry> {
        let lr_vec: Vec<crate::libs::lorawan::state::LoRaWANSensorState> = self
            .lorawan_state
            .as_ref()
            .and_then(|s| s.read().ok())
            .map(|s| {
                let mut v: Vec<_> = s.sensors.values().cloned().collect();
                v.sort_by(|a, b| a.dev_eui.cmp(&b.dev_eui));
                v
            })
            .unwrap_or_default();
        crate::libs::display::screens::ordered_sensors(ds_readings, ds_has_reported, &lr_vec)
    }

    /// Get sorted LoRaWAN dev_euis for consistent indexing
    pub fn sorted_lorawan_dev_euis(&self) -> Vec<String> {
        self.lorawan_state
            .as_ref()
            .and_then(|s| s.read().ok())
            .map(|s| {
                let mut euis: Vec<String> = s.sensors.keys().cloned().collect();
                euis.sort();
                euis
            })
            .unwrap_or_default()
    }

    /// Attach the shared provisioning-session handle. The QR-config screen
    /// reads the inner [`crate::libs::network::ProvisioningSession`] on each
    /// frame and renders its precomputed QR.
    pub fn set_provisioning_session(&mut self, session: SharedProvisioningSession) {
        self.provisioning_session = Some(session);
    }

    /// Navigate to next page (works for sensor overview and system info when not in selection mode)
    ///
    /// Wraps with a comparison rather than `%`. This runs on the button thread,
    /// where a divide-by-zero would panic and permanently kill navigation, so
    /// there is deliberately no division here to reason about — even though
    /// [`OverviewMode::total_pages`] already guarantees a non-zero count.
    pub fn next_page(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        match self.current_screen {
            Screen::SensorOverview {
                page,
                selected_sensor: None,
            } => {
                let total = self.total_pages(ds_readings, ds_has_reported);
                let next = if page + 1 >= total { 0 } else { page + 1 };
                self.current_screen = Screen::SensorOverview {
                    page: next,
                    selected_sensor: None,
                };
            }
            Screen::SystemInfo { page } => {
                // System info has 3 pages (0, 1, 2)
                let next = if page + 1 >= SYSTEM_INFO_PAGES {
                    0
                } else {
                    page + 1
                };
                self.current_screen = Screen::SystemInfo { page: next };
            }
            _ => {}
        }
    }

    /// Move forward through LoRa detail pages: 0 → 1 → 2, clamped at 2 (no wraparound).
    pub fn lorawan_detail_next(&mut self) {
        if matches!(self.current_screen, Screen::LoRaWANSensorDetail { .. }) {
            if self.lorawan_detail_page < 2 {
                self.lorawan_detail_page += 1;
                self.should_update = true;
            }
        }
    }

    /// Move backward through LoRa detail pages: 2 → 1 → 0, clamped at 0 (no wraparound).
    pub fn lorawan_detail_prev(&mut self) {
        if matches!(self.current_screen, Screen::LoRaWANSensorDetail { .. }) {
            if self.lorawan_detail_page > 0 {
                self.lorawan_detail_page -= 1;
                self.should_update = true;
            }
        }
    }

    /// Switch to QR code screen
    pub fn show_qr_code(&mut self) {
        self.current_screen = Screen::QrCodeConfig;
    }

    /// Return to sensor overview (page 0, no selection)
    pub fn show_sensor_overview(&mut self) {
        self.current_screen = Screen::SensorOverview {
            page: 0,
            selected_sensor: None,
        };
    }

    /// Switch to system info screen (page 0)
    pub fn show_system_info(&mut self) {
        self.current_screen = Screen::SystemInfo { page: 0 };
    }

    /// Switch to pairing screen with code
    pub fn show_pairing(&mut self, code: String) {
        self.current_screen = Screen::Pairing { code };
        self.should_update = true;
    }

    /// Open the front-panel local action menu, cursor on the first item.
    pub fn show_menu(&mut self) {
        self.current_screen = Screen::Menu { selected: 0 };
        self.should_update = true;
    }

    /// Move the menu cursor up, wrapping from the first to the last item.
    /// No-op off the menu screen.
    pub fn menu_up(&mut self) {
        if let Screen::Menu { selected } = self.current_screen {
            let n = MENU_ITEMS.len();
            self.current_screen = Screen::Menu {
                selected: (selected + n - 1) % n,
            };
            self.should_update = true;
        }
    }

    /// Move the menu cursor down, wrapping from the last to the first item.
    /// No-op off the menu screen.
    pub fn menu_down(&mut self) {
        if let Screen::Menu { selected } = self.current_screen {
            let n = MENU_ITEMS.len();
            self.current_screen = Screen::Menu {
                selected: (selected + 1) % n,
            };
            self.should_update = true;
        }
    }

    /// Show the Yes/No confirmation for a pending local Reboot/Shutdown,
    /// defaulting the cursor to "No".
    pub fn show_confirm(&mut self, action: LocalAction) {
        self.current_screen = Screen::Confirm {
            action,
            yes_selected: false,
        };
        self.should_update = true;
    }

    /// Flip the confirm screen's Yes/No cursor. No-op off the confirm screen.
    pub fn confirm_toggle(&mut self) {
        if let Screen::Confirm {
            action,
            yes_selected,
        } = self.current_screen
        {
            self.current_screen = Screen::Confirm {
                action,
                yes_selected: !yes_selected,
            };
            self.should_update = true;
        }
    }

    /// Show "BLE Connected" with truncated address.
    pub fn show_ble_connected(&mut self, addr: &str) {
        let short = if addr.len() > 17 { &addr[..17] } else { addr };
        self.current_screen = Screen::BleConnected {
            addr: short.to_string(),
        };
        self.should_update = true;
    }

    /// Show "Connecting WiFi..." with the SSID being attempted.
    pub fn show_ble_provisioning(&mut self, ssid: &str) {
        self.current_screen = Screen::BleProvisioning {
            ssid: ssid.to_string(),
        };
        self.should_update = true;
    }

    /// Show "WiFi OK" with IP, dwell 3s, then auto-revert via tick_timed_screens.
    pub fn show_ble_wifi_ok(&mut self, ssid: &str, ip: &str) {
        self.current_screen = Screen::BleWifiOk {
            ssid: ssid.to_string(),
            ip: ip.to_string(),
            until: std::time::Instant::now() + std::time::Duration::from_secs(3),
        };
        self.should_update = true;
    }

    /// Show "WiFi Failed" with truncated error, dwell 5s.
    pub fn show_ble_wifi_fail(&mut self, error: &str) {
        let short_err: String = error.chars().take(30).collect();
        self.current_screen = Screen::BleWifiFail {
            error: short_err,
            until: std::time::Instant::now() + std::time::Duration::from_secs(5),
        };
        self.should_update = true;
    }

    /// Auto-revert from time-limited BLE provisioning screens.
    /// Called from the display monitor loop on each tick.
    pub fn tick_timed_screens(&mut self) {
        let now = std::time::Instant::now();
        let revert = match &self.current_screen {
            Screen::BleWifiOk { until, .. } | Screen::BleWifiFail { until, .. } => now >= *until,
            _ => false,
        };
        if revert {
            self.show_sensor_overview();
        }
    }

    /// Enter selection mode (from SensorOverview)
    pub fn enter_selection_mode(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if let Screen::SensorOverview { page, .. } = self.current_screen {
            let entries = self.ordered_entries(ds_readings, ds_has_reported);
            if entries.is_empty() {
                return;
            }
            let pos = (page * screens::ROWS_PER_PAGE).min(entries.len() - 1);
            // Derive the page from the clamped cursor rather than carrying the
            // old one over. Selection mode pages over the sensor list, which can
            // be shorter than the custom-line list we're coming from — a stale
            // page past the end would render four blank rows under a "SEL"
            // header, with the cursor sitting on a page the operator can't see.
            let page = pos / screens::ROWS_PER_PAGE;
            let first_global = entries[pos].global_idx;
            self.current_screen = Screen::SensorOverview {
                page,
                selected_sensor: Some(first_global),
            };
            self.should_update = true;
        }
    }

    /// Exit selection mode (return to page mode)
    ///
    /// Takes the reading state because the page it lands on depends on the row
    /// set it is returning *to*, and the default row set hides sensors that have
    /// never reported.
    pub fn exit_selection_mode(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if let Screen::SensorOverview {
            page,
            selected_sensor: Some(_),
        } = self.current_screen
        {
            // Selection mode pages over the full sensor list, which may have
            // more pages than the custom-line list we're returning to. Without
            // this clamp the stale page would land past the end of the rows and
            // render an empty screen.
            let max_page = self
                .page_mode(ds_readings, ds_has_reported)
                .total_pages()
                .saturating_sub(1);
            self.current_screen = Screen::SensorOverview {
                page: page.min(max_page),
                selected_sensor: None,
            };
            self.should_update = true;
        }
    }

    /// Move selection cursor up within the ordered (active-first) list.
    pub fn selection_up(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if let Screen::SensorOverview {
            selected_sensor: Some(idx),
            ..
        } = self.current_screen
        {
            let entries = self.ordered_entries(ds_readings, ds_has_reported);
            if entries.is_empty() {
                return;
            }
            let pos = entries
                .iter()
                .position(|e| e.global_idx == idx)
                .unwrap_or(0);
            let new_pos = if pos == 0 { entries.len() - 1 } else { pos - 1 };
            let new_global = entries[new_pos].global_idx;
            let new_page = new_pos / screens::ROWS_PER_PAGE;
            self.current_screen = Screen::SensorOverview {
                page: new_page,
                selected_sensor: Some(new_global),
            };
            self.should_update = true;
        }
    }

    /// Move selection cursor down within the ordered (active-first) list.
    pub fn selection_down(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if let Screen::SensorOverview {
            selected_sensor: Some(idx),
            ..
        } = self.current_screen
        {
            let entries = self.ordered_entries(ds_readings, ds_has_reported);
            if entries.is_empty() {
                return;
            }
            let pos = entries
                .iter()
                .position(|e| e.global_idx == idx)
                .unwrap_or(0);
            let new_pos = if pos + 1 >= entries.len() { 0 } else { pos + 1 };
            let new_global = entries[new_pos].global_idx;
            let new_page = new_pos / screens::ROWS_PER_PAGE;
            self.current_screen = Screen::SensorOverview {
                page: new_page,
                selected_sensor: Some(new_global),
            };
            self.should_update = true;
        }
    }

    /// Enter detail view for selected sensor
    pub fn enter_detail_view(&mut self) {
        if let Screen::SensorOverview {
            selected_sensor: Some(idx),
            ..
        } = self.current_screen
        {
            if idx >= 8 {
                // LoRaWAN sensor - find dev_eui by sorted index
                let lorawan_idx = idx - 8;
                let dev_euis = self.sorted_lorawan_dev_euis();
                if let Some(dev_eui) = dev_euis.get(lorawan_idx) {
                    self.current_screen = Screen::LoRaWANSensorDetail {
                        dev_eui: dev_eui.clone(),
                    };
                    self.lorawan_detail_page = 0;
                    self.should_update = true;
                }
            } else {
                self.current_screen = Screen::SensorDetail { sensor_idx: idx };
                self.should_update = true;
            }
        }
    }

    /// Exit detail view back to selection mode
    pub fn exit_detail_view(
        &mut self,
        ds_readings: &[Option<crate::libs::sensors::state::SensorReading>; 8],
        ds_has_reported: &[bool; 8],
    ) {
        if !self.current_screen.is_sensor_detail() {
            return;
        }
        let target_global = match &self.current_screen {
            Screen::SensorDetail { sensor_idx } => Some(*sensor_idx),
            Screen::LoRaWANSensorDetail { dev_eui } => {
                let dev_euis = self.sorted_lorawan_dev_euis();
                dev_euis.iter().position(|e| e == dev_eui).map(|i| 8 + i)
            }
            _ => None,
        };
        // A LoRa sticker can leave the map while its detail view is open, in
        // which case there is no global index to go back to at all. Falling
        // through here would leave the detail screen up while the button state
        // machine has already moved to selection mode — the click would look
        // dead — so treat it like a sensor that is no longer listed.
        let entries = self.ordered_entries(ds_readings, ds_has_reported);
        let pos = target_global.and_then(|idx| entries.iter().position(|e| e.global_idx == idx));
        // Land on a real entry rather than storing an index the renderer can't
        // draw a cursor for, which would leave selection mode with no cursor.
        let (page, selected) = match pos {
            Some(pos) => (pos / screens::ROWS_PER_PAGE, Some(entries[pos].global_idx)),
            // Nothing left to select — drop out of selection mode.
            None => (0, entries.first().map(|e| e.global_idx)),
        };
        self.current_screen = Screen::SensorOverview {
            page,
            selected_sensor: selected,
        };
        self.should_update = true;
    }
}

/// Type alias for shared display state handle
pub type SharedDisplayStateHandle = Arc<Mutex<DisplayState>>;

/// Display monitor that manages the ST7920 LCD display
///
/// This monitor runs in a dedicated thread and is responsible for:
/// - Initializing the ST7920 display hardware
/// - Reading sensor and LED state from shared handles
/// - Rendering the current screen to the display
/// - Managing page navigation
pub struct DisplayMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    pub display_state: SharedDisplayStateHandle,
}

impl DisplayMonitor {
    /// Create and spawn the display monitor thread
    pub fn new(
        led_state: SharedLedStateHandle,
        gpio: Arc<Gpio>,
        sensor_state: SharedSensorStateHandle,
        power_status: crate::libs::power::SharedPowerStatus,
        hostname: String,
        device_label: String,
        app_version: String,
        timezone_offset_hours: i8,
        screen_brightness: SharedScreenBrightnessHandle,
        screen_timeout: SharedScreenTimeoutHandle,
        display_lines: SharedDisplayLinesHandle,
    ) -> io::Result<Self> {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();
        let display_state = Arc::new(Mutex::new(DisplayState::new()));
        let display_state_clone = display_state.clone();

        let thread_handle = thread::spawn(move || {
            // Contain panics and init failures: either one left bare would end
            // the thread for good, leaving the operator with a dark panel and no
            // indication why. Every argument is cloned per attempt so the loop is
            // re-callable, and the restarted loop re-runs St7920::init(),
            // resetting the controller out of whatever state it was left in.
            supervise::supervise("display", &shutdown_flag_clone, || {
                monitor::display_loop(
                    shutdown_flag_clone.clone(),
                    display_state_clone.clone(),
                    led_state.clone(),
                    gpio.clone(),
                    sensor_state.clone(),
                    power_status.clone(),
                    hostname.clone(),
                    device_label.clone(),
                    app_version.clone(),
                    timezone_offset_hours,
                    screen_brightness.clone(),
                    screen_timeout.clone(),
                    display_lines.clone(),
                )
            });
        });

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            display_state,
        })
    }

    /// Set the buzzer priority manager for mute icon display.
    /// Called after both display and buzzer are initialized.
    pub fn set_buzzer_priority(&self, bp: Arc<BuzzerPriorityManager>) {
        if let Ok(mut ds) = self.display_state.lock() {
            ds.buzzer_priority = Some(bp);
        }
    }

    /// Gracefully shutdown the display monitor thread
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

impl Drop for DisplayMonitor {
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

#[cfg(test)]
mod paging_tests {
    use super::*;

    /// A `DisplayState` with no LoRa handle attached, so the default row set is
    /// exactly the *visible* DS18B20 slots.
    fn state(custom_line_count: usize) -> DisplayState {
        let mut ds = DisplayState::new();
        ds.custom_line_count = custom_line_count;
        ds
    }

    fn no_readings() -> [Option<crate::libs::sensors::state::SensorReading>; 8] {
        Default::default()
    }

    /// Every slot has latched a reading at some point, so all 8 rows are listed.
    ///
    /// Stated explicitly because the default row set is filtered: these paging
    /// cases were written against a full 8-sensor list, and without this they
    /// would be exercising a 0-row overview instead. `ds_slot_visible` is
    /// one-way, so `has_reported` alone is enough — no live readings needed.
    fn all_reported() -> [bool; 8] {
        [true; 8]
    }

    /// Nothing has ever reported: the default row set is empty.
    fn none_reported() -> [bool; 8] {
        [false; 8]
    }

    #[test]
    fn total_pages_uses_custom_line_count_in_page_mode() {
        let (r, h) = (no_readings(), all_reported());
        assert_eq!(state(1).total_pages(&r, &h), 1);
        assert_eq!(state(4).total_pages(&r, &h), 1);
        assert_eq!(state(5).total_pages(&r, &h), 2);
        assert_eq!(state(16).total_pages(&r, &h), 4);
    }

    #[test]
    fn total_pages_uses_sensor_count_in_selection_mode() {
        let (r, h) = (no_readings(), all_reported());
        let mut ds = state(1);
        // Page mode: one custom line, one page.
        assert_eq!(ds.total_pages(&r, &h), 1);
        // Selection mode falls back to the canonical sensor list.
        ds.current_screen = Screen::SensorOverview {
            page: 0,
            selected_sensor: Some(0),
        };
        assert_eq!(
            ds.total_pages(&r, &h),
            2,
            "8 DS18B20 slots over 4 rows/page"
        );
    }

    #[test]
    fn total_pages_falls_back_to_sensor_count_with_no_custom_lines() {
        assert_eq!(state(0).total_pages(&no_readings(), &all_reported()), 2);
    }

    #[test]
    fn default_mode_pages_over_visible_sensors_only() {
        // The composition point between hide-never-reported and custom lines:
        // with no custom lines the row count is the *filtered* sensor list, not
        // all 8 slots. Guards against reverting to `total_sensor_count()`.
        let r = no_readings();
        assert_eq!(
            state(0).total_pages(&r, &none_reported()),
            1,
            "nothing visible still has one page"
        );

        let mut four = none_reported();
        four[0..4].fill(true);
        assert_eq!(
            state(0).total_pages(&r, &four),
            1,
            "4 visible rows fit one page"
        );

        let mut five = none_reported();
        five[0..5].fill(true);
        assert_eq!(
            state(0).total_pages(&r, &five),
            2,
            "5 visible rows spill onto a second page"
        );
    }

    #[test]
    fn selection_mode_pages_over_visible_sensors_only() {
        // Selection mode falls back to the canonical list, and that list is
        // filtered too — so an unreported slot is not selectable.
        let mut ds = state(1);
        ds.current_screen = Screen::SensorOverview {
            page: 0,
            selected_sensor: Some(0),
        };
        assert_eq!(ds.total_pages(&no_readings(), &none_reported()), 1);
    }

    #[test]
    fn custom_mode_is_exempt_from_hide_never_reported() {
        // A configured line is an explicit request: `overview` renders `?` for a
        // source that has never reported rather than dropping the row, so the
        // custom page count must not depend on visibility at all.
        let r = no_readings();
        assert_eq!(state(16).total_pages(&r, &none_reported()), 4);
        assert_eq!(state(16).total_pages(&r, &all_reported()), 4);
    }

    #[test]
    fn overview_mode_cannot_be_custom_with_zero_rows() {
        // The invariant that makes the paging arithmetic safe by construction.
        let (r, h) = (no_readings(), all_reported());
        assert!(!state(0).overview_mode(&r, &h).is_custom());
        assert!(state(1).overview_mode(&r, &h).is_custom());
    }

    #[test]
    fn total_pages_is_never_zero_over_full_input_space() {
        // Proves the invariant the paging arithmetic relies on, rather than
        // defending against a violation at each use site. Swept over visibility
        // as well, since the default row count now depends on it — an empty
        // filtered list is the case most likely to reintroduce a zero.
        let r = no_readings();
        for custom_line_count in 0..=64usize {
            for selected in [None, Some(0usize)] {
                for (label, h) in [
                    ("none reported", none_reported()),
                    ("all reported", all_reported()),
                ] {
                    let mut ds = state(custom_line_count);
                    ds.current_screen = Screen::SensorOverview {
                        page: 0,
                        selected_sensor: selected,
                    };
                    let total = ds.total_pages(&r, &h);
                    assert!(
                        total >= 1,
                        "total_pages() must never be 0 (lines={}, selected={:?}, {})",
                        custom_line_count,
                        selected,
                        label,
                    );

                    // And next_page() must map every valid page back into range.
                    for page in 0..total {
                        ds.current_screen = Screen::SensorOverview {
                            page,
                            selected_sensor: selected,
                        };
                        ds.next_page(&r, &h);
                        if selected.is_some() {
                            // Paging is disabled in selection mode.
                            assert_eq!(ds.current_screen.get_page(), Some(page));
                        } else {
                            let next = ds.current_screen.get_page().expect("still on overview");
                            assert!(
                                next < total,
                                "next_page() left range: {} >= {}",
                                next,
                                total
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn next_page_wraps_with_zero_custom_lines() {
        // Regression guard for the divide-by-zero this arithmetic used to have:
        // must land on a valid page, and above all must not panic.
        let mut ds = state(0);
        ds.current_screen = Screen::SensorOverview {
            page: 1,
            selected_sensor: None,
        };
        ds.next_page(&no_readings(), &all_reported());
        assert_eq!(ds.current_screen.get_page(), Some(0));
    }

    #[test]
    fn next_page_wraps_at_last_custom_page() {
        let (r, h) = (no_readings(), all_reported());
        let mut ds = state(16);
        ds.current_screen = Screen::SensorOverview {
            page: 2,
            selected_sensor: None,
        };
        ds.next_page(&r, &h);
        assert_eq!(ds.current_screen.get_page(), Some(3));
        ds.next_page(&r, &h);
        assert_eq!(ds.current_screen.get_page(), Some(0), "4 pages wrap 3 -> 0");
    }

    #[test]
    fn next_page_wraps_system_info_over_three_pages() {
        let (r, h) = (no_readings(), all_reported());
        let mut ds = state(0);
        for expected in [1, 2, 0] {
            ds.current_screen = Screen::SystemInfo {
                page: if expected == 0 {
                    SYSTEM_INFO_PAGES - 1
                } else {
                    expected - 1
                },
            };
            ds.next_page(&r, &h);
            assert_eq!(ds.current_screen.get_page(), Some(expected));
        }
    }

    #[test]
    fn exit_selection_mode_clamps_stale_page() {
        // Selection mode pages over 8 sensors (2 pages); page mode here has a
        // single custom line (1 page). Without the clamp the stale page 1 would
        // render an empty screen.
        let mut ds = state(1);
        ds.current_screen = Screen::SensorOverview {
            page: 1,
            selected_sensor: Some(4),
        };
        ds.exit_selection_mode(&no_readings(), &all_reported());
        assert_eq!(ds.current_screen.get_page(), Some(0));
        assert_eq!(ds.current_screen.get_selected_sensor(), None);
    }

    #[test]
    fn enter_selection_mode_clamps_stale_page() {
        // Mirror of exit_selection_mode_clamps_stale_page, for the entry path:
        // 16 custom lines is 4 pages, but selection mode pages over 8 sensors
        // (2 pages). Entering from custom page 3 must land on a page that
        // actually contains the cursor, not leave a blank "SEL" screen.
        let (r, h) = (no_readings(), all_reported());
        let mut ds = state(16);
        ds.current_screen = Screen::SensorOverview {
            page: 3,
            selected_sensor: None,
        };
        ds.enter_selection_mode(&r, &h);

        let page = ds.current_screen.get_page().expect("still on overview");
        let selected = ds.current_screen.get_selected_sensor().expect("cursor set");
        let total = ds.total_pages(&r, &h);
        assert!(page < total, "page {} out of {} pages", page, total);

        // And the cursor must be on the page being shown.
        let entries = ds.ordered_entries(&r, &h);
        let pos = entries
            .iter()
            .position(|e| e.global_idx == selected)
            .unwrap();
        assert_eq!(pos / screens::ROWS_PER_PAGE, page, "cursor is off-page");
    }

    #[test]
    fn enter_selection_mode_keeps_valid_page() {
        // No custom lines: page 1 of the 8-sensor list is valid in both modes
        // and must survive, cursor landing on the first row of that page.
        let (r, h) = (no_readings(), all_reported());
        let mut ds = state(0);
        ds.current_screen = Screen::SensorOverview {
            page: 1,
            selected_sensor: None,
        };
        ds.enter_selection_mode(&r, &h);
        assert_eq!(ds.current_screen.get_page(), Some(1));

        let selected = ds.current_screen.get_selected_sensor().expect("cursor set");
        let entries = ds.ordered_entries(&r, &h);
        let pos = entries
            .iter()
            .position(|e| e.global_idx == selected)
            .unwrap();
        assert_eq!(pos, screens::ROWS_PER_PAGE, "first row of page 1");
    }

    #[test]
    fn exit_selection_mode_keeps_valid_page() {
        let mut ds = state(16);
        ds.current_screen = Screen::SensorOverview {
            page: 1,
            selected_sensor: Some(4),
        };
        ds.exit_selection_mode(&no_readings(), &all_reported());
        assert_eq!(ds.current_screen.get_page(), Some(1));
    }
}

#[cfg(test)]
mod pagination_tests {
    use super::*;
    use crate::libs::alarms::AlarmState;
    use crate::libs::sensors::state::SensorReading;

    fn connected(temp: f32) -> Option<SensorReading> {
        Some(SensorReading {
            temperature: temp,
            is_connected: true,
            alarm_state: AlarmState::Normal,
        })
    }

    fn empty_ds() -> [Option<SensorReading>; 8] {
        [None, None, None, None, None, None, None, None]
    }

    /// No slot has latched a connected reading yet.
    fn no_reports() -> [bool; 8] {
        [false; 8]
    }

    #[test]
    fn total_pages_is_at_least_one_when_nothing_reported() {
        let state = DisplayState::new();
        assert_eq!(state.total_pages(&empty_ds(), &no_reports()), 1);
    }

    #[test]
    fn total_pages_counts_only_visible_sensors() {
        let state = DisplayState::new();
        let mut ds_arr = empty_ds();
        for i in 0..3 {
            ds_arr[i] = connected(20.0);
        }
        assert_eq!(state.total_pages(&ds_arr, &no_reports()), 1);
        ds_arr[4] = connected(20.0);
        ds_arr[5] = connected(20.0);
        assert_eq!(state.total_pages(&ds_arr, &no_reports()), 2);
    }

    #[test]
    fn next_page_does_not_divide_by_zero_when_nothing_reported() {
        let mut state = DisplayState::new();
        state.next_page(&empty_ds(), &no_reports());
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, .. }
        ));
    }

    /// `n` visible DS18B20 entries, slots 0..n.
    fn entries(n: usize) -> Vec<crate::libs::display::screens::OverviewEntry> {
        use crate::libs::display::screens::{OverviewEntry, OverviewKind};
        (0..n)
            .map(|i| OverviewEntry {
                kind: OverviewKind::Ds18b20,
                global_idx: i,
                active: true,
            })
            .collect()
    }

    #[test]
    fn clamp_overview_pulls_stale_page_back() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview {
            page: 3,
            selected_sensor: None,
        };
        state.clamp_overview(&entries(2));
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, .. }
        ));
    }

    #[test]
    fn clamp_overview_leaves_valid_page_alone() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview {
            page: 1,
            selected_sensor: None,
        };
        state.clamp_overview(&entries(6));
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 1, .. }
        ));
    }

    /// A cursor left pointing at a sensor that dropped off the list (a LoRa
    /// sticker leaving the map) must move to a row the renderer can draw.
    #[test]
    fn clamp_overview_moves_cursor_off_a_vanished_sensor() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview {
            page: 1,
            selected_sensor: Some(9),
        };
        state.clamp_overview(&entries(2));
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview {
                page: 0,
                selected_sensor: Some(0)
            }
        ));
        assert!(state.should_update);
    }

    #[test]
    fn clamp_overview_drops_selection_when_nothing_visible() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview {
            page: 0,
            selected_sensor: Some(3),
        };
        state.clamp_overview(&entries(0));
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview {
                page: 0,
                selected_sensor: None
            }
        ));
    }

    #[test]
    fn clamp_overview_keeps_a_still_listed_cursor() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorOverview {
            page: 1,
            selected_sensor: Some(5),
        };
        state.clamp_overview(&entries(6));
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview {
                page: 1,
                selected_sensor: Some(5)
            }
        ));
    }

    #[test]
    fn clamp_overview_ignores_other_screens() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SystemInfo { page: 2 };
        state.clamp_overview(&entries(0));
        assert!(matches!(
            state.current_screen,
            Screen::SystemInfo { page: 2 }
        ));
    }

    #[test]
    fn exit_detail_view_lands_on_a_listed_sensor() {
        let mut state = DisplayState::new();
        let mut ds_arr = empty_ds();
        ds_arr[4] = connected(20.0);
        // We were inspecting slot 1, which is not in the entry list.
        state.current_screen = Screen::SensorDetail { sensor_idx: 1 };
        state.exit_detail_view(&ds_arr, &no_reports());
        // Must select something the renderer can draw a cursor for, not slot 1.
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview {
                page: 0,
                selected_sensor: Some(4)
            }
        ));
    }

    #[test]
    fn exit_detail_view_keeps_selection_when_still_listed() {
        let mut state = DisplayState::new();
        let mut ds_arr = empty_ds();
        ds_arr[4] = connected(20.0);
        state.current_screen = Screen::SensorDetail { sensor_idx: 4 };
        state.exit_detail_view(&ds_arr, &no_reports());
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview {
                page: 0,
                selected_sensor: Some(4)
            }
        ));
    }

    #[test]
    fn exit_detail_view_drops_selection_when_nothing_listed() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SensorDetail { sensor_idx: 1 };
        state.exit_detail_view(&empty_ds(), &no_reports());
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview {
                page: 0,
                selected_sensor: None
            }
        ));
    }

    /// Regression: a sticker that leaves the map while its detail view is open
    /// has no global index left to resolve. We must still leave the detail
    /// screen — the button state machine has already moved to selection mode,
    /// so staying put makes the click look dead.
    #[test]
    fn exit_detail_view_leaves_screen_when_sticker_vanished() {
        let mut state = DisplayState::new();
        let mut ds_arr = empty_ds();
        ds_arr[4] = connected(20.0);
        state.current_screen = Screen::LoRaWANSensorDetail {
            dev_eui: "0011223344556677".to_string(),
        };
        state.exit_detail_view(&ds_arr, &no_reports());
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview {
                page: 0,
                selected_sensor: Some(4)
            }
        ));
    }

    #[test]
    fn exit_detail_view_is_a_no_op_off_a_detail_screen() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::SystemInfo { page: 1 };
        state.exit_detail_view(&empty_ds(), &no_reports());
        assert!(matches!(
            state.current_screen,
            Screen::SystemInfo { page: 1 }
        ));
    }
}

#[cfg(test)]
mod menu_tests {
    use super::*;

    #[test]
    fn show_menu_defaults_to_first_item() {
        let mut state = DisplayState::new();
        state.show_menu();
        assert!(matches!(state.current_screen, Screen::Menu { selected: 0 }));
    }

    #[test]
    fn menu_up_wraps_from_first_to_last() {
        let mut state = DisplayState::new();
        state.show_menu();
        state.menu_up();
        assert!(matches!(
            state.current_screen,
            Screen::Menu { selected } if selected == MENU_ITEMS.len() - 1
        ));
    }

    #[test]
    fn menu_down_wraps_from_last_to_first() {
        let mut state = DisplayState::new();
        state.current_screen = Screen::Menu {
            selected: MENU_ITEMS.len() - 1,
        };
        state.menu_down();
        assert!(matches!(state.current_screen, Screen::Menu { selected: 0 }));
    }

    #[test]
    fn menu_navigation_is_a_no_op_off_the_menu_screen() {
        let mut state = DisplayState::new();
        state.menu_up();
        state.menu_down();
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, .. }
        ));
    }

    #[test]
    fn show_confirm_defaults_to_no() {
        let mut state = DisplayState::new();
        state.show_confirm(LocalAction::Reboot);
        assert!(matches!(
            state.current_screen,
            Screen::Confirm {
                action: LocalAction::Reboot,
                yes_selected: false
            }
        ));
    }

    #[test]
    fn confirm_toggle_flips_between_yes_and_no() {
        let mut state = DisplayState::new();
        state.show_confirm(LocalAction::Shutdown);
        state.confirm_toggle();
        assert!(matches!(
            state.current_screen,
            Screen::Confirm {
                action: LocalAction::Shutdown,
                yes_selected: true
            }
        ));
        state.confirm_toggle();
        assert!(matches!(
            state.current_screen,
            Screen::Confirm {
                action: LocalAction::Shutdown,
                yes_selected: false
            }
        ));
    }

    #[test]
    fn confirm_toggle_is_a_no_op_off_the_confirm_screen() {
        let mut state = DisplayState::new();
        state.confirm_toggle();
        assert!(matches!(
            state.current_screen,
            Screen::SensorOverview { page: 0, .. }
        ));
    }

    #[test]
    fn get_page_returns_none_for_menu_and_confirm() {
        assert_eq!(Screen::Menu { selected: 0 }.get_page(), None);
        assert_eq!(
            Screen::Confirm {
                action: LocalAction::Reboot,
                yes_selected: false
            }
            .get_page(),
            None
        );
    }

    #[test]
    fn is_special_screen_includes_menu_and_confirm() {
        assert!(Screen::Menu { selected: 0 }.is_special_screen());
        assert!(Screen::Confirm {
            action: LocalAction::Shutdown,
            yes_selected: true
        }
        .is_special_screen());
    }
}
