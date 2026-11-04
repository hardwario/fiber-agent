/* SPDX-License-Identifier: Apache-2.0 */
#include "uart_proto.h"
#include "proto.h"
#include "cmd_dispatch.h"

#include <string.h>
#include <zephyr/kernel.h>
#include <zephyr/devicetree.h>
#include <zephyr/drivers/uart.h>
#include <zephyr/sys/ring_buffer.h>

#define UART_NODE DT_ALIAS(host_uart)

static const struct device *const uart_dev = DEVICE_DT_GET(UART_NODE);

RING_BUF_DECLARE(rx_rb, UART_RX_RING_SIZE);

static char line_buf[RX_LINE_BUFFER_SIZE];
static size_t line_len;

/* Diagnostic counters */
volatile uint32_t dbg_isr_bytes;       /* incremented in UART RX ISR */
volatile uint32_t dbg_isr_drops;       /* bytes dropped due to ring-buf full */
volatile uint32_t dbg_dispatch_calls;  /* incremented after dispatch_line */
volatile uint32_t dbg_tx_calls;        /* incremented in uart_proto_write */
volatile uint32_t dbg_tx_bytes;        /* total bytes pushed to uart_poll_out */

static void uart_isr(const struct device *dev, void *user_data)
{
	ARG_UNUSED(user_data);

	if (!uart_irq_update(dev)) {
		return;
	}

	/* Clear any latched error flags (ORE/FE/NE/PE). With CR3.EIE set,
	 * an unhandled ORE leaves the RX path stuck: RDR no longer accepts
	 * new bytes and the IRQ keeps reasserting. uart_err_check clears
	 * the flags via ICR. */
	(void)uart_err_check(dev);

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
}

int uart_proto_init(void)
{
	if (!device_is_ready(uart_dev)) {
		return -ENODEV;
	}
	uart_irq_rx_disable(uart_dev);
	uart_irq_tx_disable(uart_dev);

	int rc = uart_irq_callback_user_data_set(uart_dev, uart_isr, NULL);
	if (rc < 0) {
		return rc;
	}
	uart_irq_rx_enable(uart_dev);
	/* Match the legacy CubeIDE/HAL config: enable USART error
	 * interrupts (sets CR3.EIE). Without this, ORE/FE/NE errors
	 * silently leave the RX path stuck and lose subsequent bytes. */
	uart_irq_err_enable(uart_dev);
	return 0;
}

void uart_proto_write_bytes(const char *buf, size_t len)
{
	dbg_tx_calls++;
	for (size_t i = 0; i < len; i++) {
		uart_poll_out(uart_dev, (uint8_t)buf[i]);
		dbg_tx_bytes++;
	}
}

void uart_proto_write(const char *s)
{
	uart_proto_write_bytes(s, strlen(s));
}

void uart_proto_poll(void)
{
	uint8_t byte;
	while (ring_buf_get(&rx_rb, &byte, 1) == 1) {
		/* Accept CR, LF, or CRLF as line terminator. Empty lines
		 * (e.g. the LF following a CR in a CRLF pair) are silently
		 * ignored. The original CubeIDE firmware terminated on LF
		 * only, but the production CM4 host (fiber_app) sends CR
		 * alone. */
		if (byte == '\r' || byte == '\n') {
			if (line_len > 0) {
				line_buf[line_len] = '\0';
				dispatch_line(line_buf);
				dbg_dispatch_calls++;
				line_len = 0;
			}
			continue;
		}
		if (line_len < (RX_LINE_BUFFER_SIZE - 1)) {
			line_buf[line_len++] = (char)byte;
		} else {
			line_len = 0;
			uart_proto_write("ERR: line too long\r\n");
			/* swallow remainder of this overflowing line */
			while (ring_buf_get(&rx_rb, &byte, 1) == 1) {
				if (byte == '\r' || byte == '\n') {
					break;
				}
			}
		}
	}
}
