OUTPUT_ARCH(riscv)
ENTRY(_start)
MEMORY { LP (rwx) : ORIGIN = 0x50108000, LENGTH = 32K }
SECTIONS {
 .text : { KEEP(*(.text.entry)) *(.text .text.*) } > LP
 .rodata : { *(.rodata .rodata.* .srodata .srodata.*) } > LP
 .data : { *(.data .data.* .sdata .sdata.*) } > LP
 .bss (NOLOAD) : ALIGN(4) {
   _bss_start = .; *(.bss .bss.* .sbss .sbss.*) . = ALIGN(4); _bss_end = .;
 } > LP
 _stack_top = ORIGIN(LP) + LENGTH(LP);
 ASSERT(_start == ORIGIN(LP), "unexpected direct LP entry")
 ASSERT(_bss_end + 1024 <= _stack_top, "LP image overlaps stack")
 /DISCARD/ : { *(.eh_frame .eh_frame_hdr) }
}
