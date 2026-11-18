//! LP-private heartbeat observer. Only local slow-clock timestamps enter here.
const MASK: u64 = (1u64 << 48) - 1;
pub struct Watch {
    last: Option<u64>,
    timeout: u64,
    pub state: u32,
    pub beats: u32,
    pub missed: u32,
}
impl Watch {
    pub const fn new() -> Self {
        Self {
            last: None,
            timeout: 0,
            state: 0,
            beats: 0,
            missed: 0,
        }
    }
    pub fn age(&self, now: u64) -> u64 {
        self.last.map_or(0, |last| now.wrapping_sub(last) & MASK)
    }
    pub fn sample(&mut self, now: u64) {
        if self.state == 1 && self.age(now) >= self.timeout {
            self.state = 2;
            self.missed = self.missed.saturating_add(1).min(0x3fff_ffff);
        }
    }
    pub fn heartbeat(&mut self, now: u64, timeout: u64) {
        self.sample(now);
        self.last = Some(now);
        self.timeout = timeout;
        self.state = 1;
        self.beats = self.beats.saturating_add(1);
    }
}
