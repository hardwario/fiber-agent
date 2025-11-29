
# Fiber Embedded System

> **A modular, Rust-based embedded platform for data acquisition, logging, and device control.**

Fiber is designed for robust operation on embedded hardware (e.g., STM32 MCUs), supporting a wide range of sensors, actuators, and user interfaces. Its modular architecture makes it easy to extend and adapt for new hardware or features.

---

## Features

- **Modular design:** Acquisition, alarms, drivers, sensors, UI, and more
- **Data logging:** Persistent storage in local databases
- **UI & Display:** Embedded display and user interface management
- **Network & System:** Network communication and system management
- **Extensible:** Add new hardware, drivers, or features easily

---

## Quick Start

```sh
cd fiber
cargo build --release
# Run or flash to your target device as appropriate
```

---

## Directory Structure

- `src/` — Main source code
  - `acquisition/` — Data acquisition logic
  - `alarms/` — Alarm engine and actions
  - `drivers/` — Hardware drivers (buzzer, STM, etc.)
  - `hal/` — Hardware abstraction layer (real and mocks)
  - `sensors/` — Sensor interfaces (onewire, simulated, etc.)
  - `app.rs`, `main.rs`, etc. — Application entry and core modules
- `Cargo.toml` — Rust project manifest
- `fiber_logs.db`, `fiber_readings.db` — Local databases
- `docs/architecture.svg` — System architecture diagram

---

## Architecture Overview

<p align="center">
  <img src="docs/architecture.svg" alt="Fiber Architecture" width="600"/>
</p>

**Legend:**
- Blue: Entry Point
- Green: App Logic
- Red: Modules (acquisition, alarms, drivers, sensors)
- Yellow: Storage
- Purple: UI/Display
- Cyan: System/Network

---

## Usage

1. **Build:** See Quick Start above.
2. **Configure:** Adjust settings in `config/` as needed for your hardware.
3. **Run/Flash:** Deploy to your embedded target.
4. **Extend:** Add new modules or drivers in `src/` as needed.

---

## Contributing

Contributions are welcome! Please open issues or pull requests for bug fixes, features, or documentation improvements.

---

## License

This project is licensed under the terms of the [LICENSE](../LICENSE).
echo ds2482 0x18 | sudo tee /sys/bus/i2c/devices/i2c-10/new_device