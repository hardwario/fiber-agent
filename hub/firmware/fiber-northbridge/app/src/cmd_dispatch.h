/* SPDX-License-Identifier: Apache-2.0 */
#ifndef APP_CMD_DISPATCH_H_
#define APP_CMD_DISPATCH_H_

#include <stddef.h>
#include <stdint.h>

struct twr_identity;

/* Entry point from uart_link.c: dispatch one decoded, CRC-valid frame and
 * send back its RSP (and, for TX_CANCEL, a matching EVT_TX_DONE) via
 * uart_link_send(). */
void proto_on_frame(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len);

/* The image's identity as EVT_BOOT and GET_INFO carry it (fw_ver, git_sha, the boot reset
 * cause diag.c holds, build_flags). */
void cmd_identity(struct twr_identity *id);

#endif /* APP_CMD_DISPATCH_H_ */
