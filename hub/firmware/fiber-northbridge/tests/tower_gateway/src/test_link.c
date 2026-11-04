/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for the link codec (twr_link.c): the ranges the TWR_* decoders check on their
 * own, the event encoders, and the EVT_BOOT (8 B) and GET_INFO (188 B since 0.4.0) layouts of
 * the 0.3.0 flag day, and the ENTER_BOOTLOADER body check (0.4.0). A failure here is a wire change the Hub software has to follow: the H2
 * message set is frozen, so it needs the coordinator's agreement first.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/byteorder.h>
#include <string.h>

#include "proto.h"
#include "twr_gwc.h"
#include "twr_link.h"

BUILD_ASSERT(P2P_GET_INFO_LEN == 188, "GET_INFO is 188 B (0.4.0: + app_image_len, app_crc32)");
BUILD_ASSERT(P2P_EVT_BOOT_BODY_LEN == 8, "EVT_BOOT is frozen at 8 B");
BUILD_ASSERT(P2P_INFO_CTR_COUNT == 25, "GET_INFO carries 25 counters");
BUILD_ASSERT(P2P_TWR_DL_MAX_PT == 74, "QUEUE_PUSH plaintext cap (#486 U-1, the 96 B TOWER frame)");

#define GW_ADDR 0x4E420001u

/* ---- EVT_BOOT, GET_INFO ------------------------------------------------------------------ */

ZTEST(tower_link, test_boot_layout)
{
	static const uint8_t want[P2P_EVT_BOOT_BODY_LEN] = {
		0, 3, 0, 0x92, 0x5f, 0x72, 0x2f, 0x21,
	};
	struct twr_identity id = {
		.fw_ver = { 0, 3, 0 },
		.git_sha = { 0x92, 0x5f, 0x72, 0x2f },
		.reset_cause = 0x21,
		.build_flags = P2P_BUILD_DEBUG,
	};
	uint8_t out[P2P_EVT_BOOT_BODY_LEN + 4];

	memset(out, 0xAA, sizeof(out));
	zassert_equal(twr_link_enc_boot(&id, out), P2P_EVT_BOOT_BODY_LEN);
	zassert_mem_equal(out, want, sizeof(want));
	zassert_equal(out[P2P_EVT_BOOT_BODY_LEN], 0xAA, "build_flags is GET_INFO's only");
}

ZTEST(tower_link, test_get_info_layout)
{
	struct twr_info in;
	uint8_t out[P2P_GET_INFO_LEN + 4];
	const uint8_t *p = out;

	memset(&in, 0, sizeof(in));
	in.id = (struct twr_identity){
		.fw_ver = { 0, 3, 0 },
		.git_sha = { 0x11, 0x22, 0x33, 0x44 },
		.reset_cause = 0x05,
		.build_flags = P2P_BUILD_BENCH_RTT_BRIDGE | P2P_BUILD_DEBUG,
	};
	in.now_ms = 0x0102030405060708ull;
	in.state = P2P_NB_GATEWAY;
	in.net_id = GW_ADDR;
	in.turnaround_ms = 25;
	in.dl_gap_ms = 30;
	in.modulation = P2P_MODULATION_LORA;
	in.radio = (struct radio_config){
		.frequency_hz = 869525000,
		.sf = 9,
		.bw = P2P_BW_250K,
		.cr = 2,
		.preamble_len = 0x010C,
		.tx_power_dbm = -3,
		.flags = P2P_RADIO_FLAG_PUBLIC_SYNCWORD | P2P_RADIO_FLAG_IQ_INVERTED,
	};
	in.err_flags = 0x80000021u;
	in.last_err_code = 3;
	in.last_err_age_s = 0xA1B2C3D4u;
	for (size_t i = 0; i < P2P_INFO_CTR_COUNT; i++) {
		in.ctr[i] = 0x01000000u + (uint32_t)i;
	}
	in.ack_turnaround_last_us = 0x1234;
	in.ack_turnaround_max_us = 0xFEDC;
	in.airtime_ms_last_hour = 3600000;
	in.peers = 0x0107;
	in.peers_max = 0x0100;
	in.queue_used = 0x0203;
	in.queue_max = 0x0020;
	in.evt_ring_used = 5;
	in.evt_ring_max = 6;
	in.ctr_next = 0x11223344u;
	in.ctr_last = 0x55667788u;
	in.evt_head_seq = 0xFE;
	in.ack_ctrl_sent = 0x0A0B0C0Du;
	in.ack_time_no_anchor = 0x7E7D0102u;
	in.app_image_len = 0x00018F38u;
	in.app_crc32 = 0x6B754AACu;

	memset(out, 0xAA, sizeof(out));
	zassert_equal(twr_link_enc_info(&in, out), P2P_GET_INFO_LEN);
	zassert_equal(out[P2P_GET_INFO_LEN], 0xAA, "nothing past 188 B");

	zassert_equal(p[0], 0);
	zassert_equal(p[1], 3);
	zassert_equal(p[2], 0);
	zassert_equal(p[3], 0x11, "git_sha in display order");
	zassert_equal(p[6], 0x44);
	zassert_equal(p[7], 0x05, "reset_cause");
	zassert_equal(p[8], P2P_BUILD_BENCH_RTT_BRIDGE | P2P_BUILD_DEBUG, "build_flags");
	zassert_equal(sys_get_le64(&p[9]), 0x0102030405060708ull, "now_ms");
	zassert_equal(p[17], P2P_NB_GATEWAY, "state");
	zassert_equal(sys_get_le32(&p[18]), GW_ADDR, "net_id");
	zassert_equal(p[22], 25, "turnaround_ms");
	zassert_equal(p[23], 30, "dl_gap_ms");
	zassert_equal(p[24], P2P_MODULATION_LORA, "modulation");
	zassert_equal(sys_get_le32(&p[25]), 869525000, "freq_hz");
	zassert_equal(p[29], 9, "sf");
	zassert_equal(p[30], P2P_BW_250K, "bw");
	zassert_equal(p[31], 2, "cr");
	zassert_equal(sys_get_le16(&p[32]), 0x010C, "preamble");
	zassert_equal((int8_t)p[34], -3, "tx_power_dbm");
	zassert_equal(p[35], P2P_RADIO_FLAG_PUBLIC_SYNCWORD | P2P_RADIO_FLAG_IQ_INVERTED);
	zassert_equal(sys_get_le32(&p[36]), 0x80000021u, "err_flags");
	zassert_equal(p[40], 3, "last_err_code");
	zassert_equal(sys_get_le32(&p[41]), 0xA1B2C3D4u, "last_err_age_s");
	for (size_t i = 0; i < P2P_INFO_CTR_COUNT; i++) {
		zassert_equal(sys_get_le32(&p[45 + 4 * i]), 0x01000000u + i, "counter %zu", i);
	}
	zassert_equal(sys_get_le16(&p[145]), 0x1234, "ack_turnaround_last_us");
	zassert_equal(sys_get_le16(&p[147]), 0xFEDC, "ack_turnaround_max_us");
	zassert_equal(sys_get_le32(&p[149]), 3600000, "airtime_ms_last_hour");
	zassert_equal(sys_get_le16(&p[153]), 0x0107, "peers");
	zassert_equal(sys_get_le16(&p[155]), 0x0100, "peers_max");
	zassert_equal(sys_get_le16(&p[157]), 0x0203, "queue_used");
	zassert_equal(sys_get_le16(&p[159]), 0x0020, "queue_max");
	zassert_equal(p[161], 5, "evt_ring_used");
	zassert_equal(p[162], 6, "evt_ring_max");
	zassert_equal(sys_get_le32(&p[163]), 0x11223344u, "ctr_next");
	zassert_equal(sys_get_le32(&p[167]), 0x55667788u, "ctr_last");
	zassert_equal(p[171], 0xFE, "evt_head_seq");
	zassert_equal(sys_get_le32(&p[172]), 0x0A0B0C0Du, "ack_ctrl_sent");
	zassert_equal(sys_get_le32(&p[176]), 0x7E7D0102u, "ack_time_no_anchor");
	/* 0.4.0 tail: an older host reads the first 180 B and ignores it. */
	static const uint8_t tail[8] = { 0x38, 0x8F, 0x01, 0x00, 0xAC, 0x4A, 0x75, 0x6B };

	zassert_mem_equal(&p[180], tail, sizeof(tail), "app_image_len, app_crc32 u32 LE");
}

/* ENTER_BOOTLOADER (northbridge_update.md §10): exactly 4E 42 42 4C, else BAD_PARAM. */
ZTEST(tower_link, test_dec_enter_bootloader)
{
	uint8_t b[5] = { 0x4E, 0x42, 0x42, 0x4C, 0x00 };

	zassert_equal(twr_link_dec_enter_bootloader(b, 4), P2P_STATUS_OK);
	zassert_equal(twr_link_dec_enter_bootloader(b, 5), P2P_STATUS_BAD_PARAM, "long");
	zassert_equal(twr_link_dec_enter_bootloader(b, 3), P2P_STATUS_BAD_PARAM, "short");
	zassert_equal(twr_link_dec_enter_bootloader(b, 0), P2P_STATUS_BAD_PARAM, "empty");
	for (size_t i = 0; i < 4; i++) {
		b[i] ^= 0x80;
		zassert_equal(twr_link_dec_enter_bootloader(b, 4), P2P_STATUS_BAD_PARAM, "%zu", i);
		b[i] ^= 0x80;
	}
	b[0] = 0x4C;
	b[3] = 0x4E;
	zassert_equal(twr_link_dec_enter_bootloader(b, 4), P2P_STATUS_BAD_PARAM, "byte order");
}

/* The counter indices the Hub reads by position. */
ZTEST(tower_link, test_info_counter_order)
{
	zassert_equal(P2P_INFO_RX_TOTAL, 0);
	zassert_equal(P2P_INFO_RX_BAD_HDR, 8);
	zassert_equal(P2P_INFO_RADIO_CRC_ERR, 9);
	zassert_equal(P2P_INFO_RX_REARM_FAIL, 12);
	zassert_equal(P2P_INFO_TX_TOTAL, 13);
	zassert_equal(P2P_INFO_ACKS_SENT, 15);
	zassert_equal(P2P_INFO_ACKS_SUPPRESSED, 17);
	zassert_equal(P2P_INFO_DL_SENT, 18);
	zassert_equal(P2P_INFO_DL_EXPIRED, 21);
	zassert_equal(P2P_INFO_LINK_CRC_ERR, 22);
	zassert_equal(P2P_INFO_EVT_RESENT, 24);
}

/* ---- SET_RADIO_CONFIG --------------------------------------------------------------------- */

static void mk_radio(uint8_t *b, uint8_t sf, uint8_t bw, uint8_t cr)
{
	sys_put_le32(868100000, &b[0]);
	b[4] = sf;
	b[5] = bw;
	b[6] = cr;
	sys_put_le16(8, &b[7]);
	b[9] = (uint8_t)(int8_t)-9;
	b[10] = P2P_RADIO_FLAG_PUBLIC_SYNCWORD;
}

ZTEST(tower_link, test_radio_config)
{
	uint8_t b[P2P_RADIO_CONFIG_BODY_LEN];
	uint8_t back[P2P_RADIO_CONFIG_BODY_LEN];
	struct radio_config c;

	mk_radio(b, 7, P2P_BW_500K, 4);
	zassert_equal(twr_link_dec_radio_config(b, sizeof(b), &c), P2P_STATUS_OK);
	zassert_equal(c.frequency_hz, 868100000);
	zassert_equal(c.tx_power_dbm, -9);
	zassert_equal(twr_link_enc_radio_config(&c, back), P2P_RADIO_CONFIG_BODY_LEN);
	zassert_mem_equal(back, b, sizeof(b), "GET_INFO echoes the body");

	mk_radio(b, 12, P2P_BW_125K, 1);
	zassert_equal(twr_link_dec_radio_config(b, sizeof(b), &c), P2P_STATUS_OK);
	zassert_equal(twr_link_dec_radio_config(b, sizeof(b) - 1, &c), P2P_STATUS_BAD_PARAM);
	mk_radio(b, 6, P2P_BW_125K, 1);
	zassert_equal(twr_link_dec_radio_config(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);
	mk_radio(b, 13, P2P_BW_125K, 1);
	zassert_equal(twr_link_dec_radio_config(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);
	mk_radio(b, 7, 3, 1);
	zassert_equal(twr_link_dec_radio_config(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);
	mk_radio(b, 7, P2P_BW_125K, 0);
	zassert_equal(twr_link_dec_radio_config(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);
	mk_radio(b, 7, P2P_BW_125K, 5);
	zassert_equal(twr_link_dec_radio_config(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);
}

/* ---- TWR_* decoders ------------------------------------------------------------------------ */

static void mk_start(uint8_t *b, uint32_t net_id, uint8_t ta, uint8_t gap, uint8_t flags)
{
	sys_put_le32(net_id, &b[0]);
	b[4] = ta;
	b[5] = gap;
	b[6] = flags;
}

ZTEST(tower_link, test_dec_start)
{
	uint8_t b[P2P_TWR_START_BODY_LEN + 1];
	struct twr_start_cmd c;

	mk_start(b, GW_ADDR, 20, 60, P2P_TWR_START_ACK_ENABLED);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN, &c), P2P_STATUS_OK);
	zassert_equal(c.net_id, GW_ADDR);
	zassert_equal(c.turnaround_ms, 20);
	zassert_equal(c.dl_gap_ms, 60);
	zassert_equal(c.flags, P2P_TWR_START_ACK_ENABLED);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN - 1, &c), P2P_STATUS_BAD_PARAM);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN + 1, &c), P2P_STATUS_BAD_PARAM);

	mk_start(b, 0, 20, 20, 0);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN, &c), P2P_STATUS_BAD_PARAM);
	mk_start(b, UINT32_MAX, 20, 20, 0);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN, &c), P2P_STATUS_BAD_PARAM);
	mk_start(b, GW_ADDR, 19, 20, 0);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN, &c), P2P_STATUS_BAD_PARAM);
	mk_start(b, GW_ADDR, 61, 20, 0);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN, &c), P2P_STATUS_BAD_PARAM);
	mk_start(b, GW_ADDR, 20, 19, 0);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN, &c), P2P_STATUS_BAD_PARAM);
	mk_start(b, GW_ADDR, 20, 61, 0);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN, &c), P2P_STATUS_BAD_PARAM);
	mk_start(b, GW_ADDR, 20, 20, 0x02);
	zassert_equal(twr_link_dec_start(b, P2P_TWR_START_BODY_LEN, &c), P2P_STATUS_BAD_PARAM,
		      "undefined flag bits");
}

ZTEST(tower_link, test_dec_node_add_remove)
{
	uint8_t b[P2P_TWR_NODE_ADD_BODY_LEN];
	struct twr_node_add_cmd c;
	uint32_t addr;

	sys_put_le32(0x413, &b[0]);
	for (int i = 0; i < TWR_AES_KEY_LEN; i++) {
		b[4 + i] = (uint8_t)(0xC0 + i);
	}
	sys_put_le32(77, &b[20]);
	b[24] = P2P_TWR_NODE_HOME;
	zassert_equal(twr_link_dec_node_add(b, sizeof(b), &c), P2P_STATUS_OK);
	zassert_equal(c.addr, 0x413);
	zassert_mem_equal(c.key, &b[4], TWR_AES_KEY_LEN);
	zassert_equal(c.last_seen, 77);
	zassert_equal(c.flags, P2P_TWR_NODE_HOME);
	zassert_equal(twr_link_dec_node_add(b, sizeof(b) - 1, &c), P2P_STATUS_BAD_PARAM);
	b[24] = P2P_TWR_NODE_HOME | P2P_TWR_NODE_KEEP_NEWER;
	zassert_equal(twr_link_dec_node_add(b, sizeof(b), &c), P2P_STATUS_OK, "KEEP_NEWER");
	zassert_equal(c.flags, P2P_TWR_NODE_HOME | P2P_TWR_NODE_KEEP_NEWER);
	for (int bit = 2; bit < 8; bit++) {
		b[24] = (uint8_t)(1u << bit);
		zassert_equal(twr_link_dec_node_add(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM,
			      "undefined flag bit %d", bit);
	}
	b[24] = 0;
	zassert_equal(twr_link_dec_node_add(b, sizeof(b), &c), P2P_STATUS_OK, "a visitor");
	sys_put_le32(0, &b[0]);
	zassert_equal(twr_link_dec_node_add(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);
	sys_put_le32(UINT32_MAX, &b[0]);
	zassert_equal(twr_link_dec_node_add(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);

	sys_put_le32(0x413, &b[0]);
	zassert_equal(twr_link_dec_node_remove(b, P2P_TWR_NODE_REMOVE_BODY_LEN, &addr),
		      P2P_STATUS_OK);
	zassert_equal(addr, 0x413);
	zassert_equal(twr_link_dec_node_remove(b, 3, &addr), P2P_STATUS_BAD_PARAM);
	sys_put_le32(0, &b[0]);
	zassert_equal(twr_link_dec_node_remove(b, 4, &addr), P2P_STATUS_BAD_PARAM);
}

ZTEST(tower_link, test_dec_ctr_block)
{
	uint8_t b[P2P_TWR_CTR_BLOCK_BODY_LEN];
	struct twr_ctr_block_cmd c;

	sys_put_le32(1, &b[0]);
	sys_put_le32(1, &b[4]);
	zassert_equal(twr_link_dec_ctr_block(b, sizeof(b), &c), P2P_STATUS_OK, "one counter");
	sys_put_le32(UINT32_MAX - 1, &b[4]);
	zassert_equal(twr_link_dec_ctr_block(b, sizeof(b), &c), P2P_STATUS_OK);
	zassert_equal(c.first, 1);
	zassert_equal(c.last, UINT32_MAX - 1);
	zassert_equal(twr_link_dec_ctr_block(b, sizeof(b) - 1, &c), P2P_STATUS_BAD_PARAM);
	sys_put_le32(UINT32_MAX, &b[4]);
	zassert_equal(twr_link_dec_ctr_block(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);
	sys_put_le32(0, &b[0]);
	sys_put_le32(10, &b[4]);
	zassert_equal(twr_link_dec_ctr_block(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM);
	sys_put_le32(11, &b[0]);
	zassert_equal(twr_link_dec_ctr_block(b, sizeof(b), &c), P2P_STATUS_BAD_PARAM,
		      "first > last");
}

static size_t mk_push(uint8_t *b, uint32_t addr, uint16_t item, uint16_t ttl, uint8_t flags,
		      uint8_t len)
{
	sys_put_le32(addr, &b[0]);
	sys_put_le16(item, &b[4]);
	sys_put_le16(ttl, &b[6]);
	b[8] = flags;
	b[9] = len;
	for (int i = 0; i < len; i++) {
		b[P2P_TWR_QUEUE_PUSH_HDR_LEN + i] = (uint8_t)i;
	}
	return P2P_TWR_QUEUE_PUSH_HDR_LEN + len;
}

ZTEST(tower_link, test_dec_queue_push_drop_list)
{
	uint8_t b[P2P_TWR_QUEUE_PUSH_HDR_LEN + 80];
	struct twr_queue_push_cmd c;
	struct twr_queue_drop_cmd d;
	uint16_t start;
	size_t n;

	n = mk_push(b, 0x413, 7, 300, P2P_TWR_QUEUE_CONFIRMED, P2P_TWR_DL_MAX_PT);
	zassert_equal(twr_link_dec_queue_push(b, n, &c), P2P_STATUS_OK, "74 B fits");
	zassert_equal(c.addr, 0x413);
	zassert_equal(c.item, 7);
	zassert_equal(c.ttl_s, 300);
	zassert_equal(c.flags, P2P_TWR_QUEUE_CONFIRMED);
	zassert_equal(c.len, P2P_TWR_DL_MAX_PT);
	zassert_equal_ptr(c.pt, &b[P2P_TWR_QUEUE_PUSH_HDR_LEN], "no copy");
	zassert_equal(twr_link_dec_queue_push(b, n - 1, &c), P2P_STATUS_BAD_PARAM, "n != 10+len");
	zassert_equal(twr_link_dec_queue_push(b, 9, &c), P2P_STATUS_BAD_PARAM);

	n = mk_push(b, 0x413, 7, 300, 0, P2P_TWR_DL_MAX_PT + 1);
	zassert_equal(twr_link_dec_queue_push(b, n, &c), P2P_STATUS_BAD_PARAM, "79 B");
	n = mk_push(b, 0x413, 7, 300, 0, 0);
	zassert_equal(twr_link_dec_queue_push(b, n, &c), P2P_STATUS_BAD_PARAM, "len 0");
	n = mk_push(b, 0x413, 0, 300, 0, 1);
	zassert_equal(twr_link_dec_queue_push(b, n, &c), P2P_STATUS_BAD_PARAM, "item 0");
	n = mk_push(b, 0x413, 7, 0, 0, 1);
	zassert_equal(twr_link_dec_queue_push(b, n, &c), P2P_STATUS_BAD_PARAM, "ttl 0");
	n = mk_push(b, 0x413, 7, 1, 0x02, 1);
	zassert_equal(twr_link_dec_queue_push(b, n, &c), P2P_STATUS_BAD_PARAM, "flags");
	n = mk_push(b, 0, 7, 1, 0, 1);
	zassert_equal(twr_link_dec_queue_push(b, n, &c), P2P_STATUS_BAD_PARAM, "addr 0");
	n = mk_push(b, 0x413, 0xFFFF, 0xFFFF, 0, 1);
	zassert_equal(twr_link_dec_queue_push(b, n, &c), P2P_STATUS_OK);

	sys_put_le32(0x413, &b[0]);
	sys_put_le16(0, &b[4]);
	zassert_equal(twr_link_dec_queue_drop(b, P2P_TWR_QUEUE_DROP_BODY_LEN, &d), P2P_STATUS_OK,
		      "item 0 = all");
	zassert_equal(d.addr, 0x413);
	zassert_equal(d.item, 0);
	zassert_equal(twr_link_dec_queue_drop(b, 5, &d), P2P_STATUS_BAD_PARAM);
	sys_put_le32(UINT32_MAX, &b[0]);
	zassert_equal(twr_link_dec_queue_drop(b, 6, &d), P2P_STATUS_BAD_PARAM);

	sys_put_le16(24, &b[0]);
	zassert_equal(twr_link_dec_node_list(b, P2P_TWR_NODE_LIST_BODY_LEN, &start),
		      P2P_STATUS_OK);
	zassert_equal(start, 24);
	zassert_equal(twr_link_dec_node_list(b, 0, &start), P2P_STATUS_BAD_PARAM);
	zassert_equal(twr_link_dec_node_list(b, 3, &start), P2P_STATUS_BAD_PARAM);
}

ZTEST(tower_link, test_dec_time_sync)
{
	uint8_t b[P2P_TWR_TIME_SYNC_BODY_LEN + 1];
	uint64_t now_ms;
	uint64_t unix_ms;

	sys_put_le64(0x0102030405060708ull, &b[0]);
	sys_put_le64(1790000000123ull, &b[8]);
	zassert_equal(twr_link_dec_time_sync(b, P2P_TWR_TIME_SYNC_BODY_LEN, &now_ms, &unix_ms),
		      P2P_STATUS_OK);
	zassert_equal(now_ms, 0x0102030405060708ull);
	zassert_equal(unix_ms, 1790000000123ull);
	zassert_equal(twr_link_dec_time_sync(b, 15, &now_ms, &unix_ms), P2P_STATUS_BAD_PARAM);
	zassert_equal(twr_link_dec_time_sync(b, 17, &now_ms, &unix_ms), P2P_STATUS_BAD_PARAM);
	zassert_equal(twr_link_dec_time_sync(b, 0, &now_ms, &unix_ms), P2P_STATUS_BAD_PARAM);
}

/* ---- event encoders ------------------------------------------------------------------------ */

ZTEST(tower_link, test_enc_uplink)
{
	static const uint8_t pt[] = { 0x81, 0x7E, 0x7D };
	struct twr_uplink_evt e = {
		.t_ms = 0x0000001122334455ull,
		.rssi_dbm = -117,
		.snr_q_db = -30,
		.frame_flags = 0x01,
		.addr = 0x413,
		.counter = 0xDEADBEEF,
		.ack = P2P_TWR_ACK_PENDING,
		.len = sizeof(pt),
		.pt = pt,
	};
	uint8_t out[P2P_EVT_TWR_UPLINK_HDR_LEN + sizeof(pt)];

	zassert_equal(twr_link_enc_uplink(&e, out, sizeof(out)), sizeof(out));
	zassert_equal(sys_get_le64(&out[0]), 0x0000001122334455ull);
	zassert_equal((int16_t)sys_get_le16(&out[8]), -117);
	zassert_equal((int8_t)out[10], -30);
	zassert_equal(out[11], 0x01);
	zassert_equal(sys_get_le32(&out[12]), 0x413);
	zassert_equal(sys_get_le32(&out[16]), 0xDEADBEEF);
	zassert_equal(out[20], P2P_TWR_ACK_PENDING);
	zassert_equal(out[21], sizeof(pt));
	zassert_mem_equal(&out[22], pt, sizeof(pt));
	zassert_equal(twr_link_enc_uplink(&e, out, sizeof(out) - 1), 0, "does not fit");
}

ZTEST(tower_link, test_enc_tx_ctr_low_rx_tx_done)
{
	struct twr_tx_evt e = {
		.addr = 0x413,
		.item = 0xA55A,
		.outcome = P2P_TWR_TX_DELIVERED,
		.gw_counter = 0x01020304,
		.node_ack_counter = 0x0A0B0C0D,
		.ack_rssi = -96,
	};
	static const uint8_t frame[] = { 0x10, 0x20, 0x30 };
	uint8_t out[P2P_EVT_RX_HDR_LEN + P2P_MAX_RADIO_FRAME_LEN + 1];

	zassert_equal(twr_link_enc_tx(&e, out), P2P_EVT_TWR_TX_BODY_LEN);
	zassert_equal(sys_get_le32(&out[0]), 0x413);
	zassert_equal(sys_get_le16(&out[4]), 0xA55A);
	zassert_equal(out[6], P2P_TWR_TX_DELIVERED);
	zassert_equal(sys_get_le32(&out[7]), 0x01020304);
	zassert_equal(sys_get_le32(&out[11]), 0x0A0B0C0D, "node_ack_counter");
	zassert_equal((int8_t)out[15], -96);

	zassert_equal(twr_link_enc_ctr_low(1000, 4999, out), P2P_EVT_TWR_CTR_LOW_BODY_LEN);
	zassert_equal(sys_get_le32(&out[0]), 1000);
	zassert_equal(sys_get_le32(&out[4]), 4999);

	zassert_equal(twr_link_enc_rx(0x123456789Aull, -80, 22, frame, sizeof(frame), out,
				      sizeof(out)),
		      P2P_EVT_RX_HDR_LEN + sizeof(frame));
	zassert_equal(sys_get_le64(&out[0]), 0x123456789Aull);
	zassert_equal((int16_t)sys_get_le16(&out[8]), -80);
	zassert_equal(out[10], 22);
	zassert_equal(out[11], 0, "reserved");
	zassert_equal(out[12], sizeof(frame));
	zassert_mem_equal(&out[13], frame, sizeof(frame));
	zassert_equal(twr_link_enc_rx(0, 0, 0, frame, sizeof(frame), out, 15), 0);
	zassert_equal(twr_link_enc_rx(0, 0, 0, out, P2P_MAX_RADIO_FRAME_LEN + 1, out, sizeof(out)),
		      0);

	zassert_equal(twr_link_enc_tx_done(9, P2P_TX_DONE_LATE, 0x0102030405ull, out),
		      P2P_EVT_TX_DONE_BODY_LEN);
	zassert_equal(out[0], 9);
	zassert_equal(out[1], P2P_TX_DONE_LATE);
	zassert_equal(sys_get_le64(&out[2]), 0x0102030405ull);
}

ZTEST(tower_link, test_snr_quarter_db)
{
	zassert_equal(twr_link_snr_q(5), 20);
	zassert_equal(twr_link_snr_q(-8), -32);
	zassert_equal(twr_link_snr_q(31), 124);
	zassert_equal(twr_link_snr_q(32), 127, "clamped");
	zassert_equal(twr_link_snr_q(-33), -128, "clamped");
}

/* `p2p rx_inject` (bench shell) frame parsing. */
ZTEST(tower_link, test_hex_decode)
{
	uint8_t out[4];

	zassert_equal(twr_link_hex_decode("00a1FF7e", out, sizeof(out)), 4);
	zassert_equal(out[0], 0x00);
	zassert_equal(out[1], 0xA1);
	zassert_equal(out[2], 0xFF);
	zassert_equal(out[3], 0x7E);
	zassert_equal(twr_link_hex_decode("", out, sizeof(out)), -1, "empty");
	zassert_equal(twr_link_hex_decode("abc", out, sizeof(out)), -1, "odd length");
	zassert_equal(twr_link_hex_decode("0g", out, sizeof(out)), -1, "not hex");
	zassert_equal(twr_link_hex_decode("0011223344", out, sizeof(out)), -1, "over cap");
}

ZTEST_SUITE(tower_link, NULL, NULL, NULL, NULL, NULL);
