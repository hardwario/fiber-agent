/* SPDX-License-Identifier: Apache-2.0
 *
 * The app_valid record and the boot decision, see nb_app_valid.h.
 */
#include "nb_app_valid.h"

#include <stddef.h>

/* Reflected polynomial 0xEDB88320, one entry per nibble. */
static const uint32_t crc32_nibble[16] = {
	0x00000000u, 0x1DB71064u, 0x3B6E20C8u, 0x26D930ACu,
	0x76DC4190u, 0x6B6B51F4u, 0x4DB26158u, 0x5005713Cu,
	0xEDB88320u, 0xF00F9344u, 0xD6D6A3E8u, 0xCB61B38Cu,
	0x9B64C2B0u, 0x86D3D2D4u, 0xA00AE278u, 0xBDBDF21Cu,
};

uint32_t nb_crc32(uint32_t crc, const uint8_t *buf, uint32_t len)
{
	crc = ~crc;
	while (len-- > 0) {
		crc ^= *buf++;
		crc = (crc >> 4) ^ crc32_nibble[crc & 0xFu];
		crc = (crc >> 4) ^ crc32_nibble[crc & 0xFu];
	}
	return ~crc;
}

static uint32_t get_le32(const uint8_t *p)
{
	return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
	       ((uint32_t)p[3] << 24);
}

bool nb_app_record_parse(const uint8_t *rec, struct nb_app_record *out)
{
	uint32_t image_len = get_le32(&rec[NB_APP_VALID_OFF_IMAGE_LEN]);

	if (get_le32(&rec[NB_APP_VALID_OFF_MAGIC]) != NB_APP_VALID_MAGIC ||
	    get_le32(&rec[NB_APP_VALID_OFF_FORMAT]) != NB_APP_VALID_FORMAT ||
	    get_le32(&rec[NB_APP_VALID_OFF_REC_CRC]) !=
		    nb_crc32(0, rec, NB_APP_VALID_OFF_REC_CRC) ||
	    image_len == 0 || image_len > NB_APP_MAX_LEN) {
		return false;
	}
	if (out != NULL) {
		out->image_len = image_len;
		out->image_crc32 = get_le32(&rec[NB_APP_VALID_OFF_IMAGE_CRC]);
		out->fw_ver[0] = rec[NB_APP_VALID_OFF_FW_VER];
		out->fw_ver[1] = rec[NB_APP_VALID_OFF_FW_VER + 1];
		out->fw_ver[2] = rec[NB_APP_VALID_OFF_FW_VER + 2];
		out->git_sha = get_le32(&rec[NB_APP_VALID_OFF_GIT_SHA]);
	}
	return true;
}

bool nb_app_valid(const uint8_t *rec, const uint8_t *image, struct nb_app_record *out)
{
	struct nb_app_record r;

	if (!nb_app_record_parse(rec, &r) || nb_crc32(0, image, r.image_len) != r.image_crc32) {
		return false;
	}
	if (out != NULL) {
		*out = r;
	}
	return true;
}

enum nb_boot_target nb_boot_decide(uint32_t request, const uint8_t *rec, const uint8_t *image)
{
	if (request == NB_BOOT_REQ_MAGIC) {
		return NB_BOOT_ROM;
	}
	return nb_app_valid(rec, image, NULL) ? NB_BOOT_APP : NB_BOOT_ROM;
}
