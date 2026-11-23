package org.shenko.node.activation

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import org.json.JSONObject
import org.shenko.node.R
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Visible chooser and system-confirmation entry for an ordinary APK update.
 *
 * The selected document is copied once into private `0600` storage; nothing
 * outside this app is read again. The activity never confirms the system
 * dialog, never starts a replacement node and never writes activation
 * authority. It uses no root, Shizuku or Dhizuku path.
 */
class InstallerActivity : Activity() {
    private lateinit var message: TextView
    private lateinit var confirmButton: Button
    private val working = AtomicBoolean(false)

    @Volatile
    private var selection: CandidatePackage.Facts? = null

    @Volatile
    private var requestDigest: String? = null

    @Volatile
    private var offeredConfirmation: Intent? = null

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        message = TextView(this).apply { setText(R.string.package_install_hint) }
        setContentView(buildUi())
        if (consumeConfirmation(intent)) return
        restoreSelection()
        if (requestDigest != null) refreshStatus()
    }

    override fun onNewIntent(intent: Intent?) {
        super.onNewIntent(intent)
        if (intent == null) return
        setIntent(intent)
        consumeConfirmation(intent)
    }

    override fun onResume() {
        super.onResume()
        val digest = requestDigest ?: return
        val held = ActivationController.offeredConfirmation(digest) ?: return
        offeredConfirmation = held
        if (::confirmButton.isInitialized) confirmButton.isEnabled = true
    }

    private fun buildUi(): View {
        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val density = resources.displayMetrics.density
            setPadding(
                (24 * density).toInt(),
                (32 * density).toInt(),
                (24 * density).toInt(),
                (24 * density).toInt(),
            )
        }
        layout.addView(message)
        fun action(label: Int, perform: () -> Unit) {
            layout.addView(Button(this@InstallerActivity).apply {
                setText(label)
                setOnClickListener { perform() }
            })
        }
        action(R.string.package_install_choose) { choosePackage() }
        action(R.string.package_install_permission) { openInstallerPermission() }
        action(R.string.package_install_notifications) { requestNotificationPermission() }
        action(R.string.package_install_request) { requestInstallation() }
        action(R.string.package_install_observe) {
            if (requestDigest == null) {
                message.setText(R.string.package_install_select_first)
            } else {
                refreshStatus()
            }
        }
        action(R.string.package_install_cancel) { requestCancellation() }
        confirmButton = Button(this).apply {
            setText(R.string.package_install_confirm)
            isEnabled = false
            setOnClickListener { openConfirmation() }
        }
        layout.addView(confirmButton)
        return layout
    }

    private fun consumeConfirmation(launch: Intent): Boolean {
        if (!launch.hasExtra(ActivationController.EXTRA_INSTALL_INTENT)) return false
        return try {
            val confirmation = ActivationController.takeConfirmation(this, launch)
            if (confirmation == null) {
                false
            } else {
                // The system's confirmation Intent is opened; it is never
                // answered by this app on the user's behalf.
                startActivity(confirmation.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                finish()
                true
            }
        } catch (failure: Exception) {
            message.text = getString(R.string.package_install_failed, describeFailure(failure))
            true
        }
    }

    private fun restoreSelection() {
        try {
            val restored = CandidatePackage.readSelection(this) ?: return
            selection = restored.first
            requestDigest = restored.second
            message.text = describeSelection(restored.first)
        } catch (failure: Exception) {
            message.text = getString(R.string.package_install_failed, describeFailure(failure))
        }
    }

    private fun choosePackage() {
        try {
            startActivityForResult(
                Intent(Intent.ACTION_OPEN_DOCUMENT)
                    .setType("application/vnd.android.package-archive")
                    .addCategory(Intent.CATEGORY_OPENABLE)
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION),
                REQUEST_PICK_APK,
            )
        } catch (failure: ActivityNotFoundException) {
            message.setText(R.string.package_install_no_document_picker)
        }
    }

    @Deprecated("Uses the platform document-picker result contract")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_PICK_APK || resultCode != RESULT_OK) return
        val uri = data?.data ?: return
        runOperation(R.string.package_install_working) {
            val facts = CandidatePackage.stage(this, uri)
            selection = facts
            requestDigest = null
            CandidatePackage.persistSelection(this, facts, null)
            describeSelection(facts)
        }
    }

    private fun openInstallerPermission() {
        try {
            startActivity(
                Intent(
                    Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                    Uri.parse("package:$packageName"),
                ),
            )
        } catch (failure: ActivityNotFoundException) {
            message.text = getString(
                R.string.package_install_failed,
                "installation permission settings are unavailable",
            )
        }
    }

    private fun requestNotificationPermission() {
        try {
            if (Build.VERSION.SDK_INT >= 33) {
                requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), REQUEST_NOTIFICATIONS)
            } else {
                startActivity(
                    Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                        .putExtra(Settings.EXTRA_APP_PACKAGE, packageName),
                )
            }
        } catch (failure: ActivityNotFoundException) {
            message.setText(R.string.package_install_no_notification_settings)
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQUEST_NOTIFICATIONS) return
        message.setText(
            if (grantResults.firstOrNull() == android.content.pm.PackageManager.PERMISSION_GRANTED) {
                R.string.package_install_notifications_granted
            } else {
                R.string.package_install_notifications_denied
            },
        )
    }

    private fun requestInstallation() {
        val facts = selection ?: run {
            message.setText(R.string.package_install_select_first)
            return
        }
        runOperation(R.string.package_install_working) {
            val base = baseDigest(facts)
            val persisted = requestDigest
            val probe = persisted?.let { ActivationController.status(this, it) }
            val digest = resolveDigest(base, persisted, probe)
            requestDigest = digest
            CandidatePackage.persistSelection(this, facts, digest)
            val observation = ActivationController.install(
                this,
                digest,
                facts.apkPath,
                facts.packageName,
                facts.versionCode,
                facts.signerSha256,
                facts.apkSha256,
                facts.apkBytes,
                facts.nativeSha256,
            )
            describe(observation, digest)
        }
    }

    private fun requestCancellation() {
        val digest = requestDigest ?: run {
            message.setText(R.string.package_install_select_first)
            return
        }
        runOperation(R.string.package_install_working) {
            describe(ActivationController.abandon(this, digest), digest)
        }
    }

    private fun refreshStatus() {
        val digest = requestDigest ?: return
        runOperation(R.string.package_install_working) {
            describe(ActivationController.status(this, digest), digest)
        }
    }

    private fun openConfirmation() {
        val digest = requestDigest
        val confirmation = digest?.let { ActivationController.takeOfferedConfirmation(it) }
            ?: offeredConfirmation
        if (confirmation == null) {
            message.setText(R.string.package_install_no_confirmation)
            return
        }
        try {
            startActivity(confirmation.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            offeredConfirmation = null
            confirmButton.isEnabled = false
        } catch (failure: Exception) {
            message.text = getString(R.string.package_install_failed, describeFailure(failure))
        }
    }

    private fun resolveDigest(base: String, persisted: String?, probe: JSONObject?): String {
        if (probe == null || !probe.optBoolean("attempt_enrolled", false)) {
            return persisted ?: base
        }
        val state = probe.optString("state")
        val terminal = state in setOf("success", "failure", "abandoned", "stream_failed")
        if (!terminal) return persisted ?: base
        if (probe.optBoolean("session_actionable", false)) {
            failInstaller(
                "denied",
                "installer_session_unresolved",
                "an install session is still open; observe or cancel it before requesting another installation",
            )
        }
        // The previous attempt is settled and owns no session: a fresh
        // request identity is the honest way to retry.
        return "sha256:" + InstallerRecords.sha256Hex("$base:retry:${UUID.randomUUID()}")
    }

    private fun baseDigest(facts: CandidatePackage.Facts): String =
        "sha256:" + InstallerRecords.sha256Hex(
            "local-apk:${facts.apkSha256}:${facts.versionCode}:${facts.signerSha256}:${facts.nativeSha256}",
        )

    private fun describeSelection(facts: CandidatePackage.Facts): String =
        getString(
            R.string.package_install_selected,
            facts.packageName,
            facts.versionCode.toString(),
            facts.apkSha256.take(16),
        )

    private fun describe(observation: JSONObject, digest: String): String {
        offeredConfirmation = ActivationController.offeredConfirmation(digest)
        val session = if (observation.isNull("session_id")) "none" else observation.optString("session_id", "none")
        val text = getString(
            R.string.package_install_observation,
            observation.optString("state", "unknown"),
            session,
            observation.optBoolean("terminal_confirmed", false).toString(),
        )
        return if (offeredConfirmation == null) {
            text
        } else {
            "$text\n${getString(R.string.package_install_confirmation_ready)}"
        }
    }

    /**
     * Run one blocking installer operation off the main thread. Only one
     * operation runs at a time, so a double tap cannot start two requests.
     */
    private fun runOperation(initial: Int, operation: () -> String) {
        if (!working.compareAndSet(false, true)) return
        message.setText(initial)
        Thread({
            val outcome = try {
                operation()
            } catch (failure: Throwable) {
                getString(R.string.package_install_failed, describeFailure(failure))
            }
            runOnUiThread {
                if (!isFinishing && !isDestroyed) {
                    message.text = outcome
                    if (::confirmButton.isInitialized) {
                        confirmButton.isEnabled = offeredConfirmation != null
                    }
                }
                working.set(false)
            }
        }, "shenko-package-installer").start()
    }

    private companion object {
        const val REQUEST_PICK_APK = 1
        const val REQUEST_NOTIFICATIONS = 2
    }
}
