/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER frame codec, see twr_frame.h.
 */
#include "twr_frame.h"

#include <string.h>

static void put_le32(uint8_t *p, uint32_t v)
{
	p[0] = (uint8_t)v;
	p[1] = (uint8_t)(v >> 8);
	p[2] = (uint8_t)(v >> 16);
	p[3] = (uint8_t)(v >> 24);
}

static uint32_t get_le32(const uint8_t *p)
{
	return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
	       ((uint32_t)p[3] << 24);
}

const char *twr_err_str(enum twr_err e)
{
	switch (e) {
	case TWR_OK:
		return "ok";
	case TWR_E_SHORT:
		return "short";
	case TWR_E_VERSION:
		return "bad_ver";
	case TWR_E_TYPE:
		return "bad_type";
	case TWR_E_TOO_LONG:
		return "too_long";
	case TWR_E_MIC:
		return "mic_fail";
	}
	return "?";
}

bool twr_type_has_bulk(uint8_t type)
{
	return type == TWR_T_BULK_REQ || type == TWR_T_BULK_DATA;
}

size_t twr_hdr_len(uint8_t type)
{
	return twr_type_has_bulk(type) ? TWR_HDR_LEN_BULK : TWR_HDR_LEN;
}

size_t twr_hdr_encode(const struct twr_hdr *h, uint8_t *out)
{
	out[0] = (uint8_t)((TWR_VERSION << 5) | (h->type & 0x1Fu));
	out[1] = h->flags;
	put_le32(&out[2], h->src);
	put_le32(&out[6], h->dest);
	put_le32(&out[10], h->counter);
	if (twr_type_has_bulk(h->type)) {
		out[14] = (uint8_t)h->bulk_idx;
		out[15] = (uint8_t)(h->bulk_idx >> 8);
		out[16] = (uint8_t)(h->bulk_idx >> 16);
		return TWR_HDR_LEN_BULK;
	}
	return TWR_HDR_LEN;
}

enum twr_err twr_hdr_parse(const uint8_t *buf, size_t len, struct twr_hdr *h, size_t *hdr_len)
{
	uint8_t type;

	if (len < TWR_HDR_LEN) {
		return TWR_E_SHORT;
	}
	if ((buf[0] >> 5) != TWR_VERSION) {
		return TWR_E_VERSION;
	}
	type = buf[0] & 0x1Fu;
	if (type > TWR_T_BEACON) {
		return TWR_E_TYPE;
	}
	h->type = type;
	h->flags = buf[1];
	h->src = get_le32(&buf[2]);
	h->dest = get_le32(&buf[6]);
	h->counter = get_le32(&buf[10]);
	h->bulk_idx = 0;
	if (twr_type_has_bulk(type)) {
		if (len < TWR_HDR_LEN_BULK) {
			return TWR_E_SHORT;
		}
		h->bulk_idx = (uint32_t)buf[14] | ((uint32_t)buf[15] << 8) |
			      ((uint32_t)buf[16] << 16);
		*hdr_len = TWR_HDR_LEN_BULK;
	} else {
		*hdr_len = TWR_HDR_LEN;
	}
	return TWR_OK;
}

void twr_nonce(uint32_t src, uint32_t counter, uint32_t bulk_idx,
	       uint8_t nonce[TWR_CCM_NONCE_LEN])
{
	put_le32(&nonce[0], src);
	put_le32(&nonce[4], counter);
	nonce[8] = (uint8_t)bulk_idx;
	nonce[9] = (uint8_t)(bulk_idx >> 8);
	nonce[10] = (uint8_t)(bulk_idx >> 16);
	nonce[11] = 0;
	nonce[12] = 0;
}

size_t twr_frame_seal(const struct twr_aes128 *aes, const struct twr_hdr *h, const uint8_t *pt,
		      size_t pt_len, size_t max_payload, uint8_t *out, size_t out_cap)
{
	uint8_t nonce[TWR_CCM_NONCE_LEN];
	size_t hlen = twr_hdr_len(h->type);

	if (pt_len > max_payload || hlen + pt_len + TWR_TAG_LEN > out_cap ||
	    hlen + pt_len + TWR_TAG_LEN > TWR_MAX_FRAME) {
		return 0;
	}
	twr_hdr_encode(h, out);
	if (pt_len > 0) {
		memmove(&out[hlen], pt, pt_len);
	}
	twr_nonce(h->src, h->counter, twr_type_has_bulk(h->type) ? h->bulk_idx : 0, nonce);
	if (!twr_ccm_seal(aes, nonce, out, hlen, &out[hlen], pt_len, &out[hlen + pt_len])) {
		return 0;
	}
	return hlen + pt_len + TWR_TAG_LEN;
}

enum twr_err twr_frame_open(const struct twr_aes128 *aes, const uint8_t *frame, size_t len,
			    struct twr_hdr *h, uint8_t *pt, size_t pt_cap, size_t *pt_len)
{
	uint8_t nonce[TWR_CCM_NONCE_LEN];
	size_t hlen;
	size_t plen;
	enum twr_err err;

	*pt_len = 0;
	err = twr_hdr_parse(frame, len, h, &hlen);
	if (err != TWR_OK) {
		return err;
	}
	if (len < hlen + TWR_TAG_LEN) {
		return TWR_E_SHORT;
	}
	plen = len - hlen - TWR_TAG_LEN;
	if (plen > pt_cap) {
		return TWR_E_TOO_LONG;
	}
	if (plen > 0) {
		memcpy(pt, &frame[hlen], plen);
	}
	twr_nonce(h->src, h->counter, h->bulk_idx, nonce);
	if (!twr_ccm_open(aes, nonce, frame, hlen, pt, plen, &frame[hlen + plen])) {
		return TWR_E_MIC;
	}
	*pt_len = plen;
	return TWR_OK;
}

void twr_ack_payload_encode(uint32_t acked_counter, int rssi_dbm, bool pending,
			    uint8_t out[TWR_ACK_PAYLOAD_LEN])
{
	int r = rssi_dbm < INT8_MIN ? INT8_MIN : (rssi_dbm > INT8_MAX ? INT8_MAX : rssi_dbm);

	put_le32(&out[0], acked_counter);
	out[4] = (uint8_t)(int8_t)r;
	out[5] = pending ? TWR_ACK_FLAG_PENDING : 0u;
}

bool twr_ack_payload_parse(const uint8_t *p, size_t len, struct twr_ack_meta *m)
{
	if (len < 4) {
		return false;
	}
	m->acked_counter = get_le32(p);
	m->has_rssi = len >= 5;
	m->rssi = m->has_rssi ? (int8_t)p[4] : 0;
	m->pending = len >= 6 && (p[5] & TWR_ACK_FLAG_PENDING) != 0;
	return true;
}

/* ---- Replay window ------------------------------------------------------------------ */

void twr_replay_init(struct twr_replay_tbl *t)
{
	memset(t, 0, sizeof(*t));
}

static int lane_find(const struct twr_replay_tbl *t, uint32_t src)
{
	for (size_t i = 0; i < TWR_REPLAY_LANES; i++) {
		if (t->lane[i].used && t->lane[i].src == src) {
			return (int)i;
		}
	}
	return -1;
}

static enum twr_replay classify(uint32_t last, uint32_t ctr)
{
	if (ctr == 0 || ctr < last) {
		return TWR_RP_REPLAY;
	}
	return ctr == last ? TWR_RP_RETRANSMIT : TWR_RP_FRESH;
}

enum twr_replay twr_replay_check(const struct twr_replay_tbl *t, uint32_t src, uint32_t ctr)
{
	int i = lane_find(t, src);

	/* An unknown source starts from last_seen = 0 (tower-net-core ReplayLane::new(0)). */
	return classify(i < 0 ? 0 : t->lane[i].last, ctr);
}

enum twr_replay twr_replay_accept(struct twr_replay_tbl *t, uint32_t src, uint32_t ctr)
{
	int i = lane_find(t, src);
	enum twr_replay v = classify(i < 0 ? 0 : t->lane[i].last, ctr);

	if (v != TWR_RP_FRESH) {
		return v;
	}
	if (i < 0) {
		for (size_t j = 0; j < TWR_REPLAY_LANES; j++) {
			if (!t->lane[j].used) {
				i = (int)j;
				break;
			}
		}
		if (i < 0) {
			i = t->next_evict;
			t->next_evict = (uint8_t)((t->next_evict + 1u) % TWR_REPLAY_LANES);
		}
		t->lane[i].used = true;
		t->lane[i].src = src;
	}
	t->lane[i].last = ctr;
	return v;
}

const char *twr_replay_str(enum twr_replay r)
{
	switch (r) {
	case TWR_RP_FRESH:
		return "ok";
	case TWR_RP_RETRANSMIT:
		return "dup";
	case TWR_RP_REPLAY:
		return "replay";
	}
	return "?";
}
