/* SPDX-License-Identifier: Apache-2.0
 *
 * Northbridge radio tuning on the real radio: overrides the patched driver's weak
 * board_sx126x_*() hooks (patches/lora/nb_sx126x_hooks.h) with the operations radio_tune.c
 * computes, logs the effective tuning at boot, and gives the RTT shell `radio tune` for an A/B
 * against the same Node frames without reflashing, with a register readback of what the chip
 * really holds (docs/problems.md §1).
 *
 * The hooks run in the driver's own context (the radio work queue for TX and for arming RX,
 * the system work queue for the RX re-arm after a frame). The shell never touches the radio
 * itself: it goes through radio_ctl_call(), which runs on the radio work queue.
 */
#include "radio_tune.h"
#include "radio_tune_hw.h"
#include "diag.h"
#include "proto.h"
#include "radio_ctl.h"
#include "nb_sx126x_hooks.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <zephyr/devicetree.h>
#include <zephyr/kernel.h>
#include <zephyr/shell/shell.h>
#include <zephyr/sys/byteorder.h>
#include <zephyr/sys/util.h>

#include <stm32_ll_pwr.h>

#include <zephyr/logging/log.h>
LOG_MODULE_REGISTER(radio_tune, CONFIG_LOG_DEFAULT_LEVEL);

#define LORA_NODE DT_ALIAS(lora0)
#define NB_RFO_LP DT_ENUM_HAS_VALUE(LORA_NODE, power_amplifier_output, rfo_lp)
#define NB_RFO_LP_MAX_POWER DT_PROP(LORA_NODE, rfo_lp_max_power)

static void run_ops(const struct radio_tune_op *ops, size_t n)
{
	for (size_t i = 0; i < n; i++) {
		const struct radio_tune_op *op = &ops[i];
		uint8_t v;

		switch (op->kind) {
		case RT_OP_CMD:
			sx126x_nb_write_cmd(op->opcode, op->data, op->len);
			break;
		case RT_OP_REG_WRITE:
			sx126x_nb_write_reg(op->address, op->data[0]);
			break;
		case RT_OP_REG_BITS:
			sx126x_nb_read_regs(op->address, &v, 1);
			sx126x_nb_write_reg(op->address,
					    (uint8_t)((v & ~op->mask) | (op->data[0] & op->mask)));
			break;
		default:
			break;
		}
	}
}

static void run_phase(enum radio_tune_phase phase)
{
	struct radio_tune_op ops[RADIO_TUNE_MAX_OPS];

	run_ops(ops, radio_tune_ops(phase, ops, ARRAY_SIZE(ops)));
}

uint8_t board_sx126x_filter_reg(uint16_t address, uint8_t value)
{
	return radio_tune_filter_reg(address, value);
}

void board_sx126x_enter_mode(bool tx)
{
	run_phase(tx ? RADIO_TUNE_PHASE_TX_ENTRY : RADIO_TUNE_PHASE_RX_ENTRY);
}

bool board_sx126x_set_rf_tx_power(int8_t power, uint8_t *ramp)
{
	struct radio_tune_op ops[RADIO_TUNE_MAX_OPS];
	size_t n = radio_tune_tx_power_ops(power, NB_RFO_LP, NB_RFO_LP_MAX_POWER, ramp, ops,
					   ARRAY_SIZE(ops));

	if (n == 0) {
		return false;
	}
	run_ops(ops, n);
	return true;
}

void radio_tune_hw_boot(void)
{
	static char line[160];
	bool asleep = sx126x_nb_driver_mode() == NB_SX126X_MODE_SLEEP;
	uint8_t smps;

	/* Nothing has written SMPSC2R yet: this is the chip's own drive level, which "off"
	 * puts back after a run-time A/B. Reading it wakes the radio from the driver's sleep. */
	sx126x_nb_read_regs(RT_REG_SMPS_C2, &smps, 1);
	radio_tune_set_smps_reset_value(smps);
	run_phase(RADIO_TUNE_PHASE_BOOT);
	if (asleep) {
		sx126x_nb_sleep();
	}

	radio_tune_summary(line, sizeof(line));
	LOG_INF("radio tune: %s; SMPSC2R reset 0x%02x (%u mA)", line, smps,
		(unsigned int)radio_tune_smps_ma(smps));
}

#if defined(CONFIG_SHELL)

/* ---- RTT shell: radio tune show|set|apply|regs ---- */

/* Back to sleep after a shell access woke the radio, unless something armed it meanwhile. */
static void resleep_if_idle(bool was_asleep)
{
	if (was_asleep && sx126x_nb_driver_mode() == NB_SX126X_MODE_STDBY_RC) {
		sx126x_nb_sleep();
	}
}

static void reapply_fn(void *arg)
{
	ARG_UNUSED(arg);
	bool asleep = sx126x_nb_driver_mode() == NB_SX126X_MODE_SLEEP;

	run_phase(RADIO_TUNE_PHASE_REAPPLY);
	resleep_if_idle(asleep);
}

struct readback {
	uint8_t driver_mode;
	bool woke;
	uint8_t status;
	uint8_t err[2];
	uint8_t rx_gain;
	uint8_t retention[RT_RETENTION_LIST_LEN];
	uint8_t smps_c2;
	uint8_t ocp;
	struct nb_sx126x_shadow shadow;
	bool pwr_smps_en;
	bool pwr_smps_rdy;
};

static void readback_fn(void *arg)
{
	struct readback *rb = arg;

	rb->driver_mode = sx126x_nb_driver_mode();
	rb->woke = rb->driver_mode == NB_SX126X_MODE_SLEEP;
	/* MCU-side SMPS flags first, before this readback's own wake-up can change them. */
	rb->pwr_smps_en = LL_PWR_SMPS_GetMode() == LL_PWR_SMPS_STEP_DOWN;
	rb->pwr_smps_rdy = LL_PWR_IsActiveFlag_SMPSRDY() != 0;
	rb->status = sx126x_nb_read_cmd(RT_CMD_GET_STATUS, NULL, 0);
	sx126x_nb_read_cmd(RT_CMD_GET_ERROR, rb->err, sizeof(rb->err));
	sx126x_nb_read_regs(RT_REG_RX_GAIN, &rb->rx_gain, 1);
	sx126x_nb_read_regs(RT_REG_RETENTION_LIST, rb->retention, sizeof(rb->retention));
	sx126x_nb_read_regs(RT_REG_SMPS_C2, &rb->smps_c2, 1);
	sx126x_nb_read_regs(RT_REG_OCP, &rb->ocp, 1);
	rb->shadow = sx126x_nb_shadow;
	resleep_if_idle(rb->woke);
}

static const char *driver_mode_name(uint8_t mode)
{
	static const char *const names[] = { "SLEEP", "STDBY_RC", "STDBY_XOSC", "FS",
					     "TX",    "RX",       "RX_DC",      "CAD" };

	return mode < ARRAY_SIZE(names) ? names[mode] : "?";
}

static const char *chip_mode_name(uint8_t status)
{
	/* GetStatus bits 6:4 */
	static const char *const names[] = { "?", "?", "STBY_RC", "STBY_XOSC",
					     "FS", "RX", "TX", "?" };

	return names[(status >> 4) & 0x7];
}

static const char *ramp_name(uint8_t ramp)
{
	static const char *const names[] = { "10us", "20us", "40us", "80us",
					     "200us", "800us", "1700us", "3400us" };

	return ramp < ARRAY_SIZE(names) ? names[ramp] : "?";
}

static const char *param_str(enum radio_tune_item item, char *buf, size_t cap)
{
	switch (item) {
	case RADIO_TUNE_RX_BOOST:
		snprintf(buf, cap, "0x%02x", (unsigned int)radio_tune_param(item));
		break;
	case RADIO_TUNE_SMPS_RX:
	case RADIO_TUNE_SMPS_TX:
		snprintf(buf, cap, "%umA", (unsigned int)radio_tune_param(item));
		break;
	case RADIO_TUNE_REGULATOR:
		snprintf(buf, cap, "%s", radio_tune_get(item) ? "dcdc" : "ldo");
		break;
	case RADIO_TUNE_RAMP:
		snprintf(buf, cap, "%s", radio_tune_get(item) ? "200us" : "40us");
		break;
	default:
		buf[0] = '\0';
		break;
	}
	return buf;
}

static int cmd_tune_show(const struct shell *sh, size_t argc, char **argv)
{
	ARG_UNUSED(argc);
	ARG_UNUSED(argv);
	char param[12];

	shell_print(sh, "radio tune (fw %d.%d.%d): RX items apply when RX is armed, TX items at "
		    "the next TX; `set` re-arms RX", FW_VERSION_MAJOR, FW_VERSION_MINOR,
		    FW_VERSION_PATCH);
	for (int i = 0; i < RADIO_TUNE_ITEM_COUNT; i++) {
		const struct radio_tune_item_info *info = radio_tune_info(i);

		shell_print(sh, "  %-9s %-3s %-6s %-5s %s", info->name,
			    radio_tune_get(i) ? "on" : "off", param_str(i, param, sizeof(param)),
			    i == RADIO_TUNE_REGULATOR ? "RX+TX" : (info->changes_tx ? "TX" : "RX"),
			    info->help);
	}
	return 0;
}

static int reapply(const struct shell *sh)
{
	static char line[160];
	int rc = radio_ctl_call(reapply_fn, NULL, true);

	radio_tune_summary(line, sizeof(line));
	if (rc != 0) {
		shell_error(sh, "re-apply failed (%d): %s", rc, line);
		return rc;
	}
	LOG_INF("radio tune: %s", line);
	shell_print(sh, "applied: %s", line);
	return 0;
}

static int cmd_tune_set(const struct shell *sh, size_t argc, char **argv)
{
	if (argc != 3) {
		shell_error(sh, "usage: radio tune set <item|all> on|off|<value>");
		return -EINVAL;
	}

	bool all = strcmp(argv[1], "all") == 0;
	int item = all ? -1 : radio_tune_find(argv[1]);

	if (!all && item < 0) {
		shell_error(sh, "unknown item '%s' (see radio tune show)", argv[1]);
		return -EINVAL;
	}

	bool on = strcmp(argv[2], "on") == 0;
	bool off = strcmp(argv[2], "off") == 0;

	if (on || off) {
		for (int i = 0; i < RADIO_TUNE_ITEM_COUNT; i++) {
			if (all || i == item) {
				radio_tune_set(i, on);
			}
		}
	} else {
		char *end;
		unsigned long value = strtoul(argv[2], &end, 0);

		if (all || *end != '\0' || radio_tune_set_param(item, value) != 0) {
			shell_error(sh, "'%s' is not on/off or a value this item takes "
				    "(rx_boost 0x96|0x97, smps_rx/smps_tx 20|40|60|100)", argv[2]);
			return -EINVAL;
		}
	}
	return reapply(sh);
}

static int cmd_tune_apply(const struct shell *sh, size_t argc, char **argv)
{
	ARG_UNUSED(argc);
	ARG_UNUSED(argv);
	return reapply(sh);
}

static int cmd_tune_regs(const struct shell *sh, size_t argc, char **argv)
{
	ARG_UNUSED(argc);
	ARG_UNUSED(argv);
	struct readback rb;
	int rc = radio_ctl_call(readback_fn, &rb, false);

	if (rc != 0) {
		shell_error(sh, "readback failed (%d)", rc);
		return rc;
	}

	shell_print(sh, "driver mode %s, chip %s (status 0x%02x)%s, radio_state %u",
		    driver_mode_name(rb.driver_mode), chip_mode_name(rb.status), rb.status,
		    rb.woke ? ", woken from sleep for this readback" : "",
		    (unsigned int)diag_radio_state());
	/* GetDeviceErrors OpError, SX1261/2 datasheet: bits 0..6 and 8. */
	static const char *const err_names[] = { "RC64K_CALIB", "RC13M_CALIB", "PLL_CALIB",
						 "ADC_CALIB",   "IMG_CALIB",   "XOSC_START",
						 "PLL_LOCK",    "?",           "PA_RAMP" };
	uint16_t op_err = sys_get_be16(rb.err);

	shell_print(sh, "op errors 0x%04x", op_err);
	for (int b = 0; b < ARRAY_SIZE(err_names); b++) {
		if (op_err & BIT(b)) {
			shell_print(sh, "  %s_ERR", err_names[b]);
		}
	}
	shell_print(sh, "0x08AC rx gain   0x%02x (%s)", rb.rx_gain,
		    rb.rx_gain == RT_RX_GAIN_POWER_SAVING ? "power saving" :
		    rb.rx_gain >= RT_RX_GAIN_BOOSTED ? "boosted" : "?");

	bool gain_retained = false;
	uint8_t count = MIN(rb.retention[0], 4);

	for (uint8_t i = 0; i < count; i++) {
		uint16_t reg = sys_get_be16(&rb.retention[1 + 2 * i]);

		shell_print(sh, "0x029F retention[%u] 0x%04x", i, reg);
		gain_retained |= reg == RT_REG_RX_GAIN;
	}
	shell_print(sh, "0x029F retention %u register(s), 0x08AC %s", rb.retention[0],
		    gain_retained ? "retained" : "NOT retained");
	shell_print(sh, "0x0923 smps c2   0x%02x (drive %u mA)", rb.smps_c2,
		    (unsigned int)radio_tune_smps_ma(rb.smps_c2));
	shell_print(sh, "0x08E7 ocp       0x%02x", rb.ocp);
	shell_print(sh, "PWR SMPSEN %u SMPSRDY %u", rb.pwr_smps_en, rb.pwr_smps_rdy);

	const struct nb_sx126x_shadow *s = &rb.shadow;

	if (s->seen & NB_SX126X_SHADOW_REGULATOR) {
		shell_print(sh, "last SetRegulatorMode 0x%02x (%s)", s->regulator_mode,
			    s->regulator_mode == RT_REGULATOR_DCDC ? "dcdc" : "ldo");
	}
	if (s->seen & NB_SX126X_SHADOW_PA_CONFIG) {
		shell_print(sh, "last SetPaConfig     %02x %02x %02x %02x", s->pa_config[0],
			    s->pa_config[1], s->pa_config[2], s->pa_config[3]);
	}
	if (s->seen & NB_SX126X_SHADOW_TX_PARAMS) {
		shell_print(sh, "last SetTxParams     power %d ramp 0x%02x (%s)",
			    (int8_t)s->tx_params[0], s->tx_params[1], ramp_name(s->tx_params[1]));
	}
	if (s->seen & NB_SX126X_SHADOW_CAL_IMAGE) {
		shell_print(sh, "last CalibrateImage  %02x %02x", s->cal_image[0], s->cal_image[1]);
	}
	return 0;
}

SHELL_STATIC_SUBCMD_SET_CREATE(sub_radio_tune,
	SHELL_CMD(show, NULL, "Tuning items and their state", cmd_tune_show),
	SHELL_CMD_ARG(set, NULL, "set <item|all> on|off|<value>: switch, then re-arm RX",
		      cmd_tune_set, 3, 0),
	SHELL_CMD(apply, NULL, "Re-send the standby settings and re-arm RX", cmd_tune_apply),
	SHELL_CMD(regs, NULL, "Read back what the radio holds", cmd_tune_regs),
	SHELL_SUBCMD_SET_END);

SHELL_STATIC_SUBCMD_SET_CREATE(sub_radio,
	SHELL_CMD(tune, &sub_radio_tune, "RX sensitivity tuning (docs/problems.md 1)", NULL),
	SHELL_SUBCMD_SET_END);

SHELL_CMD_REGISTER(radio, &sub_radio, "Northbridge radio", NULL);

#endif /* CONFIG_SHELL */
