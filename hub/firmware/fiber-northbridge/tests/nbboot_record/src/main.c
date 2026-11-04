/* SPDX-License-Identifier: Apache-2.0
 *
 * ztest suite for nbboot/src/nb_app_valid.c, the code the boot stub runs on every reset: the
 * CRC-32, the app_valid record (northbridge_update.md §10) and the boot decision. A failure
 * here is a contract change the Hub's flasher (U2) and the packaging (tools/nb_app_valid.py)
 * have to follow. pinned_record is the same vector tools/test_nb_package.py checks the Python
 * side against.
 */
#include <zephyr/ztest.h>
#include <zephyr/sys/byteorder.h>
#include <string.h>

#include "nb_app_valid.h"

/* §10 flash layout: the regions tile the 256 KB flash, page aligned. */
BUILD_ASSERT(NB_BOOT_STUB_ADDR == NB_FLASH_BASE);
BUILD_ASSERT(NB_BOOT_STUB_ADDR + NB_BOOT_STUB_SIZE == NB_APP_ADDR);
BUILD_ASSERT(NB_APP_ADDR + NB_APP_MAX_LEN == NB_APP_VALID_ADDR, "slot0 is 118 pages");
BUILD_ASSERT(NB_APP_MAX_LEN == 118 * NB_FLASH_PAGE_SIZE);
BUILD_ASSERT(NB_APP_VALID_ADDR + NB_FLASH_PAGE_SIZE == NB_STORAGE_ADDR);
BUILD_ASSERT(NB_STORAGE_ADDR + 8 * NB_FLASH_PAGE_SIZE == NB_FLASH_BASE + 256 * 1024);
BUILD_ASSERT(NB_BOOT_REQ_ADDR == 0x20000000u + 64 * 1024 - 16, "last 16 B of SRAM");

/* image[i] = 7 i + 3, 64 B; fw 0.4.0, git_sha 0x1a2b3c4d (tools/test_nb_package.py). */
static const uint8_t pinned_record[NB_APP_VALID_LEN] = {
	0x4e, 0x42, 0x56, 0x41, 0x01, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
	0xf0, 0xec, 0xd9, 0xcb, 0x00, 0x04, 0x00, 0x00, 0x4d, 0x3c, 0x2b, 0x1a,
	0x01, 0xfe, 0x7d, 0xf5, 0xff, 0xff, 0xff, 0xff,
};

static uint8_t pinned_image[64];

/* Big enough for a full slot0, plus one byte to show nothing past image_len is read. */
static uint8_t slot0[NB_APP_MAX_LEN + 1];

static void fill_pinned_image(void)
{
	for (size_t i = 0; i < sizeof(pinned_image); i++) {
		pinned_image[i] = (uint8_t)(i * 7 + 3);
	}
}

static void *setup(void)
{
	fill_pinned_image();
	return NULL;
}

ZTEST_SUITE(nbboot_record, NULL, setup, NULL, NULL, NULL);

/* The record for `image`, built the way tools/nb_app_valid.py does. */
static void build_record(uint8_t rec[NB_APP_VALID_LEN], const uint8_t *image, uint32_t len)
{
	memset(rec, 0xFF, NB_APP_VALID_LEN);
	sys_put_le32(NB_APP_VALID_MAGIC, &rec[0]);
	sys_put_le32(NB_APP_VALID_FORMAT, &rec[4]);
	sys_put_le32(len, &rec[8]);
	sys_put_le32(image != NULL ? nb_crc32(0, image, len) : 0, &rec[12]);
	rec[16] = 0;
	rec[17] = 4;
	rec[18] = 0;
	rec[19] = 0;
	sys_put_le32(0xdeadbeefu, &rec[20]);
	sys_put_le32(nb_crc32(0, rec, 24), &rec[24]);
}

static uint32_t crc32_bitwise(const uint8_t *p, size_t n)
{
	uint32_t crc = 0xFFFFFFFFu;

	while (n-- > 0) {
		crc ^= *p++;
		for (int k = 0; k < 8; k++) {
			crc = (crc >> 1) ^ (0xEDB88320u & (0u - (crc & 1u)));
		}
	}
	return ~crc;
}

/* ---- CRC-32 -------------------------------------------------------------------------------- */

ZTEST(nbboot_record, test_crc32_check_value)
{
	static const uint8_t check[] = "123456789";

	zassert_equal(nb_crc32(0, check, 9), 0xCBF43926u, "CRC-32/ISO-HDLC check value");
	zassert_equal(nb_crc32(0, check, 0), 0, "empty");
	zassert_equal(nb_crc32(nb_crc32(0, check, 4), &check[4], 5), 0xCBF43926u,
		      "continues like zlib");
	zassert_equal(nb_crc32(0, pinned_image, sizeof(pinned_image)), 0xCBD9ECF0u);
}

ZTEST(nbboot_record, test_crc32_matches_bitwise)
{
	static uint8_t buf[4096];
	uint32_t x = 0x12345678u;

	for (size_t i = 0; i < sizeof(buf); i++) {
		x = x * 1103515245u + 12345u;
		buf[i] = (uint8_t)(x >> 16);
	}
	for (size_t n = 0; n <= sizeof(buf); n += 257) {
		zassert_equal(nb_crc32(0, buf, n), crc32_bitwise(buf, n), "n %zu", n);
	}
}

/* ---- the record ---------------------------------------------------------------------------- */

ZTEST(nbboot_record, test_pinned_record)
{
	struct nb_app_record r;
	uint8_t rec[NB_APP_VALID_LEN];

	zassert_mem_equal(pinned_record, "NBVA", 4, "magic in memory order");
	zassert_true(nb_app_record_parse(pinned_record, &r));
	zassert_equal(r.image_len, 64);
	zassert_equal(r.image_crc32, 0xCBD9ECF0u);
	zassert_equal(r.fw_ver[0], 0);
	zassert_equal(r.fw_ver[1], 4);
	zassert_equal(r.fw_ver[2], 0);
	zassert_equal(r.git_sha, 0x1A2B3C4Du);
	zassert_true(nb_app_valid(pinned_record, pinned_image, &r));
	zassert_true(nb_app_record_parse(pinned_record, NULL));

	/* The C builder and the Python one agree apart from git_sha (and so rec_crc32). */
	build_record(rec, pinned_image, sizeof(pinned_image));
	zassert_mem_equal(rec, pinned_record, 20);
}

ZTEST(nbboot_record, test_every_header_bit_is_checked)
{
	uint8_t rec[NB_APP_VALID_LEN];
	struct nb_app_record r = { .image_len = 0x5A5A5A5Au };

	/* magic, format, image_len, image_crc32, fw_ver + pad, git_sha, rec_crc32 */
	for (size_t bit = 0; bit < 28 * 8; bit++) {
		memcpy(rec, pinned_record, sizeof(rec));
		rec[bit / 8] ^= (uint8_t)(1u << (bit % 8));
		zassert_false(nb_app_record_parse(rec, &r), "bit %zu", bit);
		zassert_false(nb_app_valid(rec, pinned_image, NULL), "bit %zu", bit);
	}
	zassert_equal(r.image_len, 0x5A5A5A5Au, "out untouched on a reject");

	/* reserved is not part of the rule. */
	memcpy(rec, pinned_record, sizeof(rec));
	memset(&rec[28], 0, 4);
	zassert_true(nb_app_valid(rec, pinned_image, NULL));
}

ZTEST(nbboot_record, test_erased_and_blank_records)
{
	uint8_t rec[NB_APP_VALID_LEN];

	memset(rec, 0xFF, sizeof(rec));
	zassert_false(nb_app_record_parse(rec, NULL), "erased page");
	memset(rec, 0x00, sizeof(rec));
	zassert_false(nb_app_record_parse(rec, NULL), "zeroes");
}

ZTEST(nbboot_record, test_format_and_magic_values)
{
	uint8_t rec[NB_APP_VALID_LEN];

	build_record(rec, pinned_image, sizeof(pinned_image));
	zassert_true(nb_app_valid(rec, pinned_image, NULL));

	/* A future format 2 with a correct rec_crc32 is still not this stub's. */
	sys_put_le32(2, &rec[4]);
	sys_put_le32(nb_crc32(0, rec, 24), &rec[24]);
	zassert_false(nb_app_record_parse(rec, NULL), "format 2");

	/* The boot request magic is not a record magic. */
	build_record(rec, pinned_image, sizeof(pinned_image));
	sys_put_le32(NB_BOOT_REQ_MAGIC, &rec[0]);
	sys_put_le32(nb_crc32(0, rec, 24), &rec[24]);
	zassert_false(nb_app_record_parse(rec, NULL), "NBBL");
}

ZTEST(nbboot_record, test_image_len_range)
{
	uint8_t rec[NB_APP_VALID_LEN];

	build_record(rec, NULL, 0);
	zassert_false(nb_app_record_parse(rec, NULL), "0 B");
	build_record(rec, NULL, NB_APP_MAX_LEN + 1);
	zassert_false(nb_app_record_parse(rec, NULL), "past slot0");
	build_record(rec, NULL, 0xFFFFFFFFu);
	zassert_false(nb_app_record_parse(rec, NULL), "erased length");

	build_record(rec, pinned_image, 1);
	zassert_true(nb_app_valid(rec, pinned_image, NULL), "1 B");
}

ZTEST(nbboot_record, test_full_slot)
{
	uint8_t rec[NB_APP_VALID_LEN];

	for (size_t i = 0; i < sizeof(slot0); i++) {
		slot0[i] = (uint8_t)(i ^ (i >> 8));
	}
	build_record(rec, slot0, NB_APP_MAX_LEN);
	zassert_true(nb_app_valid(rec, slot0, NULL), "a full slot0");

	/* Past image_len: the record page, not the image. */
	slot0[NB_APP_MAX_LEN] ^= 0xFF;
	zassert_true(nb_app_valid(rec, slot0, NULL), "nothing past image_len is read");

	slot0[NB_APP_MAX_LEN - 1] ^= 0x01;
	zassert_false(nb_app_valid(rec, slot0, NULL), "the last byte counts");
	slot0[NB_APP_MAX_LEN - 1] ^= 0x01;
	slot0[0] ^= 0x80;
	zassert_false(nb_app_valid(rec, slot0, NULL), "the first byte counts");
}

ZTEST(nbboot_record, test_image_corruption)
{
	uint8_t image[sizeof(pinned_image)];
	struct nb_app_record r;

	for (size_t i = 0; i < sizeof(image); i++) {
		memcpy(image, pinned_image, sizeof(image));
		image[i] ^= 0x10;
		zassert_true(nb_app_record_parse(pinned_record, NULL), "the record is fine");
		zassert_false(nb_app_valid(pinned_record, image, &r), "byte %zu", i);
	}
	/* Half written: the tail still erased. */
	memcpy(image, pinned_image, sizeof(image));
	memset(&image[32], 0xFF, 32);
	zassert_false(nb_app_valid(pinned_record, image, NULL));
}

/* ---- the boot decision --------------------------------------------------------------------- */

ZTEST(nbboot_record, test_decide)
{
	uint8_t rec[NB_APP_VALID_LEN];
	uint8_t image[sizeof(pinned_image)];
	uint8_t req[4];

	sys_put_le32(NB_BOOT_REQ_MAGIC, req);
	zassert_mem_equal(req, "NBBL", 4, "request magic in memory order");

	zassert_equal(nb_boot_decide(0, pinned_record, pinned_image), NB_BOOT_APP);
	zassert_equal(nb_boot_decide(0xFFFFFFFFu, pinned_record, pinned_image), NB_BOOT_APP,
		      "RAM after power-up: any other value is no request");
	zassert_equal(nb_boot_decide(0x4E42424Cu, pinned_record, pinned_image), NB_BOOT_APP,
		      "byte-swapped magic is no request");

	/* A request never reads flash: no record, no image. */
	zassert_equal(nb_boot_decide(NB_BOOT_REQ_MAGIC, NULL, NULL), NB_BOOT_ROM);

	memset(rec, 0xFF, sizeof(rec));
	zassert_equal(nb_boot_decide(0, rec, pinned_image), NB_BOOT_ROM, "no record");

	memcpy(image, pinned_image, sizeof(image));
	image[10] ^= 1;
	zassert_equal(nb_boot_decide(0, pinned_record, image), NB_BOOT_ROM, "CRC mismatch");
}
