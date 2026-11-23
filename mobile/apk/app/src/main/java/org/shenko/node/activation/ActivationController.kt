package org.shenko.node.activation

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.os.Build
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.security.MessageDigest
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/**
 * The ordinary same-package replacement session.
 *
 * This controller creates exactly one `PackageInstaller` session for one
 * frozen request, streams the verified bytes into it, commits it with a
 * correlated status callback and records the system's answer durably. It
 * never writes the activation selector, advances an epoch, starts a
 * replacement runtime or claims node readiness, and it needs no root,
 * Shizuku or Dhizuku grant.
 *
 * Android keeps authority over installation permission and user
 * confirmation. The confirmation is surfaced (activity or notification) and
 * never confirmed on the user's behalf.
 */
object ActivationController {
    const val EXTRA_STATUS = "org.shenko.node.activation.STATUS"
    const val EXTRA_ATTEMPT = "org.shenko.node.activation.ATTEMPT"
    const val EXTRA_INSTALL_INTENT = "org.shenko.node.activation.INSTALL_INTENT"

    private const val MAX_REQUEST_DIGEST = 256
    private const val MAX_OPEN_SESSIONS = 64
    private val offeredConfirmations = ConcurrentHashMap<String, Intent>()

    /**
     * Request installation of an already-frozen candidate.
     *
     * The durable attempt is written before `createSession`. Repeating the
     * same request observes that attempt and never creates a second session.
     * A different expectation under the same request digest is refused.
     */
    @Synchronized
    fun install(
        context: Context,
        requestDigest: String,
        candidateApkPath: String,
        expectedPackage: String,
        expectedVersionCode: Long,
        expectedSignerSha256: String,
        expectedApkSha256: String,
        expectedBytes: Long,
        expectedNativeSha256: String,
    ): JSONObject {
        validateRequest(
            requestDigest,
            expectedPackage,
            expectedVersionCode,
            expectedSignerSha256,
            expectedApkSha256,
            expectedBytes,
            expectedNativeSha256,
        )
        val existing = InstallerRecords.readAttempt(context, requestDigest)
        var resumable: JSONObject? = null
        if (existing != null) {
            requireIdentity(
                existing,
                expectedPackage,
                expectedVersionCode,
                expectedSignerSha256,
                expectedApkSha256,
                expectedBytes,
                expectedNativeSha256,
            )
            // "verified" means the record was written but no
            // createSession call was started, so this attempt may continue.
            // Any other state (including "creating_session") cannot prove
            // that no session exists and stays Unknown.
            if (existing.optInt("session_id", -1) >= 0 || existing.optString("state") != "verified") {
                return statusLocked(context, requestDigest)
            }
            resumable = existing
        }
        if (!context.packageManager.canRequestPackageInstalls()) {
            failInstaller(
                "denied",
                "installer_permission_denied",
                "allow this app to request installation in Android settings",
            )
        }
        val candidate = CandidatePackage.verify(
            context,
            candidateApkPath,
            expectedPackage,
            expectedVersionCode,
            expectedSignerSha256,
            expectedApkSha256,
            expectedBytes,
            expectedNativeSha256,
        )
        val installer = context.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
        params.setAppPackageName(candidate.facts.packageName)
        params.setSize(expectedBytes)
        params.setInstallReason(PackageManager.INSTALL_REASON_USER)
        if (Build.VERSION.SDK_INT >= 31) {
            // The confirmation is the system's, not ours. Asking for a user
            // action is the opposite of confirming silently.
            params.setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_REQUIRED)
        }

        val attempt = resumable ?: JSONObject()
            .put("schema_version", InstallerRecords.SCHEMA_VERSION)
            .put("request_digest", requestDigest)
            .put("attempt_id", UUID.randomUUID().toString())
            .put("package", expectedPackage)
            .put("version_code", expectedVersionCode)
            .put("signer_sha256", expectedSignerSha256)
            .put("apk_sha256", expectedApkSha256)
            .put("apk_bytes", expectedBytes)
            .put("native_sha256", expectedNativeSha256)
            .put("candidate_abi", candidate.abi)
            .put("state", "verified")
            .put("created_at_ms", System.currentTimeMillis())
            .put("events", JSONArray())
        InstallerRecords.persistAttempt(context, attempt)
        // Record the attempt to create a session *before* the call: a crash
        // between createSession and the session-ID write must not leave a
        // resumable-looking record for a session that may already exist.
        attempt.put("state", "creating_session")
        InstallerRecords.persistAttempt(context, attempt)

        val sessionId = try {
            installer.createSession(params)
        } catch (failure: Exception) {
            // createSession may have created a session the binder never
            // returned. Without a session ID that session is unenrolled, so
            // this attempt stays Unknown instead of adopting it by package.
            attempt.put("state", "session_enrollment_unknown")
                .put("session_create_error", describeFailure(failure))
            InstallerRecords.persistAttempt(context, attempt)
            failInstaller(
                "io",
                "installer_session_create_failed",
                "the PackageInstaller session could not be created; any session it did create is unenrolled",
            )
        }
        attempt.put("session_id", sessionId).put("state", "streaming")
        InstallerRecords.persistAttempt(context, attempt)

        var commitAttempted = false
        var committed = false
        try {
            installer.openSession(sessionId).use { session ->
                val (streamedSha256, streamedBytes) = streamCandidate(candidate.facts.apkPath, session, expectedBytes)
                if (streamedSha256 != expectedApkSha256) {
                    failInstaller(
                        "denied",
                        "installer_candidate_digest_mismatch",
                        "the bytes streamed into the install session differ from the frozen digest",
                    )
                }
                attempt.put("streamed_apk_sha256", streamedSha256)
                    .put("streamed_apk_bytes", streamedBytes)
                val callback = Intent(context, InstallerStatusReceiver::class.java)
                    .setData(InstallerRecords.callbackUri(attempt))
                    .putExtra(EXTRA_STATUS, requestDigest)
                    .putExtra(EXTRA_ATTEMPT, attempt.getString("attempt_id"))
                    .putExtra("session_id", sessionId)
                val callbackFlags = PendingIntent.FLAG_CANCEL_CURRENT or
                    (if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0)
                val pending = PendingIntent.getBroadcast(
                    context,
                    sessionId,
                    callback,
                    callbackFlags,
                )
                // The system's answer after this point is observed, never
                // replayed: a crash here leaves the attempt as Unknown.
                attempt.put("state", "commit_requested")
                InstallerRecords.persistAttempt(context, attempt)
                commitAttempted = true
                session.commit(pending.intentSender)
                committed = true
            }
        } catch (failure: Throwable) {
            if (!committed && !commitAttempted) {
                val abandoned = abandonExact(context, installer, sessionId, attempt)
                attempt.put("state", if (abandoned) "stream_failed" else "stream_failed_session_open")
                if (failure is InstallerFailure) {
                    attempt.put("failure_condition", failure.condition)
                }
                InstallerRecords.persistAttempt(context, attempt)
            }
            when (failure) {
                is InstallerFailure -> throw failure
                is Exception -> failInstaller(
                    "io",
                    if (commitAttempted) "installer_commit_unknown" else "installer_stream_failed",
                    "the install session did not complete: ${failure.message.orEmpty().take(256)}",
                )
                else -> throw failure
            }
        }
        return statusLocked(context, requestDigest)
    }

    /**
     * Read the durable installer status. This never creates or adopts a
     * session and does not take the install lock, so a status observation is
     * not blocked for the duration of a large stream; the durable state and
     * the session list may be read a few milliseconds apart, and each field
     * remains a fact from its own read.
     */
    fun status(context: Context, requestDigest: String): JSONObject = statusLocked(context, requestDigest)

    /**
     * Ask the system to abandon only the exact enrolled session.
     *
     * Absence of a session, or a status callback that never arrived, cannot
     * distinguish a completed commit from abandonment; both stay Unknown.
     */
    @Synchronized
    fun abandon(context: Context, requestDigest: String): JSONObject {
        val record = InstallerRecords.readAttempt(context, requestDigest)
            ?: return unknownStatus(context, requestDigest)
        val sessionId = record.optInt("session_id", -1)
        if (sessionId < 0 || record.optString("state") == "success") {
            return statusLocked(context, requestDigest)
        }
        val installer = context.packageManager.packageInstaller
        val owned = try {
            ownedSession(installer, context, sessionId, record.optString("package"))
        } catch (failure: Exception) {
            failInstaller("io", "installer_session_query_failed", "the PackageInstaller session list is unavailable")
        }
        if (owned != null) {
            record.put("abandon_requested_at_ms", System.currentTimeMillis())
            InstallerRecords.persistAttempt(context, record)
            try {
                installer.abandonSession(sessionId)
            } catch (failure: Exception) {
                failInstaller(
                    "io",
                    "installer_abandon_failed",
                    "the system refused to abandon the enrolled session: ${failure.message.orEmpty().take(256)}",
                )
            }
        }
        return statusLocked(context, requestDigest)
    }

    /**
     * Accept only a callback for this exact enrolled attempt and OS session.
     *
     * The receiver is not exported, the callback URI carries the attempt ID
     * this app generated, and the PendingIntent is app-created. An
     * unanswered or unenrolled session is never attributed by package name.
     */
    @Synchronized
    fun recordEvent(context: Context, intent: Intent): Boolean {
        val requestDigest = intent.getStringExtra(EXTRA_STATUS) ?: return false
        val record = try {
            InstallerRecords.readAttempt(context, requestDigest)
        } catch (failure: InstallerFailure) {
            return false
        } ?: return false
        val sessionId = record.optInt("session_id", -1)
        if (sessionId < 0 || InstallerRecords.terminal(record.optString("state"))) return false
        if (intent.data != InstallerRecords.callbackUri(record)) return false
        if (intent.getStringExtra(EXTRA_ATTEMPT) != record.optString("attempt_id")) return false
        if (intent.getIntExtra("session_id", -1) != sessionId) return false
        val packageSession = intent.getIntExtra(PackageInstaller.EXTRA_SESSION_ID, -1)
        // The OS extra is a bonus correlation when the platform fills it; the
        // app-generated URI, attempt ID and session extra are already bound.
        if (packageSession > 0 && packageSession != sessionId) return false
        val sessions = try {
            context.packageManager.packageInstaller.mySessions
        } catch (failure: Exception) {
            return false
        }
        val owned = sessions.firstOrNull {
            isOwnedSession(it, context, sessionId, record.optString("package"))
        }
        if (sessions.any { it.sessionId == sessionId } && owned == null) return false
        val statusCode = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, Int.MIN_VALUE)
        val state = when (statusCode) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> "pending_user_action"
            PackageInstaller.STATUS_SUCCESS -> "success"
            PackageInstaller.STATUS_FAILURE_ABORTED -> "abandoned"
            PackageInstaller.STATUS_FAILURE,
            PackageInstaller.STATUS_FAILURE_BLOCKED,
            PackageInstaller.STATUS_FAILURE_CONFLICT,
            PackageInstaller.STATUS_FAILURE_INCOMPATIBLE,
            PackageInstaller.STATUS_FAILURE_INVALID,
            PackageInstaller.STATUS_FAILURE_STORAGE -> "failure"
            else -> null
        }
        val event = JSONObject()
            .put("status_code", statusCode)
            .put("status_message", intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE).orEmpty().take(1024))
            .put("observed_at_ms", System.currentTimeMillis())
        if (state == null) {
            InstallerRecords.appendEvent(record, event.put("status", "unrecognized_status"))
            record.put("last_unrecognized_status_code", statusCode)
            InstallerRecords.persistAttempt(context, record)
            return false
        }
        InstallerRecords.appendEvent(record, event.put("status", state))
        record.put("state", state).put("last_status_code", statusCode)
        if (owned == null && state == "pending_user_action") {
            // The enrolled session vanished without a terminal callback; the
            // pending answer is still the system's, so recording it is honest.
            record.put("session_absent_at_pending", true)
        }
        InstallerRecords.persistAttempt(context, record)
        return true
    }

    @Synchronized
    fun confirmationDelivery(context: Context, requestDigest: String, delivery: String) {
        val record = try {
            InstallerRecords.readAttempt(context, requestDigest)
        } catch (failure: InstallerFailure) {
            null
        } ?: return
        record.put("confirmation_delivery", delivery.take(512))
        InstallerRecords.persistAttempt(context, record)
    }

    /** Hold the system confirmation for the foreground activity, if any. */
    fun offerConfirmation(requestDigest: String, confirmation: Intent) {
        if (offeredConfirmations.size >= 4) offeredConfirmations.clear()
        offeredConfirmations[requestDigest] = Intent(confirmation)
    }

    /** The still-held confirmation, without consuming it. */
    fun offeredConfirmation(requestDigest: String): Intent? =
        offeredConfirmations[requestDigest]?.let { Intent(it) }

    fun takeOfferedConfirmation(requestDigest: String): Intent? =
        offeredConfirmations.remove(requestDigest)

    /** Build the launch Intent for the installer activity's confirmation entry. */
    @Synchronized
    fun confirmationLaunch(context: Context, requestDigest: String, confirmation: Intent): Intent {
        val record = InstallerRecords.readAttempt(context, requestDigest)
            ?: failInstaller("internal", "installer_record_invalid", "the installer attempt is absent")
        if (record.optString("state") != "pending_user_action") {
            failInstaller("denied", "installer_confirmation_stale", "the system confirmation no longer matches an enrolled pending attempt")
        }
        return Intent(context, InstallerActivity::class.java)
            .setData(InstallerRecords.callbackUri(record))
            .putExtra(EXTRA_STATUS, requestDigest)
            .putExtra(EXTRA_ATTEMPT, record.optString("attempt_id"))
            .putExtra(EXTRA_INSTALL_INTENT, confirmation)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
    }

    /**
     * Take the system confirmation Intent out of a validated launch.
     *
     * Only this app's non-exported activity receives this Intent, and the
     * attempt, callback URI and pending state must all agree with the durable
     * record. The confirmation itself is the system's installer Intent.
     */
    @Synchronized
    fun takeConfirmation(context: Context, launch: Intent): Intent? {
        if (!launch.hasExtra(EXTRA_INSTALL_INTENT)) return null
        val requestDigest = launch.getStringExtra(EXTRA_STATUS)
            ?: failInstaller("internal", "installer_confirmation_stale", "the confirmation launch has no request identity")
        val record = InstallerRecords.readAttempt(context, requestDigest)
            ?: failInstaller("internal", "installer_confirmation_stale", "the confirmation launch names no enrolled attempt")
        if (record.optString("state") != "pending_user_action" ||
            launch.data != InstallerRecords.callbackUri(record) ||
            launch.getStringExtra(EXTRA_ATTEMPT) != record.optString("attempt_id")
        ) {
            failInstaller("denied", "installer_confirmation_stale", "the confirmation launch does not match the enrolled pending attempt")
        }
        val confirmation = launch.parcelableIntentExtra(EXTRA_INSTALL_INTENT)
            ?: failInstaller("internal", "installer_confirmation_stale", "the system confirmation Intent is missing")
        record.put("confirmation_delivery", "activity_open_requested")
        InstallerRecords.persistAttempt(context, record)
        return confirmation
    }

    /** Enumerate this installer package's OS sessions as facts, not results. */
    @Synchronized
    fun sessionState(context: Context): JSONObject {
        val sessions = try {
            context.packageManager.packageInstaller.mySessions
        } catch (failure: Exception) {
            failInstaller("io", "installer_session_query_failed", "the PackageInstaller session list is unavailable")
        }
        val array = JSONArray()
        for (session in sessions.take(MAX_OPEN_SESSIONS)) {
            array.put(
                JSONObject()
                    .put("session_id", session.sessionId)
                    .put("app_package", session.appPackageName ?: JSONObject.NULL)
                    .put("installer_package", session.installerPackageName ?: JSONObject.NULL)
                    .put("is_active", session.isActive)
                    .put("size_bytes", session.sizeBytes),
            )
        }
        return JSONObject()
            .put("ok", true)
            .put("active", sessions.isNotEmpty())
            .put("sessions", array)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    private fun statusLocked(context: Context, requestDigest: String): JSONObject {
        val record = InstallerRecords.readAttempt(context, requestDigest)
            ?: return unknownStatus(context, requestDigest)
        val sessions = try {
            context.packageManager.packageInstaller.mySessions
        } catch (failure: Exception) {
            failInstaller("io", "installer_session_query_failed", "the PackageInstaller session list is unavailable")
        }
        val sessionId = record.optInt("session_id", -1)
        val owned = if (sessionId >= 0) {
            sessions.firstOrNull {
                isOwnedSession(it, context, sessionId, record.optString("package"))
            }
        } else {
            null
        }
        val state = record.optString("state", "session_enrollment_unknown")
        val terminal = InstallerRecords.terminal(state) && owned == null
        val openIds = JSONArray()
        for (session in sessions.take(MAX_OPEN_SESSIONS)) openIds.put(session.sessionId)
        return JSONObject()
            .put("ok", true)
            .put("attempt_enrolled", true)
            .put("state", state)
            .put("session_id", if (sessionId >= 0) sessionId else JSONObject.NULL)
            .put("attempt_id", record.optString("attempt_id"))
            .put("request_digest", requestDigest)
            .put("package", record.optString("package"))
            .put("version_code", record.optLong("version_code", -1))
            .put("session_actionable", owned != null)
            .put("terminal_confirmed", terminal)
            .put("outcome_unknown", !terminal)
            .put("open_session_ids", openIds)
            .put("confirmation_delivery", record.optString("confirmation_delivery", "none"))
            .put("restart", record.optJSONObject("restart") ?: JSONObject.NULL)
    }

    private fun unknownStatus(context: Context, requestDigest: String): JSONObject {
        val openIds = JSONArray()
        try {
            for (session in context.packageManager.packageInstaller.mySessions.take(MAX_OPEN_SESSIONS)) {
                openIds.put(session.sessionId)
            }
        } catch (failure: Exception) {
            // The unknown answer does not depend on enumerating sessions.
        }
        return JSONObject()
            .put("ok", true)
            .put("attempt_enrolled", false)
            .put("state", "installer_outcome_unknown")
            .put("request_digest", requestDigest)
            .put("session_id", JSONObject.NULL)
            .put("terminal_confirmed", false)
            .put("outcome_unknown", true)
            .put("session_actionable", false)
            .put("open_session_ids", openIds)
    }

    private fun validateRequest(
        requestDigest: String,
        expectedPackage: String,
        expectedVersionCode: Long,
        expectedSignerSha256: String,
        expectedApkSha256: String,
        expectedBytes: Long,
        expectedNativeSha256: String,
    ) {
        if (requestDigest.isEmpty() || requestDigest.length > MAX_REQUEST_DIGEST) {
            failInstaller("unsupported", "installer_request_invalid", "the installer request digest is empty or longer than 256 characters")
        }
        if (expectedPackage.isEmpty() || expectedPackage.length > 255 || expectedPackage.contains('/')) {
            failInstaller("unsupported", "installer_package_mismatch", "the expected package name is malformed")
        }
        if (expectedVersionCode <= 0) {
            failInstaller("unsupported", "installer_version_mismatch", "the expected version code is not positive")
        }
        if (expectedSignerSha256.isEmpty() || expectedSignerSha256.length > 1024) {
            failInstaller("unsupported", "installer_signer_mismatch", "the expected signer digest is empty or too long")
        }
        if (expectedApkSha256.length != 64 || expectedApkSha256.any { it !in '0'..'9' && it !in 'a'..'f' }) {
            failInstaller("unsupported", "installer_candidate_digest_mismatch", "the expected APK digest is not 64 lowercase hexadecimal characters")
        }
        if (expectedBytes !in 1L..CandidatePackage.MAX_APK_BYTES) {
            failInstaller("unsupported", "installer_candidate_oversized", "the expected package size is outside 1 byte..1 GiB")
        }
        CandidatePackage.parseNativeManifest(expectedNativeSha256)
    }

    private fun requireIdentity(
        record: JSONObject,
        expectedPackage: String,
        expectedVersionCode: Long,
        expectedSignerSha256: String,
        expectedApkSha256: String,
        expectedBytes: Long,
        expectedNativeSha256: String,
    ) {
        if (record.optString("package") != expectedPackage ||
            record.optLong("version_code", -1) != expectedVersionCode ||
            record.optString("signer_sha256") != expectedSignerSha256 ||
            record.optString("apk_sha256") != expectedApkSha256 ||
            record.optLong("apk_bytes", -1) != expectedBytes ||
            record.optString("native_sha256") != expectedNativeSha256
        ) {
            failInstaller(
                "denied",
                "installer_attempt_identity_conflict",
                "the request digest is already enrolled for a different frozen package identity",
            )
        }
    }

    private fun ownedSession(
        installer: PackageInstaller,
        context: Context,
        sessionId: Int,
        packageName: String,
    ): PackageInstaller.SessionInfo? {
        if (sessionId < 0) return null
        return installer.mySessions.firstOrNull { isOwnedSession(it, context, sessionId, packageName) }
    }

    /**
     * The exact enrolled OS session. A reported null app package is not proof
     * that the session belongs to another package; a reported different one
     * is proof that it is not ours.
     */
    private fun isOwnedSession(
        info: PackageInstaller.SessionInfo,
        context: Context,
        sessionId: Int,
        packageName: String,
    ): Boolean =
        info.sessionId == sessionId &&
            info.installerPackageName == context.packageName &&
            (info.appPackageName == null || info.appPackageName == packageName)

    private fun abandonExact(
        context: Context,
        installer: PackageInstaller,
        sessionId: Int,
        record: JSONObject,
    ): Boolean {
        return try {
            if (ownedSession(installer, context, sessionId, record.optString("package")) == null) {
                false
            } else {
                installer.abandonSession(sessionId)
                installer.mySessions.none { it.sessionId == sessionId }
            }
        } catch (failure: Exception) {
            false
        }
    }

    private fun streamCandidate(
        candidatePath: String,
        session: PackageInstaller.Session,
        expectedBytes: Long,
    ): Pair<String, Long> {
        val candidate = File(candidatePath)
        if (!candidate.isFile) {
            failInstaller("io", "installer_candidate_absent", "the verified candidate file disappeared before it was streamed")
        }
        candidate.inputStream().use { input ->
            session.openWrite("shenko.apk", 0, expectedBytes).use { output ->
                val digest = MessageDigest.getInstance("SHA-256")
                val buffer = ByteArray(1 shl 20)
                var length = 0L
                while (true) {
                    val count = input.read(buffer)
                    if (count < 0) break
                    length += count
                    if (length > expectedBytes) {
                        failInstaller(
                            "denied",
                            "installer_candidate_digest_mismatch",
                            "the candidate grew while it was streamed into the install session",
                        )
                    }
                    digest.update(buffer, 0, count)
                    output.write(buffer, 0, count)
                }
                if (length != expectedBytes) {
                    failInstaller(
                        "denied",
                        "installer_candidate_digest_mismatch",
                        "the candidate length changed while it was streamed into the install session",
                    )
                }
                val sha256 = InstallerRecords.hex(digest.digest())
                session.fsync(output)
                return sha256 to length
            }
        }
    }
}
