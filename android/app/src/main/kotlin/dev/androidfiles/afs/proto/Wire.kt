package dev.androidfiles.afs.proto

import java.io.EOFException
import java.io.InputStream
import java.io.OutputStream

/**
 * Little-endian framing codec.
 *
 * Frame layout (see docs/protocol.md):
 *   u32 length | u32 tag | u16 op | i16 status | payload
 * where `length` counts everything after itself, so payload is `length - 8` bytes.
 */

const val HEADER_TAIL = 8 // tag + op + status, i.e. the part `length` covers besides payload

class Frame(
    val tag: Int,
    val op: Int,
    val status: Int,
    val payload: ByteArray,
)

class ProtocolException(message: String) : Exception(message)

// ---------------------------------------------------------------- frame io

/** Reads exactly [n] bytes or throws. [InputStream.read] is free to return short. */
private fun InputStream.readFully(n: Int): ByteArray {
    val buf = ByteArray(n)
    var off = 0
    while (off < n) {
        val r = read(buf, off, n - off)
        if (r < 0) throw EOFException("stream closed after $off of $n bytes")
        off += r
    }
    return buf
}

fun readFrame(input: InputStream, maxFrame: Int): Frame {
    val head = input.readFully(4)
    val length = le32(head, 0)
    if (length < HEADER_TAIL || length > maxFrame) {
        // A bogus length is unrecoverable: we cannot know where the next frame starts.
        throw ProtocolException("frame length $length out of range (max $maxFrame)")
    }
    val rest = input.readFully(length)
    val tag = le32(rest, 0)
    val op = le16(rest, 4)
    val status = le16s(rest, 6)
    val payload = rest.copyOfRange(HEADER_TAIL, rest.size)
    return Frame(tag, op, status, payload)
}

/**
 * Writes one frame. Callers MUST hold a lock across this call: responses are
 * produced by a worker pool and interleaved bytes would desynchronise the stream.
 */
fun writeFrame(out: OutputStream, tag: Int, op: Int, status: Int, payload: ByteArray) {
    val head = ByteArray(12)
    putLe32(head, 0, HEADER_TAIL + payload.size)
    putLe32(head, 4, tag)
    putLe16(head, 8, op)
    putLe16(head, 10, status)
    out.write(head)
    if (payload.isNotEmpty()) out.write(payload)
    out.flush()
}

// ---------------------------------------------------------------- primitives

fun le16(b: ByteArray, o: Int): Int =
    (b[o].toInt() and 0xFF) or ((b[o + 1].toInt() and 0xFF) shl 8)

fun le16s(b: ByteArray, o: Int): Int = le16(b, o).toShort().toInt()

fun le32(b: ByteArray, o: Int): Int =
    (b[o].toInt() and 0xFF) or
        ((b[o + 1].toInt() and 0xFF) shl 8) or
        ((b[o + 2].toInt() and 0xFF) shl 16) or
        ((b[o + 3].toInt() and 0xFF) shl 24)

fun le64(b: ByteArray, o: Int): Long {
    var v = 0L
    for (i in 7 downTo 0) v = (v shl 8) or (b[o + i].toLong() and 0xFF)
    return v
}

fun putLe16(b: ByteArray, o: Int, v: Int) {
    b[o] = (v and 0xFF).toByte()
    b[o + 1] = ((v ushr 8) and 0xFF).toByte()
}

fun putLe32(b: ByteArray, o: Int, v: Int) {
    b[o] = (v and 0xFF).toByte()
    b[o + 1] = ((v ushr 8) and 0xFF).toByte()
    b[o + 2] = ((v ushr 16) and 0xFF).toByte()
    b[o + 3] = ((v ushr 24) and 0xFF).toByte()
}

fun putLe64(b: ByteArray, o: Int, v: Long) {
    for (i in 0..7) b[o + i] = ((v ushr (8 * i)) and 0xFF).toByte()
}

// ---------------------------------------------------------------- payload reader

class PayloadReader(private val b: ByteArray) {
    private var pos = 0

    private fun need(n: Int) {
        if (pos + n > b.size) throw ProtocolException("payload underrun: want $n at $pos of ${b.size}")
    }

    fun u8(): Int { need(1); return b[pos++].toInt() and 0xFF }

    fun u16(): Int { need(2); val v = le16(b, pos); pos += 2; return v }

    /** Returned as a signed Int; callers that need the full range widen it themselves. */
    fun u32(): Int { need(4); val v = le32(b, pos); pos += 4; return v }

    fun u32L(): Long = u32().toLong() and 0xFFFFFFFFL

    fun i64(): Long { need(8); val v = le64(b, pos); pos += 8; return v }

    fun str(): String {
        val n = u16()
        need(n)
        val s = String(b, pos, n, Charsets.UTF_8)
        pos += n
        return s
    }

    fun bytes(): ByteArray {
        val n = u32()
        if (n < 0) throw ProtocolException("negative byte-string length")
        need(n)
        val out = b.copyOfRange(pos, pos + n)
        pos += n
        return out
    }
}

// ---------------------------------------------------------------- payload writer

class PayloadWriter(initial: Int = 64) {
    private var b = ByteArray(initial.coerceAtLeast(16))
    private var pos = 0

    private fun ensure(n: Int) {
        if (pos + n <= b.size) return
        var cap = b.size
        while (cap < pos + n) cap = cap shl 1
        b = b.copyOf(cap)
    }

    fun u8(v: Int) = apply { ensure(1); b[pos++] = (v and 0xFF).toByte() }

    fun u8(v: Byte) = apply { ensure(1); b[pos++] = v }

    fun u16(v: Int) = apply { ensure(2); putLe16(b, pos, v); pos += 2 }

    fun u32(v: Int) = apply { ensure(4); putLe32(b, pos, v); pos += 4 }

    fun i64(v: Long) = apply { ensure(8); putLe64(b, pos, v); pos += 8 }

    fun str(s: String) = apply {
        val raw = s.toByteArray(Charsets.UTF_8)
        if (raw.size > 0xFFFF) throw ProtocolException("string too long: ${raw.size}")
        u16(raw.size)
        ensure(raw.size)
        raw.copyInto(b, pos)
        pos += raw.size
    }

    fun bytes(raw: ByteArray, off: Int = 0, len: Int = raw.size) = apply {
        u32(len)
        ensure(len)
        raw.copyInto(b, pos, off, off + len)
        pos += len
    }

    fun attr(a: Attr) = apply {
        ensure(Attr.BYTES)
        b[pos] = a.kind
        putLe32(b, pos + 1, a.mode)
        putLe64(b, pos + 5, a.size)
        putLe64(b, pos + 13, a.mtimeMs)
        putLe64(b, pos + 21, a.ctimeMs)
        putLe64(b, pos + 29, a.atimeMs)
        putLe64(b, pos + 37, a.ino)
        pos += Attr.BYTES
    }

    fun toByteArray(): ByteArray = b.copyOf(pos)
}
