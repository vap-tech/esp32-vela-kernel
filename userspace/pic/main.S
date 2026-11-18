.section .text
.globl _start
_start:
    lla t0, pointer
    lw a0, 0(t0)
    li a1, 18
    li a7, 1
    ecall
    li a7, 3
    ecall
message: .ascii "PIC: relocated OK\n"
.section .data
.balign 4
pointer: .word message
