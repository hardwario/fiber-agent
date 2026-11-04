/* SPDX-License-Identifier: Apache-2.0
 *
 * Event ring of the TOWER gateway, see twr_evq.h.
 */
#include "twr_evq.h"

#include <string.h>

void twr_evq_init(struct twr_evq *q)
{
	memset(q, 0, sizeof(*q));
	q->resend_ms = TWR_EVQ_RESEND_MS;
}

uint8_t twr_evq_seq(const struct twr_evq *q)
{
	return q->last_seq;
}

uint8_t twr_evq_used(const struct twr_evq *q)
{
	return q->n;
}

bool twr_evq_room(const struct twr_evq *q, size_t n_evts, size_t bytes)
{
	return (size_t)q->n + n_evts <= TWR_EVQ_MAX && (size_t)q->pool_used + bytes <= TWR_EVQ_POOL;
}

/* Delivery seq of the oldest outstanding entry (meaningful while n > 0). */
static uint8_t head_seq(const struct twr_evq *q)
{
	return (uint8_t)(q->last_seq - q->n + 1u);
}

uint8_t twr_evq_expect_seq(const struct twr_evq *q)
{
	/* With n == 0 this is last_seq + 1: the next seq to be handed out. */
	return head_seq(q);
}

static void copy_out(const struct twr_evq *q, uint8_t idx, uint8_t seq, struct twr_evq_out *out)
{
	const struct twr_evq_ent *e = &q->e[idx];
	size_t first = TWR_EVQ_POOL - e->off;

	out->type = e->type;
	out->seq = seq;
	out->len = e->len;
	if (first >= e->len) {
		memcpy(out->body, &q->pool[e->off], e->len);
	} else {
		memcpy(out->body, &q->pool[e->off], first);
		memcpy(&out->body[first], q->pool, e->len - first);
	}
}

bool twr_evq_push(struct twr_evq *q, uint8_t type, const uint8_t *body, size_t len,
		  uint8_t *seq)
{
	struct twr_evq_ent *e;
	size_t off;
	size_t first;

	if (len > TWR_EVQ_BODY_MAX || !twr_evq_room(q, 1, len)) {
		return false;
	}
	off = ((size_t)q->pool_head + q->pool_used) % TWR_EVQ_POOL;
	e = &q->e[(q->head + q->n) % TWR_EVQ_MAX];
	e->off = (uint16_t)off;
	e->len = (uint8_t)len;
	e->type = type;
	e->sent_ms = 0;
	first = TWR_EVQ_POOL - off;
	if (first >= len) {
		memcpy(&q->pool[off], body, len);
	} else {
		memcpy(&q->pool[off], body, first);
		memcpy(q->pool, &body[first], len - first);
	}
	q->pool_used += (uint16_t)len;
	q->n++;
	q->unsent++;
	q->last_seq++;
	*seq = q->last_seq;
	return true;
}

bool twr_evq_take_unsent(struct twr_evq *q, uint64_t now_ms, struct twr_evq_out *out)
{
	uint8_t i;

	if (q->unsent == 0) {
		return false;
	}
	i = (uint8_t)(q->n - q->unsent);
	copy_out(q, (uint8_t)((q->head + i) % TWR_EVQ_MAX), (uint8_t)(head_seq(q) + i), out);
	q->e[(q->head + i) % TWR_EVQ_MAX].sent_ms = now_ms;
	q->unsent--;
	return true;
}

bool twr_evq_ack(struct twr_evq *q, uint8_t seq)
{
	uint8_t d;

	if (q->n == 0) {
		q->ack_ignored++;
		return false;
	}
	d = (uint8_t)(seq - head_seq(q));
	if (d >= q->n) {
		q->ack_ignored++;
		return false;
	}
	/* Progress: the host keeps up, so the next resend is back on the short timeout. */
	q->resend_ms = TWR_EVQ_RESEND_MS;
	for (unsigned int k = 0; k <= d; k++) {
		const struct twr_evq_ent *e = &q->e[q->head];

		q->pool_head = (uint16_t)(((size_t)q->pool_head + e->len) % TWR_EVQ_POOL);
		q->pool_used -= e->len;
		q->head = (uint8_t)((q->head + 1u) % TWR_EVQ_MAX);
		q->n--;
	}
	if (q->unsent > q->n) {
		/* Acked ahead of the link: whatever it covered need not go out. */
		q->unsent = q->n;
	}
	if (q->n == 0) {
		q->pool_head = 0;
	}
	return true;
}

size_t twr_evq_resend_begin(struct twr_evq *q, uint64_t now_ms)
{
	uint8_t sent = (uint8_t)(q->n - q->unsent);

	/* Only what has been on the link once is resent; the unsent tail goes out first. */
	if (sent == 0 || now_ms < q->e[q->head].sent_ms + q->resend_ms) {
		return 0;
	}
	if (q->pass_ever && now_ms < q->last_pass_ms + q->resend_ms) {
		return 0;
	}
	q->pass_ever = true;
	q->last_pass_ms = now_ms;
	/* Backoff: no ack since the last send, so give the host twice as long next time. */
	q->resend_ms = q->resend_ms >= TWR_EVQ_RESEND_MAX_MS / 2u ? TWR_EVQ_RESEND_MAX_MS
								 : q->resend_ms * 2u;
	q->pass_next = head_seq(q);
	q->pass_left = sent;
	return sent;
}

bool twr_evq_resend_next(struct twr_evq *q, uint64_t now_ms, struct twr_evq_out *out)
{
	uint8_t d;
	uint8_t idx;

	if (q->pass_left == 0 || q->n == 0) {
		q->pass_left = 0;
		return false;
	}
	d = (uint8_t)(q->pass_next - head_seq(q));
	if (d >= q->n) {
		/* An ack released pass_next and possibly more: continue from the new head. */
		uint8_t skip = (uint8_t)(head_seq(q) - q->pass_next);

		if (skip >= q->pass_left) {
			q->pass_left = 0;
			return false;
		}
		q->pass_left -= skip;
		q->pass_next = head_seq(q);
		d = 0;
	}
	if (d >= (uint8_t)(q->n - q->unsent)) {
		/* Never sent yet: not this pass's (cannot happen while the pass runs on the
		 * thread that pushes, kept as a guard). */
		q->pass_left = 0;
		return false;
	}
	idx = (uint8_t)((q->head + d) % TWR_EVQ_MAX);
	copy_out(q, idx, q->pass_next, out);
	q->e[idx].sent_ms = now_ms;
	q->pass_next++;
	q->pass_left--;
	q->resent++;
	return true;
}
