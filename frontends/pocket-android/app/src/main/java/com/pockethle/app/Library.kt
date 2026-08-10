package com.pockethle.app

import android.content.Context
import java.io.File

/**
 * Helpers to compute the on-device library root and copy ContentResolver
 * URIs to a real file on disk that the native crate can open with `fs`.
 */
object LibraryPaths {
    /**
     * Root directory of the local PocketHLE library. Sits under
     * the app's external files dir so users can browse/back-up
     * `library.json`, `config.json`, and the per-game `extracted/`
     * folders without root.
     */
    fun root(ctx: Context): String {
        val dir = ctx.getExternalFilesDir(null) ?: ctx.filesDir
        val library = File(dir, "library")
        if (!library.exists()) library.mkdirs()
        val path = library.absolutePath
        NativeBridge.configureLogger(path)
        return path
    }

    /**
     * Copy a `content://` URI (e.g. from the system file picker) to a
     * temp file under `cacheDir/imports/` so the Rust side can `open()`
     * it normally. Returns the absolute path.
     */
    fun copyUriToCache(ctx: Context, uri: android.net.Uri, suggestedName: String): File {
        val cacheRoot = File(ctx.cacheDir, "imports").also { it.mkdirs() }
        val safe = suggestedName
            .lowercase()
            .replace(Regex("[^a-z0-9._-]"), "_")
            .ifEmpty { "import.cab" }
        val dest = File(cacheRoot, safe)
        ctx.contentResolver.openInputStream(uri)?.use { input ->
            dest.outputStream().use { output ->
                input.copyTo(output)
            }
        } ?: throw java.io.IOException("could not open input stream for $uri")
        return dest
    }
}
