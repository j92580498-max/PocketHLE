#include "tex.h"
#include "gles.h"

PalTex TEX[TEX_COUNT];
u32 TEXID[TEX_COUNT];

/* ---- tiny helpers ---------------------------------------------------- */

static void fill(PalTex* t, int x, int y, int w, int h, u8 i) {
    for (int yy = y; yy < y + h; yy++)
        for (int xx = x; xx < x + w; xx++)
            t->idx[yy * 32 + xx] = i;
}

static void noise(PalTex* t, u32 seed, u8 base, u8 span) {
    u32 s = seed;
    for (int i = 0; i < 32 * 32; i++) {
        s = s * 1664525u + 1013904223u;
        t->idx[i] = base + (u8)((s >> 24) & (span - 1));
    }
}

/* pack RGBA for the palette; r low byte like GL expects */
static u32 rgba(u8 r, u8 g, u8 b, u8 a) {
    return (u32)r | ((u32)g << 8) | ((u32)b << 16) | ((u32)a << 24);
}

static void solid_pal(PalTex* t, u32 c) {
    for (int i = 0; i < 256; i++) t->pal[i] = c;
}

/* ---- terrain ---------------------------------------------------------- */

static void tex_grass(PalTex* t) {
    for (int i = 0; i < 8; i++) t->pal[i] = rgba(24 + i * 6, 60 + i * 14, 22 + i * 6, 255);
    noise(t, 1234, 0, 8);
    /* darker tufts */
    for (int k = 0; k < 22; k++) {
        u32 s = 77 + k * 131;
        int x = (s * 2654435761u >> 24) & 31, y = (s * 40503u >> 24) & 31;
        t->idx[y * 32 + x] = 1;
        t->idx[y * 32 + ((x + 1) & 31)] = 2;
    }
}

static void tex_path(PalTex* t) {
    for (int i = 0; i < 8; i++) t->pal[i] = rgba(96 + i * 12, 78 + i * 9, 52 + i * 6, 255);
    noise(t, 999, 0, 8);
    /* pebbles */
    for (int k = 0; k < 14; k++) {
        u32 s = 4321 + k * 977;
        int x = (s * 2654435761u >> 24) & 31, y = (s * 40503u >> 24) & 31;
        t->idx[y * 32 + x] = 7;
        t->idx[((y + 1) & 31) * 32 + x] = 6;
    }
}

static void tex_wall(PalTex* t) {
    for (int i = 0; i < 8; i++) t->pal[i] = rgba(74 + i * 14, 74 + i * 13, 82 + i * 12, 255);
    /* brick pattern: rows of 8, offset every other row, dark mortar = 0 */
    solid_pal(t, 0);
    for (int i = 1; i < 8; i++) t->pal[i] = rgba(70 + i * 16, 68 + i * 15, 76 + i * 13, 255);
    t->pal[0] = rgba(40, 38, 44, 255);
    for (int y = 0; y < 32; y++) {
        int row = y >> 3, off = (row & 1) ? 4 : 0;
        for (int x = 0; x < 32; x++) {
            int mx = (x + off) & 7;
            int my = y & 7;
            t->idx[y * 32 + x] = (mx == 0 || my == 0) ? 0 : (u8)(1 + ((x >> 1) ^ (y >> 1)) % 7);
        }
    }
    /* moss patches */
    for (int k = 0; k < 10; k++) {
        u32 s = 31337 + k * 733;
        int x = (s * 2654435761u >> 24) & 31, y = (s * 40503u >> 24) & 31;
        if (t->idx[y * 32 + x]) t->idx[y * 32 + x] = 1;
    }
}

static void tex_water(PalTex* t) {
    for (int i = 0; i < 8; i++) t->pal[i] = rgba(14 + i * 8, 40 + i * 22, 92 + i * 20, 255);
    for (int y = 0; y < 32; y++)
        for (int x = 0; x < 32; x++) {
            int v = ((x * 3 + y * 5) >> 3) & 7;
            t->idx[y * 32 + x] = (u8)v;
        }
}

/* ---- props ------------------------------------------------------------ */

static void tex_tree(PalTex* t) {
    /* crossed quads with alpha: trunk column + leaf blob */
    for (int i = 0; i < 256; i++) t->pal[i] = 0; /* fully transparent */
    /* trunk: browns */
    for (int i = 0; i < 4; i++) t->pal[16 + i] = rgba(60 + i * 20, 40 + i * 12, 22 + i * 6, 255);
    /* leaves: greens */
    for (int i = 0; i < 6; i++) t->pal[32 + i] = rgba(18 + i * 14, 66 + i * 24, 16 + i * 12, 255);
    for (int y = 0; y < 32; y++)
        for (int x = 0; x < 32; x++) {
            int dx = x - 16, dy = y - 11;
            int in_leaves = (dx * dx * 3 + dy * dy * 2) < 130;
            int in_trunk = (x >= 14 && x <= 17 && y >= 18);
            if (in_trunk) t->idx[y * 32 + x] = (u8)(16 + (x & 3));
            else if (in_leaves && ((x + y) % 7) != 0) t->idx[y * 32 + x] = (u8)(32 + ((dx + dy) & 5));
        }
}

static void tex_pillar(PalTex* t) {
    for (int i = 0; i < 8; i++) t->pal[i] = rgba(88 + i * 10, 86 + i * 10, 94 + i * 9, 255);
    noise(t, 555, 0, 8);
    /* vertical flutes */
    for (int x = 0; x < 32; x++)
        if ((x & 7) < 2)
            for (int y = 0; y < 32; y++)
                t->idx[y * 32 + x] = (u8)((t->idx[y * 32 + x] + 4) & 7);
}

/* ---- characters: 32x32 front view, mapped on box faces ---------------- */

/* Paint a tiny character into a 32x32 sheet. Rows 0..31 map to v,
 * columns to u; each face of the boxes samples a sub-rect. */
static void paint_human(PalTex* t, u32 skin, u32 cloth, u32 cloth2, u32 hairc) {
    for (int i = 0; i < 256; i++) t->pal[i] = 0;
    t->pal[1] = skin;      /* face/hands */
    t->pal[2] = cloth;     /* tunic */
    t->pal[3] = cloth2;    /* legs/arms shade */
    t->pal[4] = hairc;     /* hair */
    t->pal[5] = rgba(20, 16, 24, 255);   /* eyes */
    t->pal[6] = rgba(230, 224, 200, 255); /* highlight */
    /* head: rows 0..7, cols 10..21 */
    fill(t, 10, 0, 12, 8, 1);
    fill(t, 9, 0, 14, 2, 4);       /* hair top */
    t->idx[3 * 32 + 12] = 5; t->idx[3 * 32 + 19] = 5;  /* eyes */
    /* torso: rows 8..19 */
    fill(t, 9, 8, 14, 12, 2);
    fill(t, 9, 17, 14, 3, 3);      /* belt shade */
    fill(t, 14, 8, 4, 6, 6);       /* highlight stripe */
    /* arms: rows 8..17, cols 4..8 and 24..28 */
    fill(t, 4, 8, 5, 10, 3);
    fill(t, 23, 8, 5, 10, 3);
    fill(t, 4, 16, 5, 2, 1);       /* hands */
    fill(t, 23, 16, 5, 2, 1);
    /* legs: rows 20..31 */
    fill(t, 10, 20, 5, 12, 3);
    fill(t, 17, 20, 5, 12, 3);
    fill(t, 10, 30, 5, 2, 5);      /* boots */
    fill(t, 17, 30, 5, 2, 5);
}

static void tex_hero(PalTex* t) {
    paint_human(t, rgba(220, 168, 128, 255), rgba(56, 96, 168, 255),
                rgba(38, 66, 120, 255), rgba(64, 40, 24, 255));
}

static void tex_elder(PalTex* t) {
    paint_human(t, rgba(214, 190, 168, 255), rgba(146, 62, 158, 255),
                rgba(104, 42, 118, 255), rgba(236, 236, 232, 255));
}

static void tex_villager(PalTex* t) {
    paint_human(t, rgba(224, 178, 140, 255), rgba(168, 138, 66, 255),
                rgba(120, 96, 44, 255), rgba(84, 52, 28, 255));
}

static void tex_slime(PalTex* t) {
    for (int i = 0; i < 256; i++) t->pal[i] = 0;
    for (int i = 0; i < 6; i++) t->pal[1 + i] = rgba(40 + i * 30, 170 + i * 12, 60 + i * 20, 255);
    t->pal[7] = rgba(240, 255, 240, 255);  /* shine */
    t->pal[8] = rgba(16, 40, 24, 255);     /* eyes */
    for (int y = 0; y < 32; y++)
        for (int x = 0; x < 32; x++) {
            int dx = x - 16, dy = (y - 20) * 2;
            if (dx * dx + dy * dy < 200) {
                int shade = (dx * dx + dy * dy) / 40;
                t->idx[y * 32 + x] = (u8)(1 + (shade & 5));
            }
        }
    t->idx[10 * 32 + 11] = 7; t->idx[9 * 32 + 12] = 7;   /* shine */
    fill(t, 12, 18, 2, 3, 8); fill(t, 19, 18, 2, 3, 8);  /* eyes */
}

static void tex_skeleton(PalTex* t) {
    for (int i = 0; i < 256; i++) t->pal[i] = 0;
    t->pal[1] = rgba(226, 224, 210, 255);  /* bone */
    t->pal[2] = rgba(180, 178, 164, 255);  /* bone shade */
    t->pal[3] = rgba(30, 26, 34, 255);     /* eye sockets */
    t->pal[4] = rgba(96, 88, 70, 255);     /* rags */
    fill(t, 10, 0, 12, 8, 1);              /* skull */
    fill(t, 12, 3, 3, 2, 3); fill(t, 18, 3, 3, 2, 3);
    fill(t, 10, 8, 12, 12, 4);             /* rags */
    for (int y = 9; y < 19; y += 2) fill(t, 11, y, 10, 1, 1);  /* ribs */
    fill(t, 4, 8, 5, 10, 1); fill(t, 23, 8, 5, 10, 1);   /* arms */
    fill(t, 10, 20, 5, 12, 1); fill(t, 17, 20, 5, 12, 1); /* legs */
    fill(t, 13, 20, 6, 2, 2);
}

static void tex_sword(PalTex* t) {
    for (int i = 0; i < 256; i++) t->pal[i] = 0;
    t->pal[1] = rgba(226, 230, 240, 255);
    t->pal[2] = rgba(150, 156, 170, 255);
    t->pal[3] = rgba(120, 84, 40, 255);   /* grip */
    t->pal[4] = rgba(200, 170, 60, 255);  /* guard */
    for (int y = 2; y < 22; y++) fill(t, 14, y, 4, 1, (y & 1) ? 1 : 2);
    fill(t, 8, 21, 16, 3, 4);
    fill(t, 14, 24, 4, 7, 3);
}

static void tex_chest(PalTex* t) {
    for (int i = 0; i < 256; i++) t->pal[i] = 0;
    t->pal[1] = rgba(120, 78, 36, 255);
    t->pal[2] = rgba(86, 54, 24, 255);
    t->pal[3] = rgba(212, 176, 60, 255);
    fill(t, 2, 6, 28, 22, 1);
    fill(t, 2, 15, 28, 3, 2);
    fill(t, 2, 6, 28, 3, 2);
    fill(t, 13, 12, 6, 9, 3);
}

/* ---- font: 16x8 glyphs of 5x7 in a 128x64 sheet ------------------------ */

static const u8 FONT5x7[][7] = {
    {0x00,0x00,0x00,0x00,0x00,0x00,0x00}, /* space */
    {0x04,0x04,0x04,0x04,0x04,0x00,0x04},
    {0x0A,0x0A,0x0A,0x00,0x00,0x00,0x00},
    {0x0A,0x0A,0x1F,0x0A,0x1F,0x0A,0x0A},
    {0x04,0x0F,0x14,0x0E,0x05,0x1E,0x04},
    {0x18,0x19,0x02,0x04,0x08,0x13,0x03},
    {0x0C,0x12,0x14,0x08,0x15,0x12,0x0D},
    {0x04,0x04,0x08,0x00,0x00,0x00,0x00},
    {0x02,0x04,0x08,0x08,0x08,0x04,0x02},
    {0x08,0x04,0x02,0x02,0x02,0x04,0x08},
    {0x0A,0x04,0x15,0x00,0x00,0x00,0x00},
    {0x00,0x04,0x04,0x1F,0x04,0x04,0x00},
    {0x00,0x00,0x00,0x00,0x0C,0x04,0x08},
    {0x00,0x00,0x00,0x1F,0x00,0x00,0x00},
    {0x00,0x00,0x00,0x00,0x00,0x0C,0x0C},
    {0x00,0x01,0x02,0x04,0x08,0x10,0x00},
    {0x0E,0x11,0x13,0x15,0x19,0x11,0x0E}, /* 0 */
    {0x04,0x0C,0x04,0x04,0x04,0x04,0x0E},
    {0x0E,0x11,0x01,0x06,0x08,0x10,0x1F},
    {0x1F,0x02,0x04,0x02,0x01,0x11,0x0E},
    {0x02,0x06,0x0A,0x12,0x1F,0x02,0x02},
    {0x1F,0x10,0x1E,0x01,0x01,0x11,0x0E},
    {0x06,0x08,0x10,0x1E,0x11,0x11,0x0E},
    {0x1F,0x01,0x02,0x04,0x08,0x08,0x08},
    {0x0E,0x11,0x11,0x0E,0x11,0x11,0x0E},
    {0x0E,0x11,0x11,0x0F,0x01,0x02,0x0C},
    {0x00,0x0C,0x0C,0x00,0x0C,0x0C,0x00},
    {0x0C,0x0C,0x00,0x0C,0x0C,0x04,0x08},
    {0x02,0x04,0x08,0x10,0x08,0x04,0x02},
    {0x00,0x00,0x1F,0x00,0x1F,0x00,0x00},
    {0x08,0x04,0x02,0x01,0x02,0x04,0x08},
    {0x0E,0x11,0x01,0x02,0x04,0x00,0x04},
    {0x0E,0x11,0x11,0x15,0x15,0x1F,0x11}, /* A */
    {0x1E,0x11,0x11,0x1E,0x11,0x11,0x1E},
    {0x0E,0x11,0x10,0x10,0x10,0x11,0x0E},
    {0x1C,0x12,0x11,0x11,0x11,0x12,0x1C},
    {0x1F,0x10,0x10,0x1E,0x10,0x10,0x1F},
    {0x1F,0x10,0x10,0x1E,0x10,0x10,0x10},
    {0x0E,0x11,0x10,0x17,0x11,0x11,0x0F},
    {0x11,0x11,0x11,0x1F,0x11,0x11,0x11},
    {0x0E,0x04,0x04,0x04,0x04,0x04,0x0E},
    {0x07,0x02,0x02,0x02,0x02,0x12,0x0C},
    {0x11,0x12,0x14,0x18,0x14,0x12,0x11},
    {0x10,0x10,0x10,0x10,0x10,0x10,0x1F},
    {0x11,0x1B,0x15,0x15,0x11,0x11,0x11},
    {0x11,0x19,0x15,0x13,0x11,0x11,0x11},
    {0x0E,0x11,0x11,0x11,0x11,0x11,0x0E},
    {0x1E,0x11,0x11,0x1E,0x10,0x10,0x10},
    {0x0E,0x11,0x11,0x11,0x15,0x12,0x0D},
    {0x1E,0x11,0x11,0x1E,0x14,0x12,0x11},
    {0x0F,0x10,0x10,0x0E,0x01,0x01,0x1E},
    {0x1F,0x04,0x04,0x04,0x04,0x04,0x04},
    {0x11,0x11,0x11,0x11,0x11,0x11,0x0E},
    {0x11,0x11,0x11,0x11,0x11,0x0A,0x04},
    {0x11,0x11,0x11,0x15,0x15,0x15,0x0A},
    {0x11,0x11,0x0A,0x04,0x0A,0x11,0x11},
    {0x11,0x11,0x11,0x0A,0x04,0x04,0x04},
    {0x1F,0x01,0x02,0x04,0x08,0x10,0x1F},
    {0x0E,0x08,0x08,0x08,0x08,0x08,0x0E},
    {0x00,0x10,0x08,0x04,0x02,0x01,0x00},
    {0x0E,0x02,0x02,0x02,0x02,0x02,0x0E},
    {0x04,0x0A,0x00,0x00,0x00,0x00,0x00},
    {0x00,0x00,0x00,0x00,0x00,0x00,0x1F},
    {0x08,0x04,0x02,0x00,0x00,0x00,0x00},
    {0x00,0x0E,0x01,0x0D,0x13,0x13,0x0D}, /* a */
    {0x10,0x10,0x1E,0x11,0x11,0x11,0x1E},
    {0x00,0x0E,0x11,0x10,0x10,0x11,0x0E},
    {0x01,0x01,0x0D,0x13,0x13,0x13,0x0D},
    {0x00,0x0E,0x11,0x1F,0x10,0x11,0x0E},
    {0x02,0x04,0x0E,0x11,0x1F,0x10,0x0E},
    {0x00,0x0D,0x13,0x13,0x0D,0x01,0x0E},
    {0x10,0x10,0x1E,0x11,0x11,0x11,0x11},
    {0x04,0x00,0x0C,0x04,0x04,0x04,0x0E},
    {0x02,0x00,0x06,0x02,0x02,0x12,0x0C},
    {0x10,0x11,0x12,0x1C,0x12,0x11,0x10},
    {0x0C,0x04,0x04,0x04,0x04,0x04,0x0E},
    {0x00,0x0A,0x15,0x15,0x15,0x15,0x15},
    {0x00,0x1E,0x11,0x11,0x11,0x11,0x11},
    {0x00,0x0E,0x11,0x11,0x11,0x11,0x0E},
    {0x00,0x1E,0x11,0x11,0x1E,0x10,0x10},
    {0x00,0x0D,0x13,0x13,0x0D,0x01,0x01},
    {0x00,0x1D,0x13,0x10,0x10,0x10,0x10},
    {0x00,0x0F,0x10,0x0E,0x01,0x01,0x1E},
    {0x0C,0x04,0x0E,0x04,0x04,0x04,0x06},
    {0x00,0x11,0x11,0x11,0x11,0x13,0x0D},
    {0x00,0x11,0x11,0x11,0x0A,0x0A,0x04},
    {0x00,0x11,0x11,0x15,0x15,0x15,0x0A},
    {0x00,0x11,0x11,0x0A,0x04,0x0A,0x11},
    {0x00,0x11,0x11,0x0F,0x01,0x11,0x0E},
    {0x00,0x1F,0x02,0x04,0x08,0x10,0x1F},
    {0x06,0x04,0x04,0x08,0x04,0x04,0x06},
    {0x04,0x04,0x04,0x04,0x04,0x04,0x04},
    {0x0C,0x04,0x04,0x02,0x04,0x04,0x0C},
    {0x00,0x00,0x08,0x15,0x02,0x00,0x00}, /* ~ */
};

int font_glyph_index(int ch) {
    if (ch < 32 || ch > 126) ch = '?';
    return ch - 32;
}

/* ---- entry ------------------------------------------------------------ */

void tex_build_all(void) {
    static const struct { void (*fn)(PalTex*); } builders[TEX_COUNT] = {
        tex_grass, tex_path, tex_wall, tex_water, tex_tree, tex_pillar,
        tex_hero, tex_elder, tex_villager, tex_slime, tex_skeleton,
        tex_sword, tex_chest, 0,
    };
    for (int i = 0; i < TEX_COUNT - 1; i++) builders[i].fn(&TEX[i]);

    /* guard ring: copy the nearest interior texel into the border so the
     * alpha-tested edges of a NEAREST-filtered quad never discard */
    for (int i = 0; i < TEX_COUNT - 1; i++) {
        if (i == 4) continue;              /* tree keeps its alpha mask */
        PalTex* t = &TEX[i];
        for (int y = 0; y < 32; y++)
            for (int x = 0; x < 32; x++)
                if (x == 0 || y == 0 || x == 31 || y == 31)
                    t->idx[y * 32 + x] = t->idx[(y < 31 ? y + 1 : y - 1) * 32 +
                                                (x < 31 ? x + 1 : x - 1)];
    }

    /* font sheet: 128x64, 16 cols x 8 rows of 8x8 cells, 5x7 glyph inside */
    PalTex* f = &TEX[TEX_FONT];
    for (int i = 0; i < 256; i++) f->pal[i] = 0;
    f->pal[1] = rgba(240, 236, 220, 255);   /* text */
    f->pal[2] = rgba(60, 48, 88, 255);      /* shadow */
    f->pal[3] = rgba(230, 60, 60, 255);     /* red (hearts) */
    f->pal[4] = rgba(90, 220, 90, 255);     /* green (xp) */
    f->pal[5] = rgba(24, 20, 36, 255);      /* panel */
    f->pal[6] = rgba(220, 196, 120, 255);   /* panel border */
    for (int g = 0; g < 95; g++) {
        int gx = (g & 15) * 8, gy = (g >> 4) * 8;
        for (int row = 0; row < 7; row++) {
            u8 bits = FONT5x7[g][row];
            for (int col = 0; col < 5; col++)
                if (bits & (0x10 >> col))
                    f->idx[(gy + 1 + row) * 128 + gx + 1 + col] = 1;
        }
    }
}

/* ---- upload ----------------------------------------------------------- */

/* Palette8 RGBA8 layout: 1024-byte palette then width*height indices. */
static void upload_pal8(u32 texobj, int w, int h, const PalTex* t) {
    static u8 buf[1024 + 128 * 64];
    u32* pal = (u32*)buf;
    for (int i = 0; i < 256; i++) pal[i] = t->pal[i];
    u8* idx = buf + 1024;
    int stride = (w > 32) ? 128 : 32;
    for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++)
            idx[y * w + x] = t->idx[y * stride + x];
    glBindTexture(GL_TEXTURE_2D, texobj);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_PALETTE8_RGBA8_OES, w, h, 0,
                 GL_PALETTE8_RGBA8_OES, GL_UNSIGNED_BYTE, buf);
    glTexParameterf(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, (GLfloat)fenum(GL_NEAREST));
    glTexParameterf(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, (GLfloat)fenum(GL_NEAREST));
    glTexParameterf(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, (GLfloat)fenum(GL_REPEAT));
    glTexParameterf(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, (GLfloat)fenum(GL_REPEAT));
}

void tex_upload_all(void) {
    glGenTextures(TEX_COUNT, TEXID);
    for (int i = 0; i < TEX_COUNT - 1; i++) upload_pal8(TEXID[i], 32, 32, &TEX[i]);
    upload_pal8(TEXID[TEX_FONT], 128, 64, &TEX[TEX_FONT]);
}
