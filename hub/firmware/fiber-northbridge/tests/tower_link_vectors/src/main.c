/* SPDX-License-Identifier: Apache-2.0
 *
 * Golden TOWER link vectors (#470 C4): plan/control/radio/p2p_tower_link_vectors.txt, the
 * same file the central's codec (control-radio, src/p2p/tower/link_vectors.rs) is checked
 * against, turned into tower_link_vectors.h at build time.
 *
 * Every vector: hdlc_encode() gives the wire byte for byte and the decoder gives back
 * exactly the frame. Commands (h2n) go through the twr_link.c decoders and must answer the
 * vector's verdict with its fields; replies and events (n2h) are produced by the encoders
 * the firmware sends them with. Streams and rejects check resync and the decoder's verdict.
 * The hb_* vectors were typed from the link doc's tables (§2-§5), not by any codec; the
 * GET_INFO ones are also checked offset by offset against the §3 table.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/byteorder.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "diag.h"
#include "hdlc.h"
#include "proto.h"
#include "radio_ctl.h"
#include "twr_gwc.h"
#include "twr_link.h"
#include "uart_link.h"

#include "tower_link_vectors.h"
#include "vec.h"

/* diag.c's EVT_LOG path; the vectors call diag_encode_log() directly. */
void uart_link_send_event(uint8_t type, const uint8_t *body, size_t body_len)
{
	ARG_UNUSED(type);
	ARG_UNUSED(body);
	ARG_UNUSED(body_len);
}

/* ---- vector parsing -------------------------------------------------------------------- */

/* The next `sep`-separated token of *p, in place; NULL at the end (strtok_r is not in C17). */
static char *tok_next(char **p, char sep)
{
	char *t;

	while (**p == sep) {
		(*p)++;
	}
	if (**p == '\0') {
		return NULL;
	}
	t = *p;
	while (**p != '\0' && **p != sep) {
		(*p)++;
	}
	if (**p == sep) {
		*(*p)++ = '\0';
	}
	return t;
}

void vec_parse(struct vec *v, const char *line)
{
	char *save;
	char *tok;
	int pos = 0;

	zassert_true(strlen(line) < sizeof(v->buf), "vector line too long");
	strcpy(v->buf, line);
	v->nf = 0;
	save = v->buf;
	while ((tok = tok_next(&save, ' ')) != NULL) {
		char *eq = strchr(tok, '=');

		if (pos < 3) {
			zassert_is_null(eq, "positional token with '=': %s", tok);
			if (pos == 0) {
				v->name = tok;
			} else if (pos == 1) {
				v->dir = tok;
			} else {
				v->msg = tok;
			}
			pos++;
			continue;
		}
		zassert_not_null(eq, "%s: token without '=': %s", v->name, tok);
		zassert_true(v->nf < VEC_MAX_FIELDS, "%s: too many fields", v->name);
		*eq = '\0';
		v->k[v->nf] = tok;
		v->v[v->nf] = eq + 1;
		v->nf++;
	}
	zassert_equal(pos, 3, "short vector line");
}

static bool vec_has(const struct vec *v, const char *key)
{
	for (int i = 0; i < v->nf; i++) {
		if (strcmp(v->k[i], key) == 0) {
			return true;
		}
	}
	return false;
}

/* GET_INFO of NB 0.3.1 (172 B, without ack_ctrl_sent / ack_time_no_anchor): a vector file
 * from before 0.3.2 still carries them, and the central keeps accepting them. */
#define GET_INFO_LEN_031 172
/* GET_INFO of NB 0.3.2 (180 B, without app_image_len / app_crc32 of 0.4.0). */
#define GET_INFO_LEN_032 180

/* The GET_INFO length a vector line was written for: its newest fields tell. Each older
 * layout is a prefix of this image's. */
static size_t get_info_vec_len(const struct vec *v)
{
	if (vec_has(v, "app_image_len")) {
		return P2P_GET_INFO_LEN;
	}
	return vec_has(v, "ack_ctrl_sent") ? GET_INFO_LEN_032 : GET_INFO_LEN_031;
}

const char *vec_get(const struct vec *v, const char *key)
{
	for (int i = 0; i < v->nf; i++) {
		if (strcmp(v->k[i], key) == 0) {
			return v->v[i];
		}
	}
	zassert_unreachable("%s: no field %s", v->name, key);
	return "";
}

long long vec_int(const struct vec *v, const char *key)
{
	const char *s = vec_get(v, key);
	char *end;
	long long r;

	if (s[0] == '-') {
		r = strtoll(s, &end, 0);
	} else {
		r = (long long)strtoull(s, &end, 0);
	}
	zassert_true(*s != '\0' && *end == '\0', "%s: %s=%s not an integer", v->name, key, s);
	return r;
}

static int nib(char c)
{
	if (c >= '0' && c <= '9') {
		return c - '0';
	}
	if (c >= 'a' && c <= 'f') {
		return c - 'a' + 10;
	}
	return -1;
}

size_t vec_hex_str(const char *s, uint8_t *out, size_t cap)
{
	size_t n = strlen(s);

	if (strcmp(s, "-") == 0) {
		return 0;
	}
	zassert_true(n % 2 == 0 && n / 2 <= cap, "hex length %zu", n);
	for (size_t i = 0; i < n / 2; i++) {
		int hi = nib(s[2 * i]);
		int lo = nib(s[2 * i + 1]);

		zassert_true(hi >= 0 && lo >= 0, "bad hex digit");
		out[i] = (uint8_t)(hi << 4 | lo);
	}
	return n / 2;
}

size_t vec_hex(const struct vec *v, const char *key, uint8_t *out, size_t cap)
{
	return vec_hex_str(vec_get(v, key), out, cap);
}

static bool is(const struct vec *v, const char *msg)
{
	return strcmp(v->msg, msg) == 0;
}

/* The frame type a message vector travels as. */
static uint8_t msg_type(const struct vec *v)
{
	static const struct {
		const char *msg;
		uint8_t type;
	} map[] = {
		{"GET_INFO", P2P_CMD_GET_INFO},
		{"SET_RADIO_CONFIG", P2P_CMD_SET_RADIO_CONFIG},
		{"RX_STOP", P2P_CMD_RX_STOP},
		{"TX_SCHEDULE", P2P_CMD_TX_SCHEDULE},
		{"TX_CANCEL", P2P_CMD_TX_CANCEL},
		{"TWR_START", P2P_CMD_TWR_START},
		{"TWR_NODE_ADD", P2P_CMD_TWR_NODE_ADD},
		{"TWR_NODE_REMOVE", P2P_CMD_TWR_NODE_REMOVE},
		{"TWR_CTR_BLOCK", P2P_CMD_TWR_CTR_BLOCK},
		{"TWR_QUEUE_PUSH", P2P_CMD_TWR_QUEUE_PUSH},
		{"TWR_QUEUE_DROP", P2P_CMD_TWR_QUEUE_DROP},
		{"TWR_NODE_LIST", P2P_CMD_TWR_NODE_LIST},
		{"TWR_EVT_ACK", P2P_CMD_TWR_EVT_ACK},
		{"TWR_TIME_SYNC", P2P_CMD_TWR_TIME_SYNC},
		{"RSP_GET_INFO", P2P_RSP(P2P_CMD_GET_INFO)},
		{"RSP_NODE_LIST", P2P_RSP(P2P_CMD_TWR_NODE_LIST)},
		{"EVT_BOOT", P2P_EVT_BOOT},
		{"EVT_RX", P2P_EVT_RX},
		{"EVT_TX_DONE", P2P_EVT_TX_DONE},
		{"EVT_LOG", P2P_EVT_LOG},
		{"EVT_TWR_UPLINK", P2P_EVT_TWR_UPLINK},
		{"EVT_TWR_TX", P2P_EVT_TWR_TX},
		{"EVT_TWR_CTR_LOW", P2P_EVT_TWR_CTR_LOW},
	};

	if (is(v, "RSP")) {
		return P2P_RSP((uint8_t)vec_int(v, "cmd"));
	}
	if (is(v, "FRAME")) {
		return (uint8_t)vec_int(v, "type");
	}
	for (size_t i = 0; i < ARRAY_SIZE(map); i++) {
		if (is(v, map[i].msg)) {
			return map[i].type;
		}
	}
	zassert_unreachable("%s: msg %s", v->name, v->msg);
	return 0;
}

void vec_frame(const struct vec *v, struct frm *f)
{
	f->type = msg_type(v);
	f->seq = (uint8_t)vec_int(v, "seq");
	f->len = (uint16_t)vec_hex(v, "body", f->body, sizeof(f->body));
}

static const char *vec_line_named(const char *name)
{
	size_t n = strlen(name);

	for (size_t i = 0; i < TOWER_LINK_VECTOR_COUNT; i++) {
		const char *l = tower_link_vectors[i];

		if (strncmp(l, name, n) == 0 && l[n] == ' ') {
			return l;
		}
	}
	zassert_unreachable("no vector %s", name);
	return NULL;
}

/* ---- deframing ------------------------------------------------------------------------- */

static void sink_cb(uint8_t type, uint8_t seq, const uint8_t *body, size_t len, void *user)
{
	struct sink *s = user;

	zassert_true(len <= P2P_MAX_BODY_LEN, "deframed body %zu", len);
	s->last.type = type;
	s->last.seq = seq;
	s->last.len = (uint16_t)len;
	memcpy(s->last.body, body, len);
	if (s->n < (int)ARRAY_SIZE(s->got)) {
		s->got[s->n] = s->last;
	}
	if (s->want != NULL && s->matched < s->nwant && frm_eq(&s->last, &s->want[s->matched])) {
		s->matched++;
	}
	s->n++;
}

void sink_feed(struct hdlc_decoder *d, struct sink *s, const uint8_t *w, size_t n)
{
	for (size_t i = 0; i < n; i++) {
		switch (hdlc_decoder_feed(d, w[i], sink_cb, s)) {
		case HDLC_FEED_CRC_ERROR:
			s->crc++;
			break;
		case HDLC_FEED_TOO_SHORT:
			s->shorts++;
			break;
		case HDLC_FEED_OVERFLOW:
			s->over++;
			break;
		default:
			break;
		}
	}
}

bool frm_eq(const struct frm *a, const struct frm *b)
{
	return a->type == b->type && a->seq == b->seq && a->len == b->len &&
	       memcmp(a->body, b->body, a->len) == 0;
}

/* ---- commands (h2n) -------------------------------------------------------------------- */

static void expect_status(const struct vec *v, enum p2p_status st)
{
	enum p2p_status want = strcmp(vec_get(v, "verdict"), "ok") == 0 ? P2P_STATUS_OK
									 : P2P_STATUS_BAD_PARAM;

	zassert_true(strcmp(vec_get(v, "verdict"), "ok") == 0 ||
			     strcmp(vec_get(v, "verdict"), "bad_param") == 0,
		     "%s: verdict", v->name);
	zassert_equal(st, want, "%s: the Northbridge answers %d, the vector says %s", v->name, st,
		      vec_get(v, "verdict"));
}

static void check_bytes(const struct vec *v, const char *key, const uint8_t *got, size_t n)
{
	static uint8_t want[300];
	size_t wn = vec_hex(v, key, want, sizeof(want));

	zassert_equal(n, wn, "%s: %s length %zu != %zu", v->name, key, n, wn);
	zassert_mem_equal(got, want, n, "%s: %s", v->name, key);
}

static void check_command(const struct vec *v, const uint8_t *b, size_t n)
{
	enum p2p_status st;

	if (is(v, "GET_INFO") || is(v, "RX_STOP")) {
		zassert_equal(n, 0, "%s: body", v->name);
		expect_status(v, P2P_STATUS_OK);
	} else if (is(v, "SET_RADIO_CONFIG")) {
		struct radio_config c;
		uint8_t again[P2P_RADIO_CONFIG_BODY_LEN];

		st = twr_link_dec_radio_config(b, n, &c);
		expect_status(v, st);
		if (st == P2P_STATUS_OK) {
			zassert_equal(c.frequency_hz, vec_int(v, "freq_hz"), "%s", v->name);
			zassert_equal(c.sf, vec_int(v, "sf"), "%s", v->name);
			zassert_equal(c.bw, vec_int(v, "bw"), "%s", v->name);
			zassert_equal(c.cr, vec_int(v, "cr"), "%s", v->name);
			zassert_equal(c.preamble_len, vec_int(v, "preamble"), "%s", v->name);
			zassert_equal(c.tx_power_dbm, vec_int(v, "tx_power_dbm"), "%s", v->name);
			zassert_equal(c.flags, vec_int(v, "flags"), "%s", v->name);
			zassert_equal(twr_link_enc_radio_config(&c, again), n, "%s", v->name);
			zassert_mem_equal(again, b, n, "%s: re-encoded", v->name);
		}
	} else if (is(v, "TX_SCHEDULE")) {
		/* Decoded inline in cmd_dispatch.c handle_tx_schedule(): the same checks. */
		zassert_true(n >= P2P_TX_SCHEDULE_HDR_LEN, "%s", v->name);
		zassert_equal(n, (size_t)P2P_TX_SCHEDULE_HDR_LEN + b[9], "%s: length", v->name);
		expect_status(v, P2P_STATUS_OK);
		zassert_equal(b[0], vec_int(v, "tx_id"), "%s", v->name);
		zassert_equal(sys_get_le64(&b[1]), (uint64_t)vec_int(v, "t_ms"), "%s", v->name);
		check_bytes(v, "frame", &b[P2P_TX_SCHEDULE_HDR_LEN], b[9]);
	} else if (is(v, "TX_CANCEL")) {
		/* cmd_dispatch.c handle_tx_cancel(). */
		zassert_equal(n, P2P_TX_CANCEL_BODY_LEN, "%s", v->name);
		expect_status(v, P2P_STATUS_OK);
		zassert_equal(b[0], vec_int(v, "tx_id"), "%s", v->name);
	} else if (is(v, "TWR_START")) {
		struct twr_start_cmd c;

		st = twr_link_dec_start(b, n, &c);
		expect_status(v, st);
		if (st == P2P_STATUS_OK) {
			zassert_equal(c.net_id, (uint32_t)vec_int(v, "net_id"), "%s", v->name);
			zassert_equal(c.turnaround_ms, vec_int(v, "turnaround_ms"), "%s", v->name);
			zassert_equal(c.dl_gap_ms, vec_int(v, "dl_gap_ms"), "%s", v->name);
			zassert_equal(c.flags, vec_int(v, "flags"), "%s", v->name);
		}
	} else if (is(v, "TWR_NODE_ADD")) {
		struct twr_node_add_cmd c;

		st = twr_link_dec_node_add(b, n, &c);
		expect_status(v, st);
		if (st == P2P_STATUS_OK) {
			zassert_equal(c.addr, (uint32_t)vec_int(v, "addr"), "%s", v->name);
			check_bytes(v, "key", c.key, sizeof(c.key));
			zassert_equal(c.last_seen, (uint32_t)vec_int(v, "last_seen"), "%s", v->name);
			zassert_equal(c.flags, vec_int(v, "flags"), "%s", v->name);
		}
	} else if (is(v, "TWR_NODE_REMOVE")) {
		uint32_t addr;

		st = twr_link_dec_node_remove(b, n, &addr);
		expect_status(v, st);
		if (st == P2P_STATUS_OK) {
			zassert_equal(addr, (uint32_t)vec_int(v, "addr"), "%s", v->name);
		}
	} else if (is(v, "TWR_CTR_BLOCK")) {
		struct twr_ctr_block_cmd c;

		st = twr_link_dec_ctr_block(b, n, &c);
		expect_status(v, st);
		if (st == P2P_STATUS_OK) {
			zassert_equal(c.first, (uint32_t)vec_int(v, "first"), "%s", v->name);
			zassert_equal(c.last, (uint32_t)vec_int(v, "last"), "%s", v->name);
		}
	} else if (is(v, "TWR_QUEUE_PUSH")) {
		struct twr_queue_push_cmd c;

		st = twr_link_dec_queue_push(b, n, &c);
		expect_status(v, st);
		if (st == P2P_STATUS_OK) {
			zassert_equal(c.addr, (uint32_t)vec_int(v, "addr"), "%s", v->name);
			zassert_equal(c.item, vec_int(v, "item"), "%s", v->name);
			zassert_equal(c.ttl_s, vec_int(v, "ttl_s"), "%s", v->name);
			zassert_equal(c.flags, vec_int(v, "flags"), "%s", v->name);
			check_bytes(v, "pt", c.pt, c.len);
		}
	} else if (is(v, "TWR_QUEUE_DROP")) {
		struct twr_queue_drop_cmd c;

		st = twr_link_dec_queue_drop(b, n, &c);
		expect_status(v, st);
		if (st == P2P_STATUS_OK) {
			zassert_equal(c.addr, (uint32_t)vec_int(v, "addr"), "%s", v->name);
			zassert_equal(c.item, vec_int(v, "item"), "%s", v->name);
		}
	} else if (is(v, "TWR_NODE_LIST")) {
		uint16_t start;

		st = twr_link_dec_node_list(b, n, &start);
		expect_status(v, st);
		zassert_equal(start, vec_int(v, "start"), "%s", v->name);
	} else if (is(v, "TWR_EVT_ACK")) {
		/* twr_cmd.c do_evt_ack(): one byte, the cumulative seq. */
		zassert_equal(n, P2P_TWR_EVT_ACK_BODY_LEN, "%s", v->name);
		expect_status(v, P2P_STATUS_OK);
		zassert_equal(b[0], vec_int(v, "ack_seq"), "%s", v->name);
	} else if (is(v, "TWR_TIME_SYNC")) {
		/* 0.3.2 (#470 §13.5): twr_cmd.c do_time_sync(). */
		uint64_t now_ms;
		uint64_t unix_ms;

		st = twr_link_dec_time_sync(b, n, &now_ms, &unix_ms);
		expect_status(v, st);
		if (st == P2P_STATUS_OK) {
			zassert_equal(now_ms, (uint64_t)vec_int(v, "now_ms"), "%s", v->name);
			zassert_equal(unix_ms, (uint64_t)vec_int(v, "unix_ms"), "%s", v->name);
		}
	} else {
		zassert_unreachable("%s: command %s", v->name, v->msg);
	}
}

/* ---- replies and events (n2h) ---------------------------------------------------------- */

static uint8_t status_code(const char *s)
{
	static const char *const names[] = {"ok",	"bad_param", "busy",	  "invalid_state",
					    "late",	"internal",  "no_space", "not_found",
					    "unsupported"};

	for (size_t i = 0; i < ARRAY_SIZE(names); i++) {
		if (strcmp(s, names[i]) == 0) {
			return (uint8_t)i;
		}
	}
	zassert_unreachable("status %s", s);
	return 0xFF;
}

static void expect_body(const struct vec *v, const uint8_t *got, size_t n, const uint8_t *body,
			size_t len)
{
	zassert_equal(n, len, "%s: encoded %zu B, vector %zu B", v->name, n, len);
	zassert_mem_equal(got, body, n, "%s: encoded body", v->name);
}

static struct twr_gwc gwc; /* large: not on the ztest stack */

static void check_reply(const struct vec *v, const uint8_t *b, size_t n)
{
	static uint8_t out[P2P_MAX_BODY_LEN + 1];
	static uint8_t tmp[P2P_MAX_BODY_LEN];
	size_t len;

	if (is(v, "RSP")) {
		zassert_equal(n, 1, "%s: status-only RSP", v->name);
		zassert_equal(b[0], status_code(vec_get(v, "status")), "%s", v->name);
	} else if (is(v, "RSP_GET_INFO")) {
		struct twr_info in = {0};
		const char *ctrs = vec_get(v, "counters");

		zassert_equal(b[0], P2P_STATUS_OK, "%s", v->name);
		vec_hex(v, "fw_ver", in.id.fw_ver, sizeof(in.id.fw_ver));
		vec_hex(v, "git_sha", in.id.git_sha, sizeof(in.id.git_sha));
		in.id.reset_cause = (uint8_t)vec_int(v, "reset_cause");
		in.id.build_flags = (uint8_t)vec_int(v, "build_flags");
		in.now_ms = (uint64_t)vec_int(v, "now_ms");
		in.state = (uint8_t)vec_int(v, "state");
		in.net_id = (uint32_t)vec_int(v, "net_id");
		in.turnaround_ms = (uint8_t)vec_int(v, "turnaround_ms");
		in.dl_gap_ms = (uint8_t)vec_int(v, "dl_gap_ms");
		in.modulation = (uint8_t)vec_int(v, "modulation");
		in.radio.frequency_hz = (uint32_t)vec_int(v, "freq_hz");
		in.radio.sf = (uint8_t)vec_int(v, "sf");
		in.radio.bw = (uint8_t)vec_int(v, "bw");
		in.radio.cr = (uint8_t)vec_int(v, "cr");
		in.radio.preamble_len = (uint16_t)vec_int(v, "preamble");
		in.radio.tx_power_dbm = (int8_t)vec_int(v, "tx_power_dbm");
		in.radio.flags = (uint8_t)vec_int(v, "radio_flags");
		in.err_flags = (uint32_t)vec_int(v, "err_flags");
		in.last_err_code = (uint8_t)vec_int(v, "last_err_code");
		in.last_err_age_s = (uint32_t)vec_int(v, "last_err_age_s");
		for (int i = 0; i < P2P_INFO_CTR_COUNT; i++) {
			char *end;

			in.ctr[i] = (uint32_t)strtoul(ctrs, &end, 0);
			zassert_true(*end == (i + 1 < P2P_INFO_CTR_COUNT ? ',' : '\0'),
				     "%s: counters", v->name);
			ctrs = end + 1;
		}
		in.ack_turnaround_last_us = (uint16_t)vec_int(v, "ack_turnaround_last_us");
		in.ack_turnaround_max_us = (uint16_t)vec_int(v, "ack_turnaround_max_us");
		in.airtime_ms_last_hour = (uint32_t)vec_int(v, "airtime_ms_last_hour");
		in.peers = (uint16_t)vec_int(v, "peers");
		in.peers_max = (uint16_t)vec_int(v, "peers_max");
		in.queue_used = (uint16_t)vec_int(v, "queue_used");
		in.queue_max = (uint16_t)vec_int(v, "queue_max");
		in.evt_ring_used = (uint8_t)vec_int(v, "evt_ring_used");
		in.evt_ring_max = (uint8_t)vec_int(v, "evt_ring_max");
		in.ctr_next = (uint32_t)vec_int(v, "ctr_next");
		in.ctr_last = (uint32_t)vec_int(v, "ctr_last");
		in.evt_head_seq = (uint8_t)vec_int(v, "evt_head_seq");
		if (vec_has(v, "ack_ctrl_sent")) {
			in.ack_ctrl_sent = (uint32_t)vec_int(v, "ack_ctrl_sent");
			in.ack_time_no_anchor = (uint32_t)vec_int(v, "ack_time_no_anchor");
		}
		if (vec_has(v, "app_image_len")) {
			in.app_image_len = (uint32_t)vec_int(v, "app_image_len");
			in.app_crc32 = (uint32_t)vec_int(v, "app_crc32");
		}
		zassert_equal(twr_link_enc_info(&in, tmp), P2P_GET_INFO_LEN);
		/* A 0.3.1 / 0.3.2 vector: this image's layout starts with it byte for byte. */
		len = get_info_vec_len(v);
		expect_body(v, tmp, len, b + 1, n - 1);
	} else if (is(v, "RSP_NODE_LIST")) {
		/* The peer rule of the vector file: peer i = addr 0x4e4c0001 + i, last_seen
		 * 0x101 * i, flags HOME, added in order. */
		uint16_t total = (uint16_t)vec_int(v, "total");
		uint8_t key[TWR_AES_KEY_LEN];

		zassert_equal(b[0], P2P_STATUS_OK, "%s", v->name);
		twr_gwc_init(&gwc);
		for (uint16_t i = 0; i < total; i++) {
			memset(key, 0x11 + i, sizeof(key));
			zassert_equal(twr_gwc_node_add(&gwc, 0x4E4C0001u + i, key, 0x101u * i,
						       P2P_TWR_NODE_HOME),
				      P2P_STATUS_OK, "%s: peer %u", v->name, i);
		}
		len = twr_gwc_node_list(&gwc, (uint16_t)vec_int(v, "start"), tmp);
		expect_body(v, tmp, len, b + 1, n - 1);
	} else if (is(v, "EVT_BOOT")) {
		struct twr_identity id = {0};

		vec_hex(v, "fw_ver", id.fw_ver, sizeof(id.fw_ver));
		vec_hex(v, "git_sha", id.git_sha, sizeof(id.git_sha));
		id.reset_cause = (uint8_t)vec_int(v, "reset_cause");
		len = twr_link_enc_boot(&id, tmp);
		expect_body(v, tmp, len, b, n);
	} else if (is(v, "EVT_RX")) {
		static uint8_t frame[P2P_MAX_RADIO_FRAME_LEN];
		size_t fl = vec_hex(v, "frame", frame, sizeof(frame));

		len = twr_link_enc_rx((uint64_t)vec_int(v, "t_ms"), (int16_t)vec_int(v, "rssi_dbm"),
				      (int8_t)vec_int(v, "snr_q_db"), frame, fl, out, sizeof(out));
		expect_body(v, out, len, b, n);
	} else if (is(v, "EVT_TX_DONE")) {
		len = twr_link_enc_tx_done((uint8_t)vec_int(v, "tx_id"),
					   (uint8_t)vec_int(v, "tx_status"),
					   (uint64_t)vec_int(v, "t_actual_ms"), tmp);
		expect_body(v, tmp, len, b, n);
	} else if (is(v, "EVT_LOG")) {
		static char text[P2P_MAX_BODY_LEN];
		size_t tl = vec_hex(v, "text", (uint8_t *)text, sizeof(text) - 1);

		text[tl] = '\0';
		len = diag_encode_log(out, sizeof(out), (uint8_t)vec_int(v, "level"),
				      (uint8_t)vec_int(v, "code"), tl ? text : NULL);
		expect_body(v, out, len, b, n);
	} else if (is(v, "EVT_TWR_UPLINK")) {
		static uint8_t pt[P2P_MAX_BODY_LEN];
		struct twr_uplink_evt e = {
			.t_ms = (uint64_t)vec_int(v, "t_ms"),
			.rssi_dbm = (int16_t)vec_int(v, "rssi_dbm"),
			.snr_q_db = (int8_t)vec_int(v, "snr_q_db"),
			.frame_flags = (uint8_t)vec_int(v, "frame_flags"),
			.addr = (uint32_t)vec_int(v, "addr"),
			.counter = (uint32_t)vec_int(v, "counter"),
			.ack = (uint8_t)vec_int(v, "ack"),
			.pt = pt,
		};

		e.len = (uint8_t)vec_hex(v, "pt", pt, sizeof(pt));
		len = twr_link_enc_uplink(&e, out, sizeof(out));
		expect_body(v, out, len, b, n);
	} else if (is(v, "EVT_TWR_TX")) {
		struct twr_tx_evt e = {
			.addr = (uint32_t)vec_int(v, "addr"),
			.item = (uint16_t)vec_int(v, "item"),
			.outcome = (uint8_t)vec_int(v, "outcome"),
			.gw_counter = (uint32_t)vec_int(v, "gw_counter"),
			.node_ack_counter = (uint32_t)vec_int(v, "node_ack_counter"),
			.ack_rssi = (int8_t)vec_int(v, "ack_rssi"),
		};

		len = twr_link_enc_tx(&e, tmp);
		expect_body(v, tmp, len, b, n);
	} else if (is(v, "EVT_TWR_CTR_LOW")) {
		len = twr_link_enc_ctr_low((uint32_t)vec_int(v, "next"),
					   (uint32_t)vec_int(v, "last"), tmp);
		expect_body(v, tmp, len, b, n);
	} else {
		zassert_unreachable("%s: reply %s", v->name, v->msg);
	}
}

/* ---- the suite ------------------------------------------------------------------------- */

static struct vec cur;
static struct vec other;
static struct frm frame_a;
static struct sink snk;
static uint8_t wire[4096];
static uint8_t enc[P2P_MAX_ENCODED_LEN];

static void check_stream(void)
{
	struct hdlc_decoder d;
	size_t wn = vec_hex(&cur, "wire", wire, sizeof(wire));
	static char expect[256];
	char *save = expect;
	int want = 0;

	memset(&snk, 0, sizeof(snk));
	hdlc_decoder_init(&d);
	sink_feed(&d, &snk, wire, wn);
	zassert_true(strlen(vec_get(&cur, "expect")) < sizeof(expect), "expect too long");
	strcpy(expect, vec_get(&cur, "expect"));
	if (strcmp(expect, "-") != 0) {
		for (char *nm = tok_next(&save, ','); nm != NULL; nm = tok_next(&save, ',')) {
			vec_parse(&other, vec_line_named(nm));
			vec_frame(&other, &frame_a);
			zassert_true(want < snk.n && want < (int)ARRAY_SIZE(snk.got), "%s: lost %s",
				     cur.name, nm);
			zassert_true(frm_eq(&snk.got[want], &frame_a), "%s: frame %d is not %s",
				     cur.name, want, nm);
			want++;
		}
	}
	zassert_equal(snk.n, want, "%s: %d frames, want %d", cur.name, snk.n, want);
}

static void check_reject(void)
{
	struct hdlc_decoder d;
	size_t wn = vec_hex(&cur, "wire", wire, sizeof(wire));
	const char *why = vec_get(&cur, "why");

	memset(&snk, 0, sizeof(snk));
	hdlc_decoder_init(&d);
	sink_feed(&d, &snk, wire, wn);
	zassert_equal(snk.n, 0, "%s: a frame came out", cur.name);
	zassert_equal(snk.crc, strcmp(why, "crc") == 0, "%s: crc", cur.name);
	zassert_equal(snk.shorts, strcmp(why, "short") == 0, "%s: short", cur.name);
	zassert_equal(snk.over, strcmp(why, "oversized") == 0, "%s: oversized", cur.name);
}

static void check_message(void)
{
	struct hdlc_decoder d;
	size_t wn = vec_hex(&cur, "wire", wire, sizeof(wire));
	int en;

	vec_frame(&cur, &frame_a);
	en = hdlc_encode(frame_a.type, frame_a.seq, frame_a.body, frame_a.len, enc, sizeof(enc));
	zassert_true(en > 0, "%s: hdlc_encode refused %u B into P2P_MAX_ENCODED_LEN = %u",
		     cur.name, frame_a.len, (unsigned)sizeof(enc));
	zassert_equal((size_t)en, wn, "%s: wire length %d != %zu", cur.name, en, wn);
	zassert_mem_equal(enc, wire, wn, "%s: wire", cur.name);

	/* Twice through one decoder: the closing flag of the first opens the second. */
	memset(&snk, 0, sizeof(snk));
	hdlc_decoder_init(&d);
	sink_feed(&d, &snk, wire, wn);
	sink_feed(&d, &snk, wire, wn);
	zassert_equal(snk.n, 2, "%s: deframed %d", cur.name, snk.n);
	zassert_true(frm_eq(&snk.got[0], &frame_a) && frm_eq(&snk.got[1], &frame_a),
		     "%s: deframed frame differs", cur.name);
	zassert_equal(snk.crc + snk.shorts + snk.over, 0, "%s: decoder errors", cur.name);

	if (strcmp(cur.dir, "h2n") == 0) {
		check_command(&cur, frame_a.body, frame_a.len);
	} else if (strcmp(cur.dir, "n2h") == 0) {
		check_reply(&cur, frame_a.body, frame_a.len);
	} else {
		zassert_equal(strcmp(cur.dir, "raw"), 0, "%s: dir %s", cur.name, cur.dir);
	}
}

ZTEST(tower_link_vectors, test_every_vector_byte_for_byte)
{
	static const char *const kinds[] = {
		"GET_INFO",	   "SET_RADIO_CONFIG", "RX_STOP",	 "TX_SCHEDULE",
		"TX_CANCEL",	   "TWR_START",	       "TWR_NODE_ADD",	 "TWR_NODE_REMOVE",
		"TWR_CTR_BLOCK",   "TWR_QUEUE_PUSH",   "TWR_QUEUE_DROP", "TWR_NODE_LIST",
		"TWR_EVT_ACK",	   "RSP",	       "RSP_GET_INFO",	 "RSP_NODE_LIST",
		"EVT_BOOT",	   "EVT_RX",	       "EVT_TX_DONE",	 "EVT_LOG",
		"EVT_TWR_UPLINK", "EVT_TWR_TX",	       "EVT_TWR_CTR_LOW",
	};
	bool seen[ARRAY_SIZE(kinds)] = {0};
	bool hand[ARRAY_SIZE(kinds)] = {0};
	int streams = 0, rejects = 0, frames = 0;

	for (size_t i = 0; i < TOWER_LINK_VECTOR_COUNT; i++) {
		vec_parse(&cur, tower_link_vectors[i]);
		if (is(&cur, "STREAM")) {
			check_stream();
			streams++;
			continue;
		}
		if (is(&cur, "REJECT")) {
			check_reject();
			rejects++;
			continue;
		}
		check_message();
		frames += is(&cur, "FRAME");
		for (size_t k = 0; k < ARRAY_SIZE(kinds); k++) {
			if (is(&cur, kinds[k])) {
				seen[k] = true;
				hand[k] |= strncmp(cur.name, "hb_", 3) == 0;
			}
		}
	}
	for (size_t k = 0; k < ARRAY_SIZE(kinds); k++) {
		zassert_true(seen[k], "no vector for %s", kinds[k]);
		zassert_true(hand[k], "no hand-built vector for %s", kinds[k]);
	}
	zassert_true(streams > 0 && rejects > 0 && frames > 0, "framing vectors missing");
	/* 117 in the C4 file; the central adds TWR_TIME_SYNC and 180 B GET_INFO lines (0.3.2),
	 * 188 B ones for 0.4.0. */
	zassert_true(TOWER_LINK_VECTOR_COUNT >= 117, "vector count %d", TOWER_LINK_VECTOR_COUNT);
}

/* The link doc's §3 table (hynek/p2p-tower-link-doc): offset after the status byte, field,
 * width, signed. Checked against the raw bytes of every RSP_GET_INFO vector. */
static const struct {
	uint8_t off;
	const char *name;
	uint8_t width;
	bool sign;
} get_info_offsets[] = {
	{0, "fw_ver", 3, false},
	{3, "git_sha", 4, false},
	{7, "reset_cause", 1, false},
	{8, "build_flags", 1, false},
	{9, "now_ms", 8, false},
	{17, "state", 1, false},
	{18, "net_id", 4, false},
	{22, "turnaround_ms", 1, false},
	{23, "dl_gap_ms", 1, false},
	{24, "modulation", 1, false},
	{25, "freq_hz", 4, false},
	{29, "sf", 1, false},
	{30, "bw", 1, false},
	{31, "cr", 1, false},
	{32, "preamble", 2, false},
	{34, "tx_power_dbm", 1, true},
	{35, "radio_flags", 1, false},
	{36, "err_flags", 4, false},
	{40, "last_err_code", 1, false},
	{41, "last_err_age_s", 4, false},
	{45, "counters", 100, false},
	{145, "ack_turnaround_last_us", 2, false},
	{147, "ack_turnaround_max_us", 2, false},
	{149, "airtime_ms_last_hour", 4, false},
	{153, "peers", 2, false},
	{155, "peers_max", 2, false},
	{157, "queue_used", 2, false},
	{159, "queue_max", 2, false},
	{161, "evt_ring_used", 1, false},
	{162, "evt_ring_max", 1, false},
	{163, "ctr_next", 4, false},
	{167, "ctr_last", 4, false},
	{171, "evt_head_seq", 1, false},
	{172, "ack_ctrl_sent", 4, false},
	{176, "ack_time_no_anchor", 4, false},
	{180, "app_image_len", 4, false},
	{184, "app_crc32", 4, false},
};

static long long le(const uint8_t *b, size_t n, bool sign)
{
	unsigned long long v = 0;

	for (size_t i = 0; i < n; i++) {
		v |= (unsigned long long)b[i] << (8 * i);
	}
	if (sign && n < 8 && (b[n - 1] & 0x80)) {
		return (long long)v - (1LL << (8 * n));
	}
	return (long long)v;
}

ZTEST(tower_link_vectors, test_get_info_offsets_field_by_field)
{
	size_t at = 0;
	int checked = 0;
	bool hand = false;

	for (size_t i = 0; i < ARRAY_SIZE(get_info_offsets); i++) {
		zassert_equal(get_info_offsets[i].off, at, "§3: %s not contiguous",
			      get_info_offsets[i].name);
		at += get_info_offsets[i].width;
	}
	zassert_equal(at, P2P_GET_INFO_LEN, "§3: %zu B", at);

	for (size_t i = 0; i < TOWER_LINK_VECTOR_COUNT; i++) {
		vec_parse(&cur, tower_link_vectors[i]);
		if (!is(&cur, "RSP_GET_INFO")) {
			continue;
		}
		vec_frame(&cur, &frame_a);
		size_t len = get_info_vec_len(&cur);

		zassert_equal(frame_a.len, 1 + len, "%s", cur.name);
		zassert_equal(frame_a.body[0], P2P_STATUS_OK, "%s", cur.name);
		const uint8_t *d = &frame_a.body[1];

		for (size_t f = 0; f < ARRAY_SIZE(get_info_offsets); f++) {
			const uint8_t *raw = &d[get_info_offsets[f].off];
			const char *nm = get_info_offsets[f].name;
			uint8_t want[8];

			if (get_info_offsets[f].off >= len) {
				break; /* a 0.3.1 / 0.3.2 vector ends earlier */
			}

			if (strcmp(nm, "fw_ver") == 0 || strcmp(nm, "git_sha") == 0) {
				size_t wn = vec_hex(&cur, nm, want, sizeof(want));

				zassert_equal(wn, get_info_offsets[f].width, "%s: %s", cur.name, nm);
				zassert_mem_equal(raw, want, wn, "%s: %s", cur.name, nm);
			} else if (strcmp(nm, "counters") == 0) {
				const char *s = vec_get(&cur, nm);

				for (int c = 0; c < P2P_INFO_CTR_COUNT; c++) {
					char *end;
					unsigned long w = strtoul(s, &end, 0);

					zassert_equal(le(raw + 4 * c, 4, false), (long long)w,
						      "%s: counter %d at %d", cur.name, c, 45 + 4 * c);
					s = end + 1;
				}
			} else {
				zassert_equal(le(raw, get_info_offsets[f].width,
						 get_info_offsets[f].sign),
					      vec_int(&cur, nm), "%s: %s at %u", cur.name, nm,
					      get_info_offsets[f].off);
			}
		}
		hand |= strcmp(cur.name, "hb_rsp_get_info") == 0;
		checked++;
	}
	zassert_true(checked >= 3 && hand, "GET_INFO vectors: %d", checked);
}

/* ---- 0.3.2 lines (#470 §13.5) ahead of the central's file ---------------------------------- */

static char synth[4096];

static void hex_append(char *p, const uint8_t *b, size_t n)
{
	static const char hx[] = "0123456789abcdef";

	p += strlen(p);
	for (size_t i = 0; i < n; i++) {
		*p++ = hx[b[i] >> 4];
		*p++ = hx[b[i] & 0xF];
	}
	*p = '\0';
}

/* `head` (name dir msg seq=... fields) + body= + wire= as the central would write them. */
static void run_synth(const char *head, uint8_t type, uint8_t seq, const uint8_t *body,
		      size_t n)
{
	int en = hdlc_encode(type, seq, body, n, enc, sizeof(enc));

	zassert_true(en > 0);
	snprintf(synth, sizeof(synth), "%s body=", head);
	hex_append(synth, body, n);
	strcat(synth, " wire=");
	hex_append(synth, enc, (size_t)en);
	vec_parse(&cur, synth);
	check_message();
}

/* The vector suite reads the lines the central adds for 0.3.2: TWR_TIME_SYNC and the 180 B
 * GET_INFO, and for 0.4.0 the 188 B GET_INFO, next to the 0.3.1 ones it keeps. */
ZTEST(tower_link_vectors, test_032_lines_are_understood)
{
	static uint8_t body[1 + P2P_GET_INFO_LEN];
	static char head[3072];
	const char *l = vec_line_named("hb_rsp_get_info");
	const char *cut = strstr(l, " body=");

	sys_put_le64(4822678189205111ull, &body[0]);
	sys_put_le64(1790000000123ull, &body[8]);
	run_synth("t_time_sync h2n TWR_TIME_SYNC seq=126 now_ms=4822678189205111 "
		  "unix_ms=1790000000123 verdict=ok",
		  P2P_CMD_TWR_TIME_SYNC, 126, body, P2P_TWR_TIME_SYNC_BODY_LEN);
	run_synth("t_time_sync_short h2n TWR_TIME_SYNC seq=1 now_ms=0 unix_ms=0 "
		  "verdict=bad_param",
		  P2P_CMD_TWR_TIME_SYNC, 1, body, P2P_TWR_TIME_SYNC_BODY_LEN - 1);

	/* hb_rsp_get_info with the two counters appended (0x7E escaped in the wire). */
	zassert_not_null(cut);
	vec_parse(&cur, l);
	vec_frame(&cur, &frame_a);
	zassert_equal(frame_a.len, 1 + 172);
	memcpy(body, frame_a.body, frame_a.len);
	sys_put_le32(0x7E000102u, &body[173]);
	sys_put_le32(0x0000007Du, &body[177]);
	snprintf(head, sizeof(head), "%.*s ack_ctrl_sent=0x7e000102 ack_time_no_anchor=125",
		 (int)(cut - l), l);
	run_synth(head, P2P_RSP(P2P_CMD_GET_INFO), 33, body, 1 + GET_INFO_LEN_032);

	/* ... and with the 0.4.0 app_valid record tail as well. */
	sys_put_le32(102200u, &body[181]);
	sys_put_le32(0x6B754AACu, &body[185]);
	snprintf(head, sizeof(head), "%.*s ack_ctrl_sent=0x7e000102 ack_time_no_anchor=125 "
		 "app_image_len=102200 app_crc32=0x6b754aac", (int)(cut - l), l);
	run_synth(head, P2P_RSP(P2P_CMD_GET_INFO), 33, body, 1 + P2P_GET_INFO_LEN);
	zassert_equal(sizeof(body), 1 + P2P_GET_INFO_LEN);

	/* EVT_TWR_UPLINK ack with the 0.3.2 flags (b7 tail, b6 TimeAns): the 0.3.1 lines with
	 * the ack byte (body offset 20) changed, in hex and in decimal. */
	static const struct {
		const char *line;
		const char *was;
		const char *now;
		uint8_t ack;
	} flagged[] = {
		{ "evt_twr_uplink_confirmed_pending", "ack=2 ", "ack=0xc2 ", 0xC2 },
		{ "evt_twr_uplink_confirmed_pending", "ack=2 ", "ack=130 ", 0x82 },
		{ "evt_twr_uplink_late", "ack=3 ", "ack=0xc3 ", 0xC3 },
		{ "evt_twr_uplink_late", "ack=3 ", "ack=0x83 ", 0x83 },
	};

	for (size_t i = 0; i < ARRAY_SIZE(flagged); i++) {
		const char *src = vec_line_named(flagged[i].line);
		const char *at = strstr(src, flagged[i].was);
		const char *bc = strstr(src, " body=");

		zassert_not_null(at);
		zassert_not_null(bc);
		vec_parse(&cur, src);
		vec_frame(&cur, &frame_a);
		zassert_true(frame_a.len > 20);
		memcpy(body, frame_a.body, frame_a.len);
		body[20] = flagged[i].ack;
		snprintf(head, sizeof(head), "%.*s%s%.*s", (int)(at - src), src, flagged[i].now,
			 (int)(bc - at - strlen(flagged[i].was)), at + strlen(flagged[i].was));
		run_synth(head, P2P_EVT_TWR_UPLINK, frame_a.seq, body, frame_a.len);
	}
}

ZTEST_SUITE(tower_link_vectors, NULL, NULL, NULL, NULL, NULL);
