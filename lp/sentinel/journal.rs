//! LP-local bounded journal. Reading is nondestructive. Sequences never wrap.
pub const CAPACITY: usize = 16;
const MASK: u64 = (1u64 << 48) - 1;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub sequence: u32,
    pub timestamp: u64,
    pub kind: u16,
    pub arguments: [u32; 2],
}
impl Record {
    const EMPTY: Self = Self {
        sequence: 0,
        timestamp: 0,
        kind: 0,
        arguments: [0; 2],
    };
    pub fn words(self) -> [u32; 5] {
        [
            self.sequence,
            self.timestamp as u32,
            ((self.timestamp >> 32) as u32 & 0xffff) | (u32::from(self.kind) << 16),
            self.arguments[0],
            self.arguments[1],
        ]
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    Overwritten,
    Unknown,
}
pub struct Journal {
    records: [Record; CAPACITY],
    origin: u64,
    next: u32,
    count: usize,
    overwritten: u32,
}
impl Journal {
    pub const fn new(origin: u64) -> Self {
        Self {
            records: [Record::EMPTY; CAPACITY],
            origin,
            next: 1,
            count: 0,
            overwritten: 0,
        }
    }
    pub fn append(&mut self, now: u64, kind: u16, arg0: u32, arg1: u32) -> Option<Record> {
        // Retire the producer at sequence exhaustion instead of reviving old IDs.
        let next = self.next.checked_add(1)?;
        let record = Record {
            sequence: self.next,
            timestamp: now.wrapping_sub(self.origin) & MASK,
            kind,
            arguments: [arg0, arg1],
        };
        self.records[(self.next as usize - 1) % CAPACITY] = record;
        self.next = next;
        if self.count == CAPACITY {
            self.overwritten += 1;
        } else {
            self.count += 1;
        }
        Some(record)
    }
    pub fn metadata(&self) -> [u32; 4] {
        let newest = self.next - 1;
        let oldest = if self.count == 0 {
            0
        } else {
            self.next - self.count as u32
        };
        [oldest, newest, self.overwritten, CAPACITY as u32]
    }
    pub fn read(&self, sequence: u32) -> Result<Record, ReadError> {
        let [oldest, newest, ..] = self.metadata();
        if sequence == 0 || sequence > newest {
            return Err(ReadError::Unknown);
        }
        if sequence < oldest {
            return Err(ReadError::Overwritten);
        }
        let record = self.records[(sequence as usize - 1) % CAPACITY];
        if record.sequence != sequence {
            return Err(ReadError::Overwritten);
        }
        Ok(record)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrap_retains_exact_tail_and_counts_overwrites() {
        let mut j = Journal::new(100);
        assert_eq!(j.metadata(), [0, 0, 0, 16]);
        for i in 0..20 {
            j.append(100 + i, 5, i as u32, 0xffff_ffff).unwrap();
        }
        assert_eq!(j.metadata(), [5, 20, 4, 16]);
        assert_eq!(j.read(4), Err(ReadError::Overwritten));
        assert_eq!(j.read(21), Err(ReadError::Unknown));
        assert_eq!(j.read(0), Err(ReadError::Unknown));
        let record = j.read(5).unwrap();
        assert_eq!((record.timestamp, record.arguments), (4, [4, 0xffff_ffff]));
        assert_eq!(j.read(5), Ok(record)); // no consuming read
        assert_eq!(j.metadata(), [5, 20, 4, 16]);
    }
    #[test]
    fn timestamp_wrap_and_packed_arguments() {
        let mut j = Journal::new(MASK - 2);
        let record = j.append(3, 0xabcd, 0x12345678, 0xfedcba98).unwrap();
        assert_eq!(record.words(), [1, 6, 0xabcd0000, 0x12345678, 0xfedcba98]);
        let mut j = Journal::new(0);
        let record = j.append(0x123456789abc, 5, 1, 2).unwrap();
        assert_eq!(record.words(), [1, 0x56789abc, 0x00051234, 1, 2]);
    }
    #[test]
    fn exhaustion_never_reuses_sequence_or_changes_tail() {
        let mut j = Journal::new(0);
        j.next = u32::MAX - 1;
        assert!(j.append(0, 1, 0, 0).is_some());
        let before = j.metadata();
        assert!(j.append(1, 1, 0, 0).is_none());
        assert_eq!(j.metadata(), before);
    }
}
