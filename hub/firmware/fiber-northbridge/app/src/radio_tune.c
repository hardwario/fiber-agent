/* SPDX-License-Identifier: Apache-2.0
 *
 * Northbridge radio tuning table and the operations of each driver hook - see radio_tune.h.
 *
 * What the Zephyr driver (loramac-node, used for st,stm32wl-subghz-radio) already does, so
 * nothing here sends it twice:
 *   - Regulator: RadioInit() sends SetRegulatorMode(USE_DCDC = 0x01) once
 *     (modules/lib/loramac-node/src/radio/sx126x/radio.c:526); this only sends LDO when
 *     the item is off, and re-sends the setting on an explicit re-apply.
 *   - Image calibration: SX126xSetRfFrequency() runs CalibrateImage(0xD7, 0xDB) for
 *     850-900 MHz on the first SetChannel (sx126x.c:409-440, :501-505), kept across the
 *     driver's warm-start sleep. Not an item: the readback shows what was sent.
 *   - Retention: RadioInit() adds 0x08AC and 0x0889 to the retention list at 0x029F
 *     (radio.c:533-534). It does not help: SX126xSetRx() writes 0x08AC = 0x94 before every
 *     SET_RX (sx126x.c:308), which is where rx_boost substitutes its value.
 *   - TX: SX126xSetRfTxPower() -> sx126x_set_tx_params(power, RADIO_RAMP_40_US), whose
 *     SetPaConfig row follows the devicetree's rfo-lp-max-power (15: 0x07/0x00/0x01/0x01,
 *     power byte 14 - (15 - p)), zephyr/drivers/lora/loramac-node/sx126x_stm32wl.c:57-78.
 */
#include "radio_tune.h"

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <zephyr/init.h>
#include <zephyr/sys/atomic.h>
#include <zephyr/sys/util.h>

#if defined(CONFIG_NB_RADIO_TUNE_SMPS_RX_MA)
#define SMPS_RX_MA_DEFAULT CONFIG_NB_RADIO_TUNE_SMPS_RX_MA
#else
#define SMPS_RX_MA_DEFAULT 40
#endif
#if defined(CONFIG_NB_RADIO_TUNE_SMPS_TX_MA)
#define SMPS_TX_MA_DEFAULT CONFIG_NB_RADIO_TUNE_SMPS_TX_MA
#else
#define SMPS_TX_MA_DEFAULT 60
#endif
#if defined(CONFIG_NB_RADIO_TUNE_RX_BOOST_VALUE)
#define RX_BOOST_VALUE_DEFAULT CONFIG_NB_RADIO_TUNE_RX_BOOST_VALUE
#else
#define RX_BOOST_VALUE_DEFAULT RT_RX_GAIN_BOOSTED
#endif

#define SMPS_MA_VALID(ma) ((ma) == 20 || (ma) == 40 || (ma) == 60 || (ma) == 100)
BUILD_ASSERT(SMPS_MA_VALID(SMPS_RX_MA_DEFAULT), "NB_RADIO_TUNE_SMPS_RX_MA: 20/40/60/100");
BUILD_ASSERT(SMPS_MA_VALID(SMPS_TX_MA_DEFAULT), "NB_RADIO_TUNE_SMPS_TX_MA: 20/40/60/100");
BUILD_ASSERT(RX_BOOST_VALUE_DEFAULT == RT_RX_GAIN_BOOSTED ||
	     RX_BOOST_VALUE_DEFAULT == RT_RX_GAIN_MAX, "NB_RADIO_TUNE_RX_BOOST_VALUE: 0x96/0x97");

static const struct radio_tune_item_info items[RADIO_TUNE_ITEM_COUNT] = {
	[RADIO_TUNE_RX_BOOST] = { "rx_boost", false, true,
				  "0x08AC boosted LNA gain instead of 0x94 before SET_RX" },
	[RADIO_TUNE_SMPS_RX] = { "smps_rx", false, true,
				 "SMPS drive level 0x0923 before SET_RX" },
	[RADIO_TUNE_REGULATOR] = { "regulator", true, false,
				   "SetRegulatorMode DC-DC+LDO (off: LDO only)" },
	[RADIO_TUNE_SMPS_TX] = { "smps_tx", true, true,
				 "SMPS drive level 0x0923 before SET_TX" },
	[RADIO_TUNE_PA_TABLE] = { "pa_table", true, false,
				  "SetPaConfig row by requested power (RFO-LP 15/14/10 dBm)" },
	[RADIO_TUNE_RAMP] = { "ramp", true, false, "PA ramp 200 us instead of 40 us" },
};

static atomic_t enabled;
static volatile uint8_t rx_boost_value;
static volatile uint16_t smps_rx_ma;
static volatile uint16_t smps_tx_ma;
/* SMPSC2R as read at boot, and whether a drive level was ever written since. */
static volatile int16_t smps_reset_value = -1;
static atomic_t smps_written;

static uint32_t kconfig_defaults(void)
{
	uint32_t mask = 0;

	mask |= IS_ENABLED(CONFIG_NB_RADIO_TUNE_RX_BOOST) ? BIT(RADIO_TUNE_RX_BOOST) : 0;
	mask |= IS_ENABLED(CONFIG_NB_RADIO_TUNE_SMPS_RX) ? BIT(RADIO_TUNE_SMPS_RX) : 0;
	mask |= IS_ENABLED(CONFIG_NB_RADIO_TUNE_DCDC) ? BIT(RADIO_TUNE_REGULATOR) : 0;
	mask |= IS_ENABLED(CONFIG_NB_RADIO_TUNE_SMPS_TX) ? BIT(RADIO_TUNE_SMPS_TX) : 0;
	mask |= IS_ENABLED(CONFIG_NB_RADIO_TUNE_PA_TABLE) ? BIT(RADIO_TUNE_PA_TABLE) : 0;
	mask |= IS_ENABLED(CONFIG_NB_RADIO_TUNE_RAMP_200US) ? BIT(RADIO_TUNE_RAMP) : 0;
	return mask;
}

void radio_tune_reset(void)
{
	atomic_set(&enabled, (atomic_val_t)kconfig_defaults());
	rx_boost_value = RX_BOOST_VALUE_DEFAULT;
	smps_rx_ma = SMPS_RX_MA_DEFAULT;
	smps_tx_ma = SMPS_TX_MA_DEFAULT;
	smps_reset_value = -1;
	atomic_clear(&smps_written);
}

static int init_once(void)
{
	radio_tune_reset();
	return 0;
}

/* Before the LoRa device (POST_KERNEL, CONFIG_LORA_INIT_PRIORITY) and so before any hook. */
SYS_INIT(init_once, PRE_KERNEL_1, 0);

bool radio_tune_get(enum radio_tune_item item)
{
	return item < RADIO_TUNE_ITEM_COUNT && atomic_test_bit(&enabled, item);
}

void radio_tune_set(enum radio_tune_item item, bool on)
{
	if (item >= RADIO_TUNE_ITEM_COUNT) {
		return;
	}
	if (on) {
		atomic_set_bit(&enabled, item);
	} else {
		atomic_clear_bit(&enabled, item);
	}
}

int radio_tune_set_param(enum radio_tune_item item, uint32_t value)
{
	switch (item) {
	case RADIO_TUNE_RX_BOOST:
		if (value != RT_RX_GAIN_BOOSTED && value != RT_RX_GAIN_MAX) {
			return -EINVAL;
		}
		rx_boost_value = (uint8_t)value;
		return 0;
	case RADIO_TUNE_SMPS_RX:
	case RADIO_TUNE_SMPS_TX:
		if (!SMPS_MA_VALID(value)) {
			return -EINVAL;
		}
		if (item == RADIO_TUNE_SMPS_RX) {
			smps_rx_ma = (uint16_t)value;
		} else {
			smps_tx_ma = (uint16_t)value;
		}
		return 0;
	default:
		return -EINVAL;
	}
}

uint32_t radio_tune_param(enum radio_tune_item item)
{
	switch (item) {
	case RADIO_TUNE_RX_BOOST:
		return rx_boost_value;
	case RADIO_TUNE_SMPS_RX:
		return smps_rx_ma;
	case RADIO_TUNE_SMPS_TX:
		return smps_tx_ma;
	default:
		return 0;
	}
}

int radio_tune_find(const char *name)
{
	for (int i = 0; i < RADIO_TUNE_ITEM_COUNT; i++) {
		if (strcmp(name, items[i].name) == 0) {
			return i;
		}
	}
	return -ENOENT;
}

const struct radio_tune_item_info *radio_tune_info(enum radio_tune_item item)
{
	return item < RADIO_TUNE_ITEM_COUNT ? &items[item] : NULL;
}

void radio_tune_set_smps_reset_value(uint8_t reg_value)
{
	smps_reset_value = reg_value;
}

/* RM0453 SUBGHZ_SMPSC2R.SMPS_DRV[2:1]: 00 20 mA, 01 40 mA, 10 60 mA, 11 100 mA. */
int radio_tune_smps_bits(uint32_t ma)
{
	switch (ma) {
	case 20:
		return 0x0 << 1;
	case 40:
		return 0x1 << 1;
	case 60:
		return 0x2 << 1;
	case 100:
		return 0x3 << 1;
	default:
		return -EINVAL;
	}
}

uint32_t radio_tune_smps_ma(uint8_t reg_value)
{
	static const uint16_t ma[] = { 20, 40, 60, 100 };

	return ma[(reg_value & RT_SMPS_DRV_MASK) >> 1];
}

uint8_t radio_tune_filter_reg(uint16_t address, uint8_t value)
{
	/* Only SX126xSetRx()'s power-saving write: SetRxBoosted() (0x96) and everything the
	 * application writes itself pass through. */
	if (address == RT_REG_RX_GAIN && value == RT_RX_GAIN_POWER_SAVING &&
	    radio_tune_get(RADIO_TUNE_RX_BOOST)) {
		return rx_boost_value;
	}
	return value;
}

static void add_op(struct radio_tune_op *ops, size_t cap, size_t *n, const struct radio_tune_op *op)
{
	if (*n < cap) {
		ops[*n] = *op;
	}
	(*n)++;
}

static void add_smps(struct radio_tune_op *ops, size_t cap, size_t *n, enum radio_tune_item item)
{
	int bits;

	if (radio_tune_get(item)) {
		bits = radio_tune_smps_bits(radio_tune_param(item));
		atomic_set(&smps_written, 1);
	} else if (atomic_get(&smps_written) && smps_reset_value >= 0) {
		/* Turned off at run time: put the chip's own drive level back. */
		bits = smps_reset_value & RT_SMPS_DRV_MASK;
	} else {
		/* Never written: leave the chip alone, as the stock driver does. */
		return;
	}
	struct radio_tune_op op = {
		.kind = RT_OP_REG_BITS,
		.address = RT_REG_SMPS_C2,
		.mask = RT_SMPS_DRV_MASK,
		.len = 1,
		.data = { (uint8_t)bits },
	};

	add_op(ops, cap, n, &op);
}

static void add_regulator(struct radio_tune_op *ops, size_t cap, size_t *n)
{
	struct radio_tune_op op = {
		.kind = RT_OP_CMD,
		.opcode = RT_CMD_SET_REGULATOR,
		.len = 1,
		.data = { radio_tune_get(RADIO_TUNE_REGULATOR) ? RT_REGULATOR_DCDC
							     : RT_REGULATOR_LDO },
	};

	add_op(ops, cap, n, &op);
}

size_t radio_tune_ops(enum radio_tune_phase phase, struct radio_tune_op *ops, size_t cap)
{
	size_t n = 0;

	switch (phase) {
	case RADIO_TUNE_PHASE_BOOT:
		/* The driver already sent DC-DC (radio.c:526): only LDO needs sending. */
		if (!radio_tune_get(RADIO_TUNE_REGULATOR)) {
			add_regulator(ops, cap, &n);
		}
		break;
	case RADIO_TUNE_PHASE_REAPPLY:
		add_regulator(ops, cap, &n);
		break;
	case RADIO_TUNE_PHASE_RX_ENTRY:
		add_smps(ops, cap, &n, RADIO_TUNE_SMPS_RX);
		break;
	case RADIO_TUNE_PHASE_TX_ENTRY:
		add_smps(ops, cap, &n, RADIO_TUNE_SMPS_TX);
		break;
	}
	return MIN(n, cap);
}

size_t radio_tune_tx_power_ops(int8_t power, bool rfo_lp, int8_t max_power, uint8_t *ramp,
			       struct radio_tune_op *ops, size_t cap)
{
	if (radio_tune_get(RADIO_TUNE_RAMP)) {
		*ramp = RT_RAMP_200_US;
	}
	if (!rfo_lp || !radio_tune_get(RADIO_TUNE_PA_TABLE)) {
		return 0;
	}

	/* The RFO-LP rows of ST's table (RM0453 "PA optimal setting", AN5457 §5.1.2), the
	 * same three the driver uses (sx126x_stm32wl.c:63-72), but picked by the requested
	 * power instead of the devicetree's rfo-lp-max-power: +15 dBm 0x07/0x0E, +14 dBm
	 * 0x04/0x0E, +10 dBm 0x01/0x0D, lower powers stepping the power byte down from the
	 * nearest row above. */
	int p = MIN(power, max_power);
	uint8_t duty;
	int reg;

	if (p >= 15) {
		duty = 0x07;
		reg = 0x0E;
	} else if (p > 10) {
		duty = 0x04;
		reg = 0x0E - (14 - p);
	} else {
		duty = 0x01;
		reg = 0x0D - (10 - p);
	}
	reg = MAX(reg, -17);

	struct radio_tune_op seq[] = {
		{ .kind = RT_OP_CMD, .opcode = RT_CMD_SET_PA_CONFIG, .len = 4,
		  .data = { duty, 0x00, 0x01, 0x01 } },
		{ .kind = RT_OP_REG_WRITE, .address = RT_REG_OCP, .len = 1,
		  .data = { RT_OCP_RFO_LP } },
		{ .kind = RT_OP_CMD, .opcode = RT_CMD_SET_TX_PARAMS, .len = 2,
		  .data = { (uint8_t)(int8_t)reg, *ramp } },
	};
	size_t n = 0;

	for (size_t i = 0; i < ARRAY_SIZE(seq); i++) {
		add_op(ops, cap, &n, &seq[i]);
	}
	return MIN(n, cap);
}

size_t radio_tune_summary(char *buf, size_t cap)
{
	int n = snprintf(buf, cap,
			 "rx_boost=%s(0x%02x) smps_rx=%s(%umA) regulator=%s smps_tx=%s(%umA) "
			 "pa_table=%s ramp=%s",
			 radio_tune_get(RADIO_TUNE_RX_BOOST) ? "on" : "off",
			 (unsigned int)rx_boost_value,
			 radio_tune_get(RADIO_TUNE_SMPS_RX) ? "on" : "off", (unsigned int)smps_rx_ma,
			 radio_tune_get(RADIO_TUNE_REGULATOR) ? "dcdc" : "ldo",
			 radio_tune_get(RADIO_TUNE_SMPS_TX) ? "on" : "off", (unsigned int)smps_tx_ma,
			 radio_tune_get(RADIO_TUNE_PA_TABLE) ? "on" : "off",
			 radio_tune_get(RADIO_TUNE_RAMP) ? "200us" : "40us");

	return n < 0 ? 0 : MIN((size_t)n, cap ? cap - 1 : 0);
}
