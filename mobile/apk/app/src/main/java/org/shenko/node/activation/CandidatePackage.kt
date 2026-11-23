package org.shenko.node.activation

import android.content.Context
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.system.Os
import android.system.OsConstants
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest
import java.util.UUID
import java.util.zip.ZipFile

/**
 * Verified facts about one candidate package. The bytes named by [apkPath]
 * are the bytes an install session streams; nothing here writes activation
 * authority.
 */
internal object CandidatePackage {
    const val MAX_APK_BYTES = 1L shl 30
    private const val MAX_NATIVE_BYTES = 256L shl 20
    private const val MAX_NATIVE_MANIFEST = 4096
    private const val SELECTION = "android-installer-selection.json"
    private const val SELECTION_SCHEMA = 1
    data class Facts(
        val apkPath: String,
        val packageName: String,
        val versionCode: Long,
        val signerSha256: String,
        val apkSha256: String,
        val apkBytes: Long,
        val nativeSha256: String,
    )

    data class Candidate(
        val facts: Facts,
        val abi: String,
        val nativeSha256ForAbi: String,
    )

    fun signatureFlags(): Int =
        if (Build.VERSION.SDK_INT >= 28) {
            PackageManager.GET_SIGNING_CERTIFICATES
        } else {
            PackageManager.GET_SIGNATURES
        }

    @Suppress("DEPRECATION")
    fun versionCode(info: PackageInfo): Long =
        if (Build.VERSION.SDK_INT >= 28) info.longVersionCode else info.versionCode.toLong()

    @Suppress("DEPRECATION")
    fun signer(info: PackageInfo): String {
        val signatures = if (Build.VERSION.SDK_INT >= 28) {
            info.signingInfo?.apkContentsSigners
        } else {
            info.signatures
        }
        if (signatures.isNullOrEmpty()) {
            failInstaller(
                "denied",
                "installer_signer_mismatch",
                "the package signing identity is unavailable, so same-signer replacement is not proved",
            )
        }
        return signatures.map {
            InstallerRecords.hex(MessageDigest.getInstance("SHA-256").digest(it.toByteArray()))
        }.sorted().joinToString(":")
    }

    fun installed(context: Context): PackageInfo =
        try {
            context.packageManager.getPackageInfo(context.packageName, signatureFlags())
        } catch (failure: PackageManager.NameNotFoundException) {
            failInstaller("internal", "installer_io", "this package's own installed identity is unavailable")
        }

    /**
     * Resolve a caller-supplied path to a canonical file inside this app's
     * private data directory. A symlink, a path outside `filesDir` or a
     * non-regular file is refused before any byte is read.
     */
    fun privateCandidate(context: Context, path: String): File {
        if (path.isEmpty() || path.length > 4096) {
            failInstaller("unsupported", "installer_candidate_absent", "the candidate package path is empty or absurdly long")
        }
        val supplied = File(path)
        if (Files.isSymbolicLink(supplied.toPath())) {
            failInstaller("denied", "installer_candidate_not_private", "the candidate package path is a symbolic link")
        }
        val root = try {
            context.filesDir.canonicalFile.toPath()
        } catch (failure: IOException) {
            failInstaller("io", "installer_io", "this app's private data directory cannot be resolved")
        }
        val canonical = try {
            supplied.canonicalFile
        } catch (failure: IOException) {
            failInstaller("io", "installer_candidate_absent", "the candidate package path cannot be resolved")
        }
        if (!canonical.toPath().startsWith(root) || canonical.toPath() == root) {
            failInstaller(
                "denied",
                "installer_candidate_not_private",
                "the candidate package must already be a file inside this app's private data directory",
            )
        }
        if (!canonical.isFile) {
            failInstaller("io", "installer_candidate_absent", "the candidate package file is absent")
        }
        return canonical
    }

    /** Fact-only inspection of private candidate bytes, for the local chooser. */
    fun inspect(context: Context, path: String): Facts {
        val file = privateCandidate(context, path)
        val length = file.length()
        if (length !in 1L..MAX_APK_BYTES) {
            failInstaller("denied", "installer_candidate_oversized", "the candidate package is empty or larger than 1 GiB")
        }
        val archive = archiveInfo(context, file)
        val (apkSha256, bytes) = digestFile(file, length)
        val native = Build.SUPPORTED_ABIS.mapNotNull { abi ->
            memberDigest(file, abi)?.let { digest -> "$abi=$digest" }
        }.joinToString(",")
        if (native.isEmpty()) {
            failInstaller(
                "denied",
                "installer_abi_missing",
                "the candidate ships no libshenko_android.so member for an ABI this device supports",
            )
        }
        return Facts(
            apkPath = file.absolutePath,
            packageName = archive.packageName,
            versionCode = versionCode(archive),
            signerSha256 = signer(archive),
            apkSha256 = apkSha256,
            apkBytes = bytes,
            nativeSha256 = native,
        )
    }

    /**
     * Verify the frozen identity of a candidate before a session exists.
     *
     * Every refusal happens here, so a foreign signer, a downgrade, a wrong
     * package or a package without a loadable native member never reaches
     * `PackageInstaller.createSession`.
     */
    fun verify(
        context: Context,
        path: String,
        expectedPackage: String,
        expectedVersionCode: Long,
        expectedSignerSha256: String,
        expectedApkSha256: String,
        expectedBytes: Long,
        expectedNativeSha256: String,
    ): Candidate {
        val file = privateCandidate(context, path)
        if (expectedPackage.isEmpty() || expectedPackage.length > 255) {
            failInstaller("unsupported", "installer_package_mismatch", "the expected package name is empty or too long")
        }
        if (expectedVersionCode <= 0) {
            failInstaller("unsupported", "installer_version_mismatch", "the expected version code is not positive")
        }
        requireDigest("installer_candidate_digest_mismatch", expectedApkSha256)
        if (expectedSignerSha256.isEmpty() || expectedSignerSha256.length > 1024) {
            failInstaller("unsupported", "installer_signer_mismatch", "the expected signer digest is empty or too long")
        }
        if (expectedBytes !in 1L..MAX_APK_BYTES) {
            failInstaller("unsupported", "installer_candidate_oversized", "the expected package size is outside 1 byte..1 GiB")
        }
        if (file.length() != expectedBytes) {
            failInstaller("denied", "installer_candidate_digest_mismatch", "the candidate length differs from the frozen length")
        }
        val installedInfo = installed(context)
        val archive = archiveInfo(context, file)
        if (archive.packageName != expectedPackage || archive.packageName != context.packageName) {
            failInstaller("denied", "installer_package_mismatch", "the candidate is not this package")
        }
        val candidateVersion = versionCode(archive)
        if (candidateVersion != expectedVersionCode) {
            failInstaller("denied", "installer_version_mismatch", "the candidate version code differs from the frozen version")
        }
        if (candidateVersion <= versionCode(installedInfo)) {
            failInstaller("denied", "installer_version_not_advanced", "the candidate version code does not advance the installed version")
        }
        val candidateSigner = signer(archive)
        if (candidateSigner != expectedSignerSha256) {
            failInstaller("denied", "installer_signer_mismatch", "the candidate signer differs from the frozen signer")
        }
        val installedSigner = signer(installedInfo)
        if (candidateSigner != installedSigner) {
            failInstaller("denied", "installer_signer_mismatch", "the candidate signer differs from the installed signer")
        }
        val expectedByAbi = parseNativeManifest(expectedNativeSha256)
        val abi = Build.SUPPORTED_ABIS.firstOrNull { expectedByAbi.containsKey(it) }
            ?: failInstaller(
                "denied",
                "installer_abi_missing",
                "the candidate ships none of the ABIs this device would load",
            )
        val (apkSha256, bytes) = digestFile(file, expectedBytes)
        if (apkSha256 != expectedApkSha256) {
            failInstaller("denied", "installer_candidate_digest_mismatch", "the candidate APK digest differs from the frozen digest")
        }
        val native = memberDigest(file, abi)
            ?: failInstaller("denied", "installer_abi_missing", "the candidate has no $abi libshenko_android.so member")
        if (native != expectedByAbi.getValue(abi)) {
            failInstaller("denied", "installer_native_mismatch", "the candidate $abi native member digest differs from the frozen digest")
        }
        return Candidate(
            facts = Facts(
                apkPath = file.absolutePath,
                packageName = archive.packageName,
                versionCode = candidateVersion,
                signerSha256 = candidateSigner,
                apkSha256 = apkSha256,
                apkBytes = bytes,
                nativeSha256 = expectedNativeSha256,
            ),
            abi = abi,
            nativeSha256ForAbi = native,
        )
    }

    fun parseNativeManifest(manifest: String): Map<String, String> {
        if (manifest.isEmpty() || manifest.length > MAX_NATIVE_MANIFEST) {
            failInstaller("unsupported", "installer_native_manifest_invalid", "the expected native digest manifest is empty or oversized")
        }
        val entries = LinkedHashMap<String, String>()
        for (entry in manifest.split(",")) {
            val separator = entry.indexOf('=')
            if (separator <= 0 || separator != entry.lastIndexOf('=')) {
                failInstaller("unsupported", "installer_native_manifest_invalid", "the expected native digest manifest is not abi=sha256 entries")
            }
            val abi = entry.substring(0, separator)
            val digest = entry.substring(separator + 1)
            if (abi.length > 32 || abi.any { !(it.isLetterOrDigit() || it == '-' || it == '_') }) {
                failInstaller("unsupported", "installer_native_manifest_invalid", "the expected native ABI name is malformed")
            }
            requireDigest("installer_native_manifest_invalid", digest)
            if (entries.put(abi, digest) != null) {
                failInstaller("unsupported", "installer_native_manifest_invalid", "the expected native manifest repeats an ABI")
            }
        }
        return entries
    }

    /**
     * Copy one document into a fresh `0600` private candidate file.
     *
     * A content-provider URI is consumed once, here. No persistent URI grant,
     * external path, shared-storage file or world-readable copy is used, and
     * the returned facts describe the bytes actually stored.
     */
    @Synchronized
    fun stage(context: Context, uri: Uri): Facts {
        val directory = InstallerRecords.candidateDirectory(context)
        if (!directory.isDirectory && !directory.mkdirs()) {
            failInstaller("io", "installer_io", "the private candidate directory cannot be created")
        }
        val staging = File(directory, ".stage-${UUID.randomUUID()}")
        try {
            val input = try {
                context.contentResolver.openInputStream(uri)
            } catch (failure: Exception) {
                failInstaller("io", "installer_candidate_absent", "the selected document cannot be opened: ${failure.message.orEmpty().take(256)}")
            } ?: failInstaller("io", "installer_candidate_absent", "the selected document has no readable stream")
            input.use { source ->
                val descriptor = Os.open(
                    staging.absolutePath,
                    OsConstants.O_CREAT or OsConstants.O_EXCL or
                        OsConstants.O_WRONLY or OsConstants.O_NOFOLLOW,
                    0x180,
                )
                FileOutputStream(descriptor).use { output ->
                    val buffer = ByteArray(1 shl 20)
                    var bytes = 0L
                    while (true) {
                        val count = source.read(buffer)
                        if (count < 0) break
                        bytes += count
                        if (bytes > MAX_APK_BYTES) {
                            failInstaller("denied", "installer_candidate_oversized", "the selected package is larger than 1 GiB")
                        }
                        output.write(buffer, 0, count)
                    }
                    if (bytes == 0L) {
                        failInstaller("io", "installer_candidate_absent", "the selected document is empty")
                    }
                    output.fd.sync()
                }
            }
            val staged = inspect(context, staging.absolutePath)
            val target = File(directory, "${staged.apkSha256}.apk")
            if ((target.exists() || Files.isSymbolicLink(target.toPath())) && !target.delete()) {
                failInstaller("io", "installer_io", "the previous identical candidate file cannot be replaced")
            }
            Files.move(staging.toPath(), target.toPath(), StandardCopyOption.ATOMIC_MOVE)
            InstallerRecords.syncDirectory(directory)
            removeOtherCandidates(directory, target.name)
            return staged.copy(apkPath = target.canonicalPath)
        } catch (failure: InstallerFailure) {
            throw failure
        } catch (failure: IOException) {
            failInstaller("io", "installer_io", "the selected package could not be stored privately: ${failure.message.orEmpty().take(256)}")
        } catch (failure: Exception) {
            failInstaller("io", "installer_io", "the selected package could not be staged: ${failure.message.orEmpty().take(256)}")
        } finally {
            if (staging.exists()) runCatching { staging.delete() }
        }
    }

    private fun removeOtherCandidates(directory: File, keep: String) {
        val files = directory.listFiles() ?: return
        for (file in files) {
            if (file.name == keep) continue
            if (Files.isSymbolicLink(file.toPath())) {
                runCatching { file.delete() }
            } else if (file.isFile) {
                runCatching { file.delete() }
            }
        }
    }

    private fun archiveInfo(context: Context, file: File): PackageInfo {
        val info = try {
            context.packageManager.getPackageArchiveInfo(file.absolutePath, signatureFlags())
        } catch (failure: Exception) {
            failInstaller("internal", "installer_io", "the candidate archive cannot be inspected: ${failure.message.orEmpty().take(256)}")
        } ?: failInstaller("denied", "installer_package_mismatch", "the candidate file has no Android package identity")
        return info
    }

    private fun digestFile(file: File, expectedBytes: Long): Pair<String, Long> {
        return try {
            file.inputStream().use { input ->
                val digest = MessageDigest.getInstance("SHA-256")
                val buffer = ByteArray(1 shl 20)
                var length = 0L
                while (true) {
                    val count = input.read(buffer)
                    if (count < 0) break
                    length += count
                    if (length > expectedBytes) {
                        failInstaller("denied", "installer_candidate_digest_mismatch", "the candidate grew while it was read")
                    }
                    digest.update(buffer, 0, count)
                }
                if (length != expectedBytes) {
                    failInstaller("denied", "installer_candidate_digest_mismatch", "the candidate length changed while it was read")
                }
                InstallerRecords.hex(digest.digest()) to length
            }
        } catch (failure: InstallerFailure) {
            throw failure
        } catch (failure: IOException) {
            failInstaller("io", "installer_candidate_absent", "the candidate bytes cannot be read: ${failure.message.orEmpty().take(256)}")
        }
    }

    private fun memberDigest(file: File, abi: String): String? {
        return try {
            ZipFile(file).use { archive ->
                val entry = archive.getEntry("lib/$abi/libshenko_android.so") ?: return null
                archive.getInputStream(entry).use { input ->
                    val digest = MessageDigest.getInstance("SHA-256")
                    val buffer = ByteArray(1 shl 20)
                    var length = 0L
                    while (true) {
                        val count = input.read(buffer)
                        if (count < 0) break
                        length += count
                        if (length > MAX_NATIVE_BYTES) {
                            failInstaller("denied", "installer_candidate_oversized", "the candidate native member exceeds its byte bound")
                        }
                        digest.update(buffer, 0, count)
                    }
                    InstallerRecords.hex(digest.digest())
                }
            }
        } catch (failure: InstallerFailure) {
            throw failure
        } catch (failure: java.util.zip.ZipException) {
            failInstaller("denied", "installer_package_mismatch", "the candidate file is not a readable APK archive")
        } catch (failure: IOException) {
            failInstaller("io", "installer_candidate_absent", "the candidate archive cannot be read: ${failure.message.orEmpty().take(256)}")
        }
    }

    private fun requireDigest(condition: String, value: String) {
        if (value.length != 64 || value.any { it !in '0'..'9' && it !in 'a'..'f' }) {
            failInstaller("unsupported", condition, "the declared digest is not 64 lowercase hexadecimal characters")
        }
    }

    // ------------------------------------------------------------------
    // Local selection (the installer Activity's chosen candidate)
    // ------------------------------------------------------------------

    fun readSelection(context: Context): Pair<Facts, String?>? {
        val file = File(context.filesDir, SELECTION)
        if (!file.exists()) return null
        if (Files.isSymbolicLink(file.toPath()) ||
            !file.isFile ||
            file.length() !in 1L..InstallerRecords.MAX_RECORD_BYTES
        ) {
            failInstaller("internal", "installer_record_invalid", "the local installer selection is not a bounded app-private file")
        }
        val record = try {
            JSONObject(file.readText(Charsets.UTF_8))
        } catch (failure: Exception) {
            failInstaller("internal", "installer_record_invalid", "the local installer selection is not valid JSON")
        }
        if (record.optInt("schema_version", -1) != SELECTION_SCHEMA) {
            failInstaller("internal", "installer_record_invalid", "the local installer selection has a different schema")
        }
        val path = record.optString("apk_path")
        val canonical = try {
            privateCandidate(context, path)
        } catch (failure: InstallerFailure) {
            return null
        }
        val facts = Facts(
            apkPath = canonical.absolutePath,
            packageName = record.optString("package"),
            versionCode = record.optLong("version_code", -1),
            signerSha256 = record.optString("signer_sha256"),
            apkSha256 = record.optString("apk_sha256"),
            apkBytes = record.optLong("apk_bytes", -1),
            nativeSha256 = record.optString("native_sha256"),
        )
        if (canonical.length() != facts.apkBytes || facts.apkBytes <= 0 || facts.packageName.isEmpty() ||
            facts.versionCode <= 0 || facts.signerSha256.isEmpty()
        ) {
            return null
        }
        val digest = record.optString("request_digest").takeIf { it.isNotEmpty() }
        return facts to digest
    }

    @Synchronized
    fun persistSelection(context: Context, facts: Facts, requestDigest: String?) {
        val record = JSONObject()
            .put("schema_version", SELECTION_SCHEMA)
            .put("apk_path", facts.apkPath)
            .put("package", facts.packageName)
            .put("version_code", facts.versionCode)
            .put("signer_sha256", facts.signerSha256)
            .put("apk_sha256", facts.apkSha256)
            .put("apk_bytes", facts.apkBytes)
            .put("native_sha256", facts.nativeSha256)
        // Omitting the key (rather than storing JSONObject.NULL) keeps
        // optString from reading the literal string "null" as a digest.
        if (requestDigest != null) record.put("request_digest", requestDigest)
        InstallerRecords.atomicWrite0600(File(context.filesDir, SELECTION), record.toString().toByteArray(Charsets.UTF_8))
    }

    @Synchronized
    fun clearSelection(context: Context) {
        runCatching { File(context.filesDir, SELECTION).delete() }
    }
}
