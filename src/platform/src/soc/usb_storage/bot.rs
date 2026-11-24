//! USB MSC Bulk-Only Transport. One removable SCSI LUN, 512-byte sectors.
use usb_device::{
    UsbError,
    bus::{InterfaceNumber, UsbBus, UsbBusAllocator},
    class_prelude::{
        ControlIn, ControlOut, DescriptorWriter, EndpointAddress, EndpointIn, EndpointOut, UsbClass,
    },
    control::{Recipient, Request, RequestType},
};

use super::SECTOR;

const PACKET: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    In,
    Out,
}

#[derive(Clone, Copy)]
struct Command {
    tag: [u8; 4],
    len: u32,
    direction: Direction,
    cdb: [u8; 16],
    cdb_len: usize,
}

impl Command {
    fn decode(packet: &[u8]) -> Option<Self> {
        if packet.len() != 31
            || &packet[..4] != b"USBC"
            || packet[12] & 0x7f != 0
            || packet[13] != 0
            || !(1..=16).contains(&packet[14])
        {
            return None;
        }
        Some(Self {
            tag: packet[4..8].try_into().unwrap(),
            len: u32::from_le_bytes(packet[8..12].try_into().unwrap()),
            direction: if packet[12] == 0x80 {
                Direction::In
            } else {
                Direction::Out
            },
            cdb: packet[15..31].try_into().unwrap(),
            cdb_len: usize::from(packet[14]),
        })
    }
}

#[derive(Clone, Copy)]
enum Data {
    Reply(usize),
    Read(u32),
    Write(u32),
    Discard(u32),
}

#[derive(Clone, Copy)]
enum Phase {
    Command,
    Data {
        cmd: Command,
        data: Data,
        offset: u32,
    },
    Status {
        cmd: Command,
        done: u32,
        code: u8,
        eject: bool,
    },
    ReadFailed {
        cmd: Command,
        done: u32,
    },
    EjectPending,
    Ejected,
    ResetRequired,
}

pub(super) trait Media {
    fn read(&mut self, lba: u32, data: &mut [u8; SECTOR]) -> bool;
    fn write(&mut self, lba: u32, data: &[u8; SECTOR]) -> bool;
    fn sync(&mut self) -> bool;
}

pub(super) struct Msc<'a, B: UsbBus> {
    input: EndpointIn<'a, B>,
    output: EndpointOut<'a, B>,
    interface: InterfaceNumber,
    phase: Phase,
    sectors: u32,
    writable: bool,
    prevented: bool,
    sense: (u8, u8),
    block: [u8; SECTOR],
    cached_lba: Option<u32>,
}

impl<'a, B: UsbBus> Msc<'a, B> {
    pub fn new(alloc: &'a UsbBusAllocator<B>, sectors: u32, writable: bool) -> Self {
        Self {
            input: alloc.bulk(PACKET as u16),
            output: alloc.bulk(PACKET as u16),
            interface: alloc.interface(),
            phase: Phase::Command,
            sectors,
            writable,
            prevented: false,
            sense: (0, 0),
            block: [0; SECTOR],
            cached_lba: None,
        }
    }

    pub fn ejected(&self) -> bool {
        matches!(self.phase, Phase::Ejected)
    }

    pub fn transferring(&self) -> bool {
        matches!(
            self.phase,
            Phase::Data { .. } | Phase::ReadFailed { .. } | Phase::Status { .. }
        )
    }

    fn status(&mut self, cmd: Command, done: u32, code: u8) {
        self.phase = Phase::Status {
            cmd,
            done,
            code,
            eject: false,
        };
    }

    fn data(&mut self, cmd: Command, data: Data) {
        self.cached_lba = None;
        self.phase = Phase::Data {
            cmd,
            data,
            offset: 0,
        };
    }

    fn reply(&mut self, cmd: Command, bytes: &[u8]) {
        if cmd.len == 0 {
            self.status(cmd, 0, 0);
        } else if cmd.direction == Direction::In {
            let count = bytes.len().min(cmd.len as usize);
            self.block[..count].copy_from_slice(&bytes[..count]);
            self.data(cmd, Data::Reply(count));
        } else {
            self.fail(cmd, (5, 0x24));
        }
    }

    fn fail(&mut self, cmd: Command, sense: (u8, u8)) {
        self.sense = sense;
        match (cmd.len, cmd.direction) {
            (0, _) => self.status(cmd, 0, 1),
            (_, Direction::In) => self.data(cmd, Data::Reply(0)),
            (_, Direction::Out) => self.data(cmd, Data::Discard(0)),
        }
    }

    fn start(&mut self, cmd: Command, media: &mut impl Media) {
        let cdb = &cmd.cdb[..cmd.cdb_len];
        let mut response = [0; 36];
        match (cdb[0], cdb.len()) {
            (0x12, 6) if cdb[1] == 0 && cdb[2] == 0 => {
                response[..5].copy_from_slice(&[0, 0x80, 4, 2, 31]);
                response[8..16].copy_from_slice(b"VELA    ");
                response[16..32].copy_from_slice(b"Tab5 microSD    ");
                response[32..].copy_from_slice(b"0002");
                self.reply(cmd, &response[..36.min(usize::from(cdb[4]))]);
            }
            (0x12, 6) if cdb[1] == 1 && cdb[2] == 0 => {
                response[3] = 1; // VPD supported page list contains page zero.
                self.reply(cmd, &response[..5.min(usize::from(cdb[4]))]);
            }
            (0x03, 6) if cdb[1] == 0 => {
                response[0] = 0x70;
                response[2] = self.sense.0;
                response[7] = 10;
                response[12] = self.sense.1;
                self.sense = (0, 0);
                self.reply(cmd, &response[..18.min(usize::from(cdb[4]))]);
            }
            (0x1a, 6) => {
                response[0] = 3;
                response[2] = if self.writable { 0 } else { 0x80 };
                self.reply(cmd, &response[..4.min(usize::from(cdb[4]))]);
            }
            (0x5a, 10) => {
                response[1] = 6;
                response[3] = if self.writable { 0 } else { 0x80 };
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]);
                self.reply(cmd, &response[..8.min(usize::from(alloc))]);
            }
            (0x00, 6) if cmd.len == 0 && self.sectors != 0 => self.status(cmd, 0, 0),
            (0x25, 10) if self.sectors != 0 => {
                response[..4].copy_from_slice(&(self.sectors - 1).to_be_bytes());
                response[4..8].copy_from_slice(&(SECTOR as u32).to_be_bytes());
                self.reply(cmd, &response[..8]);
            }
            (0x28 | 0x2a, 10) if self.sectors != 0 => {
                let lba = u32::from_be_bytes(cdb[2..6].try_into().unwrap());
                let count = u32::from(u16::from_be_bytes([cdb[7], cdb[8]]));
                let write = cdb[0] == 0x2a;
                let direction = if write { Direction::Out } else { Direction::In };
                if lba.checked_add(count).is_none_or(|end| end > self.sectors) {
                    self.fail(cmd, (5, 0x21));
                } else if count * SECTOR as u32 != cmd.len
                    || (count != 0 && cmd.direction != direction)
                {
                    self.fail(cmd, (5, 0x24));
                } else if write && !self.writable {
                    self.fail(cmd, (7, 0x27));
                } else if count == 0 {
                    self.status(cmd, 0, 0);
                } else {
                    self.data(
                        cmd,
                        if write {
                            Data::Write(lba)
                        } else {
                            Data::Read(lba)
                        },
                    );
                }
            }
            (0x1e, 6) if cmd.len == 0 && cdb[4] <= 1 => {
                self.prevented = cdb[4] == 1;
                self.status(cmd, 0, 0);
            }
            (0x35, 10) | (0x91, 16) if cmd.len == 0 => {
                if media.sync() {
                    self.status(cmd, 0, 0);
                } else {
                    self.fail(cmd, (3, 0x0c));
                }
            }
            (0x1b, 6) if cmd.len == 0 && cdb[4] & !3 == 0 => {
                let eject = cdb[4] == 2;
                if eject && self.prevented {
                    self.fail(cmd, (5, 0x53));
                } else if eject && !media.sync() {
                    self.fail(cmd, (3, 0x0c));
                } else {
                    self.phase = Phase::Status {
                        cmd,
                        done: 0,
                        code: 0,
                        eject,
                    };
                }
            }
            _ => self.fail(
                cmd,
                if self.sectors == 0 {
                    (2, 0x3a)
                } else {
                    (5, 0x20)
                },
            ),
        }
    }

    /// Advance one BOT step. At most one sector is passed to the backend.
    pub fn step(&mut self, media: &mut impl Media) -> bool {
        let mut packet = [0; PACKET];
        match self.phase {
            Phase::Command => match self.output.read(&mut packet) {
                Ok(n) => {
                    if let Some(cmd) = Command::decode(&packet[..n]) {
                        self.start(cmd, media);
                    } else {
                        self.require_reset();
                    }
                    true
                }
                Err(UsbError::WouldBlock) => false,
                Err(_) => {
                    self.require_reset();
                    true
                }
            },
            Phase::Data { cmd, data, offset } => match data {
                Data::Reply(len) => {
                    let start = offset as usize;
                    // A short response ending on a packet boundary needs a ZLP.
                    let end = (start + PACKET).min(len);
                    let short = end - start < PACKET;
                    match self.input.write(&self.block[start..end]) {
                        Ok(_) if end as u32 == cmd.len || short => {
                            self.status(cmd, end as u32, u8::from(len == 0));
                            true
                        }
                        Ok(_) => {
                            self.phase = Phase::Data {
                                cmd,
                                data,
                                offset: end as u32,
                            };
                            true
                        }
                        Err(_) => false,
                    }
                }
                Data::Read(lba) => {
                    let within = offset as usize % SECTOR;
                    let sector = lba + offset / SECTOR as u32;
                    if self.cached_lba != Some(sector) {
                        if !media.read(sector, &mut self.block) {
                            self.sense = (3, 0x11);
                            self.phase = Phase::ReadFailed { cmd, done: offset };
                            return true;
                        }
                        self.cached_lba = Some(sector);
                    }
                    if self
                        .input
                        .write(&self.block[within..within + PACKET])
                        .is_ok()
                    {
                        let offset = offset + PACKET as u32;
                        if offset == cmd.len {
                            self.status(cmd, offset, 0);
                        } else {
                            self.phase = Phase::Data { cmd, data, offset };
                        }
                        true
                    } else {
                        false
                    }
                }
                Data::Write(_) | Data::Discard(_) => match self.output.read(&mut packet) {
                    Ok(n) => {
                        let left = (cmd.len - offset) as usize;
                        if n != left.min(PACKET) {
                            self.status(cmd, offset, 2);
                            return true;
                        }
                        let next = offset + n as u32;
                        if let Data::Write(lba) = data {
                            let within = offset as usize % SECTOR;
                            self.block[within..within + n].copy_from_slice(&packet[..n]);
                            if within + n == SECTOR
                                && !media.write(lba + offset / SECTOR as u32, &self.block)
                            {
                                self.sense = (3, 0x0c);
                                if next == cmd.len {
                                    self.status(cmd, offset + n as u32 - SECTOR as u32, 1);
                                } else {
                                    self.phase = Phase::Data {
                                        cmd,
                                        data: Data::Discard(next - SECTOR as u32),
                                        offset: next,
                                    };
                                }
                                return true;
                            }
                        }
                        if next == cmd.len {
                            if let Data::Discard(committed) = data {
                                self.status(cmd, committed, 1);
                            } else {
                                self.status(cmd, next, 0);
                            }
                        } else {
                            self.phase = Phase::Data {
                                cmd,
                                data,
                                offset: next,
                            };
                        }
                        true
                    }
                    Err(UsbError::WouldBlock) => false,
                    Err(_) => {
                        self.require_reset();
                        true
                    }
                },
            },
            Phase::ReadFailed { cmd, done } => {
                if self.input.write(&[]).is_ok() {
                    self.status(cmd, done, 1);
                    true
                } else {
                    false
                }
            }
            Phase::Status {
                cmd,
                done,
                code,
                eject,
            } => {
                let mut csw = *b"USBS\0\0\0\0\0\0\0\0\0";
                csw[4..8].copy_from_slice(&cmd.tag);
                csw[8..12].copy_from_slice(&cmd.len.saturating_sub(done).to_le_bytes());
                csw[12] = code;
                if self.input.write(&csw).is_ok() {
                    self.phase = if eject {
                        Phase::EjectPending
                    } else {
                        Phase::Command
                    };
                    true
                } else {
                    false
                }
            }
            Phase::EjectPending | Phase::Ejected | Phase::ResetRequired => false,
        }
    }

    fn require_reset(&mut self) {
        self.phase = Phase::ResetRequired;
        self.input.stall();
        self.output.stall();
    }

    fn addressed(&self, req: &Request) -> bool {
        req.request_type == RequestType::Class
            && req.recipient == Recipient::Interface
            && req.index == u16::from(u8::from(self.interface))
            && req.value == 0
    }
}

impl<B: UsbBus> UsbClass<B> for Msc<'_, B> {
    fn get_configuration_descriptors(&self, writer: &mut DescriptorWriter) -> Result<(), UsbError> {
        writer.interface(self.interface, 8, 6, 0x50)?;
        writer.endpoint(&self.input)?;
        writer.endpoint(&self.output)?;
        Ok(())
    }

    fn control_in(&mut self, xfer: ControlIn<B>) {
        let req = xfer.request();
        if self.addressed(req) && req.request == 0xfe && req.length == 1 {
            let _ = xfer.accept_with(&[0]); // Max LUN.
        }
    }

    fn control_out(&mut self, xfer: ControlOut<B>) {
        let req = xfer.request();
        if self.addressed(req) && req.request == 0xff && req.length == 0 {
            self.reset();
            let _ = xfer.accept();
        }
    }

    fn reset(&mut self) {
        self.phase = Phase::Command;
        self.prevented = false;
        self.cached_lba = None;
        self.sense = (0, 0);
    }

    fn endpoint_in_complete(&mut self, addr: EndpointAddress) {
        if addr == self.input.address() && matches!(self.phase, Phase::EjectPending) {
            self.phase = Phase::Ejected;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        vec::Vec,
    };
    use usb_device::{
        UsbDirection,
        bus::PollResult,
        class_prelude::EndpointType,
        device::{UsbDeviceBuilder, UsbVidPid},
    };

    #[derive(Default)]
    struct Wire {
        from_host: VecDeque<Vec<u8>>,
        to_host: Vec<Vec<u8>>,
    }

    struct Bus(Arc<Mutex<Wire>>);

    impl UsbBus for Bus {
        fn alloc_ep(
            &mut self,
            dir: UsbDirection,
            addr: Option<EndpointAddress>,
            _: EndpointType,
            _: u16,
            _: u8,
        ) -> Result<EndpointAddress, UsbError> {
            Ok(addr.unwrap_or_else(|| EndpointAddress::from_parts(1, dir)))
        }
        fn enable(&mut self) {}
        fn reset(&self) {}
        fn set_device_address(&self, _: u8) {}
        fn write(&self, _: EndpointAddress, packet: &[u8]) -> Result<usize, UsbError> {
            self.0.lock().unwrap().to_host.push(packet.to_vec());
            Ok(packet.len())
        }
        fn read(&self, _: EndpointAddress, packet: &mut [u8]) -> Result<usize, UsbError> {
            let data = self
                .0
                .lock()
                .unwrap()
                .from_host
                .pop_front()
                .ok_or(UsbError::WouldBlock)?;
            packet[..data.len()].copy_from_slice(&data);
            Ok(data.len())
        }
        fn set_stalled(&self, _: EndpointAddress, _: bool) {}
        fn is_stalled(&self, _: EndpointAddress) -> bool {
            false
        }
        fn suspend(&self) {}
        fn resume(&self) {}
        fn poll(&self) -> PollResult {
            PollResult::None
        }
    }

    #[derive(Default)]
    struct Disk {
        writes: Vec<(u32, [u8; SECTOR])>,
        flushes: usize,
    }

    impl Media for Disk {
        fn read(&mut self, _: u32, data: &mut [u8; SECTOR]) -> bool {
            data.fill(0xa5);
            true
        }
        fn write(&mut self, lba: u32, data: &[u8; SECTOR]) -> bool {
            self.writes.push((lba, *data));
            true
        }
        fn sync(&mut self) -> bool {
            self.flushes += 1;
            true
        }
    }

    fn send(wire: &Mutex<Wire>, bytes: &[u8]) {
        wire.lock().unwrap().from_host.push_back(bytes.to_vec());
    }

    fn command(wire: &Mutex<Wire>, cdb: &[u8], len: u32, input: bool) {
        let mut cbw = [0; 31];
        cbw[..4].copy_from_slice(b"USBC");
        cbw[4..8].copy_from_slice(b"TEST");
        cbw[8..12].copy_from_slice(&len.to_le_bytes());
        cbw[12] = if input { 0x80 } else { 0 };
        cbw[14] = cdb.len() as u8;
        cbw[15..15 + cdb.len()].copy_from_slice(cdb);
        send(wire, &cbw);
    }

    fn assert_csw(wire: &Mutex<Wire>, residue: u32, status: u8) {
        let state = wire.lock().unwrap();
        let csw = state.to_host.last().unwrap();
        assert_eq!(&csw[..8], b"USBSTEST");
        assert_eq!(u32::from_le_bytes(csw[8..12].try_into().unwrap()), residue);
        assert_eq!(csw[12], status);
    }

    #[test]
    fn only_commits_complete_bounded_writes() {
        for (writable, lba, bytes, direction, expected) in [
            (true, 7, 512, false, 1),  // valid final sector
            (false, 7, 512, false, 0), // read-only
            (true, 8, 512, false, 0),  // outside media
            (true, 7, 256, false, 0),  // CBW disagrees with CDB
            (true, 7, 512, true, 0),   // wrong transfer direction
        ] {
            let wire = Arc::new(Mutex::new(Wire::default()));
            let bus = UsbBusAllocator::new(Bus(wire.clone()));
            let mut msc = Msc::new(&bus, 8, writable);
            let _device = UsbDeviceBuilder::new(&bus, UsbVidPid(1, 2)).build();
            let mut disk = Disk::default();
            command(
                &wire,
                &[0x2a, 0, 0, 0, 0, lba, 0, 0, 1, 0],
                bytes,
                direction,
            );
            assert!(msc.step(&mut disk));
            if !direction {
                for i in 0..bytes / PACKET as u32 {
                    send(&wire, &[0xa5; PACKET]);
                    assert!(msc.step(&mut disk));
                    if i + 1 != bytes / PACKET as u32 {
                        assert!(disk.writes.is_empty());
                    }
                }
            } else {
                assert!(msc.step(&mut disk)); // end failed IN phase with a ZLP
            }
            assert!(msc.step(&mut disk)); // CSW
            assert_eq!(disk.writes.len(), expected);
            if expected == 1 {
                assert_eq!(disk.writes[0], (u32::from(lba), [0xa5; SECTOR]));
                assert_csw(&wire, 0, 0);
            } else {
                assert_csw(&wire, bytes, 1);
            }
        }
    }

    #[test]
    fn eject_requires_sync_and_a_consumed_status() {
        let wire = Arc::new(Mutex::new(Wire::default()));
        let bus = UsbBusAllocator::new(Bus(wire.clone()));
        let mut msc = Msc::new(&bus, 8, true);
        let _device = UsbDeviceBuilder::new(&bus, UsbVidPid(1, 2)).build();
        let mut disk = Disk::default();
        command(&wire, &[0x1e, 0, 0, 0, 1, 0], 0, false);
        assert!(msc.step(&mut disk));
        assert!(msc.step(&mut disk));
        command(&wire, &[0x1b, 0, 0, 0, 2, 0], 0, false);
        assert!(msc.step(&mut disk));
        assert!(msc.step(&mut disk));
        assert_csw(&wire, 0, 1);
        assert_eq!(disk.flushes, 0);
        command(&wire, &[0x1e, 0, 0, 0, 0, 0], 0, false);
        assert!(msc.step(&mut disk));
        assert!(msc.step(&mut disk));
        command(&wire, &[0x1b, 0, 0, 0, 2, 0], 0, false);
        assert!(msc.step(&mut disk));
        assert_eq!(disk.flushes, 1);
        assert!(msc.step(&mut disk));
        assert_csw(&wire, 0, 0);
        assert!(!msc.ejected());
        msc.endpoint_in_complete(msc.input.address());
        assert!(msc.ejected());
    }
}
