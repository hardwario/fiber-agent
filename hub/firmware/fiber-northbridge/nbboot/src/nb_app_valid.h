/* SPDX-License-Identifier: Apache-2.0
 *
 * The Northbridge flash layout, the app_valid record and the boot decision (proximos-v2
 * plan/control/radio/northbridge_update.md §3.1, §10). Shared by the boot stub (nbboot.c),
 * the application (app/src/nb_boot.c: GET_INFO, ENTER_BOOTLOADER) and tests/nbboot_record;
 * tools/nb_app_valid.py is the Python twin that builds the record for the Hub and the CI.
 *
 * Plain freestanding C: no libc, no Zephyr, no static data besides the CRC table, so the stub
 * needs no .data/.bss start-up code and the test runs it on native_sim over plain buffers.
 */
#ifndef NB_APP_VALID_H_
#define NB_APP_VALID_H_

#include <stdbool.h>
#include <stdint.h>

/* ---- flash layout (absolute addresses; page 2 KB, programming unit 8 B) ------------------- */

#define NB_FLASH_BASE      0x08000000u
#define NB_FLASH_PAGE_SIZE 0x800u

/* nbboot, page 0. Never rewritten in the field. */
#define NB_BOOT_STUB_ADDR 0x08000000u
#define NB_BOOT_STUB_SIZE 0x800u

/* slot0: the Zephyr app, pages 1..118. */
#define NB_APP_ADDR    0x08000800u
#define NB_APP_MAX_LEN 241664u /* 118 pages */

/* app_valid: page 119; the record is its first 32 B, the rest of the page 0xFF. */
#define NB_APP_VALID_ADDR 0x0803B800u

/* storage (pages 120..127) is never touched by the stub or the flasher. */
#define NB_STORAGE_ADDR 0x0803C000u

/* The ROM (system memory) bootloader, AN3155 over USART1. */
#define NB_ROM_BOOTLOADER_ADDR 0x1FFF0000u

/* ---- boot request (RAM, outside Zephyr's SRAM region) ------------------------------------ */

/* The last 16 B of SRAM are not Zephyr's (the board dts shrinks sram0 by 16 B). The first word
 * is the boot request; SRAM survives the cold reboot because the option byte SRAM_RST is 1. */
#define NB_BOOT_REQ_ADDR  0x2000FFF0u
#define NB_BOOT_REQ_MAGIC 0x4C42424Eu /* "NBBL" in memory order: go to the ROM bootloader */

/* ---- app_valid record, little-endian, 32 B ----------------------------------------------- */

#define NB_APP_VALID_LEN    32u
#define NB_APP_VALID_MAGIC  0x4156424Eu /* "NBVA" in memory order */
#define NB_APP_VALID_FORMAT 1u

/* Field offsets. rec_crc32 covers bytes 0..23. */
#define NB_APP_VALID_OFF_MAGIC     0
#define NB_APP_VALID_OFF_FORMAT    4
#define NB_APP_VALID_OFF_IMAGE_LEN 8
#define NB_APP_VALID_OFF_IMAGE_CRC 12
#define NB_APP_VALID_OFF_FW_VER    16 /* major, minor, patch, pad 0 */
#define NB_APP_VALID_OFF_GIT_SHA   20
#define NB_APP_VALID_OFF_REC_CRC   24
#define NB_APP_VALID_OFF_RESERVED  28 /* 0xFFFFFFFF, not checked */

struct nb_app_record {
	uint32_t image_len;
	uint32_t image_crc32;
	uint8_t fw_ver[3];
	uint32_t git_sha; /* path-scoped short SHA, as EVT_BOOT shows it */
};

/* CRC-32/ISO-HDLC, the same as zlib's crc32(): start with 0, feed the previous result to
 * continue over several buffers. Nibble table (64 B): about four times the bitwise speed on the
 * reset clock, at a quarter of a byte table's size. */
uint32_t nb_crc32(uint32_t crc, const uint8_t *buf, uint32_t len);

/* The record alone: magic, format and rec_crc32 match and image_len is in (0, NB_APP_MAX_LEN].
 * Fills `out` (may be NULL) only when it returns true. Reads exactly NB_APP_VALID_LEN bytes. */
bool nb_app_record_parse(const uint8_t *rec, struct nb_app_record *out);

/* The stub's acceptance rule (§10): the record parses and crc32(image[0..image_len]) equals
 * its image_crc32. Reads the image only after the record passed. */
bool nb_app_valid(const uint8_t *rec, const uint8_t *image, struct nb_app_record *out);

enum nb_boot_target {
	NB_BOOT_APP = 0,
	NB_BOOT_ROM = 1,
};

/* What the stub starts after a reset, given the boot request word it has just read (and
 * cleared): the ROM bootloader when the request is NB_BOOT_REQ_MAGIC - flash is then not read
 * at all - or when the app is not valid; the app otherwise. */
enum nb_boot_target nb_boot_decide(uint32_t request, const uint8_t *rec, const uint8_t *image);

#endif /* NB_APP_VALID_H_ */
