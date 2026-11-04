/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway core, radio-free: peer table, downlink queue, gateway
 * TX counter block and the receive / ACK / downlink decisions of p2p_tower_gateway.md §2.3.
 * twr_gateway.c drives it from the SX126x and twr_cmd.c from the host link, both under one
 * lock; tests/tower_gateway drives it from the #470 KAT vectors on native_sim. Plain C, no
 * Zephyr, no clock of its own: every time is the caller's millisecond clock.
 *
 * Receive, in this order (§2.3): header parse (bad_hdr) -> dest == 0: raw to the central
 * (join_fwd) -> dest != net_id: drop (not_for_us) -> unknown src: raw (unknown_src) -> only
 * Data and Ack from a known src (bad_hdr) -> CCM open under the peer key (mic_fail) -> the
 * replay rule (fresh / dup / replay). A fresh Data frame is forwarded; a CONFIRMED Data frame,
 * fresh or an exact retransmit, from a HOME peer with ACK_ENABLED is ACKed with a fresh
 * gateway counter, spent when the ACK is sealed. PENDING is set when the peer has a queued
 * downlink and two counters are left (the ACK's and the downlink's). An ACK to a 0x91 uplink
 * that asks for LinkCheckReq / TimeReq carries the answers as a CTRL tail (twr_ackctrl.h),
 * rebuilt for every re-ACK of a repetition; the uplink still goes to the central whole. A
 * node's TOWER ACK
 * that is fresh and acknowledges an in-flight confirmed downlink delivers it and moves the
 * peer's last_seen; any other ACK is ignored.
 *
 * Event ring backpressure (#470 link §4.1): the EVT_TWR_UPLINK / EVT_TWR_TX a frame can
 * raise must fit the event ring before the frame is accepted. When they do not, the frame is
 * dropped whole: no ACK, no re-ACK, no last_seen update, no event (EVT_TWR_UPLINK ack 5
 * "suppressed" therefore never goes out: there is no slot to carry it). The node keeps the
 * data and sends it again later. Housekeeping outcomes wait in the queue until there is room.
 */
#ifndef APP_TWR_GWC_H_
#define APP_TWR_GWC_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "proto.h"
#include "twr_ackctrl.h"
#include "twr_crypto.h"
#include "twr_dlq.h"
#include "twr_evq.h"
#include "twr_frame.h"
#include "twr_peers.h"

/* A plain ACK (also a node's ACK of a downlink), and the longest one the gateway sends: with
 * the CTRL tail of both answers (#470 §13.5), 45 B. */
#define TWR_GWC_ACK_FRAME_LEN (TWR_HDR_LEN + TWR_ACK_PAYLOAD_LEN + TWR_TAG_LEN)
#define TWR_GWC_ACK_MAX_FRAME (TWR_GWC_ACK_FRAME_LEN + TWR_ACKCTRL_TAIL_MAX)
#define TWR_GWC_DL_MAX_FRAME  (TWR_HDR_LEN + P2P_TWR_DL_MAX_PT + TWR_TAG_LEN)

/* ---- gateway TX counter block (TWR_CTR_BLOCK) --------------------------------------- */

struct twr_ctr {
	bool ever;     /* a block was given since boot: TWR_START needs one */
	uint32_t first;
	uint32_t last;
	uint32_t next; /* next to spend; last + 1 once exhausted (last < 2^32 - 1) */
	bool low_sent; /* EVT_TWR_CTR_LOW raised for this block */
	bool low_due;  /* ...and not yet taken by twr_ctr_low_take() */
};

/* Replace the range; 1 <= first <= last < 2^32 - 1 is the caller's check. */
void twr_ctr_set(struct twr_ctr *c, uint32_t first, uint32_t last);

/* Counters left in the block, 0 when exhausted or none was given. */
uint32_t twr_ctr_left(const struct twr_ctr *c);

/* Spend one counter. False when none is left (fail-closed: nothing is sent without one). */
bool twr_ctr_take(struct twr_ctr *c, uint32_t *ctr);

/* True once per block, after the spend that left fewer than 25 % of it: raise
 * EVT_TWR_CTR_LOW{next, last}. */
bool twr_ctr_low_take(struct twr_ctr *c, uint32_t *next, uint32_t *last);

/* ---- core ----------------------------------------------------------------------------- */

struct twr_gwc_params {
	uint32_t net_id;
	uint8_t turnaround_ms;
	uint8_t dl_gap_ms;
	uint8_t flags; /* P2P_TWR_START_* */
};

struct twr_gwc_stats {
	/* GET_INFO (enum p2p_info_ctr) */
	uint32_t rx_total;
	uint32_t fresh;
	uint32_t dup;
	uint32_t replay;
	uint32_t mic_fail;
	uint32_t unknown_src;
	uint32_t join_fwd;
	uint32_t not_for_us;
	uint32_t bad_hdr;
	uint32_t acks_sent;
	uint32_t acks_late;        /* on air, but SetTx past the node's window (also in acks_sent) */
	uint32_t acks_suppressed;  /* confirmed frames not ACKed for want of a counter, and
				    * fresh frames dropped for a full event ring */
	uint32_t dl_sent;
	uint32_t dl_delivered;
	uint32_t dl_not_delivered; /* NOT_DELIVERED, RADIO_ERR and NO_COUNTER outcomes */
	uint32_t dl_expired;
	/* shell only */
	uint32_t ack_unmatched;    /* authenticated node ACKs with no in-flight downlink */
	uint32_t dl_radio_err;
	uint32_t dl_no_counter;
	uint32_t dl_late;          /* sealed downlinks that missed their slot, back in the queue */
	uint32_t rx_ring_full;     /* authenticated frames dropped for a full event ring */
	uint32_t rx_too_long;      /* frames over the 96 B TOWER MTU, dropped unparsed (#486) */
	/* GET_INFO tail (#470 §13.5) */
	uint32_t ack_ctrl_sent;      /* ACKs with a CTRL tail that went on air */
	uint32_t ack_time_no_anchor; /* TimeReqs answered without TimeAns: no anchor */
	/* shell only */
	uint32_t ack_ctrl_lc;        /* LinkCheckAns sealed */
	uint32_t ack_ctrl_time;      /* TimeAns sealed */
};

struct twr_gwc {
	bool started; /* TWR_START accepted once: p is valid (kept over RX_STOP) */
	struct twr_gwc_params p;
	uint8_t max_frame; /* longest frame the current radio config may send (ToA cap) */
	struct twr_ctr ctr;
	struct twr_gwc_stats st;
	struct twr_peers peers;
	struct twr_dlq dlq;
	struct twr_evq evq; /* EVT_TWR_UPLINK / EVT_TWR_TX until TWR_EVT_ACK */
	struct twr_time_anchor anchor; /* TWR_TIME_SYNC, for TimeAns */
	struct twr_aes128 aes; /* per-frame scratch */
};

/* EVT_TWR_TX */
struct twr_tx_evt {
	uint32_t addr;
	uint16_t item;
	uint8_t outcome; /* enum p2p_twr_outcome */
	uint32_t gw_counter;
	uint32_t node_ack_counter;
	int8_t ack_rssi; /* P2P_TWR_RSSI_NONE when none */
};

void twr_gwc_init(struct twr_gwc *g);

/* The ToA cap for the current radio config, in frame bytes (twr_gateway.c). Downlinks that no
 * longer fit fail with RADIO_ERR at the next twr_gwc_housekeep(). */
void twr_gwc_set_max_frame(struct twr_gwc *g, uint8_t max_frame);

/* Largest downlink plaintext the current cap allows (<= P2P_TWR_DL_MAX_PT). */
size_t twr_gwc_dl_max_pt(const struct twr_gwc *g);

/* ---- host commands (fields already range-checked by twr_link.c) ----------------------- */

/* INVALID_STATE before the first counter block; BAD_PARAM when a peer holds net_id. */
enum p2p_status twr_gwc_start(struct twr_gwc *g, const struct twr_gwc_params *p);

/* Upsert. A new session key drops the peer's queue without events; the same key keeps it.
 * P2P_TWR_NODE_KEEP_NEWER with the same key keeps max(own, sent) last_seen (link §4.2); a
 * different key always takes the sent value. BAD_PARAM for addr == net_id once started,
 * NO_SPACE when the table is full. */
enum p2p_status twr_gwc_node_add(struct twr_gwc *g, uint32_t addr,
				 const uint8_t key[TWR_AES_KEY_LEN], uint32_t last_seen,
				 uint8_t flags);

enum p2p_status twr_gwc_node_remove(struct twr_gwc *g, uint32_t addr);

/* Replace the counter block. The next counter never goes back: a block with last below
 * the current next is BAD_PARAM, and one that starts below it continues from it. */
enum p2p_status twr_gwc_ctr_block(struct twr_gwc *g, uint32_t first, uint32_t last);

/* NOT_FOUND for an unknown peer, BAD_PARAM for a frame over the ToA cap, NO_SPACE when the
 * pool is full. The same (addr, item) again replaces a QUEUED entry in place (a host retry
 * after a lost RSP is harmless) and answers BUSY while that item is on air or awaiting the
 * node's ACK. */
enum p2p_status twr_gwc_queue_push(struct twr_gwc *g, uint32_t addr, uint16_t item,
				   uint16_t ttl_s, uint8_t flags, const uint8_t *pt, size_t len,
				   uint64_t now_ms);

/* item 0 = the peer's whole queue (OK when empty); no events. NOT_FOUND for an unknown peer
 * or a missing item. */
enum p2p_status twr_gwc_queue_drop(struct twr_gwc *g, uint32_t addr, uint16_t item);

/* TWR_TIME_SYNC: at NB uptime now_ms the wall time was unix_ms. BAD_PARAM for an anchor
 * from the future (now_ms > nb_now_ms: a host still holding a previous boot's clock). */
enum p2p_status twr_gwc_time_sync(struct twr_gwc *g, uint64_t now_ms, uint64_t unix_ms,
				  uint64_t nb_now_ms);

struct twr_info;

/* The core's part of GET_INFO: net_id / timing (0 before the first TWR_START), its
 * counters, the table and queue sizes and the counter block (0 / 0 without one). */
void twr_gwc_info(const struct twr_gwc *g, struct twr_info *out);

/* TWR_NODE_LIST RSP payload (after the status byte); returns its length. `out` holds
 * P2P_TWR_NODE_LIST_HDR_LEN + P2P_TWR_NODE_LIST_MAX * P2P_TWR_NODE_LIST_ENTRY_LEN. */
size_t twr_gwc_node_list(const struct twr_gwc *g, uint16_t start, uint8_t *out);

/* ---- radio side ----------------------------------------------------------------------- */

enum twr_rx_action {
	TWR_RXA_DROP,     /* counted, nothing to report */
	TWR_RXA_RAW,      /* EVT_RX with the raw frame: dest == 0 or unknown src */
	TWR_RXA_UPLINK,   /* fresh Data: EVT_TWR_UPLINK (after the ACK, when there is one) */
	TWR_RXA_DUP,      /* retransmit: re-ACKed when confirmed, not forwarded */
	TWR_RXA_NODE_ACK, /* node ACK of an in-flight downlink: tx_evt is its DELIVERED */
};

struct twr_gwc_rx {
	enum twr_rx_action action;
	bool hdr_valid;
	struct twr_hdr hdr;
	uint8_t pt_len; /* UPLINK */
	uint8_t pt[TWR_MAX_PAYLOAD];

	uint8_t ack; /* enum p2p_twr_ack | P2P_TWR_ACK_FLAGS as decided here; the TX result may
		      * still make the code LATE (twr_gwc_ack_code()) */
	bool send_ack;
	bool ack_pending;
	uint32_t ack_ctr;
	uint8_t ack_len;
	bool ack_ctrl; /* the ACK carries a CTRL tail */
	uint8_t ack_frame[TWR_GWC_ACK_MAX_FRAME];

	bool has_tx_evt; /* NODE_ACK: DELIVERED; a confirmed Data frame: NO_COUNTER for the head */
	struct twr_tx_evt tx_evt;
};

/* The reception of one frame. */
struct twr_rx_meta {
	int rssi_dbm;
	int8_t snr_q;     /* quarter dB */
	uint8_t sf;       /* the current radio config's */
	uint64_t t_rx_ms; /* RxDone stamp (the frame's end), NB uptime: TimeAns */
};

/* Classify one received frame (§2.3) and, when it is due, seal the ACK. The ACK is sealed
 * however late the frame is handled: a late ACK still goes out (twr_gwc_ack_result()). */
void twr_gwc_on_rx(struct twr_gwc *g, const uint8_t *frame, size_t len,
		   const struct twr_rx_meta *meta, uint64_t now_ms, struct twr_gwc_rx *out);

/* The sealed ACK went on air (on_air; a radio error counts in tx_radio_err, not here), and
 * its SetTx landed past the node's window (late: acks_late); ctrl = it had a CTRL tail
 * (ack_ctrl_sent). */
void twr_gwc_ack_result(struct twr_gwc *g, bool on_air, bool late, bool ctrl);

/* The EVT_TWR_UPLINK ack code once the sealed ACK's TX result is known: LATE when it was
 * late or not on air. b7 / b6 (P2P_TWR_ACK_FLAGS) mean a tail went out: kept from the seal
 * when the ACK was on air (late included), cleared on a radio error. */
uint8_t twr_gwc_ack_code(uint8_t ack, bool on_air, bool late);

struct twr_dl_ref {
	int idx;
	uint32_t seq;
	uint32_t addr;
	uint16_t item;
	uint32_t gw_counter;
	bool confirmed;
	uint8_t pt_len;
};

/* After an ACK with PENDING went on air: seal the head of addr's queue with a fresh counter
 * (spent here) into `frame` (TWR_GWC_DL_MAX_FRAME) and mark it SENDING. Returns the frame
 * length, 0 when there is nothing to send; *has_evt with NO_COUNTER in *evt when the head
 * found no counter. */
size_t twr_gwc_dl_take(struct twr_gwc *g, uint32_t addr, uint64_t now_ms, uint8_t *frame,
		       struct twr_dl_ref *ref, struct twr_tx_evt *evt, bool *has_evt);

enum twr_dl_tx {
	TWR_DL_TX_OK,   /* on air */
	TWR_DL_TX_LATE, /* missed its slot, not on air: back to the queue */
	TWR_DL_TX_ERR,  /* radio error */
};

/* Outcome of the downlink twr_gwc_dl_take() sealed. OK: an unconfirmed one is SENT, a
 * confirmed one waits for the node's ACK until ack_deadline_ms. ERR: RADIO_ERR. Returns true
 * with *evt to report; false also when the item was dropped meanwhile. */
bool twr_gwc_dl_done(struct twr_gwc *g, const struct twr_dl_ref *ref, enum twr_dl_tx res,
		     uint64_t ack_deadline_ms, struct twr_tx_evt *evt);

/* TTL expiry (EXPIRED), node ACK timeouts (NOT_DELIVERED) and downlinks over the ToA cap
 * (RADIO_ERR); up to `max` events into `out`, returns how many, and never more than the event
 * ring has room for (the rest waits for the next call). Push the events into the ring before
 * the next call; call until it returns less than `max`. */
size_t twr_gwc_housekeep(struct twr_gwc *g, uint64_t now_ms, struct twr_tx_evt *out,
			 size_t max);

#endif /* APP_TWR_GWC_H_ */
