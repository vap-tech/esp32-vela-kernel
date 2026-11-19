//! HP-side tracking for the autonomous heartbeat, which has no waiting thread.
use crate::id::{Pid, Ticks};

#[derive(Default)]
pub struct Heartbeat {
    outstanding: Option<(u32, Ticks)>,
    pub offline: bool,
}
impl Heartbeat {
    pub fn sent(&mut self, sequence: u32, now: Ticks) {
        self.outstanding = Some((sequence, now.saturating_add(crate::stats::HZ)));
    }
    pub fn retry_due(&mut self, now: Ticks) -> bool {
        let Some((_, deadline)) = self.outstanding.as_mut() else {
            return false;
        };
        if now < *deadline {
            return false;
        }
        self.offline = true;
        *deadline = now.saturating_add(crate::stats::HZ);
        true
    }
    pub fn acknowledge(&mut self, sequence: u32) {
        if self.outstanding.is_some_and(|(s, _)| s == sequence) {
            self.outstanding = None;
            self.offline = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_preserves_sequence_and_recovers_on_late_ack() {
        let mut h = Heartbeat::default();
        h.sent(4, Ticks(10));
        assert!(!h.retry_due(Ticks(109)));
        assert!(h.retry_due(Ticks(110)));
        h.acknowledge(3);
        assert!(h.offline);
        assert!(!h.retry_due(Ticks(111)));
        assert!(h.retry_due(Ticks(210)));
        h.acknowledge(4);
        assert!(!h.offline);
        h.sent(5, Ticks(211));
        h.acknowledge(4);
        assert!(h.retry_due(Ticks(311)));
    }
}

#[derive(Clone, Copy)]
pub enum WakePhase {
    Requested,
    Armed,
    Fired(u32),
}
#[derive(Clone, Copy)]
pub struct WakeTicket {
    pub token: u32,
    pub owner: Pid,
    pub started: Ticks,
    pub expires: Ticks,
    pub delay_ms: u32,
    pub sleep: bool,
    pub phase: WakePhase,
}
impl WakeTicket {
    pub fn release_on_exit(&self, pid: Pid) -> bool {
        self.owner == pid && matches!(self.phase, WakePhase::Fired(_))
    }
}
#[cfg(test)]
#[test]
fn exit_retains_only_unfinished_owned_alarm() {
    let mut ticket = WakeTicket {
        token: 1,
        owner: Pid(2),
        started: Ticks(0),
        expires: Ticks(5),
        delay_ms: 10,
        sleep: false,
        phase: WakePhase::Requested,
    };
    assert!(!ticket.release_on_exit(Pid(2)));
    ticket.phase = WakePhase::Armed;
    assert!(!ticket.release_on_exit(Pid(2)));
    ticket.phase = WakePhase::Fired(1);
    assert!(!ticket.release_on_exit(Pid(3)));
    assert!(ticket.release_on_exit(Pid(2)));
}
