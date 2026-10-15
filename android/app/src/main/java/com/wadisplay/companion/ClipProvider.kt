package com.wadisplay.companion

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.os.ParcelFileDescriptor.MODE_CREATE
import android.os.ParcelFileDescriptor.MODE_READ_ONLY
import android.os.ParcelFileDescriptor.MODE_TRUNCATE
import android.os.ParcelFileDescriptor.MODE_WRITE_ONLY
import android.provider.OpenableColumns
import java.io.File
import java.io.FileNotFoundException

/**
 * The PC's clipboard image on the tablet. The clipboard helper (shell, which holds DUMP) writes it
 * with `content write` and puts its URI on the clipboard; the clipboard then lets the pasting app
 * read it. Only the newest image is kept.
 */
class ClipProvider : ContentProvider() {
    private val dir get() = File(context!!.cacheDir, "clip")

    private fun file(uri: Uri): File =
        File(dir, uri.lastPathSegment?.takeIf { NAME.matches(it) } ?: throw FileNotFoundException("$uri"))

    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor {
        val f = file(uri)
        if (mode == "r") return ParcelFileDescriptor.open(f, MODE_READ_ONLY)
        dir.mkdirs()
        dir.listFiles()?.forEach { it.delete() } // a reader that has it open keeps its copy
        return ParcelFileDescriptor.open(f, MODE_WRITE_ONLY or MODE_CREATE or MODE_TRUNCATE)
    }

    override fun getType(uri: Uri) = "image/png"

    override fun query(uri: Uri, projection: Array<String>?, selection: String?, args: Array<String>?,
                       sort: String?): Cursor {
        val f = file(uri)
        return MatrixCursor(arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)).apply {
            addRow(arrayOf(f.name, f.length()))
        }
    }

    override fun onCreate() = true
    override fun insert(uri: Uri, values: ContentValues?): Uri? = null
    override fun update(uri: Uri, values: ContentValues?, selection: String?, args: Array<String>?) = 0
    override fun delete(uri: Uri, selection: String?, args: Array<String>?) = 0

    companion object {
        const val AUTHORITY = "com.wadisplay.companion.clip"
        private val NAME = Regex("""wadisplay-\d+\.png""")
    }
}
