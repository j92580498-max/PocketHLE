#ifndef RT_H
#define RT_H

/* Minimal freestanding runtime for the ARMv4T guest.
 *
 * The EABI divmod helpers are hand-written: their ABI returns the
 * remainder in a second register pair (value-in-regs struct return),
 * which clang 14 cannot emit. The quotient/remainder cores live in C,
 * the register-ABI shims live in src/divmod.s. */

void* memset(void* dst, int c, unsigned n);
void* memcpy(void* dst, const void* src, unsigned n);
void* memmove(void* dst, const void* src, unsigned n);
int memcmp(const void* a, const void* b, unsigned n);

int __aeabi_idiv(int a, int b);
unsigned __aeabi_uidiv(unsigned a, unsigned b);
long long __aeabi_lmul(long long a, long long b);

/* C cores used by the divmod shims */
unsigned long long rt_udiv64(unsigned long long n, unsigned long long d,
                             unsigned long long* rem);
long long rt_ldiv64(long long n, long long d, long long* rem);

#endif
