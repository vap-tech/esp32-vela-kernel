ENTRY(_start)
PHDRS { code PT_LOAD FLAGS(5); data PT_LOAD FLAGS(6); }
SECTIONS {
  . = 0x4ff80000;
  .text : { *(.text*) *(.rodata*) } :code
  . = ALIGN(4096);
  .bss : { *(.bss*) } :data
}
