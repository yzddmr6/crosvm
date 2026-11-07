/* Copyright 2026 The ChromiumOS Authors
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file.
 *
 * Linked at zero and emitted as a flat binary: crosvm copies it to the base of the lent region,
 * which is also where the hypervisor sets the program counter. The code is position-independent
 * (adrp/adr only), so the link address is a formality -- but keeping it at zero makes the offsets
 * in the map file read as offsets into the image, which is what crosvm patches against.
 */

ENTRY(_start)

SECTIONS
{
	/* pvm49: linked at the address crosvm loads it to (AARCH64_PHYS_MEM_START), so an absolute pointer the compiler
	 * emits (a table of &str, a vtable) points into the image instead of near zero; crosvm refuses any other base. */
	. = 0x80000000;

	.head : { KEEP(*(.head)) }
	.text : { *(.text*) }
	.rodata : { *(.rodata*) }
	.data : { *(.data*) }

	. = ALIGN(16);
	_bss_start = .;
	.bss : { *(.bss*) *(COMMON) }
	. = ALIGN(16);
	_bss_end = .;

	/* The stack sits above BSS inside the image's own footprint, so it needs no allocator and
	 * cannot stray into the window that has not been accepted yet. */
	. = ALIGN(16);
	. = . + 0x4000;
	_stack_top = .;

	_shim_end = .;

	/DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.interp) *(.dynamic) *(.dynsym) *(.dynstr) *(.hash) *(.gnu.hash) }
}
