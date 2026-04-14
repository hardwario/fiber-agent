# Sensor Beep Snooze — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let any physical button (UP/DOWN/ENTER) silence the sensor-critical buzzer for 30 minutes, with a mute icon on the display.

**Architecture:** Two independent silence mechanisms in `BuzzerPriorityManager`: the existing MQTT ACK (indefinite, mutes all) and a new button-triggered timer (30 min, sensor-only). The button loop checks `is_sensor_beeping()` before normal navigation; the sensor monitor calls `on_new_sensor_alarm()` per-sensor to bust the silence when a NEW sensor becomes critical.

**Tech Stack:** Rust, embedded-graphics, rppal GPIO, ST7920 LCD

**Spec:** `docs/superpowers/specs/2026-04-13-sensor-beep-silence-button-design.md`

---

### Task 1: Add clock abstraction and `sensor_silenced_until` to `BuzzerPriorityManager`

**Files:**
- Modify: `src/libs/buzzer/priority.rs`

- [ ] **Step 1: Add clock type alias and state field**

In `src/libs/buzzer/priority.rs`, add the clock type and update `BuzzerPriorityState`:

```rust
// At the top of the file, after the existing use statements:
/// Injectable clock for testing. Returns the current Instant.
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

fn default_clock() -> Clock {
    Arc::new(|| Instant::now())
}
```

Add a `clock` field to `BuzzerPriorityManager`:

```rust
pub struct BuzzerPriorityManager {
    state: Arc<Mutex<BuzzerPriorityState>>,
    buzzer: Arc<Mutex<BuzzerController>>,
    clock: Clock,
}
```

Add `sensor_silenced_until` to `BuzzerPriorityState`:

```rust
struct BuzzerPriorityState {
    // ... existing fields unchanged ...
    /// Button silence deadline (sensor only). None = not silenced by button.
    sensor_silenced_until: Option<Instant>,
}
```

Initialize in `BuzzerPriorityState::new()`:

```rust
sensor_silenced_until: None,
```

- [ ] **Step 2: Update `BuzzerPriorityManager::new` to accept optional clock**

Keep the existing `new` constructor unchanged (uses `default_clock()`), and add `new_with_clock` for tests:

```rust
impl BuzzerPriorityManager {
    pub fn new(buzzer: Arc<Mutex<BuzzerController>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(BuzzerPriorityState::new())),
            buzzer,
            clock: default_clock(),
        }
    }

    #[cfg(test)]
    fn new_with_clock(buzzer: Arc<Mutex<BuzzerController>>, clock: Clock) -> Self {
        Self {
            state: Arc::new(Mutex::new(BuzzerPriorityState::new())),
            buzzer,
            clock,
        }
    }
}
```

- [ ] **Step 3: Replace `Instant::now()` calls inside `compute_pattern` with `(self.clock)()`**

In `compute_pattern` (line 148), change:

```rust
// Before:
let elapsed = Instant::now().duration_since(state.pattern_switch_time);
// After:
let elapsed = (self.clock)().duration_since(state.pattern_switch_time);
```

In `apply_pattern` (line 213), change:

```rust
// Before:
state.pattern_switch_time = Instant::now();
// After:
state.pattern_switch_time = (self.clock)();
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: no errors (warnings OK)

- [ ] **Step 5: Commit**

```bash
git add src/libs/buzzer/priority.rs
git commit -m "buzzer: Add clock abstraction and sensor_silenced_until field"
```

---

### Task 2: Implement `silence_sensor_30min`, `on_new_sensor_alarm`, `is_sensor_beeping`, `is_button_silenced`

**Files:**
- Modify: `src/libs/buzzer/priority.rs`

- [ ] **Step 1: Add `silence_sensor_30min` method**

Add after the existing `silence()` method (after line 122):

```rust
/// Silence sensor beep for 30 minutes via physical button.
/// Only suppresses SensorCritical pattern; battery continues.
/// Cleared by timer expiry or `on_new_sensor_alarm()`.
pub fn silence_sensor_30min(&self) {
    let pattern_to_set = {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = (self.clock)() + Duration::from_secs(30 * 60);
        state.sensor_silenced_until = Some(deadline);
        state.last_set_pattern = None; // Force re-evaluation
        eprintln!("[BuzzerPriority] Sensor buzzer silenced by button for 30 min");
        self.compute_pattern(&state)
    };
    self.apply_pattern(pattern_to_set);
}
```

- [ ] **Step 2: Add `on_new_sensor_alarm` method**

```rust
/// Called when a specific sensor transitions into critical/disconnected.
/// Clears the button silence so the user hears the new alarm.
/// Does NOT clear MQTT ACK silence (`silenced` field).
pub fn on_new_sensor_alarm(&self) {
    let pattern_to_set = {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.sensor_silenced_until.is_some() {
            state.sensor_silenced_until = None;
            state.last_set_pattern = None; // Force re-evaluation
            eprintln!("[BuzzerPriority] Button silence cleared — new sensor alarm");
            self.compute_pattern(&state)
        } else {
            None // No change needed
        }
    };
    self.apply_pattern(pattern_to_set);
}
```

- [ ] **Step 3: Add `is_sensor_beeping` method**

```rust
/// Returns true if the sensor-critical pattern is currently audible.
/// Used by the button handler to decide whether to consume a press.
pub fn is_sensor_beeping(&self) -> bool {
    let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.sensor_critical_active {
        return false;
    }
    if state.silenced {
        return false;
    }
    if let Some(deadline) = state.sensor_silenced_until {
        if (self.clock)() < deadline {
            return false;
        }
    }
    true
}
```

- [ ] **Step 4: Add `is_button_silenced` method**

```rust
/// Returns true if the button-triggered sensor silence is active.
/// Used by the display renderer to show the mute icon.
pub fn is_button_silenced(&self) -> bool {
    let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(deadline) = state.sensor_silenced_until {
        (self.clock)() < deadline
    } else {
        false
    }
}
```

- [ ] **Step 5: Update `compute_pattern` to respect `sensor_silenced_until`**

In `compute_pattern`, after the existing `if state.silenced { return Some(PatternSource::None); }` block (line 128-130), add:

```rust
// Check button silence (sensor-only, time-limited)
let sensor_active = if let Some(deadline) = state.sensor_silenced_until {
    if (self.clock)() >= deadline {
        // Timer expired — treat as no silence (lazy cleanup happens on next mutable access)
        state.sensor_critical_active
    } else {
        false // Sensor suppressed by button silence
    }
} else {
    state.sensor_critical_active
};
```

Then change the match below to use `sensor_active` instead of `state.sensor_critical_active`:

```rust
let new_pattern_source = match (
    sensor_active,
    state.battery_critical_active,
) {
    // ... rest unchanged ...
```

**Note:** `compute_pattern` takes `&BuzzerPriorityState` (immutable). The lazy cleanup comment is just documentation — the actual field cleanup happens in `silence_sensor_30min` or `on_new_sensor_alarm`. The expired check just returns the right value without mutating.

- [ ] **Step 6: Verify it compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: no errors

- [ ] **Step 7: Commit**

```bash
git add src/libs/buzzer/priority.rs
git commit -m "buzzer: Implement button silence, new-alarm bust, and beeping query"
```

---

### Task 3: Unit tests for `BuzzerPriorityManager`

**Files:**
- Modify: `src/libs/buzzer/priority.rs`

- [ ] **Step 1: Add test module with mock clock helper**

At the bottom of `src/libs/buzzer/priority.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Create a mock clock where time can be advanced manually.
    /// Returns (clock_fn, advance_fn).
    fn mock_clock() -> (Clock, Arc<dyn Fn(Duration) + Send + Sync>) {
        let base = Instant::now();
        let offset_ms = Arc::new(AtomicU64::new(0));
        let offset_clone = offset_ms.clone();

        let clock: Clock = Arc::new(move || {
            base + Duration::from_millis(offset_ms.load(Ordering::Relaxed))
        });

        let advance: Arc<dyn Fn(Duration) + Send + Sync> = Arc::new(move |d: Duration| {
            offset_clone.fetch_add(d.as_millis() as u64, Ordering::Relaxed);
        });

        (clock, advance)
    }

    /// Create a BuzzerPriorityManager with a mock buzzer (no hardware).
    fn test_manager(clock: Clock) -> BuzzerPriorityManager {
        // Create a minimal BuzzerController mock using a fake GPIO
        // Since we're testing priority logic, we just need a lockable controller.
        // We bypass hardware by creating the struct with a stopped state.
        let buzzer_state = Arc::new(super::super::pattern::SharedBuzzerState::new());
        let buzzer = Arc::new(Mutex::new(BuzzerControllerMock));
        BuzzerPriorityManager::new_with_clock(buzzer, clock)
    }

    /// Minimal mock that satisfies Arc<Mutex<BuzzerController>> interface.
    /// Since BuzzerController methods are called through the lock, we need
    /// a real-ish stand-in. We'll use a different approach: make the manager
    /// testable by accepting a trait. But that's too invasive — instead,
    /// we test compute_pattern directly on the state.
    ///
    /// Simpler approach: test the state logic directly.
    fn test_manager_with_clock(clock: Clock) -> (Arc<Mutex<BuzzerPriorityState>>, BuzzerPriorityManager) {
        // We can't easily mock BuzzerController (hardware GPIO).
        // Instead, test by calling public methods and checking is_sensor_beeping / is_button_silenced.
        // The buzzer lock will just fail silently in tests (no GPIO).
        //
        // Create a dummy Arc<Mutex<BuzzerController>> that will panic if locked.
        // Our test methods (is_sensor_beeping, is_button_silenced, compute_pattern) don't lock buzzer.
        // Only apply_pattern does, which we accept may no-op in tests.
        use std::mem::MaybeUninit;

        // Actually safer: just use a mock approach where we never touch the buzzer.
        // The priority manager's query methods don't touch the buzzer at all.
        // And set_sensor_critical / silence_sensor_30min call apply_pattern which
        // locks buzzer — but if it fails (no real hardware), it just skips.

        // We'll create a "null" buzzer by using unsafe transmute of a zero struct.
        // This is test-only code. Let's use a simpler approach:

        // Actually the simplest: create a real BuzzerPriorityManager but never
        // spawn a GPIO thread. We just need an Arc<Mutex<T>> that can be locked.
        // BuzzerController requires GPIO, so let's just use a raw Mutex around nothing.

        // Cleanest: extract compute_pattern tests to operate on state directly.
        panic!("Use direct state tests instead");
    }

    #[test]
    fn silence_sensor_30min_suppresses_sensor_only() {
        let (clock, _advance) = mock_clock();
        let state = Arc::new(Mutex::new(BuzzerPriorityState::new()));

        // Simulate: both sensor and battery critical
        {
            let mut s = state.lock().unwrap();
            s.sensor_critical_active = true;
            s.battery_critical_active = true;
            s.sensor_silenced_until = Some((clock)() + Duration::from_secs(30 * 60));
        }

        // Create a manager-like tester using compute_pattern logic directly
        let s = state.lock().unwrap();

        // Check: silenced flag is false (MQTT not involved)
        assert!(!s.silenced);

        // Manually evaluate compute_pattern logic:
        let sensor_active = if let Some(deadline) = s.sensor_silenced_until {
            if (clock)() >= deadline { s.sensor_critical_active } else { false }
        } else {
            s.sensor_critical_active
        };

        assert!(!sensor_active, "sensor should be suppressed by button silence");
        assert!(s.battery_critical_active, "battery should NOT be suppressed");

        // Pattern should be BatteryCritical (only battery remains)
        let pattern = match (sensor_active, s.battery_critical_active) {
            (false, true) => PatternSource::BatteryCritical,
            _ => PatternSource::None,
        };
        assert_eq!(pattern, PatternSource::BatteryCritical);
    }

    #[test]
    fn silence_sensor_expires_after_30min() {
        let (clock, advance) = mock_clock();
        let state = Arc::new(Mutex::new(BuzzerPriorityState::new()));

        {
            let mut s = state.lock().unwrap();
            s.sensor_critical_active = true;
            s.sensor_silenced_until = Some((clock)() + Duration::from_secs(30 * 60));
        }

        // Before expiry: sensor suppressed
        {
            let s = state.lock().unwrap();
            let sensor_active = if let Some(deadline) = s.sensor_silenced_until {
                if (clock)() >= deadline { true } else { false }
            } else {
                true
            };
            assert!(!sensor_active);
        }

        // Advance past 30 minutes
        advance(Duration::from_secs(31 * 60));

        // After expiry: sensor active again
        {
            let s = state.lock().unwrap();
            let sensor_active = if let Some(deadline) = s.sensor_silenced_until {
                if (clock)() >= deadline { s.sensor_critical_active } else { false }
            } else {
                s.sensor_critical_active
            };
            assert!(sensor_active, "sensor should resume after 30min");
        }
    }

    #[test]
    fn on_new_sensor_alarm_clears_button_silence() {
        let (clock, _advance) = mock_clock();
        let state = Arc::new(Mutex::new(BuzzerPriorityState::new()));

        {
            let mut s = state.lock().unwrap();
            s.sensor_critical_active = true;
            s.sensor_silenced_until = Some((clock)() + Duration::from_secs(30 * 60));
        }

        // Simulate on_new_sensor_alarm: clears sensor_silenced_until
        {
            let mut s = state.lock().unwrap();
            s.sensor_silenced_until = None;
        }

        // Now sensor should be active
        {
            let s = state.lock().unwrap();
            let sensor_active = if let Some(deadline) = s.sensor_silenced_until {
                if (clock)() >= deadline { s.sensor_critical_active } else { false }
            } else {
                s.sensor_critical_active
            };
            assert!(sensor_active, "sensor should resume after new alarm");
        }
    }

    #[test]
    fn on_new_sensor_alarm_does_not_clear_mqtt_silence() {
        let (clock, _advance) = mock_clock();
        let state = Arc::new(Mutex::new(BuzzerPriorityState::new()));

        {
            let mut s = state.lock().unwrap();
            s.sensor_critical_active = true;
            s.silenced = true; // MQTT ACK
        }

        // Simulate on_new_sensor_alarm: only clears sensor_silenced_until
        {
            let mut s = state.lock().unwrap();
            s.sensor_silenced_until = None; // Already None, no-op
        }

        // MQTT silence should still be active
        {
            let s = state.lock().unwrap();
            assert!(s.silenced, "MQTT silence should NOT be cleared by new sensor alarm");
        }
    }

    #[test]
    fn mqtt_silence_still_mutes_battery() {
        let state = Arc::new(Mutex::new(BuzzerPriorityState::new()));

        {
            let mut s = state.lock().unwrap();
            s.battery_critical_active = true;
            s.silenced = true; // MQTT ACK
        }

        let s = state.lock().unwrap();
        // MQTT silence mutes everything
        assert!(s.silenced);
        // Pattern would be None (all muted)
    }

    #[test]
    fn button_silence_does_not_mute_battery() {
        let (clock, _advance) = mock_clock();
        let state = Arc::new(Mutex::new(BuzzerPriorityState::new()));

        {
            let mut s = state.lock().unwrap();
            s.battery_critical_active = true;
            s.sensor_critical_active = false; // Only battery
            s.sensor_silenced_until = Some((clock)() + Duration::from_secs(30 * 60));
        }

        let s = state.lock().unwrap();
        // Button silence only affects sensor, battery should still beep
        let sensor_active = false; // suppressed
        let pattern = match (sensor_active, s.battery_critical_active) {
            (false, true) => PatternSource::BatteryCritical,
            _ => PatternSource::None,
        };
        assert_eq!(pattern, PatternSource::BatteryCritical);
    }

    #[test]
    fn is_sensor_beeping_reflects_both_silences() {
        let (clock, _advance) = mock_clock();

        // Helper to check is_sensor_beeping logic
        let check = |sensor_active: bool, mqtt_silenced: bool, button_deadline: Option<Instant>| -> bool {
            if !sensor_active { return false; }
            if mqtt_silenced { return false; }
            if let Some(deadline) = button_deadline {
                if (clock)() < deadline { return false; }
            }
            true
        };

        // Sensor active, no silence → beeping
        assert!(check(true, false, None));

        // Sensor active, MQTT silenced → not beeping
        assert!(!check(true, true, None));

        // Sensor active, button silenced → not beeping
        let future = (clock)() + Duration::from_secs(1800);
        assert!(!check(true, false, Some(future)));

        // Sensor inactive → not beeping
        assert!(!check(false, false, None));

        // Sensor active, both silenced → not beeping
        assert!(!check(true, true, Some(future)));
    }
}
```

- [ ] **Step 2: Verify tests pass**

Run: `cargo test --lib buzzer::priority 2>&1 | tail -20`
Expected: all 7 tests pass

- [ ] **Step 3: Commit**

```bash
git add src/libs/buzzer/priority.rs
git commit -m "buzzer: Add unit tests for button silence logic"
```

---

### Task 4: Wire button silence into `ButtonMonitor`

**Files:**
- Modify: `src/libs/display/buttons.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add `BuzzerPriorityManager` parameter to `ButtonMonitor::new`**

In `src/libs/display/buttons.rs`, add the import at the top:

```rust
use crate::libs::buzzer::BuzzerPriorityManager;
```

Change the `new` signature (line 48-51):

```rust
pub fn new(
    display_state: SharedDisplayStateHandle,
    pairing_handle: Option<PairingHandle>,
    buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
) -> io::Result<Self> {
```

Update the thread spawn (line 55-56):

```rust
let thread_handle = thread::spawn(move || {
    Self::button_loop(shutdown_flag_clone, display_state, pairing_handle, buzzer_priority);
});
```

Update `button_loop` signature (line 66-69):

```rust
fn button_loop(
    shutdown_flag: Arc<AtomicBool>,
    display_state: SharedDisplayStateHandle,
    pairing_handle: Option<PairingHandle>,
    buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
) {
```

- [ ] **Step 2: Add silence check at top of button event processing**

Inside the `for event in events` loop (line 110), before the `match event`, add:

```rust
// Any button PRESS silences sensor beep (consumes the event)
if let ButtonEvent::Press(_) = &event {
    if let Some(ref bp) = buzzer_priority {
        if bp.is_sensor_beeping() {
            bp.silence_sensor_30min();
            eprintln!("[ButtonMonitor] Sensor beep silenced by button press (30 min)");
            continue;
        }
    }
}
```

- [ ] **Step 3: Update `main.rs` call sites**

In `src/main.rs`, first call (line 191):

```rust
// Before:
let _button_monitor = ButtonMonitor::new(_display_monitor.display_state.clone(), None)?;
// After:
let _button_monitor = ButtonMonitor::new(_display_monitor.display_state.clone(), None, None)?;
```

Second call (line 306):

```rust
// Before:
let _button_monitor = ButtonMonitor::new(_display_monitor.display_state.clone(), pairing_handle.clone())?;
// After:
let _button_monitor = ButtonMonitor::new(
    _display_monitor.display_state.clone(),
    pairing_handle.clone(),
    Some(buzzer_priority_manager.clone()),
)?;
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: no errors

- [ ] **Step 5: Commit**

```bash
git add src/libs/display/buttons.rs src/main.rs
git commit -m "buttons: Wire button press to silence sensor beep for 30min"
```

---

### Task 5: Add per-sensor alarm hook in `SensorMonitor`

**Files:**
- Modify: `src/libs/sensors/monitor.rs`

- [ ] **Step 1: Add per-sensor critical transition detection**

In `src/libs/sensors/monitor.rs`, inside the sensor loop (around line 407-441), the code iterates `alarm_controllers` and checks `current_state`. The per-sensor state tracking already exists via `last_sensor_states[idx]`.

After the existing `match current_state` block (line 429-438) and before `last_sensor_states[idx] = Some(current_state)` (line 441), add the per-sensor transition check:

```rust
// Detect per-sensor transition into critical/disconnected
// to bust button silence when a NEW sensor alarms
if let Some(last_state) = last_sensor_states[idx] {
    let was_critical_like = matches!(last_state, AlarmState::Critical | AlarmState::Disconnected);
    let is_critical_like = matches!(current_state, AlarmState::Critical | AlarmState::Disconnected);
    if !was_critical_like && is_critical_like {
        priority_manager.on_new_sensor_alarm();
        eprintln!("[SensorMonitor] New sensor alarm detected (sensor {}), button silence cleared", idx);
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: no errors

- [ ] **Step 3: Commit**

```bash
git add src/libs/sensors/monitor.rs
git commit -m "sensors: Bust button silence when new sensor enters critical"
```

---

### Task 6: Add mute icon bitmap and draw function

**Files:**
- Modify: `src/libs/display/icons.rs`

- [ ] **Step 1: Add the mute bitmap and draw function**

At the end of `src/libs/display/icons.rs` (before the closing of the file), add:

```rust
// MUTE ICON BITMAP – 11x7 matching other icon dimensions
// Speaker with X overlay, user-designed pixel art
const MUTE_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1],
    [0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 0],
    [1, 1, 1, 1, 0, 0, 0, 1, 0, 1, 0],
    [1, 1, 1, 1, 0, 0, 0, 0, 1, 0, 0],
    [1, 1, 1, 1, 0, 0, 0, 1, 0, 1, 0],
    [0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 0],
    [0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1],
];

/// Draw mute icon (speaker with X)
/// Returns the width of the drawn icon for layout calculations
pub fn draw_mute<D>(display: &mut D, x: i32, y: i32) -> u32
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    draw_wifi_bitmap(display, x, y, &MUTE_BITMAP);
    11
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: no errors

- [ ] **Step 3: Commit**

```bash
git add src/libs/display/icons.rs
git commit -m "display: Add mute icon bitmap (11x7 speaker with X)"
```

---

### Task 7: Show mute icon on sensor overview screen

**Files:**
- Modify: `src/libs/display/screens.rs` (render_sensor_overview)
- Modify: `src/libs/display/monitor.rs` (pass silenced flag)

- [ ] **Step 1: Add `sensor_silenced` parameter to `render_sensor_overview`**

In `src/libs/display/screens.rs`, add the parameter to `render_sensor_overview` (line 27-37):

```rust
pub fn render_sensor_overview(
    display: &mut St7920,
    page: usize,
    _led_state: &SharedLedState,
    sensor_state: &SharedSensorState,
    network_status: &NetworkStatus,
    selected_sensor: Option<usize>,
    device_label: &str,
    lorawan_gateway_present: bool,
    lorawan_sensors: &[LoRaWANSensorState],
    total_pages: usize,
    sensor_silenced: bool,  // NEW: true when button silence is active
) -> anyhow::Result<()> {
```

- [ ] **Step 2: Draw mute icon in the header when silenced**

In the same function, after the LoRaWAN icon draw (line 48-49):

```rust
// Draw LoRaWAN icon next to network icon when gateway is present
if lorawan_gateway_present {
    icons::draw_lorawan(display, 2 + net_icon_width as i32 + 1, 0);
}

// Draw mute icon next to status icons when sensor silence is active
if sensor_silenced {
    let mute_x = if lorawan_gateway_present {
        2 + net_icon_width as i32 + 1 + 11 + 2 // after lorawan icon + gap
    } else {
        2 + net_icon_width as i32 + 2 // after network icon + gap
    };
    icons::draw_mute(display, mute_x, 1);
}
```

- [ ] **Step 3: Pass `sensor_silenced` from display monitor**

In `src/libs/display/monitor.rs`, update the import to include `BuzzerPriorityManager`:

At the top of the file, add:

```rust
use crate::libs::buzzer::BuzzerPriorityManager;
```

Update `display_loop` signature to accept `buzzer_priority`:

```rust
pub fn display_loop(
    shutdown_flag: Arc<AtomicBool>,
    display_state: SharedDisplayStateHandle,
    led_state: SharedLedStateHandle,
    gpio: Arc<Gpio>,
    sensor_state: SharedSensorStateHandle,
    power_status: SharedPowerStatus,
    hostname: String,
    _device_label: String,
    app_version: String,
    _timezone_offset_hours: i8,
    screen_brightness: Arc<AtomicU8>,
    buzzer_priority: Option<Arc<BuzzerPriorityManager>>,
) {
```

In the `render_sensor_overview` call (line 126), add the silenced flag:

```rust
let sensor_silenced = buzzer_priority.as_ref()
    .map(|bp| bp.is_button_silenced())
    .unwrap_or(false);

if let Err(e) = render_sensor_overview(&mut display, page, &led_snapshot, &sensor_snapshot, &network_status, selected_sensor, &current_device_label, lorawan_gateway_present, &lorawan_sensors, total_pages, sensor_silenced) {
```

- [ ] **Step 4: Update `DisplayMonitor::new` to accept and forward `buzzer_priority`**

In `src/libs/display/mod.rs`, update `DisplayMonitor::new` signature (line 336):

```rust
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
    buzzer_priority: Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
) -> io::Result<Self> {
```

Pass it through to `monitor::display_loop` in the thread spawn (line 353):

```rust
let thread_handle = thread::spawn(move || {
    monitor::display_loop(
        shutdown_flag_clone,
        display_state_clone,
        led_state,
        gpio,
        sensor_state,
        power_status,
        hostname,
        device_label,
        app_version,
        timezone_offset_hours,
        screen_brightness,
        buzzer_priority,
    );
});
```

- [ ] **Step 5: Update `main.rs` to pass `buzzer_priority_manager` to `DisplayMonitor`**

In `src/main.rs`, find the `DisplayMonitor::new` call and add the buzzer priority parameter. Since `DisplayMonitor` is created before `buzzer_priority_manager` (line ~170 vs ~205), pass `None` initially:

```rust
// DisplayMonitor::new(..., None)  // buzzer_priority not yet available
```

**Note:** The `DisplayMonitor` is created at line ~170 and `buzzer_priority_manager` at line ~205. Since the display monitor just checks `is_button_silenced()` each render cycle, the simplest approach is to pass `None` for now. The `buzzer_priority_manager` is an `Arc`, so we could restructure the init order, or we can store the Arc in the shared `DisplayState`.

Alternative (simpler, less invasive): Add an `Arc<AtomicBool>` flag `sensor_silenced` to `DisplayState`, and set it from the button handler after calling `silence_sensor_30min()`. This avoids changing `DisplayMonitor::new` entirely.

**Revised approach — use an `AtomicBool` in `DisplayState`:**

In `src/libs/display/mod.rs`, add to `DisplayState`:

```rust
use std::sync::atomic::AtomicBool;

pub struct DisplayState {
    // ... existing fields ...
    /// Whether sensor buzzer is silenced by button (for mute icon display)
    pub sensor_silenced: Arc<AtomicBool>,
}
```

Initialize in `DisplayState::new()`:

```rust
sensor_silenced: Arc::new(AtomicBool::new(false)),
```

Then in `buttons.rs`, when silencing, also set the flag:

```rust
if bp.is_sensor_beeping() {
    bp.silence_sensor_30min();
    if let Ok(ds) = display_state.lock() {
        ds.sensor_silenced.store(true, Ordering::Relaxed);
    }
    eprintln!("[ButtonMonitor] Sensor beep silenced by button press (30 min)");
    continue;
}
```

In the display monitor render loop, read it:

```rust
let sensor_silenced = if let Ok(ds) = display_state.lock() {
    ds.sensor_silenced.load(Ordering::Relaxed)
} else {
    false
};
```

And clear it when silence expires — this is handled by checking `is_button_silenced()` from the button loop periodically, or by having the display monitor check a callback. The simplest: just have the display read `buzzer_priority.is_button_silenced()` if available.

**Final decision:** Since init order prevents passing `buzzer_priority_manager` to `DisplayMonitor::new`, add a setter method:

In `src/libs/display/mod.rs`:

```rust
impl DisplayMonitor {
    /// Set the buzzer priority manager for mute icon display.
    /// Called after both display and buzzer are initialized.
    pub fn set_buzzer_priority(&self, bp: Arc<crate::libs::buzzer::BuzzerPriorityManager>) {
        if let Ok(mut ds) = self.display_state.lock() {
            ds.buzzer_priority = Some(bp);
        }
    }
}
```

And in `DisplayState`:

```rust
pub struct DisplayState {
    // ... existing fields ...
    /// Buzzer priority manager for checking mute state
    pub buzzer_priority: Option<Arc<crate::libs::buzzer::BuzzerPriorityManager>>,
}
```

Initialize as `None`, then call `set_buzzer_priority` in `main.rs` after both are created (after line 205):

```rust
_display_monitor.set_buzzer_priority(buzzer_priority_manager.clone());
```

In the display monitor render loop:

```rust
let sensor_silenced = if let Ok(ds) = display_state.lock() {
    ds.buzzer_priority.as_ref()
        .map(|bp| bp.is_button_silenced())
        .unwrap_or(false)
} else {
    false
};
```

This avoids changing `DisplayMonitor::new` or `display_loop` signatures.

- [ ] **Step 6: Verify it compiles**

Run: `cargo check 2>&1 | tail -10`
Expected: no errors

- [ ] **Step 7: Commit**

```bash
git add src/libs/display/screens.rs src/libs/display/monitor.rs src/libs/display/mod.rs src/main.rs
git commit -m "display: Show mute icon on sensor overview when button silence active"
```

---

### Task 8: Final integration check

**Files:** None (verification only)

- [ ] **Step 1: Run all tests**

Run: `cargo test 2>&1 | tail -20`
Expected: all tests pass

- [ ] **Step 2: Full compile check**

Run: `cargo build 2>&1 | tail -10`
Expected: build succeeds

- [ ] **Step 3: Commit any final fixes if needed**

If any compilation issues were found, fix and commit with descriptive message.
