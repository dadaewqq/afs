package dev.androidfiles.afs.proto

/** Wire constants. Mirrored in windows/afs-host/src/proto.rs — change both together. */
object Op {
    const val INIT = 0x0001
    const val STATFS = 0x0002

    const val GETATTR = 0x0010
    const val READDIR = 0x0011
    const val OPEN = 0x0012
    const val CREATE = 0x0013
    const val CLOSE = 0x0014
    const val READ = 0x0015
    const val WRITE = 0x0016
    const val TRUNCATE = 0x0017
    const val FSYNC = 0x0018

    const val MKDIR = 0x0020
    const val RMDIR = 0x0021
    const val UNLINK = 0x0022
    const val RENAME = 0x0023
    const val SETATTR = 0x0024

    const val SCAN = 0x0030
}

/** Negative errno. The host maps these onto NTSTATUS. */
object Err {
    const val OK = 0
    const val EPERM = -1
    const val ENOENT = -2
    const val EIO = -5
    const val EBADF = -9
    const val EACCES = -13
    const val EEXIST = -17
    const val ENOTDIR = -20
    const val EISDIR = -21
    const val EINVAL = -22
    const val ENOSPC = -28
    const val ENOTEMPTY = -39
    const val ENOTSUP = -95
}

object OpenFlag {
    const val READ = 1
    const val WRITE = 2
    const val APPEND = 4
    const val CREATE = 8
    const val EXCL = 16
    const val TRUNC = 32
}

object SetAttrMask {
    const val MODE = 1
    const val SIZE = 2
    const val MTIME = 4
}

object Kind {
    const val FILE: Byte = 0
    const val DIR: Byte = 1
    const val SYMLINK: Byte = 2
    const val OTHER: Byte = 3
}

object Limits {
    const val VERSION = 1

    /** Largest READ/WRITE payload the server will accept, before framing overhead. */
    const val MAX_CHUNK = 1 shl 20 // 1 MiB

    /** Framing slack on top of MAX_CHUNK for headers, paths and attr blocks. */
    const val FRAME_SLACK = 4096

    /** READDIR entries per response. Bounded so one huge directory cannot blow a frame. */
    const val MAX_DIR_ENTRIES = 512
}

/** Raised by the ops layer; carries the errno the session should report. */
class AfsException(val code: Int, message: String? = null) : Exception(message ?: "afs errno $code")

/** A stat result, encoded as the fixed 45-byte `attr` struct. */
data class Attr(
    val kind: Byte,
    val mode: Int,
    val size: Long,
    val mtimeMs: Long,
    val ctimeMs: Long,
    val atimeMs: Long,
    val ino: Long,
) {
    companion object {
        const val BYTES = 45
    }
}
