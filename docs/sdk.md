# Application SDK

`vela-sdk` is the public application crate in `src/sdk`. Its API uses ordinary
Rust slices and explicit capabilities. Applications do not depend on the kernel
crate, platform registers or the service startup ABI. The SDK is `no_std` and
currently supports static PIE RV32IMAC executables, without libc or a global heap.
Its implementation reuses `vela-user` for IPC, allocation and syscall transport.
Driver libraries stay behind that implementation dependency and are not re-exported.

## Build the independent examples

From the repository root, using the pinned Rust toolchain and installed
`riscv32imac-unknown-none-elf` target:

```sh
./scripts/build-app.sh
./scripts/build-app.sh examples/upper/Cargo.toml files
```

Outputs are `target/apps/vela-upper.elf` and `target/apps/files.elf`. The examples
have their own Cargo workspace and lockfile. Their only direct dependency is
`vela-sdk`. `scripts/check.sh` checks their formatting and builds both examples,
and runs SDK host tests alongside the existing kernel tests.

The general form is:

```sh
./scripts/build-app.sh /absolute/path/to/app/Cargo.toml binary-name
```

The script selects the user target, PIE relocation flags and the shared userspace
linker script (`src/user/link.x`), and rejects images over the 1 MiB runtime limit.
An external project must have its own release settings like `examples/upper`:
`panic = "abort"`, `opt-level = "s"`, `lto = true`, `codegen-units = 1`.
Use a path dependency pointing at this checkout's `src/sdk`, and commit that
application's Cargo.lock before using the locked build command. New projects can
generate it with `cargo generate-lockfile --manifest-path /path/to/Cargo.toml`.
The SDK and kernel ABI are developed together and are not yet independently
versioned for binary compatibility.

## Entry point

```rust,ignore
#![no_std]
#![no_main]

use vela_sdk::{App, Result, io};
vela_sdk::entry!(main);

fn main(app: App) -> Result<()> {
    io::write(b"Hello from Vela: ")?;
    io::write(app.argument())?;
    io::write(b"\n")
}
```

The entry macro obtains `App`, initializes stdin/stdout, runs `main`, prints a
returned error to stderr, closes piped stdout and exits. A panic also prints a
short diagnostic and exits. The process API currently has no numeric exit status
or unwind support. Kernel fault details remain available in `/proc/last_fault`.

`App::argument()` is one byte string, at most 80 bytes, with no implicit NUL and
no argv splitting. Empty arguments are supported. `App::files()` returns the
inherited VFS client. `App::extra_handle()` exposes an optional service capability
for explicit policies, with zero meaning absent. Application startup arguments
remain immutable above the initial user SP for the process lifetime.

## API

| API | Meaning |
|---|---|
| `io::read(&mut [u8])` | Read an input stream message, at most 96 bytes. Zero means EOF or no input. Use buffers of at least 96 bytes for full-size stream messages. |
| `io::write(&[u8])` | Write all bytes to stdout, splitting IPC messages and yielding on backpressure. Without an attached stdout endpoint, write to the console. |
| `io::ewrite(&[u8])` | Write a diagnostic to the console. Stderr is not independently redirectable yet. |
| `io::has_stdin()` | Whether a stream was attached. A plain implicit command usually has none. |
| `app.files().open(path)` | Verify a regular file and create a local path/cursor object. |
| `app.files().create(path)` | Create or truncate a file on a writable service. Parent directory must exist. |
| `File::read`, `write`, `seek`, `position` | Sequential bulk reads/writes and a local absolute byte cursor. |
| `FileSystem::metadata`, `create_dir`, `remove` | File size/type, directory creation, file or empty-directory removal. |
| `memory::Memory::new(bytes)` | Owned zeroed PSRAM with safe slice access and release on Drop. Hardware backing rounds up to 64 KiB. No global allocator is installed. |
| `process::Spawner::run(files, path, argument)` | Load an ELF, explicitly delegate VFS SEND and wait for the child. Requires a supplied spawn authority. |
| `process::sleep_ticks(n)` | Sleep for n system ticks, 10 ms per tick. |
| `process::exit()` | Close stdout and exit early. Returning from main does this automatically. |

Paths must be absolute, fit the bulk IPC path limit, and contain no NUL. Offset
arithmetic is checked before I/O. A File borrows its path and holds only a local
cursor. There is no remote descriptor to close, no stable inode identity across
rename, and no atomic POSIX open flags. Dropping a File does not send a close RPC.

Vela File IPC truncates an existing file whenever a write starts at offset zero.
`File::write` keeps that behavior, including after `seek(0)`. Write errors may
leave partial on-disk changes; do not retry them as if they were transactions.
Empty writes through File are no-ops. Use `create` to explicitly truncate to zero.
Underlying write helpers may print a service diagnostic before returning an error.

No authority is obtained from a PID or a global namespace handle. A VFS capability
limits what the file API can access. Normal shell-launched applications receive
VFS SEND, optional stream handles and usually no spawn authority. They therefore
cannot use `Spawner` successfully by default. An explicitly configured launcher
must supply EXECUTE authority and CONTROL permission for forwarding the inherited
VFS capability. The kernel enforces these rights. `Spawner::run` currently gives
its child no stdin and console stdout, and returns the child PID after completion,
not an exit code. General child stream wiring is a future SDK extension.

## Install and run on Tab5

In the Vela shell run `sdexport rw`, then identify the exported **Tab5 microSD**
on the host and mount its existing ext4 partition. Copy only the new examples:

```sh
# Adjust the mount path to the verified Tab5 partition.
cp target/apps/vela-upper.elf /media/kot/VELA/bin/upper.elf
cp target/apps/files.elf /media/kot/VELA/bin/sdk-files.elf
```

Unmount every partition and use the desktop's **Eject** action to send the SCSI
eject request. This releases the card back to Vela. USB power-off is different
and may require unplugging/reconnecting the cable. Do not leave the partition
mounted on Linux while Vela is accessing it.

Then run:

```text
upper hello vela
HELLO VELA

echo mixed Case | upper
MIXED CASE

upper /proc/last_fault
NONE

upper sdk redirect > /sd/vela-sdk-output.txt
cat /sd/vela-sdk-output.txt
SDK REDIRECT
rm /sd/vela-sdk-output.txt

sdk-files
SDK files and memory OK
```

`upper` demonstrates argument handling, an owned buffer, file reads, stream input
and redirected output. Only ASCII letters change case. `run /sd/bin/upper.elf`
passes terminal stdin instead of the implicit command's absent stdin; interactive
input continues until its input endpoint reaches EOF, so the piped form is more
convenient for a short test.

`sdk-files` demonstrates create, two sequential writes, EOF, seek, read, truncate,
metadata and removal using SDK methods. It refuses to overwrite an existing
`/sd/vela-sdk-example.txt` and cleans up its own file after a completed test.
The redirected output path above is also disposable. Choose a different unused
path if that name already exists on your card.

## Validation on Tab5, 2026-11-26

Both independent examples were installed as new microSD files and run on the
stock firmware. Literal input, piped input, `/proc` reads, redirected output and
the filesystem exercise above passed. A missing input file printed `file not
found` to the console while downstream `wc` received EOF and reported `0 0 0`.
The temporary files were removed, `/proc/last_fault` stayed `none`, and both HP
stack boundary markers remained intact. `Spawner::run` is not exercised by the
normal shell launch policy because it supplies no spawn authority.
