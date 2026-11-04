/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway runtime, the Northbridge image (proximos-v2
 * plan/control/radio/p2p_tower_gateway.md §2): owns the SX126x and drives the radio-free core
 * (twr_gwc.c) from it. Implements radio_ctl.h and twr_rt.h for cmd_dispatch.c / twr_cmd.c.
 *
 * States (enum p2p_nb_state): BOOT -> SET_RADIO_CONFIG -> CONFIGURED -> TWR_CTR_BLOCK +
 * TWR_START -> GATEWAY (continuous receive); RX_STOP returns to CONFIGURED with the tables
 * kept. Everything is RAM only: after EVT_BOOT the host rebuilds it.
 *
 * Threads:
 *   system workqueue  sx126x DIO1 work -> sx126x_twr_dio1_hook() (RxDone / TxDone stamps,
 *                     CRC and header errors) -> IrqProcess -> lora_rx_cb(): into rx_q.
 *   twr_wq (coop)     everything that touches the radio. rx_work: classify (twr_gwc_on_rx),
 *                     the ACK with SetTx gated to t_rx + turnaround_ms, EVT_TWR_UPLINK once
 *                     the ACK's fate is known, the downlink dl_gap_ms after the ACK's TxDone,
 *                     RX re-armed, then the remaining events. Also the host-scheduled TX
 *                     (TX_SCHEDULE, the JoinAccept), RX arm / release, the 100 ms
 *                     housekeeping (TTL, node-ACK timeouts, EVT_TWR_CTR_LOW, the event
 *                     ring's go-back-N resend, an RX re-arm retry) and radio_ctl_call() for
 *                     the `radio tune` shell.
 *   main              the host commands: the core under core_lock, the state and the radio
 *                     config, never the radio itself.
 *
 * Event delivery (#470 link §4.1): EVT_TWR_UPLINK and EVT_TWR_TX go through the core's event
 * ring (twr_evq.c), only ever pushed and sent from twr_wq, so the delivery seq order is the
 * wire order; the host releases them with TWR_EVT_ACK (main thread, twr_cmd.c).
 *
 * One clock for every t_ms on the link: k_cycle_get_64() in milliseconds (radio_ctl_now_ms);
 * the driver hook stamps are cycles of the same counter.
 *
 * RTT shell `p2p show|stats|peers|queue|duty|listen` for the bench and the
 * factory; it never prints a key. With
 * CONFIG_NB_TWR_RX_INJECT (bench only, GET_INFO build_flags b2) also `p2p rx_inject`.
 */
#include "radio_ctl.h"
#include "twr_gateway.h"
#include "twr_rt.h"

#include "diag.h"
#include "proto.h"
#include "twr_airtime.h"
#include "twr_frame.h"
#include "twr_gwc.h"
#include "twr_link.h"
#include "uart_link.h"
#if defined(CONFIG_NB_RADIO_TUNE)
#include "radio_tune_hw.h"
#endif

#include <stdlib.h>
#include <string.h>
#include <zephyr/device.h>
#include <zephyr/drivers/lora.h>
#include <zephyr/kernel.h>
#include <zephyr/shell/shell.h>
#include <zephyr/sys/util.h>

#include <zephyr/logging/log.h>
LOG_MODULE_REGISTER(twr_gw, CONFIG_LOG_DEFAULT_LEVEL);

/* SX126x synthesizer range (datasheet §3). */
#define RADIO_FREQ_MIN_HZ   150000000u
#define RADIO_FREQ_MAX_HZ   960000000u

/* The TX powers this board's PA can produce, from the devicetree the driver reads. Zephyr's
 * sx126x_stm32wl.c sx126x_set_tx_params() clamps a request above the output's max-power and
 * maps the rest to the PA register as base - (max - dBm), floored at -17 (RFO_LP) or -9
 * (RFO_HP). A request outside [MIN, MAX] would be clamped silently: BAD_PARAM instead (#470
 * link §2, no clamping). This board: RFO_LP, rfo-lp-max-power = 15 -> -16..15 dBm. */
#define LORA_NODE DT_ALIAS(lora0)
#if DT_ENUM_IDX(LORA_NODE, power_amplifier_output) == 0 /* rfo-lp */
#define RADIO_POWER_MAX_DBM DT_PROP(LORA_NODE, rfo_lp_max_power)
#define RADIO_POWER_BASE    (RADIO_POWER_MAX_DBM == 10 ? 13 : 14)
#define RADIO_POWER_REG_MIN (-17)
#else /* rfo-hp */
#define RADIO_POWER_MAX_DBM DT_PROP(LORA_NODE, rfo_hp_max_power)
#define RADIO_POWER_BASE    (RADIO_POWER_MAX_DBM == 14 ? 14 : 22)
#define RADIO_POWER_REG_MIN (-9)
#endif
#define RADIO_POWER_MIN_DBM (RADIO_POWER_MAX_DBM - RADIO_POWER_BASE + RADIO_POWER_REG_MIN)
/* #470 §3.3 fixes the gateway at 14 dBm. */
BUILD_ASSERT(RADIO_POWER_MIN_DBM <= 14 && RADIO_POWER_MAX_DBM >= 14);

/* sx12xx_lora_config() arms loramac's TX timeout at a fixed 4000 ms: a longer frame is cut
 * off mid-air and lora_send() never returns. No frame over ~3.95 s ToA is accepted (amendment
 * 4): SET_RADIO_CONFIG, TWR_QUEUE_PUSH and TX_SCHEDULE answer BAD_PARAM instead. */
#define TX_TOA_MAX_US 3950000u

/* twr_wq sleeps until this long before a TX deadline, then lora_send(); the TX hook spins the
 * rest. Covers lora_send()'s SPI commands ahead of SetTx (~0.5 ms) plus a 1 ms tick. */
#define GATE_LEAD_US 2500u
/* Never spin longer than this in the TX hook (a deadline further out is a bug; go now). */
#define GATE_MAX_SPIN_US 20000u

/* The node's receive window after its TX (STICKER P1 net layer, sticker-firmware
 * hynek/tower-p1-node 59c08400, app/src/app_radio_p2p.c:129-134 and p2p_twr_window_ms() at
 * :774-780): the peer's turnaround + the awaited frame's ToA + NODE_WINDOW_SYMBOLS symbols of
 * RxDone latency + NODE_WINDOW_MARGIN_MS, never under 200 ms. A frame whose SetTx lands more
 * than the symbols + margin after its slot may miss the window:
 *   ACK       slot = RxDone + turnaround_ms; later is still sent, counted in acks_late;
 *   downlink  slot = the ACK's TxDone + dl_gap_ms; later is not sent, it goes back to the
 *             queue (its counter spent). */
#define NODE_WINDOW_SYMBOLS   3u
#define NODE_WINDOW_MARGIN_MS 20u
/* A confirmed downlink's TOWER ACK comes one node turnaround (<= 60 ms) after the downlink's
 * TxDone and takes ToA(28 B); on top of that 3 symbols and this margin. */
#define DL_ACK_MARGIN_MS 100u
/* TX_SCHEDULE: twr_wq wakes this long before t_ms to prepare the radio, then gates SetTx to
 * t_ms. Past t_ms + SCHED_LATE_MS the frame is not sent: EVT_TX_DONE LATE. */
#define SCHED_LEAD_MS 8u
#define SCHED_LATE_MS 10u

#define HK_PERIOD_MS   100u
#define REARM_RETRY_MS 1000u

/* SX126x IRQ status bits (datasheet table 13-29; loramac's sx126x.h is private to its lib). */
#define SX_IRQ_TX_DONE      0x0001u
#define SX_IRQ_RX_DONE      0x0002u
#define SX_IRQ_HEADER_ERROR 0x0020u
#define SX_IRQ_CRC_ERROR    0x0040u

/* patches/lora/sx126x.c, NB_TWR_HOOKS */
void sx126x_twr_standby_xosc(void);

static const struct device *const lora_dev = DEVICE_DT_GET(DT_ALIAS(lora0));

/* ---- state, radio config ------------------------------------------------------------ */

static struct k_spinlock st_lock;
static enum p2p_nb_state nb_state; /* st_lock */
static struct radio_config rcfg;   /* st_lock; valid from CONFIGURED on */
static bool radio_ready;
static bool wq_started;

/* ---- the core -------------------------------------------------------------------------- */

static struct twr_gwc core;
static K_MUTEX_DEFINE(core_lock);

/* ---- radio counters and stamps ---------------------------------------------------------- */

struct gw_radio_stats {
	uint32_t crc_err;    /* system workqueue */
	uint32_t hdr_err;    /* system workqueue */
	uint32_t rx_overrun; /* system workqueue */
	uint32_t tx_total;   /* twr_wq, as all below */
	uint16_t turn_last_us;
	uint16_t turn_max_us;
	uint32_t sched_ok;
	uint32_t sched_late;
	uint32_t sched_err;
	uint32_t sched_cancelled;
	uint32_t ack_toa_last_us;
	uint32_t dl_toa_last_us;
	uint32_t dl_dt_last_us; /* ACK TxDone -> DL SetTx */
};

static struct gw_radio_stats rs;

static struct twr_duty duty;
static struct k_spinlock duty_lock;

static volatile uint64_t rx_done_cyc;
static volatile uint64_t tx_done_cyc;
static volatile uint64_t tx_start_cyc;
static volatile uint64_t tx_gate_cyc;
static volatile bool tx_gate_armed;

/* Watchdog gate (main.c): uptime the running TX started, 0 when none, and its budget. */
static volatile int64_t tx_started_ms;
static volatile uint32_t tx_budget_ms;

/* ---- receive queue, work queue ---------------------------------------------------------- */

struct rx_msg {
	uint64_t t_cyc;
	int16_t rssi;
	int8_t snr;
	uint8_t len;
	uint8_t data[P2P_MAX_RADIO_FRAME_LEN];
};

K_MSGQ_DEFINE(rx_q, sizeof(struct rx_msg), 8, 4);

K_THREAD_STACK_DEFINE(twr_wq_stack, 3072);
static struct k_work_q twr_wq;
static struct k_work rx_work;
static struct k_work arm_work;
static struct k_work stop_work;
static struct k_work hk_work;
static struct k_timer hk_timer;

static bool rx_armed;              /* twr_wq only */
static int64_t rearm_last_try_ms;  /* twr_wq only */

/* ---- the host-scheduled TX (one slot) --------------------------------------------------- */

static struct {
	bool active;   /* accepted, EVT_TX_DONE not yet sent */
	bool started;  /* twr_wq took it: too late for TX_CANCEL */
	uint8_t tx_id;
	uint64_t t_ms; /* P2P_TX_IMMEDIATE or the aim */
	uint8_t len;
	uint8_t frame[P2P_MAX_RADIO_FRAME_LEN];
} sched;
/* Also guards busy_until_ms: the end of the gateway's own ACK / downlink in progress. */
static struct k_spinlock sched_lock;
static uint64_t busy_until_ms;
static struct k_timer sched_timer;
static struct k_work sched_work;

/* ---- helpers ---------------------------------------------------------------------------- */

static uint64_t now_ms(void)
{
	return k_cyc_to_ms_floor64(k_cycle_get_64());
}

static uint64_t cyc_ms(uint64_t cyc)
{
	return k_cyc_to_ms_floor64(cyc);
}

static uint32_t cyc_delta_us(uint64_t later, uint64_t earlier)
{
	if (later <= earlier) {
		return 0;
	}
	return (uint32_t)MIN(k_cyc_to_us_floor64(later - earlier), (uint64_t)UINT32_MAX);
}

static enum p2p_nb_state state_get(void)
{
	k_spinlock_key_t key = k_spin_lock(&st_lock);
	enum p2p_nb_state s = nb_state;

	k_spin_unlock(&st_lock, key);
	return s;
}

static struct radio_config cfg_get(void)
{
	k_spinlock_key_t key = k_spin_lock(&st_lock);
	struct radio_config c = rcfg;

	k_spin_unlock(&st_lock, key);
	return c;
}

static bool bw_of(uint8_t wire, uint32_t *hz, enum lora_signal_bandwidth *bw)
{
	switch (wire) {
	case P2P_BW_125K:
		*hz = 125000u;
		*bw = BW_125_KHZ;
		return true;
	case P2P_BW_250K:
		*hz = 250000u;
		*bw = BW_250_KHZ;
		return true;
	case P2P_BW_500K:
		*hz = 500000u;
		*bw = BW_500_KHZ;
		return true;
	default:
		return false;
	}
}

static struct twr_phy phy_of(const struct radio_config *c)
{
	uint32_t hz = 125000u;
	enum lora_signal_bandwidth bw;

	(void)bw_of(c->bw, &hz, &bw);
	return (struct twr_phy){
		.sf = c->sf,
		.bw_hz = hz,
		.cr = c->cr,
		.preamble = c->preamble_len,
		.explicit_hdr = true, /* the driver's defaults: explicit header, CRC on */
		.crc = true,
	};
}

static uint32_t toa_us(const struct radio_config *c, size_t len)
{
	struct twr_phy p = phy_of(c);

	return twr_lora_toa_us(&p, len);
}

static uint32_t tsym_us(const struct radio_config *c)
{
	struct twr_phy p = phy_of(c);

	return twr_lora_tsym_us(&p);
}

static struct lora_modem_config modem_cfg(const struct radio_config *c, bool tx)
{
	uint32_t hz;
	enum lora_signal_bandwidth bw = BW_125_KHZ;

	(void)bw_of(c->bw, &hz, &bw);
	return (struct lora_modem_config){
		.frequency = c->frequency_hz,
		.bandwidth = bw,
		.datarate = (enum lora_datarate)c->sf,
		.coding_rate = (enum lora_coding_rate)c->cr,
		.preamble_len = c->preamble_len,
		.tx_power = c->tx_power_dbm,
		.tx = tx,
		.iq_inverted = (c->flags & P2P_RADIO_FLAG_IQ_INVERTED) != 0,
		.public_network = (c->flags & P2P_RADIO_FLAG_PUBLIC_SYNCWORD) != 0,
		.packet_crc_disable = false,
	};
}

/* ---- events ----------------------------------------------------------------------------- */

/* twr_wq only */
static uint8_t evt_body[P2P_MAX_BODY_LEN];
static struct twr_evq_out evt_out;
/* Ring events that did not fit although the frame was admitted: a bug guard, shell only. */
static uint32_t evt_ring_lost;

/* Put every ring event not yet on the link on it, oldest first. twr_wq only. */
static void ring_flush(void)
{
	for (;;) {
		bool more;

		k_mutex_lock(&core_lock, K_FOREVER);
		more = twr_evq_take_unsent(&core.evq, now_ms(), &evt_out);
		k_mutex_unlock(&core_lock);
		if (!more) {
			return;
		}
		uart_link_send(evt_out.type, evt_out.seq, evt_out.body, evt_out.len);
	}
}

/* Append a ring event; the caller holds core_lock and calls ring_flush() after. */
static void ring_push_locked(uint8_t type, const uint8_t *body, size_t len)
{
	uint8_t seq;

	if (!twr_evq_push(&core.evq, type, body, len, &seq)) {
		evt_ring_lost++;
		return;
	}
	uart_link_set_event_seq(seq);
}

static void ring_push(uint8_t type, const uint8_t *body, size_t len)
{
	k_mutex_lock(&core_lock, K_FOREVER);
	ring_push_locked(type, body, len);
	k_mutex_unlock(&core_lock);
	ring_flush();
}

/* Go-back-N from the oldest unacked event, at most one pass per resend timeout (twr_evq.h). */
static void ring_resend(void)
{
	uint64_t now = now_ms();
	size_t n;

	k_mutex_lock(&core_lock, K_FOREVER);
	n = twr_evq_resend_begin(&core.evq, now);
	k_mutex_unlock(&core_lock);
	while (n > 0) {
		bool more;

		k_mutex_lock(&core_lock, K_FOREVER);
		more = twr_evq_resend_next(&core.evq, now, &evt_out);
		k_mutex_unlock(&core_lock);
		if (!more) {
			return;
		}
		uart_link_send(evt_out.type, evt_out.seq, evt_out.body, evt_out.len);
	}
}

static void send_tx_evt(const struct twr_tx_evt *e)
{
	uint8_t b[P2P_EVT_TWR_TX_BODY_LEN];

	ring_push(P2P_EVT_TWR_TX, b, twr_link_enc_tx(e, b));
}

static void send_tx_done(uint8_t tx_id, uint8_t status, uint64_t t_actual_ms)
{
	uint8_t b[P2P_EVT_TX_DONE_BODY_LEN];

	uart_link_send_event(P2P_EVT_TX_DONE, b, twr_link_enc_tx_done(tx_id, status, t_actual_ms, b));
}

static void send_raw_rx(const struct rx_msg *m)
{
	size_t n = twr_link_enc_rx(cyc_ms(m->t_cyc), m->rssi, twr_link_snr_q(m->snr), m->data,
				   m->len, evt_body, sizeof(evt_body));

	if (n > 0) {
		uart_link_send_event(P2P_EVT_RX, evt_body, n);
	}
}

static void send_uplink(const struct rx_msg *m, const struct twr_gwc_rx *o)
{
	struct twr_uplink_evt e = {
		.t_ms = cyc_ms(m->t_cyc),
		.rssi_dbm = m->rssi,
		.snr_q_db = twr_link_snr_q(m->snr),
		.frame_flags = o->hdr.flags,
		.addr = o->hdr.src,
		.counter = o->hdr.counter,
		.ack = o->ack,
		.len = o->pt_len,
		.pt = o->pt,
	};
	size_t n = twr_link_enc_uplink(&e, evt_body, sizeof(evt_body));

	if (n > 0) {
		ring_push(P2P_EVT_TWR_UPLINK, evt_body, n);
	}
}

/* Housekeeping events (TTL, node-ACK timeouts, ToA cap) and EVT_TWR_CTR_LOW. twr_wq only. */
static void emit_core_events(void)
{
	struct twr_tx_evt evts[4];
	uint32_t next = 0;
	uint32_t last = 0;
	bool low = false;
	size_t n;

	do {
		k_mutex_lock(&core_lock, K_FOREVER);
		/* Bounded by the ring's room; pushed under the same lock so the room holds. */
		n = twr_gwc_housekeep(&core, now_ms(), evts, ARRAY_SIZE(evts));
		for (size_t i = 0; i < n; i++) {
			uint8_t b[P2P_EVT_TWR_TX_BODY_LEN];

			ring_push_locked(P2P_EVT_TWR_TX, b, twr_link_enc_tx(&evts[i], b));
		}
		if (!low) {
			low = twr_ctr_low_take(&core.ctr, &next, &last);
		}
		k_mutex_unlock(&core_lock);
		ring_flush();
	} while (n == ARRAY_SIZE(evts));

	if (low) {
		uint8_t b[P2P_EVT_TWR_CTR_LOW_BODY_LEN];

		uart_link_send_event(P2P_EVT_TWR_CTR_LOW, b, twr_link_enc_ctr_low(next, last, b));
	}
}

/* ---- driver hooks (patches/lora/sx126x.c, NB_TWR_HOOKS) ---------------------------------- */

/* Start of the DIO1 work item, before IrqProcess, with the pending IRQ status. */
void sx126x_twr_dio1_hook(uint16_t irq)
{
	uint64_t now = k_cycle_get_64();

	if ((irq & SX_IRQ_TX_DONE) != 0) {
		tx_done_cyc = now;
	}
	if ((irq & SX_IRQ_RX_DONE) != 0) {
		rx_done_cyc = now;
		if ((irq & SX_IRQ_CRC_ERROR) != 0) {
			rs.crc_err++;
		}
	}
	if ((irq & SX_IRQ_HEADER_ERROR) != 0) {
		rs.hdr_err++;
	}
}

/* SX126xSetOperatingMode(MODE_TX), right before the SetTx command: hold it to the gate. */
void sx126x_twr_tx_hook(void)
{
	if (tx_gate_armed) {
		uint64_t gate = tx_gate_cyc;
		uint64_t now = k_cycle_get_64();

		if (gate > now && gate - now < k_us_to_cyc_ceil64(GATE_MAX_SPIN_US)) {
			while (k_cycle_get_64() < gate) {
			}
		}
		tx_gate_armed = false;
	}
	tx_start_cyc = k_cycle_get_64();
}

/* ---- radio (twr_wq) --------------------------------------------------------------------- */

static void lora_rx_cb(const struct device *dev, uint8_t *data, uint16_t size, int16_t rssi,
		       int8_t snr, void *user_data)
{
	/* System workqueue, serialized: a static message keeps ~270 B off its stack. */
	static struct rx_msg m;
	uint64_t now = k_cycle_get_64();
	uint64_t t = rx_done_cyc;

	ARG_UNUSED(dev);
	ARG_UNUSED(user_data);

	/* The hook's RxDone stamp is this frame's unless it is stale. */
	if (t == 0 || t > now || now - t > k_ms_to_cyc_ceil64(100)) {
		t = now;
	}
	m.t_cyc = t;
	m.rssi = rssi;
	m.snr = snr;
	m.len = (uint8_t)MIN(size, (uint16_t)P2P_MAX_RADIO_FRAME_LEN);
	memcpy(m.data, data, m.len);
	if (k_msgq_put(&rx_q, &m, K_NO_WAIT) != 0) {
		rs.rx_overrun++;
	}
	k_work_submit_to_queue(&twr_wq, &rx_work);
}

static int modem_config_retry(struct lora_modem_config *lc)
{
	int rc = -EBUSY;

	for (int i = 0; i < 5 && rc == -EBUSY; i++) {
		rc = lora_config(lora_dev, lc);
		if (rc == -EBUSY) {
			k_usleep(200);
		}
	}
	return rc;
}

static void rx_release(void)
{
	if (rx_armed) {
		lora_recv_async(lora_dev, NULL, NULL);
		rx_armed = false;
	}
}

static void rx_arm(void)
{
	struct radio_config c = cfg_get();
	struct lora_modem_config lc = modem_cfg(&c, false);
	int rc;

	rx_release();
	rearm_last_try_ms = k_uptime_get();
	rc = modem_config_retry(&lc);
	if (rc == 0) {
		rc = lora_recv_async(lora_dev, lora_rx_cb, NULL);
	}
	if (rc == 0) {
		rx_armed = true;
		diag_set_radio_state(DIAG_RADIO_RX);
	} else {
		diag_set_radio_state(DIAG_RADIO_ERROR);
		diag_report(DIAG_CODE_RX_REARM, "rx arm failed");
	}
}

/* After a transmission: continuous RX again in GATEWAY, the radio stays asleep otherwise. */
static void rx_resume(void)
{
	if (state_get() == P2P_NB_GATEWAY) {
		rx_arm();
	} else {
		diag_set_radio_state(DIAG_RADIO_IDLE);
	}
}

/* Release RX, configure TX and warm the TCXO so SetTx starts without its 5 ms wait. */
static int tx_prepare(const struct radio_config *c)
{
	struct lora_modem_config lc = modem_cfg(c, true);
	int rc;

	rx_release();
	rc = modem_config_retry(&lc);
	if (rc == 0) {
		sx126x_twr_standby_xosc();
	}
	return rc;
}

struct tx_result {
	int rc;
	bool on_air; /* SetTx went out */
	uint64_t start_cyc;
	uint64_t done_cyc;
	uint32_t toa_calc_us;
	uint32_t toa_meas_us;
};

/* Transmit `frame` with SetTx at `gate` (k_cycle_get_64), or at once when that has passed. */
static void tx_at(const struct radio_config *c, uint64_t gate, uint8_t *frame, size_t len,
		  struct tx_result *r)
{
	uint64_t lead = k_us_to_cyc_ceil64(GATE_LEAD_US);
	uint64_t now = k_cycle_get_64();
	k_spinlock_key_t key;

	r->toa_calc_us = toa_us(c, len);
	if (gate > now + lead) {
		k_sleep(K_USEC(k_cyc_to_us_floor64(gate - lead - now)));
	}

	tx_start_cyc = 0;
	tx_done_cyc = 0;
	tx_gate_cyc = gate;
	tx_gate_armed = true;
	tx_budget_ms = 2u * (r->toa_calc_us / 1000u) + 2000u;
	tx_started_ms = k_uptime_get();
	diag_set_radio_state(DIAG_RADIO_TX);

	r->rc = lora_send(lora_dev, frame, len);

	tx_gate_armed = false;
	tx_started_ms = 0;
	r->on_air = tx_start_cyc != 0;
	r->start_cyc = tx_start_cyc != 0 ? tx_start_cyc : now;
	r->done_cyc = tx_done_cyc != 0 ? tx_done_cyc : k_cycle_get_64();
	r->toa_meas_us = r->rc == 0 ? cyc_delta_us(r->done_cyc, r->start_cyc) : 0;
	if (r->rc != 0) {
		diag_report(DIAG_CODE_TX_RADIO, "lora_send failed");
	}

	/* Airtime is spent whenever SetTx went out, even if the driver reported an error. */
	if (r->on_air) {
		rs.tx_total++;
		key = k_spin_lock(&duty_lock);
		twr_duty_add(&duty, k_uptime_get(), r->toa_calc_us, r->toa_meas_us);
		k_spin_unlock(&duty_lock, key);
	}
}

static void busy_set(uint64_t until_ms)
{
	k_spinlock_key_t key = k_spin_lock(&sched_lock);

	busy_until_ms = until_ms;
	k_spin_unlock(&sched_lock, key);
}

/* ---- receive, ACK, downlink (twr_wq) ------------------------------------------------------ */

/* twr_wq only */
static struct twr_gwc_rx rxo;
static uint8_t dl_frame[TWR_GWC_DL_MAX_FRAME];

/* How far past its slot a frame may start and still end inside the node's window. */
static uint64_t node_slack_cyc(const struct radio_config *c)
{
	return k_us_to_cyc_ceil64((uint64_t)NODE_WINDOW_SYMBOLS * tsym_us(c) +
				  NODE_WINDOW_MARGIN_MS * 1000u);
}

/* The ACK sealed by twr_gwc_on_rx(), SetTx at t_rx + turnaround. Its counter is spent
 * whatever happens here (#470 §4). A late ACK still goes out (the node may yet hear it) and
 * counts in acks_late; EVT_TWR_UPLINK then says ack 3 (late). Returns true when it went on
 * air in time, the condition for the downlink after PENDING. */
static bool send_ack(const struct rx_msg *m, const struct radio_config *c, uint8_t turnaround,
		     struct tx_result *ack)
{
	uint64_t gate = m->t_cyc + k_ms_to_cyc_floor64(turnaround);
	bool on_air = false;
	bool late = false;
	uint32_t turn_us;

	if (tx_prepare(c) != 0) {
		diag_report(DIAG_CODE_TX_RADIO, "ack tx config failed");
	} else {
		tx_at(c, gate, rxo.ack_frame, rxo.ack_len, ack);
		on_air = ack->rc == 0;
		late = on_air && ack->start_cyc > gate + node_slack_cyc(c);
	}

	k_mutex_lock(&core_lock, K_FOREVER);
	twr_gwc_ack_result(&core, on_air, late, rxo.ack_ctrl);
	k_mutex_unlock(&core_lock);
	rxo.ack = twr_gwc_ack_code(rxo.ack, on_air, late);
	if (!on_air) {
		return false;
	}
	turn_us = cyc_delta_us(ack->start_cyc, m->t_cyc);
	rs.turn_last_us = (uint16_t)MIN(turn_us, (uint32_t)UINT16_MAX);
	rs.turn_max_us = MAX(rs.turn_max_us, rs.turn_last_us);
	rs.ack_toa_last_us = ack->toa_meas_us;
	return !late;
}

/* After an ACK with PENDING: the head of the node's queue, dl_gap_ms after the ACK's TxDone.
 * The outcome event goes to *evt (sent by the caller once RX is armed again). */
static bool send_dl(const struct radio_config *c, uint32_t addr, uint8_t dl_gap,
		    const struct tx_result *ack, struct twr_tx_evt *evt)
{
	uint64_t slot = ack->done_cyc + k_ms_to_cyc_floor64(dl_gap);
	struct tx_result dl = { 0 };
	struct twr_dl_ref ref;
	enum twr_dl_tx res;
	uint64_t deadline = 0;
	bool has_evt;
	size_t n;

	k_mutex_lock(&core_lock, K_FOREVER);
	n = twr_gwc_dl_take(&core, addr, now_ms(), dl_frame, &ref, evt, &has_evt);
	k_mutex_unlock(&core_lock);
	if (n == 0) {
		/* Nothing (dropped meanwhile), or NO_COUNTER / RADIO_ERR for the head. */
		return has_evt;
	}

	if (k_cycle_get_64() + k_us_to_cyc_ceil64(GATE_LEAD_US) > slot + node_slack_cyc(c)) {
		/* SetTx would land past the node's downlink window. */
		res = TWR_DL_TX_LATE;
	} else {
		/* The radio slept after TxDone; wake it and settle the TCXO first. */
		sx126x_twr_standby_xosc();
		tx_at(c, slot, dl_frame, n, &dl);
		res = dl.rc == 0 ? TWR_DL_TX_OK : TWR_DL_TX_ERR;
	}
	if (res == TWR_DL_TX_OK) {
		rs.dl_toa_last_us = dl.toa_meas_us;
		rs.dl_dt_last_us = cyc_delta_us(dl.start_cyc, ack->done_cyc);
		deadline = cyc_ms(dl.done_cyc) + P2P_TWR_TIMING_MAX_MS +
			   toa_us(c, TWR_GWC_ACK_FRAME_LEN) / 1000u + 3u * tsym_us(c) / 1000u +
			   DL_ACK_MARGIN_MS;
	}

	k_mutex_lock(&core_lock, K_FOREVER);
	has_evt = twr_gwc_dl_done(&core, &ref, res, deadline, evt);
	k_mutex_unlock(&core_lock);
	return has_evt;
}

#if defined(CONFIG_NB_TWR_RX_INJECT)
/* Bench (HIL): the raw bytes of the last accepted (fresh) uplink, for `p2p rx_last`, so a
 * replay can be fed back with `p2p rx_inject`. Written on twr_wq, read by the shell. */
static struct rx_msg rx_last;
static bool rx_last_valid;
static struct k_spinlock rx_last_lock;

static void rx_last_save(const struct rx_msg *m)
{
	k_spinlock_key_t key = k_spin_lock(&rx_last_lock);

	rx_last = *m;
	rx_last_valid = true;
	k_spin_unlock(&rx_last_lock, key);
}
#endif

static void handle_frame(const struct rx_msg *m)
{
	struct radio_config c = cfg_get();
	struct tx_result ack = { 0 };
	struct twr_tx_evt head_evt;
	struct twr_tx_evt dl_evt = { 0 };
	bool has_head_evt;
	bool has_dl_evt = false;
	bool acked = false;
	uint8_t turnaround;
	uint8_t dl_gap;
	uint8_t max_frame;
	uint64_t t_rx_ms = cyc_ms(m->t_cyc);
	/* t_rx is the RxDone stamp, the end of the received frame: the instant a TimeAns in the
	 * ACK refers to (#470 §13.5). */
	struct twr_rx_meta meta = {
		.rssi_dbm = m->rssi,
		.snr_q = twr_link_snr_q(m->snr),
		.sf = c.sf,
		.t_rx_ms = t_rx_ms,
	};

	k_mutex_lock(&core_lock, K_FOREVER);
	turnaround = core.p.turnaround_ms;
	dl_gap = core.p.dl_gap_ms;
	max_frame = core.max_frame;
	twr_gwc_on_rx(&core, m->data, m->len, &meta, now_ms(), &rxo);
	k_mutex_unlock(&core_lock);

	if (m->len > TWR_TOWER_MAX_FRAME) {
		LOG_WRN("rx: %u B frame over the %u B TOWER MTU dropped", (unsigned int)m->len,
			(unsigned int)TWR_TOWER_MAX_FRAME);
	}

	switch (rxo.action) {
	case TWR_RXA_RAW:
		send_raw_rx(m);
		return;
	case TWR_RXA_NODE_ACK:
		send_tx_evt(&rxo.tx_evt);
		return;
	case TWR_RXA_UPLINK:
	case TWR_RXA_DUP:
		break;
	default:
		return;
	}

#if defined(CONFIG_NB_TWR_RX_INJECT)
	if (rxo.action == TWR_RXA_UPLINK) {
		rx_last_save(m);
	}
#endif

	/* A NO_COUNTER for the head of the queue from sealing the ACK: reported after it. */
	has_head_evt = rxo.has_tx_evt;
	head_evt = rxo.tx_evt;

	if (rxo.send_ack) {
		uint64_t end = t_rx_ms + turnaround + toa_us(&c, rxo.ack_len) / 1000u + 5u;

		if (rxo.ack_pending) {
			end += dl_gap + toa_us(&c, max_frame) / 1000u;
		}
		busy_set(end);
		acked = send_ack(m, &c, turnaround, &ack);
	}

	/* The ACK's fate is known and the downlink is not due for dl_gap_ms: the uplink goes to
	 * the central now, never ahead of the ACK. */
	if (rxo.action == TWR_RXA_UPLINK) {
		send_uplink(m, &rxo);
	}

	/* After a late ACK the node may not expect it: the downlink stays queued for the next
	 * confirmed uplink. */
	if (acked && rxo.ack_pending) {
		has_dl_evt = send_dl(&c, rxo.hdr.src, dl_gap, &ack, &dl_evt);
	}

	if (rxo.send_ack) {
		/* The node answers a confirmed downlink one turnaround after its RxDone: RX first,
		 * events after. */
		rx_resume();
		busy_set(0);
	}
	if (has_head_evt) {
		send_tx_evt(&head_evt);
	}
	if (has_dl_evt) {
		send_tx_evt(&dl_evt);
	}
	if (rxo.send_ack || has_head_evt) {
		emit_core_events(); /* EVT_TWR_CTR_LOW after the spends */
	}
}

static void rx_work_handler(struct k_work *work)
{
	static struct rx_msg m;

	ARG_UNUSED(work);
	while (k_msgq_get(&rx_q, &m, K_NO_WAIT) == 0) {
		/* Received before an RX_STOP took effect: dropped with the receiver. */
		if (state_get() == P2P_NB_GATEWAY) {
			handle_frame(&m);
		}
	}
}

/* ---- host-scheduled TX (twr_wq) ----------------------------------------------------------- */

static void sched_timer_expiry(struct k_timer *timer)
{
	ARG_UNUSED(timer);
	k_work_submit_to_queue(&twr_wq, &sched_work);
}

static void sched_work_handler(struct k_work *work)
{
	static uint8_t frame[P2P_MAX_RADIO_FRAME_LEN];
	struct radio_config c = cfg_get();
	struct tx_result r = { 0 };
	uint8_t status = P2P_TX_DONE_OK;
	uint64_t t_actual;
	uint64_t aim;
	uint64_t now;
	uint8_t tx_id;
	uint8_t len;
	k_spinlock_key_t key;

	ARG_UNUSED(work);
	key = k_spin_lock(&sched_lock);
	now = now_ms();
	aim = sched.t_ms == P2P_TX_IMMEDIATE ? now : sched.t_ms;
	/* Cancelled, or a stale submission ahead of a newer schedule's timer. */
	if (!sched.active || sched.started || now + SCHED_LEAD_MS + 1u < aim) {
		k_spin_unlock(&sched_lock, key);
		return;
	}
	sched.started = true;
	tx_id = sched.tx_id;
	len = sched.len;
	memcpy(frame, sched.frame, len);
	k_spin_unlock(&sched_lock, key);

	if (now > aim + SCHED_LATE_MS) {
		/* Behind the gateway's own ACK / downlink (or a long command) past the aim. */
		status = P2P_TX_DONE_LATE;
		t_actual = now;
		rs.sched_late++;
	} else {
		if (tx_prepare(&c) != 0) {
			diag_report(DIAG_CODE_TX_RADIO, "tx config failed");
			r.rc = -EIO;
		} else {
			tx_at(&c, k_ms_to_cyc_floor64(aim), frame, len, &r);
		}
		status = r.rc == 0 ? P2P_TX_DONE_OK : P2P_TX_DONE_RADIO_ERR;
		/* The TxDone stamp from the patched driver (as proto 1, 0.2.0). */
		t_actual = r.rc == 0 ? cyc_ms(r.done_cyc) : now_ms();
		if (r.rc == 0) {
			rs.sched_ok++;
		} else {
			rs.sched_err++;
		}
		rx_resume();
	}

	key = k_spin_lock(&sched_lock);
	sched.active = false;
	sched.started = false;
	k_spin_unlock(&sched_lock, key);
	send_tx_done(tx_id, status, t_actual);
}

/* ---- state changes, housekeeping (twr_wq) ------------------------------------------------- */

static void arm_work_handler(struct k_work *work)
{
	ARG_UNUSED(work);
	if (state_get() == P2P_NB_GATEWAY) {
		rx_arm();
	}
}

static void stop_work_handler(struct k_work *work)
{
	ARG_UNUSED(work);
	if (state_get() != P2P_NB_GATEWAY) {
		rx_release();
		diag_set_radio_state(DIAG_RADIO_IDLE);
	}
}

static void hk_timer_expiry(struct k_timer *timer)
{
	ARG_UNUSED(timer);
	k_work_submit_to_queue(&twr_wq, &hk_work);
}

static void hk_work_handler(struct k_work *work)
{
	ARG_UNUSED(work);
	emit_core_events();
	ring_resend();
	/* A failed re-arm leaves the receiver off: try again, the state stays GATEWAY. */
	if (state_get() == P2P_NB_GATEWAY && !rx_armed &&
	    k_uptime_get() - rearm_last_try_ms >= REARM_RETRY_MS) {
		rx_arm();
	}
}

/* ---- radio_ctl.h ------------------------------------------------------------------------ */

int radio_ctl_init(void)
{
	twr_gwc_init(&core);
	twr_duty_init(&duty);

	k_work_init(&rx_work, rx_work_handler);
	k_work_init(&arm_work, arm_work_handler);
	k_work_init(&stop_work, stop_work_handler);
	k_work_init(&hk_work, hk_work_handler);
	k_work_init(&sched_work, sched_work_handler);
	k_timer_init(&sched_timer, sched_timer_expiry, NULL);
	k_timer_init(&hk_timer, hk_timer_expiry, NULL);
	k_work_queue_init(&twr_wq);
	k_work_queue_start(&twr_wq, twr_wq_stack, K_THREAD_STACK_SIZEOF(twr_wq_stack),
			   K_PRIO_COOP(7), NULL);
	k_thread_name_set(&twr_wq.thread, "twr_wq");
	wq_started = true;
	k_timer_start(&hk_timer, K_MSEC(HK_PERIOD_MS), K_MSEC(HK_PERIOD_MS));

	if (!device_is_ready(lora_dev)) {
		diag_set_flag(DIAG_FLAG_RADIO_INIT);
		diag_set_radio_state(DIAG_RADIO_ERROR);
		return -ENODEV;
	}
	radio_ready = true;
#if defined(CONFIG_NB_RADIO_TUNE)
	/* The driver's init has run and the radio sleeps; the host link is not up yet, so
	 * nothing else drives the radio here (docs/problems.md §1). */
	radio_tune_hw_boot();
#endif
	return 0;
}

enum p2p_nb_state radio_ctl_state(void)
{
	return state_get();
}

enum p2p_status radio_ctl_set_config(const struct radio_config *cfg)
{
	uint32_t hz;
	enum lora_signal_bandwidth bw;
	uint8_t max_frame = 0;
	k_spinlock_key_t key;

	if (!bw_of(cfg->bw, &hz, &bw) || cfg->frequency_hz < RADIO_FREQ_MIN_HZ ||
	    cfg->frequency_hz > RADIO_FREQ_MAX_HZ || cfg->tx_power_dbm < RADIO_POWER_MIN_DBM ||
	    cfg->tx_power_dbm > RADIO_POWER_MAX_DBM) {
		return P2P_STATUS_BAD_PARAM;
	}
	/* The longest gateway frame that stays under the TX timeout; the longest ACK (with the
	 * CTRL tail) must fit. */
	for (size_t n = TWR_GWC_DL_MAX_FRAME; n >= TWR_GWC_ACK_MAX_FRAME; n--) {
		if (toa_us(cfg, n) <= TX_TOA_MAX_US) {
			max_frame = (uint8_t)n;
			break;
		}
	}
	if (max_frame == 0) {
		return P2P_STATUS_BAD_PARAM;
	}

	key = k_spin_lock(&st_lock);
	if (nb_state == P2P_NB_GATEWAY) {
		k_spin_unlock(&st_lock, key);
		return P2P_STATUS_INVALID_STATE;
	}
	rcfg = *cfg;
	nb_state = P2P_NB_CONFIGURED;
	k_spin_unlock(&st_lock, key);

	k_mutex_lock(&core_lock, K_FOREVER);
	twr_gwc_set_max_frame(&core, max_frame);
	k_mutex_unlock(&core_lock);
	/* Queued downlinks the new cap no longer allows fail at the next housekeeping. */
	twr_rt_kick();
	return P2P_STATUS_OK;
}

bool radio_ctl_get_config(struct radio_config *out)
{
	k_spinlock_key_t key = k_spin_lock(&st_lock);
	bool valid = nb_state != P2P_NB_BOOT;

	if (valid) {
		*out = rcfg;
	} else {
		memset(out, 0, sizeof(*out));
	}
	k_spin_unlock(&st_lock, key);
	return valid;
}

enum p2p_status radio_ctl_rx_stop(void)
{
	k_spinlock_key_t key = k_spin_lock(&st_lock);

	if (nb_state == P2P_NB_GATEWAY) {
		nb_state = P2P_NB_CONFIGURED;
	}
	k_spin_unlock(&st_lock, key);
	if (wq_started) {
		/* Applied on twr_wq after a transmission in progress; nothing re-arms meanwhile. */
		k_work_submit_to_queue(&twr_wq, &stop_work);
	}
	return P2P_STATUS_OK;
}

enum p2p_status radio_ctl_tx_schedule(uint8_t tx_id, uint64_t t_ms, const uint8_t *frame,
				       uint8_t len)
{
	struct radio_config c;
	uint64_t now;
	uint64_t start;
	uint64_t delay;
	k_spinlock_key_t key;

	if (len == 0) {
		return P2P_STATUS_BAD_PARAM;
	}
	if (state_get() == P2P_NB_BOOT) {
		return P2P_STATUS_INVALID_STATE;
	}
	if (!radio_ready) {
		return P2P_STATUS_INTERNAL;
	}
	c = cfg_get();
	if (toa_us(&c, len) > TX_TOA_MAX_US) {
		return P2P_STATUS_BAD_PARAM;
	}
	now = now_ms();
	if (t_ms != P2P_TX_IMMEDIATE && t_ms < now + P2P_TX_MIN_LEAD_MS) {
		return P2P_STATUS_LATE;
	}
	start = t_ms == P2P_TX_IMMEDIATE ? now : t_ms;

	key = k_spin_lock(&sched_lock);
	if (sched.active || start < busy_until_ms) {
		/* One slot; or the gateway's own ACK / downlink holds the radio then. */
		k_spin_unlock(&sched_lock, key);
		return P2P_STATUS_BUSY;
	}
	sched.active = true;
	sched.started = false;
	sched.tx_id = tx_id;
	sched.t_ms = t_ms;
	sched.len = len;
	memcpy(sched.frame, frame, len);
	k_spin_unlock(&sched_lock, key);

	delay = start > now + SCHED_LEAD_MS ? start - now - SCHED_LEAD_MS : 0;
	k_timer_start(&sched_timer, K_MSEC(delay), K_NO_WAIT);
	return P2P_STATUS_OK;
}

enum p2p_status radio_ctl_tx_cancel(uint8_t tx_id)
{
	k_spinlock_key_t key = k_spin_lock(&sched_lock);

	/* Unknown tx_id, or already on its way to the radio (its EVT_TX_DONE follows). */
	if (!sched.active || sched.started || sched.tx_id != tx_id) {
		k_spin_unlock(&sched_lock, key);
		return P2P_STATUS_BAD_PARAM;
	}
	k_timer_stop(&sched_timer);
	sched.active = false;
	k_spin_unlock(&sched_lock, key);
	(void)k_work_cancel(&sched_work);
	rs.sched_cancelled++;

	send_tx_done(tx_id, P2P_TX_DONE_CANCELLED, now_ms());
	return P2P_STATUS_OK;
}

void radio_ctl_get_stats(struct radio_ctl_stats *out)
{
	out->crc_err = rs.crc_err;
	out->hdr_err = rs.hdr_err;
	out->rx_overrun = rs.rx_overrun;
	out->tx_total = rs.tx_total;
	out->ack_turnaround_last_us = rs.turn_last_us;
	out->ack_turnaround_max_us = rs.turn_max_us;
}

uint64_t radio_ctl_now_ms(void)
{
	return now_ms();
}

bool radio_ctl_tx_stuck(void)
{
	int64_t started = tx_started_ms;

	return started != 0 && k_uptime_get() - started > (int64_t)tx_budget_ms;
}

/* 0.2.2: radio_ctl_call(), one call at a time (the RTT shell). */
static struct radio_call {
	struct k_work work;
	struct k_sem done;
	void (*fn)(void *arg);
	void *arg;
	bool pause_rx;
	int rc;
} call;
static K_MUTEX_DEFINE(call_lock);

static void radio_call_handler(struct k_work *work)
{
	struct radio_call *c = CONTAINER_OF(work, struct radio_call, work);
	bool was_armed = c->pause_rx && rx_armed;

	c->rc = 0;
	if (was_armed) {
		rx_release();
	}
	c->fn(c->arg);
	if (was_armed && state_get() == P2P_NB_GATEWAY) {
		rx_arm();
		if (!rx_armed) {
			c->rc = -EIO;
		}
	}
	k_sem_give(&c->done);
}

int radio_ctl_call(void (*fn)(void *arg), void *arg, bool pause_rx)
{
	if (!wq_started || !radio_ready) {
		return -ENODEV;
	}

	k_mutex_lock(&call_lock, K_FOREVER);
	k_work_init(&call.work, radio_call_handler);
	k_sem_init(&call.done, 0, 1);
	call.fn = fn;
	call.arg = arg;
	call.pause_rx = pause_rx;
	k_work_submit_to_queue(&twr_wq, &call.work);
	/* Bounded by a transmission in progress on the same queue (its air time). */
	k_sem_take(&call.done, K_FOREVER);

	int rc = call.rc;

	k_mutex_unlock(&call_lock);
	return rc;
}

/* ---- twr_rt.h --------------------------------------------------------------------------- */

struct twr_gwc *twr_rt_lock(void)
{
	k_mutex_lock(&core_lock, K_FOREVER);
	return &core;
}

void twr_rt_unlock(void)
{
	k_mutex_unlock(&core_lock);
}

enum p2p_status twr_rt_enter_gateway(void)
{
	k_spinlock_key_t key;

	if (!radio_ready || !wq_started) {
		return P2P_STATUS_INTERNAL;
	}
	key = k_spin_lock(&st_lock);
	if (nb_state == P2P_NB_BOOT) {
		k_spin_unlock(&st_lock, key);
		return P2P_STATUS_INVALID_STATE;
	}
	nb_state = P2P_NB_GATEWAY;
	k_spin_unlock(&st_lock, key);
	/* Armed on twr_wq, not here: the main thread never waits for the radio. A failure is an
	 * EVT_LOG (code 2) and retried every REARM_RETRY_MS. */
	k_work_submit_to_queue(&twr_wq, &arm_work);
	return P2P_STATUS_OK;
}

uint32_t twr_rt_airtime_ms_last_hour(void)
{
	struct twr_duty_sum s;
	k_spinlock_key_t key = k_spin_lock(&duty_lock);

	twr_duty_sum(&duty, k_uptime_get(), &s);
	k_spin_unlock(&duty_lock, key);
	return (uint32_t)MIN(s.measured_us / 1000u, (uint64_t)UINT32_MAX);
}

void twr_rt_kick(void)
{
	if (wq_started) {
		k_work_submit_to_queue(&twr_wq, &hk_work);
	}
}

/* ---- shell -------------------------------------------------------------------------------- */

static const char *state_str(enum p2p_nb_state s)
{
	switch (s) {
	case P2P_NB_BOOT:
		return "BOOT";
	case P2P_NB_CONFIGURED:
		return "CONFIGURED";
	case P2P_NB_GATEWAY:
		return "GATEWAY";
	default:
		return "?";
	}
}

void twr_gateway_print_brief(const struct shell *sh)
{
	uint32_t fresh;
	uint32_t acks;
	uint16_t peers;

	k_mutex_lock(&core_lock, K_FOREVER);
	fresh = core.st.fresh;
	acks = core.st.acks_sent;
	peers = core.peers.n;
	k_mutex_unlock(&core_lock);
	shell_print(sh, "twr: state=%s peers=%u fresh=%u acks=%u tx=%u rx_armed=%d (see `p2p show`)",
		    state_str(state_get()), peers, fresh, acks, rs.tx_total, rx_armed ? 1 : 0);
}

static int cmd_show(const struct shell *sh, size_t argc, char **argv)
{
	struct radio_config c;
	bool configured = radio_ctl_get_config(&c);
	struct twr_gwc_params p;
	struct twr_ctr ctr;
	bool started;
	uint8_t max_frame;
	size_t max_pt;
	uint16_t peers;
	uint16_t queued;
	bool sched_active;
	k_spinlock_key_t key;

	ARG_UNUSED(argc);
	ARG_UNUSED(argv);

	k_mutex_lock(&core_lock, K_FOREVER);
	p = core.p;
	started = core.started;
	ctr = core.ctr;
	max_frame = core.max_frame;
	max_pt = twr_gwc_dl_max_pt(&core);
	peers = core.peers.n;
	queued = twr_dlq_used(&core.dlq);
	k_mutex_unlock(&core_lock);
	key = k_spin_lock(&sched_lock);
	sched_active = sched.active;
	k_spin_unlock(&sched_lock, key);

	shell_print(sh, "TOWER gateway fw %d.%d.%d git %08x: state=%s", FW_VERSION_MAJOR,
		    FW_VERSION_MINOR, FW_VERSION_PATCH, (unsigned int)APP_GIT_SHA,
		    state_str(state_get()));
	if (configured) {
		shell_print(sh, "radio: freq=%u sf=%u bw=%u cr=%u preamble=%u tx_power=%d "
			    "flags=0x%02x; toa ack(%u B)=%u us, ack+ctrl(%u B)=%u us, max frame %u B "
			    "(dl pt <= %u B)",
			    c.frequency_hz, c.sf, c.bw, c.cr, c.preamble_len, c.tx_power_dbm, c.flags,
			    (unsigned int)TWR_GWC_ACK_FRAME_LEN, toa_us(&c, TWR_GWC_ACK_FRAME_LEN),
			    (unsigned int)TWR_GWC_ACK_MAX_FRAME, toa_us(&c, TWR_GWC_ACK_MAX_FRAME),
			    max_frame, (unsigned int)max_pt);
	} else {
		shell_print(sh, "radio: not configured");
	}
	if (started) {
		shell_print(sh, "twr: net_id=0x%08x turnaround=%u ms dl_gap=%u ms ack=%s", p.net_id,
			    p.turnaround_ms, p.dl_gap_ms,
			    (p.flags & P2P_TWR_START_ACK_ENABLED) != 0 ? "on" : "off");
	} else {
		shell_print(sh, "twr: not started");
	}
	if (ctr.ever) {
		shell_print(sh, "ctr: block %u..%u next=%u left=%u", ctr.first, ctr.last, ctr.next,
			    twr_ctr_left(&ctr));
	} else {
		shell_print(sh, "ctr: no block");
	}
	shell_print(sh, "peers=%u/%u queue=%u/%u rx_armed=%d tx_schedule=%s", peers,
		    (unsigned int)TWR_PEERS_MAX, queued, (unsigned int)TWR_DLQ_MAX, rx_armed ? 1 : 0,
		    sched_active ? "pending" : "idle");
	return 0;
}

static int cmd_stats(const struct shell *sh, size_t argc, char **argv)
{
	struct twr_gwc_stats s;
	struct diag_info d;
	unsigned int ev_used;
	unsigned int ev_bytes;
	unsigned int ev_seq;
	uint32_t ev_resent;
	uint32_t ev_ack_ignored;
	struct twr_time_anchor anchor;

	ARG_UNUSED(argc);
	ARG_UNUSED(argv);

	k_mutex_lock(&core_lock, K_FOREVER);
	s = core.st;
	anchor = core.anchor;
	ev_used = core.evq.n;
	ev_bytes = core.evq.pool_used;
	ev_seq = core.evq.last_seq;
	ev_resent = core.evq.resent;
	ev_ack_ignored = core.evq.ack_ignored;
	k_mutex_unlock(&core_lock);
	diag_snapshot(&d);

	shell_print(sh, "rx: total=%u fresh=%u dup=%u replay=%u mic_fail=%u unknown_src=%u "
		    "join_fwd=%u not_for_us=%u bad_hdr=%u too_long=%u", s.rx_total, s.fresh,
		    s.dup, s.replay, s.mic_fail, s.unknown_src, s.join_fwd, s.not_for_us,
		    s.bad_hdr, s.rx_too_long);
	shell_print(sh, "radio: crc_err=%u hdr_err=%u rx_overrun=%u rearm_fail=%u tx=%u "
		    "tx_radio_err=%u", rs.crc_err, rs.hdr_err, rs.rx_overrun, d.rx_rearm_fail,
		    rs.tx_total, d.tx_radio_err);
	shell_print(sh, "ack: sent=%u late=%u suppressed=%u node_ack_unmatched=%u; turnaround_us "
		    "last=%u max=%u toa_us=%u", s.acks_sent, s.acks_late, s.acks_suppressed,
		    s.ack_unmatched, rs.turn_last_us, rs.turn_max_us, rs.ack_toa_last_us);
	shell_print(sh, "ack ctrl: sent=%u link_check=%u time=%u time_no_anchor=%u anchor=%s",
		    s.ack_ctrl_sent, s.ack_ctrl_lc, s.ack_ctrl_time, s.ack_time_no_anchor,
		    anchor.valid ? "set" : "none");
	shell_print(sh, "dl: sent=%u delivered=%u not_delivered=%u expired=%u radio_err=%u "
		    "no_counter=%u late=%u; last toa_us=%u dt_us=%u", s.dl_sent, s.dl_delivered,
		    s.dl_not_delivered, s.dl_expired, s.dl_radio_err, s.dl_no_counter, s.dl_late,
		    rs.dl_toa_last_us, rs.dl_dt_last_us);
	shell_print(sh, "tx_schedule: ok=%u late=%u radio_err=%u cancelled=%u", rs.sched_ok,
		    rs.sched_late, rs.sched_err, rs.sched_cancelled);
	shell_print(sh, "link: crc_err=%u tx_drops=%u", uart_link_get_crc_err_count(),
		    uart_link_get_tx_drops());
	shell_print(sh, "evt ring: used=%u/%u bytes=%u/%u seq=%u resent=%u ack_ignored=%u "
		    "rx_ring_full=%u lost=%u", ev_used, (unsigned int)TWR_EVQ_MAX, ev_bytes,
		    (unsigned int)TWR_EVQ_POOL, ev_seq, ev_resent, ev_ack_ignored, s.rx_ring_full,
		    evt_ring_lost);
	return 0;
}

#define SHELL_PAGE 16u

struct peer_row {
	uint32_t addr;
	uint32_t last_seen;
	uint8_t flags;
	uint8_t queued;
};

static int cmd_peers(const struct shell *sh, size_t argc, char **argv)
{
	struct peer_row rows[SHELL_PAGE];
	unsigned long start = 0;
	size_t n = 0;
	uint16_t total;

	if (argc == 2) {
		char *end;

		start = strtoul(argv[1], &end, 10);
		if (*argv[1] == '\0' || *end != '\0') {
			shell_error(sh, "usage: p2p peers [<start index>]");
			return -EINVAL;
		}
	}

	k_mutex_lock(&core_lock, K_FOREVER);
	total = core.peers.n;
	for (size_t i = start; i < total && n < SHELL_PAGE; i++, n++) {
		const struct twr_peer *p = &core.peers.p[i];
		uint8_t q = 0;

		for (size_t j = 0; j < TWR_DLQ_MAX; j++) {
			q += core.dlq.e[j].state != TWR_DL_FREE && core.dlq.e[j].addr == p->addr;
		}
		rows[n] = (struct peer_row){ p->addr, p->last_seen, p->flags, q };
	}
	k_mutex_unlock(&core_lock);

	/* Never the session keys. */
	for (size_t i = 0; i < n; i++) {
		shell_print(sh, "#%lu addr=0x%08x last_seen=%u home=%d queued=%u", start + i,
			    rows[i].addr, rows[i].last_seen,
			    (rows[i].flags & P2P_TWR_NODE_HOME) != 0 ? 1 : 0, rows[i].queued);
	}
	shell_print(sh, "%u peers%s", total,
		    start + n < total ? " (more: p2p peers <start>)" : "");
	return 0;
}

static const char *dl_state_str(uint8_t s)
{
	switch (s) {
	case TWR_DL_QUEUED:
		return "queued";
	case TWR_DL_SENDING:
		return "sending";
	case TWR_DL_INFLIGHT:
		return "awaiting-ack";
	default:
		return "free";
	}
}

struct dl_row {
	uint8_t idx;
	uint8_t state;
	uint8_t flags;
	uint8_t len;
	uint16_t item;
	uint32_t addr;
	uint32_t gw_counter;
	int64_t left_ms; /* to the TTL, or to the node-ACK timeout while awaiting it */
};

static int cmd_queue(const struct shell *sh, size_t argc, char **argv)
{
	struct dl_row rows[SHELL_PAGE];
	uint64_t now = now_ms();
	unsigned int used = 0;
	size_t base = 0;

	ARG_UNUSED(argc);
	ARG_UNUSED(argv);

	/* In pages, printed outside the lock; never the plaintext. */
	while (base < TWR_DLQ_MAX) {
		size_t n = 0;

		k_mutex_lock(&core_lock, K_FOREVER);
		for (; base < TWR_DLQ_MAX && n < SHELL_PAGE; base++) {
			const struct twr_dl *e = &core.dlq.e[base];
			uint64_t until = e->state == TWR_DL_INFLIGHT ? e->ack_deadline_ms
								      : e->expires_ms;

			if (e->state == TWR_DL_FREE) {
				continue;
			}
			rows[n++] = (struct dl_row){
				.idx = (uint8_t)base,
				.state = e->state,
				.flags = e->flags,
				.len = e->len,
				.item = e->item,
				.addr = e->addr,
				.gw_counter = e->gw_counter,
				.left_ms = (int64_t)until - (int64_t)now,
			};
		}
		k_mutex_unlock(&core_lock);
		for (size_t i = 0; i < n; i++) {
			const struct dl_row *r = &rows[i];

			shell_print(sh, "[%u] addr=0x%08x item=%u %s len=%u confirmed=%d ctr=%u "
				    "%s_in_ms=%lld", r->idx, r->addr, r->item, dl_state_str(r->state),
				    r->len, (r->flags & P2P_TWR_QUEUE_CONFIRMED) != 0 ? 1 : 0,
				    r->gw_counter, r->state == TWR_DL_INFLIGHT ? "ack_timeout" : "ttl",
				    (long long)r->left_ms);
		}
		used += (unsigned int)n;
	}
	shell_print(sh, "%u/%u queued", used, (unsigned int)TWR_DLQ_MAX);
	return 0;
}

static int cmd_duty(const struct shell *sh, size_t argc, char **argv)
{
	struct twr_duty_sum s;
	k_spinlock_key_t key;
	uint32_t bp_calc, bp_meas;
	uint32_t freq_hz = cfg_get().frequency_hz;
	uint64_t budget_us = twr_duty_budget_us(freq_hz);
	unsigned int pct = (unsigned int)(budget_us / (TWR_DUTY_BUDGET_1PCT_US));

	ARG_UNUSED(argc);
	ARG_UNUSED(argv);
	key = k_spin_lock(&duty_lock);
	twr_duty_sum(&duty, k_uptime_get(), &s);
	k_spin_unlock(&duty_lock, key);
	bp_calc = twr_duty_budget_bp(s.formula_us, budget_us);
	bp_meas = twr_duty_budget_bp(s.measured_us, budget_us);
	shell_print(sh, "duty (last 60-61 min, %u %% = %u s/h at %u Hz, reported only): tx=%u", pct,
		    (unsigned int)(budget_us / 1000000u), freq_hz, s.n_tx);
	shell_print(sh, "  airtime formula=%llu us = %u.%02u %% of the %u %% budget",
		    (unsigned long long)s.formula_us, bp_calc / 100, bp_calc % 100, pct);
	shell_print(sh, "  airtime measured=%llu us = %u.%02u %% of the %u %% budget",
		    (unsigned long long)s.measured_us, bp_meas / 100, bp_meas % 100, pct);
	return 0;
}

#if defined(CONFIG_NB_TWR_RX_INJECT)
/* Bench (HIL): a captured air frame into the receive path as if the radio had received it
 * now. Everything after the radio is the real thing: classification, the replay rule, the
 * ACK on air at t_rx + turnaround, the events. */
static int cmd_rx_inject(const struct shell *sh, size_t argc, char **argv)
{
	/* The shell thread runs one command at a time; rx_q copies it. */
	static struct rx_msg m;
	long rssi = -60;
	int n;

	if (argc >= 3) {
		char *end;

		rssi = strtol(argv[2], &end, 10);
		if (*end != '\0' || rssi < -160 || rssi > 0) {
			shell_error(sh, "rssi_dbm -160..0");
			return -EINVAL;
		}
	}
	n = twr_link_hex_decode(argv[1], m.data, sizeof(m.data));
	if (n <= 0) {
		shell_error(sh, "frame: 1..%u bytes as hex digits", (unsigned int)sizeof(m.data));
		return -EINVAL;
	}
	if (state_get() != P2P_NB_GATEWAY) {
		shell_error(sh, "not in GATEWAY (state %s)", state_str(state_get()));
		return -EPERM;
	}
	m.t_cyc = k_cycle_get_64();
	m.rssi = (int16_t)rssi;
	m.snr = 0;
	m.len = (uint8_t)n;
	if (k_msgq_put(&rx_q, &m, K_NO_WAIT) != 0) {
		rs.rx_overrun++;
		shell_error(sh, "rx queue full");
		return -EBUSY;
	}
	k_work_submit_to_queue(&twr_wq, &rx_work);
	shell_print(sh, "injected %d B, rssi %ld dBm", n, rssi);
	return 0;
}

static int cmd_rx_last(const struct shell *sh, size_t argc, char **argv)
{
	static struct rx_msg m;
	char hex[2 * sizeof(m.data) + 1];
	bool valid;
	k_spinlock_key_t key;

	ARG_UNUSED(argc);
	ARG_UNUSED(argv);
	key = k_spin_lock(&rx_last_lock);
	valid = rx_last_valid;
	m = rx_last;
	k_spin_unlock(&rx_last_lock, key);
	if (!valid) {
		shell_print(sh, "no accepted uplink since boot");
		return 0;
	}
	for (unsigned int i = 0; i < m.len; i++) {
		snprintk(&hex[2 * i], 3, "%02x", m.data[i]);
	}
	hex[2 * m.len] = '\0';
	shell_print(sh, "t_ms=%llu rssi=%d len=%u", (unsigned long long)cyc_ms(m.t_cyc), m.rssi,
		    m.len);
	shell_print(sh, "%s", hex);
	return 0;
}
#else
#define cmd_rx_inject NULL
#define cmd_rx_last NULL
#endif

/*
 * Factory: receive on the shipped P2P channel (defaults.yaml p2p_radio) before any Hub has
 * configured the NB, so the provisioning station can count frames over RTT while the Hub's
 * Linux is still being written. BOOT only: a configured NB belongs to the host. Net id 0 and
 * ACKs off, so nothing is ever sent; `p2p stats` shows what was heard. The host's
 * SET_RADIO_CONFIG after RX_STOP, or a reset, ends it.
 */
static int cmd_listen(const struct shell *sh, size_t argc, char **argv)
{
	static const struct radio_config factory = {
		.frequency_hz = 869525000u,
		.sf = 7,
		.bw = P2P_BW_125K,
		.cr = 1,
		.preamble_len = 8,
		.tx_power_dbm = 14,
		.flags = 0,
	};
	enum p2p_status st;
	struct twr_gwc *g;

	ARG_UNUSED(argc);
	ARG_UNUSED(argv);

	if (radio_ctl_state() != P2P_NB_BOOT) {
		shell_error(sh, "listen: not in BOOT, the host owns the radio");
		return -EBUSY;
	}
	st = radio_ctl_set_config(&factory);
	if (st == P2P_STATUS_OK) {
		g = twr_rt_lock();
		/* The core starts only with a counter block. ACKs are off and nothing is queued,
		 * so counter 1 is never spent; the host's own block later replaces it. */
		st = twr_gwc_ctr_block(g, 1, 1);
		if (st == P2P_STATUS_OK) {
			st = twr_gwc_start(g, &(struct twr_gwc_params){
				.net_id = 0,
				.turnaround_ms = 20,
				.dl_gap_ms = 20,
				.flags = 0,
			});
		}
		twr_rt_unlock();
	}
	if (st == P2P_STATUS_OK) {
		st = twr_rt_enter_gateway();
	}
	if (st != P2P_STATUS_OK) {
		shell_error(sh, "listen: failed (status %d)", (int)st);
		return -EIO;
	}
	shell_print(sh, "listen: GATEWAY on 869525000 Hz SF7 BW125, ACKs off");
	return 0;
}

SHELL_STATIC_SUBCMD_SET_CREATE(twr_cmds,
	SHELL_CMD(show, NULL, "State, radio config, timing, counter block", cmd_show),
	SHELL_CMD(stats, NULL, "Counters since boot", cmd_stats),
	SHELL_CMD_ARG(peers, NULL, "Peer table [<start>] (no keys)", cmd_peers, 1, 1),
	SHELL_CMD(queue, NULL, "Downlink queue", cmd_queue),
	SHELL_CMD(duty, NULL, "Sliding-hour airtime vs the 1 % duty cycle", cmd_duty),
	SHELL_CMD(listen, NULL, "Factory: receive on the shipped channel from BOOT", cmd_listen),
	SHELL_COND_CMD_ARG(CONFIG_NB_TWR_RX_INJECT, rx_inject, NULL,
			   "Bench: <hex frame> [rssi_dbm] into the RX path as if received",
			   cmd_rx_inject, 2, 1),
	SHELL_COND_CMD(CONFIG_NB_TWR_RX_INJECT, rx_last, NULL,
		       "Bench: raw hex of the last accepted uplink (for rx_inject)", cmd_rx_last),
	SHELL_SUBCMD_SET_END);

SHELL_CMD_REGISTER(p2p, &twr_cmds, "P2P gateway: state, counters, factory capture", NULL);
