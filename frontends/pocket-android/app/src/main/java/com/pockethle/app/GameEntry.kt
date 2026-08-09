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
    val icon: String?,
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
                icon = obj.optString("icon").takeIf { !obj.isNull("icon") && it.isNotEmpty() },
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
    val screen: String,
) {
    fun toJson(): JSONObject = JSONObject().apply {
        put("cpu_backend", cpuBackend)
        put("max_slices", maxSlices)
        put("instructions_per_slice", instructionsPerSlice)
        put("halt_on_unimplemented", haltOnUnimplemented)
        put("screen", screen)
    }

    companion object {
        fun default(): GameSettings = GameSettings(
            cpuBackend = "unicorn",
            maxSlices = 50_000_000L,
            instructionsPerSlice = 1_000_000L,
            haltOnUnimplemented = false,
            screen = "portrait",
        )

        fun fromJson(obj: JSONObject): GameSettings = GameSettings(
            cpuBackend = obj.optString("cpu_backend", "unicorn"),
            maxSlices = obj.optLong("max_slices", 50_000_000L),
            instructionsPerSlice = obj.optLong("instructions_per_slice", 1_000_000L),
            haltOnUnimplemented = obj.optBoolean("halt_on_unimplemented", false),
            screen = obj.optString("screen", "portrait"),
        )
    }
}

data class LauncherConfig(
    val schemaVersion: Int,
    val defaultCpuBackend: String,
    val verbosity: Int,
    val lastImportDir: String?,
    val showFps: Boolean,
    val fullscreen: Boolean,
    val fullscreenMode: String,
    val orientation: String,
) {
    fun toJson(): JSONObject = JSONObject().apply {
        put("schema_version", schemaVersion)
        put("default_cpu_backend", defaultCpuBackend)
        put("verbosity", verbosity)
        if (lastImportDir != null) put("last_import_dir", lastImportDir) else put("last_import_dir", JSONObject.NULL)
        put("show_fps", showFps)
        put("fullscreen", fullscreen)
        put("fullscreen_mode", fullscreenMode)
        put("orientation", orientation)
    }

    companion object {
        fun default(): LauncherConfig = LauncherConfig(
            schemaVersion = 1,
            defaultCpuBackend = "stub",
            verbosity = 1,
            lastImportDir = null,
            showFps = true,
            fullscreen = false,
            fullscreenMode = "with_controls",
            orientation = "auto",
        )

        fun fromJson(obj: JSONObject): LauncherConfig = LauncherConfig(
            schemaVersion = obj.optInt("schema_version", 1),
            defaultCpuBackend = obj.optString("default_cpu_backend", "stub"),
            verbosity = obj.optInt("verbosity", 1),
            lastImportDir = obj.optString("last_import_dir").takeIf { !obj.isNull("last_import_dir") && it.isNotEmpty() },
            showFps = obj.optBoolean("show_fps", true),
            fullscreen = obj.optBoolean("fullscreen", false),
            fullscreenMode = obj.optString("fullscreen_mode", "with_controls"),
            orientation = obj.optString("orientation", "auto"),
        )
    }
}
