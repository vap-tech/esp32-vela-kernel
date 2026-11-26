#![no_std]
//! Small application API. All access comes from explicitly supplied capabilities.

pub mod fs;
pub mod io;
pub mod memory;
pub mod process;

pub use vela_user::TextBuf;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidArgument,
    NoCapability,
    NotFound,
    IsDirectory,
    Overflow,
    Io,
    Kernel(u32),
}

impl Error {
    pub fn message(self) -> &'static [u8] {
        match self {
            Self::InvalidArgument => b"invalid argument",
            Self::NoCapability => b"required capability missing",
            Self::NotFound => b"file not found",
            Self::IsDirectory => b"expected a regular file",
            Self::Overflow => b"file offset overflow",
            Self::Io => b"I/O failed",
            Self::Kernel(_) => b"kernel operation failed",
        }
    }
}

/// Shell launch context. The argument is one bounded byte string, not POSIX argv.
pub struct App {
    argument: &'static [u8],
    files: fs::FileSystem,
    extra: u32,
}

impl App {
    pub fn argument(&self) -> &'static [u8] {
        self.argument
    }

    pub fn files(&self) -> fs::FileSystem {
        self.files
    }

    /// Optional explicitly inherited service capability, zero when absent.
    pub fn extra_handle(&self) -> u32 {
        self.extra
    }
}

/// Define the Vela entry point for `fn(App) -> vela_sdk::Result<()>`.
/// Returning closes stdout and exits. Diagnostics always go to the console.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        /// # Safety
        /// Called only with the kernel's validated SPAWN_IMAGE launch context.
        #[unsafe(link_section = ".text.entry")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn _start(
            fs: u32,
            arg: u32,
            len: u32,
            stdin: u32,
            stdout: u32,
            extra: u32,
        ) -> ! {
            unsafe { $crate::__run($main, fs, arg, len, stdin, stdout, extra) }
        }

        #[panic_handler]
        fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
            $crate::io::ewrite(b"application panicked\n");
            $crate::process::exit()
        }
    };
}

/// # Safety
/// `arg..arg+len` must be the kernel-owned launch argument, readable and immutable
/// for the process lifetime. The kernel reserves it above the initial user SP.
#[doc(hidden)]
pub unsafe fn __run(
    main: fn(App) -> Result<()>,
    fs: u32,
    arg: u32,
    len: u32,
    stdin: u32,
    stdout: u32,
    extra: u32,
) -> ! {
    io::init(stdin, stdout);
    let result = if len as usize > vela_abi::syscall::SPAWN_ARG_MAX || (len != 0 && arg == 0) {
        Err(Error::InvalidArgument)
    } else {
        let argument = if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) }
        };
        main(App {
            argument,
            files: fs::FileSystem::from_handle(fs),
            extra,
        })
    };
    if let Err(error) = result {
        io::ewrite(error.message());
        io::ewrite(b"\n");
    }
    process::exit()
}
