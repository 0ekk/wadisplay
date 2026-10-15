package com.wadisplay.companion

import android.content.ClipData
import android.content.ClipDescription
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.Binder
import android.os.IBinder
import android.os.Parcel
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.EOFException
import java.io.File
import java.io.FileDescriptor
import java.io.FileOutputStream
import java.lang.reflect.InvocationTargetException
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * Clipboard helper, run by wadisplay-server as the adb shell user:
 * `CLASSPATH=<this apk> app_process / com.wadisplay.companion.Clip`. Shell holds
 * READ_CLIPBOARD_IN_BACKGROUND; apps, including EdgeService, cannot read the clipboard from the
 * background. Talks to the hidden IClipboard service directly.
 *
 * Frames on stdin/stdout: u32 little-endian length, then the bytes. In: 'G' = report the clip if
 * it changed since the last report, or a clip to set. A clip is 'T' + UTF-8 text or 'I' + an image
 * file (PNG from the PC; PNG or JPEG from here). Out: clips. Exits when stdin closes.
 * Images on the tablet go through ClipProvider, written with `content write`; images from other
 * apps are read with `content read`, which works once the clipboard granted shell the URI.
 */
object Clip {
    private const val PKG = "com.android.shell"
    private const val MAX_TEXT = 1 shl 20 // proto::MAX_TEXT
    private const val MAX_IMAGE = 32 shl 20 // proto::MAX_IMAGE
    private const val CONTENT = "/system/bin/content"

    @Volatile private var changed = false
    /** The text or image URI last reported or set; reading a clip shell set itself shows no toast. */
    private var last: Any? = null
    private val out = FileOutputStream(FileDescriptor.out)

    @JvmStatic
    fun main(args: Array<String>) {
        val binder = Class.forName("android.os.ServiceManager").getMethod("getService", String::class.java)
            .invoke(null, "clipboard") as IBinder
        val cb = Class.forName("android.content.IClipboard\$Stub").getMethod("asInterface", IBinder::class.java)
            .invoke(null, binder)!!
        // A bare Binder stands in for IOnPrimaryClipChangedListener; its only call is oneway.
        val listener = object : Binder() {
            override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
                if (code == FIRST_CALL_TRANSACTION) changed = true
                return true
            }
        }
        val li = Class.forName("android.content.IOnPrimaryClipChangedListener\$Stub")
            .getMethod("asInterface", IBinder::class.java).invoke(null, listener)
        call(cb, "addPrimaryClipChangedListener", li)
        System.err.println("clip helper ready")

        // One worker keeps sets and reports in order while stdin is read on, so the server's
        // writes never wait for an image to be stored.
        val work = Executors.newSingleThreadExecutor()
        val input = DataInputStream(System.`in`.buffered())
        while (true) {
            val b = try {
                ByteArray(Integer.reverseBytes(input.readInt())).also { input.readFully(it) }
            } catch (e: EOFException) {
                // Finish first: app_process closes the binder driver as soon as main returns.
                work.shutdown()
                work.awaitTermination(1, TimeUnit.MINUTES)
                return
            }
            work.execute {
                try {
                    when (b.firstOrNull()) {
                        'G'.code.toByte() -> if (changed) {
                            changed = false // before reading: a change during the read is caught next time
                            report(cb)
                        }
                        'T'.code.toByte() -> {
                            val text = String(b, 1, b.size - 1)
                            last = text
                            call(cb, "setPrimaryClip", ClipData.newPlainText("wadisplay", text))
                        }
                        'I'.code.toByte() -> setImage(cb, b)
                    }
                } catch (e: Exception) {
                    System.err.println("clip helper: ${(e as? InvocationTargetException)?.targetException ?: e}")
                }
            }
        }
    }

    private fun report(cb: Any) {
        val clip = call(cb, "getPrimaryClip", null) as ClipData? ?: return
        val item = clip.takeIf { it.itemCount > 0 }?.getItemAt(0) ?: return
        val text = item.text?.toString()
        val uri = item.uri
        if (text != null) {
            val bytes = text.toByteArray()
            if (text != last && bytes.size <= MAX_TEXT) {
                last = text
                send('T', bytes)
            }
        } else if (uri != null && uri != last && uri.authority != ClipProvider.AUTHORITY &&
            clip.description.hasMimeType("image/*")) {
            last = uri
            image(uri)?.let { send('I', it) }
        }
    }

    private fun send(kind: Char, data: ByteArray) {
        val f = ByteBuffer.allocate(5 + data.size).order(ByteOrder.LITTLE_ENDIAN)
            .putInt(1 + data.size).put(kind.code.toByte()).put(data)
        out.write(f.array())
        out.flush()
    }

    /** Stores the PC's PNG in ClipProvider under a new name (so no app shows an old copy) and puts it
     *  on the clipboard. */
    private fun setImage(cb: Any, b: ByteArray) {
        val uri = Uri.parse("content://${ClipProvider.AUTHORITY}/wadisplay-${System.currentTimeMillis()}.png")
        val p = ProcessBuilder(CONTENT, "write", "--uri", "$uri")
            .redirectOutput(File("/dev/null")).redirectError(ProcessBuilder.Redirect.INHERIT).start()
        p.outputStream.use { it.write(b, 1, b.size - 1) }
        if (p.waitFor() != 0) return System.err.println("clip helper: content write exited ${p.exitValue()}")
        last = uri
        call(cb, "setPrimaryClip", ClipData(ClipDescription("wadisplay", arrayOf("image/png")), ClipData.Item(uri)))
    }

    /** Another app's clipboard image: PNG or JPEG as is, other formats as PNG; null if unreadable. */
    private fun image(uri: Uri): ByteArray? {
        val p = ProcessBuilder(CONTENT, "read", "--uri", "$uri")
            .redirectError(ProcessBuilder.Redirect.INHERIT).start()
        val buf = ByteArrayOutputStream()
        p.inputStream.use { s ->
            val chunk = ByteArray(64 shl 10)
            while (buf.size() <= MAX_IMAGE) {
                val n = s.read(chunk)
                if (n < 0) break
                buf.write(chunk, 0, n)
            }
        }
        p.destroy()
        val b = buf.toByteArray()
        val png = b.size > 4 && b[0] == 0x89.toByte() && b[1] == 'P'.code.toByte()
        val jpeg = b.size > 3 && b[0] == 0xFF.toByte() && b[1] == 0xD8.toByte()
        if (b.isEmpty() || b.size > MAX_IMAGE) return null.also { System.err.println("clip helper: image of ${b.size} bytes skipped") }
        if (png || jpeg) return b
        val bmp = BitmapFactory.decodeByteArray(b, 0, b.size) ?: return null
        return ByteArrayOutputStream().also { bmp.compress(Bitmap.CompressFormat.PNG, 100, it) }.toByteArray()
            .takeIf { it.size <= MAX_IMAGE }
    }

    /** Calls an IClipboard method whose parameters vary by Android version: the first String is the
     *  calling package, a later one the attribution tag (null), ints are user/device ids (0). */
    private fun call(o: Any, name: String, arg: Any?): Any? {
        val m = o.javaClass.methods.first { it.name == name }
        var pkgDone = false
        val a = m.parameterTypes.map { t ->
            when (t) {
                String::class.java -> (if (pkgDone) null else PKG).also { pkgDone = true }
                Int::class.javaPrimitiveType -> 0
                else -> arg
            }
        }
        return m.invoke(o, *a.toTypedArray())
    }
}
