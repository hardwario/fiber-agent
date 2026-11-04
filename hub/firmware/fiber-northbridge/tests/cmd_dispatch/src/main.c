/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for the gateway image's command path: proto_on_frame() in cmd_dispatch.c and
 * the TWR_* commands in twr_cmd.c, over the real link codec, core and diagnostics, with the
 * radio runtime faked (gw_fakes.c). Pins what the Hub sees: the RSP status of every command
 * in every Northbridge state (BOOT -> CONFIGURED -> GATEWAY), the retired commands answering
 * UNSUPPORTED, ENTER_BOOTLOADER, and the 188 B GET_INFO filled from the right sources.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/byteorder.h>
#include <zephyr/sys/util.h>
#include <string.h>

#include "cmd_dispatch.h"
#include "diag.h"
#include "gw_fakes.h"
#include "proto.h"
#include "twr_evq.h"
#include "twr_link.h"

#define GW_ADDR   0x4E420001u
#define NODE_ADDR 0x00000413u

static const uint8_t node_key[TWR_AES_KEY_LEN] = {
	0x5a, 0x11, 0xc3, 0x07, 0x9e, 0x42, 0xd8, 0x6b,
	0x21, 0xf0, 0x3c, 0x95, 0x7a, 0x0e, 0xb4, 0x68,
};

static uint8_t seq;

static void reset(void *fixture)
{
	ARG_UNUSED(fixture);
	gw_fakes_reset();
}

ZTEST_SUITE(cmd_dispatch, NULL, NULL, reset, NULL, NULL);

/* One command, exactly one RSP 0x40|type with the seq echoed and the core's lock released;
 * returns the status byte. */
static uint8_t cmd(uint8_t type, const uint8_t *body, size_t len)
{
	int before = fake_rsp_count;

	seq++;
	proto_on_frame(type, seq, body, len);
	zassert_equal(fake_rsp_count, before + 1, "type 0x%02x: one RSP", type);
	zassert_equal(fake_rsp.type, P2P_RSP(type));
	zassert_equal(fake_rsp.seq, seq);
	zassert_true(fake_rsp.len >= 1);
	zassert_equal(fake_lock_depth, 0, "lock balanced");
	return fake_rsp.body[0];
}

/* ---- command bodies ------------------------------------------------------------------------ */

static size_t radio_body(uint8_t *b, uint8_t sf, uint8_t bw, uint8_t cr)
{
	sys_put_le32(869525000, &b[0]);
	b[4] = sf;
	b[5] = bw;
	b[6] = cr;
	sys_put_le16(8, &b[7]);
	b[9] = 14;
	b[10] = P2P_RADIO_FLAG_PUBLIC_SYNCWORD;
	return P2P_RADIO_CONFIG_BODY_LEN;
}

static uint8_t set_radio(void)
{
	uint8_t b[P2P_RADIO_CONFIG_BODY_LEN];

	return cmd(P2P_CMD_SET_RADIO_CONFIG, b, radio_body(b, 9, P2P_BW_125K, 1));
}

static uint8_t start(uint32_t net_id, uint8_t ta, uint8_t gap, uint8_t flags)
{
	uint8_t b[P2P_TWR_START_BODY_LEN];

	sys_put_le32(net_id, &b[0]);
	b[4] = ta;
	b[5] = gap;
	b[6] = flags;
	return cmd(P2P_CMD_TWR_START, b, sizeof(b));
}

static uint8_t node_add(uint32_t addr, uint32_t last_seen, uint8_t flags)
{
	uint8_t b[P2P_TWR_NODE_ADD_BODY_LEN];

	sys_put_le32(addr, &b[0]);
	memcpy(&b[4], node_key, sizeof(node_key));
	sys_put_le32(last_seen, &b[20]);
	b[24] = flags;
	return cmd(P2P_CMD_TWR_NODE_ADD, b, sizeof(b));
}

static uint8_t node_remove(uint32_t addr)
{
	uint8_t b[P2P_TWR_NODE_REMOVE_BODY_LEN];

	sys_put_le32(addr, b);
	return cmd(P2P_CMD_TWR_NODE_REMOVE, b, sizeof(b));
}

static uint8_t ctr_block(uint32_t first, uint32_t last)
{
	uint8_t b[P2P_TWR_CTR_BLOCK_BODY_LEN];

	sys_put_le32(first, &b[0]);
	sys_put_le32(last, &b[4]);
	return cmd(P2P_CMD_TWR_CTR_BLOCK, b, sizeof(b));
}

static uint8_t push(uint32_t addr, uint16_t item, uint16_t ttl_s, uint8_t flags, uint8_t len)
{
	uint8_t b[P2P_TWR_QUEUE_PUSH_HDR_LEN + P2P_TWR_DL_MAX_PT + 1];

	zassert_true(len <= P2P_TWR_DL_MAX_PT + 1);
	sys_put_le32(addr, &b[0]);
	sys_put_le16(item, &b[4]);
	sys_put_le16(ttl_s, &b[6]);
	b[8] = flags;
	b[9] = len;
	memset(&b[P2P_TWR_QUEUE_PUSH_HDR_LEN], 0xC5, len);
	return cmd(P2P_CMD_TWR_QUEUE_PUSH, b, P2P_TWR_QUEUE_PUSH_HDR_LEN + len);
}

static uint8_t drop(uint32_t addr, uint16_t item)
{
	uint8_t b[P2P_TWR_QUEUE_DROP_BODY_LEN];

	sys_put_le32(addr, &b[0]);
	sys_put_le16(item, &b[4]);
	return cmd(P2P_CMD_TWR_QUEUE_DROP, b, sizeof(b));
}

static uint8_t node_list(uint16_t first)
{
	uint8_t b[P2P_TWR_NODE_LIST_BODY_LEN];

	sys_put_le16(first, b);
	return cmd(P2P_CMD_TWR_NODE_LIST, b, sizeof(b));
}

/* BOOT -> CONFIGURED -> GATEWAY the way the Hub does it. */
static void bring_up(uint8_t ta, uint8_t gap)
{
	zassert_equal(set_radio(), P2P_STATUS_OK);
	zassert_equal(ctr_block(1000, 4999), P2P_STATUS_OK);
	zassert_equal(node_add(NODE_ADDR, 5, P2P_TWR_NODE_HOME), P2P_STATUS_OK);
	zassert_equal(start(GW_ADDR, ta, gap, P2P_TWR_START_ACK_ENABLED), P2P_STATUS_OK);
	zassert_equal(fake_radio.state, P2P_NB_GATEWAY);
}

/* GET_INFO payload (after the status byte). */
static const uint8_t *get_info(void)
{
	zassert_equal(cmd(P2P_CMD_GET_INFO, NULL, 0), P2P_STATUS_OK);
	zassert_equal(fake_rsp.len, 1 + P2P_GET_INFO_LEN, "GET_INFO is 188 B");
	return &fake_rsp.body[1];
}

static uint32_t info_ctr(const uint8_t *p, enum p2p_info_ctr i)
{
	return sys_get_le32(&p[45 + 4 * i]);
}

static uint8_t want_build_flags(void)
{
	uint8_t f = 0;

#if defined(APP_P2P_BENCH_RTT_BRIDGE)
	f |= P2P_BUILD_BENCH_RTT_BRIDGE;
#endif
#if defined(CONFIG_DEBUG_OPTIMIZATIONS) || defined(CONFIG_NO_OPTIMIZATIONS)
	f |= P2P_BUILD_DEBUG;
#endif
#if defined(CONFIG_NB_TWR_RX_INJECT)
	f |= P2P_BUILD_RX_INJECT;
#endif
	return f;
}

/* ---- GET_INFO ------------------------------------------------------------------------------ */

/* The bench `p2p rx_inject` is a Kconfig switch, off by default, and GET_INFO says when it is
 * in (testcase.yaml builds this suite both ways). */
ZTEST(cmd_dispatch, test_build_flags_rx_inject)
{
	const uint8_t *p = get_info();

	zassert_equal(P2P_BUILD_RX_INJECT, 0x04, "build_flags b2");
	zassert_equal((p[8] & P2P_BUILD_RX_INJECT) != 0, IS_ENABLED(CONFIG_NB_TWR_RX_INJECT));
}

ZTEST(cmd_dispatch, test_get_info_rejects_a_body)
{
	uint8_t b[1] = { 0 };

	zassert_equal(cmd(P2P_CMD_GET_INFO, b, 1), P2P_STATUS_BAD_PARAM);
	zassert_equal(fake_rsp.len, 1, "an error RSP carries no payload");
}

ZTEST(cmd_dispatch, test_get_info_in_boot)
{
	const uint8_t *p = get_info();
	struct diag_info d;

	diag_snapshot(&d);
	zassert_equal(p[0], FW_VERSION_MAJOR);
	zassert_equal(p[1], FW_VERSION_MINOR);
	zassert_equal(p[2], FW_VERSION_PATCH);
	/* APP_GIT_SHA=0x925f722f (CMakeLists.txt): display order on the wire */
	zassert_equal(p[3], 0x92);
	zassert_equal(p[4], 0x5f);
	zassert_equal(p[5], 0x72);
	zassert_equal(p[6], 0x2f);
	zassert_equal(p[7], d.boot_reset_cause);
	zassert_equal(p[8], want_build_flags());
	zassert_equal(p[17], P2P_NB_BOOT, "state");
	zassert_equal(sys_get_le32(&p[18]), 0, "net_id before TWR_START");
	zassert_equal(p[22], 0);
	zassert_equal(p[23], 0);
	zassert_equal(p[24], P2P_MODULATION_LORA);
	for (int i = 25; i < 36; i++) {
		zassert_equal(p[i], 0, "radio config zero before SET_RADIO_CONFIG (%d)", i);
	}
	zassert_equal(sys_get_le16(&p[153]), 0, "peers");
	zassert_equal(sys_get_le16(&p[155]), TWR_PEERS_MAX, "peers_max");
	zassert_true(sys_get_le16(&p[155]) >= 256);
	zassert_equal(sys_get_le16(&p[157]), 0, "queue_used");
	zassert_equal(sys_get_le16(&p[159]), TWR_DLQ_MAX, "queue_max");
	zassert_true(sys_get_le16(&p[159]) >= 16);
	zassert_equal(p[161], 0, "evt_ring_used");
	zassert_equal(p[162], 32, "evt_ring_max: the Kconfig default");
	zassert_equal(sys_get_le32(&p[163]), 0, "no counter block");
	zassert_equal(sys_get_le32(&p[167]), 0);
	zassert_equal(sys_get_le32(&p[180]), 0, "app_image_len: no valid record");
	zassert_equal(sys_get_le32(&p[184]), 0, "app_crc32: no valid record");
}

ZTEST(cmd_dispatch, test_get_info_sources)
{
	struct twr_gwc_stats *s = &fake_gwc.st;
	struct diag_info d;
	const uint8_t *p;

	bring_up(30, 40);
	zassert_equal(push(NODE_ADDR, 1, 60, 0, 4), P2P_STATUS_OK);

	fake_radio.now_ms = 0x0000012345678901ull;
	fake_radio.stats = (struct radio_ctl_stats){
		.crc_err = 201, .hdr_err = 202, .rx_overrun = 203, .tx_total = 204,
		.ack_turnaround_last_us = 20150, .ack_turnaround_max_us = 21999,
	};
	fake_link_crc_err = 301;
	fake_link_tx_drops = 7;
	fake_airtime_ms = 36000;
	*s = (struct twr_gwc_stats){
		.rx_total = 101, .fresh = 102, .dup = 103, .replay = 104, .mic_fail = 105,
		.unknown_src = 106, .join_fwd = 107, .not_for_us = 108, .bad_hdr = 109,
		.acks_sent = 110, .acks_late = 111, .acks_suppressed = 112, .dl_sent = 113,
		.dl_delivered = 114, .dl_not_delivered = 115, .dl_expired = 116,
		.ack_unmatched = 901, .dl_radio_err = 902, .dl_no_counter = 903, .dl_late = 904,
		.ack_ctrl_sent = 117, .ack_time_no_anchor = 118, .ack_ctrl_lc = 905,
		.ack_ctrl_time = 906,
	};

	p = get_info();
	diag_snapshot(&d);
	zassert_equal(sys_get_le64(&p[9]), 0x0000012345678901ull, "now_ms: radio_ctl's clock");
	zassert_equal(p[17], P2P_NB_GATEWAY);
	zassert_equal(sys_get_le32(&p[18]), GW_ADDR);
	zassert_equal(p[22], 30, "turnaround_ms");
	zassert_equal(p[23], 40, "dl_gap_ms");
	zassert_equal(sys_get_le32(&p[25]), 869525000);
	zassert_equal(p[29], 9);
	zassert_equal(p[30], P2P_BW_125K);
	zassert_equal(p[31], 1);
	zassert_equal(sys_get_le16(&p[32]), 8);
	zassert_equal((int8_t)p[34], 14);
	zassert_equal(p[35], P2P_RADIO_FLAG_PUBLIC_SYNCWORD);
	zassert_equal(sys_get_le32(&p[36]), d.err_flags);
	zassert_equal(p[40], d.last_err_code);

	zassert_equal(info_ctr(p, P2P_INFO_RX_TOTAL), 101);
	zassert_equal(info_ctr(p, P2P_INFO_RX_FRESH), 102);
	zassert_equal(info_ctr(p, P2P_INFO_RX_DUP), 103);
	zassert_equal(info_ctr(p, P2P_INFO_RX_REPLAY), 104);
	zassert_equal(info_ctr(p, P2P_INFO_RX_MIC_FAIL), 105);
	zassert_equal(info_ctr(p, P2P_INFO_RX_UNKNOWN_SRC), 106);
	zassert_equal(info_ctr(p, P2P_INFO_RX_JOIN_FWD), 107);
	zassert_equal(info_ctr(p, P2P_INFO_RX_NOT_FOR_US), 108);
	zassert_equal(info_ctr(p, P2P_INFO_RX_BAD_HDR), 109);
	zassert_equal(info_ctr(p, P2P_INFO_RADIO_CRC_ERR), 201);
	zassert_equal(info_ctr(p, P2P_INFO_RADIO_HDR_ERR), 202);
	zassert_equal(info_ctr(p, P2P_INFO_RX_OVERRUN), 203);
	zassert_equal(info_ctr(p, P2P_INFO_RX_REARM_FAIL), d.rx_rearm_fail);
	zassert_equal(info_ctr(p, P2P_INFO_TX_TOTAL), 204);
	zassert_equal(info_ctr(p, P2P_INFO_TX_RADIO_ERR), d.tx_radio_err);
	zassert_equal(info_ctr(p, P2P_INFO_ACKS_SENT), 110);
	zassert_equal(info_ctr(p, P2P_INFO_ACKS_LATE), 111);
	zassert_equal(info_ctr(p, P2P_INFO_ACKS_SUPPRESSED), 112);
	zassert_equal(info_ctr(p, P2P_INFO_DL_SENT), 113);
	zassert_equal(info_ctr(p, P2P_INFO_DL_DELIVERED), 114);
	zassert_equal(info_ctr(p, P2P_INFO_DL_NOT_DELIVERED), 115);
	zassert_equal(info_ctr(p, P2P_INFO_DL_EXPIRED), 116);
	zassert_equal(info_ctr(p, P2P_INFO_LINK_CRC_ERR), 301);
	zassert_equal(info_ctr(p, P2P_INFO_LINK_ERR), d.link_err + 7, "framing + TX drops");
	zassert_equal(info_ctr(p, P2P_INFO_EVT_RESENT), 0);

	zassert_equal(sys_get_le16(&p[145]), 20150, "ack_turnaround_last_us");
	zassert_equal(sys_get_le16(&p[147]), 21999, "ack_turnaround_max_us");
	zassert_equal(sys_get_le32(&p[149]), 36000, "airtime_ms_last_hour");
	zassert_equal(sys_get_le16(&p[153]), 1, "peers");
	zassert_equal(sys_get_le16(&p[157]), 1, "queue_used");
	zassert_equal(sys_get_le32(&p[163]), 1000, "ctr_next");
	zassert_equal(sys_get_le32(&p[167]), 4999, "ctr_last");
	zassert_equal(sys_get_le32(&p[172]), 117, "ack_ctrl_sent");
	zassert_equal(sys_get_le32(&p[176]), 118, "ack_time_no_anchor");
}

/* 0.4.0 tail (northbridge_update.md §10): the app_valid record's image_len and crc32. */
ZTEST(cmd_dispatch, test_get_info_app_record)
{
	const uint8_t *p;

	fake_app_image_len = 102200;
	fake_app_crc32 = 0x6B754AACu;
	p = get_info();
	zassert_equal(sys_get_le32(&p[180]), 102200, "app_image_len");
	zassert_equal(sys_get_le32(&p[184]), 0x6B754AACu, "app_crc32");
	zassert_equal(p[180], 0x38, "little-endian");
	zassert_equal(p[187], 0x6B);
}

/* ---- ENTER_BOOTLOADER (northbridge_update.md §10) ------------------------------------------ */

static const uint8_t nbbl[P2P_ENTER_BOOTLOADER_BODY_LEN] = { 0x4E, 0x42, 0x42, 0x4C };

ZTEST(cmd_dispatch, test_enter_bootloader)
{
	zassert_equal(P2P_CMD_ENTER_BOOTLOADER, 0x07);
	zassert_equal(P2P_RSP(P2P_CMD_ENTER_BOOTLOADER), 0x47);
	zassert_equal(sys_get_le32(nbbl), P2P_ENTER_BOOTLOADER_MAGIC, "\"NBBL\" as a u32 LE");

	zassert_equal(cmd(P2P_CMD_ENTER_BOOTLOADER, nbbl, sizeof(nbbl)), P2P_STATUS_OK);
	zassert_equal(fake_rsp.len, 1, "status only");
	zassert_equal(fake_rom_boot_calls, 1, "rebooted into the ROM bootloader");
	zassert_equal(fake_rom_boot_rsp_count, fake_rsp_count, "after the RSP was sent");
}

ZTEST(cmd_dispatch, test_enter_bootloader_in_every_state)
{
	zassert_equal(set_radio(), P2P_STATUS_OK);
	zassert_equal(cmd(P2P_CMD_ENTER_BOOTLOADER, nbbl, sizeof(nbbl)), P2P_STATUS_OK);
	zassert_equal(fake_rom_boot_calls, 1, "CONFIGURED");

	gw_fakes_reset();
	bring_up(30, 40);
	zassert_equal(cmd(P2P_CMD_ENTER_BOOTLOADER, nbbl, sizeof(nbbl)), P2P_STATUS_OK);
	zassert_equal(fake_rom_boot_calls, 1, "GATEWAY");
}

ZTEST(cmd_dispatch, test_enter_bootloader_wants_the_exact_magic)
{
	uint8_t b[P2P_ENTER_BOOTLOADER_BODY_LEN + 1];

	zassert_equal(cmd(P2P_CMD_ENTER_BOOTLOADER, NULL, 0), P2P_STATUS_BAD_PARAM, "empty");
	zassert_equal(cmd(P2P_CMD_ENTER_BOOTLOADER, nbbl, 3), P2P_STATUS_BAD_PARAM, "short");
	memcpy(b, nbbl, sizeof(nbbl));
	b[4] = 0;
	zassert_equal(cmd(P2P_CMD_ENTER_BOOTLOADER, b, 5), P2P_STATUS_BAD_PARAM, "long");
	for (size_t i = 0; i < sizeof(nbbl); i++) {
		for (int bit = 0; bit < 8; bit++) {
			memcpy(b, nbbl, sizeof(nbbl));
			b[i] ^= (uint8_t)(1u << bit);
			zassert_equal(cmd(P2P_CMD_ENTER_BOOTLOADER, b, sizeof(nbbl)),
				      P2P_STATUS_BAD_PARAM, "byte %zu bit %d", i, bit);
			zassert_equal(fake_rsp.len, 1);
		}
	}
	/* Byte-swapped: the boot request word as a u32 BE. */
	sys_put_be32(P2P_ENTER_BOOTLOADER_MAGIC, b);
	zassert_equal(cmd(P2P_CMD_ENTER_BOOTLOADER, b, 4), P2P_STATUS_BAD_PARAM, "BE order");
	zassert_equal(fake_rom_boot_calls, 0, "never rebooted");
}

/* ---- SET_RADIO_CONFIG, RX_STOP ------------------------------------------------------------- */

ZTEST(cmd_dispatch, test_set_radio_config)
{
	uint8_t b[P2P_RADIO_CONFIG_BODY_LEN];

	zassert_equal(set_radio(), P2P_STATUS_OK);
	zassert_equal(fake_radio.state, P2P_NB_CONFIGURED);
	zassert_equal(fake_radio.cfg.frequency_hz, 869525000);
	zassert_equal(fake_radio.cfg.sf, 9);
	zassert_equal(fake_radio.cfg.preamble_len, 8);
	zassert_equal(fake_radio.cfg.tx_power_dbm, 14);
	zassert_equal(fake_rsp.len, 1);
	zassert_equal(set_radio(), P2P_STATUS_OK, "CONFIGURED -> CONFIGURED");

	radio_body(b, 9, P2P_BW_125K, 1);
	zassert_equal(cmd(P2P_CMD_SET_RADIO_CONFIG, b, sizeof(b) - 1), P2P_STATUS_BAD_PARAM);
	radio_body(b, 13, P2P_BW_125K, 1);
	zassert_equal(cmd(P2P_CMD_SET_RADIO_CONFIG, b, sizeof(b)), P2P_STATUS_BAD_PARAM);
	radio_body(b, 6, P2P_BW_125K, 1);
	zassert_equal(cmd(P2P_CMD_SET_RADIO_CONFIG, b, sizeof(b)), P2P_STATUS_BAD_PARAM);
	radio_body(b, 9, 3, 1);
	zassert_equal(cmd(P2P_CMD_SET_RADIO_CONFIG, b, sizeof(b)), P2P_STATUS_BAD_PARAM);
	radio_body(b, 9, P2P_BW_125K, 5);
	zassert_equal(cmd(P2P_CMD_SET_RADIO_CONFIG, b, sizeof(b)), P2P_STATUS_BAD_PARAM);
	zassert_equal(fake_radio.set_config_calls, 2, "malformed bodies never reach the radio");

	/* what the radio cannot run (frequency, power, the 4 s ToA cap) is its BAD_PARAM */
	fake_radio.set_config_ret = P2P_STATUS_BAD_PARAM;
	radio_body(b, 12, P2P_BW_125K, 4);
	zassert_equal(cmd(P2P_CMD_SET_RADIO_CONFIG, b, sizeof(b)), P2P_STATUS_BAD_PARAM);
	zassert_equal(fake_radio.set_config_calls, 3);
	zassert_equal(fake_radio.cfg.sf, 9, "the old config stays");
}

ZTEST(cmd_dispatch, test_set_radio_config_in_gateway)
{
	bring_up(20, 20);
	int calls = fake_radio.set_config_calls;

	zassert_equal(set_radio(), P2P_STATUS_INVALID_STATE, "RX_STOP first");
	zassert_equal(fake_radio.set_config_calls, calls);
	zassert_equal(fake_radio.state, P2P_NB_GATEWAY);

	zassert_equal(cmd(P2P_CMD_RX_STOP, NULL, 0), P2P_STATUS_OK);
	zassert_equal(fake_radio.state, P2P_NB_CONFIGURED);
	zassert_equal(set_radio(), P2P_STATUS_OK);
	zassert_equal(start(GW_ADDR, 20, 20, P2P_TWR_START_ACK_ENABLED), P2P_STATUS_OK,
		      "the tables and the block survive RX_STOP");
	zassert_equal(fake_radio.state, P2P_NB_GATEWAY);
	zassert_equal(fake_gwc.peers.n, 1);
}

ZTEST(cmd_dispatch, test_rx_stop)
{
	uint8_t b[1] = { 0xAA };

	zassert_equal(cmd(P2P_CMD_RX_STOP, NULL, 0), P2P_STATUS_OK, "any state");
	zassert_equal(fake_radio.state, P2P_NB_BOOT);
	zassert_equal(cmd(P2P_CMD_RX_STOP, b, 1), P2P_STATUS_BAD_PARAM);
	zassert_equal(fake_radio.rx_stop_calls, 1);
}

/* ---- retired and unknown commands ---------------------------------------------------------- */

ZTEST(cmd_dispatch, test_retired_and_unknown_answer_unsupported)
{
	static const uint8_t types[] = { 0x03, 0x16, 0x00, 0x08, 0x0F, 0x1A, 0x3F };
	uint8_t b[4] = { 0 };

	for (size_t i = 0; i < ARRAY_SIZE(types); i++) {
		zassert_equal(cmd(types[i], NULL, 0), P2P_STATUS_UNSUPPORTED, "0x%02x", types[i]);
		zassert_equal(fake_rsp.len, 1);
	}
	zassert_equal(cmd(0x03, b, sizeof(b)), P2P_STATUS_UNSUPPORTED, "RX_START is gone");
	zassert_equal(fake_radio.state, P2P_NB_BOOT);
	zassert_equal(P2P_STATUS_UNSUPPORTED, 8);
}

/* ---- TWR_EVT_ACK (#470 link §4.1) ---------------------------------------------------------- */

static void evt_ack(const uint8_t *b, size_t len)
{
	int before = fake_rsp_count;

	proto_on_frame(P2P_CMD_TWR_EVT_ACK, ++seq, b, len);
	zassert_equal(fake_rsp_count, before, "TWR_EVT_ACK is never answered");
	zassert_equal(fake_lock_depth, 0, "lock balanced");
}

ZTEST(cmd_dispatch, test_twr_evt_ack_and_ring_in_get_info)
{
	uint8_t body[P2P_EVT_TWR_TX_BODY_LEN] = { 0 };
	uint8_t b[2] = { 0 };
	const uint8_t *p;
	uint8_t s;

	for (int i = 0; i < 5; i++) {
		zassert_true(twr_evq_push(&fake_gwc.evq, P2P_EVT_TWR_TX, body, sizeof(body), &s));
	}
	fake_gwc.evq.resent = 42;
	p = get_info();
	zassert_equal(p[161], 5, "evt_ring_used");
	zassert_equal(p[162], TWR_EVQ_MAX, "evt_ring_max");
	zassert_equal(info_ctr(p, P2P_INFO_EVT_RESENT), 42);

	b[0] = 3;
	evt_ack(b, 1);
	zassert_equal(twr_evq_used(&fake_gwc.evq), 2, "cumulative up to and including 3");
	evt_ack(b, 1);
	zassert_equal(fake_gwc.evq.ack_ignored, 1, "a repeated ack is ignored");
	evt_ack(b, 2);
	evt_ack(NULL, 0);
	zassert_equal(fake_gwc.evq.ack_ignored, 3, "a wrong length is ignored, not answered");
	zassert_equal(twr_evq_used(&fake_gwc.evq), 2);
	b[0] = 5;
	evt_ack(b, 1);
	p = get_info();
	zassert_equal(p[161], 0);
	zassert_equal(fake_radio.state, P2P_NB_BOOT, "valid in every state");
}

/* ---- TX_SCHEDULE, TX_CANCEL ---------------------------------------------------------------- */

static size_t sched_body(uint8_t *b, uint8_t tx_id, uint64_t t_ms, const uint8_t *f, uint8_t n)
{
	b[0] = tx_id;
	sys_put_le64(t_ms, &b[1]);
	b[9] = n;
	memcpy(&b[10], f, n);
	return P2P_TX_SCHEDULE_HDR_LEN + n;
}

ZTEST(cmd_dispatch, test_tx_schedule)
{
	static const uint8_t f[] = { 0xDE, 0xAD, 0x7E, 0xEF };
	uint8_t b[P2P_TX_SCHEDULE_HDR_LEN + sizeof(f)];
	size_t n = sched_body(b, 7, 123456789ull, f, sizeof(f));
	struct diag_info d;

	zassert_equal(cmd(P2P_CMD_TX_SCHEDULE, b, n), P2P_STATUS_OK);
	zassert_equal(fake_radio.tx_schedule_calls, 1);
	zassert_equal(fake_radio.tx_id, 7);
	zassert_equal(fake_radio.t_ms, 123456789ull);
	zassert_equal(fake_radio.frame_len, sizeof(f));
	zassert_mem_equal(fake_radio.frame, f, sizeof(f));

	n = sched_body(b, 8, P2P_TX_IMMEDIATE, f, 1);
	zassert_equal(cmd(P2P_CMD_TX_SCHEDULE, b, n), P2P_STATUS_OK);
	zassert_equal(fake_radio.t_ms, 0);

	zassert_equal(cmd(P2P_CMD_TX_SCHEDULE, b, P2P_TX_SCHEDULE_HDR_LEN - 1),
		      P2P_STATUS_BAD_PARAM);
	n = sched_body(b, 9, 100, f, 2);
	zassert_equal(cmd(P2P_CMD_TX_SCHEDULE, b, n - 1), P2P_STATUS_BAD_PARAM, "len mismatch");
	zassert_equal(fake_radio.tx_schedule_calls, 2);

	/* BUSY (a collision with the gateway's own ACK / downlink) and LATE pass through */
	fake_radio.tx_schedule_ret = P2P_STATUS_BUSY;
	zassert_equal(cmd(P2P_CMD_TX_SCHEDULE, b, n), P2P_STATUS_BUSY);
	fake_radio.tx_schedule_ret = P2P_STATUS_LATE;
	zassert_equal(cmd(P2P_CMD_TX_SCHEDULE, b, n), P2P_STATUS_LATE);
	fake_radio.tx_schedule_ret = P2P_STATUS_INTERNAL;
	zassert_equal(cmd(P2P_CMD_TX_SCHEDULE, b, n), P2P_STATUS_INTERNAL);
	diag_snapshot(&d);
	zassert_true(d.err_flags & DIAG_FLAG_TX_INTERNAL, "INTERNAL is a sticky flag");
}

ZTEST(cmd_dispatch, test_tx_cancel)
{
	uint8_t b[2] = { 0x42, 0x00 };

	zassert_equal(cmd(P2P_CMD_TX_CANCEL, b, 1), P2P_STATUS_OK);
	zassert_equal(fake_radio.tx_cancel_calls, 1);
	zassert_equal(fake_radio.cancel_tx_id, 0x42);
	zassert_equal(cmd(P2P_CMD_TX_CANCEL, b, 2), P2P_STATUS_BAD_PARAM);
	zassert_equal(cmd(P2P_CMD_TX_CANCEL, NULL, 0), P2P_STATUS_BAD_PARAM);
	zassert_equal(fake_radio.tx_cancel_calls, 1);
	fake_radio.tx_cancel_ret = P2P_STATUS_BAD_PARAM;
	zassert_equal(cmd(P2P_CMD_TX_CANCEL, b, 1), P2P_STATUS_BAD_PARAM, "unknown or started");
}

/* ---- TWR_* and the state machine ----------------------------------------------------------- */

ZTEST(cmd_dispatch, test_twr_in_boot)
{
	uint8_t b[P2P_TWR_CTR_BLOCK_BODY_LEN] = { 0 };

	zassert_equal(node_add(NODE_ADDR, 0, P2P_TWR_NODE_HOME), P2P_STATUS_INVALID_STATE);
	zassert_equal(ctr_block(1, 100), P2P_STATUS_INVALID_STATE);
	zassert_equal(push(NODE_ADDR, 1, 60, 0, 4), P2P_STATUS_INVALID_STATE);
	zassert_equal(start(GW_ADDR, 20, 20, 0), P2P_STATUS_INVALID_STATE);
	zassert_equal(fake_enter_calls, 0);
	zassert_equal(fake_kick_calls, 0);
	zassert_equal(fake_gwc.peers.n, 0);
	zassert_false(fake_gwc.ctr.ever);

	/* the ones that work in every state */
	zassert_equal(node_remove(NODE_ADDR), P2P_STATUS_NOT_FOUND);
	zassert_equal(drop(NODE_ADDR, 0), P2P_STATUS_NOT_FOUND);
	zassert_equal(node_list(0), P2P_STATUS_OK);
	zassert_equal(sys_get_le16(&fake_rsp.body[1]), 0, "total");
	zassert_equal(sys_get_le16(&fake_rsp.body[3]), P2P_TWR_NODE_LIST_END);

	/* a malformed body is BAD_PARAM whatever the state */
	zassert_equal(cmd(P2P_CMD_TWR_CTR_BLOCK, b, sizeof(b)), P2P_STATUS_BAD_PARAM);
	zassert_equal(start(0, 20, 20, 0), P2P_STATUS_BAD_PARAM);
	zassert_equal(push(NODE_ADDR, 0, 60, 0, 4), P2P_STATUS_BAD_PARAM);
}

ZTEST(cmd_dispatch, test_twr_start)
{
	zassert_equal(set_radio(), P2P_STATUS_OK);
	zassert_equal(start(GW_ADDR, 20, 20, 0), P2P_STATUS_INVALID_STATE, "no counter block");
	zassert_equal(fake_enter_calls, 0);

	zassert_equal(ctr_block(1, 100), P2P_STATUS_OK);
	zassert_equal(fake_kick_calls, 1, "CTR_LOW is the runtime's to raise");
	zassert_equal(start(GW_ADDR, 19, 20, 0), P2P_STATUS_BAD_PARAM);
	zassert_equal(start(GW_ADDR, 20, 61, 0), P2P_STATUS_BAD_PARAM);
	zassert_equal(start(UINT32_MAX, 20, 20, 0), P2P_STATUS_BAD_PARAM);
	zassert_equal(start(GW_ADDR, 20, 20, 0x80), P2P_STATUS_BAD_PARAM);
	zassert_equal(fake_radio.state, P2P_NB_CONFIGURED);

	zassert_equal(start(GW_ADDR, 25, 35, P2P_TWR_START_ACK_ENABLED), P2P_STATUS_OK);
	zassert_equal(fake_enter_calls, 1);
	zassert_equal(fake_radio.state, P2P_NB_GATEWAY);
	zassert_true(fake_gwc.started);
	zassert_equal(fake_gwc.p.net_id, GW_ADDR);
	zassert_equal(fake_gwc.p.turnaround_ms, 25);
	zassert_equal(fake_gwc.p.dl_gap_ms, 35);
	zassert_equal(fake_gwc.p.flags, P2P_TWR_START_ACK_ENABLED);
	zassert_equal(start(GW_ADDR, 25, 35, P2P_TWR_START_ACK_ENABLED), P2P_STATUS_OK,
		      "again in GATEWAY");
	zassert_equal(fake_enter_calls, 2);

	/* the radio does not come up: INTERNAL, and the state stays CONFIGURED */
	zassert_equal(cmd(P2P_CMD_RX_STOP, NULL, 0), P2P_STATUS_OK);
	fake_enter_ret = P2P_STATUS_INTERNAL;
	zassert_equal(start(GW_ADDR, 25, 35, 0), P2P_STATUS_INTERNAL);
	zassert_equal(fake_radio.state, P2P_NB_CONFIGURED);
}

ZTEST(cmd_dispatch, test_twr_node_add_remove)
{
	uint8_t b[P2P_TWR_NODE_ADD_BODY_LEN] = { 0 };

	zassert_equal(set_radio(), P2P_STATUS_OK);
	zassert_equal(node_add(NODE_ADDR, 1234, P2P_TWR_NODE_HOME), P2P_STATUS_OK);
	zassert_equal(fake_gwc.peers.n, 1);
	zassert_equal(fake_gwc.peers.p[0].addr, NODE_ADDR);
	zassert_equal(fake_gwc.peers.p[0].last_seen, 1234);
	zassert_equal(fake_gwc.peers.p[0].flags, P2P_TWR_NODE_HOME);
	zassert_mem_equal(fake_gwc.peers.p[0].key, node_key, sizeof(node_key));
	zassert_equal(fake_rsp.len, 1, "status only");

	zassert_equal(cmd(P2P_CMD_TWR_NODE_ADD, b, sizeof(b) - 1), P2P_STATUS_BAD_PARAM);
	zassert_equal(node_add(0, 0, 0), P2P_STATUS_BAD_PARAM);
	zassert_equal(node_add(NODE_ADDR, 0, 0x04), P2P_STATUS_BAD_PARAM, "undefined flag bit");
	zassert_equal(node_add(NODE_ADDR, 0, 0x80 | P2P_TWR_NODE_HOME), P2P_STATUS_BAD_PARAM);
	zassert_equal(fake_gwc.peers.p[0].last_seen, 1234, "a refused command changes nothing");

	for (uint32_t i = 1; i < TWR_PEERS_MAX; i++) {
		zassert_equal(node_add(0x1000 + i, 0, 0), P2P_STATUS_OK);
	}
	zassert_equal(node_add(0x9999, 0, 0), P2P_STATUS_NO_SPACE, "256 peers");
	zassert_equal(node_add(NODE_ADDR, 1, 0), P2P_STATUS_OK, "an upsert fits");
	zassert_equal(node_remove(0x1001), P2P_STATUS_OK);
	zassert_equal(node_remove(0x1001), P2P_STATUS_NOT_FOUND);
	zassert_equal(node_add(0x9999, 0, 0), P2P_STATUS_OK);

	zassert_equal(ctr_block(1, 10), P2P_STATUS_OK);
	zassert_equal(node_remove(0x9999), P2P_STATUS_OK);
	zassert_equal(start(GW_ADDR, 20, 20, 0), P2P_STATUS_OK);
	zassert_equal(node_add(GW_ADDR, 0, 0), P2P_STATUS_BAD_PARAM, "addr == net_id");
}

/* link §4.2: the restore after a host restart never moves a running NB's lane back. */
ZTEST(cmd_dispatch, test_twr_node_add_keep_newer)
{
	uint8_t b[P2P_TWR_NODE_ADD_BODY_LEN];
	const uint8_t keep = P2P_TWR_NODE_HOME | P2P_TWR_NODE_KEEP_NEWER;

	zassert_equal(set_radio(), P2P_STATUS_OK);
	zassert_equal(node_add(NODE_ADDR, 500, P2P_TWR_NODE_HOME), P2P_STATUS_OK);
	zassert_equal(push(NODE_ADDR, 3, 60, 0, 4), P2P_STATUS_OK);

	zassert_equal(node_add(NODE_ADDR, 400, keep), P2P_STATUS_OK);
	zassert_equal(fake_gwc.peers.p[0].last_seen, 500, "same key: max(own, sent)");
	zassert_equal(fake_gwc.peers.p[0].flags, P2P_TWR_NODE_HOME, "KEEP_NEWER is not stored");
	zassert_equal(twr_dlq_used(&fake_gwc.dlq), 1, "same key: the queue stays");
	zassert_equal(node_add(NODE_ADDR, 600, keep), P2P_STATUS_OK);
	zassert_equal(fake_gwc.peers.p[0].last_seen, 600, "a newer sent value wins");
	zassert_equal(node_add(NODE_ADDR, 550, P2P_TWR_NODE_HOME), P2P_STATUS_OK);
	zassert_equal(fake_gwc.peers.p[0].last_seen, 550, "without the flag: exactly as sent");

	/* A different key (a new join) always takes the sent value and drops the queue. */
	sys_put_le32(NODE_ADDR, &b[0]);
	memcpy(&b[4], node_key, sizeof(node_key));
	b[4] ^= 0x01;
	sys_put_le32(0, &b[20]);
	b[24] = keep;
	zassert_equal(cmd(P2P_CMD_TWR_NODE_ADD, b, sizeof(b)), P2P_STATUS_OK);
	zassert_equal(fake_gwc.peers.p[0].last_seen, 0);
	zassert_equal(twr_dlq_used(&fake_gwc.dlq), 0, "a new key drops the queue");

	/* A new peer with KEEP_NEWER: nothing to keep, the sent value. */
	zassert_equal(node_add(0x2000, 77, keep), P2P_STATUS_OK);
	zassert_equal(fake_gwc.peers.p[1].last_seen, 77);
}

ZTEST(cmd_dispatch, test_twr_queue)
{
	uint8_t b[P2P_TWR_QUEUE_PUSH_HDR_LEN + 1];
	int i;

	bring_up(20, 20);
	fake_radio.now_ms = 50000;
	zassert_equal(push(NODE_ADDR, 1, 30, P2P_TWR_QUEUE_CONFIRMED, P2P_TWR_DL_MAX_PT),
		      P2P_STATUS_OK, "74 B, the TOWER MTU (#486)");
	i = twr_dlq_find(&fake_gwc.dlq, NODE_ADDR, 1);
	zassert_true(i >= 0);
	zassert_equal(fake_gwc.dlq.e[i].len, P2P_TWR_DL_MAX_PT);
	zassert_equal(fake_gwc.dlq.e[i].flags, P2P_TWR_QUEUE_CONFIRMED);
	zassert_equal(fake_gwc.dlq.e[i].expires_ms, 50000 + 30 * 1000, "TTL on radio_ctl's clock");
	zassert_equal(push(NODE_ADDR, 2, 30, 0, P2P_TWR_DL_MAX_PT + 1), P2P_STATUS_BAD_PARAM);
	zassert_equal(push(0x777, 2, 30, 0, 1), P2P_STATUS_NOT_FOUND);
	memset(b, 0, sizeof(b));
	zassert_equal(cmd(P2P_CMD_TWR_QUEUE_PUSH, b, sizeof(b)), P2P_STATUS_BAD_PARAM);

	for (uint16_t k = 2; k <= TWR_DLQ_MAX; k++) {
		zassert_equal(push(NODE_ADDR, k, 30, 0, 1), P2P_STATUS_OK, "item %u", k);
	}
	zassert_equal(push(NODE_ADDR, 999, 30, 0, 1), P2P_STATUS_NO_SPACE);
	zassert_equal(push(NODE_ADDR, 5, 60, 0, 2), P2P_STATUS_OK, "a retry replaces in place");

	zassert_equal(drop(NODE_ADDR, 5), P2P_STATUS_OK);
	zassert_equal(drop(NODE_ADDR, 5), P2P_STATUS_NOT_FOUND);
	zassert_equal(twr_dlq_used(&fake_gwc.dlq), TWR_DLQ_MAX - 1);
	zassert_equal(drop(NODE_ADDR, 0), P2P_STATUS_OK, "item 0: all");
	zassert_equal(twr_dlq_used(&fake_gwc.dlq), 0);
	zassert_equal(drop(NODE_ADDR, 0), P2P_STATUS_OK, "empty is fine");
	zassert_equal(fake_evt_count, 0, "a drop raises no events");

	/* drop and remove still work after RX_STOP */
	zassert_equal(push(NODE_ADDR, 1, 30, 0, 1), P2P_STATUS_OK);
	zassert_equal(cmd(P2P_CMD_RX_STOP, NULL, 0), P2P_STATUS_OK);
	zassert_equal(drop(NODE_ADDR, 1), P2P_STATUS_OK);
	zassert_equal(node_remove(NODE_ADDR), P2P_STATUS_OK);
}

ZTEST(cmd_dispatch, test_twr_ctr_block)
{
	zassert_equal(set_radio(), P2P_STATUS_OK);
	zassert_equal(ctr_block(0, 10), P2P_STATUS_BAD_PARAM);
	zassert_equal(ctr_block(11, 10), P2P_STATUS_BAD_PARAM);
	zassert_equal(ctr_block(1, UINT32_MAX), P2P_STATUS_BAD_PARAM);
	zassert_equal(fake_kick_calls, 0);
	zassert_equal(ctr_block(500, 899), P2P_STATUS_OK);
	zassert_equal(fake_kick_calls, 1);
	zassert_equal(fake_gwc.ctr.first, 500);
	zassert_equal(fake_gwc.ctr.last, 899);
	zassert_equal(fake_gwc.ctr.next, 500);
	zassert_equal(ctr_block(900, 900), P2P_STATUS_OK, "a new block replaces the old one");
	zassert_equal(fake_gwc.ctr.next, 900);
	zassert_equal(ctr_block(1, 899), P2P_STATUS_BAD_PARAM, "never below the old next");
	zassert_equal(fake_gwc.ctr.next, 900);
	zassert_equal(fake_gwc.ctr.last, 900);
}

ZTEST(cmd_dispatch, test_twr_node_list)
{
	const uint8_t *p = &fake_rsp.body[1];
	uint8_t b[1] = { 0 };

	zassert_equal(set_radio(), P2P_STATUS_OK);
	for (uint32_t i = 0; i < 30; i++) {
		zassert_equal(node_add(0x2000 + i, 100 + i, i == 3 ? P2P_TWR_NODE_HOME : 0),
			      P2P_STATUS_OK);
	}
	zassert_equal(node_list(0), P2P_STATUS_OK);
	zassert_equal(fake_rsp.len, 1 + P2P_TWR_NODE_LIST_HDR_LEN + 24 * 9);
	zassert_equal(sys_get_le16(&p[0]), 30, "total");
	zassert_equal(sys_get_le16(&p[2]), 24, "next");
	zassert_equal(p[4], 24);
	zassert_equal(sys_get_le32(&p[5 + 3 * 9]), 0x2003);
	zassert_equal(sys_get_le32(&p[5 + 3 * 9 + 4]), 103);
	zassert_equal(p[5 + 3 * 9 + 8], P2P_TWR_NODE_HOME);
	for (size_t i = 0; i + sizeof(node_key) <= fake_rsp.len; i++) {
		zassert_true(memcmp(&fake_rsp.body[i], node_key, 4) != 0, "never a key");
	}

	zassert_equal(node_list(24), P2P_STATUS_OK);
	zassert_equal(p[4], 6);
	zassert_equal(sys_get_le16(&p[2]), P2P_TWR_NODE_LIST_END);
	zassert_equal(cmd(P2P_CMD_TWR_NODE_LIST, b, 1), P2P_STATUS_BAD_PARAM);
}

/* TWR_TIME_SYNC (0x19): the TimeAns anchor, any state, status-only RSP; an anchor from the
 * future (a previous boot's clock) and a body of the wrong length are BAD_PARAM. */
ZTEST(cmd_dispatch, test_twr_time_sync)
{
	uint8_t b[P2P_TWR_TIME_SYNC_BODY_LEN + 1] = { 0 };

	fake_radio.now_ms = 5000;
	sys_put_le64(4990, &b[0]);
	sys_put_le64(1790000000000ull, &b[8]);
	zassert_false(fake_gwc.anchor.valid);
	zassert_equal(cmd(P2P_CMD_TWR_TIME_SYNC, b, P2P_TWR_TIME_SYNC_BODY_LEN), P2P_STATUS_OK,
		      "in BOOT too");
	zassert_equal(fake_rsp.len, 1, "status only");
	zassert_true(fake_gwc.anchor.valid);
	zassert_equal(fake_gwc.anchor.now_ms, 4990);
	zassert_equal(fake_gwc.anchor.unix_ms, 1790000000000ull);
	zassert_equal(fake_lock_depth, 0, "lock released");

	sys_put_le64(5001, &b[0]);
	zassert_equal(cmd(P2P_CMD_TWR_TIME_SYNC, b, P2P_TWR_TIME_SYNC_BODY_LEN),
		      P2P_STATUS_BAD_PARAM, "now_ms ahead of the NB clock");
	zassert_equal(fake_gwc.anchor.now_ms, 4990, "kept");
	zassert_equal(cmd(P2P_CMD_TWR_TIME_SYNC, b, 15), P2P_STATUS_BAD_PARAM);
	zassert_equal(cmd(P2P_CMD_TWR_TIME_SYNC, b, 17), P2P_STATUS_BAD_PARAM);
	sys_put_le64(5000, &b[0]);
	zassert_equal(cmd(P2P_CMD_TWR_TIME_SYNC, b, P2P_TWR_TIME_SYNC_BODY_LEN), P2P_STATUS_OK,
		      "now");
}

/* ---- EVT_BOOT, diagnostics ------------------------------------------------------------------ */

ZTEST(cmd_dispatch, test_evt_boot_body)
{
	struct twr_identity id;
	uint8_t out[P2P_EVT_BOOT_BODY_LEN];
	static const uint8_t want[] = {
		FW_VERSION_MAJOR, FW_VERSION_MINOR, FW_VERSION_PATCH, 0x92, 0x5f, 0x72, 0x2f, 0x08,
	};

	diag_set_boot_reset_cause(0x08);
	cmd_identity(&id);
	zassert_equal(twr_link_enc_boot(&id, out), sizeof(want));
	zassert_mem_equal(out, want, sizeof(want));
	zassert_equal(id.build_flags, want_build_flags());
	zassert_equal(get_info()[7], 0x08, "GET_INFO carries the same reset_cause");
}

ZTEST(cmd_dispatch, test_diag_reports_are_counted_logged_once_per_interval_and_in_get_info)
{
	struct diag_info before;
	const uint8_t *p;

	diag_snapshot(&before);
	diag_report(DIAG_CODE_RX_REARM, "rx re-arm failed after tx");
	zassert_equal(fake_evt_count, 1, "the first report sends an EVT_LOG");
	zassert_equal(fake_evt.type, P2P_EVT_LOG);
	zassert_equal(fake_evt.body[0], DIAG_LEVEL_ERROR);
	zassert_equal(fake_evt.body[1], DIAG_CODE_RX_REARM);
	zassert_mem_equal(&fake_evt.body[2], "rx re-arm failed after tx", 25);
	zassert_equal(fake_rsp_count, 0, "an event, not an RSP");

	diag_report(DIAG_CODE_RX_REARM, "again");
	zassert_equal(fake_evt_count, 1, "a second one within 10 s is only counted");

	diag_report(DIAG_CODE_TX_RADIO, NULL);
	zassert_equal(fake_evt_count, 2, "another code has its own interval");
	zassert_equal(fake_evt.len, 2, "no detail, just level and code");
	zassert_equal(fake_evt.seq, 0, "best-effort: the delivery seq, not advanced");

	p = get_info();
	zassert_true(sys_get_le32(&p[36]) & DIAG_FLAG_RX_REARM);
	zassert_true(sys_get_le32(&p[36]) & DIAG_FLAG_TX_RADIO);
	zassert_equal(p[40], DIAG_CODE_TX_RADIO, "last_err_code is the most recent");
	zassert_true(sys_get_le32(&p[41]) < 5, "last_err_age_s");
	zassert_equal(info_ctr(p, P2P_INFO_RX_REARM_FAIL), before.rx_rearm_fail + 2);
	zassert_equal(info_ctr(p, P2P_INFO_TX_RADIO_ERR), before.tx_radio_err + 1);
}

ZTEST(cmd_dispatch, test_diag_log_body_truncates_its_detail)
{
	uint8_t out[6];

	zassert_equal(diag_encode_log(out, sizeof(out), DIAG_LEVEL_WARNING, 5, "overrun"), 6);
	zassert_equal(out[0], DIAG_LEVEL_WARNING);
	zassert_equal(out[1], 5);
	zassert_mem_equal(&out[2], "over", 4);
}
