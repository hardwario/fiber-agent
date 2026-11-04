/* SPDX-License-Identifier: Apache-2.0
 *
 * The app's side of the update through the Hub (proximos-v2
 * plan/control/radio/northbridge_update.md §3.1, §10): the app_valid record nbboot checked
 * before starting this image, and the way back into the STM32 ROM bootloader.
 */
#ifndef APP_NB_BOOT_H_
#define APP_NB_BOOT_H_

#include <stdint.h>

/* image_len and image_crc32 of the app_valid record (GET_INFO app_image_len, app_crc32);
 * both 0 when the record is not valid. The image itself is not re-checked: nbboot did that. */
void nb_boot_app_record(uint32_t *image_len, uint32_t *image_crc32);

/* ENTER_BOOTLOADER, after its RSP is queued: waits for the UART to send it, writes the boot
 * request word and does a cold reboot; nbboot then jumps to the ROM bootloader. Thread
 * context only. Does not return. */
void nb_boot_enter_rom_bootloader(void);

#endif /* APP_NB_BOOT_H_ */
