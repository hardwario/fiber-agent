/* SPDX-License-Identifier: Apache-2.0
 *
 * AES-128 encryption and AES-128-CCM (M = 8, L = 2), see twr_crypto.h.
 *
 * AES state layout follows FIPS-197 §3.4: byte i of a block is state[i % 4][i / 4], so the
 * 16 bytes are four 4-byte columns. Byte-oriented on purpose: 256 B of S-box and no T-tables,
 * ~1 KB of flash, fast enough for a 96 B frame well inside the ACK turnaround.
 */
#include "twr_crypto.h"

#include <string.h>

/* FIPS-197 §5.1.1, Figure 7 (multiplicative inverse in GF(2^8) followed by the affine map). */
static const uint8_t sbox[256] = {
	0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
	0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
	0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
	0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
	0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
	0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
	0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
	0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
	0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
	0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
	0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
	0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
	0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
	0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
	0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
	0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
};

/* Round constants Rcon[i] (FIPS-197 §5.2), x^(i-1) in GF(2^8), for AES-128's 10 rounds. */
static const uint8_t rcon[10] = { 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36 };

/* Multiplication by x in GF(2^8) modulo x^8 + x^4 + x^3 + x + 1 (FIPS-197 §4.2.1). */
static inline uint8_t xtime(uint8_t b)
{
	return (uint8_t)((b << 1) ^ (0x1b & (uint8_t)-(b >> 7)));
}

void twr_aes128_init(struct twr_aes128 *ctx, const uint8_t key[TWR_AES_KEY_LEN])
{
	uint8_t *w = ctx->rk;

	/* KeyExpansion (FIPS-197 §5.2), Nk = 4, Nr = 10: word i = 4 bytes at w[4 * i]. */
	memcpy(w, key, TWR_AES_KEY_LEN);
	for (size_t i = 4; i < 44; i++) {
		uint8_t t[4];

		memcpy(t, &w[4 * (i - 1)], 4);
		if ((i % 4) == 0) {
			/* SubWord(RotWord(t)) xor Rcon */
			uint8_t t0 = t[0];

			t[0] = (uint8_t)(sbox[t[1]] ^ rcon[i / 4 - 1]);
			t[1] = sbox[t[2]];
			t[2] = sbox[t[3]];
			t[3] = sbox[t0];
		}
		for (size_t j = 0; j < 4; j++) {
			w[4 * i + j] = (uint8_t)(w[4 * (i - 4) + j] ^ t[j]);
		}
	}
}

static void add_round_key(uint8_t s[16], const uint8_t *rk)
{
	for (size_t i = 0; i < 16; i++) {
		s[i] ^= rk[i];
	}
}

/* SubBytes and ShiftRows together: row r (bytes r, r+4, r+8, r+12) rotates left by r. */
static void sub_shift(uint8_t s[16])
{
	uint8_t t;

	s[0] = sbox[s[0]];
	s[4] = sbox[s[4]];
	s[8] = sbox[s[8]];
	s[12] = sbox[s[12]];

	t = s[1];
	s[1] = sbox[s[5]];
	s[5] = sbox[s[9]];
	s[9] = sbox[s[13]];
	s[13] = sbox[t];

	t = s[2];
	s[2] = sbox[s[10]];
	s[10] = sbox[t];
	t = s[6];
	s[6] = sbox[s[14]];
	s[14] = sbox[t];

	t = s[15];
	s[15] = sbox[s[11]];
	s[11] = sbox[s[7]];
	s[7] = sbox[s[3]];
	s[3] = sbox[t];
}

/* MixColumns (FIPS-197 §5.1.3): each column times {03}x^3 + {01}x^2 + {01}x + {02}. */
static void mix_columns(uint8_t s[16])
{
	for (size_t c = 0; c < 16; c += 4) {
		uint8_t a0 = s[c], a1 = s[c + 1], a2 = s[c + 2], a3 = s[c + 3];
		uint8_t all = (uint8_t)(a0 ^ a1 ^ a2 ^ a3);

		s[c] = (uint8_t)(a0 ^ all ^ xtime((uint8_t)(a0 ^ a1)));
		s[c + 1] = (uint8_t)(a1 ^ all ^ xtime((uint8_t)(a1 ^ a2)));
		s[c + 2] = (uint8_t)(a2 ^ all ^ xtime((uint8_t)(a2 ^ a3)));
		s[c + 3] = (uint8_t)(a3 ^ all ^ xtime((uint8_t)(a3 ^ a0)));
	}
}

void twr_aes128_encrypt(const struct twr_aes128 *ctx, const uint8_t in[TWR_AES_BLOCK_LEN],
			uint8_t out[TWR_AES_BLOCK_LEN])
{
	uint8_t s[16];

	memcpy(s, in, sizeof(s));
	add_round_key(s, &ctx->rk[0]);
	for (size_t round = 1; round < 10; round++) {
		sub_shift(s);
		mix_columns(s);
		add_round_key(s, &ctx->rk[16 * round]);
	}
	sub_shift(s);
	add_round_key(s, &ctx->rk[160]);
	memcpy(out, s, sizeof(s));
}

/* ---- CCM (RFC 3610 §2) --------------------------------------------------------------- */

#define CCM_L 2u
/* B0 flags without Adata: 8 * ((M - 2) / 2) + (L - 1) */
#define CCM_B0_FLAGS ((uint8_t)((((TWR_CCM_TAG_LEN - 2u) / 2u) << 3) | (CCM_L - 1u)))
#define CCM_B0_ADATA 0x40u
/* A_i flags: L - 1 */
#define CCM_A_FLAGS ((uint8_t)(CCM_L - 1u))

/* Running CBC-MAC: `pos` bytes of the current block have been xored into x. */
struct cbc_mac {
	const struct twr_aes128 *aes;
	uint8_t x[16];
	size_t pos;
};

static void mac_absorb(struct cbc_mac *m, const uint8_t *p, size_t n)
{
	for (size_t i = 0; i < n; i++) {
		m->x[m->pos++] ^= p[i];
		if (m->pos == 16) {
			twr_aes128_encrypt(m->aes, m->x, m->x);
			m->pos = 0;
		}
	}
}

/* Zero-pad to the block boundary: padding with zeros leaves x unchanged, so only the
 * pending encryption has to happen. */
static void mac_pad(struct cbc_mac *m)
{
	if (m->pos != 0) {
		twr_aes128_encrypt(m->aes, m->x, m->x);
		m->pos = 0;
	}
}

static void ccm_mac(const struct twr_aes128 *ctx, const uint8_t nonce[TWR_CCM_NONCE_LEN],
		    const uint8_t *aad, size_t aad_len, const uint8_t *msg, size_t len,
		    uint8_t t[16])
{
	struct cbc_mac m = { .aes = ctx, .pos = 0 };
	uint8_t b0[16];

	memset(m.x, 0, sizeof(m.x));
	b0[0] = (uint8_t)(CCM_B0_FLAGS | (aad_len > 0 ? CCM_B0_ADATA : 0u));
	memcpy(&b0[1], nonce, TWR_CCM_NONCE_LEN);
	b0[14] = (uint8_t)(len >> 8);
	b0[15] = (uint8_t)len;
	mac_absorb(&m, b0, sizeof(b0));

	if (aad_len > 0) {
		uint8_t la[2] = { (uint8_t)(aad_len >> 8), (uint8_t)aad_len };

		mac_absorb(&m, la, sizeof(la));
		mac_absorb(&m, aad, aad_len);
		mac_pad(&m);
	}
	mac_absorb(&m, msg, len);
	mac_pad(&m);
	memcpy(t, m.x, 16);
}

/* S_i = E(A_i), A_i = flags | nonce | i (2 bytes, big-endian). */
static void ccm_keystream(const struct twr_aes128 *ctx, const uint8_t nonce[TWR_CCM_NONCE_LEN],
			  uint16_t i, uint8_t s[16])
{
	s[0] = CCM_A_FLAGS;
	memcpy(&s[1], nonce, TWR_CCM_NONCE_LEN);
	s[14] = (uint8_t)(i >> 8);
	s[15] = (uint8_t)i;
	twr_aes128_encrypt(ctx, s, s);
}

/* data ^= S_1, S_2, ... */
static void ccm_ctr(const struct twr_aes128 *ctx, const uint8_t nonce[TWR_CCM_NONCE_LEN],
		    uint8_t *data, size_t len)
{
	uint8_t s[16];
	uint16_t i = 1;

	for (size_t off = 0; off < len; off += 16, i++) {
		size_t n = len - off < 16 ? len - off : 16;

		ccm_keystream(ctx, nonce, i, s);
		for (size_t j = 0; j < n; j++) {
			data[off + j] ^= s[j];
		}
	}
}

static bool ccm_lengths_ok(size_t aad_len, size_t len)
{
	return aad_len <= TWR_CCM_MAX_AAD && len <= 0xFFFFu;
}

bool twr_ccm_seal(const struct twr_aes128 *ctx, const uint8_t nonce[TWR_CCM_NONCE_LEN],
		  const uint8_t *aad, size_t aad_len, uint8_t *data, size_t len,
		  uint8_t tag[TWR_CCM_TAG_LEN])
{
	uint8_t t[16];
	uint8_t s0[16];

	if (!ccm_lengths_ok(aad_len, len)) {
		return false;
	}
	ccm_mac(ctx, nonce, aad, aad_len, data, len, t);
	ccm_ctr(ctx, nonce, data, len);
	ccm_keystream(ctx, nonce, 0, s0);
	for (size_t j = 0; j < TWR_CCM_TAG_LEN; j++) {
		tag[j] = (uint8_t)(t[j] ^ s0[j]);
	}
	return true;
}

bool twr_ccm_open(const struct twr_aes128 *ctx, const uint8_t nonce[TWR_CCM_NONCE_LEN],
		  const uint8_t *aad, size_t aad_len, uint8_t *data, size_t len,
		  const uint8_t tag[TWR_CCM_TAG_LEN])
{
	uint8_t t[16];
	uint8_t s0[16];
	uint8_t diff = 0;

	if (!ccm_lengths_ok(aad_len, len)) {
		return false;
	}
	/* CTR is its own inverse: decrypt, then MAC the recovered plaintext. */
	ccm_ctr(ctx, nonce, data, len);
	ccm_mac(ctx, nonce, aad, aad_len, data, len, t);
	ccm_keystream(ctx, nonce, 0, s0);
	for (size_t j = 0; j < TWR_CCM_TAG_LEN; j++) {
		diff |= (uint8_t)((t[j] ^ s0[j]) ^ tag[j]);
	}
	if (diff != 0) {
		memset(data, 0, len);
		return false;
	}
	return true;
}
