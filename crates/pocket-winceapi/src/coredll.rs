//! Skeleton handlers for `coredll.dll`.
//!
//! Coverage strategy: every coredll symbol that JumpyBall (our test
//! ROM) imports has a handler so that the trace is never silent. The
//! handlers fall into three buckets:
//!
//! 1. **Real implementations** — string/memory CRT routines that read
//!    and write the guest's address space. These have to behave
//!    correctly for the game to make any progress.
//! 2. **Fake handle / non-zero stubs** — for `Create*` functions, we
//!    return a non-null but obviously fake handle (`0xDEAD_xxxx`).
//!    The game's `if (h != NULL)` checks succeed and execution
//!    continues into the rendering path.
//! 3. **`zero_returning` / `one_returning` placeholders** — for
//!    everything else we just answer with `0` or `TRUE` and rely on
//!    the trace log to tell us when a deeper implementation is needed.
//!
//! The `__chkstk` / `_setjmp` / `longjmp` / `_except_handler3` quartet
//! deserves its own attention: those are CRT helpers the MS C compiler
//! emits in nearly every function prologue and `try`/`except` block,
//! and they get called many thousands of times before the game ever
//! reaches `WinMain`.

use pocket_cpu::regs::ArmReg;
use pocket_kernel::framebuffer::{colorref_to_rgb565, FB_HEIGHT, FB_WIDTH};
use pocket_kernel::gdi::{
    Surface, GDI_SCREEN_DC, STOCK_BLACK_BRUSH, STOCK_BLACK_PEN, STOCK_NULL_BRUSH, STOCK_NULL_PEN,
    STOCK_WHITE_BRUSH, STOCK_WHITE_PEN,
};
use pocket_kernel::{
    DispatchOutcome, GuestThread, KernelError, VectorIterFrame, FAKE_CURRENT_PROCESS_HANDLE,
    FAKE_CURRENT_THREAD_HANDLE, THREAD_EXIT_TRAMPOLINE_BASE, TLS_SLOT_COUNT,
    USER_KDATA_TLS_ARRAY_VA,
};
use pocket_pe::ResourceKey;

use crate::{CallCtx, WinCeDispatcher};

const FAKE_MODULE_HANDLE: u32 = 0x1000_0000;
const FAKE_HWND: u32 = 0xDEAD_0001;
const INVALID_HANDLE_VALUE: u32 = 0xFFFF_FFFF;
const PAINTSTRUCT_BYTES: u32 = 32;

pub fn register(d: &mut WinCeDispatcher) {
    let dll = "coredll.dll";

    // ---- Process / module / library ----
    d.register_handler(dll, "GetTickCount", get_tick_count);
    d.register_handler(dll, "Sleep", sleep);
    d.register_handler(dll, "ResumeThread", resume_thread);
    d.register_handler(dll, "ExitProcess", exit_process);
    d.register_handler(dll, "TerminateProcess", exit_process);
    d.register_constant(dll, "GetLastError", 0, zero_returning);
    d.register_constant(dll, "SetLastError", 0, zero_returning);
    d.register_handler(dll, "GetCommandLineW", get_command_line_w);
    d.register_handler(dll, "GetModuleHandleW", get_module_handle_w);
    d.register_handler(dll, "GetModuleFileNameW", get_module_file_name_w);
    d.register_constant(dll, "GetProcAddress", 0, null_returning);
    d.register_handler(dll, "LoadLibraryW", load_library_w);
    d.register_constant(dll, "FreeLibrary", 1, one_returning);

    // ---- CRT prologue helpers ----
    d.register_handler(dll, "__chkstk", chkstk);
    d.register_handler(dll, "_setjmp", setjmp);
    d.register_handler(dll, "longjmp", longjmp);
    d.register_handler(dll, "_except_handler3", except_handler3);

    // ---- ARMv4 soft-float helpers (no VFP). Names follow the EVC4
    // convention: `s` = single-precision, `d` = double-precision,
    // `i` = i32, `u` = u32, `i64` = i64, `u64` = u64.
    d.register_handler(dll, "__adds", soft_adds);
    d.register_handler(dll, "__subs", soft_subs);
    d.register_handler(dll, "__muls", soft_muls);
    d.register_handler(dll, "__divs", soft_divs);
    d.register_handler(dll, "__negs", soft_negs);
    d.register_handler(dll, "__cmps", soft_cmps);
    d.register_handler(dll, "__eqs", soft_eqs);
    d.register_handler(dll, "__nes", soft_nes);
    d.register_handler(dll, "__lts", soft_lts);
    d.register_handler(dll, "__les", soft_les);
    d.register_handler(dll, "__gts", soft_gts);
    d.register_handler(dll, "__ges", soft_ges);
    d.register_handler(dll, "__itos", soft_itos);
    d.register_handler(dll, "__utos", soft_utos);
    d.register_handler(dll, "__stoi", soft_stoi);
    d.register_handler(dll, "__stou", soft_stou);
    d.register_handler(dll, "__stod", soft_stod);
    d.register_handler(dll, "__addd", soft_addd);
    d.register_handler(dll, "__subd", soft_subd);
    d.register_handler(dll, "__muld", soft_muld);
    d.register_handler(dll, "__divd", soft_divd);
    d.register_handler(dll, "__negd", soft_negd);
    d.register_handler(dll, "__cmpd", soft_cmpd);
    d.register_handler(dll, "__eqd", soft_eqd);
    d.register_handler(dll, "__ned", soft_ned);
    d.register_handler(dll, "__ltd", soft_ltd);
    d.register_handler(dll, "__led", soft_led);
    d.register_handler(dll, "__gtd", soft_gtd);
    d.register_handler(dll, "__ged", soft_ged);
    d.register_handler(dll, "__itod", soft_itod);
    d.register_handler(dll, "__utod", soft_utod);
    d.register_handler(dll, "__dtoi", soft_dtoi);
    d.register_handler(dll, "__dtou", soft_dtou);
    d.register_handler(dll, "__dtos", soft_dtos);

    // ---- Memory / string CRT ----
    d.register_handler(dll, "memset", memset);
    d.register_handler(dll, "memcpy", memcpy);
    d.register_handler(dll, "memmove", memcpy);
    d.register_handler(dll, "memcmp", memcmp);
    d.register_handler(dll, "strlen", strlen);
    d.register_handler(dll, "wcslen", wcslen);
    d.register_handler(dll, "strcpy", strcpy);
    d.register_handler(dll, "strncpy", strncpy);
    d.register_handler(dll, "strcat", strcat);
    d.register_handler(dll, "strncat", strncat);
    d.register_handler(dll, "strcmp", strcmp);
    d.register_handler(dll, "strncmp", strncmp);
    d.register_handler(dll, "strchr", strchr);
    d.register_handler(dll, "strrchr", strrchr);
    d.register_handler(dll, "strstr", strstr);
    d.register_handler(dll, "_strdup", strdup);
    d.register_handler(dll, "tolower", tolower);
    d.register_handler(dll, "toupper", toupper);
    d.register_handler(dll, "wcscpy", wcscpy);
    d.register_handler(dll, "wcsncpy", wcsncpy);
    d.register_handler(dll, "wcscat", wcscat);
    d.register_handler(dll, "wcsncat", wcsncat);
    d.register_handler(dll, "wcscmp", wcscmp);
    d.register_handler(dll, "wcsncmp", wcsncmp);
    d.register_handler(dll, "_wcsnicmp", wcsnicmp);
    d.register_handler(dll, "_wcsicmp", wcsicmp);
    d.register_handler(dll, "_wcsdup", wcsdup);
    d.register_handler(dll, "wcschr", wcschr);
    d.register_handler(dll, "wcsrchr", wcsrchr);
    d.register_handler(dll, "wcsstr", wcsstr);
    d.register_handler(dll, "swprintf", swprintf);
    d.register_handler(dll, "wsprintfW", swprintf);
    d.register_handler(dll, "sprintf", sprintf);
    d.register_handler(dll, "printf", printf);
    d.register_handler(dll, "wsprintfA", sprintf);
    // CRT variadic printers — unimplemented before this PR, which made
    // the game pass uninitialized stack memory to subsequent code paths
    // (Zuma in particular feeds the result into a vector size that
    // then asks for a 2 GiB allocation).
    d.register_handler(dll, "vsprintf", vsprintf);
    d.register_handler(dll, "_vsnprintf", vsnprintf);
    d.register_handler(dll, "vsnprintf", vsnprintf);
    d.register_handler(dll, "_snprintf", snprintf);
    d.register_handler(dll, "_snwprintf", snwprintf);
    d.register_handler(dll, "vswprintf", vswprintf);
    d.register_handler(dll, "_vsnwprintf", vsnwprintf);
    d.register_handler(dll, "wcstombs", wcstombs);
    d.register_handler(dll, "mbstowcs", mbstowcs);

    // ---- File I/O backed by the VFS ----
    d.register_handler(dll, "CreateFileW", create_file_w);
    d.register_handler(dll, "ReadFile", read_file);
    d.register_handler(dll, "WriteFile", write_file);
    d.register_handler(dll, "CloseHandle", close_handle);
    d.register_handler(dll, "GetFileSize", get_file_size);
    d.register_handler(dll, "GlobalMemoryStatus", global_memory_status);
    d.register_handler(dll, "SetFilePointer", set_file_pointer);
    d.register_handler(dll, "FindFirstFileW", invalid_handle_returning);
    d.register_constant(dll, "FindNextFileW", 0, zero_returning);
    d.register_constant(dll, "FindClose", 1, one_returning);
    d.register_constant(dll, "DeleteFileW", 1, one_returning);
    d.register_constant(dll, "SetFileAttributesW", 1, one_returning);
    d.register_handler(dll, "GetFileAttributesW", get_file_attributes_w);
    d.register_constant(dll, "CreateDirectoryW", 1, one_returning);

    // ---- C-runtime style file I/O on top of the same VFS ----
    d.register_handler(dll, "fopen", crt_fopen);
    d.register_handler(dll, "_wfopen", crt_wfopen);
    d.register_handler(dll, "fclose", crt_fclose);
    d.register_handler(dll, "fread", crt_fread);
    d.register_handler(dll, "fwrite", crt_fwrite);
    d.register_handler(dll, "fseek", crt_fseek);
    d.register_handler(dll, "ftell", crt_ftell);
    d.register_handler(dll, "feof", crt_feof);
    d.register_constant(dll, "fflush", 1, one_returning);
    d.register_handler(dll, "fgetc", crt_fgetc);
    d.register_handler(dll, "fputc", crt_fputc);
    d.register_handler(dll, "fgets", crt_fgets);
    d.register_handler(dll, "fputs", crt_fputs);
    d.register_handler(dll, "rewind", crt_rewind);

    // ---- ARM signed/unsigned division helpers (MS compiler).
    // Microsoft's `__rt_*div` family has `r0=divisor, r1=dividend`
    // (flipped from the AEABI helpers). Result is in r0, remainder
    // in r1. (See LLVM commit `rL283383` for the canonical
    // documentation of this quirk.)
    d.register_handler(dll, "__rt_sdiv", rt_sdiv);
    d.register_handler(dll, "__rt_udiv", rt_udiv);
    d.register_handler(dll, "__rt_sdiv64", rt_sdiv64);
    d.register_handler(dll, "__rt_udiv64", rt_udiv64);
    d.register_handler(dll, "__rt_srsh", rt_srsh);
    d.register_handler(dll, "__rt_sdiv10", rt_sdiv10);
    d.register_handler(dll, "__rt_udiv10", rt_udiv10);

    // ---- Heap ----
    d.register_handler(dll, "LocalAlloc", local_alloc);
    d.register_handler(dll, "LocalFree", local_free);
    d.register_handler(dll, "LocalReAlloc", local_realloc);
    d.register_handler(dll, "LocalSize", local_size);
    d.register_handler(dll, "_msize", local_size);
    d.register_handler(dll, "HeapCreate", heap_create);
    d.register_constant(dll, "HeapDestroy", 1, one_returning);
    d.register_handler(dll, "HeapAlloc", heap_alloc);
    d.register_handler(dll, "HeapFree", heap_free);
    d.register_handler(dll, "HeapReAlloc", heap_realloc);
    d.register_handler(dll, "GetProcessHeap", get_process_heap);
    d.register_handler(dll, "VirtualAlloc", virtual_alloc);
    d.register_constant(dll, "VirtualFree", 1, one_returning);
    d.register_handler(dll, "malloc", malloc);
    d.register_handler(dll, "calloc", calloc);
    d.register_handler(dll, "free", free);
    d.register_handler(dll, "realloc", realloc);
    d.register_handler(dll, "_new", malloc);
    d.register_handler(dll, "_delete", free);
    // MSVC-mangled C++ scalar new/delete:
    //   ??2@YAPAXI@Z  = void* operator new(unsigned int)
    //   ??3@YAXPAX@Z  = void  operator delete(void*)
    //   ??_U@YAPAXI@Z = void* operator new[](unsigned int)
    //   ??_V@YAXPAX@Z = void  operator delete[](void*)
    d.register_handler(dll, "??2@YAPAXI@Z", malloc);
    d.register_handler(dll, "??3@YAXPAX@Z", free);
    d.register_handler(dll, "??_U@YAPAXI@Z", malloc);
    d.register_handler(dll, "??_V@YAXPAX@Z", free);

    // ---- Resources ----
    d.register_handler(dll, "FindResourceW", find_resource_w);
    d.register_handler(dll, "LoadResource", load_resource);
    d.register_handler(dll, "LockResource", lock_resource);
    d.register_handler(dll, "SizeofResource", sizeof_resource);
    d.register_handler(dll, "LoadBitmapW", load_bitmap_w);
    d.register_handler(dll, "GetObjectW", get_object_w);
    d.register_handler(dll, "LoadStringW", load_string_w);

    // ---- Window / message stubs ----
    d.register_handler(dll, "RegisterClassW", register_class_w);
    d.register_handler(dll, "CreateWindowExW", create_window_ex_w);
    d.register_handler(dll, "SetWindowLongW", set_window_long_w);
    d.register_handler(dll, "SetWindowLongA", set_window_long_w);
    d.register_handler(dll, "GetWindowLongW", get_window_long_w);
    d.register_handler(dll, "GetWindowLongA", get_window_long_w);
    d.register_handler(dll, "GetVersionExW", get_version_ex_w);
    d.register_handler(dll, "GetVersionExA", get_version_ex_w);
    d.register_handler(dll, "DestroyWindow", destroy_window);
    d.register_handler(dll, "FindWindowW", find_window_w);
    d.register_handler(dll, "GetVersion", get_version);
    d.register_handler(dll, "ShowWindow", show_window);
    d.register_handler(dll, "UpdateWindow", update_window);
    d.register_constant(dll, "MoveWindow", 1, one_returning);
    d.register_constant(dll, "SetForegroundWindow", 1, one_returning);
    d.register_handler(dll, "GetKeyState", get_key_state);
    d.register_handler(dll, "GetAsyncKeyState", get_async_key_state);
    d.register_handler(dll, "GetFocus", get_focus);
    d.register_handler(dll, "GetCapture", get_capture);
    d.register_constant(dll, "SetCapture", FAKE_HWND, one_returning);
    d.register_constant(dll, "ReleaseCapture", 1, one_returning);
    d.register_constant(dll, "SetFocus", 1, one_returning);
    d.register_constant(dll, "SetWindowPos", 1, one_returning);
    d.register_handler(dll, "SetWindowTextW", set_window_text_w);
    d.register_handler(dll, "SetWindowTextA", set_window_text_a);
    d.register_handler(dll, "GetWindowTextW", get_window_text_w);
    d.register_handler(dll, "GetWindowTextA", get_window_text_w);
    d.register_constant(dll, "GetWindowTextLengthW", 0, zero_returning);
    d.register_constant(dll, "GetWindowTextLengthA", 0, zero_returning);
    d.register_constant(dll, "DefWindowProcW", 0, zero_returning);
    d.register_handler(dll, "DispatchMessageW", dispatch_message_w);
    d.register_handler(dll, "GetMessageW", get_message_w);
    d.register_handler(dll, "PeekMessageW", peek_message_w);
    d.register_constant(dll, "TranslateMessage", 1, one_returning);
    d.register_handler(dll, "PostQuitMessage", post_quit_message);
    d.register_handler(dll, "CreateProcessW", create_process_w);
    d.register_constant(dll, "PostMessageW", 1, one_returning);
    d.register_handler(
        dll,
        "MsgWaitForMultipleObjectsEx",
        msg_wait_for_multiple_objects,
    );
    d.register_handler(
        dll,
        "MsgWaitForMultipleObjects",
        msg_wait_for_multiple_objects,
    );
    d.register_constant(dll, "EnableWindow", 1, one_returning);
    d.register_constant(dll, "MessageBeep", 1, one_returning);
    d.register_handler(dll, "PlaySoundW", play_sound_w);
    d.register_handler(dll, "PlaySoundA", play_sound_w);
    d.register_handler(dll, "sndPlaySoundW", play_sound_w);
    d.register_handler(dll, "sndPlaySoundA", play_sound_w);
    // Real-time audio backend (cpal). When the host has no output
    // device or the `audio-cpal` feature is disabled these silently
    // act as no-ops, so games keep running. Otherwise PCM samples
    // submitted via `waveOutWrite` actually reach the speaker.
    d.register_handler(dll, "waveOutGetVolume", wave_out_get_volume);
    d.register_handler(dll, "waveOutSetVolume", wave_out_set_volume);
    d.register_handler(dll, "waveOutOpen", wave_out_open);
    d.register_handler(dll, "waveOutClose", wave_out_close);
    d.register_handler(dll, "waveOutWrite", wave_out_write);
    d.register_handler(dll, "waveOutReset", wave_out_reset);
    d.register_constant(dll, "waveOutPause", 1, one_returning);
    d.register_constant(dll, "waveOutRestart", 1, one_returning);
    d.register_handler(dll, "waveOutPrepareHeader", wave_out_prepare_header);
    d.register_handler(dll, "waveOutUnprepareHeader", wave_out_unprepare_header);
    d.register_handler(dll, "waveOutGetNumDevs", wave_out_get_num_devs);
    d.register_constant(dll, "waveOutGetDevCapsW", 0, zero_returning);
    d.register_constant(dll, "waveOutGetPosition", 0, zero_returning);
    d.register_constant(dll, "waveOutMessage", 0, zero_returning);
    d.register_handler(dll, "setjmp", setjmp);
    d.register_constant(dll, "longjmp", 0, zero_returning);
    d.register_constant(dll, "SendMessageW", 0, zero_returning);
    d.register_handler(dll, "InvalidateRect", invalidate_rect);
    d.register_constant(dll, "ValidateRect", 1, one_returning);
    d.register_handler(dll, "GetSystemMetrics", get_system_metrics);
    d.register_handler(dll, "GetClientRect", get_client_rect);
    d.register_handler(dll, "GetWindowRect", get_window_rect);
    d.register_handler(dll, "GetCursorPos", get_cursor_pos);
    d.register_handler(dll, "SetCursor", set_cursor);
    d.register_handler(dll, "GetClassInfoW", get_class_info_w);
    d.register_handler(
        dll,
        "CreateDialogIndirectParamW",
        create_dialog_indirect_param_w,
    );
    d.register_handler(dll, "IsWindow", is_window);
    d.register_handler(dll, "CreateMutexW", create_mutex_w);
    d.register_handler(dll, "TlsCall", tls_call);
    d.register_handler(dll, "CeSetThreadQuantum", ce_set_thread_quantum);
    d.register_constant(dll, "ClientToScreen", 1, one_returning);
    d.register_constant(dll, "ScreenToClient", 1, one_returning);
    d.register_handler(dll, "LoadIconW", load_icon_w);
    d.register_handler(dll, "LoadCursorW", load_icon_w);
    d.register_handler(dll, "LoadAcceleratorsW", load_accelerators_w);
    d.register_constant(dll, "TranslateAcceleratorW", 0, zero_returning);
    d.register_handler(dll, "DialogBoxIndirectParamW", dialog_box_indirect_param_w);
    d.register_handler(dll, "DialogBoxParamW", dialog_box_indirect_param_w);
    d.register_constant(dll, "EndDialog", 1, one_returning);
    d.register_handler(dll, "MessageBoxW", message_box_w);
    d.register_handler(dll, "SetTimer", set_timer);
    d.register_constant(dll, "KillTimer", 1, one_returning);

    // ---- GDI (real, framebuffer-backed) ----
    d.register_handler(dll, "GetDC", get_dc);
    d.register_constant(dll, "ReleaseDC", 1, one_returning);
    d.register_handler(dll, "BeginPaint", begin_paint);
    d.register_handler(dll, "EndPaint", end_paint);
    d.register_handler(dll, "CreateCompatibleDC", create_compatible_dc);
    d.register_handler(dll, "CreateCompatibleBitmap", create_compatible_bitmap);
    d.register_handler(dll, "CreateDIBSection", create_dib_section);
    d.register_handler(dll, "CreateBitmap", create_bitmap);
    d.register_handler(dll, "CreateSolidBrush", create_solid_brush);
    d.register_handler(dll, "CreatePen", create_pen);
    d.register_handler(dll, "CreateFontIndirectW", create_font_indirect);
    d.register_handler(dll, "GetStockObject", get_stock_object);
    d.register_handler(dll, "SelectObject", select_object);
    d.register_handler(dll, "DeleteObject", delete_object);
    d.register_handler(dll, "DeleteDC", delete_object);
    d.register_handler(dll, "BitBlt", bit_blt);
    d.register_handler(dll, "StretchBlt", stretch_blt);
    d.register_handler(dll, "PatBlt", pat_blt);
    d.register_handler(dll, "Rectangle", rectangle);
    d.register_handler(dll, "Ellipse", ellipse);
    d.register_handler(dll, "RoundRect", rectangle);
    d.register_constant(dll, "Polygon", 1, one_returning);
    d.register_constant(dll, "Polyline", 1, one_returning);
    d.register_constant(dll, "MoveToEx", 1, one_returning);
    d.register_constant(dll, "LineTo", 1, one_returning);
    d.register_handler(dll, "FillRect", fill_rect);
    d.register_handler(dll, "FrameRect", fill_rect);
    d.register_handler(dll, "DrawTextW", draw_text_w);
    d.register_constant(dll, "DrawEdge", 1, one_returning);
    d.register_constant(dll, "DrawFocusRect", 1, one_returning);
    d.register_handler(dll, "SetBkMode", set_bk_mode);
    d.register_handler(dll, "SetBkColor", set_bk_color);
    d.register_handler(dll, "SetTextColor", set_text_color);
    d.register_handler(dll, "TextOutW", text_out_w);
    d.register_handler(dll, "ExtTextOutW", ext_text_out_w);
    d.register_handler(dll, "ExtEscape", ext_escape);
    d.register_handler(dll, "Escape", ext_escape);
    d.register_handler(dll, "GetDeviceCaps", get_device_caps);
    d.register_constant(dll, "SetROP2", 1, one_returning);
    d.register_constant(dll, "SetStretchBltMode", 1, one_returning);
    d.register_constant(dll, "GdiSetBatchLimit", 1, one_returning);
    d.register_constant(dll, "GdiFlush", 1, one_returning);
    d.register_handler(dll, "SetDIBitsToDevice", set_di_bits_to_device);
    d.register_handler(dll, "StretchDIBits", stretch_di_bits);
    d.register_constant(dll, "SetDIBits", 1, one_returning);
    d.register_constant(dll, "GetDIBits", 0, zero_returning);
    d.register_handler(dll, "GetPixel", get_pixel);
    d.register_handler(dll, "SetPixel", set_pixel);
    d.register_handler(dll, "GetSysColor", get_sys_color);
    d.register_handler(dll, "GetSysColorBrush", get_sys_color_brush);

    // ---- Window / desktop helpers ----
    d.register_handler(dll, "GetDesktopWindow", get_desktop_window);
    d.register_handler(dll, "GetForegroundWindow", get_foreground_window);
    d.register_handler(dll, "GetActiveWindow", get_active_window);
    d.register_handler(dll, "SetForegroundWindow", set_foreground_window);
    d.register_handler(dll, "GetParent", get_parent);
    d.register_handler(dll, "GetWindow", get_window);

    // ---- Menu APIs (Pocket PC games rarely show a menu but check
    // / update menu state on most game-state transitions). All
    // calls are tracked through a tiny in-memory bookkeeping table
    // so `GetSubMenu` returns a stable handle and `CheckMenuItem`
    // remembers the bit so a follow-up `GetMenuState` agrees.
    d.register_handler(dll, "LoadMenuW", load_menu_w);
    d.register_handler(dll, "LoadMenuA", load_menu_w);
    d.register_handler(dll, "LoadMenuIndirectW", load_menu_w);
    d.register_constant(dll, "GetMenu", 0, null_returning);
    d.register_constant(dll, "SetMenu", 1, one_returning);
    d.register_handler(dll, "DestroyMenu", destroy_menu);
    d.register_handler(dll, "GetSubMenu", get_sub_menu);
    d.register_handler(dll, "CreateMenu", create_menu);
    d.register_handler(dll, "CreatePopupMenu", create_menu);
    d.register_handler(dll, "GetMenuItemCount", get_menu_item_count);
    d.register_handler(dll, "GetMenuItemID", get_menu_item_id);
    d.register_handler(dll, "GetMenuState", get_menu_state);
    d.register_handler(dll, "CheckMenuItem", check_menu_item);
    d.register_handler(dll, "EnableMenuItem", enable_menu_item);
    d.register_handler(dll, "AppendMenuW", append_menu);
    d.register_handler(dll, "AppendMenuA", append_menu);
    d.register_handler(dll, "InsertMenuW", append_menu);
    d.register_handler(dll, "InsertMenuA", append_menu);
    d.register_handler(dll, "ModifyMenuW", modify_menu_w);
    d.register_handler(dll, "ModifyMenuA", modify_menu_w);
    d.register_handler(dll, "RemoveMenu", remove_menu_item);
    d.register_handler(dll, "DeleteMenu", remove_menu_item);
    d.register_handler(dll, "TrackPopupMenu", track_popup_menu);
    d.register_handler(dll, "TrackPopupMenuEx", track_popup_menu);
    d.register_constant(dll, "SetMenuItemInfoW", 1, one_returning);
    d.register_constant(dll, "GetMenuItemInfoW", 1, one_returning);
    d.register_constant(dll, "DrawMenuBar", 1, one_returning);

    // ---- Random / time ----
    d.register_handler(dll, "rand", rand_handler);
    // `Random()` is the WinCE-specific export used by the EVC4
    // CRT (Lawn Bowl, Enigma, …). Behaviourally identical to `rand`.
    d.register_handler(dll, "Random", rand_handler);
    d.register_handler(dll, "srand", srand_handler);
    d.register_handler(dll, "time", time_handler);
    // Backed by the same `i64 / i64 -> (i64, i64)` divider as
    // `__rt_sdiv64` — the EVC4 CRT exports both names to mean the
    // same thing (`rt_sdiv64by64` is the explicitly-typed one
    // emitted for `int64_t / int64_t` while `rt_sdiv64` is the
    // generic export that always promotes the divisor).
    d.register_handler(dll, "__rt_sdiv64by64", rt_sdiv64);
    d.register_handler(dll, "__rt_udiv64by64", rt_udiv64);

    // ---- Misc kernel/IPC stubs ----
    d.register_constant(dll, "KernelIoControl", 0, zero_returning);
    d.register_constant(dll, "SystemParametersInfoW", 1, one_returning);
    d.register_constant(dll, "GetSystemPowerStatusEx", 1, one_returning);
    d.register_constant(dll, "EventModify", 1, one_returning);
    d.register_handler(dll, "CreateEventW", create_event_w);
    d.register_constant(dll, "SetEvent", 1, one_returning);
    d.register_constant(dll, "ResetEvent", 1, one_returning);
    d.register_constant(dll, "WaitForSingleObject", 0, zero_returning);
    d.register_constant(dll, "InitializeCriticalSection", 0, zero_returning);
    d.register_constant(dll, "DeleteCriticalSection", 0, zero_returning);
    d.register_constant(dll, "EnterCriticalSection", 0, zero_returning);
    d.register_constant(dll, "LeaveCriticalSection", 0, zero_returning);
    d.register_handler(dll, "GetCurrentThreadId", get_current_thread_id);
    d.register_handler(dll, "GetCurrentProcessId", get_current_thread_id);
    d.register_handler(dll, "GetCurrentProcess", get_current_process);
    d.register_handler(dll, "GetCurrentThread", get_current_thread);
    d.register_handler(dll, "CreateThread", create_thread);
    d.register_handler(dll, "WaitForMultipleObjects", wait_for_multiple_objects);
    d.register_constant(dll, "SetThreadPriority", 1, one_returning);
    d.register_constant(dll, "TerminateThread", 1, one_returning);

    // ---- Thread-local storage ----
    d.register_handler(dll, "TlsAlloc", tls_alloc);
    d.register_handler(dll, "TlsFree", tls_free);
    d.register_handler(dll, "TlsGetValue", tls_get_value);
    d.register_handler(dll, "TlsSetValue", tls_set_value);

    // ---- Interlocked ops (single-threaded HLE: just do the op) ----
    d.register_handler(dll, "InterlockedIncrement", interlocked_increment);
    d.register_handler(dll, "InterlockedDecrement", interlocked_decrement);
    d.register_handler(dll, "InterlockedExchange", interlocked_exchange);
    d.register_handler(dll, "InterlockedExchangeAdd", interlocked_exchange_add);
    d.register_handler(
        dll,
        "InterlockedCompareExchange",
        interlocked_compare_exchange,
    );

    // ---- Misc time / random ----
    d.register_handler(dll, "GetSystemTime", get_system_time);
    d.register_handler(dll, "GetLocalTime", get_system_time);
    d.register_handler(dll, "GetSystemTimeAsFileTime", get_system_time_as_file_time);
    d.register_handler(dll, "GetCurrentFT", get_system_time_as_file_time);
    d.register_handler(dll, "SystemTimeToFileTime", system_time_to_file_time);
    d.register_handler(dll, "FileTimeToSystemTime", file_time_to_system_time);
    d.register_handler(dll, "CeGetRandomSeed", ce_get_random_seed);
    d.register_handler(dll, "QueryPerformanceCounter", query_performance_counter);
    d.register_handler(
        dll,
        "QueryPerformanceFrequency",
        query_performance_frequency,
    );
    // WinCE doesn't ship `winmm.dll` but exports `timeGetTime` from
    // `coredll.dll` itself. Pocket PC games that link against
    // `MMTimer.dll` expect `timeGetTime` to behave like
    // `GetTickCount`.
    d.register_handler(dll, "timeGetTime", time_get_time);
    // WinMineCE imports `timeGetTime` from a third-party redist
    // (`MMTimer.dll`) instead. Same semantics — a millisecond clock.
    d.register_handler("MMTimer.dll", "timeGetTime", time_get_time);
    d.register_handler("winmm.dll", "timeGetTime", time_get_time);

    // ---- Registry ----
    d.register_handler(dll, "RegOpenKeyExW", reg_open_key_ex_w);
    d.register_handler(dll, "RegCreateKeyExW", reg_create_key_ex_w);
    d.register_handler(dll, "RegQueryValueExW", reg_query_value_ex_w);
    d.register_constant(dll, "RegSetValueExW", 0, zero_returning);
    d.register_constant(dll, "RegCloseKey", 0, zero_returning);

    const REG_OWNER_KEY: u32 = 0xDEAD_9001;
    const REG_GAME_KEY: u32 = 0xDEAD_9002;
    const REG_SZ: u32 = 1;

    fn reg_open_key_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let root = ctx.arg_u32(0)?;
        let subkey_ptr = ctx.arg_u32(1)?;
        let out_key = ctx.arg_u32(4)?;
        let subkey = read_wstr(ctx, subkey_ptr, 260)
            .map(|value| String::from_utf16_lossy(&value))
            .unwrap_or_else(|_| "<invalid>".to_string());
        let key = if subkey.eq_ignore_ascii_case(r"\ControlPanel\Owner") {
            REG_OWNER_KEY
        } else if subkey.eq_ignore_ascii_case(r"\SOFTWARE\Greatelsoft.Com\MetalStrike") {
            REG_GAME_KEY
        } else {
            0
        };
        log::info!("RegOpenKeyExW(root=0x{root:08x}, subkey={subkey:?}, out=0x{out_key:08x}) -> 0x{key:08x}");
        if key == 0 {
            return Ok(DispatchOutcome::ReturnedR0(2));
        }
        if out_key != 0 {
            ctx.cpu.write_mem(out_key, &key.to_le_bytes())?;
        }
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    fn reg_create_key_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let root = ctx.arg_u32(0)?;
        let subkey_ptr = ctx.arg_u32(1)?;
        let out_key = ctx.arg_u32(7)?;
        let disposition = ctx.arg_u32(8)?;
        let subkey = read_wstr(ctx, subkey_ptr, 260)
            .map(|value| String::from_utf16_lossy(&value))
            .unwrap_or_else(|_| "<invalid>".to_string());
        let key = if subkey.eq_ignore_ascii_case(r"\ControlPanel\Owner") {
            REG_OWNER_KEY
        } else if subkey.eq_ignore_ascii_case(r"\SOFTWARE\Greatelsoft.Com\MetalStrike") {
            REG_GAME_KEY
        } else {
            0xDEAD_90FF
        };
        log::info!("RegCreateKeyExW(root=0x{root:08x}, subkey={subkey:?}, out=0x{out_key:08x}) -> 0x{key:08x}");
        if out_key != 0 {
            ctx.cpu.write_mem(out_key, &key.to_le_bytes())?;
        }
        if disposition != 0 {
            ctx.cpu.write_mem(disposition, &1u32.to_le_bytes())?;
        }
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    fn reg_query_value_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let key = ctx.arg_u32(0)?;
        let value_ptr = ctx.arg_u32(1)?;
        let value = read_wstr(ctx, value_ptr, 260)
            .map(|chars| String::from_utf16_lossy(&chars))
            .unwrap_or_else(|_| "<invalid>".to_string());
        let value_text = if key == REG_OWNER_KEY && value.eq_ignore_ascii_case("Owner") {
            Some("Argon")
        } else {
            None
        };
        let value_dword = if key == REG_GAME_KEY && value.eq_ignore_ascii_case("SN-Key1") {
            Some(1739u32)
        } else if key == REG_GAME_KEY && value.eq_ignore_ascii_case("SN-Key2") {
            Some(0u32)
        } else {
            None
        };
        let value_type = ctx.arg_u32(3)?;
        let data = ctx.arg_u32(4)?;
        let data_size = ctx.arg_u32(5)?;
        let capacity = if data_size != 0 {
            u32::from_le_bytes(ctx.cpu.read_mem(data_size, 4)?.try_into().unwrap())
        } else {
            0
        };
        log::info!("RegQueryValueExW(key=0x{key:08x}, value={value:?}, type_ptr=0x{value_type:08x}, data=0x{data:08x}, size_ptr=0x{data_size:08x}, capacity={capacity})");
        let bytes: Vec<u8>;
        let value_kind;
        if let Some(text) = value_text {
            bytes = text
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect();
            value_kind = REG_SZ;
        } else if let Some(number) = value_dword {
            bytes = number.to_le_bytes().to_vec();
            value_kind = 4;
        } else {
            return Ok(DispatchOutcome::ReturnedR0(2));
        }
        if value_type != 0 {
            ctx.cpu.write_mem(value_type, &value_kind.to_le_bytes())?;
        }
        if data_size != 0 {
            let capacity = capacity as usize;
            if data == 0 || capacity < bytes.len() {
                ctx.cpu
                    .write_mem(data_size, &(bytes.len() as u32).to_le_bytes())?;
                return Ok(DispatchOutcome::ReturnedR0(234));
            }
            ctx.cpu.write_mem(data, &bytes)?;
            ctx.cpu
                .write_mem(data_size, &(bytes.len() as u32).to_le_bytes())?;
        }
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    // ---- libm (soft-float, double-precision) ----    // ---- libm (soft-float, double-precision) ----
    d.register_handler(dll, "sin", m_sin);
    d.register_handler(dll, "cos", m_cos);
    d.register_handler(dll, "tan", m_tan);
    d.register_handler(dll, "asin", m_asin);
    d.register_handler(dll, "acos", m_acos);
    d.register_handler(dll, "atan", m_atan);
    d.register_handler(dll, "sinh", m_sinh);
    d.register_handler(dll, "cosh", m_cosh);
    d.register_handler(dll, "tanh", m_tanh);
    d.register_handler(dll, "exp", m_exp);
    d.register_handler(dll, "log", m_log);
    d.register_handler(dll, "log10", m_log10);
    d.register_handler(dll, "sqrt", m_sqrt);
    d.register_handler(dll, "floor", m_floor);
    d.register_handler(dll, "ceil", m_ceil);
    d.register_handler(dll, "fabs", m_fabs);
    d.register_handler(dll, "atan2", m_atan2);
    d.register_handler(dll, "pow", m_pow);
    d.register_handler(dll, "fmod", m_fmod);
    d.register_handler(dll, "_hypot", m_hypot);
    d.register_handler(dll, "hypot", m_hypot);
    d.register_handler(dll, "ldexp", m_ldexp);
    d.register_handler(dll, "frexp", m_frexp);
    d.register_handler(dll, "modf", m_modf);

    // ---- lstr* string helpers ----
    d.register_handler(dll, "lstrlenW", lstrlen_w);
    d.register_handler(dll, "lstrlenA", lstrlen_a);
    d.register_handler(dll, "lstrcpyW", lstrcpy_w);
    d.register_handler(dll, "lstrcpyA", lstrcpy_a);
    d.register_handler(dll, "lstrcatW", lstrcat_w);
    d.register_handler(dll, "lstrcatA", lstrcat_a);
    d.register_handler(dll, "lstrcmpW", lstrcmp_w);
    d.register_handler(dll, "lstrcmpA", lstrcmp_a);
    d.register_handler(dll, "lstrcmpiW", lstrcmpi_w);
    d.register_handler(dll, "lstrcmpiA", lstrcmpi_a);

    // ---- RECT helpers ----
    d.register_handler(dll, "SetRect", set_rect);
    d.register_handler(dll, "SetRectEmpty", set_rect_empty);
    d.register_handler(dll, "CopyRect", copy_rect);
    d.register_handler(dll, "InflateRect", inflate_rect);
    d.register_handler(dll, "OffsetRect", offset_rect);
    d.register_handler(dll, "PtInRect", pt_in_rect);
    d.register_handler(dll, "IsRectEmpty", is_rect_empty);

    // ---- Locale ----
    d.register_handler(dll, "GetUserDefaultLangID", get_user_default_lang_id);
    d.register_handler(dll, "GetUserDefaultLCID", get_user_default_lcid);
    d.register_handler(dll, "GetSystemDefaultLangID", get_system_default_lang_id);
    d.register_handler(dll, "GetThreadLocale", get_thread_locale);

    // ---- Codepage / dynamic loader ----
    d.register_handler(dll, "MultiByteToWideChar", multi_byte_to_wide_char);
    d.register_handler(dll, "WideCharToMultiByte", wide_char_to_multi_byte);
    d.register_handler(dll, "GetProcAddressW", get_proc_address_w);
    d.register_handler(dll, "RegisterWindowMessageW", register_window_message_w);
    d.register_handler(dll, "VirtualQuery", virtual_query);

    // ---- Misc Pocket-PC quirks games still try to call ----
    // `SipGetInfo(SIPINFO*)` reports the soft-input-panel state. We
    // claim "no SIP visible, full-screen rect" by zero-filling the
    // SIPINFO and returning TRUE. Games (Bejeweled, Zuma) treat the
    // function as advisory and fall back to a hard-coded screen
    // size when it fails, but spelling that out here keeps the
    // trace clean.
    d.register_handler(dll, "SipGetInfo", sip_get_info);
    d.register_constant(dll, "SipSetCurrentIM", 1, one_returning);
    d.register_constant(dll, "SipShowIM", 1, one_returning);
    d.register_constant(dll, "SipSetInfo", 1, one_returning);
    d.register_constant(dll, "SipStatus", 0, zero_returning);
    // `AllKeys(BOOL)` toggles whether the shell forwards every key
    // (incl. Power / Today) to the foreground app. PocketHLE is
    // single-app so the flag is a no-op; report success.
    d.register_constant(dll, "AllKeys", 1, one_returning);

    // Coredll exports four ordinals every modern Pocket PC binary
    // (Zuma, Bejeweled, Asphalt, Peggle, …) imports. Pocket PC 2003
    // SDK shipped no `coredll.def` for them, but the WM5 SDK's
    // `Armv4i\coredll.lib` does — and it agrees with the public
    // MSVC mangled / undecorated names below:
    //
    //   #1576  ??_L@YAXPAXIHP6AX0@Z1@Z   `vector constructor iterator'
    //   #1578  ??_M@YAXPAXIHP6AX0@Z@Z    `vector destructor iterator'
    //   #1875  __security_gen_cookie     /GS stack-cookie generator
    //   #1876  __report_gsfailure        /GS stack-cookie failure
    //
    // The ordinal map (`data/coredll-ordinals.json`) routes the
    // imports through the same friendly name registration the
    // dispatcher uses for everything else, so once the JSON has
    // them, registering by name here is enough.
    d.register_handler(dll, "??_L@YAXPAXIHP6AX0@Z1@Z", vector_ctor_iterator);
    d.register_handler(dll, "??_M@YAXPAXIHP6AX0@Z@Z", vector_dtor_iterator);
    d.register_handler(dll, "__security_gen_cookie", security_gen_cookie);
    d.register_handler(dll, "__report_gsfailure", report_gsfailure);

    // ---- Clipboard (no-op) ----
    d.register_handler(dll, "OpenClipboard", open_clipboard);
    d.register_handler(dll, "CloseClipboard", close_clipboard);
    d.register_handler(dll, "EmptyClipboard", empty_clipboard);
    d.register_handler(
        dll,
        "IsClipboardFormatAvailable",
        is_clipboard_format_available,
    );
    d.register_handler(dll, "GetClipboardData", get_clipboard_data);
    d.register_handler(dll, "SetClipboardData", set_clipboard_data);
}

// ---------- generic helpers ----------

pub(crate) fn zero_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

pub(crate) fn one_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

pub(crate) fn null_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn invalid_handle_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(INVALID_HANDLE_VALUE))
}

// ---------- C++ EH vector iterators (`??_L` / `??_M`) ----------
//
// MSVC emits these helpers — and only links them in by importing them
// from `coredll.dll` — for any code that constructs or destructs an
// array of objects with non-trivial ctors / dtors. The undecorated
// prototypes (taken straight from the WM5 SDK's `Armv4i\coredll.lib`):
//
// ```c
// // ??_L  ordinal 1576
// void __cdecl `vector constructor iterator'(
//     void *  pBegin,
//     UINT    cbElement,
//     int     nElements,
//     void   (__cdecl *pCtor)(void *),
//     void   (__cdecl *pCleanupCtor)(void *));
//
// // ??_M  ordinal 1578
// void __cdecl `vector destructor iterator'(
//     void *  pBegin,
//     UINT    cbElement,
//     int     nElements,
//     void   (__cdecl *pDtor)(void *));
// ```
//
// On real coredll the body is just a plain `for (i = 0; i < N; ++i)
// pCtor(pBegin + i * cbElement);` (and the symmetric reverse loop for
// the destructor variant). We can't run that loop directly from Rust
// because the per-element function pointer lives in *guest* code, so
// we drive the loop one element per `JumpTo` round-trip: the handler
// stashes `(p_begin, cb_element, n_elements, p_func, i, saved_lr)` in
// `KernelState::vector_iter_frames`, sets `R0 = element pointer`,
// `LR = ??_L thunk_va`, and trampolines into `pCtor`. When `pCtor`
// returns it `bx lr`s back to our thunk, the dispatcher fires us
// again, and we either advance `i` for the next element or — once
// every element has been processed — restore `LR` to the iterator's
// own caller and return.
fn vector_ctor_iterator(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    drive_vector_iter(ctx, /*is_dtor=*/ false)
}

fn vector_dtor_iterator(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    drive_vector_iter(ctx, /*is_dtor=*/ true)
}

fn drive_vector_iter(ctx: &mut CallCtx<'_>, is_dtor: bool) -> Result<DispatchOutcome, KernelError> {
    let thunk_va = ctx.thunk.thunk_va;
    let frame = match ctx.kernel.vector_iter_frames.get(&thunk_va).copied() {
        Some(mut f) => {
            // Re-entry: the previous element's ctor / dtor just `bx
            // lr`'d back to our thunk. Move on to the next one.
            f.i = f.i.saturating_add(1);
            f
        }
        None => {
            // First entry: capture the args. Order matches the MSVC
            // prototype above.
            let p_begin = ctx.arg_u32(0)?;
            let cb_element = ctx.arg_u32(1)?;
            let n_elements = ctx.arg_u32(2)? as i32;
            let p_func = ctx.arg_u32(3)?;
            // `pCleanupCtor` is the 5th argument and lives at
            // `[sp+0]` per AAPCS. Only `??_L` actually has it, but
            // reading past the end on `??_M` is harmless (the value
            // is unused) and saves a branch here.
            let p_cleanup = ctx.arg_u32(4).unwrap_or(0);
            let saved_lr = ctx.cpu.read_reg(ArmReg::Lr)?;
            log::trace!(
                "{} begin: pBegin=0x{:08x} cb={} N={} pFunc=0x{:08x} cleanup=0x{:08x} retLR=0x{:08x}",
                ctx.thunk.label(),
                p_begin,
                cb_element,
                n_elements,
                p_func,
                p_cleanup,
                saved_lr,
            );
            VectorIterFrame {
                p_begin,
                cb_element,
                n_elements,
                p_func,
                p_cleanup,
                is_dtor,
                i: 0,
                saved_lr,
            }
        }
    };

    // Termination conditions:
    //   * `n_elements <= 0` — empty array, nothing to do.
    //   * `i >= n_elements` — every element processed.
    //   * `p_func == 0` — caller passed a NULL ctor/dtor pointer; on
    //     real coredll this is a guest bug that segfaults the first
    //     call. We treat it as "no-op iteration" and return cleanly
    //     so the rest of the program isn't poisoned.
    if frame.n_elements <= 0 || frame.i >= frame.n_elements || frame.p_func == 0 {
        ctx.kernel.vector_iter_frames.remove(&thunk_va);
        ctx.cpu.write_reg(ArmReg::Lr, frame.saved_lr)?;
        // Real prototype is `void`-returning. Return 0 in R0 just so
        // the dispatcher has a defined value; callers ignore it.
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    // Compute element pointer for this iteration. `??_L` walks
    // forward, `??_M` walks backwards — the latter mirrors what
    // MSVC emits (RAII order: destruct in reverse construction
    // order).
    let elem_index: u32 = if frame.is_dtor {
        (frame.n_elements - 1 - frame.i) as u32
    } else {
        frame.i as u32
    };
    let elem_ptr = frame
        .p_begin
        .wrapping_add(elem_index.wrapping_mul(frame.cb_element));

    log::trace!(
        "{} step {}/{}: elem=0x{:08x} -> pFunc=0x{:08x}",
        ctx.thunk.label(),
        frame.i + 1,
        frame.n_elements,
        elem_ptr,
        frame.p_func,
    );

    ctx.cpu.write_reg(ArmReg::R0, elem_ptr)?;
    // Set LR so that pFunc's `bx lr` brings the CPU straight back
    // into our own thunk for the next step.
    ctx.cpu.write_reg(ArmReg::Lr, thunk_va)?;
    let target = frame.p_func;
    ctx.kernel.vector_iter_frames.insert(thunk_va, frame);
    Ok(DispatchOutcome::JumpTo(target))
}

// ---------- /GS stack-cookie helpers (`__security_gen_cookie` / `__report_gsfailure`) ----------
//
// `coredll.dll` exports the two halves of the MSVC `/GS` stack
// protector at ordinals 1875 / 1876. The compiler emits
//
// ```asm
//   prologue: ldr r0, =__security_cookie ; ldr r0, [r0]
//             eor r0, r0, sp
//             str r0, [sp, #N]            ; local cookie
//   epilogue: ldr r1, [sp, #N]
//             eor r1, r1, sp
//             ldr r0, =__security_cookie
//             ldr r0, [r0]
//             cmp r0, r1
//             bne __security_check_cookie_fail   ; tail-calls __report_gsfailure
// ```
//
// in every `/GS`-instrumented function. The runtime piece, in
// pseudo-C straight from the leaked WinCE 5.0 / 6.0 CRT source
// (`gs_support.c`):
//
// ```c
// DWORD __security_gen_cookie(void)  // ordinal 1875
// {
//     DWORD cookie;
//     SYSTEMTIME st;
//     LARGE_INTEGER pc;
//
//     GetSystemTime(&st);
//     QueryPerformanceCounter(&pc);
//
//     cookie  = ((DWORD)st.wMilliseconds << 16) | st.wMonth;
//     cookie ^= (DWORD)pc.LowPart;
//     cookie ^= GetTickCount();
//     cookie ^= (DWORD)GetCurrentProcessId();
//     cookie ^= (DWORD)GetCurrentThreadId();
//     // Force the cookie into the [1, 0xFFFFFFFE] range — `/GS`
//     // uses `0` and `0xFFFFFFFF` to mean "uninitialised".
//     if (cookie == 0)           cookie = 0xBB40E64Du;
//     if (cookie == 0xFFFFFFFFu) cookie ^= 0xBB40E64Du;
//     return cookie;
// }
//
// DECLSPEC_NORETURN
// void __report_gsfailure(void)      // ordinal 1876
// {
//     RaiseException(STATUS_STACK_BUFFER_OVERRUN, 0, 0, NULL);
//     // Unreachable — RaiseException tears the process down.
//     for (;;) ;
// }
// ```
//
// PocketHLE's HLE wrinkle: PE images built by the MSVC ARM/Thumb
// toolchain (this is the case for every Pocket PC retail title)
// generate `__security_check_cookie` as a *two*-step test, not a
// straight equality:
//
//     ldr     ip, =__security_cookie
//     ldr     ip, [ip]                 ; ip = current global
//     cmp     r0, ip                   ; r0 = saved cookie
//     lsrseq  ip, r0, #16              ; if equal, recompute flags from r0>>16
//     bxeq    lr                       ; return iff equal AND r0>>16 == 0
//     ; …falls through to bl __report_gsfailure…
//
// In other words MSVC's ARM /GS lib enforces the invariant that
// `__security_cookie`'s top 16 bits are *always zero*. Anything with
// the high half set is treated as a smashed return address and
// triggers `__report_gsfailure`. (Confirmed by disassembling
// `__security_check_cookie` out of every ARM PocketPC binary we have
// — Zuma's lives at VA 0x00112098 in `ZUMAPP~1.002`.) The MSVC ARM
// CRT picks `0x0000_B064` as the linker-baked placeholder for
// `__security_cookie` precisely because it satisfies the >>16
// constraint, and `__security_init_cookie` likewise generates a
// 16-bit-only cookie on this platform.
//
// Real WinCE coredll's `__security_gen_cookie` synthesises a fresh
// cookie from per-thread / per-process state, then masks it so the
// upper half stays zero. Under HLE we can't intercept the binary's
// own instrumentation, so we *must* hand back a value that satisfies
// the >>16 constraint or every instrumented epilogue in the process
// will trip the cookie check, regardless of whether the global
// matches the saved copy.
//
// We return the canonical MSVC-ARM placeholder `0x0000_B064`. That
// value is the literal the WinCE / VS2008 ARM linkers stamp into
// `__security_cookie` at static-init time, so when
// `__security_init_cookie` writes our return value back into the
// global it's byte-identical to what every prologue snapshotted at
// the moment of the cookie load. Cached in
// `KernelState::security_cookie` for symmetry with the real
// implementation (which never recomputes on subsequent calls) and so
// future state inspectors hand back the same constant for the life
// of the process.
const MSVC_ARM_DEFAULT_SECURITY_COOKIE: u32 = 0x0000_B064;

fn security_gen_cookie(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.security_cookie == 0 {
        ctx.kernel.security_cookie = MSVC_ARM_DEFAULT_SECURITY_COOKIE;
        log::debug!(
            "__security_gen_cookie: returning MSVC-ARM default 0x{:08x} for process \
             (matches the linker-baked __security_cookie placeholder, so init is a no-op)",
            ctx.kernel.security_cookie
        );
    }
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.security_cookie))
}

fn report_gsfailure(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // `__report_gsfailure` is `__declspec(noreturn)` in the SDK
    // header. On real WinCE it `RaiseException(STATUS_STACK_BUFFER_OVERRUN)`s,
    // which the kernel turns into process termination. The closest
    // equivalent we have under HLE is a graceful `Halt` — and crucially
    // we must NOT fall through to `ReturnedR0` because that would let
    // the corrupted-stack guest code keep running.
    let lr = ctx.cpu.read_reg(ArmReg::Lr).unwrap_or(0);
    let sp = ctx.cpu.read_reg(ArmReg::Sp).unwrap_or(0);
    let r0 = ctx.cpu.read_reg(ArmReg::R0).unwrap_or(0);
    let mut nearby = String::new();
    for off in [-0x10i32, -0xc, -0x8, -0x4, 0, 0x4, 0x8, 0xc, 0x10] {
        let a = (sp as i64 + off as i64) as u32;
        if let Ok(b) = ctx.cpu.read_mem(a, 4) {
            let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            nearby.push_str(&format!(" [sp{:+#x}]=0x{:08x}", off, v));
        }
    }
    log::error!(
        "guest invoked coredll!__report_gsfailure (#1876) from LR=0x{:08x} SP=0x{:08x} R0=0x{:08x}: \
         /GS stack-cookie mismatch detected, halting process. nearby:{}",
        lr,
        sp,
        r0,
        nearby,
    );
    Ok(DispatchOutcome::Halt)
}

// ---------- process / time ----------

fn get_tick_count(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static START: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if START.load(Ordering::Relaxed) == 0 {
        START.store(now, Ordering::Relaxed);
    }
    let delta = now - START.load(Ordering::Relaxed);
    Ok(DispatchOutcome::ReturnedR0(delta as u32))
}

fn read_guest_regs(cpu: &mut dyn pocket_cpu::Cpu) -> Result<[u32; 17], KernelError> {
    let regs = [
        ArmReg::R0,
        ArmReg::R1,
        ArmReg::R2,
        ArmReg::R3,
        ArmReg::R4,
        ArmReg::R5,
        ArmReg::R6,
        ArmReg::R7,
        ArmReg::R8,
        ArmReg::R9,
        ArmReg::R10,
        ArmReg::R11,
        ArmReg::R12,
        ArmReg::Sp,
        ArmReg::Lr,
        ArmReg::Pc,
        ArmReg::Cpsr,
    ];
    let mut values = [0u32; 17];
    for (index, reg) in regs.into_iter().enumerate() {
        values[index] = cpu.read_reg(reg)?;
    }
    Ok(values)
}
fn write_guest_regs(cpu: &mut dyn pocket_cpu::Cpu, values: &[u32; 17]) -> Result<(), KernelError> {
    let regs = [
        ArmReg::R0,
        ArmReg::R1,
        ArmReg::R2,
        ArmReg::R3,
        ArmReg::R4,
        ArmReg::R5,
        ArmReg::R6,
        ArmReg::R7,
        ArmReg::R8,
        ArmReg::R9,
        ArmReg::R10,
        ArmReg::R11,
        ArmReg::R12,
        ArmReg::Sp,
        ArmReg::Lr,
        ArmReg::Pc,
        ArmReg::Cpsr,
    ];
    for (index, reg) in regs.into_iter().enumerate() {
        cpu.write_reg(reg, values[index])?;
    }
    Ok(())
}

fn sleep(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _ms = ctx.arg_u32(0)?;
    if ctx.kernel.current_thread > 0 {
        let thread_index = ctx.kernel.current_thread - 1;
        let mut regs = read_guest_regs(ctx.cpu)?;
        let resume_pc = ctx.cpu.read_reg(ArmReg::Lr)?;
        regs[15] = resume_pc;
        let main_regs = ctx
            .kernel
            .threads
            .get(thread_index)
            .map(|thread| thread.saved_regs);
        if let Some(thread) = ctx.kernel.threads.get_mut(thread_index) {
            thread.worker_regs = regs;
            thread.worker_saved = true;
        }
        if let Some(main_regs) = main_regs {
            write_guest_regs(ctx.cpu, &main_regs)?;
            ctx.kernel.current_thread = 0;
            return Ok(DispatchOutcome::JumpTo(main_regs[15]));
        }
        ctx.kernel.current_thread = 0;
    } else if let Some((thread_index, worker_regs)) = ctx
        .kernel
        .threads
        .iter()
        .enumerate()
        .find(|(_, thread)| thread.worker_saved && !thread.finished)
        .map(|(index, thread)| (index, thread.worker_regs))
    {
        write_guest_regs(ctx.cpu, &worker_regs)?;
        ctx.kernel.current_thread = thread_index + 1;
        return Ok(DispatchOutcome::JumpTo(worker_regs[15] & !1));
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn resume_thread(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let thread_index = ctx
        .kernel
        .threads
        .iter()
        .position(|thread| thread.handle == handle && !thread.finished);
    if let Some(index) = thread_index {
        ctx.kernel.threads[index].started = true;
        log::debug!("ResumeThread(0x{handle:08x}) -> 0");
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if handle == 0xDEAD_E102 {
        log::debug!("ResumeThread(simulated child 0x{handle:08x}) -> 0");
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    log::debug!("ResumeThread(0x{handle:08x}) -> -1");
    Ok(DispatchOutcome::ReturnedR0(0xffff_ffff))
}

fn exit_process(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    log::info!("ExitProcess called by guest");
    Ok(DispatchOutcome::Halt)
}

fn create_process_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let application = ctx.arg_u32(0)?;
    let command_line = ctx.arg_u32(1)?;
    let process_info = ctx.arg_u32(9)?;
    let name = if application != 0 {
        read_wstr(ctx, application, 260).ok()
    } else if command_line != 0 {
        read_wstr(ctx, command_line, 260).ok()
    } else {
        None
    };
    log::info!(
        "CreateProcessW({}) simulated",
        name.as_ref()
            .map(|v| String::from_utf16_lossy(v))
            .unwrap_or_else(|| "<null>".to_string())
    );
    if process_info != 0 {
        ctx.cpu
            .write_mem(process_info, &0xDEAD_E101u32.to_le_bytes())?;
        ctx.cpu
            .write_mem(process_info + 4, &0xDEAD_E102u32.to_le_bytes())?;
        ctx.cpu.write_mem(process_info + 8, &1u32.to_le_bytes())?;
        ctx.cpu.write_mem(process_info + 12, &1u32.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn global_memory_status(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let status = ctx.arg_u32(0)?;
    if status == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut data = [0u8; 32];
    data[0..4].copy_from_slice(&32u32.to_le_bytes());
    data[4..8].copy_from_slice(&20u32.to_le_bytes());
    data[8..12].copy_from_slice(&(32u32 * 1024 * 1024).to_le_bytes());
    data[12..16].copy_from_slice(&(24u32 * 1024 * 1024).to_le_bytes());
    data[16..20].copy_from_slice(&(32u32 * 1024 * 1024).to_le_bytes());
    data[20..24].copy_from_slice(&(24u32 * 1024 * 1024).to_le_bytes());
    data[24..28].copy_from_slice(&(64u32 * 1024 * 1024).to_le_bytes());
    data[28..32].copy_from_slice(&(48u32 * 1024 * 1024).to_le_bytes());
    ctx.cpu.write_mem(status, &data)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_module_handle_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name_p = ctx.arg_u32(0)?;
    if name_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(FAKE_MODULE_HANDLE));
    }
    let name = String::from_utf16_lossy(&read_wstr(ctx, name_p, 260).unwrap_or_default())
        .to_ascii_lowercase();
    let handle = if name == "gx.dll" || name == "gx" {
        0x1000_0001
    } else if name == "commctrl.dll" || name == "commctrl" {
        0x1000_0002
    } else {
        FAKE_MODULE_HANDLE
    };
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn load_library_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let path_p = ctx.arg_u32(0)?;
    let path = read_wstr(ctx, path_p, 260).unwrap_or_default();
    let name = String::from_utf16_lossy(&path).to_ascii_lowercase();
    if name.ends_with("gx.dll") || name == "gx" {
        let handle = if ctx.kernel.dynamic_exports.contains_key(&0x1000_0001) {
            0x1000_0001
        } else {
            0
        };
        log::debug!("LoadLibraryW({name:?}) -> 0x{handle:08x}");
        return Ok(DispatchOutcome::ReturnedR0(handle));
    }
    if name.ends_with("commctrl.dll") || name == "commctrl" {
        let handle = if ctx.kernel.dynamic_exports.contains_key(&0x1000_0002) {
            0x1000_0002
        } else {
            0
        };
        log::debug!("LoadLibraryW({name:?}) -> 0x{handle:08x}");
        return Ok(DispatchOutcome::ReturnedR0(handle));
    }
    log::debug!("LoadLibraryW({name:?}) -> NULL");
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// Synthetic guest path of the running executable. This matches the
/// usual Pocket PC install location and contains a backslash so that
/// `wcsrchr(path, L'\\')` returns a non-null pointer.
const FAKE_EXE_PATH: &str = "\\Program Files\\Game\\Game.exe";

fn write_wide_str(
    cpu: &mut dyn pocket_cpu::Cpu,
    dst: u32,
    cap: u32,
    s: &str,
) -> Result<u32, KernelError> {
    if dst == 0 || cap == 0 {
        return Ok(0);
    }
    let mut out = Vec::with_capacity(s.len() * 2 + 2);
    let copy_n = (cap as usize).saturating_sub(1);
    for (i, ch) in s.encode_utf16().enumerate() {
        if i >= copy_n {
            break;
        }
        out.extend_from_slice(&ch.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    cpu.write_mem(dst, &out)?;
    Ok((out.len() as u32 / 2).saturating_sub(1))
}

fn get_module_file_name_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // GetModuleFileNameW(HINSTANCE hModule, LPWSTR lpFilename, DWORD nSize) -> DWORD
    let _h = ctx.arg_u32(0)?;
    let dst = ctx.arg_u32(1)?;
    let cap = ctx.arg_u32(2)?;
    let written = write_wide_str(ctx.cpu, dst, cap, FAKE_EXE_PATH)?;
    Ok(DispatchOutcome::ReturnedR0(written))
}

fn get_command_line_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // We allocate a static guest-readable string the first time we're
    // called and return its VA on every subsequent call.
    use std::sync::atomic::{AtomicU32, Ordering};
    static CACHED: AtomicU32 = AtomicU32::new(0);
    let cached = CACHED.load(Ordering::Relaxed);
    if cached != 0 {
        return Ok(DispatchOutcome::ReturnedR0(cached));
    }
    let bytes_needed = (FAKE_EXE_PATH.encode_utf16().count() as u32 + 1) * 2;
    let va = match ctx.kernel.heap.alloc(bytes_needed) {
        Some(p) => p,
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    write_wide_str(ctx.cpu, va, bytes_needed / 2, FAKE_EXE_PATH)?;
    CACHED.store(va, Ordering::Relaxed);
    Ok(DispatchOutcome::ReturnedR0(va))
}

// ---------- CRT prologue helpers ----------

/// `void __chkstk(void)` on Windows ARM is the stack-probe routine
/// inserted by the MS C compiler for any function whose locals exceed
/// one page. The real implementation walks down the stack a page at a
/// time, touching each page so the OS can grow the stack guard.
///
/// Under HLE we map the entire stack up front, so there is nothing to
/// probe — we just return immediately.
fn chkstk(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `int _setjmp(jmp_buf env)` — saves callee-saved registers + SP +
/// LR into the buffer at `r0` and returns 0. On a subsequent
/// [`longjmp`] the dispatcher restores the registers and resumes at
/// the saved LR.
///
/// jmp_buf layout used by the MS ARM compiler (32 bytes is more than
/// enough for the registers we care about):
///   `[r4, r5, r6, r7, r8, r9, r10, r11, sp, lr]`
fn setjmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let buf = ctx.arg_u32(0)?;
    let regs_to_save = [
        ArmReg::R4,
        ArmReg::R5,
        ArmReg::R6,
        ArmReg::R7,
        ArmReg::R8,
        ArmReg::R9,
        ArmReg::R10,
        ArmReg::R11,
        ArmReg::Sp,
        ArmReg::Lr,
    ];
    let mut blob = Vec::with_capacity(regs_to_save.len() * 4);
    for r in regs_to_save {
        let v = ctx.cpu.read_reg(r)?;
        blob.extend_from_slice(&v.to_le_bytes());
    }
    ctx.cpu.write_mem(buf, &blob)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `void longjmp(jmp_buf env, int value)` — restores the buffer and
/// returns from the matching `_setjmp` with `value`.
fn longjmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let buf = ctx.arg_u32(0)?;
    let val = ctx.arg_u32(1)?;
    let regs_to_restore = [
        ArmReg::R4,
        ArmReg::R5,
        ArmReg::R6,
        ArmReg::R7,
        ArmReg::R8,
        ArmReg::R9,
        ArmReg::R10,
        ArmReg::R11,
        ArmReg::Sp,
        ArmReg::Lr,
    ];
    // A NULL or otherwise unmapped jmp_buf typically means the C++
    // SEH unwinder is asking for cleanup without a matching setjmp.
    // Treat it as a no-op (`R0=value`, resume from LR) and let the
    // caller continue. If that path turns out to be a fatal abort
    // signal in some game we can revisit.
    let blob = match ctx.cpu.read_mem(buf, regs_to_restore.len() as u32 * 4) {
        Ok(b) => b,
        Err(_) => {
            log::debug!(
                "longjmp(buf=0x{buf:08x}, val={val}) with unmapped jmp_buf; treating as no-op"
            );
            let ret = if val == 0 { 1 } else { val };
            return Ok(DispatchOutcome::ReturnedR0(ret));
        }
    };
    for (i, r) in regs_to_restore.iter().enumerate() {
        let off = i * 4;
        let v = u32::from_le_bytes([blob[off], blob[off + 1], blob[off + 2], blob[off + 3]]);
        ctx.cpu.write_reg(*r, v)?;
    }
    // longjmp must return `value` (or 1 if value == 0) from setjmp's
    // call site. The dispatcher will write our return into r0 and
    // resume at LR — and the LR we just restored is exactly the
    // return address of the original setjmp.
    let ret = if val == 0 { 1 } else { val };
    Ok(DispatchOutcome::ReturnedR0(ret))
}

/// `_except_handler3` is the per-frame handler the MS C compiler
/// installs for `__try`/`__except` blocks. With no SEH machinery in
/// HLE we simply tell the runtime that we did not handle the
/// exception — `ExceptionContinueSearch == 1`.
fn except_handler3(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

// ---------- ARMv4 soft-float helpers ----------
//
// AAPCS calling convention without VFP:
//   - single-precision floats are bit-cast to u32 and passed/returned in
//     integer registers (r0 for first arg, r1 for second, ...).
//   - double-precision floats are bit-cast to u64 and passed in
//     consecutive register pairs r0:r1 (low:high) and r2:r3.
//   - 64-bit returns go in r0:r1.
//
// The actual symbol names come from the EVC4 / Microsoft Visual C
// runtime for ARM Pocket PC. `s` suffix = single-precision, `d` = double.

fn read_f32(ctx: &mut CallCtx<'_>, idx: u8) -> Result<f32, KernelError> {
    Ok(f32::from_bits(ctx.arg_u32(idx)?))
}

fn read_f64(ctx: &mut CallCtx<'_>, idx_lo: u8) -> Result<f64, KernelError> {
    let lo = ctx.arg_u32(idx_lo)? as u64;
    let hi = ctx.arg_u32(idx_lo + 1)? as u64;
    Ok(f64::from_bits((hi << 32) | lo))
}

fn ret_f32(v: f32) -> DispatchOutcome {
    DispatchOutcome::ReturnedR0(v.to_bits())
}

fn ret_f64(v: f64) -> DispatchOutcome {
    let bits = v.to_bits();
    DispatchOutcome::ReturnedR0R1(bits as u32, (bits >> 32) as u32)
}

fn soft_adds(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(read_f32(ctx, 0)? + read_f32(ctx, 1)?))
}
fn soft_subs(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(read_f32(ctx, 0)? - read_f32(ctx, 1)?))
}
fn soft_muls(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(read_f32(ctx, 0)? * read_f32(ctx, 1)?))
}
fn soft_divs(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(read_f32(ctx, 0)? / read_f32(ctx, 1)?))
}
fn soft_negs(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(-read_f32(ctx, 0)?))
}
fn soft_cmps(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let a = read_f32(ctx, 0)?;
    let b = read_f32(ctx, 1)?;
    let r: i32 = if a < b {
        -1
    } else if a > b {
        1
    } else {
        0
    };
    Ok(DispatchOutcome::ReturnedR0(r as u32))
}
fn soft_eqs(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f32(ctx, 0)? == read_f32(ctx, 1)?) as u32,
    ))
}
fn soft_nes(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f32(ctx, 0)? != read_f32(ctx, 1)?) as u32,
    ))
}
fn soft_lts(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f32(ctx, 0)? < read_f32(ctx, 1)?) as u32,
    ))
}
fn soft_les(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f32(ctx, 0)? <= read_f32(ctx, 1)?) as u32,
    ))
}
fn soft_gts(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f32(ctx, 0)? > read_f32(ctx, 1)?) as u32,
    ))
}
fn soft_ges(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f32(ctx, 0)? >= read_f32(ctx, 1)?) as u32,
    ))
}
fn soft_itos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(ctx.arg_u32(0)? as i32 as f32))
}
fn soft_utos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(ctx.arg_u32(0)? as f32))
}
fn soft_stoi(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(read_f32(ctx, 0)? as i32 as u32))
}
fn soft_stou(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let v = read_f32(ctx, 0)?;
    let r = if v < 0.0 || !v.is_finite() {
        0
    } else {
        v as u32
    };
    Ok(DispatchOutcome::ReturnedR0(r))
}
fn soft_stod(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(read_f32(ctx, 0)? as f64))
}
fn soft_addd(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(read_f64(ctx, 0)? + read_f64(ctx, 2)?))
}
fn soft_subd(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(read_f64(ctx, 0)? - read_f64(ctx, 2)?))
}
fn soft_muld(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(read_f64(ctx, 0)? * read_f64(ctx, 2)?))
}
fn soft_divd(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(read_f64(ctx, 0)? / read_f64(ctx, 2)?))
}
fn soft_negd(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(-read_f64(ctx, 0)?))
}
fn soft_cmpd(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let a = read_f64(ctx, 0)?;
    let b = read_f64(ctx, 2)?;
    let r: i32 = if a < b {
        -1
    } else if a > b {
        1
    } else {
        0
    };
    Ok(DispatchOutcome::ReturnedR0(r as u32))
}
fn soft_eqd(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f64(ctx, 0)? == read_f64(ctx, 2)?) as u32,
    ))
}
fn soft_ned(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f64(ctx, 0)? != read_f64(ctx, 2)?) as u32,
    ))
}
fn soft_ltd(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f64(ctx, 0)? < read_f64(ctx, 2)?) as u32,
    ))
}
fn soft_led(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f64(ctx, 0)? <= read_f64(ctx, 2)?) as u32,
    ))
}
fn soft_gtd(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f64(ctx, 0)? > read_f64(ctx, 2)?) as u32,
    ))
}
fn soft_ged(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        (read_f64(ctx, 0)? >= read_f64(ctx, 2)?) as u32,
    ))
}
fn soft_itod(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(ctx.arg_u32(0)? as i32 as f64))
}
fn soft_utod(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(ctx.arg_u32(0)? as f64))
}
fn soft_dtoi(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(read_f64(ctx, 0)? as i32 as u32))
}
fn soft_dtou(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let v = read_f64(ctx, 0)?;
    let r = if v < 0.0 || !v.is_finite() {
        0
    } else {
        v as u32
    };
    Ok(DispatchOutcome::ReturnedR0(r))
}
fn soft_dtos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(read_f64(ctx, 0)? as f32))
}

// ---------- mem / string CRT ----------

fn memset(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let val = ctx.arg_u32(1)? as u8;
    let len = ctx.arg_u32(2)? as usize;
    // Reuse the kernel-wide scratch buffer instead of allocating a
    // fresh `vec![val; len]` per call. Resize-with grows in-place
    // when we already have enough capacity from a previous call.
    let scratch = &mut ctx.kernel.mem_op_scratch;
    if scratch.len() < len {
        scratch.resize(len, val);
    } else {
        scratch[..len].fill(val);
    }
    ctx.cpu.write_mem(dst, &scratch[..len])?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn memcpy(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)? as usize;
    // The dominant Derby case is a per-scanline 480-byte copy
    // (240 px × 2 B) called ~25k times per frame. Going through
    // `read_mem` allocated a fresh 480-byte `Vec` per call, which
    // showed up as the top per-frame cost in `perf`. Funnel the
    // copy through a reusable scratch instead.
    let scratch = &mut ctx.kernel.mem_op_scratch;
    if scratch.len() < len {
        scratch.resize(len, 0);
    }
    ctx.cpu.read_mem_into(src, &mut scratch[..len])?;
    ctx.cpu.write_mem(dst, &scratch[..len])?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn memcmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let a = ctx.arg_u32(0)?;
    let b = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)? as usize;
    // Pull both sides through the kernel scratch buffers so we
    // skip the per-call `Vec` alloc on each side.
    let (lhs, rhs) = (
        &mut ctx.kernel.mem_op_scratch,
        &mut ctx.kernel.mem_op_scratch_b,
    );
    if lhs.len() < len {
        lhs.resize(len, 0);
    }
    if rhs.len() < len {
        rhs.resize(len, 0);
    }
    ctx.cpu.read_mem_into(a, &mut lhs[..len])?;
    ctx.cpu.read_mem_into(b, &mut rhs[..len])?;
    let r = match lhs[..len].cmp(&rhs[..len]) {
        std::cmp::Ordering::Less => -1i32,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(DispatchOutcome::ReturnedR0(r as u32))
}

/// How far ahead to read when scanning for a NUL terminator. The
/// previous implementation issued one `read_mem` (and thus one
/// `Vec<u8>` heap allocation + one Unicorn FFI call) per scanned
/// byte. Profiling Derby on a single frame showed millions of those
/// 1-byte reads dominating CPU time. Reading a chunk at a time and
/// scanning it in-process turns the loop into one syscall per ~64
/// bytes with zero per-byte allocation.
const STR_CHUNK: usize = 64;
const WSTR_CHUNK: usize = 64; // 64 wide chars → 128 bytes per syscall

fn read_cstr(ctx: &mut CallCtx<'_>, p: u32, max: u32) -> Result<Vec<u8>, KernelError> {
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; STR_CHUNK];
    let mut off: u32 = 0;
    let max = max as u64;
    while (off as u64) < max {
        let remaining = max - off as u64;
        let want = remaining.min(STR_CHUNK as u64) as usize;
        // Try the bulk read first. If it fails (most likely because
        // we'd cross into an unmapped page), fall back to the
        // byte-at-a-time path so we still find the terminator
        // somewhere inside the mapped tail.
        let chunk_ok = ctx.cpu.read_mem_into(p + off, &mut buf[..want]).is_ok();
        if chunk_ok {
            for (i, &b) in buf[..want].iter().enumerate() {
                if b == 0 {
                    return Ok(out);
                }
                out.push(b);
                if (off as u64) + i as u64 + 1 >= max {
                    return Ok(out);
                }
            }
            off += want as u32;
            continue;
        }
        // Slow path: walk byte-by-byte until we hit either the
        // terminator, the cap, or another bad-memory error.
        for i in 0..want as u32 {
            let b = match ctx.cpu.read_u8(p + off + i) {
                Ok(b) => b,
                Err(_) => return Ok(out),
            };
            if b == 0 {
                return Ok(out);
            }
            out.push(b);
        }
        off += want as u32;
    }
    Ok(out)
}

fn read_wstr(ctx: &mut CallCtx<'_>, p: u32, max: u32) -> Result<Vec<u16>, KernelError> {
    let mut out: Vec<u16> = Vec::new();
    let mut buf = [0u8; WSTR_CHUNK * 2];
    let mut off: u32 = 0;
    let max = max as u64;
    while (off as u64) < max {
        let remaining = max - off as u64;
        let want_chars = remaining.min(WSTR_CHUNK as u64) as usize;
        let want_bytes = want_chars * 2;
        let chunk_ok = ctx
            .cpu
            .read_mem_into(p + off * 2, &mut buf[..want_bytes])
            .is_ok();
        if chunk_ok {
            for i in 0..want_chars {
                let c = u16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
                if c == 0 {
                    return Ok(out);
                }
                out.push(c);
                if (off as u64) + i as u64 + 1 >= max {
                    return Ok(out);
                }
            }
            off += want_chars as u32;
            continue;
        }
        for i in 0..want_chars as u32 {
            let c = match ctx.cpu.read_u16_le(p + (off + i) * 2) {
                Ok(c) => c,
                Err(_) => return Ok(out),
            };
            if c == 0 {
                return Ok(out);
            }
            out.push(c);
        }
        off += want_chars as u32;
    }
    Ok(out)
}

fn strlen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let s = ctx.arg_u32(0)?;
    let len = read_cstr(ctx, s, 0x10000)?.len() as u32;
    Ok(DispatchOutcome::ReturnedR0(len))
}

fn wcslen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let s = ctx.arg_u32(0)?;
    let chars = read_wstr(ctx, s, 0x10000)?.len() as u32;
    Ok(DispatchOutcome::ReturnedR0(chars))
}

fn strcpy(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let mut s = read_cstr(ctx, src, 0x10000)?;
    s.push(0);
    ctx.cpu.write_mem(dst, &s)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn strncpy(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let s = read_cstr(ctx, src, n)?;
    let mut buf = s;
    buf.resize(n as usize, 0);
    ctx.cpu.write_mem(dst, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn strcat(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let dst_len = read_cstr(ctx, dst, 0x10000)?.len() as u32;
    let mut s = read_cstr(ctx, src, 0x10000)?;
    s.push(0);
    ctx.cpu.write_mem(dst + dst_len, &s)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn strncat(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let dst_len = read_cstr(ctx, dst, 0x10000)?.len() as u32;
    let mut s = read_cstr(ctx, src, n)?;
    s.push(0);
    ctx.cpu.write_mem(dst + dst_len, &s)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn strcmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pa = ctx.arg_u32(0)?;
    let pb = ctx.arg_u32(1)?;
    let a = read_cstr(ctx, pa, 0x10000)?;
    let b = read_cstr(ctx, pb, 0x10000)?;
    Ok(DispatchOutcome::ReturnedR0(cmp_to_int(a.cmp(&b)) as u32))
}

fn strncmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pa = ctx.arg_u32(0)?;
    let pb = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let a = read_cstr(ctx, pa, n)?;
    let b = read_cstr(ctx, pb, n)?;
    Ok(DispatchOutcome::ReturnedR0(cmp_to_int(a.cmp(&b)) as u32))
}

fn strchr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let s = ctx.arg_u32(0)?;
    let c = ctx.arg_u32(1)? as u8;
    let bytes = read_cstr(ctx, s, 0x10000)?;
    for (i, b) in bytes.iter().enumerate() {
        if *b == c {
            return Ok(DispatchOutcome::ReturnedR0(s + i as u32));
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn strrchr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let s = ctx.arg_u32(0)?;
    let c = ctx.arg_u32(1)? as u8;
    let bytes = read_cstr(ctx, s, 0x10000)?;
    let mut found = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b == c {
            found = Some(i);
        }
    }
    Ok(DispatchOutcome::ReturnedR0(
        found.map(|i| s + i as u32).unwrap_or(0),
    ))
}

fn strstr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let n = ctx.arg_u32(1)?;
    let hay = read_cstr(ctx, h, 0x10000)?;
    let needle = read_cstr(ctx, n, 0x10000)?;
    let pos = hay
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(h + pos as u32))
}

fn strdup(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let src = ctx.arg_u32(0)?;
    let bytes = read_cstr(ctx, src, 0x10000)?;
    let size = bytes.len().saturating_add(1) as u32;
    let Some(dst) = ctx.kernel.heap.alloc(size) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    ctx.cpu.write_mem(dst, &bytes)?;
    ctx.cpu.write_mem(dst + bytes.len() as u32, &[0])?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn wcsdup(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let src = ctx.arg_u32(0)?;
    let chars = read_wstr(ctx, src, 0x10000)?;
    let size = (chars.len() + 1).saturating_mul(2) as u32;
    let Some(dst) = ctx.kernel.heap.alloc(size) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let mut bytes = wide_to_bytes(&chars);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    ctx.cpu.write_mem(dst, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn wcscpy(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let mut s = read_wstr(ctx, src, 0x10000)?;
    s.push(0);
    let bytes = wide_to_bytes(&s);
    ctx.cpu.write_mem(dst, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn wcsncpy(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let s = read_wstr(ctx, src, n)?;
    let mut buf = s;
    buf.resize(n as usize, 0);
    let bytes = wide_to_bytes(&buf);
    ctx.cpu.write_mem(dst, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn wcscat(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let dst_len = read_wstr(ctx, dst, 0x10000)?.len() as u32;
    let mut s = read_wstr(ctx, src, 0x10000)?;
    s.push(0);
    let bytes = wide_to_bytes(&s);
    ctx.cpu.write_mem(dst + dst_len * 2, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn wcsncat(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let dst_len = read_wstr(ctx, dst, 0x10000)?.len() as u32;
    let mut s = read_wstr(ctx, src, n)?;
    s.push(0);
    let bytes = wide_to_bytes(&s);
    ctx.cpu.write_mem(dst + dst_len * 2, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn wcscmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pa = ctx.arg_u32(0)?;
    let pb = ctx.arg_u32(1)?;
    let a = read_wstr(ctx, pa, 0x10000)?;
    let b = read_wstr(ctx, pb, 0x10000)?;
    Ok(DispatchOutcome::ReturnedR0(cmp_to_int(a.cmp(&b)) as u32))
}

fn wcsncmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pa = ctx.arg_u32(0)?;
    let pb = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let a = read_wstr(ctx, pa, n)?;
    let b = read_wstr(ctx, pb, n)?;
    Ok(DispatchOutcome::ReturnedR0(cmp_to_int(a.cmp(&b)) as u32))
}

fn wcsnicmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pa = ctx.arg_u32(0)?;
    let pb = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let a: Vec<u16> = read_wstr(ctx, pa, n)?.into_iter().map(to_lower_w).collect();
    let b: Vec<u16> = read_wstr(ctx, pb, n)?.into_iter().map(to_lower_w).collect();
    Ok(DispatchOutcome::ReturnedR0(cmp_to_int(a.cmp(&b)) as u32))
}

fn wcsicmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pa = ctx.arg_u32(0)?;
    let pb = ctx.arg_u32(1)?;
    let a: Vec<u16> = read_wstr(ctx, pa, 0x10000)?
        .into_iter()
        .map(to_lower_w)
        .collect();
    let b: Vec<u16> = read_wstr(ctx, pb, 0x10000)?
        .into_iter()
        .map(to_lower_w)
        .collect();
    Ok(DispatchOutcome::ReturnedR0(cmp_to_int(a.cmp(&b)) as u32))
}

fn wcschr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let s = ctx.arg_u32(0)?;
    let c = ctx.arg_u32(1)? as u16;
    let chars = read_wstr(ctx, s, 0x10000)?;
    for (i, w) in chars.iter().enumerate() {
        if *w == c {
            return Ok(DispatchOutcome::ReturnedR0(s + i as u32 * 2));
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn wcsrchr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let s = ctx.arg_u32(0)?;
    let c = ctx.arg_u32(1)? as u16;
    let chars = read_wstr(ctx, s, 0x10000)?;
    let mut found = None;
    for (i, w) in chars.iter().enumerate() {
        if *w == c {
            found = Some(i);
        }
    }
    Ok(DispatchOutcome::ReturnedR0(
        found.map(|i| s + i as u32 * 2).unwrap_or(0),
    ))
}

fn wcsstr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let n = ctx.arg_u32(1)?;
    let hay = read_wstr(ctx, h, 0x10000)?;
    let needle = read_wstr(ctx, n, 0x10000)?;
    if needle.is_empty() {
        return Ok(DispatchOutcome::ReturnedR0(h));
    }
    if let Some(pos) = hay.windows(needle.len()).position(|w| w == needle) {
        Ok(DispatchOutcome::ReturnedR0(h + pos as u32 * 2))
    } else {
        Ok(DispatchOutcome::ReturnedR0(0))
    }
}

/// `size_t wcstombs(char *dst, const wchar_t *src, size_t n)` —
/// truncate-on-overflow narrow conversion. Lossy: any code unit
/// outside `0x00..=0xff` becomes `'?'`.
fn wcstombs(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let s = read_wstr(ctx, src, 0x10000)?;
    let mut out: Vec<u8> = s
        .iter()
        .map(|&c| if c < 0x100 { c as u8 } else { b'?' })
        .collect();
    let written = if dst != 0 && n > 0 {
        let take = (n as usize).min(out.len());
        ctx.cpu.write_mem(dst, &out[..take])?;
        if take < n as usize {
            ctx.cpu.write_mem(dst + take as u32, &[0u8])?;
        }
        take as u32
    } else {
        out.len() as u32
    };
    let _ = &mut out;
    Ok(DispatchOutcome::ReturnedR0(written))
}

/// `size_t mbstowcs(wchar_t *dst, const char *src, size_t n)`.
fn mbstowcs(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let s = read_cstr(ctx, src, 0x10000)?;
    let wide: Vec<u16> = s.iter().map(|&b| b as u16).collect();
    let written = if dst != 0 && n > 0 {
        let take = (n as usize).min(wide.len());
        let bytes = wide_to_bytes(&wide[..take]);
        ctx.cpu.write_mem(dst, &bytes)?;
        if take < n as usize {
            ctx.cpu.write_mem(dst + (take as u32) * 2, &[0u8, 0u8])?;
        }
        take as u32
    } else {
        wide.len() as u32
    };
    Ok(DispatchOutcome::ReturnedR0(written))
}

/// Read a u32 argument from the variadic tail (slot index `idx`,
/// where 0 is the first variadic argument). The first 4 args go in
/// r0..r3, the rest are on the stack.
fn read_vararg_u32(ctx: &mut CallCtx<'_>, idx: u32) -> Result<u32, KernelError> {
    if idx < 4 {
        ctx.arg_u32(idx as u8)
    } else {
        let sp = ctx.cpu.read_reg(pocket_cpu::regs::ArmReg::Sp)?;
        let off = (idx - 4) * 4;
        let bytes = ctx.cpu.read_mem(sp + off, 4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

/// Render a printf-style format string by walking it character-by-
/// character and pulling arguments from the variadic tail. Supports
/// the conversions Pocket PC games actually use: `%d` `%i` `%u`
/// `%x` `%X` `%c` `%s` `%S` `%ls` `%p`, plus an `l` length modifier
/// and a basic width/zero-padding spec.
fn render_printf(
    ctx: &mut CallCtx<'_>,
    fmt: &str,
    fmt_is_wide: bool,
    arg_start: u32,
) -> Result<String, KernelError> {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    let mut next_arg = arg_start;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Flags and width.
        let mut zero_pad = false;
        let mut width: usize = 0;
        let mut long = false;
        loop {
            match chars.peek().copied() {
                Some('0') if width == 0 => {
                    zero_pad = true;
                    chars.next();
                }
                Some(d) if d.is_ascii_digit() => {
                    width = width * 10 + (d as usize - '0' as usize);
                    chars.next();
                }
                _ => break,
            }
        }
        if matches!(chars.peek(), Some('l') | Some('L')) {
            long = true;
            chars.next();
        }
        let conv = match chars.next() {
            Some(c) => c,
            None => break,
        };
        let mut piece = String::new();
        match conv {
            '%' => piece.push('%'),
            'd' | 'i' => {
                let v = read_vararg_u32(ctx, next_arg)? as i32;
                next_arg += 1;
                piece = v.to_string();
            }
            'u' => {
                let v = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                piece = v.to_string();
            }
            'x' => {
                let v = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                piece = format!("{v:x}");
            }
            'X' => {
                let v = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                piece = format!("{v:X}");
            }
            'p' => {
                let v = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                piece = format!("{v:08X}");
            }
            'c' => {
                let v = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                if let Some(ch) = char::from_u32(v & 0xff) {
                    piece.push(ch);
                }
            }
            's' => {
                let p = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                let pulls_wide = if fmt_is_wide { !long } else { long };
                if p == 0 {
                    piece.push_str("(null)");
                } else if pulls_wide {
                    let w = read_wstr(ctx, p, 0x10000)?;
                    piece = String::from_utf16_lossy(&w);
                } else {
                    let b = read_cstr(ctx, p, 0x10000)?;
                    piece = String::from_utf8_lossy(&b).into_owned();
                }
            }
            'S' => {
                let p = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                let pulls_wide = !fmt_is_wide;
                if p == 0 {
                    piece.push_str("(null)");
                } else if pulls_wide {
                    let w = read_wstr(ctx, p, 0x10000)?;
                    piece = String::from_utf16_lossy(&w);
                } else {
                    let b = read_cstr(ctx, p, 0x10000)?;
                    piece = String::from_utf8_lossy(&b).into_owned();
                }
            }
            other => {
                piece.push('%');
                piece.push(other);
            }
        }
        if width > piece.chars().count() {
            let pad = width - piece.chars().count();
            let ch = if zero_pad { '0' } else { ' ' };
            for _ in 0..pad {
                out.push(ch);
            }
        }
        out.push_str(&piece);
    }
    Ok(out)
}

/// `int printf(const char *fmt, ...)`.
fn printf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let fmt_p = ctx.arg_u32(0)?;
    let fmt = read_cstr_string(ctx, fmt_p, 0x4000)?;
    let s = render_printf(ctx, &fmt, false, 1)?;
    log::debug!("guest printf: {s}");
    Ok(DispatchOutcome::ReturnedR0(s.len() as u32))
}

/// `int sprintf(char *dst, const char *fmt, ...)`.
fn sprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let fmt_p = ctx.arg_u32(1)?;
    let fmt = read_cstr_string(ctx, fmt_p, 0x4000)?;
    let s = render_printf(ctx, &fmt, false, 2)?;
    let mut bytes = s.into_bytes();
    bytes.push(0);
    ctx.cpu.write_mem(dst, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(bytes.len() as u32 - 1))
}

/// `int vsprintf(char *dst, const char *fmt, va_list args)`.
/// `va_list` on ARM AAPCS is just a pointer to where varargs are
/// stacked; we treat it as a u32-array. This is good enough for the
/// printf-style callers Pocket PC games use (`int`, `char*`, `void*`,
/// floating point goes through soft-float helpers anyway).
fn vsprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let fmt_p = ctx.arg_u32(1)?;
    let va_p = ctx.arg_u32(2)?;
    let fmt = read_cstr_string(ctx, fmt_p, 0x4000)?;
    let s = render_printf_va(ctx, &fmt, false, va_p)?;
    let mut bytes = s.into_bytes();
    bytes.push(0);
    if dst != 0 {
        ctx.cpu.write_mem(dst, &bytes)?;
    }
    Ok(DispatchOutcome::ReturnedR0(bytes.len() as u32 - 1))
}

/// `int _vsnprintf(char *dst, size_t cap, const char *fmt, va_list args)`.
fn vsnprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cap = ctx.arg_u32(1)?;
    let fmt_p = ctx.arg_u32(2)?;
    let va_p = ctx.arg_u32(3)?;
    let fmt = read_cstr_string(ctx, fmt_p, 0x4000)?;
    let s = render_printf_va(ctx, &fmt, false, va_p)?;
    let mut bytes = s.into_bytes();
    bytes.push(0);
    if dst != 0 && cap > 0 {
        let n = (bytes.len() as u32).min(cap) as usize;
        ctx.cpu.write_mem(dst, &bytes[..n])?;
    }
    Ok(DispatchOutcome::ReturnedR0(bytes.len() as u32 - 1))
}

/// `int _snprintf(char *dst, size_t cap, const char *fmt, ...)`.
fn snprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cap = ctx.arg_u32(1)?;
    let fmt_p = ctx.arg_u32(2)?;
    let fmt = read_cstr_string(ctx, fmt_p, 0x4000)?;
    let s = render_printf(ctx, &fmt, false, 3)?;
    let mut bytes = s.into_bytes();
    bytes.push(0);
    if dst != 0 && cap > 0 {
        let n = (bytes.len() as u32).min(cap) as usize;
        ctx.cpu.write_mem(dst, &bytes[..n])?;
    }
    Ok(DispatchOutcome::ReturnedR0(bytes.len() as u32 - 1))
}

/// `int vswprintf(wchar_t *dst, const wchar_t *fmt, va_list args)`.
fn vswprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let fmt_p = ctx.arg_u32(1)?;
    let va_p = ctx.arg_u32(2)?;
    let fmt_w = read_wstr(ctx, fmt_p, 0x4000)?;
    let fmt = String::from_utf16_lossy(&fmt_w);
    let s = render_printf_va(ctx, &fmt, true, va_p)?;
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0u16)).collect();
    let bytes = wide_to_bytes(&wide);
    if dst != 0 {
        ctx.cpu.write_mem(dst, &bytes)?;
    }
    Ok(DispatchOutcome::ReturnedR0(wide.len() as u32 - 1))
}

/// `int _vsnwprintf(wchar_t *dst, size_t cap, const wchar_t *fmt, va_list)`.
fn vsnwprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cap = ctx.arg_u32(1)?;
    let fmt_p = ctx.arg_u32(2)?;
    let va_p = ctx.arg_u32(3)?;
    let fmt_w = read_wstr(ctx, fmt_p, 0x4000)?;
    let fmt = String::from_utf16_lossy(&fmt_w);
    let s = render_printf_va(ctx, &fmt, true, va_p)?;
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0u16)).collect();
    let bytes = wide_to_bytes(&wide);
    if dst != 0 && cap > 0 {
        let n_chars = (wide.len() as u32).min(cap) as usize;
        ctx.cpu.write_mem(dst, &bytes[..n_chars * 2])?;
    }
    Ok(DispatchOutcome::ReturnedR0(wide.len() as u32 - 1))
}

/// `int _snwprintf(wchar_t *dst, size_t cap, const wchar_t *fmt, ...)`.
fn snwprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cap = ctx.arg_u32(1)?;
    let fmt_p = ctx.arg_u32(2)?;
    let fmt_w = read_wstr(ctx, fmt_p, 0x4000)?;
    let fmt = String::from_utf16_lossy(&fmt_w);
    let s = render_printf(ctx, &fmt, true, 3)?;
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0u16)).collect();
    let bytes = wide_to_bytes(&wide);
    if dst != 0 && cap > 0 {
        let n_chars = (wide.len() as u32).min(cap) as usize;
        ctx.cpu.write_mem(dst, &bytes[..n_chars * 2])?;
    }
    Ok(DispatchOutcome::ReturnedR0(wide.len() as u32 - 1))
}

/// Variant of [`render_printf`] that pulls varargs out of the
/// `va_list` pointer the caller passed instead of the current
/// stack frame.
fn render_printf_va(
    ctx: &mut CallCtx<'_>,
    fmt: &str,
    fmt_is_wide: bool,
    va_p: u32,
) -> Result<String, KernelError> {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    let mut next_off: u32 = 0;
    let read_va = |ctx: &mut CallCtx<'_>, off: u32| -> Result<u32, KernelError> {
        if va_p == 0 {
            return Ok(0);
        }
        let bytes = ctx.cpu.read_mem(va_p + off, 4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut zero_pad = false;
        let mut width: usize = 0;
        let mut long = false;
        loop {
            match chars.peek().copied() {
                Some('0') if width == 0 => {
                    zero_pad = true;
                    chars.next();
                }
                Some(d) if d.is_ascii_digit() => {
                    width = width * 10 + (d as usize - '0' as usize);
                    chars.next();
                }
                _ => break,
            }
        }
        if matches!(chars.peek(), Some('l') | Some('L')) {
            long = true;
            chars.next();
        }
        let conv = match chars.next() {
            Some(c) => c,
            None => break,
        };
        let mut piece = String::new();
        match conv {
            '%' => piece.push('%'),
            'd' | 'i' => {
                let v = read_va(ctx, next_off)? as i32;
                next_off += 4;
                piece = v.to_string();
            }
            'u' => {
                let v = read_va(ctx, next_off)?;
                next_off += 4;
                piece = v.to_string();
            }
            'x' => {
                let v = read_va(ctx, next_off)?;
                next_off += 4;
                piece = format!("{v:x}");
            }
            'X' => {
                let v = read_va(ctx, next_off)?;
                next_off += 4;
                piece = format!("{v:X}");
            }
            'p' => {
                let v = read_va(ctx, next_off)?;
                next_off += 4;
                piece = format!("{v:08X}");
            }
            'c' => {
                let v = read_va(ctx, next_off)?;
                next_off += 4;
                if let Some(ch) = char::from_u32(v & 0xff) {
                    piece.push(ch);
                }
            }
            's' => {
                let p = read_va(ctx, next_off)?;
                next_off += 4;
                let pulls_wide = if fmt_is_wide { !long } else { long };
                if p == 0 {
                    piece.push_str("(null)");
                } else if pulls_wide {
                    let w = read_wstr(ctx, p, 0x10000)?;
                    piece = String::from_utf16_lossy(&w);
                } else {
                    let b = read_cstr(ctx, p, 0x10000)?;
                    piece = String::from_utf8_lossy(&b).into_owned();
                }
            }
            'S' => {
                let p = read_va(ctx, next_off)?;
                next_off += 4;
                let pulls_wide = !fmt_is_wide;
                if p == 0 {
                    piece.push_str("(null)");
                } else if pulls_wide {
                    let w = read_wstr(ctx, p, 0x10000)?;
                    piece = String::from_utf16_lossy(&w);
                } else {
                    let b = read_cstr(ctx, p, 0x10000)?;
                    piece = String::from_utf8_lossy(&b).into_owned();
                }
            }
            other => {
                piece.push('%');
                piece.push(other);
            }
        }
        if width > piece.chars().count() {
            let pad = width - piece.chars().count();
            let ch = if zero_pad { '0' } else { ' ' };
            for _ in 0..pad {
                out.push(ch);
            }
        }
        out.push_str(&piece);
    }
    Ok(out)
}

/// `int swprintf(wchar_t *dst, const wchar_t *fmt, ...)` and
/// `int wsprintfW(LPWSTR dst, LPCWSTR fmt, ...)` (same shape).
fn swprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let fmt_p = ctx.arg_u32(1)?;
    let fmt_w = read_wstr(ctx, fmt_p, 0x4000)?;
    let fmt = String::from_utf16_lossy(&fmt_w);
    let s = render_printf(ctx, &fmt, true, 2)?;
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0u16)).collect();
    let bytes = wide_to_bytes(&wide);
    ctx.cpu.write_mem(dst, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(wide.len() as u32 - 1))
}

fn wide_to_bytes(s: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for c in s {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out
}

fn cmp_to_int(o: std::cmp::Ordering) -> i32 {
    match o {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `int tolower(int c)` — preserve EOF and convert only ASCII letters.
/// The Windows CE CRT uses the signed-char convention, so bytes above
/// 0x7f are returned unchanged rather than indexing a host locale table.
fn tolower(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let c = ctx.arg_u32(0)?;
    let value = if c == u32::MAX {
        c
    } else if (b'A' as u32..=b'Z' as u32).contains(&c) {
        c + (b'a' - b'A') as u32
    } else {
        c
    };
    Ok(DispatchOutcome::ReturnedR0(value))
}

/// `int toupper(int c)` — preserve EOF and convert only ASCII letters.
fn toupper(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let c = ctx.arg_u32(0)?;
    let value = if c == u32::MAX {
        c
    } else if (b'a' as u32..=b'z' as u32).contains(&c) {
        c - (b'a' - b'A') as u32
    } else {
        c
    };
    Ok(DispatchOutcome::ReturnedR0(value))
}

fn to_lower_w(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

// ---------- file I/O ----------

/// `HANDLE CreateFileW(LPCWSTR name, DWORD access, DWORD share, ...,
///                     DWORD creation, DWORD flags, HANDLE template)`
///
/// We honour `access` (`GENERIC_READ` 0x80000000, `GENERIC_WRITE`
/// 0x40000000) and `creation` (`CREATE_ALWAYS` 2, `CREATE_NEW` 1,
/// `OPEN_ALWAYS` 4) loosely — enough to satisfy a game that just
/// wants to load assets and persist a save file.
fn create_file_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use pocket_kernel::vfs::Access;
    let name_p = ctx.arg_u32(0)?;
    let access_flags = ctx.arg_u32(1)?;
    let creation = ctx.arg_u32(4)?;
    if name_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(INVALID_HANDLE_VALUE));
    }
    let name_w = match read_wstr(ctx, name_p, 260) {
        Ok(n) => n,
        Err(_) => return Ok(DispatchOutcome::ReturnedR0(INVALID_HANDLE_VALUE)),
    };
    let path = String::from_utf16_lossy(&name_w);
    let access = match (
        access_flags & 0x8000_0000 != 0,
        access_flags & 0x4000_0000 != 0,
    ) {
        (true, true) => Access::ReadWrite,
        (false, true) => Access::Write,
        _ => Access::Read,
    };
    let create = matches!(creation, 1 | 2 | 4);
    match ctx.kernel.vfs.open(&path, access, create) {
        Some(h) => {
            log::debug!("CreateFileW({path:?}, access={access:?}) -> 0x{h:08x}");
            Ok(DispatchOutcome::ReturnedR0(h))
        }
        None => {
            // Promoted from `trace` to `debug` so that
            // `RUST_LOG=…,pocket_winceapi=debug` reveals the exact
            // path a game tried (and failed) to open. This is the
            // single most-useful breadcrumb when figuring out which
            // asset / save-game / config file the title needs us to
            // mount under the guest VFS.
            log::debug!(
                "CreateFileW({path:?}, access={access:?}, creation={creation}) -> INVALID_HANDLE_VALUE",
            );
            Ok(DispatchOutcome::ReturnedR0(INVALID_HANDLE_VALUE))
        }
    }
}

/// `BOOL ReadFile(HANDLE h, void* buf, DWORD count, DWORD* read,
///                LPOVERLAPPED ov)`
fn read_file(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buf_p = ctx.arg_u32(1)?;
    let count = ctx.arg_u32(2)?;
    let out_read_p = ctx.arg_u32(3)?;
    if !ctx.kernel.vfs.is_open(handle) {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut buf = vec![0u8; count as usize];
    let n = ctx.kernel.vfs.read(handle, &mut buf).unwrap_or(0);
    if buf_p != 0 && n > 0 {
        ctx.cpu.write_mem(buf_p, &buf[..n])?;
    }
    if out_read_p != 0 {
        ctx.cpu.write_mem(out_read_p, &(n as u32).to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL WriteFile(HANDLE h, const void* buf, DWORD count, DWORD* written, ...)`
fn write_file(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buf_p = ctx.arg_u32(1)?;
    let count = ctx.arg_u32(2)?;
    let out_written_p = ctx.arg_u32(3)?;
    if !ctx.kernel.vfs.is_open(handle) || count == 0 {
        return Ok(DispatchOutcome::ReturnedR0(if count == 0 { 1 } else { 0 }));
    }
    let bytes = ctx.cpu.read_mem(buf_p, count)?;
    let n = ctx.kernel.vfs.write(handle, &bytes).unwrap_or(0);
    if out_written_p != 0 {
        ctx.cpu
            .write_mem(out_written_p, &(n as u32).to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn close_handle(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let _ = ctx.kernel.vfs.close(handle);
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `DWORD GetFileSize(HANDLE h, DWORD* high)`
fn get_file_size(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let high_p = ctx.arg_u32(1)?;
    let size = ctx.kernel.vfs.size(handle).unwrap_or(0);
    if high_p != 0 {
        ctx.cpu
            .write_mem(high_p, &((size >> 32) as u32).to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(size as u32))
}

/// `DWORD GetFileAttributesW(LPCWSTR path)` — query the VFS so that
/// games which probe asset paths before opening them get sensible
/// answers. Returns `FILE_ATTRIBUTE_NORMAL` (0x80) for regular files
/// and `FILE_ATTRIBUTE_DIRECTORY` (0x10) for directories. Missing
/// files / NULL pointers / unmounted prefixes return
/// `INVALID_FILE_ATTRIBUTES` (0xFFFF_FFFF) just like Windows does.
fn get_file_attributes_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    let name_p = ctx.arg_u32(0)?;
    if name_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(INVALID_FILE_ATTRIBUTES));
    }
    let name_w = match read_wstr(ctx, name_p, 260) {
        Ok(n) => n,
        Err(_) => return Ok(DispatchOutcome::ReturnedR0(INVALID_FILE_ATTRIBUTES)),
    };
    let path = String::from_utf16_lossy(&name_w);
    let host = match ctx.kernel.vfs.resolve(&path) {
        Some(p) => p,
        None => {
            log::trace!("GetFileAttributesW({path:?}) -> INVALID (no mount)");
            return Ok(DispatchOutcome::ReturnedR0(INVALID_FILE_ATTRIBUTES));
        }
    };
    let meta = match std::fs::metadata(&host) {
        Ok(m) => m,
        Err(_) => {
            log::trace!("GetFileAttributesW({path:?}) -> INVALID (host miss {host:?})");
            return Ok(DispatchOutcome::ReturnedR0(INVALID_FILE_ATTRIBUTES));
        }
    };
    let attrs = if meta.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    log::trace!("GetFileAttributesW({path:?}) -> 0x{attrs:08x}");
    Ok(DispatchOutcome::ReturnedR0(attrs))
}

/// `DWORD SetFilePointer(HANDLE h, LONG distance, LONG* hi, DWORD whence)`
fn set_file_pointer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use pocket_kernel::vfs::SeekKind;
    let handle = ctx.arg_u32(0)?;
    let distance = ctx.arg_u32(1)? as i32 as i64;
    let whence = ctx.arg_u32(3)?;
    let kind = match whence {
        0 => SeekKind::Begin,
        1 => SeekKind::Current,
        2 => SeekKind::End,
        _ => SeekKind::Begin,
    };
    let pos = ctx.kernel.vfs.seek(handle, distance, kind).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(pos as u32))
}

// ---------- C-runtime file I/O ----------

fn read_cstr_string(ctx: &mut CallCtx<'_>, p: u32, max: u32) -> Result<String, KernelError> {
    if p == 0 {
        return Ok(String::new());
    }
    let bytes = read_cstr(ctx, p, max)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn open_cstr_path(ctx: &mut CallCtx<'_>, path: &str, mode: &str) -> u32 {
    use pocket_kernel::vfs::Access;
    let access = if mode.contains('+') {
        Access::ReadWrite
    } else if mode.starts_with('w') || mode.starts_with('a') {
        Access::Write
    } else {
        Access::Read
    };
    let create = mode.starts_with('w') || mode.starts_with('a') || mode.contains('+');
    // Pocket PC games sometimes pass `Game/data.bin` without a leading
    // backslash; the VFS expects `\Game\…`. Try both spellings so the
    // ROM lookup succeeds.
    let normalized = path.replace('/', "\\");
    let candidates = [
        normalized.clone(),
        if normalized.starts_with('\\') {
            normalized.clone()
        } else {
            format!("\\{normalized}")
        },
        format!("\\Application\\{normalized}"),
        format!("\\Program Files\\Game\\{normalized}"),
        format!("\\Program Files\\Atomic Dreams\\{normalized}"),
    ];
    for cand in &candidates {
        if let Some(h) = ctx.kernel.vfs.open(cand, access, create) {
            log::trace!("fopen({cand:?}, {mode:?}) -> 0x{h:08x}");
            return h;
        }
    }
    log::trace!("fopen({path:?}, {mode:?}) -> NULL");
    0
}

fn crt_fopen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name_p = ctx.arg_u32(0)?;
    let mode_p = ctx.arg_u32(1)?;
    let name = read_cstr_string(ctx, name_p, 260)?;
    let mode = read_cstr_string(ctx, mode_p, 8)?;
    let h = open_cstr_path(ctx, &name, &mode);
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn crt_wfopen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name_p = ctx.arg_u32(0)?;
    let mode_p = ctx.arg_u32(1)?;
    let name_w = read_wstr(ctx, name_p, 260)?;
    let mode_w = read_wstr(ctx, mode_p, 8)?;
    let name = String::from_utf16_lossy(&name_w);
    let mode = String::from_utf16_lossy(&mode_w);
    let h = open_cstr_path(ctx, &name, &mode);
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn crt_fclose(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    ctx.kernel.vfs.close(h);
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn crt_fread(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let buf = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;
    let count = ctx.arg_u32(2)?;
    let h = ctx.arg_u32(3)?;
    let total = size.saturating_mul(count);
    if !ctx.kernel.vfs.is_open(h) || total == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut tmp = vec![0u8; total as usize];
    let n = ctx.kernel.vfs.read(h, &mut tmp).unwrap_or(0);
    if buf != 0 && n > 0 {
        ctx.cpu.write_mem(buf, &tmp[..n])?;
    }
    let elements = (n as u32).checked_div(size).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(elements))
}

fn crt_fwrite(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let buf = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;
    let count = ctx.arg_u32(2)?;
    let h = ctx.arg_u32(3)?;
    let total = size.saturating_mul(count);
    if !ctx.kernel.vfs.is_open(h) || total == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(buf, total)?;
    let n = ctx.kernel.vfs.write(h, &bytes).unwrap_or(0);
    let elements = (n as u32).checked_div(size).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(elements))
}

fn crt_fseek(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use pocket_kernel::vfs::SeekKind;
    let h = ctx.arg_u32(0)?;
    let off = ctx.arg_u32(1)? as i32 as i64;
    let whence = ctx.arg_u32(2)?;
    let kind = match whence {
        0 => SeekKind::Begin,
        1 => SeekKind::Current,
        2 => SeekKind::End,
        _ => SeekKind::Begin,
    };
    let r = ctx.kernel.vfs.seek(h, off, kind);
    Ok(DispatchOutcome::ReturnedR0(if r.is_some() {
        0
    } else {
        u32::MAX
    }))
}

fn crt_ftell(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use pocket_kernel::vfs::SeekKind;
    let h = ctx.arg_u32(0)?;
    let pos = ctx.kernel.vfs.seek(h, 0, SeekKind::Current).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(pos as u32))
}

fn crt_feof(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use pocket_kernel::vfs::SeekKind;
    let h = ctx.arg_u32(0)?;
    let size = ctx.kernel.vfs.size(h).unwrap_or(0);
    let pos = ctx.kernel.vfs.seek(h, 0, SeekKind::Current).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(if pos >= size { 1 } else { 0 }))
}

fn crt_rewind(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use pocket_kernel::vfs::SeekKind;
    let h = ctx.arg_u32(0)?;
    let _ = ctx.kernel.vfs.seek(h, 0, SeekKind::Begin);
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn crt_fgetc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    if !ctx.kernel.vfs.is_open(h) {
        return Ok(DispatchOutcome::ReturnedR0(u32::MAX));
    }
    let mut buf = [0u8; 1];
    let n = ctx.kernel.vfs.read(h, &mut buf).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(if n == 0 {
        u32::MAX
    } else {
        buf[0] as u32
    }))
}

fn crt_fputc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let c = ctx.arg_u32(0)?;
    let h = ctx.arg_u32(1)?;
    if !ctx.kernel.vfs.is_open(h) {
        return Ok(DispatchOutcome::ReturnedR0(u32::MAX));
    }
    let _ = ctx.kernel.vfs.write(h, &[c as u8]);
    Ok(DispatchOutcome::ReturnedR0(c & 0xFF))
}

fn crt_fgets(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let buf = ctx.arg_u32(0)?;
    let n = ctx.arg_u32(1)?;
    let h = ctx.arg_u32(2)?;
    if buf == 0 || n <= 1 || !ctx.kernel.vfs.is_open(h) {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut out = Vec::with_capacity(n as usize);
    let mut byte = [0u8; 1];
    while out.len() + 1 < n as usize {
        let read = ctx.kernel.vfs.read(h, &mut byte).unwrap_or(0);
        if read == 0 {
            break;
        }
        out.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    if out.is_empty() {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    out.push(0);
    ctx.cpu.write_mem(buf, &out)?;
    Ok(DispatchOutcome::ReturnedR0(buf))
}

fn crt_fputs(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let s_p = ctx.arg_u32(0)?;
    let h = ctx.arg_u32(1)?;
    if !ctx.kernel.vfs.is_open(h) {
        return Ok(DispatchOutcome::ReturnedR0(u32::MAX));
    }
    let s = read_cstr(ctx, s_p, 4096)?;
    let n = ctx.kernel.vfs.write(h, &s).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(if n > 0 {
        1
    } else {
        u32::MAX
    }))
}

// ---------- ARM compiler integer division helpers ----------

/// `__rt_sdiv(int divisor in r0, int dividend in r1) -> {r0=quot, r1=rem}`
fn rt_sdiv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = ctx.arg_u32(0)? as i32;
    let n = ctx.arg_u32(1)? as i32;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let q = n.wrapping_div(d) as u32;
    let r = n.wrapping_rem(d) as u32;
    Ok(DispatchOutcome::ReturnedR0R1(q, r))
}

fn rt_udiv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = ctx.arg_u32(0)?;
    let n = ctx.arg_u32(1)?;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    Ok(DispatchOutcome::ReturnedR0R1(n / d, n % d))
}

fn rt_sdiv64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // (lo,hi) of 64-bit divisor in r0,r1; (lo,hi) of dividend in r2,r3
    let d = ((ctx.arg_u32(1)? as u64) << 32 | ctx.arg_u32(0)? as u64) as i64;
    let n = ((ctx.arg_u32(3)? as u64) << 32 | ctx.arg_u32(2)? as u64) as i64;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let q = n.wrapping_div(d) as u64;
    Ok(DispatchOutcome::ReturnedR0R1(q as u32, (q >> 32) as u32))
}

fn rt_udiv64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = (ctx.arg_u32(1)? as u64) << 32 | ctx.arg_u32(0)? as u64;
    let n = (ctx.arg_u32(3)? as u64) << 32 | ctx.arg_u32(2)? as u64;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let q = n / d;
    Ok(DispatchOutcome::ReturnedR0R1(q as u32, (q >> 32) as u32))
}

fn rt_srsh(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Arithmetic right shift of a 64-bit value: r0 lo, r1 hi, r2 shift.
    let lo = ctx.arg_u32(0)?;
    let hi = ctx.arg_u32(1)?;
    let s = ctx.arg_u32(2)? & 63;
    let v = ((hi as u64) << 32 | lo as u64) as i64 >> s;
    Ok(DispatchOutcome::ReturnedR0R1(v as u32, (v >> 32) as u32))
}

fn rt_sdiv10(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ctx.arg_u32(0)? as i32;
    let q = n.wrapping_div(10) as u32;
    let r = n.wrapping_rem(10) as u32;
    Ok(DispatchOutcome::ReturnedR0R1(q, r))
}

fn rt_udiv10(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0R1(n / 10, n % 10))
}

// ---------- heap ----------

const FAKE_PROCESS_HEAP: u32 = 0x4242_4242;

fn local_alloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // LMEM_ZEROINIT flag = 0x0040
    let flags = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;
    do_alloc(ctx, size, flags & 0x0040 != 0)
}

fn local_free(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p != 0 {
        do_free(ctx, p);
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn local_realloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;
    do_realloc(ctx, p, size)
}

/// `LocalSize(HLOCAL hMem)` — return the size of the block, or 0 for
/// an unknown pointer. Doubles as the C runtime `_msize`.
fn local_size(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let sz = if p == 0 {
        0
    } else {
        ctx.kernel.heap.msize(p).unwrap_or(0)
    };
    Ok(DispatchOutcome::ReturnedR0(sz))
}

fn heap_create(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_PROCESS_HEAP))
}

fn heap_alloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // HeapAlloc(HANDLE hHeap, DWORD flags, SIZE_T size); HEAP_ZERO_MEMORY = 0x8
    let flags = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)?;
    do_alloc(ctx, size, flags & 0x8 != 0)
}

fn heap_free(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(2)?;
    if p != 0 {
        do_free(ctx, p);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn heap_realloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(2)?;
    let size = ctx.arg_u32(3)?;
    do_realloc(ctx, p, size)
}

fn get_process_heap(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_PROCESS_HEAP))
}

fn virtual_alloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // VirtualAlloc(LPVOID addr, SIZE_T size, DWORD type, DWORD protect)
    let size = ctx.arg_u32(1)?;
    do_alloc(ctx, size, true)
}

fn malloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let size = ctx.arg_u32(0)?;
    do_alloc(ctx, size, false)
}

fn calloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let nmemb = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;
    do_alloc(ctx, nmemb.saturating_mul(size), true)
}

fn free(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p != 0 {
        do_free(ctx, p);
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn realloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;
    do_realloc(ctx, p, size)
}

/// Shared allocation path for every alloc-shaped API. The host-side
/// [`pocket_kernel::Heap`] tracks the requested size out of band, so
/// `LocalSize` / `_msize` / `do_free` / `do_realloc` can recover it
/// later without trusting guest memory.
fn do_alloc(
    ctx: &mut CallCtx<'_>,
    size: u32,
    zero_init: bool,
) -> Result<DispatchOutcome, KernelError> {
    let user_ptr = match ctx.kernel.heap.alloc(size) {
        Some(p) => p,
        None => {
            log::warn!("heap exhausted; alloc({size}) failed");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    if zero_init && size > 0 {
        let zeros = vec![0u8; size as usize];
        ctx.cpu.write_mem(user_ptr, &zeros)?;
    }
    Ok(DispatchOutcome::ReturnedR0(user_ptr))
}

fn do_free(ctx: &mut CallCtx<'_>, user_ptr: u32) {
    ctx.kernel.heap.free(user_ptr);
}

fn do_realloc(
    ctx: &mut CallCtx<'_>,
    p: u32,
    new_size: u32,
) -> Result<DispatchOutcome, KernelError> {
    if p == 0 {
        return do_alloc(ctx, new_size, false);
    }
    let old_size = ctx.kernel.heap.msize(p).unwrap_or(0);
    if new_size == 0 {
        do_free(ctx, p);
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let new_p = match ctx.kernel.heap.alloc(new_size) {
        Some(np) => np,
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    let to_copy = old_size.min(new_size);
    if to_copy > 0 {
        let bytes = ctx.cpu.read_mem(p, to_copy)?;
        ctx.cpu.write_mem(new_p, &bytes)?;
    }
    do_free(ctx, p);
    Ok(DispatchOutcome::ReturnedR0(new_p))
}

// ---------- window / message ----------

fn register_class_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // The first argument is `const WNDCLASS *`. On 32-bit Windows
    // the layout is:
    //   UINT      style;          (off 0)
    //   WNDPROC   lpfnWndProc;    (off 4)
    //   int       cbClsExtra;     (off 8)
    //   int       cbWndExtra;     (off 12)
    //   HINSTANCE hInstance;      (off 16)
    //   ...
    // We only care about lpfnWndProc — capture it so DispatchMessageW
    // can trampoline into the guest WndProc.
    let lpwc = ctx.arg_u32(0)?;
    if lpwc != 0 {
        if let Ok(buf) = ctx.cpu.read_mem(lpwc + 4, 4) {
            let proc_va = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if proc_va != 0 {
                ctx.kernel.wnd_proc = proc_va;
                log::info!(
                    "RegisterClassW captured WndProc=0x{:08x} from WNDCLASS at 0x{:08x}",
                    proc_va,
                    lpwc
                );
            }
        }
    }
    // ATOMs are 16-bit; return a non-zero one.
    Ok(DispatchOutcome::ReturnedR0(0xC001))
}

fn create_window_ex_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_HWND))
}

fn show_window(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn update_window(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.wnd_proc != 0 {
        ctx.kernel.pending_message = Some((WM_PAINT, 0, 0));
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn dispatch_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // DispatchMessageW(const MSG *lpMsg) — pass the message into the
    // captured WndProc and trampoline guest execution into it. The
    // WndProc's epilogue will return to our LR (the message-loop
    // call site), so the loop continues normally.
    let lp_msg = ctx.arg_u32(0)?;
    let wnd_proc = ctx.kernel.wnd_proc;
    if wnd_proc == 0 || lp_msg == 0 {
        // No registered WndProc / no message → behave like the old
        // stub: return 0, control resumes from LR.
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let buf = match ctx.cpu.read_mem(lp_msg, 16) {
        Ok(b) => b,
        Err(_) => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    let hwnd = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let message = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let wparam = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let lparam = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    log::debug!(
        "DispatchMessageW trampoline -> WndProc(hwnd=0x{:x}, msg=0x{:x}, wp=0x{:x}, lp=0x{:x}) at 0x{:08x}",
        hwnd, message, wparam, lparam, wnd_proc
    );
    use pocket_cpu::regs::ArmReg;
    ctx.cpu.write_reg(ArmReg::R0, hwnd)?;
    ctx.cpu.write_reg(ArmReg::R1, message)?;
    ctx.cpu.write_reg(ArmReg::R2, wparam)?;
    ctx.cpu.write_reg(ArmReg::R3, lparam)?;
    // LR is already the message-loop's return address — leave it.
    Ok(DispatchOutcome::JumpTo(wnd_proc))
}

/// Build a synthetic `MSG` blob (28 bytes on 32-bit Windows) and write
/// it into the guest pointer. `message` selects which window message
/// (e.g. `WM_PAINT = 0x000F` or `WM_QUIT = 0x0012`).
fn write_synthetic_msg(
    cpu: &mut dyn pocket_cpu::Cpu,
    lp_msg: u32,
    message: u32,
    wparam: u32,
    lparam: u32,
) -> Result<(), KernelError> {
    if lp_msg == 0 {
        return Ok(());
    }
    // MSG: HWND hwnd; UINT message; WPARAM wParam; LPARAM lParam;
    //      DWORD time; POINT pt; — 28 bytes total.
    let mut msg = [0u8; 28];
    msg[0..4].copy_from_slice(&FAKE_HWND.to_le_bytes());
    msg[4..8].copy_from_slice(&message.to_le_bytes());
    msg[8..12].copy_from_slice(&wparam.to_le_bytes());
    msg[12..16].copy_from_slice(&lparam.to_le_bytes());
    cpu.write_mem(lp_msg, &msg)?;
    Ok(())
}

// Win32 window-message constants used by the message pump.
const WM_CREATE: u32 = 0x0001;
const WM_QUIT: u32 = 0x0012;
const WM_PAINT: u32 = 0x000F;
const WM_TIMER: u32 = 0x0113;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const MK_LBUTTON: u32 = 0x0001;

/// Convert a host-driven [`pocket_kernel::InputEvent`] into the
/// `(msg, wParam, lParam)` triple a real Win32 window message
/// would carry. Returns `None` for events we don't currently model.
fn input_to_message(ev: pocket_kernel::InputEvent) -> Option<(u32, u32, u32)> {
    match ev {
        pocket_kernel::InputEvent::PointerDown { x, y } => {
            let lparam = ((y as u32) << 16) | (x as u32);
            Some((WM_LBUTTONDOWN, MK_LBUTTON, lparam))
        }
        pocket_kernel::InputEvent::PointerUp { x, y } => {
            let lparam = ((y as u32) << 16) | (x as u32);
            Some((WM_LBUTTONUP, 0, lparam))
        }
        pocket_kernel::InputEvent::PointerMove { x, y } => {
            let lparam = ((y as u32) << 16) | (x as u32);
            Some((WM_MOUSEMOVE, MK_LBUTTON, lparam))
        }
        pocket_kernel::InputEvent::KeyDown { vk } => Some((WM_KEYDOWN, vk as u32, 1)),
        pocket_kernel::InputEvent::KeyUp { vk } => Some((WM_KEYUP, vk as u32, 0xC000_0001)),
    }
}

fn key_state_value(ctx: &mut CallCtx<'_>, vk: u32) -> u32 {
    let aliases = |code: usize| -> [usize; 2] {
        match code {
            0xC1..=0xC4 => [code, code + 0x10],
            0xD1..=0xD4 => [code, code - 0x10],
            _ => [code, code],
        }
    };
    let queried = if vk < 256 {
        aliases(vk as usize)
    } else {
        [usize::MAX; 2]
    };
    let pressed_now = if vk < 256 {
        ctx.kernel.pressed_keys[queried[0]] || ctx.kernel.pressed_keys[queried[1]]
    } else {
        false
    };
    let pending_state = ctx
        .kernel
        .pending_input
        .iter()
        .rev()
        .find_map(|event| match event {
            pocket_kernel::InputEvent::KeyDown { vk: pending } => {
                let keys = aliases(*pending as usize);
                Some(keys[0] == queried[0] || keys[0] == queried[1] || keys[1] == queried[0])
            }
            pocket_kernel::InputEvent::KeyUp { vk: pending } => {
                let keys = aliases(*pending as usize);
                Some(!(keys[0] == queried[0] || keys[0] == queried[1] || keys[1] == queried[0]))
            }
            _ => None,
        })
        .unwrap_or(false);
    if pressed_now || pending_state {
        0x8000
    } else {
        0
    }
}

fn get_key_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let vk = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(key_state_value(ctx, vk)))
}

fn get_async_key_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let vk = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(key_state_value(ctx, vk)))
}

fn get_focus(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_HWND))
}

fn get_capture(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_HWND))
}

fn monotonic_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn update_key_state(ctx: &mut CallCtx<'_>, ev: pocket_kernel::InputEvent) {
    match ev {
        pocket_kernel::InputEvent::KeyDown { vk } => {
            if (vk as usize) < ctx.kernel.pressed_keys.len() {
                ctx.kernel.pressed_keys[vk as usize] = true;
            }
        }
        pocket_kernel::InputEvent::KeyUp { vk } => {
            if (vk as usize) < ctx.kernel.pressed_keys.len() {
                ctx.kernel.pressed_keys[vk as usize] = false;
            }
        }
        pocket_kernel::InputEvent::PointerDown { .. }
        | pocket_kernel::InputEvent::PointerUp { .. }
        | pocket_kernel::InputEvent::PointerMove { .. } => {}
    }
}

/// Pick which fake message to deliver next given the current count
/// and the timer the guest has registered (if any).
///
/// This only fabricates *idle* traffic so the run loop never sits
/// silent — `WM_PAINT` to drive redraws and `WM_TIMER` to drive
/// timer-based game ticks (the typical PPC2003 pattern: `WM_CREATE`
/// installs a `~5 ms` timer, `WM_TIMER` runs the per-frame logic).
///
/// Real user input — taps and key presses — is exclusively the
/// frontend's responsibility via [`KernelState::pending_input`]; we
/// never synthesise user input here. Doing so would mean the game
/// "presses buttons by itself" between real presses, which is exactly
/// the user-visible bug we want to avoid.
fn synthetic_message_for(ctx: &mut CallCtx<'_>) -> (u32, u32, u32) {
    let now = monotonic_ms();
    let timer_due = ctx.kernel.synthetic_timer_id != 0 && now >= ctx.kernel.synthetic_timer_next_ms;
    let paint_due = now >= ctx.kernel.synthetic_paint_next_ms;
    if !timer_due && !paint_due {
        let next = if ctx.kernel.synthetic_timer_id != 0 {
            ctx.kernel
                .synthetic_timer_next_ms
                .min(ctx.kernel.synthetic_paint_next_ms)
        } else {
            ctx.kernel.synthetic_paint_next_ms
        };
        let wait_ms = next.saturating_sub(now);
        if wait_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(wait_ms.min(16)));
        }
    }
    let now = monotonic_ms();
    if ctx.kernel.synthetic_timer_id != 0 && now >= ctx.kernel.synthetic_timer_next_ms {
        let interval = ctx.kernel.synthetic_timer_interval_ms.max(1) as u64;
        ctx.kernel.synthetic_timer_next_ms = now.saturating_add(interval);
        return (WM_TIMER, ctx.kernel.synthetic_timer_id, 0);
    }
    ctx.kernel.synthetic_paint_next_ms = now.saturating_add(16);
    if ctx.kernel.synthetic_message_count.is_multiple_of(2) {
        (WM_PAINT, 0, 0)
    } else {
        (WM_TIMER, ctx.kernel.synthetic_timer_id.max(1), 0)
    }
}

/// Pop the next message to deliver. Real user input from the host
/// frontend drains [`KernelState::pending_input`] first so that taps
/// and D-pad presses always win over the synthetic pump; once the
/// queue is empty we fall back to fabricated traffic so games never
/// see an idle window.
fn next_message(ctx: &mut CallCtx<'_>) -> (u32, u32, u32) {
    if let Some(ev) = ctx.kernel.pending_input.pop_front() {
        update_key_state(ctx, ev);
        if let Some(triple) = input_to_message(ev) {
            return triple;
        }
    }
    synthetic_message_for(ctx)
}

/// `BOOL GetMessageW(LPMSG lpMsg, HWND hWnd, UINT wMsgFilterMin, UINT wMsgFilterMax)`
///
/// We have no real OS message queue. To drive an HLE'd Pocket PC game
/// to actually paint, we fabricate a series of `WM_PAINT` messages
/// interspersed with synthetic taps and key presses (up to
/// `synthetic_message_budget`), then signal `WM_QUIT` with a `0`
/// return so the loop tears down cleanly. Real user input from the
/// host frontend (mouse / D-pad / keyboard) is delivered before any
/// synthetic message; see [`next_message`].
fn get_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let lp_msg = ctx.arg_u32(0)?;
    let count = ctx.kernel.synthetic_message_count;
    let budget = ctx.kernel.synthetic_message_budget;
    if budget > 0 && count >= budget {
        write_synthetic_msg(ctx.cpu, lp_msg, WM_QUIT, 0, 0)?;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if !ctx.kernel.synthetic_create_sent {
        ctx.kernel.synthetic_create_sent = true;
        write_synthetic_msg(ctx.cpu, lp_msg, WM_CREATE, 0, 0)?;
        ctx.kernel.synthetic_message_count = count + 1;
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let (msg, wp, lp) = ctx
        .kernel
        .pending_message
        .take()
        .unwrap_or_else(|| next_message(ctx));
    write_synthetic_msg(ctx.cpu, lp_msg, msg, wp, lp)?;
    ctx.kernel.synthetic_message_count += 1;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn peek_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let lp_msg = ctx.arg_u32(0)?;
    let remove_mode = ctx.arg_u32(4)?;
    let count = ctx.kernel.synthetic_message_count;
    let budget = ctx.kernel.synthetic_message_budget;
    if budget > 0 && count >= budget {
        write_synthetic_msg(ctx.cpu, lp_msg, WM_QUIT, 0, 0)?;
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    if !ctx.kernel.synthetic_create_sent {
        ctx.kernel.synthetic_create_sent = true;
        write_synthetic_msg(ctx.cpu, lp_msg, WM_CREATE, 0, 0)?;
        ctx.kernel.synthetic_message_count = count + 1;
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let triple = ctx
        .kernel
        .pending_message
        .unwrap_or_else(|| next_message(ctx));
    if remove_mode != 0x0001 {
        ctx.kernel.pending_message = Some(triple);
    }
    write_synthetic_msg(ctx.cpu, lp_msg, triple.0, triple.1, triple.2)?;
    if remove_mode != 0x0001 {
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    ctx.kernel.synthetic_message_count += 1;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn post_quit_message(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let exit_code = ctx.arg_u32(0)?;
    ctx.kernel.synthetic_message_budget = 1;
    ctx.kernel.synthetic_message_count = 1;
    log::info!("PostQuitMessage({exit_code}) queued as WM_QUIT");
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `DWORD MsgWaitForMultipleObjectsEx(DWORD nCount, const HANDLE *,
/// DWORD dwMilliseconds, DWORD dwWakeMask, DWORD dwFlags)`. Real
/// Win32 returns `WAIT_OBJECT_0 + nCount` when "a new input event is
/// in the queue". Since our synthetic message pump always has more
/// messages until the budget is exhausted (and `WM_QUIT` then breaks
/// the loop), telling the guest "input ready" lets it fall through
/// to its `PeekMessageW` / `GetMessageW` loop normally.
fn msg_wait_for_multiple_objects(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n_count = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(n_count))
}

fn wait_for_multiple_objects(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let count = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(count))
}

fn get_system_metrics(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ctx.arg_u32(0)?;
    // SM_CXSCREEN=0 / SM_CYSCREEN=1 — return Pocket PC defaults so
    // the game's framebuffer math works.
    let v = match n {
        0 => 240,
        1 => 320,
        _ => 0,
    };
    Ok(DispatchOutcome::ReturnedR0(v))
}

// ---------- GDI ----------

fn get_dc(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(GDI_SCREEN_DC))
}

fn begin_paint(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // BeginPaint(hwnd, lpPaint) -> HDC. Fill the PAINTSTRUCT enough
    // for the caller (most games only read .hdc / .rcPaint).
    let _hwnd = ctx.arg_u32(0)?;
    let lp_paint = ctx.arg_u32(1)?;
    if lp_paint != 0 {
        let mut buf = [0u8; PAINTSTRUCT_BYTES as usize];
        // hdc
        buf[0..4].copy_from_slice(&GDI_SCREEN_DC.to_le_bytes());
        // fErase = 1
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        // rcPaint = (0,0, FB_WIDTH, FB_HEIGHT)
        buf[8..12].copy_from_slice(&0u32.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        buf[16..20].copy_from_slice(&FB_WIDTH.to_le_bytes());
        buf[20..24].copy_from_slice(&FB_HEIGHT.to_le_bytes());
        ctx.cpu.write_mem(lp_paint, &buf)?;
    }
    Ok(DispatchOutcome::ReturnedR0(GDI_SCREEN_DC))
}

fn end_paint(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ctx.kernel.framebuffer.mark_dirty();
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn create_compatible_dc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.kernel.gdi.create_memory_dc();
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn create_compatible_bitmap(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hdc = ctx.arg_u32(0)?;
    let w = ctx.arg_u32(1)?;
    let h = ctx.arg_u32(2)?;
    let handle = ctx.kernel.gdi.create_compatible_bitmap(w, h);
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn create_solid_brush(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let color = ctx.arg_u32(0)?;
    let h = ctx.kernel.gdi.create_solid_brush(color);
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn create_pen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _style = ctx.arg_u32(0)?;
    let width = ctx.arg_u32(1)?;
    let color = ctx.arg_u32(2)?;
    let h = ctx.kernel.gdi.create_pen(color, width);
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn create_font_indirect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // We ignore the LOGFONT contents; just allocate a font handle so
    // the caller can SelectObject it. Default height 0 is fine.
    let h = ctx.kernel.gdi.create_font(0);
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn get_stock_object(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Map stock indices to our pre-registered handles.
    let idx = ctx.arg_u32(0)?;
    let h = match idx {
        0 => STOCK_WHITE_BRUSH, // WHITE_BRUSH
        1 => 0xDEAD_5702,       // LTGRAY_BRUSH (synthetic)
        2 => 0xDEAD_5703,       // GRAY_BRUSH
        4 => STOCK_BLACK_BRUSH, // BLACK_BRUSH
        5 => STOCK_NULL_BRUSH,  // NULL_BRUSH / HOLLOW_BRUSH
        6 => STOCK_WHITE_PEN,   // WHITE_PEN
        7 => STOCK_BLACK_PEN,   // BLACK_PEN
        8 => STOCK_NULL_PEN,    // NULL_PEN
        17 => 0xDEAD_5710,      // DEFAULT_GUI_FONT
        _ => STOCK_WHITE_BRUSH,
    };
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn select_object(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dc = ctx.arg_u32(0)?;
    let obj = ctx.arg_u32(1)?;
    let prev = ctx.kernel.gdi.select_into(dc, obj);
    Ok(DispatchOutcome::ReturnedR0(prev))
}

fn delete_object(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let _ = ctx.kernel.gdi.delete(h);
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn set_bk_mode(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dc = ctx.arg_u32(0)?;
    let mode = ctx.arg_u32(1)?;
    if let Some(d) = ctx.kernel.gdi.dc_mut(dc) {
        d.bk_transparent = mode == 1; // TRANSPARENT
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn set_bk_color(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dc = ctx.arg_u32(0)?;
    let color = ctx.arg_u32(1)?;
    if let Some(d) = ctx.kernel.gdi.dc_mut(dc) {
        d.bk_color = color;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn set_text_color(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dc = ctx.arg_u32(0)?;
    let color = ctx.arg_u32(1)?;
    if let Some(d) = ctx.kernel.gdi.dc_mut(dc) {
        d.text_color = color;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// Borrow either the framebuffer or a memory bitmap as a writable
/// surface, given a DC handle.
fn surface_for_dc<'a>(state: &'a mut pocket_kernel::KernelState, dc: u32) -> Option<Surface<'a>> {
    let dc_meta = state.gdi.dc(dc)?.clone();
    match dc_meta.surface {
        pocket_kernel::gdi::DcSurface::Screen => Some(Surface::Screen(&mut state.framebuffer)),
        pocket_kernel::gdi::DcSurface::Memory => {
            let bm = dc_meta.selected_bitmap?;
            state.gdi.bitmap_mut(bm).map(Surface::Bitmap)
        }
    }
}

fn fill_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // FillRect(hdc, lprc, hbr): fill rectangle with brush colour.
    let hdc = ctx.arg_u32(0)?;
    let rc_ptr = ctx.arg_u32(1)?;
    let hbr = ctx.arg_u32(2)?;
    let rc = ctx.cpu.read_mem(rc_ptr, 16)?;
    let l = i32::from_le_bytes([rc[0], rc[1], rc[2], rc[3]]);
    let t = i32::from_le_bytes([rc[4], rc[5], rc[6], rc[7]]);
    let r = i32::from_le_bytes([rc[8], rc[9], rc[10], rc[11]]);
    let b = i32::from_le_bytes([rc[12], rc[13], rc[14], rc[15]]);
    let color = ctx
        .kernel
        .gdi
        .brush(hbr)
        .map(|b| b.color)
        .unwrap_or(0x00ff_ffff);
    let rgb = colorref_to_rgb565(color);
    if let Some(mut surf) = surface_for_dc(ctx.kernel, hdc) {
        surf.fill_rect(l, t, r - l, b - t, rgb);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn rectangle(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Rectangle(hdc, l, t, r, b)
    let hdc = ctx.arg_u32(0)?;
    let l = ctx.arg_u32(1)? as i32;
    let t = ctx.arg_u32(2)? as i32;
    let r = ctx.arg_u32(3)? as i32;
    let b = ctx.arg_u32(4)? as i32;
    let dc_meta = ctx
        .kernel
        .gdi
        .dc(hdc)
        .cloned()
        .ok_or_else(|| KernelError::Dispatch(format!("Rectangle: bad HDC 0x{hdc:08x}")))?;
    let fill_rgb = colorref_to_rgb565(dc_meta.brush_color);
    let stroke_rgb = colorref_to_rgb565(dc_meta.pen_color);
    if let Some(mut surf) = surface_for_dc(ctx.kernel, hdc) {
        surf.fill_rect(l, t, r - l, b - t, fill_rgb);
        surf.stroke_rect(l, t, r - l, b - t, stroke_rgb);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn bit_blt(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // BitBlt(hdcDest, x, y, cx, cy, hdcSrc, x1, y1, rop) → BOOL.
    let hdc_dst = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(1)? as i32;
    let y = ctx.arg_u32(2)? as i32;
    let cx = ctx.arg_u32(3)? as i32;
    let cy = ctx.arg_u32(4)? as i32;
    let hdc_src = ctx.arg_u32(5)?;
    let x1 = ctx.arg_u32(6)? as i32;
    let y1 = ctx.arg_u32(7)? as i32;
    let rop = ctx.arg_u32(8)?;
    log::debug!(
        "BitBlt(dst=0x{hdc_dst:08x} dst=({x},{y},{cx}x{cy}) src=0x{hdc_src:08x} src=({x1},{y1}) rop=0x{rop:08x})"
    );
    bit_blt_inner(ctx, hdc_dst, x, y, cx, cy, hdc_src, x1, y1)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// Decode an RGB565 pixel into 8-bit per channel using bit-replication
/// shifts. Equivalent in result to `r * 255 / 31` etc., but a few
/// times faster because the compiler can fold this into a couple of
/// shifts and ORs and avoid the integer divide.
#[inline]
fn rgb565_to_888(px: u16) -> (u8, u8, u8) {
    let r5 = ((px >> 11) & 0x1f) as u8;
    let g6 = ((px >> 5) & 0x3f) as u8;
    let b5 = (px & 0x1f) as u8;
    let r = (r5 << 3) | (r5 >> 2);
    let g = (g6 << 2) | (g6 >> 4);
    let b = (b5 << 3) | (b5 >> 2);
    (r, g, b)
}

/// Read a DIB-backed bitmap's current pixels from guest memory and
/// convert them to RGB565. This makes writes the guest performed
/// directly through `ppvBits` (after `CreateDIBSection`) visible to
/// the rendering pipeline.
///
/// Fills the supplied scratch buffer (`out`) with `width * height * 2`
/// bytes of RGB565 pixels and reuses `raw_scratch` for the row read
/// from the guest. Both buffers persist across calls in
/// [`pocket_kernel::KernelState`] so a chatty BitBlt loop doesn't
/// allocate a fresh `Vec<u8>` per call.
fn snapshot_dib_into(
    cpu: &mut dyn pocket_cpu::Cpu,
    bm: &pocket_kernel::gdi::Bitmap,
    raw_scratch: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> bool {
    let Some(bits_va) = bm.dib_bits_va else {
        return false;
    };
    let raw_len = (bm.dib_row_stride * bm.height) as usize;
    if raw_scratch.len() != raw_len {
        raw_scratch.resize(raw_len, 0);
    }
    if cpu.read_mem_into(bits_va, raw_scratch).is_err() {
        return false;
    }
    let out_len = (bm.width * bm.height * 2) as usize;
    if out.len() != out_len {
        out.resize(out_len, 0);
    }
    let row_bytes = (bm.width * 2) as usize;
    let stride = bm.dib_row_stride as usize;
    let raw = raw_scratch.as_slice();

    // Fast path: 16 bpp top-down DIBs with a row stride that already
    // matches our internal RGB565 layout collapse to a single
    // `copy_from_slice`. This is the common case for sprites the
    // game blits via `CreateDIBSection`.
    if bm.bpp == 16 {
        for src_y in 0..bm.height {
            let dst_y = if bm.dib_bottom_up {
                bm.height - 1 - src_y
            } else {
                src_y
            };
            let row_off = (src_y as usize) * stride;
            let dst_row = (dst_y as usize) * row_bytes;
            if row_off + row_bytes > raw.len() || dst_row + row_bytes > out.len() {
                continue;
            }
            out[dst_row..dst_row + row_bytes].copy_from_slice(&raw[row_off..row_off + row_bytes]);
        }
        return true;
    }

    for src_y in 0..bm.height {
        let dst_y = if bm.dib_bottom_up {
            bm.height - 1 - src_y
        } else {
            src_y
        };
        let row_off = (src_y as usize) * stride;
        let dst_row = (dst_y as usize) * row_bytes;
        for x in 0..bm.width {
            let rgb = match bm.bpp {
                8 => {
                    let idx = raw[row_off + x as usize] as usize;
                    *bm.dib_palette.get(idx).unwrap_or(&0)
                }
                4 => {
                    let b = raw[row_off + (x as usize) / 2];
                    let nib = if x & 1 == 0 { b >> 4 } else { b & 0x0F };
                    *bm.dib_palette.get(nib as usize).unwrap_or(&0)
                }
                1 => {
                    let b = raw[row_off + (x as usize) / 8];
                    let bit = 7 - (x & 7);
                    let v = ((b >> bit) & 1) as usize;
                    *bm.dib_palette.get(v).unwrap_or(&0)
                }
                24 => pocket_kernel::framebuffer::pack_rgb565(
                    raw[row_off + x as usize * 3 + 2],
                    raw[row_off + x as usize * 3 + 1],
                    raw[row_off + x as usize * 3],
                ),
                32 => pocket_kernel::framebuffer::pack_rgb565(
                    raw[row_off + x as usize * 4 + 2],
                    raw[row_off + x as usize * 4 + 1],
                    raw[row_off + x as usize * 4],
                ),
                _ => 0,
            };
            let off = dst_row + (x as usize) * 2;
            out[off] = rgb as u8;
            out[off + 1] = (rgb >> 8) as u8;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn bit_blt_inner(
    ctx: &mut CallCtx<'_>,
    hdc_dst: u32,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    hdc_src: u32,
    x1: i32,
    y1: i32,
) -> Result<(), KernelError> {
    // Materialise the source pixels into a kernel-level scratch
    // `Vec<u8>` instead of cloning the full source surface every
    // call. Derby is a particularly egregious case: the previous
    // implementation cloned the entire 153 KiB framebuffer on every
    // screen->memory blit, churning megabytes per frame through the
    // allocator. Reusing one buffer across the whole run amortises
    // away that allocation pressure.
    let mut scratch = std::mem::take(&mut ctx.kernel.bit_blt_src_scratch);
    let mut decode_scratch = std::mem::take(&mut ctx.kernel.dib_decode_scratch);

    let (src_w, src_h, ok) = read_blit_source(ctx, hdc_src, &mut scratch, &mut decode_scratch);

    if ok && src_w != 0 && src_h != 0 {
        if let Some(mut dst) = surface_for_dc(ctx.kernel, hdc_dst) {
            dst.blit_from_bytes(x, y, x1, y1, cx, cy, &scratch, src_w, src_h);
        }
    }

    // Hand the scratch buffers back so the next BitBlt reuses them.
    ctx.kernel.bit_blt_src_scratch = scratch;
    ctx.kernel.dib_decode_scratch = decode_scratch;

    sync_dst_dib_to_guest(ctx, hdc_dst)?;
    Ok(())
}

/// Resolve the source pixels of a BitBlt into `scratch` (RGB565
/// little-endian, top-down, stride = `width * 2`). Returns
/// `(width, height, ok)`. `decode_scratch` is used internally as
/// the raw guest read buffer when the source is a DIB-backed bitmap.
fn read_blit_source(
    ctx: &mut CallCtx<'_>,
    hdc_src: u32,
    scratch: &mut Vec<u8>,
    decode_scratch: &mut Vec<u8>,
) -> (u32, u32, bool) {
    let dc = match ctx.kernel.gdi.dc(hdc_src).cloned() {
        Some(d) => d,
        None => {
            scratch.clear();
            return (0, 0, false);
        }
    };
    match dc.surface {
        pocket_kernel::gdi::DcSurface::Screen => {
            let fb = &ctx.kernel.framebuffer;
            let needed = fb.pixels.len();
            if scratch.len() != needed {
                scratch.resize(needed, 0);
            }
            scratch.copy_from_slice(&fb.pixels);
            (fb.width, fb.height, true)
        }
        pocket_kernel::gdi::DcSurface::Memory => match dc.selected_bitmap {
            Some(bh) => {
                // First decide whether we have to pull pixels from
                // the guest's DIB section (the host-side `pixels`
                // cache may be out of date if the guest wrote
                // through `ppvBits`).
                let dib_meta = ctx
                    .kernel
                    .gdi
                    .bitmap(bh)
                    .filter(|b| b.dib_bits_va.is_some())
                    .cloned();
                if let Some(bm) = dib_meta {
                    if snapshot_dib_into(ctx.cpu, &bm, decode_scratch, scratch) {
                        return (bm.width, bm.height, true);
                    }
                    // Fall back to the host cache if the guest read
                    // failed for any reason.
                    let needed = bm.pixels.len();
                    if scratch.len() != needed {
                        scratch.resize(needed, 0);
                    }
                    scratch.copy_from_slice(&bm.pixels);
                    (bm.width, bm.height, true)
                } else {
                    match ctx.kernel.gdi.bitmap(bh) {
                        Some(b) => {
                            let needed = b.pixels.len();
                            if scratch.len() != needed {
                                scratch.resize(needed, 0);
                            }
                            scratch.copy_from_slice(&b.pixels);
                            (b.width, b.height, true)
                        }
                        None => {
                            scratch.clear();
                            (0, 0, false)
                        }
                    }
                }
            }
            None => {
                scratch.clear();
                (0, 0, false)
            }
        },
    }
}

/// Pocket PC games frequently `BitBlt` an asset into a `CreateDIBSection`
/// memory DC and then read the pixels out by dereferencing the
/// `ppvBits` pointer the section reported. Our drawing primitives keep
/// the canonical pixels in a host-side RGB565 cache (`Bitmap::pixels`),
/// so without an explicit flush the guest's pointer would still point
/// at zero-initialized memory and every subsequent direct-pixel read
/// (e.g. the splash-screen blit-to-FB seen in JumpyBall) would silently
/// produce a black frame.
///
/// After every operation that mutates a DC, call this to push the host
/// pixels back into the guest VA at `dib_bits_va` in the DIB's native
/// bit depth and orientation.
fn sync_dst_dib_to_guest(ctx: &mut CallCtx<'_>, hdc: u32) -> Result<(), KernelError> {
    let dc = match ctx.kernel.gdi.dc(hdc).cloned() {
        Some(dc) => dc,
        None => return Ok(()),
    };
    if !matches!(dc.surface, pocket_kernel::gdi::DcSurface::Memory) {
        return Ok(());
    }
    let bm_h = match dc.selected_bitmap {
        Some(h) => h,
        None => return Ok(()),
    };
    // Skip the encode + write_mem entirely when the host pixels
    // haven't been touched since the previous sync. Pocket Derby
    // and most other GDI-driven games hit this path: the same
    // memory DC is selected for back-to-back BitBlt sources where
    // the bitmap itself only changes once every few frames.
    let (bits_va, w, h, bpp, stride, bottom_up, palette_empty) = {
        let bm = match ctx.kernel.gdi.bitmap_mut(bm_h) {
            Some(b) => b,
            None => return Ok(()),
        };
        if !bm.host_dirty {
            return Ok(());
        }
        let Some(va) = bm.dib_bits_va else {
            // No mapped guest memory to push to \u2014 still clear the
            // dirty bit so we don't keep retrying every BitBlt.
            bm.host_dirty = false;
            return Ok(());
        };
        // Optimistically clear the dirty bit; the encode below is
        // the host -> guest sync that satisfies it.
        bm.host_dirty = false;
        (
            va,
            bm.width,
            bm.height,
            bm.bpp,
            bm.dib_row_stride,
            bm.dib_bottom_up,
            bm.dib_palette.is_empty(),
        )
    };

    // Fast path: 16 bpp top-down DIB with stride matching our
    // native row layout collapses to a single `write_mem`. Most
    // memory back-buffers fall into this case.
    if bpp == 16 && stride == w * 2 && !bottom_up {
        let bm = match ctx.kernel.gdi.bitmap(bm_h) {
            Some(b) => b,
            None => return Ok(()),
        };
        ctx.cpu.write_mem(bits_va, &bm.pixels)?;
        return Ok(());
    }

    // General path. We re-fetch the bitmap by reference here so we
    // don't have to clone its `pixels` (~150 KiB for the Derby
    // back-buffer) every BitBlt.
    let mut buf = std::mem::take(&mut ctx.kernel.dib_sync_scratch);
    let buf_len = (stride * h) as usize;
    if buf.len() != buf_len {
        buf.resize(buf_len, 0);
    }

    {
        let bm = match ctx.kernel.gdi.bitmap(bm_h) {
            Some(b) => b,
            None => {
                ctx.kernel.dib_sync_scratch = buf;
                return Ok(());
            }
        };
        encode_pixels_to_dib(bm, &mut buf);
    }
    let _ = palette_empty;

    ctx.cpu.write_mem(bits_va, &buf)?;
    ctx.kernel.dib_sync_scratch = buf;
    Ok(())
}

/// Encode `bm.pixels` (RGB565 little-endian, top-down) into the DIB
/// pixel layout described by `bm.dib_*` fields and write the result
/// into `buf`. Caller is responsible for sizing `buf` to
/// `bm.dib_row_stride * bm.height` and for actually writing the
/// result back to guest memory.
fn encode_pixels_to_dib(bm: &pocket_kernel::gdi::Bitmap, buf: &mut [u8]) {
    let stride = bm.dib_row_stride as usize;
    for src_y in 0..bm.height {
        let dst_y = if bm.dib_bottom_up {
            bm.height - 1 - src_y
        } else {
            src_y
        };
        let src_row = (src_y * bm.width * 2) as usize;
        let dst_row = (dst_y as usize) * stride;
        match bm.bpp {
            16 => {
                let row_bytes = (bm.width * 2) as usize;
                if src_row + row_bytes > bm.pixels.len() || dst_row + row_bytes > buf.len() {
                    continue;
                }
                buf[dst_row..dst_row + row_bytes]
                    .copy_from_slice(&bm.pixels[src_row..src_row + row_bytes]);
            }
            24 => {
                for x in 0..bm.width {
                    let off = src_row + (x as usize) * 2;
                    if off + 1 >= bm.pixels.len() {
                        continue;
                    }
                    let px = u16::from_le_bytes([bm.pixels[off], bm.pixels[off + 1]]);
                    let (r, g, b) = rgb565_to_888(px);
                    let off2 = dst_row + (x as usize) * 3;
                    if off2 + 2 < buf.len() {
                        buf[off2] = b;
                        buf[off2 + 1] = g;
                        buf[off2 + 2] = r;
                    }
                }
            }
            32 => {
                for x in 0..bm.width {
                    let off = src_row + (x as usize) * 2;
                    if off + 1 >= bm.pixels.len() {
                        continue;
                    }
                    let px = u16::from_le_bytes([bm.pixels[off], bm.pixels[off + 1]]);
                    let (r, g, b) = rgb565_to_888(px);
                    let off2 = dst_row + (x as usize) * 4;
                    if off2 + 3 < buf.len() {
                        buf[off2] = b;
                        buf[off2 + 1] = g;
                        buf[off2 + 2] = r;
                        buf[off2 + 3] = 0;
                    }
                }
            }
            8 if !bm.dib_palette.is_empty() => {
                for x in 0..bm.width {
                    let off = src_row + (x as usize) * 2;
                    if off + 1 >= bm.pixels.len() {
                        continue;
                    }
                    let px = u16::from_le_bytes([bm.pixels[off], bm.pixels[off + 1]]);
                    let (r, g, b) = rgb565_to_888(px);
                    let mut best_i = 0u8;
                    let mut best_d = u32::MAX;
                    for (i, &p) in bm.dib_palette.iter().enumerate() {
                        let (pr, pg, pb) = rgb565_to_888(p);
                        let dr = pr.abs_diff(r) as u32;
                        let dg = pg.abs_diff(g) as u32;
                        let db = pb.abs_diff(b) as u32;
                        let d = dr * dr + dg * dg + db * db;
                        if d < best_d {
                            best_d = d;
                            best_i = i as u8;
                        }
                    }
                    let off2 = dst_row + x as usize;
                    if off2 < buf.len() {
                        buf[off2] = best_i;
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------- Resources ----------

fn read_wide_resource_key(ctx: &mut CallCtx<'_>, raw: u32) -> Result<ResourceKey, KernelError> {
    if raw < 0x1_0000 {
        // MAKEINTRESOURCE encoding — low 16 bits are an integer ID.
        Ok(ResourceKey::Id(raw))
    } else {
        let mut name = String::new();
        let mut va = raw;
        for _ in 0..256 {
            let b = ctx.cpu.read_mem(va, 2)?;
            let cu = u16::from_le_bytes([b[0], b[1]]);
            if cu == 0 {
                break;
            }
            if let Some(c) = char::from_u32(cu as u32) {
                name.push(c);
            }
            va = va.wrapping_add(2);
        }
        Ok(ResourceKey::Name(name))
    }
}

fn find_resource_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // FindResourceW(hModule, lpName, lpType)
    let _hmod = ctx.arg_u32(0)?;
    let name_raw = ctx.arg_u32(1)?;
    let type_raw = ctx.arg_u32(2)?;
    let want_name = read_wide_resource_key(ctx, name_raw)?;
    let want_type = read_wide_resource_key(ctx, type_raw)?;
    if let Some(entry) = ctx
        .kernel
        .resources
        .iter()
        .find(|e| e.ty == want_type && e.name == want_name)
    {
        let va = ctx.kernel.image_base.wrapping_add(entry.data_rva);
        log::trace!(
            "FindResourceW(name={want_name:?}, type={want_type:?}) -> 0x{va:08x} ({} bytes)",
            entry.size
        );
        return Ok(DispatchOutcome::ReturnedR0(va));
    }
    log::trace!("FindResourceW(name={want_name:?}, type={want_type:?}) -> NULL");
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn load_resource(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // LoadResource just returns the same handle on Windows when the
    // resource is in-image. We've already encoded the data VA in the
    // FindResource result.
    let h = ctx.arg_u32(1)?;
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn lock_resource(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn sizeof_resource(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // SizeofResource(hModule, hResInfo) — hResInfo is the VA we
    // returned from FindResourceW. We look up by data_rva.
    let h = ctx.arg_u32(1)?;
    let rva = h.wrapping_sub(ctx.kernel.image_base);
    if let Some(e) = ctx.kernel.resources.iter().find(|e| e.data_rva == rva) {
        return Ok(DispatchOutcome::ReturnedR0(e.size));
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `HBITMAP LoadBitmapW(HINSTANCE hInstance, LPCWSTR lpBitmapName)` —
/// look the bitmap up in the PE's embedded resources, decode the
/// BITMAPINFO header + palette + pixel data into our internal RGB565
/// `Bitmap`, register it with the GDI state, and return the handle.
///
/// Pocket PC games typically ship 8-bpp paletted DIBs to save space;
/// we also handle 24-bpp BGR and 16-bpp RGB565/RGB555.
fn load_bitmap_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const RT_BITMAP: ResourceKey = ResourceKey::Id(2);
    let _hinst = ctx.arg_u32(0)?;
    let name_raw = ctx.arg_u32(1)?;
    let want_name = read_wide_resource_key(ctx, name_raw)?;
    let entry = match ctx
        .kernel
        .resources
        .iter()
        .find(|e| e.ty == RT_BITMAP && e.name == want_name)
        .cloned()
    {
        Some(e) => e,
        None => {
            log::trace!("LoadBitmapW(name={want_name:?}) -> NULL (resource not found)");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    // Read the bitmap data straight out of the guest's mapped image.
    let va = ctx.kernel.image_base.wrapping_add(entry.data_rva);
    let raw = match ctx.cpu.read_mem(va, entry.size) {
        Ok(b) => b,
        Err(_) => {
            log::trace!("LoadBitmapW({want_name:?}) -> NULL (image not mapped at 0x{va:08x})");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    let pixels_565 = match decode_dib_to_rgb565(&raw) {
        Some(p) => p,
        None => {
            log::trace!("LoadBitmapW({want_name:?}) -> NULL (unsupported DIB)");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    let (w, h) = pixels_565.dims;
    let handle = ctx.kernel.gdi.create_compatible_bitmap(w, h);
    if let Some(b) = ctx.kernel.gdi.bitmap_mut(handle) {
        // Bitmap::new pre-allocates `w*h*2` bytes; just blit our
        // already-RGB565-converted image on top.
        debug_assert_eq!(b.pixels.len(), pixels_565.bytes.len());
        b.pixels.copy_from_slice(&pixels_565.bytes);
    }
    log::trace!(
        "LoadBitmapW(name={want_name:?}) -> handle 0x{handle:08x} ({}x{} from {} bytes)",
        w,
        h,
        entry.size
    );
    Ok(DispatchOutcome::ReturnedR0(handle))
}

struct DecodedDib {
    bytes: Vec<u8>,
    dims: (u32, u32),
}

/// Decode a Windows DIB (`BITMAPINFOHEADER` + palette + pixels) into
/// a top-down RGB565 little-endian buffer of size `w*h*2`. Returns
/// `None` if the format is not yet implemented.
fn decode_dib_to_rgb565(raw: &[u8]) -> Option<DecodedDib> {
    if raw.len() < 40 {
        return None;
    }
    let header_size = u32::from_le_bytes(raw[0..4].try_into().ok()?);
    if header_size < 40 {
        return None;
    }
    let width = i32::from_le_bytes(raw[4..8].try_into().ok()?);
    let height_raw = i32::from_le_bytes(raw[8..12].try_into().ok()?);
    let _planes = u16::from_le_bytes(raw[12..14].try_into().ok()?);
    let bpp = u16::from_le_bytes(raw[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(raw[16..20].try_into().ok()?);
    let used_colors = u32::from_le_bytes(raw[32..36].try_into().ok()?);
    if width <= 0 || height_raw == 0 || compression != 0 {
        return None;
    }
    let bottom_up = height_raw > 0;
    let height = height_raw.unsigned_abs();
    let width = width as u32;

    // Palette table sits right after the header. For paletted
    // formats the table size is `used_colors` (or 2^bpp if zero).
    let palette_entries = match bpp {
        1 | 4 | 8 => {
            if used_colors == 0 {
                1u32 << bpp
            } else {
                used_colors
            }
        }
        _ => 0,
    };
    let palette_off = header_size as usize;
    let pixels_off = palette_off + (palette_entries as usize) * 4;
    if pixels_off > raw.len() {
        return None;
    }
    // Palette is BGRX in DIB order.
    let mut palette = vec![0u16; palette_entries as usize];
    for (i, slot) in palette.iter_mut().enumerate() {
        let p = palette_off + i * 4;
        *slot = bgrx_to_rgb565(raw[p], raw[p + 1], raw[p + 2]);
    }

    // Each row is padded to a 4-byte boundary.
    let row_bytes = match bpp {
        1 => width.div_ceil(8),
        4 => width.div_ceil(2),
        8 => width,
        16 => width * 2,
        24 => width * 3,
        32 => width * 4,
        _ => return None,
    };
    let row_stride = (row_bytes + 3) & !3;

    let mut out = vec![0u8; (width as usize) * (height as usize) * 2];
    for src_y in 0..height {
        // BMP rows are bottom-up unless the height field is negative.
        let dst_y = if bottom_up { height - 1 - src_y } else { src_y };
        let row_off = pixels_off + (src_y as usize) * (row_stride as usize);
        if row_off + row_bytes as usize > raw.len() {
            return None;
        }
        let dst_row_start = (dst_y as usize) * (width as usize) * 2;
        for x in 0..width {
            let rgb565 = match bpp {
                8 => {
                    let idx = raw[row_off + x as usize] as usize;
                    *palette.get(idx).unwrap_or(&0)
                }
                4 => {
                    let b = raw[row_off + (x as usize) / 2];
                    let nib = if x & 1 == 0 { b >> 4 } else { b & 0x0F };
                    *palette.get(nib as usize).unwrap_or(&0)
                }
                1 => {
                    let b = raw[row_off + (x as usize) / 8];
                    let bit = 7 - (x & 7);
                    let v = ((b >> bit) & 1) as usize;
                    *palette.get(v).unwrap_or(&0)
                }
                16 => u16::from_le_bytes([
                    raw[row_off + x as usize * 2],
                    raw[row_off + x as usize * 2 + 1],
                ]),
                24 => bgrx_to_rgb565(
                    raw[row_off + x as usize * 3],
                    raw[row_off + x as usize * 3 + 1],
                    raw[row_off + x as usize * 3 + 2],
                ),
                32 => bgrx_to_rgb565(
                    raw[row_off + x as usize * 4],
                    raw[row_off + x as usize * 4 + 1],
                    raw[row_off + x as usize * 4 + 2],
                ),
                _ => 0,
            };
            let off = dst_row_start + (x as usize) * 2;
            out[off] = rgb565 as u8;
            out[off + 1] = (rgb565 >> 8) as u8;
        }
    }
    Some(DecodedDib {
        bytes: out,
        dims: (width, height),
    })
}

/// 24-bit BGR → 16-bit RGB565.
fn bgrx_to_rgb565(b: u8, g: u8, r: u8) -> u16 {
    let r5 = (r as u16 >> 3) & 0x1F;
    let g6 = (g as u16 >> 2) & 0x3F;
    let b5 = (b as u16 >> 3) & 0x1F;
    (r5 << 11) | (g6 << 5) | b5
}

/// `int LoadStringW(HINSTANCE hInst, UINT uID, LPWSTR lpBuf, int cch)` —
/// look up the string in the PE's `RT_STRING` (type 6) resource.
/// Resource strings are bundled in blocks of 16; block id is
/// `(uID >> 4) + 1`, sub-index is `uID & 0xF`. Each block is a
/// stream of `(WORD len, wchar_t[len])` records, optionally padded.
///
/// Returns the number of wide chars copied (excluding the trailing
/// NUL); writes a NUL into `lpBuf[0]` and returns 0 if not found.
fn load_string_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const RT_STRING: ResourceKey = ResourceKey::Id(6);
    let _hinst = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)? & 0xFFFF;
    let buf = ctx.arg_u32(2)?;
    let cch = ctx.arg_u32(3)? as usize;

    let block_id = (id >> 4) + 1;
    let sub = (id & 0xF) as usize;
    let mut wide: Vec<u16> = Vec::new();
    if let Some(entry) = ctx
        .kernel
        .resources
        .iter()
        .find(|e| e.ty == RT_STRING && e.name == ResourceKey::Id(block_id))
        .cloned()
    {
        let va = ctx.kernel.image_base.wrapping_add(entry.data_rva);
        if let Ok(bytes) = ctx.cpu.read_mem(va, entry.size) {
            // Walk the 16 length-prefixed records.
            let mut pos = 0usize;
            for i in 0..=sub {
                if pos + 2 > bytes.len() {
                    break;
                }
                let len = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
                pos += 2;
                if i == sub {
                    let end = (pos + len * 2).min(bytes.len());
                    for w in (pos..end).step_by(2) {
                        wide.push(u16::from_le_bytes([bytes[w], bytes[w + 1]]));
                    }
                    break;
                }
                pos += len * 2;
            }
        }
    }

    if buf != 0 && cch > 0 {
        // Always at least NUL-terminate so the caller's buffer is
        // safe even when the string is missing or truncated.
        let copy = wide.len().min(cch.saturating_sub(1));
        let mut out = Vec::with_capacity((copy + 1) * 2);
        for &w in &wide[..copy] {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        ctx.cpu.write_mem(buf, &out)?;
        log::trace!(
            "LoadStringW(id={id}) -> {} chars from block {}",
            copy,
            block_id
        );
        return Ok(DispatchOutcome::ReturnedR0(copy as u32));
    }
    Ok(DispatchOutcome::ReturnedR0(wide.len() as u32))
}

/// `int GetObjectW(HGDIOBJ h, int cb, LPVOID p)` — write a `BITMAP`
/// struct (24 bytes on Windows CE) describing the selected bitmap so
/// that the game can compute the right dimensions before issuing a
/// matching `BitBlt` / `CreateDIBSection`. We only support the bitmap
/// flavour for now; everything else is no-op.
fn get_object_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let cb = ctx.arg_u32(1)?;
    let p = ctx.arg_u32(2)?;
    let (w, ht) = match ctx.kernel.gdi.bitmap(h) {
        Some(b) => (b.width, b.height),
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    if p == 0 {
        // Caller is asking for the size only.
        return Ok(DispatchOutcome::ReturnedR0(24));
    }
    if cb < 24 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    // BITMAP layout: bmType(LONG), bmWidth(LONG), bmHeight(LONG),
    //                bmWidthBytes(LONG), bmPlanes(WORD), bmBitsPixel(WORD),
    //                bmBits(LPVOID).
    let mut buf = [0u8; 24];
    buf[0..4].copy_from_slice(&0u32.to_le_bytes()); // bmType always 0
    buf[4..8].copy_from_slice(&w.to_le_bytes());
    buf[8..12].copy_from_slice(&ht.to_le_bytes());
    buf[12..16].copy_from_slice(&(w * 2).to_le_bytes());
    buf[16..18].copy_from_slice(&1u16.to_le_bytes()); // planes
    buf[18..20].copy_from_slice(&16u16.to_le_bytes()); // RGB565
    buf[20..24].copy_from_slice(&0u32.to_le_bytes()); // bmBits = NULL (managed host-side)
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(24))
}

// ---------- additional window / message handlers ----------

fn destroy_window(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn find_window_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Pocket PC games call FindWindowW on their own class to detect a
    // prior instance of themselves. We always say "no prior instance"
    // so the game proceeds with normal startup.
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `LONG SetWindowLongW(HWND hWnd, int nIndex, LONG dwNewLong)` —
/// returns the previous value (always `0` in our model). When
/// `nIndex == GWL_WNDPROC` (`-4`), we also re-bind the captured
/// guest `WndProc` so the synthetic message pump dispatches to the
/// right entry point if the game subclasses its own window.
fn set_window_long_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let n_index = ctx.arg_u32(1)? as i32;
    let new_long = ctx.arg_u32(2)?;
    if n_index == -4 {
        log::info!("SetWindowLongW(GWL_WNDPROC) re-binding WndProc=0x{new_long:08x}");
        ctx.kernel.wnd_proc = new_long;
    } else if n_index == -21 {
        ctx.kernel.window_user_data = new_long;
        log::debug!("SetWindowLongW(GWL_USERDATA)=0x{new_long:08x}");
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `LONG GetWindowLongW(HWND hWnd, int nIndex)` — return `0` for
/// every slot we don't track (the documented return when never set).
fn get_window_long_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let n_index = ctx.arg_u32(1)? as i32;
    let v = if n_index == -4 {
        ctx.kernel.wnd_proc
    } else if n_index == -21 {
        ctx.kernel.window_user_data
    } else {
        0
    };
    Ok(DispatchOutcome::ReturnedR0(v))
}

/// `BOOL SetWindowTextW(HWND hWnd, LPCWSTR lpString)` — Pocket PC
/// games (e.g. gspot) call this on every score update to refresh the
/// window's title-bar caption. We have no real window manager, so
/// just log the new caption when DEBUG tracing is enabled and report
/// success. Returns TRUE.
fn set_window_text_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    if p != 0 {
        let chars = read_wstr(ctx, p, 256).unwrap_or_default();
        let s: String = chars
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8 as char)
            .collect();
        log::debug!("SetWindowTextW({s:?})");
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL SetWindowTextA(HWND hWnd, LPCSTR lpString)` — ANSI variant.
fn set_window_text_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    if p != 0 {
        let s = read_cstr(ctx, p, 256).unwrap_or_default();
        log::debug!("SetWindowTextA({s:?})");
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `int GetWindowTextW(HWND hWnd, LPWSTR lpString, int nMaxCount)` —
/// we don't track per-window captions, so return an empty string.
fn get_window_text_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    if p != 0 && n > 0 {
        // Write a single NUL terminator (UTF-16 or ANSI both fit in 2 zero bytes).
        let _ = ctx.cpu.write_mem(p, &[0u8, 0u8]);
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `BOOL PlaySoundW(LPCWSTR pszSound, HMODULE hmod, DWORD fdwSound)` —
/// the Pocket PC sound effects helper. We don't have an audio backend
/// yet, so this is a successful no-op. Logging the requested sound
/// asset name is helpful when debugging asset-loading paths.
fn play_sound_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let _hmod = ctx.arg_u32(1)?;
    let flags = ctx.arg_u32(2)?;
    // SND_RESOURCE = 0x00040004 → pszSound is a MAKEINTRESOURCE id.
    // SND_ASYNC = 0x0001, SND_LOOP = 0x0008, etc. We ignore them.
    if p != 0 && (flags & 0x0004) == 0 {
        let chars = read_wstr(ctx, p, 256).unwrap_or_default();
        let s: String = chars
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8 as char)
            .collect();
        log::debug!("PlaySoundW({s:?}, flags=0x{flags:08x}) -> stub OK");
    } else {
        log::debug!("PlaySoundW(0x{p:08x}, flags=0x{flags:08x}) -> stub OK");
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

const OSVERSIONINFOW_BYTES: u32 = 4 + 4 * 4 + 128 * 2;

/// `BOOL GetVersionExW(LPOSVERSIONINFOW lpVersionInformation)`.
/// Reports Windows CE 4.20 (Pocket PC 2003 / PPC2003), which is
/// what every Pocket PC 2002–2003 game we target was built for.
fn get_version_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let header = ctx.cpu.read_mem(p, 4)?;
    let cb = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    // We accept any reasonable `cb` — Pocket PC games sometimes set
    // it to `sizeof(OSVERSIONINFOW)` (276), sometimes to the smaller
    // `OSVERSIONINFOEXW` ANSI shape, and sometimes to 0 (lazy init).
    // Real Windows would reject `cb == 0`, but here we'd rather fill
    // what we can and return success so the guest doesn't take a
    // failure-only code path.
    let want = if cb >= OSVERSIONINFOW_BYTES {
        OSVERSIONINFOW_BYTES
    } else {
        cb.max(20)
    };
    let mut buf = vec![0u8; want as usize];
    buf[0..4].copy_from_slice(&want.to_le_bytes());
    buf[4..8].copy_from_slice(&4u32.to_le_bytes());
    buf[8..12].copy_from_slice(&20u32.to_le_bytes());
    buf[12..16].copy_from_slice(&1081u32.to_le_bytes());
    buf[16..20].copy_from_slice(&3u32.to_le_bytes());
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `DWORD GetVersion()` — packed legacy form. Hi word = major.minor
/// (0x0414 == 4.20), low word = build (1081).
fn get_version(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0439_1404))
}

fn invalidate_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // We don't model dirty rects yet, but bumping the framebuffer
    // dirty counter means hosts (PPM dump, minifb display) re-upload.
    ctx.kernel.framebuffer.mark_dirty();
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn write_rect(ctx: &mut CallCtx<'_>, rect_ptr: u32, w: i32, h: i32) -> Result<(), KernelError> {
    if rect_ptr == 0 {
        return Ok(());
    }
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&0i32.to_le_bytes()); // left
    buf[4..8].copy_from_slice(&0i32.to_le_bytes()); // top
    buf[8..12].copy_from_slice(&w.to_le_bytes()); // right
    buf[12..16].copy_from_slice(&h.to_le_bytes()); // bottom
    ctx.cpu.write_mem(rect_ptr, &buf)?;
    Ok(())
}

fn get_client_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // GetClientRect(hWnd, lpRect) -> BOOL.
    let _hwnd = ctx.arg_u32(0)?;
    let lp_rect = ctx.arg_u32(1)?;
    write_rect(ctx, lp_rect, FB_WIDTH as i32, FB_HEIGHT as i32)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_window_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let lp_rect = ctx.arg_u32(1)?;
    write_rect(ctx, lp_rect, FB_WIDTH as i32, FB_HEIGHT as i32)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

const FAKE_ICON: u32 = 0xDEAD_1C01;
const FAKE_ACCEL: u32 = 0xDEAD_AC01;
const FAKE_TIMER_BASE: u32 = 0xDEAD_7100;

fn load_icon_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_ICON))
}

fn load_accelerators_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_ACCEL))
}

fn dialog_box_indirect_param_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Treat any modal dialog as immediately cancelled. Real games use
    // these for splash / about screens; cancelling is harmless.
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn message_box_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // IDOK = 1
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn set_timer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let id = ctx.arg_u32(1)?;
    let interval = ctx.arg_u32(2)?.max(1);
    let final_id = if id == 0 { FAKE_TIMER_BASE } else { id };
    ctx.kernel.synthetic_timer_id = final_id;
    ctx.kernel.synthetic_timer_interval_ms = interval;
    ctx.kernel.synthetic_timer_next_ms = monotonic_ms().saturating_add(interval as u64);
    Ok(DispatchOutcome::ReturnedR0(final_id))
}

fn create_event_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0xDEAD_E001))
}

fn create_thread(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _security_attributes = ctx.arg_u32(0)?;
    let stack_size = ctx.arg_u32(1)?.max(0x1000);
    let entry = ctx.arg_u32(2)?;
    let parameter = ctx.arg_u32(3)?;
    if entry == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    let thread_index = ctx.kernel.threads.len();
    let stack_top = 0x6200_0000u32.saturating_sub(thread_index as u32 * 0x0010_0000);
    let exit_va = THREAD_EXIT_TRAMPOLINE_BASE.saturating_sub(thread_index as u32 * 0x100);
    let resume_pc = ctx.cpu.read_reg(ArmReg::Lr)?;
    let mut saved_regs = [0u32; 17];
    for (index, reg) in [
        ArmReg::R0,
        ArmReg::R1,
        ArmReg::R2,
        ArmReg::R3,
        ArmReg::R4,
        ArmReg::R5,
        ArmReg::R6,
        ArmReg::R7,
        ArmReg::R8,
        ArmReg::R9,
        ArmReg::R10,
        ArmReg::R11,
        ArmReg::R12,
        ArmReg::Sp,
        ArmReg::Lr,
        ArmReg::Pc,
        ArmReg::Cpsr,
    ]
    .into_iter()
    .enumerate()
    {
        saved_regs[index] = ctx.cpu.read_reg(reg)?;
    }
    saved_regs[15] = resume_pc;
    let handle = 0xDEAD_7C00u32.saturating_add(thread_index as u32);
    let stack_size = stack_size.min(0x100000);
    let stack_base = stack_top.saturating_sub(stack_size) & !0xfff;
    ctx.cpu.map_region(
        stack_base,
        pocket_cpu::round_up_to_page(stack_size),
        pocket_cpu::Prot::READ | pocket_cpu::Prot::WRITE,
    )?;
    let thread = GuestThread::new(
        entry, parameter, stack_top, stack_size, exit_va, resume_pc, handle, saved_regs,
    );
    ctx.kernel.threads.push(thread);
    ctx.kernel.current_thread = thread_index + 1;
    ctx.cpu.add_code_hook(exit_va)?;
    ctx.cpu.write_reg(ArmReg::R0, parameter)?;
    ctx.cpu.write_reg(ArmReg::Sp, stack_top - 16)?;
    ctx.cpu.write_reg(ArmReg::Lr, exit_va)?;
    log::debug!(
        "CreateThread entry=0x{entry:08x} parameter=0x{parameter:08x} stack={} -> handle=0x{handle:08x}",
        stack_size,
    );
    Ok(DispatchOutcome::JumpTo(entry))
}

fn get_current_thread_id(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

// ---------- additional GDI handlers ----------

/// `HBITMAP CreateDIBSection(HDC hdc, const BITMAPINFO *pbmi,
///   UINT usage, void **ppvBits, HANDLE hSection, DWORD dwOffset)`
///
/// We allocate guest-visible memory for the pixel buffer, write the
/// pointer back through `ppvBits`, and register a [`Bitmap`] whose
/// pixel storage lives at that VA. Subsequent `BitBlt` reads are
/// served by re-decoding the guest's pixel store on demand.
fn create_dib_section(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hdc = ctx.arg_u32(0)?;
    let pbmi = ctx.arg_u32(1)?;
    let _usage = ctx.arg_u32(2)?;
    let pp_bits = ctx.arg_u32(3)?;
    if pbmi == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    // BITMAPINFOHEADER is 40 bytes.
    let hdr = ctx.cpu.read_mem(pbmi, 40)?;
    let bi_size = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if bi_size < 40 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bi_width = i32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let bi_height = i32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let bi_bpp = u16::from_le_bytes([hdr[14], hdr[15]]);
    let bi_compression = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
    let bi_colors_used = u32::from_le_bytes([hdr[32], hdr[33], hdr[34], hdr[35]]);
    if bi_width <= 0 || bi_height == 0 || (bi_compression != 0 && bi_compression != 3) {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let width = bi_width as u32;
    let bottom_up = bi_height > 0;
    let height = bi_height.unsigned_abs();
    let row_bytes = match bi_bpp {
        1 => width.div_ceil(8),
        4 => width.div_ceil(2),
        8 => width,
        16 => width * 2,
        24 => width * 3,
        32 => width * 4,
        _ => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    let row_stride = (row_bytes + 3) & !3;
    let pixel_size = row_stride.saturating_mul(height);

    let palette_entries = match bi_bpp {
        1 | 4 | 8 => {
            if bi_colors_used == 0 {
                1u32 << bi_bpp
            } else {
                bi_colors_used
            }
        }
        _ => 0,
    };
    let palette_off = bi_size as usize;
    let mut palette_565 = Vec::with_capacity(palette_entries as usize);
    if palette_entries > 0 {
        let pal_bytes = ctx
            .cpu
            .read_mem(pbmi + palette_off as u32, palette_entries * 4)?;
        for i in 0..palette_entries as usize {
            let p = i * 4;
            palette_565.push(pocket_kernel::framebuffer::pack_rgb565(
                pal_bytes[p + 2],
                pal_bytes[p + 1],
                pal_bytes[p],
            ));
        }
    }

    let bits_va = match ctx.kernel.heap.alloc(pixel_size.max(1)) {
        Some(p) => p,
        None => {
            log::warn!("CreateDIBSection: heap exhausted (need {pixel_size} bytes)");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    // Zero-fill so the buffer is well-defined before the game paints
    // into it.
    let zeros = vec![0u8; pixel_size as usize];
    ctx.cpu.write_mem(bits_va, &zeros)?;
    if pp_bits != 0 {
        ctx.cpu.write_mem(pp_bits, &bits_va.to_le_bytes())?;
    }

    let bm = pocket_kernel::gdi::Bitmap::new_dib(
        width,
        height,
        bi_bpp,
        bits_va,
        row_stride,
        bottom_up,
        palette_565,
    );
    let handle = ctx.kernel.gdi.register_dib(bm);
    log::debug!(
        "CreateDIBSection({}x{}, {}bpp, {}-up) -> 0x{:08x} bits=0x{:08x}",
        width,
        height,
        bi_bpp,
        if bottom_up { "bottom" } else { "top" },
        handle,
        bits_va
    );
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn create_bitmap(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let w = ctx.arg_u32(0)?;
    let h = ctx.arg_u32(1)?;
    let _planes = ctx.arg_u32(2)?;
    let _bpp = ctx.arg_u32(3)?;
    let _bits = ctx.arg_u32(4)?;
    let handle = ctx.kernel.gdi.create_compatible_bitmap(w, h);
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn ellipse(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Approximate Ellipse with a fill+stroke rect for now — Pocket PC
    // games use this primarily as a focus indicator.
    rectangle(ctx)
}

fn pat_blt(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hdc = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(1)? as i32;
    let y = ctx.arg_u32(2)? as i32;
    let w = ctx.arg_u32(3)? as i32;
    let h = ctx.arg_u32(4)? as i32;
    let _rop = ctx.arg_u32(5)?;
    let dc_meta = match ctx.kernel.gdi.dc(hdc).cloned() {
        Some(d) => d,
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    let rgb = colorref_to_rgb565(dc_meta.brush_color);
    if let Some(mut surf) = surface_for_dc(ctx.kernel, hdc) {
        surf.fill_rect(x, y, w, h, rgb);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn stretch_blt(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Treat StretchBlt as BitBlt for now — destination and source
    // sizes match in practice for the JumpyBall sprite path.
    let hdc_dst = ctx.arg_u32(0)?;
    let dx = ctx.arg_u32(1)? as i32;
    let dy = ctx.arg_u32(2)? as i32;
    let dw = ctx.arg_u32(3)? as i32;
    let dh = ctx.arg_u32(4)? as i32;
    let hdc_src = ctx.arg_u32(5)?;
    let sx = ctx.arg_u32(6)? as i32;
    let sy = ctx.arg_u32(7)? as i32;
    let _sw = ctx.arg_u32(8)? as i32;
    let _sh = ctx.arg_u32(9)? as i32;
    let _rop = ctx.arg_u32(10)?;
    bit_blt_inner(ctx, hdc_dst, dx, dy, dw, dh, hdc_src, sx, sy)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `int DrawTextW(HDC hdc, LPCWSTR text, int n, LPRECT rc, UINT fmt)`
/// — render the supplied UTF-16 string into the destination DC's
/// surface using a built-in 6×8 ASCII font. `n` may be `-1`, in which
/// case the string is NUL-terminated.
fn draw_text_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hdc = ctx.arg_u32(0)?;
    let text_p = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)? as i32;
    let rc_p = ctx.arg_u32(3)?;
    let dc_meta = match ctx.kernel.gdi.dc(hdc).cloned() {
        Some(d) => d,
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    let mut chars = Vec::new();
    if text_p != 0 {
        let max = if n < 0 { 1024 } else { (n as u32).min(1024) };
        let raw = ctx.cpu.read_mem(text_p, max * 2)?;
        for i in (0..raw.len()).step_by(2) {
            if i + 1 >= raw.len() {
                break;
            }
            let u = u16::from_le_bytes([raw[i], raw[i + 1]]);
            if n < 0 && u == 0 {
                break;
            }
            chars.push(u);
        }
    }
    let (rl, rt, rr, rb) = if rc_p != 0 {
        let r = ctx.cpu.read_mem(rc_p, 16)?;
        (
            i32::from_le_bytes([r[0], r[1], r[2], r[3]]),
            i32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            i32::from_le_bytes([r[8], r[9], r[10], r[11]]),
            i32::from_le_bytes([r[12], r[13], r[14], r[15]]),
        )
    } else {
        (0, 0, FB_WIDTH as i32, FB_HEIGHT as i32)
    };
    let color = colorref_to_rgb565(dc_meta.text_color);
    let bk_color = colorref_to_rgb565(dc_meta.bk_color);
    let glyph_w = pocket_kernel::font::GLYPH_W;
    let glyph_h = pocket_kernel::font::GLYPH_H;
    // DT_CENTER = 1, DT_VCENTER = 4, DT_SINGLELINE = 0x20.
    let fmt = ctx.arg_u32(4).unwrap_or(0);
    let pixel_w = chars.len() as i32 * glyph_w;
    let x = if fmt & 0x1 != 0 {
        rl + ((rr - rl) - pixel_w).max(0) / 2
    } else {
        rl
    };
    let y = if fmt & 0x4 != 0 {
        rt + ((rb - rt) - glyph_h).max(0) / 2
    } else {
        rt
    };
    if let Some(mut surf) = surface_for_dc(ctx.kernel, hdc) {
        if !dc_meta.bk_transparent {
            surf.fill_rect(x, y, pixel_w, glyph_h, bk_color);
        }
        pocket_kernel::font::draw_str_u16(&mut surf, x, y, &chars, color);
        surf.mark_dirty();
    }
    Ok(DispatchOutcome::ReturnedR0(glyph_h as u32))
}

/// `BOOL TextOutW(HDC, int x, int y, LPCWSTR text, int len)` — render a
/// short UTF-16 string at the given pixel coordinates using the same
/// 6×8 font as `DrawTextW`.
fn text_out_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hdc = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(1)? as i32;
    let y = ctx.arg_u32(2)? as i32;
    let text_p = ctx.arg_u32(3)?;
    let len = ctx.arg_u32(4)? as i32;
    blit_text_at(ctx, hdc, x, y, text_p, len)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL ExtTextOutW(HDC, int x, int y, UINT options, RECT* rc,
///                   LPCWSTR text, UINT len, INT* dx)`
fn ext_text_out_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hdc = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(1)? as i32;
    let y = ctx.arg_u32(2)? as i32;
    let _opts = ctx.arg_u32(3)?;
    // The 5th and 6th args go on the stack; arg_u32(4)/(5) handle that.
    let _rc = ctx.arg_u32(4).unwrap_or(0);
    let text_p = ctx.arg_u32(5).unwrap_or(0);
    let len = ctx.arg_u32(6).unwrap_or(0) as i32;
    blit_text_at(ctx, hdc, x, y, text_p, len)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn blit_text_at(
    ctx: &mut CallCtx<'_>,
    hdc: u32,
    x: i32,
    y: i32,
    text_p: u32,
    len: i32,
) -> Result<(), KernelError> {
    let dc_meta = match ctx.kernel.gdi.dc(hdc).cloned() {
        Some(d) => d,
        None => return Ok(()),
    };
    let mut chars = Vec::new();
    if text_p != 0 {
        let max = if len < 0 {
            1024
        } else {
            (len as u32).min(1024)
        };
        let raw = ctx.cpu.read_mem(text_p, max * 2)?;
        for i in (0..raw.len()).step_by(2) {
            if i + 1 >= raw.len() {
                break;
            }
            let u = u16::from_le_bytes([raw[i], raw[i + 1]]);
            if len < 0 && u == 0 {
                break;
            }
            chars.push(u);
        }
    }
    let color = colorref_to_rgb565(dc_meta.text_color);
    let bk_color = colorref_to_rgb565(dc_meta.bk_color);
    let pixel_w = chars.len() as i32 * pocket_kernel::font::GLYPH_W;
    if let Some(mut surf) = surface_for_dc(ctx.kernel, hdc) {
        if !dc_meta.bk_transparent {
            surf.fill_rect(x, y, pixel_w, pocket_kernel::font::GLYPH_H, bk_color);
        }
        pocket_kernel::font::draw_str_u16(&mut surf, x, y, &chars, color);
        surf.mark_dirty();
    }
    Ok(())
}

fn ext_escape(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // ExtEscape is used to query device-specific capabilities
    // (rotation hints, GAPI fast paths). Reporting "unsupported" (0)
    // makes the game fall back to the default GDI path.
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn get_device_caps(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hdc = ctx.arg_u32(0)?;
    let index = ctx.arg_u32(1)?;
    let v = match index {
        8 => FB_WIDTH,   // HORZRES
        10 => FB_HEIGHT, // VERTRES
        12 => 16,        // BITSPIXEL
        14 => 1,         // PLANES
        88 => 96,        // LOGPIXELSX
        90 => 96,        // LOGPIXELSY
        _ => 0,
    };
    Ok(DispatchOutcome::ReturnedR0(v))
}

// ---------- random / time ----------

fn rand_handler(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEED: AtomicU32 = AtomicU32::new(0x1234_ABCD);
    // 32-bit linear congruential generator (Numerical Recipes parameters).
    let prev = SEED.load(Ordering::Relaxed);
    let next = prev.wrapping_mul(1664525).wrapping_add(1013904223);
    SEED.store(next, Ordering::Relaxed);
    Ok(DispatchOutcome::ReturnedR0(next & 0x7FFF))
}

fn srand_handler(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn time_handler(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(now))
}

// ---------- TLS ----------

/// `DWORD TlsAlloc(void)` — return the index of an unused slot, or
/// `TLS_OUT_OF_INDEXES (0xFFFFFFFF)` if all slots are taken. We
/// track the bitmap host-side and zero-init the slot's storage in
/// guest memory so a subsequent `TlsGetValue` before any
/// `TlsSetValue` returns the documented `0`.
fn tls_alloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let used = ctx.kernel.tls_slots_used;
    for slot in 0..TLS_SLOT_COUNT {
        if used & (1u64 << slot) == 0 {
            ctx.kernel.tls_slots_used |= 1u64 << slot;
            // Zero the slot in the user kdata TLS array so the
            // first TlsGetValue returns 0 as documented.
            let slot_va = USER_KDATA_TLS_ARRAY_VA + slot * 4;
            ctx.cpu.write_mem(slot_va, &[0u8; 4])?;
            return Ok(DispatchOutcome::ReturnedR0(slot));
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0xFFFF_FFFF))
}

/// `BOOL TlsFree(DWORD dwTlsIndex)` — clear the bookkeeping bit.
fn tls_free(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let slot = ctx.arg_u32(0)?;
    if slot >= TLS_SLOT_COUNT {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    ctx.kernel.tls_slots_used &= !(1u64 << slot);
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `LPVOID TlsGetValue(DWORD dwTlsIndex)` — read the slot value
/// from the in-page TLS array.
fn tls_get_value(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let slot = ctx.arg_u32(0)?;
    if slot >= TLS_SLOT_COUNT {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(USER_KDATA_TLS_ARRAY_VA + slot * 4, 4)?;
    let v = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    Ok(DispatchOutcome::ReturnedR0(v))
}

/// `BOOL TlsSetValue(DWORD dwTlsIndex, LPVOID lpTlsValue)` — write
/// the slot value into the in-page TLS array.
fn tls_set_value(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let slot = ctx.arg_u32(0)?;
    let value = ctx.arg_u32(1)?;
    if slot >= TLS_SLOT_COUNT {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    ctx.cpu
        .write_mem(USER_KDATA_TLS_ARRAY_VA + slot * 4, &value.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

// ---------- Interlocked / atomics ----------
//
// Single-threaded HLE: just perform the op on guest memory. Real
// WinCE provides these as fast user-mode atomics through the kernel
// trap page.

fn interlocked_op<F: FnOnce(i32) -> i32>(
    ctx: &mut CallCtx<'_>,
    f: F,
) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(p, 4)?;
    let v = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let new = f(v);
    ctx.cpu.write_mem(p, &new.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(new as u32))
}

fn interlocked_increment(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    interlocked_op(ctx, |v| v.wrapping_add(1))
}

fn interlocked_decrement(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    interlocked_op(ctx, |v| v.wrapping_sub(1))
}

/// `LONG InterlockedExchange(LONG volatile *Target, LONG Value)`
/// — write `Value` into `*Target`, return the previous value.
fn interlocked_exchange(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let new = ctx.arg_u32(1)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(p, 4)?;
    let old = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    ctx.cpu.write_mem(p, &new.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(old))
}

/// `LONG InterlockedExchangeAdd(LONG volatile *Addend, LONG Value)`
/// — atomically `*Addend += Value`, return the previous `*Addend`.
fn interlocked_exchange_add(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let add = ctx.arg_u32(1)? as i32;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(p, 4)?;
    let old = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let new = old.wrapping_add(add);
    ctx.cpu.write_mem(p, &new.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(old as u32))
}

/// `LONG InterlockedCompareExchange(LONG volatile *Destination,
///   LONG Exchange, LONG Comperand)` — if `*Destination ==
/// Comperand`, replace with `Exchange`. Return the previous
/// `*Destination`.
fn interlocked_compare_exchange(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let exchange = ctx.arg_u32(1)?;
    let comperand = ctx.arg_u32(2)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(p, 4)?;
    let old = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if old == comperand {
        ctx.cpu.write_mem(p, &exchange.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(old))
}

// ---------- Time / random extras ----------

/// `void GetSystemTime(LPSYSTEMTIME lpSystemTime)` /
/// `void GetLocalTime(LPSYSTEMTIME lpSystemTime)` — fill a
/// `SYSTEMTIME` struct (16 bytes of `WORD`s):
///   wYear, wMonth, wDayOfWeek, wDay, wHour, wMinute, wSecond, wMilli
fn get_system_time(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let total_secs = now_ms / 1000;
    let ms = (now_ms % 1000) as u16;
    let secs = (total_secs % 60) as u16;
    let mins = ((total_secs / 60) % 60) as u16;
    let hours = ((total_secs / 3600) % 24) as u16;
    // We don't bother with proper civil-calendar conversion: most
    // games only care that the fields look plausible (non-zero year,
    // month in 1..=12, day in 1..=31). 2026-01-01 is a fine fake.
    let mut buf = [0u8; 16];
    buf[0..2].copy_from_slice(&2026u16.to_le_bytes()); // wYear
    buf[2..4].copy_from_slice(&1u16.to_le_bytes()); // wMonth
    buf[4..6].copy_from_slice(&4u16.to_le_bytes()); // wDayOfWeek (Thu)
    buf[6..8].copy_from_slice(&1u16.to_le_bytes()); // wDay
    buf[8..10].copy_from_slice(&hours.to_le_bytes());
    buf[10..12].copy_from_slice(&mins.to_le_bytes());
    buf[12..14].copy_from_slice(&secs.to_le_bytes());
    buf[14..16].copy_from_slice(&ms.to_le_bytes());
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `void GetSystemTimeAsFileTime(LPFILETIME lpSystemTimeAsFileTime)`
/// / `void GetCurrentFT(LPFILETIME)` — fill a `FILETIME`
/// (`{ DWORD dwLowDateTime; DWORD dwHighDateTime; }`) with the
/// number of 100-ns intervals since 1601-01-01 UTC. Real Windows
/// games (and Pocket PC games) seed PRNGs from this value, and
/// `GetCurrentFT` is the WinCE-specific ordinal-only export.
fn get_system_time_as_file_time(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    // 11644473600 seconds between 1601-01-01 and 1970-01-01.
    const EPOCH_DIFF_100NS: u64 = 11_644_473_600 * 10_000_000;
    let now_100ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100) as u64)
        .unwrap_or(0);
    let ft = now_100ns.wrapping_add(EPOCH_DIFF_100NS);
    let lo = (ft & 0xFFFF_FFFF) as u32;
    let hi = (ft >> 32) as u32;
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(&lo.to_le_bytes());
    buf[4..8].copy_from_slice(&hi.to_le_bytes());
    ctx.cpu.write_mem(p, &buf)?;
    // `GetCurrentFT` is documented to also return its argument in
    // the WinCE OAL implementation; harmless either way.
    Ok(DispatchOutcome::ReturnedR0(p))
}

/// `DWORD CeGetRandomSeed(void)` — undocumented WinCE export
/// (ordinal 1443 in older coredlls) used by a handful of games as
/// a PRNG seed source.
fn ce_get_random_seed(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEED: AtomicU32 = AtomicU32::new(0xC0DE_F00D);
    let prev = SEED.load(Ordering::Relaxed);
    let next = prev.wrapping_mul(1103515245).wrapping_add(12345);
    SEED.store(next, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(next ^ now))
}

/// `BOOL QueryPerformanceCounter(LARGE_INTEGER *count)` — fill the
/// 8-byte counter with a monotonically-increasing tick value.
fn query_performance_counter(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    let lo = (now & 0xFFFF_FFFF) as u32;
    let hi = (now >> 32) as u32;
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(&lo.to_le_bytes());
    buf[4..8].copy_from_slice(&hi.to_le_bytes());
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL QueryPerformanceFrequency(LARGE_INTEGER *freq)` — we use
/// microseconds in the counter, so report `1_000_000`.
fn query_performance_frequency(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(&1_000_000u32.to_le_bytes());
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `HANDLE GetCurrentProcess(void)` — return the kdata-page-backed
/// pseudo-handle, matching what the user-kdata `ahSys[SH_CURPROC]`
/// short-cut returns.
fn get_current_process(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_CURRENT_PROCESS_HANDLE))
}

/// `HANDLE GetCurrentThread(void)` — see `get_current_process`.
fn get_current_thread(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_CURRENT_THREAD_HANDLE))
}

// ---------- libm (soft-float, double-precision) ----------
//
// MS C compiler for ARM PocketPC emits these as imports against
// `coredll.dll` (they live there alongside the CRT). The default
// stub returning `r0=0` makes every `sin`/`cos`/`sqrt` evaluate to
// `+0.0`, which kills any game that does any trigonometry —
// e.g. Zuma's path / Asphalt 2's camera / Bejeweled gem swap
// animation. We implement them in real f64 arithmetic on the host
// and pack the result back into r0:r1.

fn libm_unary_d<F: FnOnce(f64) -> f64>(
    ctx: &mut CallCtx<'_>,
    f: F,
) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(f(read_f64(ctx, 0)?)))
}

fn libm_binary_d<F: FnOnce(f64, f64) -> f64>(
    ctx: &mut CallCtx<'_>,
    f: F,
) -> Result<DispatchOutcome, KernelError> {
    let a = read_f64(ctx, 0)?;
    let b = read_f64(ctx, 2)?;
    Ok(ret_f64(f(a, b)))
}

fn m_sin(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::sin)
}
fn m_cos(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::cos)
}
fn m_tan(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::tan)
}
fn m_asin(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::asin)
}
fn m_acos(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::acos)
}
fn m_atan(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::atan)
}
fn m_sinh(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::sinh)
}
fn m_cosh(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::cosh)
}
fn m_tanh(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::tanh)
}
fn m_exp(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::exp)
}
fn m_log(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::ln)
}
fn m_log10(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::log10)
}
fn m_sqrt(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::sqrt)
}
fn m_floor(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::floor)
}
fn m_ceil(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::ceil)
}
fn m_fabs(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::abs)
}

fn m_atan2(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_binary_d(c, f64::atan2)
}
fn m_pow(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_binary_d(c, f64::powf)
}
fn m_fmod(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_binary_d(c, |a, b| a % b)
}
fn m_hypot(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_binary_d(c, f64::hypot)
}

/// `double ldexp(double x, int exp)` — only the second argument is
/// integer-typed, so x is in r0:r1 and the exponent in r2.
fn m_ldexp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let x = read_f64(ctx, 0)?;
    let e = ctx.arg_u32(2)? as i32;
    Ok(ret_f64(x * 2.0_f64.powi(e)))
}

/// `double frexp(double x, int *eptr)` — split into mantissa &
/// binary exponent.
fn m_frexp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let x = read_f64(ctx, 0)?;
    let eptr = ctx.arg_u32(2)?;
    let (mantissa, exp) = if x == 0.0 {
        (0.0, 0i32)
    } else {
        let bits = x.to_bits();
        let raw_exp = ((bits >> 52) & 0x7FF) as i32;
        let e = raw_exp - 1022;
        let m = f64::from_bits((bits & !(0x7FFu64 << 52)) | (1022u64 << 52));
        (m, e)
    };
    if eptr != 0 {
        ctx.cpu.write_mem(eptr, &exp.to_le_bytes())?;
    }
    Ok(ret_f64(mantissa))
}

/// `double modf(double x, double *iptr)` — split into integral and
/// fractional parts.
fn m_modf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let x = read_f64(ctx, 0)?;
    let iptr = ctx.arg_u32(2)?;
    let int_part = x.trunc();
    let frac_part = x - int_part;
    if iptr != 0 {
        ctx.cpu.write_mem(iptr, &int_part.to_le_bytes())?;
    }
    Ok(ret_f64(frac_part))
}

// ---------- lstr* (16-bit Unicode and ANSI) ----------

fn lstrlen_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let s = read_wstr(ctx, p, 0x10000)?;
    Ok(DispatchOutcome::ReturnedR0(s.len() as u32))
}

fn lstrlen_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let s = read_cstr(ctx, p, 0x10000)?;
    Ok(DispatchOutcome::ReturnedR0(s.len() as u32))
}

fn lstrcpy_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    if dst == 0 || src == 0 {
        return Ok(DispatchOutcome::ReturnedR0(dst));
    }
    let mut off = 0u32;
    loop {
        let b = ctx.cpu.read_mem(src + off, 2)?;
        ctx.cpu.write_mem(dst + off, &b)?;
        if b[0] == 0 && b[1] == 0 {
            break;
        }
        off += 2;
        if off > 0x40000 {
            break;
        }
    }
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn lstrcpy_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    if dst == 0 || src == 0 {
        return Ok(DispatchOutcome::ReturnedR0(dst));
    }
    let bytes = read_cstr(ctx, src, 0x10000)?;
    let mut buf = bytes;
    buf.push(0);
    ctx.cpu.write_mem(dst, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn lstrcat_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    if dst == 0 || src == 0 {
        return Ok(DispatchOutcome::ReturnedR0(dst));
    }
    // Find end of dst.
    let mut end = dst;
    loop {
        let b = ctx.cpu.read_mem(end, 2)?;
        if b[0] == 0 && b[1] == 0 {
            break;
        }
        end += 2;
        if end - dst > 0x40000 {
            break;
        }
    }
    // Copy src (incl. terminator) onto end.
    let mut off = 0u32;
    loop {
        let b = ctx.cpu.read_mem(src + off, 2)?;
        ctx.cpu.write_mem(end + off, &b)?;
        if b[0] == 0 && b[1] == 0 {
            break;
        }
        off += 2;
        if off > 0x40000 {
            break;
        }
    }
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn lstrcat_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    if dst == 0 || src == 0 {
        return Ok(DispatchOutcome::ReturnedR0(dst));
    }
    let cur = read_cstr(ctx, dst, 0x10000)?;
    let add = read_cstr(ctx, src, 0x10000)?;
    let mut buf = cur;
    buf.extend_from_slice(&add);
    buf.push(0);
    ctx.cpu.write_mem(dst, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn cmp_to_winapi(o: std::cmp::Ordering) -> u32 {
    match o {
        std::cmp::Ordering::Less => (-1i32) as u32,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

fn lstrcmp_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let a = ctx.arg_u32(0)?;
    let b = ctx.arg_u32(1)?;
    let sa = if a == 0 {
        Vec::new()
    } else {
        read_wstr(ctx, a, 0x10000)?
    };
    let sb = if b == 0 {
        Vec::new()
    } else {
        read_wstr(ctx, b, 0x10000)?
    };
    Ok(DispatchOutcome::ReturnedR0(cmp_to_winapi(sa.cmp(&sb))))
}

fn lstrcmp_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let a = ctx.arg_u32(0)?;
    let b = ctx.arg_u32(1)?;
    let sa = if a == 0 {
        Vec::new()
    } else {
        read_cstr(ctx, a, 0x10000)?
    };
    let sb = if b == 0 {
        Vec::new()
    } else {
        read_cstr(ctx, b, 0x10000)?
    };
    Ok(DispatchOutcome::ReturnedR0(cmp_to_winapi(sa.cmp(&sb))))
}

fn lstrcmpi_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let a = ctx.arg_u32(0)?;
    let b = ctx.arg_u32(1)?;
    let to_lower = |v: Vec<u16>| -> Vec<u16> {
        v.into_iter()
            .map(|c| {
                if (b'A' as u16..=b'Z' as u16).contains(&c) {
                    c + 32
                } else {
                    c
                }
            })
            .collect()
    };
    let sa = if a == 0 {
        Vec::new()
    } else {
        to_lower(read_wstr(ctx, a, 0x10000)?)
    };
    let sb = if b == 0 {
        Vec::new()
    } else {
        to_lower(read_wstr(ctx, b, 0x10000)?)
    };
    Ok(DispatchOutcome::ReturnedR0(cmp_to_winapi(sa.cmp(&sb))))
}

fn lstrcmpi_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let a = ctx.arg_u32(0)?;
    let b = ctx.arg_u32(1)?;
    let sa = if a == 0 {
        Vec::new()
    } else {
        read_cstr(ctx, a, 0x10000)?
            .into_iter()
            .map(|c| c.to_ascii_lowercase())
            .collect::<Vec<u8>>()
    };
    let sb = if b == 0 {
        Vec::new()
    } else {
        read_cstr(ctx, b, 0x10000)?
            .into_iter()
            .map(|c| c.to_ascii_lowercase())
            .collect::<Vec<u8>>()
    };
    Ok(DispatchOutcome::ReturnedR0(cmp_to_winapi(sa.cmp(&sb))))
}

// ---------- RECT helpers ----------

fn rect_load(ctx: &mut CallCtx<'_>, p: u32) -> Result<(i32, i32, i32, i32), KernelError> {
    let bytes = ctx.cpu.read_mem(p, 16)?;
    let l = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let t = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let r = i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let b = i32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    Ok((l, t, r, b))
}

fn rect_store(
    ctx: &mut CallCtx<'_>,
    p: u32,
    l: i32,
    t: i32,
    r: i32,
    b: i32,
) -> Result<(), KernelError> {
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&l.to_le_bytes());
    buf[4..8].copy_from_slice(&t.to_le_bytes());
    buf[8..12].copy_from_slice(&r.to_le_bytes());
    buf[12..16].copy_from_slice(&b.to_le_bytes());
    ctx.cpu.write_mem(p, &buf)?;
    Ok(())
}

fn set_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let l = ctx.arg_u32(1)? as i32;
    let t = ctx.arg_u32(2)? as i32;
    let r = ctx.arg_u32(3)? as i32;
    let b = ctx.arg_u32(4)? as i32;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    rect_store(ctx, p, l, t, r, b)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn set_rect_empty(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    rect_store(ctx, p, 0, 0, 0, 0)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn copy_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    if dst == 0 || src == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(src, 16)?;
    ctx.cpu.write_mem(dst, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn inflate_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let dx = ctx.arg_u32(1)? as i32;
    let dy = ctx.arg_u32(2)? as i32;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let (l, t, r, b) = rect_load(ctx, p)?;
    rect_store(ctx, p, l - dx, t - dy, r + dx, b + dy)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn offset_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let dx = ctx.arg_u32(1)? as i32;
    let dy = ctx.arg_u32(2)? as i32;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let (l, t, r, b) = rect_load(ctx, p)?;
    rect_store(ctx, p, l + dx, t + dy, r + dx, b + dy)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn pt_in_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(1)? as i32;
    let y = ctx.arg_u32(2)? as i32;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let (l, t, r, b) = rect_load(ctx, p)?;
    let inside = (x >= l && x < r && y >= t && y < b) as u32;
    Ok(DispatchOutcome::ReturnedR0(inside))
}

fn is_rect_empty(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let (l, t, r, b) = rect_load(ctx, p)?;
    Ok(DispatchOutcome::ReturnedR0(if l >= r || t >= b {
        1
    } else {
        0
    }))
}

// ---------- Locale ----------

fn get_user_default_lang_id(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // 0x0409 = English (US)
    Ok(DispatchOutcome::ReturnedR0(0x0409))
}

fn get_user_default_lcid(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0409))
}

fn get_system_default_lang_id(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0409))
}

fn get_thread_locale(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0409))
}

// ---------- Codepage conversion ----------
//
// Most PPC games call `MultiByteToWideChar` / `WideCharToMultiByte`
// with CP_ACP (0) or CP_UTF8 (65001). The default `r0=0` stub
// makes the game think the conversion failed and frequently leads
// to a NULL deref a few frames later when the resulting empty
// string is treated as a valid pointer.

const CP_UTF8: u32 = 65001;

/// `int MultiByteToWideChar(UINT cp, DWORD flags, LPCSTR src,
///     int cbSrc, LPWSTR dst, int cchDst)`
fn multi_byte_to_wide_char(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let codepage = ctx.arg_u32(0)?;
    let _flags = ctx.arg_u32(1)?;
    let src = ctx.arg_u32(2)?;
    let cb_src_signed = ctx.arg_u32(3)? as i32;
    let dst = ctx.arg_u32(4)?;
    let cch_dst = ctx.arg_u32(5)? as i32;

    if src == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    // -1 means: src is null-terminated, include the null in the output.
    let include_null = cb_src_signed < 0;
    let cb_src = if cb_src_signed < 0 {
        let mut n = 0u32;
        loop {
            let b = ctx.cpu.read_mem(src + n, 1)?;
            n += 1;
            if b[0] == 0 {
                break;
            }
            if n > 0x40000 {
                break;
            }
        }
        n
    } else {
        cb_src_signed as u32
    };

    let raw = ctx.cpu.read_mem(src, cb_src)?;

    let wides: Vec<u16> = match codepage {
        CP_UTF8 => {
            let s = String::from_utf8_lossy(if include_null && raw.last() == Some(&0) {
                &raw[..raw.len() - 1]
            } else {
                &raw[..]
            });
            let mut v: Vec<u16> = s.encode_utf16().collect();
            if include_null {
                v.push(0);
            }
            v
        }
        _ => {
            // CP_ACP / OEM / anything else -> treat as latin-1.
            let body = if include_null && raw.last() == Some(&0) {
                &raw[..raw.len() - 1]
            } else {
                &raw[..]
            };
            let mut v: Vec<u16> = body.iter().map(|&b| b as u16).collect();
            if include_null {
                v.push(0);
            }
            v
        }
    };

    let needed = wides.len() as i32;
    if cch_dst == 0 || dst == 0 {
        return Ok(DispatchOutcome::ReturnedR0(needed as u32));
    }

    let to_write = needed.min(cch_dst) as usize;
    let mut buf = Vec::with_capacity(to_write * 2);
    for w in &wides[..to_write] {
        buf.extend_from_slice(&w.to_le_bytes());
    }
    if !buf.is_empty() {
        ctx.cpu.write_mem(dst, &buf)?;
    }
    Ok(DispatchOutcome::ReturnedR0(to_write as u32))
}

/// `int WideCharToMultiByte(UINT cp, DWORD flags, LPCWSTR src,
///     int cchSrc, LPSTR dst, int cbDst, LPCCH defChar,
///     LPBOOL usedDefault)`
fn wide_char_to_multi_byte(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let codepage = ctx.arg_u32(0)?;
    let _flags = ctx.arg_u32(1)?;
    let src = ctx.arg_u32(2)?;
    let cch_src_signed = ctx.arg_u32(3)? as i32;
    let dst = ctx.arg_u32(4)?;
    let cb_dst = ctx.arg_u32(5)? as i32;
    let _def_char = ctx.arg_u32(6)?;
    let used_default = ctx.arg_u32(7)?;

    if src == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    let include_null = cch_src_signed < 0;
    let cch_src = if cch_src_signed < 0 {
        let mut n = 0u32;
        loop {
            let b = ctx.cpu.read_mem(src + n * 2, 2)?;
            n += 1;
            if b[0] == 0 && b[1] == 0 {
                break;
            }
            if n > 0x40000 {
                break;
            }
        }
        n
    } else {
        cch_src_signed as u32
    };

    let mut wides: Vec<u16> = Vec::with_capacity(cch_src as usize);
    for i in 0..cch_src {
        let b = ctx.cpu.read_mem(src + i * 2, 2)?;
        wides.push(u16::from_le_bytes([b[0], b[1]]));
    }
    let body: &[u16] = if include_null && wides.last() == Some(&0) {
        &wides[..wides.len() - 1]
    } else {
        &wides[..]
    };

    let mut hit_default = false;
    let bytes: Vec<u8> = match codepage {
        CP_UTF8 => {
            let s = String::from_utf16_lossy(body);
            let mut v: Vec<u8> = s.into_bytes();
            if include_null {
                v.push(0);
            }
            v
        }
        _ => {
            // CP_ACP / OEM / anything else -> latin-1 (clamp >0xFF to '?').
            let mut v: Vec<u8> = Vec::with_capacity(body.len() + 1);
            for &w in body {
                if w <= 0xFF {
                    v.push(w as u8);
                } else {
                    v.push(b'?');
                    hit_default = true;
                }
            }
            if include_null {
                v.push(0);
            }
            v
        }
    };

    if used_default != 0 {
        let flag = if hit_default { 1u32 } else { 0u32 };
        ctx.cpu.write_mem(used_default, &flag.to_le_bytes())?;
    }

    let needed = bytes.len() as i32;
    if cb_dst == 0 || dst == 0 {
        return Ok(DispatchOutcome::ReturnedR0(needed as u32));
    }

    let to_write = (needed.min(cb_dst)) as usize;
    if to_write > 0 {
        ctx.cpu.write_mem(dst, &bytes[..to_write])?;
    }
    Ok(DispatchOutcome::ReturnedR0(to_write as u32))
}

fn register_window_message_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0xC100))
}

fn virtual_query(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let address = ctx.arg_u32(0)?;
    let info = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)?;
    if info == 0 || size < 16 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut buf = vec![0u8; size.min(48) as usize];
    buf[0..4].copy_from_slice(&address.to_le_bytes());
    buf[4..8].copy_from_slice(&address.to_le_bytes());
    buf[8..12].copy_from_slice(&0x1000u32.to_le_bytes());
    buf[12..16].copy_from_slice(&0x1000u32.to_le_bytes());
    ctx.cpu.write_mem(info, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(buf.len() as u32))
}

/// `FARPROC GetProcAddressW(HMODULE hModule, LPCWSTR lpProcName)`
/// — we don't have any DLLs the game can dynamically load against,
/// so always report failure (NULL). The game then has to fall back
/// to its statically-imported path.
fn get_proc_address_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let module = ctx.arg_u32(0)?;
    let name_p = ctx.arg_u32(1)?;
    let name = String::from_utf16_lossy(&read_wstr(ctx, name_p, 256).unwrap_or_default());
    let address = ctx
        .kernel
        .dynamic_exports
        .get(&module)
        .and_then(|exports| exports.get(&name).copied())
        .or_else(|| {
            ctx.kernel
                .dynamic_exports
                .get(&module)
                .and_then(|exports| exports.get(&name.to_ascii_lowercase()).copied())
        })
        .unwrap_or_else(|| {
            if name.eq_ignore_ascii_case("InitCommonControlsEx") {
                0xF000_0010
            } else {
                0
            }
        });
    log::debug!("GetProcAddressW(0x{module:08x}, {name:?}) -> 0x{address:08x}");
    Ok(DispatchOutcome::ReturnedR0(address))
}

fn get_cursor_pos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let point = ctx.arg_u32(0)?;
    if point != 0 {
        ctx.cpu.write_mem(point, &[0u8; 8])?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn set_cursor(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_class_info_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let info = ctx.arg_u32(2)?;
    if info != 0 {
        let mut wnd_class = [0u8; 48];
        wnd_class[4..8].copy_from_slice(&ctx.kernel.wnd_proc.to_le_bytes());
        wnd_class[16..20].copy_from_slice(&FAKE_MODULE_HANDLE.to_le_bytes());
        ctx.cpu.write_mem(info, &wnd_class)?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn create_dialog_indirect_param_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dialog_proc = ctx.arg_u32(3)?;
    let init_param = ctx.arg_u32(4).unwrap_or(0);
    if dialog_proc == 0 {
        return Ok(DispatchOutcome::ReturnedR0(FAKE_HWND));
    }
    use pocket_cpu::regs::ArmReg;
    ctx.cpu.write_reg(ArmReg::R0, FAKE_HWND)?;
    ctx.cpu.write_reg(ArmReg::R1, 0x0110)?;
    ctx.cpu.write_reg(ArmReg::R2, 0)?;
    ctx.cpu.write_reg(ArmReg::R3, init_param)?;
    log::debug!("CreateDialogIndirectParamW -> WM_INITDIALOG trampoline at 0x{dialog_proc:08x}");
    Ok(DispatchOutcome::JumpTo(dialog_proc))
}

fn is_window(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(
        (hwnd == FAKE_HWND || hwnd == FAKE_DESKTOP_HWND) as u32,
    ))
}

fn create_mutex_w(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0xDEAD_E300))
}

fn tls_call(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ce_set_thread_quantum(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

// ---------- Soft-Input-Panel ----------
//
// Modeled as "panel hidden, full-screen visible rect". `SIPINFO`
// layout (Windows Mobile 5/6) is `cbSize, fdwFlags, rcVisible(16),
// rcSipRect(16), dwImDataSize, pvImData` = 44 bytes; we just zero
// it out and stamp `cbSize`. Games (Bejeweled, Zuma, Asphalt 2)
// only check the flags to decide whether to lay out under the SIP.
fn sip_get_info(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.cpu.read_reg(ArmReg::R0)?;
    if p != 0 {
        let mut buf = [0u8; 44];
        // Stamp `cbSize` field if the caller set it; otherwise 44.
        let existing_size = ctx.cpu.read_mem(p, 4).unwrap_or_else(|_| vec![0; 4]);
        let cb = if existing_size.len() == 4 {
            u32::from_le_bytes([
                existing_size[0],
                existing_size[1],
                existing_size[2],
                existing_size[3],
            ])
        } else {
            0
        };
        let cb = if cb == 0 { 44 } else { cb.min(44) };
        buf[0..4].copy_from_slice(&cb.to_le_bytes());
        let _ = ctx.cpu.write_mem(p, &buf[..cb as usize]);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

// ---------- Clipboard (no-op stubs) ----------
//
// PocketHLE doesn't model a system clipboard; it's safe to behave
// as if we successfully opened an empty clipboard. The game just
// won't be able to round-trip text through it.

fn open_clipboard(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}
fn close_clipboard(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}
fn empty_clipboard(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}
fn is_clipboard_format_available(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn get_clipboard_data(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn set_clipboard_data(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

// ---------- Audio: waveOut* ---------------------------------------
//
// `waveOutOpen`/`waveOutWrite`/`waveOutClose` are the legacy Win32
// MM API the Pocket PC ships. Callbacks come in through a small
// number of formats — we only support PCM (`wFormatTag == 1`),
// which covers every Pocket PC game we've seen. Everything else is
// reported as success so the game proceeds; the audio just stays
// silent for the unsupported chunk.

const FAKE_HWAVEOUT: u32 = 0xDEAD_4001;
const MMSYSERR_NOERROR: u32 = 0;

/// `MMRESULT waveOutGetNumDevs(void)` — number of host wave-out
/// devices. We always claim one so games that probe before opening
/// don't fall back to a "no audio" code path.
fn wave_out_get_num_devs(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `MMRESULT waveOutGetVolume(HWAVEOUT, LPDWORD pdwVolume)` — write
/// 0xFFFFFFFF (max volume left + right) to the out parameter.
fn wave_out_get_volume(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    if p != 0 {
        ctx.cpu.write_mem(p, &0xFFFF_FFFFu32.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutSetVolume(HWAVEOUT, DWORD dwVolume)` — accept
/// the volume request silently. We don't have a host-side volume
/// control, so just return success.
fn wave_out_set_volume(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutOpen(LPHWAVEOUT phwo, UINT uDeviceID,
///                       LPCWAVEFORMATEX pwfx, DWORD_PTR dwCallback,
///                       DWORD_PTR dwInstance, DWORD fdwOpen)`
///
/// Reads the requested format from `pwfx`, snapshots it for the
/// audio engine, and opens the host audio device. Stores the fake
/// handle into `*phwo` if the caller asked for it, then returns
/// MMSYSERR_NOERROR. The caller's `WAVE_FORMAT_QUERY` flag (`0x1`)
/// asks us to *check* whether the format is supported without
/// opening — we report success either way.
fn wave_out_open(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let phwo = ctx.arg_u32(0)?;
    let _device_id = ctx.arg_u32(1)?;
    let pwfx = ctx.arg_u32(2)?;
    let _cb = ctx.arg_u32(3)?;
    let _inst = ctx.arg_u32(4)?;
    let flags = ctx.arg_u32(5)?;

    if pwfx != 0 {
        // WAVEFORMATEX: 18 bytes — wFormatTag (2), nChannels (2),
        // nSamplesPerSec (4), nAvgBytesPerSec (4), nBlockAlign (2),
        // wBitsPerSample (2), cbSize (2).
        let hdr = ctx.cpu.read_mem(pwfx, 18)?;
        let format_tag = u16::from_le_bytes([hdr[0], hdr[1]]);
        let channels = u16::from_le_bytes([hdr[2], hdr[3]]);
        let sample_rate = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let bits = u16::from_le_bytes([hdr[14], hdr[15]]);
        let fmt = pocket_kernel::audio::GuestFormat {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            bits_per_sample: bits.max(8),
        };
        ctx.kernel.wave_out_format = fmt;
        ctx.kernel.audio.set_guest_format(fmt);
        log::debug!(
            "waveOutOpen tag={format_tag} {sample_rate} Hz / {channels} ch / {bits}-bit, flags=0x{flags:08x}"
        );
    }

    // WAVE_FORMAT_QUERY = 0x1: don't actually open, just verify.
    if flags & 0x1 == 0 {
        ctx.kernel.audio.start();
    }
    if phwo != 0 {
        ctx.cpu.write_mem(phwo, &FAKE_HWAVEOUT.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutClose(HWAVEOUT)` — stop the host stream and
/// flush any remaining samples.
fn wave_out_close(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    ctx.kernel.audio.stop();
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutReset(HWAVEOUT)` — discard any queued samples.
fn wave_out_reset(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    ctx.kernel.audio.flush();
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutPrepareHeader(HWAVEOUT, LPWAVEHDR, UINT cbwh)`.
/// Real implementations would page-lock the buffer; we just clear
/// the WHDR_DONE flag so the guest's `dwFlags` ends up `WHDR_PREPARED`
/// (`0x2`).
fn wave_out_prepare_header(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    let p_hdr = ctx.arg_u32(1)?;
    let _cb = ctx.arg_u32(2)?;
    if p_hdr != 0 {
        // WAVEHDR.dwFlags is at offset 16. Set WHDR_PREPARED (0x2).
        let cur = ctx.cpu.read_mem(p_hdr + 16, 4)?;
        let mut flags = u32::from_le_bytes([cur[0], cur[1], cur[2], cur[3]]);
        flags = (flags & !0x1) | 0x2;
        ctx.cpu.write_mem(p_hdr + 16, &flags.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutUnprepareHeader(HWAVEOUT, LPWAVEHDR, UINT)` —
/// clear WHDR_PREPARED.
fn wave_out_unprepare_header(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    let p_hdr = ctx.arg_u32(1)?;
    let _cb = ctx.arg_u32(2)?;
    if p_hdr != 0 {
        let cur = ctx.cpu.read_mem(p_hdr + 16, 4)?;
        let mut flags = u32::from_le_bytes([cur[0], cur[1], cur[2], cur[3]]);
        flags &= !0x2;
        ctx.cpu.write_mem(p_hdr + 16, &flags.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutWrite(HWAVEOUT, LPWAVEHDR, UINT cbwh)`. Reads
/// the PCM payload from `lpData` / `dwBufferLength` and pushes it
/// into [`AudioEngine`] in i16 samples. The header's
/// `WHDR_DONE` (`0x1`) flag is set on return so the guest's send /
/// retire logic doesn't deadlock.
fn wave_out_write(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    let p_hdr = ctx.arg_u32(1)?;
    let _cb = ctx.arg_u32(2)?;
    if p_hdr == 0 {
        return Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR));
    }
    // WAVEHDR layout (Win32):
    //   +0   LPSTR  lpData
    //   +4   DWORD  dwBufferLength
    //   +8   DWORD  dwBytesRecorded
    //   +12  DWORD_PTR dwUser
    //   +16  DWORD  dwFlags
    let hdr = ctx.cpu.read_mem(p_hdr, 20)?;
    let p_data = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let n_bytes = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let mut flags = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
    if p_data != 0 && n_bytes > 0 {
        let bytes = ctx.cpu.read_mem(p_data, n_bytes)?;
        let fmt = ctx.kernel.wave_out_format;
        match fmt.bits_per_sample {
            16 => {
                let mut samples = Vec::with_capacity(bytes.len() / 2);
                for chunk in bytes.chunks_exact(2) {
                    samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
                ctx.kernel.audio.push_samples(&samples);
            }
            8 => {
                ctx.kernel.audio.push_samples_u8(&bytes);
            }
            other => {
                log::debug!("waveOutWrite: unsupported bits_per_sample={other}, dropping");
            }
        }
    }
    flags = (flags & !0x4) | 0x1; // clear WHDR_INQUEUE, set WHDR_DONE
    ctx.cpu.write_mem(p_hdr + 16, &flags.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

// ---------- GDI helpers --------------------------------------------

/// `BOOL SetDIBitsToDevice(HDC hdc, int xDest, int yDest, DWORD w,
///                          DWORD h, int xSrc, int ySrc,
///                          UINT StartScan, UINT cLines,
///                          const VOID *lpvBits,
///                          const BITMAPINFO *lpbmi,
///                          UINT ColorUse)`.
///
/// WINMINECE and a number of other Pocket PC titles use this as a
/// one-shot "blit a DIB straight to the screen" path. We decode the
/// DIB header, then walk the pixel data and `put_pixel` it into
/// the destination DC's surface.
#[allow(clippy::too_many_arguments)]
fn set_di_bits_to_device(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hdc = ctx.arg_u32(0)?;
    let x_dst = ctx.arg_u32(1)? as i32;
    let y_dst = ctx.arg_u32(2)? as i32;
    let w = ctx.arg_u32(3)?;
    let h = ctx.arg_u32(4)?;
    let x_src = ctx.arg_u32(5)? as i32;
    let y_src = ctx.arg_u32(6)? as i32;
    let _start_scan = ctx.arg_u32(7)?;
    let c_lines = ctx.arg_u32(8)?;
    let p_bits = ctx.arg_u32(9)?;
    let p_bmi = ctx.arg_u32(10)?;
    let _color_use = ctx.arg_u32(11)?;
    blit_dib(
        ctx, hdc, x_dst, y_dst, w, h, x_src, y_src, c_lines, p_bits, p_bmi,
    )?;
    Ok(DispatchOutcome::ReturnedR0(c_lines.max(h)))
}

/// `int StretchDIBits(HDC, int xDest, int yDest, int wDest, int hDest,
///                     int xSrc, int ySrc, int wSrc, int hSrc,
///                     CONST VOID *lpBits, CONST BITMAPINFO *lpbmi,
///                     UINT iUsage, DWORD rop)`.
///
/// We don't implement true stretching; if the rectangles are the
/// same size we delegate to the SetDIBitsToDevice path, otherwise
/// we fall back to a per-pixel nearest-neighbour upscale.
#[allow(clippy::too_many_arguments)]
fn stretch_di_bits(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hdc = ctx.arg_u32(0)?;
    let x_dst = ctx.arg_u32(1)? as i32;
    let y_dst = ctx.arg_u32(2)? as i32;
    let w_dst = ctx.arg_u32(3)? as i32;
    let h_dst = ctx.arg_u32(4)? as i32;
    let x_src = ctx.arg_u32(5)? as i32;
    let y_src = ctx.arg_u32(6)? as i32;
    let w_src = ctx.arg_u32(7)? as i32;
    let h_src = ctx.arg_u32(8)? as i32;
    let p_bits = ctx.arg_u32(9)?;
    let p_bmi = ctx.arg_u32(10)?;
    let _usage = ctx.arg_u32(11)?;
    let _rop = ctx.arg_u32(12)?;
    if w_dst == w_src && h_dst == h_src {
        blit_dib(
            ctx,
            hdc,
            x_dst,
            y_dst,
            w_src as u32,
            h_src as u32,
            x_src,
            y_src,
            h_src.max(0) as u32,
            p_bits,
            p_bmi,
        )?;
    } else {
        // Render src into a host-side buffer, then sample-stretch
        // into dst surface.
        let pixels = decode_dib(ctx, p_bmi, p_bits)?;
        if let Some((src_pix, sw, sh)) = pixels {
            if let Some(mut dst) = surface_for_dc(ctx.kernel, hdc) {
                let x_src = x_src.max(0) as u32;
                let y_src = y_src.max(0) as u32;
                let w_dst = w_dst.max(0) as u32;
                let h_dst = h_dst.max(0) as u32;
                let w_src_eff = (w_src.max(0) as u32).min(sw.saturating_sub(x_src));
                let h_src_eff = (h_src.max(0) as u32).min(sh.saturating_sub(y_src));
                if w_dst > 0 && h_dst > 0 && w_src_eff > 0 && h_src_eff > 0 {
                    for dy in 0..h_dst {
                        let sy = y_src + (dy * h_src_eff) / h_dst;
                        for dx in 0..w_dst {
                            let sx = x_src + (dx * w_src_eff) / w_dst;
                            let off = (sy * sw + sx) as usize * 2;
                            if off + 1 < src_pix.len() {
                                let px = u16::from_le_bytes([src_pix[off], src_pix[off + 1]]);
                                dst.put_pixel(x_dst + dx as i32, y_dst + dy as i32, px);
                            }
                        }
                    }
                    dst.mark_dirty();
                }
            }
        }
    }
    Ok(DispatchOutcome::ReturnedR0(h_dst.max(0) as u32))
}

/// Internal helper used by `SetDIBitsToDevice` and the no-stretch
/// path of `StretchDIBits`.
#[allow(clippy::too_many_arguments)]
fn blit_dib(
    ctx: &mut CallCtx<'_>,
    hdc: u32,
    x_dst: i32,
    y_dst: i32,
    w: u32,
    h: u32,
    x_src: i32,
    y_src: i32,
    c_lines: u32,
    p_bits: u32,
    p_bmi: u32,
) -> Result<(), KernelError> {
    let lines = c_lines.max(h);
    let pixels = match decode_dib(ctx, p_bmi, p_bits)? {
        Some(t) => t,
        None => return Ok(()),
    };
    let (src_pix, sw, _sh) = pixels;
    if let Some(mut dst) = surface_for_dc(ctx.kernel, hdc) {
        for row in 0..lines {
            let sy = (y_src + row as i32).max(0) as u32;
            for col in 0..w {
                let sx = (x_src + col as i32).max(0) as u32;
                let off = (sy * sw + sx) as usize * 2;
                if off + 1 < src_pix.len() {
                    let px = u16::from_le_bytes([src_pix[off], src_pix[off + 1]]);
                    dst.put_pixel(x_dst + col as i32, y_dst + row as i32, px);
                }
            }
        }
        dst.mark_dirty();
    }
    Ok(())
}

/// Decode a guest BITMAPINFO + pixel buffer into a host-side
/// `(Vec<u8>, width, height)` of RGB565. Returns `None` for
/// malformed or unsupported headers.
fn decode_dib(
    ctx: &mut CallCtx<'_>,
    p_bmi: u32,
    p_bits: u32,
) -> Result<Option<(Vec<u8>, u32, u32)>, KernelError> {
    if p_bmi == 0 || p_bits == 0 {
        return Ok(None);
    }
    let hdr = ctx.cpu.read_mem(p_bmi, 40)?;
    let bi_size = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if bi_size < 40 {
        return Ok(None);
    }
    let bi_width = i32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let bi_height = i32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let bi_bpp = u16::from_le_bytes([hdr[14], hdr[15]]);
    let bi_compression = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
    let bi_colors_used = u32::from_le_bytes([hdr[32], hdr[33], hdr[34], hdr[35]]);
    if bi_width <= 0 || bi_height == 0 || bi_compression > 3 {
        return Ok(None);
    }
    let width = bi_width as u32;
    let bottom_up = bi_height > 0;
    let height = bi_height.unsigned_abs();
    let row_bytes = match bi_bpp {
        1 => width.div_ceil(8),
        4 => width.div_ceil(2),
        8 => width,
        16 => width * 2,
        24 => width * 3,
        32 => width * 4,
        _ => return Ok(None),
    };
    let row_stride = (row_bytes + 3) & !3;
    let palette_entries = match bi_bpp {
        1 | 4 | 8 => {
            if bi_colors_used == 0 {
                1u32 << bi_bpp
            } else {
                bi_colors_used
            }
        }
        _ => 0,
    };
    let mut palette_565 = Vec::with_capacity(palette_entries as usize);
    if palette_entries > 0 {
        let pal_bytes = ctx
            .cpu
            .read_mem(p_bmi + bi_size, palette_entries * 4)
            .unwrap_or_default();
        for i in 0..palette_entries as usize {
            let p = i * 4;
            if p + 3 < pal_bytes.len() {
                palette_565.push(pocket_kernel::framebuffer::pack_rgb565(
                    pal_bytes[p + 2],
                    pal_bytes[p + 1],
                    pal_bytes[p],
                ));
            } else {
                palette_565.push(0);
            }
        }
    }
    let raw = ctx.cpu.read_mem(p_bits, row_stride * height)?;
    let mut out = vec![0u8; (width * height * 2) as usize];
    for src_y in 0..height {
        let dst_y = if bottom_up { height - 1 - src_y } else { src_y };
        let row_off = (src_y * row_stride) as usize;
        let dst_row = (dst_y * width * 2) as usize;
        for x in 0..width {
            let rgb = match bi_bpp {
                8 => {
                    let idx = raw[row_off + x as usize] as usize;
                    *palette_565.get(idx).unwrap_or(&0)
                }
                4 => {
                    let b = raw[row_off + (x as usize) / 2];
                    let nib = if x & 1 == 0 { b >> 4 } else { b & 0x0F };
                    *palette_565.get(nib as usize).unwrap_or(&0)
                }
                1 => {
                    let b = raw[row_off + (x as usize) / 8];
                    let bit = 7 - (x & 7);
                    let v = ((b >> bit) & 1) as usize;
                    *palette_565.get(v).unwrap_or(&0)
                }
                16 => u16::from_le_bytes([
                    raw[row_off + x as usize * 2],
                    raw[row_off + x as usize * 2 + 1],
                ]),
                24 => pocket_kernel::framebuffer::pack_rgb565(
                    raw[row_off + x as usize * 3 + 2],
                    raw[row_off + x as usize * 3 + 1],
                    raw[row_off + x as usize * 3],
                ),
                32 => pocket_kernel::framebuffer::pack_rgb565(
                    raw[row_off + x as usize * 4 + 2],
                    raw[row_off + x as usize * 4 + 1],
                    raw[row_off + x as usize * 4],
                ),
                _ => 0,
            };
            let off = dst_row + (x as usize) * 2;
            out[off] = rgb as u8;
            out[off + 1] = (rgb >> 8) as u8;
        }
    }
    Ok(Some((out, width, height)))
}

/// `COLORREF GetPixel(HDC, int x, int y)`. Reads the destination
/// surface at `(x, y)` and converts the RGB565 pixel back to a
/// COLORREF (`0x00BBGGRR`). Returns `CLR_INVALID` (`0xFFFFFFFF`)
/// for out-of-range reads.
fn get_pixel(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hdc = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(1)? as i32;
    let y = ctx.arg_u32(2)? as i32;
    if let Some(surf) = surface_for_dc(ctx.kernel, hdc) {
        let (sw, sh) = surf.dimensions();
        if x < 0 || y < 0 || (x as u32) >= sw || (y as u32) >= sh {
            return Ok(DispatchOutcome::ReturnedR0(0xFFFF_FFFF));
        }
        let off = (y as u32 * sw + x as u32) as usize * 2;
        let pix = surf.pixels();
        if off + 1 >= pix.len() {
            return Ok(DispatchOutcome::ReturnedR0(0xFFFF_FFFF));
        }
        let p = u16::from_le_bytes([pix[off], pix[off + 1]]);
        let r = (((p >> 11) & 0x1f) as u32 * 255 / 31) & 0xff;
        let g = (((p >> 5) & 0x3f) as u32 * 255 / 63) & 0xff;
        let b = ((p & 0x1f) as u32 * 255 / 31) & 0xff;
        Ok(DispatchOutcome::ReturnedR0(b << 16 | g << 8 | r))
    } else {
        Ok(DispatchOutcome::ReturnedR0(0xFFFF_FFFF))
    }
}

/// `COLORREF SetPixel(HDC, int x, int y, COLORREF cr)`. Writes the
/// pixel and returns the previous COLORREF (or the new one if the
/// surface was empty).
fn set_pixel(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hdc = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(1)? as i32;
    let y = ctx.arg_u32(2)? as i32;
    let cr = ctx.arg_u32(3)?;
    if let Some(mut surf) = surface_for_dc(ctx.kernel, hdc) {
        surf.put_pixel(x, y, colorref_to_rgb565(cr));
        surf.mark_dirty();
    }
    Ok(DispatchOutcome::ReturnedR0(cr))
}

/// `DWORD GetSysColor(int nIndex)`. We map a small subset to
/// reasonable Pocket PC defaults; everything else falls back to
/// silver.
fn get_sys_color(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let idx = ctx.arg_u32(0)? as i32;
    let cr = match idx {
        // COLOR_SCROLLBAR / COLOR_BACKGROUND / COLOR_INACTIVECAPTION
        0..=2 => 0x00C8C8C8,
        // COLOR_ACTIVECAPTION / COLOR_MENU
        3 | 4 => 0x00FFFFFF,
        // COLOR_WINDOW
        5 => 0x00FFFFFF,
        // COLOR_WINDOWFRAME / COLOR_MENUTEXT / COLOR_WINDOWTEXT /
        // COLOR_CAPTIONTEXT / COLOR_BTNTEXT
        6 | 7 | 8 | 9 | 18 => 0x00000000,
        // COLOR_ACTIVEBORDER / COLOR_INACTIVEBORDER
        10 | 11 => 0x00808080,
        // COLOR_APPWORKSPACE / COLOR_HIGHLIGHT
        12 => 0x00C0C0C0,
        13 => 0x00FF0000,
        // COLOR_HIGHLIGHTTEXT / COLOR_BTNFACE
        14 => 0x00FFFFFF,
        15 => 0x00C8C8C8,
        // COLOR_BTNSHADOW / COLOR_GRAYTEXT
        16 => 0x00808080,
        17 => 0x00808080,
        // Anything else — silver.
        _ => 0x00C8C8C8,
    };
    Ok(DispatchOutcome::ReturnedR0(cr))
}

/// `HBRUSH GetSysColorBrush(int nIndex)` — return a stable stock
/// handle so subsequent `SelectObject` calls have something to do.
fn get_sys_color_brush(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let idx = ctx.arg_u32(0)? as i32;
    let h = match idx {
        4 | 5 | 14 => STOCK_WHITE_BRUSH,
        6 | 7 | 8 | 9 | 18 => STOCK_BLACK_BRUSH,
        _ => STOCK_WHITE_BRUSH,
    };
    Ok(DispatchOutcome::ReturnedR0(h))
}

// ---------- Window helpers -----------------------------------------

const FAKE_DESKTOP_HWND: u32 = 0xDEAD_DE5C;

fn get_desktop_window(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_DESKTOP_HWND))
}

fn get_active_window(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_HWND))
}

fn get_foreground_window(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_HWND))
}

fn set_foreground_window(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_parent(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(FAKE_DESKTOP_HWND))
}

/// `HWND GetWindow(HWND, UINT)` — for synthetic windows we have no
/// Z-order, so always say "no neighbour" (`NULL`).
fn get_window(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

// ---------- Time helpers -------------------------------------------

/// `DWORD timeGetTime(void)` — millisecond tick count. Reuses the
/// same counter as `GetTickCount` so the two stay consistent.
fn time_get_time(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    get_tick_count(ctx)
}

/// `BOOL SystemTimeToFileTime(const SYSTEMTIME *lpSystemTime,
///                              LPFILETIME lpFileTime)`.
/// Encodes the SYSTEMTIME (16 bytes) into a 64-bit FILETIME measured
/// in 100-ns ticks since 1601-01-01 UTC. Pocket PC games use this
/// to time stamp save files.
fn system_time_to_file_time(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p_st = ctx.arg_u32(0)?;
    let p_ft = ctx.arg_u32(1)?;
    if p_st == 0 || p_ft == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let st = ctx.cpu.read_mem(p_st, 16)?;
    let year = u16::from_le_bytes([st[0], st[1]]) as i32;
    let month = u16::from_le_bytes([st[2], st[3]]) as i32;
    let day = u16::from_le_bytes([st[6], st[7]]) as i32;
    let hour = u16::from_le_bytes([st[8], st[9]]) as i64;
    let minute = u16::from_le_bytes([st[10], st[11]]) as i64;
    let second = u16::from_le_bytes([st[12], st[13]]) as i64;
    let millis = u16::from_le_bytes([st[14], st[15]]) as i64;
    let days = days_from_civil(year, month, day) - days_from_civil(1601, 1, 1);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    let ticks: u64 = secs as u64 * 10_000_000 + millis as u64 * 10_000;
    ctx.cpu.write_mem(p_ft, &ticks.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL FileTimeToSystemTime(const FILETIME *lpFileTime,
///                              LPSYSTEMTIME lpSystemTime)`.
fn file_time_to_system_time(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p_ft = ctx.arg_u32(0)?;
    let p_st = ctx.arg_u32(1)?;
    if p_ft == 0 || p_st == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let ft = ctx.cpu.read_mem(p_ft, 8)?;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&ft);
    let ticks = u64::from_le_bytes(bytes);
    let secs_total = ticks / 10_000_000;
    let millis = ((ticks % 10_000_000) / 10_000) as u16;
    let secs_in_day = (secs_total % 86_400) as i64;
    let days = (secs_total / 86_400) as i64 + days_from_civil(1601, 1, 1);
    let (year, month, day) = civil_from_days(days);
    let hour = (secs_in_day / 3600) as u16;
    let minute = ((secs_in_day % 3600) / 60) as u16;
    let second = (secs_in_day % 60) as u16;
    let dow = ((days + 1) % 7).rem_euclid(7) as u16;
    let mut buf = [0u8; 16];
    buf[0..2].copy_from_slice(&(year as u16).to_le_bytes());
    buf[2..4].copy_from_slice(&(month as u16).to_le_bytes());
    buf[4..6].copy_from_slice(&dow.to_le_bytes());
    buf[6..8].copy_from_slice(&(day as u16).to_le_bytes());
    buf[8..10].copy_from_slice(&hour.to_le_bytes());
    buf[10..12].copy_from_slice(&minute.to_le_bytes());
    buf[12..14].copy_from_slice(&second.to_le_bytes());
    buf[14..16].copy_from_slice(&millis.to_le_bytes());
    ctx.cpu.write_mem(p_st, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// Howard Hinnant's days_from_civil — `days since 1970-01-01` for
/// any (y, m, d) in the proleptic Gregorian calendar.
fn days_from_civil(y: i32, m: i32, d: i32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + (d - 1)) as i64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i32, i32, i32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as i32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

// ---------- Menu APIs ----------------------------------------------

/// `HMENU LoadMenuW(HINSTANCE, LPCWSTR lpMenuName)` — return a fresh
/// menu handle. We don't actually parse the menu resource (the games
/// just probe items via `GetSubMenu`/`CheckMenuItem`), but we do
/// register the handle in `KernelState::menus` so later state queries
/// work.
fn load_menu_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hinst = ctx.arg_u32(0)?;
    let _name = ctx.arg_u32(1)?;
    let h = ctx.kernel.next_menu_handle;
    ctx.kernel.next_menu_handle = ctx.kernel.next_menu_handle.wrapping_add(1);
    ctx.kernel.menus.insert(h, std::collections::HashMap::new());
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn create_menu(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.kernel.next_menu_handle;
    ctx.kernel.next_menu_handle = ctx.kernel.next_menu_handle.wrapping_add(1);
    ctx.kernel.menus.insert(h, std::collections::HashMap::new());
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn destroy_menu(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let removed = ctx.kernel.menus.remove(&h).is_some();
    // Drop any cached sub-menu mappings whose parent is `h`, but keep
    // sub-menu state itself around — the guest may still hold the
    // child handle and CheckMenuItem it.
    ctx.kernel.sub_menus.retain(|(k, _), _| *k != h);
    Ok(DispatchOutcome::ReturnedR0(if removed { 1 } else { 0 }))
}

/// `HMENU GetSubMenu(HMENU, int nPos)` — return a stable child
/// handle. Cached so successive calls with the same `(menu, pos)`
/// give the same value.
fn get_sub_menu(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let pos = ctx.arg_u32(1)?;
    if let Some(&cached) = ctx.kernel.sub_menus.get(&(h, pos)) {
        return Ok(DispatchOutcome::ReturnedR0(cached));
    }
    let new = ctx.kernel.next_menu_handle;
    ctx.kernel.next_menu_handle = ctx.kernel.next_menu_handle.wrapping_add(1);
    ctx.kernel
        .menus
        .insert(new, std::collections::HashMap::new());
    ctx.kernel.sub_menus.insert((h, pos), new);
    Ok(DispatchOutcome::ReturnedR0(new))
}

fn get_menu_item_count(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let n = ctx
        .kernel
        .menus
        .get(&h)
        .map(|m| m.len() as u32)
        .unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(n))
}

fn get_menu_item_id(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let pos = ctx.arg_u32(1)?;
    let m = match ctx.kernel.menus.get(&h) {
        Some(m) => m,
        None => return Ok(DispatchOutcome::ReturnedR0(0xFFFF_FFFF)),
    };
    let mut keys: Vec<&u32> = m.keys().collect();
    keys.sort();
    let id = keys
        .get(pos as usize)
        .copied()
        .copied()
        .unwrap_or(0xFFFF_FFFF);
    Ok(DispatchOutcome::ReturnedR0(id))
}

/// `BOOL CheckMenuItem(HMENU, UINT uIDCheckItem, UINT uCheck)` —
/// returns the previous flags value, or `0xFFFFFFFF` if `uIDCheckItem`
/// is unknown. We implement the toggle by remembering the latest
/// MF_CHECKED bit per (menu, id).
fn check_menu_item(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    let new = ctx.arg_u32(2)?;
    let prev = ctx
        .kernel
        .menus
        .get(&h)
        .and_then(|m| m.get(&id))
        .copied()
        .unwrap_or(0);
    ctx.kernel.menus.entry(h).or_default().insert(id, new);
    Ok(DispatchOutcome::ReturnedR0(prev))
}

fn enable_menu_item(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Pretend the previous state was "enabled".
    check_menu_item(ctx)
}

fn get_menu_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    let _flags = ctx.arg_u32(2)?;
    let v = ctx
        .kernel
        .menus
        .get(&h)
        .and_then(|m| m.get(&id))
        .copied()
        .unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(v))
}

fn append_menu(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let _flags = ctx.arg_u32(1)?;
    let id = ctx.arg_u32(2)?;
    ctx.kernel.menus.entry(h).or_default().insert(id, 0);
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn remove_menu_item(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    if let Some(m) = ctx.kernel.menus.get_mut(&h) {
        m.remove(&id);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn track_popup_menu(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn modify_menu_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    let _flags = ctx.arg_u32(2)?;
    ctx.kernel.menus.entry(h).or_default().insert(id, 0);
    Ok(DispatchOutcome::ReturnedR0(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    use pocket_kernel::{vfs::Vfs, Heap, KernelState, Thunk};
    use pocket_pe::ImportBinding;

    fn fresh_kernel() -> KernelState {
        use pocket_kernel::audio::{AudioEngine, GuestFormat};
        use pocket_kernel::{Framebuffer, GdiState};

        KernelState {
            heap: Heap::new(0x5000_0000, 0x10000),
            vfs: Vfs::new(),
            framebuffer: Framebuffer::default(),
            gdi: GdiState::new(),
            resources: vec![],
            image_base: 0,
            dynamic_exports: std::collections::HashMap::new(),
            next_module_handle: 0x1000_0001,
            fb_mapped: false,
            gx_readback_scratch: Vec::new(),
            mem_op_scratch: Vec::new(),
            mem_op_scratch_b: Vec::new(),
            bit_blt_src_scratch: Vec::new(),
            dib_sync_scratch: Vec::new(),
            dib_decode_scratch: Vec::new(),
            gx_last_pushed_counter: 0,
            synthetic_message_count: 0,
            synthetic_message_budget: 240,
            wnd_proc: 0,
            window_user_data: 0,
            synthetic_timer_id: 0,
            synthetic_timer_interval_ms: 16,
            synthetic_timer_next_ms: 0,
            synthetic_paint_next_ms: 0,
            synthetic_create_sent: false,
            pending_input: std::collections::VecDeque::new(),
            pending_message: None,
            threads: Vec::new(),
            current_thread: 0,
            pressed_keys: [false; 256],
            should_stop: false,
            tls_slots_used: 0,
            vector_iter_frames: std::collections::HashMap::new(),
            security_cookie: 0,
            audio: AudioEngine::new(),
            wave_out_format: GuestFormat::default(),
            menus: std::collections::HashMap::new(),
            next_menu_handle: 0xDEAD_2000,
            sub_menus: std::collections::HashMap::new(),
        }
    }

    fn dummy_thunk() -> Thunk {
        Thunk {
            thunk_va: 0x70000000,
            iat_va: 0x20000,
            dll: "coredll.dll".into(),
            binding: ImportBinding::Name("test".into()),
            friendly_name: None,
        }
    }

    #[test]
    fn strlen_walks_until_null() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_mem(0x1000, b"hello\0").unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = strlen(&mut c).unwrap();
        match r {
            DispatchOutcome::ReturnedR0(v) => assert_eq!(v, 5),
            _ => panic!(),
        }
    }

    #[test]
    fn setjmp_then_longjmp_restores_state() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        c.cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        c.cpu.write_reg(ArmReg::R4, 0xCAFE).unwrap();
        c.cpu.write_reg(ArmReg::Lr, 0xBADC0DE).unwrap();
        let _ = setjmp(&mut c).unwrap();
        // Trash registers so we can prove longjmp restores them.
        c.cpu.write_reg(ArmReg::R4, 0).unwrap();
        c.cpu.write_reg(ArmReg::Lr, 0).unwrap();
        c.cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        c.cpu.write_reg(ArmReg::R1, 42).unwrap();
        let r = longjmp(&mut c).unwrap();
        match r {
            DispatchOutcome::ReturnedR0(v) => assert_eq!(v, 42),
            _ => panic!(),
        }
        assert_eq!(c.cpu.read_reg(ArmReg::R4).unwrap(), 0xCAFE);
        assert_eq!(c.cpu.read_reg(ArmReg::Lr).unwrap(), 0xBADC0DE);
    }

    #[test]
    fn wcslen_counts_until_null() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let s: Vec<u8> = "hi\0"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        cpu.write_mem(0x1000, &s).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = wcslen(&mut c).unwrap();
        match r {
            DispatchOutcome::ReturnedR0(v) => assert_eq!(v, 2),
            _ => panic!(),
        }
    }

    #[test]
    fn malloc_then_free_round_trips() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x5000_0000, 0x10000, Prot::READ | Prot::WRITE)
            .unwrap();
        let initial_free = kernel.heap.free_bytes();
        cpu.write_reg(ArmReg::R0, 64).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let p = match malloc(&mut c).unwrap() {
            DispatchOutcome::ReturnedR0(p) => p,
            _ => panic!(),
        };
        assert!(p >= 0x5000_0000);
        c.cpu.write_reg(ArmReg::R0, p).unwrap();
        let _ = free(&mut c).unwrap();
        assert_eq!(c.kernel.heap.free_bytes(), initial_free);
    }

    #[test]
    fn create_file_w_with_no_mount_is_invalid() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.map_region(0x2000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        // Write a wide-string "\X\foo.txt" at 0x1000.
        let s: Vec<u8> = "\\X\\foo.txt\0"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        cpu.write_mem(0x1000, &s).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        cpu.write_reg(ArmReg::R1, 0x8000_0000).unwrap(); // GENERIC_READ
        cpu.write_reg(ArmReg::Sp, 0x2800).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = create_file_w(&mut c).unwrap();
        assert_eq!(r, DispatchOutcome::ReturnedR0(INVALID_HANDLE_VALUE));
    }

    #[test]
    fn create_file_w_with_mount_returns_real_handle() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        kernel.vfs.mount("\\App\\", dir.path());
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.map_region(0x2000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let s: Vec<u8> = "\\App\\hello.txt\0"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        cpu.write_mem(0x1000, &s).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        cpu.write_reg(ArmReg::R1, 0x8000_0000).unwrap(); // GENERIC_READ
        cpu.write_reg(ArmReg::Sp, 0x2800).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = create_file_w(&mut c).unwrap();
        match r {
            DispatchOutcome::ReturnedR0(h) => {
                assert_ne!(h, INVALID_HANDLE_VALUE);
                assert!(c.kernel.vfs.is_open(h));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn get_file_attributes_w_null_pointer_is_invalid() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.write_reg(ArmReg::R0, 0).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = get_file_attributes_w(&mut c).unwrap();
        assert_eq!(r, DispatchOutcome::ReturnedR0(0xFFFF_FFFF));
    }

    #[test]
    fn get_file_attributes_w_unmounted_prefix_is_invalid() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let s: Vec<u8> = "\\Nope\\foo.txt\0"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        cpu.write_mem(0x1000, &s).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = get_file_attributes_w(&mut c).unwrap();
        assert_eq!(r, DispatchOutcome::ReturnedR0(0xFFFF_FFFF));
    }

    #[test]
    fn get_file_attributes_w_returns_normal_for_real_file() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        kernel.vfs.mount("\\App\\", dir.path());
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let s: Vec<u8> = "\\App\\hello.txt\0"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        cpu.write_mem(0x1000, &s).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = get_file_attributes_w(&mut c).unwrap();
        assert_eq!(r, DispatchOutcome::ReturnedR0(0x0000_0080));
    }

    #[test]
    fn get_file_attributes_w_returns_directory_for_real_dir() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sounds")).unwrap();
        kernel.vfs.mount("\\App\\", dir.path());
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let s: Vec<u8> = "\\App\\sounds\0"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        cpu.write_mem(0x1000, &s).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = get_file_attributes_w(&mut c).unwrap();
        assert_eq!(r, DispatchOutcome::ReturnedR0(0x0000_0010));
    }

    #[test]
    fn get_file_attributes_w_missing_file_is_invalid() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let dir = tempfile::tempdir().unwrap();
        kernel.vfs.mount("\\App\\", dir.path());
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let s: Vec<u8> = "\\App\\does-not-exist.txt\0"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        cpu.write_mem(0x1000, &s).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = get_file_attributes_w(&mut c).unwrap();
        assert_eq!(r, DispatchOutcome::ReturnedR0(0xFFFF_FFFF));
    }

    // ---- GDI handler tests ----

    #[test]
    fn fill_rect_paints_into_framebuffer() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        // RECT { 5, 7, 25, 27 }
        let mut rect = Vec::new();
        rect.extend_from_slice(&5i32.to_le_bytes());
        rect.extend_from_slice(&7i32.to_le_bytes());
        rect.extend_from_slice(&25i32.to_le_bytes());
        rect.extend_from_slice(&27i32.to_le_bytes());
        cpu.write_mem(0x1000, &rect).unwrap();

        // Allocate a brush.
        cpu.write_reg(ArmReg::R0, 0x00ff0000).unwrap(); // COLORREF: red
        let t = dummy_thunk();
        let hbr = {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            match create_solid_brush(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(h) => h,
                _ => panic!(),
            }
        };
        // FillRect(GDI_SCREEN_DC, 0x1000, hbr).
        cpu.write_reg(ArmReg::R0, GDI_SCREEN_DC).unwrap();
        cpu.write_reg(ArmReg::R1, 0x1000).unwrap();
        cpu.write_reg(ArmReg::R2, hbr).unwrap();
        let pre = kernel.framebuffer.frame_counter;
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = fill_rect(&mut c).unwrap();
        assert_eq!(r, DispatchOutcome::ReturnedR0(1));
        assert!(kernel.framebuffer.frame_counter > pre);
        // Pixel at (5,7) must now be non-zero (red 0xF800 in RGB565,
        // little-endian on the wire).
        let off = (7 * pocket_kernel::framebuffer::FB_WIDTH as usize + 5) * 2;
        assert_ne!(kernel.framebuffer.pixels[off], 0);
    }
}
