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
