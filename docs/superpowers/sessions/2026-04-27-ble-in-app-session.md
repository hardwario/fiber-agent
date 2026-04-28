# Session Log — BLE in-app (Phase 1)

**Date:** 2026-04-27
**Branch:** `feat/ble-in-app` (across 3 repos)
**Outcome:** Phase 1 complete and validated on Raspberry Pi OS hardware. WiFi provisioning over BLE works end-to-end including the disconnect→reconnect→switch-network case.

## What was built

Moved the BLE GATT WiFi-provisioning service from `meta-fiber/recipes-connectivity/ble-fiber/` (closed-license Yocto recipe) into the main Rust application at `application/src/libs/ble/`. Phase 1 delivers a feature-flagged in-app `BleMonitor` that runs alongside the legacy Yocto service; Phase 3 will delete the recipe.

### Architecture

- New module `application/src/libs/ble/`:
  - `gatt/auth.rs` — FB01 PIN auth (constant-time compare via `subtle`)
  - `gatt/wifi.rs` — FB02 scan, FB03 connect, FB04 status, FB08 disconnect (pure parsers + nmcli wrappers)
  - `gatt/terminal.rs` — FB05/FB06 PTY shell with command blocklist
  - `gatt/device_info.rs` — FB07 hostname/version/MAC/uptime
  - `gatt/state.rs` — `ServiceState` + PIN/MAC/hostname helpers
  - `gatt/service.rs` — assembles the full GATT `Application`
  - `gatt/mod.rs` — `BleMonitor` + `BleHandle` + Tokio runtime + adapter event loop
  - `event_router.rs` — single integration point bridging `BleEvent` → display state + pairing handle
  - `config.rs` — `BleConfig` (`[ble]` YAML section)
- `BleMonitor` follows the existing monitor pattern (`MqttMonitor`, `PairingMonitor`): dedicated thread, dedicated multi-thread Tokio runtime, crossbeam channels.
- `BleMonitor::new` takes only `BleConfig` — fully decoupled from display/pairing/sensor/mqtt; the router is the only place those touch.
- Display gained 4 new screens (`BLE Connected`, `Connecting WiFi...`, `WiFi OK`, `WiFi Failed`) with auto-revert dwell timers; all strings English.
- Pairing state machine gained a `ble_active` flag; button monitor suppresses MQTT pairing combo while a BLE client is connected.

### Process

1. Brainstorming → spec at `docs/superpowers/specs/2026-04-27-ble-in-app-design.md`.
2. Plan at `docs/superpowers/plans/2026-04-27-ble-in-app.md` with 22 bite-sized tasks.
3. Subagent-driven execution with two-stage review (spec compliance + code quality) per task.

## Repositories touched

### `application` (`feat/ble-in-app`)
25 commits:
- 22 from the implementation plan (port + integration + docs)
- 1 README update — `libdbus-1-dev:arm64` + `PKG_CONFIG_ALLOW_CROSS=1` cross-compile recipe
- 2 hardware-validation fixes (see below)
- 1 cleanup of duplicate WiFi log lines

### `meta-fiber` (`feat/ble-in-app`)
2 commits:
- `0f8ec1a` — `fiber.service` now runs as `User=root` (BLE needs raw HCI), depends on `bluetooth.service`/`NetworkManager.service`/`dbus.service`, ReadWritePaths includes `/data/ble`.
- `1649c8d` — recipe `fiber.bb` adds `DEPENDS += "openssl dbus"` (build) and `RDEPENDS += bluez5 networkmanager` (runtime) so Yocto images carry the BLE prerequisites.

### `dev-platform` (`master`)
1 commit (`a8cd413`):
- `docs/manual-setup.md` — new section 7 covering Pi OS BLE setup (bluez+NM+rfkill packages, `bluetoothd --experimental` drop-in, persistent `bt-unblock.service`, NM/wlan0 verification, `ble.enabled: true` flip). Sections 7–19 renumbered to 8–20.

## Hardware validation findings

Tested on Raspberry Pi OS Bookworm (Pi CM4). Three real bugs surfaced and were fixed:

### 1. `--experimental` missing on Pi OS bluetoothd

`bluer 0.17` requires the experimental D-Bus interfaces. Pi OS's stock bluetoothd does not enable them. Symptom: `[BleMonitor] FATAL: GATT server returned error: Failed` immediately on startup. Fix is documented in dev-platform manual-setup section 7.2 (systemd drop-in adding `-E` to ExecStart).

### 2. Bluetooth radio rfkilled on boot

Pi OS often boots with `Soft blocked: yes`. `bluetoothctl show` shows `PowerState: off-blocked` and bluetoothd logs `Failed to set mode: Failed (0x03)`. Fix: `rfkill unblock bluetooth` + persistent `bt-unblock.service`. Documented in section 7.3.

### 3. WiFi connect failed after disconnect with `key-mgmt: property is missing`

Two separate bugs ported faithfully from the original ble-fiber recipe:

- `nmcli dev wifi connect` infers security from scan cache, which goes stale right after a disconnect from the same SSID.
- `nmcli dev disconnect` doesn't remove the saved profile, so NetworkManager autoconnects to the previous network and races with the new connect.

Fixed in two commits on `application/feat/ble-in-app`:
- `cef5e72` — `disconnect_wifi` now captures the active connection name first and deletes the profile after disconnect; `connect_wifi` deletes any matching stale profile before connecting; both functions log via `eprintln!` for journalctl visibility.
- `9c0f5db` — `connect_wifi` no longer uses `nmcli dev wifi connect`. It builds the profile explicitly: `connection add` → `connection modify wifi-sec.key-mgmt wpa-psk wifi-sec.psk` → `connection up`. This bypasses the cache-inference path entirely. Open networks (empty password) skip the modify step. Partial profiles are deleted on any failure.

After these fixes, all hardware test cases pass: connect, disconnect, reconnect to same network, switch to a different network.

### 4. Duplicate WiFi log lines

`service.rs` had `eprintln!` calls left over from the Task 11 port that duplicated everything `wifi.rs` now logs. Cleaned up in `7ad9c69` — scan logs kept (no equivalent in wifi.rs), connect/disconnect logs removed.

## Decisions worth remembering

- **`User=root` chosen over capabilities** for the systemd unit (Option A in the brainstorm). Trade-off documented; `CapabilityBoundingSet` left as future work.
- **Terminal-over-BLE kept** despite the open-source security concern (user choice; gated by `enable_terminal: true` in YAML for stricter deployments).
- **Acoplamento médio** with the rest of the app — display + pairing only. No sensor/MQTT/storage exposure over BLE.
- **`/data/ble/pin.txt` kept as user-editable**; `/data/ble/mac.txt` retained for backward compat in Phase 1, planned to become internal in Phase 3.
- **rustfmt is not enforced** in this codebase — kept the existing casual style.

## Next steps (not done today)

- **Phase 2 hardware validation checklist** at `application/docs/BLE_HARDWARE_TESTS.md` — most items already validated above; remaining checks (cold reboot, mobile app filter behavior, MAC in QR on first boot) can be ticked in a follow-up session.
- **Phase 3 PR** — delete `meta-fiber/recipes-connectivity/ble-fiber/`, flip `ble.enabled` default to `true`, delete `application/src/libs/ble/advertising.rs`, update `BLE_PROVISIONING_FLOW.md`.
- **Push and open PRs** — branches are local. User chose to defer push.
- **Privilege drop follow-up** — replace `User=root` with `CAP_NET_ADMIN`/`CAP_NET_RAW` capabilities.

## Final commit list

### application/feat/ble-in-app

```
7ad9c69 chore(ble): drop duplicate WiFi log lines from service.rs
9c0f5db fix(ble): use explicit nmcli profile flow for WiFi connect
cef5e72 fix(ble): cleanup stale NM profiles around connect/disconnect
b99d07b docs(readme): document libdbus-1-dev:arm64 + PKG_CONFIG cross setup
cb2efeb docs(ble): add Phase 2 hardware validation checklist
883886a feat(config): add [ble] section to fiber.config.yaml
27f3726 feat(main): wire BleMonitor and event router behind config flag
67890fc feat(ble): add event router bridging BleEvents to display+pairing
4bbc33c feat(display): add show_ble_* states for BLE provisioning UX
3fd6a62 feat(buttons): suppress MQTT pairing combo while BLE client connected
9ff6ea2 feat(pairing): expose set_ble_active on PairingHandle
f66c164 feat(pairing): add ble_active flag to coordinate with BLE provisioning
5b3b0e4 feat(ble): add BleMonitor + BleHandle thread/runtime wrapper
e0dd1c2 feat(ble): port GATT application assembly
4da9688 feat(ble): port ServiceState and PIN/hostname helpers
5d1a01a feat(ble): port terminal characteristic with explicit command policy
3f97785 feat(ble): port auth characteristic with timing-safe PIN compare
5cb7098 feat(ble): port device_info characteristic (FB07)
a674216 feat(ble): port nmcli action functions (scan/connect/status/disconnect)
d3def96 feat(ble): port nmcli wifi parsers as pure functions
767d798 feat(config): expose BleConfig as Config.ble
8f4f2f9 feat(ble): add BleConfig with serde defaults
e09f132 feat(ble): add bluer and supporting deps for in-app GATT server
c7eea9b chore: track superpowers specs and plans in git
```

### meta-fiber/feat/ble-in-app

```
1649c8d fiber-app: add dbus build-dep and bluez5/networkmanager runtime deps
0f8ec1a fiber-app: add bluetooth/dbus deps and run as root
```

### dev-platform/master

```
a8cd413 manual-setup: add Bluetooth (BLE provisioning) section
```
