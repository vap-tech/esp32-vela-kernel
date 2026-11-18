//! One LP-local alarm, independent of mailbox publication backpressure.
const MASK: u64 = (1u64 << 48) - 1;
const HALF: u64 = 1u64 << 47;
pub fn due(now: u64, deadline: u64) -> bool {
    now.wrapping_sub(deadline) & MASK < HALF
}
pub fn earliest(now: u64, a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(
            if a.wrapping_sub(now) & MASK <= b.wrapping_sub(now) & MASK {
                a
            } else {
                b
            },
        ),
        (Some(a), None) | (None, Some(a)) => Some(a),
        _ => None,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub token: u32,
    pub at: u64,
    pub delay_ms: u32,
}
#[derive(Clone, Copy)]
struct Alarm {
    token: u32,
    deadline: u64,
    delay_ms: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancel {
    Cancelled(u32),
    TooLate,
    Unknown,
}
pub struct Wakeup {
    alarm: Option<Alarm>,
    pending: Option<Event>,
    last_fired: u32,
}
impl Wakeup {
    pub const fn new() -> Self {
        Self {
            alarm: None,
            pending: None,
            last_fired: 0,
        }
    }
    pub fn arm(&mut self, token: u32, now: u64, ticks: u64, delay_ms: u32) -> bool {
        if token == 0
            || ticks == 0
            || ticks >= HALF
            || self.alarm.is_some()
            || self.pending.is_some()
        {
            return false;
        }
        self.alarm = Some(Alarm {
            token,
            deadline: now.wrapping_add(ticks) & MASK,
            delay_ms,
        });
        true
    }
    pub fn deadline(&self) -> Option<u64> {
        self.alarm.map(|a| a.deadline)
    }
    pub fn sample(&mut self, now: u64) -> Option<Event> {
        let a = self.alarm?;
        if !due(now, a.deadline) {
            return None;
        }
        let event = Event {
            token: a.token,
            at: now & MASK,
            delay_ms: a.delay_ms,
        };
        self.alarm = None;
        self.pending = Some(event);
        self.last_fired = a.token;
        Some(event)
    }
    pub fn take_event(&mut self) -> Option<Event> {
        self.pending.take()
    }
    pub fn cancel(&mut self, token: u32, now: u64) -> Cancel {
        self.sample(now);
        if self.last_fired == token && token != 0 {
            return Cancel::TooLate;
        }
        match self.alarm {
            Some(a) if a.token == token => {
                self.alarm = None;
                Cancel::Cancelled(a.delay_ms)
            }
            _ => Cancel::Unknown,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_shot_survives_publication_backpressure() {
        let mut w = Wakeup::new();
        assert!(w.arm(5, 100, 10, 10));
        assert!(!w.arm(6, 100, 10, 10));
        assert_eq!(w.sample(109), None);
        assert_eq!(
            w.sample(110),
            Some(Event {
                token: 5,
                at: 110,
                delay_ms: 10
            })
        );
        assert_eq!(w.sample(120), None);
        assert!(!w.arm(6, 120, 10, 10));
        assert_eq!(
            w.take_event(),
            Some(Event {
                token: 5,
                at: 110,
                delay_ms: 10
            })
        );
        assert_eq!(w.take_event(), None);
        assert!(w.arm(6, 120, 10, 10));
    }
    #[test]
    fn cancellation_is_exact_and_cannot_erase_fired_event() {
        let mut w = Wakeup::new();
        w.arm(1, 0, 10, 10);
        assert_eq!(w.cancel(2, 5), Cancel::Unknown);
        assert_eq!(w.cancel(1, 9), Cancel::Cancelled(10));
        assert_eq!(w.sample(10), None);
        w.arm(3, 10, 10, 10);
        assert_eq!(w.cancel(3, 20), Cancel::TooLate);
        assert_eq!(w.take_event().unwrap().token, 3);
        assert_eq!(w.cancel(3, 21), Cancel::TooLate);
    }
    #[test]
    fn alarm_and_arbitration_wrap_48bit_counter() {
        let mut w = Wakeup::new();
        assert!(w.arm(9, MASK - 3, 8, 1));
        assert_eq!(w.deadline(), Some(4));
        assert_eq!(w.sample(3), None);
        assert_eq!(w.sample(4).unwrap().token, 9);
        assert_eq!(earliest(MASK - 5, Some(3), Some(MASK - 1)), Some(MASK - 1));
        assert_eq!(earliest(10, Some(30), Some(20)), Some(20));
        assert_eq!(earliest(10, None, Some(20)), Some(20));
        assert_eq!(earliest(10, None, None), None);
    }
    #[test]
    fn rejects_unrepresentable_tokens_and_deadlines() {
        let mut w = Wakeup::new();
        assert!(!w.arm(0, 0, 1, 1));
        assert!(!w.arm(1, 0, 0, 1));
        assert!(!w.arm(1, 0, HALF, 1));
    }
}
