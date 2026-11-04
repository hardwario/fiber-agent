/*
 * Copyright (c) 2026
 * SPDX-License-Identifier: Apache-2.0
 *
 * RF switch and radio tuning for STM32WL5MOC module.
 *
 * RF switch truth table, from ST's HAL for the STM32WL5Mxx module
 * (stm32wlxx_hal.c, HAL_RADIO_SetSwitchConfig):
 *   CTRL1=PC4, CTRL2=PC5, CTRL3=PC3
 *
 *   OFF:   PC3=0, PC4=0, PC5=0
 *   RX:    PC3=1, PC4=1, PC5=1  (all HIGH)
 *   TX LP: PC3=1, PC4=1, PC5=0  (CTRL1 + CTRL3)
 *   TX HP: PC3=1, PC4=0, PC5=1  (CTRL2 + CTRL3; unused, the DT selects RFO_LP)
 *
 * The TX LP row used to be PC5 only, a state ST's table does not have: the
 * switch left the PA unconnected and only leakage reached the antenna. On the
 * P2P bench of 2026-09-26 the Northbridge's ACKs arrived ~26 dB below the
 * reciprocal uplink (-91 vs -65 dBm) and the RAK5146 next to it heard them at
 * -76 dBm; the pins were read live over SWD (GPIOC ODR 0x20 during TX).
 */

#include <zephyr/device.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/init.h>
#include <zephyr/logging/log.h>

LOG_MODULE_REGISTER(board, LOG_LEVEL_INF);

static const struct device *gpioc_dev;

#define PC3_PIN 3  /* CTRL3 */
#define PC4_PIN 4  /* CTRL1 */
#define PC5_PIN 5  /* CTRL2 */

void board_rf_switch_tx(void)
{
	/* TX LP: CTRL1 (PC4) + CTRL3 (PC3) HIGH, CTRL2 (PC5) LOW */
	gpio_pin_set(gpioc_dev, PC5_PIN, 0);
	gpio_pin_set(gpioc_dev, PC3_PIN, 1);
	gpio_pin_set(gpioc_dev, PC4_PIN, 1);
}

void board_rf_switch_rx(void)
{
	/* RX: all HIGH */
	gpio_pin_set(gpioc_dev, PC3_PIN, 1);
	gpio_pin_set(gpioc_dev, PC4_PIN, 1);
	gpio_pin_set(gpioc_dev, PC5_PIN, 1);
}

void board_rf_switch_off(void)
{
	/* OFF: all LOW */
	gpio_pin_set(gpioc_dev, PC3_PIN, 0);
	gpio_pin_set(gpioc_dev, PC4_PIN, 0);
	gpio_pin_set(gpioc_dev, PC5_PIN, 0);
}

static int stm32wl5moc_board_init(void)
{
	gpioc_dev = DEVICE_DT_GET(DT_NODELABEL(gpioc));

	if (!device_is_ready(gpioc_dev)) {
		LOG_ERR("GPIOC not ready");
		return -ENODEV;
	}

	gpio_pin_configure(gpioc_dev, PC3_PIN, GPIO_OUTPUT_LOW);
	gpio_pin_configure(gpioc_dev, PC4_PIN, GPIO_OUTPUT_LOW);
	gpio_pin_configure(gpioc_dev, PC5_PIN, GPIO_OUTPUT_LOW);

	LOG_INF("RF switch GPIOs initialized");
	return 0;
}

SYS_INIT(stm32wl5moc_board_init, POST_KERNEL, 40);
