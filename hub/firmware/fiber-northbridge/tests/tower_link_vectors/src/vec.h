/* SPDX-License-Identifier: Apache-2.0
 *
 * Shared by the vector suite (main.c) and the deframer fuzz (fuzz.c).
 */
#ifndef TOWER_LINK_VEC_H_
#define TOWER_LINK_VEC_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "hdlc.h"
#include "proto.h"

#define VEC_MAX_FIELDS 48

/* One parsed line: three positional tokens, then key=value fields (pointers into buf). */
struct vec {
	char buf[4096];
	const char *name;
	const char *dir;
	const char *msg;
	int nf;
	const char *k[VEC_MAX_FIELDS];
	const char *v[VEC_MAX_FIELDS];
};

struct frm {
	uint8_t type;
	uint8_t seq;
	uint16_t len;
	uint8_t body[P2P_MAX_BODY_LEN];
};

/* What a decoder run delivered: every frame (the first few kept), the last one, the error
 * results, and how many of `want` came out in order. */
struct sink {
	int n;
	struct frm got[8];
	struct frm last;
	int crc;
	int shorts;
	int over;
	const struct frm *want;
	int nwant;
	int matched;
};

void vec_parse(struct vec *v, const char *line);
const char *vec_get(const struct vec *v, const char *key);
long long vec_int(const struct vec *v, const char *key);
size_t vec_hex_str(const char *s, uint8_t *out, size_t cap);
size_t vec_hex(const struct vec *v, const char *key, uint8_t *out, size_t cap);
void vec_frame(const struct vec *v, struct frm *f);
bool frm_eq(const struct frm *a, const struct frm *b);
void sink_feed(struct hdlc_decoder *d, struct sink *s, const uint8_t *w, size_t n);

#endif /* TOWER_LINK_VEC_H_ */
