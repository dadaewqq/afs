package dev.androidfiles.afs.net

import android.util.Log
import dev.androidfiles.afs.fs.FsOps
import dev.androidfiles.afs.proto.AfsException
import dev.androidfiles.afs.proto.Err
import dev.androidfiles.afs.proto.Frame
import dev.androidfiles.afs.proto.Limits
import dev.androidfiles.afs.proto.Op
import dev.androidfiles.afs.proto.PayloadReader
import dev.androidfiles.afs.proto.PayloadWriter
import dev.androidfiles.afs.proto.ProtocolException
import dev.androidfiles.afs.proto.readFrame
import dev.androidfiles.afs.proto.writeFrame
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.EOFException
import java.io.IOException
import java.io.OutputStream
import java.net.Socket
import java.util.concurrent.ExecutorService
import java.util.concurrent.atomic.AtomicBoolean

private const val TAG = "AfsSession"

/**
 * One connected host.
 *
 * The read loop stays on this thread and does nothing but decode frames and hand
 * them to [workers]. Responses come back on pool threads and are serialised by
 * [writeLock]. Keeping the read loop free is what lets a host with 8 threads of
 * outstanding reads actually get 8 concurrent reads — the property MTP lacks.
 */
class Session(
    private val socket: Socket,
    private val ops: FsOps,
    private val expectedToken: String,
    private val volumeLabel: String,
    private val workers: ExecutorService,
) : Runnable {

    private val writeLock = Any()
    private val closed = AtomicBoolean(false)
    private var authed = false
    private var maxChunk = Limits.MAX_CHUNK

    override fun run() {
        socket.tcpNoDelay = true
        // Generous: a host that has gone away is detected by the read returning EOF.
        // A short timeout would kill idle-but-live mounts, which is the common case.
        socket.soTimeout = 0

        val input = BufferedInputStream(socket.getInputStream(), 64 * 1024)
        val output = BufferedOutputStream(socket.getOutputStream(), 64 * 1024)

        try {
            while (!closed.get()) {
                val frame = readFrame(input, maxChunk + Limits.FRAME_SLACK)

                if (!authed) {
                    // Nothing but INIT is allowed before the handshake completes, and a
                    // failed handshake closes the connection rather than letting the peer
                    // retry on the same socket.
                    if (frame.op != Op.INIT) {
                        respond(output, frame.tag, frame.op, Err.EPERM, ByteArray(0))
                        break
                    }
                    if (!doInit(output, frame)) break
                    continue
                }

                workers.execute { dispatch(output, frame) }
            }
        } catch (_: EOFException) {
            Log.i(TAG, "host disconnected")
        } catch (e: ProtocolException) {
            Log.w(TAG, "protocol error, dropping connection: ${e.message}")
        } catch (e: IOException) {
            Log.i(TAG, "socket closed: ${e.message}")
        } finally {
            shutdown()
        }
    }

    fun shutdown() {
        if (!closed.compareAndSet(false, true)) return
        // Releasing handles here is what stops a host crash from leaking file
        // descriptors until the service is restarted.
        ops.releaseAll()
        runCatching { socket.close() }
    }

    private fun doInit(out: OutputStream, frame: Frame): Boolean {
        val r = PayloadReader(frame.payload)
        val version = r.u32()
        val token = r.str()
        val wantChunk = r.u32()

        if (version != Limits.VERSION) {
            Log.w(TAG, "version mismatch: host $version, server ${Limits.VERSION}")
            val w = PayloadWriter().u32(Limits.VERSION).u32(0).str("").i64(0).i64(0)
            respond(out, frame.tag, Op.INIT, Err.ENOTSUP, w.toByteArray())
            return false
        }

        // Constant-time compare so a caller cannot narrow the token byte by byte.
        if (!constantTimeEquals(token, expectedToken)) {
            Log.w(TAG, "rejected host: bad token")
            respond(out, frame.tag, Op.INIT, Err.EPERM, ByteArray(0))
            return false
        }

        maxChunk = if (wantChunk in 1..Limits.MAX_CHUNK) wantChunk else Limits.MAX_CHUNK

        val fs = ops.statfs()
        val w = PayloadWriter()
            .u32(Limits.VERSION)
            .u32(maxChunk)
            .str(volumeLabel)
            .i64(fs.total)
            .i64(fs.free)
        respond(out, frame.tag, Op.INIT, Err.OK, w.toByteArray())
        authed = true
        Log.i(TAG, "host authenticated, chunk=$maxChunk")
        return true
    }

    private fun dispatch(out: OutputStream, frame: Frame) {
        val (status, payload) = try {
            Err.OK to execute(frame)
        } catch (e: AfsException) {
            e.code to ByteArray(0)
        } catch (e: ProtocolException) {
            Log.w(TAG, "malformed request op=${frame.op}: ${e.message}")
            Err.EINVAL to ByteArray(0)
        } catch (e: Exception) {
            Log.e(TAG, "unhandled error on op=${frame.op}", e)
            Err.EIO to ByteArray(0)
        }

        try {
            respond(out, frame.tag, frame.op, status, payload)
        } catch (e: IOException) {
            // The host is gone. Tear the session down instead of letting every other
            // in-flight worker discover it independently.
            Log.i(TAG, "write failed, closing session: ${e.message}")
            shutdown()
        }
    }

    private fun execute(frame: Frame): ByteArray {
        val r = PayloadReader(frame.payload)
        return when (frame.op) {
            Op.STATFS -> {
                val s = ops.statfs()
                PayloadWriter().i64(s.total).i64(s.free).i64(s.avail).u32(s.bsize).toByteArray()
            }

            Op.GETATTR -> PayloadWriter().attr(ops.getattr(r.str())).toByteArray()

            Op.READDIR -> {
                val path = r.str()
                val cookie = r.i64()
                val max = r.u32()
                val page = ops.readdir(path, cookie, max)
                val w = PayloadWriter(16 + page.entries.size * 96)
                w.u8(if (page.eof) 1 else 0)
                w.i64(page.nextCookie)
                w.u32(page.entries.size)
                for ((name, attr) in page.entries) w.str(name).attr(attr)
                w.toByteArray()
            }

            Op.OPEN -> {
                val (h, attr) = ops.open(r.str(), r.u32())
                PayloadWriter().i64(h.id).attr(attr).toByteArray()
            }

            Op.CREATE -> {
                val (h, attr) = ops.create(r.str(), r.u32(), r.u32())
                PayloadWriter().i64(h.id).attr(attr).toByteArray()
            }

            Op.CLOSE -> { ops.close(r.i64()); ByteArray(0) }

            Op.READ -> {
                val h = r.i64()
                val off = r.i64()
                val len = r.u32()
                PayloadWriter(len.coerceIn(0, maxChunk) + 8).bytes(ops.read(h, off, len)).toByteArray()
            }

            Op.WRITE -> {
                val h = r.i64()
                val off = r.i64()
                val data = r.bytes()
                PayloadWriter().u32(ops.write(h, off, data)).toByteArray()
            }

            Op.TRUNCATE -> { ops.truncate(r.i64(), r.i64()); ByteArray(0) }
            Op.FSYNC -> { ops.fsync(r.i64()); ByteArray(0) }

            Op.MKDIR -> PayloadWriter().attr(ops.mkdir(r.str(), r.u32())).toByteArray()
            Op.RMDIR -> { ops.rmdir(r.str()); ByteArray(0) }
            Op.UNLINK -> { ops.unlink(r.str()); ByteArray(0) }
            Op.RENAME -> { ops.rename(r.str(), r.str(), r.u32()); ByteArray(0) }

            Op.SETATTR -> {
                val path = r.str()
                val mask = r.u32()
                val mode = r.u32()
                val size = r.i64()
                val mtime = r.i64()
                PayloadWriter().attr(ops.setattr(path, mask, mode, size, mtime)).toByteArray()
            }

            Op.SCAN -> { ops.scan(r.str()); ByteArray(0) }

            Op.INIT -> throw AfsException(Err.EINVAL, "duplicate INIT")

            else -> throw AfsException(Err.ENOTSUP, "opcode 0x%04x".format(frame.op))
        }
    }

    private fun respond(out: OutputStream, tag: Int, op: Int, status: Int, payload: ByteArray) {
        synchronized(writeLock) { writeFrame(out, tag, op, status, payload) }
    }
}

/** Length-independent only for equal-length inputs; length itself is not secret here. */
private fun constantTimeEquals(a: String, b: String): Boolean {
    val x = a.toByteArray(Charsets.UTF_8)
    val y = b.toByteArray(Charsets.UTF_8)
    if (x.size != y.size) return false
    var diff = 0
    for (i in x.indices) diff = diff or (x[i].toInt() xor y[i].toInt())
    return diff == 0
}
