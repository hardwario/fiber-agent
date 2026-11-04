/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER radio frame codec (bench, TOWER_P0_GATEWAY): the clear header that doubles as the CCM
 * AAD, the per-frame nonce, seal/open, the ACK payload and the per-source replay window.
 * Mirrors tower-firmware src/radio/frame.rs and crates/tower-net-core {nonce,ack,replay}.rs;
 * plain C with no Zephyr dependency so tests/tower_codec runs it on native_sim.
 *
 * Wire layout, little-endian:
 *   ver_type(1) | flags(1) | src(4) | dest(4) | counter(4) [| bulk_idx(3)] | ct | tag(8)
 * ver_type = VERSION << 5 | type, so a v1 Data frame starts 0x20 and an Ack 0x21. The
 * 3-byte bulk index is present only for BulkReq/BulkData. Nonce (13 B) =
 * src(4 LE) | counter(4 LE) | bulk_idx(3 LE, 0 when absent) | 00 00.
 */
#ifndef APP_TWR_FRAME_H_
#define APP_TWR_FRAME_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "twr_crypto.h"

#define TWR_VERSION      1u
#define TWR_HDR_LEN      14u
#define TWR_HDR_LEN_BULK 17u
#define TWR_TAG_LEN      TWR_CCM_TAG_LEN
/* Upstream TOWER caps a payload at 74 B (96 B frame), and so does every STICKER since
 * sticker-firmware #486 (U-1). TWR_MAX_* stay at the LoRa PHY limit (233 B plaintext, 255 B
 * frame) for the codec and the bench P0 image; the gateway core refuses a received frame
 * over TWR_TOWER_MAX_FRAME (twr_gwc_on_rx). */
#define TWR_MAX_FRAME          255u
#define TWR_MAX_PAYLOAD        (TWR_MAX_FRAME - TWR_HDR_LEN - TWR_TAG_LEN)
#define TWR_TOWER_MAX_PAYLOAD  74u
#define TWR_TOWER_MAX_FRAME    96u

enum twr_type {
	TWR_T_DATA = 0,
	TWR_T_ACK = 1,
	TWR_T_BULK_REQ = 2,
	TWR_T_BULK_DATA = 3,
	TWR_T_JOIN_REQ = 4,
	TWR_T_JOIN_RESP = 5,
	TWR_T_JOIN_CONFIRM = 6,
	TWR_T_BEACON = 7,
};

/* Header flags byte */
#define TWR_FLAG_CONFIRMED     0x01u
#define TWR_FLAG_LAST_CHUNK    0x04u
#define TWR_FLAG_BULK_ANNOUNCE 0x08u

/* ACK payload: acked_counter(u32 LE) | rssi(i8) | flags; flags bit0 = downlink pending, bit1
 * CTRL = a TLV tail follows to the end (#470 §13.5, twr_ackctrl.h). */
#define TWR_ACK_PAYLOAD_LEN  6u
#define TWR_ACK_FLAG_PENDING 0x01u
#define TWR_ACK_FLAG_CTRL    0x02u

struct twr_hdr {
	uint8_t type;
	uint8_t flags;
	uint32_t src;
	uint32_t dest;
	uint32_t counter;
	uint32_t bulk_idx; /* 24-bit; meaningful only for BulkReq/BulkData */
};

enum twr_err {
	TWR_OK = 0,
	TWR_E_SHORT,     /* shorter than header + tag */
	TWR_E_VERSION,   /* ver_type >> 5 != 1 */
	TWR_E_TYPE,      /* type outside 0..7 */
	TWR_E_TOO_LONG,  /* payload over the cap / buffer too small */
	TWR_E_MIC,       /* CCM tag mismatch */
};

const char *twr_err_str(enum twr_err e);

bool twr_type_has_bulk(uint8_t type);
size_t twr_hdr_len(uint8_t type);

/* Write the clear header; returns its length (14 or 17). `out` holds TWR_HDR_LEN_BULK. */
size_t twr_hdr_encode(const struct twr_hdr *h, uint8_t *out);

/* Parse the clear header at the start of buf. */
enum twr_err twr_hdr_parse(const uint8_t *buf, size_t len, struct twr_hdr *h, size_t *hdr_len);

void twr_nonce(uint32_t src, uint32_t counter, uint32_t bulk_idx,
	       uint8_t nonce[TWR_CCM_NONCE_LEN]);

/* header | CCM(pt) | tag into out. `max_payload` is the caller's cap (TWR_TOWER_MAX_PAYLOAD
 * for strict TOWER, TWR_MAX_PAYLOAD for the P0 node). Returns the frame length, 0 on error. */
size_t twr_frame_seal(const struct twr_aes128 *aes, const struct twr_hdr *h, const uint8_t *pt,
		      size_t pt_len, size_t max_payload, uint8_t *out, size_t out_cap);

/* Parse, authenticate and decrypt `frame` into `pt` (capacity TWR_MAX_PAYLOAD suffices).
 * The header is valid in *h whenever the result is TWR_OK or TWR_E_MIC. */
enum twr_err twr_frame_open(const struct twr_aes128 *aes, const uint8_t *frame, size_t len,
			    struct twr_hdr *h, uint8_t *pt, size_t pt_cap, size_t *pt_len);

/* ACK payload. rssi is clamped into i8. */
void twr_ack_payload_encode(uint32_t acked_counter, int rssi_dbm, bool pending,
			    uint8_t out[TWR_ACK_PAYLOAD_LEN]);

struct twr_ack_meta {
	uint32_t acked_counter;
	bool has_rssi;
	int8_t rssi;
	bool pending;
};

/* Accepts len >= 4 like tower-net-core ack_meta (rssi / flags optional). */
bool twr_ack_payload_parse(const uint8_t *p, size_t len, struct twr_ack_meta *m);

/* ---- Replay window: one lane per source, strictly increasing counters --------------- */

enum twr_replay {
	TWR_RP_FRESH,      /* counter > last seen: deliver */
	TWR_RP_RETRANSMIT, /* counter == last seen: re-ACK if confirmed, do not deliver */
	TWR_RP_REPLAY,     /* counter < last seen, or 0: drop */
};

#define TWR_REPLAY_LANES 8u

struct twr_replay_lane {
	bool used;
	uint32_t src;
	uint32_t last;
};

struct twr_replay_tbl {
	struct twr_replay_lane lane[TWR_REPLAY_LANES];
	uint8_t next_evict;
};

void twr_replay_init(struct twr_replay_tbl *t);

/* Classify without recording. */
enum twr_replay twr_replay_check(const struct twr_replay_tbl *t, uint32_t src, uint32_t ctr);

/* Classify and, for FRESH, record ctr as the lane's last seen (a new source takes a free lane
 * or evicts round-robin). Only authenticated Data frames should be fed here. */
enum twr_replay twr_replay_accept(struct twr_replay_tbl *t, uint32_t src, uint32_t ctr);

const char *twr_replay_str(enum twr_replay r);

#endif /* APP_TWR_FRAME_H_ */
