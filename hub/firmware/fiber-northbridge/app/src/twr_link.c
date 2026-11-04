/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway link codec, see twr_link.h.
 */
#include "twr_link.h"
#include "twr_gwc.h"

#include <string.h>

static uint16_t get_le16(const uint8_t *p)
{
	return (uint16_t)(p[0] | ((uint16_t)p[1] << 8));
}

static uint32_t get_le32(const uint8_t *p)
{
	return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
	       ((uint32_t)p[3] << 24);
}

static void put_le16(uint8_t *p, uint16_t v)
{
	p[0] = (uint8_t)v;
	p[1] = (uint8_t)(v >> 8);
}

static void put_le32(uint8_t *p, uint32_t v)
{
	p[0] = (uint8_t)v;
	p[1] = (uint8_t)(v >> 8);
	p[2] = (uint8_t)(v >> 16);
	p[3] = (uint8_t)(v >> 24);
}

static void put_le64(uint8_t *p, uint64_t v)
{
	put_le32(p, (uint32_t)v);
	put_le32(&p[4], (uint32_t)(v >> 32));
}

/* ---- identity, EVT_BOOT, GET_INFO ------------------------------------------------------ */

size_t twr_link_enc_boot(const struct twr_identity *id, uint8_t out[P2P_EVT_BOOT_BODY_LEN])
{
	memcpy(&out[0], id->fw_ver, 3);
	memcpy(&out[3], id->git_sha, 4);
	out[7] = id->reset_cause;
	return P2P_EVT_BOOT_BODY_LEN;
}

size_t twr_link_enc_info(const struct twr_info *in, uint8_t out[P2P_GET_INFO_LEN])
{
	size_t n = 0;

	memcpy(&out[n], in->id.fw_ver, 3);
	n += 3;
	memcpy(&out[n], in->id.git_sha, 4);
	n += 4;
	out[n++] = in->id.reset_cause;
	out[n++] = in->id.build_flags;

	put_le64(&out[n], in->now_ms);
	n += 8;

	out[n++] = in->state;
	put_le32(&out[n], in->net_id);
	n += 4;
	out[n++] = in->turnaround_ms;
	out[n++] = in->dl_gap_ms;

	out[n++] = in->modulation;
	n += twr_link_enc_radio_config(&in->radio, &out[n]);

	put_le32(&out[n], in->err_flags);
	n += 4;
	out[n++] = in->last_err_code;
	put_le32(&out[n], in->last_err_age_s);
	n += 4;

	for (size_t i = 0; i < P2P_INFO_CTR_COUNT; i++) {
		put_le32(&out[n], in->ctr[i]);
		n += 4;
	}

	put_le16(&out[n], in->ack_turnaround_last_us);
	n += 2;
	put_le16(&out[n], in->ack_turnaround_max_us);
	n += 2;
	put_le32(&out[n], in->airtime_ms_last_hour);
	n += 4;

	put_le16(&out[n], in->peers);
	n += 2;
	put_le16(&out[n], in->peers_max);
	n += 2;
	put_le16(&out[n], in->queue_used);
	n += 2;
	put_le16(&out[n], in->queue_max);
	n += 2;
	out[n++] = in->evt_ring_used;
	out[n++] = in->evt_ring_max;
	put_le32(&out[n], in->ctr_next);
	n += 4;
	put_le32(&out[n], in->ctr_last);
	n += 4;
	out[n++] = in->evt_head_seq;
	put_le32(&out[n], in->ack_ctrl_sent);
	n += 4;
	put_le32(&out[n], in->ack_time_no_anchor);
	n += 4;
	put_le32(&out[n], in->app_image_len);
	n += 4;
	put_le32(&out[n], in->app_crc32);
	n += 4;
	return n;
}

/* ---- modem messages kept from proto 1 --------------------------------------------------- */

enum p2p_status twr_link_dec_radio_config(const uint8_t *b, size_t n, struct radio_config *out)
{
	if (n != P2P_RADIO_CONFIG_BODY_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	out->frequency_hz = get_le32(&b[0]);
	out->sf = b[4];
	out->bw = b[5];
	out->cr = b[6];
	out->preamble_len = get_le16(&b[7]);
	out->tx_power_dbm = (int8_t)b[9];
	out->flags = b[10];
	if (out->sf < 7 || out->sf > 12 || out->bw > P2P_BW_500K || out->cr < 1 || out->cr > 4) {
		return P2P_STATUS_BAD_PARAM;
	}
	return P2P_STATUS_OK;
}

size_t twr_link_enc_radio_config(const struct radio_config *c,
				 uint8_t out[P2P_RADIO_CONFIG_BODY_LEN])
{
	put_le32(&out[0], c->frequency_hz);
	out[4] = c->sf;
	out[5] = c->bw;
	out[6] = c->cr;
	put_le16(&out[7], c->preamble_len);
	out[9] = (uint8_t)c->tx_power_dbm;
	out[10] = c->flags;
	return P2P_RADIO_CONFIG_BODY_LEN;
}

size_t twr_link_enc_rx(uint64_t t_ms, int16_t rssi_dbm, int8_t snr_q_db, const uint8_t *frame,
		       size_t len, uint8_t *out, size_t cap)
{
	size_t n = P2P_EVT_RX_HDR_LEN + len;

	if (len > P2P_MAX_RADIO_FRAME_LEN || n > cap) {
		return 0;
	}
	put_le64(&out[0], t_ms);
	put_le16(&out[8], (uint16_t)rssi_dbm);
	out[10] = (uint8_t)snr_q_db;
	out[11] = 0; /* reserved */
	out[12] = (uint8_t)len;
	if (len > 0) {
		memcpy(&out[P2P_EVT_RX_HDR_LEN], frame, len);
	}
	return n;
}

size_t twr_link_enc_tx_done(uint8_t tx_id, uint8_t status, uint64_t t_actual_ms,
			    uint8_t out[P2P_EVT_TX_DONE_BODY_LEN])
{
	out[0] = tx_id;
	out[1] = status;
	put_le64(&out[2], t_actual_ms);
	return P2P_EVT_TX_DONE_BODY_LEN;
}

enum p2p_status twr_link_dec_enter_bootloader(const uint8_t *b, size_t n)
{
	if (n != P2P_ENTER_BOOTLOADER_BODY_LEN || get_le32(b) != P2P_ENTER_BOOTLOADER_MAGIC) {
		return P2P_STATUS_BAD_PARAM;
	}
	return P2P_STATUS_OK;
}

/* ---- TWR_* ------------------------------------------------------------------------------ */

/* 0 and 0xFFFFFFFF are reserved TOWER addresses (#470 §4). */
static bool addr_ok(uint32_t a)
{
	return a != 0 && a != UINT32_MAX;
}

static bool timing_ok(uint8_t ms)
{
	return ms >= P2P_TWR_TIMING_MIN_MS && ms <= P2P_TWR_TIMING_MAX_MS;
}

enum p2p_status twr_link_dec_start(const uint8_t *b, size_t n, struct twr_start_cmd *out)
{
	if (n != P2P_TWR_START_BODY_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	out->net_id = get_le32(&b[0]);
	out->turnaround_ms = b[4];
	out->dl_gap_ms = b[5];
	out->flags = b[6];
	if (!addr_ok(out->net_id) || !timing_ok(out->turnaround_ms) ||
	    !timing_ok(out->dl_gap_ms) || (out->flags & ~P2P_TWR_START_ACK_ENABLED) != 0) {
		return P2P_STATUS_BAD_PARAM;
	}
	return P2P_STATUS_OK;
}

enum p2p_status twr_link_dec_node_add(const uint8_t *b, size_t n, struct twr_node_add_cmd *out)
{
	if (n != P2P_TWR_NODE_ADD_BODY_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	out->addr = get_le32(&b[0]);
	memcpy(out->key, &b[4], TWR_AES_KEY_LEN);
	out->last_seen = get_le32(&b[20]);
	out->flags = b[24];
	if (!addr_ok(out->addr) ||
	    (out->flags & ~(P2P_TWR_NODE_HOME | P2P_TWR_NODE_KEEP_NEWER)) != 0) {
		return P2P_STATUS_BAD_PARAM;
	}
	return P2P_STATUS_OK;
}

enum p2p_status twr_link_dec_node_remove(const uint8_t *b, size_t n, uint32_t *addr)
{
	if (n != P2P_TWR_NODE_REMOVE_BODY_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	*addr = get_le32(b);
	return addr_ok(*addr) ? P2P_STATUS_OK : P2P_STATUS_BAD_PARAM;
}

enum p2p_status twr_link_dec_ctr_block(const uint8_t *b, size_t n,
				       struct twr_ctr_block_cmd *out)
{
	if (n != P2P_TWR_CTR_BLOCK_BODY_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	out->first = get_le32(&b[0]);
	out->last = get_le32(&b[4]);
	if (out->first == 0 || out->first > out->last || out->last == UINT32_MAX) {
		return P2P_STATUS_BAD_PARAM;
	}
	return P2P_STATUS_OK;
}

enum p2p_status twr_link_dec_queue_push(const uint8_t *b, size_t n,
					struct twr_queue_push_cmd *out)
{
	if (n < P2P_TWR_QUEUE_PUSH_HDR_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	out->addr = get_le32(&b[0]);
	out->item = get_le16(&b[4]);
	out->ttl_s = get_le16(&b[6]);
	out->flags = b[8];
	out->len = b[9];
	out->pt = &b[P2P_TWR_QUEUE_PUSH_HDR_LEN];
	if (n != (size_t)P2P_TWR_QUEUE_PUSH_HDR_LEN + out->len) {
		return P2P_STATUS_BAD_PARAM;
	}
	if (!addr_ok(out->addr) || out->item == 0 || out->ttl_s == 0 ||
	    (out->flags & ~P2P_TWR_QUEUE_CONFIRMED) != 0 || out->len == 0 ||
	    out->len > P2P_TWR_DL_MAX_PT) {
		return P2P_STATUS_BAD_PARAM;
	}
	return P2P_STATUS_OK;
}

enum p2p_status twr_link_dec_queue_drop(const uint8_t *b, size_t n,
					struct twr_queue_drop_cmd *out)
{
	if (n != P2P_TWR_QUEUE_DROP_BODY_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	out->addr = get_le32(&b[0]);
	out->item = get_le16(&b[4]);
	return addr_ok(out->addr) ? P2P_STATUS_OK : P2P_STATUS_BAD_PARAM;
}

enum p2p_status twr_link_dec_node_list(const uint8_t *b, size_t n, uint16_t *start)
{
	if (n != P2P_TWR_NODE_LIST_BODY_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	*start = get_le16(b);
	return P2P_STATUS_OK;
}

enum p2p_status twr_link_dec_time_sync(const uint8_t *b, size_t n, uint64_t *now_ms,
				       uint64_t *unix_ms)
{
	if (n != P2P_TWR_TIME_SYNC_BODY_LEN) {
		return P2P_STATUS_BAD_PARAM;
	}
	*now_ms = (uint64_t)get_le32(b) | ((uint64_t)get_le32(&b[4]) << 32);
	*unix_ms = (uint64_t)get_le32(&b[8]) | ((uint64_t)get_le32(&b[12]) << 32);
	return P2P_STATUS_OK;
}

size_t twr_link_enc_uplink(const struct twr_uplink_evt *e, uint8_t *out, size_t cap)
{
	size_t n = P2P_EVT_TWR_UPLINK_HDR_LEN + e->len;

	if (n > cap) {
		return 0;
	}
	put_le64(&out[0], e->t_ms);
	put_le16(&out[8], (uint16_t)e->rssi_dbm);
	out[10] = (uint8_t)e->snr_q_db;
	out[11] = e->frame_flags;
	put_le32(&out[12], e->addr);
	put_le32(&out[16], e->counter);
	out[20] = e->ack;
	out[21] = e->len;
	if (e->len > 0) {
		memcpy(&out[P2P_EVT_TWR_UPLINK_HDR_LEN], e->pt, e->len);
	}
	return n;
}

size_t twr_link_enc_tx(const struct twr_tx_evt *e, uint8_t out[P2P_EVT_TWR_TX_BODY_LEN])
{
	put_le32(&out[0], e->addr);
	put_le16(&out[4], e->item);
	out[6] = e->outcome;
	put_le32(&out[7], e->gw_counter);
	put_le32(&out[11], e->node_ack_counter);
	out[15] = (uint8_t)e->ack_rssi;
	return P2P_EVT_TWR_TX_BODY_LEN;
}

size_t twr_link_enc_ctr_low(uint32_t next, uint32_t last,
			    uint8_t out[P2P_EVT_TWR_CTR_LOW_BODY_LEN])
{
	put_le32(&out[0], next);
	put_le32(&out[4], last);
	return P2P_EVT_TWR_CTR_LOW_BODY_LEN;
}

static int hex_nibble(char c)
{
	if (c >= '0' && c <= '9') {
		return c - '0';
	}
	if (c >= 'a' && c <= 'f') {
		return c - 'a' + 10;
	}
	if (c >= 'A' && c <= 'F') {
		return c - 'A' + 10;
	}
	return -1;
}

int twr_link_hex_decode(const char *hex, uint8_t *out, size_t cap)
{
	size_t len = strlen(hex);
	size_t n = len / 2;

	if (len == 0 || (len % 2) != 0 || n > cap) {
		return -1;
	}
	for (size_t i = 0; i < n; i++) {
		int hi = hex_nibble(hex[2 * i]);
		int lo = hex_nibble(hex[2 * i + 1]);

		if (hi < 0 || lo < 0) {
			return -1;
		}
		out[i] = (uint8_t)((hi << 4) | lo);
	}
	return (int)n;
}

int8_t twr_link_snr_q(int8_t snr_db)
{
	int32_t q = (int32_t)snr_db * 4;

	if (q > INT8_MAX) {
		q = INT8_MAX;
	} else if (q < INT8_MIN) {
		q = INT8_MIN;
	}
	return (int8_t)q;
}
