/* SPDX-License-Identifier: Apache-2.0 */
#ifndef APP_UART_PROTO_H_
#define APP_UART_PROTO_H_

#include <stddef.h>

/* Bring up UART RX interrupt + ring buffer. Returns 0 on success. */
int uart_proto_init(void);

/* Drain the ring buffer, assemble lines (terminator '\n', '\r' ignored),
 * and invoke dispatch_line() for each complete line. Non-blocking.
 * Lines longer than RX_LINE_BUFFER_SIZE-1 chars are dropped with an
 * "ERR: line too long\r\n" reply.
 */
void uart_proto_poll(void);

/* Synchronous TX of a NUL-terminated string. */
void uart_proto_write(const char *s);

/* Synchronous TX of len bytes. */
void uart_proto_write_bytes(const char *buf, size_t len);

#endif /* APP_UART_PROTO_H_ */
