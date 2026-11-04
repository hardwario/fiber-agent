/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway downlink queue, see twr_dlq.h.
 */
#include "twr_dlq.h"

#include <string.h>

void twr_dlq_init(struct twr_dlq *q)
{
	memset(q, 0, sizeof(*q));
}

int twr_dlq_find(const struct twr_dlq *q, uint32_t addr, uint16_t item)
{
	for (size_t i = 0; i < TWR_DLQ_MAX; i++) {
		const struct twr_dl *e = &q->e[i];

		if (e->state != TWR_DL_FREE && e->addr == addr && e->item == item) {
			return (int)i;
		}
	}
	return -1;
}

int twr_dlq_alloc(struct twr_dlq *q)
{
	for (size_t i = 0; i < TWR_DLQ_MAX; i++) {
		struct twr_dl *e = &q->e[i];

		if (e->state == TWR_DL_FREE) {
			memset(e, 0, sizeof(*e));
			e->state = TWR_DL_QUEUED;
			e->seq = q->next_seq++;
			return (int)i;
		}
	}
	return -1;
}

int twr_dlq_head(const struct twr_dlq *q, uint32_t addr, uint64_t now_ms, size_t max_pt)
{
	int best = -1;

	for (size_t i = 0; i < TWR_DLQ_MAX; i++) {
		const struct twr_dl *e = &q->e[i];

		if (e->state != TWR_DL_QUEUED || e->addr != addr || e->expires_ms <= now_ms ||
		    e->len > max_pt) {
			continue;
		}
		/* Push order is a wrapping u32: compare by difference. */
		if (best < 0 || (int32_t)(e->seq - q->e[best].seq) < 0) {
			best = (int)i;
		}
	}
	return best;
}

int twr_dlq_find_inflight(const struct twr_dlq *q, uint32_t addr, uint32_t gw_counter)
{
	for (size_t i = 0; i < TWR_DLQ_MAX; i++) {
		const struct twr_dl *e = &q->e[i];

		if (e->state == TWR_DL_INFLIGHT && e->addr == addr && e->gw_counter == gw_counter) {
			return (int)i;
		}
	}
	return -1;
}

void twr_dlq_free(struct twr_dlq *q, int idx)
{
	if (idx >= 0 && (size_t)idx < TWR_DLQ_MAX) {
		memset(&q->e[idx], 0, sizeof(q->e[idx]));
	}
}

size_t twr_dlq_drop(struct twr_dlq *q, uint32_t addr, uint16_t item)
{
	size_t n = 0;

	for (size_t i = 0; i < TWR_DLQ_MAX; i++) {
		struct twr_dl *e = &q->e[i];

		if (e->state != TWR_DL_FREE && e->addr == addr && (item == 0 || e->item == item)) {
			twr_dlq_free(q, (int)i);
			n++;
		}
	}
	return n;
}

uint16_t twr_dlq_used(const struct twr_dlq *q)
{
	uint16_t n = 0;

	for (size_t i = 0; i < TWR_DLQ_MAX; i++) {
		if (q->e[i].state != TWR_DL_FREE) {
			n++;
		}
	}
	return n;
}
