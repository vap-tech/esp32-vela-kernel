/* Adapted from esp32-p4 src/boot.S; MIT, copyright 2026 vitalii petrenko.
 * See docs/provenance.md. Single boot hart, HP1 held in reset. */
/* Stage 1 lives entirely in SPM. No runtime code/data is referenced before load.
 * Cache register sequence matches the previously validated P4 rev1.x sequence:
 * L2 reclaimed as SRAM, L1-DCache bypassed for zero-flush DMA coherency, and
 * HP0's 16 KiB L1-ICache0 enabled and invalidated after upper payload load.
 * ROM esp_rom_spiflash_read = 0x4fc00158 (ESP-IDF v6.1 ROM ld/header).
 */
.section .text.entry,"ax"
.global _start
.option push
.option norelax
_start:
    csrci mstatus, 8
    csrw mie, zero
    /* HP1 may have been active before a warm HP reset. Hold it in global
       reset and gate its clock BEFORE cache maintenance or SRAM reload. */
    li t0, 0x500e60c0
    lw t1, 0(t0)
    ori t1, t1, 0x100
    sw t1, 0(t0)
    li t0, 0x500e6014
    lw t1, 0(t0)
    andi t1, t1, -17
    sw t1, 0(t0)
    fence iorw, iorw
    li t0, 0x500e5060
    lw t1, 0(t0)
    ori t1, t1, 1
    sw t1, 0(t0)
    fence rw, rw
    fence.i
    la sp, _boot_stack_top
    li t0, 0x40000000
    add sp, sp, t0
    la t0, _boot_trap
    csrw mtvec, t0
    call boot_watchdogs
    call boot_cache
    la sp, _boot_stack_top
    call boot_load_upper
    la t0, _bss_start
    la t1, _bss_end
1:
    bgeu t0, t1, 2f
    sw zero, 0(t0)
    addi t0, t0, 4
    j 1b
2:
    call boot_enable_icache0
    la sp, _stack_top
    la t0, vela_trap_entry
    csrw mtvec, t0
    csrw mie, zero
    call rust_main
3:  j 3b
.balign 4
_boot_trap:
    li a0, 84 /* T: trap before runtime loaded */
    j boot_fail

/* Do not depend on a reading USB host for boot failures. */
boot_fail:
    li t0, 0x500d2000
    li t2, 10000
.Lfail_poll:
    lw t1, 4(t0)
    andi t1, t1, 2
    bnez t1, .Lfail_emit
    addi t2, t2, -1
    bnez t2, .Lfail_poll
    j .Lfail_stop
.Lfail_emit:
    sw a0, 0(t0)
    li t1, 1
    sw t1, 4(t0)
.Lfail_stop:
    j .Lfail_stop

.macro disable_wdt base, config, protect, clear, update
    li t0, \base
    li t1, 0x50d83aa1
    sw t1, \protect(t0)
    lw t1, \config(t0)
    li t2, \clear
    and t1, t1, t2
    li t2, \update
    or t1, t1, t2
    sw t1, \config(t0)
    sw zero, \protect(t0)
.endm
boot_watchdogs:
    disable_wdt 0x50116000, 0, 0x18, 0x7fffefff, 0
    disable_wdt 0x500c2000, 0x48, 0x64, 0x7fffbfff, 0x400000
    disable_wdt 0x500c3000, 0x48, 0x64, 0x7fffbfff, 0x400000
    li t0, 0x50116000
    li t1, 0x50d83aa1
    sw t1, 0x20(t0)
    lw t1, 0x1c(t0)
    li t2, 0x40000000
    or t1, t1, t2
    sw t1, 0x1c(t0)
    sw zero, 0x20(t0)
    ret

.macro stop_autoload offset
    lw t0, \offset(t6)
    andi t0, t0, -2
    sw t0, \offset(t6)
.endm
boot_cache:
    li t6, 0x3ff10000
    stop_autoload 0xe4
    stop_autoload 0xf8
    stop_autoload 0x134
    stop_autoload 0x2b4
    li t1, 16
6:
    sw t1, 0x9c(t6)
    sw zero, 0xa0(t6)
    sw zero, 0xa4(t6)
    li t2, 2
7:
    li t0, 4
    sw t0, 0x98(t6)
    li t3, 1000000
8:
    lw t0, 0x98(t6)
    andi t0, t0, 16
    bnez t0, 9f
    addi t3, t3, -1
    bnez t3, 8b
    li a0, 87 /* W: cache writeback timeout */
    j boot_fail
9:
    addi t2, t2, -1
    bnez t2, 7b
    slli t1, t1, 1
    li t0, 64
    bne t1, t0, 6b
    fence rw, rw
    li t0, 0x13
    sw t0, 8(t6)
    li t0, 0x20
    sw t0, 0x274(t6)
    li t0, 0
    sw t0, 0x278(t6)
    li t0, 8
    sw t0, 0x27c(t6)
    fence rw, rw
    fence.i
    ret

/* Enable HP0's 16 KiB L1-ICache0 after SRAM payload/BSS writes while keeping
 * L1-ICache1 and L1-DCache bypassed (0x12). Invalidates all 768 KiB of internal
 * L2MEM (0x4ff00000..0x4ffc0000) in 4 KiB windows before jumping to rust_main. */
boot_enable_icache0:
    fence rw, rw
    li t6, 0x3ff10000
    lw t0, 0(t6)
    andi t0, t0, -2
    ori t0, t0, 2
    sw t0, 0(t6)
    li t0, 0x12
    sw t0, 8(t6)
    fence rw, rw
    li t0, 1
    sw t0, 0x9c(t6)
    li t1, 0x4ff00000
    li t2, 0x4ffc0000
    li t4, 0x1000
.Licache_inv_chunk:
    sw t1, 0xa0(t6)
    sw t4, 0xa4(t6)
    li t0, 1
    sw t0, 0x98(t6)
    li t3, 1000000
.Licache_inv_wait:
    lw t0, 0x98(t6)
    andi t0, t0, 16
    bnez t0, .Licache_inv_next
    addi t3, t3, -1
    bnez t3, .Licache_inv_wait
    li a0, 73 /* I: icache invalidate timeout */
    j boot_fail
.Licache_inv_next:
    add t1, t1, t4
    bltu t1, t2, .Licache_inv_chunk
    fence rw, rw
    fence.i
    ret

/* 64-byte header at Flash 0x40000; payload immediately follows at 0x40040. */
boot_load_upper:
    addi sp, sp, -96
    sw ra, 64(sp)
    sw s0, 68(sp)
    sw s1, 72(sp)
    sw s2, 76(sp)
    li a0, 0x40000
    mv a1, sp
    li a2, 64
    li t0, 0x4fc00158
    jalr ra, t0, 0
    bnez a0, boot_flash_error
    lw t0, 0(sp)
    li t1, 0x50553450 /* P4UP */
    bne t0, t1, boot_header_error
    lw t0, 4(sp)
    li t1, 0x31524550 /* PER1 */
    bne t0, t1, boot_header_error
    lw t0, 8(sp)
    li t1, 1
    bne t0, t1, boot_header_error
    la s0, _upper_start
    lw t0, 12(sp)
    bne t0, s0, boot_header_error
    la t0, _upper_load_end
    sub s1, t0, s0
    lw t0, 16(sp)
    bne t0, s1, boot_header_error
    beqz s1, boot_header_error
    andi t0, s1, 3
    bnez t0, boot_header_error
    la t0, _stack_bottom
    add t1, s0, s1
    bgtu t1, t0, boot_header_error
    lw s2, 20(sp)
    /* Bind the manifest to this exact bootstrap/payload pair. The packer
       patches this SPM slot and regenerates the ROM checksum/SHA256. */
    la t0, boot_payload_binding
    lw t1, 8(t0)
    lw t2, 12(t0)
    not t2, t2
    bne t1, t2, boot_header_error
    bne s2, t1, boot_header_error
    lw t0, 24(sp)
    li t1, 0x40040
    bne t0, t1, boot_header_error
    addi t0, sp, 28
    addi t1, sp, 64
10:
    lw t2, 0(t0)
    bnez t2, boot_header_error
    addi t0, t0, 4
    bltu t0, t1, 10b
    li a0, 0x40040
    mv a1, s0
    mv a2, s1
    li t0, 0x4fc00158
    jalr ra, t0, 0
    bnez a0, boot_flash_error
    li t0, -1
    mv t1, s0
    add t2, s0, s1
    li t5, 0xedb88320
11:
    beq t1, t2, 14f
    lbu t3, 0(t1)
    xor t0, t0, t3
    li t4, 8
12:
    andi t3, t0, 1
    srli t0, t0, 1
    beqz t3, 13f
    xor t0, t0, t5
13:
    addi t4, t4, -1
    bnez t4, 12b
    addi t1, t1, 1
    j 11b
14:
    not t0, t0
    bne t0, s2, boot_crc_error
    fence rw, rw
    fence.i
    lw ra, 64(sp)
    lw s0, 68(sp)
    lw s1, 72(sp)
    lw s2, 76(sp)
    addi sp, sp, 96
    ret
boot_header_error:
    li a0, 72 /* H */
    j boot_fail
boot_flash_error:
    li a0, 82 /* R */
    j boot_fail
boot_crc_error:
    li a0, 67 /* C */
    j boot_fail
.balign 4
boot_payload_binding:
    .ascii "P4BIND01"
    .word 0xc0dec0de, 0x3f213f21
.option pop
