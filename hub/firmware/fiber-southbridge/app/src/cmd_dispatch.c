/* SPDX-License-Identifier: Apache-2.0 */
#include "cmd_dispatch.h"
#include "proto.h"
#include "uart_proto.h"
#include "gpio_table.h"
#include "adc_read.h"
#include "led_pwm.h"

#include <string.h>
#include <stdio.h>      /* snprintf for STATUS/UPTIME output */
#include <stdlib.h>
#include <zephyr/kernel.h>

static uint8_t map_led_color(char c)
{
	switch (c) {
	case 'G': case 'g': return LED_COLOR_GREEN;
	case 'R': case 'r': return LED_COLOR_RED;
	case 'Y': case 'y': return LED_COLOR_YELLOW;
	default:            return LED_COLOR_OFF;
	}
}

static uint8_t map_pwr_color(char c)
{
	switch (c) {
	case 'G': case 'g': return PWR_COLOR_GREEN;
	case 'Y': case 'y': return PWR_COLOR_YELLOW;
	case 'L': case 'l': return PWR_COLOR_LIME;
	default:            return PWR_COLOR_OFF;
	}
}

static uint8_t map_pattern(char c)
{
	switch (c) {
	case 'L': case 'l': return BLINK_SLOW;
	case 'F': case 'f': return BLINK_FAST;
	default:            return BLINK_STEADY;
	}
}

static int parse_uint(const char *s, unsigned int *out)
{
	if (!*s) {
		return -1;
	}
	char *end = NULL;
	unsigned long v = strtoul(s, &end, 10);
	if (end == s || *end != '\0') {
		return -1;
	}
	*out = (unsigned int)v;
	return 0;
}

static const char *skip_ws(const char *p)
{
	while (*p == ' ' || *p == '\t') p++;
	return p;
}

static size_t copy_token(char *dst, size_t dstsz, const char **pp)
{
	const char *p = *pp;
	size_t n = 0;
	while (*p && *p != ' ' && *p != '\t' && n + 1 < dstsz) {
		dst[n++] = *p++;
	}
	dst[n] = '\0';
	*pp = p;
	return n;
}

static void handle_led_cmd(const char *line)
{
	/* Expect "LED <uint> <colorchar> <patternchar>". */
	const char *p = skip_ws(line + 3);  /* past "LED" */
	if (*p < '0' || *p > '9') {
		uart_proto_write("ERR: use LED <0-7> <O|G|R|Y> <S|L|F>\r\n");
		return;
	}
	unsigned int idx = 0;
	while (*p >= '0' && *p <= '9') {
		idx = idx * 10 + (unsigned)(*p - '0');
		p++;
	}
	if (idx >= LED_COUNT) {
		uart_proto_write("ERR: LED index must be 0-7\r\n");
		return;
	}
	p = skip_ws(p);
	char color_c = *p ? *p : 'O';
	while (*p && *p != ' ' && *p != '\t') p++;
	p = skip_ws(p);
	char pattern_c = *p ? *p : 'S';

	led_pwm_set_led((uint8_t)idx, map_led_color(color_c),
			map_pattern(pattern_c));
	uart_proto_write("OK\r\n");
}

static void handle_pwr_cmd(const char *line)
{
	/* Expect "PWR <colorchar> <patternchar>". */
	const char *p = skip_ws(line + 3);  /* past "PWR" */
	if (*p == '\0') {
		uart_proto_write("ERR: use PWR <O|G|Y|L> <S|L|F>\r\n");
		return;
	}
	char color_c = *p;
	while (*p && *p != ' ' && *p != '\t') p++;
	p = skip_ws(p);
	char pattern_c = *p ? *p : 'S';

	led_pwm_set_pwr(map_pwr_color(color_c), map_pattern(pattern_c));
	uart_proto_write("OK\r\n");
}

static void handle_bright_cmd(const char *action)
{
	unsigned int val;
	if (parse_uint(action, &val) < 0) {
		uart_proto_write("ERR: invalid brightness value\r\n");
		return;
	}
	if (val > 100) val = 100;
	led_pwm_set_brightness((uint8_t)val);
	uart_proto_write("OK\r\n");
}

static char color_char_for_line(uint8_t c)
{
	switch (c) {
	case LED_COLOR_GREEN:  return 'G';
	case LED_COLOR_RED:    return 'R';
	case LED_COLOR_YELLOW: return 'Y';
	default:               return 'O';
	}
}

static char color_char_for_pwr(uint8_t c)
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

static void handle_help_cmd(void)
{
	static const char help[] =
		"HELP: P<0-7> ON|OFF|TOGGLE       drive output pin\r\n"
		"HELP: LED<i><G|R> ON|OFF|TOGGLE  drive a single LED pin directly\r\n"
		"HELP: PWRLEDG|PWRLEDY ON|OFF|TOGGLE\r\n"
		"HELP: LED <0-7> <O|G|R|Y> <S|L|F>  composed line LED\r\n"
		"HELP: PWR <O|G|Y|L> <S|L|F>        composed power LED\r\n"
		"HELP: BRIGHT <0-100>             global LED brightness\r\n"
		"HELP: VIN READ | VBAT READ        ADC sample\r\n"
		"HELP: SYNC                       reset blink phase\r\n"
		"HELP: STATUS | VERSION | UPTIME  diagnostics\r\n";
	uart_proto_write(help);
}

static void handle_version_cmd(void)
{
	uart_proto_write("VERSION: " FIRMWARE_VERSION "\r\n");
}

static void handle_uptime_cmd(void)
{
	char buf[48];
	int n = snprintf(buf, sizeof(buf), "UPTIME: %lldms\r\n",
			 (long long)k_uptime_get());
	if (n > 0) {
		uart_proto_write_bytes(buf, (size_t)n);
	}
}

static void handle_status_cmd(void)
{
	char buf[64];
	int n = snprintf(buf, sizeof(buf), "STATUS: brightness=%u\r\n",
			 (unsigned)led_pwm_get_brightness());
	if (n > 0) {
		uart_proto_write_bytes(buf, (size_t)n);
	}

	for (uint8_t i = 0; i < LED_COUNT; i++) {
		uint8_t color, pattern;
		led_pwm_get_led(i, &color, &pattern);
		n = snprintf(buf, sizeof(buf), "STATUS: led%u=%c,%c\r\n",
			     i, color_char_for_line(color),
			     pattern_char(pattern));
		if (n > 0) {
			uart_proto_write_bytes(buf, (size_t)n);
		}
	}

	uint8_t pcolor, ppattern;
	led_pwm_get_pwr(&pcolor, &ppattern);
	n = snprintf(buf, sizeof(buf), "STATUS: pwr=%c,%c\r\n",
		     color_char_for_pwr(pcolor), pattern_char(ppattern));
	if (n > 0) {
		uart_proto_write_bytes(buf, (size_t)n);
	}
}

static void handle_gpio_cmd(const char *name, const char *action)
{
	const struct gpio_entry *e = gpio_table_lookup(name);
	if (!e) {
		uart_proto_write("ERR: unknown GPIO name\r\n");
		return;
	}
	if (strcmp(action, "ON") == 0) {
		gpio_table_set(e, true);
	} else if (strcmp(action, "OFF") == 0) {
		gpio_table_set(e, false);
	} else if (strcmp(action, "TOGGLE") == 0) {
		gpio_table_toggle(e);
	} else {
		uart_proto_write("ERR: unknown action (use ON/OFF/TOGGLE)\r\n");
		return;
	}
	uart_proto_write("OK\r\n");
}

void dispatch_line(char *line)
{
	char name[16];
	char action[16];

	const char *p = skip_ws(line);
	copy_token(name, sizeof(name), &p);
	p = skip_ws(p);
	copy_token(action, sizeof(action), &p);

	/* Single-token commands */
	if (strcmp(name, "SYNC") == 0) {
		led_pwm_sync();
		uart_proto_write("OK\r\n");
		return;
	}
	if (strcmp(name, "HELP") == 0) {
		handle_help_cmd();
		return;
	}
	if (strcmp(name, "VERSION") == 0) {
		handle_version_cmd();
		return;
	}
	if (strcmp(name, "UPTIME") == 0) {
		handle_uptime_cmd();
		return;
	}
	if (strcmp(name, "STATUS") == 0) {
		handle_status_cmd();
		return;
	}

	if (name[0] == '\0' || action[0] == '\0') {
		/* LED/PWR are multi-token and parsed against the full line — peek */
		if (strcmp(name, "LED") == 0) {
			handle_led_cmd(line);
			return;
		}
		if (strcmp(name, "PWR") == 0) {
			handle_pwr_cmd(line);
			return;
		}
		uart_proto_write("ERR: syntax. Use <NAME> <ON|OFF|TOGGLE|READ>\r\n");
		return;
	}

	/* Multi-token forms (LED/PWR) — parse the whole line. */
	if (strcmp(name, "LED") == 0) {
		handle_led_cmd(line);
		return;
	}
	if (strcmp(name, "PWR") == 0) {
		handle_pwr_cmd(line);
		return;
	}

	/* ADC reads */
	if (strcmp(action, "READ") == 0) {
		(void)adc_read_print(name);
		return;
	}

	/* Brightness */
	if (strcmp(name, "BRIGHT") == 0) {
		handle_bright_cmd(action);
		return;
	}

	/* Fallback: GPIO command */
	handle_gpio_cmd(name, action);
}
