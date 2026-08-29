//! Translation between AFS semantics and Windows filesystem semantics.
//!
//! Everything in this module is pure and unit-testable. The actual WinFsp trait
//! implementation lives in `winfsp_glue.rs` and does nothing but call into here,
//! which keeps the semantics (the part that is easy to get subtly wrong) out of
//! the FFI layer (the part that is tedious but mechanical).

use std::sync::Arc;

use crate::client::{AfsError, Client};
use crate::proto::{self, errno, open_flag, Attr};

// ---------------------------------------------------------------- NTSTATUS

pub const STATUS_SUCCESS: i32 = 0x0000_0000u32 as i32;
pub const STATUS_INVALID_HANDLE: i32 = 0xC000_0008u32 as i32;
pub const STATUS_END_OF_FILE: i32 = 0xC000_0011u32 as i32;
pub const STATUS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
pub const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
pub const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
pub const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
pub const STATUS_DISK_FULL: i32 = 0xC000_007Fu32 as i32;
pub const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
pub const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BBu32 as i32;
pub const STATUS_UNEXPECTED_IO_ERROR: i32 = 0xC000_00E9u32 as i32;
pub const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101u32 as i32;
pub const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;
pub const STATUS_DEVICE_NOT_CONNECTED: i32 = 0xC000_009Du32 as i32;
pub const STATUS_IO_TIMEOUT: i32 = 0xC000_00B5u32 as i32;
pub const STATUS_IO_DEVICE_ERROR: i32 = 0xC000_0185u32 as i32;

/// Maps a server errno onto the NTSTATUS Windows expects.
///
/// Getting this right is what makes Explorer behave: `STATUS_OBJECT_NAME_NOT_FOUND`
/// on a probe is normal and silent, whereas a generic error code makes Explorer
/// pop a dialog. Copy dialogs in particular key off `STATUS_OBJECT_NAME_COLLISION`
/// to offer "replace or skip", and off `STATUS_DISK_FULL` to stop cleanly.
pub fn ntstatus_for_errno(code: i16) -> i32 {
    match code {
        errno::OK => STATUS_SUCCESS,
        errno::EPERM | errno::EACCES => STATUS_ACCESS_DENIED,
        errno::ENOENT => STATUS_OBJECT_NAME_NOT_FOUND,
        errno::EBADF => STATUS_INVALID_HANDLE,
        errno::EEXIST => STATUS_OBJECT_NAME_COLLISION,
        errno::ENOTDIR => STATUS_NOT_A_DIRECTORY,
        errno::EISDIR => STATUS_FILE_IS_A_DIRECTORY,
        errno::EINVAL => STATUS_INVALID_PARAMETER,
        errno::ENOSPC => STATUS_DISK_FULL,
        errno::ENOTEMPTY => STATUS_DIRECTORY_NOT_EMPTY,
        errno::ENOTSUP => STATUS_NOT_SUPPORTED,
        errno::EIO => STATUS_IO_DEVICE_ERROR,
        _ => STATUS_UNEXPECTED_IO_ERROR,
    }
}

pub fn ntstatus_for_error(e: &AfsError) -> i32 {
    match e {
        AfsError::Remote(c) => ntstatus_for_errno(*c),
        // A dropped USB cable should read as "the device went away", not as a
        // corrupt filesystem — Windows retries the former and gives up on the latter.
        AfsError::Disconnected => STATUS_DEVICE_NOT_CONNECTED,
        AfsError::Timeout => STATUS_IO_TIMEOUT,
        AfsError::Proto(_) => STATUS_UNEXPECTED_IO_ERROR,
    }
}

// ---------------------------------------------------------------- attributes

pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

pub fn file_attributes(attr: &Attr) -> u32 {
    let mut out = if attr.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    if attr.read_only() {
        out |= FILE_ATTRIBUTE_READONLY;
    }
    out
}

/// Unix epoch milliseconds to Windows FILETIME (100ns ticks since 1601-01-01).
///
/// Saturating rather than wrapping: Android occasionally reports 0 or a negative
/// mtime for files restored from backup, and a wrapped FILETIME shows up in
/// Explorer as a date in the year 30828, which looks like corruption.
pub fn unix_ms_to_filetime(ms: i64) -> u64 {
    const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000; // 1601-01-01 -> 1970-01-01
    let ticks = ms.saturating_mul(10_000).saturating_add(EPOCH_DIFF_100NS);
    if ticks < 0 {
        0
    } else {
        ticks as u64
    }
}

pub fn filetime_to_unix_ms(ft: u64) -> i64 {
    const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;
    (ft as i64).saturating_sub(EPOCH_DIFF_100NS) / 10_000
}

/// The subset of `FSP_FSCTL_FILE_INFO` we can actually populate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileInfo {
    pub file_attributes: u32,
    pub file_size: u64,
    pub allocation_size: u64,
    pub creation_time: u64,
    pub last_access_time: u64,
    pub last_write_time: u64,
    pub change_time: u64,
    pub index_number: u64,
}

/// Windows reports allocated size, and reporting less than the file size makes
/// some tools think the file is sparse. Round up to a 4 KiB cluster.
fn allocation_size(size: u64) -> u64 {
    const CLUSTER: u64 = 4096;
    size.div_ceil(CLUSTER) * CLUSTER
}

pub fn to_file_info(attr: &Attr) -> FileInfo {
    let mtime = unix_ms_to_filetime(attr.mtime_ms);
    FileInfo {
        file_attributes: file_attributes(attr),
        file_size: attr.size,
        allocation_size: allocation_size(attr.size),
        creation_time: unix_ms_to_filetime(attr.ctime_ms),
        last_access_time: unix_ms_to_filetime(attr.atime_ms),
        last_write_time: mtime,
        change_time: mtime,
        index_number: attr.ino,
    }
}

// ---------------------------------------------------------------- open flags

// Bits from the `granted_access` mask WinFsp hands to open/create.
pub const FILE_READ_DATA: u32 = 0x0001;
pub const FILE_WRITE_DATA: u32 = 0x0002;
pub const FILE_APPEND_DATA: u32 = 0x0004;
pub const GENERIC_READ_RIGHTS: u32 = 0x8000_0000;
pub const GENERIC_WRITE_RIGHTS: u32 = 0x4000_0000;
pub const GENERIC_ALL_RIGHTS: u32 = 0x1000_0000;

/// Bit in `create_options` marking the target as a directory.
pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;

/// Folds WinFsp's `granted_access` mask into AFS open flags.
///
/// Note there is no disposition parameter here. WinFsp splits what Win32
/// `CreateFile` expresses as a single disposition across three separate
/// callbacks: `open` for a file that already exists, `create` for one that does
/// not, and `overwrite` for truncation. So an `open` never creates and never
/// truncates. An earlier draft of this file folded a disposition in at this
/// layer, modelling an API that does not exist.
pub fn flags_for_open(granted_access: u32) -> u32 {
    let mut flags = 0u32;

    if granted_access & (FILE_READ_DATA | GENERIC_READ_RIGHTS | GENERIC_ALL_RIGHTS) != 0 {
        flags |= open_flag::READ;
    }
    if granted_access & (FILE_WRITE_DATA | GENERIC_WRITE_RIGHTS | GENERIC_ALL_RIGHTS) != 0 {
        flags |= open_flag::WRITE;
    }
    if granted_access & FILE_APPEND_DATA != 0 {
        flags |= open_flag::APPEND | open_flag::WRITE;
    }

    // A handle with neither bit set is legal in Win32 (a metadata-only open) but
    // meaningless to open(2); ask for read so the server has something to do.
    if flags & (open_flag::READ | open_flag::WRITE) == 0 {
        flags |= open_flag::READ;
    }
    flags
}

/// Same, for `create`: the file must come into existence and be writable.
pub fn flags_for_create(granted_access: u32) -> u32 {
    flags_for_open(granted_access) | open_flag::CREATE | open_flag::WRITE
}

// ---------------------------------------------------------------- filesystem

/// What a WinFsp file context holds.
pub struct FileHandle {
    pub handle: u64,
    pub rel: String,
    pub is_dir: bool,
}

/// The filesystem itself: stateless apart from the connection.
pub struct AfsFs {
    pub client: Arc<Client>,
}

impl AfsFs {
    pub fn new(client: Arc<Client>) -> Self {
        AfsFs { client }
    }

    pub fn getattr(&self, win_path: &str) -> Result<FileInfo, AfsError> {
        let rel = proto::to_rel(win_path);
        Ok(to_file_info(&self.client.getattr(&rel)?))
    }

    pub fn open(
        &self,
        win_path: &str,
        granted_access: u32,
    ) -> Result<(FileHandle, FileInfo), AfsError> {
        let rel = proto::to_rel(win_path);

        // Try the open directly rather than statting first to learn the kind.
        //
        // Directories cannot be opened with RandomAccessFile on the server, so
        // they need a handle-less context — but the server already tells us that
        // by returning EISDIR. Leading with a getattr would put a second USB round
        // trip in front of *every* file open, and files are the overwhelming
        // majority. This way the common case costs one round trip and only
        // directories pay for two.
        match self.client.open(&rel, flags_for_open(granted_access)) {
            Ok((handle, attr)) => Ok((
                FileHandle {
                    handle,
                    rel,
                    is_dir: false,
                },
                to_file_info(&attr),
            )),
            Err(AfsError::Remote(errno::EISDIR)) => {
                let attr = self.client.getattr(&rel)?;
                Ok((
                    FileHandle {
                        handle: 0,
                        rel,
                        is_dir: true,
                    },
                    to_file_info(&attr),
                ))
            }
            Err(e) => Err(e),
        }
    }

    pub fn create(
        &self,
        win_path: &str,
        granted_access: u32,
        is_dir: bool,
    ) -> Result<(FileHandle, FileInfo), AfsError> {
        let rel = proto::to_rel(win_path);

        if is_dir {
            let attr = self.client.mkdir(&rel, 0o755)?;
            return Ok((
                FileHandle {
                    handle: 0,
                    rel,
                    is_dir: true,
                },
                to_file_info(&attr),
            ));
        }

        let (handle, attr) = self
            .client
            .create(&rel, flags_for_create(granted_access), 0o644)?;
        Ok((
            FileHandle {
                handle,
                rel,
                is_dir: false,
            },
            to_file_info(&attr),
        ))
    }

    /// Truncates an already-open file to zero. WinFsp's `overwrite` callback.
    pub fn overwrite(&self, fh: &FileHandle) -> Result<FileInfo, AfsError> {
        if fh.is_dir {
            return Err(AfsError::Remote(errno::EISDIR));
        }
        self.client.truncate(fh.handle, 0)?;
        Ok(to_file_info(&self.client.getattr(&fh.rel)?))
    }

    /// Stat by path rather than by handle, for the callbacks that only have a name.
    pub fn getattr_rel(&self, rel: &str) -> Result<FileInfo, AfsError> {
        Ok(to_file_info(&self.client.getattr(rel)?))
    }

    pub fn close(&self, fh: &FileHandle) {
        if !fh.is_dir && fh.handle != 0 {
            // Nothing useful to do on failure: the context is going away regardless,
            // and the server drops handles when the connection ends.
            let _ = self.client.close(fh.handle);
        }
    }

    pub fn read(&self, fh: &FileHandle, offset: u64, len: u32) -> Result<Vec<u8>, AfsError> {
        if fh.is_dir {
            return Err(AfsError::Remote(errno::EISDIR));
        }
        self.client.read(fh.handle, offset, len)
    }

    pub fn write(&self, fh: &FileHandle, offset: u64, data: &[u8]) -> Result<u32, AfsError> {
        if fh.is_dir {
            return Err(AfsError::Remote(errno::EISDIR));
        }
        self.client.write(fh.handle, offset, data)
    }

    pub fn flush(&self, fh: &FileHandle) -> Result<(), AfsError> {
        if fh.is_dir || fh.handle == 0 {
            return Ok(());
        }
        self.client.fsync(fh.handle)
    }

    pub fn set_size(&self, fh: &FileHandle, size: u64) -> Result<(), AfsError> {
        if fh.is_dir {
            return Err(AfsError::Remote(errno::EISDIR));
        }
        self.client.truncate(fh.handle, size)
    }

    pub fn readdir(&self, fh: &FileHandle) -> Result<Vec<(String, FileInfo)>, AfsError> {
        let entries = self.client.readdir(&fh.rel)?;
        Ok(entries
            .into_iter()
            .map(|(name, attr)| (name, to_file_info(&attr)))
            .collect())
    }

    pub fn rename(&self, from_win: &str, to_win: &str, replace: bool) -> Result<(), AfsError> {
        let from = proto::to_rel(from_win);
        let to = proto::to_rel(to_win);
        self.client.rename(&from, &to, u32::from(replace))
    }

    pub fn unlink(&self, fh: &FileHandle) -> Result<(), AfsError> {
        if fh.is_dir {
            self.client.rmdir(&fh.rel)
        } else {
            self.client.unlink(&fh.rel)
        }
    }

    pub fn set_basic_info(&self, fh: &FileHandle, last_write_time: u64) -> Result<(), AfsError> {
        // 0 means "do not change" in the Windows convention; passing it through
        // would stamp every touched file with 1601-01-01.
        if last_write_time == 0 {
            return Ok(());
        }
        self.client.setattr(
            &fh.rel,
            proto::setattr_mask::MTIME,
            0,
            0,
            filetime_to_unix_ms(last_write_time),
        )?;
        Ok(())
    }

    pub fn volume_info(&self) -> Result<(u64, u64), AfsError> {
        let (total, _free, avail, _bsize) = self.client.statfs()?;
        Ok((total, avail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_mapping() {
        assert_eq!(ntstatus_for_errno(errno::OK), STATUS_SUCCESS);
        assert_eq!(
            ntstatus_for_errno(errno::ENOENT),
            STATUS_OBJECT_NAME_NOT_FOUND
        );
        assert_eq!(ntstatus_for_errno(errno::ENOSPC), STATUS_DISK_FULL);
        // Anything unrecognised must still be a failure code, never success.
        assert_ne!(ntstatus_for_errno(-12345), STATUS_SUCCESS);
    }

    #[test]
    fn filetime_roundtrip() {
        // 2021-01-01T00:00:00Z
        let ms = 1_609_459_200_000i64;
        assert_eq!(filetime_to_unix_ms(unix_ms_to_filetime(ms)), ms);
    }

    #[test]
    fn filetime_clamps_negative() {
        assert_eq!(unix_ms_to_filetime(i64::MIN), 0);
    }

    #[test]
    fn allocation_rounds_up() {
        assert_eq!(allocation_size(0), 0);
        assert_eq!(allocation_size(1), 4096);
        assert_eq!(allocation_size(4096), 4096);
        assert_eq!(allocation_size(4097), 8192);
    }

    #[test]
    fn open_never_creates_or_truncates() {
        // WinFsp routes creation and truncation to `create` and `overwrite`, so an
        // open must never carry those bits no matter what access was granted.
        for access in [
            FILE_READ_DATA,
            FILE_WRITE_DATA,
            GENERIC_READ_RIGHTS,
            GENERIC_WRITE_RIGHTS,
            GENERIC_ALL_RIGHTS,
            FILE_READ_DATA | FILE_WRITE_DATA,
        ] {
            let f = flags_for_open(access);
            assert_eq!(f & open_flag::CREATE, 0, "access {access:#x} set CREATE");
            assert_eq!(f & open_flag::TRUNC, 0, "access {access:#x} set TRUNC");
        }
    }

    #[test]
    fn access_maps_to_read_and_write() {
        assert_eq!(flags_for_open(FILE_READ_DATA) & open_flag::WRITE, 0);
        assert_eq!(
            flags_for_open(FILE_WRITE_DATA) & open_flag::WRITE,
            open_flag::WRITE
        );

        // GENERIC_ALL implies both.
        let f = flags_for_open(GENERIC_ALL_RIGHTS);
        assert_eq!(f & open_flag::READ, open_flag::READ);
        assert_eq!(f & open_flag::WRITE, open_flag::WRITE);

        // Append implies write, or the server would reject the writes.
        let f = flags_for_open(FILE_APPEND_DATA);
        assert_eq!(f & open_flag::APPEND, open_flag::APPEND);
        assert_eq!(f & open_flag::WRITE, open_flag::WRITE);
    }

    #[test]
    fn metadata_only_open_still_resolves() {
        // A zero access mask is legal in Win32 but meaningless to open(2).
        assert_ne!(flags_for_open(0) & (open_flag::READ | open_flag::WRITE), 0);
    }

    #[test]
    fn create_always_creates_and_writes() {
        let f = flags_for_create(FILE_READ_DATA);
        assert_eq!(f & open_flag::CREATE, open_flag::CREATE);
        assert_eq!(f & open_flag::WRITE, open_flag::WRITE);
    }

    #[test]
    fn directory_attribute_is_set() {
        let dir = Attr {
            kind: proto::KIND_DIR,
            mode: 0o755,
            size: 0,
            mtime_ms: 0,
            ctime_ms: 0,
            atime_ms: 0,
            ino: 7,
        };
        assert_eq!(
            file_attributes(&dir) & FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_DIRECTORY
        );

        let ro = Attr {
            kind: proto::KIND_FILE,
            mode: 0o444,
            size: 10,
            ..dir
        };
        assert_eq!(
            file_attributes(&ro) & FILE_ATTRIBUTE_READONLY,
            FILE_ATTRIBUTE_READONLY
        );
    }
}
