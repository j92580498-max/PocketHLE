/* Import thunks: each imported API is a tiny stub that jumps through
 * the corresponding IAT slot, exactly like a real WinCE EXE.
 * The IAT arrays live in .idata; tools/pelink.py turns __imp_dir into
 * a proper Windows CE import directory at PE-wrap time. */

    .syntax unified
    .arm

/* ---- IAT slots --------------------------------------------------------- */
    .section .idata, "aw", %progbits
    .balign 4
    .global __iat_coredll
__iat_coredll:
    .space 4 * 32
    .global __iat_gles
__iat_gles:
    .space 4 * 48
    .global __imp_dir
__imp_dir:
    .space 0x800

/* ---- coredll.dll ------------------------------------------------------- */
    .text

.macro IMP name, slot
    .global \name
\name:
    ldr ip, 90f
    ldr ip, [ip]
    bx ip
    .balign 4
90: .word __iat_coredll + 4 * \slot
.endm

IMP ExitProcess, 0
IMP GetTickCount, 1
IMP Sleep, 2
IMP GetAsyncKeyState, 3
IMP RegisterClassW, 4
IMP CreateWindowExW, 5
IMP ShowWindow, 6
IMP UpdateWindow, 7
IMP DefWindowProcW, 8
IMP PeekMessageW, 9
IMP DispatchMessageW, 10
IMP PostQuitMessage, 11
IMP DestroyWindow, 12
IMP MessageBoxW, 13
IMP waveOutOpen, 14
IMP waveOutPrepareHeader, 15
IMP waveOutUnprepareHeader, 16
IMP waveOutWrite, 17
IMP waveOutReset, 18
IMP waveOutClose, 19

/* ---- libGLES_CM.dll ---------------------------------------------------- */

.macro GIMP name, slot
    .global \name
\name:
    ldr ip, 91f
    ldr ip, [ip]
    bx ip
    .balign 4
91: .word __iat_gles + 4 * \slot
.endm

GIMP eglGetDisplay, 0
GIMP eglInitialize, 1
GIMP eglChooseConfig, 2
GIMP eglCreateWindowSurface, 3
GIMP eglCreateContext, 4
GIMP eglMakeCurrent, 5
GIMP eglSwapBuffers, 6
GIMP eglTerminate, 7
GIMP eglGetError, 8
GIMP glEnable, 9
GIMP glDisable, 10
GIMP glClear, 11
GIMP glClearColor, 12
GIMP glMatrixMode, 13
GIMP glLoadIdentity, 14
GIMP glLoadMatrixf, 15
GIMP glFrustumf, 16
GIMP glOrthof, 17
GIMP glViewport, 18
GIMP glFogf, 19
GIMP glFogfv, 20
GIMP glHint, 21
GIMP glVertexPointer, 22
GIMP glColorPointer, 23
GIMP glTexCoordPointer, 24
GIMP glEnableClientState, 25
GIMP glDisableClientState, 26
GIMP glDrawArrays, 27
GIMP glDrawElements, 28
GIMP glBindTexture, 29
GIMP glGenTextures, 30
GIMP glTexImage2D, 31
GIMP glTexParameterf, 32
GIMP glTexEnvf, 33
GIMP glBlendFunc, 34
GIMP glAlphaFunc, 35
GIMP glDepthFunc, 36
GIMP glDepthMask, 37
GIMP glShadeModel, 38
GIMP glCullFace, 39
GIMP glColor4f, 40
GIMP glGetError, 41
