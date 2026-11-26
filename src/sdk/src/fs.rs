//! Path-based File IPC clients. A File holds a borrowed path and local byte cursor.
//! There is no remote open descriptor or close RPC in the current filesystem ABI.
use crate::{Error, Result};
use vela_abi::fs as wire;
use vela_user::{Buffer, file};

#[derive(Clone, Copy)]
pub struct FileSystem {
    handle: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    pub size: u32,
    pub is_directory: bool,
}

fn status(value: u8) -> Error {
    if value == wire::STATUS_NOENT {
        Error::NotFound
    } else {
        Error::Io
    }
}

pub(crate) fn validate_path(path: &[u8]) -> Result<()> {
    if path.len() < 2
        || !path.starts_with(b"/")
        || path.len() > wire::MAX_BULK_PATH
        || path.contains(&0)
    {
        Err(Error::InvalidArgument)
    } else {
        Ok(())
    }
}

fn end_offset(offset: u32, length: usize) -> Result<u32> {
    offset
        .checked_add(u32::try_from(length).map_err(|_| Error::Overflow)?)
        .ok_or(Error::Overflow)
}

impl FileSystem {
    /// Wrap an explicitly inherited VFS SEND capability. Zero denotes no authority.
    pub const fn from_handle(handle: u32) -> Self {
        Self { handle }
    }

    pub(crate) fn handle(self) -> Result<u32> {
        if self.handle == 0 {
            Err(Error::NoCapability)
        } else {
            Ok(self.handle)
        }
    }

    pub fn metadata(self, path: &[u8]) -> Result<Metadata> {
        validate_path(path)?;
        let (size, kind) =
            file::fs_stat_raw(self.handle()?, path, &mut Buffer::new()).map_err(status)?;
        match kind {
            wire::KIND_FILE => Ok(Metadata {
                size,
                is_directory: false,
            }),
            wire::KIND_DIR => Ok(Metadata {
                size,
                is_directory: true,
            }),
            _ => Err(Error::Io),
        }
    }

    pub fn open(self, path: &[u8]) -> Result<File<'_>> {
        if self.metadata(path)?.is_directory {
            return Err(Error::IsDirectory);
        }
        Ok(File {
            filesystem: self,
            path,
            offset: 0,
        })
    }

    /// Create or truncate a regular file on a writable service, starting at offset 0.
    pub fn create(self, path: &[u8]) -> Result<File<'_>> {
        validate_path(path)?;
        file::fs_write_bulk(self.handle()?, path, 0, &[], &mut Buffer::new())
            .map_err(|_| Error::Io)?;
        Ok(File {
            filesystem: self,
            path,
            offset: 0,
        })
    }

    pub fn remove(self, path: &[u8]) -> Result<()> {
        validate_path(path)?;
        file::fs_unlink(self.handle()?, path, &mut Buffer::new()).map_err(status)
    }

    pub fn create_dir(self, path: &[u8]) -> Result<()> {
        validate_path(path)?;
        file::fs_mkdir(self.handle()?, path, &mut Buffer::new()).map_err(status)
    }
}

pub struct File<'a> {
    filesystem: FileSystem,
    path: &'a [u8],
    offset: u32,
}

impl File<'_> {
    pub fn position(&self) -> u32 {
        self.offset
    }

    /// Change the local cursor. Seeking alone neither validates EOF nor extends a file.
    pub fn seek(&mut self, offset: u32) {
        self.offset = offset;
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        end_offset(self.offset, buffer.len())?;
        let count = file::fs_read_bulk(
            self.filesystem.handle()?,
            self.path,
            self.offset,
            buffer,
            &mut Buffer::new(),
        )
        .map_err(status)?;
        self.offset = end_offset(self.offset, count)?;
        Ok(count)
    }

    /// Write all bytes and advance the cursor. The underlying ABI truncates when
    /// writing at offset zero. Failure can leave partial on-disk changes.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let end = end_offset(self.offset, bytes.len())?;
        if bytes.is_empty() {
            return Ok(());
        }
        let count = file::fs_write_bulk(
            self.filesystem.handle()?,
            self.path,
            self.offset,
            bytes,
            &mut Buffer::new(),
        )
        .map_err(|_| Error::Io)?;
        if count != bytes.len() {
            return Err(Error::Io);
        }
        self.offset = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_paths_that_would_be_truncated_on_the_wire() {
        for path in [&b"relative"[..], b"/", b"/bad\0name", &[b'/'; 100]] {
            assert_eq!(validate_path(path), Err(Error::InvalidArgument));
        }
        assert_eq!(validate_path(b"/sd/example.txt"), Ok(()));
    }

    #[test]
    fn offset_overflow_precedes_any_io() {
        let mut file = File {
            filesystem: FileSystem::from_handle(0),
            path: b"/sd/x",
            offset: u32::MAX,
        };
        assert_eq!(file.write(b"x"), Err(Error::Overflow));
        assert_eq!(file.read(&mut [0]), Err(Error::Overflow));
        assert_eq!(file.position(), u32::MAX);
    }

    #[test]
    fn missing_authority_and_failed_reads_preserve_cursor() {
        let fs = FileSystem::from_handle(0);
        assert_eq!(fs.metadata(b"/sd/x"), Err(Error::NoCapability));
        let mut file = File {
            filesystem: fs,
            path: b"/sd/x",
            offset: 17,
        };
        assert_eq!(file.read(&mut [0]), Err(Error::NoCapability));
        assert_eq!(file.position(), 17);
    }
}
