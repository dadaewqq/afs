//! Wire codec. Mirrors android/app/src/main/kotlin/dev/androidfiles/afs/proto/ —
//! change both together. Pure std on purpose: this module is testable without a
//! device and without WinFsp.

use std::io::{self, Read, Write};

pub const VERSION: u32 = 1;
pub const MAX_CHUNK: u32 = 1 << 20;
pub const FRAME_SLACK: u32 = 4096;
pub const HEADER_TAIL: usize = 8;
pub const ATTR_BYTES: usize = 45;

pub mod op {
    pub const INIT: u16 = 0x0001;
    pub const STATFS: u16 = 0x0002;

    pub const GETATTR: u16 = 0x0010;
    pub const READDIR: u16 = 0x0011;
    pub const OPEN: u16 = 0x0012;
    pub const CREATE: u16 = 0x0013;
    pub const CLOSE: u16 = 0x0014;
    pub const READ: u16 = 0x0015;
    pub const WRITE: u16 = 0x0016;
    pub const TRUNCATE: u16 = 0x0017;
    pub const FSYNC: u16 = 0x0018;

    pub const MKDIR: u16 = 0x0020;
    pub const RMDIR: u16 = 0x0021;
    pub const UNLINK: u16 = 0x0022;
    pub const RENAME: u16 = 0x0023;
    pub const SETATTR: u16 = 0x0024;

    pub const SCAN: u16 = 0x0030;
}

pub mod errno {
    pub const OK: i16 = 0;
    pub const EPERM: i16 = -1;
    pub const ENOENT: i16 = -2;
    pub const EIO: i16 = -5;
    pub const EBADF: i16 = -9;
    pub const EACCES: i16 = -13;
    pub const EEXIST: i16 = -17;
    pub const ENOTDIR: i16 = -20;
    pub const EISDIR: i16 = -21;
    pub const EINVAL: i16 = -22;
    pub const ENOSPC: i16 = -28;
    pub const ENOTEMPTY: i16 = -39;
    pub const ENOTSUP: i16 = -95;
}

pub mod open_flag {
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const APPEND: u32 = 4;
    pub const CREATE: u32 = 8;
    pub const EXCL: u32 = 16;
    pub const TRUNC: u32 = 32;
}

pub mod setattr_mask {
    pub const MODE: u32 = 1;
    pub const SIZE: u32 = 2;
    pub const MTIME: u32 = 4;
}

pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const KIND_SYMLINK: u8 = 2;
pub const KIND_OTHER: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub kind: u8,
    pub mode: u32,
    pub size: u64,
    pub mtime_ms: i64,
    pub ctime_ms: i64,
    pub atime_ms: i64,
    pub ino: u64,
}

impl Attr {
    pub fn is_dir(&self) -> bool {
        self.kind == KIND_DIR
    }

    pub fn read_only(&self) -> bool {
        // Owner write bit of the POSIX mode the server reported.
        self.mode & 0o200 == 0
    }
}

#[derive(Debug)]
pub enum ProtoError {
    Io(io::Error),
    Underrun { want: usize, have: usize },
    BadFrame(String),
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::Io(e) => write!(f, "io: {e}"),
            ProtoError::Underrun { want, have } => {
                write!(f, "payload underrun: want {want}, have {have}")
            }
            ProtoError::BadFrame(m) => write!(f, "bad frame: {m}"),
        }
    }
}

impl std::error::Error for ProtoError {}

impl From<io::Error> for ProtoError {
    fn from(e: io::Error) -> Self {
        ProtoError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, ProtoError>;

// ------------------------------------------------------------------ frames

pub struct Frame {
    pub tag: u32,
    pub op: u16,
    pub status: i16,
    pub payload: Vec<u8>,
}

pub fn write_frame<W: Write>(
    w: &mut W,
    tag: u32,
    op: u16,
    status: i16,
    payload: &[u8],
) -> Result<()> {
    let len = (HEADER_TAIL + payload.len()) as u32;
    let mut head = [0u8; 12];
    head[0..4].copy_from_slice(&len.to_le_bytes());
    head[4..8].copy_from_slice(&tag.to_le_bytes());
    head[8..10].copy_from_slice(&op.to_le_bytes());
    head[10..12].copy_from_slice(&status.to_le_bytes());
    w.write_all(&head)?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    w.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(r: &mut R, max_frame: u32) -> Result<Frame> {
    let mut head = [0u8; 4];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head);
    if (len as usize) < HEADER_TAIL || len > max_frame {
        return Err(ProtoError::BadFrame(format!(
            "length {len} out of range (max {max_frame})"
        )));
    }

    let mut rest = vec![0u8; len as usize];
    r.read_exact(&mut rest)?;

    let tag = u32::from_le_bytes(rest[0..4].try_into().unwrap());
    let op = u16::from_le_bytes(rest[4..6].try_into().unwrap());
    let status = i16::from_le_bytes(rest[6..8].try_into().unwrap());
    let payload = rest.split_off(HEADER_TAIL);

    Ok(Frame {
        tag,
        op,
        status,
        payload,
    })
}

// ------------------------------------------------------------------ reader

pub struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.b.len() {
            return Err(ProtoError::Underrun {
                want: n,
                have: self.b.len() - self.pos,
            });
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn str(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        let raw = self.take(n)?;
        String::from_utf8(raw.to_vec())
            .map_err(|e| ProtoError::BadFrame(format!("invalid utf-8: {e}")))
    }

    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    pub fn attr(&mut self) -> Result<Attr> {
        let b = self.take(ATTR_BYTES)?;
        Ok(Attr {
            kind: b[0],
            mode: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            size: u64::from_le_bytes(b[5..13].try_into().unwrap()),
            mtime_ms: i64::from_le_bytes(b[13..21].try_into().unwrap()),
            ctime_ms: i64::from_le_bytes(b[21..29].try_into().unwrap()),
            atime_ms: i64::from_le_bytes(b[29..37].try_into().unwrap()),
            ino: u64::from_le_bytes(b[37..45].try_into().unwrap()),
        })
    }
}

// ------------------------------------------------------------------ writer

#[derive(Default)]
pub struct Writer {
    b: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { b: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> Self {
        Writer {
            b: Vec::with_capacity(n),
        }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.b.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.b.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.b.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.b.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.b.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        let raw = s.as_bytes();
        self.u16(raw.len() as u16);
        self.b.extend_from_slice(raw);
        self
    }

    pub fn bytes(&mut self, raw: &[u8]) -> &mut Self {
        self.u32(raw.len() as u32);
        self.b.extend_from_slice(raw);
        self
    }

    pub fn finish(&self) -> Vec<u8> {
        self.b.clone()
    }

    pub fn take(self) -> Vec<u8> {
        self.b
    }
}

// ------------------------------------------------------------------ paths

/// Converts a Windows-style path from WinFsp (`\Pictures\a.jpg`) into the
/// protocol form (`Pictures/a.jpg`).
///
/// WinFsp hands paths with a leading backslash and the root as `\`. The server
/// refuses `.` and `..` outright, so we do not attempt to normalise them here —
/// passing them through unchanged means the rejection happens in exactly one
/// place instead of two implementations that could disagree.
pub fn to_rel(win_path: &str) -> String {
    win_path.replace('\\', "/").trim_matches('/').to_string()
}

/// Inverse of [`to_rel`], for building the paths WinFsp expects back.
pub fn to_win(rel: &str) -> String {
    if rel.is_empty() {
        "\\".to_string()
    } else {
        format!("\\{}", rel.replace('/', "\\"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 42, op::GETATTR, 0, b"hello").unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let f = read_frame(&mut cur, MAX_CHUNK + FRAME_SLACK).unwrap();
        assert_eq!(f.tag, 42);
        assert_eq!(f.op, op::GETATTR);
        assert_eq!(f.status, 0);
        assert_eq!(f.payload, b"hello");
    }

    #[test]
    fn payload_roundtrip() {
        let mut w = Writer::new();
        w.u32(7).str("Pictures/tromsø.jpg").i64(-5).bytes(&[1, 2, 3]);
        let raw = w.take();

        let mut r = Reader::new(&raw);
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.str().unwrap(), "Pictures/tromsø.jpg");
        assert_eq!(r.i64().unwrap(), -5);
        assert_eq!(r.bytes().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn underrun_is_detected() {
        let raw = [0u8; 2];
        let mut r = Reader::new(&raw);
        assert!(r.u32().is_err());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut cur = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cur, MAX_CHUNK + FRAME_SLACK).is_err());
    }

    #[test]
    fn path_conversion() {
        assert_eq!(to_rel("\\"), "");
        assert_eq!(to_rel("\\Pictures\\a.jpg"), "Pictures/a.jpg");
        assert_eq!(to_win(""), "\\");
        assert_eq!(to_win("Pictures/a.jpg"), "\\Pictures\\a.jpg");
    }
}
