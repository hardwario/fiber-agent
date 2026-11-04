/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for the HDLC byte-stuffing framing + CRC16 in hdlc.c.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/crc.h>
#include <string.h>

#include "hdlc.h"
#include "proto.h"

/* ---- CRC16-CCITT-FALSE reference vector ------------------------------
 * "123456789" -> 0x29B1 (poly 0x1021, init 0xFFFF, no reflection).
 * Zephyr's crc16_itu_t() is the non-reflected variant despite the name -
 * crc16_ccitt() is actually reflected (CRC-16/KERMIT family). Pinning this
 * vector here catches anyone "fixing" hdlc.c to call the wrong one.
 */
ZTEST(hdlc_framing, test_crc16_ccitt_false_reference_vector)
{
	static const uint8_t check[] = "123456789";

	zassert_equal(crc16_itu_t(0xFFFF, check, sizeof(check) - 1), 0x29B1);
}

/* ---- decoder capture helper -------------------------------------------- */

#define MAX_CAPTURED 8

struct captured_frame {
	uint8_t type;
	uint8_t seq;
	uint8_t body[300];
	size_t body_len;
};

static struct captured_frame captured[MAX_CAPTURED];
static int captured_count;

static void capture_cb(uint8_t type, uint8_t seq, const uint8_t *body,
			size_t body_len, void *user_data)
{
	ARG_UNUSED(user_data);

	if (captured_count < MAX_CAPTURED) {
		struct captured_frame *f = &captured[captured_count];

		f->type = type;
		f->seq = seq;
		f->body_len = body_len;
		memcpy(f->body, body, body_len);
	}
	captured_count++;
}

static struct hdlc_decoder dec;

static void reset_state(void *fixture)
{
	ARG_UNUSED(fixture);
	hdlc_decoder_init(&dec);
	captured_count = 0;
	memset(captured, 0, sizeof(captured));
}

ZTEST_SUITE(hdlc_framing, NULL, NULL, reset_state, NULL, NULL);

static enum hdlc_feed_result feed_bytes(const uint8_t *bytes, size_t len)
{
	enum hdlc_feed_result last = HDLC_FEED_NONE;

	for (size_t i = 0; i < len; i++) {
		last = hdlc_decoder_feed(&dec, bytes[i], capture_cb, NULL);
	}
	return last;
}

/* ---- round trip ---------------------------------------------------------- */

ZTEST(hdlc_framing, test_encode_decode_round_trip)
{
	static const uint8_t body[] = {0x01, 0x02, 0x03, 0x04};
	uint8_t frame[64];

	int n = hdlc_encode(0x05, 0x2A, body, sizeof(body), frame, sizeof(frame));

	zassert_true(n > 0, "encode failed");
	zassert_equal(feed_bytes(frame, (size_t)n), HDLC_FEED_FRAME);
	zassert_equal(captured_count, 1);
	zassert_equal(captured[0].type, 0x05);
	zassert_equal(captured[0].seq, 0x2A);
	zassert_equal(captured[0].body_len, sizeof(body));
	zassert_mem_equal(captured[0].body, body, sizeof(body));
}

ZTEST(hdlc_framing, test_empty_body_round_trip)
{
	uint8_t frame[16];

	int n = hdlc_encode(0x03, 0x01, NULL, 0, frame, sizeof(frame));

	zassert_true(n > 0);
	zassert_equal(feed_bytes(frame, (size_t)n), HDLC_FEED_FRAME);
	zassert_equal(captured[0].body_len, 0);
}

/* ---- byte stuffing -------------------------------------------------------- */

ZTEST(hdlc_framing, test_flag_byte_in_body_is_escaped)
{
	static const uint8_t body[] = {HDLC_FLAG, 0x00, HDLC_FLAG};
	uint8_t frame[64];

	int n = hdlc_encode(0x01, 0x00, body, sizeof(body), frame, sizeof(frame));

	zassert_true(n > 0);
	/* every 0x7E in the body must have been escaped: only the two
	 * framing flags remain as bare 0x7E bytes. */
	int bare_flags = 0;

	for (int i = 0; i < n; i++) {
		if (frame[i] == HDLC_FLAG) {
			bare_flags++;
		}
	}
	zassert_equal(bare_flags, 2);

	zassert_equal(feed_bytes(frame, (size_t)n), HDLC_FEED_FRAME);
	zassert_mem_equal(captured[0].body, body, sizeof(body));
}

ZTEST(hdlc_framing, test_esc_byte_in_body_is_escaped)
{
	static const uint8_t body[] = {HDLC_ESC, HDLC_ESC, 0xAA};
	uint8_t frame[64];

	int n = hdlc_encode(0x01, 0x00, body, sizeof(body), frame, sizeof(frame));

	zassert_true(n > 0);
	zassert_equal(feed_bytes(frame, (size_t)n), HDLC_FEED_FRAME);
	zassert_mem_equal(captured[0].body, body, sizeof(body));
}

/* ---- streaming / split across polls --------------------------------------- */

ZTEST(hdlc_framing, test_frame_split_byte_by_byte)
{
	static const uint8_t body[] = {0xDE, 0xAD, 0xBE, 0xEF};
	uint8_t frame[64];

	int n = hdlc_encode(0x02, 0x07, body, sizeof(body), frame, sizeof(frame));

	zassert_true(n > 0);

	enum hdlc_feed_result last = HDLC_FEED_NONE;

	for (int i = 0; i < n; i++) {
		last = hdlc_decoder_feed(&dec, frame[i], capture_cb, NULL);
		if (i < n - 1) {
			zassert_equal(last, HDLC_FEED_NONE,
				      "frame must not complete before the closing flag");
		}
	}
	zassert_equal(last, HDLC_FEED_FRAME);
	zassert_mem_equal(captured[0].body, body, sizeof(body));
}

ZTEST(hdlc_framing, test_two_frames_share_one_flag)
{
	uint8_t frame_a[32];
	uint8_t frame_b[32];
	static const uint8_t body_a[] = {0x01};
	static const uint8_t body_b[] = {0x02};

	int na = hdlc_encode(0x01, 0x01, body_a, sizeof(body_a), frame_a, sizeof(frame_a));
	int nb = hdlc_encode(0x01, 0x02, body_b, sizeof(body_b), frame_b, sizeof(frame_b));

	zassert_true(na > 0 && nb > 0);

	/* Concatenate with the shared flag: frame_a already ends in 0x7E and
	 * frame_b starts with 0x7E - feed frame_b starting at its second byte
	 * to reproduce a real back-to-back HDLC stream. */
	feed_bytes(frame_a, (size_t)na);
	zassert_equal(captured_count, 1);
	feed_bytes(&frame_b[1], (size_t)(nb - 1));
	zassert_equal(captured_count, 2);
	zassert_equal(captured[1].seq, 0x02);
}

/* ---- error paths ----------------------------------------------------------- */

ZTEST(hdlc_framing, test_corrupted_crc_is_rejected)
{
	static const uint8_t body[] = {0x01, 0x02};
	uint8_t frame[32];

	int n = hdlc_encode(0x01, 0x00, body, sizeof(body), frame, sizeof(frame));

	zassert_true(n > 0);
	/* flip the first body byte (frame[0]=flag, [1]=type, [2]=seq, [3]=body[0]) */
	frame[3] ^= 0xFF;

	zassert_equal(feed_bytes(frame, (size_t)n), HDLC_FEED_CRC_ERROR);
	zassert_equal(captured_count, 0, "a corrupted frame must not dispatch");
}

ZTEST(hdlc_framing, test_short_frame_is_rejected)
{
	/* type + seq only, no crc16 - fewer than the minimum 4 bytes. */
	static const uint8_t junk[] = {HDLC_FLAG, 0x01, 0x02, HDLC_FLAG};

	zassert_equal(feed_bytes(junk, sizeof(junk)), HDLC_FEED_TOO_SHORT);
	zassert_equal(captured_count, 0);
}

ZTEST(hdlc_framing, test_double_flag_is_not_an_error)
{
	uint8_t idle[] = {HDLC_FLAG, HDLC_FLAG, HDLC_FLAG};

	zassert_equal(feed_bytes(idle, sizeof(idle)), HDLC_FEED_NONE);
	zassert_equal(captured_count, 0);
}

ZTEST(hdlc_framing, test_junk_before_first_flag_is_ignored)
{
	static const uint8_t body[] = {0x42};
	uint8_t frame[32];
	uint8_t stream[3 + sizeof(frame)];

	int n = hdlc_encode(0x01, 0x00, body, sizeof(body), frame, sizeof(frame));

	zassert_true(n > 0);
	stream[0] = 0x00;
	stream[1] = 0xFF;
	stream[2] = 0x10;
	memcpy(&stream[3], frame, (size_t)n);

	feed_bytes(stream, 3 + (size_t)n);
	zassert_equal(captured_count, 1);
	zassert_mem_equal(captured[0].body, body, sizeof(body));
}

ZTEST(hdlc_framing, test_oversized_frame_overflows_and_resyncs)
{
	uint8_t body[P2P_MAX_BODY_LEN + 16];
	uint8_t frame[sizeof(body) + 8];

	memset(body, 0x55, sizeof(body));
	/* hdlc_encode itself refuses an over-budget body */
	zassert_equal(hdlc_encode(0x01, 0x00, body, sizeof(body), frame, sizeof(frame)), -1);

	/* Feed a raw over-length frame directly at the decoder (as if a
	 * buggy peer sent one) - it must overflow, not overrun buf[]. */
	bool saw_overflow = false;

	zassert_equal(hdlc_decoder_feed(&dec, HDLC_FLAG, capture_cb, NULL), HDLC_FEED_NONE);
	for (size_t i = 0; i < sizeof(body); i++) {
		if (hdlc_decoder_feed(&dec, body[i], capture_cb, NULL) == HDLC_FEED_OVERFLOW) {
			saw_overflow = true;
		}
	}
	zassert_true(saw_overflow, "expected an HDLC_FEED_OVERFLOW among the fed bytes");
	zassert_equal(captured_count, 0);

	/* Recovery: a well-formed frame right after must decode cleanly. */
	static const uint8_t ok_body[] = {0x09};
	uint8_t ok_frame[16];
	int n = hdlc_encode(0x01, 0x00, ok_body, sizeof(ok_body), ok_frame, sizeof(ok_frame));

	zassert_true(n > 0);
	zassert_equal(feed_bytes(ok_frame, (size_t)n), HDLC_FEED_FRAME);
	zassert_equal(captured_count, 1);
}

ZTEST(hdlc_framing, test_encode_rejects_oversized_body)
{
	uint8_t body[P2P_MAX_BODY_LEN + 1] = {0};
	uint8_t frame[8];

	zassert_equal(hdlc_encode(0x01, 0x00, body, sizeof(body), frame, sizeof(frame)), -1);
}

ZTEST(hdlc_framing, test_encode_rejects_undersized_output_buffer)
{
	static const uint8_t body[] = {0x01, 0x02, 0x03};
	uint8_t frame[3]; /* far too small for flag+type+seq+body+crc+flag */

	zassert_equal(hdlc_encode(0x01, 0x00, body, sizeof(body), frame, sizeof(frame)), -1);
}
