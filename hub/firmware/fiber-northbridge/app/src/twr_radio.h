/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER P0 bench gateway runtime (TOWER_P0_GATEWAY=ON only): owns the SX126x, feeds received
 * frames to twr_gw.c, transmits the auto-ACK and the queued downlink at fixed offsets, and
 * provides the `twr` RTT shell group and the TWR_* event lines. See twr_radio.c.
 */
#ifndef APP_TWR_RADIO_H_
#define APP_TWR_RADIO_H_

#include <stdbool.h>
#include <stdint.h>

struct shell;

/* Bring up the radio in continuous RX with the boot defaults. 0 or a negative errno. */
int twr_radio_init(void);

uint32_t twr_radio_gw_addr(void);
uint32_t twr_radio_node_addr(void);

/* A transmission has been running for longer than twice its time on air plus 2 s: main.c
 * stops feeding the watchdog. */
bool twr_radio_tx_stuck(void);

/* One summary line for `p2p_status`. */
void twr_radio_print_brief(const struct shell *sh);

#endif /* APP_TWR_RADIO_H_ */
