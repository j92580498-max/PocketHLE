/* snd.c — every effect is synthesised into one 16-bit PCM buffer and
 * queued with waveOutWrite. The HLE waveOut layer copies the samples at
 * write time, so a single static buffer is enough. */
#include "snd.h"
#include "win.h"
#include "fx.h"

#define RATE 22050
#define MAXN (RATE / 3)          /* ~333 ms of mono 16-bit */

static u16 hwo;                  /* waveOut handle */
static u8 pcm[MAXN * 2];         /* 16-bit mono sample bytes */
static WAVEHDR hdr;
static u32 busy_frames;
static u8 opened;

static void put_sample(u8* p, int i, int v) {
    if (v > 32767) v = 32767;
    if (v < -32768) v = -32768;
    p[2 * i] = (u8)(v & 255);
    p[2 * i + 1] = (u8)((v >> 8) & 255);
}

static int sq(int t, int hz) {
    /* square wave via sine-table sign, phase in binary angle */
    ba ph = (ba)(((i64)t * hz * 16384 / RATE) & 16383);
    return fx_sin(ph) >= 0 ? 14000 : -14000;
}

static int sweep_tone(u8* p, int n, int hz0, int hz1, int amp) {
    fx ph = 0;
    for (int i = 0; i < n; i++) {
        fx hz = hz0 + fx_mul(hz1 - hz0, fx_div(i << 16, n << 16));
        ph += (ba)((i64)hz * 16384 / RATE);
        int v = fx_mul(amp, fx_sin((ba)ph));
        put_sample(p, i, v);
    }
    return n;
}

static int noise_hit(u8* p, int n, int amp) {
    u32 seed = 0x1234abcd;
    for (int i = 0; i < n; i++) {
        seed = seed * 1103515245 + 12345;
        int v = (int)((seed >> 16) & 1023) - 512;      /* white noise */
        v = fx_mul(v * 32, fx_div((n - i) << 16, n << 16));  /* decay */
        put_sample(p, i, v);
    }
    return n;
}

static int blip(u8* p, int n, int hz, int amp) {
    for (int i = 0; i < n; i++)
        put_sample(p, i, fx_mul(amp, fx_sin((ba)((i64)hz * i * 16384 / RATE))));
    return n;
}

static int build(int id) {
    u8* p = pcm;
    int n;
    switch (id) {
    case SND_BEEP:     n = blip(p, RATE / 16, 660, 12000); break;
    case SND_SWING:    n = noise_hit(p, RATE / 11, 8000); break;
    case SND_HIT:      n = sweep_tone(p, RATE / 9, 220, 60, 14000); break;
    case SND_HURT:     n = sweep_tone(p, RATE / 5, 400, 90, 15000); break;
    case SND_POTION:
        n = blip(p, RATE / 20, 520, 11000);
        n += blip(p + 2 * n, RATE / 20, 780, 11000);
        break;
    case SND_LEVELUP:
        n = blip(p, RATE / 18, 523, 12000);
        n += blip(p + 2 * n, RATE / 18, 659, 12000);
        n += blip(p + 2 * n, RATE / 12, 784, 12000);
        break;
    default: n = 0;
    }
    return n;
}

void snd_init(void) {
    WAVEFORMATEX fmt;
    fmt.wFormatTag = 1;
    fmt.nChannels = 1;
    fmt.nSamplesPerSec = RATE;
    fmt.wBitsPerSample = 16;
    fmt.nBlockAlign = 2;
    fmt.nAvgBytesPerSec = RATE * 2;
    fmt.cbSize = 0;
    opened = waveOutOpen(&hwo, WAVE_MAPPER, &fmt, 0, 0, CALLBACK_NULL) == 0;
}

void snd_play(int id) {
    if (!opened) return;
    int n = build(id);
    if (!n) return;
    if (busy_frames) waveOutReset(hwo);
    hdr.lpData = pcm;
    hdr.dwBufferLength = n * 2;
    hdr.dwFlags = 0;
    waveOutPrepareHeader(hwo, &hdr, sizeof(hdr));
    waveOutWrite(hwo, &hdr, sizeof(hdr));
    busy_frames = n / RATE * 30 + 2;   /* ~30 fps frames */
}

void snd_frame(void) {
    if (busy_frames) busy_frames--;
}
