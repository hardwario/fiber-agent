/* SPDX-License-Identifier: Apache-2.0
 *
 * Northbridge TOWER gateway firmware (proximos-v2 plan/control/radio/p2p_tower_gateway.md):
 * brings up the radio and the P2P UART link, announces EVT_BOOT, then pumps uart_link_poll()
 * on this thread - the commands run in cmd_dispatch.c / twr_cmd.c, the radio side in
 * twr_gateway.c.
 *
 * USART1 (ttyAMA3-side) is exclusive to the binary protocol; there is no
 * console/shell on it. Debug output (this file's shell commands, LOG_*)
 * goes out over SEGGER RTT instead - see fiber-northbridge/README.md.
 */
#include "diag.h"
#include "proto.h"
#include "uart_link.h"
#if defined(APP_TWR_P0_GATEWAY)
#include "twr_radio.h"
#else
#include "cmd_dispatch.h"
#include "radio_ctl.h"
#include "twr_gateway.h"
#include "twr_link.h"
#endif

#include <stdlib.h>
#include <string.h>
#include <zephyr/kernel.h>
#include <zephyr/device.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/drivers/hwinfo.h>
#include <zephyr/drivers/watchdog.h>
#include <zephyr/shell/shell.h>

#define LOG_LEVEL CONFIG_LOG_DEFAULT_LEVEL
#include <zephyr/logging/log.h>
LOG_MODULE_REGISTER(app);

/* The independent watchdog (northbridge_diagnostics.md §2.3, Hynek Q3: 8 s). Fed from the
 * main loop, and only while no transmission has run far past its time on air
 * (radio_ctl_tx_stuck(): twice the ToA plus 2 s): a radio that stops answering mid-TX (the
 * STICKER's F-P2P-3 wedge) then ends in a reset the host sees as reset_cause "watchdog", not
 * a silent modem. Paused while a debugger halts the core, so a J-Link session does not reset
 * it. */
#define WDT_TIMEOUT_MS 8000

static uint8_t read_and_clear_reset_cause(void)
{
	uint32_t cause = 0;

	/* Passed through as Zephyr's native reset-cause bitmask (truncated
	 * to 8 bits), not remapped to a dedicated wire enum - the spec only
	 * says "reset_cause u8" and multiple bits can be latched at once
	 * (e.g. POR + PIN together on first power-up). Flagged for the
	 * host-side implementer to confirm this interpretation.
	 */
	if (hwinfo_get_reset_cause(&cause) == 0) {
		hwinfo_clear_reset_cause();
	}
	return (uint8_t)cause;
}

#if !defined(APP_TWR_P0_GATEWAY)
/* fw_ver, git_sha, reset_cause: the host checks the image before it configures anything
 * (flag day: no protocol version byte, the gateway is the only image). */
static void send_evt_boot(void)
{
	struct twr_identity id;
	uint8_t body[P2P_EVT_BOOT_BODY_LEN];

	cmd_identity(&id);
	uart_link_send_event(P2P_EVT_BOOT, body, twr_link_enc_boot(&id, body));
}
#else
/* uart_link.c is linked for diag.c's EVT_LOG lines but never started: no commands. */
void proto_on_frame(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len)
{
	ARG_UNUSED(type);
	ARG_UNUSED(seq);
	ARG_UNUSED(body);
	ARG_UNUSED(body_len);
}
#endif /* !APP_TWR_P0_GATEWAY */

static int cmd_rf_diag(const struct shell *sh, size_t argc, char **argv)
{
	ARG_UNUSED(argc);
	ARG_UNUSED(argv);

	const struct device *gpioc = DEVICE_DT_GET(DT_NODELABEL(gpioc));

	if (!device_is_ready(gpioc)) {
		shell_error(sh, "GPIOC not ready");
		return -ENODEV;
	}

	shell_print(sh, "RF switch GPIO state:");
	shell_print(sh, "  PC3 (FE_CTRL1): %d", gpio_pin_get(gpioc, 3));
	shell_print(sh, "  PC4 (FE_CTRL2): %d", gpio_pin_get(gpioc, 4));
	shell_print(sh, "  PC5 (FE_CTRL3): %d", gpio_pin_get(gpioc, 5));

	return 0;
}

SHELL_CMD_REGISTER(rf_diag, NULL, "Show RF switch GPIO states (PC3/PC4/PC5)", cmd_rf_diag);

static int cmd_p2p_status(const struct shell *sh, size_t argc, char **argv)
{
	ARG_UNUSED(argc);
	ARG_UNUSED(argv);

	struct diag_info d;

	diag_snapshot(&d);
	shell_print(sh, "uptime_ms=%llu link_crc_err=%u link_tx_drops=%u",
		    (unsigned long long)k_uptime_get(), uart_link_get_crc_err_count(),
		    uart_link_get_tx_drops());
	shell_print(sh,
		    "diag: radio_state=%u reset_cause=0x%02x err_flags=0x%08x rx_rearm_fail=%u "
		    "tx_radio_err=%u link_err=%u last_err=%u age_s=%u",
		    d.radio_state, d.boot_reset_cause, d.err_flags, d.rx_rearm_fail, d.tx_radio_err,
		    d.link_err, d.last_err_code, d.last_err_age_s);
#if defined(APP_TWR_P0_GATEWAY)
	twr_radio_print_brief(sh);
#else
	struct radio_config cfg;

	if (radio_ctl_get_config(&cfg)) {
		shell_print(sh, "radio: freq=%u sf=%u bw=%u cr=%u preamble=%u tx_power=%d flags=0x%02x",
			    cfg.frequency_hz, cfg.sf, cfg.bw, cfg.cr, cfg.preamble_len,
			    cfg.tx_power_dbm, cfg.flags);
	} else {
		shell_print(sh, "radio: not configured");
	}
	twr_gateway_print_brief(sh);
#endif

	return 0;
}

SHELL_CMD_REGISTER(p2p_status, NULL, "Show P2P link/radio diagnostics", cmd_p2p_status);

/* HIL checks of the 0.2.0 diagnostics over the RTT shell (needs a J-Link, so not reachable
 * from the Hub): `nb_selftest log <code>` reports an error as the firmware would,
 * `nb_selftest wdt` stops feeding the watchdog, which must reset the modem within 8 s. */
static volatile bool selftest_starve_watchdog;

static int cmd_nb_selftest(const struct shell *sh, size_t argc, char **argv)
{
	if (argc == 3 && strcmp(argv[1], "log") == 0) {
		int code = atoi(argv[2]);

		if (code <= DIAG_CODE_NONE || code >= DIAG_CODE_COUNT) {
			shell_error(sh, "code 1..%d", DIAG_CODE_COUNT - 1);
			return -EINVAL;
		}
		diag_report((enum diag_code)code, "selftest");
		shell_print(sh, "reported code %d", code);
		return 0;
	}
	if (argc == 2 && strcmp(argv[1], "wdt") == 0) {
		shell_print(sh, "watchdog no longer fed: reset expected within %d ms", WDT_TIMEOUT_MS);
		selftest_starve_watchdog = true;
		return 0;
	}
	shell_error(sh, "usage: nb_selftest log <code> | nb_selftest wdt");
	return -EINVAL;
}

/* Not in the TOWER P0 gateway: there is no host link there. */
#if defined(APP_P2P_BENCH_RTT_BRIDGE) && !defined(APP_TWR_P0_GATEWAY)

static int hex_nibble(char c)
{
	if (c >= '0' && c <= '9') {
		return c - '0';
	}
	if (c >= 'a' && c <= 'f') {
		return c - 'a' + 10;
	}
	if (c >= 'A' && c <= 'F') {
		return c - 'A' + 10;
	}
	return -1;
}

static int cmd_p2p_inject(const struct shell *sh, size_t argc, char **argv)
{
	if (argc != 2) {
		shell_error(sh, "usage: p2p_inject <hex bytes, no spaces>");
		return -EINVAL;
	}

	const char *hex = argv[1];
	size_t hex_len = strlen(hex);

	if (hex_len == 0 || (hex_len % 2) != 0) {
		shell_error(sh, "hex string must be an even number of digits");
		return -EINVAL;
	}

	uint8_t bytes[512];
	size_t n = hex_len / 2;

	if (n > sizeof(bytes)) {
		shell_error(sh, "too many bytes (max %zu)", sizeof(bytes));
		return -EINVAL;
	}

	for (size_t i = 0; i < n; i++) {
		int hi = hex_nibble(hex[2 * i]);
		int lo = hex_nibble(hex[2 * i + 1]);

		if (hi < 0 || lo < 0) {
			shell_error(sh, "invalid hex digit at position %zu", 2 * i);
			return -EINVAL;
		}
		bytes[i] = (uint8_t)((hi << 4) | lo);
	}

	uart_link_bench_inject(bytes, n);
	shell_print(sh, "OK: injected %zu bytes", n);
	return 0;
}

SHELL_CMD_REGISTER(p2p_inject, NULL,
		    "Bench only: feed hex-encoded raw bytes to the P2P HDLC decoder, "
		    "as if they had just arrived over USART1",
		    cmd_p2p_inject);

#endif /* APP_P2P_BENCH_RTT_BRIDGE && !APP_TWR_P0_GATEWAY */

SHELL_CMD_REGISTER(nb_selftest, NULL, "HIL: report a diag error, or starve the watchdog",
		   cmd_nb_selftest);

static const struct device *const wdt = DEVICE_DT_GET_OR_NULL(DT_NODELABEL(iwdg));
static int wdt_channel = -1;

static void watchdog_start(void)
{
	if (wdt == NULL || !device_is_ready(wdt)) {
		LOG_WRN("no independent watchdog");
		return;
	}
	struct wdt_timeout_cfg cfg = {
		.window = { .min = 0, .max = WDT_TIMEOUT_MS },
		.flags = WDT_FLAG_RESET_SOC,
	};

	wdt_channel = wdt_install_timeout(wdt, &cfg);
	if (wdt_channel < 0 || wdt_setup(wdt, WDT_OPT_PAUSE_HALTED_BY_DBG) != 0) {
		LOG_ERR("watchdog setup failed");
		wdt_channel = -1;
	}
}

#if defined(APP_TWR_P0_GATEWAY)

/* Bench TOWER P0 gateway (twr_radio.c): the radio belongs to the gateway, the host link is not
 * started. The watchdog is kept, gated on twr_radio_tx_stuck() instead of radio_ctl's. */
int main(void)
{
	diag_set_boot_reset_cause(read_and_clear_reset_cause());
	if (twr_radio_init() != 0) {
		LOG_ERR("twr_radio_init failed");
	}
	LOG_INF("TOWER P0 gateway (fw %d.%d.%d-twr-p0, gw 0x%08X, node 0x%08X)", FW_VERSION_MAJOR,
		FW_VERSION_MINOR, FW_VERSION_PATCH, twr_radio_gw_addr(), twr_radio_node_addr());

	watchdog_start();

	while (1) {
		if (wdt_channel >= 0 && !twr_radio_tx_stuck() && !selftest_starve_watchdog) {
			wdt_feed(wdt, wdt_channel);
		}
		k_msleep(1);
	}

	return 0;
}

#else /* !APP_TWR_P0_GATEWAY */

int main(void)
{
	diag_set_boot_reset_cause(read_and_clear_reset_cause());

	/* A radio that fails to come up is reported, not fatal: the link still answers
	 * GET_INFO (radio_state error, err_flags b0) and EVT_LOG code 1, so the host can tell
	 * a broken radio from a dead modem. */
	bool radio_ok = radio_ctl_init() == 0;

	if (!radio_ok) {
		LOG_ERR("radio_ctl_init failed");
	}

	if (uart_link_init() != 0) {
		LOG_ERR("uart_link_init failed");
		return 0;
	}

	send_evt_boot();
	if (!radio_ok) {
		diag_report(DIAG_CODE_RADIO_INIT, "radio init failed");
	}
	LOG_INF("TOWER gateway ready (fw %d.%d.%d, git %08x)", FW_VERSION_MAJOR, FW_VERSION_MINOR,
		FW_VERSION_PATCH, (unsigned int)APP_GIT_SHA);

	watchdog_start();

	while (1) {
		uart_link_poll();
		if (wdt_channel >= 0 && !radio_ctl_tx_stuck() && !selftest_starve_watchdog) {
			wdt_feed(wdt, wdt_channel);
		}
		k_msleep(1);
	}

	return 0;
}

#endif /* APP_TWR_P0_GATEWAY */
