package com.pockethle.app

import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.graphics.BitmapFactory
import java.io.File
import android.widget.ImageButton
import android.widget.PopupMenu
import android.widget.TextView
import androidx.recyclerview.widget.RecyclerView

/**
 * RecyclerView adapter for the library screen — launcher-style grid
 * of game tiles (cover art + title). Tapping a tile runs the game;
 * Settings/Remove live behind the tile's overflow (⋮) button so the
 * grid stays visually clean.
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
            .inflate(R.layout.item_game_grid, parent, false)
        return ViewHolder(view)
    }

    override fun onBindViewHolder(holder: ViewHolder, position: Int) {
        holder.bind(items[position])
    }

    override fun getItemCount(): Int = items.size

    inner class ViewHolder(view: View) : RecyclerView.ViewHolder(view) {
        private val icon = view.findViewById<android.widget.ImageView>(R.id.game_icon)
        private val title: TextView = view.findViewById(R.id.game_title)
        private val publisherLabel: TextView = view.findViewById(R.id.game_publisher)
        private val moreBtn: ImageButton = view.findViewById(R.id.btn_more)

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
            publisherLabel.text = itemView.context.getString(
                R.string.publisher_label,
                entry.provider?.takeIf { it.isNotBlank() }
                    ?: itemView.context.getString(R.string.publisher_unknown),
            )
            itemView.setOnClickListener { onRun(entry) }
            moreBtn.setOnClickListener { anchor -> showOverflowMenu(anchor, entry) }
        }

        private fun showOverflowMenu(anchor: View, entry: GameEntry) {
            val popup = PopupMenu(anchor.context, anchor)
            popup.menu.add(0, 0, 0, R.string.action_settings)
            popup.menu.add(0, 1, 1, R.string.action_remove)
            popup.setOnMenuItemClickListener { item ->
                when (item.itemId) {
                    0 -> onSettings(entry)
                    1 -> onRemove(entry)
                }
                true
            }
            popup.show()
        }
    }
}
