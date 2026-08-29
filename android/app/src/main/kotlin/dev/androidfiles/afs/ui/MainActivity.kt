package dev.androidfiles.afs.ui

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.Settings
import android.view.Gravity
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updatePadding
import dev.androidfiles.afs.net.AfsService
import dev.androidfiles.afs.net.DEFAULT_PORT
import dev.androidfiles.afs.net.TokenStore

/**
 * Deliberately plain. The whole UI is a permission gate plus a start button;
 * the interesting surface is on the Windows side.
 */
class MainActivity : AppCompatActivity() {

    private lateinit var status: TextView
    private lateinit var grantButton: Button
    private lateinit var startButton: Button
    private lateinit var stopButton: Button

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val pad = (16 * resources.displayMetrics.density).toInt()
        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT,
            )
        }

        status = TextView(this).apply {
            textSize = 14f
            gravity = Gravity.START
            setPadding(0, 0, 0, pad)
            // The token is displayed here and has to be copyable — it is the one
            // value someone might reasonably want to paste into --token.
            setTextIsSelectable(true)
        }
        grantButton = Button(this).apply {
            text = "Grant all-files access"
            setOnClickListener { requestAllFilesAccess() }
        }
        startButton = Button(this).apply {
            text = "Start server"
            setOnClickListener {
                AfsService.start(this@MainActivity)
                refresh()
            }
        }
        stopButton = Button(this).apply {
            text = "Stop server"
            setOnClickListener {
                AfsService.stop(this@MainActivity)
                refresh()
            }
        }

        content.addView(status)
        content.addView(grantButton)
        content.addView(startButton)
        content.addView(stopButton)

        val scroll = ScrollView(this).apply {
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            )
            addView(content)
        }
        setContentView(scroll)

        // Edge-to-edge is enforced for targetSdk 35+, so the status and navigation
        // bars draw ON TOP of the content unless the app accounts for them. Padding
        // the ScrollView rather than the inner column keeps the gutter fixed while
        // the content scrolls under it. displayCutout is included so the text does
        // not slide under a punch-hole camera in landscape.
        ViewCompat.setOnApplyWindowInsetsListener(scroll) { view, insets ->
            val bars = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
            )
            view.updatePadding(
                left = bars.left,
                top = bars.top,
                right = bars.right,
                bottom = bars.bottom,
            )
            WindowInsetsCompat.CONSUMED
        }

        requestNotificationPermissionIfNeeded()
    }

    override fun onResume() {
        super.onResume()
        refresh()
    }

    private fun hasAllFilesAccess(): Boolean = Environment.isExternalStorageManager()

    private fun requestAllFilesAccess() {
        // This is NOT a runtime permission dialog. MANAGE_EXTERNAL_STORAGE is granted
        // from a full-screen Settings page, and the user has to find this app in a
        // list — there is no way to make it a one-tap grant.
        val intent = Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION).apply {
            data = Uri.parse("package:$packageName")
        }
        runCatching { startActivity(intent) }.onFailure {
            startActivity(Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION))
        }
    }

    private fun requestNotificationPermissionIfNeeded() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        val granted = ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED
        // Without this the foreground notification is suppressed. The service still
        // runs, so this is a nag, not a gate.
        if (!granted) requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 100)
    }

    private fun refresh() {
        val ok = hasAllFilesAccess()
        grantButton.isEnabled = !ok
        startButton.isEnabled = ok

        val token = if (ok) runCatching { TokenStore.loadOrCreate() }.getOrNull() else null

        status.text = buildString {
            appendLine("All-files access: ${if (ok) "granted" else "NOT granted"}")
            appendLine("Export root: ${Environment.getExternalStorageDirectory().absolutePath}")
            appendLine("Listening on: 127.0.0.1:$DEFAULT_PORT (loopback only)")
            appendLine()
            appendLine("Token file: ${TokenStore.tokenFile().absolutePath}")
            if (token != null) {
                appendLine("Token: $token")
            }
            appendLine()
            appendLine("On the PC:")
            appendLine("  adb forward tcp:$DEFAULT_PORT tcp:$DEFAULT_PORT")
            appendLine("  afs-host.exe --mount Z:")
            appendLine()
            appendLine("Note: /Android/data and /Android/obb are not reachable")
            appendLine("even with all-files access. See docs/limits.md.")
        }
    }
}
