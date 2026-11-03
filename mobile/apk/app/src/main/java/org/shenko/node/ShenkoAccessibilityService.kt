package org.shenko.node

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent

/**
 * User-enabled accessibility surface: node-tree observation and gesture
 * dispatch. Off unless the user explicitly enables it in system settings;
 * the probe reports it as its own parallel authority, never as an elevation
 * step toward anything else.
 */
class ShenkoAccessibilityService : AccessibilityService() {

    override fun onServiceConnected() {
        instance = this
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent) {
        val app = ShenkoApp.instance
        synchronized(app.humanActivityEvents) {
            app.humanActivityEvents.addLast(event.eventTime)
            while (app.humanActivityEvents.size > 512) {
                app.humanActivityEvents.removeFirst()
            }
        }
    }

    override fun onInterrupt() {}

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        if (instance === this) instance = null
        return super.onUnbind(intent)
    }

    fun canRetrieveWindowContent(): Boolean =
        serviceInfo?.canRetrieveWindowContent == true

    fun dispatchTap(x: Float, y: Float): Boolean {
        val path = Path().apply { moveTo(x, y) }
        val stroke = GestureDescription.StrokeDescription(path, 0, 1)
        val gesture = GestureDescription.Builder().addStroke(stroke).build()
        return dispatchGesture(gesture, null, null)
    }

    fun dispatchSwipe(x1: Float, y1: Float, x2: Float, y2: Float, durationMs: Long): Boolean {
        val path = Path().apply { moveTo(x1, y1); lineTo(x2, y2) }
        val stroke = GestureDescription.StrokeDescription(path, 0, durationMs.coerceAtLeast(1))
        val gesture = GestureDescription.Builder().addStroke(stroke).build()
        return dispatchGesture(gesture, null, null)
    }

    companion object {
        @Volatile
        var instance: ShenkoAccessibilityService? = null
            private set
    }
}
