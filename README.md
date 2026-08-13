# FIBER Agent

Embedded Rust application for medical-grade temperature monitoring on Raspberry Pi Compute Module 4.

## Overview

FIBER monitors up to 8 DS18B20 temperature sensors with a 4-level alarm system, stores readings in an encrypted database with HMAC integrity, and communicates via MQTT with EU MDR 2017/745 compliance. It also bridges LoRaWAN wireless sensors (HARDWARIO STICKER) into the same monitoring pipeline.

## Hardware

| Component | Detail |
|-----------|--------|
| **Target** | Raspberry Pi CM4 |
| **Co-processor** | STM32 via UART — LED control, ADC (Vbat, VIN) |
| **Sensors** | 8x DS18B20 (1-Wire via DS2482 I2C bridge) |
| **Display** | ST7920 128x64 LCD (SPI) |
| **Buttons** | UP (GPIO23), DOWN (GPIO25), ENTER (GPIO24) |
| **Accelerometer** | LIS2DH12 (I2C, 0x19) — motion/tamper detection |
| **Buzzer** | GPIO17, PWM volume control (0-100%) |
| **Connectivity** | MQTT (TLS), BLE, LoRaWAN (ChirpStack) |

## Implemented Features

### Temperature Monitoring
- 8 independent 1-Wire sensor lines with per-sensor configurable thresholds
- 4-level alarm states: Normal, Warning, Critical, Disconnected
- Failure debouncing and warmup filtering to prevent false alarms
- Periodic aggregation (min/max/mean) with crash recovery via JSON persistence

### Alarm System
- Per-sensor LED indication: green (normal), yellow (warning), red (critical), blinking red (disconnected)
- Buzzer patterns with priority management (critical > disconnected > power alerts)
- Configurable alarm thresholds and LED/buzzer patterns per alarm level
- MQTT alarm event publishing (QoS 2)

### Power Management
- Battery monitoring via STM32 ADC (Vbat: 3100-3400mV range)
- AC power detection via VIN threshold (>12V = AC, <12V = battery)
- Power LED: green (AC), yellow (battery), blinking yellow (low battery)
- Buzzer alerts on AC disconnect, battery mode reminder, critical battery

### Display & UI
- **Sensor Overview** — 4 rows per page with alarm indicators; built-in layout lists the 8 probes then the stickers, or compose your own rows via [`display.custom_lines`](#configurable-overview-lines)
- **Sensor Detail** — thresholds, current temp, alarm state, location
- **LoRaWAN Sensors** — remote sensor temp, humidity, battery, signal
- **QR Code** — BLE pairing info (`ble://{hostname}/{mac}/{pin}`)
- **System Info** — version, hostname, network, battery, storage, uptime (paginated)
- **Pairing Mode** — 6-character code display
- Button navigation: UP/DOWN scroll, ENTER select, UP+DOWN 2s hold enters pairing

### MQTT Communication
- TLS with CA certificate validation (insecure_skip_verify blocked in production)
- Per-topic QoS (0 for telemetry, 1 for power, 2 for alarms)
- Last Will and Testament for offline detection
- Exponential backoff reconnection (1s to 60s)
- Rate-limited command subscription with audit logging
- Publishes: sensors, power, network status, system info, alarms, aggregations

### Secure Command Protocol (EU MDR)
- Ed25519 signature verification on all incoming commands
- Challenge-response protocol: request → challenge → confirm → apply
- Nonce-based replay attack prevention
- Authorized signers registry with per-signer permissions and expiration
- Audit trail of every command attempt (success and failure)

### Device Pairing
- Initiated by 2-second button hold (UP+DOWN)
- 6-character code shown on display, entered in viewer app
- AES-256-GCM encrypted key exchange using pairing code as seed
- Device CA signs admin Ed25519 certificate for the paired user

### Storage
- SQLCipher encrypted SQLite database (WAL mode)
- HMAC-SHA256 integrity on every sensor reading
- Immutable audit trail of all operations
- Auto-generated HMAC key on first boot
- Configurable max size (default 5GB) with FIFO auto-purge
- Non-blocking writes via background thread with 100ms flush interval

### LoRaWAN Gateway Bridge
- Auto-detects ChirpStack gateway hardware
- Subscribes to ChirpStack uplink events on local Mosquitto
- Parses HARDWARIO STICKER payloads: temp, humidity, voltage, illuminance, motion
- 4-level alarm system per wireless sensor (same as wired)
- Sensor timeout tracking (default 1 hour)
- Bridges data into FIBER MQTT topic hierarchy

### Additional
- **Accelerometer**: motion/tamper detection with configurable threshold and debounce
- **BLE**: advertising control via system service, PIN-based pairing
- **Network**: WiFi/Ethernet status detection, signal strength, IP reporting
- **Buzzer volume**: software PWM, MQTT-adjustable
- **Screen brightness**: PWM backlight control, MQTT-adjustable
- **Config applier**: atomic file writes with backup, validation, and rollback

## Architecture

The application spawns dedicated threads for each subsystem:

| Thread | Interval | Role |
|--------|----------|------|
| Sensor Monitor | 1s | Read DS18B20 sensors, evaluate alarms |
| Power Monitor | 1s | Read Vbat/VIN via STM32 ADC |
| LED Monitor | 50ms | Drive per-sensor and power LEDs |
| Display Monitor | 250ms | Render LCD screens |
| Button Monitor | event | Handle physical button input |
| Buzzer Controller | event | Priority-managed audio patterns |
| Storage Thread | 100ms flush | Non-blocking SQLCipher writes |
| MQTT Monitor | event | Publish telemetry, process commands |
| Pairing Monitor | event | Handle secure pairing protocol |
| LoRaWAN Monitor | 30s | Bridge ChirpStack to FIBER MQTT |
| Accelerometer | 100ms | Motion state machine |

All threads communicate via `Arc<Mutex<T>>` shared state and crossbeam channels.

## Building

### Prerequisites

- Rust toolchain (see [`rust-toolchain.toml`](rust-toolchain.toml))
- Cross-compilation linker: `aarch64-linux-gnu-gcc`
- arm64 dev headers:
  - `libssl-dev` (required by SQLCipher)
  - `libdbus-1-dev` (required by `bluer` for the in-app BLE GATT server)

```bash
sudo apt install gcc-aarch64-linux-gnu pkg-config
```

On Ubuntu, arm64 packages require the `ports.ubuntu.com` mirror. Add arm64 architecture and configure apt sources:

```bash
sudo dpkg --add-architecture arm64
```

Edit `/etc/apt/sources.list.d/ubuntu.sources` — add `Architectures: amd64 i386` to existing entries and append an arm64 block:

```
Types: deb
URIs: http://ports.ubuntu.com/ubuntu-ports/
Suites: noble noble-updates noble-backports noble-security
Components: main restricted universe multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
Architectures: arm64
```

Then install the cross-compilation dependencies:

```bash
sudo apt update
sudo apt install libssl-dev:arm64 libdbus-1-dev:arm64
```

### Build for target

`pkg-config` needs to be told it's allowed to cross-compile and where to find the arm64 `.pc` files:

```bash
PKG_CONFIG_ALLOW_CROSS=1 \
PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
cargo build --release --target aarch64-unknown-linux-gnu
```

### Dev-platform build

Disables cryptographic verification. Requires `/data/fiber/config/DEV_MODE_ENABLED` on the device.

```bash
PKG_CONFIG_ALLOW_CROSS=1 \
PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
cargo build --release --target aarch64-unknown-linux-gnu --features dev-platform
```

### Native build (for tests + lint only — does not run on hardware)

If you only want to compile the code and run unit tests without producing a target binary:

```bash
sudo apt install libssl-dev libdbus-1-dev pkg-config
cargo build --release      # native x86_64
cargo test --lib
```

The resulting binary will not work on the device (Pi GPIO, STM32 UART, BLE adapter not present), but this is the fastest way to validate code changes.

### Yocto build

When building via the `meta-fiber` recipe (`bitbake fiber`), bitbake provides the cross-sysroot automatically. The recipe declares `DEPENDS += "openssl dbus"`, so no manual setup is needed.

## Configuration

Runtime configuration is loaded from `/data/fiber/config/fiber.config.yaml`. See [`fiber.config.yaml`](fiber.config.yaml) for defaults. Sensor thresholds are in [`fiber.sensors.config.yaml`](fiber.sensors.config.yaml).

| File | Purpose |
|------|---------|
| `fiber.config.yaml` | Main config: power, MQTT, serial, storage, display, buzzer |
| `fiber.sensors.config.yaml` | Per-sensor thresholds, alarm patterns, names, locations |
| `authorized_signers.yaml` | EU MDR authorized public keys for remote commands |

### Provisioning ChirpStack credentials

The agent authenticates to the on-device ChirpStack gRPC-web API to register STICKERs,
add and remove external gateways, and poll gateway online state. Those credentials live in
`fiber.config.yaml`:

```yaml
lorawan:
  chirpstack:
    username: admin
    password: ""
```

The API address is **not** configurable. ChirpStack always runs on the same device as the
agent, so loopback is part of the deployment contract rather than a setting.

`password` ships empty on purpose — a password must never be committed. While it is empty
the LoRaWAN bridge refuses to start rather than falling back to ChirpStack's public factory
credentials. Populating it is a deployment step.

**How it gets populated.** `meta-fiber` does it, in
`recipes-core/fiber-firstboot/`: `fiber-firstboot.sh` reads the password from
`/data/chirpstack/admin-password` (0600, on the persistent partition) and injects it into
`/data/fiber/config/fiber.config.yaml` with `set-chirpstack-creds.py`. That runs in both the
ALWAYS block, so a RAUC install that restores the bundled config is re-injected on the next
boot, and the first-boot block, because on a true first boot the config does not exist yet
when ALWAYS runs. The script is idempotent and refuses to write unless the result has exactly
one `username:`/`password:` pair under `lorawan: → chirpstack:` — duplicate keys would make
the agent's parser reject the whole config.

This repo defines only the agent's side of that contract: read
`lorawan.chirpstack.{username,password}` from `/data/fiber/config/fiber.config.yaml` on each
API call, and refuse to start the bridge while the password is blank. Because the read happens
per call rather than once at startup, a password written or rotated after the agent is already
running takes effect without a restart.

**Known limitation — this does not yet make the credential secret.** The injected value *is*
ChirpStack v4's factory-seeded `admin` password; nothing in `meta-fiber` rotates the account
yet, so a different value would simply fail to authenticate. Reading it from a file instead of
hardcoding it means rotation only has to change what writes
`/data/chirpstack/admin-password`. Rotating also means updating the other consumer of the same
pair — `meta-fiber/recipes-connectivity/lorawan-setup/files/chirpstack-provision.py:419` calls
`login(host, port, "admin", "admin")` — or first-boot LoRaWAN provisioning breaks. Taking the
credential out of the agent's source is the first half; rotating the ChirpStack-side account
is a separate change.

**Diagnosing a missing credential.** All three symptoms have the same cause:

- The bridge does not come up: `[main] ERROR: LoRaWAN monitor did not start: ChirpStack API
  credentials not provisioned: ...`. Wired DS18B20 monitoring, MQTT and the LCD are
  unaffected.
- Every external gateway reports **offline**. `get_gateways_status` cannot express a login
  failure in its return type, so it logs and reports all gateways down.
- A sticker add still saves its sensor config but logs a ChirpStack provisioning failure —
  registration there is best-effort by design, so the add itself does not fail.

No log line prints the password. `ChirpStackApiConfig` implements `Debug` by hand so the
value cannot leak through a `{:?}` on it or on the `LoRaWANConfig` that contains it.

### Configurable overview lines

`display.custom_lines` replaces the built-in overview layout with rows you choose.
Absent or empty ⇒ built-in layout. Max 16 lines (4 pages, 4 rows each). Normally
written from the FIBER Viewer via the signed `set_display_lines` command, which
requires the dedicated `set_display_lines` permission — choosing which sensors the
panel lists is a separate capability from dimming it. Certificates issued before
this release do not carry it and must be reissued; a signer holding only
`set_screen_brightness` is rejected at the ConfigRequest stage.

| Key | Values |
|-----|--------|
| `source` | `ds18b20` or `sticker` |
| `line` | DS18B20 line index `0`–`7`. Required for `ds18b20`, forbidden for `sticker` |
| `dev_eui` | Sticker DevEUI, 16 hex chars. Required for `sticker`, forbidden for `ds18b20`. Never an ordinal — sticker numbering shifts when one is added or removed |
| `field` | `ds18b20`: `temperature`, `status`. `sticker`: any LoRaWAN registry field (`temperature`, `humidity`, `voltage`, `ext_temperature_1`/`_2`, `machine_probe_temperature_1`/`_2`, `machine_probe_humidity_1`/`_2`, `pressure`, `altitude`, `illuminance`, `*_count`) plus `rssi`, `snr`, `status` |
| `label` | Row label, ≤21 printable-ASCII chars. Defaults to the sensor's configured name |
| `format.units` | Append the unit (`°C`, `%`, `V`, `hPa`, `m`, `lx`, `dBm`, `dB`). Default `true` |
| `format.decimals` | `0`–`3`. Omit for the per-field default (1 temperatures, 0 humidity/pressure/counters, 2 battery voltage) |
| `format.status_char` | Show the `N`/`W`/`C`/`E` alarm character. Default `true` |

Battery is `field: voltage`, rendered in volts — the device has no mV→% curve for
a sticker's cell, so a percentage would be invented rather than measured.

Two rows may target the same sensor (e.g. temperature and battery). Rows referencing
a probe or sticker with no data show `--.-` plus `?`; a sticker that has stopped
reporting shows `--.-` and `E` rather than its last value, so a stale reading can't
be mistaken for a live one. Double-clicking ENTER switches the overview back to the
full sensor list, keeping every sensor's detail screen reachable regardless of this
config.

See [`fiber.config.yaml`](fiber.config.yaml) for a commented example.

## EU MDR 2017/745 Compliance

Classification: **Class IIa** | Software safety: **IEC 62304 Class B**

| Area | Status | What's implemented |
|------|--------|--------------------|
| Data integrity | **Complete** | HMAC-SHA256 on every sensor reading, SHA-256 hash-chain on audit logs |
| Encryption at rest | **Complete** | SQLCipher (AES-256) on all databases |
| Encryption in transit | **Complete** | TLS on MQTT (8883) and HTTPS (443) |
| Command authorization | **Complete** | Ed25519 signatures, challenge-response protocol, nonce replay prevention |
| Access control | **Complete** | JWT auth, 8 RBAC roles, bcrypt passwords, rate limiting, session IP binding |
| Audit trail | **Complete** | Tamper-evident hash-chain on firmware and viewer, API access logs, frontend action audit |
| Configuration tracking | **Complete** | All signed changes stored with signer ID, signature, nonce, verification status |
| Privilege separation | **Complete** | Dedicated `fiber` user, systemd hardening (NoNewPrivileges, ProtectSystem=strict) |
| Firewall | **Complete** | iptables default-deny, only SSH/MQTT-TLS/HTTPS/mDNS/DHCP allowed |
| OTA updates | **Complete** | RAUC A/B partitions, signed bundles, auto-rollback on failed boot |
| Data retention | **Complete** | 3-year retention with FIFO auto-purge at 90% capacity |
| GDPR (code-level) | **95%** | Consent, right to erasure, processing records. Missing: DPIA document |

### Cryptographic algorithms

| Algorithm | Usage |
|-----------|-------|
| Ed25519 | Command signing, certificate chain |
| AES-256-GCM | Pairing key encryption |
| HMAC-SHA256 | Sensor reading integrity |
| SHA-256 | Audit hash-chain |
| SQLCipher (AES-256) | Database encryption at rest |
| PBKDF2 (480k iterations) | Key derivation for pairing |
| bcrypt | Password hashing |

### Remaining gaps (documentation, not code)

ISO 14971 risk management, IEC 62304 software development plan, ISO 13485 QMS, clinical evaluation report, technical file, instructions for use (IFU), IEC 62366 usability evaluation, GDPR DPIA.

## License

See [LICENSE](LICENSE) for details.