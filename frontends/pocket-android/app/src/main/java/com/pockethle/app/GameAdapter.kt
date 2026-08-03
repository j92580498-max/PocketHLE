package com.pockethle.app

import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.graphics.BitmapFactory
import java.io.File
import android.widget.ImageButton
import android.widget.TextView
import androidx.recyclerview.widget.RecyclerView

/**
 * RecyclerView adapter for the library screen — j2me-loader style.
 *
 * Each row is a card showing the game's display name, source CAB
 * filename, and three action buttons: run, settings, remove.
 */
class GameAdapter(
    private val onRun: (GameEntry) -> Unit,
    private val onSettings: (GameEntry) -> Unit,
    private val onRemove: (GameEntry) -> Unit,
    private val libraryRoot: String,
) : RecyclerView.Adapter<GameAdapter.ViewHolder>() {

    private var items: List<GameEntry> = emptyList()

    fun submit(newItems: List<GameEntry>) {
        items = newItems
        notifyDataSetChanged()
    }

    override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): ViewHolder {
        val view = LayoutInflater.from(parent.context)
            .inflate(R.layout.item_game, parent, false)
        return ViewHolder(view)
    }

    override fun onBindViewHolder(holder: ViewHolder, position: Int) {
        holder.bind(items[position])
    }

    override fun getItemCount(): Int = items.size

    inner class ViewHolder(view: View) : RecyclerView.ViewHolder(view) {
        private val icon = view.findViewById<android.widget.ImageView>(R.id.game_icon)
        private val title: TextView = view.findViewById(R.id.game_title)
        private val subtitle: TextView = view.findViewById(R.id.game_subtitle)
        private val backendLabel: TextView = view.findViewById(R.id.game_backend)
        private val runBtn: ImageButton = view.findViewById(R.id.btn_run)
        private val settingsBtn: ImageButton = view.findViewById(R.id.btn_settings)
        private val removeBtn: ImageButton = view.findViewById(R.id.btn_remove)

        fun bind(entry: GameEntry) {
            title.text = entry.displayName
            val iconFile = entry.icon?.let { File(libraryRoot, "games/${entry.id}/$it") }
            val bitmap = iconFile?.takeIf { it.isFile }?.let { BitmapFactory.decodeFile(it.absolutePath) }
            if (bitmap != null) {
                icon.setImageBitmap(bitmap)
                icon.imageTintList = null
            } else {
                icon.setImageResource(R.drawable.ic_game)
                icon.imageTintList = itemView.context.getColorStateList(com.google.android.material.R.color.material_dynamic_primary40)
            }
            subtitle.text = entry.provider?.takeIf { it.isNotEmpty() }
                ?: entry.sourceCab
            val backendName = when (entry.settings.cpuBackend) {
                "native_arm" -> "Native ARM"
                "unicorn" -> "Unicorn"
                "stub" -> "Stub"
                else -> entry.settings.cpuBackend
            }
            backendLabel.text = itemView.context.getString(
                R.string.backend_label,
                backendName,
            )
            runBtn.setOnClickListener { onRun(entry) }
            settingsBtn.setOnClickListener { onSettings(entry) }
            removeBtn.setOnClickListener { onRemove(entry) }
            itemView.setOnClickListener { onRun(entry) }
        }
    }
}
