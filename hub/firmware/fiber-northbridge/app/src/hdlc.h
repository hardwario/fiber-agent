/* SPDX-License-Identifier: Apache-2.0
 *
 * HDLC-style byte-stuffed framing for the P2P host<->modem link.
 * Spec: proximos-v2 plan/system/lora/design.md §3.
 *
 * Frame on the wire: 0x7E | escaped(type|seq|body|crc16) | 0x7E
 * CRC16 (see hdlc.c for the exact algorithm) is computed over the
 * unescaped type|seq|body.
 */
#ifndef APP_HDLC_H_
#define APP_HDLC_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "proto.h"

/* Streaming decoder state. Persists across uart_link_poll() calls so a
 * frame split across polls (or UART bytes) reassembles correctly.
 * P2P_MAX_FRAME_LEN is already type+seq+body+crc16 with no flag bytes
 * counted (see proto.h) - the flags are frame delimiters, consumed by
 * hdlc_decoder_feed() and never stored in buf.
 */
struct hdlc_decoder {
	uint8_t buf[P2P_MAX_FRAME_LEN];
	size_t len;
	bool in_frame;
	bool escape;
};

enum hdlc_feed_result {
	HDLC_FEED_NONE = 0,     /* byte consumed, no frame boundary yet */
	HDLC_FEED_FRAME,        /* valid frame decoded and dispatched via cb */
	HDLC_FEED_CRC_ERROR,    /* frame boundary hit, CRC did not match */
	HDLC_FEED_TOO_SHORT,    /* frame boundary hit, fewer than 4 bytes (type+seq+crc16) */
	HDLC_FEED_OVERFLOW,     /* frame exceeded the max length and was discarded */
};

typedef void (*hdlc_frame_cb)(uint8_t type, uint8_t seq, const uint8_t *body,
			       size_t body_len, void *user_data);

void hdlc_decoder_init(struct hdlc_decoder *dec);

/* Feed one raw (still-escaped) byte from the wire. Invokes cb synchronously
 * exactly when the return value is HDLC_FEED_FRAME. */
enum hdlc_feed_result hdlc_decoder_feed(struct hdlc_decoder *dec, uint8_t byte,
					 hdlc_frame_cb cb, void *user_data);

/* Encodes flag | escaped(type|seq|body|crc16) | flag into out[].
 * Returns the encoded length, or -1 if out_size is too small.
 */
int hdlc_encode(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len,
		 uint8_t *out, size_t out_size);

#endif /* APP_HDLC_H_ */
