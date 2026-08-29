//! Multiplexing client.
//!
//! One TCP connection, many concurrent requests. WinFsp calls into the filesystem
//! from a pool of threads, so the client must let those threads issue overlapping
//! requests and match the replies back by tag. Serialising here would waste the
//! server-side pipelining entirely and put us right back at MTP's throughput.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::proto::{self, errno, op, Attr, Reader, Writer};

/// How long a single request may wait. Generous, because a large WRITE on a slow
/// phone genuinely takes a while; short enough that a wedged device eventually
/// surfaces an error instead of hanging Explorer forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub enum AfsError {
    /// The server returned a negative errno.
    Remote(i16),
    /// The connection is gone.
    Disconnected,
    Timeout,
    Proto(proto::ProtoError),
}

impl std::fmt::Display for AfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AfsError::Remote(c) => write!(f, "server errno {c}"),
            AfsError::Disconnected => write!(f, "disconnected"),
            AfsError::Timeout => write!(f, "request timed out"),
            AfsError::Proto(e) => write!(f, "protocol: {e}"),
        }
    }
}

impl std::error::Error for AfsError {}

impl From<proto::ProtoError> for AfsError {
    fn from(e: proto::ProtoError) -> Self {
        AfsError::Proto(e)
    }
}

pub type Result<T> = std::result::Result<T, AfsError>;

struct Response {
    status: i16,
    payload: Vec<u8>,
}

struct Inner {
    writer: Mutex<BufWriter<TcpStream>>,
    pending: Mutex<HashMap<u32, SyncSender<Response>>>,
    next_tag: AtomicU32,
    dead: AtomicBool,
    max_chunk: u32,
}

impl Inner {
    fn fail_all(&self) {
        self.dead.store(true, Ordering::SeqCst);
        // Dropping the senders wakes every blocked caller with a disconnect,
        // instead of leaving them to hit the 120s timeout one by one.
        self.pending.lock().unwrap().clear();
    }
}

pub struct Client {
    inner: Arc<Inner>,
    pub volume_label: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl Client {
    /// Connects, performs the INIT handshake, and starts the reader thread.
    pub fn connect<A: ToSocketAddrs>(addr: A, token: &str) -> Result<Client> {
        let stream = TcpStream::connect(addr).map_err(|e| AfsError::Proto(e.into()))?;
        stream.set_nodelay(true).ok();

        let read_half = stream.try_clone().map_err(|e| AfsError::Proto(e.into()))?;
        let inner = Arc::new(Inner {
            writer: Mutex::new(BufWriter::with_capacity(64 * 1024, stream)),
            pending: Mutex::new(HashMap::new()),
            next_tag: AtomicU32::new(1),
            dead: AtomicBool::new(false),
            max_chunk: proto::MAX_CHUNK,
        });

        {
            let inner = Arc::clone(&inner);
            thread::Builder::new()
                .name("afs-reader".into())
                .spawn(move || reader_loop(read_half, inner))
                .map_err(|e| AfsError::Proto(e.into()))?;
        }

        let mut client = Client {
            inner,
            volume_label: String::new(),
            total_bytes: 0,
            free_bytes: 0,
        };

        let mut w = Writer::new();
        w.u32(proto::VERSION).str(token).u32(proto::MAX_CHUNK);
        let reply = client.call(op::INIT, &w.take())?;

        let mut r = Reader::new(&reply);
        let server_version = r.u32()?;
        let _negotiated_chunk = r.u32()?;
        client.volume_label = r.str()?;
        client.total_bytes = r.u64()?;
        client.free_bytes = r.u64()?;

        if server_version != proto::VERSION {
            return Err(AfsError::Proto(proto::ProtoError::BadFrame(format!(
                "server speaks v{server_version}, host speaks v{}",
                proto::VERSION
            ))));
        }

        Ok(client)
    }

    fn call(&self, opcode: u16, payload: &[u8]) -> Result<Vec<u8>> {
        if self.inner.dead.load(Ordering::SeqCst) {
            return Err(AfsError::Disconnected);
        }

        let tag = self.inner.next_tag.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = sync_channel::<Response>(1);
        self.inner.pending.lock().unwrap().insert(tag, tx);

        {
            let mut w = self.inner.writer.lock().unwrap();
            if let Err(e) = proto::write_frame(&mut *w, tag, opcode, 0, payload) {
                self.inner.pending.lock().unwrap().remove(&tag);
                self.inner.fail_all();
                return Err(AfsError::Proto(e));
            }
        }

        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(resp) => {
                if resp.status != errno::OK {
                    Err(AfsError::Remote(resp.status))
                } else {
                    Ok(resp.payload)
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                self.inner.pending.lock().unwrap().remove(&tag);
                Err(AfsError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(AfsError::Disconnected),
        }
    }

    pub fn max_chunk(&self) -> u32 {
        self.inner.max_chunk
    }

    pub fn is_alive(&self) -> bool {
        !self.inner.dead.load(Ordering::SeqCst)
    }

    // -------------------------------------------------------------- typed ops

    pub fn statfs(&self) -> Result<(u64, u64, u64, u32)> {
        let raw = self.call(op::STATFS, &[])?;
        let mut r = Reader::new(&raw);
        Ok((r.u64()?, r.u64()?, r.u64()?, r.u32()?))
    }

    pub fn getattr(&self, rel: &str) -> Result<Attr> {
        let mut w = Writer::new();
        w.str(rel);
        let raw = self.call(op::GETATTR, &w.take())?;
        Ok(Reader::new(&raw).attr()?)
    }

    /// Walks every page of a directory listing and returns the whole thing.
    ///
    /// WinFsp wants a complete buffer per enumeration anyway, so paging is an
    /// internal detail here — but the pages are what keep a 50,000-file directory
    /// from arriving as one enormous frame.
    pub fn readdir(&self, rel: &str) -> Result<Vec<(String, Attr)>> {
        let mut out = Vec::new();
        let mut cookie: u64 = 0;

        loop {
            let mut w = Writer::new();
            w.str(rel).u64(cookie).u32(512);
            let raw = self.call(op::READDIR, &w.take())?;

            let mut r = Reader::new(&raw);
            let eof = r.u8()? != 0;
            let next = r.u64()?;
            let count = r.u32()?;

            out.reserve(count as usize);
            for _ in 0..count {
                let name = r.str()?;
                let attr = r.attr()?;
                out.push((name, attr));
            }

            if eof {
                break;
            }
            if next == cookie {
                // Defensive: a server bug that failed to advance the cookie would
                // otherwise spin here forever holding a WinFsp thread.
                return Err(AfsError::Proto(proto::ProtoError::BadFrame(
                    "readdir cookie did not advance".into(),
                )));
            }
            cookie = next;
        }

        Ok(out)
    }

    pub fn open(&self, rel: &str, flags: u32) -> Result<(u64, Attr)> {
        let mut w = Writer::new();
        w.str(rel).u32(flags);
        let raw = self.call(op::OPEN, &w.take())?;
        let mut r = Reader::new(&raw);
        Ok((r.u64()?, r.attr()?))
    }

    pub fn create(&self, rel: &str, flags: u32, mode: u32) -> Result<(u64, Attr)> {
        let mut w = Writer::new();
        w.str(rel).u32(flags).u32(mode);
        let raw = self.call(op::CREATE, &w.take())?;
        let mut r = Reader::new(&raw);
        Ok((r.u64()?, r.attr()?))
    }

    pub fn close(&self, handle: u64) -> Result<()> {
        let mut w = Writer::new();
        w.u64(handle);
        self.call(op::CLOSE, &w.take())?;
        Ok(())
    }

    pub fn read(&self, handle: u64, offset: u64, len: u32) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        w.u64(handle).u64(offset).u32(len.min(self.max_chunk()));
        let raw = self.call(op::READ, &w.take())?;
        Ok(Reader::new(&raw).bytes()?)
    }

    pub fn write(&self, handle: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let mut w = Writer::with_capacity(data.len() + 32);
        w.u64(handle).u64(offset).bytes(data);
        let raw = self.call(op::WRITE, &w.take())?;
        Ok(Reader::new(&raw).u32()?)
    }

    pub fn truncate(&self, handle: u64, size: u64) -> Result<()> {
        let mut w = Writer::new();
        w.u64(handle).u64(size);
        self.call(op::TRUNCATE, &w.take())?;
        Ok(())
    }

    pub fn fsync(&self, handle: u64) -> Result<()> {
        let mut w = Writer::new();
        w.u64(handle);
        self.call(op::FSYNC, &w.take())?;
        Ok(())
    }

    pub fn mkdir(&self, rel: &str, mode: u32) -> Result<Attr> {
        let mut w = Writer::new();
        w.str(rel).u32(mode);
        let raw = self.call(op::MKDIR, &w.take())?;
        Ok(Reader::new(&raw).attr()?)
    }

    pub fn rmdir(&self, rel: &str) -> Result<()> {
        let mut w = Writer::new();
        w.str(rel);
        self.call(op::RMDIR, &w.take())?;
        Ok(())
    }

    pub fn unlink(&self, rel: &str) -> Result<()> {
        let mut w = Writer::new();
        w.str(rel);
        self.call(op::UNLINK, &w.take())?;
        Ok(())
    }

    pub fn rename(&self, from: &str, to: &str, flags: u32) -> Result<()> {
        let mut w = Writer::new();
        w.str(from).str(to).u32(flags);
        self.call(op::RENAME, &w.take())?;
        Ok(())
    }

    pub fn setattr(
        &self,
        rel: &str,
        mask: u32,
        mode: u32,
        size: u64,
        mtime_ms: i64,
    ) -> Result<Attr> {
        let mut w = Writer::new();
        w.str(rel).u32(mask).u32(mode).u64(size).i64(mtime_ms);
        let raw = self.call(op::SETATTR, &w.take())?;
        Ok(Reader::new(&raw).attr()?)
    }

    pub fn scan(&self, rel: &str) -> Result<()> {
        let mut w = Writer::new();
        w.str(rel);
        self.call(op::SCAN, &w.take())?;
        Ok(())
    }
}

fn reader_loop(stream: TcpStream, inner: Arc<Inner>) {
    let max_frame = proto::MAX_CHUNK + proto::FRAME_SLACK;
    let mut r = BufReader::with_capacity(64 * 1024, stream);

    loop {
        match proto::read_frame(&mut r, max_frame) {
            Ok(frame) => {
                let waiter = inner.pending.lock().unwrap().remove(&frame.tag);
                match waiter {
                    Some(tx) => {
                        // The caller may have already timed out and gone; a failed
                        // send just means nobody is listening, which is fine.
                        let _ = tx.send(Response {
                            status: frame.status,
                            payload: frame.payload,
                        });
                    }
                    None => {
                        log::debug!("response for unknown tag {}", frame.tag);
                    }
                }
            }
            Err(e) => {
                log::info!("reader loop ending: {e}");
                inner.fail_all();
                return;
            }
        }
    }
}
