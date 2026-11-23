package org.shenko.node.activation

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import org.shenko.node.R

/**
 * Durable `PackageInstaller` status.
 *
 * It records what the system installer actually decided and surfaces the
 * system confirmation when Android asks for one. It never writes the
 * activation selector, never starts a replacement runtime and never confirms
 * on the user's behalf.
 */
class InstallerStatusReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        try {
            handle(context, intent)
        } catch (failure: Exception) {
            // The durable attempt keeps its previous state, so a recording
            // failure is never reported as an install result.
            Log.e(TAG, "installer status was not recorded", failure)
        }
    }

    private fun handle(context: Context, intent: Intent) {
        if (!ActivationController.recordEvent(context, intent)) return
        val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, Int.MIN_VALUE)
        if (status != PackageInstaller.STATUS_PENDING_USER_ACTION) return
        val requestDigest = intent.getStringExtra(ActivationController.EXTRA_STATUS) ?: return
        val confirmation = intent.parcelableIntentExtra(Intent.EXTRA_INTENT)
        if (confirmation == null) {
            ActivationController.confirmationDelivery(context, requestDigest, "system_confirmation_missing")
            return
        }
        // Keep the confirmation for a foreground installer activity; the
        // notification is the durable path when the process is backgrounded.
        ActivationController.offerConfirmation(requestDigest, confirmation)
        val delivery = try {
            postConfirmation(context, requestDigest, confirmation)
        } catch (failure: Exception) {
            "notification_failed:${failure.javaClass.simpleName}:${failure.message.orEmpty().take(256)}"
        }
        ActivationController.confirmationDelivery(context, requestDigest, delivery)
    }

    private fun postConfirmation(context: Context, requestDigest: String, confirmation: Intent): String {
        val manager = context.getSystemService(NotificationManager::class.java)
            ?: return "notification_manager_unavailable"
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                context.getString(R.string.package_install_channel),
                NotificationManager.IMPORTANCE_HIGH,
            ),
        )
        if (!manager.areNotificationsEnabled()) return "notifications_disabled"
        if (Build.VERSION.SDK_INT >= 33 &&
            context.checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return "notifications_disabled"
        }
        val channel = manager.getNotificationChannel(CHANNEL_ID)
        if (channel == null || channel.importance == NotificationManager.IMPORTANCE_NONE) {
            return "notifications_disabled"
        }
        val launch = ActivationController.confirmationLaunch(context, requestDigest, confirmation)
        val pending = PendingIntent.getActivity(
            context,
            NOTIFICATION_ID,
            launch,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        manager.notify(
            NOTIFICATION_ID,
            Notification.Builder(context, CHANNEL_ID)
                .setContentTitle(context.getString(R.string.package_install_confirm_title))
                .setContentText(context.getString(R.string.package_install_confirm_text))
                .setSmallIcon(android.R.drawable.stat_sys_download)
                .setContentIntent(pending)
                .setAutoCancel(true)
                .build(),
        )
        return "notification_posted"
    }

    private companion object {
        const val TAG = "ShenkoInstaller"
        const val CHANNEL_ID = "shenko_update"
        const val NOTIFICATION_ID = 7
    }
}
