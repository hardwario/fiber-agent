/* SPDX-License-Identifier: Apache-2.0
 *
 * Diagnostic shell commands exposed via RTT (Segger J-Link).
 * Independent of the USART1 protocol — both can run in parallel.
 */
#ifdef CONFIG_SHELL

#include "proto.h"
#include "gpio_table.h"
#include "led_pwm.h"

#include <stdlib.h>
#include <string.h>
#include <zephyr/kernel.h>
#include <zephyr/shell/shell.h>

static char led_color_char(uint8_t c)
{
	switch (c) {
	case LED_COLOR_GREEN:  return 'G';
	case LED_COLOR_RED:    return 'R';
	case LED_COLOR_YELLOW: return 'Y';
	default:               return 'O';
	}
}

static char pwr_color_char(uint8_t c)
{
	switch (c) {
	case PWR_COLOR_GREEN:  return 'G';
	case PWR_COLOR_YELLOW: return 'Y';
	case PWR_COLOR_LIME:   return 'L';
	default:               return 'O';
	}
}

static char pattern_char(uint8_t p)
{
	switch (p) {
	case BLINK_SLOW: return 'L';
	case BLINK_FAST: return 'F';
	default:         return 'S';
	}
}

static uint8_t parse_led_color(char c)
{
	switch (c) {
	case 'G': case 'g': return LED_COLOR_GREEN;
	case 'R': case 'r': return LED_COLOR_RED;
	case 'Y': case 'y': return LED_COLOR_YELLOW;
	default:            return LED_COLOR_OFF;
	}
}

static uint8_t parse_pwr_color(char c)
{
	switch (c) {
	case 'G': case 'g': return PWR_COLOR_GREEN;
	case 'Y': case 'y': return PWR_COLOR_YELLOW;
	case 'L': case 'l': return PWR_COLOR_LIME;
	default:            return PWR_COLOR_OFF;
	}
}

static uint8_t parse_pattern(char c)
{
	switch (c) {
	case 'L': case 'l': return BLINK_SLOW;
	case 'F': case 'f': return BLINK_FAST;
	default:            return BLINK_STEADY;
	}
}

static int cmd_status(const struct shell *sh, size_t argc, char **argv)
{
	ARG_UNUSED(argc); ARG_UNUSED(argv);

	shell_print(sh, "firmware  : %s", FIRMWARE_VERSION);
	shell_print(sh, "uptime_ms : %lld", (long long)k_uptime_get());
	shell_print(sh, "brightness: %u", (unsigned)led_pwm_get_brightness());

	for (uint8_t i = 0; i < LED_COUNT; i++) {
		uint8_t color, pattern;
		led_pwm_get_led(i, &color, &pattern);
		shell_print(sh, "led%u      : %c %c",
			    i, led_color_char(color), pattern_char(pattern));
	}

	uint8_t pcolor, ppattern;
	led_pwm_get_pwr(&pcolor, &ppattern);
	shell_print(sh, "pwr       : %c %c",
		    pwr_color_char(pcolor), pattern_char(ppattern));
	return 0;
}

static int cmd_led(const struct shell *sh, size_t argc, char **argv)
{
	if (argc != 4) {
		shell_print(sh, "usage: led <0-7> <O|G|R|Y> <S|L|F>");
		return -EINVAL;
	}
	int idx = atoi(argv[1]);
	if (idx < 0 || idx >= LED_COUNT) {
		shell_print(sh, "idx out of range");
		return -EINVAL;
	}
	led_pwm_set_led((uint8_t)idx, parse_led_color(argv[2][0]),
			parse_pattern(argv[3][0]));
	return 0;
}

static int cmd_pwr(const struct shell *sh, size_t argc, char **argv)
{
	if (argc != 3) {
		shell_print(sh, "usage: pwr <O|G|Y|L> <S|L|F>");
		return -EINVAL;
	}
	led_pwm_set_pwr(parse_pwr_color(argv[1][0]),
			parse_pattern(argv[2][0]));
	return 0;
}

static int cmd_bright(const struct shell *sh, size_t argc, char **argv)
{
	if (argc != 2) {
		shell_print(sh, "usage: bright <0-100>");
		return -EINVAL;
	}
	int v = atoi(argv[1]);
	if (v < 0) v = 0;
	if (v > 100) v = 100;
	led_pwm_set_brightness((uint8_t)v);
	return 0;
}

static int cmd_gpio(const struct shell *sh, size_t argc, char **argv)
{
	if (argc != 3) {
		shell_print(sh, "usage: gpio <NAME> <on|off|toggle>");
		return -EINVAL;
	}
	const struct gpio_entry *e = gpio_table_lookup(argv[1]);
	if (!e) {
		shell_print(sh, "unknown gpio: %s", argv[1]);
		return -EINVAL;
	}
	if (strcmp(argv[2], "on") == 0 || strcmp(argv[2], "ON") == 0) {
		gpio_table_set(e, true);
	} else if (strcmp(argv[2], "off") == 0 || strcmp(argv[2], "OFF") == 0) {
		gpio_table_set(e, false);
	} else if (strcmp(argv[2], "toggle") == 0 || strcmp(argv[2], "TOGGLE") == 0) {
		gpio_table_toggle(e);
	} else {
		shell_print(sh, "unknown action: %s", argv[2]);
		return -EINVAL;
	}
	return 0;
}

extern volatile uint32_t dbg_isr_bytes;
extern volatile uint32_t dbg_isr_drops;
extern volatile uint32_t dbg_dispatch_calls;
extern volatile uint32_t dbg_tx_calls;
extern volatile uint32_t dbg_tx_bytes;

static int cmd_dbg(const struct shell *sh, size_t argc, char **argv)
{
	ARG_UNUSED(argc); ARG_UNUSED(argv);
	shell_print(sh, "isr_bytes      : %u", (unsigned)dbg_isr_bytes);
	shell_print(sh, "isr_drops      : %u", (unsigned)dbg_isr_drops);
	shell_print(sh, "dispatch_calls : %u", (unsigned)dbg_dispatch_calls);
	shell_print(sh, "tx_calls       : %u", (unsigned)dbg_tx_calls);
	shell_print(sh, "tx_bytes       : %u", (unsigned)dbg_tx_bytes);
	return 0;
}

SHELL_CMD_ARG_REGISTER(dbg,    NULL, "uart rx/tx diagnostic counters", cmd_dbg, 1, 0);
SHELL_CMD_ARG_REGISTER(status, NULL, "show state",         cmd_status, 1, 0);
SHELL_CMD_ARG_REGISTER(led,    NULL, "led <i> <O|G|R|Y> <S|L|F>", cmd_led, 4, 0);
SHELL_CMD_ARG_REGISTER(pwr,    NULL, "pwr <O|G|Y|L> <S|L|F>",     cmd_pwr, 3, 0);
SHELL_CMD_ARG_REGISTER(bright, NULL, "bright <0-100>",            cmd_bright, 2, 0);
SHELL_CMD_ARG_REGISTER(gpio,   NULL, "gpio <NAME> <on|off|toggle>", cmd_gpio, 3, 0);

#endif /* CONFIG_SHELL */
