/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER P0 bench gateway logic, see twr_gw.h.
 */
#include "twr_gw.h"

#include <string.h>

const char *twr_rx_res_str(enum twr_rx_res r)
{
	switch (r) {
	case TWR_RX_OK:
		return "ok";
	case TWR_RX_DUP:
		return "dup";
	case TWR_RX_REPLAY:
		return "replay";
	case TWR_RX_MIC_FAIL:
		return "mic_fail";
	case TWR_RX_SHORT:
		return "short";
	case TWR_RX_BAD_VER:
		return "bad_ver";
	case TWR_RX_BAD_TYPE:
		return "bad_type";
	case TWR_RX_NOT_FOR_US:
		return "not_for_us";
	case TWR_RX_ACK:
		return "ack";
	case TWR_RX_OTHER:
		return "other";
	}
	return "?";
}

void twr_gw_init(struct twr_gw *gw, uint32_t gw_addr, uint32_t node_addr,
		 const uint8_t key[TWR_AES_KEY_LEN])
{
	memset(gw, 0, sizeof(*gw));
	twr_aes128_init(&gw->aes, key);
	gw->gw_addr = gw_addr;
	gw->node_addr = node_addr;
	gw->tx_counter = 1;
	twr_replay_init(&gw->replay);
}

bool twr_gw_set_tx_counter(struct twr_gw *gw, uint32_t next)
{
	if (next == 0) {
		return false;
	}
	gw->tx_counter = next;
	gw->tx_locked = next == UINT32_MAX;
	return true;
}

/* Spend the current counter (after the frame using it is sealed), tower-net-core TxCounter. */
static void tx_counter_advance(struct twr_gw *gw)
{
	if (gw->tx_counter < UINT32_MAX) {
		gw->tx_counter++;
	}
	if (gw->tx_counter == UINT32_MAX) {
		gw->tx_locked = true;
	}
}

static void build_ack(struct twr_gw *gw, const struct twr_hdr *rx, int rssi_dbm,
		      struct twr_rx_out *out)
{
	uint8_t pl[TWR_ACK_PAYLOAD_LEN];
	struct twr_hdr h = {
		.type = TWR_T_ACK,
		.flags = 0,
		.src = gw->gw_addr,
		.dest = rx->src,
		.counter = gw->tx_counter,
	};
	bool pending = gw->dl_queued && rx->src == gw->node_addr;

	if (gw->tx_locked) {
		return;
	}
	twr_ack_payload_encode(rx->counter, rssi_dbm, pending, pl);
	out->ack_len = twr_frame_seal(&gw->aes, &h, pl, sizeof(pl), TWR_TOWER_MAX_PAYLOAD,
				      out->ack_frame, sizeof(out->ack_frame));
	if (out->ack_len == 0) {
		return;
	}
	out->send_ack = true;
	out->ack_pending = pending;
	out->ack_ctr = h.counter;
	tx_counter_advance(gw);
}

void twr_gw_on_rx(struct twr_gw *gw, const uint8_t *frame, size_t len, int rssi_dbm,
		  struct twr_rx_out *out)
{
	size_t hlen;
	enum twr_err err;

	out->res = TWR_RX_SHORT;
	out->hdr_valid = false;
	out->pt_len = 0;
	out->send_ack = false;
	out->ack_pending = false;
	out->ack_ctr = 0;
	out->ack_len = 0;

	err = twr_hdr_parse(frame, len, &out->hdr, &hlen);
	if (err == TWR_E_VERSION) {
		out->res = TWR_RX_BAD_VER;
		return;
	}
	if (err == TWR_E_TYPE) {
		out->res = TWR_RX_BAD_TYPE;
		return;
	}
	if (err != TWR_OK) {
		return;
	}
	out->hdr_valid = true;
	if (out->hdr.dest != gw->gw_addr) {
		out->res = TWR_RX_NOT_FOR_US;
		return;
	}

	err = twr_frame_open(&gw->aes, frame, len, &out->hdr, out->pt, sizeof(out->pt),
			     &out->pt_len);
	if (err == TWR_E_MIC) {
		out->res = TWR_RX_MIC_FAIL;
		return;
	}
	if (err != TWR_OK) {
		out->res = TWR_RX_SHORT;
		return;
	}

	if (out->hdr.type == TWR_T_ACK) {
		out->res = TWR_RX_ACK;
		return;
	}
	if (out->hdr.type != TWR_T_DATA) {
		out->res = TWR_RX_OTHER;
		return;
	}

	switch (twr_replay_accept(&gw->replay, out->hdr.src, out->hdr.counter)) {
	case TWR_RP_FRESH:
		out->res = TWR_RX_OK;
		break;
	case TWR_RP_RETRANSMIT:
		out->res = TWR_RX_DUP;
		break;
	case TWR_RP_REPLAY:
	default:
		out->res = TWR_RX_REPLAY;
		out->pt_len = 0;
		return;
	}

	if ((out->hdr.flags & TWR_FLAG_CONFIRMED) != 0) {
		build_ack(gw, &out->hdr, rssi_dbm, out);
	}
}

bool twr_gw_queue_dl(struct twr_gw *gw, const uint8_t *payload, size_t len, bool confirmed)
{
	if (len > TWR_MAX_PAYLOAD) {
		return false;
	}
	if (len > 0) {
		memcpy(gw->dl, payload, len);
	}
	gw->dl_len = (uint8_t)len;
	gw->dl_confirmed = confirmed;
	gw->dl_queued = true;
	return true;
}

size_t twr_gw_build_dl(struct twr_gw *gw, uint8_t *out, size_t cap, uint32_t *ctr)
{
	struct twr_hdr h = {
		.type = TWR_T_DATA,
		.flags = gw->dl_confirmed ? TWR_FLAG_CONFIRMED : 0,
		.src = gw->gw_addr,
		.dest = gw->node_addr,
		.counter = gw->tx_counter,
	};
	size_t n;

	if (!gw->dl_queued || gw->tx_locked) {
		return 0;
	}
	n = twr_frame_seal(&gw->aes, &h, gw->dl, gw->dl_len, TWR_MAX_PAYLOAD, out, cap);
	if (n == 0) {
		return 0;
	}
	*ctr = h.counter;
	tx_counter_advance(gw);
	gw->dl_queued = false;
	gw->dl_len = 0;
	return n;
}
