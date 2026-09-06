#include "rt.h"
#include <stdint.h>
typedef uint8_t u8;

typedef unsigned u32_;
typedef unsigned long long u64_;
typedef long long i64_;

void* memset(void* dst, int c, unsigned n) {
	u8* d = (u8*)dst;
	u8 v = (u8)c;
	while (n--) *d++ = v;
	return dst;
}

void* memcpy(void* dst, const void* src, unsigned n) {
	u8* d = (u8*)dst;
	const u8* s = (const u8*)src;
	while (n--) *d++ = *s++;
	return dst;
}

void* memmove(void* dst, const void* src, unsigned n) {
	u8* d = (u8*)dst;
	const u8* s = (const u8*)src;
	if (d < s) { while (n--) *d++ = *s++; }
	else { d += n; s += n; while (n--) *--d = *--s; }
	return dst;
}

int memcmp(const void* a, const void* b, unsigned n) {
	const u8* x = (const u8*)a;
	const u8* y = (const u8*)b;
	while (n--) {
		if (*x != *y) return (int)*x - (int)*y;
		x++; y++;
	}
	return 0;
}

/* ---- ARM EABI integer divide helpers (ARMv5 has no hardware divide) ---- */

int __aeabi_idiv(int num, int den) {
	int neg = 0;
	u32_ n, d, q = 0, bit;
	if (num < 0) { neg = !neg; n = (u32_)(-(i64_)num); } else n = (u32_)num;
	if (den < 0) { neg = !neg; d = (u32_)(-(i64_)den); } else d = (u32_)den;
	if (d == 0) return 0;
	if (n < d) return 0;
	bit = 1;
	while (d < n && !(d & 0x80000000u)) { d <<= 1; bit <<= 1; }
	while (bit) {
		if (n >= d) { n -= d; q |= bit; }
		d >>= 1;
		bit >>= 1;
	}
	return neg ? -(int)q : (int)q;
}

unsigned __aeabi_uidiv(unsigned n, unsigned d) {
	unsigned q = 0, bit = 1;
	if (d == 0) return 0;
	if (n < d) return 0;
	while (d < n && !(d & 0x80000000u)) { d <<= 1; bit <<= 1; }
	while (bit) {
		if (n >= d) { n -= d; q |= bit; }
		d >>= 1;
		bit >>= 1;
	}
	return q;
}



/* ---- 64-bit divide cores (register ABI shims live in divmod.s) -------- */

unsigned long long rt_udiv64(unsigned long long n, unsigned long long d,
                             unsigned long long* rem) {
	unsigned long long q = 0, r = 0;
	int i;
	if (d == 0) { *rem = 0; return 0; }
	for (i = 63; i >= 0; i--) {
		r = (r << 1) | ((n >> i) & 1u);
		if (r >= d) { r -= d; q |= 1ull << i; }
	}
	*rem = r;
	return q;
}

long long rt_ldiv64(long long num, long long den, long long* rem) {
	int na = num < 0, nb = den < 0;
	unsigned long long ua = na ? (unsigned long long)0 - (unsigned long long)num
	                           : (unsigned long long)num;
	unsigned long long ub = nb ? (unsigned long long)0 - (unsigned long long)den
	                           : (unsigned long long)den;
	unsigned long long r, q = rt_udiv64(ua, ub, &r);
	long long qq = (long long)q, rr = (long long)r;
	if (na != nb) qq = -qq;
	if (na) rr = -rr;
	*rem = rr;
	return qq;
}

long long __aeabi_lmul(long long a, long long b) {
	unsigned al = (unsigned)(unsigned long long)a, ah = (unsigned)((unsigned long long)a >> 32);
	unsigned bl = (unsigned)(unsigned long long)b, bh = (unsigned)((unsigned long long)b >> 32);
	unsigned long long lo = (unsigned long long)al * bl;
	unsigned long long mid = (unsigned long long)al * bh + (unsigned long long)ah * bl;
	unsigned long long hi = (unsigned long long)ah * bh;
	return (long long)(lo + (mid << 32)) + (long long)((long long)hi << 32);
}
