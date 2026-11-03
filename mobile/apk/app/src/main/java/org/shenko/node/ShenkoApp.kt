package org.shenko.node

import android.app.Application
import java.security.MessageDigest
import java.util.UUID

/**
 * App-wide state that must survive configuration changes but never leave the
 * uid: the install binding digest (probe identity) and the accessibility
 * event buffer (human-activity feed for external-takeover detection).
 *
 * The binding digest is a salted digest of an app-private install id. It
 * proves *this app install* to the node's probe; it is not a credential and
 * grants nothing.
 */
class ShenkoApp : Application() {

    val installBindingDigest: String by lazy {
        val prefs = getSharedPreferences("shenko_binding", MODE_PRIVATE)
        var installId = prefs.getString("install_id", null)
        if (installId == null) {
            installId = UUID.randomUUID().toString()
            prefs.edit().putString("install_id", installId).apply()
        }
        val digest = MessageDigest.getInstance("SHA-256")
            .digest("shenko-mobile-binding:$installId".toByteArray())
        digest.joinToString("") { "%02x".format(it) }
    }

    /** Accessibility events with wall-clock ms, bounded ring for takeover detection. */
    val humanActivityEvents = ArrayDeque<Long>()

    override fun onCreate() {
        super.onCreate()
        instance = this
    }

    companion object {
        lateinit var instance: ShenkoApp
            private set
    }
}
