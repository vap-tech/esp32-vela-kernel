//! Canonical kernel error codes. Encoded to negative RV32 ABI register values
//! only at the syscall return boundary via `Errno::to_abi`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Errno {
    Eperm = 1,
    Enoent = 2,
    Eio = 5,
    Enoexec = 8,
    Ebadf = 9,
    Eagain = 11,
    Enomem = 12,
    Efault = 14,
    Ebusy = 16,
    Einval = 22,
    Enospc = 28,
    Epipe = 32,
    Edeadlk = 35,
    Enosys = 38,
    Eoverflow = 75,
    Emsgsize = 90,
    Etimedout = 110,
    Ealready = 114,
    Ecanceled = 125,
}

impl Errno {
    pub const fn to_abi(self) -> u32 {
        (-(self as i32)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::Errno;

    #[test]
    fn abi_encoding_is_two_complement_negative_errno() {
        assert_eq!(Errno::Eperm.to_abi(), (-1i32) as u32);
        assert_eq!(Errno::Ebadf.to_abi(), (-9i32) as u32);
        assert_eq!(Errno::Eagain.to_abi(), (-11i32) as u32);
        assert_eq!(Errno::Efault.to_abi(), (-14i32) as u32);
        assert_eq!(Errno::Einval.to_abi(), (-22i32) as u32);
        assert_eq!(Errno::Enosys.to_abi(), (-38i32) as u32);
        assert_eq!(Errno::Etimedout.to_abi(), (-110i32) as u32);
        assert_eq!(Errno::Ecanceled.to_abi(), (-125i32) as u32);
    }
}
