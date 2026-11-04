/* SPDX-License-Identifier: Apache-2.0 */
#ifndef APP_GPIO_TABLE_H_
#define APP_GPIO_TABLE_H_

#include <stdint.h>
#include <stdbool.h>
#include <zephyr/drivers/gpio.h>
#include "proto.h"

struct gpio_entry {
	const char *name;
	struct gpio_dt_spec spec;
};

/* Initialize all output GPIOs as logically OFF (respects active-low). */
int gpio_table_init(void);

/* Look up by canonical name (e.g. "P0", "LED3G", "PWRLEDG"). NULL if absent. */
const struct gpio_entry *gpio_table_lookup(const char *name);

/* Drive logical on/off. Returns 0 on success. */
int gpio_table_set(const struct gpio_entry *e, bool on);
int gpio_table_toggle(const struct gpio_entry *e);

/* Indexed access to line LEDs (idx 0..7). */
const struct gpio_entry *gpio_table_led_green(uint8_t idx);
const struct gpio_entry *gpio_table_led_red(uint8_t idx);
const struct gpio_entry *gpio_table_pwr_green(void);
const struct gpio_entry *gpio_table_pwr_yellow(void);

#endif /* APP_GPIO_TABLE_H_ */
