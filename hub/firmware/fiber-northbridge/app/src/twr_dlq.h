/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway downlink queue: one pool of TWR_DLQ_MAX frames shared by
 * all peers, FIFO per peer by push order, each with a TTL (p2p_tower_gateway.md §2.1
 * TWR_QUEUE_PUSH / TWR_QUEUE_DROP). Plaintext only: a frame is sealed when it goes out, with
 * a fresh gateway counter. RAM only. Plain C, no Zephyr (tests/tower_gateway).
 *
 * Entry life: QUEUED -> SENDING (sealed, on the radio) -> freed with SENT / RADIO_ERR, or
 * INFLIGHT (confirmed, waiting for the node's TOWER ACK) -> freed with DELIVERED /
 * NOT_DELIVERED. A QUEUED entry past its TTL is freed with EXPIRED. twr_gwc.c drives it.
 */
#ifndef APP_TWR_DLQ_H_
#define APP_TWR_DLQ_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "proto.h"

#define TWR_DLQ_MAX 32u

enum twr_dl_state {
	TWR_DL_FREE = 0,
	TWR_DL_QUEUED,
	TWR_DL_SENDING,
	TWR_DL_INFLIGHT,
};

struct twr_dl {
	uint8_t state;            /* enum twr_dl_state */
	uint8_t flags;            /* P2P_TWR_QUEUE_* */
	uint8_t len;
	uint16_t item;
	uint32_t addr;
	uint32_t seq;             /* push order, FIFO per addr */
	uint32_t gw_counter;      /* SENDING / INFLIGHT: the counter it was sealed with */
	uint64_t expires_ms;      /* TTL deadline (QUEUED) */
	uint64_t ack_deadline_ms; /* INFLIGHT: NOT_DELIVERED after this */
	uint8_t pt[P2P_TWR_DL_MAX_PT];
};

struct twr_dlq {
	uint32_t next_seq;
	struct twr_dl e[TWR_DLQ_MAX];
};

void twr_dlq_init(struct twr_dlq *q);

/* Index of the entry for (addr, item) in any state but FREE, else -1. */
int twr_dlq_find(const struct twr_dlq *q, uint32_t addr, uint16_t item);

/* Take a free entry, QUEUED with the next push order; -1 when the pool is full. The caller
 * fills in the rest. */
int twr_dlq_alloc(struct twr_dlq *q);

/* The oldest QUEUED entry for addr that has not expired at now_ms and whose plaintext fits
 * max_pt, else -1. */
int twr_dlq_head(const struct twr_dlq *q, uint32_t addr, uint64_t now_ms, size_t max_pt);

/* The INFLIGHT entry for addr sealed with gw_counter, else -1. */
int twr_dlq_find_inflight(const struct twr_dlq *q, uint32_t addr, uint32_t gw_counter);

void twr_dlq_free(struct twr_dlq *q, int idx);

/* Free addr's entry `item`, or all of addr's entries for item 0, in any state. Returns how
 * many were freed. */
size_t twr_dlq_drop(struct twr_dlq *q, uint32_t addr, uint16_t item);

uint16_t twr_dlq_used(const struct twr_dlq *q);

#endif /* APP_TWR_DLQ_H_ */
