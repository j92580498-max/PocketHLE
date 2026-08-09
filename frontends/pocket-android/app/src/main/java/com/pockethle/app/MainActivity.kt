package com.pockethle.app

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.view.Menu
import android.view.MenuItem
import android.view.View
import android.widget.TextView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.appcompat.widget.Toolbar
import androidx.lifecycle.lifecycleScope
import androidx.recyclerview.widget.GridLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.floatingactionbutton.FloatingActionButton
import com.google.android.material.snackbar.Snackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

/**
 * Library screen — heavily inspired by
 * [j2me-loader](https://github.com/nikita36078/j2me-loader): a
 * RecyclerView of game cards, a "+" floating action button to import a
 * new `.CAB`, and per-card overflow menu (Run / Settings / Remove).
 */
class MainActivity : AppCompatActivity() {

    private lateinit var adapter: GameAdapter
    private lateinit var recycler: RecyclerView
    private lateinit var emptyState: TextView
    private lateinit var rootDir: String

    private val importGame = registerForActivityResult(
        ActivityResultContracts.OpenDocument(),
    ) { uri ->
        if (uri != null) handleImport(uri)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        setSupportActionBar(findViewById<Toolbar>(R.id.toolbar))

        rootDir = LibraryPaths.root(this)
        emptyState = findViewById(R.id.empty_state)
        recycler = findViewById(R.id.recycler)
        adapter = GameAdapter(
            onRun = { entry -> launchGame(entry) },
            onSettings = { entry ->
                startActivity(
                    Intent(this, GameSettingsActivity::class.java)
                        .putExtra(GameSettingsActivity.EXTRA_GAME_ID, entry.id)
                        .putExtra(GameSettingsActivity.EXTRA_GAME_NAME, entry.displayName),
                )
            },
            onRemove = { entry -> confirmRemove(entry) },
            libraryRoot = rootDir,
        )
        recycler.layoutManager = GridLayoutManager(this, gridSpanCount())
        recycler.adapter = adapter

        findViewById<FloatingActionButton>(R.id.fab_import).setOnClickListener {
            importGame.launch(arrayOf("application/vnd.ms-cab-compressed", "application/x-rar-compressed", "application/zip", "application/octet-stream", "*/*"))
        }
    }

    override fun onResume() {
        super.onResume()
        refreshLibrary()
    }

    override fun onCreateOptionsMenu(menu: Menu): Boolean {
        menuInflater.inflate(R.menu.main_menu, menu)
        return true
    }

    override fun onOptionsItemSelected(item: MenuItem): Boolean {
        return when (item.itemId) {
            R.id.action_settings -> {
                startActivity(Intent(this, SettingsActivity::class.java))
                true
            }
            R.id.action_about -> {
                AlertDialog.Builder(this)
                    .setTitle(R.string.about_title)
                    .setMessage(NativeBridge.banner())
                    .setPositiveButton(android.R.string.ok, null)
                    .show()
                true
            }
            else -> super.onOptionsItemSelected(item)
        }
    }

    /** Roughly 110dp-wide tiles, like a game launcher grid; at least 2 columns. */
    private fun gridSpanCount(): Int {
        val widthDp = resources.configuration.screenWidthDp
        return (widthDp / 110).coerceAtLeast(2)
    }

    private fun refreshLibrary() {
        val raw = NativeBridge.listGames(rootDir)
        val parsed = parseGamesOrToast(raw)
        adapter.submit(parsed)
        if (parsed.isEmpty()) {
            recycler.visibility = View.GONE
            emptyState.visibility = View.VISIBLE
        } else {
            recycler.visibility = View.VISIBLE
            emptyState.visibility = View.GONE
        }
    }

    private fun parseGamesOrToast(raw: String): List<GameEntry> {
        return try {
            // Library returns [] when empty, JSON object {ok:false,error:...} on error.
            if (raw.startsWith("{")) {
                val obj = JSONObject(raw)
                if (!obj.optBoolean("ok", true)) {
                    Snackbar.make(
                        recycler,
                        getString(R.string.error_listing_games, obj.optString("error")),
                        Snackbar.LENGTH_LONG,
                    ).show()
                }
                emptyList()
            } else {
                JSONArray(raw)
                GameEntry.listFromJson(raw)
            }
        } catch (e: Exception) {
            Snackbar.make(recycler, "Library parse failed: ${e.message}", Snackbar.LENGTH_LONG).show()
            emptyList()
        }
    }

    private fun handleImport(uri: Uri) {
        val name = queryDisplayName(uri) ?: "import.cab"
        val snack = Snackbar.make(recycler, R.string.import_in_progress, Snackbar.LENGTH_INDEFINITE)
        snack.show()
        lifecycleScope.launch {
            val result = withContext(Dispatchers.IO) {
                runCatching {
                    val cabFile = LibraryPaths.copyUriToCache(this@MainActivity, uri, name)
                    importArchiveOrExe(cabFile)
                }
            }
            snack.dismiss()
            result.fold(
                onSuccess = { raw ->
                    handleImportResult(raw)
                    refreshLibrary()
                },
                onFailure = { err ->
                    Snackbar.make(
                        recycler,
                        getString(R.string.import_failed, err.message ?: "unknown"),
                        Snackbar.LENGTH_LONG,
                    ).show()
                },
            )
        }
    }

    private fun importArchiveOrExe(file: java.io.File): String {
        return when (file.extension.lowercase()) {
            "exe" -> NativeBridge.importExe(rootDir, file.absolutePath)
            "rar" -> error("RAR is not supported on-device yet; extract it first and import the CAB or EXE")
            else -> NativeBridge.importCab(rootDir, file.absolutePath)
        }
    }

    private fun handleImportResult(raw: String) {
        try {
            val obj = JSONObject(raw)
            if (!obj.optBoolean("ok", true)) {
                Snackbar.make(
                    recycler,
                    getString(R.string.import_failed, obj.optString("error")),
                    Snackbar.LENGTH_LONG,
                ).show()
                return
            }
            val name = obj.optString("display_name", obj.optString("id", "?"))
            Snackbar.make(
                recycler,
                getString(R.string.import_success, name),
                Snackbar.LENGTH_SHORT,
            ).show()
        } catch (e: Exception) {
            Snackbar.make(recycler, raw, Snackbar.LENGTH_LONG).show()
        }
    }

    private fun queryDisplayName(uri: Uri): String? {
        return contentResolver.query(uri, null, null, null, null)?.use { c ->
            val ix = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (ix >= 0 && c.moveToFirst()) c.getString(ix) else null
        }
    }

    private fun confirmRemove(entry: GameEntry) {
        AlertDialog.Builder(this)
            .setTitle(R.string.remove_title)
            .setMessage(getString(R.string.remove_message, entry.displayName))
            .setPositiveButton(R.string.remove) { _, _ ->
                val raw = NativeBridge.removeGame(rootDir, entry.id)
                val obj = runCatching { JSONObject(raw) }.getOrNull()
                if (obj?.optBoolean("ok") == true) {
                    refreshLibrary()
                } else {
                    Snackbar.make(
                        recycler,
                        obj?.optString("error") ?: raw,
                        Snackbar.LENGTH_LONG,
                    ).show()
                }
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun launchGame(entry: GameEntry) {
        startActivity(
            Intent(this, GameActivity::class.java)
                .putExtra(GameActivity.EXTRA_GAME_ID, entry.id)
                .putExtra(GameActivity.EXTRA_GAME_NAME, entry.displayName),
        )
    }
}
