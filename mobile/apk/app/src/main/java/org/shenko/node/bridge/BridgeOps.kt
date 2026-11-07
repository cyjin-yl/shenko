package org.shenko.node.bridge

import android.content.Context
import android.graphics.BitmapFactory
import android.os.Build
import android.util.Base64
import org.json.JSONArray
import org.json.JSONObject
import org.shenko.node.ShenkoAccessibilityService
import org.shenko.node.ShenkoApp
import org.shenko.node.ShizukuUserShell
import org.shenko.node.BoundedProcessCapture
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * Device-side bridge op dispatch. Each op flows through the real OS
 * mechanism with its own live grant check; authorities are parallel — no
 * "normal → Shizuku → Dhizuku → root" ladder is encoded anywhere here.
 *
 * Frozen-argv rule: argv arrays are passed to ProcessBuilder / binder
 * surfaces verbatim. No shell command string is ever constructed, so no
 * quoting or injection hazard exists on this seam.
 */
object BridgeOps {
    private const val TAG = "ShenkoBridgeOps"
    /** Same bound the node refuses to exceed for a single capture. */
    private const val MAX_REGION_PX = 32L * 1024 * 1024

    fun dispatch(context: Context, request: JSONObject): JSONObject =
        when (request.optString("op")) {
            "probe" -> probe(context)
            "capability_list" -> capabilityList(context)
            "screen_capture" -> screenCapture(context, request)
            "node_tree_fingerprint" -> nodeTreeFingerprint(request)
            "input_batch" -> inputBatch(context, request)
            "shell_exec" -> shellExec(context, request)
            "db_snapshot" -> dbSnapshot(context, request)
            "human_activity_after" -> humanActivityAfter(request)
            else -> error("unsupported", "unknown op", false)
        }

    // ------------------------------------------------------------------
    // Probe / capability list
    // ------------------------------------------------------------------

    private fun probe(context: Context): JSONObject {
        val authorities = authorities(context)
        return JSONObject()
            .put("ok", true)
            .put("binding", JSONObject()
                .put("digest", ShenkoApp.instance.installBindingDigest)
                .put("method", "app_install_id"))
            .put("authorities", authorities)
            .put("display", displayFacts(context))
    }

    /**
     * Real screen metrics. `getRealMetrics` reports the physical display that
     * `screencap` returns, unlike window metrics which exclude system decor —
     * a session bound to the window size would place every coordinate wrong.
     */
    @Suppress("DEPRECATION")
    private fun displayFacts(context: Context): JSONObject {
        val metrics = android.util.DisplayMetrics()
        val windowManager = context.getSystemService(Context.WINDOW_SERVICE) as? android.view.WindowManager
        try {
            windowManager?.defaultDisplay?.getRealMetrics(metrics)
        } catch (e: Exception) {
            return JSONObject()
                .put("display_id", 0)
                .put("reason", "display metrics unavailable: ${e.message}")
        }
        return JSONObject()
            .put("display_id", 0)
            .put("width_px", metrics.widthPixels)
            .put("height_px", metrics.heightPixels)
            .put("density_dpi", metrics.densityDpi)
            .put("rotation_deg", surfaceRotationDegrees(context))
    }

    private fun capabilityList(context: Context): JSONObject =
        JSONObject().put("ok", true).put("authorities", authorities(context))

    private fun authorities(context: Context): JSONObject {
        val now = System.currentTimeMillis()
        return JSONObject()
            .put("root_su", probeRootSu(now))
            .put("shizuku_binder", probeShizuku(context, now))
            .put("dhizuku_owner", probeDhizuku(context, now))
            .put("accessibility", probeAccessibility(now))
            .put("normal_perm", JSONObject()
                .put("state", "available")
                .put("detail", "app uid surface (api ${Build.VERSION.SDK_INT})")
                .put("verified_at_ms", now))
    }

    private fun probeRootSu(now: Long): JSONObject {
        val su = File("/system/bin/su")
        if (!su.exists() && File("/system/xbin/su").exists().not()) {
            return JSONObject().put("state", "unavailable")
                .put("reason", "no su binary on system image")
                .put("revalidate", "device mod state changed")
        }
        return try {
            val process = ProcessBuilder("su", "0", "id").redirectErrorStream(true).start()
            val finished = process.waitFor(5, TimeUnit.SECONDS)
            if (!finished) {
                process.destroyForcibly()
                JSONObject().put("state", "unavailable")
                    .put("reason", "su probe timed out (prompt pending?)")
                    .put("revalidate", "user grants app in su manager")
            } else if (process.exitValue() == 0) {
                JSONObject().put("state", "available")
                    .put("detail", "su exec-form argv accepted (uid 0)")
                    .put("verified_at_ms", now)
            } else {
                JSONObject().put("state", "unavailable")
                    .put("reason", "su denied (exit ${process.exitValue()})")
                    .put("revalidate", "user grants app in su manager")
            }
        } catch (e: Exception) {
            JSONObject().put("state", "failed")
                .put("error", "su probe spawn failed: ${e.message}")
                .put("retryable", true)
        }
    }

    private fun probeShizuku(context: Context, now: Long): JSONObject =
        try {
            if (ShizukuUserShell.isShizukuGranted()) {
                val bound = ShizukuUserShell.ensureBound(context)
                if (bound) {
                    JSONObject().put("state", "available")
                        .put("detail", "shizuku binder + user service bound (uid ${android.os.Process.myUid()} path: ${binderOwner()})")
                        .put("verified_at_ms", now)
                } else {
                    JSONObject().put("state", "unavailable")
                        .put("reason", "shizuku granted but user service bind failed")
                        .put("revalidate", "rebind user service")
                }
            } else {
                JSONObject().put("state", "unavailable")
                    .put("reason", "shizuku permission not granted")
                    .put("revalidate", "user grants in Shizuku manager")
            }
        } catch (e: Exception) {
            JSONObject().put("state", "failed").put("error", "shizuku probe: ${e.message}").put("retryable", true)
        }

    private fun binderOwner(): String =
        try {
            val uid = rikka.shizuku.Shizuku.getUid()
            "shizuku uid=$uid"
        } catch (e: Exception) {
            "shizuku uid unknown"
        }

    private fun probeDhizuku(context: Context, now: Long): JSONObject =
        try {
            com.rosan.dhizuku.api.Dhizuku.init(context)
            if (com.rosan.dhizuku.api.Dhizuku.isPermissionGranted()) {
                JSONObject().put("state", "available")
                    .put("detail", "dhizuku device owner delegate granted")
                    .put("verified_at_ms", now)
            } else {
                JSONObject().put("state", "unavailable")
                    .put("reason", "dhizuku permission not granted")
                    .put("revalidate", "user grants in Dhizuku app")
            }
        } catch (e: Exception) {
            JSONObject().put("state", "unavailable")
                .put("reason", "dhizuku not active: ${e.message}")
                .put("revalidate", "user installs/activates Dhizuku")
        }

    private fun probeAccessibility(now: Long): JSONObject {
        val service = ShenkoAccessibilityService.instance
        return if (service != null && service.canRetrieveWindowContent()) {
            JSONObject().put("state", "available")
                .put("detail", "accessibility service enabled by user")
                .put("verified_at_ms", now)
        } else {
            JSONObject().put("state", "unavailable")
                .put("reason", "accessibility service not enabled")
                .put("revalidate", "user enables Shenko Node accessibility service")
        }
    }

    // ------------------------------------------------------------------
    // Screen capture
    // ------------------------------------------------------------------

    private fun screenCapture(context: Context, request: JSONObject): JSONObject {
        val displayId = request.optInt("display_id", 0)
        val dest = File(context.filesDir, "bridge_captures/capture_${System.currentTimeMillis()}.png")
        dest.parentFile?.mkdirs()
        // `screencap` exists on every Android build; pixel capture requires a
        // shell-grade authority. Accessibility alone can only offer the node
        // tree, never pixels — that is reported, never faked.
        val result = execShellAuthority(context, listOf("screencap", "-p", dest.absolutePath))
        if (failed(result)) {
            dest.delete()
            return error(failureKind(result), failureMessage(result), false)
        }
        if (!dest.exists() || dest.length() == 0L) {
            return error("typed_unavailable",
                "no granted pixel-capture mechanism (screencap wrote no frame; MediaProjection adapter not bundled)", false)
        }
        val bytes = dest.readBytes()
        dest.delete()
        val full = BitmapFactory.decodeByteArray(bytes, 0, bytes.size)
            ?: return error("internal", "screencap produced pixels that do not decode", false)
        // The node refuses a region request answered with a larger frame, so
        // the crop has to happen here for regioned observations to be
        // coordinate-correct.
        val cropped = cropRegion(full, request.optJSONArray("region"))
            ?: return error("unsupported", "requested region is outside the captured display", false)
        if (cropped.bitmap.width.toLong() * cropped.bitmap.height.toLong() > MAX_REGION_PX) {
            return error("unsupported", "requested region exceeds the capture area bound", false)
        }
        val png = java.io.ByteArrayOutputStream().use { sink ->
            cropped.bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG, 100, sink)
            sink.toByteArray()
        }
        if (png.isEmpty()) {
            return error("internal", "region re-encode produced no bytes", false)
        }
        val pngB64 = Base64.encodeToString(png, Base64.NO_WRAP)
        if (pngB64.length + 1024 > DeviceBridgeServer.MAX_RESPONSE_BYTES) {
            // Answering with a frame the node must reject as oversized would
            // waste the whole round-trip.
            return error("unsupported", "capture exceeds the response frame bound", false)
        }
        return JSONObject()
            .put("ok", true)
            .put("png_b64", pngB64)
            .put("width_px", cropped.bitmap.width)
            .put("height_px", cropped.bitmap.height)
            .put("region", cropped.region)
            .put("density_dpi", context.resources.displayMetrics.densityDpi)
            .put("rotation_deg", surfaceRotationDegrees(context))
            .put("secure_display", false)
            .put("capture_method", "screencap_argv")
            .put("captured_at_ms", System.currentTimeMillis())
            .put("authority_used", result.optString("authority_used"))
            .put("display", JSONObject().put("display_id", displayId))
    }

    private class Cropped(val bitmap: android.graphics.Bitmap, val region: JSONArray)

    /**
     * Display-absolute crop, or null when the request is malformed or falls
     * outside the captured frame. A full-display request reports the frame the
     * node will see, so both sides agree on the coordinate space.
     */
    private fun cropRegion(full: android.graphics.Bitmap, requested: JSONArray?): Cropped? {
        if (requested == null || requested.length() != 4) {
            return Cropped(full, JSONArray().put(0).put(0).put(full.width).put(full.height))
        }
        val x = requested.optInt(0, -1)
        val y = requested.optInt(1, -1)
        val width = requested.optInt(2, -1)
        val height = requested.optInt(3, -1)
        if (x < 0 || y < 0 || width <= 0 || height <= 0) return null
        if (x + width > full.width || y + height > full.height) return null
        return Cropped(
            android.graphics.Bitmap.createBitmap(full, x, y, width, height),
            JSONArray().put(x).put(y).put(width).put(height),
        )
    }

    private fun surfaceRotationDegrees(context: Context): Int =
        (context.getSystemService(Context.WINDOW_SERVICE) as? android.view.WindowManager)
            ?.defaultDisplay?.rotation?.let { when (it) {
                android.view.Surface.ROTATION_90 -> 90
                android.view.Surface.ROTATION_180 -> 180
                android.view.Surface.ROTATION_270 -> 270
                else -> 0
            } } ?: 0

    // ------------------------------------------------------------------
    // Semantic hierarchy fingerprint
    // ------------------------------------------------------------------

    /**
     * The node asks for the digest of the hierarchy the accessibility service
     * can actually see inside a display-absolute rectangle, under the traversal
     * bounds it declared.
     *
     * This is deliberately a separate op from [screenCapture]: pixels come from
     * a shell-grade authority while the semantic tree comes from the enabled
     * accessibility service, and one being granted never implies the other.
     * When the hierarchy is unavailable this answers with the real reason and
     * no digest — the node records a pixels-only observation instead of binding
     * a hash of nothing.
     */
    private fun nodeTreeFingerprint(request: JSONObject): JSONObject {
        val regionArray = request.optJSONArray("region")
            ?: return error(
                "unsupported",
                "region is required: a hierarchy fingerprint is always scoped to the area it covers",
                false,
            )
        if (regionArray.length() != 4) {
            return error("unsupported", "region must be [x,y,width,height]", false)
        }
        val x = regionArray.optInt(0, -1)
        val y = regionArray.optInt(1, -1)
        val width = regionArray.optInt(2, -1)
        val height = regionArray.optInt(3, -1)
        if (x < 0 || y < 0 || width <= 0 || height <= 0) {
            return error(
                "unsupported",
                "region must have a non-negative origin and a positive area",
                false,
            )
        }
        if (x.toLong() + width > Int.MAX_VALUE.toLong() || y.toLong() + height > Int.MAX_VALUE.toLong()) {
            return error("unsupported", "region exceeds the addressable display area", false)
        }
        if (width.toLong() * height.toLong() > MAX_REGION_PX) {
            return error("unsupported", "region exceeds the capture area bound", false)
        }
        val region = android.graphics.Rect(x, y, x + width, y + height)
        return try {
            NodeTreeFingerprint.compute(
                ShenkoAccessibilityService.instance,
                request.optInt("display_id", 0),
                region,
                request.optInt("max_nodes", NodeTreeFingerprint.HARD_MAX_NODES),
                request.optInt("max_depth", NodeTreeFingerprint.HARD_MAX_DEPTH),
                request.optInt("max_windows", NodeTreeFingerprint.HARD_MAX_WINDOWS),
            )
        } catch (e: NodeTreeFingerprint.Unavailable) {
            error(e.kind, e.message ?: "node hierarchy unavailable", false)
        } catch (e: Exception) {
            error("internal", "hierarchy traversal failed: ${e.message}", false)
        }
    }

    // ------------------------------------------------------------------
    // Input batch
    // ------------------------------------------------------------------

    private fun inputBatch(context: Context, request: JSONObject): JSONObject {
        val actions = request.optJSONArray("actions") ?: return error("internal", "missing actions", false)
        val receipts = JSONArray()
        var authorityUsed = "none"
        for (i in 0 until actions.length()) {
            val action = actions.getJSONObject(i)
            val kind = action.optString("kind")
            val receipt = when (kind) {
                "tap" -> {
                    val x = action.getDouble("x").toFloat()
                    val y = action.getDouble("y").toFloat()
                    if (ShenkoAccessibilityService.instance?.dispatchTap(x, y) == true) {
                        authorityUsed = "accessibility"
                        ack(i, "gesture tap", "accessibility")
                    } else {
                        val r = execShellAuthority(context, listOf("input", "tap", "$x", "$y"))
                        val used = r.optString("authority_used", "none")
                        authorityUsed = used
                        if (failed(r)) nack(i, failureMessage(r), used)
                        else ack(i, "input tap", used)
                    }
                }
                "swipe" -> {
                    val x1 = action.getDouble("x1").toFloat()
                    val y1 = action.getDouble("y1").toFloat()
                    val x2 = action.getDouble("x2").toFloat()
                    val y2 = action.getDouble("y2").toFloat()
                    val durationMs = action.optLong("duration_ms", 100)
                    if (ShenkoAccessibilityService.instance?.dispatchSwipe(x1, y1, x2, y2, durationMs) == true) {
                        authorityUsed = "accessibility"
                        ack(i, "gesture swipe", "accessibility")
                    } else {
                        val r = execShellAuthority(context,
                            listOf("input", "swipe", "$x1", "$y1", "$x2", "$y2", "$durationMs"))
                        val used = r.optString("authority_used", "none")
                        authorityUsed = used
                        if (failed(r)) nack(i, failureMessage(r), used)
                        else ack(i, "input swipe", used)
                    }
                }
                "key" -> {
                    val keycode = action.getInt("keycode")
                    if (action.optBoolean("down", false)) {
                        // `input keyevent` sends one complete press and cannot
                        // hold a key; claiming delivery would invent a state
                        // this mechanism never produced.
                        nack(i, "keyevent delivers a complete press only; a held keycode $keycode is not deliverable", "none")
                    } else {
                        val r = execShellAuthority(context, listOf("input", "keyevent", "$keycode"))
                        val used = r.optString("authority_used", "none")
                        authorityUsed = used
                        if (failed(r)) nack(i, failureMessage(r), used)
                        else ack(i, "input keyevent (complete press)", used)
                    }
                }
                "text" -> {
                    // Accessibility SET_TEXT on an arbitrary focus target is not
                    // honest here; text entry goes through the `input` argv path.
                    val r = execShellAuthority(context, listOf("input", "text", action.getString("text")))
                    val used = r.optString("authority_used", "none")
                    authorityUsed = used
                    if (failed(r)) nack(i, failureMessage(r), used)
                    else ack(i, "input text", used)
                }
                "wait" -> {
                    Thread.sleep(action.optLong("ms", 0))
                    // A local sleep is not an OS input delivery.
                    ack(i, "local wait", "none")
                }
                else -> nack(i, "unknown action kind $kind", "none")
            }
            receipts.put(receipt)
        }
        return JSONObject()
            .put("ok", true)
            .put("receipts", receipts)
            .put("authority_used", authorityUsed)
    }

    private fun ack(index: Int, note: String, authority: String): JSONObject =
        JSONObject().put("index", index).put("acked", true).put("note", note)
            .put("authority_used", authority)

    private fun nack(index: Int, note: String, authority: String): JSONObject =
        JSONObject().put("index", index).put("acked", false).put("note", note)
            .put("authority_used", authority)

    /**
     * Authority results report failure as `{"ok":false,"error":{…}}` (and a
     * timed-out process as `timed_out`), and a command that ran but failed
     * still carries a non-zero `exit_code`. Delivery needs all three to be
     * clean: treating a missing `error_kind` as success is how a refused tap
     * used to be reported as an acknowledged one.
     */
    private fun failed(result: JSONObject): Boolean =
        !result.optBoolean("ok", false) ||
            result.optBoolean("timed_out", false) ||
            (result.has("exit_code") && result.optInt("exit_code", -1) != 0)

    private fun failureKind(result: JSONObject): String =
        result.optJSONObject("error")?.optString("kind")?.takeIf { it.isNotEmpty() }
            ?: if (result.has("exit_code")) "typed_unavailable" else "io"

    private fun failureMessage(result: JSONObject): String =
        result.optJSONObject("error")?.optString("message")?.takeIf { it.isNotEmpty() }
            ?: if (result.optBoolean("timed_out", false)) {
                "authority process timed out"
            } else if (result.has("exit_code")) {
                "authority command exited ${result.optInt("exit_code", -1)}"
            } else {
                "authority produced no result"
            }

    // ------------------------------------------------------------------
    // Shell exec
    // ------------------------------------------------------------------

    private fun shellExec(context: Context, request: JSONObject): JSONObject {
        val authority = request.optString("authority", "auto")
        val argv = jsonArrayToList(request.getJSONArray("argv"))
        val env = jsonArrayToList(request.optJSONArray("env") ?: JSONArray())
        if (argv.isEmpty()) return error("internal", "argv empty", false)
        val timeoutMillis = request.optLong("timeout_secs", 10).coerceIn(1, 60) * 1000

        return when (authority) {
            "root_su" -> execRootSu(argv, env, timeoutMillis)
            "shizuku" -> execShizuku(context, argv, env, timeoutMillis)
            "dhizuku" -> execDhizuku(argv, env, timeoutMillis)
            else -> {
                val shizuku = tryShizuku(context, argv, env, timeoutMillis)
                if (shizuku != null) shizuku.put("authority_used", "shizuku")
                else {
                    val root = execRootSu(argv, env, timeoutMillis)
                    if (root.optBoolean("ok")) root.put("authority_used", "root_su") else root
                }
            }
        }
    }

    private fun tryShizuku(context: Context, argv: List<String>, env: List<String>, timeoutMillis: Long): JSONObject? {
        if (!ShizukuUserShell.isShizukuGranted()) return null
        val result = ShizukuUserShell.exec(context, argv, env, timeoutMillis)
        return if (result.has("error_kind")) null
        else JSONObject()
            .put("ok", true)
            .put("exit_code", result.optInt("exit_code", -1))
            .put("stdout_b64", result.optString("stdout_b64", ""))
            .put("stderr_b64", result.optString("stderr_b64", ""))
            .put("timed_out", result.optBoolean("timed_out", false))
    }

    private fun execShizuku(context: Context, argv: List<String>, env: List<String>, timeoutMillis: Long): JSONObject {
        if (!ShizukuUserShell.isShizukuGranted()) {
            return error("typed_unavailable", "shizuku not granted", false)
        }
        val result = ShizukuUserShell.exec(context, argv, env, timeoutMillis)
        return if (result.has("error_kind")) {
            error(result.getString("error_kind"), result.getString("message"), false)
        } else {
            JSONObject()
                .put("ok", true)
                .put("exit_code", result.optInt("exit_code", -1))
                .put("stdout_b64", result.optString("stdout_b64", ""))
                .put("stderr_b64", result.optString("stderr_b64", ""))
                .put("timed_out", result.optBoolean("timed_out", false))
                .put("authority_used", "shizuku")
        }
    }

    /**
     * Exec-form root: `su 0 ARGV...` — supported by AOSP/LineageOS su which
     * execs argv without a shell. Implementations that only accept a shell
     * command string report typed_unavailable here; constructing a shell
     * string is forbidden on this seam.
     */
    private fun execRootSu(argv: List<String>, env: List<String>, timeoutMillis: Long): JSONObject {
        return try {
            val full = mutableListOf("su", "0")
            full.addAll(argv)
            val pb = ProcessBuilder(full)
            pb.redirectErrorStream(false)
            val environment = pb.environment()
            for (entry in env) {
                val idx = entry.indexOf('=')
                if (idx > 0) environment[entry.substring(0, idx)] = entry.substring(idx + 1)
            }
            val process = pb.start()
            val captured = BoundedProcessCapture.capture(process, timeoutMillis)
            JSONObject().put("ok", true)
                .put("exit_code", captured.exitCode)
                .put("stdout_b64", Base64.encodeToString(captured.stdout, Base64.NO_WRAP))
                .put("stderr_b64", Base64.encodeToString(captured.stderr, Base64.NO_WRAP))
                .put("timed_out", captured.timedOut)
                .put("authority_used", "root_su")
        } catch (e: Exception) {
            error("typed_unavailable", "root su exec-form unavailable: ${e.message}", false)
        }
    }

    private fun execDhizuku(argv: List<String>, env: List<String>, timeoutMillis: Long): JSONObject {
        return try {
            if (!com.rosan.dhizuku.api.Dhizuku.isPermissionGranted()) {
                return error("typed_unavailable", "dhizuku permission not granted", false)
            }
            val envArray = if (env.isEmpty()) null else env.toTypedArray()
            val process = com.rosan.dhizuku.api.Dhizuku.newProcess(argv.toTypedArray(), envArray, null)
            val captured = BoundedProcessCapture.capture(process, timeoutMillis)
            JSONObject().put("ok", true)
                .put("exit_code", captured.exitCode)
                .put("stdout_b64", Base64.encodeToString(captured.stdout, Base64.NO_WRAP))
                .put("stderr_b64", Base64.encodeToString(captured.stderr, Base64.NO_WRAP))
                .put("timed_out", captured.timedOut)
                .put("authority_used", "dhizuku")
        } catch (e: Exception) {
            error("typed_unavailable", "dhizuku process unavailable: ${e.message}", false)
        }
    }

    /** Pick the strongest granted shell authority for device-side argv. */
    private fun execShellAuthority(context: Context, argv: List<String>): JSONObject {
        val shizuku = tryShizuku(context, argv, emptyList(), 15_000)
        if (shizuku != null) return shizuku.put("authority_used", "shizuku")
        val root = execRootSu(argv, emptyList(), 15_000)
        if (root.optBoolean("ok") && !root.optBoolean("timed_out", false)) {
            return root
        }
        val dhizuku = execDhizuku(argv, emptyList(), 15_000)
        if (dhizuku.optBoolean("ok")) return dhizuku
        return error("typed_unavailable", "no granted shell authority (shizuku/root/dhizuku)", false)
    }

    private fun jsonArrayToList(array: JSONArray): List<String> =
        List(array.length()) { array.getString(it) }

    // ------------------------------------------------------------------
    // App database snapshot — §24.5.1
    // ------------------------------------------------------------------

    private fun dbSnapshot(context: Context, request: JSONObject): JSONObject {
        val authority = request.optString("authority", "auto")
        val packageName = request.getString("package")
        val dbName = request.getString("db_name")
        val method = request.optString("method", "auto")

        if (method == "app_export") {
            // No app-specific export adapter is bundled; we never fake one.
            return error("unsupported", "app_export adapter not bundled for $packageName", false)
        }

        val source = locateDatabase(packageName, dbName)
            ?: return error("typed_unavailable", "database not found for $packageName/$dbName", false)
        val direct = File(source)
        if (direct.canRead()) {
            // The node uid can read the source directly: it runs the SQLite
            // backup API itself, which is the only path that can ever become
            // `snapshot_consistent`.
            return JSONObject()
                .put("ok", true)
                .put("direct_readable_path", source)
                .put("source_path", source)
                .put("method_used", "direct_readable")
        }

        // Copy the complete file set (main + WAL/SHM/journal companions)
        // through a granted shell authority. This is a byte-level recovered
        // copy at best — the node records it as `snapshot_recovered_copy`.
        val snapshotDir = File(context.filesDir, "bridge_dbsnapshots/${System.currentTimeMillis()}")
        snapshotDir.mkdirs()
        val copied = JSONArray()
        var walPresent = false
        var walSize = -1L
        var journalCopied = false
        val notes = JSONArray()

        val companions = listOf("", "-wal", "-shm", "-journal")
        for (suffix in companions) {
            val src = source + suffix
            val dst = File(snapshotDir, dbName + suffix)
            val cp = when (authority) {
                "root_su" -> execRootSu(listOf("cp", src, dst.absolutePath), emptyList(), 15_000)
                "shizuku" -> execShizuku(context, listOf("cp", src, dst.absolutePath), emptyList(), 15_000)
                "dhizuku" -> execDhizuku(listOf("cp", src, dst.absolutePath), emptyList(), 15_000)
                else -> execShellAuthority(context, listOf("cp", src, dst.absolutePath))
            }
            val okCp = cp.optBoolean("ok") && !cp.optBoolean("timed_out", false) &&
                cp.optInt("exit_code", -1) == 0 && dst.exists()
            if (suffix.isEmpty() && !okCp) {
                return error("typed_unavailable",
                    "copy of main db failed through all granted authorities", false)
            }
            if (okCp) {
                copied.put(dbName + suffix)
                when (suffix) {
                    "-wal" -> { walPresent = true; walSize = dst.length() }
                    "-journal" -> journalCopied = true
                }
            } else if (suffix.isNotEmpty()) {
                notes.put("companion $suffix not copied (absent or unreadable)")
            }
        }

        return JSONObject()
            .put("ok", true)
            .put("snapshot_dir", snapshotDir.absolutePath)
            .put("files", copied)
            .put("wal_present", walPresent)
            .put("wal_size", walSize)
            .put("journal_copied", journalCopied)
            .put("source_path", source)
            .put("method_used", "raw_copy")
            .put("notes", notes)
    }

    private fun locateDatabase(packageName: String, dbName: String): String? {
        val bases = listOf("/data/user/0", "/data/data")
        for (base in bases) {
            val path = "$base/$packageName/databases/$dbName"
            if (File(path).exists()) return path
        }
        return null
    }

    // ------------------------------------------------------------------
    // Human-activity feed (external takeover detection)
    // ------------------------------------------------------------------

    private fun humanActivityAfter(request: JSONObject): JSONObject {
        val afterMs = request.optLong("after_ms", 0)
        val app = ShenkoApp.instance
        synchronized(app.humanActivityEvents) {
            while (app.humanActivityEvents.isNotEmpty() &&
                app.humanActivityEvents.first() < afterMs - 60_000
            ) {
                app.humanActivityEvents.removeFirst()
            }
            val count = app.humanActivityEvents.count { it >= afterMs }
            return JSONObject().put("ok", true).put("human_events", count)
        }
    }

    // ------------------------------------------------------------------
    // Errors
    // ------------------------------------------------------------------

    private fun error(kind: String, message: String, retryable: Boolean): JSONObject =
        JSONObject().put("ok", false).put(
            "error",
            JSONObject().put("kind", kind).put("message", message).put("retryable", retryable)
        )
}
