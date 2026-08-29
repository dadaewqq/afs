package dev.androidfiles.afs.net

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.drawable.Icon
import android.os.Build
import android.os.Environment
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import dev.androidfiles.afs.R
import dev.androidfiles.afs.fs.MediaScan
import dev.androidfiles.afs.ui.MainActivity

private const val TAG = "AfsService"
private const val CHANNEL_ID = "afs.server"
private const val NOTIF_ID = 1

const val ACTION_START = "dev.androidfiles.afs.START"
const val ACTION_STOP = "dev.androidfiles.afs.STOP"

/**
 * Hosts [FileServer] for as long as the mount is meant to be up.
 *
 * Declared as foregroundServiceType=specialUse; see AndroidManifest.xml for why
 * connectedDevice and dataSync were both ruled out. Short version: connectedDevice
 * demands a companion-device permission we do not hold and throws at
 * startForeground(), and dataSync is capped at 6 hours per 24h on Android 15+.
 */
class AfsService : Service() {

    private var server: FileServer? = null
    private var wakeLock: PowerManager.WakeLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopServer()
                stopSelf()
                return START_NOT_STICKY
            }
            else -> startServer()
        }
        // START_STICKY: if we are killed for memory pressure the mount should come
        // back on its own rather than leaving a dead drive letter on the PC.
        return START_STICKY
    }

    private fun startServer() {
        if (server?.isRunning() == true) return

        createChannel()
        try {
            startForeground(
                NOTIF_ID,
                buildNotification(),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
        } catch (e: SecurityException) {
            // The platform validates the FGS type against the permissions we hold and
            // throws from startForeground rather than at install time. Crashing the
            // whole process over it tells the user nothing, so fail the start instead.
            Log.e(TAG, "foreground service type rejected", e)
            stopSelf()
            return
        }

        // Storage I/O does not hold the CPU awake on its own, and a suspended CPU
        // mid-transfer stalls the host until the next wakeup.
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "afs:server").also {
            it.setReferenceCounted(false)
            it.acquire()
        }

        val scanner = MediaScan(applicationContext)
        val token = TokenStore.loadOrCreate()
        val root = Environment.getExternalStorageDirectory()

        server = FileServer(
            root = root,
            token = token,
            volumeLabel = "Android",
            port = DEFAULT_PORT,
            onWritten = scanner::notifyWritten,
        ).also {
            try {
                it.start()
            } catch (e: Exception) {
                Log.e(TAG, "failed to bind port $DEFAULT_PORT", e)
                stopServer()
                stopSelf()
            }
        }
    }

    private fun stopServer() {
        server?.stop()
        server = null
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
    }

    /**
     * Android 15+ hands timed-out foreground services a window to stop themselves,
     * and force-stops (with an ANR-style crash) anything that ignores it.
     *
     * specialUse is not currently timed out — that is why it was chosen — so this
     * should never fire. It is here because the set of timed types has grown with
     * every release since 14, and discovering the next addition as a crash report
     * from a user mid-transfer is the expensive way to find out.
     */
    override fun onTimeout(startId: Int, fgsType: Int) {
        Log.w(TAG, "foreground service timed out (type=$fgsType) — stopping cleanly")
        stopServer()
        stopSelf(startId)
    }

    override fun onDestroy() {
        stopServer()
        super.onDestroy()
    }

    private fun createChannel() {
        val mgr = getSystemService(NotificationManager::class.java)
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.channel_name),
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            setShowBadge(false)
        }
        mgr.createNotificationChannel(channel)
    }

    private fun buildNotification(): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(
            this,
            1,
            Intent(this, AfsService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )

        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.notif_title))
            .setContentText(getString(R.string.notif_text, DEFAULT_PORT))
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setOngoing(true)
            .setContentIntent(open)
            .addAction(
                // The Icon overload is the only non-deprecated one, and it accepts a
                // null icon — but Kotlin needs the type spelled out to pick it.
                Notification.Action.Builder(null as Icon?, "Stop", stop).build(),
            )
            .build()
    }

    companion object {
        fun start(ctx: Context) {
            val i = Intent(ctx, AfsService::class.java).setAction(ACTION_START)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                ctx.startForegroundService(i)
            } else {
                ctx.startService(i)
            }
        }

        fun stop(ctx: Context) {
            ctx.startService(Intent(ctx, AfsService::class.java).setAction(ACTION_STOP))
        }
    }
}
