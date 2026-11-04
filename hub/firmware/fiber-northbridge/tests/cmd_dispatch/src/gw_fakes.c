/* SPDX-License-Identifier: Apache-2.0
 *
 * Test doubles, see gw_fakes.h.
 */
#include "gw_fakes.h"

#include <string.h>

#include "nb_boot.h"
#include "twr_rt.h"
#include "uart_link.h"

struct fake_frame fake_rsp;
int fake_rsp_count;
struct fake_frame fake_evt;
int fake_evt_count;
uint32_t fake_link_crc_err;
uint32_t fake_link_tx_drops;
uint8_t fake_event_seq;

struct fake_radio fake_radio;

struct twr_gwc fake_gwc;
int fake_lock_depth;
int fake_enter_calls;
enum p2p_status fake_enter_ret;
int fake_kick_calls;
uint32_t fake_airtime_ms;

uint32_t fake_app_image_len;
uint32_t fake_app_crc32;
int fake_rom_boot_calls;
int fake_rom_boot_rsp_count;

void gw_fakes_reset(void)
{
	memset(&fake_rsp, 0, sizeof(fake_rsp));
	fake_rsp_count = 0;
	memset(&fake_evt, 0, sizeof(fake_evt));
	fake_evt_count = 0;
	fake_link_crc_err = 0;
	fake_link_tx_drops = 0;
	fake_event_seq = 0;

	memset(&fake_radio, 0, sizeof(fake_radio));
	fake_radio.state = P2P_NB_BOOT;

	twr_gwc_init(&fake_gwc);
	fake_lock_depth = 0;
	fake_enter_calls = 0;
	fake_enter_ret = P2P_STATUS_OK;
	fake_kick_calls = 0;
	fake_airtime_ms = 0;

	fake_app_image_len = 0;
	fake_app_crc32 = 0;
	fake_rom_boot_calls = 0;
	fake_rom_boot_rsp_count = -1;
}

static void capture(struct fake_frame *f, uint8_t type, uint8_t seq, const uint8_t *body,
		    size_t len)
{
	f->type = type;
	f->seq = seq;
	f->len = len < sizeof(f->body) ? len : sizeof(f->body);
	if (f->len > 0) {
		memcpy(f->body, body, f->len);
	}
}

/* ---- uart_link.h ---------------------------------------------------------------------- */

void uart_link_send(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len)
{
	fake_rsp_count++;
	capture(&fake_rsp, type, seq, body, body_len);
}

void uart_link_send_event(uint8_t type, const uint8_t *body, size_t body_len)
{
	fake_evt_count++;
	capture(&fake_evt, type, fake_event_seq, body, body_len);
}

void uart_link_set_event_seq(uint8_t seq)
{
	fake_event_seq = seq;
}

uint32_t uart_link_get_crc_err_count(void)
{
	return fake_link_crc_err;
}

uint32_t uart_link_get_tx_drops(void)
{
	return fake_link_tx_drops;
}

/* ---- radio_ctl.h ---------------------------------------------------------------------- */

enum p2p_nb_state radio_ctl_state(void)
{
	return fake_radio.state;
}

enum p2p_status radio_ctl_set_config(const struct radio_config *cfg)
{
	fake_radio.set_config_calls++;
	if (fake_radio.state == P2P_NB_GATEWAY) {
		return P2P_STATUS_INVALID_STATE;
	}
	if (fake_radio.set_config_ret != P2P_STATUS_OK) {
		return fake_radio.set_config_ret;
	}
	fake_radio.cfg = *cfg;
	fake_radio.cfg_valid = true;
	fake_radio.state = P2P_NB_CONFIGURED;
	return P2P_STATUS_OK;
}

bool radio_ctl_get_config(struct radio_config *out)
{
	if (!fake_radio.cfg_valid) {
		memset(out, 0, sizeof(*out));
		return false;
	}
	*out = fake_radio.cfg;
	return true;
}

enum p2p_status radio_ctl_rx_stop(void)
{
	fake_radio.rx_stop_calls++;
	if (fake_radio.state == P2P_NB_GATEWAY) {
		fake_radio.state = P2P_NB_CONFIGURED;
	}
	return P2P_STATUS_OK;
}

enum p2p_status radio_ctl_tx_schedule(uint8_t tx_id, uint64_t t_ms, const uint8_t *frame,
				       uint8_t len)
{
	fake_radio.tx_schedule_calls++;
	fake_radio.tx_id = tx_id;
	fake_radio.t_ms = t_ms;
	fake_radio.frame_len = len;
	memcpy(fake_radio.frame, frame, len);
	return fake_radio.tx_schedule_ret;
}

enum p2p_status radio_ctl_tx_cancel(uint8_t tx_id)
{
	fake_radio.tx_cancel_calls++;
	fake_radio.cancel_tx_id = tx_id;
	return fake_radio.tx_cancel_ret;
}

void radio_ctl_get_stats(struct radio_ctl_stats *out)
{
	*out = fake_radio.stats;
}

uint64_t radio_ctl_now_ms(void)
{
	return fake_radio.now_ms;
}

/* ---- twr_rt.h ------------------------------------------------------------------------- */

struct twr_gwc *twr_rt_lock(void)
{
	fake_lock_depth++;
	return &fake_gwc;
}

void twr_rt_unlock(void)
{
	fake_lock_depth--;
}

enum p2p_status twr_rt_enter_gateway(void)
{
	fake_enter_calls++;
	if (fake_enter_ret == P2P_STATUS_OK) {
		fake_radio.state = P2P_NB_GATEWAY;
	}
	return fake_enter_ret;
}

uint32_t twr_rt_airtime_ms_last_hour(void)
{
	return fake_airtime_ms;
}

void twr_rt_kick(void)
{
	fake_kick_calls++;
}

/* ---- nb_boot.h ------------------------------------------------------------------------ */

void nb_boot_app_record(uint32_t *image_len, uint32_t *image_crc32)
{
	*image_len = fake_app_image_len;
	*image_crc32 = fake_app_crc32;
}

void nb_boot_enter_rom_bootloader(void)
{
	fake_rom_boot_calls++;
	fake_rom_boot_rsp_count = fake_rsp_count;
}
