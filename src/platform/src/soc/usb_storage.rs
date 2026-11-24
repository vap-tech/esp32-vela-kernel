//! Tab5 USB-C Full-Speed OTG and SCSI Bulk-Only Transport.
//! Register fields: ESP-IDF esp32p4 hw_ver1 and esp_hal_usb/usb_wrap_ll.h.
use core::mem::MaybeUninit;

use esp_synopsys_usb_otg::{UsbBus, UsbPeripheral};
use usb_device::{
    bus::UsbBusAllocator,
    device::{StringDescriptors, UsbDevice, UsbDeviceBuilder, UsbDeviceState, UsbRev, UsbVidPid},
};
use vela_kernel::errno::Errno;

use crate::{
    pac::{read, write},
    soc::{clock, sdcard, timer},
};

mod bot;
use bot::{Media, Msc};

const DWC: usize = 0x5004_0000;
const USB_WRAP: usize = 0x5008_0000;
const PHY_SELECT: usize = 0x5011_0100;
const SYS_CLOCK: usize = 0x500e_6018;
const PHY_CLOCK: usize = 0x5011_1044;
const OTG_RESET: usize = 0x5011_1048;
const DCTL: usize = DWC + 0x804;
// BOT has no separate host acknowledgement for removal. Keep serving the
// empty LUN until it has been idle for 500 ms, with a 3 s upper bound.
const EJECT_IDLE_TICKS: u64 = 50;
const EJECT_LIMIT_TICKS: u64 = 300;
const USB_DISCONNECT_TICKS: u64 = 25;
const DISCONNECT: u32 = 1 << 1;
const PULL_OVERRIDE: u32 = 0b1_1111 << 12;
const PULL_UP_DP: u32 = (1 << 12) | (1 << 13);
const SECTOR: usize = sdcard::SECTOR_BYTES;
pub const WRITE_BATCH_BYTES: usize = 128 * 1024;

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
static mut ALLOCATOR: MaybeUninit<UsbBusAllocator<Bus>> = MaybeUninit::uninit();
static mut ENDPOINT_MEMORY: [u32; 32] = [0; 32];

struct Session {
    device: UsbDevice<'static, Bus>,
    msc: Msc<'static, Bus>,
    attach_at: u64,
    ejected_at: Option<u64>,
    disconnected_at: Option<u64>,
    last_progress: u64,
    attached: bool,
    previous_phy: u32,
    writable: bool,
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

    /// Start an export. Writable sessions require a card and return on host eject.
    /// SAFETY: KERNEL_LOCK held, MIE masked, exclusive ownership of OTG11.
    pub unsafe fn start(
        &mut self,
        writable: bool,
        service: &mut impl FnMut(),
    ) -> Result<(), Errno> {
        if self.session.is_some() {
            return Err(Errno::Ebusy);
        }
        let sectors = unsafe { sdcard::ensure_initialized_with(service) }
            .map(|(_, _, _, sectors)| sectors)
            .unwrap_or(0);
        if writable && sectors == 0 {
            return Err(Errno::Eio);
        }
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
            let msc = Msc::new(allocator, sectors, writable);
            let device = UsbDeviceBuilder::new(allocator, UsbVidPid(0x303a, 0x4001))
                .strings(&[StringDescriptors::default()
                    .manufacturer("Space Flow")
                    .product(if writable {
                        "Vela SD"
                    } else {
                        "Vela SD (read only)"
                    })])
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
                ejected_at: None,
                disconnected_at: None,
                last_progress: 0,
                attached: false,
                previous_phy,
                writable,
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

    pub fn writable(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.writable)
    }

    /// Service a bounded number of transport steps per syscall.
    /// Return 0 for idle, 1 for activity, and 2 after the host accepted the eject CSW.
    /// SAFETY: KERNEL_LOCK held with MIE masked.
    pub unsafe fn poll(
        &mut self,
        staging: &mut [u8],
        service: &mut impl FnMut(),
    ) -> Result<u32, Errno> {
        let session = self.session.as_mut().ok_or(Errno::Einval)?;
        let now = unsafe { timer::uptime_ticks() }.ok_or(Errno::Eio)?;
        if let Some(disconnected_at) = session.disconnected_at {
            return Ok(
                if now.saturating_sub(disconnected_at) >= USB_DISCONNECT_TICKS {
                    2
                } else {
                    0
                },
            );
        }
        unsafe {
            if !session.attached {
                if now < session.attach_at {
                    return Ok(0);
                }
                update(DCTL, DISCONNECT, 0);
                session.attached = true;
            }
            update(USB_WRAP, PULL_OVERRIDE, PULL_UP_DP);
        }
        let mut activity = false;
        let mut media = SdMedia {
            accessed: false,
            service,
        };
        for _ in 0..8 {
            activity |= session.device.poll(&mut [&mut session.msc]);
            if session.msc.ejected() {
                session.ejected_at.get_or_insert(now);
            }
            if session.device.state() != UsbDeviceState::Configured {
                break;
            }
            activity |= session.msc.step(&mut media, staging);
            if media.accessed {
                break; // At most one SD transfer per syscall.
            }
        }
        if activity {
            session.last_progress = now;
        }
        if let Some(ejected_at) = session.ejected_at {
            let quiet_since = session.last_progress.max(ejected_at);
            if now.saturating_sub(ejected_at) >= EJECT_LIMIT_TICKS
                || (!session.msc.transferring()
                    && now.saturating_sub(quiet_since) >= EJECT_IDLE_TICKS)
            {
                // Leave USB-C electrically detached before the other controller
                // takes over the same port. Do not reassert the OTG pull-up.
                unsafe {
                    update(DCTL, 0, DISCONNECT);
                    update(USB_WRAP, PULL_OVERRIDE, 1 << 12);
                }
                session.disconnected_at = Some(now);
                return Ok(0);
            }
        }
        Ok(u32::from(
            activity || (session.msc.transferring() && now <= session.last_progress),
        ))
    }
}

struct SdMedia<'a, F> {
    accessed: bool,
    service: &'a mut F,
}
impl<F: FnMut()> Media for SdMedia<'_, F> {
    fn read(&mut self, lba: u32, data: &mut [u8; SECTOR]) -> bool {
        self.accessed = true;
        unsafe { sdcard::read_export_sector(lba, data, self.service).is_ok() }
    }

    fn write(&mut self, lba: u32, data: &[u8]) -> bool {
        self.accessed = true;
        unsafe { sdcard::write_export_sectors(lba, data, self.service).is_ok() }
    }

    fn sync(&mut self) -> bool {
        self.accessed = true;
        unsafe { sdcard::sync_export(self.service) }
    }
}
