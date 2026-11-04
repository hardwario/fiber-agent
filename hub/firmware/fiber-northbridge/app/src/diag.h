/* SPDX-License-Identifier: Apache-2.0
 *
 * Northbridge diagnostics: the radio state, sticky error flags and error counters GET_INFO
 * carries, and rate-limited EVT_LOG error reports in every build.
 * Spec: proximos-v2 plan/control/radio/northbridge_diagnostics.md §2.
 */
#ifndef APP_DIAG_H_
#define APP_DIAG_H_

#include <stddef.h>
#include <stdint.h>

/* §2.1 radio_state */
enum diag_radio_state {
	DIAG_RADIO_IDLE = 0,
	DIAG_RADIO_RX = 1,
	DIAG_RADIO_TX = 2,
	DIAG_RADIO_ERROR = 3,
};

/* §2.2 EVT_LOG codes, also GET_INFO last_err_code */
enum diag_code {
	DIAG_CODE_NONE = 0,
	DIAG_CODE_RADIO_INIT = 1,
	DIAG_CODE_RX_REARM = 2,
	DIAG_CODE_TX_RADIO = 3,
	DIAG_CODE_LINK_FRAMING = 4,
	DIAG_CODE_UART_OVERRUN = 5,
	DIAG_CODE_STACK = 6,
	DIAG_CODE_COUNT,
};

/* §2.2 EVT_LOG levels */
#define DIAG_LEVEL_ERROR   1
#define DIAG_LEVEL_WARNING 2
#define DIAG_LEVEL_DEBUG   3

/* §2.1 err_flags, sticky since boot */
#define DIAG_FLAG_RADIO_INIT   (1u << 0)
#define DIAG_FLAG_RX_REARM     (1u << 1)
#define DIAG_FLAG_TX_RADIO     (1u << 2)
#define DIAG_FLAG_LINK_FRAMING (1u << 3)
#define DIAG_FLAG_UART_OVERRUN (1u << 4)
#define DIAG_FLAG_TX_INTERNAL  (1u << 5)

/* What GET_INFO reads (the counters saturate at UINT32_MAX). */
struct diag_info {
	uint8_t radio_state; /* enum diag_radio_state, shell only */
	uint8_t boot_reset_cause;
	uint32_t err_flags;
	uint32_t rx_rearm_fail;
	uint32_t tx_radio_err;
	uint32_t link_err;
	uint8_t last_err_code;
	uint32_t last_err_age_s; /* 0 while no error was reported */
};

/* At most one EVT_LOG per code in this many milliseconds (§2.2). */
#define DIAG_LOG_INTERVAL_MS 10000

void diag_set_boot_reset_cause(uint8_t cause);
void diag_set_radio_state(enum diag_radio_state state);
enum diag_radio_state diag_radio_state(void);

/* Record an error: its sticky flag, its counter, the last-error code and time, and an
 * EVT_LOG (level error) unless one with the same code went out within
 * DIAG_LOG_INTERVAL_MS. `detail` is short ASCII, may be NULL. */
void diag_report(enum diag_code code, const char *detail);

/* Set a sticky flag that has no code of its own (DIAG_FLAG_TX_INTERNAL). */
void diag_set_flag(uint32_t flag);

void diag_snapshot(struct diag_info *out);

/* Encode an EVT_LOG body (§2.2): level, code, then `detail` truncated to fit `cap`.
 * Returns its length. Exposed for the ztests. */
size_t diag_encode_log(uint8_t *out, size_t cap, uint8_t level, uint8_t code, const char *detail);

#endif /* APP_DIAG_H_ */
