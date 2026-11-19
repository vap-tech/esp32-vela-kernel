#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, TextBuf, div64, dword_le, sys, word_le};

vela_user::entry!(main);

fn ratio_bps(mut busy: u64, mut denom: u64) -> u32 {
    if denom < busy {
        denom = busy;
    }
    if denom == 0 {
        return 0;
    }
    while denom > 400_000 {
        busy >>= 1;
        denom >>= 1;
    }
    let b = busy as u32;
    let d = (denom as u32).max(1);
    (b * 10_000 + d / 2) / d
}

fn ticks_16m_to_ns(ticks_16m: u32) -> (u32, u32) {
    let ns = (u64::from(ticks_16m) * 125 + 1) >> 1;
    (ns as u32, (ns >> 32) as u32)
}

/// Render a virtual `/proc/*` file into `out`.
/// Returns `0` on success, `1` on ENOENT, `2` on kernel query failure.
fn render_path(path: &[u8], out: &mut TextBuf<384>) -> u8 {
    match path {
        b"/proc/loadavg" | b"/proc/uptime" => {
            let mut snap = [0u8; abi::STATS_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_BYTES as u32,
                abi::STATS_SUMMARY,
                0,
            ) != 0
            {
                out.append(b"service unavailable\n");
                return 2;
            }
            if path == b"/proc/loadavg" {
                for offset in [32, 36, 40] {
                    let hundredths = (word_le(&snap, offset) * 100 + 32768) >> 16;
                    out.hundredths(hundredths);
                    out.push(b' ');
                }
                out.decimal(word_le(&snap, 4));
                out.push(b'/');
                out.decimal(word_le(&snap, 0));
                out.push(b' ');
                out.decimal(word_le(&snap, 12));
                out.push(b'\n');
            } else {
                let (lo, hi) = dword_le(&snap, 24);
                let (sec_lo, sec_hi, rem) = div64(lo, hi, 100);
                out.decimal64(sec_lo, sec_hi);
                out.push(b'.');
                out.push(b'0' + (rem / 10) as u8);
                out.push(b'0' + (rem % 10) as u8);
                out.push(b'\n');
            }
            0
        }
        b"/proc/interrupts" => {
            let mut snap = [0u8; abi::STATS_IRQ_ALL_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_IRQ_ALL_BYTES as u32,
                abi::STATS_IRQ,
                0,
            ) != 0
            {
                out.append(b"service unavailable\n");
                return 2;
            }
            let names: [&[u8]; 4] = [
                b"systimer0 ",
                b"usb_serial_jtag_rx ",
                b"gdma_ahb_ch1 ",
                b"lp_sentinel ",
            ];
            for (i, name) in names.into_iter().enumerate() {
                let (lo, hi) = dword_le(&snap, i * 8);
                out.append(name);
                out.decimal64(lo, hi);
                out.push(b'\n');
            }
            0
        }
        b"/proc/cpuinfo" => {
            let mut snap = [0u8; abi::STATS_CPU_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_CPU_BYTES as u32,
                abi::STATS_CPU,
                0,
            ) != 0
            {
                out.append(b"service unavailable\n");
                return 2;
            }
            let hart = word_le(&snap, 0);
            let cpu_mhz = word_le(&snap, 4);
            let apb_mhz = word_le(&snap, 8);
            let lp_hz = word_le(&snap, 12);
            let (u_lo, u_hi) = dword_le(&snap, 16);
            let (k_lo, k_hi) = dword_le(&snap, 24);
            let (i_lo, i_hi) = dword_le(&snap, 32);
            let (c_lo, c_hi) = dword_le(&snap, 40);
            let lat_last = word_le(&snap, 48);
            let lat_max = word_le(&snap, 52);

            let user = (u64::from(u_hi) << 32) | u64::from(u_lo);
            let kernel = (u64::from(k_hi) << 32) | u64::from(k_lo);
            let cap = (u64::from(c_hi) << 32) | u64::from(c_lo);
            let busy_bps = ratio_bps(user.saturating_add(kernel), cap);

            out.append(b"hart ");
            out.decimal(hart);
            out.append(b"\nisa rv32imac\ncpu_mhz ");
            out.decimal(cpu_mhz);
            out.append(b"\napb_mhz ");
            out.decimal(apb_mhz);
            out.append(b"\nlp_hz ");
            out.decimal(lp_hz);
            out.append(b"\nbusy_pct ");
            out.hundredths(busy_bps);
            out.append(b"\nuser_cycles ");
            out.decimal64(u_lo, u_hi);
            out.append(b"\nkernel_cycles ");
            out.decimal64(k_lo, k_hi);
            out.append(b"\nidle_cycles ");
            out.decimal64(i_lo, i_hi);
            out.append(b"\ncapacity_cycles ");
            out.decimal64(c_lo, c_hi);
            out.append(b"\nirq_latency_ns ");
            let (last_lo, last_hi) = ticks_16m_to_ns(lat_last);
            out.decimal64(last_lo, last_hi);
            out.append(b" max ");
            let (max_lo, max_hi) = ticks_16m_to_ns(lat_max);
            out.decimal64(max_lo, max_hi);
            out.push(b'\n');
            0
        }
        b"/proc/tasks" => {
            let mut snap = [0u8; abi::STATS_TASKS_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_TASKS_BYTES as u32,
                abi::STATS_TASKS,
                0,
            ) != 0
            {
                out.append(b"service unavailable\n");
                return 2;
            }
            out.append(b"tid pid state prio ticks user_cycles preempt stack\n");
            let stride = abi::STATS_TASK_WORDS * 4;
            for slot in 0..8 {
                let base = slot * stride;
                let tid = word_le(&snap, base);
                if tid == 0 {
                    continue;
                }
                let pid = word_le(&snap, base + 4);
                let meta = word_le(&snap, base + 8);
                let stack = word_le(&snap, base + 12);
                let preempt = word_le(&snap, base + 16);
                let (t_lo, t_hi) = dword_le(&snap, base + 20);
                let (c_lo, c_hi) = dword_le(&snap, base + 28);

                let state: &[u8] = match meta & 0xff {
                    1 => b"run",
                    2 => b"ready",
                    3 => b"sleep",
                    4 => b"recv",
                    5 => b"reply",
                    6 => b"event",
                    _ => b"dev",
                };
                let prio = (meta >> 8) & 0xff;
                let eff_prio = (meta >> 16) & 0xff;

                out.decimal(tid);
                out.push(b' ');
                out.decimal(pid);
                out.push(b' ');
                out.append(state);
                out.push(b' ');
                out.decimal(prio);
                out.push(b'/');
                out.decimal(eff_prio);
                out.push(b' ');
                out.decimal64(t_lo, t_hi);
                out.push(b' ');
                out.decimal64(c_lo, c_hi);
                out.push(b' ');
                out.decimal(preempt);
                out.push(b' ');
                out.decimal(stack);
                out.push(b'\n');
            }
            0
        }
        b"/proc/streams" => {
            let mut snap = [0u8; abi::STATS_STREAMS_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_STREAMS_BYTES as u32,
                abi::STATS_STREAMS,
                0,
            ) != 0
            {
                out.append(b"service unavailable\n");
                return 2;
            }
            let scan_state = word_le(&snap, 0);
            let active = scan_state & 1;
            let mode: &[u8] = match (scan_state >> 8) & 0xff {
                1 => b"bars",
                2 => b"orbit",
                3 => b"pixel",
                4 => b"diag",
                5 => b"boot",
                _ => b"none",
            };
            let frames = word_le(&snap, 4);
            let underruns = word_le(&snap, 8);
            let dma_errors = word_le(&snap, 12);
            let fifo = word_le(&snap, 16);
            let gdma_done = word_le(&snap, 20);
            let gdma_err = word_le(&snap, 24);
            let (b_lo, b_hi) = dword_le(&snap, 28);

            out.append(b"dsi_scanout active=");
            out.decimal(active);
            out.append(b" mode=");
            out.append(mode);
            out.append(b" frames=");
            out.decimal(frames);
            out.append(b" fifo=");
            out.decimal(fifo);
            out.append(b" underruns=");
            out.decimal(underruns);
            out.append(b" dma_errors=");
            out.decimal(dma_errors);
            out.append(b"\ngdma_ahb_ch1 completed=");
            out.decimal(gdma_done);
            out.append(b" bytes=");
            out.decimal64(b_lo, b_hi);
            out.append(b" errors=");
            out.decimal(gdma_err);
            out.push(b'\n');
            0
        }
        _ => {
            out.append(b"procfs: no such file\n");
            1
        }
    }
}

struct SnapshotCache {
    path: [u8; 32],
    path_len: usize,
    status: u8,
    text: TextBuf<384>,
}

impl SnapshotCache {
    const fn new() -> Self {
        Self {
            path: [0; 32],
            path_len: 0,
            status: 1,
            text: TextBuf::new(),
        }
    }

    fn render_or_reuse(&mut self, offset: usize, path: &[u8]) -> (u8, &[u8]) {
        let same_path =
            offset > 0 && path.len() == self.path_len && self.path[..self.path_len] == *path;
        if !same_path {
            self.text.clear();
            self.status = render_path(path, &mut self.text);
            if path.len() <= self.path.len() {
                self.path[..path.len()].copy_from_slice(path);
                self.path_len = path.len();
            } else {
                self.path_len = 0;
            }
        }
        (self.status, self.text.as_slice())
    }
}

/// Handle `op=0` (RPC ping), `op=1` (legacy single-frame read), and `op=2`
/// (`[2, offset_lo, offset_hi, path...]` -> `[status, more, slice...]`).
fn handle_request(request: &Buffer, response: &mut Buffer, cache: &mut SnapshotCache) {
    response.clear();
    let req = request.as_slice();
    match req.first().copied() {
        Some(0) if req.len() == 1 => {
            response.push(0);
        }
        Some(1) if req.len() > 1 => {
            let (status, bytes) = cache.render_or_reuse(0, &req[1..]);
            response.push(status);
            response.append(bytes);
        }
        Some(2) if req.len() > 3 => {
            let offset = u16::from_le_bytes([req[1], req[2]]) as usize;
            let (status, bytes) = cache.render_or_reuse(offset, &req[3..]);
            let start = offset.min(bytes.len());
            let end = (start + (abi::IPC_BYTES - 2)).min(bytes.len());
            let more = u8::from(status == 0 && end < bytes.len());
            response.push(status);
            response.push(more);
            response.append(&bytes[start..end]);
        }
        _ => {
            response.push(2);
            response.append(b"procfs: bad request\n");
        }
    }
}

fn main(request_handle: u32, reply_handle: u32, _a2: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    let mut cache = SnapshotCache::new();
    loop {
        let mut request = Buffer::new();
        if !request.receive(request_handle) {
            sys::exit();
        }
        let mut response = Buffer::new();
        handle_request(&request, &mut response, &mut cache);
        if request.reply != 0 {
            // A timed-out client invalidates its one-shot reply token. Keep serving.
            let _ = sys::call(
                abi::IPC_REPLY,
                request.reply,
                response.ptr() as u32,
                response.len() as u32,
                0,
            );
        } else if !response.send(reply_handle) {
            sys::exit();
        }
    }
}
