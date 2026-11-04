# Flashing the southbridge with J-Link (SEGGER)

The Zephyr build can also be flashed with `west flash -d build-south --runner jlink`;
the options below drive the J-Link tools directly.

Target: **STM32C031C6Tx** (Cortex-M0+, 32 KB Flash, 12 KB RAM)

## Prerequisites

- SEGGER J-Link debugger (any model supporting SWD)
- [J-Link Software and Documentation Pack](https://www.segger.com/downloads/jlink/) installed
- Firmware ELF: `build-south/zephyr/zephyr.elf` — built from `firmware/` with
  `west build -b fiber_southbridge -d build-south fiber-southbridge/app`
  (see [README.md](README.md)), or the `firmware:south-build` CI job artifact.
  Paths below are relative to `firmware/`.

## Wiring

Connect the J-Link to the target's SWD header:

| J-Link Pin | Target Pin |
|------------|------------|
| VTref      | 3.3V       |
| GND        | GND        |
| SWDIO      | SWDIO      |
| SWCLK      | SWCLK      |
| nRESET     | NRST       |

## Option A: Flash using J-Link Commander (interactive)

```bash
JLinkExe
```

Then enter the following commands:

```
connect
Device STM32C031C6
SI SWD
Speed 4000
erase
loadfile build-south/zephyr/zephyr.elf
r
g
q
```

## Option B: Flash using J-Link Commander (scripted)

Create a file `flash.jlink`:

```
device STM32C031C6
si SWD
speed 4000
erase
loadfile build-south/zephyr/zephyr.elf
r
g
q
```

Run it:

```bash
JLinkExe -CommandFile flash.jlink
```

## Option C: Flash using JFlash Lite (GUI)

1. Open **JFlash Lite**
2. Set device to `STM32C031C6`
3. Set interface to `SWD`, speed `4000 kHz`
4. Select `build-south/zephyr/zephyr.elf`
5. Click **Program Device**

## Option D: Flash using J-Link GDB Server

Start the GDB server:

```bash
JLinkGDBServer -device STM32C031C6 -if SWD -speed 4000
```

In another terminal, connect with `arm-none-eabi-gdb`:

```bash
arm-none-eabi-gdb build-south/zephyr/zephyr.elf
```

Inside GDB:

```
target remote :2331
monitor reset
load
monitor reset
continue
```

## Troubleshooting

| Issue | Fix |
|-------|-----|
| "Could not find core" | Check SWD wiring and VTref |
| "Failed to connect" | Ensure NRST is connected; try `connect under reset` in J-Link Commander |
| Device is locked | Run `unlock kinetis` or use J-Link Commander: `erase` with connect under reset |
| Wrong device selected | Verify with `JLinkExe` → `connect` → let it auto-detect |

To connect under reset (useful if the MCU is in a low-power state):

```
JLinkExe
connect
Device STM32C031C6
SI SWD
Speed 4000
```

When prompted for "Reset type", select **1 = Reset pin** or type:

```
r
h
```

Then proceed with `erase` and `loadfile`.
