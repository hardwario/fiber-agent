/* SPDX-License-Identifier: Apache-2.0
 *
 * Event ring of the TOWER gateway: reliable delivery of EVT_TWR_UPLINK and EVT_TWR_TX to
 * the host (#470 link protocol §4.1). The Northbridge ACKs a node locally, so an uplink
 * event lost on the link would lose the data for good; every ring event stays here until
 * the host confirms it with TWR_EVT_ACK.
 *
 * - The frame seq of a ring event is its delivery sequence number: u8, rolling, the first
 *   ring event after boot is 1. twr_evq_seq() is the newest one handed out (0 before any);
 *   the best-effort events (EVT_BOOT, EVT_RX, EVT_TX_DONE, EVT_LOG, EVT_TWR_CTR_LOW) carry
 *   it without advancing it.
 * - TWR_EVT_ACK{seq} is cumulative: everything up to and including seq is released. A seq
 *   outside the outstanding window (an old or repeated ack, or one ahead of anything sent)
 *   is ignored and counted in ack_ignored (shell only, not an error: a resend makes a
 *   repeated ack normal). TWR_EVQ_MAX < 128 keeps the window unambiguous against the
 *   host's dedup window of 128 behind its last ack.
 * - Resend is go-back-N with backoff: once the oldest unacked event has waited the resend
 *   timeout since it was last sent, all outstanding events go out again in order, at most
 *   one pass per timeout. The timeout starts at TWR_EVQ_RESEND_MS, doubles with every pass
 *   up to TWR_EVQ_RESEND_MAX_MS, and drops back to TWR_EVQ_RESEND_MS on every ack that
 *   releases an event (#130: a host whose ack waits on a slow eMMC flush got resends it
 *   only had to drop). Every re-sent event counts in `resent` (GET_INFO evt_resent).
 * - Bodies live in a byte pool sized for TWR_EVQ_MAX uplinks of the lora MTU (78 B
 *   plaintext); larger frames (the P0 node image goes up to 233 B) fill it with fewer
 *   entries. The gateway admits an air frame only when the events it can raise fit
 *   (twr_evq_room()); otherwise the frame is dropped whole (twr_gwc.c).
 *
 * Plain C, no Zephyr, no clock or lock of its own (the gateway core's lock covers it):
 * tests/tower_gateway runs it on native_sim.
 */
#ifndef APP_TWR_EVQ_H_
#define APP_TWR_EVQ_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "proto.h"

#if defined(CONFIG_NB_EVT_RING_SIZE)
#define TWR_EVQ_MAX CONFIG_NB_EVT_RING_SIZE
#else
#define TWR_EVQ_MAX 32
#endif
/* >= 32 by the spec; < 128 so the u8 seq window cannot alias (and n fits a u8). */
_Static_assert(TWR_EVQ_MAX >= 32 && TWR_EVQ_MAX <= 127, "event ring size 32..127");

/* Bytes per entry the pool is sized for: an EVT_TWR_UPLINK with a lora-MTU plaintext. */
#define TWR_EVQ_AVG_BODY (P2P_EVT_TWR_UPLINK_HDR_LEN + P2P_TWR_DL_MAX_PT)
#define TWR_EVQ_POOL     ((size_t)TWR_EVQ_MAX * TWR_EVQ_AVG_BODY)
/* The largest event body (EVT_TWR_UPLINK of a 255 B frame: 233 B plaintext). */
#define TWR_EVQ_BODY_MAX 255u

/* Go-back-N resend timeout and pass spacing (link §4.1): the first, and the cap the
 * backoff doubles it to. 300 ms until 0.4.1 (#130). */
#define TWR_EVQ_RESEND_MS     1000u
#define TWR_EVQ_RESEND_MAX_MS 4000u

struct twr_evq_ent {
	uint16_t off; /* body in the pool, may wrap */
	uint8_t len;
	uint8_t type;
	uint64_t sent_ms; /* last time it went to the link */
};

struct twr_evq {
	uint8_t head;      /* index of the oldest outstanding entry */
	uint8_t n;         /* outstanding entries */
	uint8_t unsent;    /* the newest `unsent` entries have not been on the link yet */
	uint8_t last_seq;  /* delivery seq of the newest ring event, 0 before the first */
	uint16_t pool_head; /* pool offset of the oldest body */
	uint16_t pool_used;
	/* resend pass */
	bool pass_ever;
	uint64_t last_pass_ms;
	uint32_t resend_ms; /* current timeout, TWR_EVQ_RESEND_MS..TWR_EVQ_RESEND_MAX_MS */
	uint8_t pass_next; /* seq the pass sends next */
	uint8_t pass_left; /* entries the pass has still to send */
	/* counters */
	uint32_t resent;      /* GET_INFO evt_resent */
	uint32_t ack_ignored; /* shell only */
	struct twr_evq_ent e[TWR_EVQ_MAX];
	uint8_t pool[TWR_EVQ_POOL];
};

/* One event handed to the link. */
struct twr_evq_out {
	uint8_t type;
	uint8_t seq;
	uint8_t len;
	uint8_t body[TWR_EVQ_BODY_MAX];
};

void twr_evq_init(struct twr_evq *q);

/* The newest delivery seq handed out (best-effort events carry it). */
uint8_t twr_evq_seq(const struct twr_evq *q);

/* The seq the host should expect next (GET_INFO evt_head_seq): the oldest outstanding
 * entry, or the next one to be handed out when none is outstanding. */
uint8_t twr_evq_expect_seq(const struct twr_evq *q);

/* Outstanding entries (GET_INFO evt_ring_used). */
uint8_t twr_evq_used(const struct twr_evq *q);

/* True when `n_evts` more events of `bytes` bytes in total fit. */
bool twr_evq_room(const struct twr_evq *q, size_t n_evts, size_t bytes);

/* Append one event, not yet sent; its seq is the next delivery seq. False when it does not
 * fit (the gateway checks twr_evq_room() before it commits to the events, so this is a
 * bug guard), or when len is over TWR_EVQ_BODY_MAX. */
bool twr_evq_push(struct twr_evq *q, uint8_t type, const uint8_t *body, size_t len,
		  uint8_t *seq);

/* The oldest pushed event that has not been on the link yet, marked sent at now_ms. False
 * when there is none. The caller sends it with out->seq as the frame seq. */
bool twr_evq_take_unsent(struct twr_evq *q, uint64_t now_ms, struct twr_evq_out *out);

/* TWR_EVT_ACK: release everything up to and including seq. False (ack_ignored++) when seq is
 * not one of the outstanding events. */
bool twr_evq_ack(struct twr_evq *q, uint8_t seq);

/* Go-back-N. twr_evq_resend_begin() starts a pass when it is due (the oldest outstanding
 * event was last sent the resend timeout ago or more, and so was the previous pass) and
 * returns how many events it covers, 0 when none is due; a pass doubles the timeout, up to
 * TWR_EVQ_RESEND_MAX_MS, and an ack that releases an event resets it. twr_evq_resend_next() then hands
 * them out oldest first, skipping what an ack released meanwhile; false at the end. Each
 * one is marked sent at now_ms and counts in `resent`. */
size_t twr_evq_resend_begin(struct twr_evq *q, uint64_t now_ms);
bool twr_evq_resend_next(struct twr_evq *q, uint64_t now_ms, struct twr_evq_out *out);

#endif /* APP_TWR_EVQ_H_ */
