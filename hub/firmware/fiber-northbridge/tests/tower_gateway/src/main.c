/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suites for the TOWER gateway core: twr_peers.c, twr_dlq.c and twr_gwc.c (counter
 * block, receive classification, ACK + PENDING, downlinks, TTL). The link codec (twr_link.c)
 * is test_link.c's; the command path through cmd_dispatch.c is tests/cmd_dispatch.
 *
 * The frames are the planner's #470 KAT (tests/tower_codec/ccm/tower_frame_kat.json, turned
 * into tower_kat.h at build time): the gateway must open every uplink and produce the ACK and
 * downlink frames byte for byte. Frames the KAT has no vector for are sealed here with the
 * same codec the KAT pins (tests/tower_codec).
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/util.h>
#include <string.h>

#include "proto.h"
#include "twr_crypto.h"
#include "twr_dlq.h"
#include "twr_evq.h"
#include "twr_frame.h"
#include "twr_gwc.h"
#include "twr_link.h"
#include "twr_peers.h"

#include "tower_kat.h"

#define GW_ADDR   0x4E420001u
#define NODE_ADDR 0x00000413u

static const uint8_t key_00_0f[16] = {
	0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
	0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
};

static const uint8_t other_key[16] = {
	0xf0, 0xe1, 0xd2, 0xc3, 0xb4, 0xa5, 0x96, 0x87,
	0x78, 0x69, 0x5a, 0x4b, 0x3c, 0x2d, 0x1e, 0x0f,
};

static const struct tower_kat *kat(const char *name)
{
	for (size_t i = 0; i < ARRAY_SIZE(tower_kats); i++) {
		if (strcmp(tower_kats[i].name, name) == 0) {
			return &tower_kats[i];
		}
	}
	zassert_unreachable("KAT vector %s missing", name);
	return NULL;
}

static size_t mk_frame(uint8_t type, uint8_t flags, uint32_t src, uint32_t dest, uint32_t ctr,
		       const uint8_t *pt, size_t len, const uint8_t key[16], uint8_t *out)
{
	struct twr_aes128 aes;
	struct twr_hdr h = {
		.type = type, .flags = flags, .src = src, .dest = dest, .counter = ctr,
	};
	size_t n;

	twr_aes128_init(&aes, key);
	n = twr_frame_seal(&aes, &h, pt, len, TWR_MAX_PAYLOAD, out, TWR_MAX_FRAME);
	zassert_true(n > 0, "seal");
	return n;
}

static size_t mk_uplink(uint32_t ctr, bool confirmed, uint8_t *out)
{
	static const uint8_t pt[] = { 0x81, 0x02, 0x00, 0x07 };

	return mk_frame(TWR_T_DATA, confirmed ? TWR_FLAG_CONFIRMED : 0, NODE_ADDR, GW_ADDR, ctr,
			pt, sizeof(pt), key_00_0f, out);
}

static size_t mk_node_ack(uint32_t ctr, uint32_t acked, int rssi, uint8_t *out)
{
	uint8_t pl[TWR_ACK_PAYLOAD_LEN];

	twr_ack_payload_encode(acked, rssi, false, pl);
	return mk_frame(TWR_T_ACK, 0, NODE_ADDR, GW_ADDR, ctr, pl, sizeof(pl), key_00_0f, out);
}

static struct twr_gwc g;
static struct twr_gwc_rx rx;
static uint8_t frame[TWR_MAX_FRAME];

static void gw_setup(uint32_t first, uint32_t last, uint32_t last_seen)
{
	struct twr_gwc_params p = {
		.net_id = GW_ADDR,
		.turnaround_ms = 20,
		.dl_gap_ms = 20,
		.flags = P2P_TWR_START_ACK_ENABLED,
	};

	twr_gwc_init(&g);
	zassert_equal(twr_gwc_ctr_block(&g, first, last), P2P_STATUS_OK);
	zassert_equal(twr_gwc_node_add(&g, NODE_ADDR, key_00_0f, last_seen, P2P_TWR_NODE_HOME),
		      P2P_STATUS_OK);
	zassert_equal(twr_gwc_start(&g, &p), P2P_STATUS_OK);
}

static void rx_frame(const uint8_t *f, size_t n, uint64_t now)
{
	struct twr_rx_meta m = { .rssi_dbm = -57, .snr_q = 20, .sf = 7, .t_rx_ms = now };

	twr_gwc_on_rx(&g, f, n, &m, now, &rx);
}

/* ---- peer table ----------------------------------------------------------------------- */

static struct twr_peers peers;

ZTEST(tower_peers, test_upsert_remove_full)
{
	uint8_t k[16];

	twr_peers_init(&peers);
	for (uint32_t i = 0; i < TWR_PEERS_MAX; i++) {
		memset(k, (int)i, sizeof(k));
		zassert_equal(twr_peers_upsert(&peers, 0x1000 + i, k, i, P2P_TWR_NODE_HOME),
			      TWR_PEERS_ADDED, "peer %u", i);
	}
	zassert_equal(peers.n, TWR_PEERS_MAX, "at least 256 peers (contract §2.3)");
	zassert_equal(twr_peers_upsert(&peers, 0x9999, k, 0, 0), TWR_PEERS_FULL);
	zassert_is_null(twr_peers_find(&peers, 0x9999));

	/* replacing works when full and resets the lane / key / flags */
	zassert_equal(twr_peers_upsert(&peers, 0x1005, other_key, 77, 0), TWR_PEERS_REPLACED);
	zassert_equal(peers.n, TWR_PEERS_MAX);
	zassert_equal(twr_peers_find(&peers, 0x1005)->last_seen, 77);
	zassert_equal(twr_peers_find(&peers, 0x1005)->flags, 0);
	zassert_mem_equal(twr_peers_find(&peers, 0x1005)->key, other_key, 16);

	/* remove from the middle: the last entry moves in, its old slot is wiped */
	zassert_true(twr_peers_remove(&peers, 0x1005));
	zassert_false(twr_peers_remove(&peers, 0x1005), "already gone");
	zassert_equal(peers.n, TWR_PEERS_MAX - 1);
	zassert_is_null(twr_peers_find(&peers, 0x1005));
	zassert_equal(peers.p[5].addr, 0x1000 + TWR_PEERS_MAX - 1);
	zassert_equal(peers.p[5].last_seen, TWR_PEERS_MAX - 1);
	memset(k, 0, sizeof(k));
	zassert_mem_equal(peers.p[TWR_PEERS_MAX - 1].key, k, 16, "removed key wiped");
	zassert_equal(peers.p[TWR_PEERS_MAX - 1].addr, 0);
	for (uint32_t i = 0; i < TWR_PEERS_MAX; i++) {
		if (i != 5) {
			zassert_not_null(twr_peers_find(&peers, 0x1000 + i), "peer %u", i);
		}
	}

	/* room again */
	zassert_equal(twr_peers_upsert(&peers, 0x9999, k, 0, 0), TWR_PEERS_ADDED);
	zassert_equal(twr_peers_upsert(&peers, 0x9998, k, 0, 0), TWR_PEERS_FULL);

	/* remove the last one: nothing to move */
	zassert_true(twr_peers_remove(&peers, 0x9999));
	zassert_equal(peers.n, TWR_PEERS_MAX - 1);
}

ZTEST(tower_peers, test_replay_rule)
{
	struct twr_peer p = { .addr = 1, .last_seen = 5 };

	zassert_equal(twr_peer_classify(&p, 6), TWR_RP_FRESH);
	zassert_equal(twr_peer_classify(&p, 0xFFFFFFFFu), TWR_RP_FRESH);
	zassert_equal(twr_peer_classify(&p, 5), TWR_RP_RETRANSMIT);
	zassert_equal(twr_peer_classify(&p, 4), TWR_RP_REPLAY);
	zassert_equal(twr_peer_classify(&p, 0), TWR_RP_REPLAY);

	/* a fresh session (last_seen = 0 after the join): counter 0 is still reserved */
	p.last_seen = 0;
	zassert_equal(twr_peer_classify(&p, 0), TWR_RP_REPLAY);
	zassert_equal(twr_peer_classify(&p, 1), TWR_RP_FRESH);
}

ZTEST_SUITE(tower_peers, NULL, NULL, NULL, NULL, NULL);

/* ---- downlink queue ------------------------------------------------------------------- */

static struct twr_dlq q;

static int q_add(uint32_t addr, uint16_t item, uint64_t expires, uint8_t len)
{
	int i = twr_dlq_alloc(&q);

	if (i >= 0) {
		q.e[i].addr = addr;
		q.e[i].item = item;
		q.e[i].expires_ms = expires;
		q.e[i].len = len;
	}
	return i;
}

ZTEST(tower_dlq, test_pool_fifo_drop)
{
	int a, b, c;

	twr_dlq_init(&q);
	for (uint32_t i = 0; i < TWR_DLQ_MAX; i++) {
		zassert_true(q_add(0x10 + (i % 3), (uint16_t)(i + 1), 5000, 10) >= 0);
	}
	zassert_true(TWR_DLQ_MAX >= 16, "pool of at least 16 frames (contract §2.3)");
	zassert_equal(twr_dlq_used(&q), TWR_DLQ_MAX);
	zassert_equal(twr_dlq_alloc(&q), -1, "pool full");

	twr_dlq_init(&q);
	a = q_add(0x20, 1, 5000, 10);
	b = q_add(0x21, 2, 5000, 10);
	c = q_add(0x20, 3, 5000, 10);
	zassert_equal(twr_dlq_head(&q, 0x20, 1000, 78), a, "FIFO per addr");
	zassert_equal(twr_dlq_head(&q, 0x21, 1000, 78), b);
	zassert_equal(twr_dlq_head(&q, 0x22, 1000, 78), -1);

	/* a freed slot is reused but the new entry queues behind the older one */
	twr_dlq_free(&q, a);
	zassert_equal(twr_dlq_head(&q, 0x20, 1000, 78), c);
	a = q_add(0x20, 4, 5000, 10);
	zassert_equal(a, 0, "slot 0 reused");
	zassert_equal(twr_dlq_head(&q, 0x20, 1000, 78), c, "push order, not slot order");

	/* expired and oversize entries are never the head */
	zassert_equal(twr_dlq_head(&q, 0x20, 5000, 78), -1, "expired at 5000");
	zassert_equal(twr_dlq_head(&q, 0x20, 1000, 9), -1, "over max_pt");

	/* only QUEUED entries are heads; find() sees every state */
	q.e[c].state = TWR_DL_INFLIGHT;
	q.e[c].gw_counter = 42;
	zassert_equal(twr_dlq_head(&q, 0x20, 1000, 78), a);
	zassert_equal(twr_dlq_find(&q, 0x20, 3), c);
	zassert_equal(twr_dlq_find_inflight(&q, 0x20, 42), c);
	zassert_equal(twr_dlq_find_inflight(&q, 0x20, 43), -1);
	zassert_equal(twr_dlq_find_inflight(&q, 0x21, 42), -1);

	zassert_equal(twr_dlq_drop(&q, 0x20, 4), 1);
	zassert_equal(twr_dlq_drop(&q, 0x20, 4), 0);
	zassert_equal(twr_dlq_drop(&q, 0x20, 0), 1, "all of 0x20, in flight too");
	zassert_equal(twr_dlq_used(&q), 1);
	zassert_equal(twr_dlq_find(&q, 0x21, 2), b);
}

ZTEST(tower_dlq, test_push_order_wraps)
{
	int a, b;

	twr_dlq_init(&q);
	q.next_seq = UINT32_MAX;
	a = q_add(0x30, 1, 5000, 1);
	b = q_add(0x30, 2, 5000, 1);
	zassert_equal(q.e[b].seq, 0, "wrapped");
	zassert_equal(twr_dlq_head(&q, 0x30, 0, 78), a);
}

ZTEST_SUITE(tower_dlq, NULL, NULL, NULL, NULL, NULL);

/* ---- counter block -------------------------------------------------------------------- */

ZTEST(tower_ctr, test_block_low_water_exhaustion)
{
	struct twr_ctr c = { 0 };
	uint32_t v, next, last;

	zassert_equal(twr_ctr_left(&c), 0, "no block after boot");
	zassert_false(twr_ctr_take(&c, &v));

	twr_ctr_set(&c, 1, 4096);
	zassert_equal(twr_ctr_left(&c), 4096);
	for (uint32_t i = 1; i <= 3072; i++) {
		zassert_true(twr_ctr_take(&c, &v));
		zassert_equal(v, i);
	}
	zassert_equal(twr_ctr_left(&c), 1024, "exactly 25 %% left");
	zassert_false(twr_ctr_low_take(&c, &next, &last), "not below 25 %% yet");
	zassert_true(twr_ctr_take(&c, &v));
	zassert_equal(v, 3073);
	zassert_true(twr_ctr_low_take(&c, &next, &last), "below 25 %%");
	zassert_equal(next, 3074);
	zassert_equal(last, 4096);
	zassert_false(twr_ctr_low_take(&c, &next, &last), "once per block");
	while (twr_ctr_left(&c) > 0) {
		zassert_true(twr_ctr_take(&c, &v));
	}
	zassert_equal(v, 4096);
	zassert_false(twr_ctr_take(&c, &v), "fail-closed");
	zassert_false(twr_ctr_low_take(&c, &next, &last), "still once per block");
	zassert_equal(c.next, 4097);

	/* a new block starts over, low-water included */
	twr_ctr_set(&c, 5000, 5002);
	zassert_equal(twr_ctr_left(&c), 3);
	zassert_true(twr_ctr_take(&c, &v));
	zassert_equal(v, 5000);
	zassert_false(twr_ctr_low_take(&c, &next, &last), "2 of 3 left");
	zassert_true(twr_ctr_take(&c, &v));
	zassert_false(twr_ctr_low_take(&c, &next, &last), "1 of 3 left is not < 25 %%");
	zassert_true(twr_ctr_take(&c, &v));
	zassert_true(twr_ctr_low_take(&c, &next, &last), "exhausted");
	zassert_equal(next, 5003);
	zassert_equal(last, 5002);

	/* the top of the counter space: last < 2^32 - 1 keeps next representable */
	twr_ctr_set(&c, 0xFFFFFFFDu, 0xFFFFFFFEu);
	zassert_true(twr_ctr_take(&c, &v));
	zassert_true(twr_ctr_take(&c, &v));
	zassert_equal(v, 0xFFFFFFFEu);
	zassert_false(twr_ctr_take(&c, &v));
	zassert_equal(twr_ctr_left(&c), 0);
}

ZTEST_SUITE(tower_ctr, NULL, NULL, NULL, NULL, NULL);

/* ---- event ring (#470 link §4.1) ----------------------------------------------------- */

static struct twr_evq eq;
static struct twr_evq_out eo;

static void evq_push(uint8_t type, uint8_t fill, size_t len, uint8_t want_seq)
{
	uint8_t b[TWR_EVQ_BODY_MAX];
	uint8_t s = 0;

	memset(b, fill, len);
	zassert_true(twr_evq_push(&eq, type, b, len, &s), "push seq %u", want_seq);
	zassert_equal(s, want_seq);
}

static void evq_take(uint64_t now, uint8_t want_seq, uint8_t fill, size_t len)
{
	zassert_true(twr_evq_take_unsent(&eq, now, &eo));
	zassert_equal(eo.seq, want_seq);
	zassert_equal(eo.len, len);
	for (size_t i = 0; i < len; i++) {
		zassert_equal(eo.body[i], fill, "seq %u byte %u", want_seq, i);
	}
}

ZTEST(tower_evq, test_push_take_cumulative_ack)
{
	twr_evq_init(&eq);
	zassert_equal(twr_evq_seq(&eq), 0, "best-effort events carry 0 before any ring event");
	zassert_false(twr_evq_take_unsent(&eq, 0, &eo));
	zassert_false(twr_evq_ack(&eq, 0), "nothing outstanding");

	evq_push(P2P_EVT_TWR_UPLINK, 0xA1, 40, 1);
	evq_push(P2P_EVT_TWR_TX, 0xA2, P2P_EVT_TWR_TX_BODY_LEN, 2);
	evq_push(P2P_EVT_TWR_UPLINK, 0xA3, 30, 3);
	zassert_equal(twr_evq_seq(&eq), 3);
	zassert_equal(twr_evq_used(&eq), 3);

	evq_take(10, 1, 0xA1, 40);
	zassert_equal(eo.type, P2P_EVT_TWR_UPLINK);
	evq_take(10, 2, 0xA2, P2P_EVT_TWR_TX_BODY_LEN);
	zassert_equal(eo.type, P2P_EVT_TWR_TX);
	evq_take(10, 3, 0xA3, 30);
	zassert_false(twr_evq_take_unsent(&eq, 10, &eo), "each goes out once");

	zassert_true(twr_evq_ack(&eq, 2), "cumulative: 1 and 2");
	zassert_equal(twr_evq_used(&eq), 1);
	zassert_equal(eq.pool_used, 30);
	zassert_false(twr_evq_ack(&eq, 2), "a repeated ack");
	zassert_false(twr_evq_ack(&eq, 1), "an old ack");
	zassert_false(twr_evq_ack(&eq, 4), "ahead of anything sent");
	zassert_false(twr_evq_ack(&eq, 200), "far outside the window");
	zassert_equal(eq.ack_ignored, 5);
	zassert_equal(twr_evq_used(&eq), 1, "ignored acks release nothing");
	zassert_true(twr_evq_ack(&eq, 3));
	zassert_equal(twr_evq_used(&eq), 0);
	zassert_equal(eq.pool_used, 0);
	zassert_equal(twr_evq_seq(&eq), 3, "an ack does not move the delivery seq");
	zassert_equal(eq.resent, 0);
}

ZTEST(tower_evq, test_u8_seq_and_pool_wrap)
{
	uint8_t want = 1;

	twr_evq_init(&eq);
	/* 600 events in batches of 3, bodies of changing length so the pool wraps at every
	 * offset: the seq rolls 255 -> 0 -> 1 and an ack across the wrap stays cumulative. */
	for (int round = 0; round < 200; round++) {
		uint8_t first = want;

		for (int k = 0; k < 3; k++) {
			size_t len = 1 + (size_t)((round * 7 + k * 61) % TWR_EVQ_BODY_MAX);

			evq_push(P2P_EVT_TWR_UPLINK, (uint8_t)(want ^ 0x5A), len, want);
			evq_take(round, want, (uint8_t)(want ^ 0x5A), len);
			want++;
		}
		zassert_false(twr_evq_ack(&eq, (uint8_t)(first - 1)), "the ack before the batch");
		zassert_true(twr_evq_ack(&eq, (uint8_t)(first + 1)), "round %d", round);
		zassert_equal(twr_evq_used(&eq), 1);
		zassert_true(twr_evq_ack(&eq, (uint8_t)(first + 2)));
		zassert_equal(twr_evq_used(&eq), 0);
	}
	zassert_equal(twr_evq_seq(&eq), (uint8_t)600, "600 mod 256");
	zassert_equal(want, (uint8_t)601);
}

ZTEST(tower_evq, test_expect_seq_for_a_host_start)
{
	twr_evq_init(&eq);
	zassert_equal(twr_evq_expect_seq(&eq), 1, "after boot the first ring event is 1");
	evq_push(P2P_EVT_TWR_UPLINK, 0x11, 10, 1);
	evq_push(P2P_EVT_TWR_UPLINK, 0x12, 10, 2);
	evq_push(P2P_EVT_TWR_UPLINK, 0x13, 10, 3);
	zassert_equal(twr_evq_expect_seq(&eq), 1, "the oldest unacked");
	zassert_true(twr_evq_ack(&eq, 2));
	zassert_equal(twr_evq_expect_seq(&eq), 3);
	zassert_true(twr_evq_ack(&eq, 3));
	zassert_equal(twr_evq_expect_seq(&eq), 4, "empty: the next to be handed out");

	/* Across the u8 wrap: 254, 255, 0 outstanding -> 254; all acked -> 1. */
	twr_evq_init(&eq);
	for (int i = 1; i <= 253; i++) {
		evq_push(P2P_EVT_TWR_UPLINK, 0x20, 1, (uint8_t)i);
		zassert_true(twr_evq_ack(&eq, (uint8_t)i));
	}
	evq_push(P2P_EVT_TWR_UPLINK, 0x21, 1, 254);
	evq_push(P2P_EVT_TWR_UPLINK, 0x22, 1, 255);
	evq_push(P2P_EVT_TWR_UPLINK, 0x23, 1, 0);
	zassert_equal(twr_evq_expect_seq(&eq), 254);
	zassert_true(twr_evq_ack(&eq, 0));
	zassert_equal(twr_evq_expect_seq(&eq), 1);
}

ZTEST(tower_evq, test_full_by_entries_and_by_bytes)
{
	uint8_t b[TWR_EVQ_BODY_MAX] = { 0 };
	uint8_t s;
	size_t n = 0;

	zassert_equal(TWR_EVQ_MAX, 32, "Kconfig default: at least 32 entries");
	twr_evq_init(&eq);
	for (uint8_t i = 1; i <= TWR_EVQ_MAX; i++) {
		evq_push(P2P_EVT_TWR_TX, i, P2P_EVT_TWR_TX_BODY_LEN, i);
	}
	zassert_false(twr_evq_room(&eq, 1, 1), "32 entries");
	zassert_false(twr_evq_push(&eq, P2P_EVT_TWR_TX, b, 1, &s));
	zassert_equal(twr_evq_seq(&eq), TWR_EVQ_MAX, "a refused push takes no seq");
	zassert_true(twr_evq_ack(&eq, 1));
	zassert_true(twr_evq_room(&eq, 1, P2P_EVT_TWR_TX_BODY_LEN));
	zassert_false(twr_evq_room(&eq, 2, 2 * P2P_EVT_TWR_TX_BODY_LEN));

	/* lora-MTU uplinks fill exactly the entries; 255 B ones fill the pool first. */
	twr_evq_init(&eq);
	for (uint8_t i = 1; i <= TWR_EVQ_MAX; i++) {
		evq_push(P2P_EVT_TWR_UPLINK, i, TWR_EVQ_AVG_BODY, i);
	}
	zassert_false(twr_evq_room(&eq, 0, 1), "and the pool");
	twr_evq_init(&eq);
	while (twr_evq_push(&eq, P2P_EVT_TWR_UPLINK, b, TWR_EVQ_BODY_MAX, &s)) {
		n++;
	}
	zassert_equal(n, TWR_EVQ_POOL / TWR_EVQ_BODY_MAX);
	zassert_false(twr_evq_push(&eq, P2P_EVT_TWR_UPLINK, b, TWR_EVQ_BODY_MAX + 1, &s),
		      "over the largest body");
}

ZTEST(tower_evq, test_go_back_n_resend_pacing)
{
	twr_evq_init(&eq);
	evq_push(P2P_EVT_TWR_UPLINK, 1, 10, 1);
	evq_push(P2P_EVT_TWR_TX, 2, P2P_EVT_TWR_TX_BODY_LEN, 2);
	evq_push(P2P_EVT_TWR_UPLINK, 3, 12, 3);
	zassert_equal(twr_evq_resend_begin(&eq, 5000), 0, "never sent: nothing to resend");
	evq_take(1000, 1, 1, 10);
	evq_take(1000, 2, 2, P2P_EVT_TWR_TX_BODY_LEN);
	evq_take(1100, 3, 3, 12);

	zassert_equal(twr_evq_resend_begin(&eq, 1999), 0, "the oldest waited < 1 s");
	zassert_equal(twr_evq_resend_begin(&eq, 2000), 3, "go-back-N: all outstanding");
	for (uint8_t s = 1; s <= 3; s++) {
		zassert_true(twr_evq_resend_next(&eq, 2000, &eo));
		zassert_equal(eo.seq, s, "oldest first, same seq");
		zassert_equal(eo.body[0], s);
	}
	zassert_false(twr_evq_resend_next(&eq, 2000, &eo));
	zassert_equal(eq.resent, 3);

	zassert_equal(twr_evq_resend_begin(&eq, 3999), 0, "no ack: the next pass waits 2 s");
	zassert_equal(twr_evq_resend_begin(&eq, 4000), 3);
	zassert_true(twr_evq_resend_next(&eq, 4000, &eo));
	zassert_equal(eo.seq, 1);
	zassert_true(twr_evq_ack(&eq, 2), "the host acks during the pass");
	zassert_true(twr_evq_resend_next(&eq, 4000, &eo));
	zassert_equal(eo.seq, 3, "what the ack released is skipped");
	zassert_false(twr_evq_resend_next(&eq, 4000, &eo));
	zassert_equal(eq.resent, 5);

	/* A new event not yet on the link is not part of a pass; it goes out unsent first.
	 * The ack reset the timeout to 1 s. */
	evq_push(P2P_EVT_TWR_TX, 4, P2P_EVT_TWR_TX_BODY_LEN, 4);
	zassert_equal(twr_evq_resend_begin(&eq, 4999), 0);
	zassert_equal(twr_evq_resend_begin(&eq, 5000), 1, "only seq 3");
	zassert_true(twr_evq_resend_next(&eq, 5000, &eo));
	zassert_equal(eo.seq, 3);
	zassert_false(twr_evq_resend_next(&eq, 5000, &eo));
	evq_take(5000, 4, 4, P2P_EVT_TWR_TX_BODY_LEN);

	/* All acked mid-pass: the pass ends. */
	zassert_equal(twr_evq_resend_begin(&eq, 7000), 2);
	zassert_true(twr_evq_ack(&eq, 4));
	zassert_false(twr_evq_resend_next(&eq, 7000, &eo));
	zassert_equal(twr_evq_resend_begin(&eq, 99999), 0, "nothing outstanding");
	zassert_equal(eq.resent, 6);
}

/* One resend pass at now_ms that must be due, covering `n` events. */
static void evq_pass(uint64_t now_ms, size_t n)
{
	zassert_equal(twr_evq_resend_begin(&eq, now_ms), n, "pass at %u", (unsigned int)now_ms);
	while (twr_evq_resend_next(&eq, now_ms, &eo)) {
	}
}

/* #130: a host that does not ack gets resends 1 s, 2 s and then every 4 s apart; an ack that
 * releases an event brings the timeout back to 1 s, an ignored one does not. */
ZTEST(tower_evq, test_resend_backoff)
{
	twr_evq_init(&eq);
	evq_push(P2P_EVT_TWR_UPLINK, 1, 10, 1);
	evq_take(1000, 1, 1, 10);
	zassert_equal(twr_evq_resend_begin(&eq, 1999), 0);
	evq_pass(2000, 1);
	zassert_equal(twr_evq_resend_begin(&eq, 3999), 0);
	evq_pass(4000, 1);
	zassert_equal(twr_evq_resend_begin(&eq, 7999), 0);
	evq_pass(8000, 1);
	zassert_equal(twr_evq_resend_begin(&eq, 11999), 0, "capped at 4 s");
	evq_pass(12000, 1);
	zassert_equal(eq.resent, 4);

	zassert_true(twr_evq_ack(&eq, 1));
	evq_push(P2P_EVT_TWR_UPLINK, 2, 10, 2);
	evq_take(12500, 2, 2, 10);
	zassert_equal(twr_evq_resend_begin(&eq, 13499), 0);
	evq_pass(13500, 1);
	zassert_false(twr_evq_ack(&eq, 1), "an old ack is ignored");
	zassert_equal(twr_evq_resend_begin(&eq, 15499), 0, "and does not reset the backoff");
	evq_pass(15500, 1);
	zassert_equal(eq.resent, 6);
}

ZTEST_SUITE(tower_evq, NULL, NULL, NULL, NULL, NULL);


/* ---- gateway core --------------------------------------------------------------------- */

ZTEST(tower_gwc, test_kat_uplinks_open)
{
	size_t n = 0;

	for (size_t i = 0; i < ARRAY_SIZE(tower_kats); i++) {
		const struct tower_kat *v = &tower_kats[i];

		if (v->frame_type != TWR_T_DATA || v->src != NODE_ADDR) {
			continue;
		}
		gw_setup(1, 4096, v->counter - 1);
		rx_frame(v->frame, v->frame_len, 1000);
		zassert_equal(rx.action, TWR_RXA_UPLINK, "%s", v->name);
		zassert_equal(rx.hdr.counter, v->counter, "%s", v->name);
		zassert_equal(rx.hdr.flags, v->flags, "%s", v->name);
		zassert_equal(rx.pt_len, v->plaintext_len, "%s", v->name);
		zassert_mem_equal(rx.pt, v->plaintext, v->plaintext_len, "%s", v->name);
		zassert_equal(rx.send_ack, (v->flags & TWR_FLAG_CONFIRMED) != 0, "%s", v->name);
		zassert_equal(rx.ack, (v->flags & TWR_FLAG_CONFIRMED) != 0 ? P2P_TWR_ACK_SENT
									    : P2P_TWR_ACK_NONE,
			      "%s", v->name);
		zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, v->counter);
		n++;
	}
	zassert_true(n >= 6, "uplink vectors in the KAT");

	/* the gateway's own frames are not for it (dest = the node) */
	gw_setup(1, 4096, 0);
	rx_frame(kat("ack_no_pending")->frame, kat("ack_no_pending")->frame_len, 1000);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_equal(g.st.not_for_us, 1);
}

ZTEST(tower_gwc, test_kat_ack_pending_downlink)
{
	const struct tower_kat *up = kat("uplink_confirmed");
	const struct tower_kat *ack0 = kat("ack_no_pending");
	const struct tower_kat *ack1 = kat("ack_pending");
	const struct tower_kat *dl = kat("downlink_data");
	struct twr_dl_ref ref;
	struct twr_tx_evt evt;
	bool has_evt;
	size_t n;

	gw_setup(100, 4095, up->counter - 1);

	/* confirmed uplink: forwarded, ACKed with the block's first counter */
	rx_frame(up->frame, up->frame_len, 1000);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_true(rx.send_ack);
	zassert_false(rx.ack_pending);
	zassert_equal(rx.ack, P2P_TWR_ACK_SENT);
	zassert_equal(rx.ack_ctr, 100);
	zassert_equal(rx.ack_len, ack0->frame_len);
	zassert_mem_equal(rx.ack_frame, ack0->frame, ack0->frame_len, "ack_no_pending");
	twr_gwc_ack_result(&g, true, false, false);

	/* a downlink queued; the node retransmits: dup, re-ACKed with PENDING, not forwarded */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 7, 60, 0, dl->plaintext,
					 dl->plaintext_len, 1000),
		      P2P_STATUS_OK);
	rx_frame(up->frame, up->frame_len, 1100);
	zassert_equal(rx.action, TWR_RXA_DUP);
	zassert_true(rx.send_ack);
	zassert_true(rx.ack_pending);
	zassert_equal(rx.ack, P2P_TWR_ACK_PENDING);
	zassert_equal(rx.ack_ctr, 101);
	zassert_mem_equal(rx.ack_frame, ack1->frame, ack1->frame_len, "ack_pending");
	twr_gwc_ack_result(&g, true, false, false);

	/* the downlink: sealed with the next counter at TX time */
	n = twr_gwc_dl_take(&g, NODE_ADDR, 1100, frame, &ref, &evt, &has_evt);
	zassert_false(has_evt);
	zassert_equal(n, dl->frame_len);
	zassert_mem_equal(frame, dl->frame, n, "downlink_data");
	zassert_equal(ref.gw_counter, 102);
	zassert_equal(ref.item, 7);
	zassert_false(ref.confirmed);
	zassert_equal(g.ctr.next, 103, "spent at sealing");
	zassert_equal(twr_gwc_dl_take(&g, NODE_ADDR, 1100, frame, &ref, &evt, &has_evt), 0,
		      "the item is on air, nothing else queued");

	zassert_true(twr_gwc_dl_done(&g, &ref, TWR_DL_TX_OK, 0, &evt));
	zassert_equal(evt.outcome, P2P_TWR_TX_SENT);
	zassert_equal(evt.addr, NODE_ADDR);
	zassert_equal(evt.item, 7);
	zassert_equal(evt.gw_counter, 102);
	zassert_equal(evt.node_ack_counter, 0);
	zassert_equal((uint8_t)evt.ack_rssi, P2P_TWR_RSSI_NONE);
	zassert_equal(twr_dlq_used(&g.dlq), 0);

	zassert_equal(g.st.rx_total, 2);
	zassert_equal(g.st.fresh, 1);
	zassert_equal(g.st.dup, 1);
	zassert_equal(g.st.acks_sent, 2);
	zassert_equal(g.st.dl_sent, 1);
}

ZTEST(tower_gwc, test_classification)
{
	static const uint8_t pt[] = { 0x91, 0x07, 0x00 };
	uint8_t tampered[TWR_MAX_FRAME];
	size_t n;

	gw_setup(1, 4096, 10);

	/* dest 0: a JoinReq, forwarded raw, whatever its src and key */
	n = mk_frame(TWR_T_JOIN_REQ, 0, 0x777, 0, 1, pt, sizeof(pt), other_key, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_RAW);
	zassert_equal(g.st.join_fwd, 1);
	rx_frame(frame, TWR_HDR_LEN, 1000);
	zassert_equal(rx.action, TWR_RXA_RAW, "header only is still the central's");

	/* another gateway's frame */
	n = mk_frame(TWR_T_DATA, 0, NODE_ADDR, 0x4E420002, 11, pt, sizeof(pt), key_00_0f, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_equal(g.st.not_for_us, 1);

	/* unknown src for us: raw, no ACK */
	n = mk_frame(TWR_T_DATA, TWR_FLAG_CONFIRMED, 0x414, GW_ADDR, 1, pt, sizeof(pt),
		     other_key, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_RAW);
	zassert_false(rx.send_ack);
	zassert_equal(g.st.unknown_src, 1);

	/* bad headers */
	rx_frame(frame, TWR_HDR_LEN - 1, 1000);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_false(rx.hdr_valid);
	memcpy(tampered, frame, n);
	tampered[0] = 0x40; /* version 2 */
	rx_frame(tampered, n, 1000);
	zassert_equal(g.st.bad_hdr, 2);
	n = mk_frame(TWR_T_BEACON, 0, NODE_ADDR, GW_ADDR, 11, pt, sizeof(pt), key_00_0f, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_equal(g.st.bad_hdr, 3, "known src, not Data / Ack");
	n = mk_uplink(11, true, frame);
	rx_frame(frame, TWR_HDR_LEN + 4, 1000);
	zassert_equal(g.st.bad_hdr, 4, "shorter than header + tag");

	/* MIC: tampered tag, tampered header (AAD), wrong key */
	memcpy(tampered, frame, n);
	tampered[n - 1] ^= 0x01;
	rx_frame(tampered, n, 1000);
	zassert_equal(rx.action, TWR_RXA_DROP);
	memcpy(tampered, frame, n);
	tampered[1] ^= TWR_FLAG_LAST_CHUNK;
	rx_frame(tampered, n, 1000);
	n = mk_frame(TWR_T_DATA, 0, NODE_ADDR, GW_ADDR, 11, pt, sizeof(pt), other_key, tampered);
	rx_frame(tampered, n, 1000);
	zassert_equal(g.st.mic_fail, 3);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 10, "lane untouched");
	zassert_equal(g.ctr.next, 1, "no counter spent on any of these");

	/* replay: below last_seen, and counter 0 */
	n = mk_uplink(9, true, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_false(rx.send_ack);
	n = mk_uplink(0, true, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(g.st.replay, 2);

	/* unconfirmed fresh: forwarded, no ACK, no counter */
	n = mk_uplink(11, false, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_equal(rx.ack, P2P_TWR_ACK_NONE);
	zassert_false(rx.send_ack);
	zassert_equal(g.ctr.next, 1);

	/* confirmed, and its ACK lands past the node's window: still sealed and sent, counted in
	 * acks_late as well as acks_sent; a radio error is tx_radio_err's, not an ACK count */
	n = mk_uplink(12, true, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_true(rx.send_ack);
	zassert_equal(rx.ack_ctr, 1);
	twr_gwc_ack_result(&g, true, true, false);
	zassert_equal(g.st.acks_sent, 1);
	zassert_equal(g.st.acks_late, 1);
	twr_gwc_ack_result(&g, false, false, false);
	zassert_equal(g.st.acks_sent, 1);
	zassert_equal(g.st.acks_late, 1);
	zassert_equal(g.ctr.next, 2);

	/* ACK_ENABLED off: nothing ACKed */
	g.p.flags = 0;
	n = mk_uplink(13, true, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_equal(rx.ack, P2P_TWR_ACK_NONE);
	g.p.flags = P2P_TWR_START_ACK_ENABLED;

	/* not this gateway's HOME peer: forwarded, not ACKed */
	zassert_equal(twr_gwc_node_add(&g, NODE_ADDR, key_00_0f, 13, 0), P2P_STATUS_OK);
	n = mk_uplink(14, true, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_equal(rx.ack, P2P_TWR_ACK_NONE);
	zassert_equal(g.ctr.next, 2);

	zassert_equal(g.st.rx_total, 17, "every frame, whatever became of it");
	zassert_equal(g.st.fresh, 4);
}

ZTEST(tower_gwc, test_rx_refuses_frames_over_the_tower_mtu)
{
	static uint8_t pt[TWR_MAX_PAYLOAD] = { 0x81, 0x02 };
	size_t n;

	gw_setup(1, 4096, 10);

	/* 74 B plaintext = the 96 B TOWER frame: opened, forwarded and ACKed */
	n = mk_frame(TWR_T_DATA, TWR_FLAG_CONFIRMED, NODE_ADDR, GW_ADDR, 11, pt,
		     TWR_TOWER_MAX_PAYLOAD, key_00_0f, frame);
	zassert_equal(n, TWR_TOWER_MAX_FRAME);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_equal(rx.pt_len, TWR_TOWER_MAX_PAYLOAD);
	zassert_true(rx.send_ack);
	twr_gwc_ack_result(&g, true, false, false);

	/* 97 B (and the old 100 B lora MTU): dropped before the header, no ACK, lane untouched */
	for (size_t extra = 1; extra <= 4; extra += 3) {
		n = mk_frame(TWR_T_DATA, TWR_FLAG_CONFIRMED, NODE_ADDR, GW_ADDR, 12, pt,
			     TWR_TOWER_MAX_PAYLOAD + extra, key_00_0f, frame);
		zassert_equal(n, TWR_TOWER_MAX_FRAME + extra);
		rx_frame(frame, n, 1000);
		zassert_equal(rx.action, TWR_RXA_DROP);
		zassert_false(rx.hdr_valid, "not parsed");
		zassert_false(rx.send_ack);
	}
	zassert_equal(g.st.rx_too_long, 2);
	zassert_equal(g.st.bad_hdr, 0);
	zassert_equal(g.st.mic_fail, 0);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 11);

	/* a JoinReq-shaped frame over the MTU is not forwarded raw either */
	n = mk_frame(TWR_T_JOIN_REQ, 0, 0x777, 0, 1, pt, TWR_TOWER_MAX_PAYLOAD + 1, other_key,
		     frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_equal(g.st.join_fwd, 0);
	zassert_equal(g.st.rx_too_long, 3);
}

ZTEST(tower_gwc, test_downlink_cap_is_the_tower_mtu)
{
	zassert_equal(P2P_TWR_DL_MAX_PT, TWR_TOWER_MAX_PAYLOAD, "#486 U-1");
	zassert_equal(TWR_GWC_DL_MAX_FRAME, TWR_TOWER_MAX_FRAME);
}

ZTEST(tower_gwc, test_confirmed_downlink_delivered_or_not)
{
	static const uint8_t cmd[] = { 0x81, 0x56, 0x01, 0x02 };
	struct twr_tx_evt evt, hk[4];
	struct twr_dl_ref ref;
	bool has_evt;
	size_t n;

	gw_setup(1, 4096, 1);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 9, 3600, P2P_TWR_QUEUE_CONFIRMED, cmd,
					 sizeof(cmd), 1000),
		      P2P_STATUS_OK);
	n = mk_uplink(2, true, frame);
	rx_frame(frame, n, 1000);
	zassert_true(rx.ack_pending);
	zassert_equal(rx.ack_ctr, 1);

	n = twr_gwc_dl_take(&g, NODE_ADDR, 1000, frame, &ref, &evt, &has_evt);
	zassert_equal(n, TWR_HDR_LEN + sizeof(cmd) + TWR_TAG_LEN);
	zassert_equal(frame[1], TWR_FLAG_CONFIRMED, "confirmed Data frame");
	zassert_true(ref.confirmed);
	zassert_equal(ref.gw_counter, 2);
	zassert_false(twr_gwc_dl_done(&g, &ref, TWR_DL_TX_OK, 1300, &evt), "waits for the ACK");
	zassert_equal(g.dlq.e[ref.idx].state, TWR_DL_INFLIGHT);

	/* a node ACK for something else: ignored, lane untouched */
	n = mk_node_ack(3, 77, -70, frame);
	rx_frame(frame, n, 1050);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_equal(g.st.ack_unmatched, 1);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 2);

	/* the node's ACK of our downlink */
	n = mk_node_ack(3, 2, -70, frame);
	rx_frame(frame, n, 1060);
	zassert_equal(rx.action, TWR_RXA_NODE_ACK);
	zassert_false(rx.send_ack, "an ACK is never ACKed");
	zassert_true(rx.has_tx_evt);
	zassert_equal(rx.tx_evt.outcome, P2P_TWR_TX_DELIVERED);
	zassert_equal(rx.tx_evt.item, 9);
	zassert_equal(rx.tx_evt.gw_counter, 2);
	zassert_equal(rx.tx_evt.node_ack_counter, 3);
	zassert_equal(rx.tx_evt.ack_rssi, -70);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 3, "lane moves");
	zassert_equal(twr_dlq_used(&g.dlq), 0);
	zassert_equal(g.st.dl_delivered, 1);

	/* the same ACK again: a retransmit on the lane, nothing to deliver */
	rx_frame(frame, n, 1070);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_equal(g.st.dup, 1);

	/* a 4-byte ACK payload (TOWER accepts >= 4 B) has no RSSI */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 10, 3600, P2P_TWR_QUEUE_CONFIRMED, cmd,
					 sizeof(cmd), 1000),
		      P2P_STATUS_OK);
	n = mk_uplink(4, true, frame);
	rx_frame(frame, n, 1100);
	zassert_true(rx.ack_pending);
	zassert_true(twr_gwc_dl_take(&g, NODE_ADDR, 1100, frame, &ref, &evt, &has_evt) > 0);
	twr_gwc_dl_done(&g, &ref, TWR_DL_TX_OK, 1400, &evt);
	{
		uint8_t pl[4] = { (uint8_t)ref.gw_counter, 0, 0, 0 };

		n = mk_frame(TWR_T_ACK, 0, NODE_ADDR, GW_ADDR, 5, pl, sizeof(pl), key_00_0f,
			     frame);
	}
	rx_frame(frame, n, 1150);
	zassert_equal(rx.action, TWR_RXA_NODE_ACK);
	zassert_equal((uint8_t)rx.tx_evt.ack_rssi, P2P_TWR_RSSI_NONE);

	/* no ACK from the node: NOT_DELIVERED at the deadline */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 11, 3600, P2P_TWR_QUEUE_CONFIRMED, cmd,
					 sizeof(cmd), 1000),
		      P2P_STATUS_OK);
	n = mk_uplink(6, true, frame);
	rx_frame(frame, n, 1200);
	zassert_true(twr_gwc_dl_take(&g, NODE_ADDR, 1200, frame, &ref, &evt, &has_evt) > 0);
	twr_gwc_dl_done(&g, &ref, TWR_DL_TX_OK, 1500, &evt);
	zassert_equal(twr_gwc_housekeep(&g, 1499, hk, ARRAY_SIZE(hk)), 0);
	zassert_equal(twr_gwc_housekeep(&g, 1500, hk, ARRAY_SIZE(hk)), 1);
	zassert_equal(hk[0].outcome, P2P_TWR_TX_NOT_DELIVERED);
	zassert_equal(hk[0].item, 11);
	zassert_equal(hk[0].gw_counter, ref.gw_counter);
	zassert_equal(g.st.dl_not_delivered, 1);

	/* a late ACK after that is unmatched */
	n = mk_node_ack(7, ref.gw_counter, -70, frame);
	rx_frame(frame, n, 1600);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_equal(g.st.ack_unmatched, 2);
}

ZTEST(tower_gwc, test_ttl_cap_late_radio_err_drop)
{
	static const uint8_t pl[40] = { 0x81 };
	struct twr_tx_evt evt, hk[4];
	struct twr_dl_ref ref;
	bool has_evt;
	size_t n;

	gw_setup(1, 4096, 1);

	/* TTL: expired entries are no longer PENDING, housekeeping reports them */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 1, 2, 0, pl, 5, 1000), P2P_STATUS_OK);
	n = mk_uplink(2, true, frame);
	rx_frame(frame, n, 3000);
	zassert_false(rx.ack_pending, "expired at 3000");
	zassert_equal(twr_gwc_housekeep(&g, 2999, hk, ARRAY_SIZE(hk)), 0);
	zassert_equal(twr_gwc_housekeep(&g, 3000, hk, ARRAY_SIZE(hk)), 1);
	zassert_equal(hk[0].outcome, P2P_TWR_TX_EXPIRED);
	zassert_equal(hk[0].item, 1);
	zassert_equal(hk[0].gw_counter, 0);
	zassert_equal(g.st.dl_expired, 1);

	/* ToA cap: a push over it is refused, a queued frame the cap shrinks under fails */
	twr_gwc_set_max_frame(&g, 50);
	zassert_equal(twr_gwc_dl_max_pt(&g), 50 - TWR_HDR_LEN - TWR_TAG_LEN);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 2, 60, 0, pl, 40, 3000),
		      P2P_STATUS_BAD_PARAM);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 2, 60, 0, pl, 20, 3000), P2P_STATUS_OK);
	twr_gwc_set_max_frame(&g, 30);
	n = mk_uplink(3, true, frame);
	rx_frame(frame, n, 3000);
	zassert_false(rx.ack_pending, "does not fit the cap any more");
	zassert_equal(twr_gwc_housekeep(&g, 3000, hk, ARRAY_SIZE(hk)), 1);
	zassert_equal(hk[0].outcome, P2P_TWR_TX_RADIO_ERR);
	zassert_equal(hk[0].item, 2);
	twr_gwc_set_max_frame(&g, TWR_GWC_DL_MAX_FRAME);
	zassert_equal(twr_gwc_dl_max_pt(&g), P2P_TWR_DL_MAX_PT);

	/* a sealed downlink that missed its slot goes back and is sealed again, fresh counter */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 3, 60, 0, pl, 5, 3000), P2P_STATUS_OK);
	zassert_true(twr_gwc_dl_take(&g, NODE_ADDR, 3000, frame, &ref, &evt, &has_evt) > 0);
	zassert_false(twr_gwc_dl_done(&g, &ref, TWR_DL_TX_LATE, 0, &evt));
	zassert_equal(g.st.dl_late, 1);
	{
		uint32_t first = ref.gw_counter;

		zassert_true(twr_gwc_dl_take(&g, NODE_ADDR, 3000, frame, &ref, &evt, &has_evt) >
			     0);
		zassert_equal(ref.gw_counter, first + 1, "never the same nonce twice");
	}

	/* radio error */
	zassert_true(twr_gwc_dl_done(&g, &ref, TWR_DL_TX_ERR, 0, &evt));
	zassert_equal(evt.outcome, P2P_TWR_TX_RADIO_ERR);
	zassert_equal(evt.item, 3);
	zassert_equal(evt.gw_counter, ref.gw_counter);

	/* dropped while on air: no event */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 4, 60, 0, pl, 5, 3000), P2P_STATUS_OK);
	zassert_true(twr_gwc_dl_take(&g, NODE_ADDR, 3000, frame, &ref, &evt, &has_evt) > 0);
	zassert_equal(twr_gwc_queue_drop(&g, NODE_ADDR, 4), P2P_STATUS_OK);
	zassert_false(twr_gwc_dl_done(&g, &ref, TWR_DL_TX_OK, 0, &evt));
	zassert_equal(twr_dlq_used(&g.dlq), 0);
}

ZTEST(tower_gwc, test_counter_exhaustion_fail_closed)
{
	static const uint8_t pl[] = { 0x91, 0x21 };
	struct twr_tx_evt evt;
	struct twr_dl_ref ref;
	uint32_t next, last;
	bool has_evt;
	size_t n;

	/* two counters: one ACK with PENDING and its downlink */
	gw_setup(100, 101, 1);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 5, 60, 0, pl, sizeof(pl), 1000),
		      P2P_STATUS_OK);
	n = mk_uplink(2, true, frame);
	rx_frame(frame, n, 1000);
	zassert_true(rx.ack_pending);
	zassert_equal(rx.ack_ctr, 100);
	zassert_true(twr_gwc_dl_take(&g, NODE_ADDR, 1000, frame, &ref, &evt, &has_evt) > 0);
	zassert_equal(ref.gw_counter, 101);
	zassert_true(twr_gwc_dl_done(&g, &ref, TWR_DL_TX_OK, 0, &evt));
	zassert_true(twr_ctr_low_take(&g.ctr, &next, &last));
	zassert_equal(next, 102);
	zassert_equal(last, 101);

	/* exhausted: no ACK, the queued head gets NO_COUNTER, nothing is sealed */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 6, 60, 0, pl, sizeof(pl), 1000),
		      P2P_STATUS_OK);
	n = mk_uplink(3, true, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_UPLINK, "still forwarded");
	zassert_equal(rx.ack, P2P_TWR_ACK_NO_COUNTER);
	zassert_false(rx.send_ack);
	zassert_true(rx.has_tx_evt);
	zassert_equal(rx.tx_evt.outcome, P2P_TWR_TX_NO_COUNTER);
	zassert_equal(rx.tx_evt.item, 6);
	zassert_equal(twr_dlq_used(&g.dlq), 0);
	zassert_equal(g.st.acks_suppressed, 1);
	zassert_equal(g.st.dl_no_counter, 1);
	zassert_false(twr_ctr_low_take(&g.ctr, &next, &last), "once per block");

	/* one counter left: the ACK takes it, the head cannot follow */
	zassert_equal(twr_gwc_ctr_block(&g, 300, 300), P2P_STATUS_OK);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 8, 60, 0, pl, sizeof(pl), 1000),
		      P2P_STATUS_OK);
	n = mk_uplink(4, true, frame);
	rx_frame(frame, n, 1000);
	zassert_true(rx.send_ack);
	zassert_false(rx.ack_pending);
	zassert_equal(rx.ack, P2P_TWR_ACK_SENT);
	zassert_equal(rx.ack_ctr, 300);
	zassert_true(rx.has_tx_evt);
	zassert_equal(rx.tx_evt.outcome, P2P_TWR_TX_NO_COUNTER);
	zassert_equal(rx.tx_evt.item, 8);

	/* a new block and everything flows again */
	zassert_equal(twr_gwc_ctr_block(&g, 400, 4495), P2P_STATUS_OK);
	n = mk_uplink(5, true, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.ack, P2P_TWR_ACK_SENT);
	zassert_equal(rx.ack_ctr, 400);
}

ZTEST(tower_gwc, test_ctr_block_never_goes_back)
{
	uint32_t c;

	twr_gwc_init(&g);
	zassert_equal(twr_gwc_ctr_block(&g, 100, 199), P2P_STATUS_OK);
	for (int i = 0; i < 10; i++) {
		zassert_true(twr_ctr_take(&g.ctr, &c));
	}
	zassert_equal(g.ctr.next, 110);
	zassert_equal(twr_gwc_ctr_block(&g, 1, 109), P2P_STATUS_BAD_PARAM, "ends below next");
	zassert_equal(g.ctr.next, 110, "the old block stays");
	zassert_equal(g.ctr.last, 199);
	zassert_equal(twr_gwc_ctr_block(&g, 1, 110), P2P_STATUS_OK, "next itself is left");
	zassert_equal(g.ctr.next, 110, "continues from next, never reuses");
	zassert_equal(g.ctr.last, 110);
	zassert_true(twr_ctr_take(&g.ctr, &c));
	zassert_equal(c, 110);
	zassert_equal(twr_ctr_left(&g.ctr), 0);
	zassert_equal(twr_gwc_ctr_block(&g, 110, 110), P2P_STATUS_BAD_PARAM, "used up");
	zassert_equal(twr_gwc_ctr_block(&g, 5000, 5999), P2P_STATUS_OK, "a fresh range above");
	zassert_equal(g.ctr.next, 5000);
}

ZTEST(tower_gwc, test_commands)
{
	static const uint8_t pl[P2P_TWR_DL_MAX_PT] = { 0x81 };
	struct twr_gwc_params p = { .net_id = GW_ADDR, .turnaround_ms = 20, .dl_gap_ms = 20 };
	struct twr_tx_evt evt;
	struct twr_dl_ref ref;
	bool has_evt;
	size_t n;

	twr_gwc_init(&g);
	zassert_equal(twr_gwc_start(&g, &p), P2P_STATUS_INVALID_STATE, "no counter block");
	zassert_equal(twr_gwc_ctr_block(&g, 1, 100), P2P_STATUS_OK);
	zassert_equal(twr_gwc_node_add(&g, GW_ADDR, key_00_0f, 0, P2P_TWR_NODE_HOME),
		      P2P_STATUS_OK, "net_id unknown before TWR_START");
	zassert_equal(twr_gwc_start(&g, &p), P2P_STATUS_BAD_PARAM, "a peer holds net_id");
	zassert_false(g.started);
	zassert_equal(twr_gwc_node_remove(&g, GW_ADDR), P2P_STATUS_OK);
	zassert_equal(twr_gwc_start(&g, &p), P2P_STATUS_OK);
	zassert_equal(twr_gwc_start(&g, &p), P2P_STATUS_OK, "idempotent");
	zassert_equal(twr_gwc_node_add(&g, GW_ADDR, key_00_0f, 0, P2P_TWR_NODE_HOME),
		      P2P_STATUS_BAD_PARAM);
	zassert_equal(twr_gwc_node_remove(&g, NODE_ADDR), P2P_STATUS_NOT_FOUND);

	/* a full table */
	for (uint32_t i = 0; i < TWR_PEERS_MAX; i++) {
		zassert_equal(twr_gwc_node_add(&g, 0x100 + i, key_00_0f, 0, P2P_TWR_NODE_HOME),
			      P2P_STATUS_OK);
	}
	zassert_equal(twr_gwc_node_add(&g, NODE_ADDR, key_00_0f, 0, P2P_TWR_NODE_HOME),
		      P2P_STATUS_NO_SPACE);
	zassert_equal(twr_gwc_node_add(&g, 0x100, key_00_0f, 5, P2P_TWR_NODE_HOME),
		      P2P_STATUS_OK, "replace when full");

	/* queue: unknown peer, pool full, replace, busy */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 1, 60, 0, pl, 1, 0),
		      P2P_STATUS_NOT_FOUND);
	for (uint16_t i = 0; i < TWR_DLQ_MAX; i++) {
		zassert_equal(twr_gwc_queue_push(&g, 0x100 + (i % 4), i + 1, 60, 0, pl,
						 P2P_TWR_DL_MAX_PT, 0),
			      P2P_STATUS_OK, "item %u", i + 1);
	}
	zassert_equal(twr_gwc_queue_push(&g, 0x105, 99, 60, 0, pl, 1, 0), P2P_STATUS_NO_SPACE);
	zassert_equal(twr_gwc_queue_push(&g, 0x100, 1, 120, P2P_TWR_QUEUE_CONFIRMED, pl, 3, 0),
		      P2P_STATUS_OK, "same item again: replaced in place");
	zassert_equal(twr_dlq_used(&g.dlq), TWR_DLQ_MAX);
	{
		int i = twr_dlq_find(&g.dlq, 0x100, 1);

		zassert_equal(g.dlq.e[i].len, 3);
		zassert_equal(g.dlq.e[i].flags, P2P_TWR_QUEUE_CONFIRMED);
		zassert_equal(g.dlq.e[i].expires_ms, 120000);
	}
	n = twr_gwc_dl_take(&g, 0x100, 0, frame, &ref, &evt, &has_evt);
	zassert_true(n > 0);
	zassert_equal(ref.item, 1, "oldest first");
	zassert_equal(twr_gwc_queue_push(&g, 0x100, 1, 60, 0, pl, 1, 0), P2P_STATUS_BUSY,
		      "on air");
	twr_gwc_dl_done(&g, &ref, TWR_DL_TX_OK, 500, &evt);
	zassert_equal(twr_gwc_queue_push(&g, 0x100, 1, 60, 0, pl, 1, 0), P2P_STATUS_BUSY,
		      "awaiting the node ACK");

	/* drop */
	zassert_equal(twr_gwc_queue_drop(&g, NODE_ADDR, 0), P2P_STATUS_NOT_FOUND, "no peer");
	zassert_equal(twr_gwc_queue_drop(&g, 0x1FF, 0), P2P_STATUS_OK, "empty queue, item 0");
	zassert_equal(twr_gwc_queue_drop(&g, 0x101, 1), P2P_STATUS_NOT_FOUND, "not its item");
	zassert_equal(twr_gwc_queue_drop(&g, 0x101, 2), P2P_STATUS_OK);
	zassert_equal(twr_gwc_queue_drop(&g, 0x101, 2), P2P_STATUS_NOT_FOUND);
	zassert_equal(twr_dlq_used(&g.dlq), TWR_DLQ_MAX - 1);

	/* a replacing NODE_ADD and a NODE_REMOVE drop the peer's queue, in flight included */
	zassert_equal(twr_gwc_node_add(&g, 0x100, other_key, 9, P2P_TWR_NODE_HOME),
		      P2P_STATUS_OK);
	zassert_equal(twr_dlq_find(&g.dlq, 0x100, 1), -1);
	zassert_equal(twr_dlq_used(&g.dlq), TWR_DLQ_MAX - 1 - 8);
	zassert_equal(twr_gwc_node_remove(&g, 0x102), P2P_STATUS_OK);
	zassert_equal(twr_dlq_used(&g.dlq), TWR_DLQ_MAX - 1 - 16);
	zassert_equal(twr_gwc_queue_drop(&g, 0x102, 0), P2P_STATUS_NOT_FOUND);
}

static uint32_t le32(const uint8_t *p)
{
	return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
	       ((uint32_t)p[3] << 24);
}

static uint16_t le16(const uint8_t *p)
{
	return (uint16_t)(p[0] | (p[1] << 8));
}

/* link §4.2 KEEP_NEWER, on the core. */
ZTEST(tower_gwc, test_node_add_keep_newer)
{
	const uint8_t keep = P2P_TWR_NODE_HOME | P2P_TWR_NODE_KEEP_NEWER;
	uint8_t pl[4] = { 0x81, 1, 2, 3 };
	size_t n;

	gw_setup(1, 100, 10);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 5, 60, 0, pl, sizeof(pl), 0),
		      P2P_STATUS_OK);

	/* The NB accepted counter 20 while the host restarted with last_seen 10 persisted. */
	n = mk_uplink(20, false, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_equal(twr_gwc_node_add(&g, NODE_ADDR, key_00_0f, 10, keep), P2P_STATUS_OK);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 20, "max(own, sent)");
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->flags, P2P_TWR_NODE_HOME);
	zassert_equal(twr_dlq_used(&g.dlq), 1, "same key keeps the queue");
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_DUP, "counter 20 is not fresh again");
	n = mk_uplink(15, false, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(g.st.replay, 1, "the lane did not move back");

	zassert_equal(twr_gwc_node_add(&g, NODE_ADDR, key_00_0f, 30, keep), P2P_STATUS_OK);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 30, "sent is newer");

	/* Without the flag the sent value is taken as is (the host's exact restore). */
	zassert_equal(twr_gwc_node_add(&g, NODE_ADDR, key_00_0f, 25, P2P_TWR_NODE_HOME),
		      P2P_STATUS_OK);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 25);

	/* A different key is a new session: the sent value, the queue dropped. */
	zassert_equal(twr_gwc_node_add(&g, NODE_ADDR, other_key, 0, keep), P2P_STATUS_OK);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 0);
	zassert_equal(twr_dlq_used(&g.dlq), 0);
}

/* link §4.1 backpressure: a frame whose events do not fit the ring is dropped whole. */
ZTEST(tower_gwc, test_ring_full_drops_the_frame_whole)
{
	uint8_t b[P2P_EVT_TWR_TX_BODY_LEN] = { 0 };
	uint8_t s;
	size_t n;

	gw_setup(1, 1000, 10);
	/* 30 outstanding: a confirmed fresh frame needs 3 slots (uplink + 2 for the ACK's). */
	for (int i = 0; i < 30; i++) {
		zassert_true(twr_evq_push(&g.evq, P2P_EVT_TWR_TX, b, sizeof(b), &s));
	}
	n = mk_uplink(11, true, frame);
	rx_frame(frame, n, 1000);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_false(rx.send_ack, "no ACK on air");
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 10, "no last_seen move");
	zassert_equal(g.st.acks_suppressed, 1);
	zassert_equal(g.st.rx_ring_full, 1);
	zassert_equal(g.st.fresh, 0);
	zassert_equal(g.ctr.next, 1, "no counter spent");

	/* The node's repetition is still fresh, so it is not re-ACKed either. */
	rx_frame(frame, n, 1100);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_false(rx.send_ack);
	zassert_equal(g.st.acks_suppressed, 2);

	/* An unconfirmed fresh frame needs one slot: it fits now... */
	n = mk_uplink(12, false, frame);
	rx_frame(frame, n, 1200);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_true(twr_evq_push(&g.evq, P2P_EVT_TWR_UPLINK, b, sizeof(b), &s));
	zassert_true(twr_evq_push(&g.evq, P2P_EVT_TWR_TX, b, sizeof(b), &s));
	zassert_equal(twr_evq_used(&g.evq), TWR_EVQ_MAX);
	/* ...and not once the ring is full. */
	n = mk_uplink(13, false, frame);
	rx_frame(frame, n, 1300);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 12);
	zassert_equal(g.st.acks_suppressed, 3, "every fresh frame refused");

	/* A retransmit of the accepted 12 that would be re-ACKed: dropped, not suppressed. */
	n = mk_uplink(12, true, frame);
	rx_frame(frame, n, 1400);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_false(rx.send_ack);
	zassert_equal(g.st.rx_ring_full, 4);
	zassert_equal(g.st.acks_suppressed, 3);

	/* The host acks: the node's next repetition goes through and is ACKed. */
	zassert_true(twr_evq_ack(&g.evq, 3));
	n = mk_uplink(13, true, frame);
	rx_frame(frame, n, 1500);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_true(rx.send_ack);
	zassert_equal(rx.ack, P2P_TWR_ACK_SENT);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 13);
}

ZTEST(tower_gwc, test_ring_full_defers_housekeeping_and_node_acks)
{
	uint8_t b[P2P_EVT_TWR_TX_BODY_LEN] = { 0 };
	uint8_t pl[4] = { 0x81, 1, 2, 3 };
	struct twr_tx_evt evts[4];
	struct twr_dl_ref ref;
	struct twr_tx_evt evt;
	bool has_evt;
	uint8_t s;
	size_t n;

	gw_setup(1, 1000, 10);
	for (uint16_t i = 1; i <= 3; i++) {
		zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, i, 1, 0, pl, sizeof(pl), 0),
			      P2P_STATUS_OK);
	}
	for (int i = 0; i < TWR_EVQ_MAX - 1; i++) {
		zassert_true(twr_evq_push(&g.evq, P2P_EVT_TWR_TX, b, sizeof(b), &s));
	}
	zassert_equal(twr_gwc_housekeep(&g, 5000, evts, ARRAY_SIZE(evts)), 1, "one slot left");
	zassert_equal(twr_dlq_used(&g.dlq), 2, "the rest waits in the queue");
	zassert_true(twr_evq_push(&g.evq, P2P_EVT_TWR_TX, b, sizeof(b), &s));
	zassert_equal(twr_gwc_housekeep(&g, 5000, evts, ARRAY_SIZE(evts)), 0);
	zassert_true(twr_evq_ack(&g.evq, s));
	zassert_equal(twr_gwc_housekeep(&g, 5000, evts, ARRAY_SIZE(evts)), 2);

	/* A node ACK that would deliver an in-flight downlink: no slot, not heard. */
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 9, 60, P2P_TWR_QUEUE_CONFIRMED, pl,
					 sizeof(pl), 5000), P2P_STATUS_OK);
	zassert_true(twr_gwc_dl_take(&g, NODE_ADDR, 5000, frame, &ref, &evt, &has_evt) > 0);
	zassert_false(twr_gwc_dl_done(&g, &ref, TWR_DL_TX_OK, 9000, &evt));
	for (int i = 0; i < TWR_EVQ_MAX; i++) {
		zassert_true(twr_evq_push(&g.evq, P2P_EVT_TWR_TX, b, sizeof(b), &s));
	}
	n = mk_node_ack(11, ref.gw_counter, -70, frame);
	rx_frame(frame, n, 5100);
	zassert_equal(rx.action, TWR_RXA_DROP);
	zassert_false(rx.has_tx_evt);
	zassert_equal(twr_peers_find(&g.peers, NODE_ADDR)->last_seen, 10);
	zassert_equal(twr_dlq_used(&g.dlq), 1, "still in flight");
	zassert_true(twr_evq_ack(&g.evq, s));
	rx_frame(frame, n, 5200);
	zassert_equal(rx.action, TWR_RXA_NODE_ACK);
	zassert_equal(rx.tx_evt.outcome, P2P_TWR_TX_DELIVERED);
}

ZTEST(tower_gwc, test_info_and_node_list)
{
	static const uint8_t pl[] = { 0x81 };
	struct twr_info in;
	uint8_t out[P2P_TWR_NODE_LIST_HDR_LEN + P2P_TWR_NODE_LIST_MAX *
		    P2P_TWR_NODE_LIST_ENTRY_LEN];
	size_t n;

	gw_setup(10, 99, 1);
	n = mk_uplink(2, true, frame);
	rx_frame(frame, n, 1000);
	twr_gwc_ack_result(&g, true, false, false);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 1, 60, 0, pl, 1, 1000), P2P_STATUS_OK);
	memset(&in, 0, sizeof(in));
	twr_gwc_info(&g, &in);
	zassert_equal(in.net_id, GW_ADDR);
	zassert_equal(in.turnaround_ms, 20);
	zassert_equal(in.dl_gap_ms, 20);
	zassert_equal(in.ctr[P2P_INFO_RX_TOTAL], 1);
	zassert_equal(in.ctr[P2P_INFO_RX_FRESH], 1);
	zassert_equal(in.ctr[P2P_INFO_ACKS_SENT], 1);
	zassert_equal(in.ctr[P2P_INFO_ACKS_SUPPRESSED], 0);
	zassert_equal(in.ctr_next, 11);
	zassert_equal(in.ctr_last, 99);
	zassert_equal(in.peers, 1);
	zassert_equal(in.peers_max, TWR_PEERS_MAX);
	zassert_equal(in.queue_used, 1);
	zassert_equal(in.queue_max, TWR_DLQ_MAX);
	/* the radio, link and diag counters are cmd_dispatch.c's: the core leaves them alone */
	zassert_equal(in.ctr[P2P_INFO_TX_TOTAL], 0);
	zassert_equal(in.ctr[P2P_INFO_LINK_ERR], 0);

	/* the contract's floors: 256 peers, 16 queued downlinks */
	zassert_true(TWR_PEERS_MAX >= 256);
	zassert_true(TWR_DLQ_MAX >= 16);

	twr_gwc_init(&g);
	memset(&in, 0, sizeof(in));
	twr_gwc_info(&g, &in);
	zassert_equal(in.net_id, 0, "no TWR_START yet");
	zassert_equal(in.turnaround_ms, 0);
	zassert_equal(in.ctr_next, 0, "no block: ctr_next 0");
	zassert_equal(in.ctr_last, 0);

	/* 30 peers: 24 + 6, never a key */
	for (uint32_t i = 0; i < 30; i++) {
		zassert_equal(twr_gwc_node_add(&g, 0x500 + i, key_00_0f, 1000 + i,
					       i % 2 ? P2P_TWR_NODE_HOME : 0),
			      P2P_STATUS_OK);
	}
	n = twr_gwc_node_list(&g, 0, out);
	zassert_equal(n, P2P_TWR_NODE_LIST_HDR_LEN + 24 * P2P_TWR_NODE_LIST_ENTRY_LEN);
	zassert_equal(le16(&out[0]), 30, "total");
	zassert_equal(le16(&out[2]), 24, "next");
	zassert_equal(out[4], 24);
	zassert_equal(le32(&out[5]), 0x500);
	zassert_equal(le32(&out[9]), 1000);
	zassert_equal(out[13], 0);
	zassert_equal(le32(&out[5 + 9]), 0x501);
	zassert_equal(out[13 + 9], P2P_TWR_NODE_HOME);
	for (size_t i = 0; i + sizeof(key_00_0f) <= n; i++) {
		zassert_true(memcmp(&out[i], key_00_0f, 4) != 0 || i + 4 > n,
			     "key bytes at %zu", i);
	}
	n = twr_gwc_node_list(&g, 24, out);
	zassert_equal(n, P2P_TWR_NODE_LIST_HDR_LEN + 6 * P2P_TWR_NODE_LIST_ENTRY_LEN);
	zassert_equal(le16(&out[0]), 30);
	zassert_equal(le16(&out[2]), P2P_TWR_NODE_LIST_END);
	zassert_equal(out[4], 6);
	zassert_equal(le32(&out[5 + 5 * 9]), 0x500 + 29);
	n = twr_gwc_node_list(&g, 30, out);
	zassert_equal(n, P2P_TWR_NODE_LIST_HDR_LEN);
	zassert_equal(le16(&out[2]), P2P_TWR_NODE_LIST_END);
	zassert_equal(out[4], 0);
	n = twr_gwc_node_list(&g, 0xFFFF, out);
	zassert_equal(out[4], 0);

	twr_gwc_init(&g);
	n = twr_gwc_node_list(&g, 0, out);
	zassert_equal(n, P2P_TWR_NODE_LIST_HDR_LEN);
	zassert_equal(le16(&out[0]), 0);
	zassert_equal(le16(&out[2]), P2P_TWR_NODE_LIST_END);
}

ZTEST_SUITE(tower_gwc, NULL, NULL, NULL, NULL, NULL);
