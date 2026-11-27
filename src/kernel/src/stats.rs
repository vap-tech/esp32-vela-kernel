use crate::{errno::Errno, id::Ticks};

/// Scheduler sampling contract, independent of trap/controller implementation.
/// 100 Hz scheduling; load sampled each 5 seconds. Fixed point Q16 EWMA of
/// Ready+Running tasks, alpha=exp(-5/tau), tau=60/300/900 seconds.
/// Rounded Q16 coefficients: 60296, 64453, 65173. No floating point/heap needed.
pub const HZ: u64 = 100;

/// Base 10 ms periodic quantum in 16 MHz `SYSTIMER` ticks (`10 ms * 16_000_000 Hz = 160_000`).
pub const BASE_PERIOD_16M: u64 = 160_000;

/// Maximum single-interval tickless sleep window in 10 ms ticks (`400 * 160_000 = 64_000_000 < 2^26`).
pub const MAX_TICKLESS_TICKS: u32 = 400;

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Snapshot {
    pub total_tasks: u32,
    pub runnable_tasks: u32,
    pub running_tasks: u32,
    pub last_task_id: u32,
    pub scheduler_ticks: u64,
    pub uptime_ticks: u64,
    pub load_q16: [u32; 3],
}

#[derive(Default)]
pub struct LoadAverage {
    value: [u32; 3],
}

impl LoadAverage {
    pub fn sample(&mut self, runnable: u32) {
        for (value, alpha) in self.value.iter_mut().zip([60296u64, 64453, 65173]) {
            let input = u64::from(runnable) << 16;
            *value = ((u64::from(*value) * alpha + input * (65536 - alpha) + 32768) >> 16) as u32;
        }
    }

    pub fn values(&self) -> [u32; 3] {
        self.value
    }
}

/// Operating point selection policy for HP0 DVFS (`40 MHz` XTAL <-> `180 MHz` CPLL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum FreqMode {
    /// DVFS idle governor: downshift to 40 MHz XTAL during tickless/prepared sleep
    /// when no high-bus-rate peripheral (`display` / `dma`) is active, and restore
    /// the previously active operating point (or 180 MHz on display start) on wake.
    Auto = 0,
    /// Pin 40 MHz XTAL (rejected with [`Errno::Ebusy`] while display or DMA is active).
    Pin40Mhz = 1,
    /// Pin 180 MHz CPLL even during tickless idle.
    Pin180Mhz = 2,
}

impl FreqMode {
    pub const fn from_abi(raw: u32) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Auto),
            1 => Ok(Self::Pin40Mhz),
            2 => Ok(Self::Pin180Mhz),
            _ => Err(Errno::Einval),
        }
    }

    pub const fn to_abi(self) -> u32 {
        self as u32
    }
}

/// Compute the next SYSTIMER tickless sleep interval in 10 ms ticks (`1..=max_ticks`).
///
/// Returns `1` (standard 100 Hz periodic tick) whenever `display_active` or `dma_active`
/// requires 10 ms periodic servicing, or if the earliest deadline is already due.
pub fn compute_tickless_ticks(
    now: Ticks,
    thread_deadline: Option<Ticks>,
    sentinel_deadline: Option<Ticks>,
    load_sample_deadline: Ticks,
    display_active: bool,
    dma_active: bool,
    max_ticks: u32,
) -> u32 {
    if display_active || dma_active || max_ticks <= 1 {
        return 1;
    }
    let mut earliest = load_sample_deadline;
    if let Some(d) = thread_deadline
        && d < earliest
    {
        earliest = d;
    }
    if let Some(d) = sentinel_deadline
        && d < earliest
    {
        earliest = d;
    }
    let delta = earliest.saturating_sub(now);
    delta.clamp(1, u64::from(max_ticks)) as u32
}

/// Measured per-hart cycle breakdown, timer IRQ entry latency, and DVFS/tickless telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CycleSnapshot {
    pub user_cycles: u64,
    pub kernel_cycles: u64,
    pub idle_cycles: u64,
    pub capacity_cycles: u64,
    pub irq_latency_last_16m: u32,
    pub irq_latency_max_16m: u32,
    pub freq_switches: u32,
    pub tickless_entries: u32,
    pub tickless_ticks: u64,
}

impl CycleSnapshot {
    pub const fn busy_cycles(&self) -> u64 {
        self.user_cycles.saturating_add(self.kernel_cycles)
    }

    /// Frequency-aware busy utilization in basis points (`0..=10_000`, i.e. `0.00%..=100.00%`).
    pub const fn utilization_bps(&self) -> u32 {
        let busy = self.busy_cycles();
        let denom = if self.capacity_cycles > busy {
            self.capacity_cycles
        } else {
            busy
        };
        if denom == 0 {
            return 0;
        }
        if busy <= (u64::MAX - denom / 2) / 10_000 {
            ((busy * 10_000 + denom / 2) / denom) as u32
        } else {
            ((busy as u128 * 10_000 + (denom / 2) as u128) / denom as u128) as u32
        }
    }
}

/// Frequency-aware `mcycle` and 16 MHz timer capacity/latency/DVFS/tickless tracker.
///
/// Integrates available CPU cycles across clock switches (`40 MHz` XTAL <-> `180 MHz`
/// CPLL) with exact mod-16 remainder retention so utilization never drifts even if
/// `wfi` clock-gates `mcycle`.
pub struct CycleAccounting {
    last_mcycle: u64,
    last_counter_16m: u64,
    expected_irq_16m: Option<u64>,
    cpu_mhz: u32,
    user_cycles: u64,
    kernel_cycles: u64,
    idle_cycles: u64,
    capacity_rem_16: u64,
    capacity_cycles: u64,
    irq_latency_last_16m: u32,
    irq_latency_max_16m: u32,
    irq_delays: [u32; 64],
    irq_sequence: u32,
    freq_switches: u32,
    tickless_entries: u32,
    tickless_ticks: u64,
}

impl CycleAccounting {
    pub const fn new() -> Self {
        Self {
            last_mcycle: 0,
            last_counter_16m: 0,
            expected_irq_16m: None,
            cpu_mhz: 40,
            user_cycles: 0,
            kernel_cycles: 0,
            idle_cycles: 0,
            capacity_rem_16: 0,
            capacity_cycles: 0,
            irq_latency_last_16m: 0,
            irq_latency_max_16m: 0,
            irq_delays: [0; 64],
            irq_sequence: 0,
            freq_switches: 0,
            tickless_entries: 0,
            tickless_ticks: 0,
        }
    }

    /// Read a bounded interval without resetting global counters. Delays retain
    /// full comparator lateness, including missed timer periods.
    pub fn irq_window(&self, cursor: Option<u32>) -> Option<(u32, u32, u32)> {
        let count = cursor.map_or(0, |seq| self.irq_sequence.wrapping_sub(seq));
        if count > 64 {
            return None;
        }
        let mut max = 0;
        for i in 0..count {
            let seq = self.irq_sequence.wrapping_sub(i + 1);
            max = max.max(self.irq_delays[seq as usize % 64]);
        }
        Some((self.irq_sequence, count, max))
    }

    pub const fn cpu_mhz(&self) -> u32 {
        self.cpu_mhz
    }

    pub fn start(&mut self, mcycle: u64, counter_16m: u64, cpu_mhz: u32) {
        self.last_mcycle = mcycle;
        self.last_counter_16m = counter_16m;
        self.expected_irq_16m = None;
        self.cpu_mhz = cpu_mhz;
    }

    /// Record the exact 16 MHz comparator target (`now_16m + period_16m`) when
    /// reprogramming `SYSTIMER` target 0 for tickless idle or restoring 10 ms periodic mode.
    pub fn set_expected_irq_16m(&mut self, expected_16m: Option<u64>) {
        self.expected_irq_16m = expected_16m;
    }

    /// Record a tickless idle entry programmed for `planned_ticks` (`> 1`).
    pub fn record_tickless_entry(&mut self) {
        self.tickless_entries = self.tickless_entries.saturating_add(1);
    }

    /// Record elapsed 10 ms ticks spent in a tickless sleep window upon wakeup.
    pub fn record_tickless_wake(&mut self, elapsed_ticks: u64) {
        self.tickless_ticks = self.tickless_ticks.saturating_add(elapsed_ticks);
    }

    fn advance_capacity(&mut self, counter_16m: u64) {
        let delta_16m = counter_16m.saturating_sub(self.last_counter_16m);
        self.last_counter_16m = counter_16m;
        let numer = delta_16m
            .saturating_mul(u64::from(self.cpu_mhz))
            .saturating_add(self.capacity_rem_16);
        self.capacity_cycles = self.capacity_cycles.saturating_add(numer / 16);
        self.capacity_rem_16 = numer % 16;
    }

    /// Record cycles elapsed before entering a trap; returns U-mode cycles to
    /// attribute to the interrupted user thread (`0` when waking from `wfi`).
    pub fn on_trap_entry(&mut self, mcycle: u64, from_idle: bool) -> u64 {
        let delta = mcycle.wrapping_sub(self.last_mcycle);
        self.last_mcycle = mcycle;
        if from_idle {
            self.idle_cycles = self.idle_cycles.saturating_add(delta);
            0
        } else {
            self.user_cycles = self.user_cycles.saturating_add(delta);
            delta
        }
    }

    /// Record M-mode cycles spent inside `rust_trap` right before returning to `vela_resume`.
    pub fn on_trap_exit(&mut self, mcycle: u64) {
        let delta = mcycle.wrapping_sub(self.last_mcycle);
        self.last_mcycle = mcycle;
        self.kernel_cycles = self.kernel_cycles.saturating_add(delta);
    }

    /// Update wall-clock CPU capacity and (after the initial post-boot tick) record
    /// comparator-to-ISR latency in 16 MHz ticks (`1 tick = 62.5 ns`).
    pub fn on_timer_irq(&mut self, counter_16m: u64, period_16m: u64, measure_latency: bool) {
        self.advance_capacity(counter_16m);
        let quantum_16m = if period_16m == 0 {
            BASE_PERIOD_16M
        } else {
            period_16m.min(BASE_PERIOD_16M)
        };
        if measure_latency && period_16m != 0 {
            let lat64 = match self.expected_irq_16m {
                Some(expected) => {
                    if counter_16m >= expected {
                        let delta = counter_16m - expected;
                        let steps = (delta / period_16m) + 1;
                        self.expected_irq_16m = Some(expected.saturating_add(steps * period_16m));
                        delta
                    } else {
                        counter_16m % quantum_16m
                    }
                }
                None => counter_16m % quantum_16m,
            };
            let lat = lat64.min(u64::from(u32::MAX)) as u32;
            self.irq_delays[self.irq_sequence as usize % 64] = lat;
            self.irq_sequence = self.irq_sequence.wrapping_add(1);
            let lat = lat % quantum_16m as u32;
            self.irq_latency_last_16m = lat;
            if lat > self.irq_latency_max_16m {
                self.irq_latency_max_16m = lat;
            }
        } else if let Some(expected) = self.expected_irq_16m
            && period_16m != 0
            && counter_16m >= expected
        {
            let steps = ((counter_16m - expected) / period_16m) + 1;
            self.expected_irq_16m = Some(expected.saturating_add(steps * period_16m));
        }
    }

    /// Flush capacity accumulated under the previous frequency and switch to `new_cpu_mhz`.
    pub fn sync_clock(&mut self, counter_16m: u64, new_cpu_mhz: u32) {
        self.advance_capacity(counter_16m);
        if self.cpu_mhz != new_cpu_mhz {
            self.cpu_mhz = new_cpu_mhz;
            self.freq_switches = self.freq_switches.saturating_add(1);
        }
    }

    /// Flush in-progress trap cycles and capacity up to `now` and return an atomic snapshot.
    pub fn snapshot(&mut self, mcycle: u64, counter_16m: u64) -> CycleSnapshot {
        self.on_trap_exit(mcycle);
        self.snapshot_remote(counter_16m)
    }

    /// Flush wall-clock capacity up to `counter_16m` for a remote hart without reading
    /// that hart's local `mcycle` CSR.
    pub fn snapshot_remote(&mut self, counter_16m: u64) -> CycleSnapshot {
        self.advance_capacity(counter_16m);
        CycleSnapshot {
            user_cycles: self.user_cycles,
            kernel_cycles: self.kernel_cycles,
            idle_cycles: self.idle_cycles,
            capacity_cycles: self.capacity_cycles,
            irq_latency_last_16m: self.irq_latency_last_16m,
            irq_latency_max_16m: self.irq_latency_max_16m,
            freq_switches: self.freq_switches,
            tickless_entries: self.tickless_entries,
            tickless_ticks: self.tickless_ticks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irq_windows_retain_missed_periods_and_reject_overwritten_samples() {
        let mut acc = CycleAccounting::new();
        let (cursor, _, _) = acc.irq_window(None).unwrap();
        acc.set_expected_irq_16m(Some(100));
        acc.on_timer_irq(100 + 3 * BASE_PERIOD_16M + 17, BASE_PERIOD_16M, true);
        assert_eq!(
            acc.irq_window(Some(cursor)),
            Some((1, 1, (3 * BASE_PERIOD_16M + 17) as u32))
        );
        assert_eq!(acc.irq_latency_last_16m, 17);
        assert_eq!(acc.irq_window(Some(1)), Some((1, 0, 0)));
        for i in 0..64 {
            acc.on_timer_irq(100 + (i + 4) * BASE_PERIOD_16M, BASE_PERIOD_16M, true);
        }
        assert!(acc.irq_window(Some(cursor)).is_none());
        assert!(acc.irq_window(Some(1)).is_some());
        acc.irq_sequence = u32::MAX;
        acc.on_timer_irq(100 + 69 * BASE_PERIOD_16M, BASE_PERIOD_16M, true);
        assert_eq!(acc.irq_window(Some(u32::MAX)).unwrap().1, 1);
    }

    #[test]
    fn utilization_remains_accurate_after_long_uptime() {
        let mut sample = CycleSnapshot {
            capacity_cycles: u64::MAX,
            user_cycles: u64::MAX / 2,
            ..Default::default()
        };
        assert_eq!(sample.utilization_bps(), 5000);
        sample.user_cycles = u64::MAX;
        assert_eq!(sample.utilization_bps(), 10000);
    }

    #[test]
    fn cycle_accounting_splits_user_kernel_idle_and_tracks_irq_latency_watermarks() {
        let mut acc = CycleAccounting::new();
        acc.start(1_000, 0, 40);

        // User runs 500 cycles -> trap takes 200 cycles -> enters idle.
        assert_eq!(acc.on_trap_entry(1_500, false), 500);
        // Boot tick (measure_latency = false) is ignored for latency watermarks.
        acc.on_timer_irq(160_090, 160_000, false);
        acc.on_trap_exit(1_700);
        assert_eq!(acc.irq_latency_max_16m, 0);

        // Idle runs 300 cycles -> live timer IRQ arrives 7 ticks after 320_000.
        assert_eq!(acc.on_trap_entry(2_000, true), 0);
        acc.on_timer_irq(320_007, 160_000, true);
        acc.on_trap_exit(2_150);

        // Another live timer IRQ arrives 19 ticks after 480_000, followed by 5 ticks after 640_000.
        assert_eq!(acc.on_trap_entry(2_500, true), 0);
        acc.on_timer_irq(480_019, 160_000, true);
        acc.on_trap_exit(2_600);

        assert_eq!(acc.on_trap_entry(3_000, false), 400);
        acc.on_timer_irq(640_005, 160_000, true);
        let snap = acc.snapshot(3_100, 640_016);

        assert_eq!(snap.user_cycles, 900);
        assert_eq!(snap.kernel_cycles, 200 + 150 + 100 + 100);
        assert_eq!(snap.idle_cycles, 300 + 350);
        assert_eq!(snap.irq_latency_last_16m, 5);
        assert_eq!(snap.irq_latency_max_16m, 19);
        assert_eq!(snap.capacity_cycles, 640_016 * 40 / 16);
    }

    #[test]
    fn frequency_transition_integrates_exact_capacity_without_fractional_drift() {
        let mut acc = CycleAccounting::new();
        acc.start(0, 100, 40);

        // Two odd 1-tick steps at 40 MHz (2.5 cycles/tick -> 2 + 3 = 5 cycles exact).
        acc.sync_clock(101, 40);
        acc.sync_clock(102, 40);
        assert_eq!(acc.capacity_cycles, 5);
        assert_eq!(acc.freq_switches, 0);

        // Run 98 more ticks at 40 MHz (total 100 ticks * 2.5 = 250 cycles), then switch to 180 MHz.
        acc.sync_clock(200, 180);
        assert_eq!(acc.capacity_cycles, 250);
        assert_eq!(acc.freq_switches, 1);

        // Run 4 ticks at 180 MHz (4 * 11.25 = 45 cycles -> total 295 cycles).
        acc.on_trap_entry(147, false);
        let snap = acc.snapshot(295, 204);
        assert_eq!(snap.capacity_cycles, 295);
        assert_eq!(snap.busy_cycles(), 295);
        assert_eq!(snap.utilization_bps(), 10_000);
        assert_eq!(snap.freq_switches, 1);
    }

    #[test]
    fn tickless_policy_and_expected_irq_latency_tracking() {
        assert_eq!(FreqMode::from_abi(0), Ok(FreqMode::Auto));
        assert_eq!(FreqMode::from_abi(1), Ok(FreqMode::Pin40Mhz));
        assert_eq!(FreqMode::from_abi(2), Ok(FreqMode::Pin180Mhz));
        assert_eq!(FreqMode::from_abi(3), Err(Errno::Einval));

        // Active display or DMA forces 1-tick periodic mode.
        assert_eq!(
            compute_tickless_ticks(
                Ticks(100),
                Some(Ticks(250)),
                Some(Ticks(200)),
                Ticks(500),
                true,
                false,
                MAX_TICKLESS_TICKS,
            ),
            1
        );
        assert_eq!(
            compute_tickless_ticks(
                Ticks(100),
                Some(Ticks(250)),
                Some(Ticks(200)),
                Ticks(500),
                false,
                true,
                MAX_TICKLESS_TICKS,
            ),
            1
        );

        // Otherwise picks the minimum of thread deadline, sentinel deadline, load sample, and MAX_TICKLESS_TICKS.
        assert_eq!(
            compute_tickless_ticks(
                Ticks(100),
                Some(Ticks(185)),
                Some(Ticks(200)),
                Ticks(500),
                false,
                false,
                MAX_TICKLESS_TICKS,
            ),
            85
        );
        assert_eq!(
            compute_tickless_ticks(
                Ticks(100),
                None,
                None,
                Ticks(1_000),
                false,
                false,
                MAX_TICKLESS_TICKS,
            ),
            MAX_TICKLESS_TICKS
        );

        // Verify expected_irq_16m tracks tickless comparator re-arming without phase drift.
        let mut acc = CycleAccounting::new();
        acc.start(0, 123_456, 40);
        acc.record_tickless_entry();
        let target_16m = 123_456 + 50 * 160_000;
        acc.set_expected_irq_16m(Some(target_16m));
        acc.on_timer_irq(target_16m + 14, 50 * 160_000, true);
        acc.record_tickless_wake(50);

        let snap = acc.snapshot(100, target_16m + 20);
        assert_eq!(snap.irq_latency_last_16m, 14);
        assert_eq!(snap.irq_latency_max_16m, 14);
        assert_eq!(snap.tickless_entries, 1);
        assert_eq!(snap.tickless_ticks, 50);
    }
}
