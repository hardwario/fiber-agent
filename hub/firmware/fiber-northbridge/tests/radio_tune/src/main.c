/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for radio_tune.c: the tuning table, its Kconfig defaults, the RX gain
 * substitution and the register/command operations each driver hook runs, in order.
 * radio_tune_hw.c only executes these lists on the chip; that half is HIL (`radio tune regs`).
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/util.h>
#include <errno.h>
#include <string.h>

#include "radio_tune.h"

#define RFO_LP_MAX 15 /* boards/st/stm32wl5moc/stm32wl5moc.dts */

static struct radio_tune_op ops[RADIO_TUNE_MAX_OPS];

static void reset_state(void *fixture)
{
	ARG_UNUSED(fixture);
	radio_tune_reset();
	memset(ops, 0, sizeof(ops));
}

static void assert_cmd(const struct radio_tune_op *op, uint8_t opcode, const uint8_t *data,
		       uint8_t len)
{
	zassert_equal(op->kind, RT_OP_CMD);
	zassert_equal(op->opcode, opcode, "opcode 0x%02x", op->opcode);
	zassert_equal(op->len, len);
	zassert_mem_equal(op->data, data, len);
}

/* The RX items and the stock regulator mode boot on; nothing that changes TX does. */
ZTEST(radio_tune, test_kconfig_defaults_rx_on_tx_off)
{
	zassert_true(radio_tune_get(RADIO_TUNE_RX_BOOST));
	zassert_true(radio_tune_get(RADIO_TUNE_SMPS_RX));
	zassert_true(radio_tune_get(RADIO_TUNE_REGULATOR), "DC-DC, as the driver itself");
	zassert_false(radio_tune_get(RADIO_TUNE_SMPS_TX));
	zassert_false(radio_tune_get(RADIO_TUNE_PA_TABLE));
	zassert_false(radio_tune_get(RADIO_TUNE_RAMP));
	zassert_equal(radio_tune_param(RADIO_TUNE_RX_BOOST), 0x96);
	zassert_equal(radio_tune_param(RADIO_TUNE_SMPS_RX), 40);
	zassert_equal(radio_tune_param(RADIO_TUNE_SMPS_TX), 60);
	zassert_equal(radio_tune_param(RADIO_TUNE_PA_TABLE), 0);
}

ZTEST(radio_tune, test_table_names_and_tx_flags)
{
	static const char *const names[] = { "rx_boost", "smps_rx", "regulator",
					     "smps_tx",  "pa_table", "ramp" };

	zassert_equal(ARRAY_SIZE(names), RADIO_TUNE_ITEM_COUNT);
	for (int i = 0; i < RADIO_TUNE_ITEM_COUNT; i++) {
		zassert_equal(radio_tune_find(names[i]), i, "%s", names[i]);
		zassert_str_equal(radio_tune_info(i)->name, names[i]);
	}
	zassert_equal(radio_tune_find("all"), -ENOENT);
	zassert_equal(radio_tune_find("rx"), -ENOENT);
	zassert_is_null(radio_tune_info(RADIO_TUNE_ITEM_COUNT));

	/* Only the two RX items leave TX alone. */
	zassert_false(radio_tune_info(RADIO_TUNE_RX_BOOST)->changes_tx);
	zassert_false(radio_tune_info(RADIO_TUNE_SMPS_RX)->changes_tx);
	zassert_true(radio_tune_info(RADIO_TUNE_REGULATOR)->changes_tx);
	zassert_true(radio_tune_info(RADIO_TUNE_SMPS_TX)->changes_tx);
	zassert_true(radio_tune_info(RADIO_TUNE_PA_TABLE)->changes_tx);
	zassert_true(radio_tune_info(RADIO_TUNE_RAMP)->changes_tx);
}

/* A modem that resets mid-test comes back with the stock TX path. */
ZTEST(radio_tune, test_boot_defaults_leave_tx_path_stock)
{
	uint8_t ramp = RT_RAMP_40_US;

	zassert_equal(radio_tune_tx_power_ops(14, true, RFO_LP_MAX, &ramp, ops, ARRAY_SIZE(ops)),
		      0, "driver's own SetPaConfig/SetTxParams");
	zassert_equal(ramp, RT_RAMP_40_US);

	/* Before any RX entry nothing touches the SMPS drive at TX. */
	radio_tune_set_smps_reset_value(0x06);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_TX_ENTRY, ops, ARRAY_SIZE(ops)), 0);

	/* After smps_rx set 40 mA, TX gets the chip's own level back. */
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_RX_ENTRY, ops, ARRAY_SIZE(ops)), 1);
	zassert_equal(ops[0].data[0], 0x02);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_TX_ENTRY, ops, ARRAY_SIZE(ops)), 1);
	zassert_equal(ops[0].address, RT_REG_SMPS_C2);
	zassert_equal(ops[0].data[0], 0x06, "reset value's drive bits");

	/* No regulator command at boot: the driver already sent DC-DC. */
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_BOOT, ops, ARRAY_SIZE(ops)), 0);
}

ZTEST(radio_tune, test_set_get_and_out_of_range_item)
{
	radio_tune_set(RADIO_TUNE_RAMP, true);
	zassert_true(radio_tune_get(RADIO_TUNE_RAMP));
	zassert_false(radio_tune_get(RADIO_TUNE_PA_TABLE));
	radio_tune_set(RADIO_TUNE_RAMP, false);
	zassert_false(radio_tune_get(RADIO_TUNE_RAMP));

	radio_tune_set(RADIO_TUNE_ITEM_COUNT, true);
	zassert_false(radio_tune_get(RADIO_TUNE_ITEM_COUNT));
}

ZTEST(radio_tune, test_rx_gain_substituted_only_for_setrx_write)
{
	/* SX126xSetRx() writes 0x94 before every SET_RX. */
	zassert_equal(radio_tune_filter_reg(RT_REG_RX_GAIN, 0x94), 0x96);
	/* SX126xSetRxBoosted() and anything else pass through. */
	zassert_equal(radio_tune_filter_reg(RT_REG_RX_GAIN, 0x96), 0x96);
	zassert_equal(radio_tune_filter_reg(RT_REG_SMPS_C2, 0x94), 0x94);
	zassert_equal(radio_tune_filter_reg(RT_REG_OCP, 0x18), 0x18);

	zassert_ok(radio_tune_set_param(RADIO_TUNE_RX_BOOST, 0x97));
	zassert_equal(radio_tune_filter_reg(RT_REG_RX_GAIN, 0x94), 0x97);

	radio_tune_set(RADIO_TUNE_RX_BOOST, false);
	zassert_equal(radio_tune_filter_reg(RT_REG_RX_GAIN, 0x94), 0x94);
}

ZTEST(radio_tune, test_param_validation)
{
	zassert_equal(radio_tune_set_param(RADIO_TUNE_RX_BOOST, 0x94), -EINVAL);
	zassert_equal(radio_tune_set_param(RADIO_TUNE_RX_BOOST, 0x98), -EINVAL);
	zassert_equal(radio_tune_param(RADIO_TUNE_RX_BOOST), 0x96, "unchanged on error");

	zassert_equal(radio_tune_set_param(RADIO_TUNE_SMPS_RX, 30), -EINVAL);
	zassert_ok(radio_tune_set_param(RADIO_TUNE_SMPS_RX, 100));
	zassert_equal(radio_tune_param(RADIO_TUNE_SMPS_RX), 100);
	zassert_ok(radio_tune_set_param(RADIO_TUNE_SMPS_TX, 20));
	zassert_equal(radio_tune_param(RADIO_TUNE_SMPS_TX), 20);

	zassert_equal(radio_tune_set_param(RADIO_TUNE_REGULATOR, 1), -EINVAL);
	zassert_equal(radio_tune_set_param(RADIO_TUNE_PA_TABLE, 1), -EINVAL);
	zassert_equal(radio_tune_set_param(RADIO_TUNE_RAMP, 4), -EINVAL);
}

ZTEST(radio_tune, test_smps_drive_encoding)
{
	zassert_equal(radio_tune_smps_bits(20), 0x00);
	zassert_equal(radio_tune_smps_bits(40), 0x02);
	zassert_equal(radio_tune_smps_bits(60), 0x04);
	zassert_equal(radio_tune_smps_bits(100), 0x06);
	zassert_equal(radio_tune_smps_bits(50), -EINVAL);

	zassert_equal(radio_tune_smps_ma(0x02), 40);
	zassert_equal(radio_tune_smps_ma(0xF9), 20, "only bits 2:1 count");
	zassert_equal(radio_tune_smps_ma(0x06), 100);
}

ZTEST(radio_tune, test_mode_entry_sets_smps_drive)
{
	radio_tune_set(RADIO_TUNE_SMPS_TX, true);

	size_t n = radio_tune_ops(RADIO_TUNE_PHASE_RX_ENTRY, ops, ARRAY_SIZE(ops));

	zassert_equal(n, 1);
	zassert_equal(ops[0].kind, RT_OP_REG_BITS);
	zassert_equal(ops[0].address, 0x0923);
	zassert_equal(ops[0].mask, 0x06);
	zassert_equal(ops[0].data[0], 0x02, "40 mA");

	n = radio_tune_ops(RADIO_TUNE_PHASE_TX_ENTRY, ops, ARRAY_SIZE(ops));
	zassert_equal(n, 1);
	zassert_equal(ops[0].address, 0x0923);
	zassert_equal(ops[0].data[0], 0x04, "60 mA");
}

ZTEST(radio_tune, test_smps_off_leaves_chip_alone_until_written)
{
	radio_tune_set(RADIO_TUNE_SMPS_RX, false);
	radio_tune_set_smps_reset_value(0x06);

	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_RX_ENTRY, ops, ARRAY_SIZE(ops)), 0);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_TX_ENTRY, ops, ARRAY_SIZE(ops)), 0);

	/* Switched on and off again at run time: the chip's own level goes back. */
	radio_tune_set(RADIO_TUNE_SMPS_RX, true);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_RX_ENTRY, ops, ARRAY_SIZE(ops)), 1);
	radio_tune_set(RADIO_TUNE_SMPS_RX, false);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_RX_ENTRY, ops, ARRAY_SIZE(ops)), 1);
	zassert_equal(ops[0].data[0], 0x06, "reset value's drive bits");
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_TX_ENTRY, ops, ARRAY_SIZE(ops)), 1);
	zassert_equal(ops[0].data[0], 0x06);
}

ZTEST(radio_tune, test_smps_off_without_reset_value_writes_nothing)
{
	radio_tune_ops(RADIO_TUNE_PHASE_RX_ENTRY, ops, ARRAY_SIZE(ops));
	radio_tune_set(RADIO_TUNE_SMPS_RX, false);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_RX_ENTRY, ops, ARRAY_SIZE(ops)), 0);
}

ZTEST(radio_tune, test_regulator_boot_sends_only_what_driver_did_not)
{
	/* loramac-node RadioInit() already sent DC-DC. */
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_BOOT, ops, ARRAY_SIZE(ops)), 0);

	radio_tune_set(RADIO_TUNE_REGULATOR, false);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_BOOT, ops, ARRAY_SIZE(ops)), 1);
	assert_cmd(&ops[0], 0x96, (const uint8_t[]){ 0x00 }, 1);
}

ZTEST(radio_tune, test_regulator_reapply_always_sends)
{
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_REAPPLY, ops, ARRAY_SIZE(ops)), 1);
	assert_cmd(&ops[0], 0x96, (const uint8_t[]){ 0x01 }, 1);

	radio_tune_set(RADIO_TUNE_REGULATOR, false);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_REAPPLY, ops, ARRAY_SIZE(ops)), 1);
	assert_cmd(&ops[0], 0x96, (const uint8_t[]){ 0x00 }, 1);

	/* Nothing radio-side runs at a mode entry for the regulator. */
	radio_tune_set(RADIO_TUNE_SMPS_RX, false);
	zassert_equal(radio_tune_ops(RADIO_TUNE_PHASE_RX_ENTRY, ops, ARRAY_SIZE(ops)), 0);
}

static void assert_pa_sequence(int8_t power, int8_t max, uint8_t duty, int8_t reg, uint8_t ramp)
{
	uint8_t r = RT_RAMP_40_US;
	size_t n = radio_tune_tx_power_ops(power, true, max, &r, ops, ARRAY_SIZE(ops));

	zassert_equal(n, 3, "power %d", power);
	/* The driver's own order: SetPaConfig, OCP, SetTxParams. */
	assert_cmd(&ops[0], 0x95, (const uint8_t[]){ duty, 0x00, 0x01, 0x01 }, 4);
	zassert_equal(ops[1].kind, RT_OP_REG_WRITE);
	zassert_equal(ops[1].address, 0x08E7);
	zassert_equal(ops[1].data[0], 0x18);
	assert_cmd(&ops[2], 0x8E, (const uint8_t[]){ (uint8_t)reg, ramp }, 2);
	zassert_equal(r, ramp);
}

ZTEST(radio_tune, test_pa_table_by_requested_power)
{
	radio_tune_set(RADIO_TUNE_PA_TABLE, true);
	radio_tune_set(RADIO_TUNE_RAMP, true);
	assert_pa_sequence(14, RFO_LP_MAX, 0x04, 0x0E, RT_RAMP_200_US); /* the host's 14 dBm */
	assert_pa_sequence(15, RFO_LP_MAX, 0x07, 0x0E, RT_RAMP_200_US);
	assert_pa_sequence(22, RFO_LP_MAX, 0x07, 0x0E, RT_RAMP_200_US); /* clamped to 15 */
	assert_pa_sequence(12, RFO_LP_MAX, 0x04, 0x0C, RT_RAMP_200_US);
	assert_pa_sequence(10, RFO_LP_MAX, 0x01, 0x0D, RT_RAMP_200_US);
	assert_pa_sequence(0, RFO_LP_MAX, 0x01, 0x03, RT_RAMP_200_US);
	assert_pa_sequence(-30, RFO_LP_MAX, 0x01, -17, RT_RAMP_200_US);
	assert_pa_sequence(14, 10, 0x01, 0x0D, RT_RAMP_200_US); /* a +10 dBm board */

	radio_tune_set(RADIO_TUNE_RAMP, false);
	assert_pa_sequence(14, RFO_LP_MAX, 0x04, 0x0E, RT_RAMP_40_US);
}

ZTEST(radio_tune, test_pa_table_off_leaves_driver_path)
{
	uint8_t ramp = RT_RAMP_40_US;

	radio_tune_set(RADIO_TUNE_RAMP, true);
	zassert_equal(radio_tune_tx_power_ops(14, true, RFO_LP_MAX, &ramp, ops, ARRAY_SIZE(ops)),
		      0);
	zassert_equal(ramp, RT_RAMP_200_US, "ramp alone rides on the driver's path");

	ramp = RT_RAMP_40_US;
	radio_tune_set(RADIO_TUNE_RAMP, false);
	zassert_equal(radio_tune_tx_power_ops(14, true, RFO_LP_MAX, &ramp, ops, ARRAY_SIZE(ops)),
		      0);
	zassert_equal(ramp, RT_RAMP_40_US, "stock driver: 40 us");
}

ZTEST(radio_tune, test_pa_table_never_for_rfo_hp)
{
	uint8_t ramp = RT_RAMP_40_US;

	radio_tune_set(RADIO_TUNE_PA_TABLE, true);
	radio_tune_set(RADIO_TUNE_RAMP, true);
	zassert_equal(radio_tune_tx_power_ops(14, false, RFO_LP_MAX, &ramp, ops, ARRAY_SIZE(ops)),
		      0);
	zassert_equal(ramp, RT_RAMP_200_US);
}

ZTEST(radio_tune, test_ops_never_overrun_cap)
{
	struct radio_tune_op small[2];
	uint8_t ramp = RT_RAMP_40_US;

	radio_tune_set(RADIO_TUNE_PA_TABLE, true);
	memset(small, 0xA5, sizeof(small));
	zassert_equal(radio_tune_tx_power_ops(14, true, RFO_LP_MAX, &ramp, small, 1), 1);
	zassert_equal(small[0].opcode, 0x95, "first in order");
	zassert_equal(small[1].opcode, 0xA5, "untouched past cap");
}

ZTEST(radio_tune, test_summary_line)
{
	char line[160];

	radio_tune_summary(line, sizeof(line));
	zassert_str_equal(line, "rx_boost=on(0x96) smps_rx=on(40mA) regulator=dcdc "
				"smps_tx=off(60mA) pa_table=off ramp=40us");

	for (int i = 0; i < RADIO_TUNE_ITEM_COUNT; i++) {
		radio_tune_set(i, true);
	}
	radio_tune_summary(line, sizeof(line));
	zassert_str_equal(line, "rx_boost=on(0x96) smps_rx=on(40mA) regulator=dcdc "
				"smps_tx=on(60mA) pa_table=on ramp=200us");

	for (int i = 0; i < RADIO_TUNE_ITEM_COUNT; i++) {
		radio_tune_set(i, false);
	}
	radio_tune_summary(line, sizeof(line));
	zassert_str_equal(line, "rx_boost=off(0x96) smps_rx=off(40mA) regulator=ldo "
				"smps_tx=off(60mA) pa_table=off ramp=40us");

	char tiny[8];

	zassert_equal(radio_tune_summary(tiny, sizeof(tiny)), 7);
	zassert_equal(strlen(tiny), 7);
}

ZTEST_SUITE(radio_tune, NULL, NULL, reset_state, NULL, NULL);
