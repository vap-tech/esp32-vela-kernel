# Roadmap

Current: single-hart machine-mode First Light, ROM/SPM loader, bounded USB console,
fatal integer trap diagnostics. Next: power-cycle repeatability and negative-loader
tests, then design the userspace boundary. No components below are implemented.

1. U-mode and PMP isolation: region layout, deny-by-default policy, fault tests.
2. Syscalls: ABI, trap entry/return and validated user buffers.
3. User tasks: image format, address-space ownership and task lifetime.
4. Preemptive scheduler: timer source, context format, FP policy and scheduling rules.
5. IPC: message transfer semantics and cancellation.
6. Capabilities: ownership, transfer, revocation and authority checks.
7. Userspace drivers: MMIO/IRQ delegation and DMA isolation constraints.
8. SMP: secondary boot, synchronization, cache ownership and interprocessor interrupts.
9. Portability: independently verified C6, C5, S31 SoC boot/PAC/BSP implementations.
