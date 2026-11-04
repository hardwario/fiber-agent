/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for the answers in the ACK (#470 §13.5, N1 part A): the CTRL tail builder
 * (twr_ackctrl.c) and the gateway core sealing it (twr_gwc.c), against the shared KAT
 * plan/control/radio/p2p_tower_ack_ctrl_kat.txt (ack_ctrl_kat.h at build time) and edge
 * cases of its own.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/byteorder.h>
#include <zephyr/sys/util.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "proto.h"
#include "twr_ackctrl.h"
#include "twr_airtime.h"
#include "twr_crypto.h"
#include "twr_frame.h"
#include "twr_gwc.h"
#include "twr_link.h"

#include "ack_ctrl_kat.h"

#define GW_ADDR   0x4E420001u
#define NODE_ADDR 0x00000413u

static const uint8_t key[16] = {
	0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
	0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
};

static struct twr_gwc g;
static struct twr_gwc_rx rx;

/* ---- KAT line parsing ------------------------------------------------------------------- */

struct kline {
	char buf[1024];
	const char *name;
	const char *kind;
	int nf;
	const char *k[32];
	const char *v[32];
};

/* Split at spaces in place; returns the next token or NULL. */
static char *next_tok(char **p)
{
	char *s = *p;
	char *e;

	while (*s == ' ') {
		s++;
	}
	if (*s == '\0') {
		return NULL;
	}
	e = strchr(s, ' ');
	if (e != NULL) {
		*e = '\0';
		*p = e + 1;
	} else {
		*p = s + strlen(s);
	}
	return s;
}

static void kparse(struct kline *l, const char *src)
{
	char *cur = l->buf;
	char *t;

	zassert_true(strlen(src) < sizeof(l->buf), "line too long");
	strcpy(l->buf, src);
	l->nf = 0;
	l->name = next_tok(&cur);
	l->kind = next_tok(&cur);
	zassert_not_null(l->kind, "kind");
	while ((t = next_tok(&cur)) != NULL) {
		char *eq = strchr(t, '=');

		zassert_not_null(eq, "%s: token %s", l->name, t);
		zassert_true(l->nf < (int)ARRAY_SIZE(l->k), "%s: too many fields", l->name);
		*eq = '\0';
		l->k[l->nf] = t;
		l->v[l->nf] = eq + 1;
		l->nf++;
	}
}

static const char *kget(const struct kline *l, const char *key_name)
{
	for (int i = 0; i < l->nf; i++) {
		if (strcmp(l->k[i], key_name) == 0) {
			return l->v[i];
		}
	}
	zassert_unreachable("%s: no %s", l->name, key_name);
	return "";
}

static long long kint(const struct kline *l, const char *key_name)
{
	return strtoll(kget(l, key_name), NULL, 0);
}

static size_t khex(const struct kline *l, const char *key_name, uint8_t *out, size_t cap)
{
	const char *s = kget(l, key_name);
	size_t n = strlen(s) / 2;

	if (strcmp(s, "-") == 0) {
		return 0;
	}
	zassert_true(strlen(s) % 2 == 0 && n <= cap, "%s: %s", l->name, key_name);
	for (size_t i = 0; i < n; i++) {
		char b[3] = { s[2 * i], s[2 * i + 1], 0 };

		out[i] = (uint8_t)strtoul(b, NULL, 16);
	}
	return n;
}

static size_t seal(uint8_t type, uint8_t flags, uint32_t src, uint32_t dest, uint32_t ctr,
		   const uint8_t *pt, size_t len, uint8_t *out)
{
	struct twr_aes128 aes;
	struct twr_hdr h = {
		.type = type, .flags = flags, .src = src, .dest = dest, .counter = ctr,
	};

	twr_aes128_init(&aes, key);
	return twr_frame_seal(&aes, &h, pt, len, TWR_MAX_PAYLOAD, out, TWR_MAX_FRAME);
}

static void setup(uint32_t ctr_first, uint32_t last_seen)
{
	struct twr_gwc_params p = {
		.net_id = GW_ADDR,
		.turnaround_ms = 20,
		.dl_gap_ms = 20,
		.flags = P2P_TWR_START_ACK_ENABLED,
	};

	twr_gwc_init(&g);
	zassert_equal(twr_gwc_ctr_block(&g, ctr_first, ctr_first + 1000), P2P_STATUS_OK);
	zassert_equal(twr_gwc_node_add(&g, NODE_ADDR, key, last_seen, P2P_TWR_NODE_HOME),
		      P2P_STATUS_OK);
	zassert_equal(twr_gwc_start(&g, &p), P2P_STATUS_OK);
}

/* One "ack" line through the core, with the uplink fresh (rep = false) or a repetition of
 * the last one seen (rep = true): both must give the same ACK for the same reception. */
static void run_ack_line(const struct kline *l, bool rep)
{
	static uint8_t up_pt[TWR_MAX_PAYLOAD];
	static uint8_t up_frame[TWR_MAX_FRAME];
	static uint8_t want_pt[64];
	static uint8_t want_frame[TWR_MAX_FRAME];
	static uint8_t frame[TWR_MAX_FRAME];
	static uint8_t pt[TWR_MAX_PAYLOAD];
	static const uint8_t dl[] = { 0x81, 0x56, 0x01 };
	uint32_t up_ctr = (uint32_t)kint(l, "up_ctr");
	uint32_t ack_ctr = (uint32_t)kint(l, "ack_ctr");
	size_t up_len = khex(l, "up_pt", up_pt, sizeof(up_pt));
	size_t uf_len = khex(l, "up_frame", up_frame, sizeof(up_frame));
	size_t wp_len = khex(l, "ack_pt", want_pt, sizeof(want_pt));
	size_t wf_len = khex(l, "ack_frame", want_frame, sizeof(want_frame));
	struct twr_rx_meta m = {
		.rssi_dbm = (int)kint(l, "rssi_dbm"),
		.snr_q = (int8_t)kint(l, "snr_q"),
		.sf = (uint8_t)kint(l, "sf"),
		.t_rx_ms = (uint64_t)kint(l, "t_rx_ms"),
	};
	bool anchor = strcmp(kget(l, "anchor_now_ms"), "-") != 0;
	struct twr_hdr h;
	size_t n;
	size_t pn;
	uint32_t no_anchor_before;

	/* The node's side of the vector: its uplink seals to up_frame. */
	n = seal(TWR_T_DATA, (uint8_t)kint(l, "up_flags"), NODE_ADDR, GW_ADDR, up_ctr, up_pt,
		 up_len, frame);
	zassert_equal(n, uf_len, "%s: up_frame length", l->name);
	zassert_mem_equal(frame, up_frame, n, "%s: up_frame", l->name);

	setup(ack_ctr, rep ? up_ctr : up_ctr - 1);
	if (anchor) {
		uint64_t now = (uint64_t)kint(l, "anchor_now_ms");

		zassert_equal(twr_gwc_time_sync(&g, now, (uint64_t)kint(l, "anchor_unix_ms"), now),
			      P2P_STATUS_OK);
	}
	if (kint(l, "pending")) {
		zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 7, 600, 0, dl, sizeof(dl), 0),
			      P2P_STATUS_OK);
	}
	no_anchor_before = g.st.ack_time_no_anchor;
	twr_gwc_on_rx(&g, up_frame, uf_len, &m, m.t_rx_ms, &rx);

	zassert_equal(rx.action, rep ? TWR_RXA_DUP : TWR_RXA_UPLINK, "%s: action", l->name);
	zassert_true(rx.send_ack, "%s: ACKed", l->name);
	zassert_equal(rx.ack_ctr, ack_ctr, "%s: ack counter", l->name);
	zassert_equal(rx.ack_len, wf_len, "%s/%d: ACK %u B, KAT %zu B", l->name, rep, rx.ack_len,
		      wf_len);
	zassert_mem_equal(rx.ack_frame, want_frame, wf_len, "%s/%d: ACK frame", l->name, rep);
	zassert_equal(rx.ack_ctrl, wp_len > TWR_ACK_PAYLOAD_LEN, "%s: ack_ctrl", l->name);
	zassert_equal(rx.ack_pending, kint(l, "pending") != 0, "%s: pending", l->name);
	/* EVT_TWR_UPLINK ack: the code in bits 0..5, b7 a tail went out, b6 it holds TimeAns. */
	{
		bool has_time = false;
		size_t off = TWR_ACK_PAYLOAD_LEN;
		const uint8_t *body;
		uint8_t id;
		uint8_t bl;
		uint8_t want = kint(l, "pending") ? P2P_TWR_ACK_PENDING : P2P_TWR_ACK_SENT;

		while (twr_ackctrl_next(want_pt, wp_len, &off, &id, &body, &bl)) {
			has_time |= id == TWR_CTRL_TIME;
		}
		want |= wp_len > TWR_ACK_PAYLOAD_LEN ? P2P_TWR_ACK_CTRL : 0u;
		want |= has_time ? P2P_TWR_ACK_TIME : 0u;
		zassert_equal(rx.ack, want, "%s/%d: ack 0x%02x, want 0x%02x", l->name, rep, rx.ack,
			      want);
	}
	if (!rep) {
		/* The whole plaintext still goes to the central, requests included. */
		zassert_equal(rx.pt_len, up_len, "%s", l->name);
		zassert_mem_equal(rx.pt, up_pt, up_len, "%s: EVT_TWR_UPLINK plaintext", l->name);
	}

	/* What the node does with it: open, then the CTRL bit says whether a tail follows. */
	struct twr_aes128 aes;

	twr_aes128_init(&aes, key);
	zassert_equal(twr_frame_open(&aes, rx.ack_frame, rx.ack_len, &h, pt, sizeof(pt), &pn),
		      TWR_OK, "%s", l->name);
	zassert_equal(pn, wp_len);
	zassert_mem_equal(pt, want_pt, pn, "%s: ACK plaintext", l->name);
	zassert_equal((pt[5] & TWR_ACK_FLAG_CTRL) != 0, pn > TWR_ACK_PAYLOAD_LEN, "%s", l->name);

	/* A TimeReq without an anchor is counted. */
	bool asks_time = false;
	bool asks_lc = false;

	(void)twr_ackctrl_requests(up_pt, up_len, &asks_lc, &asks_time);
	zassert_equal(g.st.ack_time_no_anchor - no_anchor_before, asks_time && !anchor ? 1u : 0u,
		      "%s: ack_time_no_anchor", l->name);
}

static void run_tail_line(const struct kline *l)
{
	static uint8_t ack_pt[64];
	static uint8_t want_frame[TWR_MAX_FRAME];
	static uint8_t frame[TWR_MAX_FRAME];
	size_t pn = khex(l, "ack_pt", ack_pt, sizeof(ack_pt));
	size_t wf = khex(l, "ack_frame", want_frame, sizeof(want_frame));
	size_t n = seal(TWR_T_ACK, 0, GW_ADDR, NODE_ADDR, (uint32_t)kint(l, "ack_ctr"), ack_pt,
			pn, frame);
	const char *ids = kget(l, "tlvs");
	size_t off = TWR_ACK_PAYLOAD_LEN;
	const uint8_t *body;
	uint8_t id;
	uint8_t bl;

	zassert_equal(n, wf, "%s", l->name);
	zassert_mem_equal(frame, want_frame, n, "%s: sealed", l->name);
	zassert_true(ack_pt[5] & TWR_ACK_FLAG_CTRL, "%s: CTRL set", l->name);
	/* The receiver's walk: every listed id in order, unknown ones skipped by len. */
	while (*ids != '\0') {
		char *end;
		unsigned long want = strtoul(ids, &end, 0);

		zassert_true(twr_ackctrl_next(ack_pt, pn, &off, &id, &body, &bl), "%s", l->name);
		zassert_equal(id, want, "%s: id", l->name);
		ids = *end == ',' ? end + 1 : end;
	}
	zassert_false(twr_ackctrl_next(ack_pt, pn, &off, &id, &body, &bl), "%s: end", l->name);
	zassert_equal(off, pn, "%s: whole tail read", l->name);
}

ZTEST(tower_ack_ctrl, test_kat)
{
	static struct kline l;
	int acks = 0;
	int tails = 0;

	for (size_t i = 0; i < ACK_CTRL_KAT_COUNT; i++) {
		kparse(&l, ack_ctrl_kat[i]);
		if (strcmp(l.kind, "ack") == 0) {
			run_ack_line(&l, false);
			run_ack_line(&l, true);
			acks++;
		} else {
			zassert_equal(strcmp(l.kind, "tail"), 0, "%s: kind %s", l.name, l.kind);
			run_tail_line(&l);
			tails++;
		}
	}
	zassert_true(acks >= 10 && tails >= 2, "KAT: %d ack, %d tail lines", acks, tails);
}

/* ---- builder ----------------------------------------------------------------------------- */

ZTEST(tower_ack_ctrl, test_margin_and_snr)
{
	/* SF7 floor -7.5 dB: SNR +5 dB -> 12 (12.5 rounded down). */
	zassert_equal(twr_ackctrl_margin(20, 7), 12);
	zassert_equal(twr_ackctrl_snr_db(20), 5);
	/* the central's own test: SF10, SNR -10.25 dB -> snr -11, margin 4 */
	zassert_equal(twr_ackctrl_snr_db(-41), -11);
	zassert_equal(twr_ackctrl_margin(-41, 10), 4);
	/* exactly at the floor, and a quarter dB under it, every SF */
	for (uint8_t sf = 7; sf <= 12; sf++) {
		int8_t at = (int8_t)(-30 - 10 * (sf - 7));

		zassert_equal(twr_ackctrl_margin(at, sf), 0, "SF%u at the floor", sf);
		zassert_equal(twr_ackctrl_margin(at - 1, sf), -1, "SF%u under", sf);
		zassert_equal(twr_ackctrl_margin(at + 3, sf), 0, "SF%u +0.75 dB", sf);
		zassert_equal(twr_ackctrl_margin(at + 4, sf), 1, "SF%u +1 dB", sf);
	}
	/* extremes stay in i8: SNR +31.75 dB at SF12 = 51 dB margin; -32 dB at SF7 = -25 */
	zassert_equal(twr_ackctrl_margin(127, 12), 51);
	zassert_equal(twr_ackctrl_margin(-128, 7), -25);
	zassert_equal(twr_ackctrl_snr_db(-128), -32);
	zassert_equal(twr_ackctrl_snr_db(-1), -1, "rounded down, not toward zero");
	zassert_equal(twr_ackctrl_snr_db(3), 0);
}

ZTEST(tower_ack_ctrl, test_wall_time)
{
	struct twr_time_anchor a = { .valid = true, .now_ms = 1000, .unix_ms = 1790000000000ull };
	uint32_t s;
	uint8_t f;

	zassert_true(twr_ackctrl_wall(&a, 1500, &s, &f));
	zassert_equal(s, 1790000000u);
	zassert_equal(f, 128);
	zassert_true(twr_ackctrl_wall(&a, 1999, &s, &f));
	zassert_equal(f, 255, ".999 s");
	zassert_true(twr_ackctrl_wall(&a, 1004, &s, &f));
	zassert_equal(f, 1, ".004 s = 1.02/256");
	zassert_true(twr_ackctrl_wall(&a, 1003, &s, &f));
	zassert_equal(f, 0, ".003 s = 0.77/256, rounded down");
	zassert_true(twr_ackctrl_wall(&a, 0, &s, &f), "before the anchor");
	zassert_equal(s, 1789999999u);
	zassert_equal(f, 0);
	zassert_true(twr_ackctrl_wall(&a, 999, &s, &f));
	zassert_equal(s, 1789999999u);
	zassert_equal(f, 255);

	a.valid = false;
	zassert_false(twr_ackctrl_wall(&a, 1500, &s, &f), "no anchor");
	zassert_false(twr_ackctrl_wall(NULL, 1500, &s, &f));
	a = (struct twr_time_anchor){ .valid = true, .now_ms = 5000, .unix_ms = 100 };
	zassert_false(twr_ackctrl_wall(&a, 1000, &s, &f), "before 1970");
	a = (struct twr_time_anchor){ .valid = true, .now_ms = 0, .unix_ms = 4294967296000ull };
	zassert_false(twr_ackctrl_wall(&a, 0, &s, &f), "past 2106");
	zassert_true(twr_ackctrl_wall(&a, 0, &s, &f) == false);
}

ZTEST(tower_ack_ctrl, test_requests)
{
	static const uint8_t both[] = { 0x91, 0x01, 2, 0xAA, 0xBB, 0x20, 0, 0x10, 1, 0x00 };
	static const uint8_t dup[] = { 0x91, 0x10, 0, 0x10, 0 };
	static const uint8_t cut[] = { 0x91, 0x10, 0, 0x20 };
	static const uint8_t cut_len[] = { 0x91, 0x10, 3, 0, 0 };
	static const uint8_t data[] = { 0x81, 0x10, 0, 0x20, 0 };
	static const uint8_t only_env[] = { 0x91 };
	struct twr_time_anchor a = { .valid = true, .now_ms = 0, .unix_ms = 1790000000000ull };
	struct twr_ackctrl_in in = {
		.rssi_dbm = -57, .snr_q = 20, .sf = 7, .t_rx_ms = 500, .counter = 9, .anchor = &a,
	};
	struct twr_ackctrl_res res;
	uint8_t tail[TWR_ACKCTRL_TAIL_MAX];
	bool lc;
	bool tm;

	zassert_true(twr_ackctrl_requests(both, sizeof(both), &lc, &tm));
	zassert_true(lc && tm, "a request with a value and others around it");
	zassert_equal(twr_ackctrl_build(both, sizeof(both), &in, tail, &res),
		      TWR_ACKCTRL_TAIL_MAX);
	zassert_equal(tail[0], 0x10, "LinkCheckAns first");
	zassert_equal(tail[6], 0x20);
	zassert_equal(sys_get_le32(&tail[13]), 9, "req_counter");
	zassert_true(res.lc && res.time && !res.time_no_anchor);

	zassert_equal(twr_ackctrl_build(dup, sizeof(dup), &in, tail, &res), 6, "answered once");
	zassert_false(twr_ackctrl_requests(cut, sizeof(cut), &lc, &tm));
	zassert_false(lc || tm);
	zassert_equal(twr_ackctrl_build(cut, sizeof(cut), &in, tail, &res), 0, "truncated");
	zassert_equal(twr_ackctrl_build(cut_len, sizeof(cut_len), &in, tail, &res), 0);
	zassert_equal(twr_ackctrl_build(data, sizeof(data), &in, tail, &res), 0, "0x81");
	zassert_equal(twr_ackctrl_build(only_env, 1, &in, tail, &res), 0, "empty list");
	zassert_equal(twr_ackctrl_build(data, 0, &in, tail, &res), 0, "empty payload");
	zassert_false(res.lc || res.time || res.time_no_anchor);

	a.valid = false;
	zassert_equal(twr_ackctrl_build(both, sizeof(both), &in, tail, &res), 6, "LC only");
	zassert_true(res.lc && !res.time && res.time_no_anchor);
}

/* ---- the core --------------------------------------------------------------------------- */

static size_t uplink(uint32_t ctr, bool confirmed, const uint8_t *pt, size_t len,
		     uint8_t *out)
{
	return seal(TWR_T_DATA, confirmed ? TWR_FLAG_CONFIRMED : 0, NODE_ADDR, GW_ADDR, ctr, pt,
		    len, out);
}

static size_t open_ack(uint8_t *pt)
{
	struct twr_aes128 aes;
	struct twr_hdr h;
	size_t pn;

	twr_aes128_init(&aes, key);
	zassert_equal(twr_frame_open(&aes, rx.ack_frame, rx.ack_len, &h, pt, TWR_MAX_PAYLOAD,
				     &pn), TWR_OK);
	return pn;
}

ZTEST(tower_ack_ctrl, test_rep_reack_rebuilds_the_tail)
{
	static const uint8_t req[] = { 0x91, 0x20, 0, 0x10, 0 };
	static uint8_t f[TWR_MAX_FRAME];
	static uint8_t pt[TWR_MAX_PAYLOAD];
	struct twr_rx_meta m = { .rssi_dbm = -80, .snr_q = 8, .sf = 7, .t_rx_ms = 3000 };
	size_t n = uplink(41, true, req, sizeof(req), f);
	uint32_t first_ctr;

	setup(500, 40);
	zassert_equal(twr_gwc_time_sync(&g, 2000, 1790000000000ull, 2000), P2P_STATUS_OK);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_equal(rx.action, TWR_RXA_UPLINK);
	zassert_true(rx.ack_ctrl);
	first_ctr = rx.ack_ctr;
	zassert_equal(open_ack(pt), 23);
	zassert_equal(sys_get_le32(&pt[14]), 1790000001u, "end of the first transmission");
	zassert_equal(pt[18], 0);
	zassert_equal(pt[9], 2, "snr 2 dB");
	twr_gwc_ack_result(&g, true, false, rx.ack_ctrl);

	/* The node missed the ACK and repeats the same frame 460 ms later, weaker. */
	m.t_rx_ms = 3460;
	m.rssi_dbm = -90;
	m.snr_q = -8;
	twr_gwc_on_rx(&g, f, n, &m, 3460, &rx);
	zassert_equal(rx.action, TWR_RXA_DUP);
	zassert_true(rx.ack_ctrl);
	zassert_equal(rx.ack_ctr, first_ctr + 1, "a fresh gateway counter");
	zassert_equal(open_ack(pt), 23);
	zassert_equal(sys_get_le32(&pt[0]), 41, "acks the same uplink");
	zassert_equal((int8_t)pt[4], -90);
	zassert_equal((int8_t)pt[8], (int8_t)pt[4], "LinkCheckAns rssi of the repetition");
	zassert_equal((int8_t)pt[9], -2, "its snr");
	zassert_equal(sys_get_le32(&pt[14]), 1790000001u);
	zassert_equal(pt[18], 117, "the repetition's end: .460 s");
	zassert_equal(sys_get_le32(&pt[19]), 41, "req_counter");
	twr_gwc_ack_result(&g, true, false, rx.ack_ctrl);
	zassert_equal(g.st.ack_ctrl_sent, 2);
	zassert_equal(g.st.ack_ctrl_lc, 2);
	zassert_equal(g.st.ack_ctrl_time, 2);
	zassert_equal(g.st.ack_time_no_anchor, 0);
}

/* EVT_TWR_UPLINK ack flags (b7 tail, b6 TimeAns in it) through the core and the TX result. */
ZTEST(tower_ack_ctrl, test_evt_ack_flags)
{
	static const uint8_t lc_time[] = { 0x91, 0x10, 0, 0x20, 0 };
	static const uint8_t time_only[] = { 0x91, 0x20, 0 };
	static const uint8_t lc_only[] = { 0x91, 0x10, 0 };
	static const uint8_t dl[] = { 0x81, 0x56, 0x01 };
	static uint8_t f[TWR_MAX_FRAME];
	static uint8_t ev[P2P_MAX_BODY_LEN];
	struct twr_rx_meta m = { .rssi_dbm = -80, .snr_q = 8, .sf = 7, .t_rx_ms = 3000 };
	size_t n;

	/* LC + Time without an anchor: a tail (LinkCheckAns only), no TimeAns. */
	setup(500, 40);
	n = uplink(41, true, lc_time, sizeof(lc_time), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_equal(rx.ack, P2P_TWR_ACK_SENT | P2P_TWR_ACK_CTRL, "0x%02x", rx.ack);
	zassert_equal(twr_gwc_ack_code(rx.ack, true, false), 0x81);
	zassert_equal(twr_gwc_ack_code(rx.ack, true, true), 0x83, "late keeps b7");
	zassert_equal(twr_gwc_ack_code(rx.ack, false, false), 0x03, "radio error: nothing went out");

	/* Time only without an anchor: no tail at all, no flag. */
	n = uplink(42, true, time_only, sizeof(time_only), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_equal(rx.ack, P2P_TWR_ACK_SENT);

	/* With an anchor and a queued downlink: PENDING, b7 and b6; late keeps both. */
	zassert_equal(twr_gwc_time_sync(&g, 0, 1790000000000ull, 0), P2P_STATUS_OK);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 7, 600, 0, dl, sizeof(dl), 0),
		      P2P_STATUS_OK);
	n = uplink(43, true, lc_time, sizeof(lc_time), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_equal(rx.ack, 0xC2, "0x%02x", rx.ack);
	zassert_equal(twr_gwc_ack_code(rx.ack, true, false), 0xC2);
	zassert_equal(twr_gwc_ack_code(rx.ack, true, true), 0xC3);
	zassert_equal(twr_gwc_ack_code(rx.ack, false, false), P2P_TWR_ACK_LATE);
	zassert_equal(twr_gwc_ack_code(rx.ack, false, true), P2P_TWR_ACK_LATE);
	zassert_equal(rx.ack & P2P_TWR_ACK_CODE_MASK, P2P_TWR_ACK_PENDING);

	/* The re-ACK of that repetition is flagged the same way. */
	twr_gwc_on_rx(&g, f, n, &m, 3100, &rx);
	zassert_equal(rx.action, TWR_RXA_DUP);
	zassert_equal(rx.ack, 0xC2, "0x%02x", rx.ack);

	/* LC only: b7, not b6. Time only with an anchor: both. */
	n = uplink(44, true, lc_only, sizeof(lc_only), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_equal(rx.ack & P2P_TWR_ACK_FLAGS, P2P_TWR_ACK_CTRL);
	n = uplink(45, true, time_only, sizeof(time_only), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_equal(rx.ack & P2P_TWR_ACK_FLAGS, P2P_TWR_ACK_FLAGS);

	/* No tail: codes unchanged by ack_code. */
	zassert_equal(twr_gwc_ack_code(P2P_TWR_ACK_SENT, true, false), P2P_TWR_ACK_SENT);
	zassert_equal(twr_gwc_ack_code(P2P_TWR_ACK_PENDING, true, true), P2P_TWR_ACK_LATE);

	/* The event carries the byte as is. */
	struct twr_uplink_evt e = {
		.t_ms = 3000, .rssi_dbm = -80, .snr_q_db = 8, .frame_flags = 1,
		.addr = NODE_ADDR, .counter = 43, .ack = 0x83, .len = sizeof(lc_time),
		.pt = lc_time,
	};

	zassert_true(twr_link_enc_uplink(&e, ev, sizeof(ev)) > 20);
	zassert_equal(ev[20], 0x83);
}

ZTEST(tower_ack_ctrl, test_no_request_no_tail)
{
	static const uint8_t data[] = { 0x81, 0x02, 0x01 };
	static const uint8_t hello[] = { 0x91, 0x02, 9, 1, 2, 3, 4, 5, 6, 7, 8, 9 };
	static uint8_t f[TWR_MAX_FRAME];
	static uint8_t pt[TWR_MAX_PAYLOAD];
	struct twr_rx_meta m = { .rssi_dbm = -80, .snr_q = 8, .sf = 7, .t_rx_ms = 3000 };
	size_t n;

	setup(500, 40);
	zassert_equal(twr_gwc_time_sync(&g, 0, 1790000000000ull, 0), P2P_STATUS_OK);
	n = uplink(41, true, data, sizeof(data), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_true(rx.send_ack);
	zassert_false(rx.ack_ctrl);
	zassert_equal(rx.ack_len, TWR_GWC_ACK_FRAME_LEN, "the 28 B ACK, unchanged");
	zassert_equal(open_ack(pt), TWR_ACK_PAYLOAD_LEN);
	zassert_equal(pt[5], 0, "flags: CTRL clear");

	n = uplink(42, true, hello, sizeof(hello), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_false(rx.ack_ctrl, "Hello is the central's");
	zassert_equal(rx.ack_len, TWR_GWC_ACK_FRAME_LEN);
	twr_gwc_ack_result(&g, true, false, rx.ack_ctrl);
	zassert_equal(g.st.ack_ctrl_sent, 0);
}

ZTEST(tower_ack_ctrl, test_unconfirmed_request_gets_nothing)
{
	static const uint8_t req[] = { 0x91, 0x10, 0, 0x20, 0 };
	static uint8_t f[TWR_MAX_FRAME];
	struct twr_rx_meta m = { .rssi_dbm = -80, .snr_q = 8, .sf = 7, .t_rx_ms = 3000 };
	size_t n = uplink(41, false, req, sizeof(req), f);

	setup(500, 40);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_equal(rx.action, TWR_RXA_UPLINK, "still forwarded");
	zassert_false(rx.send_ack);
	zassert_false(rx.ack_ctrl);
	zassert_equal(g.st.ack_time_no_anchor, 0, "no ACK, nothing counted");
	zassert_mem_equal(rx.pt, req, sizeof(req));
}

ZTEST(tower_ack_ctrl, test_no_anchor_counts_and_ack_result)
{
	static const uint8_t req[] = { 0x91, 0x20, 0 };
	static uint8_t f[TWR_MAX_FRAME];
	struct twr_rx_meta m = { .rssi_dbm = -80, .snr_q = 8, .sf = 7, .t_rx_ms = 3000 };
	size_t n = uplink(41, true, req, sizeof(req), f);

	setup(500, 40);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_true(rx.send_ack);
	zassert_false(rx.ack_ctrl, "TimeReq only, no anchor: no tail");
	zassert_equal(rx.ack_len, TWR_GWC_ACK_FRAME_LEN);
	zassert_equal(g.st.ack_time_no_anchor, 1);
	twr_gwc_ack_result(&g, true, false, rx.ack_ctrl);
	zassert_equal(g.st.ack_ctrl_sent, 0);

	/* An anchor from the future is refused and leaves none. */
	zassert_equal(twr_gwc_time_sync(&g, 3001, 1790000000000ull, 3000), P2P_STATUS_BAD_PARAM);
	zassert_false(g.anchor.valid);
	zassert_equal(twr_gwc_time_sync(&g, 3000, 1790000000000ull, 3000), P2P_STATUS_OK);
	n = uplink(42, true, req, sizeof(req), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_true(rx.ack_ctrl);
	zassert_equal(rx.ack_len, TWR_GWC_ACK_FRAME_LEN + 11);
	/* Not on air, or a radio error: not counted as sent. */
	twr_gwc_ack_result(&g, false, false, rx.ack_ctrl);
	zassert_equal(g.st.ack_ctrl_sent, 0);
	twr_gwc_ack_result(&g, true, true, rx.ack_ctrl);
	zassert_equal(g.st.ack_ctrl_sent, 1, "late but on air");
	zassert_equal(g.st.ack_time_no_anchor, 1);

	struct twr_info in;

	memset(&in, 0, sizeof(in));
	twr_gwc_info(&g, &in);
	zassert_equal(in.ack_ctrl_sent, 1);
	zassert_equal(in.ack_time_no_anchor, 1);
}

/* The longest ACK must fit the air MTU and its airtime is what the node's window grows by
 * (#470 §13.5: turnaround + ToA(28 + 17 B) + 60 ms). The ACK is sealed on RxDone, before the
 * t_rx + turnaround SetTx gate; its added cost is two AES blocks. */
ZTEST(tower_ack_ctrl, test_worst_case_length_and_time)
{
	static const uint8_t req[] = { 0x91, 0x10, 0, 0x20, 0 };
	static const uint8_t dl[] = { 0x81, 0x56, 0x01 };
	static uint8_t f[TWR_MAX_FRAME];
	struct twr_rx_meta m = { .rssi_dbm = -80, .snr_q = 8, .sf = 7, .t_rx_ms = 3000 };
	size_t n;

	BUILD_ASSERT(TWR_GWC_ACK_MAX_FRAME == 45, "28 B ACK + 17 B tail");
	BUILD_ASSERT(TWR_GWC_ACK_MAX_FRAME <= TWR_TOWER_MAX_FRAME, "fits the TOWER frame");
	BUILD_ASSERT(TWR_ACK_PAYLOAD_LEN + TWR_ACKCTRL_TAIL_MAX <= TWR_TOWER_MAX_PAYLOAD);

	setup(500, 40);
	zassert_equal(twr_gwc_time_sync(&g, 0, 1790000000000ull, 0), P2P_STATUS_OK);
	zassert_equal(twr_gwc_queue_push(&g, NODE_ADDR, 7, 600, 0, dl, sizeof(dl), 0),
		      P2P_STATUS_OK);
	n = uplink(41, true, req, sizeof(req), f);
	twr_gwc_on_rx(&g, f, n, &m, 3000, &rx);
	zassert_true(rx.ack_ctrl && rx.ack_pending);
	zassert_equal(rx.ack_len, TWR_GWC_ACK_MAX_FRAME, "worst case: both answers + PENDING");

	for (uint8_t sf = 7; sf <= 12; sf++) {
		struct twr_phy p = {
			.sf = sf, .bw_hz = 125000, .cr = 1, .preamble = 8, .explicit_hdr = true,
			.crc = true,
		};

		printk("ACK ToA SF%u BW125 CR4/5: %u B %u us, %u B %u us (+%u us)\n", sf,
		       (unsigned int)TWR_GWC_ACK_FRAME_LEN,
		       twr_lora_toa_us(&p, TWR_GWC_ACK_FRAME_LEN),
		       (unsigned int)TWR_GWC_ACK_MAX_FRAME,
		       twr_lora_toa_us(&p, TWR_GWC_ACK_MAX_FRAME),
		       twr_lora_toa_us(&p, TWR_GWC_ACK_MAX_FRAME) -
			       twr_lora_toa_us(&p, TWR_GWC_ACK_FRAME_LEN));
	}

	/* The extra work is CCM over 17 more plaintext bytes: 6 B = 1 CTR + 1 MAC data block,
	 * 23 B = 2 + 2 (plus B0, the AAD block and S0 either way): 5 -> 7 AES blocks. */
	zassert_equal(DIV_ROUND_UP(TWR_ACK_PAYLOAD_LEN, 16) * 2 + 3, 5);
	zassert_equal(DIV_ROUND_UP(TWR_ACK_PAYLOAD_LEN + TWR_ACKCTRL_TAIL_MAX, 16) * 2 + 3, 7);
}

ZTEST_SUITE(tower_ack_ctrl, NULL, NULL, NULL, NULL, NULL);
