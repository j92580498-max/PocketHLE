/* Game state, quest flow and per-frame update for Ruins of Eldwyn. */
#ifndef GAME_H
#define GAME_H

#include "fx.h"

#define MAP_W 24
#define MAP_H 24
#define MAX_ENEMIES 12
#define MAX_BLIPS 8

enum {
    T_GRASS = 0,
    T_PATH,
    T_WATER,
    T_WALL,
    T_PILLAR,
    T_TREE,
    T_GATE,
};

enum {
    Q_TALK_ELDER = 0,   /* find the elder, get the quest  */
    Q_KILL_SLIMES,      /* kill 4 slimes                  */
    Q_RETURN_ELDER,     /* report back                    */
    Q_KILL_SKELETONS,   /* kill 2 skeletons at the ruins  */
    Q_RETURN_AGAIN,     /* report back                    */
    Q_GATE_OPEN,        /* step into the gate             */
    Q_WIN,
};

typedef struct {
    u8 active;
    u8 kind;        /* 0 slime, 1 skeleton */
    fx x, z;
    ba face;
    int hp, maxhp;
    u32 hit_timer;  /* white flash after being hit */
    u32 think;      /* ai timer */
    ba hop_phase;
    fx hop_h;
} Enemy;

/* First fields mirror Enemy (active, x, z) so game.c can reuse the
 * distance helper on pickups and probe points. */
typedef struct {
    u8 active;
    u8 pad_[3];
    fx x, z;
    u8 kind;
    u32 timer;
} Pickup;

typedef struct {
    fx x, z;
    ba face;
    int hp, maxhp;
    int lvl, xp, atk;
    u32 swing;      /* frames left in sword swing */
    u32 hurt;       /* invulnerability timer */
    u32 step;       /* walk cycle phase */
    int moving;
} Player;

typedef struct {
    Player player;
    Enemy enemies[MAX_ENEMIES];
    Pickup pickups[MAX_BLIPS];
    int quest;
    int slime_kills, skel_kills;
    int potions;
    u8 dialog;              /* dialog box active */
    u8 dialog_page;         /* future use */
    u8 dialog_user;         /* who is talking: 0 elder, 1 villager */
    const char* dialog_text;/* up to 4 lines, '\n' separated */
    u8 title;               /* title screen */
    u8 dead;
    u32 dead_timer;
    u8 win;
    u32 win_timer;
    u8 chest_open;
    u32 frame;
} GameState;

extern GameState G;
extern volatile u32 g_key_state;  /* bit0 up, 1 down, 2 left, 3 right */

void game_reset(void);
void game_update(u32 ms);
void game_action(void);   /* A button: swing / advance dialog */
void game_action2(void);  /* B button: drink potion */

/* map.c */
extern const u8 MAP[MAP_H][MAP_W];
int map_tile_at(fx x, fx z);       /* tile id or -1 outside */
int map_blocked(fx x, fx z);
int map_find(u8 ch, int* tx, int* tz);
void map_init(void);

#endif
