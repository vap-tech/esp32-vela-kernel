//! Tab5 USB-C Full-Speed OTG and read-only SCSI Bulk-Only Transport.
//! Register fields: ESP-IDF esp32p4 hw_ver1 and esp_hal_usb/usb_wrap_ll.h.
use core::mem::MaybeUninit;

use esp_synopsys_usb_otg::{UsbBus, UsbPeripheral};
use usb_device::{
    bus::UsbBusAllocator,
    device::{StringDescriptors, UsbDevice, UsbDeviceBuilder, UsbDeviceState, UsbRev, UsbVidPid},
};
use usbd_storage::{
    subclass::scsi::{Scsi, ScsiCommand},
    transport::bbb::BulkOnly,
};
use vela_kernel::errno::Errno;

use crate::{
    pac::{read, write},
    soc::{clock, sdcard, timer},
};

const DWC: usize = 0x5004_0000;
const USB_WRAP: usize = 0x5008_0000;
const PHY_SELECT: usize = 0x5011_0100;
const SYS_CLOCK: usize = 0x500e_6018;
const PHY_CLOCK: usize = 0x5011_1044;
const OTG_RESET: usize = 0x5011_1048;
const DCTL: usize = DWC + 0x804;
const DISCONNECT: u32 = 1 << 1;
const PULL_OVERRIDE: u32 = 0b1_1111 << 12;
const PULL_UP_DP: u32 = (1 << 12) | (1 << 13);
const SECTOR: usize = sdcard::SECTOR_BYTES;

// USB stack entry points are private to this module and run with KERNEL_LOCK
// held and MIE masked. The driver only polls, so no IRQ shares its state.
struct UsbCriticalSection;
critical_section::set_impl!(UsbCriticalSection);
// SAFETY: every use of the crate's critical section runs under the kernel lock.
unsafe impl critical_section::Impl for UsbCriticalSection {
    unsafe fn acquire() -> critical_section::RawRestoreState {}
    unsafe fn release(_: critical_section::RawRestoreState) {}
}

struct FsOtg;
// SAFETY: one allocator per session owns OTG11. P4 FS FIFO and endpoint counts
// are checked against GHWCFG before the allocator enables the peripheral.
unsafe impl UsbPeripheral for FsOtg {
    const REGISTERS: *const () = DWC as *const ();
    const HIGH_SPEED: bool = false;
    const FIFO_DEPTH_WORDS: usize = 200;
    const ENDPOINT_COUNT: usize = 2;

    fn enable() {} // The BSP already enabled the clock and reset the core.

    fn ahb_frequency_hz(&self) -> u32 {
        unsafe { clock::rates_mhz().cpu * 1_000_000 }
    }
}

type Bus = UsbBus<FsOtg>;
type Msc = Scsi<BulkOnly<'static, Bus, [u8; SECTOR]>>;
static mut ALLOCATOR: MaybeUninit<UsbBusAllocator<Bus>> = MaybeUninit::uninit();
static mut ENDPOINT_MEMORY: [u32; 32] = [0; 32];

struct Session {
    device: UsbDevice<'static, Bus>,
    msc: Msc,
    attach_at: u64,
    attached: bool,
    previous_phy: u32,
    sectors: u32,
    offset: usize,
    data: [u8; SECTOR],
    cached_lba: Option<u32>,
    sense: (u8, u8),
}

pub struct UsbStorage {
    session: Option<Session>,
}

unsafe fn update(address: usize, clear: u32, set: u32) {
    unsafe { write(address, (read(address) & !clear) | set) };
}

impl UsbStorage {
    pub const fn new() -> Self {
        Self { session: None }
    }

    /// Start a read-only session. An absent card is reported as no media.
    /// SAFETY: KERNEL_LOCK held, MIE masked, exclusive ownership of OTG11.
    pub unsafe fn start(&mut self) -> Result<(), Errno> {
        if self.session.is_some() {
            return Err(Errno::Ebusy);
        }
        let sectors = unsafe { sdcard::ensure_initialized() }
            .map(|(_, _, _, sectors)| sectors)
            .unwrap_or(0);
        unsafe {
            let attach_at = timer::uptime_ticks().ok_or(Errno::Eio)?.saturating_add(25);
            update(SYS_CLOCK, 0, 1 << 15);
            update(PHY_CLOCK, 0, 1 << 4);
            update(OTG_RESET, 0, 1 << 3);
            update(OTG_RESET, 1 << 3, 0);
            if read(DWC + 0x40) >> 16 != 0x4f54
                || read(DWC + 0x4c) >> 16 != FsOtg::FIFO_DEPTH_WORDS as u32
                || (read(DWC + 0x48) >> 10) & 0xf == 0
                || !(0..100_000).any(|_| read(DWC + 0x10) & (1 << 31) != 0)
            {
                return Err(Errno::Eio);
            }
            update(USB_WRAP, 1 << 2, 1 << 18);
            let memory = &mut *(&raw mut ENDPOINT_MEMORY);
            // A previous stop drops the only references to the allocator and
            // bus before any new session reuses this storage.
            let allocator = (&raw mut ALLOCATOR).cast::<UsbBusAllocator<Bus>>();
            allocator.write(UsbBus::new(FsOtg, memory));
            let allocator = &*allocator;
            let msc = Msc::new(allocator, 64, 0, [0; SECTOR]).map_err(|_| Errno::Eio)?;
            let device = UsbDeviceBuilder::new(allocator, UsbVidPid(0x303a, 0x4001))
                .strings(&[StringDescriptors::default()
                    .manufacturer("Space Flow")
                    .product("Vela SD (read only)")])
                .map_err(|_| Errno::Eio)?
                .usb_rev(UsbRev::Usb200)
                .max_packet_size_0(64)
                .map_err(|_| Errno::Eio)?
                .build();

            update(DCTL, 0, DISCONNECT);
            update(DWC, 0, 0b11 << 6); // Force a valid B-peripheral session.
            update(USB_WRAP, PULL_OVERRIDE, 1 << 12);
            let previous_phy = read(PHY_SELECT) & 3;
            update(PHY_SELECT, 3, 3);
            sdcard::set_exported(true);
            self.session = Some(Session {
                device,
                msc,
                attach_at,
                attached: false,
                previous_phy,
                sectors,
                offset: 0,
                data: [0; SECTOR],
                cached_lba: None,
                sense: (0, 0),
            });
        }
        Ok(())
    }

    /// SAFETY: KERNEL_LOCK held with MIE masked.
    pub unsafe fn stop(&mut self) {
        if let Some(session) = self.session.take() {
            unsafe {
                update(DCTL, 0, DISCONNECT);
                update(USB_WRAP, PULL_OVERRIDE, 0);
                update(PHY_SELECT, 3, session.previous_phy);
                update(OTG_RESET, 0, 1 << 3);
                update(OTG_RESET, 1 << 3, 0);
                sdcard::set_exported(false);
            }
        }
    }

    /// Service a bounded number of transport steps per syscall.
    /// SAFETY: KERNEL_LOCK held with MIE masked.
    pub unsafe fn poll(&mut self) -> Result<bool, Errno> {
        let session = self.session.as_mut().ok_or(Errno::Einval)?;
        unsafe {
            if !session.attached {
                if timer::uptime_ticks().ok_or(Errno::Eio)? < session.attach_at {
                    return Ok(false);
                }
                update(DCTL, DISCONNECT, 0);
                session.attached = true;
            }
            update(USB_WRAP, PULL_OVERRIDE, PULL_UP_DP);
        }
        let mut activity = false;
        for _ in 0..8 {
            activity |= session.device.poll(&mut [&mut session.msc]);
            if session.device.state() != UsbDeviceState::Configured {
                session.offset = 0;
                session.cached_lba = None;
                break;
            }
            if session.poll_scsi() {
                activity = true;
                break; // At most one SD transfer per syscall.
            }
        }
        Ok(activity || session.offset != 0)
    }
}

impl Session {
    fn poll_scsi(&mut self) -> bool {
        let Self {
            msc,
            sectors,
            offset,
            data,
            cached_lba,
            sense,
            ..
        } = self;
        let mut accessed_sd = false;
        let _ = msc.poll_command(|mut cmd| {
            let mut response = [0; 36];
            let count = match cmd.kind {
                ScsiCommand::Inquiry {
                    evpd: false,
                    alloc_len,
                    ..
                } => {
                    response[..5].copy_from_slice(&[0, 0x80, 4, 2, 31]);
                    response[8..16].copy_from_slice(b"VELA    ");
                    response[16..32].copy_from_slice(b"Tab5 microSD    ");
                    response[32..36].copy_from_slice(b"0001");
                    36.min(usize::from(alloc_len))
                }
                ScsiCommand::Inquiry {
                    evpd: true,
                    alloc_len,
                    page_code: 0,
                } => 4.min(usize::from(alloc_len)),
                ScsiCommand::RequestSense {
                    desc: false,
                    alloc_len,
                } => {
                    response[0] = 0x70;
                    response[2] = sense.0;
                    response[7] = 10;
                    response[12] = sense.1;
                    *sense = (0, 0);
                    18.min(usize::from(alloc_len))
                }
                ScsiCommand::ModeSense6 { alloc_len, .. } => {
                    response[0] = 3;
                    response[2] = 0x80; // write protected
                    4.min(usize::from(alloc_len))
                }
                ScsiCommand::ModeSense10 { alloc_len, .. } => {
                    response[1] = 6;
                    response[3] = 0x80;
                    8.min(usize::from(alloc_len))
                }
                ScsiCommand::TestUnitReady if *sectors != 0 => 0,
                ScsiCommand::ReadCapacity10 if *sectors != 0 => {
                    response[..4].copy_from_slice(&(*sectors - 1).to_be_bytes());
                    response[4..8].copy_from_slice(&(SECTOR as u32).to_be_bytes());
                    8
                }
                ScsiCommand::Read { lba, len }
                    if lba
                        .checked_add(u32::from(len))
                        .is_some_and(|end| end <= *sectors) =>
                {
                    let bytes = usize::from(len) * SECTOR;
                    if *offset < bytes {
                        let sector = lba + (*offset / SECTOR) as u32;
                        if *cached_lba != Some(sector) {
                            accessed_sd = true;
                            if unsafe { sdcard::read_export_sector(sector, data) }.is_err() {
                                *sense = (3, 0x11);
                                cmd.fail(*offset as u32);
                                *offset = 0;
                                *cached_lba = None;
                                return Ok(());
                            }
                            *cached_lba = Some(sector);
                        }
                        *offset += cmd.write_data(&data[*offset % SECTOR..])?;
                    }
                    if *offset == bytes {
                        cmd.pass(bytes as u32);
                        *offset = 0;
                    }
                    return Ok(());
                }
                ScsiCommand::Write { .. } => {
                    *sense = (7, 0x27); // DATA PROTECT / WRITE PROTECTED
                    cmd.fail(*offset as u32);
                    *offset = 0;
                    return Ok(());
                }
                _ => {
                    *sense = if *sectors == 0 {
                        (2, 0x3a)
                    } else if matches!(cmd.kind, ScsiCommand::Read { .. }) {
                        (5, 0x21)
                    } else {
                        (5, 0x20)
                    };
                    cmd.fail(0);
                    *offset = 0;
                    return Ok(());
                }
            };
            if count == 0 {
                cmd.pass(0);
            } else {
                let sent = cmd.write_data(&response[..count])?;
                cmd.pass(sent as u32);
            }
            Ok(())
        });
        accessed_sd
    }
}
