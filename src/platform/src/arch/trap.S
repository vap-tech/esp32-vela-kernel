.section .text.vela_trap,"ax"
.balign 64
.global vela_trap_entry
vela_trap_entry:
    csrrw sp, mscratch, sp
    bnez sp, .Lfrom_user
    # Kernel trap: restore original stack, report fatally; never return.
    csrrw sp, mscratch, sp
    addi sp, sp, -144
    sw t0, 12(sp)
    addi t0, sp, 144
    sw t0, 120(sp)
    j .Lsave
.Lfrom_user:
    addi sp, sp, -144
    sw t0, 12(sp)
    csrr t0, mscratch
    sw t0, 120(sp)
    csrw mscratch, zero
.Lsave:
    sw ra, 0(sp)
    sw gp, 4(sp)
    sw tp, 8(sp)
    sw t1, 16(sp)
    sw t2, 20(sp)
    sw s0, 24(sp)
    sw s1, 28(sp)
    sw a0, 32(sp)
    sw a1, 36(sp)
    sw a2, 40(sp)
    sw a3, 44(sp)
    sw a4, 48(sp)
    sw a5, 52(sp)
    sw a6, 56(sp)
    sw a7, 60(sp)
    sw s2, 64(sp)
    sw s3, 68(sp)
    sw s4, 72(sp)
    sw s5, 76(sp)
    sw s6, 80(sp)
    sw s7, 84(sp)
    sw s8, 88(sp)
    sw s9, 92(sp)
    sw s10, 96(sp)
    sw s11, 100(sp)
    sw t3, 104(sp)
    sw t4, 108(sp)
    sw t5, 112(sp)
    sw t6, 116(sp)
    csrr t0, mcause
    sw t0, 124(sp)
    csrr t0, mepc
    sw t0, 128(sp)
    csrr t0, mtval
    sw t0, 132(sp)
    csrr t0, mstatus
    sw t0, 136(sp)
    mv a0, sp
    call rust_trap
.global vela_resume
vela_resume:
    # a0 points to a kernel-owned task frame. Drop trap call stack completely.
    mv sp, a0
    lw t0, 124(sp)
    # User contexts and the dedicated kernel idle continuation both return
    # outside any interrupt handler.
    # P4 CLIC carries MPIL in mcause[23:16]. Never carry a machine handler's
    # active level across a user context switch (not a nested IRQ return).
    li t1, 0x00ff0000
    not t1, t1
    and t0, t0, t1
    csrw mcause, t0
    lw t0, 128(sp)
    csrw mepc, t0
    lw t0, 136(sp)
    csrw mstatus, t0
    .option push
    .option norelax
    la t0, _stack_top
    .option pop
    lw t1, 136(sp)
    srli t1, t1, 11
    andi t1, t1, 3
    beqz t1, .Luser_scratch
    li t0, 0
.Luser_scratch:
    csrw mscratch, t0
    lw ra, 0(sp)
    lw gp, 4(sp)
    lw tp, 8(sp)
    lw t0, 12(sp)
    lw t1, 16(sp)
    lw t2, 20(sp)
    lw s0, 24(sp)
    lw s1, 28(sp)
    lw a0, 32(sp)
    lw a1, 36(sp)
    lw a2, 40(sp)
    lw a3, 44(sp)
    lw a4, 48(sp)
    lw a5, 52(sp)
    lw a6, 56(sp)
    lw a7, 60(sp)
    lw s2, 64(sp)
    lw s3, 68(sp)
    lw s4, 72(sp)
    lw s5, 76(sp)
    lw s6, 80(sp)
    lw s7, 84(sp)
    lw s8, 88(sp)
    lw s9, 92(sp)
    lw s10, 96(sp)
    lw s11, 100(sp)
    lw t3, 104(sp)
    lw t4, 108(sp)
    lw t5, 112(sp)
    lw t6, 116(sp)
    lw sp, 120(sp)
    mret

.balign 4
.global vela_idle_entry
vela_idle_entry:
    wfi
    j vela_idle_entry
