//! `/sys/*` platform telemetry and subsystem control nodes.

use vela_abi::{fs as fs_abi, syscall as abi};
use vela_user::{TextBuf, ina226, sys, word_le};

pub const ENTRIES: &[&[u8]] = &[b"power", b"freq", b"display", b"sentinel"];
pub const SENTINEL_ENTRIES: &[&[u8]] = &[b"status", b"log", b"sleep"];

#[derive(Clone, Copy)]
pub struct SysCaps {
    pub i2c_system: u32,
    pub display: u32,
    pub sentinel: u32,
}

pub fn read(rel: &[u8], caps: SysCaps, out: &mut TextBuf<1024>) -> u8 {
    match rel {
        b"power" => render_power(caps.i2c_system, out),
        b"freq" => freq_op(abi::FREQ_MODE_QUERY, caps.sentinel, out),
        b"display" => render_display(out),
        b"sentinel/status" => render_sentinel_status(caps.sentinel, out),
        b"sentinel/log" => render_sentinel_log(caps.sentinel, out),
        b"sentinel/sleep" => {
            out.append(b"sys/sentinel/sleep: write <1..60000 ms>\n");
            fs_abi::STATUS_ERR
        }
        _ => {
            out.append(b"sys: no such file\n");
            fs_abi::STATUS_NOENT
        }
    }
}

pub fn write(rel: &[u8], data: &[u8], caps: SysCaps, out: &mut TextBuf<1024>) -> u8 {
    let text = data.trim_ascii();
    match rel {
        b"freq" => {
            let mode = match text {
                b"auto" => abi::FREQ_MODE_AUTO,
                b"40" => abi::FREQ_MODE_40MHZ,
                b"180" => abi::FREQ_MODE_180MHZ,
                _ => {
                    out.append(b"sys/freq: expected auto | 40 | 180\n");
                    return fs_abi::STATUS_ERR;
                }
            };
            freq_op(mode, caps.sentinel, out)
        }
        b"display" => write_display(text, caps.display, out),
        b"sentinel/sleep" => write_sentinel_sleep(text, caps.sentinel, out),
        b"power" | b"sentinel/status" | b"sentinel/log" => {
            out.append(b"sys: read-only file\n");
            fs_abi::STATUS_ERR
        }
        _ => {
            out.append(b"sys: no such file\n");
            fs_abi::STATUS_NOENT
        }
    }
}

fn parse_u32_range(bytes: &[u8], min: u32, max: u32) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut val = 0u32;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    (val >= min && val <= max).then_some(val)
}

#[inline(never)]
fn render_power(i2c_system: u32, out: &mut TextBuf<1024>) -> u8 {
    let Ok(reading) = ina226::sample_tab5(i2c_system) else {
        out.append(b"service unavailable\n");
        return fs_abi::STATUS_ERR;
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
    fs_abi::STATUS_OK
}

#[inline(never)]
fn freq_op(mode: u32, sentinel_cap: u32, out: &mut TextBuf<1024>) -> u8 {
    let (cpu_mhz, active_mode) = sys::call6(abi::CLOCK_CONTROL, sentinel_cap, mode, 0, 0, 0, 0);
    if (cpu_mhz as i32) <= 0 {
        if cpu_mhz as i32 == -16 {
            out.append(b"freq busy; stop active display/DMA first\n");
        } else {
            out.append(b"service unavailable\n");
        }
        return fs_abi::STATUS_ERR;
    }
    let mode_name: &[u8] = match active_mode {
        abi::FREQ_MODE_40MHZ => b"pin_40mhz",
        abi::FREQ_MODE_180MHZ => b"pin_180mhz",
        _ => b"auto",
    };
    out.append(b"cpu_mhz=");
    out.decimal(cpu_mhz);
    out.append(b" mode=");
    out.append(mode_name);
    out.push(b'\n');
    fs_abi::STATUS_OK
}

#[inline(never)]
fn render_display(out: &mut TextBuf<1024>) -> u8 {
    let mut snap = [0u8; abi::STATS_DISPLAY_BYTES];
    if sys::call(
        abi::STATS,
        snap.as_mut_ptr() as u32,
        abi::STATS_DISPLAY_BYTES as u32,
        abi::STATS_DISPLAY,
        0,
    ) != 0
    {
        out.append(b"service unavailable\n");
        return fs_abi::STATUS_ERR;
    }
    let flags = word_le(&snap, 0);
    out.append(b"blanked=");
    out.decimal(u32::from((flags & abi::DISPLAY_FLAG_BLANKED) != 0));
    out.append(b" sd_export=");
    out.decimal(u32::from((flags & abi::DISPLAY_FLAG_SD_EXPORT) != 0));
    out.append(b" sd_export_rw=");
    out.decimal(u32::from((flags & abi::DISPLAY_FLAG_SD_EXPORT_RW) != 0));
    out.push(b'\n');
    fs_abi::STATUS_OK
}

#[inline(never)]
fn write_display(text: &[u8], display_cap: u32, out: &mut TextBuf<1024>) -> u8 {
    if text == b"off" {
        let rc = sys::call(abi::DISPLAY_TEST, display_cap, abi::DISPLAY_MODE_OFF, 0, 0);
        if rc == 0 {
            out.append(b"Display off\n");
            fs_abi::STATUS_OK
        } else {
            out.append(b"service unavailable\n");
            fs_abi::STATUS_ERR
        }
    } else if let Some(level) = parse_u32_range(text, 0, 255) {
        let rc = sys::call(
            abi::DISPLAY_TEST,
            display_cap,
            abi::DISPLAY_MODE_BACKLIGHT,
            level,
            0,
        );
        if rc == 0 {
            out.append(b"backlight=");
            out.decimal(level);
            out.push(b'\n');
            fs_abi::STATUS_OK
        } else if rc == abi::EBUSY {
            out.append(b"display busy\n");
            fs_abi::STATUS_ERR
        } else {
            out.append(b"service unavailable\n");
            fs_abi::STATUS_ERR
        }
    } else {
        out.append(b"sys/display: expected off | 0..255\n");
        fs_abi::STATUS_ERR
    }
}

fn sentinel_log_call(sentinel_cap: u32, sequence: u32, data: &mut [u8; 24]) -> (u32, u32) {
    let mut result = (0, 0);
    for _ in 0..8 {
        result = sys::call6(
            abi::SENTINEL_LOG,
            sentinel_cap,
            sequence,
            data.as_mut_ptr() as u32,
            24,
            0,
            0,
        );
        if result.0 != abi::EBUSY {
            break;
        }
        sys::sleep();
    }
    result
}

#[inline(never)]
fn render_sentinel_status(sentinel_cap: u32, out: &mut TextBuf<1024>) -> u8 {
    let mut data = [0u8; 24];
    let (result, notices) = sys::call6(
        abi::SENTINEL_QUERY,
        sentinel_cap,
        data.as_mut_ptr() as u32,
        24,
        0,
        0,
        0,
    );
    if result != 24 {
        out.append(b"service unavailable\n");
        return fs_abi::STATUS_ERR;
    }
    out.append(b"Sentinel state=");
    match word_le(&data, 0) {
        0 => out.append(b"unarmed"),
        1 => out.append(b"healthy"),
        3 => out.append(b"prepared"),
        _ => out.append(b"missed"),
    }
    out.append(b" age_ms=");
    out.decimal64(word_le(&data, 4), word_le(&data, 8));
    out.append(b" lp_hz=");
    out.decimal(word_le(&data, 12));
    out.append(b" beats=");
    out.decimal(word_le(&data, 16));
    out.append(b" missed=");
    out.decimal(word_le(&data, 20));
    out.append(b" notices=");
    out.decimal(notices);
    out.push(b'\n');
    fs_abi::STATUS_OK
}

#[inline(never)]
fn render_sentinel_log(sentinel_cap: u32, out: &mut TextBuf<1024>) -> u8 {
    let mut data = [0u8; 24];
    let (result, notices) = sentinel_log_call(sentinel_cap, 0, &mut data);
    if result != 24 {
        out.append(b"service unavailable\n");
        return fs_abi::STATUS_ERR;
    }
    let oldest = word_le(&data, 4);
    let newest = word_le(&data, 8);
    out.append(b"Sentinel log epoch=");
    out.decimal(word_le(&data, 0));
    out.append(b" hz=");
    out.decimal(word_le(&data, 20));
    out.append(b"\noldest=");
    out.decimal(oldest);
    out.append(b" newest=");
    out.decimal(newest);
    out.append(b" overwritten=");
    out.decimal(word_le(&data, 12));
    out.append(b" capacity=");
    out.decimal(word_le(&data, 16));
    out.append(b" notices=");
    out.decimal(notices);
    out.push(b'\n');

    for offset in 0..16u32 {
        let sequence = oldest.wrapping_add(offset);
        if oldest == 0 || sequence == 0 || sequence > newest {
            break;
        }
        let result = sentinel_log_call(sentinel_cap, sequence, &mut data).0;
        if result != 24 {
            if result == (-75i32) as u32 {
                out.append(b"record overwritten during read; retry\n");
            } else {
                out.append(b"service unavailable\n");
            }
            break;
        }
        out.push(b'#');
        out.decimal(word_le(&data, 0));
        out.append(b" lp_ticks=");
        out.decimal64(word_le(&data, 8), word_le(&data, 12));
        let event_name: &[u8] = match word_le(&data, 4) {
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
        out.decimal(word_le(&data, 16));
        out.append(b" a1=");
        out.decimal(word_le(&data, 20));
        out.push(b'\n');
    }
    fs_abi::STATUS_OK
}

#[inline(never)]
fn write_sentinel_sleep(text: &[u8], sentinel_cap: u32, out: &mut TextBuf<1024>) -> u8 {
    let Some(ms) = parse_u32_range(text, 1, 60_000) else {
        out.append(b"sys/sentinel/sleep: expected 1..60000 ms\n");
        return fs_abi::STATUS_ERR;
    };
    let mut result = (0, 0);
    for _ in 0..8 {
        result = sys::call6(abi::SENTINEL_PREPARE_SLEEP, sentinel_cap, ms, 0, 0, 0, 0);
        if result.0 != abi::EBUSY {
            break;
        }
        sys::sleep();
    }
    if (result.0 as i32) > 0 {
        result = sys::call6(abi::SENTINEL_WAIT_WAKE, sentinel_cap, result.0, 0, 0, 0, 0);
    }
    if (result.0 as i32) <= 0 {
        let message: &[u8] = match result.0 as i32 {
            -16 => b"sleep busy; stop active display/DMA first\n",
            -110 => b"wake timeout\n",
            _ => b"service unavailable\n",
        };
        out.append(message);
        return fs_abi::STATUS_ERR;
    }
    out.append(b"Sentinel wake token=");
    out.decimal(result.0);
    out.append(b" elapsed_ticks=");
    out.decimal(result.1);
    out.push(b'\n');
    fs_abi::STATUS_OK
}
