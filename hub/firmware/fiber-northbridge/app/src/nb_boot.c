/* SPDX-License-Identifier: Apache-2.0
 *
 * The app's side of nbboot (northbridge_update.md §3.1, §10), see nb_boot.h. The layout
 * constants are nbboot's (nbboot/src/nb_app_valid.h); the asserts below tie them to the
 * board dts this image is linked from.
 */
#include "nb_boot.h"
#include "nb_app_valid.h"
#include "uart_link.h"

#include <zephyr/devicetree.h>
#include <zephyr/kernel.h>
#include <zephyr/storage/flash_map.h>
#include <zephyr/sys/barrier.h>
#include <zephyr/sys/reboot.h>

/* The app is linked into slot0, behind the stub, ... */
BUILD_ASSERT(CONFIG_FLASH_BASE_ADDRESS + CONFIG_FLASH_LOAD_OFFSET == NB_APP_ADDR,
	     "app not linked at nbboot's NB_APP_ADDR (dts zephyr,code-partition)");
BUILD_ASSERT(PARTITION_SIZE(slot0_partition) == NB_APP_MAX_LEN, "slot0 size");
/* ... the record is where the stub and the Hub's flasher put it, ... */
BUILD_ASSERT(CONFIG_FLASH_BASE_ADDRESS + PARTITION_OFFSET(app_valid_partition) ==
	     NB_APP_VALID_ADDR, "app_valid partition");
/* ... and the boot request word is not Zephyr's RAM. */
BUILD_ASSERT(DT_REG_ADDR(DT_CHOSEN(zephyr_sram)) + DT_REG_SIZE(DT_CHOSEN(zephyr_sram)) <=
	     NB_BOOT_REQ_ADDR, "sram0 overlaps the boot request word");

/* The RSP is 6 B, but events queued before it go first: a full TX ring is 178 ms of line. */
#define NB_BOOT_DRAIN_MS 250

void nb_boot_app_record(uint32_t *image_len, uint32_t *image_crc32)
{
	struct nb_app_record r;

	if (nb_app_record_parse((const uint8_t *)NB_APP_VALID_ADDR, &r)) {
		*image_len = r.image_len;
		*image_crc32 = r.image_crc32;
	} else {
		*image_len = 0;
		*image_crc32 = 0;
	}
}

void nb_boot_enter_rom_bootloader(void)
{
	/* On a timeout the reboot goes ahead anyway: the Hub then finds the ROM bootloader by
	 * its 0x7F sync instead of by the RSP (northbridge_update.md §3.6). */
	(void)uart_link_tx_drain(NB_BOOT_DRAIN_MS);

	*(volatile uint32_t *)NB_BOOT_REQ_ADDR = NB_BOOT_REQ_MAGIC;
	barrier_dsync_fence_full();
	sys_reboot(SYS_REBOOT_COLD);
}
