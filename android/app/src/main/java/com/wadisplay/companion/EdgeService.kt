package com.wadisplay.companion

import android.app.KeyguardManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.graphics.PixelFormat
import android.graphics.Point
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.StrictMode
import android.os.SystemClock
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.WindowManager
import java.io.IOException
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Keeps a 1 px overlay strip on the tablet's return edge and reports pointer hover in it,
 * display size/rotation and screen/keyguard state to wadisplay-server on 127.0.0.1:27184.
 * Started by the server: am start-foreground-service --es tok <hex> --es edge left|right|top|bottom
 */
class EdgeService : Service() {
    companion object {
        @Volatile var status = ""
        @Volatile var statusAt = 0L
    }

    private lateinit var wm: WindowManager
    private lateinit var power: PowerManager
    private lateinit var keyguard: KeyguardManager
    private val handler = Handler(Looper.getMainLooper())
    private val sock = DatagramSocket()
    private val server = InetSocketAddress("127.0.0.1", 27184)
    private var token = ByteArray(16)
    private var edge = ""
    private var strip: View? = null
    private var stripShown = false
    private var hover = false
    private var hx = 0
    private var hy = 0
    private var wifiLock: WifiManager.WifiLock? = null

    private val heartbeat = object : Runnable {
        override fun run() {
            send()
            handler.postDelayed(this, 1000)
        }
    }

    private val screenReceiver = object : BroadcastReceiver() {
        override fun onReceive(c: Context, i: Intent) = send()
    }

    override fun onBind(intent: Intent?) = null

    override fun onCreate() {
        super.onCreate()
        // Only localhost datagrams of 29 bytes; not worth a thread hop on every hover event.
        StrictMode.setThreadPolicy(StrictMode.ThreadPolicy.LAX)
        wm = getSystemService(WindowManager::class.java)
        power = getSystemService(PowerManager::class.java)
        keyguard = getSystemService(KeyguardManager::class.java)
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel("edge", "Edge strip", NotificationManager.IMPORTANCE_MIN))
        val n = Notification.Builder(this, "edge")
            .setSmallIcon(R.drawable.ic_stat_wadisplay)
            .setContentTitle("wadisplay")
            .setContentText("Watching the screen edge for the PC mouse")
            .build()
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(1, n)
        }
        @Suppress("DEPRECATION")
        wifiLock = getSystemService(WifiManager::class.java)
            .createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "wadisplay").apply { acquire() }
        val filter = IntentFilter().apply {
            addAction(Intent.ACTION_SCREEN_ON)
            addAction(Intent.ACTION_SCREEN_OFF)
            addAction(Intent.ACTION_USER_PRESENT)
        }
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(screenReceiver, filter, RECEIVER_NOT_EXPORTED)
        } else {
            registerReceiver(screenReceiver, filter)
        }
        Thread(::receiveStatus).apply { isDaemon = true }.start()
        handler.post(heartbeat)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        intent?.getStringExtra("tok")?.let { hex ->
            token = ByteArray(16) { i -> hex.substring(2 * i, 2 * i + 2).toInt(16).toByte() }
        }
        val e = intent?.getStringExtra("edge") ?: edge.ifEmpty { "left" }
        if (e != edge || strip == null) {
            edge = e
            addStrip()
        }
        send()
        return START_REDELIVER_INTENT
    }

    override fun onDestroy() {
        handler.removeCallbacks(heartbeat)
        strip?.let { wm.removeView(it) }
        unregisterReceiver(screenReceiver)
        wifiLock?.release()
        sock.close()
        super.onDestroy()
    }

    private fun addStrip() {
        strip?.let { wm.removeView(it) }
        val horizontal = edge == "top" || edge == "bottom"
        val lp = WindowManager.LayoutParams(
            if (horizontal) WindowManager.LayoutParams.MATCH_PARENT else 1,
            if (horizontal) 1 else WindowManager.LayoutParams.MATCH_PARENT,
            WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
            WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
                WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or
                WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
            PixelFormat.TRANSLUCENT,
        )
        lp.gravity = when (edge) {
            "right" -> Gravity.RIGHT or Gravity.TOP
            "bottom" -> Gravity.BOTTOM or Gravity.LEFT
            else -> Gravity.LEFT or Gravity.TOP
        }
        lp.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
        if (Build.VERSION.SDK_INT >= 30) lp.fitInsetsTypes = 0 // cover the full edge, bars included
        val v = object : View(this) {
            override fun onWindowVisibilityChanged(visibility: Int) {
                super.onWindowVisibilityChanged(visibility)
                stripShown = visibility == VISIBLE
                send()
            }

            override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) = send()
        }
        v.setOnHoverListener { _, e ->
            when (e.actionMasked) {
                MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> {
                    hover = true
                    hx = e.rawX.toInt()
                    hy = e.rawY.toInt()
                }
                MotionEvent.ACTION_HOVER_EXIT -> hover = false
            }
            send()
            true
        }
        wm.addView(v, lp)
        strip = v
    }

    /** token[16] hover x y w h rotation screen_on locked visible, little-endian u16s. */
    private fun send() {
        val size = Point()
        @Suppress("DEPRECATION") val display = wm.defaultDisplay
        @Suppress("DEPRECATION") display.getRealSize(size)
        val on = power.isInteractive
        val locked = keyguard.isKeyguardLocked
        val flag = { b: Boolean -> (if (b) 1 else 0).toByte() }
        val b = ByteBuffer.allocate(29).order(ByteOrder.LITTLE_ENDIAN)
            .put(token).put(flag(hover))
            .putShort(hx.toShort()).putShort(hy.toShort())
            .putShort(size.x.toShort()).putShort(size.y.toShort())
            .put(display.rotation.toByte()).put(flag(on)).put(flag(locked))
            .put(flag(stripShown && on && !locked))
        try {
            sock.send(DatagramPacket(b.array(), b.capacity(), server))
        } catch (_: IOException) {
        }
    }

    private fun receiveStatus() {
        val buf = ByteArray(512)
        while (true) {
            val p = DatagramPacket(buf, buf.size)
            try {
                sock.receive(p)
            } catch (_: IOException) {
                return
            }
            status = String(buf, 0, p.length)
            statusAt = SystemClock.elapsedRealtime()
        }
    }
}
