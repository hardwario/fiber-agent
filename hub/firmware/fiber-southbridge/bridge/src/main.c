/* SPDX-License-Identifier: Apache-2.0
 *
 * Throwaway UART<->RTT bridge for fiber-southbridge (STM32C031C6).
 *
 * Transparent byte pipe between USART1 (PA9/PA10, 115200 8N1 to the CM4's
 * /dev/ttyAMA4) and SEGGER RTT channel 0, so a J-Link RTT client can act as a
 * serial terminal for whatever is talking on the other end of USART1. No
 * shell, no console subsystem, no framing/protocol — just bytes.
 *
 * WHY THIS EXISTS. The Hub's carrier board does not break out the CM4's own
 * console pins (GPIO14/15) and its USB OTG port is in host mode, so USART1 is
 * the only wire that reaches the CM4. Running this instead of the protocol
 * firmware turns the J-Link on the SWD header into that missing serial port:
 * point the Hub's Linux console at ttyAMA4 and a boot log and login prompt
 * come out of RTT channel 0. Bench tooling - it replaces the command protocol
 * rather than coexisting with it, so a Hub running this cannot drive its
 * sensor-power pins, LEDs or ADC until app/ is flashed back.
 *
 * ttyAMA4, not ttyAMA3: the comment at the top of the board .dts says ttyAMA3
 * and is wrong. Every code path that actually opens the port - tools/
 * flash_southbridge.py, meta-fiber's wlprogmanager, and v1's stm.rs - uses
 * /dev/ttyAMA4.
 *
 * RX (UART -> RTT): interrupt-driven. The UART IRQ drains the hardware FIFO
 * with uart_fifo_read() into a small stack buffer and forwards it straight
 * to SEGGER_RTT_Write() from ISR context. RTT's default NO_BLOCK_SKIP mode
 * makes that non-blocking (it drops the write if the up-buffer is full
 * instead of stalling), so it's safe to call from an ISR.
 *
 * TX (RTT -> UART): polled from the main loop. SEGGER_RTT_Read() drains
 * whatever the host has typed into the down-channel and each byte goes out
 * via uart_poll_out(). A short sleep backs off when there's nothing to do.
 */

#include <zephyr/kernel.h>
#include <zephyr/devicetree.h>
#include <zephyr/drivers/uart.h>

#include <SEGGER_RTT.h>

#define RTT_CHANNEL   0
#define ISR_RX_CHUNK  32
#define POLL_RX_CHUNK 32
#define IDLE_SLEEP_MS 1

static const struct device *const uart_dev = DEVICE_DT_GET(DT_NODELABEL(usart1));

static void uart_isr(const struct device *dev, void *user_data)
{
	ARG_UNUSED(user_data);

	if (!uart_irq_update(dev)) {
		return;
	}

	/* Clear any latched error flags (ORE/FE/NE/PE) so the RX path
	 * doesn't get stuck re-asserting the IRQ without delivering bytes. */
	(void)uart_err_check(dev);

	while (uart_irq_rx_ready(dev)) {
		uint8_t buf[ISR_RX_CHUNK];
		int n = uart_fifo_read(dev, buf, sizeof(buf));

		if (n <= 0) {
			break;
		}
		SEGGER_RTT_Write(RTT_CHANNEL, buf, (unsigned)n);
	}
}

int main(void)
{
	if (!device_is_ready(uart_dev)) {
		/* Nothing else to fall back to: no console, no shell. Loop
		 * forever rather than returning into a dead main thread. */
		for (;;) {
			k_msleep(1000);
		}
	}

	uart_irq_rx_disable(uart_dev);
	uart_irq_tx_disable(uart_dev);
	uart_irq_callback_user_data_set(uart_dev, uart_isr, NULL);
	uart_irq_rx_enable(uart_dev);
	uart_irq_err_enable(uart_dev);

	for (;;) {
		uint8_t buf[POLL_RX_CHUNK];
		unsigned n = SEGGER_RTT_Read(RTT_CHANNEL, buf, sizeof(buf));

		if (n == 0) {
			k_msleep(IDLE_SLEEP_MS);
			continue;
		}
		for (unsigned i = 0; i < n; i++) {
			uart_poll_out(uart_dev, buf[i]);
		}
	}

	return 0;
}
