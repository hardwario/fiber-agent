/* SPDX-License-Identifier: Apache-2.0 */
#include "led_pwm.h"
#include "gpio_table.h"
#include "proto.h"

#include <zephyr/kernel.h>

/*
 * Soft PWM + blink composer.
 *
 * Cadence: 1 kHz (1 ms period). The original CubeIDE firmware drove
 * TIM14 at 10 kHz, but in Zephyr a 10 kHz SysTick rate produced enough
 * timeout-queue churn to starve the USART1 RX IRQ at 115200 baud and
 * lose per-command bytes. 1 kHz tick gives 10× more headroom for the
 * UART ISR and the resulting PWM (10 levels × 100 Hz) is still
 * flicker-free.
 * - Brightness: 10 levels (~10 % resolution).
 * - Blink patterns derive from blink_tick (free-running, ms units):
 *     SLOW: 500 ms on / 500 ms off
 *     FAST: 250 ms on / 250 ms off
 * - SYNC zeroes blink_tick so multiple boards can phase-align.
 *
 * Threading: the composer runs in a dedicated thread woken by k_timer.
 * The timer callback (SysTick ISR context) only k_sem_give's the
 * semaphore, keeping SysTick latency tiny.
 *
 * Bug fix vs legacy CubeIDE firmware: legacy declared blink_tick/blink_phase
 * but never advanced or consulted them, so patterns L/F were no-ops.
 * Patterns now blink end-to-end.
 */

/* PWM cadence: composer thread is woken every 1 ms (K_TICKS(1) at 1 kHz
 * tick rate). With 10 brightness steps we get 100 Hz PWM (flicker-free)
 * and 10 % brightness resolution — original firmware used 100 steps but
 * at the cost of starving UART RX on Zephyr, see prj.conf for rationale. */
#define PWM_PERIOD          10
#define BLINK_SLOW_PERIOD   1000     /* 1 s cycle, 500 ms on/off */
#define BLINK_SLOW_ON       500
#define BLINK_FAST_PERIOD   500      /* 500 ms cycle, 250 ms on/off */
#define BLINK_FAST_ON       250

#define COMPOSER_STACK_SIZE 512
#define COMPOSER_PRIO       2   /* preemptible, above default (-2..0 coop) */

struct led_state {
	uint8_t color;
	uint8_t pattern;
};

static struct led_state line_leds[LED_COUNT];
static struct led_state pwr_led;
static uint8_t brightness_pct = 100;

static struct k_spinlock state_lock;

/* These are mutated only in the composer thread. */
static uint32_t blink_tick;
static uint16_t pwm_counter;

static uint32_t last_bright_set_ms;

static uint8_t last_applied_line[LED_COUNT] = {
	0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF
};
static uint8_t last_applied_pwr = 0xFF;

static K_SEM_DEFINE(pwm_tick_sem, 0, 1);

static void pwm_timer_handler(struct k_timer *t)
{
	ARG_UNUSED(t);
	k_sem_give(&pwm_tick_sem);
}

static K_TIMER_DEFINE(pwm_timer, pwm_timer_handler, NULL);

static inline bool blink_on_now(uint8_t pattern, uint32_t tick)
{
	switch (pattern) {
	case BLINK_SLOW:
		return (tick % BLINK_SLOW_PERIOD) < BLINK_SLOW_ON;
	case BLINK_FAST:
		return (tick % BLINK_FAST_PERIOD) < BLINK_FAST_ON;
	case BLINK_STEADY:
	default:
		return true;
	}
}

static uint8_t compose_line(const struct led_state *s, uint32_t tick,
			    bool pwm_on)
{
	if (!pwm_on || !blink_on_now(s->pattern, tick)) {
		return 0;
	}
	uint8_t g_on = (s->color == LED_COLOR_GREEN ||
			s->color == LED_COLOR_YELLOW) ? 1 : 0;
	uint8_t r_on = (s->color == LED_COLOR_RED ||
			s->color == LED_COLOR_YELLOW) ? 1 : 0;
	return (uint8_t)((g_on << 1) | r_on);
}

static uint8_t compose_pwr(const struct led_state *s, uint32_t tick,
			   bool pwm_on)
{
	if (!pwm_on || !blink_on_now(s->pattern, tick)) {
		return 0;
	}
	uint8_t g_on = (s->color == PWR_COLOR_GREEN ||
			s->color == PWR_COLOR_LIME) ? 1 : 0;
	uint8_t y_on = (s->color == PWR_COLOR_YELLOW ||
			s->color == PWR_COLOR_LIME) ? 1 : 0;
	return (uint8_t)((g_on << 1) | y_on);
}

static void apply_line_led(uint8_t idx, uint8_t composed)
{
	gpio_table_set(gpio_table_led_green(idx), (composed >> 1) & 1);
	gpio_table_set(gpio_table_led_red(idx), composed & 1);
}

static void apply_pwr_led(uint8_t composed)
{
	gpio_table_set(gpio_table_pwr_green(), (composed >> 1) & 1);
	gpio_table_set(gpio_table_pwr_yellow(), composed & 1);
}

static void composer_thread(void *a, void *b, void *c)
{
	ARG_UNUSED(a); ARG_UNUSED(b); ARG_UNUSED(c);

	struct led_state local_line[LED_COUNT];
	struct led_state local_pwr;
	uint8_t br;

	while (true) {
		k_sem_take(&pwm_tick_sem, K_FOREVER);

		/* Brief snapshot under spinlock (IRQs disabled for ~3 µs). */
		k_spinlock_key_t key = k_spin_lock(&state_lock);
		for (uint8_t i = 0; i < LED_COUNT; i++) {
			local_line[i] = line_leds[i];
		}
		local_pwr = pwr_led;
		br = brightness_pct;
		k_spin_unlock(&state_lock, key);

		blink_tick++;

		bool pwm_on;
		if (br >= 100) {
			pwm_on = true;
		} else {
			pwm_counter++;
			if (pwm_counter >= PWM_PERIOD) {
				pwm_counter = 0;
			}
			/* Scale brightness 0..100 to 0..PWM_PERIOD threshold.
			 * Without this br is compared directly to pwm_counter
			 * (0..PWM_PERIOD-1) and any br >= PWM_PERIOD reads as
			 * always-on — so the entire 10..100 range was identical
			 * to full brightness. */
			uint8_t threshold = (uint8_t)(((uint16_t)br * PWM_PERIOD + 50) / 100);
			pwm_on = (pwm_counter < threshold);
		}

		for (uint8_t i = 0; i < LED_COUNT; i++) {
			uint8_t composed = compose_line(&local_line[i],
							 blink_tick, pwm_on);
			if (composed != last_applied_line[i]) {
				apply_line_led(i, composed);
				last_applied_line[i] = composed;
			}
		}

		uint8_t composed_pwr = compose_pwr(&local_pwr, blink_tick,
						     pwm_on);
		if (composed_pwr != last_applied_pwr) {
			apply_pwr_led(composed_pwr);
			last_applied_pwr = composed_pwr;
		}
	}
}

K_THREAD_DEFINE(pwm_thread_id, COMPOSER_STACK_SIZE,
		composer_thread, NULL, NULL, NULL,
		COMPOSER_PRIO, 0, 0);

int led_pwm_init(void)
{
	k_spinlock_key_t key = k_spin_lock(&state_lock);
	for (uint8_t i = 0; i < LED_COUNT; i++) {
		line_leds[i].color = LED_COLOR_OFF;
		line_leds[i].pattern = BLINK_STEADY;
	}
	pwr_led.color = PWR_COLOR_OFF;
	pwr_led.pattern = BLINK_STEADY;
	brightness_pct = 100;
	k_spin_unlock(&state_lock, key);
	return 0;
}

void led_pwm_start(void)
{
	/* 1 ms cadence (1 kHz SysTick). Keeps UART RX latency safe — the
	 * earlier 10 kHz timer in SysTick context was starving USART1 IRQ
	 * and causing per-command byte loss. */
	k_timer_start(&pwm_timer, K_TICKS(1), K_TICKS(1));
}

void led_pwm_set_led(uint8_t idx, uint8_t color, uint8_t pattern)
{
	if (idx >= LED_COUNT) {
		return;
	}
	k_spinlock_key_t key = k_spin_lock(&state_lock);
	line_leds[idx].color = color;
	line_leds[idx].pattern = pattern;
	k_spin_unlock(&state_lock, key);
}

void led_pwm_set_pwr(uint8_t color, uint8_t pattern)
{
	k_spinlock_key_t key = k_spin_lock(&state_lock);
	pwr_led.color = color;
	pwr_led.pattern = pattern;
	k_spin_unlock(&state_lock, key);
}

void led_pwm_set_brightness(uint8_t pct)
{
	if (pct > 100) {
		pct = 100;
	}
	uint32_t now = k_uptime_get_32();
	if ((now - last_bright_set_ms) < BRIGHT_DEBOUNCE_MS) {
		return;
	}
	last_bright_set_ms = now;

	k_spinlock_key_t key = k_spin_lock(&state_lock);
	brightness_pct = pct;
	k_spin_unlock(&state_lock, key);
}

void led_pwm_get_led(uint8_t idx, uint8_t *color, uint8_t *pattern)
{
	if (idx >= LED_COUNT) {
		if (color) *color = 0;
		if (pattern) *pattern = 0;
		return;
	}
	k_spinlock_key_t key = k_spin_lock(&state_lock);
	if (color) *color = line_leds[idx].color;
	if (pattern) *pattern = line_leds[idx].pattern;
	k_spin_unlock(&state_lock, key);
}

void led_pwm_get_pwr(uint8_t *color, uint8_t *pattern)
{
	k_spinlock_key_t key = k_spin_lock(&state_lock);
	if (color) *color = pwr_led.color;
	if (pattern) *pattern = pwr_led.pattern;
	k_spin_unlock(&state_lock, key);
}

uint8_t led_pwm_get_brightness(void)
{
	k_spinlock_key_t key = k_spin_lock(&state_lock);
	uint8_t v = brightness_pct;
	k_spin_unlock(&state_lock, key);
	return v;
}

void led_pwm_sync(void)
{
	unsigned int key = irq_lock();
	blink_tick = 0;
	pwm_counter = 0;
	irq_unlock(key);
}
