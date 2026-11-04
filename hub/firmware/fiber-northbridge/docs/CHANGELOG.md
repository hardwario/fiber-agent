# Changelog — fiber-northbridge firmware

Notable changes to the northbridge firmware. Newest first. Record every change
that affects the running image or the host↔modem protocol here, alongside the
commit that made it. See [`firmware.md`](firmware.md) for the current behaviour.

Format loosely follows [Keep a Changelog](https://keepachangelog.com/).
FW version is `FW_VERSION_*` in `app/src/proto.h`. Until 0.2.x the wire protocol version was
`P2P_PROTO_VERSION`; since the 0.3.0 flag day there is none: the image ships with the Hub
software that speaks it, and `EVT_BOOT` / `GET_INFO` carry the FW version and the commit.

## [Unreleased]

### Changed
- Board: the 32 kHz LSE crystal is driven at the strongest level (`driving-capability = <3>`,
  RCC_BDCR LSEDRV = 3) instead of the reset default 0 (proximos-v2 #153). The RTC runs from
  the LSE. Build only; LSERDY/LSEDRV still to be read on hardware.

## [0.4.2] — 2026-10-13

Event ring resend with backoff (proximos-v2 #130). No wire change: any Hub software that
speaks 0.4.1 speaks 0.4.2. Build and native_sim tests only; not yet on hardware.

### Changed
- Event ring resend (link §4.1): the first go-back-N pass comes 1 s after the oldest unacked
  event was sent (was 300 ms); every further pass without an ack doubles the wait, capped at
  4 s; an ack that releases an event brings it back to 1 s. On the bench Hub every one of
  247 resends in 4.5 h was spurious (`link_crc_err` 0): the host's ack, which waits for
  its eMMC flushes, sometimes left 400–600 ms after the event. A frame truly lost on the
  UART now costs up to 1 s of delay, still no data.
- Tests: `tower_gateway` `test_go_back_n_resend_pacing` on the new timing,
  `test_resend_backoff`.

### Added
- `p2p listen` (RTT shell, factory): from BOOT only, receive on the shipped P2P channel with
  net id 0 and ACKs off, so the provisioning station counts frames before the Hub's Linux
  runs (proximos-v2 `plan/system/factory_test.md` §5.1).

### Changed
- The RTT shell group `twr` is now `p2p` (`p2p show|stats|peers|queue|duty|rx_inject`). Only
  the J-Link bench and the provisioning station use it; the Hub talks to the NB over UART.

## [0.4.1] — 2026-10-12

One MTU on both PHY profiles (sticker-firmware #486 U-1): the TOWER 96 B frame, 74 B
payload. Ships with the Hub software that sends at most 74 B (proximos-v2
`plan/control/radio/p2p_tower_gateway.md`, amendment 2026-10-12). Build and native_sim tests
only; not yet on hardware.

### Changed
- `TWR_QUEUE_PUSH` plaintext cap `P2P_TWR_DL_MAX_PT` 78 → 74 B (`BAD_PARAM` above): the
  longest downlink is a 96 B frame (was 100 B, #470 D14 / contract amendment 2).
- Receive: a frame over 96 B is dropped before its header is parsed — no ACK, no
  `EVT_TWR_UPLINK`, no raw `EVT_RX` — and counted as `too_long` in `twr stats` (shell only,
  `GET_INFO` unchanged), with a warning in the log. Until now the gateway parsed up to 255 B.
  A STICKER older than #486 that sends a 97–100 B frame is not heard until it is updated.
- Link vectors: `twr_queue_push_max_74` / `twr_queue_push_bad_len_75` replace the 78 / 79 B
  pair.

## [0.4.0] — 2026-10-08

Update through the Hub, step U1 (proximos-v2 `plan/control/radio/northbridge_update.md` §3.1,
§10). Needs one J-Link flash of `northbridge_jlink.hex` (stub + app + record) on a 0.3.x
Northbridge; after that the Hub updates the app over the ROM bootloader. Build and native_sim
tests only; not yet on hardware.

### Added
- **`nbboot`** (`nbboot/`): a 600 B bare-metal stub at page 0. It starts the app when the
  `app_valid` record at `0x0803_B800` matches the image, and the STM32 ROM bootloader when
  it does not or when the boot request word at `0x2000_FFF0` says "NBBL".
- **`0x07 ENTER_BOOTLOADER`** (body `4E 42 42 4C`): RSP `0x47`, then the boot request word
  and a cold reboot into the ROM bootloader. `BAD_PARAM` for any other body.
- `GET_INFO` grows to 188 B: `app_image_len` u32 at 180, `app_crc32` u32 at 184, from the
  `app_valid` record (0 / 0 when it is not valid).
- `tools/nb_package.py`: `northbridge.bin` + `northbridge.json` for the Hub, the record
  and the J-Link hex; `tools/nb_app_valid.py`, its record reference.
- Tests: `tests/nbboot_record`, `tools/test_nb_package.py`; `ENTER_BOOTLOADER` and the
  188 B `GET_INFO` in `cmd_dispatch`, `tower_gateway` and `tower_link_vectors`.

### Changed
- The app is linked at `0x0800_0800` (slot0, 236 KB) behind the stub; the last 16 B of SRAM
  are no longer Zephyr's (63 KB RAM). `west flash` programs `northbridge_jlink.hex`.
- `git_sha` (`EVT_BOOT`, `GET_INFO`) is the last commit that touched what goes into the
  image (`app/`, `boards/`, `patches/`, `west.yml` under `firmware/fiber-northbridge`), not
  `HEAD`, so Hub-only commits, and NB docs/tests/tools commits, leave the image unchanged.

## [0.3.2] — 2026-10-06

#470 P3 (N1 part A): the gateway answers LinkCheckReq and TimeReq in its own ACK
(sticker-firmware TOWER doc §13.5, link doc §5.1 `0x19` and §6.5). Flashed together with
the Hub software that sends `TWR_TIME_SYNC`. Build and native_sim tests only; not yet on
hardware.

### Added
- **CTRL tail in the ACK:** flag b1 `CTRL` and a TLV tail after `acked · rssi · flags` when
  a confirmed `0x91` uplink asks for `0x10` and/or `0x20`: LinkCheckAns (`rssi, snr dB,
  margin, gw_count 1`, the central's formula) and TimeAns (`unix · frac · req_counter`, the
  wall time at the end of the received frame). A retransmit's re-ACK rebuilds it. Longest
  ACK 45 B (was 28 B); `EVT_TWR_UPLINK` still carries the whole plaintext.
- **`0x19 TWR_TIME_SYNC {now_ms u64, unix_ms u64}`**: the wall clock anchor, status-only
  RSP. Without one TimeAns is left out.
- **`EVT_TWR_UPLINK` `ack` flags:** b7 the ACK carried a CTRL tail, b6 the tail holds a
  TimeAns; the code is in bits 0..5. A late ACK keeps both flags (`0xC3`); a radio
  error (nothing on air) clears them (`0x03`). LinkCheck + Time without an anchor gives b7 only.
- `GET_INFO` grows to 180 B: `ack_ctrl_sent` u32 at 172, `ack_time_no_anchor` u32 at 176.
- `twr stats` line `ack ctrl:`; `twr show` prints the ToA of the longest ACK too.
- Tests: the shared ACK CTRL KAT (`plan/control/radio/p2p_tower_ack_ctrl_kat.txt`) in
  `tests/tower_gateway`; the link vector test takes the future `0x19` and 180 B `GET_INFO`
  vectors next to the 0.3.1 ones.

## [0.3.1] — 2026-10-06

#470 P2 close-out C1 and C2 (sticker-firmware `470 - Northbridge-Hub link protocol.md` §4).
Flashed together with the Hub software that sends `TWR_EVT_ACK` (C3). Build and native_sim
tests only; not yet on hardware.

### Added
- **Event ring + `0x18 TWR_EVT_ACK {seq}`** (link §4.1). `EVT_TWR_UPLINK` and `EVT_TWR_TX`
  stay in a ring of `CONFIG_NB_EVT_RING_SIZE` entries (default 32, 32..127) until the host
  acks them; their frame `seq` is the delivery seq (u8, rolling, 1 is the first after
  boot). The ack is cumulative and has no RSP; one outside the outstanding window or of the
  wrong length is ignored (`twr stats` `ack_ignored`). Go-back-N resend from the oldest
  unacked event once it waited 300 ms, at most one pass per 300 ms (`evt_resent`).
- **Backpressure to the air:** a frame whose events do not fit the ring is dropped whole —
  no ACK, no re-ACK, no `last_seen` update, no event. Fresh ones count in
  `acks_suppressed` (`twr stats` `rx_ring_full` counts all). `EVT_TWR_UPLINK` ack 5
  "suppressed" is defined but never sent: there is no ring slot to carry it.
- **`TWR_NODE_ADD` flag b1 `KEEP_NEWER`** (link §4.2): with the same session key the peer
  keeps `max(own, sent)` `last_seen`; a different key takes the sent value. Undefined flag
  bits stay `BAD_PARAM`.
- `GET_INFO` fills `evt_ring_used`, `evt_ring_max` and `evt_resent`.
- Bench `twr rx_inject <hex> [rssi_dbm]` behind `CONFIG_NB_TWR_RX_INJECT` (default n):
  a captured air frame into the RX path as if received. `GET_INFO` `build_flags` b2.

### Changed
- The best-effort events (`EVT_BOOT`, `EVT_RX`, `EVT_TX_DONE`, `EVT_LOG`,
  `EVT_TWR_CTR_LOW`) carry the newest delivery seq without advancing it (was: one rolling
  seq over all events).
- `TWR_NODE_ADD` with the same key no longer drops the peer's queue (only a new key does,
  link §5.1), so a restore does not resend an in-flight downlink.
- RAM +4 KB (the ring's body pool is sized for 32 lora-MTU uplinks, 3.2 KB).

## [0.3.0] — 2026-09-28

**Flag day: the Northbridge is the TOWER gateway** (STICKER #470 P2). Contract: proximos-v2
`plan/control/radio/p2p_tower_gateway.md` §1–§2.3 with the coordinator's amendments 1–4 and
the flag day (`firmware.md` → Contract status; the contract text still says
`proto_version = 2` and lists `0x16`). The default image changes its protocol; it has to be
deployed together with the Hub's TOWER adapter. There is no rollback: the old P2P is
abandoned (#470 link doc §3). Build and native_sim tests only; not yet on hardware.

### Changed (breaking)
- **The default image is the TOWER gateway**, no build option: the Northbridge holds the
  session keys the central pushes (RAM only), opens and replay-checks uplinks, ACKs them
  itself `turnaround_ms` after RxDone (with `PENDING`), sends queued downlinks `dl_gap_ms`
  after the ACK's TxDone, waits for the node's ACK of a confirmed one, and spends gateway TX
  counters from the central's blocks (fail-closed, `EVT_TWR_CTR_LOW` below 25 %).
- **No `proto_version`.** `EVT_BOOT` is 8 B: `fw_ver[3] · git_sha[4] · reset_cause`
  (`git_sha` from `git rev-parse` at configure time). `GET_INFO` is a new, frozen 171 B
  layout: identity, `build_flags`, `now_ms`, state, `net_id` and timing, the radio config,
  the diagnostics, 25 counters, the ACK turnaround in µs, the sliding-hour airtime, table
  and queue sizes, the counter block (`firmware.md` → GET_INFO).
- **States** `BOOT → CONFIGURED → GATEWAY` (`GET_INFO state`): `SET_RADIO_CONFIG` enters
  `CONFIGURED`, `TWR_START` (after `TWR_CTR_BLOCK`) enters gateway receive, `RX_STOP` goes
  back keeping the tables. `SET_RADIO_CONFIG` in `GATEWAY` answers `INVALID_STATE`.
- **Retired:** `0x03 RX_START` and `0x16 TWR_GET_STATS` answer the new `8 UNSUPPORTED`, as
  does any unknown command (was `BAD_PARAM`).
- `EVT_RX` only for frames the gateway does not open (JoinReq, unknown source);
  `EVT_TX_DONE` only for `TX_SCHEDULE` (the JoinAccept).
- **One clock:** every `t_ms` / `now_ms` is the 64-bit cycle counter in ms
  (`CONFIG_CORTEX_M_SYSTICK_64BIT_CYCLE_COUNTER` in `prj.conf`); RxDone / TxDone are stamped
  from it in the driver's DIO1 hook.
- **USART1 TX through a 2048 B ring** drained by the UART interrupt instead of polled
  `uart_poll_out`; a sender waits at most 50 ms (10 ms from the radio thread), then the
  frame is dropped and counted in `link_err`. Events take their `seq` in wire order; a
  dropped one still takes its number.
- The watchdog is withheld while a TX runs longer than twice its ToA plus 2 s (was: 5 s).
- `SET_RADIO_CONFIG` power range is the board PA's, from the devicetree the driver reads
  (RFO_LP, `rfo-lp-max-power = 15`: −16..15 dBm), `BAD_PARAM` outside, no clamping (was
  −9..22 dBm, which the driver clamped to 15 dBm silently).

### Added
- Commands `0x10 TWR_START`, `0x11 TWR_NODE_ADD`, `0x12 TWR_NODE_REMOVE`,
  `0x13 TWR_CTR_BLOCK`, `0x14 TWR_QUEUE_PUSH` (plaintext ≤ 78 B, amendment 2),
  `0x15 TWR_QUEUE_DROP`, `0x17 TWR_NODE_LIST` (amendment 1); events `0x84 EVT_TWR_UPLINK`
  (sent after the ACK's TX result, never delaying it), `0x85 EVT_TWR_TX` (with
  `node_ack_counter`), `0x86 EVT_TWR_CTR_LOW`; statuses `6 NO_SPACE`, `7 NOT_FOUND`.
- 256 peers with the #470 replay rule, a pool of 32 downlinks with TTL.
- Time-on-air cap 3.95 s under the driver's 4 s TX timeout (amendment 4): `SET_RADIO_CONFIG`
  (a config whose 28 B ACK exceeds it, also frequency and power outside the SX126x range,
  amendment 3), `TWR_QUEUE_PUSH` and `TX_SCHEDULE` answer `BAD_PARAM`.
- `TX_SCHEDULE` against the gateway's own ACK / downlink: `BUSY`, or `EVT_TX_DONE LATE`
  when the ACK takes the radio.
- RTT shell `twr show|stats|peers [<start>]|queue|duty`; `p2p_status` prints a gateway line.
  No key or plaintext is ever printed.
- An RX re-arm that fails is retried every 1 s (`EVT_LOG` code 2 as before).
- ACK and downlink timing judged against the node's receive window (STICKER P1:
  turnaround + ToA + 3 symbols + 20 ms): a late ACK is still sent and counted in
  `acks_late`; a downlink that would start more than 3 symbols + 20 ms after its slot is
  not sent and goes back to the queue.
- `TWR_CTR_BLOCK` never moves the next counter back: `last` below it is `BAD_PARAM`, a block
  starting below it continues from it (#470 link doc §5.1).
- Native tests: `tower_gateway` (core against the #470 KAT, link codec, frozen `EVT_BOOT` and
  `GET_INFO` layouts), `cmd_dispatch` rewritten for the command path in every state,
  `proto_integration` on the same fakes.
- **TOWER P0 bench gateway image** (`-DBENCH_RTT_BRIDGE=ON -DTOWER_P0_GATEWAY=ON`), kept: the
  Northbridge decodes TOWER-over-LoRa frames, sends the auto-ACK at a fixed turnaround and
  one queued downlink, all driven from the RTT shell (`twr …`, `TWR_*` lines). AES-128-CCM
  in plain C (`app/src/twr_crypto.c`), `tests/tower_codec` with the KAT vectors. See
  [`firmware.md`](firmware.md#tower-p0-gateway--dtower_p0_gatewayon-bench-only). It now also
  carries the 0.2.2 radio tuning.
- 0.2.2's RX sensitivity tuning carried in unchanged (cherry-picked from
  `rxsens/northbridge-rx-sensitivity`): boot line `radio tune: rx_boost=on(0x96)
  smps_rx=on(40mA) …`, `radio tune` shell, `tests/radio_tune`.

### Removed
- `app/src/radio_ctl.c` (the proto-1 radio glue); `radio_ctl.h` is now the gateway runtime's
  seam, implemented by `app/src/twr_gateway.c`.
- `GET_INFO`'s proto-1 layout, the 0.2.1 `tx_rx_turnaround` ms pair (replaced by the ACK
  turnaround in µs) and the IRQ-latency report.
- FW version 0.3.0.

## [0.2.2] — 2026-09-28

RX sensitivity (`docs/problems.md` §1, proximos-v2 `plan/control/radio/p2p_link_adaptation.md`
WP N0): the P2P Hub has no RAK5146, and on the bench the Northbridge heard the same STICKER
frames ~7 dB below it. Build and native_sim tests only; not yet on hardware.
`P2P_PROTO_VERSION` stays 1, the wire is unchanged.

### Fixed
- **RX boosted gain is applied for the first time.** The `0x08AC = 0x97` that
  `SX126xWakeup()` wrote since 0.1.0 was overwritten before every `SET_RX` by loramac-node's
  `SX126xSetRx()` (`0x94`, power saving). The patched driver now passes every single-register
  write through `board_sx126x_filter_reg()`, which substitutes the boosted value (`0x96`) for
  that `0x94`; the dead wake-up write is gone.

### Added
- **Radio tuning items**, each a Kconfig option (`app/Kconfig.radio_tune`; the RX items and
  the regulator boot on, the three TX items boot **off**, so a modem that resets mid-test
  comes back with the stock TX path; `CONFIG_NB_RADIO_TUNE=n` for the stock driver) and
  switchable at run time:
  `rx_boost` (RX), `smps_rx` 40 mA before `SET_RX` (RX), `regulator` DC-DC + LDO (off: LDO;
  the driver already sends DC-DC, so nothing extra at boot), and three that **change TX**:
  `smps_tx` 60 mA before `SET_TX`, `pa_table` (SetPaConfig row by requested power: 14 dBm →
  `0x04`/power 14 instead of `0x07`/13), `ramp` (200 µs instead of 40 µs, the preamble starts
  ~160 µs later). Image calibration (`0xD7 0xDB`) was already done by the driver; unchanged.
- Driver hooks (`patches/lora/nb_sx126x_hooks.h`): `board_sx126x_filter_reg()`,
  `board_sx126x_enter_mode()` (before `SET_RX`/`SET_TX`), `board_sx126x_set_rf_tx_power()`;
  a shadow of the last `SetRegulatorMode`/`SetPaConfig`/`SetTxParams`/`CalibrateImage`.
- **RTT shell `radio tune show|set|apply|regs`** for a HIL A/B without reflashing: `set`
  switches an item (or `all`) and re-arms RX on the radio work queue; `regs` reads back
  `0x08AC`, the `0x029F` retention list, `0x0923`, OCP, chip status and errors, PWR SMPS
  flags and the command shadow.
- Boot log line `radio tune: …` with the effective settings and the chip's SMPS reset value.
- `radio_ctl_call()`: runs a function on the radio work queue, optionally with RX paused.
- ztest suite `tests/radio_tune` (table, defaults, gain substitution, operations and order).
- FW version 0.2.2.

## [0.2.1] — 2026-09-27

Fixes the TX->RX turnaround bench finding from 2026-09-26 (`docs/problems.md` §4
"Single-radio TX/RX turnaround"): after the modem transmitted an ACK, it did not hear a Node
frame starting ~90-100 ms after the ACK ended (a retry 2 s later was heard). `P2P_PROTO_VERSION`
stays 1.

### Fixed
- **`SX126xWaitOnBusy()` no longer rounds every BUSY wait up to a full 1 ms tick**
  (`patches/lora/sx126x.c`). It busy-waits in 20 µs steps for up to 10 ms before falling back to
  `k_sleep(K_MSEC(1))`. BUSY normally clears in microseconds, but it's checked twice per SPI
  command (`SX126xCheckDeviceReady()` and this function), and the ~15-20 commands a
  `lora_config()` + `Radio.Rx()`/`Radio.Send()` pair issues could each cost a full tick -
  likely the main contributor to the measured gap. Re-applied to the workspace's vendored
  Zephyr tree per `README.md`.

### Added
- **`tx_rx_turnaround` diagnostic**: last/max ms from the TxDone stamp
  (`EVT_TX_DONE.t_actual_ms`) to RX being armed again after a scheduled TX
  (`radio_ctl_get_turnaround()`, `radio_ctl.c::tx_work_handler()`). On the RTT shell,
  `p2p_status` prints `tx_rx_turnaround_ms last=… max=…`; on the wire, two little-endian u16
  fields appended after the existing 17 B diagnostics block in `GET_INFO` (payload offset 52,
  +4 B) — an older host reading a shorter RSP is unaffected.
- FW version 0.2.1.

### Still open (not changed here — see `docs/problems.md` §4)
- `sx12xx_lora_config()` (upstream Zephyr, `zephyr/drivers/lora/loramac-node/sx12xx_common.c`,
  not ours) calls `modem_release()` → `Radio.Sleep()` at the end of every `lora_config()` call,
  including the RX reconfiguration on this path. `lora_recv_async()`/`Radio.Rx()` right after
  then has to wake the radio again (`SX126xCheckDeviceReady()`), so the TX->RX path pays for
  two sleep→wake transitions instead of one. `RADIO_WAKEUP_TIME` is 3 ms per transition by the
  loramac-node library's own constant — a few more ms, on top of the tick-rounding fixed above.
  Not changed: it is upstream code, and the task here was to keep `Radio.Sleep()` in that file
  intact.

## [0.2.0] — 2026-09-26

Diagnostics (`proximos-v2 plan/control/radio/northbridge_diagnostics.md` §2, reviewed) and
interrupt timestamps. `P2P_PROTO_VERSION` stays 1: every wire addition is appended and an
older host ignores it. HIL on the Hub: see `firmware.md` → Testing.

### Added
- **`GET_INFO` diagnostics block** (17 B after the radio-config echo): `radio_state`,
  `boot_reset_cause`, sticky `err_flags`, `rx_rearm_fail`/`tx_radio_err`/`link_err`
  counters, `last_err_code` and its age (`diag.c`).
- **Structured `EVT_LOG` in every build**: `level | code | ASCII`, at most one per code per
  10 s; codes 1 radio init, 2 RX (re-)arm, 3 TX radio, 4 link framing, 5 UART overrun.
- **Independent watchdog**, 8 s, fed from the main loop and withheld while a scheduled TX has
  hung for 5 s; paused under a debugger (`CONFIG_WATCHDOG`).
- **Interrupt timestamps**: `EVT_RX.t_ms` and `EVT_TX_DONE.t_actual_ms` from the radio
  interrupt's work item (`patches/lora/sx126x.c` exports `sx126x_last_dio1_ms`), not the end
  of `IrqProcess` (3–4 ms later on the bench).
- RTT shell: `p2p_status` prints the diagnostics and the interrupt latency; `nb_selftest
  log <code>` / `nb_selftest wdt` for HIL.
- ztests for the diagnostics block, the `EVT_LOG` body and its rate limit.

### Changed
- The RX re-arm failure `EVT_LOG` (below, from !5) is now the structured code 2.
- A radio that fails to initialise no longer stops the firmware: the link answers with
  `radio_state` 3 and `err_flags` b0.
- FW version 0.2.0.

### From the protocol branch (!5)

### Fixed
- **Bench build: system workqueue stack 2048 → 4096 B** (`bench_rtt_bridge.conf`). The RX path
  runs on the system workqueue (`lora_rx_cb` → `uart_link_send`), and the bench `uart_link_send`
  stack-allocates the worst-case escaped frame plus its `P2P_OUT` hex line (~1.7 KB), so the
  2048 B queue overflowed on the first `EVT_RX` after a scheduled TX (fault just below
  `sys_work_q_stack`, 2026-09-08 bench). `radio_wq` had been raised to 3072 for the same
  buffers at `3b557b6`; the system workqueue had not. Production USART1 build unchanged; its
  2048 B queue still carries a 566 B `frame[]` on the RX path — see `problems.md` §4.

### Added
- **Bench build: `CONFIG_EXTRA_EXCEPTION_INFO=y`** (`bench_rtt_bridge.conf`). The 2026-09-09
  bench fault (BUS FAULT with `pc`/`lr` unmapped, "Current thread" pointing 4 B past
  `__data_region_end`, fatal reason 27) printed no stack pointer and no callee-saved registers,
  so a stack overflow could neither be confirmed nor ruled out. The fatal handler now also dumps
  `r4`–`r11` and the faulting SP. Bench build only.
- **`nb_rtt_bridge.py --raw-log PATH`** appends every byte the modem writes on RTT ch.0 to a
  file, so a fatal-handler dump is captured even though the bridge is the only RTT client. The
  2026-09-09 fault's single copy lived in `/tmp` and was lost with it.
- **`EVT_LOG` on RX re-arm failure** (`radio_ctl.c::tx_work_handler`). When re-arming the
  receiver after a scheduled TX fails, the modem now emits `EVT_LOG` with the body
  `rx re-arm failed after tx` instead of only logging over RTT. Previously the receiver went
  quiet with no wire-level trace, so the host saw unexplained `EVT_RX` silence — a dead link
  that looks like a quiet one — which is the "everything async is an event" rule
  (`proximos-v2 plan/system/lora/design.md` §2) being broken by the one path that had no event.
  `P2P_EVT_LOG` was defined and never emitted until now. The modem still decides nothing:
  whether to re-issue `RX_START` is the host's call. Host side already decodes it
  (`control-radio` `p2p/modem.rs` maps `evt::LOG` to `Event::Log`). No protocol change —
  `P2P_PROTO_VERSION` stays 1.
- **`tests/hil_p2p/nb_rtt_bridge.py`** — an RTT↔PTY byte pipe that lets Proximos's own
  `control-radio` drive this modem over the bench build, instead of the Python central in
  `nb_hil.py`. Deliberately has no keys and no protocol knowledge, so the implementation under
  test stays the Rust one. Framing covered offline by `test_nb_rtt_bridge.py` (9 tests).
- **`CONFIG_SHELL_CMD_BUFF_SIZE=1200`** in `bench_rtt_bridge.conf`. `p2p_inject` is a shell
  command, so a whole hex-encoded frame must fit one shell line: worst case
  `P2P_MAX_ENCODED_LEN` (566 B) is 1132 hex characters plus `"p2p_inject "` = 1143, against a
  Zephyr default of 256 that truncates silently. Costs ~1.9 KB of RAM in the bench build only
  (35.6% → 38.5%); the production build is unaffected.

### Changed
- **`GET_INFO`'s error counter documented as *link frame errors***, not radio CRC
  (`docs/firmware.md`). `uart_link.c` increments one counter for CRC mismatches *and* framing
  faults (`HDLC_FEED_TOO_SHORT`, `HDLC_FEED_OVERFLOW`), and none of it concerns the radio — a
  frame the radio drops never reaches that layer. Counter semantics unchanged; only the name
  and the docs were misleading.
- **Two-probe P2P HIL harness** (`tests/hil_p2p/`) — a Python central that drives
  this northbridge over RTT and runs the P2P join→telemetry→ACK exchange against a
  real STICKER, no Proximos/CM4/PTY bridge. Its crypto is verified offline against
  sticker-firmware's own known-answer vectors (`test_p2p_proto.py`).
- **`tests/proto_integration/`** native_sim suite — end-to-end raw-HDLC-bytes →
  decode → dispatch → RSP (the `uart_link_bench_inject`/USART1-ISR path), including
  that a P2P frame containing `0x7E`/`0x7D` survives HDLC byte-stuffing to the
  dispatched TX_SCHEDULE. Complements the framing-only and dispatch-only suites.

## 0.1.1 — RF switch TX fix (2026-09-26)

`GET_INFO` / `EVT_BOOT` report 0.1.1, so a fixed image is told apart from 0.1.0.

### Fixed
- **RF switch TX state** (`boards/st/stm32wl5moc/board.c`) — `board_rf_switch_tx()`
  now drives TX LP as ST's HAL for STM32WL5Mxx defines it: PC3 = PC4 = 1, PC5 = 0
  (was PC5 only, a state ST's table does not have, which left the PA unconnected).
  Measured on the P2P bench with a STICKER at SF7: the Northbridge's ACKs went from
  −91 dBm / SNR 8 to −65 dBm / SNR 12 (+26 dB), and the UL/DL gap from ~26 dB to
  ~7 dB. RX (all three high) was already right. The truth table in the board DTS,
  `README.md` and `docs/problems.md` now cites the ST source.

## 0.1.0 — P2P gateway modem (dev, MR !3)

Protocol version 1. First firmware that turns the STM32WL5MOC into the P2P
transport's dumb/keyless/stateless gateway modem.

### Added
- **P2P host↔modem binary protocol** (`376f825`) — HDLC framing + CRC16, the
  `GET_INFO`/`SET_RADIO_CONFIG`/`RX_START`/`RX_STOP`/`TX_SCHEDULE`/`TX_CANCEL`
  command set, and `EVT_BOOT`/`EVT_RX`/`EVT_TX_DONE`/`EVT_LOG` events, over
  USART1 (→ CM4 `ttyAMA3`). Async RX with no address filtering; absolute-clock
  scheduled TX for hitting a node's RX1 window. Spec: proximos-v2
  `plan/system/lora/design.md` §3–4.
- **Bench RTT bridge** (`c30f791`, reworked `3b557b6`) — `-DBENCH_RTT_BRIDGE=ON`
  carries the same protocol over the RTT **shell channel (0)** as
  `p2p_inject <hex>` in / `P2P_OUT <hex>` out text lines, for rigs with no
  CM4/USB-serial on the real UART. Outgoing frames are written synchronously
  (not via deferred `LOG_INF`, whose latency would blow the ~1 s RX1 window).
  The build flag auto-pulls `bench_rtt_bridge.conf`
  (`CONFIG_SEGGER_RTT_BUFFER_SIZE_DOWN=256`) so a whole `p2p_inject` frame lands
  in one RTT write; the radio workqueue stack grows 2048→3072 B for the
  hex-encode path. Never for production.

### Fixed
- **MemManage fault on the first received frame** (`b6d270a`) — the sx126x DIO1
  IRQ runs the loramac RxDone chain (→ `uart_link_send`) on the system
  workqueue, whose default 1024 B stack overflowed and silently killed RX.
  Raised to 2048 B.

### Verified
- **Live HIL 2026-08-28** — real STICKER ↔ this northbridge ↔ `control-radio`
  central over the RTT bridge: full join + telemetry + extended (RSSI/SNR) ACK
  and clock-sync ACK proved end to end.

### Known issues
- **Weak signal / low RSSI** — see [`problems.md`](problems.md); RX-boost patch
  applied but unverified, radio-init parity with the original HAL firmware still
  pending.
