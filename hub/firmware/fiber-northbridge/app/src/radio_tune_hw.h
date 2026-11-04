/* SPDX-License-Identifier: Apache-2.0
 *
 * Northbridge radio tuning on the real radio (radio_tune_hw.c): the patched driver's
 * board_sx126x_*() hooks, the boot step and the `radio tune` RTT shell. CONFIG_NB_RADIO_TUNE.
 */
#ifndef APP_RADIO_TUNE_HW_H_
#define APP_RADIO_TUNE_HW_H_

/* Once from radio_ctl_init(), after the LoRa device is ready and before the host link is up:
 * reads the SMPS drive level the chip starts with, sends what differs from the driver's own
 * init (RADIO_TUNE_PHASE_BOOT), puts the radio back to sleep and logs the effective tuning. */
void radio_tune_hw_boot(void);

#endif /* APP_RADIO_TUNE_HW_H_ */
