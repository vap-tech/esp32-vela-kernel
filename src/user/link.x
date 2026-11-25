ENTRY(_start)
PHDRS {
  code PT_LOAD FLAGS(5);
  data PT_LOAD FLAGS(6);
  dynamic PT_DYNAMIC FLAGS(6);
}
SECTIONS {
  . = 0;
  .text : {
    KEEP(*(.text.entry))
    *(.text .text.*)
    *(.rodata .rodata.* .srodata .srodata.*)
  } :code
  .rela.dyn : { *(.rela.dyn .rela.*) } :code
  .dynsym : { *(.dynsym) } :code
  .dynstr : { *(.dynstr) } :code
  .gnu.hash : { *(.gnu.hash) } :code
  .hash : { *(.hash) } :code
  . = (. <= 0x1000)  ? 0x1000  :
      (. <= 0x2000)  ? 0x2000  :
      (. <= 0x4000)  ? 0x4000  :
      (. <= 0x8000)  ? 0x8000  :
      (. <= 0x10000) ? 0x10000 :
      (. <= 0x20000) ? 0x20000 :
      (. <= 0x40000) ? 0x40000 :
      (. <= 0x80000) ? 0x80000 : 0x100000;
  .data : {
    *(.data .data.* .data.rel.ro .data.rel.ro.* .sdata .sdata.* .got .got.*)
  } :data
  .dynamic : { *(.dynamic) } :data :dynamic
  .bss : { *(.bss .bss.* .sbss .sbss.* COMMON) } :data
  /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment) }
}
