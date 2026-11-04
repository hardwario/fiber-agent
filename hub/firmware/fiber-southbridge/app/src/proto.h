/* SPDX-License-Identifier: Apache-2.0 */
#ifndef APP_PROTO_H_
#define APP_PROTO_H_

/* Both come from app/CMakeLists.txt; the fallbacks are for builds that do not set them
 * (the native_sim tests). The Hub parses this string: plan/system/southbridge_update.md §3.1.
 */
#ifndef SB_FW_VERSION_STR
#define SB_FW_VERSION_STR     "0.0.0"
#endif
#ifndef SB_GIT_SHA_STR
#define SB_GIT_SHA_STR        "00000000"
#endif

#define FIRMWARE_VERSION      "south-zephyr " SB_FW_VERSION_STR " g" SB_GIT_SHA_STR

#define RX_LINE_BUFFER_SIZE   32
#define UART_RX_RING_SIZE     512
#define BRIGHT_DEBOUNCE_MS    50

#define LED_COUNT             8

/* LED color encoding (matches legacy firmware) */
#define LED_COLOR_OFF     0
#define LED_COLOR_GREEN   1
#define LED_COLOR_RED     2
#define LED_COLOR_YELLOW  3   /* green + red simultaneously */

/* PWR LED color encoding */
#define PWR_COLOR_OFF     0
#define PWR_COLOR_GREEN   1
#define PWR_COLOR_YELLOW  2
#define PWR_COLOR_LIME    3   /* green + yellow simultaneously */

/* Blink patterns */
#define BLINK_STEADY      0
#define BLINK_SLOW        1   /* 500 ms on / 500 ms off */
#define BLINK_FAST        2   /* 250 ms on / 250 ms off */

#endif /* APP_PROTO_H_ */
