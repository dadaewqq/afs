package dev.androidfiles.afs.fs

import dev.androidfiles.afs.proto.AfsException
import dev.androidfiles.afs.proto.Err
import java.io.File
import java.io.RandomAccessFile
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

/**
 * One open file.
 *
 * [raf] is shared by every worker thread serving this handle, and RandomAccessFile
 * keeps a single file pointer, so seek+read must be atomic against seek+write.
 * The lock is per-handle rather than global: two threads working on two different
 * files never contend, which is the whole point of pipelining the protocol.
 */
class OpenFile(
    val id: Long,
    val rel: String,
    val file: File,
    val raf: RandomAccessFile,
    val writable: Boolean,
) {
    val lock = Any()

    /** Set on the first successful write; drives the MediaStore rescan on close. */
    val dirty = AtomicBoolean(false)

    fun close() {
        try {
            raf.close()
        } catch (_: Exception) {
            // Nothing useful to do; the handle is going away either way.
        }
    }
}

/** Snapshot of a directory listing, held across paged READDIR calls. */
class DirSnapshot(val rel: String, val names: List<String>)

private const val MAX_SNAPSHOTS = 64

class HandleTable {
    private val next = AtomicLong(1)
    private val files = ConcurrentHashMap<Long, OpenFile>()
    private val dirs = ConcurrentHashMap<Long, DirSnapshot>()

    fun add(rel: String, file: File, raf: RandomAccessFile, writable: Boolean): OpenFile {
        val h = OpenFile(next.getAndIncrement(), rel, file, raf, writable)
        files[h.id] = h
        return h
    }

    fun get(id: Long): OpenFile = files[id] ?: throw AfsException(Err.EBADF, "handle $id")

    fun remove(id: Long): OpenFile? = files.remove(id)

    fun addDir(snapshot: DirSnapshot): Long {
        val id = next.getAndIncrement()
        dirs[id] = snapshot

        // A host that starts an enumeration and walks away leaks the snapshot, and
        // Explorer abandons listings constantly (close the window mid-scroll). Cap the
        // table and evict lowest-id first: ids come from a counter, so that is oldest
        // first. A host whose snapshot is evicted gets EINVAL and restarts the listing.
        if (dirs.size > MAX_SNAPSHOTS) {
            dirs.keys.sorted()
                .take(dirs.size - MAX_SNAPSHOTS)
                .forEach { dirs.remove(it) }
        }
        return id
    }

    fun getDir(cookieHigh: Long): DirSnapshot? = dirs[cookieHigh]

    fun dropDir(cookieHigh: Long) {
        dirs.remove(cookieHigh)
    }

    /** Called when a connection drops, so a host crash cannot leak fds. */
    fun closeAll(): List<OpenFile> {
        val open = files.values.toList()
        files.clear()
        dirs.clear()
        open.forEach { it.close() }
        return open
    }

    fun openCount(): Int = files.size
}
