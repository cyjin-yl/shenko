package org.shenko.node

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import org.shenko.node.bridge.DeviceBridgeServer

/**
 * Foreground service hosting the Shenko node in-process:
 * - starts the app-owned device bridge (UNIX socket + per-incarnation token)
 *   so the Rust node can reach real Android mechanisms (SPEC §24.5);
 * - loads the Rust cdylib (`shenko_android`) and calls `nativeStart` on a
 *   dedicated thread, passing the app filesDir as the node data dir.
 *
 * The Rust runtime blocks on that thread until shutdown. Destroying a service
 * does not prove that its native thread stopped. There is no second runtime
 * and no exec'd helper.
 */
class ShenkoNodeService : Service() {

    private lateinit var bridgeServer: DeviceBridgeServer
    private var nodeThread: Thread? = null

    private external fun nativeStart(filesDir: String, port: Int): String

    override fun onCreate() {
        super.onCreate()
        bridgeServer = DeviceBridgeServer(this, filesDir)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val port = intent?.getIntExtra(EXTRA_PORT, DEFAULT_PORT) ?: DEFAULT_PORT
        startAsForeground()
        if (!bridgeRunning) {
            runCatching { bridgeServer.start() }
            bridgeRunning = true
        }
        if (nodeThread == null) {
            nodeThread = Thread({ runNode(port) }, "shenko-node").apply { start() }
        }
        return START_STICKY
    }

    @Volatile
    private var bridgeRunning = false

    private fun runNode(port: Int) {
        try {
            System.loadLibrary("shenko_android")
            val status = nativeStart(filesDir.absolutePath, port)
            Log.i(TAG, "node exited: $status")
        } catch (e: UnsatisfiedLinkError) {
            Log.e(TAG, "native library missing; build the Rust shell (cargo ndk)", e)
        } catch (e: Exception) {
            Log.e(TAG, "node crashed: ${e.message}", e)
        } finally {
            stopSelf()
        }
    }

    private fun startAsForeground() {
        val channel = NotificationChannel(CHANNEL_ID, "Shenko Node", NotificationManager.IMPORTANCE_LOW)
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        val notification: Notification = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Shenko Node")
            .setContentText("Agent runtime active")
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    override fun onDestroy() {
        runCatching { bridgeServer.stop() }
        bridgeRunning = false
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val TAG = "ShenkoNodeService"
        private const val CHANNEL_ID = "shenko_node"
        private const val NOTIFICATION_ID = 1
        const val DEFAULT_PORT = 7860
        const val EXTRA_PORT = "port"

        fun start(context: Context, port: Int = DEFAULT_PORT) {
            val intent = Intent(context, ShenkoNodeService::class.java)
                .putExtra(EXTRA_PORT, port)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }
    }
}
