#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, TextBuf, div64, dword_le, i2c, ina226, kbd, sys, word_le};

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

/// Render a virtual `/proc/*` or `/dev/*` file into `out`.
/// Returns `0` on success, `1` on ENOENT, `2` on kernel/device query failure.
/// Capability handles for the board I2C buses (`vela_abi::syscall::I2C_BUS_*`).
#[derive(Clone, Copy)]
struct I2cCaps {
    system: u32,
    ext1: u32,
}

impl I2cCaps {
    fn bus(self, id: u8) -> Option<u32> {
        match id {
            abi::I2C_BUS_SYSTEM => Some(self.system),
            abi::I2C_BUS_EXT1 => Some(self.ext1),
            _ => None,
        }
    }
}

/// `kbdled [args]`: show or change the keyboard's RGB LEDs. Returns the status code.
fn kbd_led(args: &[u8], ext1: u32, out: &mut TextBuf<512>) -> u8 {
    let mut bus = kbd::SysBus(ext1);
    let has_args = args.iter().any(|&c| c != b' ');
    if has_args {
        let Some(command) = kbd::parse_led_command(args) else {
            out.append(b"kbd led: bad arguments (auto | off | bright 0..100 | R G B [R G B])\n");
            return 1;
        };
        if kbd::apply_led(&mut bus, command).is_err() {
            out.append(b"kbd led: no keyboard\n");
            return 2;
        }
    }
    let Ok(status) = kbd::read_led_status(&mut bus) else {
        out.append(b"kbd led: no keyboard\n");
        return 2;
    };
    out.append(if status.custom {
        b"kbd led: mode=custom bright="
    } else {
        b"kbd led: mode=auto bright="
    });
    out.decimal(u32::from(status.brightness));
    for (index, color) in status.colors.iter().enumerate() {
        out.append(if index == 0 { b" rgb1=" } else { b" rgb2=" });
        for channel in [color.r, color.g, color.b] {
            for nibble in [channel >> 4, channel & 15] {
                out.push(b"0123456789abcdef"[usize::from(nibble)]);
            }
        }
    }
    out.push(b'\n');
    0
}

fn render_path(path: &[u8], i2c: I2cCaps, out: &mut TextBuf<512>) -> u8 {
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
            let mut snap = [0u8; abi::STATS_IRQ_FULL_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_IRQ_FULL_BYTES as u32,
                abi::STATS_IRQ,
                0,
            ) != 0
            {
                out.append(b"service unavailable\n");
                return 2;
            }
            let names: [&[u8]; 10] = [
                b"systimer0 ",
                b"usb_serial_jtag_rx ",
                b"gdma_ahb_ch1 ",
                b"lp_sentinel ",
                b"ipi_hp0 ",
                b"ipi_hp1 ",
                b"pmp_shootdowns ",
                b"kbd_gpio50 ",
                b"kbd_arm_low ",
                b"kbd_arm_wait ",
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
            let mut snap = [0u8; abi::STATS_CPU_SMP_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_CPU_SMP_BYTES as u32,
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
            let freq_meta = word_le(&snap, 56);
            let tickless_entries = word_le(&snap, 60);
            let (tl_lo, tl_hi) = dword_le(&snap, 64);
            let harts_online = word_le(&snap, 72);
            let hp1_online = word_le(&snap, 76);
            let (u1_lo, u1_hi) = dword_le(&snap, 80);
            let (k1_lo, k1_hi) = dword_le(&snap, 88);
            let (i1_lo, i1_hi) = dword_le(&snap, 96);
            let (c1_lo, c1_hi) = dword_le(&snap, 104);
            let (ipi_lo, ipi_hi) = dword_le(&snap, 112);
            let (sd_lo, sd_hi) = dword_le(&snap, 120);

            let user = (u64::from(u_hi) << 32) | u64::from(u_lo);
            let kernel = (u64::from(k_hi) << 32) | u64::from(k_lo);
            let cap = (u64::from(c_hi) << 32) | u64::from(c_lo);
            let busy_bps = ratio_bps(user.saturating_add(kernel), cap);
            let user1 = (u64::from(u1_hi) << 32) | u64::from(u1_lo);
            let kernel1 = (u64::from(k1_hi) << 32) | u64::from(k1_lo);
            let cap1 = (u64::from(c1_hi) << 32) | u64::from(c1_lo);
            let busy1_bps = ratio_bps(user1.saturating_add(kernel1), cap1);
            let freq_mode: &[u8] = match freq_meta & 0xff {
                1 => b"pin_40mhz",
                2 => b"pin_180mhz",
                _ => b"auto",
            };
            let freq_switches = freq_meta >> 8;

            out.append(b"hart ");
            out.decimal(hart);
            out.append(b"\nharts_online ");
            out.decimal(harts_online);
            out.append(b"\nisa rv32imafc_xesppie\ncpu_mhz ");
            out.decimal(cpu_mhz);
            out.append(b"\napb_mhz ");
            out.decimal(apb_mhz);
            out.append(b"\nlp_hz ");
            out.decimal(lp_hz);
            out.append(b"\nfreq_mode ");
            out.append(freq_mode);
            out.append(b"\nfreq_switches ");
            out.decimal(freq_switches);
            out.append(b"\ntickless_sleeps ");
            out.decimal(tickless_entries);
            out.append(b" ticks ");
            out.decimal64(tl_lo, tl_hi);
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
            out.append(b"\nhp1_online ");
            out.decimal(hp1_online);
            out.append(b"\nhp1_busy_pct ");
            out.hundredths(busy1_bps);
            out.append(b"\nhp1_user_cycles ");
            out.decimal64(u1_lo, u1_hi);
            out.append(b"\nhp1_kernel_cycles ");
            out.decimal64(k1_lo, k1_hi);
            out.append(b"\nhp1_idle_cycles ");
            out.decimal64(i1_lo, i1_hi);
            out.append(b"\nipi_total ");
            out.decimal64(ipi_lo, ipi_hi);
            out.append(b"\npmp_shootdowns ");
            out.decimal64(sd_lo, sd_hi);
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
            out.append(b"tid pid state prio hart/aff mig ticks user_cycles preempt stack\n");
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
                let smp_meta = (meta >> 24) & 0xff;
                let aff = smp_meta & 3;
                let hart = (smp_meta >> 2) & 1;
                let mig = (smp_meta >> 4) & 0xf;

                out.decimal(tid);
                out.push(b' ');
                out.decimal(pid);
                out.push(b' ');
                out.append(state);
                out.push(b' ');
                out.decimal(prio);
                out.push(b'/');
                out.decimal(eff_prio);
                out.append(b" hp");
                out.decimal(hart);
                out.push(b'/');
                out.decimal(aff);
                out.push(b' ');
                out.decimal(mig);
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
                abi::DISPLAY_MODE_CONSOLE => b"console",
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
        b"/proc/power" => {
            let Ok(reading) = ina226::sample_tab5(i2c.system) else {
                out.append(b"service unavailable\n");
                return 2;
            };
            out.append(b"sensor ina226\nbus i2c_gpio31_32\naddr 0x41\nid ");
            out.hex16(reading.mfg_id);
            out.push(b':');
            out.hex16(reading.die_id);
            out.append(b"\nshunt_mohm ");
            out.decimal(ina226::TAB5_SHUNT_MOHM);
            out.append(b"\nrail battery_2s\nsupply ");
            out.append(reading.supply.as_bytes());
            out.append(b"\nbattery ");
            out.append(reading.battery.as_bytes());
            out.append(b"\nvbus_mv ");
            out.decimal(reading.vbus_mv);
            out.append(b"\nvshunt_uv ");
            out.signed_decimal(reading.vshunt_uv);
            out.append(b"\ncurrent_ma ");
            out.signed_decimal(reading.current_ma);
            out.append(b"\npower_mw ");
            out.decimal(reading.power_mw);
            out.push(b'\n');
            0
        }
        b"/proc/sdcard" => {
            let mut snap = [0u8; abi::STATS_SDCARD_EXT_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_SDCARD_EXT_BYTES as u32,
                abi::STATS_SDCARD,
                0,
            ) != 0
            {
                out.append(b"service unavailable\n");
                return 2;
            }
            let flags = word_le(&snap, 0);
            let ready = (flags & 1) != 0;
            let sdhc = (flags & 2) != 0;
            let verified = (flags & 4) != 0;
            let provisioned = (flags & 8) != 0;
            let sdmmc_4bit = (flags & 16) != 0;
            let high_speed = (flags & 32) != 0;
            let ocr = word_le(&snap, 4);
            let sectors = word_le(&snap, 8);
            let asset_lba = word_le(&snap, 12);
            let asset_bytes = word_le(&snap, 16);
            let read_sectors = word_le(&snap, 20);
            let written_sectors = word_le(&snap, 24);
            let errors = word_le(&snap, 28);
            let clk_khz = word_le(&snap, 32);
            let bench_w_kib = word_le(&snap, 36);
            let bench_r_kib = word_le(&snap, 40);

            out.append(b"bus ");
            out.append(if sdmmc_4bit {
                b"sdmmc_4bit_idma_gpio39_44"
            } else {
                b"spi_gpio39_44"
            });
            out.append(b"\nrail ldo_vo4_3300mv\nclk_khz ");
            out.decimal(clk_khz);
            out.append(b"\nhigh_speed ");
            out.decimal(u32::from(high_speed));
            out.append(b"\nstate ");
            out.append(if ready { b"ready" } else { b"uninitialized" });
            out.append(b"\ntype ");
            out.append(if !ready {
                b"none"
            } else if sdhc {
                b"sdhc"
            } else {
                b"sdsc"
            });
            out.append(b"\nocr ");
            out.hex32(ocr);
            out.append(b"\nsectors ");
            out.decimal(sectors);
            out.append(b"\ncapacity_mib ");
            out.decimal(sectors / 2048);
            out.append(b"\nasset_lba ");
            out.decimal(asset_lba);
            out.append(b"\nasset_bytes ");
            out.decimal(asset_bytes);
            out.append(b"\nassets ");
            out.append(if verified { b"verified" } else { b"unverified" });
            out.append(b"\nprovisioned ");
            out.decimal(u32::from(provisioned));
            out.append(b"\nread_sectors ");
            out.decimal(read_sectors);
            out.append(b"\nwritten_sectors ");
            out.decimal(written_sectors);
            out.append(b"\nbench_write_kib_s ");
            out.decimal(bench_w_kib);
            out.append(b"\nbench_read_kib_s ");
            out.decimal(bench_r_kib);
            out.append(b"\nerrors ");
            out.decimal(errors);
            out.push(b'\n');
            0
        }
        b"/dev/random" | b"/dev/urandom" => {
            let mut snap = [0u8; abi::STATS_RANDOM_BYTES];
            if sys::call(
                abi::STATS,
                snap.as_mut_ptr() as u32,
                abi::STATS_RANDOM_BYTES as u32,
                abi::STATS_RANDOM,
                0,
            ) != 0
            {
                out.append(b"service unavailable\n");
                return 2;
            }
            let cfg = word_le(&snap, 0);
            let last_raw = word_le(&snap, 4);
            let (s_lo, s_hi) = dword_le(&snap, 8);
            out.append(b"device ");
            out.append(path);
            out.append(b"\nsource esp32p4_lp_trng_0x501101a4\nrng_cfg ");
            out.hex16(cfg as u16);
            out.append(b"\nlast_raw ");
            out.hex32(last_raw);
            out.append(b"\nhw_samples ");
            out.decimal64(s_lo, s_hi);
            out.append(b"\nentropy_256b ");
            for i in 0..8 {
                if i > 0 {
                    out.push(b' ');
                }
                out.hex32(word_le(&snap, 16 + i * 4));
            }
            out.push(b'\n');
            0
        }
        b"/proc/i2c" => {
            for info in &i2c::BUSES {
                i2c::render_bus_line(out, info);
            }
            0
        }
        _ if path == b"/ctl/kbdled" || path.starts_with(b"/ctl/kbdled ") => {
            let args = path.get(12..).unwrap_or(&[]);
            kbd_led(args, i2c.ext1, out)
        }
        _ if i2c::bus_from_dev_path(path).is_some() => {
            let id = i2c::bus_from_dev_path(path).unwrap_or(0);
            match (i2c::bus_info(id), i2c.bus(id)) {
                (Some(info), Some(cap)) => {
                    i2c::render_scan_sys(out, info, cap);
                    0
                }
                _ => {
                    out.append(b"procfs: no such file\n");
                    1
                }
            }
        }
        b"/dev/zero" => {
            out.append(b"device /dev/zero\nsource kernel_zero_stream\nzero_256b");
            for _ in 0..8 {
                out.append(b" 0x00000000");
            }
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
    text: TextBuf<512>,
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

    fn render_or_reuse(&mut self, offset: usize, path: &[u8], i2c: I2cCaps) -> (u8, &[u8]) {
        let same_path =
            offset > 0 && path.len() == self.path_len && self.path[..self.path_len] == *path;
        if !same_path {
            self.text.clear();
            self.status = render_path(path, i2c, &mut self.text);
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
fn handle_request(
    request: &Buffer,
    i2c: I2cCaps,
    response: &mut Buffer,
    cache: &mut SnapshotCache,
) {
    response.clear();
    let req = request.as_slice();
    match req.first().copied() {
        Some(0) if req.len() == 1 => {
            response.push(0);
        }
        Some(1) if req.len() > 1 => {
            let (status, bytes) = cache.render_or_reuse(0, &req[1..], i2c);
            response.push(status);
            response.append(bytes);
        }
        Some(2) if req.len() > 3 => {
            let offset = u16::from_le_bytes([req[1], req[2]]) as usize;
            let (status, bytes) = cache.render_or_reuse(offset, &req[3..], i2c);
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

fn main(
    request_handle: u32,
    reply_handle: u32,
    i2c_system: u32,
    i2c_ext1: u32,
    _a4: u32,
    _a5: u32,
) -> ! {
    let i2c = I2cCaps {
        system: i2c_system,
        ext1: i2c_ext1,
    };
    let mut cache = SnapshotCache::new();
    let mut request = Buffer::new();
    let mut response = Buffer::new();
    let mut pending_token = 0u32;
    loop {
        let ok = if pending_token != 0 {
            request.reply_and_receive(pending_token, &response, request_handle)
        } else {
            request.receive(request_handle)
        };
        if !ok {
            sys::exit();
        }
        handle_request(&request, i2c, &mut response, &mut cache);
        pending_token = request.reply;
        if pending_token == 0 && !response.send(reply_handle) {
            sys::exit();
        }
    }
}
