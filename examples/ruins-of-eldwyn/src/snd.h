/* Tiny waveOut sound effects. */
#ifndef SND_H
#define SND_H

enum {
    SND_BEEP = 0,
    SND_SWING,
    SND_HIT,
    SND_HURT,
    SND_POTION,
    SND_LEVELUP,
    SND_COUNT,
};

void snd_init(void);
void snd_play(int id);
void snd_frame(void);   /* call once per frame: releases busy flag */

#endif
