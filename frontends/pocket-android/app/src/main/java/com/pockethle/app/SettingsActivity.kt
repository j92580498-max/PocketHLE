package com.pockethle.app

import android.os.Bundle
import androidx.appcompat.app.AppCompatActivity
import androidx.appcompat.widget.Toolbar
import androidx.preference.ListPreference
import androidx.preference.Preference
import androidx.preference.PreferenceFragmentCompat
import androidx.preference.SeekBarPreference
import androidx.preference.SwitchPreferenceCompat
import org.json.JSONObject

/** Global launcher settings (default backend, log verbosity). */
class SettingsActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_settings)
        setSupportActionBar(findViewById<Toolbar>(R.id.toolbar))
        supportActionBar?.setDisplayHomeAsUpEnabled(true)
        if (savedInstanceState == null) {
            supportFragmentManager.beginTransaction()
                .replace(R.id.preferences_container, GlobalPreferencesFragment())
                .commit()
        }
    }

    override fun onSupportNavigateUp(): Boolean {
        finish()
        return true
    }

    class GlobalPreferencesFragment : PreferenceFragmentCompat() {
        private lateinit var rootDir: String
        private var current: LauncherConfig = LauncherConfig.default()

        override fun onCreatePreferences(savedInstanceState: Bundle?, rootKey: String?) {
            rootDir = LibraryPaths.root(requireContext())
            current = readConfig() ?: LauncherConfig.default()
            setPreferencesFromResource(R.xml.preferences_global, rootKey)
            findPreference<ListPreference>("default_cpu_backend")?.apply {
                value = current.defaultCpuBackend
                setOnPreferenceChangeListener { _, newValue ->
                    current = current.copy(defaultCpuBackend = newValue.toString())
                    writeConfig()
                    true
                }
            }
            findPreference<SeekBarPreference>("verbosity")?.apply {
                value = current.verbosity
                setOnPreferenceChangeListener { _, newValue ->
                    current = current.copy(verbosity = (newValue as Int))
                    writeConfig()
                    true
                }
            }
            findPreference<SwitchPreferenceCompat>("show_fps")?.apply {
                isChecked = current.showFps
                setOnPreferenceChangeListener { _, newValue ->
                    current = current.copy(showFps = newValue as Boolean)
                    writeConfig()
                    true
                }
            }
            findPreference<Preference>("library_root")?.summary = rootDir
        }

        private fun readConfig(): LauncherConfig? {
            val raw = NativeBridge.readConfig(rootDir)
            return runCatching {
                val obj = JSONObject(raw)
                if (obj.has("ok") && !obj.optBoolean("ok", true)) null
                else LauncherConfig.fromJson(obj)
            }.getOrNull()
        }

        private fun writeConfig() {
            NativeBridge.writeConfig(rootDir, current.toJson().toString())
        }
    }
}
