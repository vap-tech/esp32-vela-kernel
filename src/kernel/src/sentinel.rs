//! HP-side state machines for the LP Sentinel request/heartbeat link and wake tickets.
//!
//! Because HP and LP share a single hardware mailbox request slot without locks,
//! at most one request may own `(ARG0, ARG1, REQUEST_SEQ, COMMAND)` until LP
//! answers that exact `sequence`. When any request (autonomous heartbeat or user
//! command) exceeds its response deadline, [`RequestLink`] transitions the link
//! to `offline` so callers immediately observe [`Errno::Etimedout`] instead of a
//! permanently stuck [`Errno::Ebusy`], while preserving slot ownership and
//! periodically reasserting the doorbell until LP's late reply arrives.

use crate::{
    errno::Errno,
    id::{Pid, Ticks},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    Heartbeat,
    Command(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InFlight {
    pub sequence: u32,
    pub kind: RequestKind,
    pub argument: u32,
    deadline: Ticks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    Heartbeat(u32),
    Command(u32),
}

#[derive(Default)]
pub struct RequestLink {
    ready: bool,
    sequence: u32,
    outstanding: Option<InFlight>,
    offline: bool,
}

impl RequestLink {
    pub const fn new() -> Self {
        Self {
            ready: false,
            sequence: 0,
            outstanding: None,
            offline: false,
        }
    }

    pub fn mark_ready(&mut self) {
        self.ready = true;
    }

    pub const fn is_ready(&self) -> bool {
        self.ready
    }

    pub const fn is_offline(&self) -> bool {
        self.offline
    }

    pub const fn is_pending(&self) -> bool {
        self.outstanding.is_some()
    }

    pub const fn in_flight(&self) -> Option<InFlight> {
        self.outstanding
    }

    /// Return `Ok(ready)` when healthy/booting, or `Err(Errno::Etimedout)` while offline.
    pub const fn status(&self) -> Result<bool, Errno> {
        if self.offline {
            Err(Errno::Etimedout)
        } else {
            Ok(self.ready)
        }
    }

    /// Allocate the next monotonic sequence number and arm a 1-second (`HZ`) deadline.
    pub fn submit(&mut self, kind: RequestKind, argument: u32, now: Ticks) -> Result<u32, Errno> {
        if !self.ready {
            return Err(Errno::Eagain);
        }
        if self.offline {
            return Err(Errno::Etimedout);
        }
        if self.outstanding.is_some() {
            return Err(Errno::Ebusy);
        }
        if self.sequence == i32::MAX as u32 {
            return Err(Errno::Eoverflow);
        }
        self.sequence += 1;
        self.outstanding = Some(InFlight {
            sequence: self.sequence,
            kind,
            argument,
            deadline: now.saturating_add(crate::stats::HZ),
        });
        Ok(self.sequence)
    }

    /// Check whether the in-flight request has exceeded its deadline; if so, mark
    /// the link offline and schedule the next idempotent doorbell reassertion.
    pub fn poll_timeout(&mut self, now: Ticks) -> Option<Retry> {
        let inflight = self.outstanding.as_mut()?;
        if now < inflight.deadline {
            return None;
        }
        self.offline = true;
        inflight.deadline = now.saturating_add(crate::stats::HZ);
        Some(match inflight.kind {
            RequestKind::Heartbeat => Retry::Heartbeat(inflight.sequence),
            RequestKind::Command(cmd) => Retry::Command(cmd),
        })
    }

    /// Complete the in-flight request matching `sequence` and clear any offline state.
    pub fn complete(&mut self, sequence: u32) -> Option<InFlight> {
        if self.outstanding.is_some_and(|req| req.sequence == sequence) {
            self.offline = false;
            self.outstanding.take()
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakePhase {
    Requested,
    Armed,
    Fired(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
mod tests {
    use super::*;

    #[test]
    fn heartbeat_retry_preserves_sequence_and_recovers_on_late_ack() {
        let mut link = RequestLink::new();
        assert_eq!(link.status(), Ok(false));
        assert_eq!(
            link.submit(RequestKind::Heartbeat, 3000, Ticks(0)),
            Err(Errno::Eagain)
        );

        link.mark_ready();
        assert_eq!(link.status(), Ok(true));
        assert_eq!(link.submit(RequestKind::Heartbeat, 3000, Ticks(10)), Ok(1));
        assert!(!link.is_offline());
        assert_eq!(link.poll_timeout(Ticks(109)), None);
        assert_eq!(link.poll_timeout(Ticks(110)), Some(Retry::Heartbeat(1)));
        assert!(link.is_offline());
        assert_eq!(link.status(), Err(Errno::Etimedout));

        // Stale sequence never clears the active request or offline flag.
        assert_eq!(link.complete(0), None);
        assert!(link.is_offline());
        assert_eq!(link.poll_timeout(Ticks(111)), None);
        assert_eq!(link.poll_timeout(Ticks(210)), Some(Retry::Heartbeat(1)));

        // Late ACK for sequence 1 restores the link and frees the mailbox slot.
        let done = link.complete(1).unwrap();
        assert_eq!(done.sequence, 1);
        assert!(!link.is_offline());
        assert!(!link.is_pending());
        assert_eq!(link.status(), Ok(true));

        assert_eq!(link.submit(RequestKind::Heartbeat, 3000, Ticks(211)), Ok(2));
        assert_eq!(link.complete(1), None);
        assert_eq!(link.poll_timeout(Ticks(311)), Some(Retry::Heartbeat(2)));
    }

    #[test]
    fn command_timeout_reports_etimedout_instead_of_stuck_ebusy_and_recovers() {
        let mut link = RequestLink::new();
        link.mark_ready();

        let seq = link
            .submit(RequestKind::Command(0x21), 77, Ticks(50))
            .unwrap();
        assert_eq!(seq, 1);
        // Concurrent request before the 1s deadline reports Ebusy.
        assert_eq!(
            link.submit(RequestKind::Command(0x22), 0, Ticks(90)),
            Err(Errno::Ebusy)
        );
        assert_eq!(link.poll_timeout(Ticks(149)), None);

        // At the deadline, link becomes offline: subsequent callers get Etimedout,
        // never a permanently stuck Ebusy, while the mailbox slot stays reserved.
        assert_eq!(link.poll_timeout(Ticks(150)), Some(Retry::Command(0x21)));
        assert!(link.is_offline());
        assert!(link.is_pending());
        assert_eq!(link.status(), Err(Errno::Etimedout));
        assert_eq!(
            link.submit(RequestKind::Command(0x22), 0, Ticks(151)),
            Err(Errno::Etimedout)
        );
        assert_eq!(link.in_flight().unwrap().argument, 77);
        assert_eq!(link.poll_timeout(Ticks(250)), Some(Retry::Command(0x21)));

        // If LP eventually answers sequence 1, the link recovers cleanly.
        let finished = link.complete(1).unwrap();
        assert_eq!(finished.kind, RequestKind::Command(0x21));
        assert_eq!(finished.argument, 77);
        assert!(!link.is_offline());
        assert!(!link.is_pending());
        assert_eq!(
            link.submit(RequestKind::Command(0x22), 0, Ticks(251)),
            Ok(2)
        );
    }

    #[test]
    fn sequence_exhaustion_returns_eoverflow() {
        let mut link = RequestLink::new();
        link.mark_ready();
        link.sequence = i32::MAX as u32;
        assert_eq!(
            link.submit(RequestKind::Command(1), 0, Ticks(0)),
            Err(Errno::Eoverflow)
        );
    }

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
}
