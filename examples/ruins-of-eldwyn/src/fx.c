#include "fx.h"

#include "sintab.h"

fx fx_div(fx a, fx b) {
	if (b == 0) return a >= 0 ? 0x7FFFFFFF : (int32_t)0x80000000;
	int64_t n = (int64_t)a << 16;
	return (fx)(n / b);
}

u32 fx_to_f32(fx v) {
	u32 sign = 0;
	u32 n;
	if (v == 0) return 0;
	if (v < 0) { sign = 0x80000000u; n = (u32)(-(int64_t)v); }
	else n = (u32)v;
	/* value = n / 2^16; normalize so MSB lands in the implicit-1 slot */
	int lz = 0;
	u32 t = n;
	while (!(t & 0x80000000u)) { t <<= 1; lz++; }
	int e = 31 - lz;            /* MSB index of n */
	int32_t exp = 127 + e - 16; /* unbiased exponent of the value */
	if (exp <= 0) return 0;     /* flush to zero */
	if (exp >= 255) exp = 254;  /* clamp, should not happen */
	/* mantissa: 23 bits after the MSB */
	u32 mant;
	if (e <= 23) mant = (n << (23 - e)) & 0x7FFFFFu;
	else mant = (n >> (e - 23)) & 0x7FFFFFu;
	return sign | ((u32)exp << 23) | mant;
}

fx fx_tan(ba a) {
	fx s = fx_sin(a), c = fx_cos(a);
	return fx_div(s, c);
}

void mat_identity(Mat4* o) {
	o->m[0] = FX_ONE; o->m[1] = 0; o->m[2] = 0; o->m[3] = 0;
	o->m[4] = 0; o->m[5] = FX_ONE; o->m[6] = 0; o->m[7] = 0;
	o->m[8] = 0; o->m[9] = 0; o->m[10] = FX_ONE; o->m[11] = 0;
	o->m[12] = 0; o->m[13] = 0; o->m[14] = 0; o->m[15] = FX_ONE;
}

/* row-major a * b: o[r][c] = sum_k a[r][k]*b[k][c] */
void mat_mul(Mat4* o, const Mat4* a, const Mat4* b) {
	Mat4 r;
	for (int row = 0; row < 4; row++) {
		for (int col = 0; col < 4; col++) {
			int64_t s = 0;
			for (int k = 0; k < 4; k++)
				s += ((int64_t)a->m[row * 4 + k] * b->m[k * 4 + col]) >> 16;
			if (s > 0x7FFFFFFFll) s = 0x7FFFFFFFll;
			if (s < (int64_t)-0x80000000ll) s = (int64_t)-0x80000000ll;
			r.m[row * 4 + col] = (fx)s;
		}
	}
	*o = r;
}

void mat_translate(Mat4* o, fx x, fx y, fx z) {
	mat_identity(o);
	o->m[3] = x; o->m[7] = y; o->m[11] = z;
}

void mat_scale(Mat4* o, fx x, fx y, fx z) {
	mat_identity(o);
	o->m[0] = x; o->m[5] = y; o->m[10] = z;
}

void mat_rot_y(Mat4* o, ba a) {
	fx s = fx_sin(a), c = fx_cos(a);
	o->m[0] = c; o->m[1] = 0; o->m[2] = -s; o->m[3] = 0;
	o->m[4] = 0; o->m[5] = FX_ONE; o->m[6] = 0; o->m[7] = 0;
	o->m[8] = s; o->m[9] = 0; o->m[10] = c; o->m[11] = 0;
	o->m[12] = 0; o->m[13] = 0; o->m[14] = 0; o->m[15] = FX_ONE;
}

void mat_rot_x(Mat4* o, ba a) {
	fx s = fx_sin(a), c = fx_cos(a);
	o->m[0] = FX_ONE; o->m[1] = 0; o->m[2] = 0; o->m[3] = 0;
	o->m[4] = 0; o->m[5] = c; o->m[6] = s; o->m[7] = 0;
	o->m[8] = 0; o->m[9] = -s; o->m[10] = c; o->m[11] = 0;
	o->m[12] = 0; o->m[13] = 0; o->m[14] = 0; o->m[15] = FX_ONE;
}

void mat_perspective(Mat4* o, fx fovy, fx aspect, fx zn, fx zf) {
	fx t = fx_mul(fx_sin(RAD2BA(fovy) >> 1), zn);
	fx b = -t;
	fx r = fx_mul(t, aspect);
	fx l = -r;
	fx rl = r - l, tb = t - b, nf = zn - zf;
	mat_identity(o);
	o->m[0] = fx_div(FX(2) * zn, rl);
	o->m[5] = fx_div(FX(2) * zn, tb);
	o->m[8] = fx_div(r + l, rl);
	o->m[9] = fx_div(t + b, tb);
	o->m[10] = fx_div(zf + zn, nf);
	o->m[11] = -FX_ONE;
	o->m[14] = fx_div(FX(2) * fx_mul(zn, zf), nf);
	o->m[15] = 0;
}

void mat_ortho(Mat4* o, fx l, fx r, fx b, fx t, fx n, fx f) {
	mat_identity(o);
	o->m[0] = fx_div(FX(2), r - l);
	o->m[5] = fx_div(FX(2), t - b);
	o->m[10] = fx_div(-FX(2), f - n);
	o->m[3] = fx_div(-(r + l), r - l);
	o->m[7] = fx_div(-(t + b), t - b);
	o->m[11] = fx_div(-(f + n), f - n);
}

/* Top-down chase camera: rotate the world pitch-down about X after
 * translating the eye to the origin. GL looks down -Z, so the target
 * must land on the -Z side; with eye above and behind the player the
 * sign works out. */
void mat_lookat(Mat4* o, fx ex, fx ey, fx ez, fx tx, fx ty, fx tz) {
	Mat4 t, r;
	mat_translate(&t, -ex, -ey, -ez);
	mat_rot_x(&r, RAD2BA(-FX_PI / 3));
	mat_mul(o, &r, &t);
}

/* arctan(dy/dx) to a binary angle, quadrant-aware. Used for facing
 * directions; 0.5 degree accuracy is far beyond what the low-poly
 * models can express. */
ba ba_atan2(fx dy, fx dx) {
    if (dx == 0 && dy == 0) return 0;
    fx ax = dx >= 0 ? dx : -dx;
    fx ay = dy >= 0 ? dy : -dy;
    fx mn = ax < ay ? ax : ay;
    fx mx = ax < ay ? ay : ax;
    fx ratio = fx_div(mn, mx);          /* [0, 1] in 16.16 */
    fx acc = 0, pow = ratio, term;
    for (int i = 1; i < 12; i += 2) {   /* atan(r) = r - r^3/3 + r^5/5 ... */
        term = fx_div(fx_mul(pow, FX_ONE), i * FX_ONE);
        acc += (i & 2) ? -term : term;
        pow = fx_mul(fx_mul(pow, ratio), ratio);
        if (term == 0) break;
    }
    fx ang = mn == ax ? (FX_PI / 2 - acc) : acc;   /* measured from +X */
    ba out = RAD2BA(ang);
    if (dx >= 0 && dy >= 0) return out;
    if (dx < 0 && dy >= 0) return 8192 - out;
    if (dx < 0 && dy < 0) return 8192 + out;
    return 16384 - out;
}
