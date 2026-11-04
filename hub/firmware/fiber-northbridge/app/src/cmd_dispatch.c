/* SPDX-License-Identifier: Apache-2.0
 *
 * Command dispatch of the gateway image: parses host->Northbridge command frames, validates
 * wire parameters, calls into radio_ctl (twr_gateway.c) or the TOWER gateway commands
 * (twr_cmd.c), and sends back the RSP. An unknown command - the retired RX_START (0x03) and
 * TWR_GET_STATS (0x16) included - answers UNSUPPORTED. TWR_EVT_ACK (0x18) is the one
 * command that is never answered.
 */
#include "cmd_dispatch.h"
#include "diag.h"
#include "nb_boot.h"
#include "proto.h"
#include "radio_ctl.h"
#include "twr_cmd.h"
#include "twr_gwc.h"
#include "twr_link.h"
#include "twr_rt.h"
#include "uart_link.h"

#include <string.h>
#include <zephyr/sys/byteorder.h>

static void send_rsp(uint8_t cmd, uint8_t seq, enum p2p_status status,
		      const uint8_t *payload, size_t payload_len)
{
	/* GET_INFO is the largest RSP here (twr_cmd.c sends its own). */
	uint8_t body[1 + P2P_GET_INFO_LEN];
	size_t n = 0;

	body[n++] = (uint8_t)status;
	if (status == P2P_STATUS_OK && payload_len > 0 && payload_len <= P2P_GET_INFO_LEN) {
		memcpy(&body[n], payload, payload_len);
		n += payload_len;
	}
	uart_link_send(P2P_RSP(cmd), seq, body, n);
}

void cmd_identity(struct twr_identity *id)
{
	struct diag_info d;
	uint32_t sha = APP_GIT_SHA;

	id->fw_ver[0] = FW_VERSION_MAJOR;
	id->fw_ver[1] = FW_VERSION_MINOR;
	id->fw_ver[2] = FW_VERSION_PATCH;
	/* Display order: the first byte is the first two hex digits of the commit. */
	sys_put_be32(sha, id->git_sha);
	diag_snapshot(&d);
	id->reset_cause = d.boot_reset_cause;
	id->build_flags = 0;
#if defined(APP_P2P_BENCH_RTT_BRIDGE)
	id->build_flags |= P2P_BUILD_BENCH_RTT_BRIDGE;
#endif
#if defined(CONFIG_DEBUG_OPTIMIZATIONS) || defined(CONFIG_NO_OPTIMIZATIONS)
	id->build_flags |= P2P_BUILD_DEBUG;
#endif
#if defined(CONFIG_NB_TWR_RX_INJECT)
	id->build_flags |= P2P_BUILD_RX_INJECT;
#endif
}

static void handle_get_info(uint8_t seq, size_t body_len)
{
	struct twr_info in;
	struct diag_info d;
	struct radio_ctl_stats rs;
	uint8_t payload[P2P_GET_INFO_LEN];

	if (body_len != 0) {
		send_rsp(P2P_CMD_GET_INFO, seq, P2P_STATUS_BAD_PARAM, NULL, 0);
		return;
	}

	memset(&in, 0, sizeof(in));
	cmd_identity(&in.id);
	in.now_ms = radio_ctl_now_ms();
	in.state = (uint8_t)radio_ctl_state();
	in.modulation = P2P_MODULATION_LORA;
	(void)radio_ctl_get_config(&in.radio); /* zeroed before SET_RADIO_CONFIG */

	diag_snapshot(&d);
	in.err_flags = d.err_flags;
	in.last_err_code = d.last_err_code;
	in.last_err_age_s = d.last_err_age_s;

	radio_ctl_get_stats(&rs);
	in.ctr[P2P_INFO_RADIO_CRC_ERR] = rs.crc_err;
	in.ctr[P2P_INFO_RADIO_HDR_ERR] = rs.hdr_err;
	in.ctr[P2P_INFO_RX_OVERRUN] = rs.rx_overrun;
	in.ctr[P2P_INFO_RX_REARM_FAIL] = d.rx_rearm_fail;
	in.ctr[P2P_INFO_TX_TOTAL] = rs.tx_total;
	in.ctr[P2P_INFO_TX_RADIO_ERR] = d.tx_radio_err;
	in.ctr[P2P_INFO_LINK_CRC_ERR] = uart_link_get_crc_err_count();
	/* Framing errors and RX overruns (diag.c), plus events the TX ring had no room for. */
	in.ctr[P2P_INFO_LINK_ERR] = d.link_err + uart_link_get_tx_drops();
	in.ack_turnaround_last_us = rs.ack_turnaround_last_us;
	in.ack_turnaround_max_us = rs.ack_turnaround_max_us;

	/* The duty ledger has its own lock; never taken inside the core's. */
	in.airtime_ms_last_hour = twr_rt_airtime_ms_last_hour();
	/* The core's counters, tables and the event ring (evt_ring_*, evt_resent). */
	twr_gwc_info(twr_rt_lock(), &in);
	twr_rt_unlock();
	nb_boot_app_record(&in.app_image_len, &in.app_crc32);

	send_rsp(P2P_CMD_GET_INFO, seq, P2P_STATUS_OK, payload, twr_link_enc_info(&in, payload));
}

static void handle_set_radio_config(uint8_t seq, const uint8_t *body, size_t body_len)
{
	struct radio_config cfg;
	enum p2p_status status = twr_link_dec_radio_config(body, body_len, &cfg);

	if (status == P2P_STATUS_OK && radio_ctl_state() == P2P_NB_GATEWAY) {
		/* A radio change is RX_STOP -> SET_RADIO_CONFIG -> TWR_START. */
		status = P2P_STATUS_INVALID_STATE;
	}
	if (status == P2P_STATUS_OK) {
		status = radio_ctl_set_config(&cfg);
	}
	send_rsp(P2P_CMD_SET_RADIO_CONFIG, seq, status, NULL, 0);
}

static void handle_rx_stop(uint8_t seq, size_t body_len)
{
	enum p2p_status status =
		(body_len != 0) ? P2P_STATUS_BAD_PARAM : radio_ctl_rx_stop();

	send_rsp(P2P_CMD_RX_STOP, seq, status, NULL, 0);
}

static void handle_tx_schedule(uint8_t seq, const uint8_t *body, size_t body_len)
{
	if (body_len < P2P_TX_SCHEDULE_HDR_LEN) {
		send_rsp(P2P_CMD_TX_SCHEDULE, seq, P2P_STATUS_BAD_PARAM, NULL, 0);
		return;
	}

	uint8_t tx_id = body[0];
	uint64_t t_ms = sys_get_le64(&body[1]);
	uint8_t len = body[9];

	if (body_len != (size_t)P2P_TX_SCHEDULE_HDR_LEN + len) {
		send_rsp(P2P_CMD_TX_SCHEDULE, seq, P2P_STATUS_BAD_PARAM, NULL, 0);
		return;
	}

	enum p2p_status status =
		radio_ctl_tx_schedule(tx_id, t_ms, &body[P2P_TX_SCHEDULE_HDR_LEN], len);

	if (status == P2P_STATUS_INTERNAL) {
		diag_set_flag(DIAG_FLAG_TX_INTERNAL);
	}
	send_rsp(P2P_CMD_TX_SCHEDULE, seq, status, NULL, 0);
}

static void handle_tx_cancel(uint8_t seq, const uint8_t *body, size_t body_len)
{
	if (body_len != P2P_TX_CANCEL_BODY_LEN) {
		send_rsp(P2P_CMD_TX_CANCEL, seq, P2P_STATUS_BAD_PARAM, NULL, 0);
		return;
	}

	send_rsp(P2P_CMD_TX_CANCEL, seq, radio_ctl_tx_cancel(body[0]), NULL, 0);
}

/* northbridge_update.md §10: the RSP goes out first, then the reboot into the ROM bootloader.
 * Accepted in every state: the update stops the gateway whatever it was doing. */
static void handle_enter_bootloader(uint8_t seq, const uint8_t *body, size_t body_len)
{
	enum p2p_status status = twr_link_dec_enter_bootloader(body, body_len);

	send_rsp(P2P_CMD_ENTER_BOOTLOADER, seq, status, NULL, 0);
	if (status == P2P_STATUS_OK) {
		nb_boot_enter_rom_bootloader();
	}
}

void proto_on_frame(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len)
{
	switch (type) {
	case P2P_CMD_GET_INFO:
		handle_get_info(seq, body_len);
		break;
	case P2P_CMD_SET_RADIO_CONFIG:
		handle_set_radio_config(seq, body, body_len);
		break;
	case P2P_CMD_RX_STOP:
		handle_rx_stop(seq, body_len);
		break;
	case P2P_CMD_TX_SCHEDULE:
		handle_tx_schedule(seq, body, body_len);
		break;
	case P2P_CMD_TX_CANCEL:
		handle_tx_cancel(seq, body, body_len);
		break;
	case P2P_CMD_ENTER_BOOTLOADER:
		handle_enter_bootloader(seq, body, body_len);
		break;
	default:
		/* TWR_* (twr_cmd.c answers them itself). */
		if (twr_cmd_handle(type, seq, body, body_len)) {
			break;
		}
		/* Unknown command type: still answered under the 0x40|type convention so the
		 * host can correlate via seq. */
		send_rsp(type, seq, P2P_STATUS_UNSUPPORTED, NULL, 0);
		break;
	}
}
