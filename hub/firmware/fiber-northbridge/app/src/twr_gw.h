/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER P0 bench gateway logic, radio-free: classify a received frame, build the auto-ACK
 * and the one queued downlink. twr_radio.c drives it from the SX126x; tests/tower_codec drives
 * it from the KAT vectors on native_sim.
 *
 * Behaviour follows tower-firmware's gateway: a frame for another dest is dropped before CCM;
 * CCM is verified before the replay window is consulted; only authenticated Data frames move
 * the per-source window; a CONFIRMED Data frame that is fresh or an exact retransmit is ACKed
 * (the retransmit is not delivered again); the ACK carries the gateway's own TX counter,
 * spent once the frame is sealed.
 */
#ifndef APP_TWR_GW_H_
#define APP_TWR_GW_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "twr_crypto.h"
#include "twr_frame.h"

#define TWR_ACK_FRAME_LEN (TWR_HDR_LEN + TWR_ACK_PAYLOAD_LEN + TWR_TAG_LEN)

enum twr_rx_res {
	TWR_RX_OK,         /* fresh authenticated Data: delivered */
	TWR_RX_DUP,        /* counter == last seen: re-ACK if confirmed, not delivered */
	TWR_RX_REPLAY,     /* counter < last seen (or 0): dropped */
	TWR_RX_MIC_FAIL,   /* CCM tag mismatch */
	TWR_RX_SHORT,
	TWR_RX_BAD_VER,
	TWR_RX_BAD_TYPE,
	TWR_RX_NOT_FOR_US, /* dest != gateway address; not authenticated */
	TWR_RX_ACK,        /* authenticated Ack frame (the gateway sends none that need one) */
	TWR_RX_OTHER,      /* authenticated frame of a type P0 does not handle */
};

const char *twr_rx_res_str(enum twr_rx_res r);

struct twr_gw {
	struct twr_aes128 aes;
	uint32_t gw_addr;
	uint32_t node_addr; /* downlink target; PENDING is only ever reported to this source */
	uint32_t tx_counter; /* next counter to use; 1 after init */
	bool tx_locked;      /* counter reached 2^32 - 1: no more frames (tower-net-core) */
	struct twr_replay_tbl replay;

	bool dl_queued;
	bool dl_confirmed;
	uint8_t dl_len;
	uint8_t dl[TWR_MAX_PAYLOAD];
};

struct twr_rx_out {
	enum twr_rx_res res;
	bool hdr_valid; /* hdr parsed (res OK..MIC_FAIL, NOT_FOR_US, ACK, OTHER) */
	struct twr_hdr hdr;
	size_t pt_len; /* authenticated plaintext in pt (OK, DUP, ACK, OTHER) */
	uint8_t pt[TWR_MAX_PAYLOAD];

	bool send_ack;    /* ack_frame holds a sealed Ack to transmit */
	bool ack_pending; /* ...with the PENDING flag set */
	uint32_t ack_ctr; /* the gateway TX counter the Ack was sealed with */
	size_t ack_len;
	uint8_t ack_frame[TWR_ACK_FRAME_LEN];
};

void twr_gw_init(struct twr_gw *gw, uint32_t gw_addr, uint32_t node_addr,
		 const uint8_t key[TWR_AES_KEY_LEN]);

/* Process one received LoRa frame; rssi_dbm goes into the ACK payload. */
void twr_gw_on_rx(struct twr_gw *gw, const uint8_t *frame, size_t len, int rssi_dbm,
		  struct twr_rx_out *out);

/* Queue the single downlink (replaces any queued one). len <= TWR_MAX_PAYLOAD. */
bool twr_gw_queue_dl(struct twr_gw *gw, const uint8_t *payload, size_t len, bool confirmed);

/* Seal the queued downlink as a Data frame gw -> node_addr, spend a TX counter and clear the
 * queue. Returns the frame length (0 = nothing queued or counter locked). */
size_t twr_gw_build_dl(struct twr_gw *gw, uint8_t *out, size_t cap, uint32_t *ctr);

/* Set the next TX counter (bench recovery after a gateway reboot). 0 is not a valid counter. */
bool twr_gw_set_tx_counter(struct twr_gw *gw, uint32_t next);

#endif /* APP_TWR_GW_H_ */
