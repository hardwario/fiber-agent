/* SPDX-License-Identifier: Apache-2.0
 *
 * Link codec of the gateway image: the TWR_* command and event bodies of
 * p2p_tower_gateway.md §2, EVT_BOOT, GET_INFO, SET_RADIO_CONFIG, EVT_RX and EVT_TX_DONE, all
 * little-endian. Decoders check the length and every range a field carries on its own
 * (BAD_PARAM); what depends on gateway state (NOT_FOUND, NO_SPACE, addr == net_id, ...) is
 * twr_gwc.c's. Plain C, no Zephyr (tests/tower_gateway).
 *
 * Stricter than the contract spells out, on purpose: flag bits the contract does not define,
 * a QUEUE_PUSH item 0 / ttl_s 0 / len 0 are BAD_PARAM rather than ignored, so a central and
 * a Northbridge that disagree on the frozen message set fail loudly at the first command.
 */
#ifndef APP_TWR_LINK_H_
#define APP_TWR_LINK_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "proto.h"
#include "radio_ctl.h"
#include "twr_crypto.h"

/* ---- identity, EVT_BOOT, GET_INFO ------------------------------------------------------ */

struct twr_identity {
	uint8_t fw_ver[3];
	uint8_t git_sha[4]; /* display order */
	uint8_t reset_cause;
	uint8_t build_flags; /* P2P_BUILD_* */
};

/* EVT_BOOT: fw_ver[3] git_sha[4] reset_cause, P2P_EVT_BOOT_BODY_LEN bytes. */
size_t twr_link_enc_boot(const struct twr_identity *id, uint8_t out[P2P_EVT_BOOT_BODY_LEN]);

/* GET_INFO RSP payload (after the status byte), P2P_GET_INFO_LEN bytes:
 *     0 fw_ver[3] · 3 git_sha[4] · 7 reset_cause u8 · 8 build_flags u8
 *     9 now_ms u64
 *    17 state u8 · 18 net_id u32 · 22 turnaround_ms u8 · 23 dl_gap_ms u8
 *    24 modulation u8 · 25 freq_hz u32 · 29 sf · 30 bw · 31 cr · 32 preamble u16
 *       · 34 tx_power_dbm i8 · 35 flags (= modulation + the SET_RADIO_CONFIG body)
 *    36 err_flags u32 · 40 last_err_code u8 · 41 last_err_age_s u32
 *    45 counters, P2P_INFO_CTR_COUNT x u32 in enum p2p_info_ctr order
 *   145 ack_turnaround_last_us u16 · 147 ack_turnaround_max_us u16
 *   149 airtime_ms_last_hour u32
 *   153 peers u16 · 155 peers_max u16 · 157 queue_used u16 · 159 queue_max u16
 *       · 161 evt_ring_used u8 · 162 evt_ring_max u8 · 163 ctr_next u32 · 167 ctr_last u32
 *   171 evt_head_seq u8: the seq the host should expect next from the ring - the oldest
 *       unacked event, or the next one to be handed out when the ring is empty (1 after
 *       boot). Lets a (re)started host start at the head instead of guessing.
 *   172 ack_ctrl_sent u32: ACKs with a CTRL tail on air · 176 ack_time_no_anchor u32: TimeReqs
 *       left without TimeAns for want of a TWR_TIME_SYNC anchor (#470 §13.5, 0.3.2)
 *   180 app_image_len u32 · 184 app_crc32 u32: the running image's app_valid record
 *       (northbridge_update.md §10), both 0 when the record is not valid (0.4.0)
 */
struct twr_info {
	struct twr_identity id;
	uint64_t now_ms;
	uint8_t state; /* enum p2p_nb_state */
	uint32_t net_id;
	uint8_t turnaround_ms;
	uint8_t dl_gap_ms;
	uint8_t modulation;
	struct radio_config radio; /* all zero before SET_RADIO_CONFIG */
	uint32_t err_flags;
	uint8_t last_err_code;
	uint32_t last_err_age_s;
	uint32_t ctr[P2P_INFO_CTR_COUNT];
	uint16_t ack_turnaround_last_us;
	uint16_t ack_turnaround_max_us;
	uint32_t airtime_ms_last_hour;
	uint16_t peers;
	uint16_t peers_max;
	uint16_t queue_used;
	uint16_t queue_max;
	uint8_t evt_ring_used;
	uint8_t evt_ring_max;
	uint32_t ctr_next;
	uint32_t ctr_last;
	uint8_t evt_head_seq;
	uint32_t ack_ctrl_sent;
	uint32_t ack_time_no_anchor;
	uint32_t app_image_len;
	uint32_t app_crc32;
};

size_t twr_link_enc_info(const struct twr_info *in, uint8_t out[P2P_GET_INFO_LEN]);

/* ---- modem messages kept from proto 1 --------------------------------------------------- */

/* SET_RADIO_CONFIG: P2P_RADIO_CONFIG_BODY_LEN bytes, sf 7..12, bw P2P_BW_*, cr 1..4. What
 * this radio can run (frequency, power, time on air) is radio_ctl_set_config()'s check. */
enum p2p_status twr_link_dec_radio_config(const uint8_t *b, size_t n, struct radio_config *out);

/* The same P2P_RADIO_CONFIG_BODY_LEN bytes back (GET_INFO). */
size_t twr_link_enc_radio_config(const struct radio_config *c,
				 uint8_t out[P2P_RADIO_CONFIG_BODY_LEN]);

/* EVT_RX: t_ms u64, rssi i16, snr_q i8, reserved u8, len u8, frame. 0 when over `cap`. */
size_t twr_link_enc_rx(uint64_t t_ms, int16_t rssi_dbm, int8_t snr_q_db, const uint8_t *frame,
		       size_t len, uint8_t *out, size_t cap);

/* EVT_TX_DONE: tx_id, status (enum p2p_tx_done_status), t_actual_ms u64 (the TxDone). */
size_t twr_link_enc_tx_done(uint8_t tx_id, uint8_t status, uint64_t t_actual_ms,
			    uint8_t out[P2P_EVT_TX_DONE_BODY_LEN]);

/* ENTER_BOOTLOADER: OK only for the exact P2P_ENTER_BOOTLOADER_BODY_LEN byte magic. */
enum p2p_status twr_link_dec_enter_bootloader(const uint8_t *b, size_t n);

/* ---- TWR_* ------------------------------------------------------------------------------ */

struct twr_start_cmd {
	uint32_t net_id;
	uint8_t turnaround_ms;
	uint8_t dl_gap_ms;
	uint8_t flags;
};

struct twr_node_add_cmd {
	uint32_t addr;
	uint8_t key[TWR_AES_KEY_LEN];
	uint32_t last_seen;
	uint8_t flags;
};

struct twr_ctr_block_cmd {
	uint32_t first;
	uint32_t last;
};

struct twr_queue_push_cmd {
	uint32_t addr;
	uint16_t item;
	uint16_t ttl_s;
	uint8_t flags;
	uint8_t len;
	const uint8_t *pt; /* points into the command body */
};

struct twr_queue_drop_cmd {
	uint32_t addr;
	uint16_t item;
};

/* TWR_START: net_id not 0 / 0xFFFFFFFF, turnaround_ms and dl_gap_ms 20..60, flags bit 0. */
enum p2p_status twr_link_dec_start(const uint8_t *b, size_t n, struct twr_start_cmd *out);

/* TWR_NODE_ADD: addr not 0 / 0xFFFFFFFF, flags bits 0 (HOME) and 1 (KEEP_NEWER). */
enum p2p_status twr_link_dec_node_add(const uint8_t *b, size_t n, struct twr_node_add_cmd *out);

/* TWR_NODE_REMOVE: addr not 0 / 0xFFFFFFFF. */
enum p2p_status twr_link_dec_node_remove(const uint8_t *b, size_t n, uint32_t *addr);

/* TWR_CTR_BLOCK: 1 <= first <= last < 2^32 - 1. */
enum p2p_status twr_link_dec_ctr_block(const uint8_t *b, size_t n,
				       struct twr_ctr_block_cmd *out);

/* TWR_QUEUE_PUSH: n == 10 + len, item != 0, ttl_s != 0, flags bit 0, 1 <= len <= 78. */
enum p2p_status twr_link_dec_queue_push(const uint8_t *b, size_t n,
					struct twr_queue_push_cmd *out);

/* TWR_QUEUE_DROP: any item, 0 = all. */
enum p2p_status twr_link_dec_queue_drop(const uint8_t *b, size_t n,
					struct twr_queue_drop_cmd *out);

/* TWR_TIME_SYNC: now_ms u64, unix_ms u64; any values (the NB-clock check is the core's). */
enum p2p_status twr_link_dec_time_sync(const uint8_t *b, size_t n, uint64_t *now_ms,
				       uint64_t *unix_ms);

/* TWR_NODE_LIST */
enum p2p_status twr_link_dec_node_list(const uint8_t *b, size_t n, uint16_t *start);

/* EVT_TWR_UPLINK */
struct twr_uplink_evt {
	uint64_t t_ms;
	int16_t rssi_dbm;
	int8_t snr_q_db;
	uint8_t frame_flags;
	uint32_t addr;
	uint32_t counter;
	uint8_t ack; /* enum p2p_twr_ack */
	uint8_t len;
	const uint8_t *pt;
};

/* Returns the body length, 0 when it does not fit `cap`. */
size_t twr_link_enc_uplink(const struct twr_uplink_evt *e, uint8_t *out, size_t cap);

struct twr_tx_evt;

/* EVT_TWR_TX, P2P_EVT_TWR_TX_BODY_LEN bytes. */
size_t twr_link_enc_tx(const struct twr_tx_evt *e, uint8_t out[P2P_EVT_TWR_TX_BODY_LEN]);

/* EVT_TWR_CTR_LOW, P2P_EVT_TWR_CTR_LOW_BODY_LEN bytes. */
size_t twr_link_enc_ctr_low(uint32_t next, uint32_t last,
			    uint8_t out[P2P_EVT_TWR_CTR_LOW_BODY_LEN]);

/* Hex digits (either case, no separators) to bytes: the byte count, -1 for an odd length, a
 * non-hex digit, an empty string or more than `cap` bytes. For the bench shell (`twr
 * rx_inject`). */
int twr_link_hex_decode(const char *hex, uint8_t *out, size_t cap);

/* SNR in whole dB to the wire's quarter-dB i8, clamped (EVT_RX, EVT_TWR_UPLINK). */
int8_t twr_link_snr_q(int8_t snr_db);

#endif /* APP_TWR_LINK_H_ */
