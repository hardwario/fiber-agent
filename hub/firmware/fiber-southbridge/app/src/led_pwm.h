/* SPDX-License-Identifier: Apache-2.0 */
#ifndef APP_LED_PWM_H_
#define APP_LED_PWM_H_

#include <stdint.h>

int led_pwm_init(void);
void led_pwm_start(void);

void led_pwm_set_led(uint8_t idx, uint8_t color, uint8_t pattern);
void led_pwm_set_pwr(uint8_t color, uint8_t pattern);
void led_pwm_set_brightness(uint8_t pct);
void led_pwm_sync(void);

void led_pwm_get_led(uint8_t idx, uint8_t *color, uint8_t *pattern);
void led_pwm_get_pwr(uint8_t *color, uint8_t *pattern);
uint8_t led_pwm_get_brightness(void);


#endif /* APP_LED_PWM_H_ */
