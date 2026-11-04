/* SPDX-License-Identifier: Apache-2.0
 *
 * Answers in the ACK (#470 §13.5, N1 part A): the CTRL TLV tail the gateway appends to the
 * ACK of a confirmed 0x91 uplink that asks for LinkCheckReq (0x10) and/or TimeReq (0x20).
 *
 *   ACK plaintext: acked_counter(4) | rssi i8 | flags (b0 PENDING, b1 CTRL) [| TLV...]
 *   TLV: id(1) | len(1) | body, to the end of the plaintext (the 0x91 codec, §8.2)
 *   0x10 LinkCheckAns: rssi i8 | snr i8 dB | margin i8 dB | gw_count u8 (= 1)
 *   0x20 TimeAns:      unix u32 LE | frac (1/256 s) | req_counter u32 LE
 *
 * LinkCheckAns uses the central's formula (control-radio LinkCheckAns::from_reception): SNR
 * and the margin over the SF demodulation floor (-7.5 dB at SF7, 2.5 dB lower per step), both
 * rounded down. TimeAns is the wall time at the end of the received uplink (its RxDone stamp)
 * through the TWR_TIME_SYNC anchor; req_counter is that uplink's counter. Without an anchor
 * TimeAns is left out. The tail order is fixed: LinkCheckAns, then TimeAns.
 *
 * Plain C, no Zephyr (tests/tower_gateway).
 */
#ifndef APP_TWR_ACKCTRL_H_
#define APP_TWR_ACKCTRL_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define TWR_CTRL_ENVELOPE        0x91u
#define TWR_CTRL_LINK_CHECK      0x10u
#define TWR_CTRL_TIME            0x20u
#define TWR_CTRL_LINK_CHECK_LEN  4u
#define TWR_CTRL_TIME_LEN        9u
#define TWR_LINK_CHECK_GW_COUNT  1u /* only the home gateway ACKs (P7 limit) */

/* Both answers: 2 + 4 + 2 + 9 = 17 B. */
#define TWR_ACKCTRL_TAIL_MAX (2u + TWR_CTRL_LINK_CHECK_LEN + 2u + TWR_CTRL_TIME_LEN)

/* The wall clock anchor of TWR_TIME_SYNC: at NB uptime now_ms the wall time was unix_ms. */
struct twr_time_anchor {
	bool valid;
	uint64_t now_ms;
	uint64_t unix_ms;
};

/* What the gateway knows about the received uplink. */
struct twr_ackctrl_in {
	int rssi_dbm;
	int8_t snr_q;     /* quarter dB */
	uint8_t sf;       /* 7..12 */
	uint64_t t_rx_ms; /* RxDone (end of the received frame), NB uptime */
	uint32_t counter; /* the uplink's counter: TimeAns req_counter */
	const struct twr_time_anchor *anchor;
};

struct twr_ackctrl_res {
	bool lc;             /* LinkCheckAns in the tail */
	bool time;           /* TimeAns in the tail */
	bool time_no_anchor; /* TimeReq asked, no anchor (or an unmappable time): no TimeAns */
};

/* Margin in dB over the SF floor from SNR in quarter dB, rounded down, clamped into i8. */
int8_t twr_ackctrl_margin(int8_t snr_q, uint8_t sf);

/* SNR in quarter dB to whole dB, rounded down. */
int8_t twr_ackctrl_snr_db(int8_t snr_q);

/* Wall time of NB uptime t_ms through the anchor: false without an anchor or when the result
 * does not fit unix u32 (before 1970 or after 2106). */
bool twr_ackctrl_wall(const struct twr_time_anchor *a, uint64_t t_ms, uint32_t *unix_s,
		      uint8_t *frac);

/* Which of 0x10 / 0x20 a confirmed uplink's plaintext asks for. False when it is not a 0x91
 * envelope or its TLV list is truncated (a list cut short is not trusted, as on the central). */
bool twr_ackctrl_requests(const uint8_t *pt, size_t len, bool *link_check, bool *time);

/* Build the tail for the uplink plaintext `pt` into `tail` (TWR_ACKCTRL_TAIL_MAX). Returns
 * its length, 0 when there is nothing to answer. */
size_t twr_ackctrl_build(const uint8_t *pt, size_t len, const struct twr_ackctrl_in *in,
			 uint8_t tail[TWR_ACKCTRL_TAIL_MAX], struct twr_ackctrl_res *res);

/* TLV walk of an ACK tail (the receiver's side, for tests and tools): the next TLV at *off.
 * False at the end or on a truncated TLV. */
bool twr_ackctrl_next(const uint8_t *tail, size_t len, size_t *off, uint8_t *id,
		      const uint8_t **body, uint8_t *body_len);

#endif /* APP_TWR_ACKCTRL_H_ */
