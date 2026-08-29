package dev.androidfiles.afs.fs

import dev.androidfiles.afs.proto.AfsException
import dev.androidfiles.afs.proto.Attr
import dev.androidfiles.afs.proto.Err
import dev.androidfiles.afs.proto.Kind
import java.io.File
import java.io.IOException
import java.nio.file.Files
import java.nio.file.NoSuchFileException
import java.nio.file.attribute.BasicFileAttributes

/**
 * Path resolution and the traversal guard.
 *
 * Every path arriving from the host is untrusted. The rule is deliberately strict
 * rather than clever: split on '/', and reject any component that is empty, ".",
 * ".." or contains a NUL or a backslash. Normalising ".." away instead of refusing
 * it is how these guards usually get defeated, so we simply never accept it.
 */
class Root(val dir: File) {

    private val canonicalRoot: String = dir.canonicalPath.trimEnd(File.separatorChar)

    fun resolve(rel: String): File {
        if (rel.isEmpty()) return dir

        var out = dir
        for (part in rel.split('/')) {
            if (part.isEmpty() || part == "." || part == "..") {
                throw AfsException(Err.EINVAL, "bad path component in '$rel'")
            }
            // Reject NUL (0) and backslash (92) without spelling them as char literals.
            if (part.any { it.code == 0 || it.code == 92 }) {
                throw AfsException(Err.EINVAL, "illegal character in '$rel'")
            }
            out = File(out, part)
        }

        // Second gate: a symlink inside the tree could still point outside it, and
        // component checks cannot see that. Canonicalising catches it.
        val canon = out.canonicalPath
        if (canon != canonicalRoot && !canon.startsWith(canonicalRoot + File.separator)) {
            throw AfsException(Err.EPERM, "path escapes export root")
        }
        return out
    }
}

/**
 * Synthetic inode number.
 *
 * The paths we can reach on Android do not expose a real inode through the java.io
 * API, and MediaStore ids only cover indexed media. WinFsp only needs a value that
 * is stable while a file keeps its name and unlikely to collide, so a 64-bit
 * FNV-1a over the relative path does the job.
 */
fun inodeFor(rel: String): Long {
    var h = -0x340d631b7bdddcdbL // 14695981039346656037 (FNV offset basis)
    for (b in rel.toByteArray(Charsets.UTF_8)) {
        h = h xor (b.toLong() and 0xFF)
        h *= 0x100000001b3L
    }
    // Never hand back 0; some consumers treat it as "no inode".
    return if (h == 0L) 1L else h
}

/**
 * Stats a file in a **single** syscall.
 *
 * The obvious spelling of this function — `exists()`, `isDirectory()`, `isFile()`,
 * `length()`, `lastModified()`, `canWrite()` — costs one syscall each, and every
 * one of them crosses the FUSE daemon that backs `/storage/emulated/0`. That is
 * invisible on one file and ruinous on a directory listing, where it multiplies
 * by the entry count: a 2,000-photo folder cost ~18,000 FUSE round trips, which
 * at the ~0.3-0.5ms FUSE charges per operation is entire seconds of latency
 * before a single byte of image data moves.
 *
 * `Files.readAttributes` is one `stat()` returning kind, size and all three
 * timestamps together.
 */
fun statOf(f: File, rel: String): Attr {
    val attrs: BasicFileAttributes = try {
        Files.readAttributes(f.toPath(), BasicFileAttributes::class.java)
    } catch (e: NoSuchFileException) {
        throw AfsException(Err.ENOENT, rel)
    } catch (e: IOException) {
        throw AfsException(Err.EIO, e.message)
    }

    val kind = when {
        attrs.isDirectory -> Kind.DIR
        attrs.isRegularFile -> Kind.FILE
        attrs.isSymbolicLink -> Kind.SYMLINK
        else -> Kind.OTHER
    }

    val mtime = attrs.lastModifiedTime().toMillis()
    // Free now that one stat carries all three. Fall back to mtime when the
    // filesystem reports nothing: a zero timestamp renders as 1601-01-01 in
    // Explorer and confuses anything sorting by date.
    val ctime = attrs.creationTime().toMillis().let { if (it > 0L) it else mtime }
    val atime = attrs.lastAccessTime().toMillis().let { if (it > 0L) it else mtime }

    // Everything reachable under /storage/emulated/0 with all-files access is
    // writable, so the extra access() syscall per entry only ever confirmed what
    // we already knew. Report 0755/0644 and skip it.
    val mode = if (attrs.isDirectory) 0b111_101_101 else 0b110_100_100

    return Attr(
        kind = kind,
        mode = mode,
        size = if (attrs.isDirectory) 0L else attrs.size(),
        mtimeMs = mtime,
        ctimeMs = ctime,
        atimeMs = atime,
        ino = inodeFor(rel),
    )
}

fun joinRel(parent: String, name: String): String =
    if (parent.isEmpty()) name else "$parent/$name"
