/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway commands 0x10..0x15 and 0x17..0x19 (p2p_tower_gateway.md §2.1, #470 link
 * §5.1), routed here by
 * cmd_dispatch.c. TWR_NODE_ADD, TWR_CTR_BLOCK and TWR_QUEUE_PUSH need SET_RADIO_CONFIG first
 * (state CONFIGURED or GATEWAY), TWR_START also a counter block; before that they answer
 * INVALID_STATE. TWR_NODE_REMOVE, TWR_QUEUE_DROP, TWR_NODE_LIST, TWR_EVT_ACK and
 * TWR_TIME_SYNC work in every state; TWR_EVT_ACK is never answered.
 */
#ifndef APP_TWR_CMD_H_
#define APP_TWR_CMD_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* Handle one TWR_* command and send its RSP (none for TWR_EVT_ACK). False when `type` is not
 * one of them. */
bool twr_cmd_handle(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len);

#endif /* APP_TWR_CMD_H_ */
