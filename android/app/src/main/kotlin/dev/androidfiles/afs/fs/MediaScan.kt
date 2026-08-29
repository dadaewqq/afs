package dev.androidfiles.afs.fs

import android.content.Context
import android.media.MediaScannerConnection
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.io.File
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.atomic.AtomicBoolean

private const val TAG = "AfsMediaScan"

/** How long to wait for a burst of writes to settle before scanning. */
private const val DEBOUNCE_MS = 750L

/** Upper bound per scan call, so one enormous copy cannot build an unbounded batch. */
private const val MAX_BATCH = 512

/**
 * Pushes written paths back into MediaStore.
 *
 * This exists because raw file I/O to /storage/emulated/0 is invisible to
 * MediaStore. MTP does not need it — it writes through MediaProvider, so the
 * Gallery updates for free. We bypass that entirely, so without this a photo
 * copied onto the phone simply never appears in any media app.
 *
 * Writes are debounced rather than scanned per-close: copying a 500-photo folder
 * would otherwise fire 500 separate scans, and MediaScannerConnection is happy to
 * take them in one batch.
 */
class MediaScan(private val context: Context) {

    private val pending = ConcurrentLinkedQueue<String>()
    private val scheduled = AtomicBoolean(false)
    private val handler = Handler(Looper.getMainLooper())

    fun notifyWritten(file: File) {
        // Directories are not media; MediaStore indexes their contents, not them.
        if (file.isDirectory) return
        pending.add(file.absolutePath)

        if (scheduled.compareAndSet(false, true)) {
            handler.postDelayed({ flush() }, DEBOUNCE_MS)
        }
    }

    private fun flush() {
        scheduled.set(false)

        val batch = ArrayList<String>(minOf(pending.size, MAX_BATCH))
        while (batch.size < MAX_BATCH) {
            val p = pending.poll() ?: break
            batch.add(p)
        }
        if (batch.isEmpty()) return

        // Deleted files are included on purpose: scanning a path that no longer
        // exists is how MediaStore learns to drop the stale row.
        MediaScannerConnection.scanFile(context, batch.toTypedArray(), null, null)
        Log.d(TAG, "scanned ${batch.size} path(s)")

        // Anything that arrived past MAX_BATCH still needs a pass.
        if (pending.isNotEmpty() && scheduled.compareAndSet(false, true)) {
            handler.postDelayed({ flush() }, DEBOUNCE_MS)
        }
    }
}
