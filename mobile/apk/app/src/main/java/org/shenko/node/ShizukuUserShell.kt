package org.shenko.node

import android.content.ComponentName
import android.content.Context
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.IBinder
import android.util.Base64
import rikka.shizuku.Shizuku
import rikka.shizuku.Shizuku.UserServiceArgs
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Shizuku-backed frozen-argv shell.
 *
 * Shizuku's `newProcess` is private API of the manager; the documented,
 * versioned surface is a user-service binder (UserServiceArgs +
 * bindUserService). Our [ShellService] runs inside the Shizuku-started
 * process and executes argv verbatim with ProcessBuilder. ADB-started and
 * root-started Shizuku are *different identities*; the Rust side reports the
 * binder uid truthfully in the probe and never assumes they are equivalent.
 */
object ShizukuUserShell {

    private const val USER_SERVICE_SUFFIX = "shenko_shell"

    class ShellService : IShenkoShellService.Stub() {
        override fun execJson(argv: Array<String>, env: Array<String>, timeoutMillis: Long): String {
            val timedOut = booleanArrayOf(false)
            val process = try {
                val pb = ProcessBuilder(*argv)
                pb.directory(File("/"))
                pb.redirectErrorStream(false)
                val environment = pb.environment()
                for (entry in env) {
                    val idx = entry.indexOf('=')
                    if (idx > 0) environment[entry.substring(0, idx)] = entry.substring(idx + 1)
                }
                pb.start()
            } catch (e: Exception) {
                return org.json.JSONObject()
                    .put("exit_code", -1)
                    .put("stdout_b64", "")
                    .put("stderr_b64", Base64.encodeToString(
                        ("spawn failed: ${e.message}").toByteArray(), Base64.NO_WRAP))
                    .put("timed_out", false)
                    .toString()
            }
            val finished = process.waitFor(timeoutMillis, TimeUnit.MILLISECONDS)
            if (!finished) {
                timedOut[0] = true
                process.destroyForcibly()
                process.waitFor(1, TimeUnit.SECONDS)
            }
            val stdout = drain(process.inputStream)
            val stderr = drain(process.errorStream)
            return org.json.JSONObject()
                .put("exit_code", if (timedOut[0]) -1 else process.exitValue())
                .put("stdout_b64", Base64.encodeToString(stdout, Base64.NO_WRAP))
                .put("stderr_b64", Base64.encodeToString(stderr, Base64.NO_WRAP))
                .put("timed_out", timedOut[0])
                .toString()
        }

        override fun destroy() {
            // Shizuku tears the user-service process down after unbind.
        }

        private fun drain(stream: java.io.InputStream): ByteArray {
            val out = java.io.ByteArrayOutputStream()
            val buf = ByteArray(64 * 1024)
            var read = stream.read(buf)
            var total = 0
            val cap = 4 * 1024 * 1024
            while (read > 0 && total < cap) {
                val take = minOf(read, cap - total)
                out.write(buf, 0, take)
                total += take
                if (take < read) break
                read = stream.read(buf)
            }
            return out.toByteArray()
        }
    }

    @Volatile
    private var service: IShenkoShellService? = null
    private var latch: CountDownLatch = CountDownLatch(1)

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            service = IShenkoShellService.Stub.asInterface(binder)
            latch.countDown()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            service = null
        }
    }

    fun isShizukuGranted(): Boolean =
        try {
            Shizuku.isPreV11() || Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED
        } catch (e: Exception) {
            false
        }

    fun ensureBound(context: Context): Boolean {
        service?.let { return true }
        if (!isShizukuGranted()) return false
        return try {
            latch = CountDownLatch(1)
            val args = UserServiceArgs(
                ComponentName(context, ShellService::class.java.name)
            ).processNameSuffix(USER_SERVICE_SUFFIX).debuggable(false).version(1)
            Shizuku.bindUserService(args, connection)
            latch.await(5, TimeUnit.SECONDS)
            service != null
        } catch (e: Exception) {
            false
        }
    }

    /** Execute frozen argv through the Shizuku user service. Never a shell string. */
    fun exec(context: Context, argv: List<String>, env: List<String>, timeoutMillis: Long): org.json.JSONObject {
        if (!ensureBound(context)) {
            return org.json.JSONObject()
                .put("error_kind", "typed_unavailable")
                .put("message", "shizuku not granted or user service not bound")
        }
        val svc = service ?: return org.json.JSONObject()
            .put("error_kind", "typed_unavailable")
            .put("message", "shizuku user service unbound")
        return try {
            org.json.JSONObject(svc.execJson(argv.toTypedArray(), env.toTypedArray(), timeoutMillis))
        } catch (e: Exception) {
            org.json.JSONObject()
                .put("error_kind", "internal")
                .put("message", "shizuku exec failed: ${e.message}")
        }
    }
}
