/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway runtime (the Northbridge image): radio_ctl.h and twr_rt.h are its API; this
 * header only adds what main.c's shell needs. See twr_gateway.c.
 */
#ifndef APP_TWR_GATEWAY_H_
#define APP_TWR_GATEWAY_H_

struct shell;

/* One summary line for `p2p_status`. */
void twr_gateway_print_brief(const struct shell *sh);

#endif /* APP_TWR_GATEWAY_H_ */
