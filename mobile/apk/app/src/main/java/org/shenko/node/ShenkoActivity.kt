package org.shenko.node

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

/** User-visible entry to the existing app-owned foreground service. */
class ShenkoActivity : Activity() {
    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        val message = TextView(this).apply { setText(R.string.node_start_hint) }
        fun startNode() {
            try {
                ShenkoNodeService.start(this)
                message.setText(R.string.node_start_requested)
            } catch (failure: SecurityException) {
                message.text = getString(R.string.node_start_failed, failure.message)
            } catch (failure: IllegalStateException) {
                message.text = getString(R.string.node_start_failed, failure.message)
            }
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            val density = resources.displayMetrics.density
            setPadding((24 * density).toInt(), (32 * density).toInt(),
                (24 * density).toInt(), (24 * density).toInt())
            addView(message)
            addView(Button(this@ShenkoActivity).apply {
                setText(R.string.node_start)
                setOnClickListener { startNode() }
            })
            addView(Button(this@ShenkoActivity).apply {
                setText(R.string.node_open_ui)
                setOnClickListener {
                    try {
                        startActivity(Intent(Intent.ACTION_VIEW,
                            Uri.parse("http://127.0.0.1:${ShenkoNodeService.DEFAULT_PORT}")))
                    } catch (failure: ActivityNotFoundException) {
                        message.setText(R.string.node_browser_unavailable)
                    }
                }
            })
        })
        // Configuration changes keep the existing service and native owner.
        if (state == null) startNode()
    }
}
