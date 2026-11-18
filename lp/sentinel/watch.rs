//! LP-private heartbeat observer. Only local slow-clock timestamps enter here.
const MASK: u64 = (1u64 << 48) - 1;
pub struct Watch {
    last: Option<u64>,
    timeout: u64,
    window_start: u64,
    pub state: u32,
    pub beats: u32,
    pub missed: u32,
}
impl Watch {
    pub const fn new() -> Self {
        Self {
            last: None,
            timeout: 0,
            window_start: 0,
            state: 0,
            beats: 0,
            missed: 0,
        }
    }
    pub fn age(&self, now: u64) -> u64 {
        self.last.map_or(0, |last| now.wrapping_sub(last) & MASK)
    }
    pub fn sample(&mut self, now: u64) {
        if self.state == 1 && (now.wrapping_sub(self.window_start) & MASK) >= self.timeout {
            self.state = 2;
            self.missed = self.missed.saturating_add(1).min(0x3fff_ffff);
        }
    }
    pub fn deadline(&self) -> Option<u64> {
        (self.state == 1).then(|| (self.window_start + self.timeout) & MASK)
    }
    pub fn prepare(&mut self) {
        debug_assert_eq!(self.state, 1);
        self.state = 3;
    }
    pub fn end_sleep(&mut self, now: u64) {
        if self.state == 3 {
            self.state = 1;
            self.window_start = now;
        }
    }
    pub fn heartbeat(&mut self, now: u64, timeout: u64) {
        self.sample(now);
        self.last = Some(now);
        self.window_start = now;
        self.timeout = timeout;
        self.state = 1;
        self.beats = self.beats.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn once_per_missed_interval_and_recovery() {
        let mut w = Watch::new();
        assert_eq!(w.deadline(), None);
        w.heartbeat(100, 10);
        w.sample(109);
        assert_eq!((w.state, w.missed), (1, 0));
        w.sample(110);
        w.sample(111);
        assert_eq!((w.state, w.missed, w.deadline()), (2, 1, None));
        w.heartbeat(120, 10);
        assert_eq!((w.state, w.beats, w.missed), (1, 2, 1));
        w.sample(130);
        assert_eq!((w.state, w.missed), (2, 2));
    }
    #[test]
    fn handles_48bit_counter_wrap() {
        let mut w = Watch::new();
        w.heartbeat(MASK - 3, 8);
        assert_eq!(w.deadline(), Some(4));
        w.sample(3);
        assert_eq!(w.state, 1);
        w.sample(4);
        assert_eq!((w.state, w.missed), (2, 1));
    }
    #[test]
    fn late_heartbeat_preserves_missed_transition() {
        let mut w = Watch::new();
        w.heartbeat(0, 10);
        w.heartbeat(11, 10);
        assert_eq!((w.state, w.beats, w.missed), (1, 2, 1));
    }
}

#[cfg(test)]
#[test]
fn prepared_window_preserves_heartbeat_age_and_rearms_on_return() {
    let mut watch = Watch::new();
    watch.heartbeat(MASK - 4, 10);
    watch.prepare();
    watch.sample(100);
    assert_eq!((watch.deadline(), watch.missed), (None, 0));
    watch.end_sleep(100);
    assert_eq!(
        (watch.age(101), watch.beats, watch.deadline()),
        (106, 1, Some(110))
    );
    watch.sample(109);
    assert_eq!(watch.missed, 0);
    watch.sample(110);
    assert_eq!(watch.missed, 1);
}
