/* SPDX-License-Identifier: Apache-2.0
 *
 * ProXimos northbridge 0.2.2: what the patched sx126x driver (patches/lora/sx126x.c) exposes
 * to the application's radio tuning (app/src/radio_tune_hw.c, docs/problems.md §1).
 *
 * Plain C types only, so the application includes this without the loramac-node headers.
 * The board_sx126x_*() hooks are weak no-ops in the driver; the application overrides them
 * when CONFIG_NB_RADIO_TUNE=y. The driver calls them only for CONFIG_BOARD_STM32WL5MOC.
 */
#ifndef NB_SX126X_HOOKS_H_
#define NB_SX126X_HOOKS_H_

#include <stdbool.h>
#include <stdint.h>

/* RadioOperatingModes_t MODE_SLEEP and MODE_STDBY_RC (loramac-node sx126x.h), checked in
 * the driver. */
#define NB_SX126X_MODE_SLEEP    0
#define NB_SX126X_MODE_STDBY_RC 1

/* A single-register write is about to go out over SPI: return the value to write instead
 * (e.g. the RX gain 0x08AC that SX126xSetRx() sets to 0x94 before every SET_RX). */
uint8_t board_sx126x_filter_reg(uint16_t address, uint8_t value);

/* The driver is entering TX (`tx`) or RX (SX126xSetOperatingMode(), called from
 * SX126xSetTx()/SX126xSetRx() before the SET_TX/SET_RX command): the chip is awake and in
 * STDBY or FS, so register access is safe here. Not called for SLEEP, STDBY, RX_DC or CAD. */
void board_sx126x_enter_mode(bool tx);

/* SX126xSetRfTxPower(): return true when the board has sent SetPaConfig/SetTxParams itself.
 * Otherwise the driver runs its own sx126x_set_tx_params(power, *ramp); the hook may change
 * *ramp (a RadioRampTimes_t code, 0x02 = 40 us on entry). */
bool board_sx126x_set_rf_tx_power(int8_t power, uint8_t *ramp);

/* The parameters of the last commands whose values the chip cannot report back, as the
 * driver sent them (SX126xWriteCommand()). `seen` has a NB_SX126X_SHADOW_* bit for each
 * command sent at least once since boot. */
#define NB_SX126X_SHADOW_REGULATOR (1u << 0)
#define NB_SX126X_SHADOW_PA_CONFIG (1u << 1)
#define NB_SX126X_SHADOW_TX_PARAMS (1u << 2)
#define NB_SX126X_SHADOW_CAL_IMAGE (1u << 3)

struct nb_sx126x_shadow {
	uint8_t seen;
	uint8_t regulator_mode; /* SetRegulatorMode 0x96: 0x00 LDO, 0x01 DC-DC + LDO */
	uint8_t pa_config[4];   /* SetPaConfig 0x95: paDutyCycle, hpMax, deviceSel, paLut */
	uint8_t tx_params[2];   /* SetTxParams 0x8E: power, rampTime */
	uint8_t cal_image[2];   /* CalibrateImage 0x98: freq1, freq2 */
};

extern struct nb_sx126x_shadow sx126x_nb_shadow;

/* Thin wrappers over the loramac-node SX126x accessors, for the application's register
 * readback. Each one wakes a sleeping chip, as every SX126x SPI access does. */
void sx126x_nb_read_regs(uint16_t address, uint8_t *buf, uint16_t len);
void sx126x_nb_write_reg(uint16_t address, uint8_t value);
void sx126x_nb_write_cmd(uint8_t opcode, const uint8_t *buf, uint16_t len);
/* Returns the status byte the chip clocks out with the command. */
uint8_t sx126x_nb_read_cmd(uint8_t opcode, uint8_t *buf, uint16_t len);
/* The driver's view of the operating mode (RadioOperatingModes_t). */
uint8_t sx126x_nb_driver_mode(void);
/* Radio.Sleep(): warm-start sleep, as the Zephyr driver leaves the radio between uses. */
void sx126x_nb_sleep(void);

#endif /* NB_SX126X_HOOKS_H_ */
