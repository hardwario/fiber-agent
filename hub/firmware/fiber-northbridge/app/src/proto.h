/* SPDX-License-Identifier: Apache-2.0
 *
 * Northbridge <-> Hub link constants. Normative: sticker-firmware
 * doc/plan/470 - Northbridge-Hub link protocol.md (framing §2, flag day §3, GET_INFO §3.1,
 * messages §5, states §6.1), to be folded into proximos-v2
 * plan/control/radio/p2p_tower_gateway.md.
 *
 * Flag day (0.3.0): this image is the TOWER gateway and is flashed together with the Hub
 * software that speaks it. There is no proto_version byte, no negotiation and no rollback to
 * the old P2P modem (link §3); fw_ver and git_sha are for diagnostics only.
 */
#ifndef APP_PROTO_H_
#define APP_PROTO_H_

#include <stdint.h>

#define FW_VERSION_MAJOR 0
#define FW_VERSION_MINOR 4
#define FW_VERSION_PATCH 2

/* The first 4 bytes of the last commit that touched this firmware, display order
 * (app/CMakeLists.txt passes it; 0 when git was not available). */
#ifndef APP_GIT_SHA
#define APP_GIT_SHA 0x00000000u
#endif

/* HDLC framing */
#define HDLC_FLAG    0x7E
#define HDLC_ESC     0x7D
#define HDLC_ESC_XOR 0x20

/* type(1) + seq(1) + body + crc16(2); body itself is capped separately */
#define P2P_MAX_BODY_LEN  280
#define P2P_MAX_FRAME_LEN (1 + 1 + P2P_MAX_BODY_LEN + 2)
#define P2P_MAX_RADIO_FRAME_LEN 255

/* Worst case: every unescaped byte - type, seq, body and both CRC bytes - needs 2 output
 * bytes, plus the two framing flags: 570 B. */
#define P2P_MAX_ENCODED_LEN (2 + 2 * P2P_MAX_FRAME_LEN)

#define UART_RX_RING_SIZE 512

/* Host -> modem commands, each answered by RSP (type 0x40|cmd) */
#define P2P_CMD_GET_INFO         0x01
#define P2P_CMD_SET_RADIO_CONFIG 0x02
/* 0x03 RX_START is retired (TWR_START enters receive); it answers UNSUPPORTED. */
#define P2P_CMD_RX_STOP          0x04
#define P2P_CMD_TX_SCHEDULE      0x05
#define P2P_CMD_TX_CANCEL        0x06
/* Reboot into the STM32 ROM bootloader for an update (northbridge_update.md §10). */
#define P2P_CMD_ENTER_BOOTLOADER 0x07

/* TOWER gateway (p2p_tower_gateway.md §2.1) */
#define P2P_CMD_TWR_START        0x10
#define P2P_CMD_TWR_NODE_ADD     0x11
#define P2P_CMD_TWR_NODE_REMOVE  0x12
#define P2P_CMD_TWR_CTR_BLOCK    0x13
#define P2P_CMD_TWR_QUEUE_PUSH   0x14
#define P2P_CMD_TWR_QUEUE_DROP   0x15
/* 0x16 TWR_GET_STATS is retired (its counters are in GET_INFO); it answers UNSUPPORTED. */
#define P2P_CMD_TWR_NODE_LIST    0x17
/* Cumulative ack of the ring events (link §4.1); never answered (no RSP). */
#define P2P_CMD_TWR_EVT_ACK      0x18
/* Wall clock anchor for TimeAns in the ACK (link §5.1, #470 §13.5); status-only RSP. */
#define P2P_CMD_TWR_TIME_SYNC    0x19

#define P2P_RSP_BIT 0x40u
#define P2P_RSP(cmd) ((uint8_t)(P2P_RSP_BIT | (cmd)))

/* Modem -> host events (unsolicited) */
#define P2P_EVT_BOOT    0x80
#define P2P_EVT_RX      0x81
#define P2P_EVT_TX_DONE 0x82
#define P2P_EVT_LOG     0x83

/* TOWER gateway (p2p_tower_gateway.md §2.2) */
#define P2P_EVT_TWR_UPLINK  0x84
#define P2P_EVT_TWR_TX      0x85
#define P2P_EVT_TWR_CTR_LOW 0x86

/* RSP status byte (first byte of every RSP body) */
enum p2p_status {
	P2P_STATUS_OK            = 0,
	P2P_STATUS_BAD_PARAM     = 1,
	P2P_STATUS_BUSY          = 2,
	P2P_STATUS_INVALID_STATE = 3,
	P2P_STATUS_LATE          = 4,
	P2P_STATUS_INTERNAL      = 5,
	P2P_STATUS_NO_SPACE      = 6, /* peer table / queue pool full */
	P2P_STATUS_NOT_FOUND     = 7, /* no such peer / queue item */
	P2P_STATUS_UNSUPPORTED   = 8, /* unknown command */
};

/* The Northbridge's state (GET_INFO): BOOT -> SET_RADIO_CONFIG -> CONFIGURED -> TWR_CTR_BLOCK +
 * TWR_START -> GATEWAY; RX_STOP goes back to CONFIGURED and keeps the tables. */
enum p2p_nb_state {
	P2P_NB_BOOT       = 0,
	P2P_NB_CONFIGURED = 1,
	P2P_NB_GATEWAY    = 2,
};

/* GET_INFO build_flags */
#define P2P_BUILD_BENCH_RTT_BRIDGE (1u << 0)
#define P2P_BUILD_DEBUG            (1u << 1)
#define P2P_BUILD_RX_INJECT        (1u << 2) /* bench shell `p2p rx_inject` (NB_TWR_RX_INJECT) */

/* GET_INFO radio modulation */
#define P2P_MODULATION_LORA 0

/* EVT_TX_DONE status byte */
enum p2p_tx_done_status {
	P2P_TX_DONE_OK        = 0,
	P2P_TX_DONE_LATE      = 1,
	P2P_TX_DONE_RADIO_ERR = 2,
	P2P_TX_DONE_CANCELLED = 3,
};

/* SET_RADIO_CONFIG wire bandwidth codes (do NOT match the Zephyr
 * lora_signal_bandwidth enum, which is keyed in kHz - see twr_gateway.c) */
#define P2P_BW_125K 0
#define P2P_BW_250K 1
#define P2P_BW_500K 2

/* SET_RADIO_CONFIG flags bitfield */
#define P2P_RADIO_FLAG_PUBLIC_SYNCWORD (1u << 0)
#define P2P_RADIO_FLAG_IQ_INVERTED     (1u << 1)

/* TX_SCHEDULE: t_ms == 0 means "transmit immediately" */
#define P2P_TX_IMMEDIATE 0

/* Reject a schedule closer than this to "now" as LATE rather than racing
 * the setup time - HIL-tunable, see docs/problems.md. */
#define P2P_TX_MIN_LEAD_MS 20

/* Wire body sizes (little-endian on this link; the radio frame itself is
 * carried as an opaque byte string). */
#define P2P_RADIO_CONFIG_BODY_LEN 11 /* freq(4) sf(1) bw(1) cr(1) preamble(2) tx_power(1) flags(1) */
#define P2P_TX_SCHEDULE_HDR_LEN   10 /* tx_id(1) t_ms(8) len(1), followed by frame[len] */
#define P2P_TX_CANCEL_BODY_LEN    1  /* tx_id(1) */
#define P2P_EVT_BOOT_BODY_LEN     8  /* fw_ver(3) git_sha(4) reset_cause(1) */
#define P2P_EVT_RX_HDR_LEN        13 /* t_ms(8) rssi(2) snr(1) reserved(1) len(1), followed by frame[len] */
#define P2P_EVT_TX_DONE_BODY_LEN  10 /* tx_id(1) status(1) t_actual_ms(8) */

/* ENTER_BOOTLOADER: exactly 4E 42 42 4C ("NBBL"), i.e. this u32 little-endian; anything else
 * is BAD_PARAM. The same value is the boot request word nbboot reads (nb_app_valid.h). */
#define P2P_ENTER_BOOTLOADER_BODY_LEN 4
#define P2P_ENTER_BOOTLOADER_MAGIC    0x4C42424Eu

/* GET_INFO RSP payload after the status byte, see twr_link.h for the layout: 188 B since
 * 0.4.0 (app_image_len, app_crc32), 180 B in 0.3.2 (ack_ctrl_sent, ack_time_no_anchor),
 * 172 B in 0.3.1, 171 B before. */
#define P2P_GET_INFO_LEN 188

/* GET_INFO counters, 25 x u32 in this order. */
enum p2p_info_ctr {
	P2P_INFO_RX_TOTAL,
	P2P_INFO_RX_FRESH,
	P2P_INFO_RX_DUP,
	P2P_INFO_RX_REPLAY,
	P2P_INFO_RX_MIC_FAIL,
	P2P_INFO_RX_UNKNOWN_SRC,
	P2P_INFO_RX_JOIN_FWD,
	P2P_INFO_RX_NOT_FOR_US,
	P2P_INFO_RX_BAD_HDR,
	P2P_INFO_RADIO_CRC_ERR,
	P2P_INFO_RADIO_HDR_ERR,
	P2P_INFO_RX_OVERRUN,
	P2P_INFO_RX_REARM_FAIL,
	P2P_INFO_TX_TOTAL,
	P2P_INFO_TX_RADIO_ERR,
	P2P_INFO_ACKS_SENT,
	P2P_INFO_ACKS_LATE,
	P2P_INFO_ACKS_SUPPRESSED,
	P2P_INFO_DL_SENT,
	P2P_INFO_DL_DELIVERED,
	P2P_INFO_DL_NOT_DELIVERED,
	P2P_INFO_DL_EXPIRED,
	P2P_INFO_LINK_CRC_ERR,
	P2P_INFO_LINK_ERR,
	P2P_INFO_EVT_RESENT,
	P2P_INFO_CTR_COUNT, /* 25 */
};

/* TOWER gateway bodies (p2p_tower_gateway.md §2), see twr_link.h for the field layouts. */
#define P2P_TWR_START_BODY_LEN       7  /* net_id(4) turnaround_ms(1) dl_gap_ms(1) flags(1) */
#define P2P_TWR_NODE_ADD_BODY_LEN    25 /* addr(4) session_key(16) last_seen(4) flags(1) */
#define P2P_TWR_NODE_REMOVE_BODY_LEN 4  /* addr(4) */
#define P2P_TWR_CTR_BLOCK_BODY_LEN   8  /* first(4) last(4) */
#define P2P_TWR_QUEUE_PUSH_HDR_LEN   10 /* addr(4) item(2) ttl_s(2) flags(1) len(1), then pt */
#define P2P_TWR_QUEUE_DROP_BODY_LEN  6  /* addr(4) item(2) */
#define P2P_TWR_NODE_LIST_BODY_LEN   2  /* start(2) */
#define P2P_TWR_NODE_LIST_HDR_LEN    5  /* total(2) next(2) n(1), then n x entry */
#define P2P_TWR_NODE_LIST_ENTRY_LEN  9  /* addr(4) last_seen(4) flags(1) */
#define P2P_TWR_NODE_LIST_MAX        24 /* entries per RSP */
#define P2P_TWR_NODE_LIST_END        0xFFFFu
#define P2P_EVT_TWR_UPLINK_HDR_LEN   22 /* t_ms(8) rssi(2) snr_q(1) frame_flags(1) addr(4)
					 * counter(4) ack(1) len(1), then plaintext */
#define P2P_EVT_TWR_TX_BODY_LEN      16 /* addr(4) item(2) outcome(1) gw_counter(4)
					 * node_ack_counter(4) ack_rssi(1) */
#define P2P_EVT_TWR_CTR_LOW_BODY_LEN 8  /* next(4) last(4) */
#define P2P_TWR_EVT_ACK_BODY_LEN     1  /* seq(1) */
#define P2P_TWR_TIME_SYNC_BODY_LEN   16 /* now_ms(8) unix_ms(8) */

/* TWR_START flags */
#define P2P_TWR_START_ACK_ENABLED (1u << 0)
/* TWR_NODE_ADD flags */
#define P2P_TWR_NODE_HOME         (1u << 0)
/* Restore merge (link §4.2): with the same session key the peer keeps max(own, sent)
 * last_seen. Never stored with the peer (TWR_NODE_LIST shows HOME only). */
#define P2P_TWR_NODE_KEEP_NEWER   (1u << 1)
/* TWR_QUEUE_PUSH flags */
#define P2P_TWR_QUEUE_CONFIRMED   (1u << 0)

/* TWR_START turnaround_ms / dl_gap_ms range */
#define P2P_TWR_TIMING_MIN_MS 20
#define P2P_TWR_TIMING_MAX_MS 60

/* TWR_QUEUE_PUSH plaintext cap: 14 B header + 74 B + 8 B tag = the TOWER 96 B frame, one MTU
 * on both PHY profiles (sticker-firmware #486 U-1, 0.4.1; was 78 B / 100 B, #470 D14). */
#define P2P_TWR_DL_MAX_PT 74

/* EVT_TWR_UPLINK ack field */
enum p2p_twr_ack {
	P2P_TWR_ACK_NONE       = 0, /* not confirmed (or this gateway does not ACK the peer) */
	P2P_TWR_ACK_SENT       = 1,
	P2P_TWR_ACK_PENDING    = 2, /* ACK sent with PENDING */
	P2P_TWR_ACK_LATE       = 3, /* ACK sent past the node's window, or not on air (radio error) */
	P2P_TWR_ACK_NO_COUNTER = 4, /* suppressed: no gateway TX counter left */
	/* Ring full (link §4.1). Never emitted by this image: the frame is dropped whole, so
	 * there is no ring slot for an event that would carry it (acks_suppressed counts it). */
	P2P_TWR_ACK_SUPPRESSED = 5,
};
/* EVT_TWR_UPLINK ack flag bits (0.3.2, #470 §13.5), OR-ed onto the code above, which keeps
 * bits 0..5 (P2P_TWR_ACK_CODE_MASK). b7: the sealed ACK carried a CTRL tail; b6: that tail
 * holds a TimeAns (0x20). LinkCheck + Time without an anchor: b7 set, b6 clear. */
#define P2P_TWR_ACK_CTRL      0x80u
#define P2P_TWR_ACK_TIME      0x40u
#define P2P_TWR_ACK_CODE_MASK 0x3Fu
#define P2P_TWR_ACK_FLAGS     (P2P_TWR_ACK_CTRL | P2P_TWR_ACK_TIME)

/* EVT_TWR_TX outcome */
enum p2p_twr_outcome {
	P2P_TWR_TX_DELIVERED     = 0,
	P2P_TWR_TX_SENT          = 1,
	P2P_TWR_TX_NOT_DELIVERED = 2,
	P2P_TWR_TX_EXPIRED       = 3,
	P2P_TWR_TX_RADIO_ERR     = 4,
	P2P_TWR_TX_NO_COUNTER    = 5,
};

/* EVT_TWR_TX ack_rssi when there is none */
#define P2P_TWR_RSSI_NONE 0x7F

#endif /* APP_PROTO_H_ */
