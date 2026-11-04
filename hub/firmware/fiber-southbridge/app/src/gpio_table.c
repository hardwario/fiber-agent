/* SPDX-License-Identifier: Apache-2.0 */
#include "gpio_table.h"

#include <string.h>
#include <zephyr/kernel.h>
#include <zephyr/devicetree.h>

#define GET(node) GPIO_DT_SPEC_GET(DT_NODELABEL(node), gpios)

static const struct gpio_entry entries[] = {
	{ "P0", GET(p0) },
	{ "P1", GET(p1) },
	{ "P2", GET(p2) },
	{ "P3", GET(p3) },
	{ "P4", GET(p4) },
	{ "P5", GET(p5) },
	{ "P6", GET(p6) },
	{ "P7", GET(p7) },

	{ "LED0G", GET(led0g) },
	{ "LED1G", GET(led1g) },
	{ "LED2G", GET(led2g) },
	{ "LED3G", GET(led3g) },
	{ "LED4G", GET(led4g) },
	{ "LED5G", GET(led5g) },
	{ "LED6G", GET(led6g) },
	{ "LED7G", GET(led7g) },

	{ "LED0R", GET(led0r) },
	{ "LED1R", GET(led1r) },
	{ "LED2R", GET(led2r) },
	{ "LED3R", GET(led3r) },
	{ "LED4R", GET(led4r) },
	{ "LED5R", GET(led5r) },
	{ "LED6R", GET(led6r) },
	{ "LED7R", GET(led7r) },

	{ "PWRLEDG", GET(pwrledg) },
	{ "PWRLEDY", GET(pwrledy) },
};

#define ENTRY_COUNT (sizeof(entries) / sizeof(entries[0]))

/* Indices into entries[] (kept stable so other modules can fetch by slot). */
#define IDX_LED_G_BASE  8
#define IDX_LED_R_BASE  16
#define IDX_PWR_G       24
#define IDX_PWR_Y       25

int gpio_table_init(void)
{
	for (size_t i = 0; i < ENTRY_COUNT; i++) {
		const struct gpio_dt_spec *s = &entries[i].spec;
		if (!gpio_is_ready_dt(s)) {
			continue;   /* skip missing ports; don't kill main */
		}
		/* P0..P7 (indices 0..7) boot HIGH per spec. LEDs and PWR
		 * LEDs boot OFF (logically inactive, respects active-low). */
		gpio_flags_t init_flag = (i < 8) ? GPIO_OUTPUT_HIGH
						 : GPIO_OUTPUT_INACTIVE;
		(void)gpio_pin_configure_dt(s, init_flag);
	}
	return 0;
}

const struct gpio_entry *gpio_table_lookup(const char *name)
{
	for (size_t i = 0; i < ENTRY_COUNT; i++) {
		if (strcmp(name, entries[i].name) == 0) {
			return &entries[i];
		}
	}
	return NULL;
}

int gpio_table_set(const struct gpio_entry *e, bool on)
{
	return gpio_pin_set_dt(&e->spec, on ? 1 : 0);
}

int gpio_table_toggle(const struct gpio_entry *e)
{
	return gpio_pin_toggle_dt(&e->spec);
}

const struct gpio_entry *gpio_table_led_green(uint8_t idx)
{
	if (idx >= LED_COUNT) {
		return NULL;
	}
	return &entries[IDX_LED_G_BASE + idx];
}

const struct gpio_entry *gpio_table_led_red(uint8_t idx)
{
	if (idx >= LED_COUNT) {
		return NULL;
	}
	return &entries[IDX_LED_R_BASE + idx];
}

const struct gpio_entry *gpio_table_pwr_green(void) { return &entries[IDX_PWR_G]; }
const struct gpio_entry *gpio_table_pwr_yellow(void) { return &entries[IDX_PWR_Y]; }
