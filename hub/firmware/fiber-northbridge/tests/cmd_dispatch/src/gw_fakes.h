/* SPDX-License-Identifier: Apache-2.0
 *
 * Test doubles for the gateway image's command path: uart_link.h (captures RSPs and events),
 * radio_ctl.h (the Northbridge state machine without a radio), twr_rt.h (one static core,
 * a lock that counts) and nb_boot.h (the app_valid record, the reboot into the ROM
 * bootloader). tests/cmd_dispatch and tests/proto_integration link these against the
 * real cmd_dispatch.c, twr_cmd.c, twr_link.c, twr_gwc.c and diag.c.
 */
#ifndef GW_FAKES_H_
#define GW_FAKES_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "proto.h"
#include "radio_ctl.h"
#include "twr_gwc.h"

struct fake_frame {
	uint8_t type;
	uint8_t seq;
	size_t len;
	uint8_t body[P2P_MAX_BODY_LEN];
};

/* uart_link.h */
extern struct fake_frame fake_rsp;   /* last uart_link_send() */
extern int fake_rsp_count;
extern struct fake_frame fake_evt;   /* last uart_link_send_event() */
extern int fake_evt_count;
extern uint32_t fake_link_crc_err;
extern uint32_t fake_link_tx_drops;
extern uint8_t fake_event_seq;       /* uart_link_set_event_seq(): best-effort events' seq */

/* radio_ctl.h: set_config moves BOOT -> CONFIGURED, rx_stop GATEWAY -> CONFIGURED, as the
 * real runtime does; the _ret fields are returned instead when not OK. */
struct fake_radio {
	enum p2p_nb_state state;
	bool cfg_valid;
	struct radio_config cfg;
	int set_config_calls;
	enum p2p_status set_config_ret;
	int rx_stop_calls;
	int tx_schedule_calls;
	uint8_t tx_id;
	uint64_t t_ms;
	uint8_t frame[P2P_MAX_RADIO_FRAME_LEN];
	uint8_t frame_len;
	enum p2p_status tx_schedule_ret;
	int tx_cancel_calls;
	uint8_t cancel_tx_id;
	enum p2p_status tx_cancel_ret;
	struct radio_ctl_stats stats;
	uint64_t now_ms;
};
extern struct fake_radio fake_radio;

/* twr_rt.h: enter_gateway moves to GATEWAY unless enter_ret is not OK. */
extern struct twr_gwc fake_gwc;
extern int fake_lock_depth;
extern int fake_enter_calls;
extern enum p2p_status fake_enter_ret;
extern int fake_kick_calls;
extern uint32_t fake_airtime_ms;

/* nb_boot.h: the record's fields GET_INFO reports; the reboot only counts, and notes how
 * many RSPs had been sent when it came (the RSP must go first). */
extern uint32_t fake_app_image_len;
extern uint32_t fake_app_crc32;
extern int fake_rom_boot_calls;
extern int fake_rom_boot_rsp_count;

void gw_fakes_reset(void);

#endif /* GW_FAKES_H_ */
