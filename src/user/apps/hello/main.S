.section .text
.globl _start
_start:
    la t0, scratch
    lw t1, 0(t0)
    bnez t1, fail
    li t1, 42
    sw t1, 0(t0)
    lw t2, 0(t0)
    bne t1, t2, fail
    la a0, message
    li a1, 18
    j print
fail:
    la a0, failure
    li a1, 14
print:
    li a7, 1
    ecall
    li a7, 3
    ecall
.section .rodata
message: .ascii "ELF: data, BSS OK\n"
failure: .ascii "ELF: BSS FAIL\n"
.section .bss
.balign 4
scratch: .space 4
