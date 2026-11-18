OUTPUT_ARCH(riscv)
ENTRY(_start)
MEMORY {
 ENTRY (rx) : ORIGIN = 0x30100000, LENGTH = 8K
 BOOT_STACK (rw) : ORIGIN = 0x4ff26000, LENGTH = 8K
 UPPER (rwx) : ORIGIN = 0x4ff40000, LENGTH = 432K
 STACK (rw) : ORIGIN = 0x4ffac000, LENGTH = 80K
}
SECTIONS {
 .entry : ALIGN(16) { _entry_start = .; KEEP(*(.text.entry)); _entry_end = .; } > ENTRY
 .upper : ALIGN(64) {
  _upper_start = .;
  *(.text .text.*);
  . = ALIGN(4);
  *(.rodata .rodata.* .srodata .srodata.*);
  . = ALIGN(4);
  *(.data .data.* .sdata .sdata.* .got .got.*);
  . = ALIGN(4096); _user_code_start = .;
  KEEP(*(.user.text)); KEEP(*(.user.rodata));
  _user_code_used_end = .;
  . = _user_code_start + 4096; _user_code_end = .;
  . = ALIGN(4); _upper_load_end = .;
 } > UPPER
 .bss (NOLOAD) : ALIGN(16) {
  _bss_start = .; *(.bss .bss.* .sbss .sbss.*); *(COMMON);
  . = ALIGN(4096); _user_stack0 = .; . += 4096;
  _user_stack1 = .; . += 4096;
  _user_stack2 = .; . += 4096;
  . = ALIGN(16); _bss_end = .;
 } > UPPER
 _boot_stack_top = ORIGIN(BOOT_STACK) + LENGTH(BOOT_STACK);
 _stack_bottom = ORIGIN(STACK);
 _stack_top = ORIGIN(STACK) + LENGTH(STACK);
 ASSERT(_user_code_used_end <= _user_code_end, "User code exceeds 4 KiB")
 ASSERT(SIZEOF(.entry) <= 8192, "SPM entry exceeds 8KiB")
 ASSERT(_upper_start == 0x4ff40000, "ROM reservation violated")
 ASSERT(_bss_end <= _stack_bottom, "Runtime overlaps stack")
 ASSERT(_upper_load_end > _upper_start, "Empty payload")
 /DISCARD/ : { *(.eh_frame .eh_frame_hdr); }
}
