/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for the TOWER P0 bench gateway codec: twr_crypto.c (AES-128, CCM),
 * twr_frame.c (header, nonce, seal/open, ACK payload, replay window), twr_gw.c (RX
 * classification, auto-ACK, queued downlink) and twr_airtime.c (ToA, duty ledger).
 *
 * Reference data:
 *   - FIPS-197 Appendix B and C.1 AES-128 blocks;
 *   - ccm_py_vectors.h, Python `cryptography` AESCCM (gen_vectors.py, committed);
 *   - tower_kat.h, generated at build time by kat_to_header.py from every
 *     ccm/tower_*_kat.json (the planner's KAT from the upstream Rust crates). Every vector
 *     must match byte for byte.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/util.h>
#include <string.h>

#include "twr_airtime.h"
#include "twr_crypto.h"
#include "twr_frame.h"
#include "twr_gw.h"

#include "ccm_py_vectors.h"
#include "tower_kat.h"

#define GW_ADDR   0x4E420001u
#define NODE_ADDR 0x00000413u

static const uint8_t key_00_0f[16] = {
	0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
	0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
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

/* ---- AES-128 ------------------------------------------------------------------------ */

ZTEST(tower_codec, test_aes_fips197)
{
	/* FIPS-197 Appendix C.1 */
	static const uint8_t pt_c1[16] = {
		0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
		0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
	};
	static const uint8_t ct_c1[16] = {
		0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30,
		0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a,
	};
	/* FIPS-197 Appendix B */
	static const uint8_t key_b[16] = {
		0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6,
		0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c,
	};
	static const uint8_t pt_b[16] = {
		0x32, 0x43, 0xf6, 0xa8, 0x88, 0x5a, 0x30, 0x8d,
		0x31, 0x31, 0x98, 0xa2, 0xe0, 0x37, 0x07, 0x34,
	};
	static const uint8_t ct_b[16] = {
		0x39, 0x25, 0x84, 0x1d, 0x02, 0xdc, 0x09, 0xfb,
		0xdc, 0x11, 0x85, 0x97, 0x19, 0x6a, 0x0b, 0x32,
	};
	struct twr_aes128 aes;
	uint8_t out[16];

	twr_aes128_init(&aes, key_00_0f);
	twr_aes128_encrypt(&aes, pt_c1, out);
	zassert_mem_equal(out, ct_c1, 16);

	twr_aes128_init(&aes, key_b);
	twr_aes128_encrypt(&aes, pt_b, out);
	zassert_mem_equal(out, ct_b, 16);
	/* Last round key of Appendix A.1's expansion: d0 14 f9 a8 c9 ee 25 89 ... */
	zassert_equal(aes.rk[160], 0xd0);
	zassert_equal(aes.rk[175], 0xa6);

	/* in place */
	memcpy(out, pt_b, 16);
	twr_aes128_encrypt(&aes, out, out);
	zassert_mem_equal(out, ct_b, 16);
}

/* ---- CCM against Python cryptography AESCCM ----------------------------------------- */

ZTEST(tower_codec, test_ccm_python_vectors)
{
	static uint8_t buf[256];
	uint8_t tag[8];

	for (size_t i = 0; i < ARRAY_SIZE(ccm_py_vectors); i++) {
		const struct ccm_py_vec *v = &ccm_py_vectors[i];
		struct twr_aes128 aes;

		twr_aes128_init(&aes, v->key);
		memcpy(buf, v->pt, v->pt_len);
		zassert_true(twr_ccm_seal(&aes, v->nonce, v->aad, v->aad_len, buf, v->pt_len, tag),
			     "vec %zu seal", i);
		zassert_mem_equal(buf, v->ct, v->pt_len, "vec %zu ciphertext", i);
		zassert_mem_equal(tag, v->tag, 8, "vec %zu tag", i);

		zassert_true(twr_ccm_open(&aes, v->nonce, v->aad, v->aad_len, buf, v->pt_len,
					  v->tag), "vec %zu open", i);
		zassert_mem_equal(buf, v->pt, v->pt_len, "vec %zu plaintext", i);

		/* a flipped tag bit fails and wipes the buffer */
		memcpy(buf, v->ct, v->pt_len);
		memcpy(tag, v->tag, 8);
		tag[i % 8] ^= 0x80;
		zassert_false(twr_ccm_open(&aes, v->nonce, v->aad, v->aad_len, buf, v->pt_len,
					   tag), "vec %zu tampered tag", i);
		for (size_t j = 0; j < v->pt_len; j++) {
			zassert_equal(buf[j], 0, "vec %zu not wiped", i);
		}
	}
}

/* ---- TOWER KAT (upstream Rust crates) ---------------------------------------------- */

ZTEST(tower_codec, test_tower_kat_all_vectors)
{
	static uint8_t frame[TWR_MAX_FRAME];
	static uint8_t pt[TWR_MAX_PAYLOAD];

	zassert_true(ARRAY_SIZE(tower_kats) >= 9, "expected the 9 planner vectors");
	for (size_t i = 0; i < ARRAY_SIZE(tower_kats); i++) {
		const struct tower_kat *v = &tower_kats[i];
		struct twr_aes128 aes;
		struct twr_hdr h = {
			.type = v->frame_type,
			.flags = v->flags,
			.src = v->src,
			.dest = v->dest,
			.counter = v->counter,
		};
		struct twr_hdr got;
		uint8_t hdr[TWR_HDR_LEN_BULK];
		uint8_t nonce[TWR_CCM_NONCE_LEN];
		size_t n, pt_len;

		twr_aes128_init(&aes, v->key);

		n = twr_hdr_encode(&h, hdr);
		zassert_equal(n, v->header_len, "%s header len", v->name);
		zassert_mem_equal(hdr, v->header, n, "%s header", v->name);

		twr_nonce(v->src, v->counter, 0, nonce);
		zassert_equal(v->nonce_len, sizeof(nonce));
		zassert_mem_equal(nonce, v->nonce, sizeof(nonce), "%s nonce", v->name);

		n = twr_frame_seal(&aes, &h, v->plaintext, v->plaintext_len, TWR_MAX_PAYLOAD,
				   frame, sizeof(frame));
		zassert_equal(n, v->frame_len, "%s frame len %zu", v->name, n);
		zassert_equal(n, v->frame_len_field, "%s frame_len field", v->name);
		zassert_mem_equal(frame, v->frame, n, "%s frame", v->name);
		zassert_mem_equal(&frame[v->header_len], v->ciphertext, v->ciphertext_len,
				  "%s ciphertext", v->name);
		zassert_mem_equal(&frame[n - TWR_TAG_LEN], v->tag, TWR_TAG_LEN, "%s tag",
				  v->name);
		/* the strict TOWER cap agrees for every planner vector */
		zassert_equal(twr_frame_seal(&aes, &h, v->plaintext, v->plaintext_len,
					     TWR_TOWER_MAX_PAYLOAD, frame, sizeof(frame)), n);

		zassert_equal(twr_frame_open(&aes, v->frame, v->frame_len, &got, pt, sizeof(pt),
					     &pt_len), TWR_OK, "%s open", v->name);
		zassert_equal(got.type, v->frame_type);
		zassert_equal(got.flags, v->flags);
		zassert_equal(got.src, v->src);
		zassert_equal(got.dest, v->dest);
		zassert_equal(got.counter, v->counter);
		zassert_equal(pt_len, v->plaintext_len, "%s pt len", v->name);
		zassert_mem_equal(pt, v->plaintext, pt_len, "%s plaintext", v->name);
	}
}

/* ---- Frame codec -------------------------------------------------------------------- */

ZTEST(tower_codec, test_nonce_layout)
{
	static const uint8_t want[13] = {
		0x01, 0x02, 0x03, 0x04, /* src LE */
		0x05, 0x06, 0x07, 0x08, /* counter LE */
		0x09, 0x0a, 0x0b,       /* bulk index LE */
		0x00, 0x00,
	};
	uint8_t n[13];

	twr_nonce(0x04030201u, 0x08070605u, 0x000b0a09u, n);
	zassert_mem_equal(n, want, sizeof(want));
	/* bits above the 24-bit bulk index never reach the nonce */
	twr_nonce(0x04030201u, 0x08070605u, 0xff0b0a09u, n);
	zassert_mem_equal(n, want, sizeof(want));
}

ZTEST(tower_codec, test_frame_round_trip)
{
	static uint8_t pt[TWR_MAX_PAYLOAD], got_pt[TWR_MAX_PAYLOAD], frame[TWR_MAX_FRAME];
	static const size_t lens[] = { 0, 1, 15, 16, 17, 74, 75, 200, TWR_MAX_PAYLOAD };
	struct twr_aes128 aes;

	twr_aes128_init(&aes, key_00_0f);
	for (size_t i = 0; i < sizeof(pt); i++) {
		pt[i] = (uint8_t)(i * 7 + 3);
	}
	for (size_t i = 0; i < ARRAY_SIZE(lens); i++) {
		struct twr_hdr h = {
			.type = TWR_T_DATA, .flags = TWR_FLAG_CONFIRMED,
			.src = NODE_ADDR, .dest = GW_ADDR, .counter = 1000 + i,
		};
		struct twr_hdr got;
		size_t n, got_len;

		n = twr_frame_seal(&aes, &h, pt, lens[i], TWR_MAX_PAYLOAD, frame, sizeof(frame));
		zassert_equal(n, TWR_HDR_LEN + lens[i] + TWR_TAG_LEN, "len %zu", lens[i]);
		zassert_equal(frame[0], 0x20);
		zassert_equal(twr_frame_open(&aes, frame, n, &got, got_pt, sizeof(got_pt),
					     &got_len), TWR_OK, "len %zu", lens[i]);
		zassert_equal(got_len, lens[i]);
		zassert_mem_equal(got_pt, pt, lens[i]);
		zassert_equal(got.counter, 1000 + i);
	}

	/* caps: 75 B over the TOWER cap, 234 B over the PHY */
	{
		struct twr_hdr h = { .type = TWR_T_DATA, .src = 1, .dest = 2, .counter = 1 };

		zassert_equal(twr_frame_seal(&aes, &h, pt, 75, TWR_TOWER_MAX_PAYLOAD, frame,
					     sizeof(frame)), 0);
		zassert_equal(twr_frame_seal(&aes, &h, pt, TWR_MAX_PAYLOAD + 1,
					     TWR_MAX_PAYLOAD + 1, frame, sizeof(frame)), 0);
		zassert_equal(twr_frame_seal(&aes, &h, pt, 10, TWR_MAX_PAYLOAD, frame, 31), 0);
	}

	/* bulk types carry a 17 B header whose index is in the AAD and the nonce */
	{
		struct twr_hdr h = {
			.type = TWR_T_BULK_DATA, .flags = TWR_FLAG_LAST_CHUNK,
			.src = NODE_ADDR, .dest = GW_ADDR, .counter = 7, .bulk_idx = 0x0a0b0c,
		};
		struct twr_hdr got;
		size_t n, got_len;

		n = twr_frame_seal(&aes, &h, pt, 20, TWR_MAX_PAYLOAD, frame, sizeof(frame));
		zassert_equal(n, TWR_HDR_LEN_BULK + 20 + TWR_TAG_LEN);
		zassert_equal(frame[0], 0x23);
		zassert_equal(frame[14], 0x0c);
		zassert_equal(frame[16], 0x0a);
		zassert_equal(twr_frame_open(&aes, frame, n, &got, got_pt, sizeof(got_pt),
					     &got_len), TWR_OK);
		zassert_equal(got.bulk_idx, 0x0a0b0c);
		frame[15] ^= 1;
		zassert_equal(twr_frame_open(&aes, frame, n, &got, got_pt, sizeof(got_pt),
					     &got_len), TWR_E_MIC);
	}
}

ZTEST(tower_codec, test_aad_and_ciphertext_tamper)
{
	const struct tower_kat *v = kat("uplink_confirmed");
	static uint8_t frame[TWR_MAX_FRAME], pt[TWR_MAX_PAYLOAD];
	struct twr_aes128 aes;
	struct twr_hdr h;
	size_t pt_len;

	twr_aes128_init(&aes, v->key);
	/* every header byte after ver_type (flags, src, dest, counter) is AAD */
	for (size_t i = 1; i < v->frame_len; i++) {
		memcpy(frame, v->frame, v->frame_len);
		frame[i] ^= 0x01;
		zassert_equal(twr_frame_open(&aes, frame, v->frame_len, &h, pt, sizeof(pt),
					     &pt_len), TWR_E_MIC, "byte %zu", i);
		zassert_equal(pt_len, 0);
	}
	/* ver_type: another valid type under the same version is still an AAD change */
	memcpy(frame, v->frame, v->frame_len);
	frame[0] = 0x21;
	zassert_equal(twr_frame_open(&aes, frame, v->frame_len, &h, pt, sizeof(pt), &pt_len),
		      TWR_E_MIC);
	/* a truncated frame fails too */
	zassert_equal(twr_frame_open(&aes, v->frame, v->frame_len - 1, &h, pt, sizeof(pt),
				     &pt_len), TWR_E_MIC);
}

ZTEST(tower_codec, test_header_parse_errors)
{
	uint8_t buf[32] = { 0 };
	struct twr_hdr h;
	size_t hl;

	buf[0] = 0x20;
	zassert_equal(twr_hdr_parse(buf, TWR_HDR_LEN - 1, &h, &hl), TWR_E_SHORT);
	zassert_equal(twr_hdr_parse(buf, TWR_HDR_LEN, &h, &hl), TWR_OK);
	zassert_equal(hl, TWR_HDR_LEN);

	buf[0] = 0x40; /* version 2 */
	zassert_equal(twr_hdr_parse(buf, sizeof(buf), &h, &hl), TWR_E_VERSION);
	buf[0] = 0x00; /* version 0 */
	zassert_equal(twr_hdr_parse(buf, sizeof(buf), &h, &hl), TWR_E_VERSION);
	buf[0] = 0x28; /* v1, type 8 */
	zassert_equal(twr_hdr_parse(buf, sizeof(buf), &h, &hl), TWR_E_TYPE);
	buf[0] = 0x27; /* v1, Beacon */
	zassert_equal(twr_hdr_parse(buf, sizeof(buf), &h, &hl), TWR_OK);

	buf[0] = 0x22; /* BulkReq needs 17 B */
	zassert_equal(twr_hdr_parse(buf, 16, &h, &hl), TWR_E_SHORT);
	zassert_equal(twr_hdr_parse(buf, 17, &h, &hl), TWR_OK);
	zassert_equal(hl, TWR_HDR_LEN_BULK);

	/* header present but no room for the tag */
	buf[0] = 0x20;
	{
		struct twr_aes128 aes;
		uint8_t pt[4];
		size_t pl;

		twr_aes128_init(&aes, key_00_0f);
		zassert_equal(twr_frame_open(&aes, buf, TWR_HDR_LEN + 7, &h, pt, sizeof(pt), &pl),
			      TWR_E_SHORT);
		zassert_equal(twr_frame_open(&aes, buf, TWR_HDR_LEN + 8 + 5, &h, pt, sizeof(pt),
					     &pl), TWR_E_TOO_LONG);
	}
}

/* ---- ACK payload -------------------------------------------------------------------- */

ZTEST(tower_codec, test_ack_payload)
{
	uint8_t p[TWR_ACK_PAYLOAD_LEN];
	struct twr_ack_meta m;

	twr_ack_payload_encode(0x04030201u, -57, false, p);
	zassert_mem_equal(p, ((uint8_t[]){ 0x01, 0x02, 0x03, 0x04, 0xc7, 0x00 }), 6);
	twr_ack_payload_encode(2, -57, true, p);
	zassert_mem_equal(p, kat("ack_pending")->plaintext, 6);
	twr_ack_payload_encode(2, -57, false, p);
	zassert_mem_equal(p, kat("ack_no_pending")->plaintext, 6);

	/* rssi clamps into i8 */
	twr_ack_payload_encode(1, -200, false, p);
	zassert_equal((int8_t)p[4], -128);
	twr_ack_payload_encode(1, 300, false, p);
	zassert_equal((int8_t)p[4], 127);

	zassert_true(twr_ack_payload_parse(kat("ack_pending")->plaintext, 6, &m));
	zassert_equal(m.acked_counter, 2);
	zassert_true(m.has_rssi);
	zassert_equal(m.rssi, -57);
	zassert_true(m.pending);

	/* 4 B is enough (rssi and flags optional), 3 B is not */
	zassert_true(twr_ack_payload_parse(p, 4, &m));
	zassert_false(m.has_rssi);
	zassert_false(m.pending);
	zassert_false(twr_ack_payload_parse(p, 3, &m));
}

/* ---- Replay window ------------------------------------------------------------------ */

ZTEST(tower_codec, test_replay_window)
{
	struct twr_replay_tbl t;

	twr_replay_init(&t);
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, 0), TWR_RP_REPLAY, "counter 0");
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, 5), TWR_RP_FRESH);
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, 5), TWR_RP_RETRANSMIT);
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, 4), TWR_RP_REPLAY);
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, 1), TWR_RP_REPLAY);
	/* node reboot: counter jumps +256, still strictly greater */
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, 5 + 256), TWR_RP_FRESH);
	zassert_equal(twr_replay_check(&t, NODE_ADDR, 5 + 256), TWR_RP_RETRANSMIT);
	/* check does not record */
	zassert_equal(twr_replay_check(&t, NODE_ADDR, 1000), TWR_RP_FRESH);
	zassert_equal(twr_replay_check(&t, NODE_ADDR, 999), TWR_RP_FRESH);
	/* lanes are per source */
	zassert_equal(twr_replay_accept(&t, 0x1111, 3), TWR_RP_FRESH);
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, 262), TWR_RP_FRESH);
	zassert_equal(twr_replay_accept(&t, 0x1111, 3), TWR_RP_RETRANSMIT);
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, UINT32_MAX), TWR_RP_FRESH);
	zassert_equal(twr_replay_accept(&t, NODE_ADDR, UINT32_MAX), TWR_RP_RETRANSMIT);

	/* a 9th source evicts round-robin; the evicted source restarts from 0 */
	twr_replay_init(&t);
	for (uint32_t s = 1; s <= TWR_REPLAY_LANES; s++) {
		zassert_equal(twr_replay_accept(&t, s, 10), TWR_RP_FRESH);
	}
	zassert_equal(twr_replay_accept(&t, 100, 10), TWR_RP_FRESH);
	zassert_equal(twr_replay_check(&t, 1, 5), TWR_RP_FRESH, "lane 1 evicted");
	zassert_equal(twr_replay_check(&t, 2, 5), TWR_RP_REPLAY, "lane 2 kept");
}

/* ---- Gateway logic ------------------------------------------------------------------ */

static struct twr_gw gw;
static struct twr_rx_out rx;

static void gw_fresh(void)
{
	twr_gw_init(&gw, GW_ADDR, NODE_ADDR, key_00_0f);
}

ZTEST(tower_codec, test_gw_kat_ack_pending_downlink)
{
	const struct tower_kat *up = kat("uplink_confirmed");
	const struct tower_kat *ack0 = kat("ack_no_pending");
	const struct tower_kat *ack1 = kat("ack_pending");
	const struct tower_kat *dl = kat("downlink_data");
	static uint8_t frame[TWR_MAX_FRAME];
	uint32_t ctr = 0;
	size_t n;

	gw_fresh();
	zassert_equal(gw.tx_counter, 1, "counter starts at 1");
	zassert_true(twr_gw_set_tx_counter(&gw, 100));

	/* confirmed uplink: delivered and ACKed with the gateway's counter 100 */
	twr_gw_on_rx(&gw, up->frame, up->frame_len, -57, &rx);
	zassert_equal(rx.res, TWR_RX_OK, "%s", twr_rx_res_str(rx.res));
	zassert_equal(rx.pt_len, up->plaintext_len);
	zassert_mem_equal(rx.pt, up->plaintext, rx.pt_len);
	zassert_true(rx.send_ack);
	zassert_false(rx.ack_pending);
	zassert_equal(rx.ack_ctr, 100);
	zassert_equal(rx.ack_len, ack0->frame_len);
	zassert_mem_equal(rx.ack_frame, ack0->frame, rx.ack_len, "ack_no_pending");
	zassert_equal(gw.tx_counter, 101);

	/* queue a downlink; the node retransmits the same counter: dup, re-ACK with PENDING */
	zassert_true(twr_gw_queue_dl(&gw, dl->plaintext, dl->plaintext_len, false));
	twr_gw_on_rx(&gw, up->frame, up->frame_len, -57, &rx);
	zassert_equal(rx.res, TWR_RX_DUP);
	zassert_true(rx.send_ack);
	zassert_true(rx.ack_pending);
	zassert_equal(rx.ack_ctr, 101);
	zassert_mem_equal(rx.ack_frame, ack1->frame, ack1->frame_len, "ack_pending");

	/* the downlink goes out with the next counter and empties the queue */
	n = twr_gw_build_dl(&gw, frame, sizeof(frame), &ctr);
	zassert_equal(n, dl->frame_len);
	zassert_equal(ctr, 102);
	zassert_mem_equal(frame, dl->frame, n, "downlink_data");
	zassert_false(gw.dl_queued);
	zassert_equal(twr_gw_build_dl(&gw, frame, sizeof(frame), &ctr), 0);
	zassert_equal(gw.tx_counter, 103);
}

ZTEST(tower_codec, test_gw_classification)
{
	const struct tower_kat *un = kat("uplink_unconfirmed");
	const struct tower_kat *up = kat("uplink_confirmed");
	const struct tower_kat *ack0 = kat("ack_no_pending");
	const struct tower_kat *empty = kat("uplink_empty");
	static uint8_t frame[TWR_MAX_FRAME];

	gw_fresh();

	/* unconfirmed: delivered, no ACK, no counter spent */
	twr_gw_on_rx(&gw, un->frame, un->frame_len, -80, &rx);
	zassert_equal(rx.res, TWR_RX_OK);
	zassert_false(rx.send_ack);
	zassert_equal(gw.tx_counter, 1);

	/* an unconfirmed retransmit is a dup without ACK */
	twr_gw_on_rx(&gw, un->frame, un->frame_len, -80, &rx);
	zassert_equal(rx.res, TWR_RX_DUP);
	zassert_false(rx.send_ack);

	/* empty payload (counter 5) is fresh */
	twr_gw_on_rx(&gw, empty->frame, empty->frame_len, -80, &rx);
	zassert_equal(rx.res, TWR_RX_OK);
	zassert_equal(rx.pt_len, 0);

	/* counter 2 < 5: replay, dropped, not ACKed although confirmed */
	twr_gw_on_rx(&gw, up->frame, up->frame_len, -80, &rx);
	zassert_equal(rx.res, TWR_RX_REPLAY);
	zassert_false(rx.send_ack);
	zassert_equal(rx.pt_len, 0);

	/* MIC failure does not move the window */
	memcpy(frame, kat("uplink_control_timereq")->frame, 25);
	frame[20] ^= 0x10;
	twr_gw_on_rx(&gw, frame, 25, -80, &rx);
	zassert_equal(rx.res, TWR_RX_MIC_FAIL);
	zassert_false(rx.send_ack);
	zassert_equal(twr_replay_check(&gw.replay, NODE_ADDR, 5), TWR_RP_RETRANSMIT);

	/* another dest: dropped before CCM */
	memcpy(frame, up->frame, up->frame_len);
	frame[6] ^= 0xff;
	twr_gw_on_rx(&gw, frame, up->frame_len, -80, &rx);
	zassert_equal(rx.res, TWR_RX_NOT_FOR_US);

	/* the gateway's own ACK is not for it (dest = node) */
	twr_gw_on_rx(&gw, ack0->frame, ack0->frame_len, -80, &rx);
	zassert_equal(rx.res, TWR_RX_NOT_FOR_US);

	/* an authenticated Ack toward the gateway is reported, lanes untouched */
	{
		struct twr_aes128 aes;
		struct twr_hdr h = {
			.type = TWR_T_ACK, .src = NODE_ADDR, .dest = GW_ADDR, .counter = 1,
		};
		uint8_t pl[TWR_ACK_PAYLOAD_LEN];
		size_t n;

		twr_aes128_init(&aes, key_00_0f);
		twr_ack_payload_encode(102, -60, false, pl);
		n = twr_frame_seal(&aes, &h, pl, sizeof(pl), TWR_MAX_PAYLOAD, frame, sizeof(frame));
		twr_gw_on_rx(&gw, frame, n, -80, &rx);
		zassert_equal(rx.res, TWR_RX_ACK);
		zassert_false(rx.send_ack);
		zassert_equal(rx.pt_len, 6);
		zassert_equal(twr_replay_check(&gw.replay, NODE_ADDR, 5), TWR_RP_RETRANSMIT);
	}

	/* short / bad version / bad type */
	twr_gw_on_rx(&gw, up->frame, 10, -80, &rx);
	zassert_equal(rx.res, TWR_RX_SHORT);
	memcpy(frame, up->frame, up->frame_len);
	frame[0] = 0x40;
	twr_gw_on_rx(&gw, frame, up->frame_len, -80, &rx);
	zassert_equal(rx.res, TWR_RX_BAD_VER);
	frame[0] = 0x3f;
	twr_gw_on_rx(&gw, frame, up->frame_len, -80, &rx);
	zassert_equal(rx.res, TWR_RX_BAD_TYPE);
}

ZTEST(tower_codec, test_gw_pending_only_for_node)
{
	static uint8_t frame[TWR_MAX_FRAME];
	struct twr_aes128 aes;
	struct twr_hdr h = {
		.type = TWR_T_DATA, .flags = TWR_FLAG_CONFIRMED,
		.src = 0x0000beef, .dest = GW_ADDR, .counter = 9,
	};
	uint8_t pt[3] = { 1, 2, 3 };
	struct twr_ack_meta m;
	uint8_t ack_pt[TWR_ACK_PAYLOAD_LEN];
	struct twr_hdr ah;
	size_t n, al;

	gw_fresh();
	twr_aes128_init(&aes, key_00_0f);
	zassert_true(twr_gw_queue_dl(&gw, pt, sizeof(pt), true));

	/* another source with the shared bench key: ACKed, but no PENDING for it */
	n = twr_frame_seal(&aes, &h, pt, sizeof(pt), TWR_MAX_PAYLOAD, frame, sizeof(frame));
	twr_gw_on_rx(&gw, frame, n, -130, &rx);
	zassert_equal(rx.res, TWR_RX_OK);
	zassert_true(rx.send_ack);
	zassert_false(rx.ack_pending);
	zassert_equal(twr_frame_open(&aes, rx.ack_frame, rx.ack_len, &ah, ack_pt,
				     sizeof(ack_pt), &al), TWR_OK);
	zassert_equal(ah.type, TWR_T_ACK);
	zassert_equal(ah.src, GW_ADDR);
	zassert_equal(ah.dest, 0x0000beef);
	zassert_true(twr_ack_payload_parse(ack_pt, al, &m));
	zassert_equal(m.acked_counter, 9);
	zassert_equal(m.rssi, -128, "rssi clamped");
	zassert_false(m.pending);
	zassert_true(gw.dl_queued, "queue untouched");

	/* the node gets PENDING; a confirmed DL sets CONFIRMED on the Data frame */
	h.src = NODE_ADDR;
	n = twr_frame_seal(&aes, &h, pt, sizeof(pt), TWR_MAX_PAYLOAD, frame, sizeof(frame));
	twr_gw_on_rx(&gw, frame, n, -70, &rx);
	zassert_true(rx.ack_pending);
	{
		uint32_t ctr;

		n = twr_gw_build_dl(&gw, frame, sizeof(frame), &ctr);
		zassert_equal(frame[0], 0x20);
		zassert_equal(frame[1], TWR_FLAG_CONFIRMED);
		zassert_equal(ctr, 3);
	}
}

ZTEST(tower_codec, test_gw_tx_counter_ceiling)
{
	const struct tower_kat *hi = kat("uplink_counter_high");
	const struct tower_kat *up = kat("uplink_confirmed");
	static uint8_t frame[TWR_MAX_FRAME];
	uint32_t ctr;

	gw_fresh();
	zassert_false(twr_gw_set_tx_counter(&gw, 0));
	zassert_true(twr_gw_set_tx_counter(&gw, UINT32_MAX - 1));

	/* the last usable counter is 2^32 - 2, then the gateway locks */
	twr_gw_on_rx(&gw, up->frame, up->frame_len, -57, &rx);
	zassert_true(rx.send_ack);
	zassert_equal(rx.ack_ctr, UINT32_MAX - 1);
	zassert_true(gw.tx_locked);

	twr_gw_on_rx(&gw, hi->frame, hi->frame_len, -57, &rx);
	zassert_equal(rx.res, TWR_RX_OK, "rx still works when locked");
	zassert_false(rx.send_ack);
	zassert_true(twr_gw_queue_dl(&gw, frame, 1, false));
	zassert_equal(twr_gw_build_dl(&gw, frame, sizeof(frame), &ctr), 0);
	zassert_false(twr_gw_queue_dl(&gw, frame, TWR_MAX_PAYLOAD + 1, false));
}

/* ---- Airtime ------------------------------------------------------------------------ */

ZTEST(tower_codec, test_lora_toa)
{
	struct twr_phy p = {
		.sf = 7, .bw_hz = 125000, .cr = 1, .preamble = 8,
		.explicit_hdr = true, .crc = true,
	};

	zassert_equal(twr_lora_tsym_us(&p), 1024);
	zassert_equal(twr_lora_toa_us(&p, 96), 164096);
	zassert_equal(twr_lora_toa_us(&p, TWR_ACK_FRAME_LEN), 66816);
	zassert_equal(twr_lora_toa_us(&p, 0), 25856); /* preamble + 8 symbols */
	p.sf = 9;
	zassert_equal(twr_lora_toa_us(&p, 28), 226304);
	p.sf = 10;
	zassert_equal(twr_lora_toa_us(&p, 96), 985088);
	p.sf = 11; /* low data rate optimisation on */
	zassert_equal(twr_lora_toa_us(&p, 28), 905216);
	p.sf = 12;
	zassert_equal(twr_lora_toa_us(&p, 96), 3940352);
	zassert_equal(twr_lora_toa_us(&p, 255), 9019392);
	zassert_equal(twr_lora_toa_us(&p, TWR_ACK_FRAME_LEN), 1646592);
}

ZTEST(tower_codec, test_duty_ledger)
{
	static struct twr_duty d;
	struct twr_duty_sum s;
	const int64_t min = 60000;

	twr_duty_init(&d);
	twr_duty_add(&d, 0, 164096, 165000);
	twr_duty_add(&d, 30 * min + 5, 66816, 67000);
	twr_duty_sum(&d, 59 * min, &s);
	zassert_equal(s.n_tx, 2);
	zassert_equal(s.formula_us, 164096 + 66816);
	zassert_equal(s.measured_us, 165000 + 67000);

	/* minute 0 still counts 60 minutes later (61 buckets), then ages out */
	twr_duty_sum(&d, 60 * min + 59999, &s);
	zassert_equal(s.n_tx, 2);
	twr_duty_sum(&d, 61 * min, &s);
	zassert_equal(s.n_tx, 1);
	zassert_equal(s.formula_us, 66816);

	/* a bucket slot reused 61 minutes later starts from zero */
	twr_duty_add(&d, 61 * min, 1000, 0);
	twr_duty_sum(&d, 61 * min, &s);
	zassert_equal(s.n_tx, 2);
	zassert_equal(s.formula_us, 66816 + 1000);

	/* 36 s is the whole 1 % budget */
	zassert_equal(twr_duty_budget_bp(36000000ull, TWR_DUTY_BUDGET_1PCT_US), 10000);
	zassert_equal(twr_duty_budget_bp(164096, TWR_DUTY_BUDGET_1PCT_US), 45);
	zassert_equal(twr_duty_budget_bp(36000000ull, TWR_DUTY_BUDGET_10PCT_US), 1000);

	/* 10 % only inside h1.6, 869.400-869.650 MHz */
	zassert_equal(twr_duty_budget_us(868100000u), TWR_DUTY_BUDGET_1PCT_US);
	zassert_equal(twr_duty_budget_us(869399999u), TWR_DUTY_BUDGET_1PCT_US);
	zassert_equal(twr_duty_budget_us(869400000u), TWR_DUTY_BUDGET_10PCT_US);
	zassert_equal(twr_duty_budget_us(869525000u), TWR_DUTY_BUDGET_10PCT_US);
	zassert_equal(twr_duty_budget_us(869650000u), TWR_DUTY_BUDGET_10PCT_US);
	zassert_equal(twr_duty_budget_us(869650001u), TWR_DUTY_BUDGET_1PCT_US);
}

ZTEST_SUITE(tower_codec, NULL, NULL, NULL, NULL, NULL);
