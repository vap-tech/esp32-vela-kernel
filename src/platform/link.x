OUTPUT_ARCH(riscv)
ENTRY(_start)
MEMORY {
 ENTRY (rx) : ORIGIN = 0x30100000, LENGTH = 8K
 DISPLAY (rw) : ORIGIN = 0x4ff00000, LENGTH = 144K
 BOOT_STACK (rw) : ORIGIN = 0x4ff26000, LENGTH = 8K
 UPPER (rwx) : ORIGIN = 0x4ff40000, LENGTH = 432K
 STACK (rw) : ORIGIN = 0x4ffac000, LENGTH = 80K
}
SECTIONS {
 .display (NOLOAD) : ALIGN(64) {
  _orbit_lli = .; . += 4 * 3 * 64;
  _orbit_black = .; . += 256;
  . = ALIGN(64);
  _orbit_band = .; . += 720 * 64 * 2;
  . = ALIGN(64);
  _sdmmc_idma_chain = .; . += 16 * 256;
  . = ALIGN(64);
  _sdmmc_dma_buf = .; . += 32768;
 } > DISPLAY
 .entry : ALIGN(16) {
  _entry_start = .;
  KEEP(*(.text.entry));
  . = ALIGN(64);
  KEEP(*(.text.vela_trap));
  _entry_end = .;
 } > ENTRY
 .upper : ALIGN(64) {
  _upper_start = .;
  *(.text .text.*);
  . = ALIGN(4);
  *(.rodata .rodata.* .srodata .srodata.*);
  . = ALIGN(4);
  *(.data .data.* .sdata .sdata.* .got .got.*);
  . = ALIGN(4); _upper_load_end = .;
 } > UPPER
 .bss (NOLOAD) : ALIGN(16) {
  _bss_start = .; *(.bss .bss.* .sbss .sbss.*); *(COMMON);
  . = ALIGN(16); _bss_end = .;
 } > UPPER
 _dma_page = ORIGIN(STACK) - 8192;
 _dma_descriptors = _dma_page + 4096;
 ASSERT(SIZEOF(.display) <= LENGTH(DISPLAY), "Display exceeds SRAM reservation")
 ASSERT(ORIGIN(DISPLAY) + LENGTH(DISPLAY) <= ORIGIN(BOOT_STACK), "Display overlaps boot stack")
 ASSERT(_bss_end <= _dma_page, "Runtime overlaps DMA arena")
 _boot_stack_top = ORIGIN(BOOT_STACK) + LENGTH(BOOT_STACK);
 _stack_bottom = ORIGIN(STACK);
 _hp1_stack_top = ORIGIN(STACK) + LENGTH(STACK) / 2;
 _stack_top = ORIGIN(STACK) + LENGTH(STACK);
 ASSERT(SIZEOF(.entry) <= 8192, "SPM entry exceeds 8KiB")
 ASSERT(_upper_start == 0x4ff40000, "ROM reservation violated")
 ASSERT(_bss_end <= _stack_bottom, "Runtime overlaps stack")
 ASSERT(_upper_load_end > _upper_start, "Empty payload")
 /DISCARD/ : { *(.eh_frame .eh_frame_hdr); }
}
