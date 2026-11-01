package org.shenko.node.bridge

import android.content.Context
import android.net.LocalServerSocket
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.util.Log
import org.json.JSONObject
import java.io.File
import java.security.SecureRandom

/**
 * App-owned UNIX-socket bridge server (SPEC §24.5):
 * - socket: `<filesDir>/device_bridge.sock`, filesystem namespace, same uid;
 * - token: `<filesDir>/device_bridge.token` (0600), rewritten per server
 *   incarnation. The token binds the *bridge service incarnation* — it is
 *   not a grant and never elevates any operation;
 * - framing: one handshake line, one request line (≤ 64 KiB), one response
 *   line (≤ 16 MiB). One round-trip per connection; the node reconnects
 *   each time so app-process death surfaces as a connect failure.
 *
 * Authorities are probed per op inside [BridgeOps]; this class only
 * authenticates and dispatches.
 */
class DeviceBridgeServer(
    private val context: Context,
    private val filesDir: File,
) {
    companion object {
        private const val TAG = "ShenkoBridge"
        const val PROTOCOL_VERSION = 1
        const val MAX_REQUEST_BYTES = 64 * 1024
        const val MAX_RESPONSE_BYTES = 16 * 1024 * 1024
        private const val MAX_HANDSHAKE_BYTES = 4 * 1024
    }

    private val socketFile get() = File(filesDir, "device_bridge.sock")
    private val tokenFile get() = File(filesDir, "device_bridge.token")

    @Volatile
    private var running = false
    private var server: LocalServerSocket? = null
    @Volatile
    private var token: String = ""

    fun start() {
        stop()
        token = newToken().also { writeToken(it) }
        socketFile.delete()
        server = LocalServerSocket(
            LocalSocketAddress(socketFile.absolutePath, LocalSocketAddress.Namespace.FILESYSTEM)
        )
        running = true
        Thread({ acceptLoop() }, "shenko-bridge").apply { isDaemon = true }.start()
        Log.i(TAG, "bridge listening on ${socketFile.absolutePath}")
    }

    fun stop() {
        running = false
        try {
            server?.close()
        } catch (_: Exception) {
        }
        server = null
        socketFile.delete()
    }

    private fun newToken(): String {
        val bytes = ByteArray(32)
        SecureRandom().nextBytes(bytes)
        return bytes.joinToString("") { "%02x".format(it) }
    }

    private fun writeToken(value: String) {
        tokenFile.writeText(value)
        tokenFile.setReadable(false, false)
        tokenFile.setWritable(true, false)
        tokenFile.setExecutable(false, false)
        tokenFile.setReadable(true, true) // owner-only read
    }

    private fun acceptLoop() {
        while (running) {
            val connection = try {
                server?.accept()
            } catch (e: Exception) {
                if (running) Log.w(TAG, "accept failed: ${e.message}")
                null
            } ?: continue
            Thread({ handle(connection) }, "shenko-bridge-conn").apply { isDaemon = true }.start()
        }
    }

    private fun handle(connection: LocalSocket) {
        try {
            connection.soTimeout = 30_000
            val input = connection.inputStream
            val output = connection.outputStream

            val handshakeLine = readLine(input, MAX_HANDSHAKE_BYTES) ?: return
            val handshake = try {
                JSONObject(handshakeLine)
            } catch (e: Exception) {
                writeJson(output, refuse("internal", "handshake parse failed", false))
                return
            }
            val version = handshake.optInt("version", -1)
            val presented = handshake.optString("token", "")
            if (version != PROTOCOL_VERSION) {
                writeJson(output, refuse("unsupported", "protocol version $version not supported", false))
                return
            }
            if (presented != token || token.isEmpty()) {
                writeJson(output, refuse("denied", "bridge token mismatch", false))
                return
            }
            writeJson(output, JSONObject().put("ok", true))

            val requestLine = readLine(input, MAX_REQUEST_BYTES) ?: return
            val response = try {
                val request = JSONObject(requestLine)
                BridgeOps.dispatch(context, request)
            } catch (e: Exception) {
                JSONObject().put("ok", false).put(
                    "error",
                    JSONObject()
                        .put("kind", "internal")
                        .put("message", "dispatch failed: ${e.message}")
                        .put("retryable", false)
                )
            }
            if (response.toString().length > MAX_RESPONSE_BYTES) {
                writeJson(output, refuse("internal", "response exceeds 16 MiB bound", false))
                return
            }
            writeJson(output, response)
        } catch (e: Exception) {
            Log.w(TAG, "connection failed: ${e.message}")
        } finally {
            try {
                connection.close()
            } catch (_: Exception) {
            }
        }
    }

    private fun refuse(kind: String, message: String, retryable: Boolean): JSONObject =
        JSONObject().put("ok", false).put(
            "error",
            JSONObject().put("kind", kind).put("message", message).put("retryable", retryable)
        )

    /** Read one LF-terminated line bounded by [cap] bytes; null on EOF/violation. */
    private fun readLine(input: java.io.InputStream, cap: Int): String? {
        val builder = StringBuilder()
        while (true) {
            val byte = input.read()
            if (byte == -1) return null
            if (byte == '\n'.code) return builder.toString()
            builder.append(byte.toChar())
            if (builder.length > cap) return null
        }
    }

    private fun writeJson(output: java.io.OutputStream, value: JSONObject) {
        output.write((value.toString() + "\n").toByteArray())
        output.flush()
    }
}
