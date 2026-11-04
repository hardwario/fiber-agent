/* SPDX-License-Identifier: Apache-2.0 */
#include "hdlc.h"

#include <string.h>
#include <zephyr/sys/byteorder.h>
#include <zephyr/sys/crc.h>

/* CRC16-CCITT (poly 0x1021, init 0xFFFF, no reflection - aka CRC-16/CCITT-FALSE).
 *
 * Zephyr's naming is the opposite of what it looks like: crc16_ccitt() is
 * the *reflected* variant (CRC-16/KERMIT-family). The non-reflected
 * CCITT-FALSE this link's spec calls for is crc16_itu_t(). Using the
 * wrong one produces a CRC that matches on-device but never matches a
 * reference/host implementation - verified against the CRC-16/CCITT-FALSE
 * test vector in tests/hdlc_framing.
 */
static uint16_t frame_crc16(const uint8_t *data, size_t len)
{
	return crc16_itu_t(0xFFFF, data, len);
}

void hdlc_decoder_init(struct hdlc_decoder *dec)
{
	memset(dec, 0, sizeof(*dec));
}

static enum hdlc_feed_result finish_frame(struct hdlc_decoder *dec, hdlc_frame_cb cb,
					   void *user_data)
{
	size_t len = dec->len;

	dec->len = 0;
	dec->escape = false;

	if (len == 0) {
		/* Two flags in a row (or leading flag) - not an error. */
		return HDLC_FEED_NONE;
	}
	if (len < 4) {
		/* type + seq + crc16 is the minimum possible frame. */
		return HDLC_FEED_TOO_SHORT;
	}

	size_t body_len = len - 4;
	uint16_t crc_rx = sys_get_le16(&dec->buf[len - 2]);
	uint16_t crc_calc = frame_crc16(dec->buf, len - 2);

	if (crc_rx != crc_calc) {
		return HDLC_FEED_CRC_ERROR;
	}

	cb(dec->buf[0], dec->buf[1], &dec->buf[2], body_len, user_data);
	return HDLC_FEED_FRAME;
}

enum hdlc_feed_result hdlc_decoder_feed(struct hdlc_decoder *dec, uint8_t byte,
					 hdlc_frame_cb cb, void *user_data)
{
	if (byte == HDLC_FLAG) {
		enum hdlc_feed_result result = finish_frame(dec, cb, user_data);

		dec->in_frame = true;
		return result;
	}

	if (!dec->in_frame) {
		/* Junk before the first flag (line noise, a stray console
		 * byte) - ignore until framing starts. */
		return HDLC_FEED_NONE;
	}

	if (byte == HDLC_ESC) {
		dec->escape = true;
		return HDLC_FEED_NONE;
	}

	if (dec->escape) {
		byte ^= HDLC_ESC_XOR;
		dec->escape = false;
	}

	if (dec->len >= sizeof(dec->buf)) {
		/* Overflow: drop the whole frame, resync on the next flag. */
		dec->in_frame = false;
		dec->len = 0;
		dec->escape = false;
		return HDLC_FEED_OVERFLOW;
	}

	dec->buf[dec->len++] = byte;
	return HDLC_FEED_NONE;
}

int hdlc_encode(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len,
		 uint8_t *out, size_t out_size)
{
	if (body_len > P2P_MAX_BODY_LEN) {
		return -1;
	}

	uint8_t payload[2 + P2P_MAX_BODY_LEN + 2];

	payload[0] = type;
	payload[1] = seq;
	memcpy(&payload[2], body, body_len);

	size_t unescaped_len = 2 + body_len;
	uint16_t crc = frame_crc16(payload, unescaped_len);

	sys_put_le16(crc, &payload[unescaped_len]);
	unescaped_len += 2;

	size_t out_len = 0;

	if (out_len >= out_size) {
		return -1;
	}
	out[out_len++] = HDLC_FLAG;

	for (size_t i = 0; i < unescaped_len; i++) {
		uint8_t b = payload[i];

		if (b == HDLC_FLAG || b == HDLC_ESC) {
			if (out_len + 2 > out_size) {
				return -1;
			}
			out[out_len++] = HDLC_ESC;
			out[out_len++] = b ^ HDLC_ESC_XOR;
		} else {
			if (out_len + 1 > out_size) {
				return -1;
			}
			out[out_len++] = b;
		}
	}

	if (out_len >= out_size) {
		return -1;
	}
	out[out_len++] = HDLC_FLAG;

	return (int)out_len;
}
