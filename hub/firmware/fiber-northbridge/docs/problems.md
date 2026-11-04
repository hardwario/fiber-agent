# Problems Log — STM32WL5MOC LoRa Bring-up

Reference firmware for correct behavior: **`fiber-northbridge`** (the proven
production firmware). All "expected" values below come from matching its radio
init sequence.

Last updated from session `docs/sessions/2026-03-23-summary.md`.

---

## 1. Weak signal / RSSI too low  🟡 LARGELY EXPLAINED (2026-09-26)

*Amended 2026-09-26:* on the P2P bench the Northbridge's **TX** was ~26 dB down, which was
the RF-switch table (§2, fixed in 0.1.1: downlink at the node −91 → −65 dBm). With identical
antennas on the RAK5146 and the Northbridge in the same Hub, the same STICKER frames arrived
at −51 dBm on the RAK5146 and −58 dBm on the Northbridge: **~7 dB** of RX difference, not
60. What remains may be the unported radio init below; worth a look, no longer blocking.

*Amended 2026-09-28 (0.2.2, proximos-v2 `plan/control/radio/p2p_link_adaptation.md` WP N0):*
the P2P Hub has no RAK5146, so this receiver alone sets the P2P range. The reference
firmware's source is in no checkout here and not on gitlab.hardwario.com; the values are
this log's. Against the Zephyr driver actually used (loramac-node, `st,stm32wl-subghz-radio`):

1. **RX boosted gain never ran.** loramac-node `SX126xSetRx()` writes `0x08AC = 0x94`
   (power saving) right before every `SET_RX` (`modules/lib/loramac-node/src/radio/sx126x/
   sx126x.c:308`), after the wake-up write of `0x97`; `RadioInit()` already puts `0x08AC`
   in the retention list (`radio.c:533`). 0.2.2 substitutes the boosted value in that very
   write (`rx_boost`, `0x96`; `0x97` selectable) and drops the dead wake-up write.
2. **SMPS drive `0x0923`**: never written by Zephyr. 0.2.2 `smps_rx` 40 mA before
   `SET_RX`, `smps_tx` 60 mA before `SET_TX` (read-modify-write of bits 2:1).
3. **Regulator DC-DC + LDO**: already sent once by `RadioInit()` (`radio.c:526`), unchanged;
   `regulator off` switches to LDO for an A/B.
4. **PA duty cycle**: Zephyr picks the row by `rfo-lp-max-power` (15 → `0x07`, power byte 13
   at 14 dBm); `pa_table` picks it by the requested power (14 dBm → `0x04`, 14). TX only.
5. **Image calibration**: already `CalibrateImage(0xD7, 0xDB)` on the first `SetChannel`
   (`sx126x.c:409-440, 501-505`), kept over the warm-start sleep; unchanged, the readback
   shows it.
6. **Ramp**: Zephyr 40 µs (`SX126xSetRfTxPower()`); `ramp` 200 µs. TX only.

Each is a Kconfig option (`app/Kconfig.radio_tune`; RX items and the regulator boot on, the
TX items `smps_tx`/`pa_table`/`ramp` boot off) and a `radio tune` item on the
RTT shell, so an A/B against the same Node frames needs no reflash; `radio tune regs` reads
back what the chip holds. Not yet measured on hardware: compare SNR and frame loss at the
edge of range against the RAK5146, not RSSI at close range (`firmware.md` → Debug shell).


The headline problem. Sub-GHz link works, but received signal is far weaker
than it should be.

- **Symptom**: At 20 cm distance, RSSI reads **-106 to -110 dBm**.
  Expected (per `fiber-northbridge`) is roughly **-30 to -50 dBm** at that range.
- **Impact**: ~60 dB of missing sensitivity/gain — usable range is drastically
  reduced.
- **Config in use**: 869.525 MHz (EU band), SF7, BW 125, TX power 14,
  RFO_LP (low-power PA), private sync word 0x1424.

### What was tried
- **RX boosted gain**: register `0x08AC` set to `0x97` (max) inside
  `SX126xWakeup()`, applied after *every* wake from sleep, before any RX op.
  Patch is deployed but **results not yet verified**. *(2026-09-28: it never took effect,
  see the amendment above.)*

### Remaining differences from `fiber-northbridge` to investigate
These are init steps present in the working firmware that Zephyr's driver does
not replicate (in likely-priority order):

1. **SMPS drive level** — `fiber-northbridge` uses 60 mA for TX, 40 mA for RX
   (register `0x0923`).
2. **Regulator mode** — DC-DC + LDO (opcode `0x96`, param `0x01`).
3. **PA duty cycle tuning** — different duty cycles per power level.
4. **Image calibration** — calibration for the 868–870 MHz band.
5. **Ramp time** — `fiber-northbridge` uses 200 µs; Zephyr defaults to 40 µs.

### Next steps
1. Verify whether the RX boost gain patch alone improves RSSI.
2. If still poor, port the remaining radio tuning above from `fiber-northbridge`
   one at a time and re-measure.

*Measured 2026-09-28 (bench Hub fiber-4e970f4a, FW 0.2.2-rxsens 2447733, run-time A/B over RTT):*
- **Setup.** STICKER 5722 ran release FW at 2 dBm, reporting every 60 s at SF7 on 868.1 MHz. It sat
  in a closed microwave oven, which put it at the edge of the band: RSSI −106…−108 dBm, SNR
  +1…+3 dB.
- **Modes.** A = `rx_boost off` + `smps_rx off` (0x08AC = 0x94, the 0.2.1 path). B =
  `rx_boost 0x96` + `smps_rx 40 mA` (the 0.2.2 boot default). Five 15-min slots alternated
  A/B/A/B/A, with 44 A frames and 35 B frames.
- **Result.** At the same RSSI, B raises the level-normalised SNR (SNR − RSSI) by
  **+1.04 ± 0.15 dB**; raw SNR rose +0.72 ± 0.27 dB. This held in every pair. No frame was lost in
  either mode, since ~8 dB of margin remained.
- **Reading.** The gain is ~1 dB, not the 2–3 dB expected. The noise floor estimated from the
  packets is ≈ −110.5 dBm at the chip, against ≈ −117 dBm thermal plus NF. That suggests the
  receiver is limited by in-band noise or interference (the Hub's own electronics, the on-chip
  DC-DC, or traffic on 868.1), not by LNA gain.
- **Next.** A/B `regulator off` (LDO) and `rx_boost 0x97`. Measure the idle noise floor with
  GetRssiInst on several frequencies and with Hub subsystems off (WiFi, RAK card, display).

---

## 2. RF switch truth table wrong  ✅ RESOLVED

Blocked TX/RX entirely until fixed — the antenna switch was in the wrong state.

- **Root cause**: The STM32WL5MOC's internal RF switch truth table differs from
  the generic ST BSP that Zephyr assumes. It is a **3-GPIO** switch, but
  Zephyr's `sx126x` LoRa driver models only a **2-GPIO** switch and cannot
  express the required states.
- **Correct truth table** (ST's HAL for STM32WL5Mxx, `stm32wlxx_hal.c`
  `HAL_RADIO_SetSwitchConfig`; CTRL1=PC4, CTRL2=PC5, CTRL3=PC3):

  | State | PC3 | PC4 | PC5 | Note                        |
  |-------|-----|-----|-----|-----------------------------|
  | OFF   |  0  |  0  |  0  | all LOW                     |
  | RX    |  1  |  1  |  1  | all HIGH                    |
  | TX LP |  1  |  1  |  0  | CTRL1 + CTRL3 (RFO_LP)      |
  | TX HP |  1  |  0  |  1  | CTRL2 + CTRL3 (RFO_HP, unused) |

- *Amended 2026-09-26:* the table first recorded here had **TX LP = `0 0 1`**
  (PC5 only), "recovered from working `fiber-northbridge` firmware". That state is
  not in ST's table: RX worked, but TX left the PA unconnected. On the P2P bench
  the Northbridge's ACKs reached a STICKER ~26 dB below the reciprocal uplink
  (dl_rssi −91 dBm vs −65), confirmed live over SWD (GPIOC ODR 0x20 during TX).
  With TX LP = `1 1 0` the same frames arrive at −65 dBm / SNR 12 and the UL/DL
  gap is ~7 dB. Fixed in `boards/st/stm32wl5moc/board.c`.
- **Solution**: Patched `zephyr/drivers/lora/loramac-node/sx126x.c` to call
  board-level weak functions `board_rf_switch_tx() / _rx() / _off()` when
  `CONFIG_BOARD_STM32WL5MOC` is set, preserving default behavior for other
  boards. `boards/st/stm32wl5moc/board.c` implements the 3-GPIO control to
  match `fiber-northbridge` exactly.

---

## 3. In-tree driver patch — maintenance risk  ⚠️ TECH DEBT

- The fix for problem #2 lives **in-tree** at
  `zephyr/drivers/lora/loramac-node/sx126x.c` (RF switch hooks + RX boost on
  wakeup). It will be **overwritten on any Zephyr update** (`west update`).
- **Mitigation to do**: convert the patch into a proper out-of-tree Zephyr
  module / overlay instead of editing the vendored driver directly.
- *Amended 2026-09-28:* `app/CMakeLists.txt` now compiles `patches/lora/sx126x.c`
  in place of the vendored file in the `loramac-node` library, so `west update`
  no longer loses it and nothing is copied into the Zephyr tree (the configure
  step fails if the vendored file is not found exactly once). What remains is
  that the patch is a full copy of an upstream file: re-diff it on every Zephyr
  pin move.

---

## Open items carried forward
- [ ] Verify RX boost gain actually improves RSSI (problem #1) — 0.2.2 is the first build
  where it is applied; HIL A/B with `radio tune`.
- [x] Port remaining radio init from `fiber-northbridge` if RSSI still weak — 0.2.2, each
  switchable; to be measured.
- [ ] Clean up debug logging in `app/prj.conf`.
- [x] Turn the `sx126x.c` patch into a maintainable module (problem #3) — built
  from the repo since 2026-09-28; still a full-file copy.

---

## 4. P2P protocol firmware — needs HIL verification  🔴 OPEN, not yet flashed

The binary host<->modem protocol (`hdlc.c`/`uart_link.c`/`cmd_dispatch.c`/
`radio_ctl.c`, spec `plan/system/lora/design.md` §3-4) is a first
implementation against that spec, unit-tested in `tests/` for its
pure-logic pieces (framing/CRC, command parsing) but **never built or run
on real hardware yet**. Known gaps to check on the bench:

- **`TX_SETUP_LEAD_MS` (`radio_ctl.c`, currently 15 ms)** — the lead time
  before a scheduled `t_ms` at which the modem starts reconfiguring for
  TX. Picked without HIL data; the design doc's own bench numbers
  (`system/lora/design.md` §4) are for the *central's* scheduling margin
  (~30 ms on top of `rx1_delay`), a different number from this modem-
  internal one. Needs a real sweep, same as `rx1_delay`/RX-window width.
- **`EVT_RX.snr_q_db` unit conversion** — the wire spec names this field
  "quarter-dB, sx126x native", but Zephyr's generic `lora_recv_cb` only
  exposes whole-dB `int8_t`. `radio_ctl.c` converts by ×4 (a legitimate
  unit conversion, not fabricated precision), but true quarter-dB
  *resolution* would need reading the SX126x SNR register directly,
  bypassing this driver API. Confirm with the host-side implementer
  whether ×4-quantized precision is acceptable for phase 1.
- **`EVT_TX_DONE.t_actual_ms` precision** — captured after the blocking
  `lora_send()` call returns, not at the TxDone IRQ. Same class of
  imprecision as the node-side gap already tracked against
  `sticker-firmware#118`; the generic LoRa API gives no TxDone-IRQ hook to
  do better without a driver-level change.
- **GET_INFO's `rx_count`/`tx_count` semantics** — implemented as LoRa
  packet counts (successful `EVT_RX`/completed `EVT_TX_DONE` OK), separate
  from `crc_err` (UART-link HDLC frame errors, not radio CRC). The wire
  spec doesn't disambiguate; confirm this reading matches what the host
  side expects before relying on it for diagnostics.
- **Single-radio TX/RX turnaround** — `radio_ctl.c` releases async RX
  (`lora_recv_async(dev, NULL, NULL)`) before every scheduled TX and
  re-arms it after, per the bench finding in `system/lora/design.md` §4
  (the sx12xx driver's single modem lock). Not yet exercised against a
  live over-the-air join/ACK round trip on this specific firmware build —
  only against the throwaway bench responder mentioned in that doc.
  > **2026-09-26 bench**: exercised live — the turnaround itself measured > ~90-100 ms (should
  > be a few ms), missing the Node's retry-free frame. **Fixed in 0.2.1**:
  > `SX126xWaitOnBusy()` (`patches/lora/sx126x.c`) rounded every BUSY poll up to a full 1 ms
  > tick instead of the actual few-µs wait; now measured and exposed as `tx_rx_turnaround`
  > (`GET_INFO`/`p2p_status`), see CHANGELOG.md [0.2.1]. **Still open**: `sx12xx_lora_config()`
  > (upstream, not ours) puts the radio back to sleep at the end of every `lora_config()` call,
  > so the RX reconfiguration + `lora_recv_async()` pair on this path forces two sleep→wake
  > transitions (`RADIO_WAKEUP_TIME` 3 ms each by the loramac-node library's own constant)
  > instead of one — a few more ms not addressed here, since it means changing upstream code.
- **System workqueue stack margin (production build)** — the bench build needed 4096 B
  (see CHANGELOG, 2026-09-08). The production `uart_link_send` still puts a 566 B
  `frame[P2P_MAX_ENCODED_LEN]` on the RX path's system-workqueue stack, and 2048 B was chosen
  after a fault with no measured margin (`b6d270a`). Either raise `prj.conf`
  `CONFIG_SYSTEM_WORKQUEUE_STACK_SIZE` to 3072 or make `uart_link_send`'s buffers static under a
  spinlock (it is called from the system workqueue, `radio_wq` and the shell/main thread).
  Measure with `CONFIG_INIT_STACKS` + `k_thread_stack_space_get` before deciding.
