package com.wadisplay.companion

import android.app.Activity
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.view.Gravity
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView

/** Connection status plus a text field for testing the PC keyboard. */
class MainActivity : Activity() {
    private lateinit var statusView: TextView
    private val handler = Handler(Looper.getMainLooper())
    private val tick = object : Runnable {
        override fun run() {
            update()
            handler.postDelayed(this, 500)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pad = (16 * resources.displayMetrics.density).toInt()
        statusView = TextView(this).apply { textSize = 18f }
        val field = EditText(this).apply {
            hint = "Typing test: type here with the PC keyboard"
            minLines = 6
            gravity = Gravity.TOP
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            addView(TextView(this@MainActivity).apply {
                text = "wadisplay companion"
                textSize = 24f
            })
            addView(statusView)
            addView(field, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        })
        field.requestFocus()
    }

    override fun onResume() {
        super.onResume()
        handler.post(tick)
    }

    override fun onPause() {
        handler.removeCallbacks(tick)
        super.onPause()
    }

    private fun update() {
        val age = SystemClock.elapsedRealtime() - EdgeService.statusAt
        statusView.text = when {
            !Settings.canDrawOverlays(this) ->
                "Overlay permission missing. Start wadisplay.exe on the PC; it grants it over adb."
            EdgeService.statusAt == 0L || age > 3000 ->
                "wadisplay-server is not running. Start wadisplay.exe on the PC."
            else -> EdgeService.status
        }
    }
}
