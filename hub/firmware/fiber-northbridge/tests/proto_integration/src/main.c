/* SPDX-License-Identifier: Apache-2.0
 *
 * End-to-end integration ztest for the northbridge's receive path:
 *   raw HDLC bytes -> hdlc_decoder_feed() -> proto_on_frame() -> RSP.
 *
 * The hdlc_framing suite tests framing alone; the cmd_dispatch suite tests
 * dispatch alone (calling proto_on_frame() directly). Neither exercises the
 * two together - which is exactly the path uart_link_bench_inject() and the
 * real USART1 ISR drive. This suite closes that gap by linking the REAL
 * hdlc.c, cmd_dispatch.c and twr_cmd.c against the radio_ctl/twr_rt/uart_link
 * doubles the cmd_dispatch suite uses (tests/cmd_dispatch/src/gw_fakes.c).
 *
 * The headline cases: a P2P radio frame carried in a TX_SCHEDULE, a session
 * key in TWR_NODE_ADD and a downlink in TWR_QUEUE_PUSH can all contain
 * 0x7E/0x7D bytes; HDLC byte-stuffing must survive all the way to the
 * dispatched frame, or the Northbridge would corrupt what goes on air.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/byteorder.h>
#include <zephyr/sys/util.h>
#include <string.h>

#include "proto.h"
#include "hdlc.h"
#include "cmd_dispatch.h"
#include "gw_fakes.h"
#include "radio_ctl.h"
#include "twr_dlq.h"

/* ---- harness ------------------------------------------------------------- */

static struct hdlc_decoder dec;

/* Capture for the response-frame round-trip test (test_response_frame_...). */
static struct {
	bool got;
	uint8_t type, seq, body[P2P_MAX_BODY_LEN];
	size_t body_len;
} rt;

static void rt_cb(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len,
		  void *user_data)
{
	ARG_UNUSED(user_data);
	rt.got = true;
	rt.type = type;
	rt.seq = seq;
	rt.body_len = MIN(body_len, sizeof(rt.body));
	memcpy(rt.body, body, rt.body_len);
}

static void reset_state(void *fixture)
{
	ARG_UNUSED(fixture);
	gw_fakes_reset();
	memset(&rt, 0, sizeof(rt));
	hdlc_decoder_init(&dec);
}

ZTEST_SUITE(proto_integration, NULL, NULL, reset_state, NULL, NULL);

/* The wire callback the real ISR / bench-inject path uses: dispatch on a
 * complete, CRC-valid frame. */
static void on_frame(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len,
		     void *user_data)
{
	ARG_UNUSED(user_data);
	proto_on_frame(type, seq, body, body_len);
}

/* Feed an HDLC-encoded buffer through the decoder ONE BYTE AT A TIME (as the
 * UART ISR / p2p_inject would), dispatching every complete frame. */
static void feed(const uint8_t *buf, size_t len)
{
	for (size_t i = 0; i < len; i++) {
		hdlc_decoder_feed(&dec, buf[i], on_frame, NULL);
	}
}

/* -------------------------------------------------------------------------- */

ZTEST(proto_integration, test_get_info_decodes_and_dispatches)
{
	uint8_t enc[64];
	int n = hdlc_encode(P2P_CMD_GET_INFO, 0x11, NULL, 0, enc, sizeof(enc));

	zassert_true(n > 0, "encode failed");
	feed(enc, n);

	zassert_equal(fake_rsp_count, 1, "GET_INFO not dispatched to a single RSP");
	zassert_equal(fake_rsp.type, P2P_RSP(P2P_CMD_GET_INFO), "wrong RSP type");
	zassert_equal(fake_rsp.seq, 0x11, "RSP seq must echo the command seq");
	zassert_equal(fake_rsp.body[0], P2P_STATUS_OK, "GET_INFO RSP not OK");
	zassert_equal(fake_rsp.len, 1 + P2P_GET_INFO_LEN, "GET_INFO is 188 B");
}

ZTEST(proto_integration, test_set_radio_config_fields_survive_framing)
{
	/* freq/preamble are multi-byte little-endian; prove they decode intact
	 * through HDLC framing + dispatch, not just via a direct proto_on_frame. */
	uint8_t body[P2P_RADIO_CONFIG_BODY_LEN];
	size_t k = 0;

	sys_put_le32(868100000, &body[k]);
	k += 4;
	body[k++] = 10;                 /* sf */
	body[k++] = P2P_BW_125K;        /* bw */
	body[k++] = 1;                  /* cr = 4/5 */
	sys_put_le16(8, &body[k]);
	k += 2;
	body[k++] = (uint8_t)14;        /* tx_power */
	body[k++] = 0;                  /* flags */

	uint8_t enc[64];
	int n = hdlc_encode(P2P_CMD_SET_RADIO_CONFIG, 0, body, sizeof(body), enc, sizeof(enc));

	feed(enc, n);

	zassert_equal(fake_radio.set_config_calls, 1, "config not applied");
	zassert_equal(fake_radio.cfg.frequency_hz, 868100000, "freq mangled by framing");
	zassert_equal(fake_radio.cfg.sf, 10, "sf mangled");
	zassert_equal(fake_radio.cfg.preamble_len, 8, "preamble mangled");
	zassert_equal(fake_rsp.body[0], P2P_STATUS_OK, "SET_RADIO_CONFIG RSP not OK");
}

ZTEST(proto_integration, test_tx_schedule_frame_with_flag_and_esc_bytes)
{
	/* A P2P radio frame that contains the HDLC flag (0x7E) and escape (0x7D)
	 * bytes must reach radio_ctl_tx_schedule() byte-for-byte intact. */
	static const uint8_t p2p_frame[] = {
		0x7E, 0x7D, 0x00, 0x11, 0x7E, 0x22, 0x7D, 0x33, 0x7E, 0x7D, 0x44,
	};
	uint8_t body[1 + 8 + 1 + sizeof(p2p_frame)];
	size_t k = 0;

	body[k++] = 7;                  /* tx_id */
	sys_put_le64(1234567, &body[k]);
	k += 8;
	body[k++] = sizeof(p2p_frame);  /* len */
	memcpy(&body[k], p2p_frame, sizeof(p2p_frame));
	k += sizeof(p2p_frame);

	uint8_t enc[128];
	int n = hdlc_encode(P2P_CMD_TX_SCHEDULE, 0, body, k, enc, sizeof(enc));

	feed(enc, n);

	zassert_equal(fake_radio.tx_schedule_calls, 1, "TX_SCHEDULE not dispatched");
	zassert_equal(fake_radio.tx_id, 7, "tx_id mangled");
	zassert_equal(fake_radio.t_ms, 1234567, "t_ms mangled");
	zassert_equal(fake_radio.frame_len, sizeof(p2p_frame), "frame length wrong");
	zassert_mem_equal(fake_radio.frame, p2p_frame, sizeof(p2p_frame),
			  "0x7E/0x7D bytes in the radio frame did not survive HDLC stuffing");
}

ZTEST(proto_integration, test_response_frame_reencodes_and_decodes)
{
	/* The RSP proto_on_frame() produced must itself round-trip back through
	 * the framing a host would decode it with - the largest one, GET_INFO,
	 * included. */
	static const uint8_t cmds[] = { P2P_CMD_RX_STOP, P2P_CMD_GET_INFO };

	for (size_t c = 0; c < ARRAY_SIZE(cmds); c++) {
		uint8_t enc[64];
		int n = hdlc_encode(cmds[c], 0x05, NULL, 0, enc, sizeof(enc));

		memset(&rt, 0, sizeof(rt));
		feed(enc, n);
		zassert_equal(fake_rsp_count, (int)c + 1, "0x%02x not dispatched", cmds[c]);

		uint8_t rsp_enc[P2P_MAX_ENCODED_LEN];
		int rn = hdlc_encode(fake_rsp.type, fake_rsp.seq, fake_rsp.body, fake_rsp.len,
				     rsp_enc, sizeof(rsp_enc));
		zassert_true(rn > 0, "RSP re-encode failed");

		struct hdlc_decoder d2;

		hdlc_decoder_init(&d2);
		for (int i = 0; i < rn; i++) {
			hdlc_decoder_feed(&d2, rsp_enc[i], rt_cb, NULL);
		}
		zassert_true(rt.got, "RSP frame did not decode back");
		zassert_equal(rt.type, P2P_RSP(cmds[c]), "RSP type not preserved");
		zassert_equal(rt.seq, 0x05, "RSP seq not preserved");
		zassert_equal(rt.body_len, fake_rsp.len, "RSP body length changed");
		zassert_mem_equal(rt.body, fake_rsp.body, fake_rsp.len);
	}
}

ZTEST(proto_integration, test_retired_rx_start_is_unsupported_on_the_wire)
{
	uint8_t enc[64];
	int n = hdlc_encode(0x03, 0x44, NULL, 0, enc, sizeof(enc));

	feed(enc, n);
	zassert_equal(fake_rsp_count, 1);
	zassert_equal(fake_rsp.type, P2P_RSP(0x03));
	zassert_equal(fake_rsp.seq, 0x44);
	zassert_equal(fake_rsp.body[0], P2P_STATUS_UNSUPPORTED);
}

static uint8_t send_cmd(uint8_t type, const uint8_t *body, size_t len)
{
	uint8_t enc[P2P_MAX_ENCODED_LEN];
	int before = fake_rsp_count;
	int n = hdlc_encode(type, 0x01, body, len, enc, sizeof(enc));

	zassert_true(n > 0, "encode failed");
	feed(enc, n);
	zassert_equal(fake_rsp_count, before + 1, "0x%02x: one RSP", type);
	return fake_rsp.body[0];
}

ZTEST(proto_integration, test_twr_key_and_downlink_with_flag_and_esc_bytes)
{
	/* Session keys and downlink plaintext are random bytes: 0x7E / 0x7D in them are
	 * the common case, not an edge one. */
	static const uint8_t key[16] = {
		0x7E, 0x7D, 0x5E, 0x5D, 0x7E, 0x7E, 0x7D, 0x7D,
		0x00, 0xFF, 0x7E, 0x20, 0x7D, 0x01, 0x7E, 0x7D,
	};
	static const uint8_t pt[] = { 0x7D, 0x7E, 0x7D, 0x5E, 0x7E };
	uint8_t radio_cfg[P2P_RADIO_CONFIG_BODY_LEN] = { 0 };
	uint8_t add[P2P_TWR_NODE_ADD_BODY_LEN];
	uint8_t push[P2P_TWR_QUEUE_PUSH_HDR_LEN + sizeof(pt)];
	int i;

	sys_put_le32(868100000, &radio_cfg[0]);
	radio_cfg[4] = 7;
	radio_cfg[6] = 1;
	zassert_equal(send_cmd(P2P_CMD_SET_RADIO_CONFIG, radio_cfg, sizeof(radio_cfg)),
		      P2P_STATUS_OK);

	sys_put_le32(0x7E7D0413u, &add[0]);
	memcpy(&add[4], key, sizeof(key));
	sys_put_le32(0x7D7E0000u, &add[20]);
	add[24] = P2P_TWR_NODE_HOME;
	zassert_equal(send_cmd(P2P_CMD_TWR_NODE_ADD, add, sizeof(add)), P2P_STATUS_OK);
	zassert_equal(fake_gwc.peers.n, 1);
	zassert_equal(fake_gwc.peers.p[0].addr, 0x7E7D0413u);
	zassert_equal(fake_gwc.peers.p[0].last_seen, 0x7D7E0000u);
	zassert_mem_equal(fake_gwc.peers.p[0].key, key, sizeof(key), "key mangled");

	sys_put_le32(0x7E7D0413u, &push[0]);
	sys_put_le16(0x7E7D, &push[4]);
	sys_put_le16(60, &push[6]);
	push[8] = 0;
	push[9] = sizeof(pt);
	memcpy(&push[P2P_TWR_QUEUE_PUSH_HDR_LEN], pt, sizeof(pt));
	zassert_equal(send_cmd(P2P_CMD_TWR_QUEUE_PUSH, push, sizeof(push)), P2P_STATUS_OK);
	i = twr_dlq_find(&fake_gwc.dlq, 0x7E7D0413u, 0x7E7D);
	zassert_true(i >= 0, "downlink not queued");
	zassert_equal(fake_gwc.dlq.e[i].len, sizeof(pt));
	zassert_mem_equal(fake_gwc.dlq.e[i].pt, pt, sizeof(pt), "downlink mangled");
}

ZTEST(proto_integration, test_crc_error_frame_is_not_dispatched)
{
	uint8_t enc[64];
	int n = hdlc_encode(P2P_CMD_GET_INFO, 0, NULL, 0, enc, sizeof(enc));

	/* Corrupt a payload byte (index 1 is the first framed byte = type). */
	enc[1] ^= 0xFF;
	feed(enc, n);

	zassert_equal(fake_rsp_count, 0, "a CRC-corrupted frame was still dispatched");
}

ZTEST(proto_integration, test_evt_log_frame_round_trips_to_the_host)
{
	/* B2: the RX re-arm failure had no wire representation - it surfaced to the host as
	 * unexplained EVT_RX silence, a dead link that looks like a quiet one. It is now an
	 * EVT_LOG with an ASCII body. Provoking a real re-arm failure needs a radio, so what is
	 * pinned here is the framing contract the host decodes: type 0x83, body is the text,
	 * and it survives HDLC round-tripping including any byte stuffing. The host side maps
	 * evt::LOG to Event::Log and logs `EVT_LOG: {}` (control-radio p2p/modem.rs). */
	static const char msg[] = "rx re-arm failed after tx";
	uint8_t enc[128];
	int n = hdlc_encode(P2P_EVT_LOG, 0x2A, (const uint8_t *)msg, sizeof(msg) - 1, enc,
			    sizeof(enc));

	zassert_true(n > 0, "EVT_LOG encode failed");

	struct hdlc_decoder d;

	hdlc_decoder_init(&d);
	for (int i = 0; i < n; i++) {
		hdlc_decoder_feed(&d, enc[i], rt_cb, NULL);
	}

	zassert_true(rt.got, "EVT_LOG frame did not decode");
	zassert_equal(rt.type, P2P_EVT_LOG, "EVT_LOG must keep event type 0x83");
	zassert_equal(rt.seq, 0x2A, "event seq not preserved");
	zassert_equal(rt.body_len, sizeof(msg) - 1, "EVT_LOG body length changed");
	zassert_mem_equal(rt.body, msg, sizeof(msg) - 1, "EVT_LOG body is the text verbatim");
}
