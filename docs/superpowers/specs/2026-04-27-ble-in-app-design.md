# BLE-in-app — Design Spec

**Date:** 2026-04-27
**Status:** Approved (pending implementation plan)
**Author:** brainstorming session with Enzo Frese

## Goal

Move the BLE GATT WiFi-provisioning service from the Yocto layer (`meta-fiber/recipes-connectivity/ble-fiber/`) into the main Rust application (`application/`), so the application becomes a complete, open-sourceable artifact. Today the BLE service is a separate closed-source binary; after this work, the open-source application repo contains all device behavior except OS-level system services.

## Background

### Current state

**Yocto layer** owns:
- `recipes-connectivity/ble-fiber/` — standalone Rust binary (~1010 lines), `LICENSE = "CLOSED"`, runs as a systemd service `ble-fiber.service` with `User=root`. Uses `bluer 0.17` (D-Bus → BlueZ) to serve a custom GATT service `0xFB00` with eight characteristics:

  | UUID | Name | Operations | Auth |
  |------|------|-----------|------|
  | `FB01` | Auth | Write, Read | No |
  | `FB02` | WiFi Scan | Read | Yes |
  | `FB03` | WiFi Connect | Write | Yes |
  | `FB04` | WiFi Status | Read, Notify | Yes |
  | `FB05` | Terminal TX | Write | Yes |
  | `FB06` | Terminal RX | Notify | Yes |
  | `FB07` | Device Info | Read | No |
  | `FB08` | WiFi Disconnect | Write | Yes |

- `recipes-connectivity/{bluez5, bluez5-agent, bluetooth-starter, bt-mac-address}` — BlueZ stack and MAC bring-up at boot.
- `recipes-connectivity/networkmanager-persistent` — NetworkManager (used by BLE WiFi characteristics via `nmcli`).

**Application** owns:
- `src/libs/ble/advertising.rs` — thin wrapper over `btmgmt` for start/stop advertising. Not a GATT server.
- Monitor pattern (`MqttMonitor`, `PairingMonitor`, `LoRaWANMonitor`, `SensorMonitor`, etc.) implemented via thread + crossbeam channels + Tokio runtime.
- `QrCodeGenerator` reads `/data/ble/mac.txt` and `/data/ble/pin.txt` (written by the BLE service) to render the device QR.

**IPC between processes today:** files in `/data/ble/` (`mac.txt`, `pin.txt`).

### Motivation

Make the application open-source-ready: today, Bluetooth is "missing" from the public application repo because it lives in a closed-license Yocto recipe. Unifying gives one auditable codebase.

### Non-goals

- Live medical data over BLE (rejected during brainstorming as too large a surface for the open-source release).
- Tokio runtime consolidation (each monitor keeps its own runtime, matching existing pattern).
- Replacing `root` privileges with capabilities-based privilege drop (future work).
- Replacing the plaintext PIN file with hardware-backed key storage (future work).

## Architecture

### Module layout

```
application/src/libs/ble/
├── mod.rs              # exports BleMonitor, BleHandle, BleEvent, BleCommand
├── advertising.rs      # (existing) — kept in Phase 1, deleted in Phase 3
├── gatt/
│   ├── mod.rs          # BleMonitor: spawns its own tokio runtime, runs GATT loop
│   ├── service.rs      # builds the GATT Application + Service FB00
│   ├── auth.rs         # FB01 — PIN auth, timing-safe compare
│   ├── wifi.rs         # FB02–FB04, FB08 — nmcli wrappers + parsers
│   ├── terminal.rs     # FB05/FB06 — `script`-based bash with PTY, command blocklist
│   ├── device_info.rs  # FB07 — hostname, version, uptime, mac
│   └── state.rs        # ServiceState (auth flag, PIN, hostname, mac, terminal_notifier)
└── config.rs           # parses `[ble]` section of fiber.config.yaml
```

The original `ble-fiber/files/src/main.rs` (1010 lines, single file) is split by characteristic so each file is ~150–200 lines. This makes auditing easier (important for the open-source release) and keeps the design-for-isolation principle: each characteristic is a unit with one purpose.

### Public API

```rust
pub struct BleHandle {
    command_tx: Sender<BleCommand>,
    event_rx: Arc<Mutex<Receiver<BleEvent>>>,
}

pub enum BleCommand {
    EnableAdvertising,
    DisableAdvertising,
    Shutdown,
}

pub enum BleEvent {
    ClientConnected { addr: String },
    ClientDisconnected,
    AuthSuccess,
    AuthFailed,
    WifiConnecting { ssid: String },
    WifiConnected { ssid: String, ip: String },
    WifiFailed { error: String },
}

pub struct BleMonitor { /* ... */ }

impl BleMonitor {
    pub fn new(config: BleConfig) -> Result<Self, Box<dyn std::error::Error>>;

    pub fn handle(&self) -> BleHandle;
}
```

`BleMonitor::new` takes only the BLE config. It does not receive `display_state`, `pairing_handle`, `sensor_state`, `storage_handle`, or `mqtt_handle`. Integration with display and pairing happens in a separate router thread (described in the next section), so the BLE module stays decoupled from rest-of-app semantics.

The runtime model matches `MqttMonitor`: a dedicated `tokio::runtime::Runtime` lives inside the monitor thread; outside callers interact only via crossbeam channels.

### Wire-up in `main.rs`

```rust
// After PairingMonitor, before PowerMonitor:
let (_ble_monitor, ble_handle) = if config.ble.enabled {
    match BleMonitor::new(config.ble.clone()) {
        Ok(m) => { let h = m.handle(); (Some(m), Some(h)) }
        Err(e) => { eprintln!("[main] Warning: BLE monitor failed: {}", e); (None, None) }
    }
} else {
    (None, None)
};

// BLE event router thread — only spawned if BLE is active.
// Drains ble_handle.event_rx and dispatches to display_state and pairing_handle.
let _ble_router = ble_handle.as_ref().map(|h| {
    spawn_ble_event_router(
        h.clone(),
        _display_monitor.display_state.clone(),
        pairing_handle.clone(),
    )
});
```

## Configuration

New section in `fiber.config.yaml`:

```yaml
ble:
  enabled: false             # Phase 1 default; flipped to true in Phase 3
  pin_file: /data/ble/pin.txt
  default_pin: "123456"
  enable_terminal: true      # FB05/FB06 — can be disabled in stricter deployments
  advertising_name: null     # null → uses hostname
```

### Persistence

- **`/data/ble/pin.txt` — kept.** It is the user-facing way to change the PIN without a rebuild. Created with mode `0600` and `default_pin` if absent.
- **`/data/ble/mac.txt` — eliminated as IPC.** After Phase 3 the MAC is read in-process via `bluer::Adapter::address()` and surfaced through a shared state handle the `QrCodeGenerator` consults. During Phase 1 the file remains the source of truth (Yocto still writes it). During Phase 2 the application reads from `BleMonitor` first and falls back to the file.

## Integration with display and pairing

### Display feedback

Four new states added to `display_state.rs`. **All visible strings are English.**

| Method | Trigger event | LCD content |
|---|---|---|
| `show_ble_connected(addr)` | `BleEvent::ClientConnected` | Bluetooth icon + `"BLE Connected"` + truncated address |
| `show_ble_provisioning(ssid)` | `BleEvent::WifiConnecting` | `"Connecting WiFi..."` + SSID |
| `show_ble_wifi_ok(ssid, ip)` | `BleEvent::WifiConnected` | `"WiFi OK"` + IP, dwell 3 s, then `show_sensor_overview()` |
| `show_ble_wifi_fail(err)` | `BleEvent::WifiFailed` | `"WiFi Failed"` + truncated error, dwell 5 s |

A lightweight "BLE event router" thread (`spawn_ble_event_router`) is started in `main.rs` after `BleMonitor::new()`. It drains `BleHandle.event_rx` and dispatches to `display_state` and `pairing_handle`. This is the only place where BLE events touch the rest of the application — keeping the integration explicit and reviewable.

### Pairing-MQTT coordination

`PairingStateMachine` gains a `ble_active: bool` field.

- On `BleEvent::ClientConnected`: router calls `pairing_handle.set_ble_active(true)`. If pairing-via-MQTT is in progress, it is cancelled. Display returns to sensor overview before the BLE state takes the screen.
- On `BleEvent::ClientDisconnected`: router calls `pairing_handle.set_ble_active(false)`.
- `ButtonMonitor` checks `state.ble_active` before reacting to UP+DOWN; if true, the combo is ignored.

This avoids dual-pairing UI conflicts on a single LCD.

### Explicitly NOT integrated

`BleMonitor::new` does **not** receive `sensor_state`, `storage_handle`, or `mqtt_handle`. The BLE surface stays out of the medical data path.

## Phased migration

### Phase 1 — Add `BleMonitor` to the application (PR #1)

1. Add to `Cargo.toml`: `bluer = "0.17"` with `features = ["full"]`, `tokio-util = "0.7"`, `regex = "1.10"`.
2. Create `application/src/libs/ble/gatt/` and `config.rs` by porting `meta-fiber/recipes-connectivity/ble-fiber/files/src/main.rs`, split per the module layout above.
3. Add `[ble]` section to `fiber.config.yaml` with `enabled: false`.
4. Wire `BleMonitor` in `main.rs` conditioned on `config.ble.enabled`.
5. Update `fiber_app.service` (in Yocto) with `User=root`, `After=bluetooth.service NetworkManager.service dbus.service`, `Requires=bluetooth.service NetworkManager.service dbus.service`.

**Exit criterion:** `cargo test` and `cargo build` pass; with `enabled: false`, the application runs identically to today (Yocto BLE service still owns BLE).

### Phase 2 — Hardware validation (no PR; manual hardware run by Enzo)

On a test unit:
1. `systemctl stop ble-fiber`
2. Edit `/data/fiber/config/fiber.config.yaml` → `ble.enabled: true`
3. `systemctl restart fiber_app`

Smoke checklist (in `docs/BLE_HARDWARE_TESTS.md`):

- [ ] QR code shows correct MAC (no `00:00:00:00:00:00` fallback)
- [ ] Mobile app scans, authenticates with PIN, reads WiFi scan
- [ ] WiFi connect via BLE succeeds, IP appears
- [ ] Terminal-over-BLE responds to `ls`, `pwd`, `cat /etc/hostname`
- [ ] LCD shows `"BLE Connected"`, `"Connecting WiFi..."`, `"WiFi OK"`
- [ ] Pairing-via-MQTT blocked while a BLE client is connected (UP+DOWN no-op)
- [ ] Cold reboot: everything starts in the right order, MAC correct on first read

**Exit criterion:** all smoke tests pass on at least one real device.

### Phase 3 — Remove from Yocto (PR #3)

In `meta-fiber/`:
- Delete `recipes-connectivity/ble-fiber/` entirely.
- Remove `ble-fiber` from image manifests (search `recipes-core/images/*.bb`, `conf/`).

In `application/`:
- Flip `ble.enabled` default to `true`.
- Delete `application/src/libs/ble/advertising.rs` (the `btmgmt` wrapper, now obsolete).
- Update `docs/BLE_PROVISIONING_FLOW.md` to reference `application/src/libs/ble/` instead of the removed recipe.

Top-level: ensure no remaining `LICENSE = "CLOSED"` references describe code now living in the application.

**Exit criterion:** Yocto image builds without `ble-fiber`; the open-source application repo contains all of BLE.

## Testing

### Unit tests (per-PR, in CI)

- `gatt/wifi.rs` — pure parsers for `nmcli -t -f SSID,SIGNAL,SECURITY dev wifi list` and `nmcli -t -f DEVICE,STATE,CONNECTION dev status`.
- `gatt/state.rs` — auth state machine: not-authenticated → authenticated → disconnect resets.
- `gatt/auth.rs` — PIN comparison uses timing-safe equality (`subtle` crate or `constant_time_eq`).
- `config.rs` — `[ble]` section parsing with defaults.
- `gatt/terminal.rs` — command blocklist rejects `rm -rf /`, `mkfs`, `dd if=` before spawn.

### Integration tests (no hardware, in CI)

- `nmcli` mocked via a temp-dir `PATH` injection: a bash script that emits canned `nmcli -t` output. Validates `wifi.rs` end-to-end without a real adapter.
- `bluer`/D-Bus is **not** mocked. GATT runtime is exercised only on hardware.

### Hardware tests (Phase 2, manual)

- Checklist file `docs/BLE_HARDWARE_TESTS.md` (created during Phase 1 PR).
- Run by Enzo before Phase 3.

### Security-relevant tests (open-source signal)

- PIN file is created with mode `0600`.
- Terminal blocklist rejects `rm -rf /`, `mkfs`, `dd if=`.
- `BleEvent::ClientDisconnected` resets `authenticated` to false.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| `bluer`/D-Bus crash takes down the medical app | `BleMonitor` thread runs with `catch_unwind` and auto-restarts (mirrors `MqttMonitor`). Failure of BLE must not stop `SensorMonitor`. |
| Application now runs as `root` | Documented in this spec. Future work: `CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW`. |
| Boot race during Phase 1 (both services active) | `BleMonitor::new()` checks `systemctl is-active ble-fiber`; if active, logs warning and refuses to start. |
| BlueZ adapter contention during Phase 2 validation | Phase 2 doc requires `systemctl stop ble-fiber` before flipping `enabled: true`. |
| Open-source exposes default PIN `123456` | Already public in `BLE_PROVISIONING_FLOW.md`. README addition recommends changing PIN in production. |
| Terminal-over-BLE is now public attack surface | Documented as a field-debug feature; `ble.enable_terminal: false` disables it. |

## What stays in Yocto

- `bluez5`, `bluez5-agent` — BlueZ stack + D-Bus daemon.
- `bt-mac-address` — initial BT MAC bring-up at boot.
- `bluetooth-starter` — HCI/rfkill init at boot.
- `networkmanager-persistent` — `nmcli` runtime.
- Kernel Bluetooth modules.
- `fiber_app.service` — updated to run as root and depend on `bluetooth.service`, `NetworkManager.service`, `dbus.service`.

## What leaves Yocto

- The entire `recipes-connectivity/ble-fiber/` directory (`.bb`, `Cargo.toml`, `src/main.rs`, `ble-fiber.service`).
- `/data/ble/mac.txt` as an IPC mechanism (the file path itself may still be created, but no longer the source of truth).

## Out of scope

- Refactor to a single shared Tokio runtime for all monitors.
- Privilege drop (capabilities instead of root).
- New characteristic FB09 "Live Sensor".
- Replacing the plaintext PIN file with a stronger secret store.

## References

- Source: `/home/frese/fiber/meta-fiber/recipes-connectivity/ble-fiber/files/src/main.rs` (1010 lines)
- Protocol doc: `application/docs/BLE_PROVISIONING_FLOW.md`
- Monitor pattern reference: `application/src/libs/mqtt/`, `application/src/libs/pairing/mod.rs`
- BlueZ stack (kept in Yocto): `meta-fiber/recipes-connectivity/{bluez5,bluez5-agent,bluetooth-starter,bt-mac-address}`
