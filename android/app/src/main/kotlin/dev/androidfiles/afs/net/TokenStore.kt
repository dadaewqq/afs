package dev.androidfiles.afs.net

import android.os.Environment
import java.io.File
import java.security.SecureRandom

/**
 * The shared secret between the app and afs-host.exe.
 *
 * It lives at /sdcard/.afs/token, deliberately, so the host can fetch it with
 * `adb shell cat`. That places it exactly at the trust boundary we already
 * depend on: whoever can run adb against this device is who we are willing to
 * serve. Storing it in app-private storage would make it unreadable to the host
 * without root; storing it nowhere would leave the adb-forwarded port open to
 * any local process on the PC.
 *
 * It is NOT a defence against someone holding the unlocked phone.
 */
object TokenStore {

    private const val DIR = ".afs"
    private const val FILE = "token"

    fun tokenFile(): File = File(File(Environment.getExternalStorageDirectory(), DIR), FILE)

    /** Returns the existing token, creating one on first run. */
    fun loadOrCreate(): String {
        val f = tokenFile()
        if (f.isFile) {
            val existing = runCatching { f.readText().trim() }.getOrNull()
            if (!existing.isNullOrEmpty()) return existing
        }

        val token = generate()
        f.parentFile?.mkdirs()
        f.writeText(token)
        // Best-effort: the FAT-derived permissions on shared storage largely ignore
        // this, which is why the token is a speed bump and not the security model.
        runCatching { f.setReadable(true, false) }
        return token
    }

    fun regenerate(): String {
        val f = tokenFile()
        f.parentFile?.mkdirs()
        val token = generate()
        f.writeText(token)
        return token
    }

    private fun generate(): String {
        val bytes = ByteArray(24)
        SecureRandom().nextBytes(bytes)
        val sb = StringBuilder(bytes.size * 2)
        for (b in bytes) sb.append("%02x".format(b.toInt() and 0xFF))
        return sb.toString()
    }
}
