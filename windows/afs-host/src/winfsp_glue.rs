//! WinFsp binding layer.
//!
//! Written against winfsp 0.13.1. Everything here is thin: the semantics live in
//! [`crate::fs`], which is pure and unit-tested, and this module does nothing but
//! marshal arguments and convert errors.
//!
//! Two structural facts about WinFsp that shape the code below:
//!
//! - There is no disposition parameter. What Win32 `CreateFile` expresses as one
//!   disposition, WinFsp splits across `open` (existing file), `create` (new file)
//!   and `overwrite` (truncate). None of the three needs to interpret the others.
//! - Deletion is two-phase. `set_delete` only records intent; the file must be
//!   removed in `cleanup` when `FspCleanupDelete` is set. Deleting in `set_delete`
//!   corrupts the handle state the driver is still tracking.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo as FspFileInfo, FileSecurity, FileSystemContext,
    OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
use winfsp::{FspError, U16CStr};

use crate::client::{AfsError, Client};
use crate::fs::{self, AfsFs, FileInfo};
use crate::stats::{OpTimer, Stats};

/// Options for a mount.
pub struct MountOptions {
    pub mount_point: String,
    /// How long Windows may cache metadata and directory listings, in ms.
    ///
    /// The highest-leverage number in this file. Applications stat the same files
    /// relentlessly — Explorer while painting a window, Photos re-enumerating a
    /// folder on every next/previous — and each uncached request is a USB round
    /// trip plus a stat on the phone.
    ///
    /// `u32::MAX` hands everything to the WinFsp cache manager permanently, which
    /// is wrong here because the phone writes to this volume too. The default
    /// trades a few seconds of staleness for not re-reading a folder that has not
    /// changed.
    pub cache_ms: u32,
}

fn to_fsp(e: AfsError) -> FspError {
    FspError::NTSTATUS(fs::ntstatus_for_error(&e))
}

/// Writes an entry name into a `DirInfo` **without** a trailing NUL.
///
/// This is not a style preference — `DirInfo::set_name` causes an infinite
/// directory-enumeration loop. It appends a NUL and counts it in the entry's
/// `Size`, and WinFsp derives the stored filename's length from `Size` alone
/// (`FileNameBuf[]` is a flexible array member with no implied terminator). So the
/// entry lands in the directory buffer as `"name\0"`, while the marker the FSD
/// sends back to resume the next page is the plain NUL-terminated `"name"`.
///
/// `"name" < "name\0"`, so the buffer's binary search for the marker resolves to a
/// position *before* that entry and hands it back again — and the FSD derives the
/// same marker from it, forever. The visible symptom is the last entry that fit in
/// the first response buffer repeating without end.
///
/// WinFsp's own memfs computes `Size` as header + `FileName.length() * sizeof(WCHAR)`
/// with no NUL, which is what this reproduces. `DirInfo::reset()` has already zeroed
/// the name buffer, so the string remains NUL-terminated in memory regardless.
fn set_entry_name<const N: usize>(dir_info: &mut DirInfo<N>, name: &str) -> winfsp::Result<()> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    dir_info.set_name_raw(wide.as_slice())
}

fn to_fsp_info(i: &FileInfo) -> FspFileInfo {
    FspFileInfo {
        file_attributes: i.file_attributes,
        reparse_tag: 0,
        allocation_size: i.allocation_size,
        file_size: i.file_size,
        creation_time: i.creation_time,
        last_access_time: i.last_access_time,
        last_write_time: i.last_write_time,
        change_time: i.change_time,
        index_number: i.index_number,
        hard_links: 0,
        ea_size: 0,
    }
}

/// The relative path of a directory's parent. `""` is the root's parent, itself.
fn parent_of(rel: &str) -> String {
    match rel.rsplit_once('/') {
        Some((head, _)) => head.to_string(),
        None => String::new(),
    }
}

type DirEntries = Arc<Vec<(String, FspFileInfo)>>;

/// Directories to keep listings for. Small on purpose — this exists to make
/// repeated visits to the *same* folder cheap, not to mirror the device.
const MAX_CACHED_DIRS: usize = 32;

/// Attributes are cheap to keep and expensive to fetch, so keep a lot of them.
/// A 2,000-file folder is one directory; several open at once is normal.
const MAX_CACHED_ATTRS: usize = 20_000;

/// Per-file attributes, keyed by relative path.
///
/// Populated wholesale whenever a directory listing is fetched — the listing
/// already carries every child's attributes, so indexing them costs nothing and
/// turns the two hottest callbacks into pure memory lookups.
///
/// This matters because of how applications actually walk a folder. Measured with
/// Windows Photos on a 1,973-file directory: ~64 `open` and ~80
/// `get_security_by_name` per second, sustained for ~30 seconds, with *zero*
/// reads — it opens every file in the folder just to stat it, since WinFsp
/// returns attributes in the open response. At a ~10ms USB round trip each that
/// is half a minute of apparent hang, and none of it is enumeration.
struct AttrCache {
    ttl: Duration,
    map: Mutex<HashMap<String, (Instant, FspFileInfo)>>,
}

impl AttrCache {
    fn new(ttl: Duration) -> Self {
        AttrCache {
            ttl,
            map: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, FspFileInfo)>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, rel: &str) -> Option<FspFileInfo> {
        let map = self.lock();
        let (fetched, info) = map.get(rel)?;
        (fetched.elapsed() < self.ttl).then(|| info.clone())
    }

    /// Indexes a directory listing. `.` and `..` are skipped — they are aliases,
    /// and caching them under a child key would be wrong.
    fn put_listing(&self, dir: &str, entries: &[(String, FspFileInfo)]) {
        let now = Instant::now();
        let mut map = self.lock();

        // Crude but adequate: this is a cache, and the cost of being wrong is one
        // re-read. Anything cleverer would need per-entry bookkeeping that costs
        // more than it saves.
        if map.len() + entries.len() > MAX_CACHED_ATTRS {
            map.clear();
        }

        for (name, info) in entries {
            if name == "." || name == ".." {
                continue;
            }
            let rel = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            map.insert(rel, (now, info.clone()));
        }
    }

    fn put(&self, rel: &str, info: &FspFileInfo) {
        let mut map = self.lock();
        if map.len() >= MAX_CACHED_ATTRS {
            map.clear();
        }
        map.insert(rel.to_string(), (Instant::now(), info.clone()));
    }

    fn invalidate(&self, rel: &str) {
        self.lock().remove(rel);
    }
}

/// One directory's cache entry.
///
/// `fetching` is a single-flight token, and it is the reason this is not just a
/// map of values. `read_directory` runs under WinFsp's *shared namespace lock*,
/// and `create` / `rename` / `cleanup`-delete need that lock exclusively. So a
/// 600ms enumeration blocks writers for 600ms — and if several dispatcher
/// threads miss the cache at once they each run their own full enumeration,
/// serially holding the lock, while every writer queues behind them. That is what
/// made the Photos app appear to hang rather than merely stutter.
struct Slot {
    value: RwLock<Option<(Instant, DirEntries)>>,
    fetching: Mutex<()>,
    /// Monotonic access stamp, for eviction without touching `value`'s lock.
    last_used: AtomicU64,
}

impl Slot {
    fn fresh(&self, ttl: Duration) -> Option<DirEntries> {
        let guard = self.value.read().unwrap_or_else(|e| e.into_inner());
        let (fetched, entries) = guard.as_ref()?;
        (fetched.elapsed() < ttl).then(|| Arc::clone(entries))
    }

    /// Any cached listing, fresh or not.
    fn stale(&self) -> Option<DirEntries> {
        let guard = self.value.read().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|(_, entries)| Arc::clone(entries))
    }

    fn store(&self, entries: &DirEntries) {
        let mut guard = self.value.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some((Instant::now(), Arc::clone(entries)));
    }

    fn clear(&self) {
        let mut guard = self.value.write().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

/// Directory listings, cached across file handles.
///
/// WinFsp's per-handle `DirBuffer` does not cover this, and neither does
/// `dir_info_timeout`: an image viewer paging through a folder opens a *fresh
/// handle* for each navigation, and a fresh handle gets an empty buffer that we
/// would refill from the phone. Measured on a 1,971-file folder, that was a full
/// re-enumeration — around 600ms — several times per second while navigating.
///
/// So the cache lives above the handle, keyed by path. Entries are invalidated
/// explicitly whenever we change a directory's contents, so writes made from
/// Windows appear immediately regardless of the TTL; the TTL only bounds how long
/// a change made *on the phone* can go unnoticed.
struct DirCache {
    ttl: Duration,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    tick: AtomicU64,
}

impl DirCache {
    fn new(ttl: Duration) -> Self {
        DirCache {
            ttl,
            slots: Mutex::new(HashMap::new()),
            tick: AtomicU64::new(0),
        }
    }

    /// A poisoned lock must not take the mount down — this is a cache, and the
    /// correct response to losing it is to re-read the directory.
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Slot>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn slot(&self, rel: &str) -> Arc<Slot> {
        let stamp = self.tick.fetch_add(1, Ordering::Relaxed);
        let mut map = self.map();

        if let Some(existing) = map.get(rel) {
            existing.last_used.store(stamp, Ordering::Relaxed);
            return Arc::clone(existing);
        }

        if map.len() >= MAX_CACHED_DIRS {
            // Least recently used. Linear over 32 entries and no value locks
            // touched, so eviction never contends with an in-flight fetch.
            if let Some(victim) = map
                .iter()
                .min_by_key(|(_, s)| s.last_used.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
            {
                map.remove(&victim);
            }
        }

        let slot = Arc::new(Slot {
            value: RwLock::new(None),
            fetching: Mutex::new(()),
            last_used: AtomicU64::new(stamp),
        });
        map.insert(rel.to_string(), Arc::clone(&slot));
        slot
    }

    /// Returns a cached listing, or runs `fetch` — at most once per path at a time.
    ///
    /// A caller that finds a fetch already in flight returns the stale listing
    /// immediately rather than queueing. Serving a listing a few seconds out of
    /// date is a far better trade than holding the namespace lock while a second
    /// redundant enumeration crosses the USB link.
    fn get_or_fetch<F>(&self, rel: &str, stats: &Stats, fetch: F) -> winfsp::Result<DirEntries>
    where
        F: FnOnce() -> winfsp::Result<DirEntries>,
    {
        let slot = self.slot(rel);

        if let Some(fresh) = slot.fresh(self.ttl) {
            stats.dir_hit.fetch_add(1, Ordering::Relaxed);
            return Ok(fresh);
        }

        let _token = match slot.fetching.try_lock() {
            Ok(token) => token,
            Err(_) => {
                if let Some(stale) = slot.stale() {
                    stats.dir_stale.fetch_add(1, Ordering::Relaxed);
                    log::debug!("serving stale listing for '{rel}' — refresh in flight");
                    return Ok(stale);
                }
                // Nothing cached at all, so there is no alternative to waiting for
                // whoever is already fetching this path.
                slot.fetching.lock().unwrap_or_else(|e| e.into_inner())
            }
        };

        // Re-check: the value may have landed while we were acquiring the token.
        if let Some(fresh) = slot.fresh(self.ttl) {
            stats.dir_hit.fetch_add(1, Ordering::Relaxed);
            return Ok(fresh);
        }

        stats.dir_fetch.fetch_add(1, Ordering::Relaxed);
        let entries = fetch()?;
        slot.store(&entries);
        Ok(entries)
    }

    fn invalidate(&self, rel: &str) {
        // Clone the Arc out and drop the map guard before clearing: holding the
        // outer lock across an inner write-lock would stall every other path's
        // cache lookup behind one invalidation.
        let slot = self.map().get(rel).map(Arc::clone);
        if let Some(slot) = slot {
            slot.clear();
        }
    }

    /// Called after creating, deleting or renaming — the containing directory's
    /// listing is now wrong.
    fn invalidate_parent(&self, rel: &str) {
        self.invalidate(&parent_of(rel));
    }
}

/// Per-handle state.
///
/// Must be `Sync`: under WinFsp's default fine-grained guard strategy the driver
/// calls into the filesystem from many dispatcher threads at once through shared
/// references. That concurrency is the entire point — it is what keeps the
/// pipelined protocol busy — so the coarse (globally serialised) strategy would
/// undo the design.
pub struct AfsFileContext {
    pub rel: String,
    pub is_dir: bool,
    /// Server-side handle, opened lazily. 0 means "not opened yet".
    ///
    /// Deferring the open is what makes a folder walk cheap. An application that
    /// opens a file only to read its attributes — which is the common case, and
    /// exactly what Photos does to every file in a directory — never causes a
    /// round trip at all, because the attributes came from the cached listing.
    /// The handle is created on the first operation that genuinely needs one.
    handle: Mutex<u64>,
    /// Flags to open with, when it finally happens.
    open_flags: u32,
    pub dir_buffer: DirBuffer,
    /// Set by `set_delete`, acted on in `cleanup`.
    pub delete_pending: AtomicBool,
    /// Set by anything that changes the file's size, so `close` knows to drop the
    /// containing directory's cached listing rather than leave a stale size in it.
    pub modified: AtomicBool,
}

impl AfsFileContext {
    fn new(rel: String, is_dir: bool, handle: u64, open_flags: u32) -> Self {
        AfsFileContext {
            rel,
            is_dir,
            handle: Mutex::new(handle),
            open_flags,
            dir_buffer: DirBuffer::new(),
            delete_pending: AtomicBool::new(false),
            modified: AtomicBool::new(false),
        }
    }

    fn raw_handle(&self) -> u64 {
        *self.handle.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct AfsFileSystem {
    pub inner: AfsFs,
    pub label: String,
    dir_cache: DirCache,
    attr_cache: AttrCache,
    stats: Arc<Stats>,
}

impl AfsFileSystem {
    pub fn new(client: Arc<Client>, cache_ttl: Duration, stats: Arc<Stats>) -> Self {
        let label = if client.volume_label.is_empty() {
            "Android".to_string()
        } else {
            client.volume_label.clone()
        };
        AfsFileSystem {
            inner: AfsFs::new(client),
            label,
            dir_cache: DirCache::new(cache_ttl),
            attr_cache: AttrCache::new(cache_ttl),
            stats,
        }
    }

    fn path_of(file_name: &U16CStr) -> String {
        crate::proto::to_rel(&file_name.to_string_lossy())
    }

    /// Attributes for a path, from cache when possible.
    fn attr_of(&self, rel: &str) -> winfsp::Result<FspFileInfo> {
        if let Some(hit) = self.attr_cache.get(rel) {
            return Ok(hit);
        }
        let info = to_fsp_info(&self.inner.getattr_rel(rel).map_err(to_fsp)?);
        self.attr_cache.put(rel, &info);
        Ok(info)
    }

    /// Resolves the context to a real server handle, opening it if this is the
    /// first operation that actually needs one.
    fn handle_for(&self, context: &AfsFileContext) -> winfsp::Result<fs::FileHandle> {
        if context.is_dir {
            return Ok(fs::FileHandle {
                handle: 0,
                rel: context.rel.clone(),
                is_dir: true,
            });
        }

        let mut guard = context.handle.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == 0 {
            let (handle, attr) = self
                .inner
                .client
                .open(&context.rel, context.open_flags)
                .map_err(to_fsp)?;
            *guard = handle;
            // The server answers with a protocol Attr; convert through the fs
            // layer rather than reaching for to_fsp_info, which takes fs::FileInfo.
            self.attr_cache
                .put(&context.rel, &to_fsp_info(&fs::to_file_info(&attr)));
        }

        Ok(fs::FileHandle {
            handle: *guard,
            rel: context.rel.clone(),
            is_dir: false,
        })
    }

    /// A directory's full entry list, from cache when it is fresh.
    ///
    /// The synthesised `.` and `..` are built here and cached with the rest, so a
    /// cache hit costs no USB traffic at all — not even the two extra stats those
    /// entries would otherwise need.
    fn dir_entries(&self, fh: &fs::FileHandle) -> winfsp::Result<DirEntries> {
        let rel = &fh.rel;

        self.dir_cache.get_or_fetch(rel, &self.stats, || {
            let started = Instant::now();
            let mut entries: Vec<(String, FspFileInfo)> = Vec::new();

            // Windows expects "." and ".." on every non-root directory. The server
            // never sends them — the traversal guard rejects those components by
            // design — so they are synthesised, and cached along with the rest so a
            // hit costs no round trips at all.
            if !rel.is_empty() {
                let self_info = self.inner.getattr_rel(rel).map_err(to_fsp)?;
                let parent_info = self.inner.getattr_rel(&parent_of(rel)).map_err(to_fsp)?;
                entries.push((".".to_string(), to_fsp_info(&self_info)));
                entries.push(("..".to_string(), to_fsp_info(&parent_info)));
            }

            for (name, info) in self.inner.readdir(fh).map_err(to_fsp)? {
                entries.push((name, to_fsp_info(&info)));
            }

            let elapsed = started.elapsed();
            let shown = if rel.is_empty() { "\\" } else { rel.as_str() };
            let count = entries.len();
            // Anything slow enough to feel gets a line without needing -v.
            if elapsed >= Duration::from_millis(400) {
                log::info!("enumerated {count} entries in '{shown}' in {elapsed:.1?}");
            } else {
                log::debug!("enumerated {count} entries in '{shown}' in {elapsed:.1?}");
            }

            // The listing already carries every child's attributes, so indexing
            // them here is free and makes the subsequent per-file walk — which is
            // what applications actually do — cost no round trips.
            self.attr_cache.put_listing(rel, &entries);

            Ok(Arc::new(entries))
        })
    }
}

impl FileSystemContext for AfsFileSystem {
    type FileContext = AfsFileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let rel = Self::path_of(file_name);
        let _t = OpTimer::new(&self.stats.security, "get_security_by_name", &rel);
        let info = self.attr_of(&rel)?;

        Ok(FileSecurity {
            reparse: false,
            // The volume declares persistent_acls(false), so no descriptor is
            // written and Windows falls back to the mounting user's rights.
            // Android shared storage has no ACLs worth modelling.
            sz_security_descriptor: 0,
            attributes: info.file_attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = file_name.to_string_lossy();
        let _t = OpTimer::new(&self.stats.open, "open", &path);
        let rel = crate::proto::to_rel(&path);
        let flags = fs::flags_for_open(granted_access);

        // Fast path: attributes already known, so answer without touching USB and
        // leave the handle unopened. Most opens never read a byte.
        if let Some(info) = self.attr_cache.get(&rel) {
            let is_dir = info.file_attributes & fs::FILE_ATTRIBUTE_DIRECTORY != 0;
            *file_info.as_mut() = info;
            return Ok(AfsFileContext::new(rel, is_dir, 0, flags));
        }

        let (fh, info) = self.inner.open(&path, granted_access).map_err(to_fsp)?;
        self.attr_cache.put(&fh.rel, &to_fsp_info(&info));
        *file_info.as_mut() = to_fsp_info(&info);

        Ok(AfsFileContext::new(fh.rel, fh.is_dir, fh.handle, flags))
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = file_name.to_string_lossy();
        let _t = OpTimer::new(&self.stats.create, "create", &path);
        let is_dir = create_options & fs::FILE_DIRECTORY_FILE != 0;

        let (fh, info) = self
            .inner
            .create(&path, granted_access, is_dir)
            .map_err(to_fsp)?;
        *file_info.as_mut() = to_fsp_info(&info);

        // The containing directory just gained an entry, and a new directory has
        // its own (empty) listing. Explicit invalidation is what lets the TTL be
        // generous: it only ever bounds staleness from changes made on the phone,
        // never from changes made here.
        self.dir_cache.invalidate_parent(&fh.rel);
        self.attr_cache.invalidate(&fh.rel);
        if is_dir {
            self.dir_cache.invalidate(&fh.rel);
        }

        // create always opens for real, so the handle is live from the start.
        let flags = fs::flags_for_create(granted_access);
        Ok(AfsFileContext::new(fh.rel, fh.is_dir, fh.handle, flags))
    }

    fn close(&self, context: Self::FileContext) {
        let _t = OpTimer::new(&self.stats.close, "close", &context.rel);

        // A file whose size changed makes its directory's cached listing wrong.
        // Doing this on close rather than on every write keeps a large copy from
        // dropping the listing thousands of times.
        if context.modified.load(Ordering::SeqCst) {
            self.dir_cache.invalidate_parent(&context.rel);
            self.attr_cache.invalidate(&context.rel);
        }

        // Zero means the handle was never actually opened — the whole point of
        // the lazy path. There is nothing to close on the device.
        let handle = context.raw_handle();
        if handle != 0 {
            self.inner.close(&fs::FileHandle {
                handle,
                rel: context.rel.clone(),
                is_dir: context.is_dir,
            });
        }
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let _t = OpTimer::new(&self.stats.cleanup, "cleanup", &context.rel);
        use winfsp::constants::FspCleanupFlags::FspCleanupDelete;

        // The delete has to happen here, not in set_delete: the driver is still
        // tracking the handle at set_delete time, and removing the file underneath
        // it leaves the two views inconsistent.
        if FspCleanupDelete.is_flagged(flags) || context.delete_pending.load(Ordering::SeqCst) {
            // Deletion is by path, so this must NOT go through handle_for: opening
            // the file just to delete it would be a wasted round trip, and cleanup
            // returns nothing, so it has no way to report a failed open anyway.
            let fh = fs::FileHandle {
                handle: 0,
                rel: context.rel.clone(),
                is_dir: context.is_dir,
            };
            // Cleanup has no way to report failure — a Windows limitation — so the
            // best we can do is log it.
            if let Err(e) = self.inner.unlink(&fh) {
                log::warn!("delete of {} failed during cleanup: {e}", context.rel);
            } else {
                self.dir_cache.invalidate_parent(&context.rel);
                if context.is_dir {
                    self.dir_cache.invalidate(&context.rel);
                }
                self.attr_cache.invalidate(&context.rel);
            }
        }
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FspFileInfo,
    ) -> winfsp::Result<()> {
        // A None context means "flush the whole volume". We have no volume-wide
        // buffer to flush, so that is a no-op rather than an error.
        let Some(context) = context else {
            return Ok(());
        };

        let fh = self.handle_for(context)?;
        self.inner.flush(&fh).map_err(to_fsp)?;

        if !context.is_dir {
            let info = self.inner.getattr_rel(&context.rel).map_err(to_fsp)?;
            *file_info = to_fsp_info(&info);
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FspFileInfo,
    ) -> winfsp::Result<()> {
        let _t = OpTimer::new(&self.stats.get_info, "get_file_info", &context.rel);
        let info = self.inner.getattr_rel(&context.rel).map_err(to_fsp)?;
        *file_info = to_fsp_info(&info);
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FspFileInfo,
    ) -> winfsp::Result<()> {
        let fh = self.handle_for(context)?;
        let info = self.inner.overwrite(&fh).map_err(to_fsp)?;
        context.modified.store(true, Ordering::SeqCst);
        *file_info = to_fsp_info(&info);
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        // A zero-length read is not an EOF condition; answering it with the
        // end-of-file status below would be wrong.
        if buffer.is_empty() {
            return Ok(0);
        }

        let _t = OpTimer::new(&self.stats.read, "read", &context.rel);
        let fh = self.handle_for(context)?;

        // Loop until the buffer is full or the file ends.
        //
        // A single READ is capped at the protocol's chunk size, so one call cannot
        // necessarily satisfy a larger request. Returning short would be read as
        // end-of-file by the caller and silently truncate the file.
        let mut filled = 0usize;
        while filled < buffer.len() {
            let want = (buffer.len() - filled) as u32;
            let chunk = self
                .inner
                .read(&fh, offset + filled as u64, want)
                .map_err(to_fsp)?;
            if chunk.is_empty() {
                break;
            }
            let n = chunk.len().min(buffer.len() - filled);
            buffer[filled..filled + n].copy_from_slice(&chunk[..n]);
            filled += n;
        }

        if filled == 0 {
            // Reading at or past EOF. WinFsp expects STATUS_END_OF_FILE rather than
            // a zero-length success, or the caller loops forever.
            return Err(FspError::NTSTATUS(fs::STATUS_END_OF_FILE));
        }

        self.stats
            .read_bytes
            .fetch_add(filled as u64, Ordering::Relaxed);
        Ok(filled as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FspFileInfo,
    ) -> winfsp::Result<u32> {
        let _t = OpTimer::new(&self.stats.write, "write", &context.rel);
        let fh = self.handle_for(context)?;

        // Only these two flags need the current size, and neither is set on an
        // ordinary sequential copy — the hot path. Statting unconditionally would
        // add a USB round trip to every chunk of every file Explorer writes.
        let (offset, slice) = if write_to_eof || constrained_io {
            let current = self.inner.getattr_rel(&context.rel).map_err(to_fsp)?;
            let offset = if write_to_eof {
                current.file_size
            } else {
                offset
            };

            if constrained_io {
                // Constrained IO must not extend the file: clip to what already
                // fits, and report zero written if it lies entirely past EOF.
                if offset >= current.file_size {
                    *file_info = to_fsp_info(&current);
                    return Ok(0);
                }
                let room = (current.file_size - offset) as usize;
                (offset, &buffer[..buffer.len().min(room)])
            } else {
                (offset, buffer)
            }
        } else {
            (offset, buffer)
        };

        let written = self.inner.write(&fh, offset, slice).map_err(to_fsp)?;
        context.modified.store(true, Ordering::SeqCst);
        self.stats
            .write_bytes
            .fetch_add(u64::from(written), Ordering::Relaxed);

        // A second stat, so the size and mtime we hand back reflect the write.
        // This costs one round trip per write; returning the new size in the WRITE
        // response would remove it, at the cost of a protocol revision.
        let after = self.inner.getattr_rel(&context.rel).map_err(to_fsp)?;
        *file_info = to_fsp_info(&after);

        Ok(written)
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FspFileInfo,
    ) -> winfsp::Result<()> {
        let fh = self.handle_for(context)?;

        // Allocation size is a hint about reserved space, not the end-of-file mark.
        // The server has no way to preallocate, so honouring it would only ever
        // truncate live data — treat it as advisory and ignore it.
        if !set_allocation_size {
            self.inner.set_size(&fh, new_size).map_err(to_fsp)?;
            context.modified.store(true, Ordering::SeqCst);
        }

        let info = self.inner.getattr_rel(&context.rel).map_err(to_fsp)?;
        *file_info = to_fsp_info(&info);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let _t = OpTimer::new(&self.stats.readdir, "read_directory", &context.rel);
        // acquire() fails when the buffer is already populated and we did not ask
        // for a reset, which is the normal "continue a paged enumeration" path —
        // hence filling only on success, then always reading.
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            let entries = self.dir_entries(&fs::FileHandle {
                handle: 0,
                rel: context.rel.clone(),
                is_dir: true,
            })?;

            let mut dir_info = DirInfo::<255>::new();
            for (name, info) in entries.iter() {
                dir_info.reset();
                // A name longer than the 255-wide-char buffer cannot be represented;
                // skip it rather than aborting the whole listing.
                if set_entry_name(&mut dir_info, name).is_err() {
                    log::warn!("skipping over-long name in {}: {name}", context.rel);
                    continue;
                }
                // FspFileInfo is Clone but not Copy. It is eleven scalar fields, so
                // this is a memcpy — nothing next to the round trip it replaces.
                *dir_info.file_info_mut() = info.clone();
                lock.write(&mut dir_info)?;
            }

            drop(lock);
        }

        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let from = file_name.to_string_lossy();
        let to = new_file_name.to_string_lossy();
        self.inner
            .rename(&from, &to, replace_if_exists)
            .map_err(to_fsp)?;

        // Both ends move: an entry left one directory and joined another (often
        // the same one, which invalidating twice handles harmlessly).
        let from_rel = crate::proto::to_rel(&from);
        let to_rel = crate::proto::to_rel(&to);
        self.dir_cache.invalidate_parent(&from_rel);
        self.dir_cache.invalidate_parent(&to_rel);
        // A renamed directory carries its own listing to the new key.
        self.dir_cache.invalidate(&from_rel);
        self.dir_cache.invalidate(&to_rel);
        self.attr_cache.invalidate(&from_rel);
        self.attr_cache.invalidate(&to_rel);
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FspFileInfo,
    ) -> winfsp::Result<()> {
        let _t = OpTimer::new(&self.stats.set_info, "set_basic_info", &context.rel);
        let fh = self.handle_for(context)?;
        // Only mtime is settable through java.io on the server; the rest are
        // accepted and dropped rather than failing the call, because Explorer sets
        // all of them on every copy and a hard error would break ordinary pastes.
        self.inner
            .set_basic_info(&fh, last_write_time)
            .map_err(to_fsp)?;

        // The timestamp we just changed is part of the cached attributes.
        self.attr_cache.invalidate(&context.rel);
        self.dir_cache.invalidate_parent(&context.rel);

        let info = self.inner.getattr_rel(&context.rel).map_err(to_fsp)?;
        self.attr_cache.put(&context.rel, &to_fsp_info(&info));
        *file_info = to_fsp_info(&info);
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        // Record intent only. The contract is explicit that the file must NOT be
        // removed here; cleanup does it.
        context.delete_pending.store(delete_file, Ordering::SeqCst);
        Ok(())
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let (total, avail) = self.inner.volume_info().map_err(to_fsp)?;
        out_volume_info.total_size = total;
        out_volume_info.free_size = avail;
        out_volume_info.set_volume_label(&self.label);
        Ok(())
    }
}

/// Mounts and blocks until the device goes away or `shutdown` is set.
pub fn mount(client: Arc<Client>, opts: &MountOptions, shutdown: Arc<AtomicBool>) -> Result<()> {
    let mount_point = opts.mount_point.as_str();
    // Sets up the delayload linkage to the system WinFsp DLL. Must happen before
    // any other winfsp call.
    let _init = winfsp::winfsp_init().context(
        "could not load the WinFsp DLL.\n\
         \n\
         If WinFsp IS installed, check that the `system` feature is enabled on the\n\
         winfsp dependency in Cargo.toml — without it the crate never consults the\n\
         registry and only looks for winfsp-x64.dll on PATH.\n\
         \n\
         Otherwise install it from https://winfsp.dev",
    )?;

    let mut params = VolumeParams::new();
    params
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .volume_serial_number(0xAF50_0001)
        .file_info_timeout(opts.cache_ms)
        // dir_info_timeout is the one that matters for browsing a photo folder.
        // Without it, every next/previous in an image viewer re-enumerates the
        // whole directory across USB; with it, WinFsp answers the repeat
        // enumerations itself and never reaches us.
        .dir_info_timeout(opts.cache_ms)
        .volume_info_timeout(opts.cache_ms)
        .security_timeout(opts.cache_ms)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .reparse_points(false)
        .named_streams(false)
        // Only post cleanup when something actually changed, so read-only browsing
        // does not generate a cleanup storm over USB.
        .post_cleanup_when_modified_only(true)
        .filesystem_name("AFS");

    let stats = Arc::new(Stats::default());
    let fs = AfsFileSystem::new(
        Arc::clone(&client),
        Duration::from_millis(u64::from(opts.cache_ms)),
        Arc::clone(&stats),
    );

    // The guard strategy must be named explicitly. `FileSystemHost<T, S = FineGuard>`
    // has a default type parameter, but defaults do not participate in inference —
    // without this annotation `S` stays open and `start()` is ambiguous between the
    // FineGuard and CoarseGuard impls.
    //
    // FineGuard is also the strategy we want, not merely the default: it leaves file
    // I/O unguarded so reads and writes run concurrently across dispatcher threads.
    // CoarseGuard serialises every callback behind one lock, which would quietly
    // undo the pipelining the whole protocol is built around.
    let mut host: FileSystemHost<AfsFileSystem, FineGuard> = FileSystemHost::new(params, fs)
        .map_err(|e| anyhow::anyhow!("failed to create filesystem: {e:?}"))?;

    host.mount(mount_point)
        .map_err(|e| anyhow::anyhow!("failed to mount at {mount_point}: {e:?}"))?;

    host.start()
        .map_err(|e| anyhow::anyhow!("failed to start dispatcher: {e:?}"))?;

    log::info!("mounted at {mount_point} — press Ctrl+C to unmount");

    // Report what the filesystem is actually being asked to do. Without this the
    // only visible operation is whatever happens to be logged individually, which
    // is a poor basis for deciding what is slow.
    {
        let stats = Arc::clone(&stats);
        let shutdown = Arc::clone(&shutdown);
        let client = Arc::clone(&client);
        std::thread::Builder::new()
            .name("afs-stats".into())
            .spawn(move || {
                while client.is_alive() && !shutdown.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(5));
                    if let Some(report) = stats.take_report() {
                        log::info!("5s: {report}");
                    }
                }
            })
            .ok();
    }

    // Poll rather than park, on two conditions: if the cable is pulled the
    // client's reader thread marks the connection dead, and unmounting promptly
    // beats leaving a drive letter that hangs every process which touches it.
    while client.is_alive() && !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));
    }

    if !client.is_alive() {
        log::warn!("device disconnected — unmounting");
    }

    host.unmount();
    host.stop();
    log::info!("unmounted {mount_point}");
    Ok(())
}
