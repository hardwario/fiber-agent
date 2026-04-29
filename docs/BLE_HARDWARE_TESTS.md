# BLE In-App — Phase 2 Hardware Validation Checklist

**Purpose:** Confirm the in-app `BleMonitor` behaves identically to the legacy
Yocto `ble-fiber` service before Phase 3 deletes the recipe.

**Prerequisites:**
- Application built and flashed with `[ble] enabled: false` is the baseline.
- A development unit you can SSH into.
- A mobile device with nRF Connect (or equivalent BLE GATT explorer).

## Procedure

1. **Stop the legacy service:**
   ```bash
   systemctl stop ble-fiber
   systemctl status ble-fiber  # confirm 'inactive (dead)'
   ```

2. **Enable the in-app server:**
   Edit `/data/fiber/config/fiber.config.yaml`, set `ble.enabled: true`.

3. **Restart the application:**
   ```bash
   systemctl restart fiber_app
   journalctl -u fiber_app -f
   ```
   Look for: `[main] Starting BLE monitor (in-app GATT server)...` and
   `[BleMonitor] BLE advertising started`.

## Smoke Tests

- [ ] **QR code shows correct MAC** — boot the device, the LCD QR contains the
      adapter's real MAC (not `00:00:00:00:00:00`).
- [ ] **Mobile scan + auth** — open nRF Connect, find the FIBER-* device, write
      the PIN to `0xFB01`, read back `{"success":true,"message":"Authenticated"}`.
- [ ] **WiFi scan** — read `0xFB02`, see a JSON list of nearby networks.
- [ ] **WiFi connect** — write `{"ssid":"TestNet","password":"…"}` to `0xFB03`.
      Confirm `0xFB04` returns `connected: true` with an IP.
- [ ] **Terminal works** — subscribe to `0xFB06` notifications, write `ls -la /home`
      to `0xFB05`, see directory listing notifications.
- [ ] **Terminal blocklist** — write `rm -rf /` to `0xFB05`, confirm rejection
      message instead of execution.
- [ ] **LCD reflects BLE state**:
      - On client connect → `"BLE Connected"`
      - On WiFi connect attempt → `"Connecting WiFi..."`
      - On WiFi success → `"WiFi OK"` for ~3 s
      - On WiFi failure → `"WiFi Failed"` for ~5 s
- [ ] **Pairing-MQTT blocked while BLE connected** — with the BLE client still
      connected, hold UP+DOWN. Confirm pairing screen does **not** appear.
      Disconnect the BLE client; UP+DOWN should now work normally.
- [ ] **Cold reboot** — power-cycle the device. After boot, the QR shows the
      MAC immediately (no stale `00:00:00:00:00:00` fallback). The journal
      shows BleMonitor starting after `bluetooth.service`.

## Rollback

If anything fails:
1. Edit config back to `ble.enabled: false`.
2. `systemctl restart fiber_app`
3. `systemctl start ble-fiber`
4. File a bug with the journal output.
