/* SPDX-License-Identifier: Apache-2.0
 *
 * nbboot: the Northbridge boot stub in flash page 0 (proximos-v2
 * plan/control/radio/northbridge_update.md §3.1, §10). Bare metal, no Zephyr, no libc.
 *
 * On every reset it reads and clears the boot request word (NB_BOOT_REQ_ADDR) and starts
 *   - the ROM bootloader (AN3155 on USART1) when the app asked for it (ENTER_BOOTLOADER) or
 *     when the app in slot0 is not valid (app_valid record, CRC-32 of the image), or
 *   - the app at 0x0800_0800 otherwise, with VTOR pointing at its vector table.
 * Either way straight after a reset, so the IWDG (software option, IWDG_SW = 1) is not running
 * yet: the ROM bootloader does not feed it.
 *
 * It runs on the reset clock (MSI 4 MHz) and touches no peripheral: the ROM bootloader and the
 * app both start from the reset state. Two exceptions, both ending in a system reset:
 *   - Entered other than from a reset - the ROM bootloader's GO 0x0800_0000 after a flash, a
 *     debugger - it resets once first, so the app never inherits the ROM bootloader's clocks,
 *     USART and NVIC set-up (a one-shot mark next to the request word stops a loop).
 *   - An NMI (a flash ECC double error: a double word half programmed when the power went) or a
 *     fault while it checks the image: it requests the ROM bootloader and resets.
 */
#include <stdint.h>

#include "nb_app_valid.h"

#define SCB_VTOR       (*(volatile uint32_t *)0xE000ED08u)
#define SCB_AIRCR      (*(volatile uint32_t *)0xE000ED0Cu)
#define AIRCR_VECTKEY  0x05FA0000u
#define AIRCR_SYSRESET (1u << 2)

/* RCC (RM0453): reset values CFGR.SWS = MSI (0), APB2ENR = 0. */
#define RCC_CFGR          (*(volatile uint32_t *)0x58000008u)
#define RCC_CFGR_SWS_MASK (3u << 2)
#define RCC_APB2ENR       (*(volatile uint32_t *)0x58000060u)

/* FLASH_ECCR: ECCD, write 1 to clear. */
#define FLASH_ECCR      (*(volatile uint32_t *)0x58004018u)
#define FLASH_ECCR_ECCD (1u << 31)

/* The stub's own word in the reserved RAM tail, right after the boot request: set before the
 * clean-up reset, so that a state the reset does not clear cannot loop. */
#define NB_STUB_RESET_MARK_ADDR 0x2000FFF4u
#define NB_STUB_RESET_MARK      0x5453524Eu /* "NRST" */

#define BOOT_REQ   (*(volatile uint32_t *)NB_BOOT_REQ_ADDR)
#define RESET_MARK (*(volatile uint32_t *)NB_STUB_RESET_MARK_ADDR)

/* nbboot.ld: the initial MSP, NB_BOOT_REQ_ADDR (the stack grows down from below it). */
extern uint32_t __stack_top;

void nbboot_reset(void) __attribute__((noreturn));
void nbboot_fault(void) __attribute__((noreturn));

/* SP, Reset, NMI, HardFault, then the configurable faults and the system exceptions, none of
 * them enabled here; every one but Reset ends in the ROM bootloader. */
__attribute__((section(".vectors"), used)) static const uintptr_t vectors[16] = {
	(uintptr_t)&__stack_top,
	(uintptr_t)nbboot_reset,
	(uintptr_t)nbboot_fault, (uintptr_t)nbboot_fault, (uintptr_t)nbboot_fault,
	(uintptr_t)nbboot_fault, (uintptr_t)nbboot_fault, 0, 0, 0, 0,
	(uintptr_t)nbboot_fault, (uintptr_t)nbboot_fault, 0,
	(uintptr_t)nbboot_fault, (uintptr_t)nbboot_fault,
};

static inline void barrier(void)
{
	__asm volatile("dsb 0xF\n\tisb 0xF" ::: "memory");
}

static __attribute__((noreturn)) void system_reset(void)
{
	barrier();
	SCB_AIRCR = AIRCR_VECTKEY | AIRCR_SYSRESET;
	barrier();
	for (;;) {
	}
}

/* Start the image whose vector table is at `vt`: VTOR, MSP and PC from the table. */
static __attribute__((noreturn)) void start_image(uint32_t vt)
{
	uint32_t sp = ((const volatile uint32_t *)vt)[0];
	uint32_t pc = ((const volatile uint32_t *)vt)[1];

	SCB_VTOR = vt;
	barrier();
	__asm volatile("msr msp, %0\n\t"
		       "bx %1\n" : : "r"(sp), "r"(pc) : "memory");
	__builtin_unreachable();
}

static int from_reset_state(void)
{
	return SCB_VTOR == 0u && (RCC_CFGR & RCC_CFGR_SWS_MASK) == 0u && RCC_APB2ENR == 0u;
}

void nbboot_reset(void)
{
	if (RESET_MARK != NB_STUB_RESET_MARK && !from_reset_state()) {
		RESET_MARK = NB_STUB_RESET_MARK;
		system_reset();
	}
	RESET_MARK = 0u;

	uint32_t request = BOOT_REQ;

	BOOT_REQ = 0u;
	if (nb_boot_decide(request, (const uint8_t *)NB_APP_VALID_ADDR,
			   (const uint8_t *)NB_APP_ADDR) == NB_BOOT_APP) {
		start_image(NB_APP_ADDR);
	}
	start_image(NB_ROM_BOOTLOADER_ADDR);
}

void nbboot_fault(void)
{
	FLASH_ECCR = FLASH_ECCR_ECCD;
	BOOT_REQ = NB_BOOT_REQ_MAGIC;
	system_reset();
}
