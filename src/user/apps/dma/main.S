.section .text
.globl _start
_start:
    mv s0, a0
    mv s2, a1
    li t3, 2048
    add s1, s0, t3
    li t0, 0
fill:
    xor t1, t0, t3
    add t2, s0, t0
    sb t1, 0(t2)
    add t2, s1, t0
    not t1, t1
    sb t1, 0(t2)
    addi t0, t0, 1
    bltu t0, t3, fill
    mv a0, s2
    mv a1, t3
    li a7, 25
    ecall
    bnez a0, fail
    li t0, 0
verify:
    add t2, s1, t0
    lbu t1, 0(t2)
    andi t2, t0, 255
    bne t1, t2, fail
    addi t0, t0, 1
    bltu t0, t3, verify
    lla a0, passed
    li a1, 19
    j print
fail:
    lla a0, failed
    li a1, 21
print:
    li a7, 1
    ecall
    li a7, 3
    ecall
passed: .ascii "DMA: 2048 bytes OK\n"
passed_end:
failed: .ascii "DMA: transfer failed\n"
failed_end:
