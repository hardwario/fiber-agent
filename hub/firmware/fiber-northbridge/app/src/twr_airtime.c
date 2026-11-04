/* SPDX-License-Identifier: Apache-2.0
 *
 * LoRa time-on-air and airtime ledger, see twr_airtime.h.
 */
#include "twr_airtime.h"

#include <string.h>

uint32_t twr_lora_tsym_us(const struct twr_phy *phy)
{
	return (uint32_t)(((uint64_t)1000000u << phy->sf) / phy->bw_hz);
}

uint32_t twr_lora_toa_us(const struct twr_phy *phy, size_t pl)
{
	uint64_t tsym_num = (uint64_t)1000000u << phy->sf; /* Tsym = tsym_num / bw_hz */
	bool de = (tsym_num / phy->bw_hz) >= 16384u;
	int32_t sf = phy->sf;
	int32_t num = 8 * (int32_t)pl - 4 * sf + 28 + (phy->crc ? 16 : 0) -
		      (phy->explicit_hdr ? 0 : 20);
	int32_t den = 4 * (sf - (de ? 2 : 0));
	int32_t n_payload = 8;
	uint64_t quarter_syms; /* symbols x 4, so the preamble's 4.25 stays integral */

	if (num > 0) {
		n_payload += ((num + den - 1) / den) * (phy->cr + 4);
	}
	quarter_syms = (uint64_t)phy->preamble * 4u + 17u + (uint64_t)n_payload * 4u;
	return (uint32_t)((quarter_syms * tsym_num) / (4u * (uint64_t)phy->bw_hz));
}

void twr_duty_init(struct twr_duty *d)
{
	memset(d, 0, sizeof(*d));
}

static uint32_t minute_tag(int64_t now_ms)
{
	return (uint32_t)(now_ms / 60000) + 1u;
}

void twr_duty_add(struct twr_duty *d, int64_t now_ms, uint32_t formula_us,
		  uint32_t measured_us)
{
	uint32_t m = minute_tag(now_ms);
	struct twr_duty_bucket *b = &d->b[m % TWR_DUTY_BUCKETS];

	if (b->minute != m) {
		memset(b, 0, sizeof(*b));
		b->minute = m;
	}
	b->n_tx++;
	b->formula_us += formula_us;
	b->measured_us += measured_us;
}

void twr_duty_sum(const struct twr_duty *d, int64_t now_ms, struct twr_duty_sum *s)
{
	uint32_t m = minute_tag(now_ms);

	memset(s, 0, sizeof(*s));
	for (size_t i = 0; i < TWR_DUTY_BUCKETS; i++) {
		const struct twr_duty_bucket *b = &d->b[i];

		if (b->minute == 0 || b->minute > m || m - b->minute >= TWR_DUTY_BUCKETS) {
			continue;
		}
		s->n_tx += b->n_tx;
		s->formula_us += b->formula_us;
		s->measured_us += b->measured_us;
	}
}

uint64_t twr_duty_budget_us(uint32_t freq_hz)
{
	if (freq_hz >= TWR_DUTY_H16_MIN_HZ && freq_hz <= TWR_DUTY_H16_MAX_HZ) {
		return TWR_DUTY_BUDGET_10PCT_US;
	}
	return TWR_DUTY_BUDGET_1PCT_US;
}

uint32_t twr_duty_budget_bp(uint64_t airtime_us, uint64_t budget_us)
{
	return (uint32_t)((airtime_us * 10000u) / budget_us);
}
