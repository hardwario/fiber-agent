/* SPDX-License-Identifier: Apache-2.0 */
#include "proto.h"
#include "gpio_table.h"
#include "uart_proto.h"
#include "adc_read.h"
#include "led_pwm.h"

#include <zephyr/kernel.h>
#include <zephyr/sys/printk.h>

/* Visual fault indicator: flash all red LEDs forever. Used when an init step
 * returns a hard failure — gives a physical signal in lieu of UART/RTT (which
 * may both be down). The caller is expected to printk() the specifics first. */
static void fault_blink(void)
{
	for (;;) {
		for (uint8_t i = 0; i < LED_COUNT; i++) {
			gpio_table_set(gpio_table_led_red(i), true);
		}
		k_msleep(200);
		for (uint8_t i = 0; i < LED_COUNT; i++) {
			gpio_table_set(gpio_table_led_red(i), false);
		}
		k_msleep(200);
	}
}

static void startup_animation(void)
{
	gpio_table_set(gpio_table_pwr_yellow(), true);

	for (uint8_t i = 0; i < LED_COUNT; i++) {
		gpio_table_set(gpio_table_led_green(i), true);
		k_msleep(125);
	}
	k_msleep(500);

	for (uint8_t i = 0; i < LED_COUNT; i++) {
		gpio_table_set(gpio_table_led_green(i), false);
	}
	gpio_table_set(gpio_table_pwr_yellow(), false);
}

int main(void)
{
	int rc;

	printk("south-zephyr boot\n");

	rc = gpio_table_init();
	if (rc < 0) {
		printk("gpio_table_init failed: %d\n", rc);
	}

	startup_animation();   /* run before any UART/ADC dependence on those */

	rc = uart_proto_init();
	if (rc < 0) {
		printk("uart_proto_init failed: %d\n", rc);
		/* Stay alive with a visible fault pattern so we never hang
		 * silently in uart_poll_out below. */
		fault_blink();
	}

	rc = adc_read_init();
	if (rc < 0) {
		printk("adc_read_init failed: %d\n", rc);
		/* Not fatal for the protocol path — just warn. */
	}

	rc = led_pwm_init();
	if (rc < 0) {
		printk("led_pwm_init failed: %d\n", rc);
	}
	led_pwm_start();

	printk("south-zephyr ready\n");
	uart_proto_write("GPIO UART control ready\r\n");
	uart_proto_write("Use: P0 ON, P7 TOGGLE, LED3G OFF, LED7R ON, VIN READ, VBAT READ\r\n");

	for (;;) {
		uart_proto_poll();
		k_msleep(1);
	}
	return 0;
}
