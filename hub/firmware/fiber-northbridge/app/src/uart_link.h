/* SPDX-License-Identifier: Apache-2.0 */
#ifndef APP_UART_LINK_H_
#define APP_UART_LINK_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* Bring up the dedicated P2P UART (ttyAMA3-side link, USART1/PA9-PA10 on
 * this module) RX/TX interrupts + ring buffers. Returns 0 on success. */
int uart_link_init(void);

/* Drain the RX ring buffer through the HDLC decoder and dispatch any complete,
 * valid frames via proto_on_frame() (cmd_dispatch.c). Non-blocking. */
void uart_link_poll(void);

/* Encode one HDLC frame and queue it for the UART interrupt; returns once it is queued.
 * seq is the incoming command's for an RSP, the delivery seq for a ring event. Threads only (never an ISR). When the TX ring
 * has no room the caller waits up to 50 ms (preemptible) or 10 ms (cooperative thread),
 * then the frame is dropped and counted (uart_link_get_tx_drops()). */
void uart_link_send(uint8_t type, uint8_t seq, const uint8_t *body, size_t body_len);

/* The same for a best-effort EVT_* (EVT_BOOT, EVT_RX, EVT_TX_DONE, EVT_LOG,
 * EVT_TWR_CTR_LOW): it carries the current delivery seq without advancing it (#470 link
 * §4.1). The ring events (EVT_TWR_UPLINK, EVT_TWR_TX) go through uart_link_send() with
 * their own delivery seq instead. */
void uart_link_send_event(uint8_t type, const uint8_t *body, size_t body_len);

/* The newest delivery seq the event ring handed out (0 at boot): what the best-effort
 * events carry from now on. Any thread. */
void uart_link_set_event_seq(uint8_t seq);

/* Wait until everything queued so far has left the UART (TX ring empty, last stop bit
 * sent), at most timeout_ms. True when it has. Threads only. Used before a reboot. */
bool uart_link_tx_drain(int32_t timeout_ms);

/* Count of malformed frames (CRC mismatch, too-short, or oversized) seen
 * since boot - surfaced in GET_INFO as link_crc_err. */
uint32_t uart_link_get_crc_err_count(void);

/* Frames dropped for want of room on the link since boot (GET_INFO link_err). */
uint32_t uart_link_get_tx_drops(void);

#if defined(APP_P2P_BENCH_RTT_BRIDGE)
/* Bench only (-DBENCH_RTT_BRIDGE=ON): feed raw bytes to the HDLC decoder as
 * if they had just arrived over USART1 - the shell's p2p_inject command is
 * the only caller. Lets a host drive the protocol via rttt's send_command/
 * read_log tools (text in, text out) instead of a raw byte-stream channel
 * rttt has no tool for. */
void uart_link_bench_inject(const uint8_t *bytes, size_t len);
#endif

#endif /* APP_UART_LINK_H_ */
