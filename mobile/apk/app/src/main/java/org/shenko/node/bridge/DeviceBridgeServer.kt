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
    private var boundSocket: LocalSocket? = null
    @Volatile
    private var token: String = ""

    fun start() {
        stop()
        token = newToken().also { writeToken(it) }
        socketFile.delete()
        val socket = LocalSocket()
        val listener = try {
            socket.bind(LocalSocketAddress(socketFile.absolutePath, LocalSocketAddress.Namespace.FILESYSTEM))
            LocalServerSocket(socket.fileDescriptor)
        } catch (error: Exception) {
            socket.close()
            throw error
        }
        // LocalServerSocket(FileDescriptor) borrows the descriptor; retain its
        // actual owner so stop closes the filesystem socket and wakes accept.
        boundSocket = socket
        server = listener
        running = true
        Thread({ acceptLoop(listener) }, "shenko-bridge").apply { isDaemon = true }.start()
        Log.i(TAG, "bridge listening on ${socketFile.absolutePath}")
    }

    fun stop() {
        running = false
        try {
            server?.close()
        } catch (_: Exception) {
        }
        server = null
        try {
            boundSocket?.close()
        } catch (_: Exception) {
        }
        boundSocket = null
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

    private fun acceptLoop(listener: LocalServerSocket) {
        while (running) {
            val connection = try {
                listener.accept()
            } catch (e: Exception) {
                if (running) Log.w(TAG, "accept failed: ${e.message}")
                null
            } ?: break
            Thread({ handle(connection) }, "shenko-bridge-conn").apply { isDaemon = true }.start()
        }
    }

    private fun handle(connection: LocalSocket) {
        try {
            connection.soTimeout = 30_000
            val input = connection.inputStream.buffered()
            val output = connection.outputStream

            val handshakeLine = BridgeFrames.readLine(input, MAX_HANDSHAKE_BYTES) ?: return
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

            val requestLine = BridgeFrames.readLine(input, MAX_REQUEST_BYTES) ?: return
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
            val responseBytes = response.toString().toByteArray(Charsets.UTF_8)
            if (responseBytes.size > MAX_RESPONSE_BYTES) {
                writeJson(output, refuse("internal", "response exceeds 16 MiB bound", false))
                return
            }
            writeFrame(output, responseBytes)
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

    private fun writeJson(output: java.io.OutputStream, value: JSONObject) {
        writeFrame(output, value.toString().toByteArray(Charsets.UTF_8))
    }

    private fun writeFrame(output: java.io.OutputStream, bytes: ByteArray) {
        output.write(bytes)
        output.write('\n'.code)
        output.flush()
    }
}
