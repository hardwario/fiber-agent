# Sensor Beep Silence via Physical Button — Design

**Date:** 2026-04-13
**Status:** Approved design, implementation pending

## Problem

When a sensor is in critical state, the buzzer beeps continuously. Today the buzzer can only be silenced via MQTT ACK (commit `8077790`, `BuzzerPriorityManager::silence()`). The user needs a physical-button way to silence the sensor beep locally, without needing network/MQTT.

## Decisions

| # | Question | Decision |
|---|----------|----------|
| 1 | Which button triggers silence? | **Any button** (UP/DOWN/ENTER). First press while sensor beeping is consumed as silence — does not navigate, does not start hold countdown. |
| 2 | Duration of silence? | **30 minutes**, then auto-resume if sensor still critical. |
| 3 | New sensor becomes critical during silence? | Silence is **cleared** — user must hear the new event. |
| 4 | Scope: sensor only, or also battery? | **Sensor only**. Battery critical alarm keeps beeping independently. |
| 5 | Visual feedback? | Discrete mute icon on sensor overview screen while button silence is active. No countdown. |
| 6 | MQTT ACK behavior? | **Unchanged** (indefinite, cleared on next sensor off→on transition). Button silence is a separate mechanism. |

## Architecture

Two independent silence mechanisms coexist in `BuzzerPriorityManager`:

| Source | State field | Duration | Scope | Cleared by |
|---|---|---|---|---|
| MQTT ACK (existing) | `silenced: bool` | Indefinite | Sensor + battery | Sensor off→on transition in `set_sensor_critical` |
| Button (new) | `sensor_silenced_until: Option<Instant>` | 30 min | Sensor only | Timer expiration **or** any new sensor entering critical |

Neither flag overrides the other. In `compute_pattern`:

1. If `silenced` (MQTT) → return `PatternSource::None` (mute everything).
2. Else, if `sensor_silenced_until` is `Some(deadline)` and `deadline > now` → suppress `SensorCritical` from the pattern choice. Battery-critical pattern is unaffected.
3. Else → current logic unchanged.

## Component Changes

### 1. `src/libs/buzzer/priority.rs`

**New state fields:**
```rust
struct BuzzerPriorityState {
    // ... existing fields ...
    /// Button silence deadline (sensor only). None when not silenced by button.
    sensor_silenced_until: Option<Instant>,
}
```

**New public methods:**
```rust
/// Silence sensor beep for 30 minutes. Does not affect battery pattern.
pub fn silence_sensor_30min(&self);

/// Called when a new sensor transitions into critical state.
/// Clears button silence (`sensor_silenced_until`) but does NOT clear MQTT `silenced` flag.
pub fn on_new_sensor_alarm(&self);

/// Returns true if the sensor-critical pattern is currently audible
/// (sensor critical AND not silenced by button AND not silenced by MQTT).
/// Used by the button handler to decide whether to consume the press.
pub fn is_sensor_beeping(&self) -> bool;
```

**`compute_pattern` changes:**
- Keep the existing `if state.silenced { return None }` short-circuit (MQTT ACK).
- Before the `match (sensor_critical_active, battery_critical_active)`, check `sensor_silenced_until`: if active and not expired, treat `sensor_critical_active` as `false` for pattern selection purposes. If expired, clear it (lazy cleanup).

**Clock abstraction:**
Introduce a minimal clock injection (e.g., `clock: Arc<dyn Fn() -> Instant + Send + Sync>` or a tiny `Clock` trait) so tests can advance time without sleeping. Default uses `Instant::now`. Refactor is local to `priority.rs`.

### 2. `src/libs/display/buttons.rs`

At the top of each `ButtonEvent::Press(_)` arm (before the state `match`), check:

```rust
if priority_manager.is_sensor_beeping() {
    priority_manager.silence_sensor_30min();
    continue; // consume the event — no navigation, no hold start
}
```

- `priority_manager` must be passed into `ButtonMonitor::new` (alongside `pairing_handle`).
- `Release` events need no change: since the press was consumed and state stayed `Idle`, there's no hold in flight.
- Intended side-effect: pressing while on `SelectionMode` / `ShowingDetail` / `ShowingQr` / etc. during a sensor beep only silences — the display state is preserved. Next press resumes normal navigation.

### 3. `src/libs/sensors/monitor.rs`

Around the per-sensor state comparison (~L436-441), after detecting `last_sensor_states[idx]` vs `current_state`, add a per-sensor transition check:

```rust
let was_critical_like = matches!(previous_state, Some(SensorState::Critical) | Some(SensorState::Disconnected));
let is_critical_like = matches!(current_state, SensorState::Critical | SensorState::Disconnected);

if !was_critical_like && is_critical_like {
    priority_manager.on_new_sensor_alarm();
}
```

(Field names adapt to actual `SensorState` enum variants at implementation time.)

This complements the existing aggregate `set_sensor_critical(true)` which only fires on aggregate transitions. Without this hook, silencing sensor A and then sensor B going critical would leave the silence in effect — violating decision #3.

### 4. `src/libs/display/icons.rs`

Add the mute bitmap provided by the user, same format as existing icons (11×7, `[[u8; 11]; 7]`):

```rust
const MUTE_BITMAP: [[u8; 11]; 7] = [
    [0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1],  // line 0
    [0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 0],  // line 1
    [1, 1, 1, 1, 0, 0, 0, 1, 0, 1, 0],  // line 2
    [1, 1, 1, 1, 0, 0, 0, 0, 1, 0, 0],  // line 3
    [1, 1, 1, 1, 0, 0, 0, 1, 0, 1, 0],  // line 4
    [0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 0],  // line 5
    [0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1],  // line 6
];

pub fn draw_mute<D>(display: &mut D, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    // Reuse draw_wifi_bitmap helper (generic row-run blitter)
    draw_wifi_bitmap(display, x, y, &MUTE_BITMAP);
}
```

### 5. Display rendering

In the sensor overview screen renderer (location TBD during implementation — likely in `src/libs/display/screens.rs`), add a call to `draw_mute` when `priority_manager.is_button_silenced()` returns true.

- Position: alongside the existing status icons in the header. Exact coordinates chosen at implementation time after inspecting the current layout.
- Icon only appears while `sensor_silenced_until` is active (button silence). Not shown for MQTT ACK silence (preserves current production behavior).

A small `is_button_silenced(&self) -> bool` accessor on `BuzzerPriorityManager` exposes this to the renderer.

## Tests (`src/libs/buzzer/priority.rs`)

Unit tests covering the pure logic (using the injectable clock):

1. `silence_sensor_30min_suppresses_sensor_only` — sensor + battery critical → apply button silence → pattern is `BatteryCritical`.
2. `silence_sensor_expires_after_30min` — apply silence → advance clock > 30 min → pattern returns to `SensorCritical`.
3. `on_new_sensor_alarm_clears_button_silence` — apply silence → call `on_new_sensor_alarm()` → pattern returns `SensorCritical`.
4. `on_new_sensor_alarm_does_not_clear_mqtt_silence` — apply MQTT `silence()` → call `on_new_sensor_alarm()` → pattern remains `None`.
5. `mqtt_silence_still_mutes_battery` — battery-only critical + MQTT `silence()` → `None` (regression guard for current behavior).
6. `button_silence_does_not_mute_battery` — battery-only critical + `silence_sensor_30min()` → `BatteryCritical`.
7. `is_sensor_beeping_reflects_both_silences` — returns true only when sensor-critical active AND neither silence is blocking it.

Button-handler and sensor-monitor integration validated manually on hardware — the logic gate is the unit tests above.

## Non-goals

- Changing MQTT ACK semantics (explicitly kept as-is per decision #6).
- Per-sensor silence UI (single global button silence for sensors).
- Configurable silence duration (30 min hardcoded for now).
- Publishing button silence events to MQTT.
