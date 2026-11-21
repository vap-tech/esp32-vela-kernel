//! Userspace I2C bus table and address scan on top of the capability-gated
//! `I2C_TRANSFER` syscall.
//!
//! Each bus is reached through its own `Authority::I2c(bus)` handle. A scan only issues
//! address-only probes (SMBus "quick write"), which never transfer a data byte, so it
//! is safe on any bus: `Ok` = a device ACKed, `-ENXIO` = nobody answered,
//! any other error = the bus itself is faulty (SCL/SDA held low).

use crate::{TextBuf, sys};
use vela_abi::syscall::{I2C_BUS_COUNT, I2C_BUS_EXT1, I2C_BUS_SYSTEM};

pub const ADDR_MIN: u8 = 0x08;
pub const ADDR_MAX: u8 = 0x77;
const ENXIO: i32 = -6;

/// Static description of one board bus. The kernel BSP owns the real access policy;
/// this table is only what `/proc/i2c` and `/dev/i2c-N` report.
pub struct BusInfo {
    pub id: u8,
    pub name: &'static [u8],
    pub sda: u8,
    pub scl: u8,
    pub int: Option<u8>,
    /// Addresses userspace may read/write (probing is allowed everywhere).
    pub data_access: &'static [u8],
}

pub const BUSES: [BusInfo; I2C_BUS_COUNT as usize] = [
    BusInfo {
        id: I2C_BUS_SYSTEM,
        name: b"system",
        sda: 31,
        scl: 32,
        int: None,
        data_access: b"0x41",
    },
    BusInfo {
        id: I2C_BUS_EXT1,
        name: b"ext_port1",
        sda: 0,
        scl: 1,
        int: Some(50),
        data_access: b"any",
    },
];

pub fn bus_info(id: u8) -> Option<&'static BusInfo> {
    BUSES.iter().find(|bus| bus.id == id)
}

/// Parse `/dev/i2c-N` into a bus id.
pub fn bus_from_dev_path(path: &[u8]) -> Option<u8> {
    let digits = path.strip_prefix(b"/dev/i2c-")?;
    match digits {
        [d @ b'0'..=b'9'] if bus_info(d - b'0').is_some() => Some(d - b'0'),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub found: u8,
    pub faults: u8,
}

/// Probe `ADDR_MIN..=ADDR_MAX`; `probe(addr)` follows the syscall convention.
/// `found(addr)` is called for each ACKing address in ascending order.
pub fn scan<P, F>(mut probe: P, mut found: F) -> ScanSummary
where
    P: FnMut(u8) -> Result<(), i32>,
    F: FnMut(u8),
{
    let mut summary = ScanSummary::default();
    for addr in ADDR_MIN..=ADDR_MAX {
        match probe(addr) {
            Ok(()) => {
                summary.found += 1;
                found(addr);
            }
            Err(ENXIO) => {}
            Err(_) => summary.faults += 1,
        }
    }
    summary
}

fn pins<const N: usize>(out: &mut TextBuf<N>, info: &BusInfo) {
    out.append(b"sda=gpio");
    out.decimal(u32::from(info.sda));
    out.append(b" scl=gpio");
    out.decimal(u32::from(info.scl));
    if let Some(int) = info.int {
        out.append(b" int=gpio");
        out.decimal(u32::from(int));
    }
}

/// One `/proc/i2c` line describing `info`.
pub fn render_bus_line<const N: usize>(out: &mut TextBuf<N>, info: &BusInfo) {
    out.append(b"bus ");
    out.decimal(u32::from(info.id));
    out.push(b' ');
    out.append(info.name);
    out.push(b' ');
    pins(out, info);
    out.append(b" data_access=");
    out.append(info.data_access);
    out.append(b" node=/dev/i2c-");
    out.decimal(u32::from(info.id));
    out.push(b'\n');
}

/// `/dev/i2c-N` view: bus identity plus a live address scan through `probe`.
pub fn render_scan<const N: usize, P>(out: &mut TextBuf<N>, info: &BusInfo, probe: P)
where
    P: FnMut(u8) -> Result<(), i32>,
{
    out.append(b"device /dev/i2c-");
    out.decimal(u32::from(info.id));
    out.append(b"\nbus ");
    out.append(info.name);
    out.push(b'\n');
    pins(out, info);
    out.append(b"\ndata_access ");
    out.append(info.data_access);
    out.append(b"\nscan ");
    out.hex8(ADDR_MIN);
    out.append(b"..");
    out.hex8(ADDR_MAX);
    let mut addrs = [0u8; (ADDR_MAX - ADDR_MIN + 1) as usize];
    let mut n = 0;
    let summary = scan(probe, |addr| {
        addrs[n] = addr;
        n += 1;
    });
    // Summary first: if a pathological bus answers everywhere, only the tail of the
    // address list is truncated by the fixed-size procfs buffer.
    out.append(b"\ncount ");
    out.decimal(u32::from(summary.found));
    out.append(b"\nstate ");
    out.append(if summary.faults == 0 {
        b"ok".as_slice()
    } else {
        b"bus_fault".as_slice()
    });
    out.append(b"\nfound");
    if n == 0 {
        out.append(b" none");
    }
    for &addr in &addrs[..n] {
        out.push(b' ');
        out.hex8(addr);
    }
    out.push(b'\n');
}

/// Scan through the `I2C_TRANSFER` probe using the bus handle `cap`.
pub fn render_scan_sys<const N: usize>(out: &mut TextBuf<N>, info: &BusInfo, cap: u32) {
    render_scan(out, info, |addr| sys::i2c_transfer(cap, addr, &[], &mut []));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text<const N: usize>(out: &TextBuf<N>) -> &str {
        core::str::from_utf8(out.as_slice()).unwrap()
    }

    #[test]
    fn dev_path_names_only_known_buses() {
        assert_eq!(bus_from_dev_path(b"/dev/i2c-0"), Some(0));
        assert_eq!(bus_from_dev_path(b"/dev/i2c-1"), Some(1));
        assert_eq!(bus_from_dev_path(b"/dev/i2c-2"), None);
        assert_eq!(bus_from_dev_path(b"/dev/i2c-10"), None);
        assert_eq!(bus_from_dev_path(b"/dev/i2c-"), None);
        assert_eq!(bus_from_dev_path(b"/proc/i2c"), None);
    }

    #[test]
    fn scan_reports_acks_in_order_and_counts_faults() {
        let present = [0x41u8, 0x6d];
        let mut seen = [0u8; 4];
        let mut n = 0;
        let s = scan(
            |a| {
                if present.contains(&a) {
                    Ok(())
                } else {
                    Err(ENXIO)
                }
            },
            |a| {
                seen[n] = a;
                n += 1;
            },
        );
        assert_eq!(
            s,
            ScanSummary {
                found: 2,
                faults: 0
            }
        );
        assert_eq!(&seen[..n], &[0x41, 0x6d]);

        let s = scan(|_| Err(-5), |_| panic!("no device on a faulty bus"));
        assert_eq!(
            s,
            ScanSummary {
                found: 0,
                faults: 112
            }
        );
    }

    #[test]
    fn renders_found_addresses_and_state() {
        let mut out = TextBuf::<512>::new();
        let info = bus_info(I2C_BUS_EXT1).unwrap();
        render_scan(
            &mut out,
            info,
            |a| if a == 0x6d { Ok(()) } else { Err(ENXIO) },
        );
        assert_eq!(
            text(&out),
            "device /dev/i2c-1\nbus ext_port1\nsda=gpio0 scl=gpio1 int=gpio50\n\
             data_access any\nscan 0x08..0x77\ncount 1\nstate ok\nfound 0x6d\n"
        );

        let mut out = TextBuf::<512>::new();
        render_scan(&mut out, info, |_| Err(ENXIO));
        assert!(text(&out).ends_with("count 0\nstate ok\nfound none\n"));

        let mut out = TextBuf::<512>::new();
        render_scan(&mut out, info, |_| Err(-5));
        assert!(text(&out).ends_with("count 0\nstate bus_fault\nfound none\n"));
    }

    #[test]
    fn pathological_full_scan_keeps_the_summary() {
        let mut out = TextBuf::<512>::new();
        render_scan(&mut out, bus_info(0).unwrap(), |_| Ok(()));
        // All 112 addresses answering is the worst case: the summary must survive even if
        // the tail of the address list is truncated.
        assert!(text(&out).contains("\ncount 112\nstate ok\nfound 0x08 0x09"));
    }

    #[test]
    fn bus_line_lists_pins_and_policy() {
        let mut out = TextBuf::<128>::new();
        render_bus_line(&mut out, bus_info(0).unwrap());
        assert_eq!(
            text(&out),
            "bus 0 system sda=gpio31 scl=gpio32 data_access=0x41 node=/dev/i2c-0\n"
        );
    }
}
