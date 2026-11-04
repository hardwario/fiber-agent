/* SPDX-License-Identifier: Apache-2.0
 *
 * Northbridge radio tuning, 0.2.2 (docs/problems.md §1; proximos-v2
 * plan/control/radio/p2p_link_adaptation.md WP N0): the radio init steps the Zephyr sx126x
 * driver lacks, each switchable at build time (Kconfig.radio_tune) and at run time (the
 * `radio tune` RTT shell), and the register/command operations each driver hook performs for
 * the current settings.
 *
 * Pure logic, no radio access: radio_tune_hw.c runs the operations from the hooks of the
 * patched driver (patches/lora/nb_sx126x_hooks.h), tests/radio_tune checks them on native_sim.
 */
#ifndef APP_RADIO_TUNE_H_
#define APP_RADIO_TUNE_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

enum radio_tune_item {
	RADIO_TUNE_RX_BOOST,  /* RX: boosted LNA gain in 0x08AC instead of 0x94 */
	RADIO_TUNE_SMPS_RX,   /* RX: SMPS drive level (0x0923) before SET_RX */
	RADIO_TUNE_REGULATOR, /* RX+TX: SetRegulatorMode DC-DC + LDO; off = LDO only */
	RADIO_TUNE_SMPS_TX,   /* TX: SMPS drive level before SET_TX */
	RADIO_TUNE_PA_TABLE,  /* TX: SetPaConfig row by requested power, not by max power */
	RADIO_TUNE_RAMP,      /* TX: 200 us PA ramp instead of the driver's 40 us */
	RADIO_TUNE_ITEM_COUNT,
};

struct radio_tune_item_info {
	const char *name;
	bool changes_tx;
	bool has_param;
	const char *help;
};

/* SX1261/2 datasheet and STM32WL RM0453 (SUBGHZ) registers and commands. */
#define RT_REG_RETENTION_LIST   0x029F /* count, then up to 4 big-endian addresses */
#define RT_RETENTION_LIST_LEN   9
#define RT_REG_TX_MODULATION    0x0889
#define RT_REG_RX_GAIN          0x08AC
#define RT_REG_OCP              0x08E7
#define RT_REG_SMPS_C2          0x0923 /* SUBGHZ_SMPSC2R: drive level in bits 2:1 */
#define RT_SMPS_DRV_MASK        0x06
#define RT_RX_GAIN_POWER_SAVING 0x94
#define RT_RX_GAIN_BOOSTED      0x96
#define RT_RX_GAIN_MAX          0x97
#define RT_OCP_RFO_LP           0x18 /* 60 mA, as the driver's sx126x_stm32wl.c */
#define RT_CMD_SET_TX_PARAMS    0x8E
#define RT_CMD_SET_PA_CONFIG    0x95
#define RT_CMD_SET_REGULATOR    0x96
#define RT_CMD_CALIBRATE_IMAGE  0x98
#define RT_CMD_GET_ERROR        0x17
#define RT_CMD_GET_STATUS       0xC0
#define RT_REGULATOR_LDO        0x00
#define RT_REGULATOR_DCDC       0x01
#define RT_RAMP_40_US           0x02
#define RT_RAMP_200_US          0x04

enum radio_tune_op_kind {
	RT_OP_CMD,       /* command `opcode` with data[0..len) */
	RT_OP_REG_WRITE, /* register `address` = data[0] */
	RT_OP_REG_BITS,  /* register `address`: the `mask` bits = data[0], others kept */
};

struct radio_tune_op {
	uint8_t kind;
	uint8_t opcode;
	uint16_t address;
	uint8_t mask;
	uint8_t len;
	uint8_t data[4];
};

#define RADIO_TUNE_MAX_OPS 4

/* When a list of operations runs. */
enum radio_tune_phase {
	/* Once after the driver's own init (radio_ctl_init), chip awake in STDBY_RC: only
	 * what differs from what the driver already sent. */
	RADIO_TUNE_PHASE_BOOT,
	/* `radio tune set`/`apply`, RX paused, chip awake in STDBY_RC: every standby-only
	 * setting, whether or not it differs. RX is re-armed afterwards. */
	RADIO_TUNE_PHASE_REAPPLY,
	/* board_sx126x_enter_mode(false/true): before SET_RX/SET_TX, chip awake. */
	RADIO_TUNE_PHASE_RX_ENTRY,
	RADIO_TUNE_PHASE_TX_ENTRY,
};

/* Back to the Kconfig defaults; forgets the SMPS reset value and whether it was written. */
void radio_tune_reset(void);

bool radio_tune_get(enum radio_tune_item item);
void radio_tune_set(enum radio_tune_item item, bool on);

/* The item's parameter: rx_boost the gain byte (0x96 or 0x97), smps_rx/smps_tx the drive in
 * mA (20, 40, 60, 100). -EINVAL for a value out of range or an item without a parameter. */
int radio_tune_set_param(enum radio_tune_item item, uint32_t value);
uint32_t radio_tune_param(enum radio_tune_item item);

/* Item index by name, -ENOENT if none. */
int radio_tune_find(const char *name);
const struct radio_tune_item_info *radio_tune_info(enum radio_tune_item item);

/* The SMPSC2R value the chip had before any write (read at boot), so "off" restores it. */
void radio_tune_set_smps_reset_value(uint8_t reg_value);

/* SMPS drive code (bits 2:1 of SMPSC2R) for `ma`, -EINVAL unless 20/40/60/100; and back. */
int radio_tune_smps_bits(uint32_t ma);
uint32_t radio_tune_smps_ma(uint8_t reg_value);

/* board_sx126x_filter_reg(): the value to write instead of `value`. */
uint8_t radio_tune_filter_reg(uint16_t address, uint8_t value);

/* The operations of `phase`, in the order they run; returns their count (<= cap, and cap
 * >= RADIO_TUNE_MAX_OPS always suffices). */
size_t radio_tune_ops(enum radio_tune_phase phase, struct radio_tune_op *ops, size_t cap);

/* board_sx126x_set_rf_tx_power(): 0 when the driver's sx126x_set_tx_params(power, *ramp)
 * should run (*ramp possibly changed), else the operations that replace it (SetPaConfig,
 * OCP, SetTxParams, in that order - the driver's own order). `rfo_lp`/`max_power` are the
 * devicetree's power-amplifier-output and rfo-lp-max-power. */
size_t radio_tune_tx_power_ops(int8_t power, bool rfo_lp, int8_t max_power, uint8_t *ramp,
			       struct radio_tune_op *ops, size_t cap);

/* One line, e.g. "rx_boost=on(0x96) smps_rx=on(40mA) ... ramp=on(200us)". */
size_t radio_tune_summary(char *buf, size_t cap);

#endif /* APP_RADIO_TUNE_H_ */
