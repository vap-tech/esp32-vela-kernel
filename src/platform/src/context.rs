use vela_kernel::task::Context;

use crate::arch::TrapFrame;

// a0/a1 live at `registers[8]`/`[9]` (see `TrapFrame::user`).
impl Context for TrapFrame {
    fn set_return(&mut self, value: u32) {
        self.registers[8] = value;
    }
    fn set_return2(&mut self, value: u32) {
        self.registers[9] = value;
    }
}
