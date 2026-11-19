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

    /// Exhaustive on purpose: adding a variant without pinning its ABI value
    /// here is a compile error.
    fn linux_value(e: Errno) -> i32 {
        match e {
            Errno::Eperm => 1,
            Errno::Enoent => 2,
            Errno::Eio => 5,
            Errno::Enoexec => 8,
            Errno::Ebadf => 9,
            Errno::Eagain => 11,
            Errno::Enomem => 12,
            Errno::Efault => 14,
            Errno::Ebusy => 16,
            Errno::Einval => 22,
            Errno::Enospc => 28,
            Errno::Epipe => 32,
            Errno::Edeadlk => 35,
            Errno::Enosys => 38,
            Errno::Eoverflow => 75,
            Errno::Emsgsize => 90,
            Errno::Etimedout => 110,
            Errno::Ealready => 114,
            Errno::Ecanceled => 125,
        }
    }

    const ALL: [Errno; 19] = [
        Errno::Eperm,
        Errno::Enoent,
        Errno::Eio,
        Errno::Enoexec,
        Errno::Ebadf,
        Errno::Eagain,
        Errno::Enomem,
        Errno::Efault,
        Errno::Ebusy,
        Errno::Einval,
        Errno::Enospc,
        Errno::Epipe,
        Errno::Edeadlk,
        Errno::Enosys,
        Errno::Eoverflow,
        Errno::Emsgsize,
        Errno::Etimedout,
        Errno::Ealready,
        Errno::Ecanceled,
    ];

    #[test]
    fn abi_values_match_linux_errno_and_are_distinct_negatives() {
        for (i, e) in ALL.into_iter().enumerate() {
            let v = linux_value(e);
            assert_eq!(e as i32, v);
            assert_eq!(e.to_abi(), (-v) as u32);
            // Userspace treats any `(result as i32) < 0` as an error.
            assert!((e.to_abi() as i32) < 0, "{e:?} must decode as negative");
            for other in &ALL[..i] {
                assert_ne!(e.to_abi(), other.to_abi());
            }
        }
    }
}
