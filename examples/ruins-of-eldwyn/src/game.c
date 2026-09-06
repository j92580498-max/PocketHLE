/* Quest flow, entity AI, combat. */
#include "game.h"
#include "rt.h"
#include "snd.h"

GameState G;

static int spawn_pos(int* tx, int* tz, u8 ch) {
    for (int y = 0; y < MAP_H; y++)
        for (int x = 0; x < MAP_W; x++)
            if (MAP[y][x] == ch) { *tx = x; *tz = y; return 1; }
    return 0;
}

static void tile_center(int tx, int tz, fx* x, fx* z) {
    *x = ((tx << 1) + 1) << 16;
    *z = ((tz << 1) + 1) << 16;
}

static void spawn_enemies(u8 kind, int count) {
    int n = 0;
    for (int y = 0; y < MAP_H && n < count; y++)
        for (int x = 0; x < MAP_W && n < count; x++) {
            u8 c = MAP[y][x];
            int want = (kind == 0) ? (c == 'S') : (c == 'K');
            if (!want) continue;
            for (int i = 0; i < MAX_ENEMIES; i++) {
                Enemy* e = &G.enemies[i];
                if (e->active) continue;
                e->active = 1;
                e->kind = kind;
                tile_center(x, y, &e->x, &e->z);
                e->face = 0;
                e->maxhp = kind ? 6 : 3;
                e->hp = e->maxhp;
                e->hit_timer = 0;
                e->think = (u32)(i * 97 + 131);
                e->hop_phase = (ba)(i * 1024);
                e->hop_h = 0;
                n++;
                break;
            }
        }
}

void game_reset(void) {
    memset(&G, 0, sizeof(G));
    map_init();
    int tx, tz;
    if (!spawn_pos(&tx, &tz, '@')) { tx = 9; tz = 10; }
    tile_center(tx, tz, &G.player.x, &G.player.z);
    G.player.face = 12288; /* facing up (away from camera, -Z) */
    G.player.maxhp = 6;
    G.player.hp = 6;
    G.player.lvl = 1;
    G.player.atk = 2;
    G.quest = Q_TALK_ELDER;
    G.title = 0;
    spawn_enemies(0, 5);
    spawn_enemies(1, 2);
}

static int enemy_near(Enemy* e, fx x, fx z, fx r) {
    fx dx = e->x - x, dz = e->z - z;
    fx d2 = fx_mul(dx, dx) + fx_mul(dz, dz);
    return d2 < fx_mul(r, r);
}

static Player* pl(void) { return &G.player; }

static void spawn_pickup(fx x, fx z, int kind) {
    for (int i = 0; i < MAX_BLIPS; i++) {
        Pickup* p = &G.pickups[i];
        if (p->active) continue;
        p->active = 1;
        p->x = x; p->z = z; p->kind = kind; p->timer = 0;
        return;
    }
}

static const char* const DIALOGS[][4] = {
    /* Q_TALK_ELDER */
    {"ELDER:  THE OLD GATE IN THE", "NORTH-EAST RUIN HAS SEALED", "ITSELF. CLEAR THE SLIMES THAT", "FESTER IN OUR FIELDS - 4 OF THEM!", 0},
    /* Q_RETURN_ELDER */
    {"ELDER:  WELL DONE, WANDERER.", "BUT THE RUIN CRAWLS WITH THE", "UNDEAD NOW. FELL 2 SKELETONS", "AND THE GATE MAY LISTEN.", 0},
    /* Q_RETURN_AGAIN */
    {"ELDER:  THE GATE IS OPEN.", "STEP THROUGH IT... OR STAY", "AND GUARD US. THE CHOICE", "IS YOURS, HERO.", 0},
};

void game_show_dialog(int idx) {
    G.dialog = 1;
    G.dialog_page = 0;
    G.dialog_text = 0;
    /* pack 4 lines into one string with '\n' */
    static char buf[4][128];
    buf[idx][0] = 0;
    for (int i = 0; i < 4; i++) {
        const char* line = DIALOGS[idx][i];
        if (!line) break;
        int len = 0;
        while (buf[idx][len]) len++;
        const char* s = line;
        if (len) buf[idx][len++] = '\n';
        while (*s) buf[idx][len++] = *s++;
        buf[idx][len] = 0;
    }
    G.dialog_text = buf[idx];
}

static void try_move(Player* p, fx dx, fx dz) {
    fx nx = p->x + dx, nz = p->z + dz;
    fx pad = FX(0.35);
    if (!map_blocked(nx + (dx > 0 ? pad : -pad), p->z)) p->x = nx;
    if (!map_blocked(p->x, nz + (dz > 0 ? pad : -pad))) p->z = nz;
}

static void kill_enemy(Enemy* e) {
    e->active = 0;
    if (e->kind == 0) G.slime_kills++;
    else G.skel_kills++;
    Player* p = pl();
    p->xp += e->kind ? 30 : 10;
    while (p->xp >= p->lvl * 50) {
        p->xp -= p->lvl * 50;
        p->lvl++;
        p->maxhp += 2;
        p->hp = p->maxhp;
        p->atk++;
        snd_play(SND_LEVELUP);
    }
    if ((G.frame & 1) && e->kind == 0) spawn_pickup(e->x, e->z, 0);
}

static void hurt_player(int dmg) {
    Player* p = pl();
    if (p->hurt || G.dead || G.title) return;
    p->hp -= dmg;
    p->hurt = 45;
    snd_play(SND_HURT);
    if (p->hp <= 0) {
        p->hp = 0;
        G.dead = 1;
        G.dead_timer = 0;
    }
}

static void update_enemy(Enemy* e) {
    Player* p = pl();
    e->think++;
    /* hop animation */
    e->hop_phase += (e->kind ? 96 : 192);
    fx s = fx_sin(e->hop_phase);
    e->hop_h = s > 0 ? s : 0;

    fx speed = e->kind ? FX(1.1) : FX(0.7);
    fx reach = FX(0.9);
    if (!enemy_near(e, p->x, p->z, FX(6))) {
        /* wander */
        if ((e->think & 63) == 0) e->face = (ba)(e->think * 2311);
        fx dx = fx_mul(fx_sin(e->face + 4096), fx_div(speed, 4));
        fx dz = fx_mul(fx_sin(e->face), fx_div(speed, 4));
        if (!map_blocked(e->x + dx, e->z + dz)) { e->x += dx; e->z += dz; }
        return;
    }
    /* chase */
    fx dx = p->x - e->x, dz = p->z - e->z;
    e->face = ba_atan2(dx, dz);
    if (!enemy_near(e, p->x, p->z, reach)) {
        fx step = fx_div(speed, 16);
        fx mx = fx_mul(fx_sin(e->face + 4096), step);
        fx mz = fx_mul(fx_sin(e->face), step);
        if (!map_blocked(e->x + mx, e->z + mz)) { e->x += mx; e->z += mz; }
    } else {
        /* touch damage on a slow cadence */
        if ((e->think & 31) == 0) hurt_player(e->kind ? 2 : 1);
    }
}

static void do_swing(void) {
    Player* p = pl();
    if (p->swing) return;
    p->swing = 14;
    snd_play(SND_SWING);
    fx reach = FX(1.6);
    fx ax = p->x + fx_mul(fx_sin(p->face + 4096), reach);
    fx az = p->z + fx_mul(fx_sin(p->face), reach);
    for (int i = 0; i < MAX_ENEMIES; i++) {
        Enemy* e = &G.enemies[i];
        if (!e->active) continue;
        if (enemy_near(e, ax, az, FX(1.2))) {
            e->hp -= p->atk;
            e->hit_timer = 12;
            snd_play(SND_HIT);
            if (e->hp <= 0) kill_enemy(e);
        }
    }
}

void game_action(void) {
    if (G.title) { G.title = 0; snd_play(SND_BEEP); return; }
    if (G.dead) { if (G.dead_timer > 60) game_reset(); return; }
    if (G.win) return;
    if (G.dialog) {
        G.dialog = 0;
        if (G.quest == Q_TALK_ELDER) { G.quest = Q_KILL_SLIMES; }
        else if (G.quest == Q_RETURN_ELDER) { G.quest = Q_KILL_SKELETONS; }
        else if (G.quest == Q_RETURN_AGAIN) { G.quest = Q_GATE_OPEN; }
        snd_play(SND_BEEP);
        return;
    }
    do_swing();
}

void game_action2(void) {
    Player* p = pl();
    if (G.potions > 0 && p->hp < p->maxhp && !G.dead && !G.title) {
        G.potions--;
        p->hp += 4;
        if (p->hp > p->maxhp) p->hp = p->maxhp;
        snd_play(SND_POTION);
    }
}

void game_update(u32 ms) {
    G.frame++;
    if (G.title) return;
    if (G.dead) { G.dead_timer++; return; }
    if (G.win) { G.win_timer++; return; }

    Player* p = pl();
    if (p->hurt) p->hurt--;
    if (p->swing) p->swing--;

    if (!G.dialog) {
                fx speed = FX(2.2);
        fx dx = 0, dz = 0;
        ba face = p->face;
        if (g_key_state & 1)  { dz -= speed; face = 12288; }
        if (g_key_state & 2)  { dz += speed; face = 4096; }
        if (g_key_state & 4)  { dx -= speed; face = 8192; }
        if (g_key_state & 8)  { dx += speed; face = 0; }
        if (dx || dz) {
            try_move(p, dx, dz);
            p->moving = 1;
            p->step += 1280;
        } else {
            p->moving = 0;
        }
        p->face = face;
    }

    for (int i = 0; i < MAX_ENEMIES; i++)
        if (G.enemies[i].active) update_enemy(&G.enemies[i]);

    for (int i = 0; i < MAX_BLIPS; i++) {
        Pickup* pk = &G.pickups[i];
        if (!pk->active) continue;
        pk->timer++;
        if (pk->timer > 600) { pk->active = 0; continue; }
        if (enemy_near((Enemy*)pk, p->x, p->z, FX(0.8))) {  /* reuse dist check */
            pk->active = 0;
            if (pk->kind == 0) { p->hp += 2; if (p->hp > p->maxhp) p->hp = p->maxhp; }
            else G.potions++;
            snd_play(SND_POTION);
        }
    }

    /* chests: one-shot at map 'C' */
    if (!G.chest_open) {
        int tx, tz;
        if (spawn_pos(&tx, &tz, 'C')) {
            fx cx, cz;
            tile_center(tx, tz, &cx, &cz);
            Enemy chest_probe; chest_probe.x = cx; chest_probe.z = cz;
            if (enemy_near(&chest_probe, p->x, p->z, FX(2.3))) {
                G.chest_open = 1;
                G.potions += 2;
                spawn_pickup(cx, cz, 1);
                snd_play(SND_POTION);
            }
        }
    }

    /* quest triggers */
    if (G.quest == Q_KILL_SLIMES && G.slime_kills >= 4) G.quest = Q_RETURN_ELDER;
    if (G.quest == Q_KILL_SKELETONS && G.skel_kills >= 2) G.quest = Q_RETURN_AGAIN;

    /* elder proximity */
    {
        int tx, tz;
        if (spawn_pos(&tx, &tz, 'E')) {
            fx ex, ez;
            tile_center(tx, tz, &ex, &ez);
            Enemy elder_probe; elder_probe.x = ex; elder_probe.z = ez;
            if (!G.dialog && G.quest != Q_GATE_OPEN && G.quest != Q_WIN) {
                if (enemy_near(&elder_probe, p->x, p->z, FX(2.3))) {
                    int d = (G.quest == Q_TALK_ELDER) ? 0
                          : (G.quest == Q_RETURN_ELDER) ? 1 : 2;
                    if ((G.quest == Q_TALK_ELDER) || (G.quest == Q_RETURN_ELDER) || (G.quest == Q_RETURN_AGAIN))
                        game_show_dialog(d);
                }
            }
        }
    }

    /* gate */
    if (G.quest == Q_GATE_OPEN) {
        int tx, tz;
        if (spawn_pos(&tx, &tz, 'G')) {
            fx gx2, gz2;
            tile_center(tx, tz, &gx2, &gz2);
            Enemy gate_probe; gate_probe.x = gx2; gate_probe.z = gz2;
            if (enemy_near(&gate_probe, p->x, p->z, FX(2.4))) {
                G.win = 1;
                G.win_timer = 0;
                snd_play(SND_LEVELUP);
            }
        }
    }
}
