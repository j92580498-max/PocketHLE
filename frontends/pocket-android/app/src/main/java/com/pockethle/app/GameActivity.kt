package com.pockethle.app

import android.annotation.SuppressLint
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Rect
import android.graphics.Typeface
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.widget.ProgressBar
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.appcompat.widget.Toolbar
import java.nio.ByteBuffer
import java.nio.ByteOrder
import org.json.JSONObject

/**
 * Hosts the emulator output for one game.
 *
 * The implementation drives the emulator session-style (see
 * `pocket-android-jni::runner`): a Rust worker thread runs the
 * emulator and the activity polls the latest framebuffer on the UI
 * thread roughly every 33 ms (~30 Hz), feeds touches and virtual
 * gamepad presses straight back into the kernel, and asks the
 * worker to stop on Back / `onDestroy`. The previous single-shot
 * `NativeBridge.runGame` API blocked until the emulator exited and
 * never streamed intermediate frames, which looked like an infinite
 * loading spinner once the real Unicorn backend was wired up.
 */
class GameActivity : AppCompatActivity(), SurfaceHolder.Callback {

    private lateinit var surface: SurfaceView
    private lateinit var progress: ProgressBar
    private lateinit var status: TextView

    /** Cached handle from `nativeStartGame` (`0` once we've finished). */
    @Volatile private var session: Long = 0

    /** Most recent framebuffer the worker produced — held so we can
     * repaint after `surfaceChanged` resizes the SurfaceView even if
     * the worker has not produced a new frame yet. */
    private var lastFrame: FrameSnapshot? = null

    /**
     * j2me-loader-style FPS counter. Counts frames painted to the
     * SurfaceView in a 1-second sliding window and exposes the
     * latest value as the `displayed` text drawn in [paintFrame].
     * Toggleable via the global "Show FPS counter" preference; when
     * disabled the overlay is skipped entirely so it costs nothing.
     */
    private val fpsCounter = FpsCounter()

    /** Mirrors `LauncherConfig::show_fps`. Read once at activity
     * start; toggling the global preference mid-game does not
     * affect an already-running session, the same way j2me-loader
     * applies its `Settings.showFps` snapshot at MIDlet start. */
    private var showFps: Boolean = true

    private val mainHandler = Handler(Looper.getMainLooper())

    /** Polling tick. ~30 Hz keeps the SurfaceView smooth without
     * burning the CPU on a phone. */
    private val pollTick = object : Runnable {
        override fun run() {
            if (session == 0L) return
            val raw = NativeBridge.nativePollFrame(session)
            if (raw != null) {
                decodeFrame(raw)?.let { frame ->
                    lastFrame = frame
                    paintFrame(frame)
                }
            }
            if (NativeBridge.nativeIsRunning(session) == 0) {
                // Worker exited on its own (game called ExitProcess
                // / hit max_slices / errored out). Reap it so we
                // surface the summary in the status panel.
                finishSession()
                return
            }
            mainHandler.postDelayed(this, POLL_INTERVAL_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_game)
        setSupportActionBar(findViewById<Toolbar>(R.id.toolbar))
        supportActionBar?.setDisplayHomeAsUpEnabled(true)

        val name = intent.getStringExtra(EXTRA_GAME_NAME) ?: "PocketHLE"
        title = name

        surface = findViewById(R.id.surface)
        surface.holder.addCallback(this)
        progress = findViewById(R.id.progress)
        status = findViewById(R.id.status)

        showFps = readShowFpsPreference()

        wireSurfaceTouchInput()
        wireVirtualGamepad()

        val id = intent.getStringExtra(EXTRA_GAME_ID)
        if (id == null) {
            status.text = getString(R.string.run_failed_no_id)
            progress.visibility = View.GONE
            return
        }

        val rootDir = LibraryPaths.root(this)
        val handle = NativeBridge.nativeStartGame(rootDir, id)
        if (handle == 0L) {
            progress.visibility = View.GONE
            status.text = "Could not start emulator (see logcat)."
            return
        }
        session = handle
        status.text = "Backend: Unicorn (ARM)\nRunning…"
        // The spinner gets hidden the moment the first frame arrives.
        mainHandler.postDelayed(pollTick, POLL_INTERVAL_MS)
    }

    override fun onSupportNavigateUp(): Boolean {
        finish()
        return true
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        // Ask the emulator to wind down gracefully; the polling
        // tick will notice the worker exited and call finishSession.
        if (session != 0L) {
            NativeBridge.nativeRequestStop(session)
        }
        @Suppress("DEPRECATION")
        super.onBackPressed()
    }

    override fun onDestroy() {
        finishSession()
        mainHandler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    /**
     * Stop the emulator if it is still running, free the native
     * session, and surface the textual summary in the status panel.
     */
    private fun finishSession() {
        val handle = session
        if (handle == 0L) return
        session = 0
        progress.visibility = View.GONE
        // `nativeFinishGame` blocks on the worker thread join, so
        // do it off the UI thread to keep the UI responsive — the
        // join is usually fast (the Stop signal already fired) but
        // a long emulator slice can drag it out a few hundred ms.
        Thread {
            NativeBridge.nativeRequestStop(handle)
            val summary = NativeBridge.nativeFinishGame(handle)
            mainHandler.post {
                status.text = summary
            }
        }.start()
    }

    // -------------------------------------------------------------------
    // Surface rendering
    // -------------------------------------------------------------------

    private fun decodeFrame(raw: ByteArray): FrameSnapshot? {
        if (raw.size < 8) return null
        val buf = ByteBuffer.wrap(raw).order(ByteOrder.LITTLE_ENDIAN)
        val w = buf.int
        val h = buf.int
        if (w <= 0 || h <= 0) return null
        val pixelBytes = w * h * 4
        if (raw.size < 8 + pixelBytes) return null
        val rgba = ByteArray(pixelBytes)
        System.arraycopy(raw, 8, rgba, 0, pixelBytes)
        return FrameSnapshot(w, h, rgba)
    }

    private fun paintFrame(frame: FrameSnapshot) {
        val holder = surface.holder
        val canvas = holder.lockCanvas() ?: return
        try {
            // Hide the spinner the moment we have something to draw.
            progress.visibility = View.GONE
            canvas.drawColor(Color.BLACK)
            val bitmap = Bitmap.createBitmap(frame.width, frame.height, Bitmap.Config.ARGB_8888)
            val pixelInts = IntArray(frame.width * frame.height)
            var i = 0
            var p = 0
            while (i + 3 < frame.rgba.size) {
                val r = frame.rgba[i].toInt() and 0xff
                val g = frame.rgba[i + 1].toInt() and 0xff
                val b = frame.rgba[i + 2].toInt() and 0xff
                val a = frame.rgba[i + 3].toInt() and 0xff
                pixelInts[p++] = (a shl 24) or (r shl 16) or (g shl 8) or b
                i += 4
            }
            bitmap.setPixels(pixelInts, 0, frame.width, 0, 0, frame.width, frame.height)
            val w = canvas.width
            val h = canvas.height
            val scale = minOf(
                w.toFloat() / frame.width,
                h.toFloat() / frame.height,
            )
            val dstW = (frame.width * scale).toInt()
            val dstH = (frame.height * scale).toInt()
            val left = (w - dstW) / 2
            val top = (h - dstH) / 2
            val src = Rect(0, 0, frame.width, frame.height)
            val dst = Rect(left, top, left + dstW, top + dstH)
            canvas.drawBitmap(bitmap, src, dst, Paint(Paint.FILTER_BITMAP_FLAG))
            // j2me-loader-style FPS counter: count this frame in the
            // 1-second sliding window and (if enabled) draw the
            // latest published count as green text on a translucent
            // black background pinned to the upper-left corner of
            // the framebuffer rect.
            fpsCounter.recordFrame()
            if (showFps) {
                drawFpsOverlay(canvas, left, top)
            }
        } finally {
            holder.unlockCanvasAndPost(canvas)
        }
    }

    private fun drawFpsOverlay(canvas: android.graphics.Canvas, left: Int, top: Int) {
        val text = "FPS ${fpsCounter.lastSecondCount}"
        val bgPaint = Paint().apply {
            color = 0x90000000.toInt()
            style = Paint.Style.FILL
        }
        val textPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = 0xFF00FF00.toInt()
            textSize = FPS_TEXT_SIZE_PX
            typeface = Typeface.MONOSPACE
        }
        val padding = 6f
        val textWidth = textPaint.measureText(text)
        val metrics = textPaint.fontMetrics
        val textHeight = metrics.descent - metrics.ascent
        val rectLeft = left + padding
        val rectTop = top + padding
        val rectRight = rectLeft + textWidth + padding * 2
        val rectBottom = rectTop + textHeight + padding * 2
        canvas.drawRect(rectLeft, rectTop, rectRight, rectBottom, bgPaint)
        canvas.drawText(
            text,
            rectLeft + padding,
            rectTop + padding - metrics.ascent,
            textPaint,
        )
    }

    /**
     * Read the global "Show FPS counter" preference from the
     * library's `config.json`. Falls back to `true` if the file is
     * missing or malformed, matching `LauncherConfig::default`.
     */
    private fun readShowFpsPreference(): Boolean {
        val raw = NativeBridge.readConfig(LibraryPaths.root(this))
        return runCatching {
            val obj = JSONObject(raw)
            if (obj.has("ok") && !obj.optBoolean("ok", true)) true
            else obj.optBoolean("show_fps", true)
        }.getOrDefault(true)
    }

    /**
     * j2me-loader-inspired FPS sampler. `recordFrame()` is called
     * once per painted frame; every full second of wall-clock the
     * total is latched into [lastSecondCount] (the number drawn in
     * the overlay) and the running counter is reset. Mirrors the
     * `FpsCounter` class shipped with j2me-loader, just without
     * the periodic `Timer`: we already have a UI repaint cadence,
     * so we sample lazily.
     */
    private class FpsCounter {
        private var windowStart: Long = 0L
        private var inFlight: Int = 0

        @Volatile var lastSecondCount: Int = 0
            private set

        fun recordFrame() {
            val now = SystemClock.uptimeMillis()
            if (windowStart == 0L) {
                windowStart = now
            }
            inFlight += 1
            if (now - windowStart >= 1000L) {
                lastSecondCount = inFlight
                inFlight = 0
                windowStart = now
            }
        }
    }

    // -------------------------------------------------------------------
    // SurfaceHolder.Callback
    // -------------------------------------------------------------------

    override fun surfaceCreated(holder: SurfaceHolder) {
        lastFrame?.let { paintFrame(it) }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        lastFrame?.let { paintFrame(it) }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) = Unit

    // -------------------------------------------------------------------
    // Input plumbing
    // -------------------------------------------------------------------

    /**
     * Forward any touches on the framebuffer surface as
     * `WM_LBUTTONDOWN` / `WM_LBUTTONUP` events with stylus
     * coordinates in 240×320 game space — the same mapping the
     * desktop GUI uses.
     */
    @SuppressLint("ClickableViewAccessibility")
    private fun wireSurfaceTouchInput() {
        surface.setOnTouchListener { v, event ->
            val handle = session
            if (handle == 0L) return@setOnTouchListener false
            val frame = lastFrame ?: return@setOnTouchListener true
            val mapped = mapTouchToGame(v, event, frame) ?: return@setOnTouchListener true
            val (gx, gy) = mapped
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    NativeBridge.nativeSendInput(
                        handle,
                        NativeBridge.INPUT_POINTER_DOWN,
                        gx,
                        gy,
                    )
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    NativeBridge.nativeSendInput(
                        handle,
                        NativeBridge.INPUT_POINTER_UP,
                        gx,
                        gy,
                    )
                    v.performClick()
                }
            }
            true
        }
    }

    /**
     * j2me-loader-inspired virtual gamepad: a D-pad on the left and
     * three action / soft-key buttons on the right. The button views
     * live in `activity_game.xml`. We listen for touch events
     * directly so the WM_KEYDOWN/WM_KEYUP pair is fired as the user
     * presses and releases the button — not just once per click.
     */
    private fun wireVirtualGamepad() {
        bindVk(R.id.btn_up, VK_UP)
        bindVk(R.id.btn_down, VK_DOWN)
        bindVk(R.id.btn_left, VK_LEFT)
        bindVk(R.id.btn_right, VK_RIGHT)
        bindVk(R.id.btn_action, VK_RETURN)
        bindVk(R.id.btn_a, VK_A)
        bindVk(R.id.btn_b, VK_B)
        bindVk(R.id.btn_c, VK_C)
        bindVk(R.id.btn_soft1, VK_TSOFT1)
        bindVk(R.id.btn_soft2, VK_TSOFT2)
    }

    @SuppressLint("ClickableViewAccessibility")
    private fun bindVk(viewId: Int, vk: Int) {
        val btn = findViewById<View?>(viewId) ?: return
        btn.setOnTouchListener { v, event ->
            val handle = session
            if (handle == 0L) return@setOnTouchListener false
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    NativeBridge.nativeSendInput(
                        handle,
                        NativeBridge.INPUT_KEY_DOWN,
                        vk,
                        0,
                    )
                    v.isPressed = true
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    NativeBridge.nativeSendInput(
                        handle,
                        NativeBridge.INPUT_KEY_UP,
                        vk,
                        0,
                    )
                    v.isPressed = false
                    v.performClick()
                }
            }
            true
        }
    }

    /**
     * Map a screen-space touch on the SurfaceView into the
     * 240×320 game-space coordinates the kernel expects. Returns
     * `null` if the touch landed in the letter-box around the
     * scaled framebuffer.
     */
    private fun mapTouchToGame(
        v: View,
        event: MotionEvent,
        frame: FrameSnapshot,
    ): Pair<Int, Int>? {
        val viewW = v.width.toFloat()
        val viewH = v.height.toFloat()
        if (viewW <= 0 || viewH <= 0) return null
        val scale = minOf(viewW / frame.width, viewH / frame.height)
        val drawnW = frame.width * scale
        val drawnH = frame.height * scale
        val dx = event.x - (viewW - drawnW) / 2f
        val dy = event.y - (viewH - drawnH) / 2f
        if (dx < 0f || dy < 0f || dx >= drawnW || dy >= drawnH) return null
        val gx = (dx / scale).toInt().coerceIn(0, frame.width - 1)
        val gy = (dy / scale).toInt().coerceIn(0, frame.height - 1)
        return gx to gy
    }

    private data class FrameSnapshot(
        val width: Int,
        val height: Int,
        val rgba: ByteArray,
    )

    companion object {
        const val EXTRA_GAME_ID = "com.pockethle.app.EXTRA_GAME_ID"
        const val EXTRA_GAME_NAME = "com.pockethle.app.EXTRA_GAME_NAME"

        // Win32 virtual-key codes — same set the desktop GUI uses.
        private const val VK_UP = 0x26
        private const val VK_DOWN = 0x28
        private const val VK_LEFT = 0x25
        private const val VK_RIGHT = 0x27
        private const val VK_RETURN = 0xD4 // Pocket PC center / Start.
        private const val VK_A = 0xD1
        private const val VK_B = 0xD2
        private const val VK_C = 0xD3
        private const val VK_TSOFT1 = 0xC1
        private const val VK_TSOFT2 = 0xC2

        /** Polling cadence in ms. 33 ≈ 30 Hz. */
        private const val POLL_INTERVAL_MS = 33L

        /**
         * Font size of the FPS overlay drawn in [drawFpsOverlay].
         * Picked to roughly match the height of j2me-loader's
         * `FpsCounter` text on a phone screen (~14sp at xxhdpi),
         * but expressed in pixels because the overlay is painted
         * directly on the SurfaceView.
         */
        private const val FPS_TEXT_SIZE_PX = 28f
    }
}
