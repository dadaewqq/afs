package dev.androidfiles.afs.net

import android.util.Log
import dev.androidfiles.afs.fs.FsOps
import dev.androidfiles.afs.fs.Root
import java.io.File
import java.io.IOException
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.Collections
import java.util.concurrent.Executors
import java.util.concurrent.SynchronousQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

private const val TAG = "AfsServer"

const val DEFAULT_PORT = 8577

/**
 * Accept loop.
 *
 * Binds loopback only. That is the security boundary: the socket is reachable
 * solely through `adb forward`, so an attacker needs USB debugging authorisation
 * on this device before the token even comes into play. Binding 0.0.0.0 would put
 * the whole of shared storage on every Wi-Fi network the phone joins.
 */
class FileServer(
    private val root: File,
    private val token: String,
    private val volumeLabel: String,
    private val port: Int = DEFAULT_PORT,
    private val onWritten: (File) -> Unit,
) {
    private val running = AtomicBoolean(false)
    private var serverSocket: ServerSocket? = null
    private var acceptThread: Thread? = null

    private val sessions = Collections.synchronizedList(mutableListOf<Session>())

    /**
     * Unbounded thread count with a SynchronousQueue rather than a fixed pool:
     * a fixed pool can deadlock here, because a slow WRITE occupying every worker
     * would stall the CLOSE that would release it. Threads are cheap relative to
     * the I/O they wrap and idle ones are reaped after 30s.
     */
    private val workers: ThreadPoolExecutor = ThreadPoolExecutor(
        4, 256, 30L, TimeUnit.SECONDS, SynchronousQueue(),
        Executors.defaultThreadFactory(),
        ThreadPoolExecutor.CallerRunsPolicy(),
    )

    fun start() {
        if (!running.compareAndSet(false, true)) return

        val ss = ServerSocket(port, 8, InetAddress.getByName("127.0.0.1"))
        ss.reuseAddress = true
        serverSocket = ss

        acceptThread = Thread({ acceptLoop(ss) }, "afs-accept").apply {
            isDaemon = false
            start()
        }
        Log.i(TAG, "listening on 127.0.0.1:$port, root=${root.absolutePath}")
    }

    private fun acceptLoop(ss: ServerSocket) {
        while (running.get()) {
            val socket: Socket = try {
                ss.accept()
            } catch (e: IOException) {
                if (running.get()) Log.w(TAG, "accept failed: ${e.message}")
                break
            }

            // Each connection gets its own FsOps, so its handle table dies with it.
            val ops = FsOps(Root(root), onWritten)
            val session = Session(socket, ops, token, volumeLabel, workers)
            sessions.add(session)

            Thread({
                try {
                    session.run()
                } finally {
                    sessions.remove(session)
                }
            }, "afs-session-${socket.port}").start()
        }
        Log.i(TAG, "accept loop ended")
    }

    fun stop() {
        if (!running.compareAndSet(true, false)) return
        runCatching { serverSocket?.close() }
        synchronized(sessions) { sessions.toList() }.forEach { it.shutdown() }
        sessions.clear()
        workers.shutdownNow()
        Log.i(TAG, "stopped")
    }

    fun isRunning(): Boolean = running.get()

    fun sessionCount(): Int = sessions.size
}
