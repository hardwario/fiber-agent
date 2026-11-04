/* SPDX-License-Identifier: Apache-2.0
 *
 * AES-128 (encrypt direction only) and AES-128-CCM with the TOWER radio's fixed parameters:
 * 13-byte nonce (L = 2), 8-byte tag (M = 8). Plain C with no Zephyr dependency, so the
 * native_sim ztests (tests/tower_codec) link the same file the firmware runs.
 *
 * Written from FIPS-197 (AES) and RFC 3610 / NIST SP 800-38C (CCM); the S-box is the
 * FIPS-197 §5.1.1 table. Bench-only today (TOWER_P0_GATEWAY), no constant-time claims
 * beyond the tag comparison.
 */
#ifndef APP_TWR_CRYPTO_H_
#define APP_TWR_CRYPTO_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define TWR_AES_BLOCK_LEN  16
#define TWR_AES_KEY_LEN    16
#define TWR_CCM_NONCE_LEN  13
#define TWR_CCM_TAG_LEN    8
/* The CBC-MAC length prefix used here is the 2-byte form, 0 < l(a) < 2^16 - 2^8. */
#define TWR_CCM_MAX_AAD    0xFEFFu

/* Expanded AES-128 key: 11 round keys. Set up once per key with twr_aes128_init(). */
struct twr_aes128 {
	uint8_t rk[11 * TWR_AES_BLOCK_LEN];
};

void twr_aes128_init(struct twr_aes128 *ctx, const uint8_t key[TWR_AES_KEY_LEN]);

/* One ECB block; `in` and `out` may alias. */
void twr_aes128_encrypt(const struct twr_aes128 *ctx, const uint8_t in[TWR_AES_BLOCK_LEN],
			uint8_t out[TWR_AES_BLOCK_LEN]);

/* Encrypt `data` (len bytes) in place and write the 8-byte tag. `aad` is authenticated but
 * not encrypted. Returns false only for an AAD or payload the 2-byte encodings cannot carry. */
bool twr_ccm_seal(const struct twr_aes128 *ctx, const uint8_t nonce[TWR_CCM_NONCE_LEN],
		  const uint8_t *aad, size_t aad_len, uint8_t *data, size_t len,
		  uint8_t tag[TWR_CCM_TAG_LEN]);

/* Decrypt `data` in place and verify `tag`. On false (authentication failure) `data` is
 * wiped to zeros so a caller cannot act on unauthenticated plaintext. */
bool twr_ccm_open(const struct twr_aes128 *ctx, const uint8_t nonce[TWR_CCM_NONCE_LEN],
		  const uint8_t *aad, size_t aad_len, uint8_t *data, size_t len,
		  const uint8_t tag[TWR_CCM_TAG_LEN]);

#endif /* APP_TWR_CRYPTO_H_ */
