package org.shenko.node.activation

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.system.Os
import android.system.OsConstants
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest
import java.util.UUID

/**
 * A refused or failed installer operation.
 *
 * `kind` is one of the bridge protocol's stable error kinds and `condition`
 * is the machine-readable condition token. The bridge returns
 * `"<condition>: <message>"`, so a reader that only knows the protocol's
 * `error.kind`/`error.message` fields still receives the exact cause.
 *
 * This is a failure to perform the requested installer operation. It never
 * means an earlier session was abandoned or that the package is unchanged:
 * the durable attempt record is the only installation-state evidence.
 */
internal class InstallerFailure(
    val kind: String,
    val condition: String,
    message: String,
) : Exception(message) {
    init {
        require(kind in setOf("typed_unavailable", "denied", "io", "unsupported", "timeout", "internal")) {
            "unknown bridge error kind for an installer failure"
        }
    }

    fun detail(): String = "$condition: ${message ?: condition}"
}

internal fun failInstaller(kind: String, condition: String, message: String): Nothing =
    throw InstallerFailure(kind, condition, message)

internal fun describeFailure(error: Throwable): String =
    (error as? InstallerFailure)?.detail()
        ?: "${error.javaClass.simpleName}: ${error.message.orEmpty().take(512)}"

/** Read a nested `Intent` extra without relying on an API-33-only overload. */
@Suppress("DEPRECATION")
internal fun Intent.parcelableIntentExtra(name: String): Intent? =
    if (Build.VERSION.SDK_INT >= 33) {
        getParcelableExtra(name, Intent::class.java)
    } else {
        getParcelableExtra(name) as? Intent
    }

/**
 * Durable, app-private installer bookkeeping.
 *
 * Every attempt is one bounded JSON file under the app's private data
 * directory. Writes are created `0600` with `O_NOFOLLOW`/`O_EXCL`, fsynced,
 * atomically renamed and followed by a directory fsync, so a repeated request
 * after process death observes the same attempt instead of creating a second
 * install session.
 *
 * The record is evidence, not authority: it never writes the activation
 * selector, never advances an epoch and never admits a runtime owner.
 */
internal object InstallerRecords {
    /** Attempt-record schema; additive fields keep earlier records readable. */
    const val SCHEMA_VERSION = 2
    const val MAX_RECORD_BYTES = 64L * 1024L
    // Bounded so one 64 KiB record can never exceed its write bound even with
    // the maximum retained diagnostic status text.
    private const val MAX_EVENTS = 16
    private const val RESTART_SUMMARY = "android-package-restart.json"

    fun hex(bytes: ByteArray): String = bytes.joinToString("") { "%02x".format(it) }

    fun sha256Hex(text: String): String =
        hex(MessageDigest.getInstance("SHA-256").digest(text.toByteArray(Charsets.UTF_8)))

    fun attemptFile(context: Context, requestDigest: String): File {
        if (requestDigest.isEmpty() || requestDigest.length > 256) {
            failInstaller(
                "unsupported",
                "installer_request_invalid",
                "the installer request digest is empty or longer than 256 characters",
            )
        }
        return File(context.filesDir, "android-installer-${sha256Hex(requestDigest)}.json")
    }

    fun candidateDirectory(context: Context): File =
        File(context.filesDir, "installer-candidates")

    fun readAttempt(context: Context, requestDigest: String): JSONObject? {
        val file = attemptFile(context, requestDigest)
        if (!file.exists()) return null
        if (Files.isSymbolicLink(file.toPath()) ||
            !file.isFile ||
            file.length() !in 1L..MAX_RECORD_BYTES
        ) {
            failInstaller(
                "internal",
                "installer_record_invalid",
                "the installer attempt record is not a bounded regular app-private file",
            )
        }
        val record = try {
            JSONObject(file.readText(Charsets.UTF_8))
        } catch (failure: Exception) {
            failInstaller(
                "internal",
                "installer_record_invalid",
                "the installer attempt record is not valid JSON: ${failure.message.orEmpty().take(256)}",
            )
        }
        if (record.optInt("schema_version", -1) != SCHEMA_VERSION ||
            record.optString("request_digest") != requestDigest
        ) {
            failInstaller(
                "internal",
                "installer_record_invalid",
                "the installer attempt record has a different schema or request identity",
            )
        }
        return record
    }

    fun persistAttempt(context: Context, record: JSONObject) {
        val requestDigest = record.optString("request_digest")
        val target = attemptFile(context, requestDigest)
        record.put("schema_version", SCHEMA_VERSION)
        record.put("updated_at_ms", System.currentTimeMillis())
        val bytes = record.toString().toByteArray(Charsets.UTF_8)
        if (bytes.size > MAX_RECORD_BYTES) {
            failInstaller(
                "internal",
                "installer_record_invalid",
                "the installer attempt record exceeds its byte bound",
            )
        }
        atomicWrite0600(target, bytes)
    }

    /**
     * Create [target] as a fresh `0600` file, fsync it, rename it over the
     * destination and fsync the parent directory. A crash either leaves the
     * previous record or the complete new one.
     */
    fun atomicWrite0600(target: File, bytes: ByteArray) {
        val parent = target.parentFile
            ?: failInstaller("internal", "installer_record_invalid", "record target has no parent directory")
        val staging = File(parent, ".${target.name}.${UUID.randomUUID()}.tmp")
        try {
            val descriptor = Os.open(
                staging.absolutePath,
                OsConstants.O_CREAT or OsConstants.O_EXCL or
                    OsConstants.O_WRONLY or OsConstants.O_NOFOLLOW,
                0x180,
            )
            FileOutputStream(descriptor).use { output ->
                output.write(bytes)
                output.fd.sync()
            }
            Files.move(
                staging.toPath(),
                target.toPath(),
                StandardCopyOption.ATOMIC_MOVE,
                StandardCopyOption.REPLACE_EXISTING,
            )
        } catch (failure: InstallerFailure) {
            throw failure
        } catch (failure: IOException) {
            failInstaller(
                "io",
                "installer_io",
                "the installer record could not be written durably: ${failure.message.orEmpty().take(256)}",
            )
        } catch (failure: Exception) {
            failInstaller(
                "io",
                "installer_io",
                "the installer record could not be written durably: ${failure.message.orEmpty().take(256)}",
            )
        } finally {
            if (staging.exists()) runCatching { staging.delete() }
        }
        syncDirectory(parent)
    }

    fun syncDirectory(directory: File) {
        try {
            val descriptor = Os.open(
                directory.absolutePath,
                OsConstants.O_RDONLY or OsConstants.O_NOFOLLOW or OsConstants.O_NONBLOCK,
                0,
            )
            try {
                if (!OsConstants.S_ISDIR(Os.fstat(descriptor).st_mode)) {
                    failInstaller("io", "installer_io", "the installer record parent is not a directory")
                }
                Os.fsync(descriptor)
            } finally {
                Os.close(descriptor)
            }
        } catch (failure: InstallerFailure) {
            throw failure
        } catch (failure: Exception) {
            failInstaller(
                "io",
                "installer_io",
                "the installer record directory could not be fsynced: ${failure.message.orEmpty().take(256)}",
            )
        }
    }

    fun callbackUri(record: JSONObject): Uri =
        Uri.Builder()
            .scheme("shenko-installer")
            .authority("attempt")
            .appendPath(record.optString("attempt_id"))
            .build()

    /**
     * States after which this attempt will not progress without a new request.
     * `stream_failed_session_open` is deliberately not terminal: a session may
     * still be committed or abandoned by the system.
     */
    fun terminal(state: String): Boolean =
        state in setOf("success", "failure", "abandoned", "stream_failed")

    fun appendEvent(record: JSONObject, event: JSONObject) {
        val events = record.optJSONArray("events") ?: JSONArray().also { record.put("events", it) }
        events.put(event)
        while (events.length() > MAX_EVENTS) {
            events.remove(0)
        }
    }

    /** Attempt records, newest first. Unreadable records are skipped as facts. */
    fun listAttempts(context: Context): List<JSONObject> {
        val files = context.filesDir.listFiles { file ->
            file.isFile && file.name.startsWith("android-installer-") && file.name.endsWith(".json")
        } ?: return emptyList()
        val records = ArrayList<JSONObject>()
        for (file in files.take(256)) {
            if (Files.isSymbolicLink(file.toPath()) || file.length() !in 1L..MAX_RECORD_BYTES) continue
            val record = runCatching { JSONObject(file.readText(Charsets.UTF_8)) }.getOrNull() ?: continue
            if (record.optInt("schema_version", -1) != SCHEMA_VERSION) continue
            records.add(record)
        }
        records.sortByDescending { it.optLong("created_at_ms", 0) }
        return records
    }

    /**
     * Record an observed package replacement and what this process did about
     * it. The summary is written for every `MY_PACKAGE_REPLACED` delivery,
     * including replacements with no enrolled installer attempt.
     */
    fun persistRestartSummary(context: Context, summary: JSONObject) {
        summary.put("schema_version", 1)
        summary.put("recorded_at_ms", System.currentTimeMillis())
        val bytes = summary.toString().toByteArray(Charsets.UTF_8)
        if (bytes.size > MAX_RECORD_BYTES) {
            failInstaller("internal", "installer_record_invalid", "the restart summary exceeds its byte bound")
        }
        atomicWrite0600(File(context.filesDir, RESTART_SUMMARY), bytes)
    }
}
