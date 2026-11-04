/* SPDX-License-Identifier: Apache-2.0
 *
 * nbboot: flash page 0 only (northbridge_update.md §10). The link fails when the stub outgrows
 * the page. RAM ends below the reserved tail at NB_BOOT_REQ_ADDR (0x2000_FFF0), so the stack,
 * which starts there and grows down, never touches the boot request word.
 */
MEMORY
{
	FLASH (rx) : ORIGIN = 0x08000000, LENGTH = 2K
	RAM (rw)   : ORIGIN = 0x20000000, LENGTH = 64K - 16
}

ENTRY(nbboot_reset)

__stack_top = ORIGIN(RAM) + LENGTH(RAM);

SECTIONS
{
	.text :
	{
		KEEP(*(.vectors))
		*(.text*)
		*(.rodata*)
	} > FLASH

	/* No start-up code copies or clears anything: the stub must not have any. */
	.data : { *(.data*) } > RAM AT > FLASH
	.bss (NOLOAD) : { *(.bss*) *(COMMON) } > RAM

	/DISCARD/ : { *(.ARM.exidx*) *(.ARM.extab*) *(.comment) }
}

ASSERT(SIZEOF(.data) == 0, "nbboot: no .data, there is no start-up code to copy it")
ASSERT(SIZEOF(.bss) == 0, "nbboot: no .bss, there is no start-up code to clear it")
ASSERT(__stack_top == 0x2000FFF0, "nbboot: the stack must end at the boot request word")
