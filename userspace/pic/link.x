ENTRY(_start)
PHDRS { code PT_LOAD FLAGS(5); data PT_LOAD FLAGS(6); dynamic PT_DYNAMIC FLAGS(6); }
SECTIONS {
  . = 0;
  .text : { *(.text*) } :code
  .rela.dyn : { *(.rela.dyn) } :code
  .dynsym : { *(.dynsym) } :code
  .dynstr : { *(.dynstr) } :code
  .gnu.hash : { *(.gnu.hash) } :code
  .hash : { *(.hash) } :code
  . = ALIGN(4096);
  .data : { *(.data*) } :data
  .dynamic : { *(.dynamic) } :data :dynamic
}
