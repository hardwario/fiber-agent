/* SPDX-License-Identifier: Apache-2.0
 *
 * Radio seam of the gateway image, implemented by twr_gateway.c: the radio config, the
 * Northbridge state, RX_STOP, the host-scheduled TX (the JoinAccept) and the radio counters.
 * Kept separate from cmd_dispatch.c so unit tests can link it against a fake instead of the
 * Zephyr LoRa driver - see tests/cmd_dispatch.
 */
#ifndef APP_RADIO_CTL_H_
#define APP_RADIO_CTL_H_

#include <stdbool.h>
#include <stdint.h>

#include "proto.h"

struct radio_config {
	uint32_t frequency_hz;
	uint8_t sf;          /* 7..12 */
	uint8_t bw;           /* P2P_BW_* wire code, not a Zephyr enum value */
	uint8_t cr;           /* 1..4 = 4/5..4/8 */
	uint16_t preamble_len;
	int8_t tx_power_dbm;
	uint8_t flags;        /* P2P_RADIO_FLAG_* */
};

/* Radio counters for GET_INFO (rx_rearm_fail and tx_radio_err are diag.c's). */
struct radio_ctl_stats {
	uint32_t crc_err;    /* RxDone with a CRC error; the driver drops the frame */
	uint32_t hdr_err;    /* LoRa header errors */
	uint32_t rx_overrun; /* received frames lost because the receive queue was full */
	uint32_t tx_total;   /* transmissions that went on air: ACKs, downlinks, TX_SCHEDULE */
	uint16_t ack_turnaround_last_us; /* RxDone -> ACK TX start, saturating */
	uint16_t ack_turnaround_max_us;
};

/* Brings up the LoRa device and the gateway runtime. Returns 0 on success. */
int radio_ctl_init(void);

enum p2p_nb_state radio_ctl_state(void);

/* Validates and stores cfg (BOOT / CONFIGURED -> CONFIGURED); it takes effect at the next
 * TWR_START or TX_SCHEDULE. BAD_PARAM for a config this radio cannot run: a frequency
 * outside the SX126x range, a power the board PA cannot produce (from the devicetree, no
 * clamping; -16..15 dBm here), or one where even the 28 B ACK would take longer than the
 * driver's 4 s TX timeout. INVALID_STATE in GATEWAY (a radio change is RX_STOP ->
 * SET_RADIO_CONFIG -> TWR_START). */
enum p2p_status radio_ctl_set_config(const struct radio_config *cfg);

/* Returns false (and zeroes *out) if SET_RADIO_CONFIG was never accepted since boot. */
bool radio_ctl_get_config(struct radio_config *out);

/* Leaves gateway receive (GATEWAY -> CONFIGURED); the tables stay. OK in any state. */
enum p2p_status radio_ctl_rx_stop(void);

/* t_ms == P2P_TX_IMMEDIATE transmits right away. frame/len is copied. INVALID_STATE before
 * SET_RADIO_CONFIG, BAD_PARAM over the time-on-air cap, BUSY when a scheduled TX is pending
 * or the gateway's own ACK / downlink holds the radio then, LATE under P2P_TX_MIN_LEAD_MS. */
enum p2p_status radio_ctl_tx_schedule(uint8_t tx_id, uint64_t t_ms,
				       const uint8_t *frame, uint8_t len);
enum p2p_status radio_ctl_tx_cancel(uint8_t tx_id);

void radio_ctl_get_stats(struct radio_ctl_stats *out);

/* The Northbridge's monotonic millisecond clock - the timestamp domain every t_ms and
 * GET_INFO now_ms uses. */
uint64_t radio_ctl_now_ms(void);

/* True while a transmission has been running far longer than its time on air: a radio
 * that stopped answering (main.c stops feeding the watchdog). */
bool radio_ctl_tx_stuck(void);

/* 0.2.2: runs fn(arg) on the radio work queue, serialized with scheduled TX, and waits for it
 * (the `radio tune` shell). With `pause_rx`, async RX is released first and re-armed with the
 * current radio config afterwards, so RX-side tuning takes effect. Returns 0; -ENODEV without
 * a radio; -EIO if RX could not be re-armed (EVT_LOG code 2, as after a TX). */
int radio_ctl_call(void (*fn)(void *arg), void *arg, bool pause_rx);

#endif /* APP_RADIO_CTL_H_ */
