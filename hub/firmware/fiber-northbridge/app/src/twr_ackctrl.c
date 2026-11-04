/* SPDX-License-Identifier: Apache-2.0
 *
 * Answers in the ACK, see twr_ackctrl.h.
 */
#include "twr_ackctrl.h"

static int8_t clamp8(int32_t v)
{
	return (int8_t)(v < INT8_MIN ? INT8_MIN : (v > INT8_MAX ? INT8_MAX : v));
}

/* floor(a / 4) for a negative a too. */
static int32_t floor_div4(int32_t a)
{
	return a >= 0 ? a / 4 : -((-a + 3) / 4);
}

int8_t twr_ackctrl_snr_db(int8_t snr_q)
{
	return clamp8(floor_div4(snr_q));
}

int8_t twr_ackctrl_margin(int8_t snr_q, uint8_t sf)
{
	/* In quarter dB: floor = -30 at SF7, 10 lower per step; margin = snr_q - floor. */
	int32_t steps = sf > 7 ? (int32_t)sf - 7 : 0;

	return clamp8(floor_div4((int32_t)snr_q + 30 + 10 * steps));
}

bool twr_ackctrl_wall(const struct twr_time_anchor *a, uint64_t t_ms, uint32_t *unix_s,
		      uint8_t *frac)
{
	int64_t wall;

	if (a == NULL || !a->valid) {
		return false;
	}
	/* Both clocks stay far below 2^63 ms. */
	wall = (int64_t)a->unix_ms + ((int64_t)t_ms - (int64_t)a->now_ms);
	if (wall < 0 || wall / 1000 > (int64_t)UINT32_MAX) {
		return false;
	}
	*unix_s = (uint32_t)(wall / 1000);
	*frac = (uint8_t)((wall % 1000) * 256 / 1000);
	return true;
}

bool twr_ackctrl_requests(const uint8_t *pt, size_t len, bool *link_check, bool *time)
{
	size_t off = 1;

	*link_check = false;
	*time = false;
	if (len < 1 || pt[0] != TWR_CTRL_ENVELOPE) {
		return false;
	}
	while (off < len) {
		if (len - off < 2 || len - off - 2 < pt[off + 1]) {
			*link_check = false;
			*time = false;
			return false;
		}
		*link_check |= pt[off] == TWR_CTRL_LINK_CHECK;
		*time |= pt[off] == TWR_CTRL_TIME;
		off += 2u + pt[off + 1];
	}
	return true;
}

static void put_le32(uint8_t *p, uint32_t v)
{
	p[0] = (uint8_t)v;
	p[1] = (uint8_t)(v >> 8);
	p[2] = (uint8_t)(v >> 16);
	p[3] = (uint8_t)(v >> 24);
}

size_t twr_ackctrl_build(const uint8_t *pt, size_t len, const struct twr_ackctrl_in *in,
			 uint8_t tail[TWR_ACKCTRL_TAIL_MAX], struct twr_ackctrl_res *res)
{
	bool lc;
	bool tm;
	size_t n = 0;
	uint32_t unix_s;
	uint8_t frac;

	*res = (struct twr_ackctrl_res){ 0 };
	if (!twr_ackctrl_requests(pt, len, &lc, &tm)) {
		return 0;
	}
	if (lc) {
		tail[n++] = TWR_CTRL_LINK_CHECK;
		tail[n++] = TWR_CTRL_LINK_CHECK_LEN;
		tail[n++] = (uint8_t)clamp8(in->rssi_dbm);
		tail[n++] = (uint8_t)twr_ackctrl_snr_db(in->snr_q);
		tail[n++] = (uint8_t)twr_ackctrl_margin(in->snr_q, in->sf);
		tail[n++] = TWR_LINK_CHECK_GW_COUNT;
		res->lc = true;
	}
	if (tm) {
		if (twr_ackctrl_wall(in->anchor, in->t_rx_ms, &unix_s, &frac)) {
			tail[n++] = TWR_CTRL_TIME;
			tail[n++] = TWR_CTRL_TIME_LEN;
			put_le32(&tail[n], unix_s);
			tail[n + 4] = frac;
			put_le32(&tail[n + 5], in->counter);
			n += TWR_CTRL_TIME_LEN;
			res->time = true;
		} else {
			/* The node asks again later. */
			res->time_no_anchor = true;
		}
	}
	return n;
}

bool twr_ackctrl_next(const uint8_t *tail, size_t len, size_t *off, uint8_t *id,
		      const uint8_t **body, uint8_t *body_len)
{
	size_t o = *off;

	if (o >= len || len - o < 2 || len - o - 2 < tail[o + 1]) {
		return false;
	}
	*id = tail[o];
	*body_len = tail[o + 1];
	*body = &tail[o + 2];
	*off = o + 2u + tail[o + 1];
	return true;
}
