package com.pockethle.app

/**
 * Thin Kotlin facade around the Rust JNI library `libpockethle_jni.so`.
 *
 * Most native methods take the absolute path of the library root as
 * their first argument and return a JSON-encoded string. JSON is
 * used as the lingua franca to keep the FFI surface minimal — no
 * shared data classes between Rust and Kotlin.
 *
 * The session-based methods (`nativeStartGame`, `nativePollFrame`,
 * `nativeSendInput`, `nativeRequestStop`, `nativeFinishGame`) are
 * the ones [GameActivity] uses to drive the real-time emulator.
 * They do *not* round-trip JSON; they exchange a `long` opaque
 * handle and raw `ByteArray` blobs to avoid the overhead of
 * encoding the framebuffer on every poll.
 */
object NativeBridge {
    init {
        System.loadLibrary("pockethle_jni")
    }

    @JvmStatic external fun banner(): String

    /** Returns a JSON array of [GameEntry]. */
    @JvmStatic external fun listGames(libraryRoot: String): String

    /** Returns the freshly imported game as a JSON [GameEntry]. */
    @JvmStatic external fun importCab(libraryRoot: String, cabPath: String): String
    /** Imports a standalone ARM Pocket PC executable. */
    @JvmStatic external fun importExe(libraryRoot: String, exePath: String): String

    /** Returns `{"ok":true}` on success or `{"ok":false,"error":...}`. */
    @JvmStatic external fun removeGame(libraryRoot: String, id: String): String

    /** Returns the launcher config as JSON. */
    @JvmStatic external fun readConfig(libraryRoot: String): String

    /** Persists the launcher config from a JSON blob. */
    @JvmStatic external fun writeConfig(libraryRoot: String, configJson: String): String

    /** Returns the per-game settings as JSON. */
    @JvmStatic external fun readGameSettings(libraryRoot: String, id: String): String

    /** Persists the per-game settings from a JSON blob. */
    @JvmStatic external fun writeGameSettings(
        libraryRoot: String,
        id: String,
        settingsJson: String,
    ): String

    /**
     * Legacy single-shot run. Blocks the caller until the emulator
     * has finished, then returns a JSON object of the form
     * `{"ok":true,"summary":"...","frame":{"width":W,"height":H,"rgba_b64":"..."}}`
     * or `{"ok":false,"error":"..."}`. Kept for compatibility with
     * other callers but no longer used by [GameActivity] because
     * the real Unicorn backend takes long enough that the call
     * looks like a hang to the user.
     */
    @JvmStatic external fun runGame(libraryRoot: String, id: String): String

    // -------------------------------------------------------------------
    // Session-based emulator API. See `pocket-android-jni::runner`.
    // -------------------------------------------------------------------

    /**
     * Spawn a worker thread that drives the emulator for [id].
     * Returns an opaque handle (>0 on success, 0 on failure). Pass
     * the same handle into every other `native…` call below until
     * [nativeFinishGame] is called, after which the handle is
     * invalid and **must not** be passed back in.
     */
    @JvmStatic external fun nativeStartGame(libraryRoot: String, id: String): Long

    /**
     * Poll for the latest framebuffer the worker has produced.
     * Returns `null` if no new frame is available, or a flat byte
     * array `[w0 w1 w2 w3 h0 h1 h2 h3 r0 g0 b0 a0 r1 g1 b1 a1 …]`
     * (width and height as little-endian `u32`).
     */
    @JvmStatic external fun nativePollFrame(handle: Long): ByteArray?

    /** `1` while the worker is still running, `0` once it has exited. */
    @JvmStatic external fun nativeIsRunning(handle: Long): Int

    /**
     * Forward a single input event into the running emulator.
     *
     * @param kind one of [INPUT_KEY_DOWN], [INPUT_KEY_UP],
     *   [INPUT_POINTER_DOWN], [INPUT_POINTER_UP].
     * @param a virtual-key code (Win32 VK_*) for KEY_*; X
     *   coordinate (in 240×320 game space) for POINTER_*.
     * @param b ignored for KEY_*; Y coordinate for POINTER_*.
     */
    @JvmStatic external fun nativeSendInput(
        handle: Long,
        kind: Int,
        a: Int,
        b: Int,
    ): Int

    /** Ask the worker to stop at the next slice boundary. */
    @JvmStatic external fun nativeRequestStop(handle: Long)

    /**
     * Join the worker thread, free the session, and return the
     * textual summary it produced. After this call the handle is
     * invalid and **must not** be passed back in.
     */
    @JvmStatic external fun nativeFinishGame(handle: Long): String

    const val INPUT_KEY_DOWN: Int = 0
    const val INPUT_KEY_UP: Int = 1
    const val INPUT_POINTER_DOWN: Int = 2
    const val INPUT_POINTER_UP: Int = 3
}
