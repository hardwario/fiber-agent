/* SPDX-License-Identifier: Apache-2.0
 *
 * TOWER gateway peer table: addr -> session key, last_seen, flags, in
 * RAM only (#470 D15, p2p_tower_gateway.md §2.1 TWR_NODE_ADD / TWR_NODE_REMOVE). The central
 * restores it after every EVT_BOOT with each peer's exact last_seen, so there is no replay
 * window across a Northbridge reboot. Plain C, no Zephyr: tests/tower_gateway runs it on
 * native_sim.
 *
 * A compact array searched linearly: 256 compares cost a few microseconds at 48 MHz, far
 * inside the 20 ms ACK turnaround, and removal swaps the last entry in (NODE_LIST order is
 * for diagnostics only). The key is kept raw (16 B) and expanded per frame; 256 expanded
 * AES keys would take 45 KB.
 */
#ifndef APP_TWR_PEERS_H_
#define APP_TWR_PEERS_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "twr_crypto.h"
#include "twr_frame.h"

#define TWR_PEERS_MAX 256u

struct twr_peer {
	uint32_t addr;
	uint32_t last_seen; /* highest accepted counter from this node; 0 = none yet */
	uint8_t flags;      /* P2P_TWR_NODE_* */
	uint8_t key[TWR_AES_KEY_LEN];
};

struct twr_peers {
	uint16_t n;
	struct twr_peer p[TWR_PEERS_MAX];
};

enum twr_peers_res {
	TWR_PEERS_ADDED,
	TWR_PEERS_REPLACED,
	TWR_PEERS_FULL,
};

void twr_peers_init(struct twr_peers *t);

struct twr_peer *twr_peers_find(struct twr_peers *t, uint32_t addr);

/* Insert, or replace an existing addr's key, last_seen and flags. */
enum twr_peers_res twr_peers_upsert(struct twr_peers *t, uint32_t addr,
				    const uint8_t key[TWR_AES_KEY_LEN], uint32_t last_seen,
				    uint8_t flags);

/* Remove addr and wipe its key. False when absent. */
bool twr_peers_remove(struct twr_peers *t, uint32_t addr);

/* The #470 §4 replay rule for an authenticated frame: counter 0 and counter < last_seen are
 * replays, == last_seen is a retransmit, > last_seen is fresh. Records nothing. */
enum twr_replay twr_peer_classify(const struct twr_peer *p, uint32_t counter);

#endif /* APP_TWR_PEERS_H_ */
