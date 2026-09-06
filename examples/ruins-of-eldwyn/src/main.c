/* main.c — window, EGL context, message pump, fixed 30 Hz game loop. */
#include "win.h"
#include "gles.h"
#include "fx.h"
#include "tex.h"
#include "game.h"
#include "render.h"
#include "snd.h"

volatile u32 g_key_state;

static u16 wcls[16], wtitle[32];

static void to_wide(const char* s, u16* out) {
    while (*s) *out++ = (u16)*s++;
    *out = 0;
}

static HWND hwnd;
static u32 egl_dpy, egl_cfg, egl_surf, egl_ctx;

static u32 wnd_proc(HWND h, u32 msg, u32 wp, i32 lp) {
    if (msg == WM_DESTROY) PostQuitMessage(0);
    return DefWindowProcW(h, msg, wp, lp);
}

static int egl_setup(void) {
    egl_dpy = eglGetDisplay(0);
    if (eglInitialize(egl_dpy, 0, 0) == 0) return 0;
    i32 attribs[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RED_SIZE, 5, EGL_GREEN_SIZE, 6, EGL_BLUE_SIZE, 5,
        EGL_DEPTH_SIZE, 16,
        EGL_NONE,
    };
    i32 num = 0;
    if (eglChooseConfig(egl_dpy, attribs, &egl_cfg, 1, &num) == 0 || num < 1) return 0;
    egl_surf = eglCreateWindowSurface(egl_dpy, egl_cfg, (u32)hwnd, 0);
    egl_ctx = eglCreateContext(egl_dpy, egl_cfg, 0, 0);
    if (!egl_surf || !egl_ctx) return 0;
    if (eglMakeCurrent(egl_dpy, egl_surf, egl_surf, egl_ctx) == 0) return 0;
    return 1;
}

int WinMain(HINSTANCE inst, HINSTANCE prev, const u16* cmd, int show) {
    (void)prev; (void)cmd;

    to_wide("ELDWYN", wcls);
    to_wide("Ruins of Eldwyn", wtitle);

    WNDCLASSW wc;
    wc.style = 0;
    wc.lpfnWndProc = (u32)wnd_proc;
    wc.cbClsExtra = 0;
    wc.cbWndExtra = 0;
    wc.hInstance = (u32)inst;
    wc.hIcon = 0;
    wc.hCursor = 0;
    wc.hbrBackground = 0;
    wc.lpszMenuName = 0;
    wc.lpszClassName = wcls;
    RegisterClassW(&wc);

    hwnd = CreateWindowExW(0, wcls, wtitle, 0,
                           0, 0, 240, 320, 0, 0, (u32)inst, 0);
    if (!hwnd) ExitProcess(1);
    ShowWindow(hwnd, SW_SHOWNORMAL);
    UpdateWindow(hwnd);

    if (!egl_setup()) {
        MessageBoxW(0, wtitle, wcls, 0);
        ExitProcess(2);
    }
    glViewport(0, 0, 240, 320);

    tex_build_all();
    tex_upload_all();
    game_reset();
    snd_init();
    renderer_init();

    u32 prev_a = 0, prev_b = 0;
    for (;;) {
        MSG m;
        while (PeekMessageW(&m, 0, 0, 0, 1)) {
            if (m.message == WM_QUIT) goto done;
            DispatchMessageW(&m);
        }

        u32 ks = 0;
        if (GetAsyncKeyState(VK_UP) & 0x8000) ks |= 1;
        if (GetAsyncKeyState(VK_DOWN) & 0x8000) ks |= 2;
        if (GetAsyncKeyState(VK_LEFT) & 0x8000) ks |= 4;
        if (GetAsyncKeyState(VK_RIGHT) & 0x8000) ks |= 8;
        g_key_state = ks;

        u32 a = (GetAsyncKeyState(VK_A) & 0x8000) != 0;
        if (a && !prev_a) game_action();
        prev_a = a;

        u32 b = (GetAsyncKeyState(VK_B) & 0x8000) != 0;
        if (b && !prev_b) game_action2();
        prev_b = b;

        game_update(32);
        snd_frame();
        renderer_frame();
        eglSwapBuffers(egl_dpy, egl_surf);
        GetTickCount();
    }
done:
    eglTerminate(egl_dpy);
    return 0;
}
