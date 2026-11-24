mod device;
mod ipc;
mod memory;
mod sentinel;
mod thread;

use vela_kernel::errno::Errno;

use crate::{arch, runtime::Runtime, syscall};

#[derive(Clone, Copy)]
pub struct Args {
    pub number: u32,
    pub a0: u32,
    pub a1: u32,
    pub a2: u32,
    pub a3: u32,
    pub a4: u32,
    pub a5: u32,
}

impl Args {
    fn from_frame(frame: &arch::TrapFrame) -> Self {
        Self {
            number: frame.registers[15],
            a0: frame.registers[8],
            a1: frame.registers[9],
            a2: frame.registers[10],
            a3: frame.registers[11],
            a4: frame.registers[12],
            a5: frame.registers[13],
        }
    }
}

impl Runtime {
    pub fn dispatch(&mut self, frame: &mut arch::TrapFrame, switch: &mut bool) -> u32 {
        let args = Args::from_frame(frame);
        let current = self.scheduler.current().process;
        let result = match args.number {
            syscall::SD_EXPORT => self.sys_sd_export(current, args),
            syscall::SD_PARTITION => self.sys_sd_partition(current, args),
            syscall::SPAWN_IMAGE | syscall::SPAWN_IMAGE_WAIT => {
                self.sys_spawn_image(current, args, switch)
            }
            syscall::WRITE => self.sys_write(args),
            syscall::YIELD => self.sys_yield(switch),
            syscall::EXIT => self.sys_exit(switch),
            syscall::THREAD_PRIORITY => self.sys_thread_priority(current, args, switch),
            syscall::THREAD_AFFINITY => self.sys_thread_affinity(current, args, frame, switch),
            syscall::IPC_CALL => self.sys_ipc_call(current, args, switch),
            syscall::IPC_REPLY => self.sys_ipc_reply(current, args, switch),
            syscall::IPC_REPLY_RECV => self.sys_ipc_reply_recv(current, args, frame, switch),
            syscall::CAP_GRANT | syscall::MEM_GRANT => self.sys_cap_or_mem_grant(current, args),
            syscall::CAP_REVOKE => self.sys_cap_revoke(current, args, switch),
            syscall::MEM_CAP => self.sys_mem_cap(current, args),
            syscall::MEM_ALLOC => self.sys_mem_alloc(current, args, frame),
            syscall::MEM_FREE => self.sys_mem_free(current, args),
            syscall::THREAD_CREATE => self.sys_thread_create(current, args),
            syscall::PROCESS_EXIT => self.sys_process_exit(switch),
            syscall::DISPLAY_TEST => self.sys_display_test(current, args),
            syscall::SENTINEL_CONTROL => self.sys_sentinel_control(current, args),
            syscall::SENTINEL_ARM_WAKE | syscall::SENTINEL_PREPARE_SLEEP => {
                self.sys_sentinel_arm_or_prepare(current, args, switch)
            }
            syscall::SENTINEL_WAIT_WAKE => {
                self.sys_sentinel_wait_wake(current, args, frame, switch)
            }
            syscall::SENTINEL_CANCEL_WAKE => self.sys_sentinel_cancel_wake(current, args, switch),
            syscall::SENTINEL_HELLO
            | syscall::SENTINEL_QUERY
            | syscall::SENTINEL_DIAG
            | syscall::SENTINEL_LOG => self.sys_sentinel_command(current, args, switch),
            syscall::SENTINEL_STATUS => self.sys_sentinel_status(current, args, frame),
            syscall::TICKS => self.sys_ticks(),
            syscall::STATS => self.sys_stats(args),
            syscall::ENDPOINT_CREATE => self.sys_endpoint_create(current),
            syscall::ENDPOINT_CLOSE => self.sys_endpoint_close(current, args),
            syscall::SEND => self.sys_send(current, args, switch),
            syscall::RECV => self.sys_recv(current, args, frame, switch),
            syscall::DMA_COPY => self.sys_dma_copy(current, args, switch),
            syscall::EVENT_WAIT => self.sys_event_wait(current, args, switch),
            syscall::IRQ_ACK => self.sys_irq_ack(current, args),
            syscall::IRQ_BIND => self.sys_irq_bind(current, args),
            syscall::IRQ_UNBIND => self.sys_irq_unbind(current, args, switch),
            syscall::I2C_TRANSFER => self.sys_i2c_transfer(current, args),
            syscall::CLOCK_CONTROL => self.sys_clock_control(current, args, frame),
            syscall::CONSOLE_READ => self.sys_console_read(current, args),
            syscall::CONSOLE_LOG => self.sys_console_log(current, args, frame),
            syscall::SLEEP => self.sys_sleep(args, switch),
            _ => Err(Errno::Enosys),
        };
        match result {
            Ok(value) => value,
            Err(errno) => errno.to_abi(),
        }
    }
}
