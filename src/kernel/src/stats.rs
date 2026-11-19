/// Scheduler sampling contract, independent of trap/controller implementation.
/// 100 Hz scheduling; load sampled each 5 seconds. Fixed point Q16 EWMA of
/// Ready+Running tasks, alpha=exp(-5/tau), tau=60/300/900 seconds.
/// Rounded Q16 coefficients: 60296, 64453, 65173. No floating point/heap needed.
pub const HZ: u64 = 100;

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

/// Measured per-hart cycle breakdown and timer IRQ entry latency snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CycleSnapshot {
    pub user_cycles: u64,
    pub kernel_cycles: u64,
    pub idle_cycles: u64,
    pub capacity_cycles: u64,
    pub irq_latency_last_16m: u32,
    pub irq_latency_max_16m: u32,
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
        ((busy.saturating_mul(10_000) + denom / 2) / denom) as u32
    }
}

/// Frequency-aware `mcycle` and 16 MHz timer capacity/latency tracker.
///
/// Integrates available CPU cycles across clock switches (`40 MHz` XTAL -> `180 MHz`
/// CPLL) with exact mod-16 remainder retention so utilization never drifts even if
/// `wfi` clock-gates `mcycle`.
pub struct CycleAccounting {
    last_mcycle: u64,
    last_counter_16m: u64,
    cpu_mhz: u32,
    user_cycles: u64,
    kernel_cycles: u64,
    idle_cycles: u64,
    capacity_rem_16: u64,
    capacity_cycles: u64,
    irq_latency_last_16m: u32,
    irq_latency_max_16m: u32,
}

impl CycleAccounting {
    pub const fn new() -> Self {
        Self {
            last_mcycle: 0,
            last_counter_16m: 0,
            cpu_mhz: 40,
            user_cycles: 0,
            kernel_cycles: 0,
            idle_cycles: 0,
            capacity_rem_16: 0,
            capacity_cycles: 0,
            irq_latency_last_16m: 0,
            irq_latency_max_16m: 0,
        }
    }

    pub fn start(&mut self, mcycle: u64, counter_16m: u64, cpu_mhz: u32) {
        self.last_mcycle = mcycle;
        self.last_counter_16m = counter_16m;
        self.cpu_mhz = cpu_mhz;
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
        if measure_latency && period_16m != 0 {
            let lat = (counter_16m % period_16m).min(u64::from(u32::MAX)) as u32;
            self.irq_latency_last_16m = lat;
            if lat > self.irq_latency_max_16m {
                self.irq_latency_max_16m = lat;
            }
        }
    }

    /// Flush capacity accumulated under the previous frequency and switch to `new_cpu_mhz`.
    pub fn sync_clock(&mut self, counter_16m: u64, new_cpu_mhz: u32) {
        self.advance_capacity(counter_16m);
        self.cpu_mhz = new_cpu_mhz;
    }

    /// Flush in-progress trap cycles and capacity up to `now` and return an atomic snapshot.
    pub fn snapshot(&mut self, mcycle: u64, counter_16m: u64) -> CycleSnapshot {
        self.on_trap_exit(mcycle);
        self.advance_capacity(counter_16m);
        CycleSnapshot {
            user_cycles: self.user_cycles,
            kernel_cycles: self.kernel_cycles,
            idle_cycles: self.idle_cycles,
            capacity_cycles: self.capacity_cycles,
            irq_latency_last_16m: self.irq_latency_last_16m,
            irq_latency_max_16m: self.irq_latency_max_16m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Run 98 more ticks at 40 MHz (total 100 ticks * 2.5 = 250 cycles), then switch to 180 MHz.
        acc.sync_clock(200, 180);
        assert_eq!(acc.capacity_cycles, 250);

        // Run 4 ticks at 180 MHz (4 * 11.25 = 45 cycles -> total 295 cycles).
        acc.on_trap_entry(147, false);
        let snap = acc.snapshot(295, 204);
        assert_eq!(snap.capacity_cycles, 295);
        assert_eq!(snap.busy_cycles(), 295);
        assert_eq!(snap.utilization_bps(), 10_000);
    }
}
