# Zephyr in-tree patches

These files are **modified copies of vendored Zephyr sources**. They are kept
here because they live inside the `zephyr/` tree in a west workspace and are
therefore not part of this repo — and would be lost on `west update`.

## `lora/sx126x.c`

Overlays `zephyr/drivers/lora/loramac-node/sx126x.c` (at the pinned
main-branch revision; the older v4.3.0 tag spelled it `loramac_node`).

Changes on top of upstream:

1. **3-GPIO RF switch** — under `CONFIG_BOARD_STM32WL5MOC`, calls board-level
   weak hooks `board_rf_switch_tx() / _rx() / _off()` (implemented in
   `boards/st/stm32wl5moc/board.c`). Other boards keep default behavior.
2. **Radio tuning hooks** (0.2.2, `nb_sx126x_hooks.h`), same boards:
   `board_sx126x_filter_reg()` on every single-register write (the RX boosted
   gain replaces the `0x94` `SX126xSetRx()` writes), `board_sx126x_enter_mode()`
   before `SET_RX`/`SET_TX` (SMPS drive), `board_sx126x_set_rf_tx_power()`
   (PA config, ramp); weak no-ops, implemented in `app/src/radio_tune_hw.c`.
   Plus a shadow of the last regulator/PA/TX-params/image-calibration commands
   and plain-typed register accessors for the readback. Up to 0.2.1 this item
   was a `0x08AC = 0x97` write in `SX126xWakeup()`, which `SX126xSetRx()`
   overwrote before every RX.
3. **BUSY polling** (0.2.1) — `SX126xWaitOnBusy()` busy-waits in 20 µs steps.
4. **Interrupt stamp** (0.2.0) — `sx126x_last_dio1_ms`.

Nothing to copy since 2026-09-28: `app/CMakeLists.txt` removes the vendored
`drivers/lora/loramac-node/sx126x.c` from the `loramac-node` library and compiles
this file instead (a configure error if the vendored file is not found exactly
once). The copy into the Zephyr tree is only needed for branches before that:

```bash
cp patches/lora/sx126x.c <workspace>/zephyr/drivers/lora/loramac-node/sx126x.c
```

> Still a full-file copy of an upstream driver: re-diff it against the new
> `sx126x.c` whenever `west.yml` moves the Zephyr pin.
