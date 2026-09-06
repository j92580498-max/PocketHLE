/* Minimal guest-side surface for what the game uses of Windows CE. */
#ifndef WIN_H
#define WIN_H

#include "fx.h"

typedef u32 HWND;
typedef u32 HINSTANCE;

#define SW_SHOWNORMAL 1

typedef struct {
    u32 style;
    u32 lpfnWndProc;
    int cbClsExtra;
    int cbWndExtra;
    u32 hInstance;
    u32 hIcon;
    u32 hCursor;
    u32 hbrBackground;
    const u16* lpszMenuName;
    const u16* lpszClassName;
} WNDCLASSW;

typedef struct {
    u32 message;
    u32 wParam;
    i32 lParam;
} MSG;

#define WM_PAINT    0x000F
#define WM_KEYDOWN  0x0100
#define WM_KEYUP    0x0101
#define WM_QUIT     0x0012
#define WM_CLOSE    0x0010
#define WM_DESTROY  0x0002

/* --- coredll imports --------------------------------------------------- */
extern void ExitProcess(u32 code);
extern u32 GetTickCount(void);
extern void Sleep(u32 ms);
extern i16 GetAsyncKeyState(i32 vk);
extern u16 RegisterClassW(const WNDCLASSW* wc);
extern HWND CreateWindowExW(u32 exstyle, const u16* cls, const u16* name,
                            u32 style, int x, int y, int w, int h,
                            HWND parent, u32 menu, u32 inst, u32 param);
extern int ShowWindow(HWND h, int cmd);
extern int UpdateWindow(HWND h);
extern i32 DefWindowProcW(HWND h, u32 msg, u32 wp, i32 lp);
extern void PostQuitMessage(int code);
extern int PeekMessageW(MSG* m, HWND h, u32 min, u32 max, u32 remove);
extern int TranslateMessage(const MSG* m);
extern i32 DispatchMessageW(const MSG* m);
extern int DestroyWindow(HWND h);
extern u32 GetModuleFileNameW(u32 mod, u16* out, u32 len);
extern int MessageBoxW(HWND h, const u16* text, const u16* caption, u32 type);

/* waveOut */
#define WAVE_MAPPER 0xFFFFFFFFu
#define CALLBACK_NULL 0
typedef struct {
    u16 wFormatTag;
    u16 nChannels;
    u32 nSamplesPerSec;
    u32 nAvgBytesPerSec;
    u16 nBlockAlign;
    u16 wBitsPerSample;
    u16 cbSize;
} WAVEFORMATEX;
typedef struct {
    const u8* lpData;
    u32 dwBufferLength;
    u32 dwBytesRecorded;
    u32 dwUser;
    u32 dwFlags;
    u32 dwLoops;
    u32 lpNext;
    u32 reserved;
} WAVEHDR;
#define WHDR_DONE 1
#define WHDR_INQUEUE 0x10
extern u32 waveOutOpen(u32* h, u32 dev, const WAVEFORMATEX* fmt, u32 cb, u32 inst, u32 flags);
extern u32 waveOutPrepareHeader(u32 h, WAVEHDR* hdr, u32 size);
extern u32 waveOutUnprepareHeader(u32 h, WAVEHDR* hdr, u32 size);
extern u32 waveOutWrite(u32 h, WAVEHDR* hdr, u32 size);
extern u32 waveOutReset(u32 h);
extern u32 waveOutClose(u32 h);

/* GAPI virtual keys the CLI maps onto */
#define VK_UP 0x26
#define VK_DOWN 0x28
#define VK_LEFT 0x25
#define VK_RIGHT 0x27
#define VK_A 0xD1
#define VK_B 0xD2
#define VK_START 0xD4

#endif
