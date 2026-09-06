/* fx.h — integer-only math for the guest.
 *
 * The ARMv4T guest has no FPU and we link no floating-point runtime,
 * so every value the game computes lives in 16.16 fixed point. The GL
 * layer reads IEEE bit patterns out of the argument registers, so the
 * only bridge to it is fx_to_f32(), a pure integer conversion. */
#ifndef FX_H
#define FX_H

#include <stdint.h>

typedef int32_t fx;   /* 16.16 fixed point */
typedef uint32_t u32;
typedef int32_t i32;
typedef uint16_t u16;
typedef int16_t i16;
typedef uint8_t u8;
typedef int8_t i8;
typedef int64_t i64;

#define FX_ONE 65536
#define FX_HALF 32768
#define FX_PI 205887   /* pi in 16.16 */
#define FX_2PI 411775
#define FX(x) ((fx)((x) * 65536.0 + ((x) >= 0 ? 0.5 : -0.5)))

/* Binary angles: a full turn is 16384 units, so a angle indexes the
 * sine table directly. RAD2BA converts 16.16 radians (one umull, no
 * division). */
typedef i32 ba;

#define RAD2BA(x) ((ba)(((i64)(x) * 2608) >> 16))

extern const i16 SINTAB[16384];

/* sine/cosine of a binary angle; result is 16.16 */
static inline fx fx_sin(ba a) { return (fx)((i32)SINTAB[(u32)a & 16383] << 2); }
static inline fx fx_cos(ba a) { return (fx)((i32)SINTAB[((u32)a + 4096) & 16383] << 2); }

/* (a*b) >> 16 with a 64-bit intermediate */
static inline fx fx_mul(fx a, fx b) { return (fx)(((i64)a * b) >> 16); }

fx fx_div(fx a, fx b);
fx fx_tan(ba a);
ba ba_atan2(fx dy, fx dx);

/* IEEE-754 f32 bit pattern from a 16.16 fixed value (pure integer) */
u32 fx_to_f32(fx v);

/* IEEE bits of the float (float)n — for GLenums pushed through
 * GLfloat entry points (glFogf mode, glTexEnvf, glTexParameterf). */
static inline u32 fenum(u32 n) { return fx_to_f32((fx)(n << 16)); }

/* ---- 4x4 matrices, row-major storage, elements 16.16 ----
 * GL wants column-major; mat_upload walks the transpose when emitting
 * bit patterns, so Mat4 stays readable as rows. */
typedef struct { fx m[16]; } Mat4;

void mat_identity(Mat4* o);
void mat_mul(Mat4* o, const Mat4* a, const Mat4* b);
void mat_translate(Mat4* o, fx x, fx y, fx z);
void mat_scale(Mat4* o, fx x, fx y, fx z);
void mat_rot_y(Mat4* o, ba a);
void mat_rot_x(Mat4* o, ba a);
void mat_perspective(Mat4* o, fx fovy, fx aspect, fx zn, fx zf);
void mat_ortho(Mat4* o, fx l, fx r, fx b, fx t, fx n, fx f);
void mat_lookat(Mat4* o, fx ex, fx ey, fx ez, fx tx, fx ty, fx tz);

#endif
