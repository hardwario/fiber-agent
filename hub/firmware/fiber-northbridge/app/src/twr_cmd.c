/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway commands, see twr_cmd.h. Decode (twr_link.c), apply to the core under the
 * runtime's lock (twr_gwc.c), answer RSP = 0x40|cmd with the status byte first. Runs on the
 * main thread (uart_link_poll -> proto_on_frame).
 */
#include "twr_cmd.h"

#include "proto.h"
#include "radio_ctl.h"
#include "twr_gwc.h"
#include "twr_link.h"
#include "twr_rt.h"
#include "uart_link.h"

#include <string.h>

/* The largest RSP is TWR_NODE_LIST's. */
#define TWR_RSP_MAX_PAYLOAD \
	(P2P_TWR_NODE_LIST_HDR_LEN + P2P_TWR_NODE_LIST_MAX * P2P_TWR_NODE_LIST_ENTRY_LEN)

static void send_rsp(uint8_t cmd, uint8_t seq, enum p2p_status status, const uint8_t *payload,
		     size_t payload_len)
{
	uint8_t body[1 + TWR_RSP_MAX_PAYLOAD];
	size_t n = 0;

	body[n++] = (uint8_t)status;
	if (status == P2P_STATUS_OK && payload_len > 0 && payload_len <= TWR_RSP_MAX_PAYLOAD) {
		memcpy(&body[n], payload, payload_len);
		n += payload_len;
	}
	uart_link_send(P2P_RSP(cmd), seq, body, n);
}

static enum p2p_status do_start(const uint8_t *b, size_t n)
{
	struct twr_start_cmd c;
	enum p2p_status st = twr_link_dec_start(b, n, &c);
	struct twr_gwc *g;

	if (st != P2P_STATUS_OK) {
		return st;
	}
	if (radio_ctl_state() == P2P_NB_BOOT) {
		return P2P_STATUS_INVALID_STATE;
	}
	g = twr_rt_lock();
	st = twr_gwc_start(g, &(struct twr_gwc_params){
		.net_id = c.net_id,
		.turnaround_ms = c.turnaround_ms,
		.dl_gap_ms = c.dl_gap_ms,
		.flags = c.flags,
	});
	twr_rt_unlock();
	if (st != P2P_STATUS_OK) {
		return st;
	}
	return twr_rt_enter_gateway();
}

/* SET_RADIO_CONFIG was accepted: the tables may be filled. */
static bool configured(void)
{
	return radio_ctl_state() != P2P_NB_BOOT;
}

static enum p2p_status do_node_add(const uint8_t *b, size_t n)
{
	struct twr_node_add_cmd c;
	enum p2p_status st = twr_link_dec_node_add(b, n, &c);

	if (st == P2P_STATUS_OK && !configured()) {
		st = P2P_STATUS_INVALID_STATE;
	}
	if (st == P2P_STATUS_OK) {
		st = twr_gwc_node_add(twr_rt_lock(), c.addr, c.key, c.last_seen, c.flags);
		twr_rt_unlock();
	}
	/* The command body held a session key on the stack: do not leave it there. */
	memset(&c, 0, sizeof(c));
	__asm__ volatile("" : : "r"(&c) : "memory");
	return st;
}

static enum p2p_status do_node_remove(const uint8_t *b, size_t n)
{
	uint32_t addr;
	enum p2p_status st = twr_link_dec_node_remove(b, n, &addr);

	if (st == P2P_STATUS_OK) {
		st = twr_gwc_node_remove(twr_rt_lock(), addr);
		twr_rt_unlock();
	}
	return st;
}

static enum p2p_status do_ctr_block(const uint8_t *b, size_t n)
{
	struct twr_ctr_block_cmd c;
	enum p2p_status st = twr_link_dec_ctr_block(b, n, &c);

	if (st == P2P_STATUS_OK && !configured()) {
		st = P2P_STATUS_INVALID_STATE;
	}
	if (st == P2P_STATUS_OK) {
		st = twr_gwc_ctr_block(twr_rt_lock(), c.first, c.last);
		twr_rt_unlock();
		twr_rt_kick();
	}
	return st;
}

static enum p2p_status do_queue_push(const uint8_t *b, size_t n)
{
	struct twr_queue_push_cmd c;
	enum p2p_status st = twr_link_dec_queue_push(b, n, &c);

	if (st == P2P_STATUS_OK && !configured()) {
		st = P2P_STATUS_INVALID_STATE;
	}
	if (st == P2P_STATUS_OK) {
		uint64_t now = radio_ctl_now_ms();

		st = twr_gwc_queue_push(twr_rt_lock(), c.addr, c.item, c.ttl_s, c.flags, c.pt, c.len,
					now);
		twr_rt_unlock();
	}
	return st;
}

static enum p2p_status do_queue_drop(const uint8_t *b, size_t n)
{
	struct twr_queue_drop_cmd c;
	enum p2p_status st = twr_link_dec_queue_drop(b, n, &c);

	if (st == P2P_STATUS_OK) {
		st = twr_gwc_queue_drop(twr_rt_lock(), c.addr, c.item);
		twr_rt_unlock();
	}
	return st;
}

static void do_node_list(uint8_t seq, const uint8_t *b, size_t n)
{
	uint8_t payload[TWR_RSP_MAX_PAYLOAD];
	uint16_t start;
	size_t len;

	if (twr_link_dec_node_list(b, n, &start) != P2P_STATUS_OK) {
		send_rsp(P2P_CMD_TWR_NODE_LIST, seq, P2P_STATUS_BAD_PARAM, NULL, 0);
		return;
	}
	len = twr_gwc_node_list(twr_rt_lock(), start, payload);
	twr_rt_unlock();
	send_rsp(P2P_CMD_TWR_NODE_LIST, seq, P2P_STATUS_OK, payload, len);
}

/* TWR_EVT_ACK: no RSP (link §5.1). A body of the wrong length cannot be answered BAD_PARAM;
 * it is ignored like an ack outside the window (ack_ignored, `p2p stats`), and the ring
 * simply resends. */
static void do_evt_ack(const uint8_t *b, size_t n)
{
	struct twr_gwc *g = twr_rt_lock();

	if (n == P2P_TWR_EVT_ACK_BODY_LEN) {
		(void)twr_evq_ack(&g->evq, b[0]);
	} else {
		g->evq.ack_ignored++;
	}
	twr_rt_unlock();
}

/* TWR_TIME_SYNC: the wall clock anchor for TimeAns in the ACK (#470 §13.5). Accepted in any
 * state; the core keeps it until the next one or a reset. */
static enum p2p_status do_time_sync(const uint8_t *b, size_t n)
{
	uint64_t now_ms;
	uint64_t unix_ms;
	enum p2p_status st = twr_link_dec_time_sync(b, n, &now_ms, &unix_ms);

	if (st == P2P_STATUS_OK) {
		uint64_t nb_now = radio_ctl_now_ms();

		st = twr_gwc_time_sync(twr_rt_lock(), now_ms, unix_ms, nb_now);
		twr_rt_unlock();
	}
	return st;
}

bool twr_cmd_handle(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len)
{
	enum p2p_status st;

	switch (type) {
	case P2P_CMD_TWR_EVT_ACK:
		do_evt_ack(body, body_len);
		return true;
	case P2P_CMD_TWR_START:
		st = do_start(body, body_len);
		break;
	case P2P_CMD_TWR_NODE_ADD:
		st = do_node_add(body, body_len);
		break;
	case P2P_CMD_TWR_NODE_REMOVE:
		st = do_node_remove(body, body_len);
		break;
	case P2P_CMD_TWR_CTR_BLOCK:
		st = do_ctr_block(body, body_len);
		break;
	case P2P_CMD_TWR_QUEUE_PUSH:
		st = do_queue_push(body, body_len);
		break;
	case P2P_CMD_TWR_QUEUE_DROP:
		st = do_queue_drop(body, body_len);
		break;
	case P2P_CMD_TWR_NODE_LIST:
		do_node_list(seq, body, body_len);
		return true;
	case P2P_CMD_TWR_TIME_SYNC:
		st = do_time_sync(body, body_len);
		break;
	default:
		return false;
	}
	send_rsp(type, seq, st, NULL, 0);
	return true;
}
