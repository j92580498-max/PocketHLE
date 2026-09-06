/* EABI divmod shims. clang 14 cannot emit the value-in-regs struct
 * return these helpers use ({q, r} split across two register pairs),
 * so the register-level ABI is written by hand around the C cores in
 * rt.c. Everything is ARM-mode, ARMv4T-clean. */

    .syntax unified
    .arm

/* r0 = quotient, r1 = remainder */
    .global __aeabi_idivmod
__aeabi_idivmod:
    push {r4, r5, lr}
    mov  r4, r0
    mov  r5, r1
    bl   __aeabi_idiv
    mul  r1, r0, r5
    rsb  r1, r1, r4
    pop  {r4, r5, pc}

    .global __aeabi_uidivmod
__aeabi_uidivmod:
    push {r4, r5, lr}
    mov  r4, r0
    mov  r5, r1
    bl   __aeabi_uidiv
    mul  r1, r0, r5
    rsb  r1, r1, r4
    pop  {r4, r5, pc}

/* {r0,r1} = quotient, {r2,r3} = remainder */
    .global __aeabi_uldivmod
__aeabi_uldivmod:
    push {r4, lr}
    sub  sp, sp, #8
    mov  r4, sp
    str  r4, [sp]
    bl   rt_udiv64
    ldr  r2, [r4]
    ldr  r3, [r4, #4]
    add  sp, sp, #8
    pop  {r4, pc}

    .global __aeabi_ldivmod
__aeabi_ldivmod:
    push {r4, lr}
    sub  sp, sp, #8
    mov  r4, sp
    str  r4, [sp]
    bl   rt_ldiv64
    ldr  r2, [r4]
    ldr  r3, [r4, #4]
    add  sp, sp, #8
    pop  {r4, pc}
