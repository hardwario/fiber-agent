/* SPDX-License-Identifier: Apache-2.0
 *
 * Deframer fuzz (#470 C4), the firmware twin of the central's
 * deframer_fuzz_resyncs_and_never_yields_a_bad_frame: random, truncated and corrupted
 * streams with a fixed seed and a bounded number of rounds. hdlc_decoder_feed() never
 * delivers a body over 280 B, resyncs after garbage, and every valid frame between two pieces
 * of garbage comes out, in order.
 */
#include <zephyr/ztest.h>
#include <string.h>

#include "hdlc.h"
#include "proto.h"
#include "tower_link_vectors.h"
#include "vec.h"

#define FUZZ_SEED	   0x0470C4C47E7D0001ull
#define FUZZ_ROUNDS	   10000
#define FUZZ_NOISE_CHUNKS  2000
#define FUZZ_MAX_FRAMES	   4
#define FUZZ_STREAM_CAP	   16384
#define FUZZ_WIRE_CAP	   (2 + 2 * P2P_MAX_FRAME_LEN)

static uint64_t rng;

/* xorshift64*: the same generator as the central's fuzz. */
static uint64_t next(void)
{
	rng ^= rng >> 12;
	rng ^= rng << 25;
	rng ^= rng >> 27;
	return rng * 0x2545F4914F6CDD1Dull;
}

static size_t below(size_t n)
{
	return (size_t)(next() % n);
}

/* The message vectors' wires: fragments of real frames for the garbage. */
static uint8_t vec_wires[TOWER_LINK_VECTOR_COUNT][FUZZ_WIRE_CAP];
static size_t vec_wire_len[TOWER_LINK_VECTOR_COUNT];
static size_t vec_wire_count;
static struct vec fv;

static void load_wires(void)
{
	vec_wire_count = 0;
	for (size_t i = 0; i < TOWER_LINK_VECTOR_COUNT; i++) {
		vec_parse(&fv, tower_link_vectors[i]);
		if (strcmp(fv.msg, "STREAM") == 0 || strcmp(fv.msg, "REJECT") == 0) {
			continue;
		}
		vec_wire_len[vec_wire_count] =
			vec_hex(&fv, "wire", vec_wires[vec_wire_count], FUZZ_WIRE_CAP);
		vec_wire_count++;
	}
	zassert_true(vec_wire_count > 0, "no vector wires");
}

/* A random but valid frame: bodies biased towards the two special bytes. */
static void random_frame(struct frm *f)
{
	size_t sel = below(8);

	f->len = sel == 0 ? 0 : sel == 1 ? P2P_MAX_BODY_LEN : (uint16_t)below(P2P_MAX_BODY_LEN + 1);
	for (size_t i = 0; i < f->len; i++) {
		size_t k = below(4);

		f->body[i] = k == 0 ? HDLC_FLAG : k == 1 ? HDLC_ESC : (uint8_t)next();
	}
	f->type = (uint8_t)next();
	f->seq = (uint8_t)next();
}

static uint8_t stream[FUZZ_STREAM_CAP];
static size_t slen;

static void put(const uint8_t *b, size_t n)
{
	zassert_true(slen + n <= sizeof(stream), "fuzz stream overflow");
	memcpy(&stream[slen], b, n);
	slen += n;
}

static void put_frame(const struct frm *f)
{
	uint8_t w[FUZZ_WIRE_CAP];
	int n = hdlc_encode(f->type, f->seq, f->body, f->len, w, sizeof(w));

	zassert_true(n > 0, "hdlc_encode refused %u B", f->len);
	put(w, (size_t)n);
}

/* Garbage that may hold flags, escapes and fragments of real frames. */
static void put_garbage(void)
{
	static const uint8_t specials[] = {HDLC_FLAG, HDLC_ESC, 0x5E, 0x5D};
	static const char console[] = "[00:00:01.234] <err> radio: rearm -5\r\n";
	uint8_t b;

	switch (below(4)) {
	case 0:
		for (size_t n = below(64); n > 0; n--) {
			b = (uint8_t)next();
			put(&b, 1);
		}
		break;
	case 1: {
		/* A truncated real frame (no closing flag). */
		size_t i = below(vec_wire_count);
		size_t wl = vec_wire_len[i];

		put(vec_wires[i], wl - 1 - below(wl - 1));
		break;
	}
	case 2:
		for (size_t n = below(16); n > 0; n--) {
			put(&specials[below(4)], 1);
		}
		break;
	default:
		put((const uint8_t *)console, sizeof(console) - 1);
		break;
	}
}

static struct frm want[FUZZ_MAX_FRAMES];
static struct frm corrupt_next;
static struct sink fs;

ZTEST(tower_link_fuzz, test_deframer_fuzz_resyncs)
{
	struct hdlc_decoder d;
	size_t recovered = 0;

	load_wires();
	rng = FUZZ_SEED;
	for (int round = 0; round < FUZZ_ROUNDS; round++) {
		size_t nw = 1 + below(FUZZ_MAX_FRAMES);
		const uint8_t flag = HDLC_FLAG;

		slen = 0;
		put_garbage();
		for (size_t i = 0; i < nw; i++) {
			random_frame(&want[i]);
			put_frame(&want[i]);
			if (below(2) == 0) {
				put_garbage();
				/* Garbage often ends inside a frame: a flag closes it first. */
				put(&flag, 1);
			}
		}
		memset(&fs, 0, sizeof(fs));
		fs.want = want;
		fs.nwant = (int)nw;
		hdlc_decoder_init(&d);
		sink_feed(&d, &fs, stream, slen);
		/* Garbage may hold whole frames of its own (a vector wire, a lucky CRC): the
		 * wanted frames must come out in order, as a subsequence. */
		zassert_equal(fs.matched, (int)nw, "round %d: %d of %zu frames recovered", round,
			      fs.matched, nw);
		recovered += nw;

		/* Corruption: flip one bit, drop or insert one byte of a valid wire; a following
		 * valid frame always comes out, last. */
		struct frm *f = &want[0];
		uint8_t w[FUZZ_WIRE_CAP + 1];
		int n;
		size_t at;

		random_frame(f);
		n = hdlc_encode(f->type, f->seq, f->body, f->len, w, FUZZ_WIRE_CAP);
		zassert_true(n > 0, "encode");
		at = 1 + below((size_t)n - 2);
		switch (below(3)) {
		case 0:
			w[at] ^= (uint8_t)(1u << below(8));
			break;
		case 1:
			memmove(&w[at], &w[at + 1], (size_t)n - at - 1);
			n--;
			break;
		default:
			memmove(&w[at + 1], &w[at], (size_t)n - at);
			w[at] = (uint8_t)next();
			n++;
			break;
		}
		slen = 0;
		put(w, (size_t)n);
		random_frame(&corrupt_next);
		put_frame(&corrupt_next);
		memset(&fs, 0, sizeof(fs));
		hdlc_decoder_init(&d);
		sink_feed(&d, &fs, stream, slen);
		zassert_true(fs.n >= 1 && frm_eq(&fs.last, &corrupt_next),
			     "round %d: no resync after corruption", round);
	}
	zassert_true(recovered >= FUZZ_ROUNDS, "%zu frames recovered", recovered);

	/* Pure noise through one decoder: never more than 280 B out (sink_cb asserts it). */
	memset(&fs, 0, sizeof(fs));
	hdlc_decoder_init(&d);
	for (int c = 0; c < FUZZ_NOISE_CHUNKS; c++) {
		slen = 0;
		for (size_t n = below(600); n > 0; n--) {
			uint8_t b = (uint8_t)next();

			put(&b, 1);
		}
		sink_feed(&d, &fs, stream, slen);
	}
	TC_PRINT("fuzz: %d rounds, %zu frames recovered, %d noise chunks (%d frames out)\n",
		 FUZZ_ROUNDS, recovered, FUZZ_NOISE_CHUNKS, fs.n);
}

ZTEST_SUITE(tower_link_fuzz, NULL, NULL, NULL, NULL, NULL);
