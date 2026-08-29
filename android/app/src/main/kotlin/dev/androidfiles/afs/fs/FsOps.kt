package dev.androidfiles.afs.fs

import android.os.StatFs
import dev.androidfiles.afs.proto.AfsException
import dev.androidfiles.afs.proto.Attr
import dev.androidfiles.afs.proto.Err
import dev.androidfiles.afs.proto.Limits
import dev.androidfiles.afs.proto.OpenFlag
import dev.androidfiles.afs.proto.SetAttrMask
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.io.RandomAccessFile
import java.util.stream.IntStream

class StatFsResult(val total: Long, val free: Long, val avail: Long, val bsize: Int)

class DirPage(val entries: List<Pair<String, Attr>>, val eof: Boolean, val nextCookie: Long)

/**
 * The actual file operations, one method per opcode.
 *
 * Everything here is called from the session worker pool, so it must be
 * thread-safe. Per-file state lives behind [OpenFile.lock]; path operations rely
 * on the underlying filesystem being atomic enough, which matches what POSIX
 * callers already assume.
 */
class FsOps(
    private val root: Root,
    /** Invoked with an absolute path when a written file is closed. */
    private val onWritten: (File) -> Unit,
) {
    val handles = HandleTable()

    // ------------------------------------------------------------ metadata

    fun statfs(): StatFsResult {
        val s = StatFs(root.dir.absolutePath)
        return StatFsResult(
            total = s.totalBytes,
            free = s.freeBytes,
            avail = s.availableBytes,
            bsize = s.blockSizeLong.toInt(),
        )
    }

    fun getattr(rel: String): Attr = statOf(root.resolve(rel), rel)

    fun setattr(rel: String, mask: Int, mode: Int, size: Long, mtimeMs: Long): Attr {
        val f = root.resolve(rel)
        if (!f.exists()) throw AfsException(Err.ENOENT, rel)

        if (mask and SetAttrMask.SIZE != 0) {
            if (f.isDirectory) throw AfsException(Err.EISDIR, rel)
            try {
                RandomAccessFile(f, "rw").use { it.setLength(size) }
            } catch (e: IOException) {
                throw AfsException(Err.EIO, e.message)
            }
        }

        if (mask and SetAttrMask.MTIME != 0) {
            // Best-effort: setLastModified silently fails on some Android volumes.
            // Not worth failing the whole call over — Explorer sets mtime on every copy.
            f.setLastModified(mtimeMs)
        }

        if (mask and SetAttrMask.MODE != 0) {
            // Only the write bit is meaningful through java.io, and only sometimes.
            val writable = mode and 0b010_000_000 != 0
            f.setWritable(writable, true)
        }

        return statOf(f, rel)
    }

    // ------------------------------------------------------------ directories

    fun readdir(rel: String, cookie: Long, maxEntries: Int): DirPage {
        val limit = maxEntries.coerceIn(1, Limits.MAX_DIR_ENTRIES)

        val snapshotId: Long
        val offset: Int
        val snapshot: DirSnapshot

        if (cookie == 0L) {
            val dir = root.resolve(rel)
            if (!dir.exists()) throw AfsException(Err.ENOENT, rel)
            if (!dir.isDirectory) throw AfsException(Err.ENOTDIR, rel)

            // list() can return null on an unreadable directory even when it exists.
            val names = dir.list()?.sorted() ?: throw AfsException(Err.EACCES, rel)
            snapshot = DirSnapshot(rel, names)
            snapshotId = handles.addDir(snapshot)
            offset = 0
        } else {
            snapshotId = cookie ushr 32
            offset = (cookie and 0xFFFFFFFFL).toInt()
            snapshot = handles.getDir(snapshotId)
                // An expired snapshot cannot be silently restarted: that would make the
                // host re-emit entries it has already seen. Fail and let it start over.
                ?: throw AfsException(Err.EINVAL, "stale readdir cookie")
            if (snapshot.rel != rel) throw AfsException(Err.EINVAL, "cookie/path mismatch")
        }

        val end = minOf(offset + limit, snapshot.names.size)
        val page = snapshot.names.subList(offset, end)

        // Stat the page in parallel.
        //
        // Each stat is a blocking round trip through the FUSE daemon backing
        // /storage/emulated/0, so the wall time is almost entirely waiting rather
        // than CPU — exactly the shape that parallelises well. Serially, a
        // 2,000-entry folder measured ~600ms, which the host then spends holding
        // WinFsp's namespace lock while every writer queues behind it.
        //
        // Writing into a pre-sized array by index keeps the sorted order and needs
        // no synchronisation: every task owns one distinct slot.
        val statted = arrayOfNulls<Pair<String, Attr>>(page.size)
        IntStream.range(0, page.size).parallel().forEach { i ->
            val name = page[i]
            val childRel = joinRel(rel, name)
            val child = File(root.dir, childRel.replace('/', File.separatorChar))
            // One stat per entry, doubling as the existence check — a file deleted
            // between snapshot and stat is normal here, so ENOENT just means skip.
            // Probing with exists() first would add a second FUSE round trip per
            // entry to learn what this call already reports.
            statted[i] = try {
                name to statOf(child, childRel)
            } catch (e: AfsException) {
                null
            }
        }
        val out = statted.filterNotNull()

        val eof = end >= snapshot.names.size
        if (eof) handles.dropDir(snapshotId)

        val next = if (eof) 0L else (snapshotId shl 32) or (end.toLong() and 0xFFFFFFFFL)
        return DirPage(out, eof, next)
    }

    fun mkdir(rel: String, @Suppress("UNUSED_PARAMETER") mode: Int): Attr {
        val f = root.resolve(rel)
        if (f.exists()) throw AfsException(Err.EEXIST, rel)
        if (!f.mkdir()) throw AfsException(Err.EIO, "mkdir failed: $rel")
        return statOf(f, rel)
    }

    fun rmdir(rel: String) {
        val f = root.resolve(rel)
        if (!f.exists()) throw AfsException(Err.ENOENT, rel)
        if (!f.isDirectory) throw AfsException(Err.ENOTDIR, rel)
        val kids = f.list()
        if (kids != null && kids.isNotEmpty()) throw AfsException(Err.ENOTEMPTY, rel)
        if (!f.delete()) throw AfsException(Err.EIO, "rmdir failed: $rel")
    }

    fun unlink(rel: String) {
        val f = root.resolve(rel)
        if (!f.exists()) throw AfsException(Err.ENOENT, rel)
        if (f.isDirectory) throw AfsException(Err.EISDIR, rel)
        if (!f.delete()) throw AfsException(Err.EACCES, "unlink failed: $rel")
        onWritten(f) // let MediaStore drop the stale row
    }

    fun rename(fromRel: String, toRel: String, @Suppress("UNUSED_PARAMETER") flags: Int) {
        val from = root.resolve(fromRel)
        val to = root.resolve(toRel)
        if (!from.exists()) throw AfsException(Err.ENOENT, fromRel)

        // File.renameTo is not POSIX rename: it will not overwrite on all Android
        // volumes, and it reports failure with a bare boolean. Unlink the target
        // first so the common Explorer "replace existing file" flow works.
        if (to.exists() && !to.isDirectory) to.delete()

        if (!from.renameTo(to)) {
            // Cross-volume renames land here. Copy-then-delete is the honest fallback,
            // but it is not atomic and a large file would stall the request, so refuse
            // and let the host fall back to its own copy loop with progress.
            throw AfsException(Err.EIO, "rename failed (cross-volume?): $fromRel -> $toRel")
        }
        onWritten(from)
        onWritten(to)
    }

    // ------------------------------------------------------------ files

    fun open(rel: String, flags: Int): Pair<OpenFile, Attr> {
        val f = root.resolve(rel)
        val wantWrite = flags and (OpenFlag.WRITE or OpenFlag.APPEND or OpenFlag.TRUNC) != 0
        val wantCreate = flags and OpenFlag.CREATE != 0

        if (f.exists()) {
            if (wantCreate && flags and OpenFlag.EXCL != 0) throw AfsException(Err.EEXIST, rel)
            if (f.isDirectory) throw AfsException(Err.EISDIR, rel)
        } else {
            if (!wantCreate) throw AfsException(Err.ENOENT, rel)
            val parent = f.parentFile
            if (parent != null && !parent.exists()) throw AfsException(Err.ENOENT, "parent of $rel")
        }

        val mode = if (wantWrite) "rw" else "r"
        val raf = try {
            RandomAccessFile(f, mode)
        } catch (e: FileNotFoundException) {
            // RandomAccessFile reports both "missing" and "denied" as FileNotFoundException.
            // Existence is the only signal we have to tell them apart.
            throw AfsException(if (f.exists()) Err.EACCES else Err.ENOENT, e.message)
        }

        if (wantWrite && flags and OpenFlag.TRUNC != 0) {
            try {
                raf.setLength(0)
            } catch (e: IOException) {
                raf.close()
                throw AfsException(Err.EIO, e.message)
            }
        }

        val h = handles.add(rel, f, raf, wantWrite)
        if (wantWrite && flags and OpenFlag.TRUNC != 0) h.dirty.set(true)
        return h to statOf(f, rel)
    }

    fun create(rel: String, flags: Int, mode: Int): Pair<OpenFile, Attr> =
        open(rel, flags or OpenFlag.CREATE or OpenFlag.WRITE)
            .also { if (mode != 0) runCatching { it.first.file.setWritable(true, true) } }

    fun close(id: Long) {
        val h = handles.remove(id) ?: throw AfsException(Err.EBADF, "handle $id")
        synchronized(h.lock) { h.close() }
        if (h.dirty.get()) onWritten(h.file)
    }

    fun read(id: Long, offset: Long, len: Int): ByteArray {
        if (len < 0 || len > Limits.MAX_CHUNK) throw AfsException(Err.EINVAL, "read len $len")
        val h = handles.get(id)
        return synchronized(h.lock) {
            try {
                val size = h.raf.length()
                if (offset >= size) return@synchronized ByteArray(0)
                val n = minOf(len.toLong(), size - offset).toInt()
                val buf = ByteArray(n)
                h.raf.seek(offset)
                h.raf.readFully(buf)
                buf
            } catch (e: IOException) {
                throw AfsException(Err.EIO, e.message)
            }
        }
    }

    fun write(id: Long, offset: Long, data: ByteArray): Int {
        val h = handles.get(id)
        if (!h.writable) throw AfsException(Err.EACCES, "handle ${h.rel} is read-only")
        return synchronized(h.lock) {
            try {
                h.raf.seek(offset)
                h.raf.write(data)
                h.dirty.set(true)
                data.size
            } catch (e: IOException) {
                // A full volume surfaces as a generic IOException here; the message is
                // the only hint, so match on it to give the host a usable ENOSPC.
                val full = e.message?.contains("space", ignoreCase = true) == true
                throw AfsException(if (full) Err.ENOSPC else Err.EIO, e.message)
            }
        }
    }

    fun truncate(id: Long, size: Long) {
        val h = handles.get(id)
        if (!h.writable) throw AfsException(Err.EACCES, h.rel)
        synchronized(h.lock) {
            try {
                h.raf.setLength(size)
                h.dirty.set(true)
            } catch (e: IOException) {
                throw AfsException(Err.EIO, e.message)
            }
        }
    }

    fun fsync(id: Long) {
        val h = handles.get(id)
        synchronized(h.lock) {
            try {
                h.raf.fd.sync()
            } catch (e: IOException) {
                throw AfsException(Err.EIO, e.message)
            }
        }
    }

    fun scan(rel: String) {
        onWritten(root.resolve(rel))
    }

    /** Connection teardown: drop every fd this session held. */
    fun releaseAll() {
        handles.closeAll().forEach { if (it.dirty.get()) onWritten(it.file) }
    }
}
