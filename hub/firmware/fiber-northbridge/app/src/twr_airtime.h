/* SPDX-License-Identifier: Apache-2.0
 *
 * LoRa time-on-air (Semtech AN1200.13 / SX1261 datasheet §6.1.4) and a sliding-hour airtime
 * ledger for the TOWER P0 bench gateway. Plain C, host-testable (tests/tower_codec).
 */
#ifndef APP_TWR_AIRTIME_H_
#define APP_TWR_AIRTIME_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

struct twr_phy {
	uint8_t sf;        /* 7..12 */
	uint32_t bw_hz;    /* 125000, 250000, 500000 */
	uint8_t cr;        /* coding rate 4/(4+cr): 1 = 4/5 .. 4 = 4/8 */
	uint16_t preamble; /* programmed preamble symbols */
	bool explicit_hdr;
	bool crc;
};

/* Symbol time in microseconds (rounded down; exact for BW 125/250/500 kHz). */
uint32_t twr_lora_tsym_us(const struct twr_phy *phy);

/* Time on air of a `pl`-byte packet, microseconds. Low-data-rate optimisation is assumed on
 * whenever Tsym >= 16.384 ms, which is what the loramac SX126x driver programs. */
uint32_t twr_lora_toa_us(const struct twr_phy *phy, size_t pl);

/* One bucket per minute; 61 of them so "the last hour" always includes the current,
 * partial minute (conservative: up to one extra minute is counted). */
#define TWR_DUTY_BUCKETS 61u
/* The duty limit per hour, in microseconds: 1 % in the g1 / g3 sub-bands, 10 % in h1.6
 * (869.400-869.650 MHz, ETSI EN 300 220 / ERC 70-03). */
#define TWR_DUTY_BUDGET_1PCT_US 36000000ull
#define TWR_DUTY_BUDGET_10PCT_US 360000000ull
#define TWR_DUTY_H16_MIN_HZ 869400000u
#define TWR_DUTY_H16_MAX_HZ 869650000u

struct twr_duty_bucket {
	uint32_t minute; /* uptime minute + 1; 0 = unused */
	uint32_t n_tx;
	uint64_t formula_us;
	uint64_t measured_us;
};

struct twr_duty {
	struct twr_duty_bucket b[TWR_DUTY_BUCKETS];
};

struct twr_duty_sum {
	uint32_t n_tx;
	uint64_t formula_us;
	uint64_t measured_us;
};

void twr_duty_init(struct twr_duty *d);

/* Record one transmission at uptime `now_ms`: the formula ToA and the measured TX start ->
 * TX done time (0 when not measured). */
void twr_duty_add(struct twr_duty *d, int64_t now_ms, uint32_t formula_us,
		  uint32_t measured_us);

/* Sum over the sliding hour ending at `now_ms`. */
void twr_duty_sum(const struct twr_duty *d, int64_t now_ms, struct twr_duty_sum *s);

/* The hourly budget of the sub-band `freq_hz` is in (see TWR_DUTY_BUDGET_*), microseconds. */
uint64_t twr_duty_budget_us(uint32_t freq_hz);

/* Share of `budget_us` used, in hundredths of a percent (10000 = the whole budget). */
uint32_t twr_duty_budget_bp(uint64_t airtime_us, uint64_t budget_us);

#endif /* APP_TWR_AIRTIME_H_ */
