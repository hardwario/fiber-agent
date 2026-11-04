/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER P0 bench gateway runtime (-DTOWER_P0_GATEWAY=ON, bench RTT images only).
 *
 * The Northbridge becomes a standalone TOWER gateway driven from the RTT shell: no UART link,
 * no cmd_dispatch, no twr_gateway.c (radio_ctl.h here is only radio_ctl_call() for the
 * `radio tune` shell). PHY: LoRa BW125 CR4/5, preamble 8, explicit header, CRC on,
 * the P2P private sync word (public_network = false, as the central configures P2P today),
 * IQ normal, 14 dBm. Boot defaults match the P0 node image: 869.525 MHz SF7, gateway
 * 0x4E420001, node 0x00000413, static key 00..0f.
 *
 * Threads:
 *   system workqueue  sx126x DIO1 work -> sx126x_twr_dio1_hook() (RxDone / TxDone stamps)
 *                     -> IrqProcess -> lora_rx_cb(): copy into rx_q, kick rx_work.
 *   twr_wq (coop)     rx_work: decode + classify (twr_gw.c), TWR_RX line, and for a
 *                     CONFIRMED Data frame the ACK: release RX, lora_config(TX), take the
 *                     radio to STDBY_XOSC (TCXO settled), sleep to just before
 *                     t_rx + turnaround, lora_send(); sx126x_twr_tx_hook() then spins in
 *                     Radio.Send() to the exact deadline before SetTx goes out. With PENDING
 *                     the queued Data frame follows `window` ms after the ACK's TxDone. RX is
 *                     re-armed after every transmission.
 *   shell             `twr ...`; configuration changes are applied on twr_wq.
 *
 * Event lines (channel 0, SEGGER_RTT_Write like the P2P_OUT bridge lines), timestamps are
 * k_cycle_get_64() in microseconds truncated to u32 (wraps every 71.6 min):
 *   TWR_RX <frame hex> rssi=<dBm> snr=<dB> t_rx_us=<u32> res=<ok|dup|replay|mic_fail|...>
 *          [type= src= dest= ctr=] [pt=<plaintext hex>]
 *   TWR_ACK t_tx_us=<u32> ctr=<acked counter> pending=<0|1> ack_ctr=<gw counter>
 *          turnaround_us=<RxDone -> TX start> toa_us=<TX start -> TxDone> toa_calc_us=<formula>
 *   TWR_DL t_tx_us=<u32> len=<frame bytes> toa_us= toa_calc_us= ctr=<gw counter>
 *          dt_us=<ACK TxDone -> DL TX start> pl_len= confirmed=<0|1>
 *   TWR_RXERR kind=<crc|hdr> t_us=<u32>
 */
#include "twr_radio.h"

#include "diag.h"
#include "proto.h"
#include "radio_ctl.h"
#include "twr_airtime.h"
#include "twr_frame.h"
#include "twr_gw.h"

#include <string.h>
#include <zephyr/device.h>
#include <zephyr/drivers/lora.h>
#include <zephyr/kernel.h>
#include <zephyr/linker/section_tags.h>
#include <zephyr/shell/shell.h>
#include <zephyr/sys/printk.h>
#include <zephyr/sys/util.h>

#include <SEGGER_RTT.h>
#if defined(CONFIG_NB_RADIO_TUNE)
#include "radio_tune_hw.h"
#endif

#include <zephyr/logging/log.h>
LOG_MODULE_REGISTER(twr, CONFIG_LOG_DEFAULT_LEVEL);

#define TWR_GW_ADDR 0x4E420001u
#ifndef TWR_NODE_ADDR_DEFAULT
#define TWR_NODE_ADDR_DEFAULT 0x00000413u
#endif

#define TWR_FREQ_DEFAULT_HZ 869525000u
#define TWR_FREQ_MIN_HZ     863000000u
#define TWR_FREQ_MAX_HZ     870000000u
#define TWR_SF_DEFAULT      7u
#define TWR_BW_HZ           125000u
#define TWR_CR              1u /* 4/5 */
#define TWR_PREAMBLE        8u
#define TWR_TX_POWER_DBM    14

/* RxDone -> ACK TX start, a fixed wait as in TOWER (ACK_TURNAROUND). */
#define TWR_TURNAROUND_DEFAULT_MS 20u
#define TWR_TURNAROUND_MIN_MS     20u
#define TWR_TURNAROUND_MAX_MS     60u
/* ACK TxDone -> downlink TX start (TOWER's DOWNLINK_TURNAROUND). */
#define TWR_WINDOW_DEFAULT_MS 20u
#define TWR_WINDOW_MIN_MS     10u
#define TWR_WINDOW_MAX_MS     1000u

/* The node's windows (P0 node image SC 32efd9fb), for `twr show` and the `pend` check:
 * ACK RX window max(200, 20 + ToA(28 B) + 60) ms from RX armed, downlink window after PENDING
 * max(1000, ToA(96 B) + 200) ms. */
#define NODE_ACK_WIN_MIN_MS 200u
#define NODE_DL_WIN_MIN_MS  1000u

/* twr_wq sleeps until this long before a TX deadline, then lora_send(); the TX hook spins the
 * rest. Covers lora_send()'s SPI commands ahead of SetTx (~0.5 ms) plus a 1 ms tick. */
#define TWR_GATE_LEAD_US 2500u
/* Never spin longer than this in the TX hook (a deadline further out is a bug; go now). */
#define TWR_GATE_MAX_SPIN_US 20000u

/* sx12xx_lora_config() arms loramac's TX timeout at a fixed 4000 ms: a longer frame is cut
 * off mid-air, and lora_send() then never returns (its release after the k_poll timeout
 * fails and it waits K_FOREVER). So no frame over ~3.95 s ToA is sent: <= 100 B at SF12,
 * <= 198 B at SF11, anything at SF10 and below. */
#define TWR_TX_TOA_MAX_US 3950000u

/* SX126x IRQ status bits (datasheet table 13-29; loramac's sx126x.h is private to its lib). */
#define SX_IRQ_TX_DONE      0x0001u
#define SX_IRQ_RX_DONE      0x0002u
#define SX_IRQ_HEADER_ERROR 0x0020u
#define SX_IRQ_CRC_ERROR    0x0040u

/* patches/lora/sx126x.c, compiled with NB_TWR_HOOKS for this image */
void sx126x_twr_standby_xosc(void);

static const uint8_t bench_key[TWR_AES_KEY_LEN] = {
	0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
	0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
};

static const struct device *const lora_dev = DEVICE_DT_GET(DT_ALIAS(lora0));

struct twr_cfg {
	uint32_t freq_hz;
	uint8_t sf;
	uint16_t turnaround_ms;
	uint16_t window_ms;
};

static struct twr_cfg cfg = {
	.freq_hz = TWR_FREQ_DEFAULT_HZ,
	.sf = TWR_SF_DEFAULT,
	.turnaround_ms = TWR_TURNAROUND_DEFAULT_MS,
	.window_ms = TWR_WINDOW_DEFAULT_MS,
};
static struct k_spinlock cfg_lock;

static struct twr_gw gw;
static K_MUTEX_DEFINE(gw_lock);

struct twr_stats {
	uint32_t rx;         /* frames the driver delivered (CRC ok) */
	uint32_t ok;
	uint32_t dup;
	uint32_t replay;
	uint32_t mic_fail;
	uint32_t bad_hdr;    /* short / bad version / bad type */
	uint32_t not_for_us;
	uint32_t ack_rx;
	uint32_t other;
	uint32_t crc_err;    /* RxDone with CRC error (driver drops these) */
	uint32_t hdr_err;
	uint32_t rx_overrun; /* rx_q full */
	uint32_t acks;
	uint32_t dl;
	uint32_t tx_err;
	uint32_t rearm_fail;
	uint32_t turnaround_last_us;
	uint32_t turnaround_max_us;
	uint32_t ack_toa_last_us;
	uint32_t dl_toa_last_us;
};

static struct twr_stats st;

static struct twr_duty duty;
static struct k_spinlock duty_lock;

struct rx_msg {
	uint64_t t_cyc;
	int16_t rssi;
	int8_t snr;
	uint8_t len;
	uint8_t data[TWR_MAX_FRAME];
};

K_MSGQ_DEFINE(rx_q, sizeof(struct rx_msg), 4, 4);

K_THREAD_STACK_DEFINE(twr_wq_stack, 3072);
static struct k_work_q twr_wq;
static struct k_work rx_work;
static struct k_work cfg_work;

static bool rx_armed; /* twr_wq only */
static bool radio_ready;

/* Driver hook stamps (k_cycle_get_64). */
static volatile uint64_t rx_done_cyc;
static volatile uint64_t tx_done_cyc;
static volatile uint64_t tx_start_cyc;
static volatile uint64_t tx_gate_cyc;
static volatile bool tx_gate_armed;

/* Watchdog gate (main.c): uptime the running TX started, 0 when none, and its budget. */
static volatile int64_t tx_started_ms;
static volatile uint32_t tx_budget_ms;

/* The gateway TX counter survives a warm reset (watchdog, J-Link reset) in .noinit, so the
 * node's replay window for the gateway does not drop the next downlinks and the (static
 * bench key, counter) nonces are not reused. A power cycle starts again at 1; `twr ctr`. */
#define TWR_RETAINED_MAGIC 0x54575230u /* "TWR0" */
static __noinit struct {
	uint32_t magic;
	uint32_t next;
	uint32_t inv;
} retained;

static void retained_save(uint32_t next)
{
	retained.next = next;
	retained.inv = ~next;
	retained.magic = TWR_RETAINED_MAGIC;
}

static uint32_t retained_load(void)
{
	if (retained.magic == TWR_RETAINED_MAGIC && retained.inv == ~retained.next &&
	    retained.next != 0) {
		return retained.next;
	}
	return 1;
}

/* ---- helpers ------------------------------------------------------------------------ */

static uint32_t cyc_us32(uint64_t cyc)
{
	return (uint32_t)k_cyc_to_us_floor64(cyc);
}

static uint32_t cyc_delta_us(uint64_t later, uint64_t earlier)
{
	if (later <= earlier) {
		return 0;
	}
	return (uint32_t)MIN(k_cyc_to_us_floor64(later - earlier), (uint64_t)UINT32_MAX);
}

static struct twr_cfg cfg_get(void)
{
	k_spinlock_key_t key = k_spin_lock(&cfg_lock);
	struct twr_cfg c = cfg;

	k_spin_unlock(&cfg_lock, key);
	return c;
}

static struct twr_phy phy_of(const struct twr_cfg *c)
{
	return (struct twr_phy){
		.sf = c->sf,
		.bw_hz = TWR_BW_HZ,
		.cr = TWR_CR,
		.preamble = TWR_PREAMBLE,
		.explicit_hdr = true,
		.crc = true,
	};
}

static uint32_t toa_us(const struct twr_cfg *c, size_t len)
{
	struct twr_phy p = phy_of(c);

	return twr_lora_toa_us(&p, len);
}

static uint32_t node_ack_window_ms(const struct twr_cfg *c)
{
	return MAX(NODE_ACK_WIN_MIN_MS, 20u + toa_us(c, TWR_ACK_FRAME_LEN) / 1000u + 60u);
}

static uint32_t node_dl_window_ms(const struct twr_cfg *c)
{
	return MAX(NODE_DL_WIN_MIN_MS, toa_us(c, TWR_TOWER_MAX_FRAME) / 1000u + 200u);
}

static struct lora_modem_config modem_cfg(const struct twr_cfg *c, bool tx)
{
	return (struct lora_modem_config){
		.frequency = c->freq_hz,
		.bandwidth = BW_125_KHZ,
		.datarate = (enum lora_datarate)c->sf,
		.coding_rate = CR_4_5,
		.preamble_len = TWR_PREAMBLE,
		.tx_power = TWR_TX_POWER_DBM,
		.tx = tx,
		.iq_inverted = false,
		.public_network = false,
		.packet_crc_disable = false,
	};
}

static size_t hex_put(char *out, size_t cap, const uint8_t *b, size_t n)
{
	static const char digits[] = "0123456789abcdef";
	size_t p = 0;

	for (size_t i = 0; i < n && p + 2 < cap; i++) {
		out[p++] = digits[b[i] >> 4];
		out[p++] = digits[b[i] & 0x0f];
	}
	out[p] = '\0';
	return p;
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

static int hex_parse(const char *hex, uint8_t *out, size_t cap)
{
	size_t n = strlen(hex);

	if (n == 0 || (n % 2) != 0 || n / 2 > cap) {
		return -EINVAL;
	}
	for (size_t i = 0; i < n / 2; i++) {
		int hi = hex_nibble(hex[2 * i]);
		int lo = hex_nibble(hex[2 * i + 1]);

		if (hi < 0 || lo < 0) {
			return -EINVAL;
		}
		out[i] = (uint8_t)((hi << 4) | lo);
	}
	return (int)(n / 2);
}

/* Unsigned decimal (base 10, up to 10 digits) or hex (base 16, up to 8 digits), no sign. */
static bool parse_u32(const char *s, int base, uint32_t *out)
{
	uint64_t v = 0;
	size_t n = s != NULL ? strlen(s) : 0;

	if (n == 0 || n > (base == 16 ? 8u : 10u)) {
		return false;
	}
	for (size_t i = 0; i < n; i++) {
		int d = hex_nibble(s[i]);

		if (d < 0 || d >= base) {
			return false;
		}
		v = v * (uint64_t)base + (uint64_t)d;
	}
	if (v > UINT32_MAX) {
		return false;
	}
	*out = (uint32_t)v;
	return true;
}

/* ---- event lines -------------------------------------------------------------------- */

/* twr_wq only: TWR_RX / TWR_ACK / TWR_DL. Frame + plaintext hex + fields. */
static char evt_line[2 * TWR_MAX_FRAME + 2 * TWR_MAX_PAYLOAD + 192];

static void evt_write(size_t n)
{
	if (n > sizeof(evt_line) - 1) {
		n = sizeof(evt_line) - 1;
	}
	evt_line[n++] = '\n';
	SEGGER_RTT_Write(0, evt_line, (unsigned int)n);
}

/* snprintk() returns the untruncated length: keep the write position inside the buffer. */
static size_t evt_advance(size_t p, int n, size_t cap)
{
	return n < 0 ? p : MIN(p + (size_t)n, cap - 1);
}

static void emit_rx(const struct rx_msg *m, const struct twr_rx_out *o)
{
	size_t cap = sizeof(evt_line) - 1; /* one byte for the '\n' */
	size_t p = 0;

	p = evt_advance(p, snprintk(evt_line, cap, "TWR_RX "), cap);
	p += hex_put(&evt_line[p], cap - p, m->data, m->len);
	p = evt_advance(p, snprintk(&evt_line[p], cap - p, " rssi=%d snr=%d t_rx_us=%u res=%s",
				    m->rssi, m->snr, cyc_us32(m->t_cyc),
				    twr_rx_res_str(o->res)), cap);
	if (o->hdr_valid) {
		p = evt_advance(p, snprintk(&evt_line[p], cap - p,
					    " type=%u src=0x%08x dest=0x%08x ctr=%u", o->hdr.type,
					    o->hdr.src, o->hdr.dest, o->hdr.counter), cap);
	}
	if (o->res == TWR_RX_OK || o->res == TWR_RX_DUP || o->res == TWR_RX_ACK ||
	    o->res == TWR_RX_OTHER) {
		p = evt_advance(p, snprintk(&evt_line[p], cap - p, " pt="), cap);
		p += hex_put(&evt_line[p], cap - p, o->pt, o->pt_len);
	}
	evt_write(p);
}

/* System workqueue (DIO1 hook): its own small buffer. */
static void emit_rxerr(const char *kind, uint64_t t_cyc)
{
	char line[48];
	int n = snprintk(line, sizeof(line), "TWR_RXERR kind=%s t_us=%u\n", kind, cyc_us32(t_cyc));

	if (n > 0) {
		SEGGER_RTT_Write(0, line, (unsigned int)MIN((size_t)n, sizeof(line) - 1));
	}
}

/* ---- driver hooks (patches/lora/sx126x.c, NB_TWR_HOOKS) ------------------------------ */

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
			st.crc_err++;
			emit_rxerr("crc", now);
		}
	}
	if ((irq & SX_IRQ_HEADER_ERROR) != 0) {
		st.hdr_err++;
		emit_rxerr("hdr", now);
	}
}

/* SX126xSetOperatingMode(MODE_TX), right before the SetTx command: hold it to the gate. */
void sx126x_twr_tx_hook(void)
{
	if (tx_gate_armed) {
		uint64_t gate = tx_gate_cyc;
		uint64_t now = k_cycle_get_64();

		if (gate > now && gate - now < k_us_to_cyc_ceil64(TWR_GATE_MAX_SPIN_US)) {
			while (k_cycle_get_64() < gate) {
			}
		}
		tx_gate_armed = false;
	}
	tx_start_cyc = k_cycle_get_64();
}

/* ---- radio -------------------------------------------------------------------------- */

static void lora_rx_cb(const struct device *dev, uint8_t *data, uint16_t size, int16_t rssi,
		       int8_t snr, void *user_data)
{
	/* System workqueue, serialized: a static message keeps ~270 B off its stack. */
	static struct rx_msg m;
	uint64_t now = k_cycle_get_64();
	uint64_t t = rx_done_cyc;

	ARG_UNUSED(dev);
	ARG_UNUSED(user_data);

	st.rx++;
	/* The hook's RxDone stamp is this frame's unless it is stale. */
	if (t == 0 || t > now || now - t > k_ms_to_cyc_ceil64(100)) {
		t = now;
	}
	m.t_cyc = t;
	m.rssi = rssi;
	m.snr = snr;
	m.len = (uint8_t)MIN(size, TWR_MAX_FRAME);
	memcpy(m.data, data, m.len);
	if (k_msgq_put(&rx_q, &m, K_NO_WAIT) != 0) {
		st.rx_overrun++;
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

static void rx_arm(const struct twr_cfg *c)
{
	struct lora_modem_config lc = modem_cfg(c, false);
	int rc;

	rx_release();
	rc = modem_config_retry(&lc);
	if (rc == 0) {
		rc = lora_recv_async(lora_dev, lora_rx_cb, NULL);
	}
	if (rc == 0) {
		rx_armed = true;
		diag_set_radio_state(DIAG_RADIO_RX);
	} else {
		st.rearm_fail++;
		diag_set_radio_state(DIAG_RADIO_ERROR);
		LOG_ERR("RX arm failed (%d)", rc);
	}
}

/* Release RX, configure TX and warm the TCXO so SetTx starts without its 5 ms wait. */
static int tx_prepare(const struct twr_cfg *c)
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
	uint64_t start_cyc;
	uint64_t done_cyc;
	uint32_t toa_calc_us;
	uint32_t toa_meas_us;
};

/* Transmit `frame` with SetTx at `gate` (k_cycle_get_64), or at once when that has passed. */
static void tx_at(const struct twr_cfg *c, uint64_t gate, uint8_t *frame, size_t len,
		  struct tx_result *r)
{
	uint64_t lead = k_us_to_cyc_ceil64(TWR_GATE_LEAD_US);
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
	r->start_cyc = tx_start_cyc != 0 ? tx_start_cyc : now;
	r->done_cyc = tx_done_cyc != 0 ? tx_done_cyc : k_cycle_get_64();
	r->toa_meas_us = r->rc == 0 ? cyc_delta_us(r->done_cyc, r->start_cyc) : 0;
	if (r->rc != 0) {
		st.tx_err++;
		LOG_ERR("lora_send failed (%d)", r->rc);
	}

	/* Airtime is spent whenever SetTx went out, even if the driver reported an error. */
	if (tx_start_cyc != 0) {
		key = k_spin_lock(&duty_lock);
		twr_duty_add(&duty, k_uptime_get(), r->toa_calc_us, r->toa_meas_us);
		k_spin_unlock(&duty_lock, key);
	}
}

static void send_ack_and_dl(const struct rx_msg *m, struct twr_rx_out *o)
{
	static uint8_t dl_frame[TWR_MAX_FRAME];
	struct twr_cfg c = cfg_get();
	struct tx_result ack = { 0 };
	struct tx_result dl = { 0 };
	uint64_t gate = m->t_cyc + k_ms_to_cyc_floor64(c.turnaround_ms);
	uint32_t turnaround_us;
	int rc = tx_prepare(&c);

	if (rc != 0) {
		st.tx_err++;
		snprintk(evt_line, sizeof(evt_line) - 2,
			 "TWR_ACK err=%d ctr=%u pending=%d ack_ctr=%u", rc, o->hdr.counter,
			 o->ack_pending ? 1 : 0, o->ack_ctr);
		evt_write(strlen(evt_line));
		rx_arm(&c);
		return;
	}

	tx_at(&c, gate, o->ack_frame, o->ack_len, &ack);
	turnaround_us = cyc_delta_us(ack.start_cyc, m->t_cyc);
	if (ack.rc == 0) {
		st.acks++;
		st.turnaround_last_us = turnaround_us;
		st.turnaround_max_us = MAX(st.turnaround_max_us, turnaround_us);
		st.ack_toa_last_us = ack.toa_meas_us;
		snprintk(evt_line, sizeof(evt_line) - 2,
			 "TWR_ACK t_tx_us=%u ctr=%u pending=%d ack_ctr=%u turnaround_us=%u "
			 "toa_us=%u toa_calc_us=%u len=%u",
			 cyc_us32(ack.start_cyc), o->hdr.counter, o->ack_pending ? 1 : 0,
			 o->ack_ctr, turnaround_us, ack.toa_meas_us, ack.toa_calc_us,
			 (unsigned int)o->ack_len);
	} else {
		snprintk(evt_line, sizeof(evt_line) - 2,
			 "TWR_ACK err=%d t_tx_us=%u ctr=%u pending=%d ack_ctr=%u", ack.rc,
			 cyc_us32(ack.start_cyc), o->hdr.counter, o->ack_pending ? 1 : 0,
			 o->ack_ctr);
	}
	evt_write(strlen(evt_line));

	if (ack.rc == 0 && o->ack_pending) {
		uint32_t dl_ctr = 0;
		size_t n;
		bool confirmed;
		uint8_t pl_len;
		uint32_t dl_toa;

		k_mutex_lock(&gw_lock, K_FOREVER);
		confirmed = gw.dl_confirmed;
		pl_len = gw.dl_len;
		dl_toa = toa_us(&c, TWR_HDR_LEN + pl_len + TWR_TAG_LEN);
		if (gw.dl_queued && dl_toa > TWR_TX_TOA_MAX_US) {
			/* `twr sf` moved after `twr pend` checked it: drop, never cut mid-air. */
			gw.dl_queued = false;
			gw.dl_len = 0;
			n = 0;
		} else {
			n = twr_gw_build_dl(&gw, dl_frame, sizeof(dl_frame), &dl_ctr);
			retained_save(gw.tx_counter);
		}
		k_mutex_unlock(&gw_lock);

		if (n == 0 && dl_toa > TWR_TX_TOA_MAX_US) {
			st.tx_err++;
			snprintk(evt_line, sizeof(evt_line) - 2,
				 "TWR_DL err=toa pl_len=%u toa_calc_us=%u (over the 4 s TX timeout)",
				 pl_len, dl_toa);
			evt_write(strlen(evt_line));
		} else if (n == 0) {
			snprintk(evt_line, sizeof(evt_line) - 2, "TWR_DL err=%s",
				 gw.tx_locked ? "ctr_locked" : "empty");
			evt_write(strlen(evt_line));
		} else {
			/* The radio slept after TxDone; wake it and settle the TCXO first. */
			sx126x_twr_standby_xosc();
			tx_at(&c, ack.done_cyc + k_ms_to_cyc_floor64(c.window_ms), dl_frame, n,
			      &dl);
			if (dl.rc == 0) {
				st.dl++;
				st.dl_toa_last_us = dl.toa_meas_us;
				snprintk(evt_line, sizeof(evt_line) - 2,
					 "TWR_DL t_tx_us=%u len=%u toa_us=%u toa_calc_us=%u ctr=%u "
					 "dt_us=%u pl_len=%u confirmed=%d",
					 cyc_us32(dl.start_cyc), (unsigned int)n, dl.toa_meas_us,
					 dl.toa_calc_us, dl_ctr,
					 cyc_delta_us(dl.start_cyc, ack.done_cyc), pl_len,
					 confirmed ? 1 : 0);
			} else {
				snprintk(evt_line, sizeof(evt_line) - 2,
					 "TWR_DL err=%d len=%u ctr=%u pl_len=%u confirmed=%d",
					 dl.rc, (unsigned int)n, dl_ctr, pl_len,
					 confirmed ? 1 : 0);
			}
			evt_write(strlen(evt_line));
		}
	}

	rx_arm(&c);
}

static void handle_frame(const struct rx_msg *m)
{
	static struct twr_rx_out o;

	k_mutex_lock(&gw_lock, K_FOREVER);
	twr_gw_on_rx(&gw, m->data, m->len, m->rssi, &o);
	retained_save(gw.tx_counter);
	k_mutex_unlock(&gw_lock);

	switch (o.res) {
	case TWR_RX_OK:
		st.ok++;
		break;
	case TWR_RX_DUP:
		st.dup++;
		break;
	case TWR_RX_REPLAY:
		st.replay++;
		break;
	case TWR_RX_MIC_FAIL:
		st.mic_fail++;
		break;
	case TWR_RX_NOT_FOR_US:
		st.not_for_us++;
		break;
	case TWR_RX_ACK:
		st.ack_rx++;
		break;
	case TWR_RX_OTHER:
		st.other++;
		break;
	default:
		st.bad_hdr++;
		break;
	}

	/* The line goes out before the ACK: it costs microseconds, the turnaround is fixed. */
	emit_rx(m, &o);
	if (o.send_ack) {
		send_ack_and_dl(m, &o);
	}
}

static void rx_work_handler(struct k_work *work)
{
	static struct rx_msg m;

	ARG_UNUSED(work);
	while (k_msgq_get(&rx_q, &m, K_NO_WAIT) == 0) {
		handle_frame(&m);
	}
}

static void cfg_work_handler(struct k_work *work)
{
	struct twr_cfg c = cfg_get();

	ARG_UNUSED(work);
	rx_arm(&c);
}

int twr_radio_init(void)
{
	twr_gw_init(&gw, TWR_GW_ADDR, TWR_NODE_ADDR_DEFAULT, bench_key);
	twr_gw_set_tx_counter(&gw, retained_load());
	retained_save(gw.tx_counter);
	if (gw.tx_counter != 1) {
		LOG_INF("TX counter resumes at %u (retained over a warm reset)", gw.tx_counter);
	}
	twr_duty_init(&duty);

	k_work_init(&rx_work, rx_work_handler);
	k_work_init(&cfg_work, cfg_work_handler);
	k_work_queue_init(&twr_wq);
	k_work_queue_start(&twr_wq, twr_wq_stack, K_THREAD_STACK_SIZEOF(twr_wq_stack),
			   K_PRIO_COOP(7), NULL);
	k_thread_name_set(&twr_wq.thread, "twr_wq");

	if (!device_is_ready(lora_dev)) {
		diag_set_radio_state(DIAG_RADIO_ERROR);
		return -ENODEV;
	}
	radio_ready = true;
#if defined(CONFIG_NB_RADIO_TUNE)
	/* Before RX is first armed: nothing else drives the radio yet (docs/problems.md §1). */
	radio_tune_hw_boot();
#endif
	k_work_submit_to_queue(&twr_wq, &cfg_work);
	return 0;
}

/* The `radio tune` shell's seam (radio_ctl.h), on twr_wq like every radio access here. */
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
	if (was_armed) {
		struct twr_cfg cf = cfg_get();

		rx_arm(&cf);
		c->rc = rx_armed ? 0 : -EIO;
	}
	k_sem_give(&c->done);
}

int radio_ctl_call(void (*fn)(void *arg), void *arg, bool pause_rx)
{
	if (!radio_ready) {
		return -ENODEV;
	}
	k_mutex_lock(&call_lock, K_FOREVER);
	k_work_init(&call.work, radio_call_handler);
	k_sem_init(&call.done, 0, 1);
	call.fn = fn;
	call.arg = arg;
	call.pause_rx = pause_rx;
	k_work_submit_to_queue(&twr_wq, &call.work);
	k_sem_take(&call.done, K_FOREVER);

	int rc = call.rc;

	k_mutex_unlock(&call_lock);
	return rc;
}

uint32_t twr_radio_gw_addr(void)
{
	return TWR_GW_ADDR;
}

uint32_t twr_radio_node_addr(void)
{
	return gw.node_addr;
}

bool twr_radio_tx_stuck(void)
{
	int64_t started = tx_started_ms;

	return started != 0 && k_uptime_get() - started > (int64_t)tx_budget_ms;
}

void twr_radio_print_brief(const struct shell *sh)
{
	struct twr_cfg c = cfg_get();

	shell_print(sh, "twr: freq=%u sf=%u rx=%u ok=%u acks=%u dl=%u rx_armed=%d (see `twr show`)",
		    c.freq_hz, c.sf, st.rx, st.ok, st.acks, st.dl, rx_armed ? 1 : 0);
}

/* ---- shell -------------------------------------------------------------------------- */

static void cfg_apply(void)
{
	k_work_submit_to_queue(&twr_wq, &cfg_work);
}

static int cmd_sf(const struct shell *sh, size_t argc, char **argv)
{
	uint32_t sf;
	k_spinlock_key_t key;

	if (argc != 2 || !parse_u32(argv[1], 10, &sf) || sf < 7 || sf > 12) {
		shell_error(sh, "usage: twr sf <7..12>");
		return -EINVAL;
	}
	key = k_spin_lock(&cfg_lock);
	cfg.sf = (uint8_t)sf;
	k_spin_unlock(&cfg_lock, key);
	cfg_apply();
	shell_print(sh, "sf=%u (RX re-armed)", sf);
	return 0;
}

static int cmd_freq(const struct shell *sh, size_t argc, char **argv)
{
	uint32_t hz;
	k_spinlock_key_t key;

	if (argc != 2 || !parse_u32(argv[1], 10, &hz) || hz < TWR_FREQ_MIN_HZ ||
	    hz > TWR_FREQ_MAX_HZ) {
		shell_error(sh, "usage: twr freq <%u..%u Hz>", TWR_FREQ_MIN_HZ, TWR_FREQ_MAX_HZ);
		return -EINVAL;
	}
	key = k_spin_lock(&cfg_lock);
	cfg.freq_hz = hz;
	k_spin_unlock(&cfg_lock, key);
	cfg_apply();
	shell_print(sh, "freq=%u (RX re-armed)", hz);
	return 0;
}

static int cmd_turnaround(const struct shell *sh, size_t argc, char **argv)
{
	uint32_t ms;
	k_spinlock_key_t key;

	if (argc != 2 || !parse_u32(argv[1], 10, &ms) || ms < TWR_TURNAROUND_MIN_MS ||
	    ms > TWR_TURNAROUND_MAX_MS) {
		shell_error(sh, "usage: twr turnaround <%u..%u ms>", TWR_TURNAROUND_MIN_MS,
			    TWR_TURNAROUND_MAX_MS);
		return -EINVAL;
	}
	key = k_spin_lock(&cfg_lock);
	cfg.turnaround_ms = (uint16_t)ms;
	k_spin_unlock(&cfg_lock, key);
	shell_print(sh, "turnaround=%u ms (RxDone -> ACK TX start)", ms);
	return 0;
}

static int cmd_window(const struct shell *sh, size_t argc, char **argv)
{
	uint32_t ms;
	k_spinlock_key_t key;

	if (argc != 2 || !parse_u32(argv[1], 10, &ms) || ms < TWR_WINDOW_MIN_MS ||
	    ms > TWR_WINDOW_MAX_MS) {
		shell_error(sh, "usage: twr window <%u..%u ms>", TWR_WINDOW_MIN_MS,
			    TWR_WINDOW_MAX_MS);
		return -EINVAL;
	}
	key = k_spin_lock(&cfg_lock);
	cfg.window_ms = (uint16_t)ms;
	k_spin_unlock(&cfg_lock, key);
	shell_print(sh, "window=%u ms (ACK TxDone -> downlink TX start)", ms);
	return 0;
}

static void print_dl_fit(const struct shell *sh, const struct twr_cfg *c, size_t pl_len)
{
	size_t frame = TWR_HDR_LEN + pl_len + TWR_TAG_LEN;
	uint32_t end_ms = c->window_ms + (toa_us(c, frame) + 999u) / 1000u;
	uint32_t win_ms = node_dl_window_ms(c);

	if (end_ms > win_ms) {
		shell_warn(sh, "downlink frame %zu B ends %u ms after the ACK at SF%u, past the node's "
			   "%u ms window", frame, end_ms, c->sf, win_ms);
	}
}

static int cmd_pend(const struct shell *sh, size_t argc, char **argv)
{
	static uint8_t buf[TWR_MAX_PAYLOAD];
	struct twr_cfg c = cfg_get();
	bool confirmed = false;
	int n;

	if (argc == 1) {
		k_mutex_lock(&gw_lock, K_FOREVER);
		shell_print(sh, "downlink queued=%d len=%u confirmed=%d", gw.dl_queued ? 1 : 0,
			    gw.dl_len, gw.dl_confirmed ? 1 : 0);
		k_mutex_unlock(&gw_lock);
		return 0;
	}
	if (argc == 2 && strcmp(argv[1], "clear") == 0) {
		k_mutex_lock(&gw_lock, K_FOREVER);
		gw.dl_queued = false;
		gw.dl_len = 0;
		k_mutex_unlock(&gw_lock);
		shell_print(sh, "downlink queue cleared");
		return 0;
	}
	if (argc == 3 && strcmp(argv[2], "c") == 0) {
		confirmed = true;
	} else if (argc != 2) {
		shell_error(sh, "usage: twr pend [<hex payload, 1..%u B> [c]] | twr pend clear",
			    (unsigned int)TWR_MAX_PAYLOAD);
		return -EINVAL;
	}
	n = hex_parse(argv[1], buf, sizeof(buf));
	if (n <= 0) {
		shell_error(sh, "payload: even number of hex digits, 1..%u bytes",
			    (unsigned int)TWR_MAX_PAYLOAD);
		return -EINVAL;
	}
	if (toa_us(&c, TWR_HDR_LEN + (size_t)n + TWR_TAG_LEN) > TWR_TX_TOA_MAX_US) {
		shell_error(sh, "frame %d B takes %u us at SF%u, over the driver's 4 s TX timeout",
			    n + (int)(TWR_HDR_LEN + TWR_TAG_LEN),
			    toa_us(&c, TWR_HDR_LEN + (size_t)n + TWR_TAG_LEN), c.sf);
		return -EINVAL;
	}
	k_mutex_lock(&gw_lock, K_FOREVER);
	twr_gw_queue_dl(&gw, buf, (size_t)n, confirmed);
	k_mutex_unlock(&gw_lock);
	shell_print(sh, "downlink queued: %d B%s to 0x%08x after the next ACK (PENDING), frame %d B",
		    n, confirmed ? " confirmed" : "", gw.node_addr,
		    n + (int)(TWR_HDR_LEN + TWR_TAG_LEN));
	print_dl_fit(sh, &c, (size_t)n);
	return 0;
}

static int cmd_node(const struct shell *sh, size_t argc, char **argv)
{
	uint32_t addr;
	const char *s;

	if (argc == 1) {
		shell_print(sh, "node=0x%08x", gw.node_addr);
		return 0;
	}
	s = argv[1];
	if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) {
		s += 2;
	}
	if (argc != 2 || strlen(s) > 8 || !parse_u32(s, 16, &addr)) {
		shell_error(sh, "usage: twr node <hex32>");
		return -EINVAL;
	}
	k_mutex_lock(&gw_lock, K_FOREVER);
	gw.node_addr = addr;
	k_mutex_unlock(&gw_lock);
	shell_print(sh, "node=0x%08x", addr);
	return 0;
}

static int cmd_ctr(const struct shell *sh, size_t argc, char **argv)
{
	uint32_t next;

	if (argc == 2) {
		if (!parse_u32(argv[1], 10, &next) || next == 0) {
			shell_error(sh, "usage: twr ctr [<next TX counter, >= 1>]");
			return -EINVAL;
		}
		k_mutex_lock(&gw_lock, K_FOREVER);
		twr_gw_set_tx_counter(&gw, next);
		retained_save(gw.tx_counter);
		k_mutex_unlock(&gw_lock);
	} else if (argc != 1) {
		shell_error(sh, "usage: twr ctr [<next TX counter, >= 1>]");
		return -EINVAL;
	}
	shell_print(sh, "tx_counter next=%u locked=%d", gw.tx_counter, gw.tx_locked ? 1 : 0);
	return 0;
}

static int cmd_stats(const struct shell *sh, size_t argc, char **argv)
{
	if (argc == 2 && strcmp(argv[1], "reset") == 0) {
		memset(&st, 0, sizeof(st));
		shell_print(sh, "stats reset");
		return 0;
	}
	shell_print(sh, "rx=%u ok=%u dup=%u replay=%u mic_fail=%u bad_hdr=%u not_for_us=%u "
		    "ack_rx=%u other=%u", st.rx, st.ok, st.dup, st.replay, st.mic_fail, st.bad_hdr,
		    st.not_for_us, st.ack_rx, st.other);
	shell_print(sh, "crc_err=%u hdr_err=%u rx_overrun=%u rearm_fail=%u", st.crc_err,
		    st.hdr_err, st.rx_overrun, st.rearm_fail);
	shell_print(sh, "acks=%u dl=%u tx_err=%u", st.acks, st.dl, st.tx_err);
	shell_print(sh, "turnaround_us last=%u max=%u; toa_us ack_last=%u dl_last=%u",
		    st.turnaround_last_us, st.turnaround_max_us, st.ack_toa_last_us,
		    st.dl_toa_last_us);
	return 0;
}

static int cmd_duty(const struct shell *sh, size_t argc, char **argv)
{
	struct twr_duty_sum s;
	k_spinlock_key_t key;
	uint32_t bp_calc, bp_meas;
	uint32_t freq_hz = cfg_get().freq_hz;
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

static int cmd_show(const struct shell *sh, size_t argc, char **argv)
{
	struct twr_cfg c = cfg_get();

	ARG_UNUSED(argc);
	ARG_UNUSED(argv);
	shell_print(sh, "TOWER P0 gateway fw %d.%d.%d-twr-p0: gw=0x%08x node=0x%08x key=00..0f "
		    "(static bench key)", FW_VERSION_MAJOR, FW_VERSION_MINOR, FW_VERSION_PATCH,
		    TWR_GW_ADDR, gw.node_addr);
	shell_print(sh, "phy: freq=%u sf=%u bw=125 cr=4/5 preamble=%u hdr=explicit crc=on "
		    "sync=private iq=normal tx_power=%d", c.freq_hz, c.sf, TWR_PREAMBLE,
		    TWR_TX_POWER_DBM);
	shell_print(sh, "timing: turnaround=%u ms (RxDone -> ACK TX) window=%u ms "
		    "(ACK TxDone -> DL TX)", c.turnaround_ms, c.window_ms);
	shell_print(sh, "toa: ack(%u B)=%u us max(%u B)=%u us; node windows: ack=%u ms dl=%u ms",
		    (unsigned int)TWR_ACK_FRAME_LEN, toa_us(&c, TWR_ACK_FRAME_LEN),
		    (unsigned int)TWR_TOWER_MAX_FRAME, toa_us(&c, TWR_TOWER_MAX_FRAME),
		    node_ack_window_ms(&c), node_dl_window_ms(&c));
	k_mutex_lock(&gw_lock, K_FOREVER);
	shell_print(sh, "tx_counter next=%u locked=%d; downlink queued=%d len=%u confirmed=%d",
		    gw.tx_counter, gw.tx_locked ? 1 : 0, gw.dl_queued ? 1 : 0, gw.dl_len,
		    gw.dl_confirmed ? 1 : 0);
	k_mutex_unlock(&gw_lock);
	shell_print(sh, "radio: rx_armed=%d", rx_armed ? 1 : 0);
	return 0;
}

SHELL_STATIC_SUBCMD_SET_CREATE(twr_cmds,
	SHELL_CMD_ARG(sf, NULL, "Spreading factor <7..12>", cmd_sf, 2, 0),
	SHELL_CMD_ARG(freq, NULL, "Carrier <Hz> (863-870 MHz)", cmd_freq, 2, 0),
	SHELL_CMD_ARG(turnaround, NULL, "RxDone -> ACK TX wait <20..60 ms>", cmd_turnaround, 2, 0),
	SHELL_CMD_ARG(window, NULL, "ACK TxDone -> downlink TX wait <ms>", cmd_window, 2, 0),
	SHELL_CMD_ARG(pend, NULL, "Queue one downlink: <hex> [c] | clear | (show)", cmd_pend, 1,
		      2),
	SHELL_CMD_ARG(node, NULL, "Downlink / PENDING target address <hex32>", cmd_node, 1, 1),
	SHELL_CMD_ARG(ctr, NULL, "Gateway TX counter: show or set <next>", cmd_ctr, 1, 1),
	SHELL_CMD_ARG(stats, NULL, "Counters [reset]", cmd_stats, 1, 1),
	SHELL_CMD(duty, NULL, "Sliding-hour airtime vs the 1 % duty cycle", cmd_duty),
	SHELL_CMD(show, NULL, "PHY, addresses, timing", cmd_show),
	SHELL_SUBCMD_SET_END);

SHELL_CMD_REGISTER(twr, &twr_cmds, "TOWER P0 bench gateway", NULL);
