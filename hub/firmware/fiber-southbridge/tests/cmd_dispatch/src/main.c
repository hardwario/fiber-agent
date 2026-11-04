/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for dispatch_line() in cmd_dispatch.c.
 *
 * Compiles the real cmd_dispatch.c against test doubles for its whole
 * link surface (uart_proto_write*, led_pwm_*, gpio_table_*,
 * adc_read_print). The doubles capture UART output and the last call
 * arguments so tests can assert both the reply text and the hardware
 * side effect of each command.
 */
#include <zephyr/ztest.h>
#include <string.h>

#include "proto.h"
#include "cmd_dispatch.h"
#include "uart_proto.h"
#include "gpio_table.h"
#include "led_pwm.h"
#include "adc_read.h"

/* ---- uart_proto double: capture all TX into tx_buf ------------------- */

static char tx_buf[1024];
static size_t tx_len;

void uart_proto_write_bytes(const char *buf, size_t len)
{
	size_t room = sizeof(tx_buf) - 1 - tx_len;

	if (len > room) {
		len = room;
	}
	memcpy(tx_buf + tx_len, buf, len);
	tx_len += len;
	tx_buf[tx_len] = '\0';
}

void uart_proto_write(const char *s)
{
	uart_proto_write_bytes(s, strlen(s));
}

/* ---- led_pwm double: record last call, canned getters ---------------- */

static struct {
	int set_led_calls;
	uint8_t led_idx, led_color, led_pattern;
	int set_pwr_calls;
	uint8_t pwr_color, pwr_pattern;
	int set_brightness_calls;
	uint8_t brightness;
	int sync_calls;
} led;

#define CANNED_BRIGHTNESS 42u

void led_pwm_set_led(uint8_t idx, uint8_t color, uint8_t pattern)
{
	led.set_led_calls++;
	led.led_idx = idx;
	led.led_color = color;
	led.led_pattern = pattern;
}

void led_pwm_set_pwr(uint8_t color, uint8_t pattern)
{
	led.set_pwr_calls++;
	led.pwr_color = color;
	led.pwr_pattern = pattern;
}

void led_pwm_set_brightness(uint8_t pct)
{
	led.set_brightness_calls++;
	led.brightness = pct;
}

void led_pwm_sync(void)
{
	led.sync_calls++;
}

void led_pwm_get_led(uint8_t idx, uint8_t *color, uint8_t *pattern)
{
	ARG_UNUSED(idx);
	*color = LED_COLOR_GREEN;
	*pattern = BLINK_STEADY;
}

void led_pwm_get_pwr(uint8_t *color, uint8_t *pattern)
{
	*color = PWR_COLOR_YELLOW;
	*pattern = BLINK_FAST;
}

uint8_t led_pwm_get_brightness(void)
{
	return CANNED_BRIGHTNESS;
}

/* ---- gpio_table double: fixed fake table, record last action --------- */

static const struct gpio_entry fake_entries[] = {
	{ .name = "P0" },
	{ .name = "LED3G" },
	{ .name = "PWRLEDG" },
};

enum gpio_action { GPIO_NONE, GPIO_SET_ON, GPIO_SET_OFF, GPIO_TOGGLED };

static struct {
	const struct gpio_entry *last_entry;
	enum gpio_action last_action;
} gpio;

const struct gpio_entry *gpio_table_lookup(const char *name)
{
	for (size_t i = 0; i < ARRAY_SIZE(fake_entries); i++) {
		if (strcmp(fake_entries[i].name, name) == 0) {
			return &fake_entries[i];
		}
	}
	return NULL;
}

int gpio_table_set(const struct gpio_entry *e, bool on)
{
	gpio.last_entry = e;
	gpio.last_action = on ? GPIO_SET_ON : GPIO_SET_OFF;
	return 0;
}

int gpio_table_toggle(const struct gpio_entry *e)
{
	gpio.last_entry = e;
	gpio.last_action = GPIO_TOGGLED;
	return 0;
}

/* ---- adc_read double -------------------------------------------------- */

static char adc_last_channel[16];
static int adc_calls;

int adc_read_print(const char *channel_name)
{
	adc_calls++;
	strncpy(adc_last_channel, channel_name, sizeof(adc_last_channel) - 1);
	adc_last_channel[sizeof(adc_last_channel) - 1] = '\0';
	return 0;
}

/* ---- helpers ----------------------------------------------------------- */

/* dispatch_line() mutates its argument, so run each command on a copy. */
static void run(const char *cmd)
{
	char buf[64];

	strncpy(buf, cmd, sizeof(buf) - 1);
	buf[sizeof(buf) - 1] = '\0';
	dispatch_line(buf);
}

static void reset_state(void *fixture)
{
	ARG_UNUSED(fixture);
	tx_len = 0;
	tx_buf[0] = '\0';
	memset(&led, 0, sizeof(led));
	memset(&gpio, 0, sizeof(gpio));
	adc_calls = 0;
	adc_last_channel[0] = '\0';
}

ZTEST_SUITE(cmd_dispatch, NULL, NULL, reset_state, NULL, NULL);

/* ---- single-token commands -------------------------------------------- */

ZTEST(cmd_dispatch, test_sync)
{
	run("SYNC");
	zassert_equal(led.sync_calls, 1);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_version)
{
	run("VERSION");
	zassert_str_equal(tx_buf, "VERSION: " FIRMWARE_VERSION "\r\n");
}

ZTEST(cmd_dispatch, test_version_format)
{
	/* The Hub's update check parses "south-zephyr <x.y.z> g<sha8>". */
	run("VERSION");
	zassert_str_equal(tx_buf, "VERSION: south-zephyr 0.0.0 g00000000\r\n");
}

ZTEST(cmd_dispatch, test_help)
{
	run("HELP");
	zassert_true(strncmp(tx_buf, "HELP:", 5) == 0,
		     "HELP reply must start with \"HELP:\" (got \"%.16s\")", tx_buf);
}

ZTEST(cmd_dispatch, test_uptime)
{
	run("UPTIME");
	zassert_true(strncmp(tx_buf, "UPTIME: ", 8) == 0,
		     "got \"%.16s\"", tx_buf);
	zassert_true(tx_len >= 12 && strcmp(tx_buf + tx_len - 4, "ms\r\n") == 0,
		     "UPTIME reply must end with \"ms\\r\\n\" (got \"%s\")", tx_buf);
}

ZTEST(cmd_dispatch, test_status)
{
	run("STATUS");
	zassert_not_null(strstr(tx_buf, "STATUS: brightness=42"));
	/* canned getters: every line LED reads back green/steady */
	zassert_not_null(strstr(tx_buf, "STATUS: led0=G,S"));
	zassert_not_null(strstr(tx_buf, "STATUS: led7=G,S"));
	/* canned power LED: yellow/fast */
	zassert_not_null(strstr(tx_buf, "STATUS: pwr=Y,F"));
}

/* ---- GPIO commands ------------------------------------------------------ */

ZTEST(cmd_dispatch, test_gpio_on)
{
	run("P0 ON");
	zassert_equal(gpio.last_entry, &fake_entries[0]);
	zassert_equal(gpio.last_action, GPIO_SET_ON);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_gpio_off)
{
	run("LED3G OFF");
	zassert_equal(gpio.last_entry, &fake_entries[1]);
	zassert_equal(gpio.last_action, GPIO_SET_OFF);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_gpio_toggle)
{
	run("PWRLEDG TOGGLE");
	zassert_equal(gpio.last_entry, &fake_entries[2]);
	zassert_equal(gpio.last_action, GPIO_TOGGLED);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_gpio_leading_whitespace_tolerated)
{
	run("  P0 \t ON");
	zassert_equal(gpio.last_entry, &fake_entries[0]);
	zassert_equal(gpio.last_action, GPIO_SET_ON);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_gpio_unknown_name)
{
	run("FOO ON");
	zassert_equal(gpio.last_action, GPIO_NONE);
	zassert_str_equal(tx_buf, "ERR: unknown GPIO name\r\n");
}

ZTEST(cmd_dispatch, test_gpio_unknown_action)
{
	run("P0 BLINK");
	zassert_equal(gpio.last_action, GPIO_NONE);
	zassert_str_equal(tx_buf, "ERR: unknown action (use ON/OFF/TOGGLE)\r\n");
}

/* ---- ADC reads ---------------------------------------------------------- */

ZTEST(cmd_dispatch, test_adc_vin_read)
{
	run("VIN READ");
	zassert_equal(adc_calls, 1);
	zassert_str_equal(adc_last_channel, "VIN");
	/* the reply line is emitted by adc_read_print itself */
	zassert_equal(tx_len, 0, "dispatcher must not write for READ");
}

ZTEST(cmd_dispatch, test_adc_vbat_read)
{
	run("VBAT READ");
	zassert_equal(adc_calls, 1);
	zassert_str_equal(adc_last_channel, "VBAT");
}

/* ---- composed LED command ----------------------------------------------- */

ZTEST(cmd_dispatch, test_led_full_form)
{
	run("LED 3 G F");
	zassert_equal(led.set_led_calls, 1);
	zassert_equal(led.led_idx, 3);
	zassert_equal(led.led_color, LED_COLOR_GREEN);
	zassert_equal(led.led_pattern, BLINK_FAST);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_led_index_out_of_range)
{
	run("LED 9 G S");
	zassert_equal(led.set_led_calls, 0);
	zassert_str_equal(tx_buf, "ERR: LED index must be 0-7\r\n");
}

ZTEST(cmd_dispatch, test_led_non_numeric_index)
{
	run("LED x");
	zassert_equal(led.set_led_calls, 0);
	zassert_str_equal(tx_buf, "ERR: use LED <0-7> <O|G|R|Y> <S|L|F>\r\n");
}

ZTEST(cmd_dispatch, test_led_defaults_to_off_steady)
{
	run("LED 3");
	zassert_equal(led.set_led_calls, 1);
	zassert_equal(led.led_idx, 3);
	zassert_equal(led.led_color, LED_COLOR_OFF);
	zassert_equal(led.led_pattern, BLINK_STEADY);
	zassert_str_equal(tx_buf, "OK\r\n");
}

/* ---- composed PWR command ----------------------------------------------- */

ZTEST(cmd_dispatch, test_pwr_full_form)
{
	run("PWR Y L");
	zassert_equal(led.set_pwr_calls, 1);
	zassert_equal(led.pwr_color, PWR_COLOR_YELLOW);
	zassert_equal(led.pwr_pattern, BLINK_SLOW);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_pwr_bare_is_usage_error)
{
	run("PWR");
	zassert_equal(led.set_pwr_calls, 0);
	zassert_str_equal(tx_buf, "ERR: use PWR <O|G|Y|L> <S|L|F>\r\n");
}

/* ---- BRIGHT ------------------------------------------------------------- */

ZTEST(cmd_dispatch, test_bright)
{
	run("BRIGHT 50");
	zassert_equal(led.set_brightness_calls, 1);
	zassert_equal(led.brightness, 50);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_bright_clamped_to_100)
{
	run("BRIGHT 150");
	zassert_equal(led.brightness, 100);
	zassert_str_equal(tx_buf, "OK\r\n");
}

ZTEST(cmd_dispatch, test_bright_non_numeric)
{
	run("BRIGHT abc");
	zassert_equal(led.set_brightness_calls, 0);
	zassert_str_equal(tx_buf, "ERR: invalid brightness value\r\n");
}

/* ---- syntax edges -------------------------------------------------------- */

ZTEST(cmd_dispatch, test_empty_line_is_syntax_error)
{
	run("");
	zassert_str_equal(tx_buf, "ERR: syntax. Use <NAME> <ON|OFF|TOGGLE|READ>\r\n");
}

ZTEST(cmd_dispatch, test_commands_are_case_sensitive)
{
	/* lowercase falls through the keyword checks into the GPIO path */
	run("led 3 g f");
	zassert_equal(led.set_led_calls, 0);
	zassert_str_equal(tx_buf, "ERR: unknown GPIO name\r\n");
}
