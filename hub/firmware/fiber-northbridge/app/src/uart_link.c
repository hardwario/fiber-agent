/* SPDX-License-Identifier: Apache-2.0
 *
 * P2P binary protocol transport over the dedicated northbridge<->CM4 link
 * (USART1 / ttyAMA3 on the host side). Mirrors fiber-southbridge's
 * uart_proto.c ISR + ring-buffer pattern, but frames HDLC/binary instead
 * of ASCII lines.
 *
 * TX goes through a ring drained by the UART interrupt: the gateway sends events from its
 * radio workqueue between an ACK and the downlink that follows it, and a blocking
 * uart_poll_out() of a 280 B body (~25 ms at 115200 Bd) there would push the downlink out of
 * its slot. Senders are serialized by one mutex, so a frame is never interleaved with
 * another. A frame goes into the ring whole or not at all; a sender that finds no room waits
 * a bounded time (TX_WAIT_*_MS) and then drops the frame (tx_drops, GET_INFO link_err).
 *
 * Event seq (#470 link §4.1): the ring events (EVT_TWR_UPLINK, EVT_TWR_TX) carry their
 * delivery seq from the gateway's event ring (twr_evq.c), which also resends what the host
 * did not ack, so a dropped one is not lost. The best-effort events carry the current
 * delivery seq without advancing it (uart_link_set_event_seq()).
 *
 * Bench alternative: build with -DBENCH_RTT_BRIDGE=ON (app/CMakeLists.txt)
 * for rigs with no CM4/USB-serial wired to the real UART. Frames go over
 * the RTT shell as hex text instead of raw USART1 bytes: rttt (the J-Link
 * RTT console this project drives bench units with) exposes a
 * command/log-line tool surface, not a raw byte-stream one, so a host
 * drives this with the `p2p_inject <hex>` shell command (main.c) and reads
 * outgoing frames back as `P2P_OUT <hex>` log lines. Never for production.
 */
#include "uart_link.h"
#include "proto.h"
#include "hdlc.h"
#include "cmd_dispatch.h"
#include "diag.h"

#include <string.h>
#include <zephyr/kernel.h>
#include <zephyr/sys/atomic.h>

/* How long a sender waits for room on the link before it drops the frame: the main thread
 * (RSPs) may wait for most of a full ring to drain; the cooperative radio workqueue
 * (events) must not hold up an ACK or a downlink for long. */
#define TX_WAIT_PREEMPT_MS 50
#define TX_WAIT_COOP_MS    10

static struct hdlc_decoder dec;
static uint32_t frame_err_count;
static atomic_t tx_drops;

static K_MUTEX_DEFINE(tx_mutex);
/* Under tx_mutex: the one frame being encoded. */
static uint8_t tx_frame[P2P_MAX_ENCODED_LEN];
/* The newest delivery seq of the event ring: the seq of every best-effort event. */
static atomic_t event_seq;

static void on_hdlc_frame(uint8_t type, uint8_t seq, const uint8_t *body,
			   size_t body_len, void *user_data)
{
	ARG_UNUSED(user_data);

	proto_on_frame(type, seq, body, body_len);
}

static void feed_byte(uint8_t byte)
{
	enum hdlc_feed_result r = hdlc_decoder_feed(&dec, byte, on_hdlc_frame, NULL);

	switch (r) {
	case HDLC_FEED_CRC_ERROR:
	case HDLC_FEED_TOO_SHORT:
	case HDLC_FEED_OVERFLOW:
		frame_err_count++;
		diag_report(DIAG_CODE_LINK_FRAMING, "hdlc frame error");
		break;
	default:
		break;
	}
}

/* Put one encoded frame on the link, whole, by `deadline` (k_uptime_get()). */
static bool tx_write(const uint8_t *frame, size_t len, int64_t deadline);

static void link_send(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len)
{
	int32_t budget_ms = k_is_preempt_thread() ? TX_WAIT_PREEMPT_MS : TX_WAIT_COOP_MS;
	int64_t deadline = k_uptime_get() + budget_ms;
	int n;

	if (k_is_in_isr() || k_mutex_lock(&tx_mutex, K_MSEC(budget_ms)) != 0) {
		atomic_inc(&tx_drops);
		return;
	}
	n = hdlc_encode(type, seq, body, body_len, tx_frame, sizeof(tx_frame));
	/* n < 0 is only reachable with a caller bug (body_len > P2P_MAX_BODY_LEN). */
	if (n > 0 && !tx_write(tx_frame, (size_t)n, deadline)) {
		atomic_inc(&tx_drops);
	}
	k_mutex_unlock(&tx_mutex);
}

void uart_link_send(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len)
{
	link_send(type, seq, body, body_len);
}

void uart_link_send_event(uint8_t type, const uint8_t *body, size_t body_len)
{
	link_send(type, (uint8_t)atomic_get(&event_seq), body, body_len);
}

void uart_link_set_event_seq(uint8_t seq)
{
	atomic_set(&event_seq, seq);
}

#if defined(APP_P2P_BENCH_RTT_BRIDGE)

#include <SEGGER_RTT.h>

static const char hex_digits[] = "0123456789abcdef";

/* Under tx_mutex: "P2P_OUT " + hex + '\n'. */
static char tx_line[8 + 2 * P2P_MAX_ENCODED_LEN + 1];

int uart_link_init(void)
{
	hdlc_decoder_init(&dec);
	return 0;
}

void uart_link_poll(void)
{
	/* No async byte stream to drain in this mode - p2p_inject (main.c)
	 * feeds the decoder directly and synchronously. */
}

void uart_link_bench_inject(const uint8_t *bytes, size_t len)
{
	for (size_t i = 0; i < len; i++) {
		feed_byte(bytes[i]);
	}
}

static bool tx_write(const uint8_t *frame, size_t len, int64_t deadline)
{
	size_t p = 0;

	ARG_UNUSED(deadline);

	/* P2P_OUT <hex> written SYNCHRONOUSLY to RTT channel 0 (the shell/log
	 * channel rttt reads as its terminal). Deliberately NOT via LOG_INF:
	 * Zephyr's deferred logging queues the message to a low-priority thread,
	 * adding hundreds of ms before it reaches RTT - fatal for a host-side
	 * central that must answer an EVT_RX JoinRequest inside the node's ~1 s
	 * RX1 window. A direct SEGGER_RTT_Write puts the line on the wire in
	 * microseconds. The reactor parses P2P_OUT lines by regex, so
	 * interleaving with shell output is harmless. */
	memcpy(tx_line, "P2P_OUT ", 8);
	p = 8;
	for (size_t i = 0; i < len; i++) {
		tx_line[p++] = hex_digits[frame[i] >> 4];
		tx_line[p++] = hex_digits[frame[i] & 0x0f];
	}
	tx_line[p++] = '\n';
	SEGGER_RTT_Write(0, tx_line, (unsigned int)p);
	return true;
}

bool uart_link_tx_drain(int32_t timeout_ms)
{
	/* tx_write() is synchronous here. */
	ARG_UNUSED(timeout_ms);
	return true;
}

#else /* !APP_P2P_BENCH_RTT_BRIDGE: the real USART1/ttyAMA3 link */

#include <zephyr/devicetree.h>
#include <zephyr/drivers/uart.h>
#include <zephyr/sys/ring_buffer.h>

#define UART_NODE DT_ALIAS(p2p_uart)

/* Room for a few worst-case frames or a burst of small events; 178 ms of line time. */
#define UART_TX_RING_SIZE 2048

static const struct device *const uart_dev = DEVICE_DT_GET(UART_NODE);

RING_BUF_DECLARE(rx_rb, UART_RX_RING_SIZE);
RING_BUF_DECLARE(tx_rb, UART_TX_RING_SIZE);
/* The TX ring between tx_write() and the ISR (one producer at a time: tx_mutex). */
static struct k_spinlock tx_lock;

/* Diagnostic counters, mirroring uart_proto.c's naming. */
volatile uint32_t dbg_isr_bytes;
volatile uint32_t dbg_isr_drops;
/* Hardware overruns seen by the ISR, reported from the poll loop (an ISR must not send). */
static volatile uint32_t uart_overruns;
static uint32_t uart_overruns_reported;
static uint32_t isr_drops_reported;
volatile uint32_t dbg_tx_bytes;

static void uart_isr(const struct device *dev, void *user_data)
{
	ARG_UNUSED(user_data);

	if (!uart_irq_update(dev)) {
		return;
	}

	/* Clear latched ORE/FE/NE/PE - with CR3.EIE set an unhandled ORE
	 * leaves RX stuck (see fiber-southbridge/app/src/uart_proto.c for
	 * the same finding on the sibling MCU). */
	if (uart_err_check(dev) & UART_ERROR_OVERRUN) {
		uart_overruns++;
	}

	while (uart_irq_rx_ready(dev)) {
		uint8_t byte;
		int n = uart_fifo_read(dev, &byte, 1);

		if (n != 1) {
			break;
		}
		if (ring_buf_put(&rx_rb, &byte, 1) == 1) {
			dbg_isr_bytes++;
		} else {
			dbg_isr_drops++;
		}
	}

	if (uart_irq_tx_ready(dev)) {
		k_spinlock_key_t key = k_spin_lock(&tx_lock);
		uint8_t *p;
		uint32_t n = ring_buf_get_claim(&tx_rb, &p, 16);

		if (n == 0) {
			uart_irq_tx_disable(dev);
		} else {
			int sent = uart_fifo_fill(dev, p, (int)n);

			sent = sent > 0 ? sent : 0;
			ring_buf_get_finish(&tx_rb, (uint32_t)sent);
			dbg_tx_bytes += (uint32_t)sent;
		}
		k_spin_unlock(&tx_lock, key);
	}
}

int uart_link_init(void)
{
	if (!device_is_ready(uart_dev)) {
		return -ENODEV;
	}

	hdlc_decoder_init(&dec);

	uart_irq_rx_disable(uart_dev);
	uart_irq_tx_disable(uart_dev);

	int rc = uart_irq_callback_user_data_set(uart_dev, uart_isr, NULL);

	if (rc < 0) {
		return rc;
	}
	uart_irq_rx_enable(uart_dev);
	uart_irq_err_enable(uart_dev);
	return 0;
}

void uart_link_poll(void)
{
	uint8_t byte;

	while (ring_buf_get(&rx_rb, &byte, 1) == 1) {
		feed_byte(byte);
	}

	/* An overrun in hardware or a full ring both lose bytes of a frame; report them here,
	 * in thread context, once per poll however many there were. */
	uint32_t overruns = uart_overruns;
	uint32_t drops = dbg_isr_drops;

	if (overruns != uart_overruns_reported || drops != isr_drops_reported) {
		uart_overruns_reported = overruns;
		isr_drops_reported = drops;
		diag_report(DIAG_CODE_UART_OVERRUN, "uart rx overrun");
	}
}

static bool tx_write(const uint8_t *frame, size_t len, int64_t deadline)
{
	for (;;) {
		k_spinlock_key_t key = k_spin_lock(&tx_lock);
		bool fits = ring_buf_space_get(&tx_rb) >= len;

		if (fits) {
			ring_buf_put(&tx_rb, frame, (uint32_t)len);
		}
		/* TC interrupt on: the ISR drains the ring and turns it off when it is empty. */
		uart_irq_tx_enable(uart_dev);
		k_spin_unlock(&tx_lock, key);
		if (fits) {
			return true;
		}
		if (k_uptime_get() >= deadline) {
			return false;
		}
		k_sleep(K_MSEC(1));
	}
}

bool uart_link_tx_drain(int32_t timeout_ms)
{
	int64_t deadline = k_uptime_get() + timeout_ms;

	for (;;) {
		/* The ISR empties the ring into the TX register; TC then says the shift register
		 * is idle too, i.e. the last byte is on the wire. */
		if (ring_buf_is_empty(&tx_rb) && uart_irq_tx_complete(uart_dev) > 0) {
			return true;
		}
		if (k_uptime_get() >= deadline) {
			return false;
		}
		k_sleep(K_MSEC(1));
	}
}

#endif /* APP_P2P_BENCH_RTT_BRIDGE */

uint32_t uart_link_get_crc_err_count(void)
{
	return frame_err_count;
}

uint32_t uart_link_get_tx_drops(void)
{
	return (uint32_t)atomic_get(&tx_drops);
}
