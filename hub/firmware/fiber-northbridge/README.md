# fiber-northbridge (Zephyr)

Zephyr-based firmware for the **STM32WL5MOC** sub-GHz LoRa module — the
northbridge node of the fiber link, and since 0.3.0 the **TOWER gateway** of the P2P
transport (STICKER #470 P2). Contract: proximos-v2
`plan/control/radio/p2p_tower_gateway.md` §1–§2.3 (plus the amendments and the 0.3.0 flag
day recorded in `docs/firmware.md` → Contract status); the HDLC framing and the modem
commands it keeps are `plan/system/lora/design.md` §3–4.

It boots on the STM32WL5MOC, brings up the Sub-GHz radio and speaks the HDLC-framed binary
link over the dedicated USART1 to the RPi CM4's `ttyAMA3`. The Hub's central
(`control-radio`'s TOWER adapter) pushes the radio config, a gateway TX counter block and the
session key of every joined Node; the Northbridge then opens and replay-checks the Nodes'
TOWER frames itself, ACKs them `turnaround_ms` after RxDone, sends queued downlinks and
reports uplinks and outcomes as events. Nothing is kept in flash: after `EVT_BOOT` the
central rebuilds it all. See `docs/firmware.md` for the behaviour.

**Flag day (0.3.0):** there is no `proto_version` byte any more; the gateway image is
flashed together with the Hub software that speaks it. There is no rollback to the old P2P
modem image (proto 1: `RX_START`, host-side crypto): it is abandoned. The link is specified
by sticker-firmware `doc/plan/470 - Northbridge-Hub link protocol.md` (normative).

The interactive **LoRa shell** (`lora config/send/recv`) from earlier bench
testing (see `docs/sessions/`) has been removed from this build: it and the
P2P protocol engine would both drive the radio, and letting a human shell
command race the protocol's own state machine is exactly the kind of bug
that's invisible until it corrupts a live session. Check out the commit
before the P2P protocol landed if you need that shell-only build for bench
link testing.

---

## Repository layout

```
fiber-northbridge/
├── app/                     # Zephyr application
│   ├── CMakeLists.txt
│   ├── prj.conf
│   └── src/
│       ├── main.c           # boot, EVT_BOOT, watchdog, RTT shell (rf_diag, p2p_status)
│       ├── proto.h          # link constants (p2p_tower_gateway.md §2, design.md §3-4)
│       ├── hdlc.[ch]        # HDLC byte-stuffing framing + CRC16
│       ├── uart_link.[ch]   # USART1 ISR + RX/TX rings <-> HDLC <-> cmd_dispatch
│       ├── cmd_dispatch.[ch]# GET_INFO, SET_RADIO_CONFIG, RX_STOP, TX_SCHEDULE/CANCEL
│       ├── twr_cmd.[ch]     # TWR_* commands -> the core
│       ├── twr_link.[ch]    # link codec: command/event bodies, EVT_BOOT, GET_INFO
│       ├── twr_gwc.[ch]     # gateway core: receive, ACK, downlinks, counter block
│       ├── twr_peers.[ch], twr_dlq.[ch]  # peer table (256), downlink pool (32)
│       ├── twr_crypto.[ch], twr_frame.[ch], twr_airtime.[ch]  # AES/CCM, #470 frame, ToA
│       ├── twr_gateway.[ch] # the runtime on the SX126x (radio_ctl.h, twr_rt.h), `twr` shell
│       ├── diag.[ch]        # err_flags, counters, EVT_LOG
│       ├── radio_tune*.[ch] # 0.2.2 RX sensitivity tuning, `radio tune` shell
│       ├── nb_boot.[ch]     # ENTER_BOOTLOADER reboot, app_valid record for GET_INFO
│       └── twr_gw.c, twr_radio.[ch]  # TOWER P0 bench image only
├── boards/st/stm32wl5moc/   # Out-of-tree board definition
│   ├── board.yml, board.cmake, board.c   # 3-GPIO RF switch control
│   ├── Kconfig.stm32wl5moc, *_defconfig
│   ├── stm32wl5moc.dts, *-pinctrl.dtsi
│   └── stm32wl5moc.yaml     # twister metadata
├── nbboot/                  # 2 KB boot stub at page 0 (bare metal, built with the app)
│   └── src/nb_app_valid.[ch]# app_valid record check, shared with the app and tests
├── patches/lora/sx126x.c    # Patched Zephyr LoRa driver (see below)
├── tools/nb_package.py      # northbridge.bin/.json for the Hub, J-Link hex (+ tests)
├── scripts/shell.py         # Interactive serial console (runs on the RPi CM4)
├── tests/                   # native_sim ztest suites, see Tests
└── docs/
    ├── firmware.md          # What the current FW does (behaviour reference)
    ├── CHANGELOG.md         # Change log — record every behaviour/protocol change
    ├── problems.md          # Known issues — READ THIS (RSSI/weak signal)
    ├── plans/               # Board bring-up plan
    └── sessions/            # Session summaries
```

> **Note:** the Zephyr SDK, the `zephyr/` tree, and all fetched `modules/` are
> intentionally **not** included here — only project-owned files. See setup
> below to recreate the build environment.

---

## Hardware

- **Module**: STM32WL5MOC (STM32WL55xx SoC, integrated Sub-GHz LoRa radio)
- **Debug/flash**: J-Link → `STM32WL55JC`
- **P2P protocol link**: USART1 on PA9/PA10 → RPi CM4 `ttyAMA3` (binary
  HDLC protocol only — no console/shell on it, see Usage below)
- **Debug console/shell**: SEGGER RTT over the same J-Link/SWD connection
- **Clock**: HSI → PLL → 48 MHz
- **TCXO**: DIO3 at 1.7 V (integrated in module)
- **RF switch**: 3-GPIO front-end control on PC3 / PC4 / PC5 (see below)

---

## Prerequisites

- **Zephyr**: pinned in `west.yml` (development happened on `main` @ v4.3.99;
  the manifest pins the nearest release tag)
- **Zephyr SDK**: 1.0.0 (ARM toolchain)
- **west** + Python venv with Zephyr requirements
- **J-Link** tools for flashing

---

## Setup

This project carries its own west manifest (`west.yml`); the monorepo's
`firmware/` directory becomes the west workspace topdir (`zephyr/` and
`modules/` are fetched there and gitignored):

1. **Initialize the workspace** (from `firmware/`)

   ```bash
   west init -l fiber-northbridge
   west update
   west zephyr-export
   python3 -m venv .venv && source .venv/bin/activate
   pip install -r zephyr/scripts/requirements.txt
   ```

   If the workspace is already initialized for `fiber-southbridge`, switch
   manifests instead of re-initializing:

   ```bash
   west config manifest.path fiber-northbridge && west update
   ```

   The board definition under `boards/` is picked up automatically —
   `app/CMakeLists.txt` adds it to `BOARD_ROOT`.

2. **The LoRa driver patch** (critical — see next section) needs no manual
   step since 2026-09-28: `app/CMakeLists.txt` compiles
   `patches/lora/sx126x.c` in place of the vendored
   `zephyr/drivers/lora/loramac-node/sx126x.c`, so `west update` cannot undo
   it. Older branches still need the copy:

   ```bash
   cp fiber-northbridge/patches/lora/sx126x.c zephyr/drivers/lora/loramac-node/sx126x.c
   ```

## The LoRa driver patch (`patches/lora/sx126x.c`)

The STM32WL5MOC has a **3-GPIO** RF antenna switch, but Zephyr's `sx126x` driver
only models a **2-GPIO** switch and cannot express the required states. The
patched driver calls board-level weak hooks
(`board_rf_switch_tx() / _rx() / _off()`, implemented in
`boards/st/stm32wl5moc/board.c`) when `CONFIG_BOARD_STM32WL5MOC` is set, and
keeps default behavior for all other boards. Since 0.3.0 it always carries the gateway's
hooks (`NB_TWR_HOOKS`: RxDone/TxDone stamps, the TX gate right before `SET_TX`, standby on
the oscillator between ACK and downlink). Since 0.2.2 it calls the radio tuning hooks of
`patches/lora/nb_sx126x_hooks.h` (RX boosted gain, SMPS drive, PA config and ramp;
`docs/firmware.md` → Radio); until then it wrote `0x08AC = 0x97` on every wake, which the
driver overwrote before every RX. Since 0.2.0 it also exports `sx126x_last_dio1_ms`, the
uptime at which the radio interrupt's work item started, which the application uses to stamp
`EVT_RX`/`EVT_TX_DONE` (`docs/firmware.md` → Timestamps).

RF switch truth table, from ST's HAL for the STM32WL5Mxx module
(`stm32wlxx_hal.c`, `HAL_RADIO_SetSwitchConfig`; CTRL1=PC4, CTRL2=PC5, CTRL3=PC3):

| State | PC3 | PC4 | PC5 | Note                                  |
|-------|-----|-----|-----|---------------------------------------|
| OFF   |  0  |  0  |  0  |                                       |
| RX    |  1  |  1  |  1  |                                       |
| TX LP |  1  |  1  |  0  | RFO_LP, what the device tree selects  |
| TX HP |  1  |  0  |  1  | RFO_HP, unused                        |

Until 2026-09-26 the TX LP row was `0 0 1` (PC5 only), a state ST's table does not
have: the PA was left unconnected and only leakage reached the antenna, ~26 dB
below the reciprocal uplink. See `docs/problems.md` #2 and `docs/CHANGELOG.md`.

The patch is a full copy of the vendored driver file with these changes, and
`app/CMakeLists.txt` swaps it in for the vendored one in the `loramac-node`
library (the build stops if that file is not found, e.g. after a Zephyr bump
renamed it). It is still sensitive to the pinned Zephyr revision: re-diff it
against the new driver when `west.yml` moves (`docs/problems.md` #3).

---

## Build & flash

```bash
west build -b stm32wl5moc app
west flash --runner jlink -d build --dev-id <J-Link SN>
```

The app is linked at `0x0800_0800` behind the `nbboot` stub (`docs/firmware.md` → Flash
layout and nbboot). Every build also writes `build/nbboot/nbboot.bin`,
`build/zephyr/northbridge.{bin,json}` (the Hub's update image and manifest) and
`build/zephyr/northbridge_jlink.hex` (stub + app + `app_valid` record), and `west flash`
programs the latter: `zephyr.hex` alone has no record, so the stub would start the ROM
bootloader instead of the app.

Other images: `-DBENCH_RTT_BRIDGE=ON` carries the same link over RTT for J-Link-only rigs;
`-DBENCH_RTT_BRIDGE=ON -DTOWER_P0_GATEWAY=ON` is the standalone P0 bench image
(`docs/firmware.md` → Build variants).

---

## Usage — the gateway link

The host (RPi CM4, `/dev/ttyAMA3`, 115200 8N1) drives the Northbridge entirely over the
binary HDLC link — there is no interactive shell on it. The sequence (contract §3.4):

1. The Northbridge announces `EVT_BOOT` (`fw_ver · git_sha · reset_cause`) after every
   reset: the tables, the queue and the counter block are empty (state `BOOT`).
2. The host sends `GET_INFO` (188 B: identity, clock, state, radio config, counters),
   `TWR_TIME_SYNC` (the wall clock anchor for TimeAns in the ACK), `SET_RADIO_CONFIG`
   (→ `CONFIGURED`), a `TWR_CTR_BLOCK`, one `TWR_NODE_ADD` per Node with its `last_seen`, then `TWR_START{net_id, turnaround_ms, dl_gap_ms, ACK_ENABLED}`
   (→ `GATEWAY`, continuous receive) and re-pushes the pending downlinks
   (`TWR_QUEUE_PUSH`).
3. Uplinks come up as `EVT_TWR_UPLINK` (after the Northbridge's own ACK, which answers
   LinkCheckReq / TimeReq itself, #470 TOWER §13.5), JoinReqs and
   unknown sources as `EVT_RX`; downlink outcomes as `EVT_TWR_TX`; a running-out counter
   block as `EVT_TWR_CTR_LOW`.
4. The JoinAccept goes down with `TX_SCHEDULE` at an absolute Northbridge-clock `t_ms`;
   `EVT_TX_DONE` reports it. A radio change is `RX_STOP` → `SET_RADIO_CONFIG` →
   `TWR_START`.

`RX_START` (0x03) and `TWR_GET_STATS` (0x16) are retired and answer `UNSUPPORTED`.
`ENTER_BOOTLOADER` (0x07, body "NBBL") reboots into the STM32 ROM bootloader for an update.

Debug shell over RTT (`scripts/shell.py` is for the CM4 UART link, not RTT):

- `rf_diag` — print the RF switch GPIO states (PC3/PC4/PC5).
- `p2p_status` — diagnostics, link counters, radio config, a gateway summary.
- `p2p show|stats|peers [<start>]|queue|duty` — the gateway's state, counters, peer table
  (never a key), downlink queue and sliding-hour airtime; `p2p listen` starts the factory
  capture from BOOT.
- `radio tune show|set|apply|regs` — switch the RX-sensitivity tuning at run
  time and read back the radio registers (`docs/firmware.md` → Debug shell).

---

## Tests

Pure-logic unit tests run under `native_sim`, no hardware required: HDLC framing/CRC
(`hdlc_framing`), the command path in every state (`cmd_dispatch`, `proto_integration`), the
AES/CCM codec and the #470 frame KAT (`tower_codec`), the gateway core and the frozen
`EVT_BOOT` / `GET_INFO` layouts (`tower_gateway`), the radio tuning table and its register
operations (`radio_tune`), and `nbboot`'s record check (`nbboot_record`):

```bash
west twister -T tests -p native_sim/native/64 --force-platform -v --inline-logs
python3 tools/test_nb_package.py    # or: python3 -m pytest tools/
```

`twr_gateway.c` itself (the SX126x side: TX gating, stamps, RX re-arm) is exercised only
on hardware. `tests/hil_p2p/`'s Python HIL speaks proto 1 and is stale against 0.3.0.

---

## Known issues

See **`docs/problems.md`** for the full log. Headline open issue:

- **Weak signal / low RSSI** — ~7 dB below the RAK5146 in the same Hub
  (2026-09-26). 0.2.2 applies the RX boosted gain for the first time and ports
  the SMPS drive, PA and ramp settings, each switchable with `radio tune`; the RX
  boost A/B on the bench Hub is in `docs/problems.md` §1. 0.3.0 carries it unchanged.
