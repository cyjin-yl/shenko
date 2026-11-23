package org.shenko.node.activation

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import org.json.JSONObject
import org.shenko.node.ShenkoNodeService
import java.io.File

/**
 * Restart activation after an ordinary package replacement.
 *
 * Android delivers `MY_PACKAGE_REPLACED` to the newly installed package. This
 * receiver reads back the installed version and signer, matches them against
 * an enrolled successful installer attempt and then *requests* the existing
 * node service. A foreground-service start can be refused by the platform;
 * "requested" is never reported as node readiness, and installer success is
 * never reported as installed-byte or loaded-JNI proof.
 *
 * No selector, epoch, runtime-owner admission or admission token is written
 * here. When no enrolled attempt matches, the node is not started
 * automatically: opening Shenko remains the ordinary user entry.
 */
class PackageReplacedReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_MY_PACKAGE_REPLACED) return
        val summary = JSONObject().put("trigger", "my_package_replaced")
        var installedVersion = -1L
        var installedSigner: String? = null
        try {
            val info = CandidatePackage.installed(context)
            installedVersion = CandidatePackage.versionCode(info)
            installedSigner = CandidatePackage.signer(info)
            val sourceDir = info.applicationInfo?.sourceDir
            summary.put("installed_version_code", installedVersion)
                .put("installed_version_name", info.versionName ?: JSONObject.NULL)
                .put("installed_signer_sha256", installedSigner)
                .put("installed_base_apk_path", sourceDir ?: JSONObject.NULL)
                .put("installed_base_apk_bytes", sourceDir?.let { File(it).length() } ?: -1L)
                // Installer success is not installed-byte or loaded-JNI proof.
                .put("installed_bytes_verified", false)
        } catch (failure: Exception) {
            summary.put("installed_identity_error", describeFailure(failure))
        }

        val matching = if (installedVersion > 0 && !installedSigner.isNullOrEmpty()) {
            runCatching {
                InstallerRecords.listAttempts(context).filter { record ->
                    record.optString("state") == "success" &&
                        record.optJSONObject("restart") == null &&
                        record.optLong("version_code", -1) == installedVersion &&
                        record.optString("signer_sha256") == installedSigner
                }
            }.getOrElse { failure ->
                summary.put("attempt_scan_error", describeFailure(failure))
                emptyList()
            }
        } else {
            emptyList()
        }
        summary.put("enrolled_attempts_matched", matching.size)

        if (matching.isEmpty()) {
            summary.put("condition", "successor_start_not_requested")
                .put(
                    "detail",
                    "no enrolled successful installer attempt matches the installed version and signer; " +
                        "opening Shenko starts the node deliberately",
                )
            persistSummary(context, summary)
            return
        }

        var startFailure: Exception? = null
        try {
            ShenkoNodeService.start(context)
        } catch (failure: Exception) {
            startFailure = failure
        }
        val condition = if (startFailure == null) "successor_start_requested" else "successor_start_refused"
        val restart = JSONObject()
            .put("condition", condition)
            .put(
                "detail",
                startFailure?.let { describeFailure(it) }
                    ?: "foreground-service start requested; node readiness is not observed here",
            )
            .put("requested_at_ms", System.currentTimeMillis())
            .put("installed_version_code", installedVersion)
            .put("installed_signer_sha256", installedSigner)
            .put("installed_bytes_verified", false)
        for (record in matching) {
            runCatching {
                record.put("restart", JSONObject(restart.toString()))
                InstallerRecords.persistAttempt(context, record)
            }
        }
        summary.put("condition", condition).put("detail", restart.optString("detail"))
        persistSummary(context, summary)
        if (startFailure != null) {
            Log.e(TAG, "successor start refused after package replacement", startFailure)
        }
    }

    private fun persistSummary(context: Context, summary: JSONObject) {
        runCatching { InstallerRecords.persistRestartSummary(context, summary) }
            .onFailure { failure -> Log.e(TAG, "restart summary not written: ${failure.message}") }
    }

    private companion object {
        const val TAG = "ShenkoRestart"
    }
}
