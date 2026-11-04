/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway core, see twr_gwc.h.
 */
#include "twr_gwc.h"
#include "twr_link.h"

#include <string.h>

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

/* ---- counter block ------------------------------------------------------------------ */

void twr_ctr_set(struct twr_ctr *c, uint32_t first, uint32_t last)
{
	c->ever = true;
	c->first = first;
	c->last = last;
	c->next = first;
	c->low_sent = false;
	c->low_due = false;
}

uint32_t twr_ctr_left(const struct twr_ctr *c)
{
	if (!c->ever || c->next > c->last) {
		return 0;
	}
	return c->last - c->next + 1u;
}

bool twr_ctr_take(struct twr_ctr *c, uint32_t *ctr)
{
	uint32_t size;

	if (twr_ctr_left(c) == 0) {
		return false;
	}
	*ctr = c->next++;
	/* size - 1 cannot overflow: last < 2^32 - 1. */
	size = c->last - c->first + 1u;
	if (!c->low_sent && (uint64_t)twr_ctr_left(c) * 4u < (uint64_t)size) {
		c->low_sent = true;
		c->low_due = true;
	}
	return true;
}

bool twr_ctr_low_take(struct twr_ctr *c, uint32_t *next, uint32_t *last)
{
	if (!c->low_due) {
		return false;
	}
	c->low_due = false;
	*next = c->next;
	*last = c->last;
	return true;
}

/* ---- core ----------------------------------------------------------------------------- */

void twr_gwc_init(struct twr_gwc *g)
{
	memset(g, 0, sizeof(*g));
	twr_peers_init(&g->peers);
	twr_dlq_init(&g->dlq);
	twr_evq_init(&g->evq);
	g->max_frame = (uint8_t)TWR_GWC_DL_MAX_FRAME;
}

void twr_gwc_set_max_frame(struct twr_gwc *g, uint8_t max_frame)
{
	g->max_frame = max_frame;
}

size_t twr_gwc_dl_max_pt(const struct twr_gwc *g)
{
	size_t overhead = TWR_HDR_LEN + TWR_TAG_LEN;

	if (g->max_frame <= overhead) {
		return 0;
	}
	return g->max_frame - overhead < P2P_TWR_DL_MAX_PT ? g->max_frame - overhead
							   : P2P_TWR_DL_MAX_PT;
}

static void evt_fill(struct twr_tx_evt *e, const struct twr_dl *dl, uint8_t outcome)
{
	e->addr = dl->addr;
	e->item = dl->item;
	e->outcome = outcome;
	e->gw_counter = dl->gw_counter;
	e->node_ack_counter = 0;
	e->ack_rssi = (int8_t)P2P_TWR_RSSI_NONE;
}

/* ---- host commands -------------------------------------------------------------------- */

enum p2p_status twr_gwc_start(struct twr_gwc *g, const struct twr_gwc_params *p)
{
	if (!g->ctr.ever) {
		return P2P_STATUS_INVALID_STATE;
	}
	if (twr_peers_find(&g->peers, p->net_id) != NULL) {
		return P2P_STATUS_BAD_PARAM;
	}
	g->p = *p;
	g->started = true;
	return P2P_STATUS_OK;
}

/* Constant time: the sent key is compared with a stored one. */
static bool key_eq(const uint8_t a[TWR_AES_KEY_LEN], const uint8_t b[TWR_AES_KEY_LEN])
{
	uint8_t d = 0;

	for (size_t i = 0; i < TWR_AES_KEY_LEN; i++) {
		d |= a[i] ^ b[i];
	}
	return d == 0;
}

enum p2p_status twr_gwc_node_add(struct twr_gwc *g, uint32_t addr,
				 const uint8_t key[TWR_AES_KEY_LEN], uint32_t last_seen,
				 uint8_t flags)
{
	const struct twr_peer *old;
	bool same_key;

	if (g->started && addr == g->p.net_id) {
		return P2P_STATUS_BAD_PARAM;
	}
	old = twr_peers_find(&g->peers, addr);
	same_key = old != NULL && key_eq(old->key, key);
	if (same_key && (flags & P2P_TWR_NODE_KEEP_NEWER) != 0 && old->last_seen > last_seen) {
		/* A restore with the host's persisted last_seen while this NB kept running
		 * (link §4.2): its lane never moves back, or frames it accepted since would
		 * pass again. */
		last_seen = old->last_seen;
	}
	switch (twr_peers_upsert(&g->peers, addr, key, last_seen,
				 flags & (uint8_t)~P2P_TWR_NODE_KEEP_NEWER)) {
	case TWR_PEERS_FULL:
		return P2P_STATUS_NO_SPACE;
	case TWR_PEERS_REPLACED:
		/* A new key (a re-join): its lane restarts at last_seen and whatever was queued
		 * under the old key is gone. The same key (a restore) keeps the queue, so an
		 * in-flight downlink is not sent twice. */
		if (!same_key) {
			twr_dlq_drop(&g->dlq, addr, 0);
		}
		return P2P_STATUS_OK;
	default:
		return P2P_STATUS_OK;
	}
}

enum p2p_status twr_gwc_node_remove(struct twr_gwc *g, uint32_t addr)
{
	if (!twr_peers_remove(&g->peers, addr)) {
		return P2P_STATUS_NOT_FOUND;
	}
	twr_dlq_drop(&g->dlq, addr, 0);
	return P2P_STATUS_OK;
}

enum p2p_status twr_gwc_ctr_block(struct twr_gwc *g, uint32_t first, uint32_t last)
{
	uint32_t next = g->ctr.next;
	bool ever = g->ctr.ever;

	/* The NB never goes below the old next (#470 link §5.1): a block that ends before it
	 * would reuse spent counters. */
	if (ever && last < next) {
		return P2P_STATUS_BAD_PARAM;
	}
	twr_ctr_set(&g->ctr, first, last);
	if (ever && next > first) {
		g->ctr.next = next;
	}
	return P2P_STATUS_OK;
}

enum p2p_status twr_gwc_queue_push(struct twr_gwc *g, uint32_t addr, uint16_t item,
				   uint16_t ttl_s, uint8_t flags, const uint8_t *pt, size_t len,
				   uint64_t now_ms)
{
	struct twr_dl *e;
	int idx;

	if (twr_peers_find(&g->peers, addr) == NULL) {
		return P2P_STATUS_NOT_FOUND;
	}
	if (len > P2P_TWR_DL_MAX_PT || len > twr_gwc_dl_max_pt(g)) {
		return P2P_STATUS_BAD_PARAM;
	}
	idx = twr_dlq_find(&g->dlq, addr, item);
	if (idx >= 0 && g->dlq.e[idx].state != TWR_DL_QUEUED) {
		return P2P_STATUS_BUSY;
	}
	if (idx < 0) {
		idx = twr_dlq_alloc(&g->dlq);
		if (idx < 0) {
			return P2P_STATUS_NO_SPACE;
		}
	}
	e = &g->dlq.e[idx];
	e->addr = addr;
	e->item = item;
	e->flags = flags;
	e->len = (uint8_t)len;
	memcpy(e->pt, pt, len);
	e->expires_ms = now_ms + (uint64_t)ttl_s * 1000u;
	return P2P_STATUS_OK;
}

enum p2p_status twr_gwc_queue_drop(struct twr_gwc *g, uint32_t addr, uint16_t item)
{
	if (twr_peers_find(&g->peers, addr) == NULL) {
		return P2P_STATUS_NOT_FOUND;
	}
	if (twr_dlq_drop(&g->dlq, addr, item) == 0 && item != 0) {
		return P2P_STATUS_NOT_FOUND;
	}
	return P2P_STATUS_OK;
}

enum p2p_status twr_gwc_time_sync(struct twr_gwc *g, uint64_t now_ms, uint64_t unix_ms,
				  uint64_t nb_now_ms)
{
	if (now_ms > nb_now_ms) {
		return P2P_STATUS_BAD_PARAM;
	}
	g->anchor = (struct twr_time_anchor){
		.valid = true,
		.now_ms = now_ms,
		.unix_ms = unix_ms,
	};
	return P2P_STATUS_OK;
}

void twr_gwc_info(const struct twr_gwc *g, struct twr_info *out)
{
	const struct twr_gwc_stats *s = &g->st;
	uint32_t *c = out->ctr;

	if (g->started) {
		out->net_id = g->p.net_id;
		out->turnaround_ms = g->p.turnaround_ms;
		out->dl_gap_ms = g->p.dl_gap_ms;
	}
	c[P2P_INFO_RX_TOTAL] = s->rx_total;
	c[P2P_INFO_RX_FRESH] = s->fresh;
	c[P2P_INFO_RX_DUP] = s->dup;
	c[P2P_INFO_RX_REPLAY] = s->replay;
	c[P2P_INFO_RX_MIC_FAIL] = s->mic_fail;
	c[P2P_INFO_RX_UNKNOWN_SRC] = s->unknown_src;
	c[P2P_INFO_RX_JOIN_FWD] = s->join_fwd;
	c[P2P_INFO_RX_NOT_FOR_US] = s->not_for_us;
	c[P2P_INFO_RX_BAD_HDR] = s->bad_hdr;
	c[P2P_INFO_ACKS_SENT] = s->acks_sent;
	c[P2P_INFO_ACKS_LATE] = s->acks_late;
	c[P2P_INFO_ACKS_SUPPRESSED] = s->acks_suppressed;
	c[P2P_INFO_DL_SENT] = s->dl_sent;
	c[P2P_INFO_DL_DELIVERED] = s->dl_delivered;
	c[P2P_INFO_DL_NOT_DELIVERED] = s->dl_not_delivered;
	c[P2P_INFO_DL_EXPIRED] = s->dl_expired;
	c[P2P_INFO_EVT_RESENT] = g->evq.resent;
	out->peers = g->peers.n;
	out->peers_max = TWR_PEERS_MAX;
	out->queue_used = (uint16_t)twr_dlq_used(&g->dlq);
	out->queue_max = TWR_DLQ_MAX;
	out->evt_ring_used = twr_evq_used(&g->evq);
	out->evt_ring_max = TWR_EVQ_MAX;
	out->evt_head_seq = twr_evq_expect_seq(&g->evq);
	out->ctr_next = g->ctr.ever ? g->ctr.next : 0;
	out->ctr_last = g->ctr.ever ? g->ctr.last : 0;
	out->ack_ctrl_sent = s->ack_ctrl_sent;
	out->ack_time_no_anchor = s->ack_time_no_anchor;
}

size_t twr_gwc_node_list(const struct twr_gwc *g, uint16_t start, uint8_t *out)
{
	size_t n = 0;
	size_t p = P2P_TWR_NODE_LIST_HDR_LEN;
	uint16_t total = g->peers.n;
	uint16_t next;

	for (uint32_t i = start; i < total && n < P2P_TWR_NODE_LIST_MAX; i++, n++) {
		const struct twr_peer *peer = &g->peers.p[i];

		/* Never the key. */
		put_le32(&out[p], peer->addr);
		put_le32(&out[p + 4], peer->last_seen);
		out[p + 8] = peer->flags;
		p += P2P_TWR_NODE_LIST_ENTRY_LEN;
	}
	next = (uint32_t)start + n < total ? (uint16_t)(start + n) : P2P_TWR_NODE_LIST_END;
	put_le16(&out[0], total);
	put_le16(&out[2], next);
	out[4] = (uint8_t)n;
	return p;
}

/* ---- radio side ----------------------------------------------------------------------- */

static void no_counter_for_head(struct twr_gwc *g, int head, struct twr_tx_evt *evt,
				bool *has_evt)
{
	evt_fill(evt, &g->dlq.e[head], P2P_TWR_TX_NO_COUNTER);
	evt->gw_counter = 0;
	*has_evt = true;
	twr_dlq_free(&g->dlq, head);
	g->st.dl_no_counter++;
	g->st.dl_not_delivered++;
}

/* A node's TOWER ACK: authenticated, and fresh on the peer's lane. */
static void on_node_ack(struct twr_gwc *g, struct twr_peer *peer, const struct twr_hdr *h,
			const uint8_t *pt, size_t pt_len, struct twr_gwc_rx *out)
{
	struct twr_ack_meta m;
	int idx;

	if (!twr_ack_payload_parse(pt, pt_len, &m)) {
		g->st.bad_hdr++;
		return;
	}
	idx = twr_dlq_find_inflight(&g->dlq, peer->addr, m.acked_counter);
	if (idx < 0) {
		/* Not ours to act on; the lane stays where it is (a later frame moves it). */
		g->st.ack_unmatched++;
		return;
	}
	if (!twr_evq_room(&g->evq, 1, P2P_EVT_TWR_TX_BODY_LEN)) {
		/* No slot for DELIVERED: as if not heard. The lane stays; the node's ACK of a
		 * later downlink or its next uplink moves it. */
		g->st.rx_ring_full++;
		return;
	}
	peer->last_seen = h->counter;
	evt_fill(&out->tx_evt, &g->dlq.e[idx], P2P_TWR_TX_DELIVERED);
	out->tx_evt.node_ack_counter = h->counter;
	out->tx_evt.ack_rssi = m.has_rssi ? m.rssi : (int8_t)P2P_TWR_RSSI_NONE;
	out->has_tx_evt = true;
	out->action = TWR_RXA_NODE_ACK;
	twr_dlq_free(&g->dlq, idx);
	g->st.dl_delivered++;
}

static void seal_ack(struct twr_gwc *g, const struct twr_peer *peer, const struct twr_hdr *up,
		     const uint8_t *pt, size_t pt_len, const struct twr_rx_meta *meta,
		     uint64_t now_ms, struct twr_gwc_rx *out)
{
	uint8_t pl[TWR_ACK_PAYLOAD_LEN + TWR_ACKCTRL_TAIL_MAX];
	struct twr_ackctrl_res res;
	struct twr_ackctrl_in in;
	size_t tail;
	struct twr_hdr h;
	int head = twr_dlq_head(&g->dlq, peer->addr, now_ms, twr_gwc_dl_max_pt(g));
	uint32_t left = twr_ctr_left(&g->ctr);
	bool pending = head >= 0 && left >= 2;
	uint32_t ctr;
	size_t n;

	if (head >= 0 && !pending) {
		/* The downlink had its chance and there is no counter for it: the central takes
		 * NO_COUNTER as its cue for a new block (§3.4) and pushes the item again. */
		no_counter_for_head(g, head, &out->tx_evt, &out->has_tx_evt);
	}
	if (!twr_ctr_take(&g->ctr, &ctr)) {
		out->ack = P2P_TWR_ACK_NO_COUNTER;
		g->st.acks_suppressed++;
		return;
	}

	/* The counter is spent from here on, whatever happens to the frame (#470 §4). */
	h = (struct twr_hdr){
		.type = TWR_T_ACK,
		.flags = 0,
		.src = g->p.net_id,
		.dest = peer->addr,
		.counter = ctr,
	};
	twr_ack_payload_encode(up->counter, meta->rssi_dbm, pending, pl);
	/* Answers in the ACK (#470 §13.5), built afresh for every (re-)ACK: a repetition's
	 * TimeAns is the end of that repetition. */
	in = (struct twr_ackctrl_in){
		.rssi_dbm = meta->rssi_dbm,
		.snr_q = meta->snr_q,
		.sf = meta->sf,
		.t_rx_ms = meta->t_rx_ms,
		.counter = up->counter,
		.anchor = &g->anchor,
	};
	tail = twr_ackctrl_build(pt, pt_len, &in, &pl[TWR_ACK_PAYLOAD_LEN], &res);
	if (tail > 0) {
		pl[5] |= TWR_ACK_FLAG_CTRL;
	}
	g->st.ack_ctrl_lc += res.lc ? 1u : 0u;
	g->st.ack_ctrl_time += res.time ? 1u : 0u;
	g->st.ack_time_no_anchor += res.time_no_anchor ? 1u : 0u;
	n = twr_frame_seal(&g->aes, &h, pl, TWR_ACK_PAYLOAD_LEN + tail, TWR_TOWER_MAX_PAYLOAD,
			   out->ack_frame, sizeof(out->ack_frame));
	if (n == 0) {
		/* Unreachable with a valid key and payload; nothing goes on air. */
		out->ack = P2P_TWR_ACK_LATE;
		return;
	}
	out->send_ack = true;
	out->ack_pending = pending;
	out->ack_ctr = ctr;
	out->ack_len = (uint8_t)n;
	out->ack_ctrl = tail > 0;
	out->ack = (uint8_t)((pending ? P2P_TWR_ACK_PENDING : P2P_TWR_ACK_SENT) |
			     (tail > 0 ? P2P_TWR_ACK_CTRL : 0u) |
			     (res.time ? P2P_TWR_ACK_TIME : 0u));
}

uint8_t twr_gwc_ack_code(uint8_t ack, bool on_air, bool late)
{
	if (!on_air) {
		/* Radio error: no tail went out. */
		return P2P_TWR_ACK_LATE;
	}
	if (late) {
		return (uint8_t)(P2P_TWR_ACK_LATE | (ack & P2P_TWR_ACK_FLAGS));
	}
	return ack;
}

void twr_gwc_on_rx(struct twr_gwc *g, const uint8_t *frame, size_t len,
		   const struct twr_rx_meta *meta, uint64_t now_ms, struct twr_gwc_rx *out)
{
	struct twr_peer *peer;
	enum twr_replay rp;
	enum twr_err err;
	size_t hlen;
	size_t pt_len = 0;
	size_t n_evts;
	size_t evt_bytes;
	bool acks;

	out->action = TWR_RXA_DROP;
	out->hdr_valid = false;
	out->pt_len = 0;
	out->ack = P2P_TWR_ACK_NONE;
	out->send_ack = false;
	out->ack_pending = false;
	out->ack_ctr = 0;
	out->ack_len = 0;
	out->ack_ctrl = false;
	out->has_tx_evt = false;

	g->st.rx_total++;
	if (len > TWR_TOWER_MAX_FRAME) {
		/* One MTU on both PHY profiles (sticker-firmware #486 U-1): no node sends more, so a
		 * longer frame is noise or a foreign protocol. Not parsed, not forwarded. */
		g->st.rx_too_long++;
		return;
	}
	if (twr_hdr_parse(frame, len, &out->hdr, &hlen) != TWR_OK) {
		g->st.bad_hdr++;
		return;
	}
	out->hdr_valid = true;
	if (out->hdr.dest == 0) {
		/* JoinReq (or anything else sent to no one): the central opens it (§3.2). */
		g->st.join_fwd++;
		out->action = TWR_RXA_RAW;
		return;
	}
	if (out->hdr.dest != g->p.net_id) {
		g->st.not_for_us++;
		return;
	}
	peer = twr_peers_find(&g->peers, out->hdr.src);
	if (peer == NULL) {
		g->st.unknown_src++;
		out->action = TWR_RXA_RAW;
		return;
	}
	if (out->hdr.type != TWR_T_DATA && out->hdr.type != TWR_T_ACK) {
		/* Bulk, join and beacon frames from a peer are not part of P2. */
		g->st.bad_hdr++;
		return;
	}

	twr_aes128_init(&g->aes, peer->key);
	err = twr_frame_open(&g->aes, frame, len, &out->hdr, out->pt, sizeof(out->pt), &pt_len);
	if (err == TWR_E_MIC) {
		g->st.mic_fail++;
		return;
	}
	if (err != TWR_OK) {
		g->st.bad_hdr++;
		return;
	}

	rp = twr_peer_classify(peer, out->hdr.counter);
	if (rp == TWR_RP_REPLAY) {
		g->st.replay++;
		return;
	}
	if (out->hdr.type == TWR_T_ACK) {
		if (rp == TWR_RP_RETRANSMIT) {
			g->st.dup++;
			return;
		}
		on_node_ack(g, peer, &out->hdr, out->pt, pt_len, out);
		return;
	}

	acks = (out->hdr.flags & TWR_FLAG_CONFIRMED) != 0 &&
	       (peer->flags & P2P_TWR_NODE_HOME) != 0 &&
	       (g->p.flags & P2P_TWR_START_ACK_ENABLED) != 0;
	/* Room for every event this frame can raise: the uplink (fresh only) and, when it is
	 * ACKed, the head's NO_COUNTER or the downlink's outcome (2 kept, 1 is the most). */
	n_evts = (rp == TWR_RP_FRESH ? 1u : 0u) + (acks ? 2u : 0u);
	evt_bytes = (rp == TWR_RP_FRESH ? P2P_EVT_TWR_UPLINK_HDR_LEN + pt_len : 0u) +
		    (acks ? 2u * P2P_EVT_TWR_TX_BODY_LEN : 0u);
	if (n_evts > 0 && !twr_evq_room(&g->evq, n_evts, evt_bytes)) {
		/* link §4.1 backpressure: no ACK and no last_seen update, or the node's
		 * repetitions would be re-ACKed for data the host never got. */
		g->st.rx_ring_full++;
		if (rp == TWR_RP_FRESH) {
			g->st.acks_suppressed++;
		}
		return;
	}

	if (rp == TWR_RP_FRESH) {
		peer->last_seen = out->hdr.counter;
		g->st.fresh++;
		out->action = TWR_RXA_UPLINK;
		out->pt_len = (uint8_t)pt_len;
	} else {
		g->st.dup++;
		out->action = TWR_RXA_DUP;
	}

	if (acks) {
		seal_ack(g, peer, &out->hdr, out->pt, pt_len, meta, now_ms, out);
	}
}

void twr_gwc_ack_result(struct twr_gwc *g, bool on_air, bool late, bool ctrl)
{
	if (on_air) {
		g->st.acks_sent++;
	}
	if (on_air && ctrl) {
		g->st.ack_ctrl_sent++;
	}
	if (on_air && late) {
		g->st.acks_late++;
	}
}

size_t twr_gwc_dl_take(struct twr_gwc *g, uint32_t addr, uint64_t now_ms, uint8_t *frame,
		       struct twr_dl_ref *ref, struct twr_tx_evt *evt, bool *has_evt)
{
	const struct twr_peer *peer = twr_peers_find(&g->peers, addr);
	struct twr_dl *e;
	struct twr_hdr h;
	uint32_t ctr;
	size_t n;
	int head;

	*has_evt = false;
	if (peer == NULL) {
		return 0;
	}
	head = twr_dlq_head(&g->dlq, addr, now_ms, twr_gwc_dl_max_pt(g));
	if (head < 0) {
		return 0;
	}
	if (!twr_ctr_take(&g->ctr, &ctr)) {
		no_counter_for_head(g, head, evt, has_evt);
		return 0;
	}

	e = &g->dlq.e[head];
	h = (struct twr_hdr){
		.type = TWR_T_DATA,
		.flags = (e->flags & P2P_TWR_QUEUE_CONFIRMED) != 0 ? TWR_FLAG_CONFIRMED : 0,
		.src = g->p.net_id,
		.dest = addr,
		.counter = ctr,
	};
	twr_aes128_init(&g->aes, peer->key);
	n = twr_frame_seal(&g->aes, &h, e->pt, e->len, P2P_TWR_DL_MAX_PT, frame,
			   TWR_GWC_DL_MAX_FRAME);
	e->gw_counter = ctr;
	if (n == 0) {
		evt_fill(evt, e, P2P_TWR_TX_RADIO_ERR);
		*has_evt = true;
		twr_dlq_free(&g->dlq, head);
		g->st.dl_radio_err++;
		g->st.dl_not_delivered++;
		return 0;
	}
	e->state = TWR_DL_SENDING;
	*ref = (struct twr_dl_ref){
		.idx = head,
		.seq = e->seq,
		.addr = addr,
		.item = e->item,
		.gw_counter = ctr,
		.confirmed = (e->flags & P2P_TWR_QUEUE_CONFIRMED) != 0,
		.pt_len = e->len,
	};
	return n;
}

bool twr_gwc_dl_done(struct twr_gwc *g, const struct twr_dl_ref *ref, enum twr_dl_tx res,
		     uint64_t ack_deadline_ms, struct twr_tx_evt *evt)
{
	struct twr_dl *e;

	if (ref->idx < 0 || (size_t)ref->idx >= TWR_DLQ_MAX) {
		return false;
	}
	e = &g->dlq.e[ref->idx];
	if (e->state != TWR_DL_SENDING || e->seq != ref->seq) {
		/* Dropped (TWR_QUEUE_DROP, NODE_REMOVE, a replacing NODE_ADD) while on air. */
		return false;
	}
	switch (res) {
	case TWR_DL_TX_OK:
		g->st.dl_sent++;
		if (ref->confirmed) {
			e->state = TWR_DL_INFLIGHT;
			e->ack_deadline_ms = ack_deadline_ms;
			return false;
		}
		evt_fill(evt, e, P2P_TWR_TX_SENT);
		twr_dlq_free(&g->dlq, ref->idx);
		return true;
	case TWR_DL_TX_LATE:
		/* Sealed but never on air: the spent counter is simply skipped; the next attempt
		 * seals again with a fresh one. */
		e->state = TWR_DL_QUEUED;
		e->gw_counter = 0;
		g->st.dl_late++;
		return false;
	default:
		evt_fill(evt, e, P2P_TWR_TX_RADIO_ERR);
		twr_dlq_free(&g->dlq, ref->idx);
		g->st.dl_radio_err++;
		g->st.dl_not_delivered++;
		return true;
	}
}

size_t twr_gwc_housekeep(struct twr_gwc *g, uint64_t now_ms, struct twr_tx_evt *out,
			 size_t max)
{
	size_t max_pt = twr_gwc_dl_max_pt(g);
	size_t n = 0;

	for (size_t i = 0; i < TWR_DLQ_MAX && n < max; i++) {
		struct twr_dl *e = &g->dlq.e[i];

		if (!twr_evq_room(&g->evq, n + 1, (n + 1) * P2P_EVT_TWR_TX_BODY_LEN)) {
			/* The outcome waits in the queue until the host acks some events. */
			break;
		}

		if (e->state == TWR_DL_QUEUED && e->expires_ms <= now_ms) {
			evt_fill(&out[n++], e, P2P_TWR_TX_EXPIRED);
			g->st.dl_expired++;
		} else if (e->state == TWR_DL_QUEUED && e->len > max_pt) {
			/* SET_RADIO_CONFIG moved the ToA cap below this frame after it was queued:
			 * refused rather than cut off by the 4 s TX timeout (amendment 4). */
			evt_fill(&out[n++], e, P2P_TWR_TX_RADIO_ERR);
			g->st.dl_radio_err++;
			g->st.dl_not_delivered++;
		} else if (e->state == TWR_DL_INFLIGHT && e->ack_deadline_ms <= now_ms) {
			evt_fill(&out[n++], e, P2P_TWR_TX_NOT_DELIVERED);
			g->st.dl_not_delivered++;
		} else {
			continue;
		}
		twr_dlq_free(&g->dlq, (int)i);
	}
	return n;
}
