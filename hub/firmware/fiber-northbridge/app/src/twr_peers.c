/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway peer table, see twr_peers.h.
 */
#include "twr_peers.h"

#include <string.h>

/* memset() that the compiler may not drop as a dead store before the entry is reused. */
static void wipe(void *p, size_t n)
{
	volatile uint8_t *b = p;

	while (n-- > 0) {
		*b++ = 0;
	}
}

void twr_peers_init(struct twr_peers *t)
{
	wipe(t, sizeof(*t));
}

struct twr_peer *twr_peers_find(struct twr_peers *t, uint32_t addr)
{
	for (uint16_t i = 0; i < t->n; i++) {
		if (t->p[i].addr == addr) {
			return &t->p[i];
		}
	}
	return NULL;
}

enum twr_peers_res twr_peers_upsert(struct twr_peers *t, uint32_t addr,
				    const uint8_t key[TWR_AES_KEY_LEN], uint32_t last_seen,
				    uint8_t flags)
{
	struct twr_peer *p = twr_peers_find(t, addr);
	enum twr_peers_res res = TWR_PEERS_REPLACED;

	if (p == NULL) {
		if (t->n >= TWR_PEERS_MAX) {
			return TWR_PEERS_FULL;
		}
		p = &t->p[t->n++];
		res = TWR_PEERS_ADDED;
	}
	p->addr = addr;
	p->last_seen = last_seen;
	p->flags = flags;
	memcpy(p->key, key, TWR_AES_KEY_LEN);
	return res;
}

bool twr_peers_remove(struct twr_peers *t, uint32_t addr)
{
	struct twr_peer *p = twr_peers_find(t, addr);
	struct twr_peer *last;

	if (p == NULL) {
		return false;
	}
	last = &t->p[t->n - 1];
	if (p != last) {
		*p = *last;
	}
	wipe(last, sizeof(*last));
	t->n--;
	return true;
}

enum twr_replay twr_peer_classify(const struct twr_peer *p, uint32_t counter)
{
	if (counter == 0 || counter < p->last_seen) {
		return TWR_RP_REPLAY;
	}
	return counter == p->last_seen ? TWR_RP_RETRANSMIT : TWR_RP_FRESH;
}
