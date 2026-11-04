# fiber-northbridge firmware — what it does

Reference for the **current** behaviour of the northbridge firmware. The
[`README.md`](../README.md) covers hardware, setup and build; this document
describes what the running image actually does. Record every change to the behaviour here
in [`CHANGELOG.md`](CHANGELOG.md).

The link is specified by sticker-firmware `doc/plan/470 - Northbridge-Hub link protocol.md`
(**normative**, 840d2fb7; its copy is to be folded into proximos-v2
`plan/control/radio/p2p_tower_gateway.md`, whose §1–§2.3 hold the frozen H2 message set).
Where this image still differs from it: [Contract status](#contract-status). The framing and
the modem commands it keeps come from proximos-v2 `plan/system/lora/design.md` §3–4. The air side is sticker-firmware
`doc/plan/470 - TOWER protocol as the P2P transport.md` §4, §5, §6.6, §9.4, §13.1. Constants
are mirrored in [`app/src/proto.h`](../app/src/proto.h), byte layouts in
[`app/src/twr_link.h`](../app/src/twr_link.h).

- **FW version:** 0.4.0 (`FW_VERSION_*` in `proto.h`); `git_sha` is the last commit that
  touched `app/`, `boards/`, `patches/` or `west.yml` of `firmware/fiber-northbridge`
- **Protocol version:** none (link §3, flag day). The gateway image is flashed together with
  the Hub software that speaks it: no negotiation, no capability bits, no backward
  compatibility and no rollback to the old P2P modem. `fw_ver` and `git_sha` in `EVT_BOOT` /
  `GET_INFO` are for diagnostics only; the central logs a mismatch, never switches on it.
- **Board:** STM32WL5MOC (STM32WL55xx, integrated Sub-GHz LoRa radio)

---

## Role — the TOWER gateway

The Northbridge terminates TOWER-over-LoRa (#470) on air. It holds the session keys of the
Nodes the central pushed (`TWR_NODE_ADD`), opens and replay-checks their uplinks, ACKs them
itself `turnaround_ms` after RxDone, sends queued downlinks and spends gateway TX counters
from blocks the central grants. The ACK has to leave 20–60 ms after RxDone, which a round
trip over the UART to the Hub cannot hit reliably; that is why the gateway lives here.

The central (`control-radio`'s TOWER adapter, contract §3) keeps everything persistent: the
sessions and their `last_seen`, the end of the last counter block, the join itself (a JoinReq
comes up raw as `EVT_RX`, the JoinAccept goes down with `TX_SCHEDULE`). **Nothing is stored in
flash:** after `EVT_BOOT` the peer table, the downlink queue and the counter block are empty
and the central rebuilds them (contract §3.4). A key is never printed, logged or sent back to
the host.

---

## Build variants

| | Transport | Build | Use |
|---|---|---|---|
| **Gateway** (default) | HDLC over **USART1** (PA9/PA10 → RPi CM4 `ttyAMA3`, 115200 8N1) | `west build -b stm32wl5moc app` | The Hub |
| **Gateway, bench RTT bridge** | HDLC over the **RTT shell channel (0)** as text | `… app -- -DBENCH_RTT_BRIDGE=ON` | Rigs with no CM4/USB-serial on the real UART (J-Link only) |
| **TOWER P0 bench** | none (standalone; RTT shell `twr …` + `TWR_*` lines) | `… app -- -DBENCH_RTT_BRIDGE=ON -DTOWER_P0_GATEWAY=ON` | The P0 bench link to one node, no Hub (see below) |

The gateway image and its bench variant are the same firmware; only the byte transport
differs. On USART1 there is **no console/shell** — it carries the binary protocol exclusively
(the devicetree removes the `chosen{}` console and adds a `p2p-uart` alias). The 0.2.x modem
image (host-side crypto, `RX_START`) is gone: the old P2P is abandoned, not a fallback.

### Bench RTT bridge (`-DBENCH_RTT_BRIDGE=ON`, never production)

`rttt`, the J-Link RTT console this project drives bench units with, exposes a
command/log-line surface, not a raw byte stream — so the bench build carries the
same HDLC frames over the interactive **shell channel (0)** as text:

- **host → modem:** `p2p_inject <hex>` shell command feeds raw bytes to the HDLC
  decoder as if they had arrived over USART1.
- **modem → host:** each outgoing frame is written **synchronously** as a
  `P2P_OUT <hex>` line (a direct `SEGGER_RTT_Write`, *not* deferred `LOG_INF` —
  logging latency would blow the node's ~1 s RX1 window).

`-DBENCH_RTT_BRIDGE=ON` automatically pulls in
[`app/bench_rtt_bridge.conf`](../app/bench_rtt_bridge.conf); nothing extra is needed on the
build line. It sets two things, both of which are about a whole frame surviving one hop:

- `CONFIG_SEGGER_RTT_BUFFER_SIZE_DOWN=256` — a whole `p2p_inject` frame must land in a single
  RTT write, or the scheduled JoinAccept TX misses its aim time and the node never pairs.
- `CONFIG_SHELL_CMD_BUFF_SIZE=1200` — `p2p_inject` is a *shell command*, so the hex also has to
  fit one shell line. Worst case is `P2P_MAX_ENCODED_LEN` (566 B) → 1132 hex characters plus
  `"p2p_inject "`, i.e. 1143. Zephyr's default 256 silently truncates anything longer, which on
  this bridge looks like a frame the modem never answers rather than like a buffer that is too
  small.

Both move `_SEGGER_RTT`, so **re-read its address from the `.elf`** rather than reusing one from
a previous build:

```sh
nm build-nb-rtt/zephyr/zephyr.elf | grep ' _SEGGER_RTT$'
```

One remaining ceiling, for anyone injecting hand-made frames: `cmd_p2p_inject` decodes into a
512-byte buffer and refuses more by name (`too many bytes (max 512)`). That is under the 566 B
worst case but far above any real frame — a `TX_SCHEDULE` carrying a 255 B radio frame encodes
to about 272 B — and it fails loudly rather than truncating.

**Driving it from `control-radio`.** `tests/hil_p2p/nb_rtt_bridge.py` turns this text channel
back into a serial device (it has no protocol knowledge, so it works with 0.3.0 unchanged): it hands out a PTY, injects whole HDLC frames as `p2p_inject`, and
writes decoded `P2P_OUT` payloads back. It is a byte pipe with no protocol knowledge, so the
implementation under test stays the Rust central:

```sh
python3 fiber-northbridge/tests/hil_p2p/nb_rtt_bridge.py \
    --nb-sn <SN> --elf build-nb-rtt/zephyr/zephyr.elf --link /tmp/p2p-modem --verbose
PROXIMOS_P2P_UART=/tmp/p2p-modem control-radio-p2p-host --no-mqtt
```

One RTT client per probe: stop `rttt` and `nb_hil.py` before starting it.

### TOWER P0 gateway (`-DTOWER_P0_GATEWAY=ON`, bench only)

The P0 bench image, kept for a Hub-less link test: the Northbridge decodes TOWER frames
itself, ACKs them and sends one queued downlink, all driven from the RTT shell. There is no
UART link and no command path (`app/src/twr_gw.c`, `twr_radio.c`); the `north:~$` shell,
`rf_diag`, `p2p_status` and `radio tune` stay. It is accepted only together with
`-DBENCH_RTT_BRIDGE=ON`. It shares the codec (`twr_crypto.c`, `twr_frame.c`,
`twr_airtime.c`, `tests/tower_codec`) and the driver hooks with the gateway image, not its
protocol: P0 has one static key and one node, the gateway image everything below.

- **PHY:** LoRa BW125 CR4/5, preamble 8, explicit header, CRC on, the P2P private sync word,
  IQ normal, 14 dBm. Boot defaults match the P0 node image: **869.525 MHz, SF7**, gateway
  `0x4E420001`, node `0x00000413` (`-DTOWER_P0_NODE_ADDR=` at build time), static key
  `00..0f`.
- **Frame:** `ver_type|flags|src|dest|counter|payload|tag8`, little-endian, AES-128-CCM with
  the 14 B header as AAD and nonce `src‖ctr‖0(3)‖0(4)`. Frames up to 255 B are received.
- **ACK:** a CONFIRMED Data frame that is fresh (counter > last from that source) or an exact
  retransmit (counter == last, not delivered again) is ACKed `turnaround` ms after RxDone
  (default 20). Payload `acked ctr (u32 LE) ‖ rssi (i8) ‖ flags` with `PENDING` = bit 0 when
  a downlink is queued for the node. The ACK uses the gateway's own TX counter. The counter
  survives a warm reset (`.noinit`) but restarts at 1 after a power cycle (`twr ctr <n>`).
- **Downlink:** `twr pend <hex> [c]` queues one; it goes out `window` ms (default 20) after the
  PENDING ACK's TxDone and the queue empties. `c` sets CONFIRMED. `pend` refuses a frame
  whose time on air exceeds the driver's 4 s TX timeout (`lora_config()` hardcodes it; a cut
  TX blocks `lora_send()`) and warns if it would not fit the node's downlink window.
- **Shell:** `twr sf <7..12>`, `twr freq <Hz>`, `twr turnaround <20..60>`,
  `twr window <10..1000>`, `twr pend [<hex> [c] | clear]`, `twr node [<hex32>]`,
  `twr ctr [<next>]`, `twr stats [reset]`, `twr duty`, `twr show`.
- **Events** (channel 0, written directly like `P2P_OUT`; timestamps are µs from the 64-bit
  cycle counter, truncated to u32):

  ```text
  TWR_RX <frame hex> rssi= snr= t_rx_us= res=<ok|dup|replay|mic_fail|...> [type= src= dest= ctr=] [pt=<hex>]
  TWR_ACK t_tx_us= ctr= pending= ack_ctr= turnaround_us= toa_us= toa_calc_us= len=
  TWR_DL t_tx_us= len= toa_us= toa_calc_us= ctr= dt_us= pl_len= confirmed=
  TWR_RXERR kind=<crc|hdr> t_us=
  ```

  `toa_us` is measured (TX start to TxDone), `toa_calc_us` comes from the Semtech formula; both
  are recorded in the sliding-hour ledger shown by `twr duty`. The 1 % limit is reported, not
  enforced.

## Flash layout and nbboot (0.4.0)

Spec: proximos-v2 `plan/control/radio/northbridge_update.md` §3.1, §10. The Hub updates the
app through the STM32 ROM bootloader (AN3155 on USART1); a 2 KB stub in front of the app
decides on every reset whether the app may run.

| Address | Size | Partition (dts) | Content |
|---|---|---|---|
| `0x0800_0000` | 2 KB (page 0) | `boot_stub_partition` | `nbboot`, bare metal, never updated over the Hub |
| `0x0800_0800` | 236 KB (pages 1..118) | `slot0_partition`, `zephyr,code-partition` | the app (`CONFIG_FLASH_LOAD_OFFSET` 0x800) |
| `0x0803_B800` | 2 KB (page 119) | `app_valid_partition` | the 32 B `app_valid` record, the rest 0xFF |
| `0x0803_C000` | 16 KB | `storage_partition` | unchanged (unused today) |

**`app_valid` record** (LE): `0 magic "NBVA" 0x4156424E · 4 format 1 · 8 image_len ·
12 image_crc32 · 16 fw_ver[3] + 0 · 20 git_sha u32 · 24 rec_crc32 (bytes 0..23) ·
28 0xFFFFFFFF`. CRCs are CRC-32/ISO-HDLC (zlib). Valid when magic, format and `rec_crc32`
match, `0 < image_len ≤ 241664` and the CRC of `image_len` bytes from `0x0800_0800` equals
`image_crc32`. The Hub writes it last, after the image is verified; the C check is
`nbboot/src/nb_app_valid.c` (shared with the app and `tests/nbboot_record`), the Python twin
`tools/nb_app_valid.py`.

**Boot request word:** a u32 at `0x2000_FFF0`, the last 16 B of SRAM, outside Zephyr's
`sram0` (the dts shrinks it by 16 B, `CONFIG_SRAM_SIZE` floors to 63 KB). `0x4C42424E`
("NBBL") means "go to the ROM bootloader". It survives the reset because the option bytes
keep SRAM on a system reset (`SRAM_RST` = 1, as read on the bench NB in U0).

**`nbboot`** (`nbboot/`, plain CMake + the Zephyr SDK compiler, no Zephyr; 600 B), on reset:

1. If it was not entered from a reset (the ROM bootloader's `GO 0x0800_0000` arrives with
   clocks and peripherals set up), it marks the fact in the reserved 16 B and does one system
   reset, so the app always starts from reset state.
2. Reads and clears the boot request word.
3. Request "NBBL" or no valid record → sets `VTOR` and `MSP` from `0x1FFF_0000` and jumps
   to the ROM bootloader. Otherwise → `VTOR = 0x0800_0800`, `MSP` from the app's table,
   jumps to its reset vector (Zephyr sets `VTOR` again in `relocate_vector_table()`).

It runs on the reset clock (MSI 4 MHz) and touches no clock, GPIO or UART: the CRC of a
~100 KB image adds about 0.4 s to every boot (a full slot about 1 s). A fault or NMI in the
stub (a flash ECC double error in a half-programmed page) requests the ROM bootloader and
resets. The IWDG is in software mode (`IWDG_SW` = 1): it does not run after a reset, so
neither the stub nor the ROM bootloader is watched; the app starts it (8 s) as before.

**Build outputs** (every app build, `app/CMakeLists.txt`): `build/nbboot/nbboot.{elf,bin,hex}`,
and from `tools/nb_package.py` `build/zephyr/northbridge.bin` (the app padded with 0xFF to a
multiple of 8), `northbridge.json` (`format, board, load_addr, fw_version, git_sha,
image_len, crc32, sha256`), `app_valid.bin` and `northbridge_jlink.hex` (nbboot + app +
record). `west flash` programs `northbridge_jlink.hex` (board.cmake), so a J-Link flash
leaves a layout that boots; `zephyr.hex` alone has no record and would land in the ROM
bootloader. A 0.3.x Northbridge has neither the stub nor `ENTER_BOOTLOADER`: it needs that
one J-Link flash (`northbridge_update.md` §4) before the Hub can update it.

---

## Host↔Northbridge link

Frames are HDLC-framed (`0x7E` flag, `0x7D` escape), `type(1) | seq(1) | body | crc16(2)`,
little-endian bodies; radio frames and plaintexts are opaque byte strings. Every command is
answered by one `RSP` (`0x40 | cmd`) with the command's `seq` and a status byte first:
`0 OK`, `1 BAD_PARAM`, `2 BUSY`, `3 INVALID_STATE`, `4 LATE`, `5 INTERNAL`, `6 NO_SPACE`,
`7 NOT_FOUND`, `8 UNSUPPORTED` (an unknown or retired command). Every event takes the next
`seq` of one rolling counter, in the order events reach the wire; an event the TX ring had no
room for still takes its number, so the host sees the gap (and `link_err` counts it).

### States

`BOOT` → `SET_RADIO_CONFIG` → `CONFIGURED` → `TWR_CTR_BLOCK` + `TWR_START` → `GATEWAY`
(continuous receive), link §6.1. `RX_STOP` goes back to `CONFIGURED` and keeps the peer
table, the queue and the counter block; a reset (`EVT_BOOT`) returns to `BOOT` from any state
with everything empty. `GET_INFO` reports the state. The restore (`TWR_NODE_ADD`,
`TWR_QUEUE_PUSH`, `TWR_CTR_BLOCK`) runs in `CONFIGURED` before `TWR_START`; a radio change is
`RX_STOP` → `SET_RADIO_CONFIG` → `TWR_START`.

| Command | `BOOT` | `CONFIGURED` | `GATEWAY` |
|---|---|---|---|
| `GET_INFO`, `RX_STOP`, `TX_CANCEL`, `TWR_NODE_REMOVE`, `TWR_QUEUE_DROP`, `TWR_NODE_LIST`, `ENTER_BOOTLOADER` | ✓ | ✓ | ✓ |
| `SET_RADIO_CONFIG` | ✓ → `CONFIGURED` | ✓ | `INVALID_STATE` (`RX_STOP` first) |
| `TWR_NODE_ADD`, `TWR_CTR_BLOCK`, `TWR_QUEUE_PUSH`, `TX_SCHEDULE` | `INVALID_STATE` | ✓ | ✓ |
| `TWR_START` | `INVALID_STATE` | ✓ → `GATEWAY` (needs a counter block) | ✓ (idempotent) |

A malformed body is `BAD_PARAM` in every state: the body is decoded before the state is
checked.

### Commands (host → Northbridge)

| ID | Name | Northbridge specifics |
|---|---|---|
| `0x01` | `GET_INFO` | Empty body. 188 B payload, see [GET_INFO](#get_info-188-b). |
| `0x02` | `SET_RADIO_CONFIG` | Body unchanged (11 B: freq, SF 7–12, BW code, CR 1–4, preamble, power, flags). `BAD_PARAM` also for what this radio cannot run: a frequency outside 150–960 MHz, a power the board PA cannot produce (−16..15 dBm: RFO_LP with `rfo-lp-max-power = 15`, read from the devicetree like the driver does; no clamping), or a config in which even the 28 B ACK takes longer than 3.95 s on air (the driver cuts a TX at 4 s). It sets the time-on-air cap for downlinks: the longest frame that stays under 3.95 s. The config takes effect at the next `TWR_START` or `TX_SCHEDULE`. |
| `0x03` | `RX_START` | **Retired**, `UNSUPPORTED`. `TWR_START` enters receive. |
| `0x04` | `RX_STOP` | `GATEWAY` → `CONFIGURED`, `OK` in any state. The receiver is released on the radio thread after the RSP. |
| `0x05` | `TX_SCHEDULE` | The JoinAccept (raw frame, central-sealed) at an absolute `t_ms`, or now for `0`. `BAD_PARAM` for `len` 0 or a frame over the ToA cap; `LATE` under 20 ms of lead; `BUSY` while another one is pending or when its start falls inside the gateway's own ACK / downlink. If the gateway's ACK takes the radio anyway, or the TX cannot start by `t_ms` + 10 ms, `EVT_TX_DONE` says `LATE` and nothing is sent. `INTERNAL` without a radio. |
| `0x06` | `TX_CANCEL` | `OK` and `EVT_TX_DONE CANCELLED` for a pending one; `BAD_PARAM` for an unknown `tx_id` or one already on air. |
| `0x07` | `ENTER_BOOTLOADER` | Body exactly `4E 42 42 4C` ("NBBL"), anything else `BAD_PARAM`. `OK` in any state; after the RSP `0x47` has left the UART (at most 250 ms) the Northbridge writes the boot request word and does a cold reboot, and `nbboot` starts the ROM bootloader (AN3155, 8E1). See [Flash layout and nbboot](#flash-layout-and-nbboot-040). |
| `0x10` | `TWR_START` | `net_id` not `0`/`0xFFFFFFFF`, `turnaround_ms` and `dl_gap_ms` 20..60, flags bit 0 `ACK_ENABLED` only. `INVALID_STATE` before a counter block, `BAD_PARAM` while a peer holds `net_id`, `INTERNAL` when the radio is down. Again in `GATEWAY`: new timing, receive re-armed. |
| `0x11` | `TWR_NODE_ADD` | Upsert; a replaced peer's lane restarts at `last_seen` and its queue is dropped without events. `BAD_PARAM` for addr `0`, `0xFFFFFFFF`, `== net_id` (once started) or flag bits other than `HOME`; `NO_SPACE` at 256 peers. The key is wiped from the stack after the call. |
| `0x12` | `TWR_NODE_REMOVE` | Drops the peer and its queue, in-flight downlink included, without events. `NOT_FOUND` if absent. |
| `0x13` | `TWR_CTR_BLOCK` | `1 ≤ first ≤ last < 2³²−1`; replaces the block. The next counter never goes back (link §5.1): `last` below the current next is `BAD_PARAM` and the old block stays; a block starting below it continues from it. |
| `0x14` | `TWR_QUEUE_PUSH` | `len` 1..74 (the 96 B TOWER frame, #486; 78 before 0.4.1), `item` ≠ 0, `ttl_s` ≠ 0, flags bit 0 `CONFIRMED` only; `BAD_PARAM` for a frame over the current ToA cap, `NOT_FOUND` for an unknown peer, `NO_SPACE` at 32 queued frames. The same (addr, item) again replaces a queued entry (a host retry after a lost RSP is harmless) and answers `BUSY` while that item is on air or awaiting the node's ACK. |
| `0x15` | `TWR_QUEUE_DROP` | `item 0` = the node's whole queue (`OK` when empty); `NOT_FOUND` for an unknown peer or item. No events. |
| `0x16` | `TWR_GET_STATS` | **Retired**, `UNSUPPORTED`; its counters are in `GET_INFO`. |
| `0x17` | `TWR_NODE_LIST` | `start u16` → `total, next (0xFFFF = end), n, n × {addr, last_seen, flags}`, 24 per RSP, never a key. |
| `0x19` | `TWR_TIME_SYNC` | `now_ms u64 · unix_ms u64` (16 B, LE): at link time `now_ms` the wall time was `unix_ms`. The anchor for TimeAns in the ACK (#470 link §5.1, §6.5). Status-only RSP, any state, replaces the previous anchor; `BAD_PARAM` for another length or a `now_ms` ahead of the Northbridge's clock (a previous boot's). Lost at reset: until the host sends it again, TimeReqs go unanswered (`ack_time_no_anchor`). |

### Events (Northbridge → host)

| ID | Name | When |
|---|---|---|
| `0x80` | `EVT_BOOT` | Once after reset, **8 B**: `fw_ver[3] · git_sha[4] · reset_cause` (Zephyr's `RESET_*` bits: `0x10` watchdog). All state is gone. |
| `0x81` | `EVT_RX` | In `GATEWAY`, frames the gateway does not open: `dest == 0` (JoinReq) and an unknown `src` with `dest == net_id`. Body unchanged, `t_ms` = RxDone, `snr` in quarter dB. |
| `0x82` | `EVT_TX_DONE` | `TX_SCHEDULE` only: `OK` with `t_actual_ms` = TxDone, `LATE`, `RADIO_ERR`, `CANCELLED`. |
| `0x83` | `EVT_LOG` | Error reports, see [Diagnostics](#diagnostics). |
| `0x84` | `EVT_TWR_UPLINK` | A fresh uplink, after the ACK's TX result is known and before the downlink, so `ack` is exact: `1`/`2` the ACK went on air in time (with `PENDING`), `3` it went on air past the node's window or not at all (radio error), `4` no counter left, `0` not confirmed or not ACKed here; the code is in bits 0..5. Flags (0.3.2): b7 the sealed ACK carried a CTRL tail, b6 that tail holds a TimeAns (LinkCheck + Time without an anchor: b7 only); set when the tail went out: kept when the code becomes `3` for a late ACK, cleared when it is `3` for a radio error (nothing on air); set on re-ACKed repetitions too. It never delays the ACK. |
| `0x85` | `EVT_TWR_TX` | A downlink's outcome: `SENT` at the TxDone of an unconfirmed one; `DELIVERED` with `node_ack_counter` and `ack_rssi` when the node's fresh TOWER ACK acknowledges the downlink's `gw_counter`; `NOT_DELIVERED` when none came by the deadline; `EXPIRED` at the TTL; `RADIO_ERR` (a radio error, or the frame no longer fits a new radio config); `NO_COUNTER` when its turn came without a counter. |
| `0x86` | `EVT_TWR_CTR_LOW` | Once per block, after the spend that left fewer than 25 % of it: `next, last`. |

### Gateway behaviour

- **Receive** (§2.3): header → `dest == 0` raw up → `dest ≠ net_id` dropped (`not_for_us`) →
  unknown `src` raw up → only Data and Ack from a known `src` (`bad_hdr`) → CCM open under the
  peer key (`mic_fail`) → the replay rule: `counter > last_seen` fresh (forwarded,
  `last_seen` moves), `==` an exact retransmit (re-ACKed, not forwarded, `dup`), `<` dropped
  (`replay`). Counter 0 is a replay. A Data frame is forwarded once.
- **ACK:** a CONFIRMED Data frame, fresh or a retransmit, from a `HOME` peer with
  `ACK_ENABLED` gets a TOWER ACK (`acked_counter · rssi · flags`, `src = net_id`) sealed with
  a fresh gateway counter. `SetTx` is gated to `t_rx + turnaround_ms` (the RxDone stamp from
  the driver's DIO1 hook; the TX hook spins the last microseconds right before `SET_TX`).
  The ACK is always sent, however late it is handled. When its `SetTx` lands more than
  3 symbols + 20 ms after `t_rx + turnaround_ms` (the node's window, below) it counts in
  `acks_late` and `ack = 3`. `PENDING` is set when the peer has a queued downlink and two
  counters are left, the ACK's and the downlink's.
- **Answers in the ACK** (#470 TOWER §13.5): when the confirmed Data frame is a `0x91`
  envelope whose TLV list asks for LinkCheckReq `0x10` and/or TimeReq `0x20`, the ACK
  plaintext gets flag b1 `CTRL` and a TLV tail to its end: `0x10` LinkCheckAns
  `rssi i8 · snr i8 dB · margin i8 dB · gw_count 1` (the central's formula: SNR and the
  margin over the SF floor, −7.5 dB at SF7 and 2.5 dB lower per step, both rounded down),
  then `0x20` TimeAns `unix u32 · frac (1/256 s, rounded down) · req_counter u32`: the wall
  time at the end of the received frame (its RxDone stamp) through the `TWR_TIME_SYNC`
  anchor, `req_counter` its counter. Without an anchor TimeAns is left out
  (`ack_time_no_anchor`). A truncated TLV list gets no tail. A retransmit's re-ACK builds
  the tail anew from that reception. The longest ACK is 45 B (28 + 17), ≤ the 96 B TOWER
  frame; at SF7/BW125 it takes 92.4 ms on air instead of 66.8 ms. It is sealed on RxDone,
  before the `t_rx + turnaround_ms` gate (two more AES blocks). `EVT_TWR_UPLINK` carries
  the whole plaintext, the requests included; its `ack` gets b7 (tail) and b6 (TimeAns).
- **The node's window** (STICKER P1 net layer, sticker-firmware hynek/tower-p1-node
  `app/src/app_radio_p2p.c` :129–134, `p2p_twr_window_ms()` :774–780): after its TX the node
  listens for turnaround + ToA of the awaited frame + 3 symbols + 20 ms, at least 200 ms.
- **Downlink:** after a `PENDING` ACK went on air in time, the head of the peer's queue is
  sealed with a fresh counter and sent `dl_gap_ms` after the ACK's TxDone (after a late ACK
  it stays queued). One whose `SetTx` would land more than 3 symbols + 20 ms after that slot
  is not sent and goes back to the queue (its counter is spent; it is sealed again next
  time). A confirmed downlink then waits for the node's TOWER ACK until its TxDone +
  60 ms + ToA(28 B) + 3 symbols + 100 ms, one attempt; the central decides about a retry.
  An authenticated node ACK that matches nothing in flight moves nothing.
- **Counters:** spent right after sealing, before TX (#470 §4); none left → fail-closed:
  confirmed uplinks are forwarded with `ack = 4`, a head of the queue gets `NO_COUNTER`,
  nothing is sealed until a new block arrives.
- **Radio:** receive is continuous in `GATEWAY`; after every TX sequence (ACK, downlink,
  scheduled TX) RX is re-armed before the remaining events go out. A failed re-arm reports
  `EVT_LOG` code 2 and is retried every 1 s; the state stays `GATEWAY`.
- **Housekeeping** every 100 ms: TTL expiry, node-ACK deadlines, downlinks over a new ToA
  cap, `EVT_TWR_CTR_LOW`.
- **Duty cycle:** the sliding-hour ledger counts the measured airtime of every TX
  (`GET_INFO airtime_ms_last_hour`, `twr duty`); report-only in P2.
- **Clock:** one clock for every `t_ms` and `now_ms` on the link, the 64-bit cycle counter in
  milliseconds (`radio_ctl_now_ms()`); the driver hook stamps are cycles of the same counter.
- **Host link TX:** frames go through a 2048 B ring drained by the USART1 interrupt. A sender
  waits for room at most 50 ms (10 ms from the cooperative radio thread), then the frame is
  dropped and counted in `link_err`; the radio never waits on the UART.
- **Capacity:** 256 peers, 32 queued downlinks (the contract's floors are 256 and 16).

### GET_INFO (188 B)

The payload after the status byte, frozen by the flag day (pinned by
`tests/tower_gateway` `test_get_info_layout` and `tests/cmd_dispatch`):

| Offset | Field | Type |
|---|---|---|
| 0 | `fw_ver` | u8[3] |
| 3 | `git_sha` (first 4 bytes of the last commit that touched its `app/`, `boards/`, `patches/` or `west.yml`, display order; 0 without git) | u8[4] |
| 7 | `reset_cause` (this boot's) | u8 |
| 8 | `build_flags`: b0 bench RTT bridge, b1 debug optimisation | u8 |
| 9 | `now_ms` (ms since boot, the link's one clock; a drop means a reboot) | u64 |
| 17 | `state`: 0 `BOOT`, 1 `CONFIGURED`, 2 `GATEWAY` | u8 |
| 18 | `net_id` (0 before the first `TWR_START`) | u32 |
| 22 | `turnaround_ms` · 23 `dl_gap_ms` | u8, u8 |
| 24 | `modulation`: 0 LoRa | u8 |
| 25 | the `SET_RADIO_CONFIG` body echoed (all zero before it) | 11 B |
| 36 | `err_flags` (sticky since boot) | u32 |
| 40 | `last_err_code` · 41 `last_err_age_s` | u8, u32 |
| 45 | 25 counters, u32 each, in this order: `rx_total, rx_fresh, rx_dup, rx_replay, rx_mic_fail, rx_unknown_src, rx_join_fwd, rx_not_for_us, rx_bad_hdr, radio_crc_err, radio_hdr_err, rx_overrun, rx_rearm_fail, tx_total, tx_radio_err, acks_sent, acks_late, acks_suppressed, dl_sent, dl_delivered, dl_not_delivered, dl_expired, link_crc_err, link_err, evt_resent` | 100 B |
| 145 | `ack_turnaround_last_us` · 147 `ack_turnaround_max_us` (RxDone → ACK SetTx, saturating) | u16, u16 |
| 149 | `airtime_ms_last_hour` | u32 |
| 153 | `peers` · 155 `peers_max` · 157 `queue_used` · 159 `queue_max` | u16 × 4 |
| 161 | `evt_ring_used` · 162 `evt_ring_max` (0 / 0: no event ring yet) | u8, u8 |
| 163 | `ctr_next` · 167 `ctr_last` (0 / 0 without a block) | u32, u32 |
| 171 | `evt_head_seq` (the seq to expect next from the event ring) | u8 |
| 172 | `ack_ctrl_sent` (ACKs with a CTRL tail on air) · 176 `ack_time_no_anchor` (TimeReqs left without TimeAns for want of an anchor) | u32, u32 |
| 180 | `app_image_len` · 184 `app_crc32` from the `app_valid` record (0 / 0 when it is not valid), 0.4.0 | u32, u32 |

The table follows link §3.1: its field order is the byte order. `acks_sent` counts ACKs on
air; `acks_late` those of them whose `SetTx` landed past the node's window (a radio error
is `tx_radio_err`); `acks_suppressed`
confirmed frames not ACKed for want of a counter; `dl_not_delivered` includes the
`RADIO_ERR` and `NO_COUNTER` outcomes. `link_crc_err` counts frames this end rejected on the
host link, `link_err` framing errors, UART overruns and frames dropped for want of TX room:
both are about the cable and the host framer, never the antenna. `evt_resent` is 0.

### Contract status

The #470 link doc (840d2fb7) is normative; `p2p_tower_gateway.md` (2026-09-28, amendments
1–8) predates the flag day. This image implements the link doc's §2, §3, §3.1, §5 and §6.1
except for what the link doc itself schedules later or leaves in conflict:

- **Not yet (link §4, after the first E2E):** the event ring with `0x18 TWR_EVT_ACK` and
  resend (`0x18` answers `UNSUPPORTED`, `evt_resent`, `evt_ring_used` / `_max` are 0),
  `ack = 5` suppressed-ring-full, and `TWR_NODE_ADD` flag b1 `KEEP_NEWER` (`BAD_PARAM`
  today, as an undefined flag).
- **SNR:** `EVT_RX` and `EVT_TWR_UPLINK` carry quarter dB (`snr_q_db`, as in
  `p2p_tower_gateway.md` §2.2 and 0.2.x); the link doc §5.2 says whole dB. Unchanged until
  the coordinator settles it (frozen body).
- **`EVT_LOG`:** sent in every build (`northbridge_diagnostics.md` §2.2); the link doc says
  debug builds only.

Where the older contract text differs, as the coordinator set it:

- **Flag day:** no `proto_version` byte in `EVT_BOOT` or `GET_INFO` (the contract still
  says `proto_version = 2` in §1, §2, §3 and "proto detection" in §3.4); `EVT_BOOT` is the
  8 B above, `GET_INFO` the 188 B above; `0x03 RX_START` and `0x16 TWR_GET_STATS` (still in
  §2.1) answer `UNSUPPORTED`; the states and `8 UNSUPPORTED` are new.
- Amendment 1: `0x17 TWR_NODE_LIST`. Amendment 2: `TWR_QUEUE_PUSH` up to 78 B (74 B since 0.4.1, #486).
  Amendment 3: `SET_RADIO_CONFIG` unchanged, `BAD_PARAM` for a config the radio cannot run.
  Amendment 4: time on air capped under the driver's 4 s TX timeout (3.95 s), `BAD_PARAM`.
- The H2 message set is frozen with these; no ID or body changes without the coordinator.

### Diagnostics

Spec: proximos-v2 `plan/control/radio/northbridge_diagnostics.md` §2 (0.2.0), carried into
the 188 B `GET_INFO` above.

- **`err_flags`:** b0 radio init failed, b1 RX (re-)arm failed, b2 TX radio error, b3 HDLC
  frame error on the link, b4 UART overrun (hardware or ring full), b5 `TX_SCHEDULE`
  answered `INTERNAL`.
- **`EVT_LOG`** (`level | code | ASCII detail`, level 1 = error), at most one per code per
  10 s: `1` radio init failed, `2` RX (re-)arm failed, `3` TX radio error, `4` link framing
  error, `5` UART overrun; `6` (stack/heap near limit) is reserved. The Northbridge reports
  the fact; `GET_INFO` carries the counts in the meantime.
- **Watchdog:** the IWDG, 8 s, fed from the main loop and not fed while a TX has run longer
  than twice its time on air plus 2 s, so a radio that stops answering mid-transmission ends
  in a reset with `reset_cause` `0x10` instead of a silent gateway. Paused while a debugger
  halts the core.
- **A radio that fails to initialise** does not stop the firmware: the link comes up,
  `err_flags` b0 is set, and `TWR_START` / `TX_SCHEDULE` answer `INTERNAL`.

### Timestamps

`EVT_RX.t_ms`, `EVT_TWR_UPLINK.t_ms` and an `OK` `EVT_TX_DONE.t_actual_ms` are the RxDone /
TxDone moments, stamped from the cycle counter in the patched driver's DIO1 hook
(`sx126x_twr_dio1_hook()`) before loramac's `IrqProcess` chain runs (3–4 ms on the 2026-09-26
bench). The ACK gate and the downlink slot are computed from the same stamps.

---

## Radio

`SET_RADIO_CONFIG` parameters: frequency (Hz), SF 7–12, bandwidth (wire codes
`P2P_BW_125K/250K/500K` — **not** the Zephyr kHz enum), CR 4/5–4/8, preamble
length, TX power (dBm), and flags (`PUBLIC_SYNCWORD`, `IQ_INVERTED`).

- **RF switch:** the module's 3-GPIO antenna switch (PC3/PC4/PC5; RX = `1 1 1`,
  TX LP = `1 1 0`, OFF = `0 0 0`, per ST's HAL for STM32WL5Mxx) is driven by
  board hooks — Zephyr's stock `sx126x` driver only models a 2-GPIO switch, so a
  **patched driver** (`patches/lora/sx126x.c`) calls `board_rf_switch_tx/_rx/_off`
  and, since 0.2.2, substitutes the RX boosted gain (below). `app/CMakeLists.txt` compiles the patch in place of the
  vendored driver file, so `west update` no longer undoes it (see `README.md`).
- **0.2.1: `SX126xWaitOnBusy()` busy-waits in short steps** (`patches/lora/sx126x.c`) instead
  of `k_sleep(K_MSEC(1))`. BUSY normally clears in a few to a few tens of microseconds, but
  every SPI command waits on it (twice, in fact — `SX126xCheckDeviceReady()` before, this
  after), and `k_sleep(K_MSEC(1))` rounds each such wait up to a whole 1 ms tick
  (`CONFIG_SYS_CLOCK_TICKS_PER_SEC=1000`). With the ~15-20 commands a `lora_config()` +
  `Radio.Rx()`/`Radio.Send()` pair issues, that alone accounted for most of the 0.2.1 TX->RX
  turnaround bug (`docs/problems.md` §4). It now spins with `k_busy_wait()` in 20 µs steps for up to 10 ms
  before falling back to `k_sleep(K_MSEC(1))`, so a genuinely long operation still can't spin
  forever.
- **0.2.2: RX sensitivity tuning** (`app/src/radio_tune*.c`, `docs/problems.md` §1),
  carried into the gateway image unchanged (it is radio-level, not protocol). The
  patched driver calls three board hooks (`patches/lora/nb_sx126x_hooks.h`) where it would
  otherwise overwrite a setting:

  | Item | Where | Default | Changes TX |
  |---|---|---|---|
  | `rx_boost` | `0x08AC`: `0x94` → `0x96` in `SX126xSetRx()`'s write before every `SET_RX` | on, `0x96` | no |
  | `smps_rx` | `0x0923` bits 2:1 before `SET_RX` | on, 40 mA | no |
  | `regulator` | `SetRegulatorMode`; the driver sends DC-DC + LDO at init, off sends LDO | on (DC-DC) | yes |
  | `smps_tx` | `0x0923` bits 2:1 before `SET_TX`; off restores the chip's boot value | off (60 mA when on) | yes |
  | `pa_table` | `SetPaConfig` row by requested power (+15/+14/+10 dBm) | off | yes |
  | `ramp` | `SetTxParams` ramp 200 µs instead of 40 µs | off | yes |

  The TX items boot off, so a modem that resets mid-test (watchdog, power) comes back with
  the stock TX path; they are switched on over RTT for a test.

  Kconfig (`app/Kconfig.radio_tune`) sets the boot state, `CONFIG_NB_RADIO_TUNE=n` builds the
  stock driver behaviour (power-saving gain, chip-default SMPS drive, 40 µs ramp); the boot
  log prints `radio tune: …`. RX items take effect when RX is armed, TX items at the next
  TX. Image calibration (`CalibrateImage 0xD7 0xDB`, 863–870 MHz) is the driver's own.
- **Threads and stacks:** the sx126x DIO1 work runs the loramac RxDone chain on the system
  workqueue (2048 B; 4096 B in the bench build), which only stamps and queues the frame
  (8 deep, `rx_overrun` when full). Everything that touches the radio runs on `twr_wq`, a
  cooperative work queue with a 3072 B stack: classification, the ACK and downlink, the
  scheduled TX, RX arm and release, housekeeping and the `radio tune` calls. The main thread
  runs the host commands and never waits on the radio.
- **Driver hooks** (`patches/lora/sx126x.c`, `NB_TWR_HOOKS`, always built):
  `sx126x_twr_dio1_hook()` stamps RxDone / TxDone and counts CRC and header errors;
  `sx126x_twr_tx_hook()` spins to the TX gate as the last thing before `SET_TX`, after the
  tuning writes; `sx126x_twr_standby_xosc()` keeps the oscillator up between the ACK and
  the downlink.

---

## Debug shell (RTT, channel 0)

Always available over the J-Link/SWD RTT console (independent of the host link). Nothing on
it prints a key or a plaintext.

- `rf_diag` — print the RF-switch GPIO states (PC3/PC4/PC5).
- `p2p_status` — uptime, the diagnostics, link counters (CRC errors, TX drops), the radio
  config and one gateway summary line.
- `p2p show` — state, radio config, timing, counter block. `p2p stats` — the counters since
  boot, the ones `GET_INFO` does not carry included (scheduled TX outcomes, unmatched node
  ACKs, late downlinks, the last ACK and downlink ToA, the ACK answers: `ack ctrl: sent=
  link_check= time= time_no_anchor= anchor=`). `p2p peers [<start>]` — the peer table,
  16 per page (addr, `last_seen`, flags). `p2p queue` — the downlink queue. `p2p duty` — the
  sliding-hour airtime against 1 %. `p2p listen` — factory capture: from BOOT only, receive on
  the shipped channel (869.525 MHz, SF7, 125 kHz, 4/5) with net id 0 and ACKs off, so nothing
  is sent; `p2p stats` counts what was heard.
- `nb_selftest log <code>` — report a diagnostics error as the firmware would (HIL of the
  `EVT_LOG` path); `nb_selftest wdt` — stop feeding the watchdog, which must reset the
  Northbridge within 8 s.
- `radio tune show` — the tuning items, their state and parameter.
  `radio tune set <item|all> on|off|<value>` — switch one (or all), or set its parameter
  (`rx_boost 0x96|0x97`, `smps_rx`/`smps_tx 20|40|60|100` mA), then re-send the standby
  settings and re-arm RX on the radio work queue (RX paused for a few ms).
  `radio tune apply` — the same without a change. `radio tune regs` — read back `0x08AC`,
  the `0x029F` retention list, `0x0923`, OCP `0x08E7`, chip status and `GetDeviceErrors`, the
  MCU's PWR SMPS flags, and the last `SetRegulatorMode`/`SetPaConfig`/`SetTxParams`/
  `CalibrateImage` the driver sent. A radio asleep (RX off) is woken for the readback.
  A/B on the same Node frames: alternate `radio tune set rx_boost off` / `on` (or another
  item) between uplinks and compare SNR and frame loss at the edge of range, not RSSI up
  close. `all on` also switches the TX items on; a reset brings back the boot state.
- `p2p_inject <hex>` — **bench build only** (`-DBENCH_RTT_BRIDGE=ON`): feed raw
  bytes to the HDLC decoder as if received over USART1.

---

## Testing

- **native_sim ztests** (no hardware, `west twister -T tests -p native_sim/native/64
  --force-platform`):
  - `hdlc_framing` — framing and CRC;
  - `cmd_dispatch` — the command path (`cmd_dispatch.c`, `twr_cmd.c`) over the real codec,
    core and diagnostics with the radio runtime faked: every command's status in every
    state, the retired commands, `GET_INFO` filled from the right sources, `EVT_BOOT`;
  - `proto_integration` — raw HDLC bytes → decode → dispatch → RSP, 0x7E/0x7D in radio
    frames, keys and downlinks;
  - `tower_codec` — FIPS-197, CCM and the #470 TOWER frame KAT
    (`tests/tower_codec/ccm/tower_frame_kat.json`);
  - `tower_gateway` — the core against the same KAT (open every uplink, produce the ACK and
    downlink frames byte for byte), the replay rule, ACK + `PENDING`, confirmed downlinks,
    TTL, the counter block and its fail-closed end, the peer table and the queue; the link
    codec with the frozen `EVT_BOOT` and `GET_INFO` layouts;
  - `radio_tune` — the tuning table, its defaults, the RX gain substitution, and each hook's
    register / command operations in order;
  - `nbboot_record` — `nbboot`'s record check and boot decision (`nb_app_valid.c`): the CRC,
    a pinned record shared with `tools/test_nb_package.py`, every header bit, erased flash,
    the length range, a corrupt image, the "NBBL" request.
- **`tools/test_nb_package.py`** (`python3 -m pytest tools/` or run directly): the record
  builder and `nb_package.py`'s outputs (padding, manifest, the J-Link hex).
- `twr_gateway.c` (the SX126x side: gating, stamps, RX re-arm) is exercised only on
  hardware.
- **`tests/hil_p2p/`** — the two-probe Python HIL speaks proto 1 (the 0.2.x image) and is
  stale against 0.3.0; `nb_rtt_bridge.py` is a byte pipe and works with either.
- **HIL on the Hub, 2026-09-26 (0.2.0)** — boot and `GET_INFO` diagnostics, the `EVT_LOG` rate
  limit, the watchdog reset (`reset_cause` 0x11) and a bad-CRC frame on `ttyAMA3` were checked
  on the 0.2.0 image with `control-radio` as the host; 0.3.0 has not been on hardware yet.

## Known limitations

See [`docs/problems.md`](problems.md) for the full log. The weak-signal issue (§1) is
largely explained: the TX half was the RF-switch table (fixed in 0.1.1); on 2026-09-26 the
Northbridge received a STICKER at −58 dBm against −51 dBm on the RAK5146 in the same Hub
with identical antennas — 7 dB, not 60. The RX boost A/B on the bench Hub is recorded there.
