/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway runtime seam: what twr_cmd.c and cmd_dispatch.c need from the radio runtime
 * (twr_gateway.c) beyond radio_ctl.h. tests/cmd_dispatch links a fake instead.
 */
#ifndef APP_TWR_RT_H_
#define APP_TWR_RT_H_

#include <stdint.h>

#include "proto.h"

struct twr_gwc;

/* The gateway core under its lock (a mutex: never from an ISR). */
struct twr_gwc *twr_rt_lock(void);
void twr_rt_unlock(void);

/* TWR_START accepted by the core: enter gateway receive (CONFIGURED -> GATEWAY; re-armed
 * with the current radio config when already in it). INTERNAL when the radio is not up. */
enum p2p_status twr_rt_enter_gateway(void);

/* Airtime of every TX in the sliding hour, ms (the duty ledger, report-only in P2). */
uint32_t twr_rt_airtime_ms_last_hour(void);

/* A command changed the counter block or the queue: let the runtime raise what is due
 * (EVT_TWR_CTR_LOW, housekeeping) outside the command's RSP. */
void twr_rt_kick(void);

#endif /* APP_TWR_RT_H_ */
