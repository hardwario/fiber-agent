/* SPDX-License-Identifier: Apache-2.0
 *
 * Northbridge diagnostics, see diag.h. Written from the radio workqueue, the system
 * workqueue and the main thread, so every field sits under one spinlock; the EVT_LOG
 * itself goes out after the lock is released.
 */
#include "diag.h"
#include "proto.h"
#include "uart_link.h"

#include <stdbool.h>
#include <string.h>
#include <zephyr/kernel.h>

static struct k_spinlock lock;

static uint8_t radio_state = DIAG_RADIO_IDLE;
static uint8_t boot_reset_cause;
static uint32_t err_flags;
static uint32_t rx_rearm_fail;
static uint32_t tx_radio_err;
static uint32_t link_err;
static uint8_t last_err_code;
static int64_t last_err_ms;
static int64_t last_log_ms[DIAG_CODE_COUNT];
static bool log_sent[DIAG_CODE_COUNT];

static const uint32_t code_flag[DIAG_CODE_COUNT] = {
	[DIAG_CODE_RADIO_INIT] = DIAG_FLAG_RADIO_INIT,
	[DIAG_CODE_RX_REARM] = DIAG_FLAG_RX_REARM,
	[DIAG_CODE_TX_RADIO] = DIAG_FLAG_TX_RADIO,
	[DIAG_CODE_LINK_FRAMING] = DIAG_FLAG_LINK_FRAMING,
	[DIAG_CODE_UART_OVERRUN] = DIAG_FLAG_UART_OVERRUN,
};

static void bump(uint32_t *counter)
{
	if (*counter != UINT32_MAX) {
		(*counter)++;
	}
}

void diag_set_boot_reset_cause(uint8_t cause)
{
	k_spinlock_key_t key = k_spin_lock(&lock);

	boot_reset_cause = cause;
	k_spin_unlock(&lock, key);
}

void diag_set_radio_state(enum diag_radio_state state)
{
	k_spinlock_key_t key = k_spin_lock(&lock);

	radio_state = (uint8_t)state;
	k_spin_unlock(&lock, key);
}

enum diag_radio_state diag_radio_state(void)
{
	k_spinlock_key_t key = k_spin_lock(&lock);
	enum diag_radio_state state = (enum diag_radio_state)radio_state;

	k_spin_unlock(&lock, key);
	return state;
}

void diag_set_flag(uint32_t flag)
{
	k_spinlock_key_t key = k_spin_lock(&lock);

	err_flags |= flag;
	k_spin_unlock(&lock, key);
}

size_t diag_encode_log(uint8_t *out, size_t cap, uint8_t level, uint8_t code, const char *detail)
{
	if (cap < 2) {
		return 0;
	}
	size_t n = 0;

	out[n++] = level;
	out[n++] = code;
	if (detail != NULL) {
		size_t len = strlen(detail);

		if (len > cap - n) {
			len = cap - n;
		}
		memcpy(&out[n], detail, len);
		n += len;
	}
	return n;
}

void diag_report(enum diag_code code, const char *detail)
{
	if (code <= DIAG_CODE_NONE || code >= DIAG_CODE_COUNT) {
		return;
	}

	int64_t now = k_uptime_get();
	bool send = false;
	k_spinlock_key_t key = k_spin_lock(&lock);

	err_flags |= code_flag[code];
	switch (code) {
	case DIAG_CODE_RX_REARM:
		bump(&rx_rearm_fail);
		break;
	case DIAG_CODE_TX_RADIO:
		bump(&tx_radio_err);
		break;
	case DIAG_CODE_LINK_FRAMING:
	case DIAG_CODE_UART_OVERRUN:
		bump(&link_err);
		break;
	default:
		break;
	}
	last_err_code = (uint8_t)code;
	last_err_ms = now;
	if (!log_sent[code] || now - last_log_ms[code] >= DIAG_LOG_INTERVAL_MS) {
		log_sent[code] = true;
		last_log_ms[code] = now;
		send = true;
	}
	k_spin_unlock(&lock, key);

	/* A link error is not reported over the link it failed on while it is still failing;
	 * the next EVT_LOG goes out once DIAG_LOG_INTERVAL_MS has passed, and GET_INFO carries
	 * the counter in the meantime. */
	if (send) {
		uint8_t body[2 + 48];
		size_t n = diag_encode_log(body, sizeof(body), DIAG_LEVEL_ERROR, (uint8_t)code,
					   detail);

		uart_link_send_event(P2P_EVT_LOG, body, n);
	}
}

void diag_snapshot(struct diag_info *out)
{
	int64_t now = k_uptime_get();
	k_spinlock_key_t key = k_spin_lock(&lock);

	out->radio_state = radio_state;
	out->boot_reset_cause = boot_reset_cause;
	out->err_flags = err_flags;
	out->rx_rearm_fail = rx_rearm_fail;
	out->tx_radio_err = tx_radio_err;
	out->link_err = link_err;
	out->last_err_code = last_err_code;
	out->last_err_age_s = 0;
	if (last_err_code != DIAG_CODE_NONE) {
		int64_t age = (now - last_err_ms) / 1000;

		out->last_err_age_s = age > (int64_t)UINT32_MAX ? UINT32_MAX : (uint32_t)age;
	}
	k_spin_unlock(&lock, key);
}
