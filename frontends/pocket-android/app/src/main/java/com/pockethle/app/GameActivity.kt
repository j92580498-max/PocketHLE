package com.pockethle.app

import android.annotation.SuppressLint
import android.opengl.GLES20
import android.opengl.GLSurfaceView
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.MotionEvent
import android.view.View
import android.widget.ImageButton
import android.widget.ProgressBar
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.appcompat.widget.Toolbar
import java.nio.ByteBuffer
import java.nio.ByteOrder
import org.json.JSONObject
import android.content.pm.ActivityInfo

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
class GameActivity : AppCompatActivity() {

    private lateinit var surface: GLSurfaceView
    private lateinit var progress: ProgressBar
    private lateinit var status: TextView
    private lateinit var fpsOverlay: TextView
    private lateinit var toolbar: Toolbar
    private lateinit var fullscreenButton: ImageButton
    private lateinit var gameControls: View
    private lateinit var glRenderer: FrameRenderer

    /** Cached handle from `nativeStartGame` (`0` once we've finished). */
    @Volatile private var session: Long = 0

    /** Most recent framebuffer the worker produced — held so we can
     * repaint after `surfaceChanged` resizes the SurfaceView even if
     * the worker has not produced a new frame yet. */
    private var lastFrame: FrameSnapshot? = null
    private var frameBuffer: ByteArray = ByteArray(0)

    /**
     * j2me-loader-style FPS counter. Counts frames painted to the
     * SurfaceView in a 1-second sliding window and exposes the
     * latest value as the `displayed` text drawn in [paintFrame].
     * Toggleable via the global "Show FPS counter" preference; when
     * disabled the overlay is skipped entirely so it costs nothing.
     */
    private val fpsCounter = FpsCounter()
    @Volatile private var audioRunning = false
    private var audioThread: Thread? = null
    private var audioTrack: AudioTrack? = null

    /** Mirrors `LauncherConfig::show_fps`. Read once at activity
     * start; toggling the global preference mid-game does not
     * affect an already-running session, the same way j2me-loader
     * applies its `Settings.showFps` snapshot at MIDlet start. */
    private var showFps: Boolean = true

    private var fullscreen: Boolean = false
    private var fullscreenMode: String = "with_controls"

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
        val config = readLauncherConfig()
        fullscreen = config.fullscreen
        fullscreenMode = config.fullscreenMode
        requestedOrientation = orientationFor(config.orientation)
        setContentView(R.layout.activity_game)
        toolbar = findViewById(R.id.toolbar)
        setSupportActionBar(toolbar)
        fullscreenButton = findViewById(R.id.btn_fullscreen)
        fullscreenButton.setOnClickListener { toggleFullscreenWithControls() }
        gameControls = findViewById(R.id.game_controls)
        updateFullscreenLayout()
        if (fullscreen) {
            toolbar.visibility = View.GONE
            hideSystemBars()
        }
        supportActionBar?.setDisplayHomeAsUpEnabled(true)

        val name = intent.getStringExtra(EXTRA_GAME_NAME) ?: "PocketHLE"
        title = name

        surface = findViewById(R.id.surface)
        glRenderer = FrameRenderer()
        surface.setEGLContextClientVersion(2)
        surface.setRenderer(glRenderer)
        surface.renderMode = GLSurfaceView.RENDERMODE_WHEN_DIRTY
        progress = findViewById(R.id.progress)
        fpsOverlay = findViewById(R.id.fps_overlay)
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
        startAudio(handle)
        status.text = "Backend: Unicorn (ARM)\nRunning…"
        // The spinner gets hidden the moment the first frame arrives.
        mainHandler.postDelayed(pollTick, POLL_INTERVAL_MS)
    }

    override fun onResume() {
        super.onResume()
        val handle = session
        if (handle != 0L && !audioRunning) startAudio(handle)
    }

    override fun onPause() {
        stopAudio()
        super.onPause()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && fullscreen) hideSystemBars()
    }

    override fun onSupportNavigateUp(): Boolean {
        finish()
        return true
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (fullscreen) {
            exitFullscreenWithControls()
            return
        }
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
        surface.onPause()
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
        stopAudio()
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

    private fun startAudio(handle: Long) {
        stopAudio()
        audioRunning = true
        audioThread = Thread({
            var track: AudioTrack? = null
            try {
                var packed = 0L
                while (audioRunning && session == handle && packed == 0L) {
                    packed = NativeBridge.nativeAudioFormat(handle)
                    if (packed == 0L) Thread.sleep(20)
                }
                if (!audioRunning || session != handle || packed == 0L) {
                    android.util.Log.w("PocketHLE", "Audio format was not announced by the guest")
                    return@Thread
                }
                val rate = (packed ushr 16).toInt().coerceIn(8000, 48000)
                val channels = (packed and 0xffff).toInt().coerceIn(1, 2)
                val channelMask = if (channels == 2) AudioFormat.CHANNEL_OUT_STEREO else AudioFormat.CHANNEL_OUT_MONO
                val minBuffer = AudioTrack.getMinBufferSize(rate, channelMask, AudioFormat.ENCODING_PCM_16BIT)
                val bufferSize = maxOf(minBuffer.takeIf { it > 0 } ?: 0, rate * channels * 2 / 2, 4096)
                track = AudioTrack.Builder()
                    .setAudioAttributes(AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_GAME)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                        .build())
                    .setAudioFormat(AudioFormat.Builder()
                        .setSampleRate(rate)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setChannelMask(channelMask)
                        .build())
                    .setBufferSizeInBytes(bufferSize)
                    .setTransferMode(AudioTrack.MODE_STREAM)
                    .build()
                if (track?.state != AudioTrack.STATE_INITIALIZED) {
                    android.util.Log.e("PocketHLE", "AudioTrack was not initialized")
                    return@Thread
                }
                audioTrack = track
                track.play()
                android.util.Log.i("PocketHLE", "AudioTrack started: ${rate}Hz, ${channels}ch, buffer=${bufferSize}B")
                while (audioRunning && session == handle) {
                    val pcm = NativeBridge.nativePollAudio(handle, 4096)
                    if (pcm != null && pcm.isNotEmpty()) {
                        writeAudio(track, pcm)
                    } else {
                        writeSilence(track, maxOf(channels * rate / 200, 256))
                        Thread.sleep(5)
                    }
                }
            } catch (error: Throwable) {
                android.util.Log.e("PocketHLE", "AudioTrack playback failed", error)
            } finally {
                try { track?.pause() } catch (_: Throwable) {}
                try { track?.flush() } catch (_: Throwable) {}
                try { track?.release() } catch (_: Throwable) {}
                if (audioTrack === track) audioTrack = null
            }
        }, "pockethle-audio")
        audioThread?.start()
    }

    private fun stopAudio() {
        audioRunning = false
        audioTrack?.pause()
        audioTrack?.flush()
        val oldThread = audioThread
        audioThread = null
        audioTrack = null
        oldThread?.interrupt()
        if (oldThread !== Thread.currentThread()) {
            try { oldThread?.join(250) } catch (_: InterruptedException) { Thread.currentThread().interrupt() }
        }
    }

    private fun writeAudio(track: AudioTrack, pcm: ShortArray) {
        var offset = 0
        while (offset < pcm.size && audioRunning) {
            val written = track.write(pcm, offset, pcm.size - offset, AudioTrack.WRITE_BLOCKING)
            if (written <= 0) return
            offset += written
        }
    }

    private fun writeSilence(track: AudioTrack, samples: Int) {
        writeAudio(track, ShortArray(samples))
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
        if (frameBuffer.size != pixelBytes) {
            frameBuffer = ByteArray(pixelBytes)
        }
        System.arraycopy(raw, 8, frameBuffer, 0, pixelBytes)
        return FrameSnapshot(w, h, frameBuffer)
    }

    private fun paintFrame(frame: FrameSnapshot) {
        progress.visibility = View.GONE
        glRenderer.submit(frame)
        surface.requestRender()
        fpsCounter.recordFrame()
        updateFpsOverlay()
    }

    private fun updateFpsOverlay() {
        if (showFps) {
            fpsOverlay.text = "FPS ${fpsCounter.lastSecondCount}"
            fpsOverlay.visibility = View.VISIBLE
        } else {
            fpsOverlay.visibility = View.GONE
        }
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

    private fun orientationFor(value: String): Int = when (value) {
        "portrait" -> ActivityInfo.SCREEN_ORIENTATION_PORTRAIT
        "landscape" -> ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE
        else -> ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
    }

    private fun hideSystemBars() {
        @Suppress("DEPRECATION")
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or
                View.SYSTEM_UI_FLAG_FULLSCREEN or
                View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or
                View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN or
                View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION or
                View.SYSTEM_UI_FLAG_LAYOUT_STABLE
            )
        window.decorView.setOnSystemUiVisibilityChangeListener { visibility ->
            if (fullscreen && visibility and View.SYSTEM_UI_FLAG_FULLSCREEN == 0) {
                hideSystemBars()
            }
        }
    }

    private fun toggleFullscreenWithControls() {
        if (fullscreen) {
            exitFullscreenWithControls()
        } else {
            fullscreen = true
            updateFullscreenLayout()
            fullscreenButton.setImageResource(R.drawable.ic_fullscreen_exit)
            hideSystemBars()
        }
    }

    private fun exitFullscreenWithControls() {
        fullscreen = false
        updateFullscreenLayout()
        fullscreenButton.setImageResource(R.drawable.ic_fullscreen)
        showSystemBars()
    }

    private fun updateFullscreenLayout() {
        toolbar.visibility = if (fullscreen) View.GONE else View.VISIBLE
        gameControls.visibility = if (fullscreen && fullscreenMode == "without_controls") View.GONE else View.VISIBLE
    }

    private fun showSystemBars() {
        @Suppress("DEPRECATION")
        window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_LAYOUT_STABLE
        window.decorView.setOnSystemUiVisibilityChangeListener(null)
    }

    private fun readLauncherConfig(): LauncherConfig {
        val raw = NativeBridge.readConfig(LibraryPaths.root(this))
        return runCatching {
            val obj = JSONObject(raw)
            if (obj.has("ok") && !obj.optBoolean("ok", true)) LauncherConfig.default()
            else LauncherConfig.fromJson(obj)
        }.getOrDefault(LauncherConfig.default())
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
                MotionEvent.ACTION_MOVE -> {
                    NativeBridge.nativeSendInput(
                        handle,
                        NativeBridge.INPUT_POINTER_MOVE,
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
        bindVk(R.id.btn_action, VK_A)
        bindVk(R.id.btn_turbo, VK_TURBO)
        bindVk(R.id.btn_a, VK_B)
        bindVk(R.id.btn_b, VK_C)
        bindVk(R.id.btn_c, VK_START)
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
     * streamed game-space coordinates the kernel expects. Returns
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

    private class FrameRenderer : GLSurfaceView.Renderer {
        @Volatile private var pending: FrameSnapshot? = null
        private var texture = 0
        private var program = 0
        private var vertexBuffer: java.nio.FloatBuffer? = null
        private var positionHandle = 0
        private var texCoordHandle = 0
        private var textureHandle = 0
        private var samplerHandle = 0
        private var textureWidth = 0
        private var textureHeight = 0
        private var viewportWidth = 1
        private var viewportHeight = 1

        fun submit(frame: FrameSnapshot) {
            pending = frame
        }

        override fun onSurfaceCreated(gl: javax.microedition.khronos.opengles.GL10?, config: javax.microedition.khronos.egl.EGLConfig?) {
            GLES20.glClearColor(0f, 0f, 0f, 1f)
            program = buildProgram(VERTEX_SHADER, FRAGMENT_SHADER)
            val textures = IntArray(1)
            GLES20.glGenTextures(1, textures, 0)
            texture = textures[0]
            GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, texture)
            GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_NEAREST)
            GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_NEAREST)
            GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE)
            GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE)
            vertexBuffer = java.nio.ByteBuffer.allocateDirect(VERTICES.size * 4)
                .order(java.nio.ByteOrder.nativeOrder()).asFloatBuffer().apply { put(VERTICES); position(0) }
            positionHandle = GLES20.glGetAttribLocation(program, "aPosition")
            texCoordHandle = GLES20.glGetAttribLocation(program, "aTexCoord")
            textureHandle = GLES20.glGetUniformLocation(program, "uTexture")
            samplerHandle = textureHandle
        }

        override fun onSurfaceChanged(gl: javax.microedition.khronos.opengles.GL10?, width: Int, height: Int) {
            GLES20.glViewport(0, 0, width, height)
            viewportWidth = width
            viewportHeight = height
        }

        override fun onDrawFrame(gl: javax.microedition.khronos.opengles.GL10?) {
            GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT)
            val frame = pending ?: return
            GLES20.glUseProgram(program)
            GLES20.glActiveTexture(GLES20.GL_TEXTURE0)
            GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, texture)
            val pixels = java.nio.ByteBuffer.wrap(frame.rgba)
            if (frame.width != textureWidth || frame.height != textureHeight) {
                GLES20.glTexImage2D(GLES20.GL_TEXTURE_2D, 0, GLES20.GL_RGBA, frame.width, frame.height, 0, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, pixels)
                textureWidth = frame.width
                textureHeight = frame.height
            } else {
                GLES20.glTexSubImage2D(GLES20.GL_TEXTURE_2D, 0, 0, 0, frame.width, frame.height, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, pixels)
            }
            val scale = minOf(viewportWidth.toFloat() / frame.width, viewportHeight.toFloat() / frame.height)
            val drawnWidth = frame.width * scale
            val drawnHeight = frame.height * scale
            val left = (viewportWidth - drawnWidth) / viewportWidth - 1f
            val right = (viewportWidth + drawnWidth) / viewportWidth - 1f
            val bottom = 1f - (viewportHeight + drawnHeight) / viewportHeight
            val top = 1f - (viewportHeight - drawnHeight) / viewportHeight
            vertexBuffer?.let { buffer ->
                buffer.clear()
                buffer.put(floatArrayOf(
                    left, bottom, 0f, 1f,
                    right, bottom, 1f, 1f,
                    left, top, 0f, 0f,
                    right, top, 1f, 0f,
                ))
                buffer.position(0)
                GLES20.glEnableVertexAttribArray(positionHandle)
                GLES20.glVertexAttribPointer(positionHandle, 2, GLES20.GL_FLOAT, false, 16, buffer)
                buffer.position(2)
                GLES20.glEnableVertexAttribArray(texCoordHandle)
                GLES20.glVertexAttribPointer(texCoordHandle, 2, GLES20.GL_FLOAT, false, 16, buffer)
                GLES20.glUniform1i(samplerHandle, 0)
                GLES20.glDrawArrays(GLES20.GL_TRIANGLE_STRIP, 0, 4)
            }
        }

        private fun buildProgram(vertex: String, fragment: String): Int {
            val vs = compileShader(GLES20.GL_VERTEX_SHADER, vertex)
            val fs = compileShader(GLES20.GL_FRAGMENT_SHADER, fragment)
            return GLES20.glCreateProgram().also { p ->
                GLES20.glAttachShader(p, vs)
                GLES20.glAttachShader(p, fs)
                GLES20.glLinkProgram(p)
                GLES20.glDeleteShader(vs)
                GLES20.glDeleteShader(fs)
            }
        }

        private fun compileShader(type: Int, source: String): Int {
            return GLES20.glCreateShader(type).also { shader ->
                GLES20.glShaderSource(shader, source)
                GLES20.glCompileShader(shader)
            }
        }

        companion object {
            private val VERTICES = floatArrayOf(
                -1f, -1f, 0f, 1f,
                 1f, -1f, 1f, 1f,
                -1f,  1f, 0f, 0f,
                 1f,  1f, 1f, 0f,
            )
            private const val VERTEX_SHADER = "attribute vec2 aPosition; attribute vec2 aTexCoord; varying vec2 vTexCoord; void main() { gl_Position = vec4(aPosition, 0.0, 1.0); vTexCoord = aTexCoord; }"
            private const val FRAGMENT_SHADER = "precision mediump float; varying vec2 vTexCoord; uniform sampler2D uTexture; void main() { gl_FragColor = texture2D(uTexture, vTexCoord); }"
        }
    }

    companion object {
        const val EXTRA_GAME_ID = "com.pockethle.app.EXTRA_GAME_ID"
        const val EXTRA_GAME_NAME = "com.pockethle.app.EXTRA_GAME_NAME"

        // Win32 virtual-key codes — same set the desktop GUI uses.
        private const val VK_UP = 0x26
        private const val VK_DOWN = 0x28
        private const val VK_LEFT = 0x25
        private const val VK_RIGHT = 0x27
        private const val VK_RETURN = 0x0D // Host Enter; HLE remaps it to GAPI vkA.
        private const val VK_A = 0xD1
        private const val VK_B = 0xD2
        private const val VK_C = 0xD3
        private const val VK_START = 0xD4
        private const val VK_TURBO = 0x32 // Asphalt 2 SPV: key 2 is turbo.
        private const val VK_TSOFT1 = 0xC1
        private const val VK_TSOFT2 = 0xC2

        /** Polling cadence in ms. 33 ≈ 30 Hz. */
        private const val POLL_INTERVAL_MS = 16L

    }
}
