package com.pockethle.app

import org.json.JSONArray
import org.json.JSONObject

/**
 * Mirror of `pocket_library::GameEntry` on the Kotlin side. Built by
 * parsing the JSON blob returned from [NativeBridge.listGames].
 */
data class GameEntry(
    val id: String,
    val displayName: String,
    val provider: String?,
    val executable: String,
    val sourceCab: String,
    val importedAt: Long,
    val settings: GameSettings,
) {
    companion object {
        fun fromJson(obj: JSONObject): GameEntry {
            val settingsObj = obj.optJSONObject("settings") ?: JSONObject()
            return GameEntry(
                id = obj.getString("id"),
                displayName = obj.getString("display_name"),
                provider = obj.optString("provider").takeIf { !obj.isNull("provider") && it.isNotEmpty() },
                executable = obj.getString("executable"),
                sourceCab = obj.getString("source_cab"),
                importedAt = obj.optLong("imported_at"),
                settings = GameSettings.fromJson(settingsObj),
            )
        }

        fun listFromJson(json: String): List<GameEntry> {
            val arr = JSONArray(json)
            val out = ArrayList<GameEntry>(arr.length())
            for (i in 0 until arr.length()) {
                out.add(fromJson(arr.getJSONObject(i)))
            }
            return out
        }
    }
}

data class GameSettings(
    val cpuBackend: String, // "stub" or "unicorn"
    val maxSlices: Long,
    val instructionsPerSlice: Long,
    val haltOnUnimplemented: Boolean,
) {
    fun toJson(): JSONObject = JSONObject().apply {
        put("cpu_backend", cpuBackend)
        put("max_slices", maxSlices)
        put("instructions_per_slice", instructionsPerSlice)
        put("halt_on_unimplemented", haltOnUnimplemented)
    }

    companion object {
        fun default(): GameSettings = GameSettings(
            cpuBackend = "stub",
            maxSlices = 1024L,
            instructionsPerSlice = 1_000_000L,
            haltOnUnimplemented = false,
        )

        fun fromJson(obj: JSONObject): GameSettings = GameSettings(
            cpuBackend = obj.optString("cpu_backend", "stub"),
            maxSlices = obj.optLong("max_slices", 1024L),
            instructionsPerSlice = obj.optLong("instructions_per_slice", 1_000_000L),
            haltOnUnimplemented = obj.optBoolean("halt_on_unimplemented", false),
        )
    }
}

data class LauncherConfig(
    val schemaVersion: Int,
    val defaultCpuBackend: String,
    val verbosity: Int,
    val lastImportDir: String?,
    /**
     * Render a j2me-loader-style FPS counter on top of the
     * in-game framebuffer. Defaults to `true` to mirror the Rust
     * side ([`pocket_library::LauncherConfig::show_fps`]); legacy
     * `config.json` files that pre-date this field are upgraded
     * with the same default.
     */
    val showFps: Boolean,
) {
    fun toJson(): JSONObject = JSONObject().apply {
        put("schema_version", schemaVersion)
        put("default_cpu_backend", defaultCpuBackend)
        put("verbosity", verbosity)
        if (lastImportDir != null) put("last_import_dir", lastImportDir) else put("last_import_dir", JSONObject.NULL)
        put("show_fps", showFps)
    }

    companion object {
        fun default(): LauncherConfig = LauncherConfig(
            schemaVersion = 1,
            defaultCpuBackend = "stub",
            verbosity = 1,
            lastImportDir = null,
            showFps = true,
        )

        fun fromJson(obj: JSONObject): LauncherConfig = LauncherConfig(
            schemaVersion = obj.optInt("schema_version", 1),
            defaultCpuBackend = obj.optString("default_cpu_backend", "stub"),
            verbosity = obj.optInt("verbosity", 1),
            lastImportDir = obj.optString("last_import_dir").takeIf { !obj.isNull("last_import_dir") && it.isNotEmpty() },
            showFps = obj.optBoolean("show_fps", true),
        )
    }
}
