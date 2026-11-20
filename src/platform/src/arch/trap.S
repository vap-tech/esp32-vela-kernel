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
    csrr t0, mhartid
    beqz t0, .Lhart0_stack
    la t0, _hp1_stack_top
    j .Lcheck_mpp
.Lhart0_stack:
    la t0, _stack_top
.Lcheck_mpp:
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

.section .text.hp1_entry,"ax"
.balign 4
.global vela_hp1_entry
vela_hp1_entry:
    csrci mstatus, 8
    csrw mie, zero
    csrw mscratch, zero
    .option push
    .option norelax
    la sp, _hp1_stack_top
    la t0, vela_trap_entry
    .option pop
    ori t0, t0, 3
    csrw mtvec, t0
    fence rw, rw
    fence.i
    call rust_hp1_main
1:  j 1b

# Extended Context Save & Restore (ESP32-P4 v1.3 Xespv2p1 PIE + RV32F FPU).
# Input: a0 = 16-byte aligned *mut/*const ExtendedContext in internal L2MEM SRAM.
.section .text,"ax"
.balign 4
.global vela_ext_save
vela_ext_save:
    # 1. Enable PIE (CSR 0x7f2 = 1) and save q0..q7, QACC_L/H, UA_STATE, XACC, SAR, SAR_BYTES, FFT_BIT_WIDTH, CFG.
    csrwi 0x7f2, 1
    .word 0x8201223b # esp.vst.128.ip q0, a0, 16  (offset 0 -> 16)
    .word 0x8201263b # esp.vst.128.ip q1, a0, 16  (offset 16 -> 32)
    .word 0x82012a3b # esp.vst.128.ip q2, a0, 16  (offset 32 -> 48)
    .word 0x82012e3b # esp.vst.128.ip q3, a0, 16  (offset 48 -> 64)
    .word 0x8201323b # esp.vst.128.ip q4, a0, 16  (offset 64 -> 80)
    .word 0x8201363b # esp.vst.128.ip q5, a0, 16  (offset 80 -> 96)
    .word 0x82013a3b # esp.vst.128.ip q6, a0, 16  (offset 96 -> 112)
    .word 0x82013e3b # esp.vst.128.ip q7, a0, 16  (offset 112 -> 128)
    .word 0xa001423b # esp.st.qacc.l.l.128.ip a0, 16 (offset 128 -> 144)
    .word 0x8001423b # esp.st.qacc.l.h.128.ip a0, 16 (offset 144 -> 160)
    .word 0xe001423b # esp.st.qacc.h.l.128.ip a0, 16 (offset 160 -> 176)
    .word 0xc001423b # esp.st.qacc.h.h.128.ip a0, 16 (offset 176 -> 192)
    .word 0xa001453b # esp.st.ua.state.ip a0, 16     (offset 192 -> 208)
    .word 0x84b001df # esp.movx.r.xacc.l a1
    sw a1, 0(a0)
    .word 0x8cb001df # esp.movx.r.xacc.h a1
    sw a1, 4(a0)
    .word 0x80b001df # esp.movx.r.sar a1
    sw a1, 8(a0)
    .word 0x88b001df # esp.movx.r.sar.bytes a1
    sw a1, 12(a0)
    .word 0x84d001df # esp.movx.r.fft.bit.width a1
    sw a1, 16(a0)
    .word 0x80d001df # esp.movx.r.cfg a1
    sw a1, 20(a0)
    csrwi 0x7f2, 0
    # 2. Enable FPU (mstatus.FS = Initial) and save f0..f31 + fcsr at offset 240..372 (32..164 from a0=base+208).
    li a1, 0x2000
    csrs mstatus, a1
    .option push
    .option arch, +f
    fsw f0, 32(a0)
    fsw f1, 36(a0)
    fsw f2, 40(a0)
    fsw f3, 44(a0)
    fsw f4, 48(a0)
    fsw f5, 52(a0)
    fsw f6, 56(a0)
    fsw f7, 60(a0)
    fsw f8, 64(a0)
    fsw f9, 68(a0)
    fsw f10, 72(a0)
    fsw f11, 76(a0)
    fsw f12, 80(a0)
    fsw f13, 84(a0)
    fsw f14, 88(a0)
    fsw f15, 92(a0)
    fsw f16, 96(a0)
    fsw f17, 100(a0)
    fsw f18, 104(a0)
    fsw f19, 108(a0)
    fsw f20, 112(a0)
    fsw f21, 116(a0)
    fsw f22, 120(a0)
    fsw f23, 124(a0)
    fsw f24, 128(a0)
    fsw f25, 132(a0)
    fsw f26, 136(a0)
    fsw f27, 140(a0)
    fsw f28, 144(a0)
    fsw f29, 148(a0)
    fsw f30, 152(a0)
    fsw f31, 156(a0)
    frcsr a1
    sw a1, 160(a0)
    .option pop
    ret

.balign 4
.global vela_ext_restore
vela_ext_restore:
    # 1. Enable PIE (CSR 0x7f2 = 1) and restore q0..q7, QACC_L/H, UA_STATE, XACC, SAR, SAR_BYTES, FFT_BIT_WIDTH, CFG.
    csrwi 0x7f2, 1
    .word 0x0201223b # esp.vld.128.ip q0, a0, 16  (offset 0 -> 16)
    .word 0x0201263b # esp.vld.128.ip q1, a0, 16  (offset 16 -> 32)
    .word 0x02012a3b # esp.vld.128.ip q2, a0, 16  (offset 32 -> 48)
    .word 0x02012e3b # esp.vld.128.ip q3, a0, 16  (offset 48 -> 64)
    .word 0x0201323b # esp.vld.128.ip q4, a0, 16  (offset 64 -> 80)
    .word 0x0201363b # esp.vld.128.ip q5, a0, 16  (offset 80 -> 96)
    .word 0x02013a3b # esp.vld.128.ip q6, a0, 16  (offset 96 -> 112)
    .word 0x02013e3b # esp.vld.128.ip q7, a0, 16  (offset 112 -> 128)
    .word 0x2001423b # esp.ld.qacc.l.l.128.ip a0, 16 (offset 128 -> 144)
    .word 0x0001423b # esp.ld.qacc.l.h.128.ip a0, 16 (offset 144 -> 160)
    .word 0x6001423b # esp.ld.qacc.h.l.128.ip a0, 16 (offset 160 -> 176)
    .word 0x4001423b # esp.ld.qacc.h.h.128.ip a0, 16 (offset 176 -> 192)
    .word 0x2001453b # esp.ld.ua.state.ip a0, 16     (offset 192 -> 208)
    lw a1, 0(a0)
    .word 0x94b1805f # esp.movx.w.xacc.l a1
    lw a1, 4(a0)
    .word 0x9cb1805f # esp.movx.w.xacc.h a1
    lw a1, 8(a0)
    .word 0x90b1805f # esp.movx.w.sar a1
    lw a1, 12(a0)
    .word 0x98b1805f # esp.movx.w.sar.bytes a1
    lw a1, 16(a0)
    .word 0x94d1805f # esp.movx.w.fft.bit.width a1
    lw a1, 20(a0)
    .word 0x90d1805f # esp.movx.w.cfg a1
    lw a1, 24(a0)
    bnez a1, 1f
    li a1, 2
1:  csrw 0x7f2, a1
    # 2. Enable FPU (mstatus.FS = Initial) and restore f0..f31 + fcsr at offset 240..372 (32..164 from a0=base+208).
    li a1, 0x2000
    csrs mstatus, a1
    .option push
    .option arch, +f
    lw a1, 160(a0)
    fscsr a1
    flw f0, 32(a0)
    flw f1, 36(a0)
    flw f2, 40(a0)
    flw f3, 44(a0)
    flw f4, 48(a0)
    flw f5, 52(a0)
    flw f6, 56(a0)
    flw f7, 60(a0)
    flw f8, 64(a0)
    flw f9, 68(a0)
    flw f10, 72(a0)
    flw f11, 76(a0)
    flw f12, 80(a0)
    flw f13, 84(a0)
    flw f14, 88(a0)
    flw f15, 92(a0)
    flw f16, 96(a0)
    flw f17, 100(a0)
    flw f18, 104(a0)
    flw f19, 108(a0)
    flw f20, 112(a0)
    flw f21, 116(a0)
    flw f22, 120(a0)
    flw f23, 124(a0)
    flw f24, 128(a0)
    flw f25, 132(a0)
    flw f26, 136(a0)
    flw f27, 140(a0)
    flw f28, 144(a0)
    flw f29, 148(a0)
    flw f30, 152(a0)
    flw f31, 156(a0)
    .option pop
    ret
