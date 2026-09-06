/* Procedural palette textures. Everything is authored at runtime:
 * 32x32 paletted tiles for the world, a 5x7 bitmap font for the UI.
 * All textures upload as GL_PALETTE8_RGBA8_OES — the PS1-era way to
 * keep VRAM small and the palette visibly limited. */
#ifndef TEX_H
#define TEX_H

#include "fx.h"

#define TEX_COUNT 14

enum {
    TEX_GRASS = 0,
    TEX_PATH,
    TEX_WALL,
    TEX_WATER,
    TEX_TREE,
    TEX_PILLAR,
    TEX_HERO,
    TEX_ELDER,
    TEX_VILLAGER,
    TEX_SLIME,
    TEX_SKELETON,
    TEX_SWORD,
    TEX_CHEST,
    TEX_FONT,
};

/* one 32x32 indexed image plus its 256-entry RGBA palette */
typedef struct {
    u8 idx[128 * 64];   /* font sheet is the largest user */
    u32 pal[256];      /* RGBA bytes, r at bits 0..7 (GL order) */
} PalTex;

extern PalTex TEX[TEX_COUNT];

void tex_build_all(void);

/* upload every texture; fills TEXID[] with GL names */
extern u32 TEXID[TEX_COUNT];
void tex_upload_all(void);

/* 5x7 font: glyph index = printable ASCII - 32 */
int font_glyph_index(int ch);

#endif
