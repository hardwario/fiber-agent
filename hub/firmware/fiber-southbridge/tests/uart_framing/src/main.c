/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for the UART line framing in uart_proto.c.
 *
 * Compiles the real uart_proto.c and drives uart_proto_poll() by putting
 * bytes straight into its (non-static) rx_rb ring buffer — the same seam
 * the RX ISR uses. dispatch_line() is doubled here to capture the framed
 * lines. uart_proto_init() is never called, so no UART interrupts are
 * involved; uart_proto_write() runs for real against native_sim's uart0,
 * and TX assertions use the dbg_tx_bytes counter as a byte-count proxy
 * for the reply content.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/ring_buffer.h>
#include <string.h>

#include "proto.h"
#include "uart_proto.h"
#include "cmd_dispatch.h"

/* Internal seams of uart_proto.c (all deliberately non-static there). */
extern struct ring_buf rx_rb;
extern volatile uint32_t dbg_tx_calls;
extern volatile uint32_t dbg_tx_bytes;
extern volatile uint32_t dbg_dispatch_calls;

#define ERR_TOO_LONG_LEN (sizeof("ERR: line too long\r\n") - 1) /* 20 */

/* ---- dispatch_line double: capture framed lines ----------------------- */

#define MAX_CAPTURED 8

static char captured[MAX_CAPTURED][RX_LINE_BUFFER_SIZE];
static int captured_count;

void dispatch_line(char *line)
{
	if (captured_count < MAX_CAPTURED) {
		strncpy(captured[captured_count], line,
			RX_LINE_BUFFER_SIZE - 1);
		captured[captured_count][RX_LINE_BUFFER_SIZE - 1] = '\0';
	}
	captured_count++;
}

/* ---- helpers ------------------------------------------------------------ */

static uint32_t tx_bytes_at_start;

static void feed(const char *bytes)
{
	size_t len = strlen(bytes);
	uint32_t put = ring_buf_put(&rx_rb, (const uint8_t *)bytes, len);

	zassert_equal(put, (uint32_t)len, "ring buffer overflow in feed()");
}

static uint32_t tx_delta(void)
{
	return dbg_tx_bytes - tx_bytes_at_start;
}

static void reset_state(void *fixture)
{
	ARG_UNUSED(fixture);

	/* Flush line_buf/line_len residue a previous test may have left in
	 * uart_proto.c's static state: a terminator dispatches any partial
	 * line as junk, which we then discard with the capture reset.
	 */
	feed("\n");
	uart_proto_poll();

	captured_count = 0;
	memset(captured, 0, sizeof(captured));
	tx_bytes_at_start = dbg_tx_bytes;
}

ZTEST_SUITE(uart_framing, NULL, NULL, reset_state, NULL, NULL);

/* ---- line termination ---------------------------------------------------- */

ZTEST(uart_framing, test_lf_terminates_line)
{
	feed("P0 ON\n");
	uart_proto_poll();
	zassert_equal(captured_count, 1);
	zassert_str_equal(captured[0], "P0 ON");
}

ZTEST(uart_framing, test_cr_terminates_line)
{
	/* the production CM4 host (fiber_app) sends CR alone */
	feed("P0 ON\r");
	uart_proto_poll();
	zassert_equal(captured_count, 1);
	zassert_str_equal(captured[0], "P0 ON");
}

ZTEST(uart_framing, test_crlf_dispatches_once)
{
	feed("P0 ON\r\n");
	uart_proto_poll();
	zassert_equal(captured_count, 1, "LF of CRLF must not dispatch an empty line");
	zassert_str_equal(captured[0], "P0 ON");
}

ZTEST(uart_framing, test_empty_lines_are_skipped)
{
	feed("A\nB\n\n\nC\n");
	uart_proto_poll();
	zassert_equal(captured_count, 3);
	zassert_str_equal(captured[0], "A");
	zassert_str_equal(captured[1], "B");
	zassert_str_equal(captured[2], "C");
}

ZTEST(uart_framing, test_line_split_across_polls)
{
	feed("P0 ");
	uart_proto_poll();
	zassert_equal(captured_count, 0);

	feed("ON\n");
	uart_proto_poll();
	zassert_equal(captured_count, 1);
	zassert_str_equal(captured[0], "P0 ON");
}

/* ---- length limit ---------------------------------------------------------- */

ZTEST(uart_framing, test_max_length_line_intact)
{
	/* RX_LINE_BUFFER_SIZE - 1 = 31 chars is the longest legal line */
	static const char max_line[] = "0123456789012345678901234567890"; /* 31 */

	BUILD_ASSERT(sizeof(max_line) - 1 == RX_LINE_BUFFER_SIZE - 1);

	feed(max_line);
	feed("\n");
	uart_proto_poll();
	zassert_equal(captured_count, 1);
	zassert_str_equal(captured[0], max_line);
	zassert_equal(tx_delta(), 0);
}

ZTEST(uart_framing, test_overlong_line_dropped_with_error)
{
	/* 40 chars overflows at the 32nd; the remainder up to the terminator
	 * is swallowed, and the following line must still dispatch cleanly.
	 */
	feed("0123456789012345678901234567890123456789\n");
	feed("OK2\n");
	uart_proto_poll();

	zassert_equal(captured_count, 1, "overlong line must not dispatch");
	zassert_str_equal(captured[0], "OK2");
	/* byte-count proxy for the "ERR: line too long\r\n" reply */
	zassert_equal(tx_delta(), ERR_TOO_LONG_LEN);
}

ZTEST(uart_framing, test_recovery_after_overflow_without_terminator)
{
	/* Overflow with no terminator in the buffer yet: the error is
	 * emitted, the fragment is discarded, and framing recovers on the
	 * next complete line.
	 */
	feed("0123456789012345678901234567890123456789");
	uart_proto_poll();
	zassert_equal(captured_count, 0);
	zassert_equal(tx_delta(), ERR_TOO_LONG_LEN);

	feed("X\n");
	uart_proto_poll();
	zassert_equal(captured_count, 1);
	zassert_str_equal(captured[0], "X");
}
