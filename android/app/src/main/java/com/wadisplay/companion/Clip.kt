package com.wadisplay.companion

import android.content.ClipData
import android.os.Binder
import android.os.IBinder
import android.os.Parcel
import java.io.DataInputStream
import java.io.EOFException
import java.io.FileDescriptor
import java.io.FileOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Clipboard helper, run by wadisplay-server as the adb shell user:
 * `CLASSPATH=<this apk> app_process / com.wadisplay.companion.Clip`. Shell holds
 * READ_CLIPBOARD_IN_BACKGROUND; apps, including EdgeService, cannot read the clipboard from the
 * background. Talks to the hidden IClipboard service directly.
 *
 * Frames on stdin/stdout: u32 little-endian length, a type byte, UTF-8 text.
 * In: 'G' = report the clip if it changed since the last report, 'S' + text = set the clip.
 * Out: 'T' + text. Exits when stdin closes.
 */
object Clip {
    private const val PKG = "com.android.shell"
    private const val MAX = 1 shl 20 // proto::MAX_CLIP

    @Volatile private var changed = false
    /** The text last reported or set; reading a clip shell set itself shows no access toast. */
    private var last: String? = null

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

        val input = DataInputStream(System.`in`.buffered())
        val out = FileOutputStream(FileDescriptor.out)
        while (true) {
            val b = try {
                ByteArray(Integer.reverseBytes(input.readInt())).also { input.readFully(it) }
            } catch (e: EOFException) {
                return
            }
            try {
                when (b.firstOrNull()) {
                    'G'.code.toByte() -> if (changed) {
                        changed = false // before reading: a change during the read is caught next time
                        val clip = call(cb, "getPrimaryClip", null) as ClipData?
                        val text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()
                        val bytes = text?.toByteArray() ?: ByteArray(0)
                        if (text != null && text != last && bytes.size <= MAX) {
                            last = text
                            val f = ByteBuffer.allocate(5 + bytes.size).order(ByteOrder.LITTLE_ENDIAN)
                                .putInt(1 + bytes.size).put('T'.code.toByte()).put(bytes)
                            out.write(f.array())
                            out.flush()
                        }
                    }
                    'S'.code.toByte() -> {
                        val text = String(b, 1, b.size - 1)
                        last = text
                        call(cb, "setPrimaryClip", ClipData.newPlainText("wadisplay", text))
                    }
                }
            } catch (e: Exception) {
                System.err.println("clip helper: $e")
            }
        }
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
