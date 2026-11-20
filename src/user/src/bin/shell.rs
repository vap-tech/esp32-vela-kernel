#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, TextBuf, arena::Arena, sys, word_le};

vela_user::entry!(main);

const FAILED: &[u8] = b"service unavailable\n";

#[derive(Clone, Copy)]
struct Caps {
    request: u32,
    spawn: u32,
    display: u32,
    sentinel: u32,
}

fn read_file(path: &[u8], request_handle: u32) {
    let mut out = TextBuf::<512>::new();
    let mut offset = 0u16;
    for _ in 0..16 {
        let mut request = Buffer::new();
        request.push(2);
        request.push(offset as u8);
        request.push((offset >> 8) as u8);
        request.append(path);
        let mut response = Buffer::new();
        let result = sys::call6(
            abi::IPC_CALL,
            request_handle,
            request.ptr() as u32,
            request.len() as u32,
            response.mut_ptr() as u32,
            abi::IPC_BYTES as u32,
            100,
        )
        .0;
        if result < 2 || result > abi::IPC_BYTES as u32 {
            sys::write(FAILED);
            return;
        }
        response.set_len(result as usize);
        let status = response.get(0);
        let more = response.get(1);
        let payload = &response.as_slice()[2..];
        // Error replies carry userspace diagnostics, never fabricated file data.
        out.append(payload);
        if status != 0 || more == 0 || payload.is_empty() {
            break;
        }
        offset = offset.saturating_add(payload.len() as u16);
    }
    sys::write(out.as_slice());
}

extern "C" fn yield_worker(iters: u32) -> ! {
    for _ in 0..iters {
        sys::call(abi::YIELD, 0, 0, 0, 0);
    }
    sys::exit();
}

fn bench_syscall() -> u32 {
    sys::sleep();
    let start = sys::rdcycle64();
    for _ in 0..64 {
        core::hint::black_box(sys::call(abi::TICKS, 0, 0, 0, 0));
    }
    let elapsed = sys::rdcycle64().wrapping_sub(start);
    ((elapsed + 32) >> 6) as u32
}

fn bench_ctx_switch() -> Option<u32> {
    let (prev_aff, _) = sys::call6(abi::THREAD_AFFINITY, 0, abi::AFFINITY_HP0, 0, 0, 0, 0);
    let (base, size) = sys::call6(abi::MEM_ALLOC, 4096, 0, 0, 0, 0, 0);
    if (base as i32) < 0 || size < 4096 {
        let _ = sys::call6(abi::THREAD_AFFINITY, 0, prev_aff.clamp(1, 3), 0, 0, 0, 0);
        return None;
    }
    sys::sleep();
    let tid = sys::call6(
        abi::THREAD_CREATE,
        yield_worker as *const () as usize as u32,
        base,
        size,
        64,
        1,
        abi::AFFINITY_HP0,
    )
    .0;
    if (tid as i32) <= 0 {
        let _ = sys::call(abi::MEM_FREE, base, size, 0, 0);
        let _ = sys::call6(abi::THREAD_AFFINITY, 0, prev_aff.clamp(1, 3), 0, 0, 0, 0);
        return None;
    }
    let start = sys::rdcycle64();
    for _ in 0..64 {
        sys::call(abi::YIELD, 0, 0, 0, 0);
    }
    let elapsed = sys::rdcycle64().wrapping_sub(start);
    let mut res = None;
    for _ in 0..8 {
        sys::call(abi::YIELD, 0, 0, 0, 0);
        if sys::call(abi::MEM_FREE, base, size, 0, 0) == 0 {
            res = Some(((elapsed + 64) >> 7) as u32);
            break;
        }
    }
    let _ = sys::call6(abi::THREAD_AFFINITY, 0, prev_aff.clamp(1, 3), 0, 0, 0, 0);
    res
}

fn bench_ipc_rtt(request_handle: u32) -> Option<u32> {
    let req = [0u8];
    let mut resp = [0u8; 4];
    sys::sleep();
    let start = sys::rdcycle64();
    for _ in 0..64 {
        let rc = sys::call6(
            abi::IPC_CALL,
            request_handle,
            req.as_ptr() as u32,
            1,
            resp.as_mut_ptr() as u32,
            resp.len() as u32,
            100,
        )
        .0;
        if rc != 1 || resp[0] != 0 {
            return None;
        }
    }
    let elapsed = sys::rdcycle64().wrapping_sub(start);
    Some(((elapsed + 32) >> 6) as u32)
}

fn run_bench(request_handle: u32) {
    let syscall_c = bench_syscall();
    let Some(switch_c) = bench_ctx_switch() else {
        sys::write(FAILED);
        return;
    };
    let Some(ipc_c) = bench_ipc_rtt(request_handle) else {
        sys::write(FAILED);
        return;
    };
    let mut cpu = [0u8; abi::STATS_CPU_BYTES];
    if sys::call(
        abi::STATS,
        cpu.as_mut_ptr() as u32,
        abi::STATS_CPU_BYTES as u32,
        abi::STATS_CPU,
        0,
    ) != 0
    {
        sys::write(FAILED);
        return;
    };
    let cpu_mhz = word_le(&cpu, 4);
    let lat_last_16m = word_le(&cpu, 48);
    let lat_max_16m = word_le(&cpu, 52);
    let lat_last_ns = ((u64::from(lat_last_16m) * 125 + 1) >> 1) as u32;
    let lat_max_ns = ((u64::from(lat_max_16m) * 125 + 1) >> 1) as u32;
    let lat_last_c = ((u64::from(lat_last_16m) * u64::from(cpu_mhz) + 8) >> 4) as u32;
    let lat_max_c = ((u64::from(lat_max_16m) * u64::from(cpu_mhz) + 8) >> 4) as u32;

    let mut out = Buffer::new();
    out.append(b"bench cpu_mhz=");
    out.decimal(cpu_mhz);
    out.append(b" syscall=");
    out.decimal(syscall_c);
    out.append(b"c ctx_switch=");
    out.decimal(switch_c);
    out.append(b"c ipc_rtt=");
    out.decimal(ipc_c);
    out.append(b"c\n");
    sys::write(out.as_slice());

    out.clear();
    out.append(b"irq_latency last=");
    out.decimal(lat_last_ns);
    out.append(b"ns(");
    out.decimal(lat_last_c);
    out.append(b"c) max=");
    out.decimal(lat_max_ns);
    out.append(b"ns(");
    out.decimal(lat_max_c);
    out.append(b"c)\n");
    sys::write(out.as_slice());
}

fn arena_probe() -> bool {
    let Ok(mut arena) = Arena::new(4096) else {
        return false;
    };
    let layout = core::alloc::Layout::from_size_align(31, 64).unwrap();
    let first = arena.allocate(layout);
    let second = arena.allocate(layout);
    let mut ok = false;
    if let (Some(a), Some(b)) = (first, second) {
        // SAFETY: distinct live allocations, both at least 31 writable bytes.
        unsafe {
            a.as_ptr().write_volatile(0x5a);
            b.as_ptr().write_volatile(0xa5);
            ok = a.as_ptr().read_volatile() == 0x5a && b.as_ptr().read_volatile() == 0xa5;
        }
        ok &= a.as_ptr() as usize % 64 == 0 && b.as_ptr() as usize % 64 == 0;
    }
    ok &= arena
        .allocate(core::alloc::Layout::from_size_align(4096, 1).unwrap())
        .is_none();
    let released = arena.release().is_ok();
    ok && released && arena.allocate(layout).is_none()
}

fn psram_probe() -> bool {
    let Ok(mut arena) = Arena::new_psram(128 * 1024) else {
        return false;
    };
    let in_pool = (0x8800_0000..0x8a00_0000).contains(&arena.base())
        && arena.size() == 128 * 1024
        && arena.base() % arena.size() == 0;
    let layout = core::alloc::Layout::from_size_align(64 * 1024, 64).unwrap();
    let first = arena.allocate(layout);
    let second = arena.allocate(layout);
    let mut ok = false;
    if let (Some(a), Some(b)) = (first, second) {
        // SAFETY: distinct 64 KiB PSRAM suballocations inside mapped 128 KiB NAPOT region.
        unsafe {
            let a32 = a.as_ptr() as *mut u32;
            let b32 = b.as_ptr() as *mut u32;
            a32.write_volatile(0x5a6b_7c8d);
            a32.add((64 * 1024 - 4) / 4).write_volatile(0x1357_9bdf);
            b32.write_volatile(0xa594_8372);
            b32.add((64 * 1024 - 4) / 4).write_volatile(0x2468_ace0);
            ok = a32.read_volatile() == 0x5a6b_7c8d
                && a32.add((64 * 1024 - 4) / 4).read_volatile() == 0x1357_9bdf
                && b32.read_volatile() == 0xa594_8372
                && b32.add((64 * 1024 - 4) / 4).read_volatile() == 0x2468_ace0;
        }
    }
    let cap = sys::call(abi::MEM_CAP, arena.base() as u32, 0, 0, 0);
    let released = arena.release().is_ok();
    in_pool && ok && (cap as i32) >= 0 && released
}

fn sentinel_data(sentinel_cap: u32, sequence: u32, data: &mut Buffer) -> (u32, u32) {
    let mut result = (0, 0);
    for _ in 0..8 {
        result = sys::call6(
            abi::SENTINEL_LOG,
            sentinel_cap,
            sequence,
            data.mut_ptr() as u32,
            24,
            0,
            0,
        );
        if result.0 != (-16i32) as u32 {
            break;
        }
        sys::sleep(); // bounded retry if automatic heartbeat currently owns the request slot
    }
    if result.0 == 24 {
        data.set_len(24);
    }
    result
}

fn sentinel_log(sentinel_cap: u32) {
    let mut data = Buffer::new();
    let (result, notices) = sentinel_data(sentinel_cap, 0, &mut data);
    if result != 24 {
        sys::write(FAILED);
        return;
    }
    let oldest = data.word(4);
    let newest = data.word(8);
    let mut out = Buffer::new();
    out.append(b"Sentinel log epoch=");
    out.decimal(data.word(0));
    out.append(b" hz=");
    out.decimal(data.word(20));
    out.push(b'\n');
    sys::write(out.as_slice());

    out.clear();
    out.append(b"oldest=");
    out.decimal(oldest);
    out.append(b" newest=");
    out.decimal(newest);
    out.append(b" overwritten=");
    out.decimal(data.word(12));
    out.append(b" capacity=");
    out.decimal(data.word(16));
    out.push(b'\n');
    sys::write(out.as_slice());

    out.clear();
    out.append(b" diagnostic_notices=");
    out.decimal(notices);
    out.push(b'\n');
    sys::write(out.as_slice());

    // Bounded snapshot tail; new arrivals never make this an infinite dump.
    for offset in 0..16u32 {
        let sequence = oldest.wrapping_add(offset);
        if oldest == 0 || sequence == 0 || sequence > newest {
            break;
        }
        let result = sentinel_data(sentinel_cap, sequence, &mut data).0;
        if result != 24 {
            if result == (-75i32) as u32 {
                sys::write(b"record overwritten during read; retry log\n");
            } else {
                sys::write(FAILED);
            }
            break;
        }
        out.clear();
        out.push(b'#');
        out.decimal(data.word(0));
        out.append(b" lp_ticks=");
        out.decimal64(data.word(8), data.word(12));
        let event_name: &[u8] = match data.word(4) {
            1 => b" boot",
            2 => b" first_heartbeat",
            3 => b" timeout",
            4 => b" recovered",
            5 => b" hp_diagnostic",
            6 => b" wake_armed",
            7 => b" wake_fired",
            8 => b" wake_cancelled",
            _ => b" unknown",
        };
        out.append(event_name);
        out.append(b" a0=");
        out.decimal(data.word(16));
        out.append(b" a1=");
        out.decimal(data.word(20));
        out.push(b'\n');
        sys::write(out.as_slice());
    }
}

fn wake_command(line: &Buffer, sentinel_cap: u32) -> bool {
    const PREPARE_PREFIX: &[u8] = b"sentinel prepare ";
    const SLEEP_PREFIX: &[u8] = b"sentinel sleep ";
    const ARM_PREFIX: &[u8] = b"sentinel arm ";
    const WAKE_PREFIX: &[u8] = b"sentinel wake ";

    let wait = line.eq_at(0, b"sentinel wait");
    let cancel = line.eq_at(0, b"sentinel cancel");
    let sleep = line.starts_with(SLEEP_PREFIX);
    let prepare = sleep || line.starts_with(PREPARE_PREFIX);
    let arm = prepare || line.starts_with(ARM_PREFIX);
    let wake = sleep || line.starts_with(WAKE_PREFIX);
    if !wait && !cancel && !arm && !wake {
        return false;
    }
    let number = if cancel {
        abi::SENTINEL_CANCEL_WAKE
    } else if wait {
        abi::SENTINEL_WAIT_WAKE
    } else if prepare {
        abi::SENTINEL_PREPARE_SLEEP
    } else {
        abi::SENTINEL_ARM_WAKE
    };
    let argument = if arm || wake {
        let prefix_len = if sleep {
            SLEEP_PREFIX.len()
        } else if prepare {
            PREPARE_PREFIX.len()
        } else if arm {
            ARM_PREFIX.len()
        } else {
            WAKE_PREFIX.len()
        };
        let Some(ms) = line.delay_ms(prefix_len) else {
            sys::write(b"wake delay must be 1..60000 ms\n");
            return true;
        };
        ms
    } else {
        0
    };
    let mut result = (0, 0);
    for _ in 0..8 {
        result = sys::call6(number, sentinel_cap, argument, 0, 0, 0, 0);
        if result.0 != (-16i32) as u32 {
            break;
        }
        sys::sleep();
    }
    if wake && (result.0 as i32) > 0 {
        result = sys::call6(abi::SENTINEL_WAIT_WAKE, sentinel_cap, result.0, 0, 0, 0, 0);
    }
    if (result.0 as i32) <= 0 {
        let message: &[u8] = match result.0 as i32 {
            -16 if prepare => b"sleep busy; stop active display/DMA or cancel prior alarm\n",
            -16 => b"wake busy; collect or cancel prior alarm\n",
            -2 => b"no pending wake\n",
            -114 => b"wake already fired; use sentinel wait\n",
            -110 => b"wake timeout; LP slot retained\n",
            _ => FAILED,
        };
        sys::write(message);
    } else if cancel {
        sys::write(b"Sentinel wake cancelled\n");
    } else {
        let mut out = Buffer::new();
        if sleep {
            out.append(b"Sentinel sleep WAKE_EVENT token=");
        } else if prepare {
            out.append(b"Sentinel sleep prepared token=");
        } else if arm {
            out.append(b"Sentinel wake armed token=");
        } else {
            out.append(b"Sentinel WAKE_EVENT token=");
        }
        out.decimal(result.0);
        if !arm || sleep {
            out.append(b" elapsed_ticks=");
            out.decimal(result.1);
        }
        out.push(b'\n');
        sys::write(out.as_slice());
    }
    true
}

/// Load `f0 = base`, `f1 = step`, `q0 = [a; 8]`, `q1 = [b; 8]`, and `XACC = 0`.
/// Requires `scratch` to be a 16-byte aligned 32-byte buffer in internal L2MEM SRAM.
#[inline(never)]
unsafe fn fpu_pie_seed(scratch: *mut u32, f_base_bits: u32, f_step_bits: u32, q_a: i16, q_b: i16) {
    let pack = |v: i16| -> u32 { (v as u16 as u32) | ((v as u16 as u32) << 16) };
    unsafe {
        for i in 0..4 {
            scratch.add(i).write_volatile(pack(q_a));
            scratch.add(4 + i).write_volatile(pack(q_b));
        }
        let p = scratch as usize;
        core::arch::asm!(
            ".option push",
            ".option arch, +f",
            "fmv.w.x f0, a1",
            "fmv.w.x f1, a2",
            ".option pop",
            ".word 0x0201223b", // esp.vld.128.ip q0, a0, 16
            ".word 0x0201263b", // esp.vld.128.ip q1, a0, 16
            ".word 0x0000005b", // esp.zero.xacc
            inout("a0") p => _,
            in("a1") f_base_bits,
            in("a2") f_step_bits,
            options(nostack)
        );
    }
}

/// Execute one step of in-register `FPU` (`f0 += f1`) and `PIE` (`q0 += q1`, `XACC += dot(q1, q1)`).
#[inline(never)]
unsafe fn fpu_pie_step() {
    unsafe {
        core::arch::asm!(
            ".option push",
            ".option arch, +f",
            "fadd.s f0, f0, f1",
            ".option pop",
            ".word 0x0684065f", // esp.vadd.s16 q0, q0, q1
            ".word 0x26c3005f", // esp.vmulas.s16.xacc q1, q1
            options(nomem, nostack)
        );
    }
}

/// Read back live `f0` bits, `q0` first word (`s16x2`), and `XACC_L` after context switches / migrations.
#[inline(never)]
unsafe fn fpu_pie_readout(scratch: *mut u32) -> (u32, u32, u32) {
    unsafe {
        let p = scratch as usize;
        let f0_bits: u32;
        let xacc_lo: u32;
        core::arch::asm!(
            ".option push",
            ".option arch, +f",
            "fmv.x.w a2, f0",
            ".option pop",
            ".word 0x8201223b", // esp.vst.128.ip q0, a0, 16
            ".word 0x84b001df", // esp.movx.r.xacc.l a1
            "fence rw, rw",
            inout("a0") p => _,
            out("a1") xacc_lo,
            out("a2") f0_bits,
            options(nostack)
        );
        (f0_bits, scratch.read_volatile(), xacc_lo)
    }
}

extern "C" fn smp_worker(state_ptr: u32) -> ! {
    let state = state_ptr as *mut u32;
    let worker_scratch = (state_ptr + 64) as *mut u32;
    unsafe {
        let (_, hart0) = sys::call6(abi::THREAD_AFFINITY, 0, 0, 0, 0, 0, 0);
        state.add(1).write_volatile(hart0);
        // Seed live FPU (f0=10.0, f1=0.5) and PIE (q0=[10;8], q1=[2;8], XACC=0) on HP1.
        fpu_pie_seed(worker_scratch, 0x4120_0000, 0x3f00_0000, 10, 2);
        for i in 0..16u32 {
            fpu_pie_step();
            state.write_volatile(i + 1);
            sys::call(abi::YIELD, 0, 0, 0, 0);
        }
        // Migrate worker HP1 -> HP0 while keeping FPU (f0, f1) and PIE (q0, q1, XACC) live!
        let _ = sys::call6(abi::THREAD_AFFINITY, 0, abi::AFFINITY_HP0, 0, 0, 0, 0);
        let (_, hart_m0) = sys::call6(abi::THREAD_AFFINITY, 0, 0, 0, 0, 0, 0);
        state.add(2).write_volatile(hart_m0);
        for i in 16..24u32 {
            fpu_pie_step();
            state.write_volatile(i + 1);
            sys::call(abi::YIELD, 0, 0, 0, 0);
        }
        // Migrate worker HP0 -> HP1 and finish the remaining 8 FPU + PIE iterations!
        let _ = sys::call6(abi::THREAD_AFFINITY, 0, abi::AFFINITY_HP1, 0, 0, 0, 0);
        let (_, hart_m1) = sys::call6(abi::THREAD_AFFINITY, 0, 0, 0, 0, 0, 0);
        state.add(3).write_volatile(hart_m1);
        for i in 24..32u32 {
            fpu_pie_step();
            state.write_volatile(i + 1);
            sys::call(abi::YIELD, 0, 0, 0, 0);
        }
        // After 32 steps across HP1 -> HP0 -> HP1:
        // f0 = 10.0 + 32 * 0.5 = 26.0 (0x41d0_0000)
        // q0 = [10 + 32 * 2; 8] = [74; 8] (word0 = 0x004a_004a)
        // XACC = 32 * (8 * 2 * 2) = 1024
        let (f0_bits, q0_word, xacc_lo) = fpu_pie_readout(worker_scratch);
        let ok = f0_bits == 0x41d0_0000 && q0_word == 0x004a_004a && xacc_lo == 1024;
        state.add(5).write_volatile(u32::from(ok));
        state.add(4).write_volatile(1);
    }
    // Stay briefly in U-mode on HP1 so the parent on HP0 can trigger a live IPI PMP shootdown
    // on a separate shared page while this thread is actively executing on HP1.
    for _ in 0..16 {
        sys::call(abi::YIELD, 0, 0, 0, 0);
    }
    sys::exit();
}

fn run_smp_test() {
    let (prev_aff, _) = sys::call6(abi::THREAD_AFFINITY, 0, abi::AFFINITY_HP0, 0, 0, 0, 0);
    let (base, size) = sys::call6(abi::MEM_ALLOC, 4096, 0, 0, 0, 0, 0);
    if (base as i32) < 0 || size < 4096 {
        let _ = sys::call6(abi::THREAD_AFFINITY, 0, prev_aff.clamp(1, 3), 0, 0, 0, 0);
        sys::write(FAILED);
        return;
    }
    let (scratch_base, scratch_size) = sys::call6(abi::MEM_ALLOC, 4096, 0, 0, 0, 0, 0);
    if (scratch_base as i32) < 0 || scratch_size < 4096 {
        let _ = sys::call(abi::MEM_FREE, base, size, 0, 0);
        let _ = sys::call6(abi::THREAD_AFFINITY, 0, prev_aff.clamp(1, 3), 0, 0, 0, 0);
        sys::write(FAILED);
        return;
    }
    let state_ptr = base as *mut u32;
    let parent_scratch = (base + 128) as *mut u32;
    unsafe {
        for i in 0..16 {
            state_ptr.add(i).write_volatile(0);
        }
    }
    let tid = sys::call6(
        abi::THREAD_CREATE,
        smp_worker as *const () as usize as u32,
        base,
        size,
        base,
        1,
        abi::AFFINITY_HP1,
    )
    .0;
    if (tid as i32) <= 0 {
        let _ = sys::call(abi::MEM_FREE, scratch_base, scratch_size, 0, 0);
        let _ = sys::call(abi::MEM_FREE, base, size, 0, 0);
        let _ = sys::call6(abi::THREAD_AFFINITY, 0, prev_aff.clamp(1, 3), 0, 0, 0, 0);
        sys::write(FAILED);
        return;
    }
    // Concurrently exercise FPU (f0=100.0, f1=1.25) and PIE (q0=[100;8], q1=[3;8]) on HP0
    // while the worker thread migrates HP1 -> HP0 -> HP1 and preempts/interleaves on HP0!
    let mut parent_steps = 0u32;
    unsafe {
        fpu_pie_seed(parent_scratch, 0x42c8_0000, 0x3fa0_0000, 100, 3);
    }
    for _ in 0..128 {
        if parent_steps < 16 {
            unsafe { fpu_pie_step() };
            parent_steps += 1;
        }
        if unsafe { state_ptr.add(4).read_volatile() } != 0 && parent_steps == 16 {
            break;
        }
        sys::call(abi::YIELD, 0, 0, 0, 0);
    }
    while parent_steps < 16 {
        unsafe { fpu_pie_step() };
        parent_steps += 1;
    }
    let (p_f0, p_q0, p_xacc) = unsafe { fpu_pie_readout(parent_scratch) };
    // Parent after 16 steps: f0 = 100.0 + 16 * 1.25 = 120.0 (0x42f0_0000),
    // q0 = [100 + 16 * 3; 8] = [148; 8] (0x0094_0094), XACC = 16 * (8 * 3 * 3) = 1152.
    let parent_ext_ok = p_f0 == 0x42f0_0000 && p_q0 == 0x0094_0094 && p_xacc == 1152;
    // Free scratch_base while worker is active on HP1 -> triggers synchronous IPI PMP shootdown!
    let _ = sys::call(abi::MEM_FREE, scratch_base, scratch_size, 0, 0);
    let iters = unsafe { state_ptr.read_volatile() };
    let start_hart = unsafe { state_ptr.add(1).read_volatile() };
    let mig_hp0 = unsafe { state_ptr.add(2).read_volatile() };
    let mig_hp1 = unsafe { state_ptr.add(3).read_volatile() };
    let worker_ext_ok = unsafe { state_ptr.add(5).read_volatile() } == 1;
    let mut freed = false;
    for _ in 0..32 {
        sys::call(abi::YIELD, 0, 0, 0, 0);
        if sys::call(abi::MEM_FREE, base, size, 0, 0) == 0 {
            freed = true;
            break;
        }
    }
    let _ = sys::call6(abi::THREAD_AFFINITY, 0, prev_aff.clamp(1, 3), 0, 0, 0, 0);
    let mut cpu = [0u8; abi::STATS_CPU_SMP_BYTES];
    if !freed
        || sys::call(
            abi::STATS,
            cpu.as_mut_ptr() as u32,
            abi::STATS_CPU_SMP_BYTES as u32,
            abi::STATS_CPU,
            0,
        ) != 0
    {
        sys::write(FAILED);
        return;
    }
    let harts_online = word_le(&cpu, 72);
    let ipi_total = word_le(&cpu, 112);
    let shootdowns = word_le(&cpu, 120);
    let mut out = Buffer::new();
    out.append(b"smp harts=");
    out.decimal(harts_online);
    out.append(b" worker_tid=");
    out.decimal(tid);
    out.append(b" iters=");
    out.decimal(iters);
    out.append(b" path=hp");
    out.decimal(start_hart);
    out.append(b"->hp");
    out.decimal(mig_hp0);
    out.append(b"->hp");
    out.decimal(mig_hp1);
    out.append(b" ipi=");
    out.decimal(ipi_total);
    out.append(b" shootdowns=");
    out.decimal(shootdowns);
    if parent_ext_ok && worker_ext_ok {
        out.append(b" fpu_pie=ok\n");
    } else {
        out.append(b" fpu_pie=FAIL\n");
    }
    sys::write(out.as_slice());
}

fn smp_command(text: &[u8]) -> bool {
    if text == b"smp" {
        run_smp_test();
        return true;
    }
    let mask = match text {
        b"smp status" => 0,
        b"smp pin 0" => abi::AFFINITY_HP0,
        b"smp pin 1" => abi::AFFINITY_HP1,
        b"smp any" => abi::AFFINITY_ANY,
        _ => return false,
    };
    let (aff, hart) = sys::call6(abi::THREAD_AFFINITY, 0, mask, 0, 0, 0, 0);
    if (aff as i32) <= 0 {
        sys::write(FAILED);
        return true;
    }
    let mut out = Buffer::new();
    out.append(b"shell hart=hp");
    out.decimal(hart);
    out.append(b" affinity=");
    out.decimal(aff);
    out.push(b'\n');
    sys::write(out.as_slice());
    true
}

fn freq_command(text: &[u8], sentinel_cap: u32) -> bool {
    let mode = match text {
        b"freq" => abi::FREQ_MODE_QUERY,
        b"freq auto" => abi::FREQ_MODE_AUTO,
        b"freq 40" => abi::FREQ_MODE_40MHZ,
        b"freq 180" => abi::FREQ_MODE_180MHZ,
        _ => return false,
    };
    let (cpu_mhz, active_mode) = sys::call6(abi::CLOCK_CONTROL, sentinel_cap, mode, 0, 0, 0, 0);
    if (cpu_mhz as i32) <= 0 {
        if cpu_mhz as i32 == -16 {
            sys::write(b"freq busy; stop active display/DMA before downclocking to 40 MHz\n");
        } else {
            sys::write(FAILED);
        }
        return true;
    }
    let mode_name: &[u8] = match active_mode {
        abi::FREQ_MODE_40MHZ => b"pin_40mhz",
        abi::FREQ_MODE_180MHZ => b"pin_180mhz",
        _ => b"auto",
    };
    let mut out = Buffer::new();
    out.append(b"cpu_mhz=");
    out.decimal(cpu_mhz);
    out.append(b" mode=");
    out.append(mode_name);
    out.push(b'\n');
    sys::write(out.as_slice());
    true
}

fn command(line: &Buffer, caps: Caps) {
    let text = line.as_slice();
    if text.is_empty() {
        return;
    }
    if wake_command(line, caps.sentinel) || freq_command(text, caps.sentinel) || smp_command(text) {
        return;
    }
    match text {
        b"help" => {
            sys::write(
                b"help | cat <path> | uptime | bench | smp [status/pin 0/pin 1/any] | freq [auto/40/180] | sdcard | run hello/pic/pic-psram/dma | memtest\n",
            );
            sys::write(
                b"sentinel [hello/status/pause/resume/log/mark] | sentinel sleep/wake/arm/prepare <ms> | sentinel wait/cancel | display test/orbit/pixel/diag/boot/off\n",
            );
        }
        b"sdcard" => {
            let rc = sys::call(
                abi::DISPLAY_TEST,
                caps.display,
                abi::DISPLAY_MODE_SDCARD_PROBE,
                0,
                0,
            );
            if rc == 0 {
                sys::write(b"microSD assets verified\n");
            } else {
                let mut out = Buffer::new();
                out.append(b"sdcard error ");
                out.decimal(0u32.wrapping_sub(rc));
                out.push(b'\n');
                sys::write(out.as_slice());
            }
        }
        b"display off" => {
            let rc = sys::call(abi::DISPLAY_TEST, caps.display, abi::DISPLAY_MODE_OFF, 0, 0);
            if rc == 0 {
                sys::write(b"Display scanout stopped and framebuffer released\n");
            } else {
                sys::write(FAILED);
            }
        }
        b"display test" | b"display orbit" | b"display pixel" | b"display diag"
        | b"display boot" => {
            let (mode, ok_msg): (u32, &[u8]) = match text {
                b"display orbit" => (abi::DISPLAY_MODE_ORBIT, b"Orbit scanout active\n"),
                b"display pixel" => (abi::DISPLAY_MODE_PIXEL, b"Pixel scanout active (360,640)\n"),
                b"display diag" => (abi::DISPLAY_MODE_DIAG, b"Diagnostic scanout active\n"),
                b"display boot" => (abi::DISPLAY_MODE_BOOT, b"Boot image scanout active\n"),
                _ => (abi::DISPLAY_MODE_BARS, b"ST7121 DSI bars enabled\n"),
            };
            let mut rc = sys::call(abi::DISPLAY_TEST, caps.display, mode, 0, 0);
            while (rc as i32) > 0 {
                sys::sleep_ticks(rc);
                rc = sys::call(
                    abi::DISPLAY_TEST,
                    caps.display,
                    abi::DISPLAY_MODE_STEP,
                    0,
                    0,
                );
            }
            if rc == 0 {
                sys::write(ok_msg);
            } else {
                let mut out = Buffer::new();
                out.append(b"display error ");
                out.decimal(0u32.wrapping_sub(rc));
                out.push(b'\n');
                sys::write(out.as_slice());
            }
        }
        b"run hello" | b"run pic" | b"run pic-psram" | b"run dma" => {
            let selector = match text {
                b"run pic" => 1,
                b"run dma" => 2,
                b"run pic-psram" => 3,
                _ => 0,
            };
            if (sys::call(abi::SPAWN_HELLO, caps.spawn, selector, 0, 0) as i32) < 0 {
                sys::write(FAILED);
            }
        }
        b"sentinel log" => sentinel_log(caps.sentinel),
        b"sentinel mark" => {
            let ticks = sys::call(abi::TICKS, 0, 0, 0, 0);
            let seq = sys::call(abi::SENTINEL_DIAG, caps.sentinel, 0x4d41_524b, ticks, 0);
            if (seq as i32) > 0 {
                let mut out = Buffer::new();
                out.append(b"Sentinel ACK seq ");
                out.decimal(seq);
                out.push(b'\n');
                sys::write(out.as_slice());
            } else {
                sys::write(FAILED);
            }
        }
        b"sentinel pause" | b"sentinel resume" => {
            let resume = text == b"sentinel resume";
            if sys::call(
                abi::SENTINEL_CONTROL,
                caps.sentinel,
                u32::from(resume),
                0,
                0,
            ) == 0
            {
                if resume {
                    sys::write(b"Sentinel heartbeat resumed\n");
                } else {
                    sys::write(b"Sentinel heartbeat paused\n");
                }
            } else {
                sys::write(FAILED);
            }
        }
        b"sentinel status" => {
            let mut data = Buffer::new();
            let (result, notices) = sys::call6(
                abi::SENTINEL_QUERY,
                caps.sentinel,
                data.mut_ptr() as u32,
                24,
                0,
                0,
                0,
            );
            if result == 24 {
                data.set_len(24);
                let mut out = Buffer::new();
                out.append(b"Sentinel state=");
                match data.word(0) {
                    0 => out.append(b"unarmed"),
                    1 => out.append(b"healthy"),
                    3 => out.append(b"prepared"),
                    _ => out.append(b"missed"),
                }
                out.append(b" age_ms=");
                out.decimal64(data.word(4), data.word(8));
                out.append(b" lp_hz=");
                out.decimal(data.word(12));
                out.append(b" beats=");
                out.decimal(data.word(16));
                out.append(b" missed=");
                out.decimal(data.word(20));
                out.append(b" notices=");
                out.decimal(notices);
                out.push(b'\n');
                sys::write(out.as_slice());
            } else {
                sys::write(FAILED);
            }
        }
        b"sentinel hello" => {
            let seq = sys::call(abi::SENTINEL_HELLO, caps.sentinel, 0, 0, 0);
            if (seq as i32) > 0 {
                let mut out = Buffer::new();
                out.append(b"Sentinel ACK seq ");
                out.decimal(seq);
                out.push(b'\n');
                sys::write(out.as_slice());
            } else {
                sys::write(FAILED);
            }
        }
        b"sentinel" => {
            let (version, epoch) = sys::call6(abi::SENTINEL_STATUS, caps.sentinel, 0, 0, 0, 0, 0);
            let mut out = Buffer::new();
            if version == (-110i32) as u32 {
                out.append(b"Sentinel heartbeat timeout, epoch ");
            } else if version != 0 {
                out.append(b"Sentinel READY v");
                out.decimal(version);
                out.append(b", epoch ");
            } else {
                out.append(b"Sentinel pending, epoch ");
            }
            out.decimal(epoch);
            out.push(b'\n');
            sys::write(out.as_slice());
        }
        b"memtest" => {
            if arena_probe() && psram_probe() {
                sys::write(b"arena: SRAM + PSRAM aligned allocations, bounds, release OK\n");
            } else {
                sys::write(b"arena: FAIL\n");
            }
        }
        b"bench" => run_bench(caps.request),
        b"uptime" => read_file(b"/proc/uptime", caps.request),
        _ if text.len() > 4 && text.starts_with(b"cat ") => {
            read_file(&text[4..], caps.request);
        }
        _ => sys::write(b"unknown command (try help)\n"),
    }
}

fn main(
    _reply_handle: u32,
    request_handle: u32,
    input_handle: u32,
    spawn_cap: u32,
    display_cap: u32,
    sentinel_cap: u32,
) -> ! {
    let caps = Caps {
        request: request_handle,
        spawn: spawn_cap,
        display: display_cap,
        sentinel: sentinel_cap,
    };
    let mut line = Buffer::new();
    let mut skip_lf = false;
    let mut overflow = false;
    sys::write(b"vela:/$ ");
    loop {
        let mut input = Buffer::new();
        if !input.receive(input_handle) {
            sys::exit();
        }
        for i in 0..input.len() {
            let byte = input.get(i);
            if byte == b'\n' && skip_lf {
                skip_lf = false;
                continue;
            }
            skip_lf = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                sys::write(b"\n");
                if overflow {
                    sys::write(b"line too long\n");
                } else {
                    command(&line, caps);
                }
                line.clear();
                overflow = false;
                sys::write(b"vela:/$ ");
            } else if byte == 8 || byte == 127 {
                if line.pop() {
                    sys::write(b"\x08 \x08");
                }
            } else if (32..127).contains(&byte) {
                if line.len() < 80 {
                    line.push(byte);
                    sys::write(core::slice::from_ref(&byte));
                } else {
                    overflow = true;
                }
            }
        }
    }
}
