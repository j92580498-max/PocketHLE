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

use image::GenericImageView;
use pocket_cpu::regs::ArmReg;
use pocket_cpu::Prot;
use pocket_kernel::controls::{ControlAction, ControlClass, Controls};
use pocket_kernel::framebuffer::colorref_to_rgb565;
use pocket_kernel::gdi::{
    rop3, Surface, GDI_SCREEN_DC, STOCK_BLACK_BRUSH, STOCK_BLACK_PEN, STOCK_DKGRAY_BRUSH,
    STOCK_GRAY_BRUSH, STOCK_LTGRAY_BRUSH, STOCK_NULL_BRUSH, STOCK_NULL_PEN, STOCK_SYSTEM_FONT,
    STOCK_WHITE_BRUSH, STOCK_WHITE_PEN,
};
use pocket_kernel::msgbox::MessageBox;
use pocket_kernel::{
    module_file_name, CreateStage, DispatchOutcome, GuestCallFrame, GuestThread, InputEvent,
    KernelError, KernelState, LoadedModule, ModalDialog, QsortFrame, VectorIterFrame,
    WaveCallbackKind, DEFAULT_STACK_TOP, FAKE_CURRENT_PROCESS_HANDLE, FAKE_CURRENT_THREAD_HANDLE,
    HSS_MODULE_HANDLE, MODULE_REGION_END, MODULE_REGION_STRIDE, PROCESS_INSTANCE_HANDLE,
    SLOT_ALIAS_BASE, SYNTHETIC_FRAMEBUFFER_BASE, THREAD_EXIT_TRAMPOLINE_BASE, TLS_SLOT_COUNT,
    USER_KDATA_TLS_ARRAY_VA,
};
use pocket_pe::{ResourceEntry, ResourceKey};

use crate::{CallCtx, WinCeDispatcher};

/// Must stay identical to the `hInstance` the kernel hands the guest
/// entry point, otherwise `hInstance == GetModuleHandle(NULL)` checks
/// inside the game fail.
const FAKE_MODULE_HANDLE: u32 = PROCESS_INSTANCE_HANDLE;
const FAKE_HWND: u32 = 0xDEAD_0001;
/// Handle for the modeless dialog a title creates over its main window
/// through `CreateDialogIndirectParamW`. Deliberately distinct from
/// [`FAKE_HWND`]: Solitaire keeps the frame window and the board dialog
/// in two different globals and drives them independently — a shared
/// handle makes `GetWindowRect` on one return the other's geometry.
const FAKE_DIALOG_HWND: u32 = 0xDEAD_0002;
/// Handle for a *modal* dialog running under `DialogBoxIndirectParamW`.
///
/// Distinct from [`FAKE_DIALOG_HWND`] because a modal dialog is
/// routinely raised *over* a modeless one — Solitaire's Options dialog
/// goes up while its docked button strip is still on screen — and the
/// two sets of controls must not be mistaken for each other.
const FAKE_MODAL_HWND: u32 = 0xDEAD_0003;
/// Handle for the commctrl status bar docked to the bottom of the main
/// window (`CreateStatusWindow{A,W}`). The guest addresses it only
/// through `SendMessage`, and those `SB_*` messages must be consumed by
/// the control rather than trampolined into the app's own WndProc —
/// hence a handle distinct from [`FAKE_HWND`].
pub const FAKE_STATUSBAR_HWND: u32 = 0xDEAD_0C01;
const INVALID_HANDLE_VALUE: u32 = 0xFFFF_FFFF;

/// Every window handle we ever hand the guest. Handlers that answer
/// questions *about* a window (`IsWindow`, `IsWindowVisible`, ...) have
/// to accept all of them — a check against [`FAKE_HWND`] alone makes the
/// guest treat its own dialog as destroyed and skip the work it drives.
fn is_live_hwnd(hwnd: u32) -> bool {
    hwnd == FAKE_HWND
        || hwnd == FAKE_DIALOG_HWND
        || hwnd == FAKE_MODAL_HWND
        || hwnd == FAKE_DESKTOP_HWND
        || hwnd == FAKE_STATUSBAR_HWND
        || Controls::is_child_hwnd(hwnd)
}
const PAINTSTRUCT_BYTES: u32 = 32;

pub fn register(d: &mut WinCeDispatcher) {
    let dll = "coredll.dll";

    // ---- Process / module / library ----
    d.register_handler(dll, "GetTickCount", get_tick_count);
    d.register_handler(dll, "Sleep", sleep);
    d.register_handler(dll, "SuspendThread", suspend_thread);
    d.register_handler(dll, "ResumeThread", resume_thread);
    d.register_handler(dll, "ExitProcess", exit_process);
    d.register_handler(dll, "TerminateProcess", exit_process);
    d.register_constant(dll, "GetLastError", 0, zero_returning);
    d.register_constant(dll, "SetLastError", 0, zero_returning);
    d.register_handler(dll, "GetCommandLineW", get_command_line_w);
    d.register_handler(dll, "GetModuleHandleW", get_module_handle_w);
    d.register_handler(dll, "GetModuleFileNameW", get_module_file_name_w);
    d.register_handler(dll, "GetFileVersionInfoSizeW", get_file_version_info_size_w);
    d.register_handler(dll, "GetFileVersionInfoW", get_file_version_info_w);
    d.register_handler(dll, "VerQueryValueW", ver_query_value_w);
    d.register_handler(dll, "GetModuleInformation", get_module_information);
    d.register_handler(dll, "GetProcAddress", get_proc_address_a);
    d.register_handler(dll, "GetProcAddressA", get_proc_address_a);
    d.register_handler(dll, "LoadLibraryW", load_library_w);
    d.register_handler(dll, "DecompressImageIndirect", decompress_image_indirect);
    d.register_handler(dll, "#1", decompress_image_indirect);
    d.register_handler(dll, "_strupr", strupr);
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
    d.register_handler(dll, "__stoi64", soft_stoi64);
    d.register_handler(dll, "__stou64", soft_stou64);
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
    d.register_handler(dll, "__i64tod", soft_i64tod);
    d.register_handler(dll, "__u64tod", soft_u64tod);
    d.register_handler(dll, "__i64tos", soft_i64tos);
    d.register_handler(dll, "__u64tos", soft_u64tos);
    d.register_handler(dll, "__dtoi64", soft_dtoi64);
    d.register_handler(dll, "__dtou64", soft_dtou64);

    // ---- Memory / string CRT ----
    d.register_handler(dll, "memset", memset);
    d.register_handler(dll, "memcpy", memcpy);
    d.register_handler(dll, "memmove", memcpy);
    d.register_handler(dll, "memchr", memchr);
    d.register_handler(dll, "memcmp", memcmp);
    d.register_handler(dll, "strlen", strlen);
    d.register_handler(dll, "wcslen", wcslen);
    d.register_handler(dll, "strcpy", strcpy);
    d.register_handler(dll, "strncpy", strncpy);
    d.register_handler(dll, "strcat", strcat);
    d.register_handler(dll, "strncat", strncat);
    d.register_handler(dll, "strcmp", strcmp);
    d.register_handler(dll, "strncmp", strncmp);
    // Case-insensitive compares. Leaving these unimplemented used to
    // return `0` — i.e. "the strings are equal" — which silently sent
    // games down the wrong branch (Sonic Unleashed picked the wrong
    // asset descriptor and then dereferenced a null object).
    d.register_handler(dll, "_stricmp", stricmp);
    d.register_handler(dll, "_strcmpi", stricmp);
    d.register_handler(dll, "_strnicmp", strnicmp);
    d.register_handler(dll, "_strncmpi", strnicmp);
    d.register_handler(dll, "atoi", atoi_handler);
    d.register_handler(dll, "atol", atoi_handler);
    d.register_handler(dll, "atof", atof_handler);
    d.register_handler(dll, "_itoa", itoa_handler);
    d.register_handler(dll, "_itow", itow_handler);
    d.register_handler(dll, "_isctype", isctype);
    d.register_handler(dll, "strchr", strchr);
    d.register_handler(dll, "strrchr", strrchr);
    d.register_handler(dll, "strstr", strstr);
    d.register_handler(dll, "strspn", strspn);
    d.register_handler(dll, "strcspn", strcspn);
    d.register_handler(dll, "strpbrk", strpbrk);
    d.register_handler(dll, "strtok", strtok);
    d.register_handler(dll, "_strdup", strdup);
    d.register_handler(dll, "tolower", tolower);
    d.register_handler(dll, "toupper", toupper);
    d.register_handler(dll, "wcscpy", wcscpy);
    d.register_handler(dll, "wcsncpy", wcsncpy);
    d.register_handler(dll, "wcscat", wcscat);
    d.register_handler(dll, "wcsncat", wcsncat);
    d.register_handler(dll, "wcscmp", wcscmp);
    d.register_handler(dll, "wcsncmp", wcsncmp);
    d.register_handler(dll, "wcsnicmp", wcsnicmp);
    d.register_handler(dll, "_wcsnicmp", wcsnicmp);
    d.register_handler(dll, "_wcsicmp", wcsicmp);
    d.register_handler(dll, "_wcsdup", wcsdup);
    d.register_handler(dll, "wcschr", wcschr);
    d.register_handler(dll, "wcsrchr", wcsrchr);
    d.register_handler(dll, "wcspbrk", wcspbrk);
    d.register_handler(dll, "wcstok", wcstok);
    d.register_handler(dll, "wcsstr", wcsstr);
    d.register_handler(dll, "_wtol", wtol);
    d.register_handler(dll, "_wtoi", wtol);
    d.register_handler(dll, "CharUpperW", char_upper_w);
    d.register_handler(dll, "_wcsupr", char_upper_w);
    d.register_handler(dll, "CharLowerW", char_lower_w);
    d.register_handler(dll, "CharUpperA", char_upper_a);
    d.register_handler(dll, "CharLowerA", char_lower_a);
    d.register_handler(dll, "swprintf", swprintf);
    d.register_handler(dll, "wsprintfW", swprintf);
    d.register_handler(dll, "sprintf", sprintf);
    d.register_handler(dll, "printf", printf);
    d.register_handler(dll, "fprintf", crt_fprintf);
    d.register_handler(dll, "vfprintf", crt_vfprintf);
    d.register_handler(dll, "_getstdfilex", get_std_file);
    d.register_handler(dll, "_wfreopen", wfreopen);
    d.register_handler(dll, "freopen", freopen);
    d.register_handler(dll, "wsprintfA", sprintf);
    // CRT variadic printers — unimplemented before this PR, which made
    // the game pass uninitialized stack memory to subsequent code paths
    // (Zuma in particular feeds the result into a vector size that
    // then asks for a 2 GiB allocation).
    d.register_handler(dll, "vsprintf", vsprintf);
    d.register_handler(dll, "_vsnprintf", vsnprintf);
    d.register_handler(dll, "vsnprintf", vsnprintf);
    d.register_handler(dll, "_snprintf", snprintf);
    d.register_handler(dll, "sscanf", sscanf);
    d.register_handler(dll, "swscanf", swscanf);
    d.register_handler(dll, "_snwprintf", snwprintf);
    d.register_handler(dll, "vswprintf", vswprintf);
    d.register_handler(dll, "_vsnwprintf", vsnwprintf);
    d.register_handler(dll, "wcstombs", wcstombs);
    d.register_handler(dll, "mbstowcs", mbstowcs);
    // strsafe.h — the bounded printers Microsoft told CE developers to
    // use instead of the CRT ones. Solitaire formats its Time and Score
    // readouts with `StringCbPrintfW`, so leaving it unimplemented wrote
    // nothing into the buffer and both fields came out blank.
    d.register_handler(dll, "StringCbPrintfW", string_cb_printf_w);
    d.register_handler(dll, "StringCchPrintfW", string_cch_printf_w);
    d.register_handler(dll, "StringCbCopyW", string_cb_copy_w);
    d.register_handler(dll, "StringCbLengthW", string_cb_length_w);
    d.register_handler(dll, "StringCbCatW", string_cb_cat_w);
    d.register_handler(dll, "StringCbCopyNW", string_cb_copy_nw);
    d.register_handler(dll, "_wcslwr", wcslwr);
    d.register_handler(dll, "StringCbVPrintfW", string_cb_vprintf_w);
    d.register_handler(dll, "StringCchVPrintfW", string_cch_vprintf_w);
    // `NKDbgPrintfW` is the kernel debug printer. It has no observable
    // effect for the guest, but it is variadic, so a stub that returns
    // without consuming its arguments is fine while an unimplemented
    // call is not — the warning alone hid the missing printer above.
    d.register_handler(dll, "NKDbgPrintfW", nk_dbg_printf_w);

    // ---- File I/O backed by the VFS ----
    d.register_handler(dll, "CreateFileW", create_file_w);
    d.register_handler(dll, "ReadFile", read_file);
    d.register_handler(dll, "WriteFile", write_file);
    d.register_handler(dll, "FlushFileBuffers", flush_file_buffers);
    d.register_handler(dll, "CloseHandle", close_handle);
    d.register_handler(dll, "GetFileSize", get_file_size);
    d.register_handler(dll, "GlobalMemoryStatus", global_memory_status);
    d.register_handler(dll, "GetStoreInformation", get_store_information);
    d.register_handler(dll, "GetDiskFreeSpaceExW", get_disk_free_space_ex_w);
    d.register_handler(dll, "#323", get_store_information);
    d.register_handler(dll, "DeviceIoControl", device_io_control);
    d.register_handler(dll, "SetFilePointer", set_file_pointer);
    d.register_handler(dll, "FindFirstFileW", find_first_file_w);
    d.register_handler(dll, "FindNextFileW", find_next_file_w);
    d.register_handler(dll, "FindClose", find_close);
    d.register_handler(
        dll,
        "FindFirstChangeNotificationW",
        find_first_change_notification_w,
    );
    d.register_handler(
        dll,
        "FindNextChangeNotification",
        find_next_change_notification,
    );
    d.register_handler(
        dll,
        "FindCloseChangeNotification",
        find_close_change_notification,
    );
    d.register_handler(dll, "DeleteFileW", delete_file_w);
    d.register_constant(dll, "SetFileAttributesW", 1, one_returning);
    d.register_handler(dll, "GetFileAttributesW", get_file_attributes_w);
    d.register_handler(dll, "CreateDirectoryW", create_directory_w);
    d.register_handler(dll, "RemoveDirectoryW", remove_directory_w);
    d.register_constant(dll, "CopyFileW", 1, one_returning);
    d.register_handler(dll, "MoveFileW", move_file_w);
    d.register_constant(dll, "SetEndOfFile", 1, one_returning);
    d.register_constant(dll, "GetFileInformationByHandle", 0, zero_returning);
    d.register_constant(dll, "OpenProcess", 0, zero_returning);
    d.register_constant(dll, "GetExitCodeProcess", 1, one_returning);

    // ---- C-runtime style file I/O on top of the same VFS ----
    d.register_handler(dll, "fopen", crt_fopen);
    d.register_handler(dll, "_wfopen", crt_wfopen);
    d.register_handler(dll, "fclose", crt_fclose);
    d.register_handler(dll, "fread", crt_fread);
    d.register_handler(dll, "fwrite", crt_fwrite);
    d.register_handler(dll, "fseek", crt_fseek);
    d.register_handler(dll, "ftell", crt_ftell);
    d.register_handler(dll, "fgetpos", crt_fgetpos);
    d.register_handler(dll, "fsetpos", crt_fsetpos);
    d.register_handler(dll, "feof", crt_feof);
    d.register_handler(dll, "fflush", crt_fflush);
    d.register_handler(dll, "_fcloseall", crt_fcloseall);
    // CeGCC's C runtime startup (`crt3.c`) calls `_fpreset` before it
    // reaches `main`; MSVC-built images never do.
    d.register_handler(dll, "_fpreset", crt_fpreset);
    d.register_handler(dll, "fgetc", crt_fgetc);
    d.register_handler(dll, "fputc", crt_fputc);
    d.register_handler(dll, "fgets", crt_fgets);
    d.register_handler(dll, "fputs", crt_fputs);
    d.register_handler(dll, "fgetws", crt_fgetws);
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
    // Remainder counterparts. `timeGetTime`-style 64-bit millisecond
    // maths (Sonic Unleashed's SDL timer layer) divides and then takes
    // the remainder; returning 0 from the unimplemented stub made the
    // engine compute a zero frame delta forever.
    // The `…64by64` spellings the eVC/ARM CRT emits pass the operands
    // in natural AAPCS order (`r0:r1` = dividend, `r2:r3` = divisor),
    // unlike the 32-bit `__rt_sdiv` / `__rt_udiv` pair which really is
    // divisor-first. Observed in Sonic Unleashed, which computes
    // `FILETIME / 10000` as `__rt_udiv64by64(0x01dc7b59_072cbaf0,
    // 10000)`: reading it divisor-first made every clock read 0, so the
    // game froze on its splash screen with its timers never advancing.
    d.register_handler(dll, "__rt_urem64by64", rt_urem64by64);
    d.register_handler(dll, "__rt_srem64by64", rt_srem64by64);
    d.register_handler(dll, "__rt_urem64", rt_urem64);
    d.register_handler(dll, "__rt_srem64", rt_srem64);
    d.register_handler(dll, "__rt_urem", rt_urem);
    d.register_handler(dll, "__rt_srem", rt_srem);

    // ---- ANSI CRT helpers used by SDL-based ports ----
    d.register_handler(dll, "strtol", strtol_handler);
    d.register_handler(dll, "strtoul", strtol_handler);
    d.register_handler(dll, "isctype", isctype);

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
    d.register_handler(dll, "qsort", qsort);
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
    d.register_handler(dll, "??2@YAPAXI@Z", operator_new);
    d.register_handler(dll, "??3@YAXPAX@Z", free);
    d.register_handler(dll, "??_U@YAPAXI@Z", operator_new);
    d.register_handler(dll, "??_V@YAXPAX@Z", free);

    // ---- Resources ----
    d.register_handler(dll, "FindResourceW", find_resource_w);
    d.register_handler(dll, "LoadResource", load_resource);
    d.register_handler(dll, "LockResource", lock_resource);
    d.register_handler(dll, "SizeofResource", sizeof_resource);
    d.register_handler(dll, "LoadBitmapW", load_bitmap_w);
    d.register_handler(dll, "LoadImageW", load_bitmap_w);
    d.register_handler(dll, "SHLoadDIBitmap", sh_load_dibitmap);
    d.register_handler(dll, "GetObjectW", get_object_w);
    d.register_handler(dll, "LoadStringW", load_string_w);

    // ---- Window / message stubs ----
    d.register_handler(dll, "RegisterClassW", register_class_w);
    d.register_handler(dll, "CreateWindowExW", create_window_ex_w);
    d.register_handler(dll, "SetWindowLongW", set_window_long_w);
    d.register_handler(dll, "SetWindowLongA", set_window_long_w);
    d.register_handler(dll, "GetWindowLongW", get_window_long_w);
    d.register_handler(dll, "GetWindowLongA", get_window_long_w);
    d.register_handler(dll, "GetClassNameW", get_class_name_w);
    d.register_handler(dll, "GetDlgItem", get_dlg_item);
    d.register_handler(dll, "CheckRadioButton", check_radio_button);
    d.register_handler(dll, "EnumWindows", enum_windows);
    d.register_handler(dll, "IsWindowVisible", is_window_visible);
    d.register_handler(dll, "IsWindowEnabled", is_window_enabled);
    d.register_handler(
        dll,
        "GetWindowThreadProcessId",
        get_window_thread_process_id,
    );
    d.register_handler(dll, "OutputDebugStringW", output_debug_string_w);
    d.register_handler(dll, "GetVersionExW", get_version_ex_w);
    d.register_handler(dll, "GetVersionExA", get_version_ex_w);
    d.register_handler(dll, "DestroyWindow", destroy_window);
    d.register_handler(dll, "FindWindowW", find_window_w);
    d.register_handler(dll, "GetVersion", get_version);
    d.register_handler(dll, "ShowWindow", show_window);
    d.register_handler(dll, "UpdateWindow", update_window);
    d.register_handler(dll, "MoveWindow", move_window);
    d.register_constant(dll, "SetForegroundWindow", 1, one_returning);
    // PPC2002 apps disable the IME on the very first line of WinMain.
    // There is no IME here, so "already disabled" is the honest answer.
    d.register_constant(dll, "ImmDisableIME", 1, one_returning);
    d.register_constant(dll, "BringWindowToTop", 1, one_returning);
    d.register_constant(dll, "SetActiveWindow", FAKE_HWND, one_returning);
    d.register_handler(dll, "GetKeyState", get_key_state);
    d.register_handler(dll, "GetAsyncKeyState", get_async_key_state);
    d.register_handler(dll, "keybd_event", keybd_event);
    d.register_handler(dll, "GetFocus", get_focus);
    d.register_handler(dll, "GetCapture", get_capture);
    d.register_constant(dll, "SetCapture", FAKE_HWND, one_returning);
    d.register_constant(dll, "ReleaseCapture", 1, one_returning);
    d.register_handler(dll, "SetFocus", set_focus);
    d.register_handler(dll, "SetWindowPos", set_window_pos);
    d.register_constant(dll, "AdjustWindowRectEx", 1, one_returning);
    d.register_constant(dll, "MapWindowPoints", 1, one_returning);
    d.register_constant(dll, "ClipCursor", 1, one_returning);
    d.register_constant(dll, "SetCursorPos", 1, one_returning);
    d.register_constant(dll, "GetSystemPaletteEntries", 0, zero_returning);
    d.register_constant(dll, "RealizePalette", 1, one_returning);
    d.register_constant(dll, "CreatePalette", 0xDEAD_5709, one_returning);
    d.register_handler(dll, "SetWindowTextW", set_window_text_w);
    d.register_handler(dll, "SetWindowTextA", set_window_text_a);
    d.register_handler(dll, "GetWindowTextW", get_window_text_w);
    d.register_handler(dll, "GetWindowTextA", get_window_text_w);
    d.register_constant(dll, "GetWindowTextLengthW", 0, zero_returning);
    d.register_constant(dll, "GetWindowTextLengthA", 0, zero_returning);
    d.register_handler(dll, "DefWindowProcW", def_window_proc_w);
    d.register_handler(dll, "DefWindowProcA", def_window_proc_w);
    d.register_handler(dll, "DispatchMessageW", dispatch_message_w);
    d.register_handler(dll, "CallWindowProcW", call_window_proc_w);
    d.register_handler(dll, "GetMessageW", get_message_w);
    d.register_handler(dll, "GetMessagePos", get_message_pos);
    d.register_handler(dll, "PeekMessageW", peek_message_w);
    d.register_constant(dll, "TranslateMessage", 1, one_returning);
    d.register_handler(dll, "PostQuitMessage", post_quit_message);
    d.register_handler(dll, "CreateProcessW", create_process_w);
    d.register_handler(dll, "PostMessageW", post_message_w);
    d.register_handler(dll, "PostThreadMessageW", post_thread_message_w);
    d.register_handler(dll, "CreateMsgQueue", create_msg_queue);
    d.register_handler(dll, "ReadMsgQueue", read_msg_queue);
    d.register_handler(dll, "WriteMsgQueue", write_msg_queue);
    d.register_handler(dll, "GetMsgQueueInfo", get_msg_queue_info);
    d.register_handler(dll, "CloseMsgQueue", close_msg_queue);
    d.register_handler(dll, "OpenMsgQueue", open_msg_queue);
    d.register_handler(
        dll,
        "RequestPowerNotifications",
        request_power_notifications,
    );
    d.register_handler(dll, "StopPowerNotifications", stop_power_notifications);
    d.register_handler(dll, "GetSystemPowerState", get_system_power_state);
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
    d.register_handler(dll, "waveOutPause", wave_out_pause);
    d.register_handler(dll, "waveOutRestart", wave_out_restart);
    d.register_handler(dll, "waveOutPrepareHeader", wave_out_prepare_header);
    d.register_handler(dll, "waveOutUnprepareHeader", wave_out_unprepare_header);
    d.register_handler(dll, "waveOutGetNumDevs", wave_out_get_num_devs);
    d.register_handler(dll, "waveOutGetDevCaps", wave_out_get_dev_caps);
    d.register_handler(dll, "waveOutGetDevCapsW", wave_out_get_dev_caps);
    d.register_handler(dll, "waveOutGetPosition", wave_out_get_position);
    d.register_constant(dll, "waveOutMessage", 0, zero_returning);
    d.register_handler(dll, "setjmp", setjmp);
    d.register_handler(dll, "SendMessageW", send_message_w);
    d.register_handler(dll, "GetUpdateRect", get_update_rect);
    d.register_handler(dll, "InvalidateRect", invalidate_rect);
    d.register_constant(dll, "ValidateRect", 1, one_returning);
    d.register_handler(dll, "GetSystemMetrics", get_system_metrics);
    d.register_handler(dll, "GetClientRect", get_client_rect);
    d.register_handler(dll, "WindowFromPoint", window_from_point);
    d.register_handler(dll, "ChildWindowFromPoint", window_from_point);
    d.register_handler(dll, "GetWindowRect", get_window_rect);
    d.register_handler(dll, "GetCursorPos", get_cursor_pos);
    d.register_handler(dll, "SetCursor", set_cursor);
    d.register_handler(dll, "GetClassInfoW", get_class_info_w);
    d.register_handler(
        dll,
        "CreateDialogIndirectParamW",
        create_dialog_indirect_param_w,
    );
    // FALSE means "not a dialog message" so the caller falls through to
    // TranslateMessage / DispatchMessageW, which is where our synthetic
    // WM_PAINT and real taps get delivered. Claiming a message here would
    // swallow every one of them.
    d.register_constant(dll, "IsDialogMessageW", 0, zero_returning);
    d.register_handler(dll, "SetDlgItemTextW", set_dlg_item_text_w);
    d.register_handler(dll, "IsWindow", is_window);
    d.register_handler(dll, "CreateMutexW", create_mutex_w);
    d.register_handler(dll, "CreateSemaphoreW", create_semaphore_w);
    d.register_handler(dll, "ReleaseSemaphore", release_semaphore);
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
    d.register_handler(dll, "EndDialog", end_dialog);
    d.register_handler(dll, "MessageBoxW", message_box_w);
    d.register_handler(dll, "SetTimer", set_timer);
    d.register_constant(dll, "KillTimer", 1, one_returning);
    d.register_handler(dll, "RegisterHotKey", register_hot_key);
    d.register_handler(dll, "UnregisterHotKey", unregister_hot_key);

    // ---- GDI (real, framebuffer-backed) ----
    d.register_handler(dll, "GetDC", get_dc);
    // Same DC: this HLE has one screen surface and no non-client area to
    // distinguish. An EGL/GL renderer asks for the window DC to build
    // its surface, and a NULL here reads as "no display" and aborts
    // start-up.
    d.register_handler(dll, "GetWindowDC", get_dc);
    d.register_constant(dll, "ReleaseDC", 1, one_returning);
    d.register_handler(dll, "BeginPaint", begin_paint);
    d.register_handler(dll, "EndPaint", end_paint);
    d.register_handler(dll, "CreateCompatibleDC", create_compatible_dc);
    d.register_handler(dll, "CreateCompatibleBitmap", create_compatible_bitmap);
    d.register_handler(dll, "CreateDIBSection", create_dib_section);
    d.register_handler(dll, "CreateBitmap", create_bitmap);
    d.register_handler(dll, "SetBitmapBits", set_bitmap_bits);
    d.register_handler(dll, "CreateSolidBrush", create_solid_brush);
    d.register_handler(dll, "CreatePen", create_pen);
    d.register_handler(dll, "CreateFontIndirectW", create_font_indirect);
    d.register_handler(dll, "GetStockObject", get_stock_object);
    d.register_handler(dll, "SelectObject", select_object);
    d.register_handler(dll, "DeleteObject", delete_object);
    d.register_handler(dll, "DeleteDC", delete_dc);
    d.register_handler(dll, "BitBlt", bit_blt);
    d.register_handler(dll, "TransparentImage", transparent_image);
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
    d.register_handler(dll, "SetTextAlign", set_text_align);
    d.register_handler(dll, "TextOutW", text_out_w);
    d.register_handler(dll, "ExtTextOutW", ext_text_out_w);
    d.register_handler(dll, "ExtEscape", ext_escape);
    d.register_handler(dll, "Escape", ext_escape);
    d.register_handler(dll, "GetDeviceCaps", get_device_caps);
    // ---- Display mode enumeration (SDL 1.2 needs a non-empty list) ----
    d.register_handler(dll, "EnumDisplaySettings", enum_display_settings);
    d.register_handler(dll, "EnumDisplaySettingsW", enum_display_settings);
    d.register_handler(dll, "EnumDisplaySettingsExW", enum_display_settings);
    // DISP_CHANGE_SUCCESSFUL — we only ever have one mode, so any
    // request for it succeeds.
    d.register_handler(dll, "ChangeDisplaySettings", change_display_settings_ex);
    d.register_handler(dll, "ChangeDisplaySettingsEx", change_display_settings_ex);
    d.register_handler(dll, "ChangeDisplaySettingsExW", change_display_settings_ex);
    d.register_handler(dll, "ChangeDisplaySettingsW", change_display_settings_ex);
    d.register_handler(dll, "SHGetSpecialFolderPath", sh_get_special_folder_path);
    d.register_handler(dll, "SHGetSpecialFolderPathW", sh_get_special_folder_path);
    d.register_handler(dll, "GetKeyboardLayout", keyboard_layout);
    d.register_handler(dll, "LoadKeyboardLayoutW", keyboard_layout);
    d.register_handler(dll, "ActivateKeyboardLayout", keyboard_layout);
    d.register_handler(dll, "GetKeyboardLayoutNameW", get_keyboard_layout_name_w);
    d.register_handler(dll, "GetUserDefaultUILanguage", user_default_ui_language);
    d.register_handler(dll, "GetUserDefaultLangID", user_default_ui_language);
    d.register_constant(dll, "ReleaseMutex", 1, one_returning);
    // SDL 1.2's windib video driver builds its mode list from
    // `EnumDisplaySettings`; with no modes at all every fullscreen
    // `SDL_SetVideoMode` fails with "No video mode large enough".
    d.register_handler(dll, "GetSystemDefaultUILanguage", en_us_lang_id);
    d.register_handler(dll, "GetUserDefaultLCID", en_us_lang_id);
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
    d.register_handler(dll, "__rt_sdiv64by64", rt_sdiv64by64);
    d.register_handler(dll, "__rt_udiv64by64", rt_udiv64by64);
    // Same argument order as the division helpers (r0:r1 divisor,
    // r2:r3 dividend); the remainder comes back in r0:r1. Leaving
    // these unimplemented returned 0 for every `%`, which is how
    // Sonic Unleashed ended up with a zero frame delta and stalled.
    // Same argument order (r0:r1 = divisor, r2:r3 = dividend), but the
    // result is the remainder. Games use these for wall-clock maths;
    // returning 0 froze Sonic Unleashed's frame timer.

    // ---- Misc kernel/IPC stubs ----
    d.register_constant(dll, "KernelIoControl", 0, zero_returning);
    // `SystemIdleTimerReset` tells the power manager the user is still
    // there so the backlight and suspend timers restart. There is no
    // idle timer to reset here, and the documented return is the number
    // of milliseconds until the next timeout, which nothing checks.
    //
    // It has to be a constant rather than an unimplemented stub because
    // games call it from the render loop to keep the screen alive: Toy
    // Golf issues it once per frame, 79 240 times in a 60-frame run,
    // and every one of those was a host round-trip plus a logged
    // "unimplemented call" warning.
    d.register_constant(dll, "SystemIdleTimerReset", 0, zero_returning);
    // The IME is a text-entry service. No emulated guest has an input
    // method open, so the correct answer to "give me the input context
    // for this window" is NULL, and positioning a composition window
    // that does not exist is a no-op. Toy Golf asks once at startup.
    d.register_constant(dll, "ImmGetContext", 0, zero_returning);
    d.register_constant(dll, "ImmReleaseContext", 1, one_returning);
    d.register_constant(dll, "ImmSetCompositionWindow", 1, one_returning);
    d.register_constant(dll, "SystemParametersInfoW", 1, one_returning);
    d.register_constant(dll, "GetSystemPowerStatusEx", 1, one_returning);
    d.register_handler(dll, "CreateEventW", create_event_w);
    d.register_handler(dll, "CreateEventA", create_event_w);
    d.register_handler(dll, "OpenEventW", open_event_w);
    d.register_handler(dll, "OpenEventA", open_event_w);
    d.register_handler(dll, "SetEvent", set_event);
    d.register_handler(dll, "PulseEvent", set_event);
    d.register_handler(dll, "ResetEvent", reset_event);
    d.register_handler(dll, "EventModify", event_modify);
    d.register_handler(dll, "WaitForSingleObject", wait_for_single_object);
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
    d.register_constant(dll, "GetThreadPriority", 0, zero_returning);
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
    d.register_handler(dll, "InterlockedTestExchange", interlocked_compare_exchange);
    d.register_handler(dll, "GetSystemInfo", get_system_info);
    d.register_constant(dll, "SetKMode", 1, one_returning);
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
    //
    // Backed by a real (in-memory) key/value store — see
    // `pocket_kernel::registry`. Pocket PC installers write the paths a
    // game later reads back, so stubbing these out made titles bail:
    // Astraware Bejeweled calls `ExitProcess(0x42)` when
    // `HKLM\SOFTWARE\Apps\Astraware Bejeweled\SaveDir` is missing.
    d.register_handler(dll, "RegOpenKeyExW", reg_open_key_ex_w);
    d.register_handler(dll, "RegCreateKeyExW", reg_create_key_ex_w);
    d.register_handler(dll, "RegQueryValueExW", reg_query_value_ex_w);
    d.register_handler(dll, "RegQueryInfoKeyW", reg_query_info_key_w);
    d.register_handler(dll, "RegSetValueExW", reg_set_value_ex_w);
    d.register_handler(dll, "RegDeleteValueW", reg_delete_value_w);
    d.register_handler(dll, "RegCloseKey", reg_close_key);
    d.register_handler(dll, "RegFlushKey", reg_flush_key);

    /// `ERROR_FILE_NOT_FOUND` — what a real device returns for a key or
    /// value that was never written.
    const ERROR_NOT_FOUND: u32 = 2;
    /// `ERROR_MORE_DATA`.
    const ERROR_MORE_DATA: u32 = 234;

    fn read_key_path(ctx: &mut CallCtx<'_>, root: u32, subkey_ptr: u32) -> (String, String) {
        let subkey = if subkey_ptr == 0 {
            String::new()
        } else {
            read_wstr(ctx, subkey_ptr, 260)
                .map(|value| String::from_utf16_lossy(&value))
                .unwrap_or_default()
        };
        let path = ctx
            .kernel
            .registry
            .resolve(root, &subkey)
            .unwrap_or_else(|| pocket_kernel::registry::canonical_key(&subkey));
        (subkey, path)
    }

    fn reg_open_key_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let root = ctx.arg_u32(0)?;
        let subkey_ptr = ctx.arg_u32(1)?;
        let out_key = ctx.arg_u32(4)?;
        let (subkey, path) = read_key_path(ctx, root, subkey_ptr);
        let Some(handle) = ctx.kernel.registry.open(&path) else {
            log::debug!("RegOpenKeyExW(root=0x{root:08x}, {subkey:?}) -> ERROR_FILE_NOT_FOUND");
            return Ok(DispatchOutcome::ReturnedR0(ERROR_NOT_FOUND));
        };
        log::debug!("RegOpenKeyExW(root=0x{root:08x}, {subkey:?}) -> 0x{handle:08x}");
        if out_key != 0 {
            ctx.cpu.write_mem(out_key, &handle.to_le_bytes())?;
        }
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    fn reg_create_key_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let root = ctx.arg_u32(0)?;
        let subkey_ptr = ctx.arg_u32(1)?;
        let out_key = ctx.arg_u32(7)?;
        let disposition = ctx.arg_u32(8)?;
        let (subkey, path) = read_key_path(ctx, root, subkey_ptr);
        let existed = ctx.kernel.registry.contains_key(&path);
        let handle = ctx.kernel.registry.create_and_open(&path);
        log::debug!("RegCreateKeyExW(root=0x{root:08x}, {subkey:?}) -> 0x{handle:08x}");
        if out_key != 0 {
            ctx.cpu.write_mem(out_key, &handle.to_le_bytes())?;
        }
        if disposition != 0 {
            // REG_OPENED_EXISTING_KEY (2) or REG_CREATED_NEW_KEY (1).
            let value: u32 = if existed { 2 } else { 1 };
            ctx.cpu.write_mem(disposition, &value.to_le_bytes())?;
        }
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    fn reg_query_value_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let key = ctx.arg_u32(0)?;
        let value_ptr = ctx.arg_u32(1)?;
        let type_ptr = ctx.arg_u32(3)?;
        let data = ctx.arg_u32(4)?;
        let size_ptr = ctx.arg_u32(5)?;
        let name = if value_ptr == 0 {
            String::new()
        } else {
            read_wstr(ctx, value_ptr, 260)
                .map(|chars| String::from_utf16_lossy(&chars))
                .unwrap_or_default()
        };
        let Some(path) = ctx.kernel.registry.path_for(key) else {
            log::debug!("RegQueryValueExW(0x{key:08x}, {name:?}) -> bad key handle");
            return Ok(DispatchOutcome::ReturnedR0(ERROR_NOT_FOUND));
        };
        let Some(value) = ctx.kernel.registry.value(&path, &name) else {
            log::debug!("RegQueryValueExW({path}, {name:?}) -> ERROR_FILE_NOT_FOUND");
            return Ok(DispatchOutcome::ReturnedR0(ERROR_NOT_FOUND));
        };
        let bytes = value.to_bytes();
        let kind = value.type_code();
        log::debug!(
            "RegQueryValueExW({path}, {name:?}) -> type={kind}, {} bytes",
            bytes.len()
        );
        if type_ptr != 0 {
            ctx.cpu.write_mem(type_ptr, &kind.to_le_bytes())?;
        }
        let capacity = if size_ptr != 0 {
            u32::from_le_bytes(ctx.cpu.read_mem(size_ptr, 4)?.try_into().unwrap())
        } else {
            0
        };
        if size_ptr != 0 {
            ctx.cpu
                .write_mem(size_ptr, &(bytes.len() as u32).to_le_bytes())?;
        }
        if data == 0 {
            // Size query — the caller only wanted `*lpcbData`.
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        if (capacity as usize) < bytes.len() {
            return Ok(DispatchOutcome::ReturnedR0(ERROR_MORE_DATA));
        }
        ctx.cpu.write_mem(data, &bytes)?;
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    fn reg_query_info_key_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let key = ctx.arg_u32(0)?;
        let class_ptr = ctx.arg_u32(1)?;
        let class_capacity = ctx.arg_u32(2)?;
        let subkeys_ptr = ctx.arg_u32(3)?;
        let max_subkey_len_ptr = ctx.arg_u32(4)?;
        let max_class_len_ptr = ctx.arg_u32(5)?;
        let values_ptr = ctx.arg_u32(6)?;
        let max_value_name_len_ptr = ctx.arg_u32(7)?;
        let max_value_len_ptr = ctx.arg_u32(8)?;
        let security_ptr = ctx.arg_u32(9)?;
        let last_write_ptr = ctx.arg_u32(10)?;

        let Some(path) = ctx.kernel.registry.path_for(key) else {
            return Ok(DispatchOutcome::ReturnedR0(ERROR_NOT_FOUND));
        };
        let _ = class_ptr;
        let _ = class_capacity;
        let _ = security_ptr;
        let _ = last_write_ptr;
        let subkey_count = 0u32;
        let max_subkey_len = 0u32;
        let max_class_len = 0u32;
        let value_count = if ctx.kernel.registry.contains_key(&path) {
            1
        } else {
            0
        };
        let max_value_name_len = 0u32;
        let max_value_len = 0u32;
        for (ptr, value) in [
            (subkeys_ptr, subkey_count),
            (max_subkey_len_ptr, max_subkey_len),
            (max_class_len_ptr, max_class_len),
            (values_ptr, value_count),
            (max_value_name_len_ptr, max_value_name_len),
            (max_value_len_ptr, max_value_len),
        ] {
            if ptr != 0 {
                ctx.cpu.write_mem(ptr, &value.to_le_bytes())?;
            }
        }
        log::debug!("RegQueryInfoKeyW({path}) -> values={value_count}");
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    fn reg_set_value_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        use pocket_kernel::registry::RegistryValue;
        let key = ctx.arg_u32(0)?;
        let value_ptr = ctx.arg_u32(1)?;
        let kind = ctx.arg_u32(3)?;
        let data = ctx.arg_u32(4)?;
        let size = ctx.arg_u32(5)?;
        let name = if value_ptr == 0 {
            String::new()
        } else {
            read_wstr(ctx, value_ptr, 260)
                .map(|chars| String::from_utf16_lossy(&chars))
                .unwrap_or_default()
        };
        let Some(path) = ctx.kernel.registry.path_for(key) else {
            return Ok(DispatchOutcome::ReturnedR0(ERROR_NOT_FOUND));
        };
        let raw = if data != 0 && size != 0 {
            ctx.cpu.read_mem(data, size.min(0x1000))?
        } else {
            Vec::new()
        };
        let value = match kind {
            // REG_SZ / REG_EXPAND_SZ
            1 | 2 => {
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .take_while(|unit| *unit != 0)
                    .collect();
                RegistryValue::Sz(String::from_utf16_lossy(&units))
            }
            // REG_DWORD
            4 if raw.len() >= 4 => {
                RegistryValue::Dword(u32::from_le_bytes(raw[..4].try_into().unwrap()))
            }
            _ => RegistryValue::Binary(raw),
        };
        log::debug!("RegSetValueExW({path}, {name:?}) <- type={kind}");
        ctx.kernel.registry.set_value(&path, &name, value);
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    fn reg_delete_value_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let key = ctx.arg_u32(0)?;
        let value_ptr = ctx.arg_u32(1)?;
        let name = if value_ptr == 0 {
            String::new()
        } else {
            read_wstr(ctx, value_ptr, 260)
                .map(|chars| String::from_utf16_lossy(&chars))
                .unwrap_or_default()
        };
        let Some(path) = ctx.kernel.registry.path_for(key) else {
            return Ok(DispatchOutcome::ReturnedR0(ERROR_NOT_FOUND));
        };
        let removed = ctx.kernel.registry.delete_value(&path, &name);
        log::debug!("RegDeleteValueW({path}, {name:?}) -> removed={removed}");
        Ok(DispatchOutcome::ReturnedR0(if removed {
            0
        } else {
            ERROR_NOT_FOUND
        }))
    }

    fn reg_close_key(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let key = ctx.arg_u32(0)?;
        ctx.kernel.registry.close(key);
        Ok(DispatchOutcome::ReturnedR0(0))
    }

    /// `RegFlushKey(hKey)`
    ///
    /// Writes a key's pending changes through to the device's registry
    /// file. Our registry lives in memory and every write is already
    /// visible, so there is nothing to flush: report `ERROR_SUCCESS`.
    /// Bejeweled 2 flushes its save key after every game and treats a
    /// failure as "storage is gone".
    fn reg_flush_key(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
        let key = ctx.arg_u32(0)?;
        log::debug!("RegFlushKey(key=0x{key:08x}) -> ERROR_SUCCESS");
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
    d.register_handler(dll, "floorf", m_floorf);
    d.register_handler(dll, "ceil", m_ceil);
    d.register_handler(dll, "abs", m_abs_i32);
    d.register_handler(dll, "fabs", m_fabs);
    d.register_handler(dll, "atan2", m_atan2);
    d.register_handler(dll, "pow", m_pow);
    d.register_handler(dll, "fmod", m_fmod);
    d.register_handler(dll, "_hypot", m_hypot);
    d.register_handler(dll, "hypot", m_hypot);
    d.register_handler(dll, "ldexp", m_ldexp);
    d.register_handler(dll, "frexp", m_frexp);
    d.register_handler(dll, "modf", m_modf);

    // ---- libm (soft-float, single-precision) ----
    d.register_handler(dll, "sinf", m_sinf);
    d.register_handler(dll, "cosf", m_cosf);
    d.register_handler(dll, "tanf", m_tanf);
    d.register_handler(dll, "asinf", m_asinf);
    d.register_handler(dll, "acosf", m_acosf);
    d.register_handler(dll, "atanf", m_atanf);
    d.register_handler(dll, "sinhf", m_sinhf);
    d.register_handler(dll, "coshf", m_coshf);
    d.register_handler(dll, "tanhf", m_tanhf);
    d.register_handler(dll, "expf", m_expf);
    d.register_handler(dll, "logf", m_logf);
    d.register_handler(dll, "log10f", m_log10f);
    d.register_handler(dll, "sqrtf", m_sqrtf);
    d.register_handler(dll, "ceilf", m_ceilf);
    d.register_handler(dll, "fabsf", m_fabsf);
    d.register_handler(dll, "atan2f", m_atan2f);
    d.register_handler(dll, "powf", m_powf);
    d.register_handler(dll, "fmodf", m_fmodf);
    d.register_handler(dll, "hypotf", m_hypotf);
    d.register_handler(dll, "_hypotf", m_hypotf);
    d.register_handler(dll, "ldexpf", m_ldexpf);
    d.register_handler(dll, "frexpf", m_frexpf);
    d.register_handler(dll, "modff", m_modff);

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
    d.register_handler(dll, "IntersectRect", intersect_rect);
    d.register_handler(dll, "UnionRect", union_rect);
    d.register_handler(dll, "InflateRect", inflate_rect);
    d.register_handler(dll, "OffsetRect", offset_rect);
    d.register_handler(dll, "PtInRect", pt_in_rect);
    d.register_handler(dll, "IsRectEmpty", is_rect_empty);

    // ---- Locale ----
    d.register_handler(dll, "GetSystemDefaultLangID", get_system_default_lang_id);
    d.register_handler(dll, "GetThreadLocale", get_thread_locale);
    d.register_handler(dll, "GetLocaleInfoW", get_locale_info_w);
    d.register_handler(dll, "GetACP", get_acp);
    d.register_constant(dll, "ShowCursor", 1, one_returning);
    d.register_handler(dll, "CreateCursor", create_cursor);
    d.register_handler(dll, "DestroyCursor", destroy_cursor);

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
    d.register_constant(dll, "CacheSync", 1, one_returning);
    d.register_constant(dll, "ord:1825", 0, zero_returning);
    d.register_constant(dll, "?set_new_handler@@YAP6AXXZP6AXXZ@Z", 0, zero_returning);

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
// `KernelState::vector_iter_stack`, sets `R0 = element pointer`,
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
    let sp_now = ctx.cpu.read_reg(ArmReg::Sp)?;

    // Iterations whose callback can no longer return — the guest
    // unwound past their frame, e.g. through a C++ exception — would
    // otherwise sit on the stack forever and swallow a later call.
    while ctx
        .kernel
        .vector_iter_stack
        .last()
        .is_some_and(|f| f.sp_at_call < sp_now)
    {
        ctx.kernel.vector_iter_stack.pop();
    }

    // Tell "the element callback just branched back to us" apart from
    // "the element callback started an array of its own". The former
    // re-enters with the SP the callback was entered with; the latter
    // is a fresh `bl` from inside the callback's own frame, so its SP
    // is lower.
    let resuming = ctx
        .kernel
        .vector_iter_stack
        .last()
        .is_some_and(|f| f.thunk_va == thunk_va && f.sp_at_call == sp_now);

    if resuming {
        let frame = ctx
            .kernel
            .vector_iter_stack
            .last_mut()
            .expect("stack is non-empty when resuming");
        frame.i = frame.i.saturating_add(1);
    } else {
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
            "{} begin: pBegin=0x{:08x} cb={} N={} pFunc=0x{:08x} cleanup=0x{:08x} retLR=0x{:08x} depth={}",
            ctx.thunk.label(),
            p_begin,
            cb_element,
            n_elements,
            p_func,
            p_cleanup,
            saved_lr,
            ctx.kernel.vector_iter_stack.len() + 1,
        );
        ctx.kernel.vector_iter_stack.push(VectorIterFrame {
            p_begin,
            cb_element,
            n_elements,
            p_func,
            p_cleanup,
            is_dtor,
            i: 0,
            saved_lr,
            thunk_va,
            sp_at_call: sp_now,
        });
    }

    let frame = *ctx
        .kernel
        .vector_iter_stack
        .last()
        .expect("a frame was just pushed or resumed");

    // Termination conditions:
    //   * `n_elements <= 0` — empty array, nothing to do.
    //   * `i >= n_elements` — every element processed.
    //   * `p_func == 0` — caller passed a NULL ctor/dtor pointer; on
    //     real coredll this is a guest bug that segfaults the first
    //     call. We treat it as "no-op iteration" and return cleanly
    //     so the rest of the program isn't poisoned.
    if frame.n_elements <= 0 || frame.i >= frame.n_elements || frame.p_func == 0 {
        ctx.kernel.vector_iter_stack.pop();
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
    Ok(DispatchOutcome::JumpTo(frame.p_func))
}

// ---------- qsort ----------

// `void qsort(void *base, size_t num, size_t width,
//             int (*compar)(const void *, const void *))`
//
// Same problem as the `??_L` / `??_M` iterators above: the interesting
// part of the algorithm — the comparison — is guest code, so we cannot
// run the sort to completion inside this handler. We therefore drive a
// binary insertion sort one comparison at a time. Each call either
// starts a fresh sort or resumes the one parked in
// `KernelState::qsort_frames`, reading the previous comparison's result
// out of R0.
//
// Asphalt 2 3D needs this to build the race: it sorts its track segment
// and opponent tables right after the "LOADING" screen, and with `qsort`
// unimplemented the tables stayed in file order.
fn qsort(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let thunk_va = ctx.thunk.thunk_va;

    let mut frame = match ctx.kernel.qsort_frames.get(&thunk_va).copied() {
        Some(mut f) => {
            // Resume: R0 holds `compar(elem[i], elem[mid])`. A negative
            // result means element `i` sorts before `mid`, so the
            // insertion point is in the lower half. Ties go right,
            // which is what keeps the sort stable.
            let result = ctx.cpu.read_reg(ArmReg::R0)? as i32;
            if result < 0 {
                f.hi = f.mid;
            } else {
                f.lo = f.mid + 1;
            }
            f
        }
        None => {
            let base = ctx.arg_u32(0)?;
            let num = ctx.arg_u32(1)?;
            let width = ctx.arg_u32(2)?;
            let compar = ctx.arg_u32(3)?;
            let saved_lr = ctx.cpu.read_reg(ArmReg::Lr)?;
            log::trace!(
                "qsort: base=0x{base:08x} num={num} width={width} compar=0x{compar:08x} retLR=0x{saved_lr:08x}"
            );
            // Nothing to do for an empty / single-element array, a
            // zero-width element, or a NULL comparator. Real coredll
            // would fault on the last one; returning cleanly keeps a
            // guest bug from taking the whole emulator down.
            if num < 2 || width == 0 || compar == 0 {
                return Ok(DispatchOutcome::ReturnedR0(0));
            }
            QsortFrame {
                base,
                num,
                width,
                compar,
                i: 1,
                lo: 0,
                hi: 1,
                mid: 0,
                saved_lr,
            }
        }
    };

    loop {
        if frame.lo < frame.hi {
            // Still searching for element `i`'s insertion point.
            frame.mid = frame.lo + (frame.hi - frame.lo) / 2;
            let a = frame.base.wrapping_add(frame.i.wrapping_mul(frame.width));
            let b = frame.base.wrapping_add(frame.mid.wrapping_mul(frame.width));
            ctx.cpu.write_reg(ArmReg::R0, a)?;
            ctx.cpu.write_reg(ArmReg::R1, b)?;
            // `compar`'s `bx lr` lands back in this thunk, which
            // re-enters us with the result in R0.
            ctx.cpu.write_reg(ArmReg::Lr, thunk_va)?;
            let target = frame.compar;
            ctx.kernel.qsort_frames.insert(thunk_va, frame);
            return Ok(DispatchOutcome::JumpTo(target));
        }

        // Search finished: `lo` is where element `i` belongs. Rotate
        // `[lo, i]` right by one so the element lands there. This is
        // the only part that touches memory, and it is a pure host-side
        // move — no guest round-trip needed.
        if frame.lo < frame.i {
            let width = frame.width as usize;
            let elem_i = frame.base.wrapping_add(frame.i.wrapping_mul(frame.width));
            let elem_lo = frame.base.wrapping_add(frame.lo.wrapping_mul(frame.width));
            let key = ctx.cpu.read_mem(elem_i, frame.width)?;
            let span = (frame.i - frame.lo) as usize * width;
            let block = ctx.cpu.read_mem(elem_lo, span as u32)?;
            ctx.cpu
                .write_mem(elem_lo.wrapping_add(frame.width), &block)?;
            ctx.cpu.write_mem(elem_lo, &key)?;
        }

        frame.i += 1;
        if frame.i >= frame.num {
            // Sorted. Hand control back to qsort's caller.
            ctx.kernel.qsort_frames.remove(&thunk_va);
            ctx.cpu.write_reg(ArmReg::Lr, frame.saved_lr)?;
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        frame.lo = 0;
        frame.hi = frame.i;
    }
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

fn version_resource(kernel: &KernelState) -> Option<(u32, u32)> {
    kernel.resource_scopes().find_map(|(entries, base)| {
        entries
            .iter()
            .find(|entry| entry.ty == ResourceKey::Id(16) && entry.name == ResourceKey::Id(1))
            .map(|entry| (base.wrapping_add(entry.data_rva), entry.size))
    })
}

fn get_file_version_info_size_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let file_ptr = ctx.arg_u32(0)?;
    let handle_ptr = ctx.arg_u32(1)?;
    let file = if file_ptr == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&read_wstr(ctx, file_ptr, 260)?)
    };
    let Some((_, size)) = version_resource(ctx.kernel) else {
        log::debug!("GetFileVersionInfoSizeW({file:?}) -> 0");
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    if handle_ptr != 0 {
        ctx.cpu.write_mem(handle_ptr, &0u32.to_le_bytes())?;
    }
    log::debug!("GetFileVersionInfoSizeW({file:?}) -> {size}");
    Ok(DispatchOutcome::ReturnedR0(size))
}

fn get_file_version_info_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let file_ptr = ctx.arg_u32(0)?;
    let _handle = ctx.arg_u32(1)?;
    let length = ctx.arg_u32(2)?;
    let data = ctx.arg_u32(3)?;
    let file = if file_ptr == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&read_wstr(ctx, file_ptr, 260)?)
    };
    let Some((source, size)) = version_resource(ctx.kernel) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    if data == 0 || length < size {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(source, size)?;
    ctx.cpu.write_mem(data, &bytes)?;
    log::debug!("GetFileVersionInfoW({file:?}, {length}) -> {size}");
    Ok(DispatchOutcome::ReturnedR0(1))
}

#[derive(Clone, Copy)]
struct VersionNode {
    end: usize,
    value: usize,
    value_bytes: usize,
    value_units: u32,
}

fn version_word(bytes: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    Some(u16::from_le_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn version_node(bytes: &[u8], start: usize) -> Option<(String, VersionNode)> {
    let length = version_word(bytes, start)? as usize;
    let value_units = version_word(bytes, start + 2)? as u32;
    let kind = version_word(bytes, start + 4)?;
    let end = start.checked_add(length)?.min(bytes.len());
    let mut key_at = start.checked_add(6)?;
    let mut key = Vec::new();
    loop {
        let ch = version_word(bytes, key_at)?;
        key_at = key_at.checked_add(2)?;
        if ch == 0 {
            break;
        }
        key.push(ch);
    }
    let value = (key_at + 3) & !3;
    let value_bytes = if kind == 1 {
        value_units.checked_mul(2)? as usize
    } else {
        value_units as usize
    };
    if value.checked_add(value_bytes)? > end {
        return None;
    }
    Some((
        String::from_utf16_lossy(&key),
        VersionNode {
            end,
            value,
            value_bytes,
            value_units,
        },
    ))
}

fn version_child(bytes: &[u8], parent: VersionNode, wanted: &str) -> Option<(String, VersionNode)> {
    let mut at = (parent.value + parent.value_bytes + 3) & !3;
    while at + 6 <= parent.end {
        let (key, node) = version_node(bytes, at)?;
        if key.eq_ignore_ascii_case(wanted) {
            return Some((key, node));
        }
        if node.end <= at {
            break;
        }
        at = (node.end + 3) & !3;
    }
    None
}

fn ver_query_value_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let block = ctx.arg_u32(0)?;
    let subblock = ctx.arg_u32(1)?;
    let value_out = ctx.arg_u32(2)?;
    let length_out = ctx.arg_u32(3)?;
    if block == 0 || subblock == 0 || value_out == 0 || length_out == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let query = String::from_utf16_lossy(&read_wstr(ctx, subblock, 260)?);
    let Some((_, size)) = version_resource(ctx.kernel) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let bytes = ctx.cpu.read_mem(block, size)?;
    let Some((_, mut node)) = version_node(&bytes, 0) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let mut value_offset = node.value;
    for part in query.split('\\').filter(|part| !part.is_empty()) {
        let Some((_, child)) = version_child(&bytes, node, part) else {
            return Ok(DispatchOutcome::ReturnedR0(0));
        };
        value_offset = child.value;
        node = child;
    }
    let units = if query == "\\" {
        node.value_bytes as u32
    } else {
        node.value_units
    };
    ctx.cpu.write_mem(
        value_out,
        &block.wrapping_add(value_offset as u32).to_le_bytes(),
    )?;
    ctx.cpu.write_mem(length_out, &units.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(1))
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

/// `GetCurrentThreadId` for the main thread. Workers get `2`, `3`, ...
const MAIN_THREAD_ID: u32 = 1;

/// Id of whichever guest thread currently owns the CPU.
fn current_thread_id(ctx: &CallCtx<'_>) -> u32 {
    match ctx.kernel.current_thread.checked_sub(1) {
        None => MAIN_THREAD_ID,
        Some(index) => ctx
            .kernel
            .threads
            .get(index)
            .map(|thread| thread.id)
            .filter(|id| *id != 0)
            .unwrap_or(MAIN_THREAD_ID + 1 + index as u32),
    }
}

/// Park the running worker so that its blocking call is *retried* when
/// it next gets the CPU, instead of returning a value it never saw a
/// message for.
///
/// `GetMessageW` has no "queue was empty" return: `FALSE` means
/// `WM_QUIT` and ends the pump. A worker whose own queue is empty
/// therefore cannot be resumed with `r0 = 0` — Spore Origins' loader
/// thread exits on that and the game never draws again. Parking with
/// the API thunk as the resume address makes the call block the way it
/// does on the device: it re-dispatches, and either finds a message or
/// yields again.
fn park_worker_and_retry(ctx: &mut CallCtx<'_>) -> Result<Option<DispatchOutcome>, KernelError> {
    let thunk_va = ctx.thunk.thunk_va;
    park_worker_at(ctx, Some(0), Some(thunk_va))
}

/// Park the running worker and re-run the call it is blocked in with
/// its **arguments intact**.
///
/// [`park_worker_and_retry`] rewrites `r0` on the way out, which is
/// harmless when the retried call re-reads its state from elsewhere but
/// destroys the first argument of a call that has one. A blocking
/// `WaitForSingleObject(handle, INFINITE)` has to see the same handle
/// when it runs again, otherwise it wakes up waiting on handle 0.
fn park_worker_and_reevaluate(
    ctx: &mut CallCtx<'_>,
) -> Result<Option<DispatchOutcome>, KernelError> {
    let thunk_va = ctx.thunk.thunk_va;
    park_worker_at(ctx, None, Some(thunk_va))
}

/// Park the worker thread that is currently running and give the CPU
/// back to the main thread.
///
/// Only one guest thread executes at a time in this HLE, so every
/// blocking call a worker makes is a scheduling point. `return_r0` is
/// what that blocking call will appear to have returned once the worker
/// is resumed. Returns `None` when the main thread is the one running.
fn park_worker(
    ctx: &mut CallCtx<'_>,
    return_r0: u32,
) -> Result<Option<DispatchOutcome>, KernelError> {
    park_worker_at(ctx, Some(return_r0), None)
}

/// Shared body of [`park_worker`] / [`park_worker_and_retry`] /
/// [`park_worker_and_reevaluate`].
///
/// `resume_at` is where the worker continues when it is scheduled
/// again: `None` means "after the call" (the normal case), `Some(va)`
/// re-enters the API thunk so the blocking call runs again.
/// `return_r0` of `None` leaves `r0` alone, which only makes sense
/// together with a re-entering `resume_at`.
fn park_worker_at(
    ctx: &mut CallCtx<'_>,
    return_r0: Option<u32>,
    resume_at: Option<u32>,
) -> Result<Option<DispatchOutcome>, KernelError> {
    let Some(thread_index) = ctx.kernel.current_thread.checked_sub(1) else {
        return Ok(None);
    };
    let mut regs = read_guest_regs(ctx.cpu)?;
    regs[15] = match resume_at {
        Some(va) => va,
        None => ctx.cpu.read_reg(ArmReg::Lr)?,
    };
    if let Some(value) = return_r0 {
        regs[0] = value;
    }
    let main_regs = ctx
        .kernel
        .threads
        .get(thread_index)
        .map(|thread| thread.saved_regs);
    if let Some(thread) = ctx.kernel.threads.get_mut(thread_index) {
        thread.worker_regs = regs;
        thread.worker_saved = true;
    }
    ctx.kernel.current_thread = 0;
    let Some(main_regs) = main_regs else {
        return Ok(None);
    };
    write_guest_regs(ctx.cpu, &main_regs)?;
    Ok(Some(DispatchOutcome::JumpTo(main_regs[15] & !1)))
}

/// Hand the CPU to a worker thread that parked itself earlier.
///
/// Snapshots the *current* (main) context first: `saved_regs` is what
/// the worker's next park — or its exit trampoline — restores, so
/// leaving the stale `CreateThread`-time snapshot there would rewind
/// the main thread into the middle of its startup path with dead
/// registers. Bejeweled's watchdog thread made that corruption visible
/// (`r4` came back as `2` and the game faulted).
fn resume_worker(
    ctx: &mut CallCtx<'_>,
    return_r0: u32,
) -> Result<Option<DispatchOutcome>, KernelError> {
    if ctx.kernel.current_thread != 0 {
        return Ok(None);
    }
    let Some((thread_index, worker_regs)) = ctx
        .kernel
        .threads
        .iter()
        .enumerate()
        .find(|(_, thread)| thread.worker_saved && thread.started && !thread.finished)
        .map(|(index, thread)| (index, thread.worker_regs))
    else {
        return Ok(None);
    };
    let mut main_regs = read_guest_regs(ctx.cpu)?;
    main_regs[0] = return_r0;
    main_regs[15] = ctx.cpu.read_reg(ArmReg::Lr)?;
    if let Some(thread) = ctx.kernel.threads.get_mut(thread_index) {
        thread.saved_regs = main_regs;
        thread.resume_pc = main_regs[15];
        thread.worker_saved = false;
    }
    write_guest_regs(ctx.cpu, &worker_regs)?;
    ctx.kernel.current_thread = thread_index + 1;
    log::debug!(
        "scheduling worker thread {} at 0x{:08x}",
        thread_index,
        worker_regs[15]
    );
    Ok(Some(DispatchOutcome::JumpTo(worker_regs[15] & !1)))
}

/// `void Sleep(DWORD dwMilliseconds)`
///
/// Also a wave-buffer delivery point, and Zuma cannot finish a level
/// without it. Its sound engine shuts a stream down like this
/// (`ZumaPPC.exe` 1.50, function at `0x000e80e8`):
///
/// ```text
///     strb #1, [r4, #160]        ; stop_request = 1
/// spin:
///     mov  r0, #0
///     bl   Sleep                 ; Sleep(0)
///     ldrb r3, [r4, #161]        ; stop_ack
///     cmp  r3, #0
///     beq  spin
/// ```
///
/// and the only code that ever writes `stop_ack` is its `waveOutProc`
/// at `0x000e896c`, which checks `stop_request` on entry. On WinCE the
/// audio driver's own thread delivers `WOM_DONE`, so the acknowledgement
/// arrives while the game sleeps. Here the callback used to be handed
/// over only by the message pump, which a `Sleep` spin never reaches:
/// the game burned a core forever after nine to eighty frames, which is
/// also why it looked like terrible performance rather than a hang.
///
/// A guest calling `Sleep` has voluntarily given up the CPU, which is
/// exactly when a driver thread would have run, so serve the queue here
/// too. `WaitFor*Object*` deliberately does not do this: those handlers
/// re-run their call on re-entry, and stacking the callback detour on
/// top of that has no game asking for it.
fn sleep(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Must come first: when `waveOutProc` returns, the thunk re-fires
    // this handler with R0..R3 holding the callback's result rather than
    // the sleep duration, and `wave_out_enter_callback` is what puts the
    // original arguments back.
    let thunk_va = ctx.thunk.thunk_va;
    service_wave_out(ctx)?;
    if let Some(outcome) = wave_out_enter_callback(ctx, thunk_va)? {
        return Ok(outcome);
    }
    let _ms = ctx.arg_u32(0)?;
    if let Some(outcome) = park_worker(ctx, 0)? {
        return Ok(outcome);
    }
    if let Some(outcome) = resume_worker(ctx, 0)? {
        return Ok(outcome);
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `SuspendThread(HANDLE)` is used by Avalanche's cooperative worker
/// control loop to pause a helper before it changes display state.
///
/// PocketHLE already serializes guest threads at API boundaries, so the
/// operation is represented by marking the synthetic thread as not ready;
/// the current worker yields immediately and the main thread continues.
fn suspend_thread(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    if handle == FAKE_CURRENT_THREAD_HANDLE {
        if let Some(outcome) = park_worker(ctx, 0)? {
            return Ok(outcome);
        }
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if let Some(index) = ctx
        .kernel
        .threads
        .iter()
        .position(|thread| thread.handle == handle && !thread.finished)
    {
        ctx.kernel.threads[index].started = false;
        if let Some(outcome) = park_worker(ctx, 0)? {
            return Ok(outcome);
        }
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    Ok(DispatchOutcome::ReturnedR0(0xffff_ffff))
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

fn get_store_information(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let info = ctx.arg_u32(0)?;
    if info == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    ctx.cpu
        .write_mem(info, &(64u32 * 1024 * 1024).to_le_bytes())?;
    ctx.cpu
        .write_mem(info + 4, &(48u32 * 1024 * 1024).to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_disk_free_space_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let free_available = ctx.arg_u32(1)?;
    let total_bytes = ctx.arg_u32(2)?;
    let free_bytes = ctx.arg_u32(3)?;
    const TOTAL: u64 = 64 * 1024 * 1024;
    const FREE: u64 = 48 * 1024 * 1024;
    for (ptr, value) in [
        (free_available, FREE),
        (total_bytes, TOTAL),
        (free_bytes, FREE),
    ] {
        if ptr != 0 {
            ctx.cpu.write_mem(ptr, &value.to_le_bytes())?;
        }
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL DeviceIoControl(HANDLE h, DWORD code, void* in, DWORD in_len,
///                       void* out, DWORD out_len, DWORD* returned,
///                       OVERLAPPED* overlapped)`
///
/// Only volume handles are answered — see `Vfs::volume`. Ball Busters
/// opens `\SD Card\Vol:` during startup and polls it with two storage
/// IOCTLs; a `FALSE` from either one is how it decides the card was
/// ejected, and it then sits on its "SD card removed" screen forever
/// instead of booting. Everything else keeps falling through to the
/// unimplemented-call warning, which is where a real missing IOCTL
/// should show up.
fn device_io_control(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    /// `CTL_CODE(FILE_DEVICE_DISK, 0x709, METHOD_BUFFERED, FILE_READ_ACCESS)`
    const IOCTL_DISK_GET_STORAGEID: u32 = 0x0007_1c24;
    let handle = ctx.arg_u32(0)?;
    let code = ctx.arg_u32(1)?;
    let in_len = ctx.arg_u32(3)?;
    let out_buf = ctx.arg_u32(4)?;
    let out_len = ctx.arg_u32(5)?;
    let returned_p = ctx.arg_u32(6)?;

    if ctx.kernel.vfs.is_registration_service(handle) {
        if out_buf != 0 && out_len > 0 {
            ctx.cpu.write_mem(out_buf, &vec![0u8; out_len as usize])?;
        }
        if returned_p != 0 {
            ctx.cpu.write_mem(returned_p, &0u32.to_le_bytes())?;
        }
        log::debug!(
            "DeviceIoControl(REG1: h=0x{handle:08x}, code=0x{code:08x}, in_len={in_len}, out_len={out_len}) -> TRUE"
        );
        return Ok(DispatchOutcome::ReturnedR0(1));
    }

    let Some(volume) = ctx.kernel.vfs.volume(handle) else {
        // MAS1 is the Gizmondo's hardware MP3 decoder. The official SDK
        // defines these as FILE_DEVICE_SOUND IOCTLs (0x1d), not as disk
        // controls. Decode the submitted chunks into the same PCM engine
        // used by waveOut so every frontend can play the card's music.
        if ctx.kernel.vfs.is_mp3_decoder(handle) {
            const MASG_START: u32 = 0x001d_0ff0;
            const MASG_WRITE: u32 = 0x001d_0ff4;
            const MASG_STOP: u32 = 0x001d_0ff8;
            const MASG_PAUSE: u32 = 0x001d_1000;
            const MASG_RESUME: u32 = 0x001d_1008;
            const MASG_STATE: u32 = 0x001d_1010;
            const MASG_GET_POSITION: u32 = 0x001d_1018;
            const MASG_GET_VOLUME: u32 = 0x001d_1020;
            const MASG_SET_VOLUME: u32 = 0x001d_1028;
            const MASG_HAS_MP3: u32 = 0x001d_1030;
            let input_buf = ctx.arg_u32(2)?;
            let is_start = code == MASG_START;
            let is_write = code == MASG_WRITE;
            let is_stop = code == MASG_STOP;
            let is_pause = code == MASG_PAUSE;
            let is_resume = code == MASG_RESUME;
            let is_set_volume = code == MASG_SET_VOLUME;

            if (is_start || is_write) && in_len > 0 {
                let data = ctx.cpu.read_mem(input_buf, in_len)?;
                let _ = ctx.kernel.vfs.feed_mp3_decoder_data(handle, &data);
                if let Some((sample_rate, channels, samples)) =
                    ctx.kernel.vfs.take_mp3_decoder_pcm(handle)
                {
                    let format = pocket_kernel::audio::GuestFormat {
                        sample_rate,
                        channels,
                        bits_per_sample: 16,
                    };
                    ctx.kernel.wave_out_format = format;
                    ctx.kernel.audio.set_guest_format(format);
                    ctx.kernel.audio.push_samples(&samples);
                    ctx.kernel.audio.start();
                }
            } else if is_stop {
                ctx.kernel.vfs.stop_mp3_decoder(handle);
                ctx.kernel.audio.flush();
            } else if is_pause {
                ctx.kernel.vfs.pause_mp3_decoder(handle, true);
                ctx.kernel.audio.set_paused(true);
            } else if is_resume {
                ctx.kernel.vfs.pause_mp3_decoder(handle, false);
                ctx.kernel.audio.set_paused(false);
            } else if is_set_volume && in_len >= 4 {
                let value = u32::from_le_bytes(
                    ctx.cpu.read_mem(input_buf, 4)?.try_into().unwrap_or([0; 4]),
                );
                ctx.kernel.vfs.set_mp3_decoder_volume(handle, value);
            }

            if out_buf != 0 && out_len > 0 {
                let reply = ctx
                    .kernel
                    .vfs
                    .mp3_decoder_reply(handle, code, out_len as usize);
                ctx.cpu.write_mem(out_buf, &reply)?;
            }
            if returned_p != 0 {
                let returned = if code == MASG_STATE
                    || code == MASG_GET_POSITION
                    || code == MASG_GET_VOLUME
                    || code == MASG_HAS_MP3
                {
                    out_len.min(4)
                } else {
                    0
                };
                ctx.cpu.write_mem(returned_p, &returned.to_le_bytes())?;
            }
            log::debug!(
                "DeviceIoControl(MAS1: h=0x{handle:08x}, code=0x{code:08x}, \
                 in_len={in_len}, out_len={out_len}) -> TRUE"
            );
            return Ok(DispatchOutcome::ReturnedR0(1));
        }
        log::debug!(
            "DeviceIoControl(h=0x{handle:08x}, code=0x{code:08x}, in_len={in_len}, \
             out_len={out_len}) -> FALSE (not a volume handle)"
        );
        return Ok(DispatchOutcome::ReturnedR0(0));
    };

    // `IOCTL_DISK_GET_STORAGEID` fills a `STORAGE_IDENTIFICATION`: a
    // four-DWORD header — size, flags, then *byte offsets from the start
    // of the header* to a manufacturer and a serial string — followed by
    // the strings themselves. Ball Busters reads its card's serial with
    // `strtoul((char *)id + id->dwSerialNumOffset, NULL, 10)`, so the
    // reply has to carry a real decimal string at a real offset;
    // answering with a zeroed buffer left the offset at 0 and parsed
    // serial 0, which the game read as an empty slot and showed
    // "SD card removed" for.
    if code == IOCTL_DISK_GET_STORAGEID {
        let prefix = volume.prefix.clone();
        let serial = volume.serial();
        let mfr = b"PocketHLE\0";
        let serial_text = format!("{serial}\0").into_bytes();
        let total = 16 + mfr.len() + serial_text.len();
        let mut reply = Vec::with_capacity(total);
        reply.extend_from_slice(&(total as u32).to_le_bytes());
        // `dwFlags` stays 0. Bit 0 is "manufacturer ID invalid" and bit 1
        // "serial number invalid"; Ball Busters tests bit 1 before it
        // trusts the offsets and gives up on the card when it is set.
        reply.extend_from_slice(&0u32.to_le_bytes());
        reply.extend_from_slice(&16u32.to_le_bytes());
        reply.extend_from_slice(&(16 + mfr.len() as u32).to_le_bytes());
        reply.extend_from_slice(mfr);
        reply.extend_from_slice(&serial_text);

        // The strings do not fit in the bare header, so the first call —
        // which is the only size a caller can guess — has to fail with
        // the required size in `dwSize` rather than truncate. Ball
        // Busters expects exactly that: it reallocates to `dwSize` and
        // asks again, and a truncated decimal serial would parse as a
        // different, wrong number.
        if (out_len as usize) < reply.len() {
            log::debug!(
                "DeviceIoControl(volume {prefix:?}, IOCTL_DISK_GET_STORAGEID, out_len={out_len}) \
                 -> FALSE (needs {} bytes)",
                reply.len()
            );
            if out_buf != 0 && out_len >= 4 {
                ctx.cpu
                    .write_mem(out_buf, &(reply.len() as u32).to_le_bytes())?;
            }
            if returned_p != 0 {
                ctx.cpu.write_mem(returned_p, &0u32.to_le_bytes())?;
            }
            // A real driver would also `SetLastError`
            // `ERROR_INSUFFICIENT_BUFFER` here. There is no last-error
            // state in this HLE — `GetLastError` is a constant-zero thunk
            // — and the size in `dwSize` is what a caller retries from.
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        log::debug!(
            "DeviceIoControl(volume {prefix:?}, IOCTL_DISK_GET_STORAGEID, out_len={out_len}) \
             -> TRUE (serial {serial})"
        );
        if out_buf != 0 {
            let mut out = vec![0u8; out_len as usize];
            out[..reply.len()].copy_from_slice(&reply);
            ctx.cpu.write_mem(out_buf, &out)?;
        }
        if returned_p != 0 {
            ctx.cpu
                .write_mem(returned_p, &(reply.len() as u32).to_le_bytes())?;
        }
        return Ok(DispatchOutcome::ReturnedR0(1));
    }

    log::debug!(
        "DeviceIoControl(volume {:?}, code=0x{code:08x}, in_len={in_len}, out_buf=0x{out_buf:08x}, \
         out_len={out_len}) -> TRUE",
        volume.prefix
    );
    // The guest sized the buffer, so zero it and report it filled: a
    // query answered with success but stale stack contents is worse
    // than one answered with zeroes.
    if out_buf != 0 && out_len > 0 {
        let out = vec![0u8; out_len as usize];
        ctx.cpu.write_mem(out_buf, &out)?;
    }
    if returned_p != 0 {
        ctx.cpu.write_mem(returned_p, &out_len.to_le_bytes())?;
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
    } else if let Some(h) = gles_module_handle(&name) {
        h
    } else {
        FAKE_MODULE_HANDLE
    };
    Ok(DispatchOutcome::ReturnedR0(handle))
}

/// Map a `libGLES_*` module name to the handle
/// [`pocket_kernel::build_dynamic_exports`] filed its thunks under.
/// Returns `None` for anything that is not one of the two client
/// libraries we emulate.
pub(crate) fn gles_module_handle(name: &str) -> Option<u32> {
    let stem = name.rsplit(['\\', '/']).next().unwrap_or(name);
    // Guests spell these every which way — `libGLES_CM.dll`,
    // `libgles_cm`, `\Windows\libGLES_CL.dll`. WinCE filenames are
    // case-insensitive, so fold before matching or the real-world
    // spelling misses and `GetProcAddress` hands back null.
    let stem = stem.to_ascii_lowercase();
    let stem = stem.strip_suffix(".dll").unwrap_or(&stem);
    match stem {
        "libgles_cm" => Some(pocket_kernel::GLES_CM_MODULE_HANDLE),
        "libgles_cl" => Some(pocket_kernel::GLES_CL_MODULE_HANDLE),
        _ => None,
    }
}

fn get_module_information(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _ = ctx.arg_u32(0)?;
    let module = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    let size = ctx.arg_u32(3)?;
    if out == 0 || size < 12 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let (base, image_size, entry) = if module == 0 || module == FAKE_MODULE_HANDLE {
        (
            ctx.kernel.image_base,
            ctx.kernel.image_size,
            ctx.kernel.image_entry,
        )
    } else if let Some(loaded) = ctx.kernel.module_by_handle(module) {
        (loaded.base, loaded.image_size, loaded.image_entry)
    } else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let mut info = [0u8; 12];
    info[0..4].copy_from_slice(&base.to_le_bytes());
    info[4..8].copy_from_slice(&image_size.to_le_bytes());
    info[8..12].copy_from_slice(&entry.to_le_bytes());
    ctx.cpu.write_mem(out, &info)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn load_library_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let path_p = ctx.arg_u32(0)?;
    let path = read_wstr(ctx, path_p, 260).unwrap_or_default();
    let name = String::from_utf16_lossy(&path).to_ascii_lowercase();
    if name.ends_with("coredll.dll") || name == "coredll" {
        log::debug!("LoadLibraryW({name:?}) -> 0x{FAKE_MODULE_HANDLE:08x}");
        return Ok(DispatchOutcome::ReturnedR0(FAKE_MODULE_HANDLE));
    }
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
    if name.ends_with("hss.dll") || name == "hss" {
        let handle = if ctx.kernel.dynamic_exports.contains_key(&HSS_MODULE_HANDLE) {
            HSS_MODULE_HANDLE
        } else {
            0
        };
        log::debug!("LoadLibraryW({name:?}) -> 0x{handle:08x} (PocketHLE HSS)");
        return Ok(DispatchOutcome::ReturnedR0(handle));
    }
    if name.ends_with("ole32.dll") || name == "ole32" {
        let handle = if ctx
            .kernel
            .dynamic_exports
            .contains_key(&pocket_kernel::OLE32_MODULE_HANDLE)
        {
            pocket_kernel::OLE32_MODULE_HANDLE
        } else {
            0
        };
        log::debug!("LoadLibraryW({name:?}) -> 0x{handle:08x} (PocketHLE OLE32)");
        return Ok(DispatchOutcome::ReturnedR0(handle));
    }
    if name.ends_with("imgdecmp.dll") || name == "imgdecmp" {
        let handle = if ctx.kernel.dynamic_exports.contains_key(&FAKE_MODULE_HANDLE) {
            FAKE_MODULE_HANDLE
        } else {
            0
        };
        log::debug!("LoadLibraryW({name:?}) -> 0x{handle:08x} (PocketHLE image decoder)");
        return Ok(DispatchOutcome::ReturnedR0(handle));
    }
    if let Some(handle) = gles_module_handle(&name) {
        // Only report success if the loader actually synthesized
        // thunks for this library — otherwise `GetProcAddress` would
        // hand back null for every entry point and the guest would be
        // worse off than if the load had failed outright.
        let handle = if ctx.kernel.dynamic_exports.contains_key(&handle) {
            handle
        } else {
            0
        };
        log::info!("LoadLibraryW({name:?}) -> 0x{handle:08x} (PocketHLE GLES)");
        return Ok(DispatchOutcome::ReturnedR0(handle));
    }
    // Already resident? CE hands back the same base and bumps the
    // reference count.
    if let Some(existing) = ctx.kernel.module_by_name(&name).map(|m| m.handle) {
        if let Some(m) = ctx.kernel.modules.iter_mut().find(|m| m.handle == existing) {
            m.refcount = m.refcount.saturating_add(1);
        }
        log::debug!("LoadLibraryW({name:?}) -> 0x{existing:08x} (cached)");
        return Ok(DispatchOutcome::ReturnedR0(existing));
    }
    match load_resource_module(ctx, &name)? {
        Some(base) => {
            log::info!("LoadLibraryW({name:?}) -> 0x{base:08x}");
            Ok(DispatchOutcome::ReturnedR0(base))
        }
        None => {
            log::debug!("LoadLibraryW({name:?}) -> NULL");
            Ok(DispatchOutcome::ReturnedR0(0))
        }
    }
}

/// Map a satellite DLL found next to the game so its *resources* become
/// reachable.
///
/// PocketHLE never executes guest code out of a runtime-loaded module:
/// nothing resolves its exports, and the titles that call `LoadLibraryW`
/// on a companion DLL (Solitaire + `pegcards.dll`, for instance) import
/// no `GetProcAddress` — they only want `FindResourceW` / `LoadBitmapW` /
/// `LoadStringW` to see the artwork inside. So we skip base relocations
/// and the IAT and simply place the image's sections at a fresh 16 MiB
/// slot in the module region, recording its resource directory.
///
/// Returns `None` (i.e. guest-visible NULL) when the file can't be found
/// or parsed, or when the module region is exhausted.
fn load_resource_module(ctx: &mut CallCtx<'_>, request: &str) -> Result<Option<u32>, KernelError> {
    let Some(host_path) = ctx.kernel.find_module_file(request) else {
        log::debug!("LoadLibraryW({request:?}): no host file found");
        return Ok(None);
    };
    let image = match pocket_pe::load_file(&host_path) {
        Ok(img) => img,
        Err(e) => {
            log::warn!(
                "LoadLibraryW({request:?}): {} is not a PE ({e})",
                host_path.display()
            );
            return Ok(None);
        }
    };
    let base = ctx.kernel.next_module_base;
    if base
        .checked_add(MODULE_REGION_STRIDE)
        .is_none_or(|end| end > MODULE_REGION_END)
    {
        log::warn!("LoadLibraryW({request:?}): module region exhausted");
        return Ok(None);
    }
    for s in &image.sections {
        let mut prot = Prot::READ;
        if s.is_writable() {
            prot |= Prot::WRITE;
        }
        if s.is_executable() {
            prot |= Prot::EXEC;
        }
        let aligned = pocket_cpu::round_up_to_page(s.virtual_size.max(s.data.len() as u32));
        if aligned == 0 {
            continue;
        }
        ctx.cpu
            .map_region(base + s.virtual_address, aligned, prot)?;
        ctx.cpu.write_mem(base + s.virtual_address, &s.data)?;
    }
    ctx.kernel.next_module_base = base + MODULE_REGION_STRIDE;
    let name = module_file_name(request);
    log::info!(
        "loaded module {name} from {} at 0x{base:08x} ({} sections, {} resources)",
        host_path.display(),
        image.sections.len(),
        image.resources.len()
    );
    ctx.kernel.modules.push(LoadedModule {
        handle: base,
        name,
        base,
        image_size: image.size_of_image,
        image_entry: base.wrapping_add(image.entry_point),
        resources: image.resources,
        refcount: 1,
    });
    Ok(Some(base))
}

/// Find a resource in the scope named by `hModule`, falling back to a
/// search across every loaded image.
///
/// Games are careless with the `hInstance` they pass here — a satellite
/// DLL's resource is often requested with the EXE's instance handle, or
/// with a handle from a `LoadLibraryW` we never saw — so a miss in the
/// nominal scope is not fatal. Returns the entry (cloned, so no borrow
/// of `ctx.kernel` outlives the lookup) together with the image base its
/// `data_rva` is relative to.
fn lookup_resource(
    kernel: &KernelState,
    hmodule: u32,
    ty: &ResourceKey,
    name: &ResourceKey,
) -> Option<(ResourceEntry, u32)> {
    let (scoped, scoped_base) = kernel.resource_scope(hmodule);
    if let Some(e) = scoped.iter().find(|e| e.ty == *ty && e.name == *name) {
        return Some((e.clone(), scoped_base));
    }
    for (entries, base) in kernel.resource_scopes() {
        if let Some(e) = entries.iter().find(|e| e.ty == *ty && e.name == *name) {
            return Some((e.clone(), base));
        }
    }
    None
}

/// Resolve a resource *address* previously handed to the guest by
/// `FindResourceW` back to the entry it came from, whichever image that
/// was.
fn resource_at_address(kernel: &KernelState, addr: u32) -> Option<(ResourceEntry, u32)> {
    for (entries, base) in kernel.resource_scopes() {
        let rva = addr.wrapping_sub(base);
        if let Some(e) = entries.iter().find(|e| e.data_rva == rva) {
            return Some((e.clone(), base));
        }
    }
    None
}

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
    let path = ctx.kernel.module_path.clone();
    let written = write_wide_str(ctx.cpu, dst, cap, &path)?;
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
    let path = ctx.kernel.module_path.clone();
    let bytes_needed = (path.encode_utf16().count() as u32 + 1) * 2;
    let va = match ctx.kernel.heap.alloc(bytes_needed) {
        Some(p) => p,
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    write_wide_str(ctx.cpu, va, bytes_needed / 2, &path)?;
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
fn soft_stoi64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_i64(read_f64(ctx, 0)? as i64))
}
fn soft_stou64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let v = read_f64(ctx, 0)?;
    let r = if v < 0.0 || !v.is_finite() {
        0
    } else {
        v as u64
    };
    Ok(DispatchOutcome::ReturnedR0R1(r as u32, (r >> 32) as u32))
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
/// The MS ARM CE CRT's 64-bit integer <-> floating point helpers.
///
/// SkyForce Reloaded's first-run benchmark converts the tick deltas it
/// measures (`__int64`) to `double` before dividing, so a missing
/// `__i64tod` left it comparing garbage and the "BENCHMARKING PLEASE
/// WAIT..." screen never finished.
fn read_i64(ctx: &mut CallCtx<'_>, idx_lo: u8) -> Result<i64, KernelError> {
    let lo = ctx.arg_u32(idx_lo)? as u64;
    let hi = ctx.arg_u32(idx_lo + 1)? as u64;
    Ok(((hi << 32) | lo) as i64)
}

fn ret_i64(v: i64) -> DispatchOutcome {
    let bits = v as u64;
    DispatchOutcome::ReturnedR0R1(bits as u32, (bits >> 32) as u32)
}

fn soft_i64tod(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(read_i64(ctx, 0)? as f64))
}
fn soft_u64tod(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f64(read_i64(ctx, 0)? as u64 as f64))
}
fn soft_i64tos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(read_i64(ctx, 0)? as f32))
}
fn soft_u64tos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(read_i64(ctx, 0)? as u64 as f32))
}
fn soft_dtoi64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_i64(read_f64(ctx, 0)? as i64))
}
fn soft_dtou64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let v = read_f64(ctx, 0)?;
    let r = if v < 0.0 || !v.is_finite() {
        0
    } else {
        v as u64
    };
    Ok(DispatchOutcome::ReturnedR0R1(r as u32, (r >> 32) as u32))
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
    sync_direct_framebuffer_write(ctx, dst, len)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn sync_direct_framebuffer_write(
    ctx: &mut CallCtx<'_>,
    dst: u32,
    len: usize,
) -> Result<(), KernelError> {
    let base = SYNTHETIC_FRAMEBUFFER_BASE;
    let end = base.saturating_add(ctx.kernel.framebuffer.byte_size());
    let write_end = dst.saturating_add(len as u32);
    if dst >= end || write_end <= base {
        return Ok(());
    }
    let fb_len = ctx.kernel.framebuffer.pixels.len();
    if ctx.kernel.gx_readback_scratch.len() != fb_len {
        ctx.kernel.gx_readback_scratch.resize(fb_len, 0);
    }
    ctx.cpu
        .read_mem_into(base, &mut ctx.kernel.gx_readback_scratch)?;
    if ctx.kernel.gx_readback_scratch != ctx.kernel.framebuffer.pixels {
        ctx.kernel
            .framebuffer
            .pixels
            .copy_from_slice(&ctx.kernel.gx_readback_scratch);
        ctx.kernel.framebuffer.mark_dirty();
        ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
        // Tell the run loop the guest announces its own frames, so it
        // can stop polling for plain stores it will never find.
        ctx.kernel.direct_fb_frames = ctx.kernel.direct_fb_frames.saturating_add(1);
    }
    Ok(())
}

fn memchr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let value = ctx.arg_u32(1)? as u8;
    let len = ctx.arg_u32(2)? as usize;
    if len == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let scratch = &mut ctx.kernel.mem_op_scratch;
    if scratch.len() < len {
        scratch.resize(len, 0);
    }
    ctx.cpu.read_mem_into(ptr, &mut scratch[..len])?;
    let result = scratch[..len]
        .iter()
        .position(|byte| *byte == value)
        .map(|offset| ptr.wrapping_add(offset as u32))
        .unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(result))
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

/// `int _stricmp(const char *a, const char *b)` — ASCII-case-folded
/// compare, the CRT spelling of `strcasecmp`.
fn stricmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pa = ctx.arg_u32(0)?;
    let pb = ctx.arg_u32(1)?;
    let a = read_cstr(ctx, pa, 0x10000)?.to_ascii_lowercase();
    let b = read_cstr(ctx, pb, 0x10000)?.to_ascii_lowercase();
    Ok(DispatchOutcome::ReturnedR0(cmp_to_int(a.cmp(&b)) as u32))
}

/// `int _strnicmp(const char *a, const char *b, size_t n)`.
fn strnicmp(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pa = ctx.arg_u32(0)?;
    let pb = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let a = read_cstr(ctx, pa, n)?.to_ascii_lowercase();
    let b = read_cstr(ctx, pb, n)?.to_ascii_lowercase();
    Ok(DispatchOutcome::ReturnedR0(cmp_to_int(a.cmp(&b)) as u32))
}

/// `int atoi(const char *s)` / `long atol(const char *s)`. C semantics:
/// skip leading whitespace, optional sign, then as many digits as
/// parse; anything else yields `0` rather than an error.
fn atof_handler(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let text = read_cstr_string(ctx, p, 0x1000)?;
    let value = text.trim().parse::<f64>().unwrap_or(0.0);
    let bits = value.to_bits();
    ctx.cpu.write_reg(ArmReg::R0, bits as u32)?;
    ctx.cpu.write_reg(ArmReg::R1, (bits >> 32) as u32)?;
    Ok(DispatchOutcome::ReturnedR0(bits as u32))
}

fn atoi_handler(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let text = read_cstr_string(ctx, p, 0x1000)?;
    let mut it = text.trim_start().chars().peekable();
    let mut digits = String::new();
    if matches!(it.peek(), Some('+') | Some('-')) {
        digits.push(it.next().unwrap());
    }
    while let Some(c) = it.peek() {
        if c.is_ascii_digit() {
            digits.push(it.next().unwrap());
        } else {
            break;
        }
    }
    let value = digits.parse::<i64>().unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(value as i32 as u32))
}

/// MSVC `_isctype(int c, int mask)`: returns the subset of `mask` the
/// character satisfies. `isalpha`, `isdigit`, `isspace` and friends are
/// macros that call straight into this, so returning 0 unconditionally
/// broke every parser the guest CRT has.
fn itow_handler(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value = ctx.arg_u32(0)? as i32;
    let dst = ctx.arg_u32(1)?;
    let radix = ctx.arg_u32(2)?;
    if dst == 0 || !(2..=36).contains(&radix) {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let negative = value < 0 && radix == 10;
    let mut magnitude = if negative {
        (value as i64).unsigned_abs()
    } else {
        value as u32 as u64
    };
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    loop {
        digits.push(alphabet[(magnitude % radix as u64) as usize] as u16);
        magnitude /= radix as u64;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        digits.push(b'-' as u16);
    }
    digits.reverse();
    digits.push(0);
    ctx.cpu.write_mem(dst, &wide_to_bytes(&digits))?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn itoa_handler(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value = ctx.arg_u32(0)? as i32;
    let dst = ctx.arg_u32(1)?;
    let radix = ctx.arg_u32(2)?;
    if dst == 0 || !(2..=36).contains(&radix) {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let negative = value < 0 && radix == 10;
    let mut magnitude = if value < 0 && radix == 10 {
        (value as i64).unsigned_abs()
    } else {
        value as u32 as u64
    };
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    loop {
        digits.push(alphabet[(magnitude % radix as u64) as usize]);
        magnitude /= radix as u64;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        digits.push(b'-');
    }
    digits.reverse();
    digits.push(0);
    ctx.cpu.write_mem(dst, &digits)?;
    Ok(DispatchOutcome::ReturnedR0(dst))
}

fn isctype(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const UPPER: u32 = 0x0001;
    const LOWER: u32 = 0x0002;
    const DIGIT: u32 = 0x0004;
    const SPACE: u32 = 0x0008;
    const PUNCT: u32 = 0x0010;
    const CONTROL: u32 = 0x0020;
    const BLANK: u32 = 0x0040;
    const HEX: u32 = 0x0080;
    const ALPHA: u32 = 0x0100;

    let c = ctx.arg_u32(0)?;
    let mask = ctx.arg_u32(1)?;
    let ch = (c & 0xff) as u8;
    let mut bits = 0u32;
    if ch.is_ascii_uppercase() {
        bits |= UPPER | ALPHA;
    }
    if ch.is_ascii_lowercase() {
        bits |= LOWER | ALPHA;
    }
    if ch.is_ascii_digit() {
        bits |= DIGIT;
    }
    if matches!(ch, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        bits |= SPACE;
    }
    if ch.is_ascii_punctuation() {
        bits |= PUNCT;
    }
    if ch < 0x20 || ch == 0x7f {
        bits |= CONTROL;
    }
    if ch == b' ' || ch == b'\t' {
        bits |= BLANK;
    }
    if ch.is_ascii_hexdigit() {
        bits |= HEX;
    }
    Ok(DispatchOutcome::ReturnedR0(bits & mask))
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
    // An empty needle matches at the start, and a miss is NULL — not
    // the haystack. `windows(0)` also panics, so it must not be reached.
    if needle.is_empty() {
        return Ok(DispatchOutcome::ReturnedR0(h));
    }
    let found = hay
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|pos| h + pos as u32);
    Ok(DispatchOutcome::ReturnedR0(found.unwrap_or(0)))
}

/// `char *strtok(char *s, const char *delim)`
///
/// Destructive and stateful: the token is terminated in place and the
/// scan position is remembered so the next call can be made with a NULL
/// string. Sticky Balls tokenises its language table this way right
/// after the language-select screen and uses the result without a NULL
/// check, so with no handler here it read address 0 and the CPU faulted
/// one frame later.
fn strtok(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (sp, delimp) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    // A NULL string means "carry on from last time"; a finished scan
    // has nothing to carry on from.
    let base = if sp != 0 { sp } else { ctx.kernel.strtok_pos };
    if base == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let delims = read_cstr(ctx, delimp, 0x10000)?;
    let rest = read_cstr(ctx, base, 0x10000)?;
    // Leading delimiters are skipped, and a run of nothing but
    // delimiters ends the scan.
    let Some(start) = rest.iter().position(|b| !delims.contains(b)) else {
        ctx.kernel.strtok_pos = 0;
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let token = base + start as u32;
    match rest[start..].iter().position(|b| delims.contains(b)) {
        Some(len) => {
            // The delimiter becomes this token's terminator, which is
            // why the caller's buffer has to be writable.
            let end = token + len as u32;
            ctx.cpu.write_mem(end, &[0])?;
            ctx.kernel.strtok_pos = end + 1;
        }
        // The last token runs to the string's own terminator, so there
        // is nothing left to hand out.
        None => ctx.kernel.strtok_pos = 0,
    }
    Ok(DispatchOutcome::ReturnedR0(token))
}

/// `size_t strspn(const char *s, const char *accept)` — length of the
/// leading run of `s` made up only of bytes from `accept`.
fn strspn(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (sp, setp) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    let s = read_cstr(ctx, sp, 0x10000)?;
    let set = read_cstr(ctx, setp, 0x10000)?;
    let n = s.iter().take_while(|b| set.contains(b)).count();
    Ok(DispatchOutcome::ReturnedR0(n as u32))
}

/// `size_t strcspn(const char *s, const char *reject)` — length of the
/// leading run of `s` containing no byte from `reject`.
fn strcspn(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (sp, setp) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    let s = read_cstr(ctx, sp, 0x10000)?;
    let set = read_cstr(ctx, setp, 0x10000)?;
    let n = s.iter().take_while(|b| !set.contains(b)).count();
    Ok(DispatchOutcome::ReturnedR0(n as u32))
}

/// `char *strpbrk(const char *s, const char *accept)` — first byte of
/// `s` that appears in `accept`, or NULL.
fn strpbrk(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (base, setp) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    let s = read_cstr(ctx, base, 0x10000)?;
    let set = read_cstr(ctx, setp, 0x10000)?;
    let found = s.iter().position(|b| set.contains(b));
    Ok(DispatchOutcome::ReturnedR0(
        found.map_or(0, |i| base + i as u32),
    ))
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

fn wtol(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let s = read_wstr(ctx, ptr, 0x1000)?;
    let text = String::from_utf16_lossy(&s);
    let value = text.trim().parse::<i32>().unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(value as u32))
}

fn wcspbrk(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let s = ctx.arg_u32(0)?;
    let accept = ctx.arg_u32(1)?;
    let chars = read_wstr(ctx, s, 0x10000)?;
    let accepted = read_wstr(ctx, accept, 0x10000)?;
    for (i, ch) in chars.iter().enumerate() {
        if accepted.contains(ch) {
            return Ok(DispatchOutcome::ReturnedR0(s + i as u32 * 2));
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

thread_local! {
    /// Saved scan position for [`wcstok`], mirroring the CRT's per-thread
    /// `wcstok` state. Zero means "no tokenising in progress".
    static WCSTOK_NEXT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// `wchar_t *wcstok(wchar_t *str, const wchar_t *delim)`
///
/// Splits `str` in place: leading delimiters are skipped, the delimiter that
/// ends the token is overwritten with `L'\0'`, and the position after it is
/// remembered so later calls can pass `NULL` to continue. Astraware's Pocket PC
/// titles tokenise their resource-database lists with it, so a stub that always
/// answered `NULL` derailed their loader.
fn wcstok(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let str_ptr = ctx.arg_u32(0)?;
    let delim_ptr = ctx.arg_u32(1)?;
    let delims = read_wstr(ctx, delim_ptr, 256)?;
    let mut cur = if str_ptr != 0 {
        str_ptr
    } else {
        WCSTOK_NEXT.with(|c| c.get())
    };
    if cur == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut wide = [0u8; 2];
    // Skip the run of delimiters in front of the next token.
    loop {
        if ctx.cpu.read_mem_into(cur, &mut wide).is_err() {
            WCSTOK_NEXT.with(|c| c.set(0));
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        let ch = u16::from_le_bytes(wide);
        if ch == 0 {
            WCSTOK_NEXT.with(|c| c.set(0));
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        if !delims.contains(&ch) {
            break;
        }
        cur += 2;
    }
    let start = cur;
    // Terminate the token at the next delimiter and remember where to resume.
    loop {
        if ctx.cpu.read_mem_into(cur, &mut wide).is_err() {
            WCSTOK_NEXT.with(|c| c.set(0));
            break;
        }
        let ch = u16::from_le_bytes(wide);
        if ch == 0 {
            WCSTOK_NEXT.with(|c| c.set(0));
            break;
        }
        if delims.contains(&ch) {
            ctx.cpu.write_mem(cur, &[0, 0])?;
            WCSTOK_NEXT.with(|c| c.set(cur + 2));
            break;
        }
        cur += 2;
    }
    Ok(DispatchOutcome::ReturnedR0(start))
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
/// Consume the length modifiers in front of a `printf` conversion,
/// returning `(long, short)`.
///
/// `h` is not decoration to skip: in a wide format string `%hs` takes a
/// *narrow* `char*`. Treating the `h` as an unknown conversion both left
/// `%hs` sitting in the output and consumed no argument, which then
/// shifted every later conversion in the same format along by one. Ball
/// Busters builds the path of its SD-card marker file with
/// `swprintf(L"\\SD Card\\%hs\\%hs", …)`, could not open the unformatted
/// result, and concluded the card had been ejected. `w` is the Windows
/// spelling of `l` for strings.
///
/// `hh` and `ll` collapse onto `h` and `l`: they differ only in integer
/// width, and every integer in both renderers is read as 32 bits.
fn scan_length_mods(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> (bool, bool) {
    let mut long = false;
    let mut short = false;
    loop {
        match chars.peek().copied() {
            Some('l') | Some('L') | Some('w') => {
                long = true;
                chars.next();
            }
            Some('h') => {
                short = true;
                chars.next();
            }
            _ => break,
        }
    }
    (long, short)
}

/// Whether a `%s` / `%c` conversion pulls a wide argument: a wide format
/// takes wide strings and a narrow one narrow strings, with `h` and
/// `l`/`w` each overriding that default.
fn pulls_wide_arg(fmt_is_wide: bool, long: bool, short: bool) -> bool {
    if fmt_is_wide {
        !short
    } else {
        long
    }
}

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
        let (long, short) = scan_length_mods(&mut chars);
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
                // `%hd` is a short widened to int by the call; narrow it
                // back so a negative short prints as one.
                piece = if short {
                    (v as i16).to_string()
                } else {
                    v.to_string()
                };
            }
            'u' => {
                let v = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                piece = if short {
                    (v as u16).to_string()
                } else {
                    v.to_string()
                };
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
                // `%c` follows the format's own width unless a modifier
                // overrides it, matching `%s` below.
                let mask = if pulls_wide_arg(fmt_is_wide, long, short) {
                    0xffff
                } else {
                    0xff
                };
                if let Some(ch) = char::from_u32(v & mask) {
                    piece.push(ch);
                }
            }
            's' => {
                let p = read_vararg_u32(ctx, next_arg)?;
                next_arg += 1;
                // A wide format takes wide strings, a narrow one narrow
                // strings; `h` and `l`/`w` each override that default.
                let pulls_wide = pulls_wide_arg(fmt_is_wide, long, short);
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

/// Pseudo-`FILE *` handles handed out by `_getstdfilex(0|1|2)`.
///
/// Games built with Microsoft's CE CRT reach `stdout` / `stderr`
/// through `_getstdfilex`, then log with `fprintf`. Returning NULL
/// (the old stub) silently discarded every diagnostic the game
/// produced — including the messages that explain why it stopped
/// making progress — so hand out recognisable fake streams instead
/// and mirror anything written to them into the emulator log.
const STD_FILE_BASE: u32 = 0x5D10_0000;

fn std_file_label(h: u32) -> Option<&'static str> {
    match h {
        v if v == STD_FILE_BASE => Some("stdin"),
        v if v == STD_FILE_BASE + 1 => Some("stdout"),
        v if v == STD_FILE_BASE + 2 => Some("stderr"),
        _ => None,
    }
}

/// `FILE *_getstdfilex(int which)` — 0 = stdin, 1 = stdout, 2 = stderr.
fn get_std_file(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let which = ctx.arg_u32(0)?.min(2);
    Ok(DispatchOutcome::ReturnedR0(STD_FILE_BASE + which))
}

/// `FILE *_wfreopen(const wchar_t *path, const wchar_t *mode, FILE *stream)`
///
/// Games redirect `stdout` into a log file on the device. We keep the
/// stream identity (so later `fprintf`s still land in the emulator
/// log) but also open the file so the log is written where the game
/// expects it.
fn reopen_common(
    ctx: &mut CallCtx<'_>,
    path: &str,
    mode: &str,
    stream: u32,
) -> Result<DispatchOutcome, KernelError> {
    if std_file_label(stream).is_some() {
        log::debug!(
            "freopen({path:?}, {mode:?}) on {} stream",
            std_file_label(stream).unwrap_or("")
        );
        return Ok(DispatchOutcome::ReturnedR0(stream));
    }
    let h = open_cstr_path(ctx, path, mode);
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn wfreopen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let path_p = ctx.arg_u32(0)?;
    let mode_p = ctx.arg_u32(1)?;
    let stream = ctx.arg_u32(2)?;
    let path = String::from_utf16_lossy(&read_wstr(ctx, path_p, 260)?);
    let mode = String::from_utf16_lossy(&read_wstr(ctx, mode_p, 8)?);
    reopen_common(ctx, &path, &mode, stream)
}

fn freopen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let path_p = ctx.arg_u32(0)?;
    let mode_p = ctx.arg_u32(1)?;
    let stream = ctx.arg_u32(2)?;
    let path = read_cstr_string(ctx, path_p, 260)?;
    let mode = read_cstr_string(ctx, mode_p, 8)?;
    reopen_common(ctx, &path, &mode, stream)
}

/// `int fprintf(FILE *stream, const char *fmt, ...)`.
fn crt_fprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let stream = ctx.arg_u32(0)?;
    let fmt_p = ctx.arg_u32(1)?;
    let fmt = read_cstr_string(ctx, fmt_p, 0x4000)?;
    let s = render_printf(ctx, &fmt, false, 2)?;
    emit_stream_text(ctx, stream, &s)?;
    Ok(DispatchOutcome::ReturnedR0(s.len() as u32))
}

/// `int vfprintf(FILE *stream, const char *fmt, va_list ap)`.
fn crt_vfprintf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let stream = ctx.arg_u32(0)?;
    let fmt_p = ctx.arg_u32(1)?;
    let va_p = ctx.arg_u32(2)?;
    let fmt = read_cstr_string(ctx, fmt_p, 0x4000)?;
    let s = render_printf_va(ctx, &fmt, false, va_p)?;
    emit_stream_text(ctx, stream, &s)?;
    Ok(DispatchOutcome::ReturnedR0(s.len() as u32))
}

/// Route text produced by `fprintf` / `vfprintf` either into the
/// emulator log (std streams) or into the backing VFS file.
fn emit_stream_text(ctx: &mut CallCtx<'_>, stream: u32, text: &str) -> Result<(), KernelError> {
    if let Some(label) = std_file_label(stream) {
        log::debug!("guest {label}: {}", text.trim_end_matches(['\r', '\n']));
        return Ok(());
    }
    if ctx.kernel.vfs.is_open(stream) {
        ctx.kernel.vfs.write(stream, text.as_bytes());
    } else {
        log::debug!("guest fprintf to unknown stream 0x{stream:08x}: {text}");
    }
    Ok(())
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

/// The shared engine behind `sscanf`. `input` is the subject string,
/// `fmt` the format, and `next_arg` yields successive pointer
/// arguments (register args then stack args, exactly like the
/// printf side).
///
/// Returns the number of *assigned* conversions, or `-1` for an early
/// input failure — the same `EOF`-vs-zero distinction real `sscanf`
/// makes, which callers use to tell "nothing matched" from "the string
/// ran out first".
fn run_sscanf(
    ctx: &mut CallCtx<'_>,
    input: &str,
    fmt: &str,
    mut next_arg: impl FnMut(&mut CallCtx<'_>) -> Result<u32, KernelError>,
) -> Result<i32, KernelError> {
    let src = input.as_bytes();
    let f = fmt.as_bytes();
    let mut si = 0usize;
    let mut fi = 0usize;
    let mut assigned = 0i32;

    while fi < f.len() {
        let fc = f[fi];

        // Whitespace in the format matches any run of input whitespace,
        // including none at all.
        if fc.is_ascii_whitespace() {
            fi += 1;
            while si < src.len() && src[si].is_ascii_whitespace() {
                si += 1;
            }
            continue;
        }

        if fc != b'%' {
            // Literal: must match exactly.
            if si >= src.len() {
                return Ok(if assigned == 0 { -1 } else { assigned });
            }
            if src[si] != fc {
                return Ok(assigned);
            }
            si += 1;
            fi += 1;
            continue;
        }

        fi += 1;
        if fi >= f.len() {
            break;
        }
        if f[fi] == b'%' {
            while si < src.len() && src[si].is_ascii_whitespace() {
                si += 1;
            }
            if si >= src.len() || src[si] != b'%' {
                return Ok(if assigned == 0 && si >= src.len() {
                    -1
                } else {
                    assigned
                });
            }
            si += 1;
            fi += 1;
            continue;
        }

        // `*` suppresses the assignment: the conversion still happens
        // and still has to succeed, it just consumes no argument.
        let suppress = f[fi] == b'*';
        if suppress {
            fi += 1;
        }

        // Field width.
        let mut width = 0usize;
        while fi < f.len() && f[fi].is_ascii_digit() {
            width = width * 10 + (f[fi] - b'0') as usize;
            fi += 1;
        }
        let width = if width == 0 { usize::MAX } else { width };

        // Length modifiers. `ll` (and the MSVC `I64`) widen to 64 bits;
        // `h` narrows to 16. `l` on a float means `double`.
        let mut short_form = false;
        let mut long_count = 0u32;
        loop {
            match f.get(fi) {
                Some(b'h') => {
                    short_form = true;
                    fi += 1;
                }
                Some(b'l') | Some(b'L') => {
                    long_count += 1;
                    fi += 1;
                }
                Some(b'I') if f[fi..].starts_with(b"I64") => {
                    long_count = 2;
                    fi += 3;
                }
                _ => break,
            }
        }
        let long_long = long_count >= 2;

        let Some(&conv) = f.get(fi) else { break };
        fi += 1;

        // Every conversion except `%c`, `%[` and `%n` skips leading
        // whitespace first.
        if !matches!(conv, b'c' | b'[' | b'n') {
            while si < src.len() && src[si].is_ascii_whitespace() {
                si += 1;
            }
        }

        match conv {
            b'n' => {
                // Not a conversion: reports how far we have read. Never
                // counts toward the return value.
                if !suppress {
                    let addr = next_arg(ctx)?;
                    scanf_store_int(ctx, addr, si as i64, short_form, long_long)?;
                }
            }
            b'd' | b'i' | b'u' | b'x' | b'X' | b'o' | b'p' => {
                let start = si;
                let mut neg = false;
                if si < src.len() && (src[si] == b'+' || src[si] == b'-') {
                    neg = src[si] == b'-';
                    si += 1;
                }
                // `%i` sniffs the base from the prefix; `%x` accepts an
                // optional `0x`; the rest are fixed.
                let mut radix: u32 = match conv {
                    b'x' | b'X' | b'p' => 16,
                    b'o' => 8,
                    b'i' => 0,
                    _ => 10,
                };
                if (radix == 16 || radix == 0)
                    && src.len() > si + 1
                    && src[si] == b'0'
                    && (src[si + 1] | 0x20) == b'x'
                {
                    si += 2;
                    radix = 16;
                } else if radix == 0 {
                    radix = if src.get(si) == Some(&b'0') { 8 } else { 10 };
                }
                let digits_from = si;
                let limit = start.saturating_add(width);
                let mut acc: i64 = 0;
                let mut any = false;
                while si < src.len() && si < limit {
                    let Some(d) = (src[si] as char).to_digit(radix) else {
                        break;
                    };
                    acc = acc.wrapping_mul(radix as i64).wrapping_add(d as i64);
                    any = true;
                    si += 1;
                }
                if !any {
                    // A lone `0x` with no hex digits still yields the 0.
                    if radix == 16 && digits_from > start && digits_from - start >= 2 {
                        si = digits_from - 1;
                        acc = 0;
                    } else {
                        return Ok(if assigned == 0 { -1 } else { assigned });
                    }
                }
                if neg {
                    acc = acc.wrapping_neg();
                }
                if !suppress {
                    let addr = next_arg(ctx)?;
                    scanf_store_int(ctx, addr, acc, short_form, long_long)?;
                    assigned += 1;
                }
            }
            b'f' | b'e' | b'E' | b'g' | b'G' | b'a' => {
                let start = si;
                let limit = start.saturating_add(width);
                let mut end = si;
                if end < src.len() && (src[end] == b'+' || src[end] == b'-') {
                    end += 1;
                }
                while end < src.len() && end < limit && src[end].is_ascii_digit() {
                    end += 1;
                }
                if end < src.len() && end < limit && src[end] == b'.' {
                    end += 1;
                    while end < src.len() && end < limit && src[end].is_ascii_digit() {
                        end += 1;
                    }
                }
                // Only accept an exponent if it actually has digits, so
                // "1e" parses as 1.0 and leaves the 'e' unread.
                if end < src.len() && end < limit && (src[end] | 0x20) == b'e' {
                    let mut probe = end + 1;
                    if probe < src.len() && (src[probe] == b'+' || src[probe] == b'-') {
                        probe += 1;
                    }
                    if probe < src.len() && src[probe].is_ascii_digit() {
                        while probe < src.len() && probe < limit && src[probe].is_ascii_digit() {
                            probe += 1;
                        }
                        end = probe;
                    }
                }
                let text = std::str::from_utf8(&src[start..end]).unwrap_or("");
                let Ok(value) = text.parse::<f64>() else {
                    return Ok(if assigned == 0 { -1 } else { assigned });
                };
                si = end;
                if !suppress {
                    let addr = next_arg(ctx)?;
                    if addr != 0 {
                        // Bare `%f` writes a 4-byte float; `%lf` a
                        // double. Getting this backwards silently
                        // corrupts the caller's next field.
                        if long_count >= 1 {
                            ctx.cpu.write_mem(addr, &value.to_bits().to_le_bytes())?;
                        } else {
                            ctx.cpu
                                .write_mem(addr, &(value as f32).to_bits().to_le_bytes())?;
                        }
                    }
                    assigned += 1;
                }
            }
            b'c' => {
                let n = if width == usize::MAX { 1 } else { width };
                if si + n > src.len() {
                    return Ok(if assigned == 0 { -1 } else { assigned });
                }
                let bytes = src[si..si + n].to_vec();
                si += n;
                if !suppress {
                    let addr = next_arg(ctx)?;
                    if addr != 0 {
                        // `%c` does *not* append a terminator.
                        ctx.cpu.write_mem(addr, &bytes)?;
                    }
                    assigned += 1;
                }
            }
            b's' => {
                let start = si;
                let limit = start.saturating_add(width);
                while si < src.len() && si < limit && !src[si].is_ascii_whitespace() {
                    si += 1;
                }
                if si == start {
                    return Ok(if assigned == 0 { -1 } else { assigned });
                }
                let mut bytes = src[start..si].to_vec();
                bytes.push(0);
                if !suppress {
                    let addr = next_arg(ctx)?;
                    if addr != 0 {
                        ctx.cpu.write_mem(addr, &bytes)?;
                    }
                    assigned += 1;
                }
            }
            b'[' => {
                let (negated, members, next_fi) = scanf_scanset(f, fi);
                fi = next_fi;
                let start = si;
                let limit = start.saturating_add(width);
                while si < src.len() && si < limit {
                    let hit = members.contains(&src[si]);
                    if hit == negated {
                        break;
                    }
                    si += 1;
                }
                if si == start {
                    return Ok(if assigned == 0 { -1 } else { assigned });
                }
                let mut bytes = src[start..si].to_vec();
                bytes.push(0);
                if !suppress {
                    let addr = next_arg(ctx)?;
                    if addr != 0 {
                        ctx.cpu.write_mem(addr, &bytes)?;
                    }
                    assigned += 1;
                }
            }
            other => {
                log::warn!("sscanf: unsupported conversion %{}", other as char);
                return Ok(assigned);
            }
        }
    }

    Ok(assigned)
}

/// `int sscanf(const char *src, const char *fmt, ...)`.
///
/// Games use this to parse their own config and asset text — Call of
/// Duty 2 runs it ~40 times during startup on material and shader
/// definitions. Returning a bogus value here leaves the parsed structs
/// full of stack garbage, so it is worth doing properly.
fn sscanf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let src_p = ctx.arg_u32(0)?;
    let fmt_p = ctx.arg_u32(1)?;
    if src_p == 0 || fmt_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(-1i32 as u32));
    }
    let input = read_cstr_string(ctx, src_p, 0x4000)?;
    let fmt = read_cstr_string(ctx, fmt_p, 0x4000)?;
    let mut idx = 2u8;
    let n = run_sscanf(ctx, &input, &fmt, move |c| {
        let a = c.arg_u32(idx)?;
        idx += 1;
        Ok(a)
    })?;
    Ok(DispatchOutcome::ReturnedR0(n as u32))
}

/// `int swscanf(const wchar_t *src, const wchar_t *fmt, ...)`.
/// Widens to the same engine: every conversion Pocket PC titles use is
/// ASCII-representable, and the `%s` / `%c` stores are narrow because
/// the guest asked for wide chars only in the format string.
fn swscanf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let src_p = ctx.arg_u32(0)?;
    let fmt_p = ctx.arg_u32(1)?;
    if src_p == 0 || fmt_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(-1i32 as u32));
    }
    let input = String::from_utf16_lossy(&read_wstr(ctx, src_p, 0x4000)?);
    let fmt = String::from_utf16_lossy(&read_wstr(ctx, fmt_p, 0x4000)?);
    let mut idx = 2u8;
    let n = run_sscanf(ctx, &input, &fmt, move |c| {
        let a = c.arg_u32(idx)?;
        idx += 1;
        Ok(a)
    })?;
    Ok(DispatchOutcome::ReturnedR0(n as u32))
}

/// Store an integer conversion result through a guest pointer,
/// honouring the `h` / `ll` length modifiers that decide how wide the
/// destination actually is.
fn scanf_store_int(
    ctx: &mut CallCtx<'_>,
    addr: u32,
    value: i64,
    short_form: bool,
    long_long: bool,
) -> Result<(), KernelError> {
    if addr == 0 {
        return Ok(());
    }
    if long_long {
        ctx.cpu.write_mem(addr, &(value as u64).to_le_bytes())?;
    } else if short_form {
        ctx.cpu.write_mem(addr, &(value as u16).to_le_bytes())?;
    } else {
        ctx.cpu.write_mem(addr, &(value as u32).to_le_bytes())?;
    }
    Ok(())
}

/// Parse the body of a `%[...]` scanset, starting just past the `[`.
/// Returns `(negated, members, index_of_closing_bracket + 1)`.
///
/// A `]` immediately after `[` or `[^` is a literal member, not the
/// terminator — that is what lets `%[^]]` mean "everything but a
/// bracket".
fn scanf_scanset(fmt: &[u8], mut fi: usize) -> (bool, Vec<u8>, usize) {
    let mut negated = false;
    if fmt.get(fi) == Some(&b'^') {
        negated = true;
        fi += 1;
    }
    let mut members = Vec::new();
    if fmt.get(fi) == Some(&b']') {
        members.push(b']');
        fi += 1;
    }
    while fi < fmt.len() && fmt[fi] != b']' {
        // `a-z` ranges. A trailing `-` is a literal.
        if fmt[fi] == b'-' && fi + 1 < fmt.len() && fmt[fi + 1] != b']' && !members.is_empty() {
            let lo = *members.last().unwrap();
            let hi = fmt[fi + 1];
            for c in lo..=hi {
                members.push(c);
            }
            fi += 2;
            continue;
        }
        members.push(fmt[fi]);
        fi += 1;
    }
    // Step past the `]` if it is there; a malformed set runs to the end.
    (negated, members, (fi + 1).min(fmt.len()))
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

/// `S_OK`, and the `STRSAFE_E_INSUFFICIENT_BUFFER` the strsafe
/// functions answer with when the result had to be truncated.
const S_OK: u32 = 0;
const STRSAFE_E_INSUFFICIENT_BUFFER: u32 = 0x8007_007A;

/// Write `text` into `dst` as a NUL-terminated wide string, truncating
/// to `cap_chars` the way the strsafe family does: the terminator is
/// always written as long as there is room for a single character, and
/// truncation is reported rather than being silently successful.
///
/// This is the one behaviour that separates strsafe from `_snwprintf`,
/// which leaves the buffer unterminated on overflow.
fn write_wide_bounded(
    ctx: &mut CallCtx<'_>,
    dst: u32,
    cap_chars: u32,
    text: &str,
) -> Result<u32, KernelError> {
    if dst == 0 || cap_chars == 0 {
        return Ok(STRSAFE_E_INSUFFICIENT_BUFFER);
    }
    let room = cap_chars as usize - 1;
    let units: Vec<u16> = text.encode_utf16().collect();
    let truncated = units.len() > room;
    let mut wide: Vec<u16> = units.into_iter().take(room).collect();
    wide.push(0);
    ctx.cpu.write_mem(dst, &wide_to_bytes(&wide))?;
    Ok(if truncated {
        STRSAFE_E_INSUFFICIENT_BUFFER
    } else {
        S_OK
    })
}

fn copy_wide_bounded(
    ctx: &mut CallCtx<'_>,
    dst: u32,
    cb_dest: u32,
    src: u32,
    max_chars: usize,
) -> Result<u32, KernelError> {
    let chars = read_wstr(ctx, src, max_chars as u32)?;
    let text = String::from_utf16_lossy(&chars);
    write_wide_bounded(ctx, dst, cb_dest / 2, &text)
}

/// `HRESULT StringCbCatW(LPWSTR dst, size_t cbDest, LPCWSTR src)`.
fn string_cb_cat_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cb_dest = ctx.arg_u32(1)?;
    let src = ctx.arg_u32(2)?;
    let mut text = String::from_utf16_lossy(&read_wstr(ctx, dst, 0x10000)?);
    text.push_str(&String::from_utf16_lossy(&read_wstr(ctx, src, 0x10000)?));
    let hr = write_wide_bounded(ctx, dst, cb_dest / 2, &text)?;
    Ok(DispatchOutcome::ReturnedR0(hr))
}

/// `HRESULT StringCbCopyNW(LPWSTR dst, size_t cbDest, LPCWSTR src, size_t cbToCopy)`.
fn string_cb_copy_nw(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cb_dest = ctx.arg_u32(1)?;
    let src = ctx.arg_u32(2)?;
    let cb_to_copy = ctx.arg_u32(3)?;
    let hr = copy_wide_bounded(ctx, dst, cb_dest, src, (cb_to_copy / 2) as usize)?;
    Ok(DispatchOutcome::ReturnedR0(hr))
}

/// `_wcslwr` lowercases a NUL-terminated wide string in place.
fn wcslwr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    char_lower_w(ctx)
}

/// `HRESULT StringCbCopyW(LPWSTR dst, size_t cbDest, LPCWSTR src)`.
fn string_cb_copy_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cb = ctx.arg_u32(1)?;
    let src = ctx.arg_u32(2)?;
    let text = String::from_utf16_lossy(&read_wstr(ctx, src, 0x10000)?);
    let hr = write_wide_bounded(ctx, dst, cb / 2, &text)?;
    Ok(DispatchOutcome::ReturnedR0(hr))
}

/// `HRESULT StringCbLengthW(LPCWSTR src, size_t cbMax, size_t *pcb)`.
fn string_cb_length_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let src = ctx.arg_u32(0)?;
    let cb_max = ctx.arg_u32(1)?;
    let out_bytes = ctx.arg_u32(2)?;
    let max_chars = (cb_max / 2) as usize;
    let chars = read_wstr(ctx, src, max_chars as u32)?;
    if out_bytes != 0 {
        ctx.cpu
            .write_mem(out_bytes, &((chars.len() * 2) as u32).to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if chars.len() < max_chars {
        S_OK
    } else {
        STRSAFE_E_INSUFFICIENT_BUFFER
    }))
}
/// `HRESULT StringCchPrintfW(LPWSTR dst, size_t cchDest, LPCWSTR fmt, ...)`
/// — capacity counted in *characters*.
fn string_cch_printf_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cch = ctx.arg_u32(1)?;
    let fmt_p = ctx.arg_u32(2)?;
    let fmt = String::from_utf16_lossy(&read_wstr(ctx, fmt_p, 0x4000)?);
    let text = render_printf(ctx, &fmt, true, 3)?;
    let hr = write_wide_bounded(ctx, dst, cch, &text)?;
    Ok(DispatchOutcome::ReturnedR0(hr))
}

/// `HRESULT StringCbPrintfW(LPWSTR dst, size_t cbDest, LPCWSTR fmt, ...)`
/// — capacity counted in *bytes*, hence the halving.
fn string_cb_printf_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cb = ctx.arg_u32(1)?;
    let fmt_p = ctx.arg_u32(2)?;
    let fmt = String::from_utf16_lossy(&read_wstr(ctx, fmt_p, 0x4000)?);
    let text = render_printf(ctx, &fmt, true, 3)?;
    let hr = write_wide_bounded(ctx, dst, cb / 2, &text)?;
    log::trace!("StringCbPrintfW({fmt:?}) -> {text:?}");
    Ok(DispatchOutcome::ReturnedR0(hr))
}

/// `HRESULT StringCchVPrintfW(LPWSTR, size_t cch, LPCWSTR fmt, va_list)`.
fn string_cch_vprintf_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cch = ctx.arg_u32(1)?;
    let fmt_p = ctx.arg_u32(2)?;
    let va_p = ctx.arg_u32(3)?;
    let fmt = String::from_utf16_lossy(&read_wstr(ctx, fmt_p, 0x4000)?);
    let text = render_printf_va(ctx, &fmt, true, va_p)?;
    let hr = write_wide_bounded(ctx, dst, cch, &text)?;
    Ok(DispatchOutcome::ReturnedR0(hr))
}

/// `HRESULT StringCbVPrintfW(LPWSTR, size_t cb, LPCWSTR fmt, va_list)`.
fn string_cb_vprintf_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let cb = ctx.arg_u32(1)?;
    let fmt_p = ctx.arg_u32(2)?;
    let va_p = ctx.arg_u32(3)?;
    let fmt = String::from_utf16_lossy(&read_wstr(ctx, fmt_p, 0x4000)?);
    let text = render_printf_va(ctx, &fmt, true, va_p)?;
    let hr = write_wide_bounded(ctx, dst, cb / 2, &text)?;
    Ok(DispatchOutcome::ReturnedR0(hr))
}

/// `int NKDbgPrintfW(LPCWSTR fmt, ...)` — debug output to the kernel
/// serial port. We render it into the log at trace level and report the
/// character count, which is what the real one returns.
fn nk_dbg_printf_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let fmt_p = ctx.arg_u32(0)?;
    let fmt = String::from_utf16_lossy(&read_wstr(ctx, fmt_p, 0x4000)?);
    let text = render_printf(ctx, &fmt, true, 1)?;
    log::trace!("NKDbgPrintfW: {}", text.trim_end());
    Ok(DispatchOutcome::ReturnedR0(text.chars().count() as u32))
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
        let (long, short) = scan_length_mods(&mut chars);
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
                piece = if short {
                    (v as i16).to_string()
                } else {
                    v.to_string()
                };
            }
            'u' => {
                let v = read_va(ctx, next_off)?;
                next_off += 4;
                piece = if short {
                    (v as u16).to_string()
                } else {
                    v.to_string()
                };
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
                let mask = if pulls_wide_arg(fmt_is_wide, long, short) {
                    0xffff
                } else {
                    0xff
                };
                if let Some(ch) = char::from_u32(v & mask) {
                    piece.push(ch);
                }
            }
            's' => {
                let p = read_va(ctx, next_off)?;
                next_off += 4;
                let pulls_wide = pulls_wide_arg(fmt_is_wide, long, short);
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

fn strupr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = read_cstr(ctx, p, 0x10000)?;
    let upper: Vec<u8> = bytes.into_iter().map(|c| c.to_ascii_uppercase()).collect();
    ctx.cpu.write_mem(p, &upper)?;
    Ok(DispatchOutcome::ReturnedR0(p))
}

fn to_lower_w(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

fn char_upper_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let chars = read_wstr(ctx, p, 0x10000)?;
    let text = String::from_utf16_lossy(&chars);
    let upper: String = text.to_uppercase();
    ctx.cpu.write_mem(
        p,
        &upper
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )?;
    Ok(DispatchOutcome::ReturnedR0(p))
}

fn char_lower_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let chars = read_wstr(ctx, p, 0x10000)?;
    let text = String::from_utf16_lossy(&chars);
    let lower: String = text.to_lowercase();
    ctx.cpu.write_mem(
        p,
        &lower
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )?;
    Ok(DispatchOutcome::ReturnedR0(p))
}

fn char_upper_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = read_cstr(ctx, p, 0x10000)?;
    let upper: Vec<u8> = String::from_utf8_lossy(&bytes).to_uppercase().into_bytes();
    ctx.cpu.write_mem(p, &upper)?;
    Ok(DispatchOutcome::ReturnedR0(p))
}

fn char_lower_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = read_cstr(ctx, p, 0x10000)?;
    let lower: Vec<u8> = String::from_utf8_lossy(&bytes).to_lowercase().into_bytes();
    ctx.cpu.write_mem(p, &lower)?;
    Ok(DispatchOutcome::ReturnedR0(p))
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
    // Writes to the `MAS1:` MP3 decoder are frames for the hardware, not
    // bytes for a file, and it is not in the file handle table.
    if ctx.kernel.vfs.is_mp3_decoder(handle) {
        let total = ctx.kernel.vfs.feed_mp3_decoder(handle, u64::from(count));
        log::trace!(
            "WriteFile(MAS1: h=0x{handle:08x}, {count} bytes) -> TRUE (total {})",
            total.unwrap_or(0)
        );
        if out_written_p != 0 {
            ctx.cpu.write_mem(out_written_p, &count.to_le_bytes())?;
        }
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
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

fn flush_file_buffers(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let ok = ctx.kernel.vfs.flush(handle).is_ok();
    Ok(DispatchOutcome::ReturnedR0(u32::from(ok)))
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
/// `RemoveDirectoryW(lpPathName) -> BOOL`.
///
/// Both Asphalt 2 builds call this at start-up to tidy up a scratch
/// directory. We report success without touching the host filesystem,
/// which mirrors `CreateDirectoryW` — that is also a no-op, so a
/// directory the guest believes it created never existed and removing
/// it must succeed. Refusing to delete through a mount is deliberate:
/// mounts point at the extracted CAB (or whatever `--rom-dir` names),
/// and a guest should not be able to erase host content.
fn create_directory_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name_p = ctx.arg_u32(0)?;
    if name_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let name = String::from_utf16_lossy(&read_wstr(ctx, name_p, 260)?);
    let ok = ctx.kernel.vfs.create_dir(&name);
    log::debug!("CreateDirectoryW({name:?}) -> {ok}");
    Ok(DispatchOutcome::ReturnedR0(u32::from(ok)))
}

fn delete_file_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name_p = ctx.arg_u32(0)?;
    if name_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let name = String::from_utf16_lossy(&read_wstr(ctx, name_p, 260)?);
    let ok = ctx.kernel.vfs.delete_file(&name);
    log::debug!("DeleteFileW({name:?}) -> {ok}");
    Ok(DispatchOutcome::ReturnedR0(u32::from(ok)))
}

fn move_file_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let from_p = ctx.arg_u32(0)?;
    let to_p = ctx.arg_u32(1)?;
    if from_p == 0 || to_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let from = String::from_utf16_lossy(&read_wstr(ctx, from_p, 260)?);
    let to = String::from_utf16_lossy(&read_wstr(ctx, to_p, 260)?);
    let ok = ctx.kernel.vfs.move_file(&from, &to);
    log::debug!("MoveFileW({from:?}, {to:?}) -> {ok}");
    Ok(DispatchOutcome::ReturnedR0(u32::from(ok)))
}

fn remove_directory_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name_p = ctx.arg_u32(0)?;
    if name_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if let Ok(name_w) = read_wstr(ctx, name_p, 260) {
        log::debug!(
            "RemoveDirectoryW({:?}) -> 1 (no-op)",
            String::from_utf16_lossy(&name_w)
        );
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

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
    // ROM lookup succeeds. Writable paths must try the rooted form
    // first so the writable save overlay wins over broader read-only
    // mounts such as `\Application\`.
    let normalized = path.replace('/', "\\").trim_start_matches('\\').to_string();
    let normalized_lower = normalized.to_ascii_lowercase();
    let without_program_files = normalized
        .get(
            normalized_lower
                .strip_prefix("program files\\")
                .map(|prefix| prefix.len())
                .map(|_| "Program Files\\".len())
                .unwrap_or(0)..,
        )
        .unwrap_or(&normalized);
    let rooted = format!("\\{normalized}");
    let mut candidates = if create {
        vec![
            rooted.clone(),
            format!("\\{without_program_files}"),
            normalized.clone(),
        ]
    } else {
        vec![
            normalized.clone(),
            format!("\\{without_program_files}"),
            rooted.clone(),
        ]
    };
    candidates.extend([
        format!("\\Application\\{normalized}"),
        format!("\\Program Files\\{normalized}"),
        format!("\\Program Files\\OmniGSoft\\MiniKayak1.1\\{normalized}"),
        format!("\\Program Files\\OmniGSoft\\MiniKayak1.1\\resources\\{normalized}"),
        format!("\\Program Files\\Game\\{normalized}"),
        format!("\\Program Files\\Atomic Dreams\\{normalized}"),
    ]);
    // Last resort: look for the bare file name in every mount root.
    //
    // A game usually hard-codes the install directory a real setup.exe
    // would have created -- Asphalt 2 3D opens
    // `\Program Files\Asphalt 2 3D\light.bar` -- while a host that just
    // mounts the extracted cabinet has the file one level up. The CLI
    // works around this by also mounting the `_setup.xml` install dir,
    // but the launcher (and `--rom-dir`) do not, which is why a title
    // could run from `pockethle run game.cab` and fail from the library.
    // Matching on the file name keeps both paths working without
    // hard-coding another per-title prefix.
    if let Some(basename) = normalized.rsplit('\\').next() {
        if basename != normalized && !basename.is_empty() {
            for (prefix, _) in ctx.kernel.vfs.mounts_snapshot() {
                candidates.push(format!("{prefix}{basename}"));
            }
        }
    }
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
    // A game that silently gets a NULL `FILE*` usually goes on to spin
    // or bail somewhere far away from here, so say which path missed.
    if h == 0 {
        log::debug!("fopen({name:?}, {mode:?}) -> NULL");
    } else {
        log::trace!("fopen({name:?}, {mode:?}) -> 0x{h:08x}");
    }
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
    // Same reasoning as `crt_fopen`: a NULL `FILE*` surfaces as a hang
    // or a bail-out far from the open, so name the path that missed.
    if h == 0 {
        log::debug!("_wfopen({name:?}, {mode:?}) -> NULL");
    } else {
        log::trace!("_wfopen({name:?}, {mode:?}) -> 0x{h:08x}");
    }
    Ok(DispatchOutcome::ReturnedR0(h))
}

fn crt_fclose(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let result = if ctx.kernel.vfs.flush(h).is_ok() && ctx.kernel.vfs.close(h) {
        0
    } else {
        u32::MAX
    };
    Ok(DispatchOutcome::ReturnedR0(result))
}

/// `_fcloseall` — flush and close every open stream, returning the count.
///
/// CeGCC's `crt3.c` calls this during teardown, right before
/// `ExitProcess`. MSVC-built games reach `ExitProcess` without it, which
/// is why it only showed up once a mingw32ce binary was loaded.
fn crt_fcloseall(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let closed = ctx.kernel.vfs.close_all() as u32;
    log::debug!("_fcloseall() -> {closed}");
    Ok(DispatchOutcome::ReturnedR0(closed))
}

/// `_fpreset` — reset the floating-point unit to its default state.
///
/// On the device this clears the pending FP exception flags and restores
/// the default rounding mode and precision. We never raise guest FP
/// exceptions and never change the rounding mode away from the default,
/// so the state it would reset to is the state we are always in and there
/// is nothing to do. `void` return, so r0 does not matter.
///
/// CeGCC's startup calls this unconditionally before `main`, so leaving it
/// unimplemented put a warning on the very first dispatched call of every
/// mingw32ce binary.
fn crt_fpreset(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn crt_fflush(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(
        if ctx.kernel.vfs.flush(h).is_ok() {
            0
        } else {
            u32::MAX
        },
    ))
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
    log::trace!("fread(0x{h:08x}, {size}x{count}) -> {elements} ({n} bytes)");
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
    log::trace!("fseek(0x{h:08x}, {off}, {whence}) -> {r:?}");
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

/// `int fgetpos(FILE *stream, fpos_t *pos)`
///
/// eVC4's `fpos_t` is a 32-bit `long`, but desktop MSVC widened it to
/// `__int64`; games compiled against either header read only as many
/// bytes as their own declaration says. We write the offset as a
/// zero-extended 64-bit little-endian value, which satisfies both, and
/// return 0 for success.
///
/// Sky Force Reloaded sizes `data.pak` with
/// `fseek(f, 0, SEEK_END); fgetpos(f, &size); fclose(f)`. While
/// `fgetpos` was unimplemented the reported size was whatever happened
/// to be on the stack, so the game's archive loader walked off the end
/// of its own tables and faulted before the first frame.
fn crt_fgetpos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use pocket_kernel::vfs::SeekKind;
    let h = ctx.arg_u32(0)?;
    let out = ctx.arg_u32(1)?;
    let Some(pos) = ctx.kernel.vfs.seek(h, 0, SeekKind::Current) else {
        return Ok(DispatchOutcome::ReturnedR0(u32::MAX));
    };
    if out != 0 {
        ctx.cpu.write_mem(out, &pos.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `int fsetpos(FILE *stream, const fpos_t *pos)` — the inverse of
/// [`crt_fgetpos`]; only the low 32 bits are meaningful for the file
/// sizes a Pocket PC game deals with.
fn crt_fsetpos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use pocket_kernel::vfs::SeekKind;
    let h = ctx.arg_u32(0)?;
    let src = ctx.arg_u32(1)?;
    if src == 0 {
        return Ok(DispatchOutcome::ReturnedR0(u32::MAX));
    }
    let mut buf = [0u8; 4];
    ctx.cpu.read_mem_into(src, &mut buf)?;
    let pos = u32::from_le_bytes(buf);
    match ctx.kernel.vfs.seek(h, pos as i64, SeekKind::Begin) {
        Some(_) => Ok(DispatchOutcome::ReturnedR0(0)),
        None => Ok(DispatchOutcome::ReturnedR0(u32::MAX)),
    }
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

fn crt_fgetws(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
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
    if out.last() == Some(&b'\n') {
        out.pop();
    }
    let mut wide = Vec::with_capacity(out.len() + 1);
    for byte in out {
        wide.extend_from_slice(&(byte as u16).to_le_bytes());
    }
    wide.extend_from_slice(&0u16.to_le_bytes());
    ctx.cpu.write_mem(buf, &wide)?;
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

fn rt_urem64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = (ctx.arg_u32(1)? as u64) << 32 | ctx.arg_u32(0)? as u64;
    let n = (ctx.arg_u32(3)? as u64) << 32 | ctx.arg_u32(2)? as u64;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let r = n % d;
    Ok(DispatchOutcome::ReturnedR0R1(r as u32, (r >> 32) as u32))
}

fn rt_srem64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = ((ctx.arg_u32(1)? as u64) << 32 | ctx.arg_u32(0)? as u64) as i64;
    let n = ((ctx.arg_u32(3)? as u64) << 32 | ctx.arg_u32(2)? as u64) as i64;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let r = n.wrapping_rem(d);
    Ok(DispatchOutcome::ReturnedR0R1(r as u32, (r >> 32) as u32))
}

/// `__rt_udiv64by64(uint64 dividend /*r0:r1*/, uint64 divisor /*r2:r3*/)`
/// — quotient in `r0:r1`.
///
/// Note the operand order: the 32-bit `__rt_udiv` helper is
/// divisor-first, but the explicit 64-by-64 helpers are not. Both
/// orders are indistinguishable for small values, which is why the
/// bug survived until a game divided a `FILETIME` by 10000.
fn rt_udiv64by64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = (ctx.arg_u32(1)? as u64) << 32 | ctx.arg_u32(0)? as u64;
    let d = (ctx.arg_u32(3)? as u64) << 32 | ctx.arg_u32(2)? as u64;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let q = n / d;
    Ok(DispatchOutcome::ReturnedR0R1(q as u32, (q >> 32) as u32))
}

/// `__rt_sdiv64by64(int64 dividend, int64 divisor)` — quotient in `r0:r1`.
fn rt_sdiv64by64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ((ctx.arg_u32(1)? as u64) << 32 | ctx.arg_u32(0)? as u64) as i64;
    let d = ((ctx.arg_u32(3)? as u64) << 32 | ctx.arg_u32(2)? as u64) as i64;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let q = n.wrapping_div(d);
    Ok(DispatchOutcome::ReturnedR0R1(q as u32, (q >> 32) as u32))
}

/// `__rt_urem64by64(uint64 dividend, uint64 divisor)` — remainder in `r0:r1`.
fn rt_urem64by64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = (ctx.arg_u32(1)? as u64) << 32 | ctx.arg_u32(0)? as u64;
    let d = (ctx.arg_u32(3)? as u64) << 32 | ctx.arg_u32(2)? as u64;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let r = n % d;
    Ok(DispatchOutcome::ReturnedR0R1(r as u32, (r >> 32) as u32))
}

/// `__rt_srem64by64(int64 dividend, int64 divisor)` — remainder in `r0:r1`.
fn rt_srem64by64(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ((ctx.arg_u32(1)? as u64) << 32 | ctx.arg_u32(0)? as u64) as i64;
    let d = ((ctx.arg_u32(3)? as u64) << 32 | ctx.arg_u32(2)? as u64) as i64;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0R1(0, 0));
    }
    let r = n.wrapping_rem(d);
    Ok(DispatchOutcome::ReturnedR0R1(r as u32, (r >> 32) as u32))
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
    let _ = flags; // we always zero-fill, so LMEM_ZEROINIT is implied
    do_alloc(ctx, size)
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
    let _ = flags; // we always zero-fill, so HEAP_ZERO_MEMORY is implied
    do_alloc(ctx, size)
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
    let addr = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;

    // The reserve/commit split does not exist here: a reservation is
    // already backed by real heap memory. So a commit naming an address
    // inside a live reservation has nothing left to do and must hand
    // that same address back. Allocating a second block instead would
    // double the guest's footprint — a game that reserves 29 MB and
    // then commits it would consume 58 MB and exhaust the heap on its
    // next request.
    if addr != 0 {
        let base = ctx.kernel.heap.base();
        let end = base.saturating_add(ctx.kernel.heap.size());
        let fits = addr >= base && addr.checked_add(size).is_some_and(|a| a <= end);
        if fits {
            return Ok(DispatchOutcome::ReturnedR0(addr));
        }
    }
    do_alloc(ctx, size)
}

fn malloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let size = ctx.arg_u32(0)?;
    do_alloc(ctx, size)
}

fn operator_new(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let size = ctx.arg_u32(0)?;
    do_alloc(ctx, size)
}

fn calloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let nmemb = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;
    do_alloc(ctx, nmemb.saturating_mul(size))
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
fn do_alloc(ctx: &mut CallCtx<'_>, size: u32) -> Result<DispatchOutcome, KernelError> {
    let user_ptr = match ctx.kernel.heap.alloc(size) {
        Some(p) => p,
        None => {
            log::warn!("heap exhausted; alloc({size}) failed");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    // Every allocation is handed back zero-filled, including the ones
    // whose Win32/CRT contract does not promise it (`malloc`,
    // `operator new`, `LocalAlloc` without `LMEM_ZEROINIT`).
    //
    // On a real device the CRT heap grows by committing fresh pages from
    // the kernel, and those pages are zero. A lot of Pocket PC game code
    // quietly depends on that: it allocates a multi-hundred-KB state
    // struct, fills in the fields it cares about, and treats every
    // pointer slot it never touched as NULL. Asphalt 2 3D is one of
    // these - its track loader walks `objects[i]` and calls the
    // destructor plus `operator delete` on every non-NULL entry, so one
    // stale word becomes a wild-pointer free the instant a race starts
    // loading (READ_UNMAPPED at 0xffffffff). Zeroing here is cheaper and
    // more faithful than reproducing WinCE's heap reuse pattern.
    if size > 0 {
        let zeros = vec![0u8; size as usize];
        ctx.cpu.write_mem(user_ptr, &zeros)?;
    }
    if std::env::var("POCKETHLE_TRACE_ALLOC").is_ok() && size >= 0x1000 {
        let lr = ctx.cpu.read_reg(pocket_cpu::regs::ArmReg::Lr).unwrap_or(0);
        eprintln!("[trace-alloc] ptr=0x{user_ptr:08x} size=0x{size:08x} lr=0x{lr:08x}");
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
        return do_alloc(ctx, new_size);
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
        // hbrBackground sits at +28, between hCursor and lpszMenuName.
        // It is either a real HBRUSH or the `COLOR_xxx + 1` shorthand,
        // which is how apps ask for a system colour without creating an
        // object; the two are told apart by magnitude, since a genuine
        // brush handle is one of our `0xDEAD_xxxx` values.
        let hbr = ctx.cpu.read_u32_le(lpwc + 28).unwrap_or(0);
        ctx.kernel.window_background = class_background_color(ctx, hbr);
        if let Ok(buf) = ctx.cpu.read_mem(lpwc + 4, 4) {
            let proc_va = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if proc_va != 0 {
                ctx.kernel.wnd_proc = proc_va;
                let class_name_ptr = ctx.cpu.read_u32_le(lpwc + 36).unwrap_or(0);
                if class_name_ptr != 0 {
                    if let Ok(chars) = read_wstr(ctx, class_name_ptr, 128) {
                        let name = String::from_utf16_lossy(&chars)
                            .trim_end_matches('\0')
                            .to_string();
                        if !name.is_empty() {
                            ctx.kernel.window_class_procs.insert(name, proc_va);
                        }
                    }
                }
                log::info!(
                    "RegisterClassW captured WndProc=0x{:08x} from WNDCLASS at 0x{:08x} (hbrBackground=0x{:08x} -> {:?})",
                    proc_va,
                    lpwc,
                    hbr,
                    ctx.kernel.window_background,
                );
            }
        }
    }
    // ATOMs are 16-bit; return a non-zero one.
    Ok(DispatchOutcome::ReturnedR0(0xC001))
}

fn create_window_ex_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Must be the first thing we do: the thunk re-fires this whole handler
    // when the hijacked `WndProc` returns, and by then R0..R3 hold the
    // callback's result rather than the original arguments.
    if let Some(frame) = ctx.kernel.create_frame {
        // A `CreateWindowExW` issued from *inside* the WM_CREATE handler runs
        // on a deeper stack; only SP back at its saved value means the
        // `WndProc` we hijacked has actually returned.
        if ctx.cpu.read_reg(ArmReg::Sp)? >= frame.sp {
            ctx.kernel.create_frame = None;
            ctx.kernel.create_stage = CreateStage::Idle;
            let result = ctx.cpu.read_reg(ArmReg::R0)?;
            if result as i32 == -1 {
                log::warn!("WndProc returned -1 from WM_CREATE; the guest wanted creation to fail");
            }
            ctx.cpu.write_reg(ArmReg::Sp, frame.sp)?;
            ctx.cpu.write_reg(ArmReg::R1, frame.args[1])?;
            ctx.cpu.write_reg(ArmReg::R2, frame.args[2])?;
            ctx.cpu.write_reg(ArmReg::R3, frame.args[3])?;
            ctx.cpu.write_reg(ArmReg::Lr, frame.lr)?;
            return Ok(DispatchOutcome::ReturnedR0(FAKE_HWND));
        }
    }
    let class_arg = ctx.arg_u32(1)?;
    let class_name = if class_arg != 0 {
        String::from_utf16_lossy(&read_wstr(ctx, class_arg, 128).unwrap_or_default())
            .trim_end_matches('\0')
            .to_string()
    } else {
        String::new()
    };
    // A built-in control class: on a device the window procedure lives
    // inside coredll, not in the application. Hand back a handle of its
    // own and let `controls` own the pixels — crucially *without*
    // touching `window_procs`, `pending_create` or `pending_startup`,
    // because routing a child onto the parent's `WndProc` made every
    // child re-enter the parent's WM_CREATE, which creates the children,
    // and CERF BlankApp looped there forever.
    if let Some(class) = ControlClass::from_class_name(&class_name) {
        let window_name = ctx.arg_u32(2)?;
        let text = if window_name != 0 {
            String::from_utf16_lossy(&read_wstr(ctx, window_name, 256).unwrap_or_default())
                .trim_end_matches('\0')
                .to_string()
        } else {
            String::new()
        };
        let style = ctx.arg_u32(3)?;
        let x = ctx.arg_u32(4)? as i32;
        let y = ctx.arg_u32(5)? as i32;
        let cx = ctx.arg_u32(6)? as i32;
        let cy = ctx.arg_u32(7)? as i32;
        let parent = ctx.arg_u32(8)?;
        // For a child window `hMenu` is the control id — what
        // `GetDlgItem` looks up and what the parent matches in
        // `LOWORD(wParam)` of `WM_COMMAND`.
        let id = ctx.arg_u32(9)?;
        let parent = if parent != 0 { parent } else { FAKE_HWND };
        let hwnd = ctx
            .kernel
            .controls
            .create(parent, class, id, text.clone(), style, x, y, cx, cy);
        log::debug!(
            "CreateWindowExW(class={class_name:?}, id={id}, text={text:?}) -> child hwnd=0x{hwnd:08x}"
        );
        return Ok(DispatchOutcome::ReturnedR0(hwnd));
    }
    let wnd_proc = ctx
        .kernel
        .window_class_procs
        .get(&class_name)
        .copied()
        .unwrap_or(ctx.kernel.wnd_proc);
    if wnd_proc != 0 {
        ctx.kernel.wnd_proc = wnd_proc;
    }
    // GWL_USERDATA belongs to the application. Windows initializes it to
    // zero; allocating a non-zero synthetic value here makes games that
    // install their object during WM_CREATE treat a zeroed buffer as an
    // already-live vtable and dereference address 0x00000004.
    ctx.kernel.window_user_data = 0;
    ctx.kernel.window_userdata.remove(&FAKE_HWND);
    ctx.kernel.window_procs.insert(FAKE_HWND, wnd_proc);
    ctx.kernel
        .window_classes
        .insert(FAKE_HWND, class_name.clone());
    let create_struct = ctx.kernel.heap.alloc(0x40).unwrap_or(0);
    if create_struct != 0 {
        // CREATESTRUCTW: lpCreateParams, hInstance, hMenu, hwndParent, cy,
        // cx, y, x, style, lpszName, lpszClass, dwExStyle. Real WndProcs
        // dereference these (Bejeweled reads the class name string on
        // WM_CREATE), so fill every field from the actual CreateWindowExW
        // arguments instead of leaving them zeroed.
        let ex_style = ctx.arg_u32(0)?;
        let window_name = ctx.arg_u32(2)?;
        let style = ctx.arg_u32(3)?;
        let x = ctx.arg_u32(4)?;
        let y = ctx.arg_u32(5)?;
        let cx = ctx.arg_u32(6)?;
        let cy = ctx.arg_u32(7)?;
        let h_menu = ctx.arg_u32(9)?;
        let h_instance = ctx.arg_u32(10)?;
        let create_params = ctx.arg_u32(11)?;
        let h_instance = if h_instance != 0 {
            h_instance
        } else {
            FAKE_MODULE_HANDLE
        };
        let fields: [u32; 12] = [
            create_params,
            h_instance,
            h_menu,
            FAKE_HWND,
            cy,
            cx,
            y,
            x,
            style,
            window_name,
            class_arg,
            ex_style,
        ];
        let mut buf = [0u8; 0x40];
        for (index, value) in fields.iter().enumerate() {
            buf[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        ctx.cpu.write_mem(create_struct, &buf)?;
        let (screen_w, screen_h) = screen_dims(ctx);
        let size_lparam = (screen_w & 0xFFFF) | ((screen_h & 0xFFFF) << 16);
        ctx.kernel.pending_startup.clear();
        ctx.kernel
            .pending_startup
            .push_back((WM_SIZE, 0, size_lparam));
        ctx.kernel.pending_startup.push_back((WM_SHOWWINDOW, 1, 0));
        ctx.kernel.pending_startup.push_back((WM_ACTIVATE, 1, 0));
        ctx.kernel.pending_startup.push_back((WM_SETFOCUS, 0, 0));
    }
    log::debug!(
        "CreateWindowExW(class={class_name:?}) -> hwnd=0x{FAKE_HWND:08x}, wndproc=0x{wnd_proc:08x}"
    );
    if wnd_proc != 0 && create_struct != 0 && !ctx.kernel.synthetic_create_sent {
        // Real Windows dispatches WM_CREATE from inside CreateWindowExW, and
        // titles that run window-init code straight afterwards depend on it:
        // Solitaire caches its window context in a global on WM_CREATE and
        // asserts if its post-create init reads that global back unset. So
        // detour into the WndProc now and land back in this thunk when it
        // returns; `pending_create` stays unset so the pump can't repeat it.
        let args = [
            ctx.cpu.read_reg(ArmReg::R0)?,
            ctx.cpu.read_reg(ArmReg::R1)?,
            ctx.cpu.read_reg(ArmReg::R2)?,
            ctx.cpu.read_reg(ArmReg::R3)?,
        ];
        let frame = GuestCallFrame {
            args,
            lr: ctx.cpu.read_reg(ArmReg::Lr)?,
            sp: ctx.cpu.read_reg(ArmReg::Sp)?,
        };
        // WndProc's four arguments all travel in registers, so the guest
        // stack needs no adjustment here.
        ctx.cpu.write_reg(ArmReg::R0, FAKE_HWND)?;
        ctx.cpu.write_reg(ArmReg::R1, WM_CREATE)?;
        ctx.cpu.write_reg(ArmReg::R2, 0)?;
        ctx.cpu.write_reg(ArmReg::R3, create_struct)?;
        ctx.cpu.write_reg(ArmReg::Lr, ctx.thunk.thunk_va)?;
        ctx.kernel.create_frame = Some(frame);
        ctx.kernel.create_stage = CreateStage::Create;
        ctx.kernel.synthetic_create_sent = true;
        log::debug!("WM_CREATE -> WndProc(0x{wnd_proc:08x}) from CreateWindowExW");
        return Ok(DispatchOutcome::JumpTo(wnd_proc));
    }
    if create_struct != 0 {
        ctx.kernel.pending_create = Some((FAKE_HWND, create_struct));
    }
    Ok(DispatchOutcome::ReturnedR0(FAKE_HWND))
}

/// `BOOL ShowWindow(HWND hWnd, int nCmdShow)`.
///
/// Real Windows sizes the window here and dispatches `WM_SIZE`
/// synchronously before returning. Solitaire builds its whole board
/// layout in the `WM_SIZE` arm of its `WndProc`, then starts drawing
/// straight after `ShowWindow` / `UpdateWindow` — well before it first
/// reaches its message pump. Leaving `WM_SIZE` on the startup queue
/// therefore left every pile rectangle zeroed, so the game logged
/// `Invalid call to DrawBackground!` each frame and stacked every card
/// at (0,0). Delivering it here — rather than from `CreateWindowExW`,
/// which runs before the game has allocated its state struct — matches
/// the real ordering and gives the guest a laid-out board.
fn show_window(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Must come first: the thunk re-fires this whole handler when the
    // hijacked `WndProc` returns.
    if ctx.kernel.create_stage == CreateStage::Size {
        if let Some(frame) = ctx.kernel.create_frame {
            // A nested `ShowWindow` from inside the WM_SIZE handler runs on a
            // deeper stack; only SP back at its saved value is a real return.
            if ctx.cpu.read_reg(ArmReg::Sp)? >= frame.sp {
                ctx.kernel.create_frame = None;
                ctx.kernel.create_stage = CreateStage::Idle;
                ctx.cpu.write_reg(ArmReg::Sp, frame.sp)?;
                ctx.cpu.write_reg(ArmReg::R1, frame.args[1])?;
                ctx.cpu.write_reg(ArmReg::R2, frame.args[2])?;
                ctx.cpu.write_reg(ArmReg::R3, frame.args[3])?;
                ctx.cpu.write_reg(ArmReg::Lr, frame.lr)?;
                return Ok(DispatchOutcome::ReturnedR0(1));
            }
        }
    }
    let hwnd = ctx.arg_u32(0)?;
    // A control or panel: `ShowWindow` is how an app hides part of a
    // dialog it built from a template. Solitaire shows and hides its
    // Time and Score readouts this way.
    let cmd_show = ctx.arg_u32(1).unwrap_or(1);
    let show = cmd_show != SW_HIDE;
    if let Some(child) = ctx.kernel.controls.get_mut(hwnd) {
        let id = child.id;
        child.visible = show;
        log::debug!("ShowWindow(control id={id}, cmd={cmd_show}) -> visible={show}");
        repaint_controls(ctx);
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    if let Some(panel) = ctx.kernel.controls.panel_mut(hwnd) {
        panel.visible = show;
        if ctx.kernel.wnd_proc != 0 {
            ctx.kernel.pending_message = Some((FAKE_HWND, WM_PAINT, 0, 0));
        }
        repaint_controls(ctx);
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let wnd_proc = ctx.kernel.window_procs.get(&hwnd).copied().unwrap_or(0);
    // Only the first show of the main window needs the sizing pass, and
    // only once no other guest-callback detour is already in flight.
    if wnd_proc != 0
        && hwnd == FAKE_HWND
        && !ctx.kernel.synthetic_size_sent
        && ctx.kernel.create_frame.is_none()
    {
        let args = [
            ctx.cpu.read_reg(ArmReg::R0)?,
            ctx.cpu.read_reg(ArmReg::R1)?,
            ctx.cpu.read_reg(ArmReg::R2)?,
            ctx.cpu.read_reg(ArmReg::R3)?,
        ];
        let frame = GuestCallFrame {
            args,
            lr: ctx.cpu.read_reg(ArmReg::Lr)?,
            sp: ctx.cpu.read_reg(ArmReg::Sp)?,
        };
        let (screen_w, screen_h) = screen_dims(ctx);
        let size_lparam = (screen_w & 0xFFFF) | ((screen_h & 0xFFFF) << 16);
        // The queued copy would otherwise arrive a second time.
        ctx.kernel
            .pending_startup
            .retain(|&(msg, _, _)| msg != WM_SIZE);
        ctx.cpu.write_reg(ArmReg::R0, hwnd)?;
        ctx.cpu.write_reg(ArmReg::R1, WM_SIZE)?;
        ctx.cpu.write_reg(ArmReg::R2, 0)?;
        ctx.cpu.write_reg(ArmReg::R3, size_lparam)?;
        ctx.cpu.write_reg(ArmReg::Lr, ctx.thunk.thunk_va)?;
        ctx.kernel.create_frame = Some(frame);
        ctx.kernel.create_stage = CreateStage::Size;
        ctx.kernel.synthetic_size_sent = true;
        log::debug!("WM_SIZE({screen_w}x{screen_h}) -> WndProc(0x{wnd_proc:08x}) from ShowWindow");
        return Ok(DispatchOutcome::JumpTo(wnd_proc));
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn update_window(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.wnd_proc != 0 {
        ctx.kernel.pending_message = Some((FAKE_HWND, WM_PAINT, 0, 0));
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn call_window_proc_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let proc = ctx.arg_u32(0)?;
    let hwnd = ctx.arg_u32(1)?;
    let message = ctx.arg_u32(2)?;
    let wparam = ctx.arg_u32(3)?;
    let lparam = ctx.arg_u32(4)?;
    if proc == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    use pocket_cpu::regs::ArmReg;
    ctx.cpu.write_reg(ArmReg::R0, hwnd)?;
    ctx.cpu.write_reg(ArmReg::R1, message)?;
    ctx.cpu.write_reg(ArmReg::R2, wparam)?;
    ctx.cpu.write_reg(ArmReg::R3, lparam)?;
    Ok(DispatchOutcome::JumpTo(proc))
}

/// `BOOL PostMessageW(HWND hWnd, UINT Msg, WPARAM wParam, LPARAM lParam)`
///
/// Unlike `SendMessageW` this must not run the window procedure
/// inline: the message goes on the queue and comes back out of
/// `GetMessageW` / `PeekMessageW` later. The wave-out driver uses the
/// same queue to report finished buffers, so a real implementation
/// here is what lets `MM_WOM_DONE` reach a game's message loop.
fn post_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let message = ctx.arg_u32(1)?;
    let wparam = ctx.arg_u32(2)?;
    let lparam = ctx.arg_u32(3)?;
    // Bound the queue: a guest that posts faster than it pumps would
    // otherwise grow it without limit.
    if ctx.kernel.posted_messages.len() < 256 {
        ctx.kernel
            .posted_messages
            .push_back((hwnd, message, wparam, lparam));
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn post_thread_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let thread_id = ctx.arg_u32(0)?;
    let message = ctx.arg_u32(1)?;
    let wparam = ctx.arg_u32(2)?;
    let lparam = ctx.arg_u32(3)?;
    // A worker's queue is its own. Only messages aimed at the main
    // thread (or at an id we never handed out — a game that posts to
    // thread 0) fall back to the window queue, which is what every
    // title relied on before per-thread queues existed.
    let worker = ctx
        .kernel
        .threads
        .iter_mut()
        .find(|thread| thread.id == thread_id && thread_id != 0 && !thread.finished);
    match worker {
        Some(thread) => {
            // Same bound as `PostMessageW`: a guest that posts faster
            // than it pumps must not grow the queue without limit.
            if thread.messages.len() < 256 {
                thread.messages.push_back((message, wparam, lparam));
            }
            log::debug!("PostThreadMessageW(thread={thread_id}, msg=0x{message:04x}, wp=0x{wparam:08x}, lp=0x{lparam:08x}) -> worker queue");
        }
        None => {
            if ctx.kernel.posted_messages.len() < 256 {
                ctx.kernel
                    .posted_messages
                    .push_back((0, message, wparam, lparam));
            }
            log::debug!("PostThreadMessageW(thread=0x{thread_id:08x}, msg=0x{message:04x}, wp=0x{wparam:08x}, lp=0x{lparam:08x}) -> window queue");
        }
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

const WAIT_TIMEOUT_RESULT: u32 = 0x102;

fn read_msg_queue_options(
    ctx: &mut CallCtx<'_>,
    ptr: u32,
) -> Result<(u32, u32, bool), KernelError> {
    if ptr == 0 {
        return Ok((0, 0, true));
    }
    let raw = ctx.cpu.read_mem(ptr, 20)?;
    let max_messages = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    let max_message_size = u32::from_le_bytes(raw[12..16].try_into().unwrap());
    let read_access = u32::from_le_bytes(raw[16..20].try_into().unwrap()) != 0;
    Ok((max_messages, max_message_size, read_access))
}

fn create_msg_queue(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name = ctx.arg_u32(0)?;
    let options = ctx.arg_u32(1)?;
    let (max_messages, max_message_size, read_access) = read_msg_queue_options(ctx, options)?;
    let handle = ctx.kernel.next_msg_queue_handle;
    ctx.kernel.next_msg_queue_handle = ctx.kernel.next_msg_queue_handle.wrapping_add(1);
    ctx.kernel.msg_queues.insert(
        handle,
        pocket_kernel::MsgQueue {
            max_messages: max_messages.max(1),
            max_message_size: max_message_size.max(1),
            read_access,
            messages: std::collections::VecDeque::new(),
        },
    );
    log::debug!("CreateMsgQueue(name=0x{name:08x}, options=0x{options:08x}, read_access={read_access}, max_messages={max_messages}, max_message_size={max_message_size}) -> 0x{handle:08x}");
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn read_msg_queue(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buffer = ctx.arg_u32(1)?;
    let buffer_size = ctx.arg_u32(2)?;
    let bytes_read = ctx.arg_u32(3)?;
    let timeout = ctx.arg_u32(4)?;
    let flags = ctx.arg_u32(5)?;
    let Some(queue) = ctx.kernel.msg_queues.get_mut(&handle) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let Some(message) = queue.messages.pop_front() else {
        if bytes_read != 0 {
            ctx.cpu.write_mem(bytes_read, &0u32.to_le_bytes())?;
        }
        if flags != 0 {
            ctx.cpu.write_mem(flags, &0u32.to_le_bytes())?;
        }
        if timeout != 0 && timeout != 0xFFFF_FFFF {
            return Ok(DispatchOutcome::ReturnedR0(WAIT_TIMEOUT_RESULT));
        }
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let n = message.len().min(buffer_size as usize);
    if buffer != 0 && n != 0 {
        ctx.cpu.write_mem(buffer, &message[..n])?;
    }
    if bytes_read != 0 {
        ctx.cpu.write_mem(bytes_read, &(n as u32).to_le_bytes())?;
    }
    if flags != 0 {
        ctx.cpu.write_mem(flags, &0u32.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if n == message.len() {
        1
    } else {
        0
    }))
}

fn write_msg_queue(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buffer = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)?;
    let _timeout = ctx.arg_u32(3)?;
    let _flags = ctx.arg_u32(4)?;
    let Some(queue) = ctx.kernel.msg_queues.get_mut(&handle) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    if !queue.read_access
        || buffer == 0
        || size > queue.max_message_size
        || queue.messages.len() as u32 >= queue.max_messages
    {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = ctx.cpu.read_mem(buffer, size)?;
    queue.messages.push_back(bytes);
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `HANDLE RequestPowerNotifications(HANDLE hMsgQ, DWORD dwFlags)`
///
/// The power manager would start pushing `POWER_BROADCAST` records into
/// the caller's message queue. Nothing in the emulator suspends or
/// changes power state, so the queue simply stays empty — but the call
/// has to succeed: Spore Origins treats a NULL return as a fatal
/// initialisation error.
fn request_power_notifications(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let queue = ctx.arg_u32(0)?;
    let flags = ctx.arg_u32(1)?;
    log::debug!(
        "RequestPowerNotifications(queue=0x{queue:08x}, flags=0x{flags:08x}) -> 0xdeade4f0"
    );
    Ok(DispatchOutcome::ReturnedR0(0xDEAD_E4F0))
}

/// `BOOL StopPowerNotifications(HANDLE h)`
fn stop_power_notifications(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `DWORD GetSystemPowerState(LPVOID pSystemPowerState, DWORD dwLen,
///                             LPDWORD pdwFlags, DWORD dwFlags)`.
///
/// Diamond Twister polls this before every iteration of its window loop.
/// The desktop HLE has no suspend/resume power manager, so report the
/// active state and clear the optional flag output instead of returning
/// zero without touching the caller's buffers.
fn get_system_power_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let state = ctx.arg_u32(0)?;
    let state_len = ctx.arg_u32(1)?;
    let flags_out = ctx.arg_u32(2)?;
    if state != 0 && state_len >= 4 {
        ctx.cpu.write_mem(state, &0u32.to_le_bytes())?;
    }
    if flags_out != 0 {
        ctx.cpu.write_mem(flags_out, &0u32.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn get_msg_queue_info(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let info = ctx.arg_u32(1)?;
    let Some(queue) = ctx.kernel.msg_queues.get(&handle) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    if info != 0 {
        let mut raw = [0u8; 24];
        raw[0..4].copy_from_slice(&24u32.to_le_bytes());
        raw[4..8].copy_from_slice(&(queue.messages.len() as u32).to_le_bytes());
        raw[8..12].copy_from_slice(&queue.max_messages.to_le_bytes());
        raw[12..16].copy_from_slice(&queue.max_message_size.to_le_bytes());
        raw[16..20].copy_from_slice(&(queue.messages.len() as u32).to_le_bytes());
        ctx.cpu.write_mem(info, &raw)?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn close_msg_queue(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(
        if ctx.kernel.msg_queues.remove(&handle).is_some() {
            1
        } else {
            0
        },
    ))
}

fn open_msg_queue(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let process_id = ctx.arg_u32(0)?;
    let source_handle = ctx.arg_u32(1)?;
    let options = ctx.arg_u32(2)?;
    let source = match ctx.kernel.msg_queues.get(&source_handle) {
        Some(source) => (
            source.max_messages,
            source.max_message_size,
            source.messages.clone(),
        ),
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    let handle = ctx.kernel.next_msg_queue_handle;
    ctx.kernel.next_msg_queue_handle = ctx.kernel.next_msg_queue_handle.wrapping_add(1);
    let (_, _, read_access) = read_msg_queue_options(ctx, options)?;
    let queue = pocket_kernel::MsgQueue {
        max_messages: source.0,
        max_message_size: source.1,
        read_access,
        messages: source.2,
    };
    ctx.kernel.msg_queues.insert(handle, queue);
    log::debug!(
        "OpenMsgQueue(process=0x{process_id:08x}, source=0x{source_handle:08x}) -> 0x{handle:08x}"
    );
    Ok(DispatchOutcome::ReturnedR0(handle))
}

/// Status-bar control messages we honour. `WM_USER` is 0x400.
mod sb {
    pub const SETTEXTA: u32 = 0x0401;
    pub const GETTEXTA: u32 = 0x0402;
    pub const SETPARTS: u32 = 0x0404;
    pub const GETPARTS: u32 = 0x0406;
    pub const SETTEXTW: u32 = 0x040B;
    pub const GETTEXTW: u32 = 0x040D;
    pub const SIMPLE: u32 = 0x0409;
}

/// Handle a message addressed to the status bar control.
///
/// Returns `None` for messages we don't model, so the caller can fall
/// through to its default handling. The control is a *real* window on a
/// device, so none of these may reach the application's WndProc.
fn status_bar_message(
    ctx: &mut CallCtx<'_>,
    message: u32,
    wparam: u32,
    lparam: u32,
) -> Result<Option<u32>, KernelError> {
    match message {
        sb::SETPARTS => {
            // wParam = part count, lParam = array of right-hand edges.
            let count = (wparam as usize).min(256);
            let mut edges = Vec::with_capacity(count);
            for i in 0..count {
                let addr = lparam.wrapping_add((i * 4) as u32);
                edges.push(ctx.cpu.read_u32_le(addr).unwrap_or(0) as i32);
            }
            log::debug!("SB_SETPARTS({count}) edges={edges:?}");
            let bar = ctx.kernel.status_bar.get_or_insert_with(Default::default);
            bar.set_parts(edges);
            Ok(Some(1))
        }
        sb::SETTEXTW | sb::SETTEXTA => {
            // The low byte of wParam is the part index; the high byte
            // carries drawing style flags (SBT_OWNERDRAW etc.) we don't
            // model. lParam may be NULL to clear the part.
            let part = (wparam & 0xff) as usize;
            const MAX_PART_TEXT: u32 = 256;
            let text = if lparam == 0 {
                String::new()
            } else if message == sb::SETTEXTW {
                String::from_utf16_lossy(&read_wstr(ctx, lparam, MAX_PART_TEXT)?)
            } else {
                read_cstr_string(ctx, lparam, MAX_PART_TEXT)?
            };
            log::debug!("SB_SETTEXT(part={part}) {text:?}");
            let bar = ctx.kernel.status_bar.get_or_insert_with(Default::default);
            bar.set_part_text(part, text);
            // The control repaints itself; the app never invalidates it.
            repaint_status_bar(ctx);
            Ok(Some(1))
        }
        sb::GETPARTS => {
            let bar = match ctx.kernel.status_bar.as_ref() {
                Some(b) => b,
                None => return Ok(Some(0)),
            };
            let have = bar.part_edges.len();
            let want = (wparam as usize).min(have);
            let edges: Vec<i32> = bar.part_edges[..want].to_vec();
            if lparam != 0 {
                for (i, edge) in edges.iter().enumerate() {
                    let addr = lparam.wrapping_add((i * 4) as u32);
                    ctx.cpu.write_mem(addr, &edge.to_le_bytes())?;
                }
            }
            Ok(Some(have as u32))
        }
        sb::GETTEXTW | sb::GETTEXTA => {
            let part = (wparam & 0xff) as usize;
            let text = ctx
                .kernel
                .status_bar
                .as_ref()
                .and_then(|b| b.part_text.get(part))
                .cloned()
                .unwrap_or_default();
            if lparam != 0 {
                if message == sb::GETTEXTW {
                    let mut buf: Vec<u8> =
                        text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
                    buf.extend_from_slice(&[0, 0]);
                    ctx.cpu.write_mem(lparam, &buf)?;
                } else {
                    let mut buf: Vec<u8> = text.bytes().collect();
                    buf.push(0);
                    ctx.cpu.write_mem(lparam, &buf)?;
                }
            }
            // LOWORD = length; HIWORD = drawing style (0 = plain).
            Ok(Some(text.chars().count() as u32 & 0xffff))
        }
        // SB_SIMPLE toggles between the multi-part and single-part
        // views. We always render the parts we were given, so just
        // acknowledge it.
        sb::SIMPLE => Ok(Some(1)),
        _ => Ok(None),
    }
}

/// Repaint the status bar strip straight into the framebuffer.
///
/// On a device the control owns those pixels and invalidates itself
/// when its text changes; the application never repaints it, so waiting
/// for the guest's next `WM_PAINT` would leave the bar blank (and the
/// guest's own paint would overdraw it anyway).
fn repaint_status_bar(ctx: &mut CallCtx<'_>) {
    let Some(bar) = ctx.kernel.status_bar.clone() else {
        return;
    };
    let mut surf = Surface::Screen(&mut ctx.kernel.framebuffer);
    bar.render(&mut surf);
    ctx.kernel.framebuffer.mark_dirty();
}

/// Repaint every built-in control over whatever the guest just drew.
///
/// Same reasoning as [`repaint_status_bar`]: the controls are sibling
/// windows that paint *after* the parent has filled its client area, so
/// an application that fills the whole client rect on `WM_PAINT` — which
/// is exactly what CERF BlankApp does — would otherwise erase them.
fn repaint_controls(ctx: &mut CallCtx<'_>) {
    if ctx.kernel.controls.is_empty() {
        return;
    }
    let controls = ctx.kernel.controls.clone();
    let mut surf = Surface::Screen(&mut ctx.kernel.framebuffer);
    controls.render(&mut surf);
    ctx.kernel.framebuffer.mark_dirty();
}

/// The window messages a built-in control answers itself.
///
/// Only the handful real Pocket PC code sends: the text pair an app uses
/// to read an `EDIT` back or relabel a `STATIC`, the check-state pair,
/// and `WM_SETFONT`, which we accept and ignore because our renderer has
/// exactly one font. Anything else returns `0`, the same as a control
/// that does not handle a message.
fn control_message(
    ctx: &mut CallCtx<'_>,
    hwnd: u32,
    message: u32,
    wparam: u32,
    lparam: u32,
) -> Result<u32, KernelError> {
    const WM_SETTEXT: u32 = 0x000C;
    const WM_GETTEXT: u32 = 0x000D;
    const WM_GETTEXTLENGTH: u32 = 0x000E;
    const WM_SETFONT: u32 = 0x0030;
    const BM_GETCHECK: u32 = 0x00F0;
    const BM_SETCHECK: u32 = 0x00F1;
    const EM_SETSEL: u32 = 0x00B1;
    const EM_REPLACESEL: u32 = 0x00C2;

    match message {
        WM_SETTEXT | EM_REPLACESEL => {
            let text = if lparam != 0 {
                String::from_utf16_lossy(&read_wstr(ctx, lparam, 512).unwrap_or_default())
                    .trim_end_matches('\0')
                    .to_string()
            } else {
                String::new()
            };
            set_control_text(ctx, hwnd, &text);
            Ok(1)
        }
        WM_GETTEXT => {
            let text = ctx
                .kernel
                .controls
                .get(hwnd)
                .map(|c| c.text.clone())
                .unwrap_or_default();
            if lparam == 0 || wparam == 0 {
                return Ok(0);
            }
            let room = (wparam as usize).saturating_sub(1);
            let units: Vec<u16> = text.encode_utf16().take(room).collect();
            let mut bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
            bytes.extend_from_slice(&[0u8, 0u8]);
            let _ = ctx.cpu.write_mem(lparam, &bytes);
            Ok(units.len() as u32)
        }
        WM_GETTEXTLENGTH => Ok(ctx
            .kernel
            .controls
            .get(hwnd)
            .map(|c| c.text.encode_utf16().count() as u32)
            .unwrap_or(0)),
        BM_GETCHECK => Ok(ctx
            .kernel
            .controls
            .get(hwnd)
            .map(|c| u32::from(c.checked))
            .unwrap_or(0)),
        BM_SETCHECK => {
            if let Some(child) = ctx.kernel.controls.get_mut(hwnd) {
                let id = child.id;
                child.checked = wparam != 0;
                log::debug!("BM_SETCHECK(id={id}) -> checked={}", wparam != 0);
                repaint_controls(ctx);
            }
            Ok(0)
        }
        // Accepted and ignored: one font, and no selection model.
        WM_SETFONT | EM_SETSEL => Ok(1),
        _ => {
            log::debug!("control 0x{hwnd:08x} ignoring message 0x{message:04x}");
            Ok(0)
        }
    }
}

fn send_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let message = ctx.arg_u32(1)?;
    let wparam = ctx.arg_u32(2)?;
    let lparam = ctx.arg_u32(3)?;
    // The status bar is a control we implement ourselves — its messages
    // must never trampoline into the application's WndProc.
    if hwnd == FAKE_STATUSBAR_HWND {
        if let Some(r) = status_bar_message(ctx, message, wparam, lparam)? {
            return Ok(DispatchOutcome::ReturnedR0(r));
        }
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    // Likewise a built-in control: its window procedure lives in coredll,
    // so a message aimed at it must be answered here rather than handed
    // to the application.
    if Controls::is_child_hwnd(hwnd) {
        let r = control_message(ctx, hwnd, message, wparam, lparam)?;
        return Ok(DispatchOutcome::ReturnedR0(r));
    }
    let proc = ctx
        .kernel
        .window_procs
        .get(&hwnd)
        .copied()
        .unwrap_or(ctx.kernel.wnd_proc);
    if proc == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    use pocket_cpu::regs::ArmReg;
    ctx.cpu.write_reg(ArmReg::R0, hwnd)?;
    ctx.cpu.write_reg(ArmReg::R1, message)?;
    ctx.cpu.write_reg(ArmReg::R2, wparam)?;
    ctx.cpu.write_reg(ArmReg::R3, lparam)?;
    Ok(DispatchOutcome::JumpTo(proc))
}

fn dispatch_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // The WndProc returns through this thunk's address. On that second
    // entry the original MSG arguments have been replaced by the callback's
    // return registers, so restore the interrupted call instead of
    // dispatching the same message again.
    if let Some(frame) = ctx.kernel.message_frame.take() {
        use pocket_cpu::regs::ArmReg;
        for (index, value) in frame.args.iter().enumerate() {
            ctx.cpu.write_reg(
                match index {
                    0 => ArmReg::R0,
                    1 => ArmReg::R1,
                    2 => ArmReg::R2,
                    _ => ArmReg::R3,
                },
                *value,
            )?;
        }
        ctx.cpu.write_reg(ArmReg::Sp, frame.sp)?;
        ctx.cpu.write_reg(ArmReg::Lr, frame.lr)?;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    // DispatchMessageW(const MSG *lpMsg) — pass the message into the
    // captured WndProc and trampoline guest execution into it. The
    // WndProc's epilogue will return to our LR (the message-loop
    // call site), so the loop continues normally.
    let lp_msg = ctx.arg_u32(0)?;
    let hwnd = if lp_msg != 0 {
        ctx.cpu.read_u32_le(lp_msg).unwrap_or(FAKE_HWND)
    } else {
        FAKE_HWND
    };
    let wnd_proc = (ctx
        .kernel
        .window_procs
        .get(&hwnd)
        .copied()
        .unwrap_or(ctx.kernel.wnd_proc))
        & !1;
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
    if message == WM_QUIT {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    log::debug!(
        "DispatchMessageW trampoline -> WndProc(hwnd=0x{:x}, msg=0x{:x}, wp=0x{:x}, lp=0x{:x}) at 0x{:08x}",
        hwnd, message, wparam, lparam, wnd_proc
    );
    use pocket_cpu::regs::ArmReg;
    let frame = pocket_kernel::GuestCallFrame {
        args: [
            ctx.cpu.read_reg(ArmReg::R0)?,
            ctx.cpu.read_reg(ArmReg::R1)?,
            ctx.cpu.read_reg(ArmReg::R2)?,
            ctx.cpu.read_reg(ArmReg::R3)?,
        ],
        lr: ctx.cpu.read_reg(ArmReg::Lr)?,
        sp: ctx.cpu.read_reg(ArmReg::Sp)?,
    };
    ctx.kernel.message_frame = Some(frame);
    ctx.cpu.write_reg(ArmReg::R0, hwnd)?;
    ctx.cpu.write_reg(ArmReg::R1, message)?;
    ctx.cpu.write_reg(ArmReg::R2, wparam)?;
    ctx.cpu.write_reg(ArmReg::R3, lparam)?;
    ctx.cpu.write_reg(ArmReg::Lr, ctx.thunk.thunk_va)?;
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
    write_synthetic_msg_for_hwnd(cpu, lp_msg, FAKE_HWND, message, wparam, lparam)
}

fn write_synthetic_msg_for_hwnd(
    cpu: &mut dyn pocket_cpu::Cpu,
    lp_msg: u32,
    hwnd: u32,
    message: u32,
    wparam: u32,
    lparam: u32,
) -> Result<(), KernelError> {
    if lp_msg == 0 {
        return Ok(());
    }
    let mut msg = [0u8; 28];
    msg[0..4].copy_from_slice(&hwnd.to_le_bytes());
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
const WM_ERASEBKGND: u32 = 0x0014;
const WM_TIMER: u32 = 0x0113;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SIZE: u32 = 0x0005;

// Window styles read off a `DLGTEMPLATE`.
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_BORDER: u32 = 0x0080_0000;

/// `ShowWindow(SW_HIDE)`.
const SW_HIDE: u32 = 0;
const WM_ACTIVATE: u32 = 0x0006;
const WM_SETFOCUS: u32 = 0x0007;
const WM_SHOWWINDOW: u32 = 0x0018;
const WM_INITDIALOG: u32 = 0x0110;
const WM_DESTROY: u32 = 0x0002;
const WM_COMMAND: u32 = 0x0111;
const MK_LBUTTON: u32 = 0x0001;
/// `BN_CLICKED` — the notification code a button packs into the high
/// half of `WM_COMMAND`'s `wParam`. Zero, so `LOWORD(wParam)` is the
/// bare control id, which is what BlankApp's handler switches on.
const BN_CLICKED: u32 = 0;

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
            Some((WM_LBUTTONUP, MK_LBUTTON, lparam))
        }
        pocket_kernel::InputEvent::PointerMove { x, y } => {
            let lparam = ((y as u32) << 16) | (x as u32);
            Some((WM_MOUSEMOVE, MK_LBUTTON, lparam))
        }
        pocket_kernel::InputEvent::KeyDown { vk } => Some((WM_KEYDOWN, vk as u32, 1)),
        pocket_kernel::InputEvent::KeyUp { vk } => Some((WM_KEYUP, vk as u32, 0xC000_0001)),
    }
}

/// Give the built-in controls first refusal on a host input event.
///
/// On a device the tap never reaches the application at all: it goes to
/// the control's own window, and what the parent sees is the
/// `WM_COMMAND` the control chooses to send. So:
///
/// * `Some(Some((hwnd, msg, wp, lp)))` — a control turned the event
///   into a message for the parent (a button was clicked). `hwnd` is
///   the window the message is addressed to, which for a dialog's
///   control is the dialog, not the application's main window.
/// * `Some(None)` — a control swallowed the event (focus moved, a
///   character was typed); the application must not see it.
/// * `None` — no control was involved, so the event carries on to the
///   application unchanged. This is the path every full-screen game
///   takes, which is why a title with no controls is unaffected.
#[allow(clippy::option_option)]
fn controls_take_input(
    ctx: &mut CallCtx<'_>,
    ev: pocket_kernel::InputEvent,
) -> Option<Option<(u32, u32, u32, u32)>> {
    if ctx.kernel.controls.is_empty() {
        return None;
    }
    let action = match ev {
        pocket_kernel::InputEvent::PointerDown { x, y } => {
            ctx.kernel.controls.pointer_down(x as i32, y as i32)
        }
        pocket_kernel::InputEvent::PointerUp { x, y } => {
            ctx.kernel.controls.pointer_up(x as i32, y as i32)
        }
        pocket_kernel::InputEvent::KeyDown { vk } => ctx.kernel.controls.key_down(vk),
        // Moves and key releases are not control input; a release only
        // matters through the press that captured it.
        _ => None,
    }?;
    // The control's appearance changed (pressed, focused, new text), and
    // on a device it would have invalidated itself.
    repaint_controls(ctx);
    match action {
        ControlAction::Clicked { parent, id, hwnd } => {
            log::debug!("control id={id} clicked -> WM_COMMAND to 0x{parent:08x}");
            let wparam = (id & 0xFFFF) | (BN_CLICKED << 16);
            // Addressed to the control's *own* parent, not to the main
            // window. Solitaire's button strip is a dialog, and its
            // handlers live in the DialogProc registered for that panel;
            // stamping the MSG with the main window instead delivered
            // every `WM_COMMAND` to a proc that ignores these ids, which
            // is why the buttons visibly pressed but did nothing.
            Some(Some((parent, WM_COMMAND, wparam, hwnd)))
        }
        ControlAction::Consumed => Some(None),
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

fn keybd_event(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    let vk = ctx.arg_u32(0)? as u16;
    let flags = ctx.arg_u32(2)?;
    let event = if flags & KEYEVENTF_KEYUP != 0 {
        InputEvent::KeyUp { vk }
    } else {
        InputEvent::KeyDown { vk }
    };
    if ctx.kernel.pending_input.len() < 256 {
        ctx.kernel.pending_input.push_back(event);
    }
    log::debug!("keybd_event(vk=0x{vk:02x}, flags=0x{flags:08x}) -> {event:?}");
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `HWND GetFocus()` — the focused control if the user has tapped one,
/// otherwise the top-level window.
fn get_focus(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let focus = ctx.kernel.controls.focus;
    Ok(DispatchOutcome::ReturnedR0(if focus != 0 {
        focus
    } else {
        FAKE_HWND
    }))
}

/// `HWND SetFocus(HWND hWnd)` — returns the previously focused window.
///
/// This used to be a constant `1` thunk. A dialog that focuses its edit
/// field on `WM_INITDIALOG` depends on it landing somewhere real, or the
/// caret sits on nothing and typed keys go to the application instead.
fn set_focus(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let previous = ctx.kernel.controls.focus;
    if ctx
        .kernel
        .controls
        .get(hwnd)
        .is_some_and(|c| c.is_focusable())
    {
        ctx.kernel.controls.focus = hwnd;
        repaint_controls(ctx);
    } else if hwnd == 0 || is_live_hwnd(hwnd) {
        // Focus moved off the controls onto the top-level window.
        ctx.kernel.controls.focus = 0;
        if previous != 0 {
            repaint_controls(ctx);
        }
    }
    Ok(DispatchOutcome::ReturnedR0(if previous != 0 {
        previous
    } else {
        FAKE_HWND
    }))
}

/// `BOOL MoveWindow(HWND hWnd, int X, int Y, int nWidth, int nHeight, BOOL bRepaint)`.
///
/// Controls are routinely created `0x0` and positioned from the parent's
/// `WM_SIZE` handler, which is exactly what CERF BlankApp does — so
/// without this the buttons and the edit field would never acquire a
/// rectangle and nothing would be drawn or hit-tested.
fn move_window(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(1)? as i32;
    let y = ctx.arg_u32(2)? as i32;
    let w = ctx.arg_u32(3)? as i32;
    let h = ctx.arg_u32(4)? as i32;
    if let Some(child) = ctx.kernel.controls.get_mut(hwnd) {
        child.x = x;
        child.y = y;
        child.w = w;
        child.h = h;
        log::debug!("MoveWindow(0x{hwnd:08x}) -> ({x},{y},{w},{h})");
        repaint_controls(ctx);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `SetWindowPos(hWnd, hWndInsertAfter, X, Y, cx, cy, uFlags)`
///
/// The same repositioning as [`move_window`], reached by a different
/// route: the caller may ask for the move only, the resize only, or
/// neither, via `SWP_NOMOVE` / `SWP_NOSIZE`.
fn set_window_pos(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    /// `SWP_NOSIZE`.
    const SWP_NOSIZE: u32 = 0x0001;
    /// `SWP_NOMOVE`.
    const SWP_NOMOVE: u32 = 0x0002;

    let hwnd = ctx.arg_u32(0)?;
    let x = ctx.arg_u32(2)? as i32;
    let y = ctx.arg_u32(3)? as i32;
    let w = ctx.arg_u32(4)? as i32;
    let h = ctx.arg_u32(5)? as i32;
    let flags = ctx.arg_u32(6).unwrap_or(0);
    log::debug!("SetWindowPos(0x{hwnd:08x}, ({x},{y},{w},{h}), flags=0x{flags:04x})");
    let moved = flags & SWP_NOMOVE == 0;
    let sized = flags & SWP_NOSIZE == 0;
    if let Some(child) = ctx.kernel.controls.get_mut(hwnd) {
        if moved {
            child.x = x;
            child.y = y;
        }
        if sized {
            child.w = w;
            child.h = h;
        }
        repaint_controls(ctx);
    } else if let Some(panel) = ctx.kernel.controls.panel_mut(hwnd) {
        if moved {
            panel.x = x;
            panel.y = y;
        }
        if sized {
            panel.w = w;
            panel.h = h;
        }
        // Moving the panel moves its children with it and uncovers
        // whatever it used to sit over, so the frame window has to
        // repaint too — the controls alone would leave the old panel
        // face behind.
        if ctx.kernel.wnd_proc != 0 {
            ctx.kernel.pending_message = Some((FAKE_HWND, WM_PAINT, 0, 0));
        }
        repaint_controls(ctx);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
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
    if let Some(triple) = synthetic_message_if_due(ctx) {
        return triple;
    }
    // Nothing was due even after the wait (clock granularity) — fall
    // back to a paint so a blocking `GetMessageW` never stalls.
    ctx.kernel.synthetic_paint_next_ms = monotonic_ms().saturating_add(SYNTHETIC_PAINT_INTERVAL_MS);
    (WM_PAINT, 0, 0)
}

/// Interval between fabricated `WM_PAINT` messages, in milliseconds
/// (~60 Hz).
const SYNTHETIC_PAINT_INTERVAL_MS: u64 = 16;

/// How far a synthetic-pump deadline may fall behind the wall clock
/// before [`advance_deadline`] gives up on catching it up.
const SYNTHETIC_PUMP_MAX_LAG_MS: u64 = 100;

/// Reschedule a synthetic-pump deadline on a fixed cadence rather than
/// from "now".
///
/// Zuma registers a 1 ms timer and runs its game logic from `WM_TIMER`,
/// pumping ~20 of them between two rendered frames. Rescheduling from
/// `now` meant the pump could never hand out more than one message per
/// real millisecond, so those 20 messages cost 20 ms of `sleep` *on top
/// of* the 22 ms the frame's emulation took — half of a 46 ms frame
/// spent idling on a clock we own. Advancing from the previous deadline
/// instead lets the messages the guest missed while it was busy be
/// delivered back to back, which is what a device fast enough to keep
/// up does implicitly.
///
/// [`SYNTHETIC_PUMP_MAX_LAG_MS`] bounds the catch-up so a genuinely
/// long stall — a level load, a first-frame asset decode — cannot queue
/// seconds' worth of `WM_TIMER`, matching WinCE's habit of coalescing
/// timer messages rather than letting them pile up.
fn advance_deadline(previous: u64, interval: u64, now: u64) -> u64 {
    let interval = interval.max(1);
    let next = previous.saturating_add(interval);
    if now.saturating_sub(next) > SYNTHETIC_PUMP_MAX_LAG_MS {
        now.saturating_add(interval)
    } else {
        next
    }
}

/// Non-blocking variant of [`synthetic_message_for`]: hand back the
/// next fabricated message only when it is genuinely *due*, otherwise
/// `None` to mean "the queue is empty right now".
///
/// This distinction is what `PeekMessageW` needs and `GetMessageW`
/// does not. The canonical PPC2003 / Windows Mobile main loop renders
/// on the **idle** branch:
///
/// ```c
/// while (running) {
///     if (PeekMessage(&msg, NULL, 0, 0, PM_REMOVE)) {
///         TranslateMessage(&msg);
///         DispatchMessage(&msg);
///     } else {
///         RenderFrame();          /* GXBeginDraw ... GXEndDraw */
///     }
/// }
/// ```
///
/// A `PeekMessageW` that unconditionally claims a message is waiting
/// starves `RenderFrame()` forever, so the game pumps messages at full
/// speed and never draws a single pixel (Asphalt 2 3D behaved exactly
/// this way — 240 dispatched messages, `frame_counter=0`).
fn synthetic_message_if_due(ctx: &mut CallCtx<'_>) -> Option<(u32, u32, u32)> {
    let now = monotonic_ms();
    if ctx.kernel.synthetic_timer_id != 0 && now >= ctx.kernel.synthetic_timer_next_ms {
        let interval = ctx.kernel.synthetic_timer_interval_ms.max(1) as u64;
        ctx.kernel.synthetic_timer_next_ms =
            advance_deadline(ctx.kernel.synthetic_timer_next_ms, interval, now);
        return Some((WM_TIMER, ctx.kernel.synthetic_timer_id, 0));
    }
    if now >= ctx.kernel.synthetic_paint_next_ms {
        ctx.kernel.synthetic_paint_next_ms = advance_deadline(
            ctx.kernel.synthetic_paint_next_ms,
            SYNTHETIC_PAINT_INTERVAL_MS,
            now,
        );
        return Some((WM_PAINT, 0, 0));
    }
    None
}

/// Pop the next message to deliver. Real user input from the host
/// frontend drains [`KernelState::pending_input`] first so that taps
/// and D-pad presses always win over the synthetic pump; once the
/// queue is empty we fall back to fabricated traffic so games never
/// see an idle window.
fn next_message(ctx: &mut CallCtx<'_>) -> (u32, u32, u32, u32) {
    while let Some(ev) = take_pending_input(ctx) {
        update_key_state(ctx, ev);
        // The built-in controls get the event first, exactly as the OS
        // would hand it to the control's own window procedure.
        if let Some(handled) = controls_take_input(ctx, ev) {
            match handled {
                Some(addressed) => return addressed,
                // Swallowed by a control: loop round for the next event
                // rather than handing the application a tap it never
                // would have seen.
                None => continue,
            }
        }
        if let Some((msg, wp, lp)) = input_to_message(ev) {
            return (FAKE_HWND, msg, wp, lp);
        }
    }
    // Same ordering as `next_message_if_due`: posted driver / guest
    // messages before the synthetic paint pump.
    if let Some(posted) = take_posted_message(ctx) {
        return posted;
    }
    let (msg, wp, lp) = synthetic_message_for(ctx);
    (FAKE_HWND, msg, wp, lp)
}

/// Pop one queued host input event, rewriting its virtual key for GAPI
/// guests.
///
/// A game that fetched its key list from `GXGetDefaultKeys` compares
/// every `WM_KEYDOWN` against that table and drops anything else, so the
/// host's confirm key (`VK_RETURN`) has to arrive as `vkA`. Doing the
/// rewrite here — rather than in each frontend — keeps `--key enter`,
/// the desktop launcher and the Android on-screen pad in agreement, and
/// leaves non-GAPI titles that read `VK_RETURN` directly untouched.
fn take_pending_input(ctx: &mut CallCtx<'_>) -> Option<InputEvent> {
    let ev = ctx.kernel.pending_input.pop_front()?;
    let queried = ctx.kernel.gapi_keys_queried;
    let remapped = match ev {
        InputEvent::KeyDown { vk } => InputEvent::KeyDown {
            vk: pocket_kernel::gapi::remap_host_key(vk, queried),
        },
        InputEvent::KeyUp { vk } => InputEvent::KeyUp {
            vk: pocket_kernel::gapi::remap_host_key(vk, queried),
        },
        other => other,
    };
    if remapped != ev {
        log::debug!("remapped host input {ev:?} to {remapped:?} for GAPI guest");
    }
    Some(remapped)
}

/// Non-blocking counterpart of [`next_message`] for `PeekMessageW`:
/// real host input first, then a fabricated message only if one is
/// due, else `None` ("queue empty") so the guest can run its idle /
/// render path.
/// Pop the next posted message this thread is allowed to see.
///
/// `MM_WOM_DONE` needs routing. The usual Pocket PC streaming layout is
/// a dedicated mixer thread that loops on `PeekMessage` waiting for the
/// notification, while `waveOutOpen` itself is called from the main
/// thread during start-up. The main loop's `WndProc` ignores
/// `MM_WOM_DONE`, so if its pump takes the message the buffer is never
/// refilled and the music stops after the primed buffers. Give the
/// notification to the mixer thread whenever the game runs one, and to
/// the main thread otherwise.
fn take_posted_message(ctx: &mut CallCtx<'_>) -> Option<(u32, u32, u32, u32)> {
    let front = ctx.kernel.posted_messages.front().copied()?;
    if front.1 == MM_WOM_DONE {
        let has_mixer_thread = ctx
            .kernel
            .threads
            .iter()
            .any(|thread| thread.started && !thread.finished);
        if has_mixer_thread && ctx.kernel.current_thread == 0 {
            return None;
        }
    }
    ctx.kernel.posted_messages.pop_front()
}

fn next_message_if_due(ctx: &mut CallCtx<'_>) -> Option<(u32, u32, u32, u32)> {
    while let Some(ev) = take_pending_input(ctx) {
        update_key_state(ctx, ev);
        if let Some(handled) = controls_take_input(ctx, ev) {
            match handled {
                Some(addressed) => return Some(addressed),
                None => continue,
            }
        }
        if let Some((msg, wp, lp)) = input_to_message(ev) {
            return Some((FAKE_HWND, msg, wp, lp));
        }
    }
    // Driver notifications (`MM_WOM_DONE`) outrank the synthetic
    // paint/timer pump: a game streaming music refills its buffer from
    // this message and would otherwise run dry.
    if let Some(posted) = take_posted_message(ctx) {
        return Some(posted);
    }
    let (msg, wp, lp) = synthetic_message_if_due(ctx)?;
    Some((FAKE_HWND, msg, wp, lp))
}

/// How many more times a guest may ask for a message after it has been
/// told to quit. A pump that honours `WM_QUIT` breaks out at once and
/// only comes back through here while it tears down, so this is slack,
/// not a budget.
const QUIT_POLL_GRACE: u64 = 64;

/// Hand the guest the `WM_QUIT` that its spent message budget earned,
/// and stop the run once it is clear the quit is being ignored.
///
/// X-Forge (Ball Busters, Sticky Balls) only ever acts on a `WM_QUIT`
/// that arrived through `GetMessage`; the one its `PeekMessage` pump
/// gets is dispatched like any other message and dropped. Answering it
/// forever meant the game never reached its render branch again and
/// spun in the pump until `max_slices` ran out — Ball Busters froze on
/// the publisher logo at frame ~230 and burned the rest of the run at a
/// fortieth of its normal frame rate. Halting instead is what the
/// exhausted budget meant in the first place.
///
/// `found` is the "a message was written" return value, which differs
/// between the two pumps: `PeekMessage` reports TRUE, `GetMessage`
/// reports FALSE precisely because the message is `WM_QUIT`.
fn quit_or_halt(
    ctx: &mut CallCtx<'_>,
    lp_msg: u32,
    count: u64,
    budget: u64,
    found: u32,
) -> Result<DispatchOutcome, KernelError> {
    ctx.kernel.synthetic_message_count = count.saturating_add(1);
    if count >= budget.saturating_add(QUIT_POLL_GRACE) {
        log::info!(
            "message budget {budget} spent and WM_QUIT ignored for \
             {QUIT_POLL_GRACE} more polls; halting"
        );
        return Ok(DispatchOutcome::Halt);
    }
    write_synthetic_msg(ctx.cpu, lp_msg, WM_QUIT, 0, 0)?;
    Ok(DispatchOutcome::ReturnedR0(found))
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
fn get_message_pos(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn get_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // A real driver retires wave buffers on its own thread; the
    // message pump is our equivalent "time has passed" hook, so do it
    // before handing the guest its next message.
    service_wave_out(ctx)?;
    let thunk_va = ctx.thunk.thunk_va;
    if let Some(outcome) = wave_out_enter_callback(ctx, thunk_va)? {
        return Ok(outcome);
    }
    let lp_msg = ctx.arg_u32(0)?;
    let count = ctx.kernel.synthetic_message_count;
    let budget = ctx.kernel.synthetic_message_budget;
    if budget > 0 && count >= budget {
        return quit_or_halt(ctx, lp_msg, count, budget, 0);
    }
    // A worker running its own pump must never be handed the window
    // queue's synthetic paint/timer traffic: it would keep dispatching
    // forever and the main thread — the one that actually renders —
    // would never get the CPU back.
    if let Some(thread_index) = ctx.kernel.current_thread.checked_sub(1) {
        let queued = ctx
            .kernel
            .threads
            .get_mut(thread_index)
            .and_then(|thread| thread.messages.pop_front());
        if let Some((msg, wp, lp)) = queued {
            write_synthetic_msg_for_hwnd(ctx.cpu, lp_msg, 0, msg, wp, lp)?;
            return Ok(DispatchOutcome::ReturnedR0(1));
        }
        if let Some(outcome) = park_worker_and_retry(ctx)? {
            return Ok(outcome);
        }
    }
    if let Some((hwnd, create_lparam)) = ctx.kernel.pending_create.take() {
        write_synthetic_msg_for_hwnd(ctx.cpu, lp_msg, hwnd, WM_CREATE, 0, create_lparam)?;
        ctx.kernel.synthetic_message_count = count + 1;
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    if let Some((msg, wp, lp)) = ctx.kernel.pending_startup.pop_front() {
        write_synthetic_msg(ctx.cpu, lp_msg, msg, wp, lp)?;
        ctx.kernel.synthetic_message_count = count + 1;
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    if let Some((hwnd, msg, wp, lp)) = take_posted_message(ctx) {
        write_synthetic_msg_for_hwnd(ctx.cpu, lp_msg, hwnd, msg, wp, lp)?;
        ctx.kernel.synthetic_message_count = count + 1;
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let (hwnd, msg, wp, lp) = ctx
        .kernel
        .pending_message
        .take()
        .unwrap_or_else(|| next_message(ctx));
    write_synthetic_msg_for_hwnd(ctx.cpu, lp_msg, hwnd, msg, wp, lp)?;
    ctx.kernel.synthetic_message_count += 1;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn peek_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    service_wave_out(ctx)?;
    let thunk_va = ctx.thunk.thunk_va;
    if let Some(outcome) = wave_out_enter_callback(ctx, thunk_va)? {
        return Ok(outcome);
    }
    let lp_msg = ctx.arg_u32(0)?;
    let remove_mode = ctx.arg_u32(4)?;
    let count = ctx.kernel.synthetic_message_count;
    let budget = ctx.kernel.synthetic_message_budget;
    if budget > 0 && count >= budget {
        return quit_or_halt(ctx, lp_msg, count, budget, 1);
    }
    // Worker threads see only what was posted to them; an empty queue
    // is a yield back to the main thread, not a window message.
    if let Some(thread_index) = ctx.kernel.current_thread.checked_sub(1) {
        let queued = ctx.kernel.threads.get_mut(thread_index).and_then(|thread| {
            if remove_mode & 0x0001 != 0 {
                thread.messages.pop_front()
            } else {
                thread.messages.front().copied()
            }
        });
        if let Some((msg, wp, lp)) = queued {
            write_synthetic_msg_for_hwnd(ctx.cpu, lp_msg, 0, msg, wp, lp)?;
            return Ok(DispatchOutcome::ReturnedR0(1));
        }
        if let Some(outcome) = park_worker(ctx, 0)? {
            return Ok(outcome);
        }
    }
    if let Some((hwnd, create_lparam)) = ctx.kernel.pending_create.take() {
        write_synthetic_msg_for_hwnd(ctx.cpu, lp_msg, hwnd, WM_CREATE, 0, create_lparam)?;
        ctx.kernel.synthetic_message_count = count + 1;
        if remove_mode != 0x0001 {
            ctx.kernel.pending_message = Some((hwnd, WM_CREATE, 0, create_lparam));
        }
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let addressed = match ctx.kernel.pending_message.take().or_else(|| {
        ctx.kernel
            .pending_startup
            .pop_front()
            .map(|(msg, wp, lp)| (FAKE_HWND, msg, wp, lp))
    }) {
        Some(addressed) => addressed,
        // Nothing pending and nothing due: report an empty queue so
        // the guest falls through to its idle / render branch. An
        // empty queue is also the natural scheduling point — the main
        // loop is idle, so let a parked worker (typically the mixer
        // thread) run, and conversely park a worker that has drained
        // its own queue.
        None => match next_message_if_due(ctx) {
            Some(addressed) => addressed,
            None => {
                if let Some(outcome) = park_worker(ctx, 0)? {
                    return Ok(outcome);
                }
                if let Some(outcome) = resume_worker(ctx, 0)? {
                    return Ok(outcome);
                }
                return Ok(DispatchOutcome::ReturnedR0(0));
            }
        },
    };
    write_synthetic_msg_for_hwnd(
        ctx.cpu,
        lp_msg,
        addressed.0,
        addressed.1,
        addressed.2,
        addressed.3,
    )?;
    if remove_mode != 0x0001 {
        ctx.kernel.pending_message = Some(addressed);
    } else {
        ctx.kernel.synthetic_message_count += 1;
    }
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
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 0x102;
    const WAIT_FAILED: u32 = 0xffff_ffff;
    const INFINITE: u32 = 0xffff_ffff;

    let count = ctx.arg_u32(0)?;
    let handles_ptr = ctx.arg_u32(1)?;
    let wait_all = ctx.arg_u32(2)? != 0;
    let timeout = ctx.arg_u32(3)?;
    if count == 0 || count > 64 || handles_ptr == 0 {
        return Ok(DispatchOutcome::ReturnedR0(WAIT_FAILED));
    }

    let raw = ctx.cpu.read_mem(handles_ptr, count.saturating_mul(4))?;
    let handles: Vec<u32> = raw
        .chunks_exact(4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .collect();
    if handles.len() != count as usize {
        return Ok(DispatchOutcome::ReturnedR0(WAIT_FAILED));
    }

    let is_signalled = |kernel: &KernelState, handle: u32| {
        kernel
            .events
            .get(&handle)
            .map(|event| event.signalled)
            .or_else(|| {
                kernel
                    .msg_queues
                    .get(&handle)
                    .map(|queue| !queue.messages.is_empty())
            })
            .or_else(|| {
                kernel
                    .threads
                    .iter()
                    .find(|thread| thread.handle == handle)
                    .map(|thread| thread.finished)
            })
            .unwrap_or(false)
    };

    let ready_index = handles
        .iter()
        .position(|&handle| is_signalled(ctx.kernel, handle));
    let ready = if wait_all {
        handles
            .iter()
            .all(|&handle| is_signalled(ctx.kernel, handle))
    } else {
        ready_index.is_some()
    };
    if ready {
        if wait_all {
            for handle in &handles {
                if let Some(event) = ctx.kernel.events.get_mut(handle) {
                    if !event.manual_reset {
                        event.signalled = false;
                    }
                }
            }
            return Ok(DispatchOutcome::ReturnedR0(WAIT_OBJECT_0));
        }
        let index = ready_index.expect("ready implies one signalled handle");
        if let Some(event) = ctx.kernel.events.get_mut(&handles[index]) {
            if !event.manual_reset {
                event.signalled = false;
            }
        }
        return Ok(DispatchOutcome::ReturnedR0(WAIT_OBJECT_0 + index as u32));
    }

    // Nothing is signalled. Only one guest thread runs at a time here,
    // so whoever would signal one of these objects needs the CPU first:
    // an unsatisfied wait is a scheduling point.
    //
    // An infinite wait re-runs the call when the worker is scheduled
    // again instead of resuming with a value, the same way
    // `wait_for_single_object` does -- `WAIT_TIMEOUT` cannot be the
    // answer to a wait that has no timeout, and a guest that branches on
    // the returned index believes it. Ball Busters' card monitor waits
    // forever on `{re-check event, SDMMC_REMOVE_EVENT}` and reads any
    // index but 0 as the card having been pulled, so parking it with
    // `WAIT_TIMEOUT` handed it 258 and put it on its "SD card removed"
    // screen instead of its main menu.
    if timeout == INFINITE {
        if let Some(outcome) = park_worker_and_reevaluate(ctx)? {
            return Ok(outcome);
        }
        // The main thread is the one waiting, so hand the CPU to a parked
        // worker: it is the only thing that can make any of these objects
        // signalled. Ball Busters' card check runs entirely on the worker
        // it just spawned and then waits for it here, so skipping the
        // handoff left both of its card flags at their startup zeroes and
        // put the game on the "SD card removed" screen even though the
        // check itself now passes.
        if let Some(outcome) = resume_worker(ctx, WAIT_OBJECT_0)? {
            return Ok(outcome);
        }
        // Nothing else can run. Honouring the wait would deadlock the
        // process, so keep the permissive answer -- but say so, because
        // it is a lie the guest did not earn: a worker loop that reads
        // index 0 as "quit" exits on it, and then whoever was waiting
        // for that worker spins forever.
        log::debug!(
            "WaitForMultipleObjects({count} handles, INFINITE) unsatisfied with nothing \
             else runnable; answering WAIT_OBJECT_0 for handle 0x{:08x}",
            handles[0]
        );
        return Ok(DispatchOutcome::ReturnedR0(WAIT_OBJECT_0));
    }
    if let Some(outcome) = park_worker(ctx, WAIT_TIMEOUT)? {
        return Ok(outcome);
    }
    if let Some(outcome) = resume_worker(ctx, WAIT_TIMEOUT)? {
        return Ok(outcome);
    }
    Ok(DispatchOutcome::ReturnedR0(WAIT_TIMEOUT))
}

fn get_system_metrics(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ctx.arg_u32(0)?;
    let (w, h) = screen_dims(ctx);
    // SM_CXSCREEN=0 / SM_CYSCREEN=1, and the SM_*FULLSCREEN /
    // SM_CX*MAXIMIZED aliases Pocket PC games use to size a
    // full-screen window. All of them follow the live panel so a
    // rotated display reports landscape metrics.
    let v = match n {
        0 | 16 | 61 | 78 => w, // SM_CXSCREEN, SM_CXFULLSCREEN, SM_CXMAXIMIZED, SM_CXVIRTUALSCREEN
        1 | 17 | 62 | 79 => h, // SM_CYSCREEN, SM_CYFULLSCREEN, SM_CYMAXIMIZED, SM_CYVIRTUALSCREEN
        _ => 0,
    };
    Ok(DispatchOutcome::ReturnedR0(v))
}

// ---------- GDI ----------

fn get_dc(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(GDI_SCREEN_DC))
}

/// `DefWindowProcW(hwnd, msg, wParam, lParam)`.
///
/// Used to be a constant `0`, which is the right answer for nearly
/// every message but a lie for the two that paint. `DefWindowProc` is
/// where a window with no `WM_PAINT` handler of its own gets its
/// background: it erases the client area with the class brush and
/// leaves the child controls to draw themselves. HelloWorld has no
/// paint handler at all — its `WndProc` only answers `WM_COMMAND` —
/// so before this every pixel it didn't own stayed black, including
/// the ones under its black `STATIC` caption.
///
/// The erase is deliberately conditional on the class having asked for
/// a background: `hbrBackground = NULL` means "the app paints it all",
/// and a GAPI title that writes the framebuffer straight through
/// [`crate::gx`] must never have its frame wiped out from under it.
fn def_window_proc_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let message = ctx.arg_u32(1)?;
    match message {
        WM_PAINT | WM_ERASEBKGND => {
            erase_window_background(ctx);
            // WM_ERASEBKGND returns "I erased it"; WM_PAINT returns 0.
            let r0 = u32::from(message == WM_ERASEBKGND);
            Ok(DispatchOutcome::ReturnedR0(r0))
        }
        _ => Ok(DispatchOutcome::ReturnedR0(0)),
    }
}

/// Fill the client area with the registered class background brush and
/// let the controls repaint on top, as a device's `DefWindowProc` plus
/// display driver would.
fn erase_window_background(ctx: &mut CallCtx<'_>) {
    let Some(cr) = ctx.kernel.window_background else {
        return;
    };
    // A guest holding the GAPI framebuffer owns every pixel; erasing
    // would fight its own back-buffer.
    if ctx.kernel.fb_mapped {
        return;
    }
    let rgb = colorref_to_rgb565(cr);
    let (w, h) = (
        ctx.kernel.framebuffer.width as i32,
        ctx.kernel.framebuffer.height as i32,
    );
    {
        let mut surf = Surface::Screen(&mut ctx.kernel.framebuffer);
        surf.fill_rect(0, 0, w, h, rgb);
    }
    repaint_controls(ctx);
    repaint_status_bar(ctx);
    ctx.kernel.framebuffer.mark_dirty();
}

fn begin_paint(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // BeginPaint(hwnd, lpPaint) -> HDC. Fill the PAINTSTRUCT enough
    // for the caller (most games only read .hdc / .rcPaint).
    let _hwnd = ctx.arg_u32(0)?;
    let lp_paint = ctx.arg_u32(1)?;
    let (screen_w, screen_h) = screen_dims(ctx);
    if lp_paint != 0 {
        let mut buf = [0u8; PAINTSTRUCT_BYTES as usize];
        // hdc
        buf[0..4].copy_from_slice(&GDI_SCREEN_DC.to_le_bytes());
        // fErase = 1
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        // rcPaint = (0,0, screen width, screen height)
        buf[8..12].copy_from_slice(&0u32.to_le_bytes());
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());
        buf[16..20].copy_from_slice(&screen_w.to_le_bytes());
        buf[20..24].copy_from_slice(&screen_h.to_le_bytes());
        ctx.cpu.write_mem(lp_paint, &buf)?;
    }
    Ok(DispatchOutcome::ReturnedR0(GDI_SCREEN_DC))
}

fn end_paint(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // The game has already presented its completed surface when it calls
    // EndPaint. Repainting the shell here can expose an all-white DIB
    // frame before the next game frame is ready. Controls are composited
    // by the frame hook at the presentation boundary instead.
    if !ctx.kernel.fb_mapped {
        repaint_controls(ctx);
        repaint_status_bar(ctx);
    }
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
        0 => STOCK_WHITE_BRUSH,       // WHITE_BRUSH
        1 => STOCK_LTGRAY_BRUSH,      // LTGRAY_BRUSH
        2 => STOCK_GRAY_BRUSH,        // GRAY_BRUSH
        3 => STOCK_DKGRAY_BRUSH,      // DKGRAY_BRUSH
        4 => STOCK_BLACK_BRUSH,       // BLACK_BRUSH
        5 => STOCK_NULL_BRUSH,        // NULL_BRUSH / HOLLOW_BRUSH
        6 => STOCK_WHITE_PEN,         // WHITE_PEN
        7 => STOCK_BLACK_PEN,         // BLACK_PEN
        8 => STOCK_NULL_PEN,          // NULL_PEN
        13 | 17 => STOCK_SYSTEM_FONT, // SYSTEM_FONT / DEFAULT_GUI_FONT
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

fn delete_dc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let deleted = ctx.kernel.gdi.delete(h);
    Ok(DispatchOutcome::ReturnedR0(u32::from(deleted)))
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

fn set_text_align(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dc = ctx.arg_u32(0)?;
    let mode = ctx.arg_u32(1)?;
    let previous = ctx
        .kernel
        .gdi
        .dc_mut(dc)
        .map(|d| {
            let previous = d.text_align;
            d.text_align = mode;
            previous
        })
        .unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(previous))
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
    bit_blt_inner(ctx, hdc_dst, x, y, cx, cy, hdc_src, x1, y1, rop)?;
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
    // game blits via `CreateDIBSection`. RGB555 DIBs need a per-pixel
    // re-pack instead, since only the blue channel lines up.
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
            if bm.dib_rgb555 {
                let src = &raw[row_off..row_off + row_bytes];
                let dst = &mut out[dst_row..dst_row + row_bytes];
                for (s, d) in src.chunks_exact(2).zip(dst.chunks_exact_mut(2)) {
                    let p = pocket_kernel::gdi::rgb555_to_rgb565(u16::from_le_bytes([s[0], s[1]]));
                    d.copy_from_slice(&p.to_le_bytes());
                }
            } else {
                out[dst_row..dst_row + row_bytes]
                    .copy_from_slice(&raw[row_off..row_off + row_bytes]);
            }
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
    rop: u32,
) -> Result<(), KernelError> {
    // Materialise the source pixels into a kernel-level scratch
    // `Vec<u8>` instead of cloning the full source surface every
    // call. Derby is a particularly egregious case: the previous
    // implementation cloned the entire 153 KiB framebuffer on every
    // screen->memory blit, churning megabytes per frame through the
    // allocator. Reusing one buffer across the whole run amortises
    // away that allocation pressure.
    // The pattern operand comes from the destination DC's selected
    // brush. Only a handful of ROP codes reference it, so a missing DC
    // is not itself a reason to bail.
    let pat = ctx
        .kernel
        .gdi
        .dc(hdc_dst)
        .map(|d| colorref_to_rgb565(d.brush_color))
        .unwrap_or(0);

    // ROPs that ignore the source (BLACKNESS, WHITENESS, DSTINVERT,
    // PATCOPY, ...) need no source rectangle at all — real GDI does not
    // read one, and requiring a valid `hdcSrc` here would drop the
    // operation entirely.
    if !rop3::uses_src(rop) {
        if hdc_dst == GDI_SCREEN_DC {
            adapt_panel_to_presentation(ctx, x, y, cx, cy);
        }
        if let Some(mut dst) = surface_for_dc(ctx.kernel, hdc_dst) {
            dst.fill_rect_rop(x, y, cx, cy, pat, rop);
        }
        sync_dst_dib_to_guest(ctx, hdc_dst)?;
        if hdc_dst == GDI_SCREEN_DC {
            ctx.kernel.framebuffer.mark_dirty();
        }
        return Ok(());
    }

    let mut scratch = std::mem::take(&mut ctx.kernel.bit_blt_src_scratch);
    let mut decode_scratch = std::mem::take(&mut ctx.kernel.dib_decode_scratch);

    let (src_w, src_h, ok) = read_blit_source(ctx, hdc_src, &mut scratch, &mut decode_scratch);

    if hdc_dst == GDI_SCREEN_DC {
        adapt_panel_to_presentation(ctx, x, y, cx, cy);
    }

    if ok && src_w != 0 && src_h != 0 {
        if let Some(mut dst) = surface_for_dc(ctx.kernel, hdc_dst) {
            dst.blit_from_bytes_rop(x, y, x1, y1, cx, cy, &scratch, src_w, src_h, rop, pat);
        }
    }

    // Hand the scratch buffers back so the next BitBlt reuses them.
    ctx.kernel.bit_blt_src_scratch = scratch;
    ctx.kernel.dib_decode_scratch = decode_scratch;

    sync_dst_dib_to_guest(ctx, hdc_dst)?;
    if hdc_dst == GDI_SCREEN_DC {
        ctx.kernel.framebuffer.mark_dirty();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stretch_blt_inner(
    ctx: &mut CallCtx<'_>,
    hdc_dst: u32,
    dx: i32,
    dy: i32,
    dw: i32,
    dh: i32,
    hdc_src: u32,
    sx: i32,
    sy: i32,
    sw: i32,
    sh: i32,
    rop: u32,
) -> Result<(), KernelError> {
    if dw <= 0 || dh <= 0 || sw <= 0 || sh <= 0 {
        return Ok(());
    }
    let pat = ctx
        .kernel
        .gdi
        .dc(hdc_dst)
        .map(|d| colorref_to_rgb565(d.brush_color))
        .unwrap_or(0);
    let mut scratch = std::mem::take(&mut ctx.kernel.bit_blt_src_scratch);
    let mut decode_scratch = std::mem::take(&mut ctx.kernel.dib_decode_scratch);
    let (src_w, src_h, ok) = read_blit_source(ctx, hdc_src, &mut scratch, &mut decode_scratch);
    log::debug!(
        "StretchBlt source resolved: ok={ok} size={src_w}x{src_h} bytes={} nonzero={}",
        scratch.len(),
        scratch.iter().any(|&byte| byte != 0)
    );
    if hdc_dst == GDI_SCREEN_DC {
        adapt_panel_to_presentation(ctx, dx, dy, dw, dh);
    }
    if ok && src_w != 0 && src_h != 0 {
        if let Some(mut dst) = surface_for_dc(ctx.kernel, hdc_dst) {
            for y in 0..dh {
                let source_y = sy + (y * sh / dh);
                for x in 0..dw {
                    let source_x = sx + (x * sw / dw);
                    if source_x < 0
                        || source_y < 0
                        || source_x >= src_w as i32
                        || source_y >= src_h as i32
                    {
                        continue;
                    }
                    let off = (source_y as usize * src_w as usize + source_x as usize) * 2;
                    if off + 1 >= scratch.len() {
                        continue;
                    }
                    let source = u16::from_le_bytes([scratch[off], scratch[off + 1]]);
                    let destination = 0;
                    let pixel = pocket_kernel::gdi::rop3::apply(rop, pat, source, destination);
                    dst.put_pixel(dx + x, dy + y, pixel);
                }
            }
            dst.mark_dirty();
        }
    }
    ctx.kernel.bit_blt_src_scratch = scratch;
    ctx.kernel.dib_decode_scratch = decode_scratch;
    sync_dst_dib_to_guest(ctx, hdc_dst)?;
    if hdc_dst == GDI_SCREEN_DC {
        ctx.kernel.framebuffer.mark_dirty();
    }
    Ok(())
}

/// Let the emulated panel take the shape of the surface the game
/// presents to the screen.
///
/// Windows Mobile titles frequently draw in an orientation the
/// portrait 240x320 default cannot hold: Sonic Unleashed renders
/// landscape 320x240 and blits that straight to the screen DC,
/// because on the real handheld the display is rotated first. With a
/// pinned portrait panel that blit was clipped and the visible frame
/// was a corner crop of the real image.
///
/// Only full-screen presentations qualify: the blit must start at the
/// top-left and cover at least as many pixels as the current panel,
/// which rules out dirty-rectangle updates shrinking the display.
fn adapt_panel_to_presentation(ctx: &mut CallCtx<'_>, x: i32, y: i32, cx: i32, cy: i32) {
    const MIN_EDGE: i32 = 64;
    const MAX_EDGE: i32 = 2048;
    if x != 0 || y != 0 {
        return;
    }
    if !(MIN_EDGE..=MAX_EDGE).contains(&cx) || !(MIN_EDGE..=MAX_EDGE).contains(&cy) {
        return;
    }
    let (w, h) = (cx as u32, cy as u32);
    let (cur_w, cur_h) = (ctx.kernel.framebuffer.width, ctx.kernel.framebuffer.height);
    if (w, h) == (cur_w, cur_h) || w * h < cur_w * cur_h {
        return;
    }
    log::info!("panel follows presented surface: {cur_w}x{cur_h} -> {w}x{h}");
    ctx.kernel.framebuffer.resize(w, h);
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
    let (bits_va, w, h, bpp, stride, bottom_up, palette_empty, rgb555) = {
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
            bm.dib_rgb555,
        )
    };

    // Fast path: 16 bpp top-down RGB565 DIB with stride matching our
    // native row layout collapses to a single `write_mem`. Most
    // memory back-buffers fall into this case.
    if bpp == 16 && stride == w * 2 && !bottom_up && !rgb555 {
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
                if bm.dib_rgb555 {
                    let src = &bm.pixels[src_row..src_row + row_bytes];
                    let dst = &mut buf[dst_row..dst_row + row_bytes];
                    for (s, d) in src.chunks_exact(2).zip(dst.chunks_exact_mut(2)) {
                        let p =
                            pocket_kernel::gdi::rgb565_to_rgb555(u16::from_le_bytes([s[0], s[1]]));
                        d.copy_from_slice(&p.to_le_bytes());
                    }
                } else {
                    buf[dst_row..dst_row + row_bytes]
                        .copy_from_slice(&bm.pixels[src_row..src_row + row_bytes]);
                }
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
    let hmod = ctx.arg_u32(0)?;
    let name_raw = ctx.arg_u32(1)?;
    let type_raw = ctx.arg_u32(2)?;
    let want_name = read_wide_resource_key(ctx, name_raw)?;
    let want_type = read_wide_resource_key(ctx, type_raw)?;
    if let Some((entry, base)) = lookup_resource(ctx.kernel, hmod, &want_type, &want_name) {
        let va = base.wrapping_add(entry.data_rva);
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
    if let Some((entry, _base)) = resource_at_address(ctx.kernel, h) {
        return Ok(DispatchOutcome::ReturnedR0(entry.size));
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
/// `HBITMAP SHLoadDIBitmap(LPCTSTR pszFileName)` — the Windows CE shell
/// helper used by older GAPI games for external BMP assets.
fn sh_load_dibitmap(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let path_ptr = ctx.arg_u32(0)?;
    if path_ptr == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let path = String::from_utf16_lossy(&read_wstr(ctx, path_ptr, 260)?);
    let Some(bytes) = read_guest_file(ctx, &path) else {
        log::debug!("SHLoadDIBitmap({path:?}) -> NULL (file not found)");
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let decoded = match image::load_from_memory(&bytes) {
        Ok(image) => image.to_rgb8(),
        Err(error) => {
            log::debug!("SHLoadDIBitmap({path:?}) decode failed: {error}");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    let (width, height) = decoded.dimensions();
    let handle = ctx.kernel.gdi.create_compatible_bitmap(width, height);
    let Some(bitmap) = ctx.kernel.gdi.bitmap_mut(handle) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    for y in 0..height {
        for x in 0..width {
            let pixel = decoded.get_pixel(x, y).0;
            let offset = ((y * width + x) * 2) as usize;
            let rgb565 = pocket_kernel::framebuffer::pack_rgb565(pixel[0], pixel[1], pixel[2]);
            bitmap.pixels[offset..offset + 2].copy_from_slice(&rgb565.to_le_bytes());
        }
    }
    log::debug!("SHLoadDIBitmap({path:?}) -> 0x{handle:08x} ({width}x{height})");
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn load_bitmap_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const RT_BITMAP: ResourceKey = ResourceKey::Id(2);
    let hinst = ctx.arg_u32(0)?;
    let name_raw = ctx.arg_u32(1)?;
    let want_name = read_wide_resource_key(ctx, name_raw)?;
    let (entry, base) = match lookup_resource(ctx.kernel, hinst, &RT_BITMAP, &want_name) {
        Some(found) => found,
        None => {
            log::trace!("LoadBitmapW(name={want_name:?}) -> NULL (resource not found)");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    // Read the bitmap data straight out of the owning module's mapped image.
    let va = base.wrapping_add(entry.data_rva);
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
                // `compression` is checked to be BI_RGB above, so a
                // 16-bpp resource DIB is RGB555 by definition.
                16 => pocket_kernel::gdi::rgb555_to_rgb565(u16::from_le_bytes([
                    raw[row_off + x as usize * 2],
                    raw[row_off + x as usize * 2 + 1],
                ])),
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
    let hinst = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)? & 0xFFFF;
    let buf = ctx.arg_u32(2)?;
    let cch = ctx.arg_u32(3)? as usize;

    let block_id = (id >> 4) + 1;
    let sub = (id & 0xF) as usize;
    let mut wide: Vec<u16> = Vec::new();
    if let Some((entry, base)) =
        lookup_resource(ctx.kernel, hinst, &RT_STRING, &ResourceKey::Id(block_id))
    {
        let va = base.wrapping_add(entry.data_rva);
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
    let (w, ht, bpp, bits_va) = match ctx.kernel.gdi.bitmap(h) {
        Some(b) => (b.width, b.height, b.bpp, b.dib_bits_va.unwrap_or(0)),
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
    // `bmWidthBytes` is the DWORD-aligned stride of the *original*
    // surface, and `bmBits` has to be the guest-visible pixel buffer
    // for DIB sections: Astraware's Bejeweled renders its whole frame
    // by writing straight into `BITMAP.bmBits` after a single
    // `GetObject` call, so reporting NULL sent every pixel store to a
    // low unmapped address and killed the process before its first
    // frame. Non-DIB bitmaps stay host-side and keep reporting NULL.
    let bpp = if bpp == 0 { 16 } else { bpp };
    let stride = (w * u32::from(bpp)).div_ceil(32) * 4;
    let mut buf = [0u8; 24];
    buf[0..4].copy_from_slice(&0u32.to_le_bytes()); // bmType always 0
    buf[4..8].copy_from_slice(&w.to_le_bytes());
    buf[8..12].copy_from_slice(&ht.to_le_bytes());
    buf[12..16].copy_from_slice(&stride.to_le_bytes());
    buf[16..18].copy_from_slice(&1u16.to_le_bytes()); // planes
    buf[18..20].copy_from_slice(&bpp.to_le_bytes());
    buf[20..24].copy_from_slice(&bits_va.to_le_bytes());
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(24))
}

// ---------- additional window / message handlers ----------

/// `GetDlgItem(hDlg, nIDDlgItem)`
///
/// A built-in control created through `CreateWindowExW` is looked up by
/// its id — that is how BlankApp finds the three children it lays out
/// from `WM_SIZE`.
///
/// For anything else we have no real control hierarchy, so the child
/// resolves to its parent. That keeps `SetFocus` / `IsWindowVisible` /
/// `SendMessageW` on the result routed at the proc that owns the dialog,
/// which is where the guest's own handling lives anyway. Returning
/// `NULL` instead makes callers assume the dialog failed to build and
/// bail out.
fn get_dlg_item(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    if let Some(child) = ctx.kernel.controls.by_id(hwnd, id) {
        let found = child.hwnd;
        log::trace!("GetDlgItem(0x{hwnd:08x}, {id}) -> 0x{found:08x}");
        return Ok(DispatchOutcome::ReturnedR0(found));
    }
    log::trace!("GetDlgItem(0x{hwnd:08x}, {id}) -> no such control");
    Ok(DispatchOutcome::ReturnedR0(if is_live_hwnd(hwnd) {
        hwnd
    } else {
        0
    }))
}

/// `CheckRadioButton(hDlg, nIDFirst, nIDLast, nIDCheckButton)`
///
/// Checks one radio button in an id range and clears every other button
/// in that range. Solitaire calls this from `WM_INITDIALOG` to reflect
/// the stored Draw and Scoring settings, so without it the Options
/// dialog opens with no radio selected in either group.
fn check_radio_button(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let first = ctx.arg_u32(1)?;
    let last = ctx.arg_u32(2)?;
    let check = ctx.arg_u32(3)?;
    let hit = ctx
        .kernel
        .controls
        .check_radio_range(hwnd, first, last, check);
    log::debug!("CheckRadioButton(ids {first}..={last}, check {check}) -> {hit} affected");
    if hit > 0 {
        repaint_controls(ctx);
    }
    Ok(DispatchOutcome::ReturnedR0(u32::from(hit > 0)))
}

fn enum_windows(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let callback = ctx.arg_u32(0)?;
    let lparam = ctx.arg_u32(1)?;
    if callback != 0 {
        use pocket_cpu::regs::ArmReg;
        ctx.cpu.write_reg(ArmReg::R0, FAKE_HWND)?;
        ctx.cpu.write_reg(ArmReg::R1, lparam)?;
        log::debug!("EnumWindows callback trampoline -> 0x{callback:08x}");
        return Ok(DispatchOutcome::JumpTo(callback));
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `IsWindowVisible(hwnd)`
///
/// For a control or a dialog panel this is the real answer, tracked by
/// `ShowWindow`. For a top-level window it stays "yes if it exists",
/// since we never hide the game's own window.
///
/// Solitaire depends on the distinction. It creates the Time and Score
/// statics hidden, and its status-line updater shows them only after
/// checking `IsWindowVisible` first — so a handle that always claimed to
/// be visible made the app skip its own `ShowWindow(SW_SHOW)` and left
/// both readouts permanently blank.
fn is_window_visible(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let visible = ctx
        .kernel
        .controls
        .is_visible(hwnd)
        .unwrap_or_else(|| is_live_hwnd(hwnd));
    Ok(DispatchOutcome::ReturnedR0(u32::from(visible)))
}

/// `IsWindowEnabled(hwnd)`
///
/// We never disable the game's window, so any live handle is enabled.
/// Returning `FALSE` here makes dialog-driven titles (Bejeweled's "New
/// User" name entry) drop the input they just received.
fn is_window_enabled(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(is_live_hwnd(hwnd) as u32))
}

/// `GetWindowThreadProcessId(hwnd, lpdwProcessId)`
///
/// One process, one UI thread: report the main thread's id and, when
/// asked, the process id `GetCurrentProcessId` hands out.
fn get_window_thread_process_id(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let process_id_out = ctx.arg_u32(1)?;
    if process_id_out != 0 {
        ctx.cpu
            .write_mem(process_id_out, &MAIN_THREAD_ID.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(MAIN_THREAD_ID))
}

/// `OutputDebugStringW(text)`
///
/// Forward the guest's own debug tracing into our log; it is often the
/// only clue a game gives about what it thinks went wrong.
fn output_debug_string_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let text_ptr = ctx.arg_u32(0)?;
    if text_ptr != 0 {
        if let Ok(text) = read_wstr(ctx, text_ptr, 512) {
            let text = String::from_utf16_lossy(&text);
            log::debug!("OutputDebugStringW: {}", text.trim_end());
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `BOOL DestroyWindow(HWND hWnd)`.
///
/// Destroying a control just drops it. Destroying the top-level window
/// is how a Pocket PC app quits — CERF BlankApp's "Exit" button calls it
/// and expects the `WM_DESTROY` that follows to reach its `WndProc`,
/// which is where its `PostQuitMessage` lives.
fn destroy_window(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    if Controls::is_child_hwnd(hwnd) {
        ctx.kernel.controls.destroy(hwnd);
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    if is_live_hwnd(hwnd) {
        ctx.kernel.controls.destroy_children_of(hwnd);
        if ctx.kernel.posted_messages.len() < 256 {
            ctx.kernel
                .posted_messages
                .push_back((hwnd, WM_DESTROY, 0, 0));
        }
        log::debug!("DestroyWindow(0x{hwnd:08x}) -> WM_DESTROY queued");
    }
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
    let hwnd = ctx.arg_u32(0)?;
    let n_index = ctx.arg_u32(1)? as i32;
    let new_long = ctx.arg_u32(2)?;
    let previous = if n_index == -4 {
        let previous = ctx
            .kernel
            .window_procs
            .get(&hwnd)
            .copied()
            .or_else(|| {
                ctx.kernel
                    .window_classes
                    .get(&hwnd)
                    .and_then(|class| ctx.kernel.window_class_procs.get(class).copied())
            })
            .unwrap_or(ctx.kernel.wnd_proc);
        log::info!("SetWindowLongW(GWL_WNDPROC) re-binding WndProc=0x{new_long:08x} (previous=0x{previous:08x})");
        ctx.kernel.wnd_proc = new_long;
        ctx.kernel.window_procs.insert(hwnd, new_long);
        previous
    } else if n_index == -21 {
        let previous = ctx
            .kernel
            .window_userdata
            .get(&hwnd)
            .copied()
            .unwrap_or(ctx.kernel.window_user_data);
        ctx.kernel.window_user_data = new_long;
        ctx.kernel.window_userdata.insert(hwnd, new_long);
        log::debug!("SetWindowLongW(GWL_USERDATA)=0x{new_long:08x} (previous=0x{previous:08x})");
        previous
    } else {
        0
    };
    Ok(DispatchOutcome::ReturnedR0(previous))
}

/// `LONG GetWindowLongW(HWND hWnd, int nIndex)` — return `0` for
/// every slot we don't track (the documented return when never set).
fn get_window_long_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let n_index = ctx.arg_u32(1)? as i32;
    let v = if n_index == -4 {
        ctx.kernel
            .window_procs
            .get(&hwnd)
            .copied()
            .or_else(|| {
                ctx.kernel
                    .window_classes
                    .get(&hwnd)
                    .and_then(|class| ctx.kernel.window_class_procs.get(class).copied())
            })
            .unwrap_or(ctx.kernel.wnd_proc)
    } else if n_index == -21 {
        ctx.kernel
            .window_userdata
            .get(&hwnd)
            .copied()
            .unwrap_or(ctx.kernel.window_user_data)
    } else {
        0
    };
    Ok(DispatchOutcome::ReturnedR0(v))
}

/// `BOOL SetDlgItemTextW(HWND hDlg, int nIDDlgItem, LPCWSTR lpString)` —
/// Solitaire uses it to refresh the score / time labels on its status
/// dialog. We draw no real controls, so log the text and report success.
fn set_dlg_item_text_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    let p = ctx.arg_u32(2)?;
    let text = if p != 0 {
        String::from_utf16_lossy(&read_wstr(ctx, p, 256).unwrap_or_default())
            .trim_end_matches('\0')
            .to_string()
    } else {
        String::new()
    };
    if let Some(child) = ctx.kernel.controls.by_id(hwnd, id) {
        let child_hwnd = child.hwnd;
        if let Some(c) = ctx.kernel.controls.get_mut(child_hwnd) {
            c.text = text.clone();
            repaint_controls(ctx);
        }
        log::debug!("SetDlgItemTextW(id={id}, text={text:?}) -> control updated");
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    log::debug!("SetDlgItemTextW(id={id}, {text:?})");
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL SetWindowTextW(HWND hWnd, LPCWSTR lpString)` — Pocket PC
/// games (e.g. gspot) call this on every score update to refresh the
/// window's title-bar caption. We have no real window manager, so
/// just log the new caption when DEBUG tracing is enabled and report
/// success. Returns TRUE.
fn set_window_text_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let text = if p != 0 {
        String::from_utf16_lossy(&read_wstr(ctx, p, 256).unwrap_or_default())
            .trim_end_matches('\0')
            .to_string()
    } else {
        String::new()
    };
    set_control_text(ctx, hwnd, &text);
    log::debug!("SetWindowTextW(hwnd=0x{hwnd:08x}, {text:?})");
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL SetWindowTextA(HWND hWnd, LPCSTR lpString)` — ANSI variant.
fn set_window_text_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let text = if p != 0 {
        read_cstr_string(ctx, p, 256).unwrap_or_default()
    } else {
        String::new()
    };
    set_control_text(ctx, hwnd, &text);
    log::debug!("SetWindowTextA(hwnd=0x{hwnd:08x}, {text:?})");
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// Point a built-in control at new text and repaint it.
///
/// Nothing happens for a top-level window: that text is the title-bar
/// caption, which our shell-less framebuffer has nowhere to show.
fn set_control_text(ctx: &mut CallCtx<'_>, hwnd: u32, text: &str) {
    if let Some(child) = ctx.kernel.controls.get_mut(hwnd) {
        child.text = text.to_string();
        repaint_controls(ctx);
    }
}

/// `int GetWindowTextW(HWND hWnd, LPWSTR lpString, int nMaxCount)`.
///
/// A control hands back the text it holds — for an `EDIT` that is
/// whatever the user typed, which is the whole point of a working input
/// field. Anything else has no caption we track, so it reads back empty.
fn get_window_text_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    if p == 0 || n == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let text = ctx
        .kernel
        .controls
        .get(hwnd)
        .map(|c| c.text.clone())
        .unwrap_or_default();
    // Reserve one unit for the terminator, as the real API does.
    let room = (n as usize).saturating_sub(1);
    let units: Vec<u16> = text.encode_utf16().take(room).collect();
    let mut bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    bytes.extend_from_slice(&[0u8, 0u8]);
    let _ = ctx.cpu.write_mem(p, &bytes);
    Ok(DispatchOutcome::ReturnedR0(units.len() as u32))
}

/// `BOOL PlaySoundW(LPCWSTR pszSound, HMODULE hmod, DWORD fdwSound)`.
///
/// WinCE exposes PlaySound as a small WAV convenience API. `pszSound` is
/// either a guest path, an in-image resource name/ID, or NULL (stop). The
/// old implementation returned TRUE without decoding anything, which made
/// every resource-backed effect silently disappear.
fn play_sound_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const SND_ASYNC: u32 = 0x0001;
    const SND_LOOP: u32 = 0x0008;
    const SND_NOSTOP: u32 = 0x0010;
    const SND_PURGE: u32 = 0x0040;
    const SND_RESOURCE: u32 = 0x0004_0004;
    const SND_MEMORY: u32 = 0x0004;

    let sound = ctx.arg_u32(0)?;
    let _hmod = ctx.arg_u32(1)?;
    let flags = ctx.arg_u32(2)?;
    if sound == 0 {
        if flags & SND_PURGE != 0 || flags == 0 {
            ctx.kernel.audio.flush();
            log::debug!("PlaySoundW(NULL, flags=0x{flags:08x}) -> stopped");
        }
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    if flags & SND_RESOURCE != 0 {
        let name = read_wide_resource_key(ctx, sound)?;
        if let Some((resource, base)) =
            lookup_resource(ctx.kernel, _hmod, &ResourceKey::Id(10), &name)
        {
            let va = base.wrapping_add(resource.data_rva);
            let bytes = ctx.cpu.read_mem(va, resource.size)?;
            let ok = submit_wave_bytes(ctx, &bytes, flags, "resource")?;
            log::debug!(
                "PlaySoundW(resource={name:?}, bytes={}, async={}, loop={}) -> {ok}",
                bytes.len(),
                flags & SND_ASYNC != 0,
                flags & SND_LOOP != 0
            );
            return Ok(DispatchOutcome::ReturnedR0(ok as u32));
        }
        log::debug!("PlaySoundW(resource={name:?}) -> missing");
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = if flags & SND_MEMORY != 0 {
        let size = ctx
            .kernel
            .resources
            .iter()
            .find(|e| e.ty == ResourceKey::Id(10))
            .map(|e| e.size)
            .unwrap_or(0);
        if size == 0 {
            Vec::new()
        } else {
            ctx.cpu.read_mem(sound, size)?
        }
    } else {
        let path = read_wstr(ctx, sound, 260).unwrap_or_default();
        let path = String::from_utf16_lossy(&path);
        read_guest_file(ctx, &path).unwrap_or_default()
    };
    let ok = if bytes.is_empty() {
        false
    } else {
        submit_wave_bytes(ctx, &bytes, flags, "file")?
    };
    log::debug!(
        "PlaySoundW(sound=0x{sound:08x}, bytes={}, async={}, loop={}, nostop={}) -> {ok}",
        bytes.len(),
        flags & SND_ASYNC != 0,
        flags & SND_LOOP != 0,
        flags & SND_NOSTOP != 0
    );
    Ok(DispatchOutcome::ReturnedR0(ok as u32))
}

fn read_guest_file(ctx: &mut CallCtx<'_>, path: &str) -> Option<Vec<u8>> {
    use pocket_kernel::vfs::Access;
    let handle = ctx.kernel.vfs.open(path, Access::Read, false)?;
    let size = ctx.kernel.vfs.size(handle)? as usize;
    let mut bytes = vec![0u8; size];
    let n = ctx.kernel.vfs.read(handle, &mut bytes)?;
    let _ = ctx.kernel.vfs.close(handle);
    bytes.truncate(n);
    Some(bytes)
}

/// Read a guest file by path. Shared with `hss.dll`, which loads its
/// clips the same way `PlaySound` does.
pub(crate) fn read_guest_file_for(ctx: &mut CallCtx<'_>, path: &str) -> Option<Vec<u8>> {
    read_guest_file(ctx, path)
}

/// Read a NUL-terminated UTF-16 guest string as a `String`.
pub(crate) fn read_guest_wstr_lossy(
    ctx: &mut CallCtx<'_>,
    p: u32,
    max: u32,
) -> Result<String, KernelError> {
    Ok(String::from_utf16_lossy(&read_wstr(ctx, p, max)?))
}

/// Decode a PCM `.wav` into interleaved `i16` samples.
///
/// Split out of [`submit_wave_bytes`] so `hss.dll` can reuse it: both
/// paths accept exactly the formats [`parse_pcm_wave`] admits, and a
/// second decoder would be a second set of rounding bugs.
pub(crate) fn decode_pcm_wave(
    bytes: &[u8],
) -> Option<(pocket_kernel::audio::GuestFormat, Vec<i16>)> {
    let (fmt, data) = parse_pcm_wave(bytes)?;
    let samples: Vec<i16> = match fmt.bits_per_sample {
        8 => data.iter().map(|&b| (b as i16 - 128) * 256).collect(),
        16 => data
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect(),
        _ => return None,
    };
    Some((fmt, samples))
}

fn submit_wave_bytes(
    ctx: &mut CallCtx<'_>,
    bytes: &[u8],
    flags: u32,
    source: &str,
) -> Result<bool, KernelError> {
    let Some((fmt, samples)) = decode_pcm_wave(bytes) else {
        log::debug!(
            "PlaySoundW {source}: unsupported/non-PCM WAV ({} bytes)",
            bytes.len()
        );
        return Ok(false);
    };
    ctx.kernel.wave_out_format = fmt;
    ctx.kernel.audio.set_guest_format(fmt);
    if flags & 0x0040 != 0 {
        ctx.kernel.audio.flush();
    }
    ctx.kernel
        .audio
        .play_voice(&samples, fmt, flags & 0x0008 != 0);
    ctx.kernel.audio.start();
    Ok(true)
}

fn parse_pcm_wave(bytes: &[u8]) -> Option<(pocket_kernel::audio::GuestFormat, &[u8])> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut p = 12usize;
    let mut fmt = None;
    let mut data = None;
    while p + 8 <= bytes.len() {
        let id = &bytes[p..p + 4];
        let size = u32::from_le_bytes(bytes[p + 4..p + 8].try_into().ok()?) as usize;
        let end = p.checked_add(8)?.checked_add(size)?.min(bytes.len());
        if id == b"fmt " && size >= 16 && p + 24 <= bytes.len() {
            let tag = u16::from_le_bytes(bytes[p + 8..p + 10].try_into().ok()?);
            let channels = u16::from_le_bytes(bytes[p + 10..p + 12].try_into().ok()?);
            let rate = u32::from_le_bytes(bytes[p + 12..p + 16].try_into().ok()?);
            let bits = u16::from_le_bytes(bytes[p + 22..p + 24].try_into().ok()?);
            if tag != 1 || channels == 0 || rate == 0 || !matches!(bits, 8 | 16) {
                return None;
            }
            fmt = Some(pocket_kernel::audio::GuestFormat {
                sample_rate: rate,
                channels,
                bits_per_sample: bits,
            });
        } else if id == b"data" {
            data = Some(&bytes[p + 8..end]);
        }
        p = p + 8 + size + (size & 1);
    }
    Some((fmt?, data?))
}

const OSVERSIONINFOW_BYTES: u32 = 4 + 4 * 4 + 128 * 2;

/// `BOOL GetVersionExW(LPOSVERSIONINFOW lpVersionInformation)`.
/// Reports Windows CE 5.2 / Windows Mobile 6.1, the platform family
/// required by newer native Windows Mobile games such as Zenonia.
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
    buf[4..8].copy_from_slice(&5u32.to_le_bytes());
    buf[8..12].copy_from_slice(&2u32.to_le_bytes());
    buf[12..16].copy_from_slice(&19208u32.to_le_bytes());
    buf[16..20].copy_from_slice(&3u32.to_le_bytes());
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `DWORD GetVersion()` — packed legacy Windows CE form.
fn get_version(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x4B08_0205))
}

/// `BOOL InvalidateRect(HWND, const RECT*, BOOL bErase)`.
///
/// Marking a region invalid asks for a *future* `WM_PAINT`; it changes
/// no pixels by itself. `frame_counter` is what the hosts use to decide
/// a new frame is ready to present, so bumping it here fabricates
/// frames out of a request to draw one.
///
/// Toy Golf makes that visible: it invalidates from its idle loop and
/// calls this ~100 000 times over a couple of million slices, so the
/// counter advanced on nearly every slice boundary. The CLI's dump hook
/// spent its whole `--max-frames` budget on identical black frames
/// captured before the game had drawn anything, and the run stopped
/// with twelve black PPMs while the framebuffer went on to receive a
/// fully rendered 320x240 image.
///
/// The pixels that do change all arrive through drawing calls that mark
/// the framebuffer dirty themselves — `BitBlt`, `PatBlt`, the GDI
/// primitives in `pocket_kernel::gdi`, and `sync_direct_framebuffer_write`
/// for guests that write the mapping directly — so nothing needs this
/// one to notice a repaint.
fn invalidate_rect(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL GetUpdateRect(HWND hWnd, LPRECT lpRect, BOOL bErase)`.
///
/// The synthetic message pump delivers a WM_PAINT for the emulated
/// panel, but it does not maintain a native window-region object. A
/// guest that checks this function before BeginPaint therefore used to
/// discard every paint message and leave frame_counter at zero. Report
/// the pending full-screen update and its rectangle instead.
fn get_update_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hwnd = ctx.arg_u32(0)?;
    let lp_rect = ctx.arg_u32(1)?;
    let _erase = ctx.arg_u32(2)?;
    let (width, height) = screen_dims(ctx);
    write_rect(ctx, lp_rect, width as i32, height as i32)?;
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

fn get_class_name_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let buffer = ctx.arg_u32(1)?;
    let capacity = ctx.arg_u32(2)?.min(256) as usize;
    let name = ctx
        .kernel
        .window_classes
        .get(&hwnd)
        .cloned()
        .unwrap_or_default();
    let utf16: Vec<u16> = name
        .encode_utf16()
        .take(capacity.saturating_sub(1))
        .collect();
    let mut bytes = vec![0u8; capacity.saturating_mul(2)];
    for (i, ch) in utf16.iter().enumerate() {
        bytes[i * 2..i * 2 + 2].copy_from_slice(&ch.to_le_bytes());
    }
    if buffer != 0 && !bytes.is_empty() {
        ctx.cpu.write_mem(buffer, &bytes)?;
    }
    Ok(DispatchOutcome::ReturnedR0(utf16.len() as u32))
}

/// Current geometry of the emulated panel.
///
/// Everything that reports "how big is the screen" has to agree with
/// the live framebuffer, because a game may rotate the display at any
/// point (see [`change_display_settings_ex`]).
/// Read a little-endian `DWORD` out of guest memory.
fn read_u32(ctx: &mut CallCtx<'_>, addr: u32) -> Result<u32, KernelError> {
    let raw = ctx.cpu.read_mem(addr, 4)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn screen_dims(ctx: &CallCtx<'_>) -> (u32, u32) {
    (ctx.kernel.framebuffer.width, ctx.kernel.framebuffer.height)
}

fn window_from_point(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _point = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(FAKE_HWND))
}

fn get_client_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // GetClientRect(hWnd, lpRect) -> BOOL. A control's client rect is
    // its own size with the origin at zero, not the whole screen — and a
    // dialog's is its window less the border and caption, which is what
    // a DialogProc laying itself out asks for.
    let hwnd = ctx.arg_u32(0)?;
    let lp_rect = ctx.arg_u32(1)?;
    let (w, h) = match ctx.kernel.controls.get(hwnd) {
        Some(child) => (child.w, child.h),
        None => match ctx.kernel.controls.panel(hwnd) {
            Some(panel) => {
                let frame = 2 * i32::from(panel.border);
                (panel.w - frame, panel.h - frame - panel.caption_h())
            }
            None => {
                let (w, h) = screen_dims(ctx);
                (w as i32, h as i32)
            }
        },
    };
    write_rect(ctx, lp_rect, w, h)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_window_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let lp_rect = ctx.arg_u32(1)?;
    // A control's window rect is where it sits on screen, which for a
    // control inside a dialog means through its panel's origin.
    let rect = ctx.kernel.controls.screen_rect(hwnd).or_else(|| {
        ctx.kernel
            .controls
            .panel(hwnd)
            .map(|p| (p.x, p.y, p.w, p.h))
    });
    if let Some((x, y, w, h)) = rect {
        if lp_rect != 0 {
            let mut buf = [0u8; 16];
            buf[0..4].copy_from_slice(&x.to_le_bytes());
            buf[4..8].copy_from_slice(&y.to_le_bytes());
            buf[8..12].copy_from_slice(&(x + w).to_le_bytes());
            buf[12..16].copy_from_slice(&(y + h).to_le_bytes());
            ctx.cpu.write_mem(lp_rect, &buf)?;
        }
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let (w, h) = screen_dims(ctx);
    write_rect(ctx, lp_rect, w as i32, h as i32)?;
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

/// `DialogBoxIndirectParamW(hInstance, lpTemplate, hWndParent, lpDialogFunc, dwInitParam)`
/// and `DialogBoxParamW`, which reaches us with the template already
/// resolved to a pointer.
///
/// A *modal* dialog. Like `MessageBoxW` this call blocks: on a device it
/// runs its own message pump and returns only when the `DialogProc`
/// calls `EndDialog`, whose `nResult` is the return value. So the guest
/// is parked in its own API thunk while the host presents frames and the
/// dialog's controls take the stylus — see [`message_box_w`] for the
/// same technique spelled out.
///
/// This used to return `1` immediately without drawing anything, which
/// reads as "the user cancelled instantly". Solitaire's Options dialog
/// is one of these, and it is the only way to switch its Time and Score
/// readouts on: the app hides those statics with `ShowWindow(SW_HIDE)`
/// and shows them again when the dialog's checkboxes say so.
fn dialog_box_indirect_param_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // First pass: materialise the template and register the DialogProc.
    if ctx.kernel.modal_dialog.is_none() {
        let lp_template = ctx.arg_u32(1)?;
        let dialog_proc = ctx.arg_u32(3)?;
        if dialog_proc == 0 {
            // Nothing to pump: behave as the old stub did rather than
            // park forever on a dialog that can never call `EndDialog`.
            return Ok(DispatchOutcome::ReturnedR0(1));
        }
        build_dialog_from_template_for(ctx, lp_template, FAKE_MODAL_HWND, true);
        // A template we could not parse leaves no panel behind; without
        // controls the dialog can never be answered.
        if ctx.kernel.controls.panel(FAKE_MODAL_HWND).is_none() {
            log::debug!("DialogBoxIndirectParamW: no panel materialised, reporting cancelled");
            return Ok(DispatchOutcome::ReturnedR0(1));
        }
        ctx.kernel.modal_dialog = Some(ModalDialog {
            hwnd: FAKE_MODAL_HWND,
            proc: dialog_proc,
            result: None,
            initialised: false,
            dispatched: None,
            spins: 0,
        });
        // Route `SendMessageW` / `DispatchMessageW` for this handle at
        // the DialogProc rather than the frame window's WndProc.
        ctx.kernel.window_procs.insert(FAKE_MODAL_HWND, dialog_proc);
        ctx.kernel.framebuffer.mark_dirty();
        log::debug!("DialogBoxIndirectParamW: modal dialog up, proc=0x{dialog_proc:08x}");
    }

    // `WM_INITDIALOG` first, exactly once: the app fills its checkboxes
    // in from its preferences there, so the dialog is wrong until it has
    // run.
    let pending_init = matches!(ctx.kernel.modal_dialog.as_ref(), Some(d) if !d.initialised);
    if pending_init {
        let (hwnd, proc) = {
            let d = ctx.kernel.modal_dialog.as_mut().expect("checked above");
            d.initialised = true;
            (d.hwnd, d.proc)
        };
        let init_param = ctx.arg_u32(4).unwrap_or(0);
        // Re-enter this handler when the DialogProc returns, so the pump
        // below picks up from the next pass.
        let thunk_va = ctx.thunk.thunk_va;
        ctx.cpu.write_reg(ArmReg::R0, hwnd)?;
        ctx.cpu.write_reg(ArmReg::R1, WM_INITDIALOG)?;
        ctx.cpu.write_reg(ArmReg::R2, 0)?;
        ctx.cpu.write_reg(ArmReg::R3, init_param)?;
        ctx.cpu.write_reg(ArmReg::Lr, thunk_va)?;
        log::debug!("DialogBoxIndirectParamW -> WM_INITDIALOG at 0x{proc:08x}");
        return Ok(DispatchOutcome::JumpTo(proc));
    }

    // Feed the dialog's controls every queued input event. It is modal,
    // so nothing here may reach the application underneath.
    let modal_hwnd = ctx
        .kernel
        .modal_dialog
        .as_ref()
        .map(|d| d.hwnd)
        .unwrap_or(FAKE_MODAL_HWND);
    while let Some(ev) = take_pending_input(ctx) {
        update_key_state(ctx, ev);
        let action = match ev {
            InputEvent::PointerDown { x, y } => ctx
                .kernel
                .controls
                .pointer_down_in(modal_hwnd, x as i32, y as i32),
            InputEvent::PointerUp { x, y } => ctx
                .kernel
                .controls
                .pointer_up_in(modal_hwnd, x as i32, y as i32),
            InputEvent::KeyDown { vk } => ctx.kernel.controls.key_down_in(modal_hwnd, vk),
            _ => None,
        };
        log::trace!("modal dialog input {ev:?} -> {action:?}");
        repaint_controls(ctx);
        ctx.kernel.framebuffer.mark_dirty();
        // A click on one of the dialog's buttons is a `WM_COMMAND` for
        // its own DialogProc — which is where `EndDialog` gets called.
        if let Some(ControlAction::Clicked { parent, id, hwnd }) = action {
            let wparam = (id & 0xFFFF) | (BN_CLICKED << 16);
            let proc = ctx
                .kernel
                .modal_dialog
                .as_ref()
                .map(|d| d.proc)
                .unwrap_or(0);
            if proc != 0 {
                let thunk_va = ctx.thunk.thunk_va;
                // The caption's OK box is chrome, not a child window:
                // the shell reports it with a null `lParam`, the same as
                // a menu command, and only a control passes its handle.
                let from_caption = hwnd == parent;
                let lparam = if from_caption { 0 } else { hwnd };
                log::debug!("modal dialog control id={id} clicked -> WM_COMMAND to DialogProc");
                if let Some(d) = ctx.kernel.modal_dialog.as_mut() {
                    d.dispatched = Some(id);
                }
                ctx.cpu.write_reg(ArmReg::R0, parent)?;
                ctx.cpu.write_reg(ArmReg::R1, WM_COMMAND)?;
                ctx.cpu.write_reg(ArmReg::R2, wparam)?;
                ctx.cpu.write_reg(ArmReg::R3, lparam)?;
                ctx.cpu.write_reg(ArmReg::Lr, thunk_va)?;
                return Ok(DispatchOutcome::JumpTo(proc));
            }
        }
    }

    // `EndDialog` was called: tear the dialog down and return its result.
    if let Some(result) = ctx.kernel.modal_dialog.as_ref().and_then(|d| d.result) {
        log::debug!("DialogBoxIndirectParamW answered with {result}");
        close_modal_dialog(ctx);
        return Ok(DispatchOutcome::ReturnedR0(result));
    }

    // `DefDlgProc` behaviour: an `IDOK` / `IDCANCEL` the DialogProc did
    // not act on ends the dialog with that id. Solitaire's Options
    // applies each setting as it is toggled and returns FALSE from
    // `WM_COMMAND(IDOK)` — on a device the default handler is what
    // actually closes it, so without this the dialog stays up forever.
    if let Some(id) = ctx
        .kernel
        .modal_dialog
        .as_mut()
        .and_then(|d| d.dispatched.take())
    {
        if id == pocket_kernel::msgbox::id::OK || id == pocket_kernel::msgbox::id::CANCEL {
            log::debug!("DialogBoxIndirectParamW: DefDlgProc ends the dialog with {id}");
            close_modal_dialog(ctx);
            return Ok(DispatchOutcome::ReturnedR0(id));
        }
    }

    // Still up. Bound the wait so a headless run cannot spin forever.
    let spins = {
        let d = ctx.kernel.modal_dialog.as_mut().expect("still present");
        d.spins = d.spins.saturating_add(1);
        d.spins
    };
    if spins >= MESSAGE_BOX_MAX_SPINS {
        log::debug!("DialogBoxIndirectParamW unanswered after {spins} spins, reporting cancelled");
        close_modal_dialog(ctx);
        // IDCANCEL, the same answer dismissing the dialog would give.
        return Ok(DispatchOutcome::ReturnedR0(2));
    }
    Ok(DispatchOutcome::JumpTo(ctx.thunk.thunk_va))
}

/// Drop a finished modal dialog: its controls, its panel, its proc
/// registration, and the repaint that reveals what it was covering.
fn close_modal_dialog(ctx: &mut CallCtx<'_>) {
    let hwnd = match ctx.kernel.modal_dialog.take() {
        Some(d) => d.hwnd,
        None => return,
    };
    ctx.kernel.controls.destroy_children_of(hwnd);
    ctx.kernel.window_procs.remove(&hwnd);
    repaint_controls(ctx);
    ctx.kernel.framebuffer.mark_dirty();
}

/// `EndDialog(hDlg, nResult)`
///
/// Closes the modal dialog a parked [`dialog_box_indirect_param_w`] is
/// pumping, and hands it `nResult` to return. This was a
/// `register_constant` — a dispatcher-bypassing thunk that could never
/// have any effect — which is why a modal dialog had no way to close.
fn end_dialog(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let result = ctx.arg_u32(1)?;
    log::debug!("EndDialog(hwnd=0x{hwnd:08x}, result={result})");
    if let Some(dialog) = ctx.kernel.modal_dialog.as_mut() {
        dialog.result = Some(result);
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// How many times `MessageBoxW` may re-dispatch before it gives up and
/// answers itself.
///
/// A modal box on a device waits forever, and so would this one — which
/// is right in front of a user and wrong in a headless run, where there
/// is nobody to tap Yes and the emulator would spin until it was killed.
/// The cap keeps `--max-frames` runs and the test suite finite; at the
/// frontends' frame rate it is several seconds of real waiting, far
/// longer than a person needs to see the box and answer it.
const MESSAGE_BOX_MAX_SPINS: u32 = 100_000;

/// `MessageBoxW` — a real, modal message box.
///
/// This is a *blocking* API: on a device it does not return until the
/// user has answered, and the answer is its return value. The guest is
/// therefore parked inside its own API thunk — the same trick
/// [`park_worker_and_retry`] uses — so each pass through here either
/// finds the box still unanswered and re-dispatches, or takes the
/// answer down and returns it. Meanwhile the host keeps presenting
/// frames, and [`KernelState::composite_controls`] draws the box on top
/// of them, so the user actually sees what they are answering.
///
/// The returned id matters: Solitaire's Exit button only calls
/// `PostQuitMessage` when it gets `IDYES` back, so the old stub's
/// blanket `IDOK` meant Exit did nothing at all.
fn message_box_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // First pass: build the box from the arguments and show it.
    if ctx.kernel.modal.is_none() {
        let text_p = ctx.arg_u32(1)?;
        let caption_p = ctx.arg_u32(2)?;
        let u_type = ctx.arg_u32(3)?;
        let text = String::from_utf16_lossy(&read_wstr(ctx, text_p, 1024)?);
        let caption = if caption_p == 0 {
            String::new()
        } else {
            String::from_utf16_lossy(&read_wstr(ctx, caption_p, 260)?)
        };
        let (sw, sh) = (
            ctx.kernel.framebuffer.width as i32,
            ctx.kernel.framebuffer.height as i32,
        );
        log::debug!("MessageBoxW(text={text:?}, caption={caption:?}, type=0x{u_type:08x})");
        ctx.kernel.modal = Some(MessageBox::new(&text, &caption, u_type, sw, sh));
        ctx.kernel.message_box_spins = 0;
        ctx.kernel.framebuffer.mark_dirty();
    }

    // Feed the box every input event the host has queued. These must not
    // reach the application: it is modal, and on a device its window
    // owns the stylus until it closes.
    while let Some(ev) = take_pending_input(ctx) {
        update_key_state(ctx, ev);
        let Some(modal) = ctx.kernel.modal.as_mut() else {
            break;
        };
        match ev {
            InputEvent::PointerDown { x, y } => modal.pointer_down(x as i32, y as i32),
            InputEvent::PointerUp { x, y } => modal.pointer_up(x as i32, y as i32),
            InputEvent::KeyDown { vk } => modal.key_down(vk),
            _ => {}
        }
        ctx.kernel.framebuffer.mark_dirty();
    }

    // Answered — take the box down and hand the id back.
    if let Some(result) = ctx.kernel.modal.as_ref().and_then(|m| m.result) {
        log::debug!("MessageBoxW answered with {result}");
        ctx.kernel.modal = None;
        // The box was covering guest pixels; the frame after this one has
        // to be drawn without it.
        repaint_controls(ctx);
        ctx.kernel.framebuffer.mark_dirty();
        return Ok(DispatchOutcome::ReturnedR0(result));
    }

    // Still waiting. Give up rather than hang a headless run forever.
    ctx.kernel.message_box_spins = ctx.kernel.message_box_spins.saturating_add(1);
    if ctx.kernel.message_box_spins >= MESSAGE_BOX_MAX_SPINS {
        let fallback = ctx
            .kernel
            .modal
            .as_ref()
            .map(|m| m.default_result())
            .unwrap_or(1);
        log::debug!(
            "MessageBoxW unanswered after {MESSAGE_BOX_MAX_SPINS} spins, taking {fallback}"
        );
        ctx.kernel.modal = None;
        repaint_controls(ctx);
        ctx.kernel.framebuffer.mark_dirty();
        return Ok(DispatchOutcome::ReturnedR0(fallback));
    }

    // Re-enter our own thunk: the call blocks, exactly as it does on the
    // device, and the run loop gets to present a frame in between.
    Ok(DispatchOutcome::JumpTo(ctx.thunk.thunk_va))
}

fn register_hot_key(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    let modifiers = ctx.arg_u32(2)?;
    let key = ctx.arg_u32(3)?;
    log::debug!(
        "RegisterHotKey(hwnd=0x{hwnd:08x}, id={id}, modifiers=0x{modifiers:08x}, key=0x{key:04x})"
    );
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn unregister_hot_key(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    log::debug!("UnregisterHotKey(hwnd=0x{hwnd:08x}, id={id})");
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

/// `HANDLE CreateEventW(LPSECURITY_ATTRIBUTES, BOOL bManualReset,
///                       BOOL bInitialState, LPCWSTR lpName)`
///
/// Hands out a fake handle and remembers the event's real state, which
/// `WaitForSingleObject` needs in order to answer honestly.
fn create_event_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let manual_reset = ctx.arg_u32(1)? != 0;
    let initial = ctx.arg_u32(2)? != 0;
    let handle = 0xDEAD_E001u32.wrapping_add(ctx.kernel.events.len() as u32);
    ctx.kernel.events.insert(
        handle,
        pocket_kernel::EventObject {
            manual_reset,
            signalled: initial,
        },
    );
    log::debug!("CreateEvent(manual={manual_reset}, initial={initial}) -> 0x{handle:08x}");
    Ok(DispatchOutcome::ReturnedR0(handle))
}

/// `BOOL SetEvent(HANDLE)` — also serves `PulseEvent`. We have no
/// blocked waiters to release synchronously, so a pulse is just a set;
/// the next wait consumes it (auto-reset) or sees it (manual).
fn open_event_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _access = ctx.arg_u32(0)?;
    let _inherit = ctx.arg_u32(1)?;
    let name_ptr = ctx.arg_u32(2)?;
    let name = if name_ptr == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&read_wstr(ctx, name_ptr, 260).unwrap_or_default())
    };
    if let Some((&handle, _)) = ctx.kernel.events.iter().find(|(_, event)| event.signalled) {
        log::debug!("OpenEvent({name:?}) -> existing 0x{handle:08x}");
        return Ok(DispatchOutcome::ReturnedR0(handle));
    }
    let handle = 0xDEAD_E001u32.wrapping_add(ctx.kernel.events.len() as u32);
    ctx.kernel.events.insert(
        handle,
        pocket_kernel::EventObject {
            manual_reset: false,
            signalled: false,
        },
    );
    log::debug!("OpenEvent({name:?}) -> 0x{handle:08x}");
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn set_event(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    if let Some(ev) = ctx.kernel.events.get_mut(&handle) {
        ev.signalled = true;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL ResetEvent(HANDLE)`.
fn reset_event(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    if let Some(ev) = ctx.kernel.events.get_mut(&handle) {
        ev.signalled = false;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL EventModify(HANDLE, DWORD action)` — the syscall that
/// `SetEvent`, `ResetEvent` and `PulseEvent` are macros over in the
/// Windows CE SDK headers.
///
/// A guest built against that SDK imports `EventModify` by ordinal and
/// never mentions the three names at all, so stubbing this out meant
/// every event those guests set or cleared was silently dropped while
/// the `SetEvent` handler next to it sat unused. Ball Busters' card
/// monitor signals its "the card checked out" event with
/// `EventModify(h, EVENT_SET)`, and with the signal going nowhere the
/// main thread's wait on it could only ever be answered by guesswork.
fn event_modify(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const EVENT_PULSE: u32 = 1;
    const EVENT_RESET: u32 = 2;
    const EVENT_SET: u32 = 3;

    let handle = ctx.arg_u32(0)?;
    let action = ctx.arg_u32(1)?;
    let Some(ev) = ctx.kernel.events.get_mut(&handle) else {
        log::debug!("EventModify(0x{handle:08x}, {action}) -> TRUE (not an event)");
        return Ok(DispatchOutcome::ReturnedR0(1));
    };
    match action {
        EVENT_SET => ev.signalled = true,
        EVENT_RESET => ev.signalled = false,
        // A pulse releases whoever is already waiting and leaves the
        // event clear. Only one guest thread runs at a time here, so the
        // waiters have not been able to observe anything since the last
        // scheduling point: leaving it signalled is what lets them see
        // the pulse at all, and matches what `PulseEvent` is registered
        // to do.
        EVENT_PULSE => ev.signalled = true,
        _ => {
            log::debug!("EventModify(0x{handle:08x}, {action}) -> FALSE (unknown action)");
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `DWORD WaitForSingleObject(HANDLE, DWORD dwMilliseconds)`
///
/// Returns `WAIT_OBJECT_0` for anything that really is signalled (a
/// set event, a finished thread, an unknown handle waited on
/// forever) and `WAIT_TIMEOUT` for a finite wait on something that
/// isn't. Getting the timeout case right is what lets a guest worker
/// thread keep looping instead of tearing itself down on its first
/// iteration.
///
/// A wait that would block also doubles as a scheduling point: this
/// HLE runs one guest thread at a time, so the caller is parked and
/// the other side of the ping-pong gets the CPU.
fn wait_for_single_object(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 0x102;
    const INFINITE: u32 = 0xFFFF_FFFF;

    let handle = ctx.arg_u32(0)?;
    let timeout = ctx.arg_u32(1)?;

    // A point-to-point message queue is a waitable object on the
    // device: the reader blocks on the handle and only calls
    // `ReadMsgQueue` once the wait is satisfied. Whoever fills that
    // queue is another guest thread, so an empty queue has to be a
    // scheduling point -- answering `WAIT_OBJECT_0` here spins the
    // reader forever and starves the writer, which is how Bejeweled
    // hangs on its loading screen.
    if let Some(queue) = ctx.kernel.msg_queues.get(&handle) {
        if !queue.messages.is_empty() {
            return Ok(DispatchOutcome::ReturnedR0(WAIT_OBJECT_0));
        }
        if let Some(outcome) = park_worker_and_retry(ctx)? {
            return Ok(outcome);
        }
        if let Some(outcome) = resume_worker(ctx, WAIT_TIMEOUT)? {
            return Ok(outcome);
        }
        return Ok(DispatchOutcome::ReturnedR0(WAIT_TIMEOUT));
    }

    if let Some(ev) = ctx.kernel.events.get_mut(&handle) {
        if ev.signalled {
            if !ev.manual_reset {
                ev.signalled = false;
            }
            return Ok(DispatchOutcome::ReturnedR0(WAIT_OBJECT_0));
        }
    } else if let Some(semaphore) = ctx.kernel.semaphores.get_mut(&handle) {
        if semaphore.count > 0 {
            semaphore.count -= 1;
            return Ok(DispatchOutcome::ReturnedR0(WAIT_OBJECT_0));
        }
    } else if !ctx
        .kernel
        .threads
        .iter()
        .any(|thread| thread.handle == handle && !thread.finished)
    {
        // Unknown handle (mutex, already-reaped thread): we don't model
        // it, so keep the old permissive answer.
        return Ok(DispatchOutcome::ReturnedR0(WAIT_OBJECT_0));
    }

    // A live thread handle or an unsignalled event.
    //
    // For a worker, yield: this HLE runs one guest thread at a time, so
    // whoever would signal the object needs the CPU before the wait can
    // possibly be satisfied. An infinite wait re-runs the call when the
    // worker is scheduled again rather than returning a value --
    // `WAIT_TIMEOUT` is a lie for `INFINITE`, and `WAIT_OBJECT_0` is the
    // lie that kept Toy Golf's mixer thread spinning: it waits forever
    // on the buffer-done event and, told the wait succeeded, refilled
    // buffers without ever yielding, so the main thread drew two frames
    // and then starved.
    if timeout == INFINITE {
        if let Some(outcome) = park_worker_and_reevaluate(ctx)? {
            return Ok(outcome);
        }
        // The main thread is the one waiting, and nothing else can make
        // the object signalled from here. Honouring the wait would
        // deadlock the process, so keep the permissive answer.
        return Ok(DispatchOutcome::ReturnedR0(WAIT_OBJECT_0));
    }
    if let Some(outcome) = park_worker(ctx, WAIT_TIMEOUT)? {
        return Ok(outcome);
    }
    if let Some(outcome) = resume_worker(ctx, WAIT_TIMEOUT)? {
        return Ok(outcome);
    }
    Ok(DispatchOutcome::ReturnedR0(WAIT_TIMEOUT))
}

/// `dwCreationFlags` bit that asks for a thread that does not run
/// until `ResumeThread`.
const CREATE_SUSPENDED: u32 = 0x4;

fn create_thread(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _security_attributes = ctx.arg_u32(0)?;
    let stack_size = ctx.arg_u32(1)?.max(0x1000);
    let entry = ctx.arg_u32(2)?;
    let parameter = ctx.arg_u32(3)?;
    let creation_flags = ctx.arg_u32(4)?;
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
    // Thread ids start at 2: id 1 is the main thread, and 0 must stay
    // reserved because `PostThreadMessageW(0, ...)` is what a guest
    // ends up posting when it never learned a real id.
    let thread_id = MAIN_THREAD_ID + 1 + thread_index as u32;
    // The creator resumes with the new thread's handle in R0 — that is
    // `CreateThread`'s return value. Without this the guest stores a
    // stale R0 (usually 0) as its thread handle and every later
    // `WaitForSingleObject` / `TerminateThread` on it misses.
    saved_regs[0] = handle;
    // WinCE treats a zero or tiny requested size as a request for the
    // process default, and several CRT worker entry points use more than
    // one 4 KiB page before reaching their first wait. Mapping only the
    // literal request makes the first deep prologue write below the
    // allocation and starves the main thread before it can present the
    // next frame.
    let stack_size = stack_size.clamp(pocket_kernel::DEFAULT_STACK_SIZE, 0x100000);
    let stack_base = stack_top.saturating_sub(stack_size) & !0xfff;
    ctx.cpu.map_region(
        stack_base.saturating_sub(0x2000),
        pocket_cpu::round_up_to_page(stack_size.saturating_add(0x3000)),
        pocket_cpu::Prot::READ | pocket_cpu::Prot::WRITE,
    )?;
    let mut thread = GuestThread::new(
        entry, parameter, stack_top, stack_size, exit_va, resume_pc, handle, saved_regs,
    );
    thread.id = thread_id;
    // `lpThreadId` is not optional in practice: a game that opens a
    // waveOut device with `CALLBACK_THREAD` passes the id it read back
    // here, and leaving the caller's variable untouched made it ask the
    // driver to notify thread 0.
    let thread_id_out = ctx.arg_u32(5)?;
    if thread_id_out != 0 {
        ctx.cpu.write_mem(thread_id_out, &thread_id.to_le_bytes())?;
    }
    ctx.cpu.add_code_hook(exit_va)?;
    let suspended = creation_flags & CREATE_SUSPENDED != 0;
    log::debug!(
        "CreateThread entry=0x{entry:08x} parameter=0x{parameter:08x} stack={} suspended={suspended} -> handle=0x{handle:08x} id={thread_id}",
        stack_size,
    );
    // Park every new thread at its entry point and let the creator run
    // on, whether or not `CREATE_SUSPENDED` was asked for.
    //
    // `CREATE_SUSPENDED` is not a detail we can skip -- a game that asks
    // for it finishes wiring the thread's state up *after* `CreateThread`
    // returns (Asphalt 2's mixer thread waits on an event that
    // `CreateEvent` has not produced yet) -- but the same is true without
    // the flag. On the device `CreateThread` returns to the creator,
    // which keeps its timeslice and finishes initialising the object the
    // new thread is about to read. Entering the entry point immediately
    // instead makes the thread observe a half-built object: Toy Golf's
    // audio thread dereferences the mixer its creator has not stored yet
    // and faults on a NULL `[r5,#0x38]`.
    //
    // The difference between the two is only *when* the thread becomes
    // eligible to run: a suspended one waits for `ResumeThread` to set
    // `started`, a normal one is eligible right away. `resume_worker`
    // hands it the CPU at the creator's next blocking call -- `Sleep`,
    // `WaitForSingleObject`, `WaitForMultipleObjects` or the message
    // pump -- which is exactly where a cooperative scheduler can switch.
    let mut worker_regs = [0u32; 17];
    worker_regs[0] = parameter;
    worker_regs[13] = stack_top - 16;
    worker_regs[14] = exit_va;
    worker_regs[15] = entry;
    worker_regs[16] = ctx.cpu.read_reg(ArmReg::Cpsr)?;
    thread.worker_regs = worker_regs;
    thread.worker_saved = true;
    thread.started = !suspended;
    ctx.kernel.threads.push(thread);
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn get_current_thread_id(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(current_thread_id(ctx)))
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
    let mut bi_width = i32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let mut bi_height = i32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let mut bi_bpp = u16::from_le_bytes([hdr[14], hdr[15]]);
    let mut bi_compression = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
    let bi_colors_used = u32::from_le_bytes([hdr[32], hdr[33], hdr[34], hdr[35]]);
    let malformed = bi_size != 40 || bi_width <= 0 || bi_height == 0;
    if malformed {
        log::debug!(
            "CreateDIBSection info=0x{pbmi:08x} has invalid header; using screen-sized RGB565 fallback"
        );
        bi_width = ctx.kernel.framebuffer.width as i32;
        bi_height = -(ctx.kernel.framebuffer.height as i32);
        bi_bpp = 16;
        bi_compression = 0;
    }
    log::debug!(
        "CreateDIBSection info=0x{pbmi:08x} size={bi_size} width={bi_width} height={bi_height} bpp={bi_bpp} compression={bi_compression} colors={bi_colors_used}"
    );
    if bi_width < 0 || bi_height == 0 || (bi_compression != 0 && bi_compression != 3) {
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
    // Decide the 16-bpp channel layout. Win32 says `biBitCount = 16`
    // with `BI_RGB` is 5-5-5 (top bit unused); 5-6-5 requires
    // `BI_BITFIELDS` plus an explicit mask triple stored immediately
    // after the header. Asphalt 2's Motorola Q9 build asks for
    // `16bpp/BI_RGB` and writes genuine 555 pixels, so reading them
    // as 565 shifted every channel and turned the artwork green.
    let rgb555 = bi_bpp == 16
        && if bi_compression == 3 {
            let masks = ctx.cpu.read_mem(pbmi + bi_size, 12).unwrap_or_default();
            masks.len() == 12
                && u32::from_le_bytes([masks[4], masks[5], masks[6], masks[7]]) == 0x0000_03E0
        } else {
            true
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
        rgb555,
    );
    let handle = ctx.kernel.gdi.register_dib(bm);
    log::debug!(
        "CreateDIBSection({}x{}, {}bpp{}, {}-up) -> 0x{:08x} bits=0x{:08x}",
        width,
        height,
        bi_bpp,
        if rgb555 { " RGB555" } else { "" },
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

/// `LONG SetBitmapBits(HBITMAP hbmp, DWORD cb, const void *bits)`.
///
/// DoubleSkill creates a 240x320 16-bpp DDB, fills it with one complete
/// RGB565 frame, then selects it into a memory DC and blits it to the
/// screen in `WM_PAINT`. Returning zero leaves the source bitmap black,
/// so the game appears to run while every rendered frame is blank.
fn set_bitmap_bits(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let bitmap = ctx.arg_u32(0)?;
    let byte_count = ctx.arg_u32(1)?;
    let bits = ctx.arg_u32(2)?;
    if bits == 0 || byte_count == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    let Some(target) = ctx.kernel.gdi.bitmap_mut(bitmap) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let expected = target.pixels.len() as u32;
    let count = byte_count.min(expected) as usize;
    let data = ctx.cpu.read_mem(bits, count as u32)?;
    target.pixels[..count].copy_from_slice(&data);
    target.host_dirty = true;
    Ok(DispatchOutcome::ReturnedR0(count as u32))
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
    let rop = ctx.arg_u32(5)?;
    let dc_meta = match ctx.kernel.gdi.dc(hdc).cloned() {
        Some(d) => d,
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    let rgb = colorref_to_rgb565(dc_meta.brush_color);
    if let Some(mut surf) = surface_for_dc(ctx.kernel, hdc) {
        // PatBlt has no source operand, so BLACKNESS/WHITENESS/
        // DSTINVERT/PATINVERT are as common here as PATCOPY.
        surf.fill_rect_rop(x, y, w, h, rgb, rop);
    }
    sync_dst_dib_to_guest(ctx, hdc)?;
    if hdc == GDI_SCREEN_DC {
        ctx.kernel.framebuffer.mark_dirty();
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
    let sw = ctx.arg_u32(8)? as i32;
    let sh = ctx.arg_u32(9)? as i32;
    let rop = ctx.arg_u32(10)?;
    stretch_blt_inner(ctx, hdc_dst, dx, dy, dw, dh, hdc_src, sx, sy, sw, sh, rop)?;
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
        (
            0,
            0,
            ctx.kernel.framebuffer.width as i32,
            ctx.kernel.framebuffer.height as i32,
        )
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

fn aligned_text_origin(
    dc: &pocket_kernel::gdi::Dc,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> (i32, i32) {
    let x = match dc.text_align & 0x6 {
        0x6 => x - width / 2,
        0x2 => x - width,
        _ => x,
    };
    let y = if dc.text_align & 0x18 == 0x18 {
        y - height
    } else {
        y
    };
    (x, y)
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
    let (x, y) = aligned_text_origin(&dc_meta, x, y, pixel_w, pocket_kernel::font::GLYPH_H);
    if let Some(mut surf) = surface_for_dc(ctx.kernel, hdc) {
        if !dc_meta.bk_transparent {
            surf.fill_rect(x, y, pixel_w, pocket_kernel::font::GLYPH_H, bk_color);
        }
        pocket_kernel::font::draw_str_u16(&mut surf, x, y, &chars, color);
        surf.mark_dirty();
    }
    Ok(())
}

fn ext_escape(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const GETGXINFO: u32 = 0x0002_0000;
    const GETRAWFRAMEBUFFER: u32 = 0x0002_0001;
    const KF_DIRECT565: u32 = 0x0000_0080;
    let escape = ctx.arg_u32(1)?;
    let out_count = ctx.arg_u32(4)?;
    let out_data = ctx.arg_u32(5)?;
    if out_data == 0 || out_count == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if matches!(escape, GETRAWFRAMEBUFFER | GETGXINFO) && !ctx.kernel.fb_mapped {
        let bytes = (ctx.kernel.framebuffer.byte_size() + 0xfff) & !0xfff;
        ctx.cpu
            .map_region(SYNTHETIC_FRAMEBUFFER_BASE, bytes, Prot::READ | Prot::WRITE)?;
        ctx.cpu
            .write_mem(SYNTHETIC_FRAMEBUFFER_BASE, &ctx.kernel.framebuffer.pixels)?;
        ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
        ctx.kernel.fb_mapped = true;
    }
    log::debug!(
        "ExtEscape escape=0x{escape:08x} out_count={out_count} out_data=0x{out_data:08x} fb_mapped={}",
        ctx.kernel.fb_mapped
    );
    match escape {
        GETRAWFRAMEBUFFER if out_count >= 24 => {
            let mut raw = [0u8; 24];
            raw[0..2].copy_from_slice(&1u16.to_le_bytes());
            raw[2..4].copy_from_slice(&(ctx.kernel.framebuffer.bpp as u16).to_le_bytes());
            raw[4..8].copy_from_slice(&pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE.to_le_bytes());
            raw[8..12].copy_from_slice(&(ctx.kernel.framebuffer.bpp / 8).to_le_bytes());
            raw[12..16].copy_from_slice(&ctx.kernel.framebuffer.stride_bytes().to_le_bytes());
            raw[16..20].copy_from_slice(&ctx.kernel.framebuffer.width.to_le_bytes());
            raw[20..24].copy_from_slice(&ctx.kernel.framebuffer.height.to_le_bytes());
            ctx.cpu.write_mem(out_data, &raw)?;
            Ok(DispatchOutcome::ReturnedR0(24))
        }
        GETGXINFO if out_count >= 0x84 => {
            let mut info = [0u8; 0x84];
            info[0..4].copy_from_slice(&100u32.to_le_bytes());
            info[4..8].copy_from_slice(&pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE.to_le_bytes());
            info[8..12].copy_from_slice(&ctx.kernel.framebuffer.stride_bytes().to_le_bytes());
            info[12..16].copy_from_slice(&ctx.kernel.framebuffer.width.to_le_bytes());
            info[16..20].copy_from_slice(&ctx.kernel.framebuffer.height.to_le_bytes());
            info[20..24].copy_from_slice(&ctx.kernel.framebuffer.bpp.to_le_bytes());
            info[24..28].copy_from_slice(&KF_DIRECT565.to_le_bytes());
            ctx.cpu.write_mem(out_data, &info)?;
            Ok(DispatchOutcome::ReturnedR0(0x84))
        }
        _ => Ok(DispatchOutcome::ReturnedR0(0)),
    }
}

/// `BOOL EnumDisplaySettings(LPCTSTR device, DWORD iModeNum, DEVMODE *dm)`
///
/// SDL 1.2 (which every Gameloft Windows Mobile title links against)
/// builds its list of legal full-screen modes from this call and then
/// refuses to start with "No video mode large enough for WxH" when the
/// list is empty. We expose exactly one mode — the emulated panel —
/// for `iModeNum == 0` and for `ENUM_CURRENT_SETTINGS`, and report
/// "no more modes" for anything else so the enumeration loop ends.
fn enum_display_settings(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Field offsets inside `DEVMODEW`; identical on Windows CE and
    // desktop Win32 up to `dmDisplayFrequency`.
    const DM_FIELDS: u32 = 72;
    const DM_BITSPERPEL: u32 = 168;
    const DM_PELSWIDTH: u32 = 172;
    const DM_PELSHEIGHT: u32 = 176;
    const DM_DISPLAYFLAGS: u32 = 180;
    const DM_DISPLAYFREQUENCY: u32 = 184;
    // DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY
    const FIELDS_MASK: u32 = 0x0004_0000 | 0x0008_0000 | 0x0010_0000 | 0x0040_0000;
    const ENUM_CURRENT_SETTINGS: u32 = 0xFFFF_FFFF;
    const ENUM_REGISTRY_SETTINGS: u32 = 0xFFFF_FFFE;

    let mode_num = ctx.arg_u32(1)?;
    let dm = ctx.arg_u32(2)?;
    if dm == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if !matches!(mode_num, 0 | ENUM_CURRENT_SETTINGS | ENUM_REGISTRY_SETTINGS) {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    ctx.cpu
        .write_mem(dm + DM_FIELDS, &FIELDS_MASK.to_le_bytes())?;
    ctx.cpu
        .write_mem(dm + DM_BITSPERPEL, &16u32.to_le_bytes())?;
    let (screen_w, screen_h) = screen_dims(ctx);
    ctx.cpu
        .write_mem(dm + DM_PELSWIDTH, &screen_w.to_le_bytes())?;
    ctx.cpu
        .write_mem(dm + DM_PELSHEIGHT, &screen_h.to_le_bytes())?;
    ctx.cpu
        .write_mem(dm + DM_DISPLAYFLAGS, &0u32.to_le_bytes())?;
    ctx.cpu
        .write_mem(dm + DM_DISPLAYFREQUENCY, &60u32.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `LONG ChangeDisplaySettingsEx(LPCTSTR device, DEVMODE *dm, HWND, DWORD flags, LPVOID)`
///
/// Landscape Pocket PC games (Sonic Unleashed, Asphalt, most Gameloft
/// titles) ship for a portrait 240x320 panel and rotate the display to
/// 320x240 on startup, then render one landscape 320x240 surface per
/// frame. Returning "success" without actually re-shaping the emulated
/// panel left the game blitting a 320-pixel-wide image into a
/// 240-pixel-wide framebuffer, so a third of every frame was clipped
/// away.
///
/// Three spellings of the request are honoured: an explicit
/// `dmPelsWidth`/`dmPelsHeight` mode, Windows CE's
/// `DM_DISPLAYORIENTATION` (`dmDisplayOrientation` at the *end* of
/// `DEVMODEW`, not the desktop `dmOrientation` printer field), and the
/// CE-only `DM_DISPLAYQUERYORIENTATION` probe that asks which
/// orientations the driver can do before requesting one. Xtrakt sends
/// the probe first and, when the answer is "none", gives up on rotating
/// and draws its landscape layout sideways into the portrait panel.
fn change_display_settings_ex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const DM_PELSWIDTH_FLAG: u32 = 0x0008_0000;
    const DM_PELSHEIGHT_FLAG: u32 = 0x0010_0000;
    /// CE-only: "rotate to `dmDisplayOrientation`".
    const DM_DISPLAYORIENTATION_FLAG: u32 = 0x0080_0000;
    /// CE-only: "report the rotations you support, do not change mode".
    const DM_DISPLAYQUERYORIENTATION_FLAG: u32 = 0x0100_0000;
    // Offsets inside `DEVMODEW` (see `enum_display_settings`).
    const OFF_FIELDS: u32 = 72;
    const OFF_PELSWIDTH: u32 = 172;
    const OFF_PELSHEIGHT: u32 = 176;
    /// `dmDisplayOrientation`, the last DWORD of CE's 192-byte DEVMODEW.
    const OFF_DISPLAYORIENTATION: u32 = 188;
    /// CE's `DMDO_*` are chosen so a driver can OR them into a
    /// capability mask: 90=1, 180=2, 270=4, with 0 meaning upright.
    const DMDO_0: u32 = 0;
    const DMDO_90: u32 = 1;
    const DMDO_270: u32 = 4;
    /// "Report whether this mode is possible, but do not apply it."
    const CDS_TEST: u32 = 0x0000_0002;
    /// Refuse absurd modes; a bad DEVMODE would otherwise allocate
    /// gigabytes for the panel.
    const MAX_EDGE: u32 = 2048;

    let dm = ctx.arg_u32(1)?;
    if dm == 0 {
        // NULL DEVMODE means "return to the default mode".
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let fields = read_u32(ctx, dm + OFF_FIELDS)?;
    let flags = ctx.arg_u32(3)?;
    let (cur_w, cur_h) = screen_dims(ctx);

    if fields & DM_DISPLAYQUERYORIENTATION_FLAG != 0 {
        // We can present any rotation, because rotating is only a
        // matter of what shape we hand the guest — it draws in the
        // rotated coordinate system itself.
        let caps = (DMDO_90 | 2 | DMDO_270).to_le_bytes();
        ctx.cpu.write_mem(dm + OFF_DISPLAYORIENTATION, &caps)?;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    // `CDS_TEST` with `DM_DISPLAYORIENTATION` is a read, not a write:
    // the driver reports the orientation currently in effect and
    // changes nothing. Xtrakt uses it to decide whether it needs to
    // un-rotate the panel before drawing, so answering with the
    // capability mask left over from the probe above would send it
    // chasing a rotation that never happened.
    if fields & DM_DISPLAYORIENTATION_FLAG != 0 && flags & CDS_TEST != 0 {
        // The emulated panel is never rotated out from under the guest.
        ctx.cpu
            .write_mem(dm + OFF_DISPLAYORIENTATION, &DMDO_0.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    let mut want: Option<(u32, u32)> = None;

    if fields & (DM_PELSWIDTH_FLAG | DM_PELSHEIGHT_FLAG) != 0 {
        let w = read_u32(ctx, dm + OFF_PELSWIDTH)?;
        let h = read_u32(ctx, dm + OFF_PELSHEIGHT)?;
        if (1..=MAX_EDGE).contains(&w) && (1..=MAX_EDGE).contains(&h) {
            want = Some((w, h));
        }
    }
    if want.is_none() && fields & DM_DISPLAYORIENTATION_FLAG != 0 {
        let orientation = read_u32(ctx, dm + OFF_DISPLAYORIENTATION)?;
        let landscape = matches!(orientation, DMDO_90 | DMDO_270);
        let (long_edge, short_edge) = if cur_w >= cur_h {
            (cur_w, cur_h)
        } else {
            (cur_h, cur_w)
        };
        want = Some(if landscape {
            (long_edge, short_edge)
        } else {
            (short_edge, long_edge)
        });
    }

    if let Some((w, h)) = want {
        if (w, h) != (cur_w, cur_h) {
            log::info!("ChangeDisplaySettingsEx: panel {cur_w}x{cur_h} -> {w}x{h}");
            ctx.kernel.framebuffer.resize(w, h);
        }
    }
    // DISP_CHANGE_SUCCESSFUL
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `BOOL SHGetSpecialFolderPath(HWND, LPTSTR path, int csidl, BOOL create)`
///
/// The `csidl` actually matters: Sonic Unleashed asks for
/// `CSIDL_PROGRAM_FILES` (0x26) and then appends
/// `\Gameloft\<title>\data.bar`, so answering `\My Documents` for
/// every folder id sent it looking for its archive in the wrong place
/// and it threw before drawing anything. Return the documented Windows
/// Mobile location for each id instead.
fn sh_get_special_folder_path(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let path = ctx.arg_u32(1)?;
    let csidl = ctx.arg_u32(2)? & 0xff;
    if path == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let folder = match csidl {
        0x00 | 0x10 => "\\Windows\\Desktop", // DESKTOP / DESKTOPDIRECTORY
        0x02 => "\\Windows\\Start Menu\\Programs", // PROGRAMS
        0x08 => "\\Windows\\Recent",         // RECENT
        0x0b => "\\Windows\\Start Menu",     // STARTMENU
        0x14 => "\\Windows\\Fonts",          // FONTS
        0x1a | 0x1c => "\\Application Data", // APPDATA / LOCAL_APPDATA
        0x24 | 0x25 => "\\Windows",          // WINDOWS / SYSTEM
        0x26 => "\\Program Files",           // PROGRAM_FILES
        0x27 => "\\My Documents\\My Pictures", // MYPICTURES
        _ => "\\My Documents",               // PERSONAL and friends
    };
    write_wide_str(ctx.cpu, path, 260, folder)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `HKL GetKeyboardLayout(DWORD)` / `HKL LoadKeyboardLayoutW(...)`.
/// Any non-zero HKL means "US English keyboard" as far as the guest is
/// concerned; returning 0 made SDL treat the keyboard as absent.
fn keyboard_layout(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0409_0409))
}

/// `LANGID GetUserDefaultUILanguage()` — 0x0409 = en-US.
fn user_default_ui_language(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0409))
}

/// `int GetKeyboardLayoutNameW(LPWSTR name)` — the 8-hex-digit KLID of
/// the US layout.
fn get_keyboard_layout_name_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    if dst == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    write_wide_str(ctx.cpu, dst, 9, "00000409")?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_device_caps(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _hdc = ctx.arg_u32(0)?;
    let index = ctx.arg_u32(1)?;
    let (screen_w, screen_h) = screen_dims(ctx);
    let v = match index {
        8 => screen_w,  // HORZRES
        10 => screen_h, // VERTRES
        12 => 16,       // BITSPIXEL
        14 => 1,        // PLANES
        88 => 96,       // LOGPIXELSX
        90 => 96,       // LOGPIXELSY
        _ => 0,
    };
    Ok(DispatchOutcome::ReturnedR0(v))
}

/// `unsigned __rt_urem(unsigned divisor, unsigned dividend)`.
fn rt_urem(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = ctx.arg_u32(0)?;
    let n = ctx.arg_u32(1)?;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    Ok(DispatchOutcome::ReturnedR0(n % d))
}

/// `int __rt_srem(int divisor, int dividend)`.
fn rt_srem(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = ctx.arg_u32(0)? as i32;
    let n = ctx.arg_u32(1)? as i32;
    if d == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    Ok(DispatchOutcome::ReturnedR0(n.wrapping_rem(d) as u32))
}

/// Parse the leading integer of an ASCII string, C `atoi` semantics
/// (skip whitespace, optional sign, stop at the first non-digit).
fn parse_leading_i64(text: &str, radix: u32) -> i64 {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    let mut negative = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        negative = bytes[i] == b'-';
        i += 1;
    }
    let mut radix = radix;
    if (radix == 0 || radix == 16)
        && bytes.len() > i + 1
        && bytes[i] == b'0'
        && (bytes[i + 1] | 0x20) == b'x'
    {
        radix = 16;
        i += 2;
    } else if radix == 0 {
        radix = if bytes.len() > i && bytes[i] == b'0' {
            8
        } else {
            10
        };
    }
    let mut value: i64 = 0;
    while i < bytes.len() {
        let Some(digit) = (bytes[i] as char).to_digit(radix) else {
            break;
        };
        value = value
            .saturating_mul(radix as i64)
            .saturating_add(digit as i64);
        i += 1;
    }
    if negative {
        -value
    } else {
        value
    }
}

/// `long strtol(const char *s, char **end, int base)`.
fn strtol_handler(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let end_out = ctx.arg_u32(1)?;
    let base = ctx.arg_u32(2)?;
    let text = read_cstr_string(ctx, ptr, 0x1000)?;
    let value = parse_leading_i64(&text, base);
    if end_out != 0 {
        // We don't track how many characters were consumed precisely;
        // point past the whole token, which is what callers use to
        // detect "nothing parsed" vs "parsed something".
        let consumed = text.trim_start().len() as u32;
        ctx.cpu.write_mem(
            end_out,
            &(ptr + (text.len() as u32 - consumed) + consumed).to_le_bytes(),
        )?;
    }
    Ok(DispatchOutcome::ReturnedR0(value as i32 as u32))
}

/// US-English locale id (`0x0409`) for the `GetUser*Language` family.
fn en_us_lang_id(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0409))
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

/// `GetSystemInfo(LPSYSTEM_INFO)`.
///
/// `SYSTEM_INFO` has no `dwLength` — the first member is the union
/// `{ DWORD dwOemId; WORD wProcessorArchitecture, wReserved; }`. Laying
/// it out as if a length came first shifts every later field by four
/// bytes, which leaves `dwAllocationGranularity` reading as zero.
/// Allocators size their pools by rounding up to that granularity, so a
/// zero makes the round-up collapse to nothing and the pool base ends up
/// a small integer — the guest then writes through it and faults on a
/// near-null address rather than reporting out of memory.
fn get_system_info(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p != 0 {
        let mut data = [0u8; 36];
        // wProcessorArchitecture = PROCESSOR_ARCHITECTURE_ARM (5),
        // wReserved = 0.
        data[0..2].copy_from_slice(&5u16.to_le_bytes());
        data[2..4].copy_from_slice(&0u16.to_le_bytes());
        data[4..8].copy_from_slice(&4096u32.to_le_bytes()); // dwPageSize
        data[8..12].copy_from_slice(&SLOT_ALIAS_BASE.to_le_bytes()); // lpMinimumApplicationAddress
        data[12..16].copy_from_slice(&DEFAULT_STACK_TOP.to_le_bytes()); // lpMaximumApplicationAddress
        data[16..20].copy_from_slice(&1u32.to_le_bytes()); // dwActiveProcessorMask
        data[20..24].copy_from_slice(&1u32.to_le_bytes()); // dwNumberOfProcessors
                                                           // dwProcessorType = PROCESSOR_STRONGARM.
        data[24..28].copy_from_slice(&2577u32.to_le_bytes());
        // dwAllocationGranularity: WinCE reserves virtual memory in 64K
        // blocks, and guests use this to size their own pools.
        data[28..32].copy_from_slice(&65536u32.to_le_bytes());
        data[32..34].copy_from_slice(&4u16.to_le_bytes()); // wProcessorLevel
        data[34..36].copy_from_slice(&0u16.to_le_bytes()); // wProcessorRevision
        ctx.cpu.write_mem(p, &data)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
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

/// The `f`-suffixed libm entry points. These are *not* the double
/// functions with a narrower name: a single-precision argument occupies
/// one core register under soft-float AAPCS, so `sqrtf` takes its
/// argument in r0 alone where `sqrt` takes r0:r1. Reading a float as
/// half of a double yields a denormal near zero, so the two families
/// have to stay separate.
fn libm_unary_s<F: FnOnce(f32) -> f32>(
    ctx: &mut CallCtx<'_>,
    f: F,
) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(f(read_f32(ctx, 0)?)))
}

fn libm_binary_s<F: FnOnce(f32, f32) -> f32>(
    ctx: &mut CallCtx<'_>,
    f: F,
) -> Result<DispatchOutcome, KernelError> {
    let a = read_f32(ctx, 0)?;
    let b = read_f32(ctx, 1)?;
    Ok(ret_f32(f(a, b)))
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

fn m_floorf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(ret_f32(read_f32(c, 0)?.floor()))
}
fn m_ceil(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_d(c, f64::ceil)
}
fn m_abs_i32(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value = c.arg_u32(0)? as i32;
    Ok(DispatchOutcome::ReturnedR0(value.unsigned_abs()))
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

// ---------- single-precision libm ----------
//
// X-Forge titles work in `float` throughout, so these — `sqrtf` above
// all — are the hot path, not the double versions. Sticky Balls calls
// `sqrtf` roughly 160 times a frame to normalise vectors; answering
// those with the unimplemented-import stub's zero collapses every
// direction it computes.

fn m_sinf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::sin)
}
fn m_cosf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::cos)
}
fn m_tanf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::tan)
}
fn m_asinf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::asin)
}
fn m_acosf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::acos)
}
fn m_atanf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::atan)
}
fn m_sinhf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::sinh)
}
fn m_coshf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::cosh)
}
fn m_tanhf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::tanh)
}
fn m_expf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::exp)
}
fn m_logf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::ln)
}
fn m_log10f(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::log10)
}
fn m_sqrtf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::sqrt)
}
fn m_ceilf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::ceil)
}
fn m_fabsf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_unary_s(c, f32::abs)
}
fn m_atan2f(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_binary_s(c, f32::atan2)
}
fn m_powf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_binary_s(c, f32::powf)
}
fn m_fmodf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_binary_s(c, |a, b| a % b)
}
fn m_hypotf(c: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    libm_binary_s(c, f32::hypot)
}

/// `float ldexpf(float x, int exp)` — x in r0, exponent in r1.
fn m_ldexpf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let x = read_f32(ctx, 0)?;
    let e = ctx.arg_u32(1)? as i32;
    Ok(ret_f32(x * 2.0_f32.powi(e)))
}

/// `float frexpf(float x, int *eptr)`.
fn m_frexpf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let x = read_f32(ctx, 0)?;
    let eptr = ctx.arg_u32(1)?;
    let (mantissa, exp) = if x == 0.0 || !x.is_finite() {
        (x, 0i32)
    } else {
        let bits = x.to_bits();
        let raw_exp = ((bits >> 23) & 0xFF) as i32;
        let e = raw_exp - 126;
        let m = f32::from_bits((bits & !(0xFFu32 << 23)) | (126u32 << 23));
        (m, e)
    };
    if eptr != 0 {
        ctx.cpu.write_mem(eptr, &exp.to_le_bytes())?;
    }
    Ok(ret_f32(mantissa))
}

/// `float modff(float x, float *iptr)`.
fn m_modff(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let x = read_f32(ctx, 0)?;
    let iptr = ctx.arg_u32(1)?;
    let int_part = x.trunc();
    if iptr != 0 {
        ctx.cpu.write_mem(iptr, &int_part.to_le_bytes())?;
    }
    Ok(ret_f32(x - int_part))
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

/// `BOOL IntersectRect(LPRECT lprcDst, const RECT *lprcSrc1, const RECT *lprcSrc2)`
///
/// Solitaire calls this ~900 times per paint pass to clip each card
/// rectangle against the update region. Leaving it unimplemented
/// returns `r0 = 0` ("rectangles do not intersect") *and* never fills
/// `lprcDst`, so the game concludes there is nothing to redraw and
/// skips the blit for every card.
fn intersect_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src1 = ctx.arg_u32(1)?;
    let src2 = ctx.arg_u32(2)?;
    if dst == 0 || src1 == 0 || src2 == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let (l1, t1, r1, b1) = rect_load(ctx, src1)?;
    let (l2, t2, r2, b2) = rect_load(ctx, src2)?;
    let l = l1.max(l2);
    let t = t1.max(t2);
    let r = r1.min(r2);
    let b = b1.min(b2);
    // Win32 zeroes the destination when the intersection is empty.
    if l >= r || t >= b {
        rect_store(ctx, dst, 0, 0, 0, 0)?;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    rect_store(ctx, dst, l, t, r, b)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `BOOL UnionRect(LPRECT lprcDst, const RECT *lprcSrc1, const RECT *lprcSrc2)`
///
/// The bounding box of both sources, ignoring empty ones.
fn union_rect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let src1 = ctx.arg_u32(1)?;
    let src2 = ctx.arg_u32(2)?;
    if dst == 0 || src1 == 0 || src2 == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let a = rect_load(ctx, src1)?;
    let b = rect_load(ctx, src2)?;
    let empty = |(l, t, r, b): (i32, i32, i32, i32)| l >= r || t >= b;
    let out = match (empty(a), empty(b)) {
        (true, true) => {
            rect_store(ctx, dst, 0, 0, 0, 0)?;
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        (true, false) => b,
        (false, true) => a,
        (false, false) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
    };
    rect_store(ctx, dst, out.0, out.1, out.2, out.3)?;
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

fn get_system_default_lang_id(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0409))
}

fn get_thread_locale(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0x0409))
}

fn get_locale_info_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _locale = ctx.arg_u32(0)?;
    let kind = ctx.arg_u32(1)?;
    let dst = ctx.arg_u32(2)?;
    let cap = ctx.arg_u32(3)?;
    let value = match kind {
        0x00000002 => "en-US",
        0x00000004 => "English",
        0x0000000e => "US",
        0x0000000f => "USD",
        _ => "",
    };
    if dst == 0 || cap == 0 {
        return Ok(DispatchOutcome::ReturnedR0(
            (value.encode_utf16().count() + 1) as u32,
        ));
    }
    let written = write_wide_str(ctx.cpu, dst, cap, value)?;
    Ok(DispatchOutcome::ReturnedR0(written + 1))
}

fn get_acp(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1252))
}

fn create_cursor(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0xDEAD_6801))
}

fn destroy_cursor(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
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

/// `UINT RegisterWindowMessageW(LPCWSTR name)`
///
/// The contract is that the same string always maps to the same ID and
/// two different strings never share one, system-wide. A constant made
/// the first half true and the second half false, which matters to any
/// guest that registers more than one: Ball Busters registers eight
/// Gizmondo shell messages — card, USB, call, battery and so on — and
/// routes them by comparing the incoming `msg` against each stored ID in
/// turn, so with every ID equal the first comparison answered for all
/// eight and the game acted on the wrong event.
///
/// Derived from the name by hash rather than from a table, so the ID is
/// stable across runs and across processes without any state to keep.
/// `RegisterWindowMessage` IDs live in the `0xC000..=0xFFFF` string
/// atom range.
fn register_window_message_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name_p = ctx.arg_u32(0)?;
    let name = if name_p == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&read_wstr(ctx, name_p, 260).unwrap_or_default())
    };
    if name.is_empty() {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    // FNV-1a over the name, folded into the atom range. Registered
    // message names are compared case-sensitively by Windows, so hash
    // the string as given.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let id = 0xC000 + (hash % 0x4000);
    log::debug!("RegisterWindowMessageW({name:?}) -> 0x{id:04x}");
    Ok(DispatchOutcome::ReturnedR0(id))
}

fn virtual_query(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let address = ctx.arg_u32(0)?;
    let info = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)?;
    if info == 0 || size < 16 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let (base, region_size, state, protect, kind): (u32, u32, u32, u32, u32) =
        if address < 0x0001_0000 {
            (0, 0x0001_0000, 0x0001_0000, 0, 0)
        } else if address < 0x0010_0000 {
            (0x0001_0000, 0x000f_0000, 0x0000_1000, 0x20, 0x0002_0000)
        } else {
            (address & !0x000f_ffff, 0x0010_0000, 0x0001_0000, 0, 0)
        };
    let mut buf = vec![0u8; size.min(48) as usize];
    buf[0..4].copy_from_slice(&base.to_le_bytes());
    buf[4..8].copy_from_slice(&base.to_le_bytes());
    buf[8..12].copy_from_slice(&protect.to_le_bytes());
    buf[12..16].copy_from_slice(&region_size.to_le_bytes());
    if buf.len() >= 20 {
        buf[16..20].copy_from_slice(&state.to_le_bytes());
    }
    if buf.len() >= 24 {
        buf[20..24].copy_from_slice(&protect.to_le_bytes());
    }
    if buf.len() >= 28 {
        buf[24..28].copy_from_slice(&kind.to_le_bytes());
    }
    ctx.cpu.write_mem(info, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(buf.len() as u32))
}

fn get_proc_address_a(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let module = ctx.arg_u32(0)?;
    let raw_name = ctx.arg_u32(1)?;
    let module = if module == 0 {
        FAKE_MODULE_HANDLE
    } else {
        module
    };
    let name = if raw_name < 0x10000 {
        format!("#{}", raw_name & 0xffff)
    } else {
        read_cstr_string(ctx, raw_name, 256)?
    };
    let address = resolve_dynamic_export(ctx, module, &name);
    log::debug!("GetProcAddressA(0x{module:08x}, {name:?}) -> 0x{address:08x}");
    Ok(DispatchOutcome::ReturnedR0(address))
}

/// `FARPROC GetProcAddressW(HMODULE hModule, LPCWSTR lpProcName)`
/// — we don't have any DLLs the game can dynamically load against,
/// so always report failure (NULL). The game then has to fall back
/// to its statically-imported path.
fn get_proc_address_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let module = ctx.arg_u32(0)?;
    let name_p = ctx.arg_u32(1)?;
    let module = if module == 0 {
        FAKE_MODULE_HANDLE
    } else {
        module
    };
    let name = if name_p < 0x10000 {
        format!("#{}", name_p & 0xffff)
    } else {
        String::from_utf16_lossy(&read_wstr(ctx, name_p, 256).unwrap_or_default())
    };
    let address = resolve_dynamic_export(ctx, module, &name);
    log::debug!("GetProcAddressW(0x{module:08x}, {name:?}) -> 0x{address:08x}");
    Ok(DispatchOutcome::ReturnedR0(address))
}

fn resolve_dynamic_export(ctx: &CallCtx<'_>, module: u32, name: &str) -> u32 {
    if module == 0x1000_0001 && name.eq_ignore_ascii_case("?GXCloseInput@@YAHXZ") {
        return ctx
            .kernel
            .dynamic_exports
            .get(&module)
            .and_then(|exports| exports.get("?GXOpenInput@@YAHXZ").copied())
            .unwrap_or(0);
    }
    ctx.kernel
        .dynamic_exports
        .get(&module)
        .and_then(|exports| exports.get(name).copied())
        .or_else(|| {
            ctx.kernel
                .dynamic_exports
                .get(&module)
                .and_then(|exports| exports.get(&name.to_ascii_lowercase()).copied())
        })
        .or_else(|| {
            if module == FAKE_MODULE_HANDLE {
                ctx.kernel
                    .dynamic_exports
                    .get(&FAKE_MODULE_HANDLE)
                    .and_then(|exports| exports.get(name).copied())
                    .or_else(|| {
                        ctx.kernel
                            .dynamic_exports
                            .get(&FAKE_MODULE_HANDLE)
                            .and_then(|exports| exports.get(&name.to_ascii_lowercase()).copied())
                    })
            } else {
                None
            }
        })
        .or_else(|| {
            ctx.kernel
                .dynamic_exports
                .get(&pocket_kernel::OLE32_MODULE_HANDLE)
                .and_then(|exports| exports.get(name).copied())
                .or_else(|| {
                    ctx.kernel
                        .dynamic_exports
                        .get(&pocket_kernel::OLE32_MODULE_HANDLE)
                        .and_then(|exports| exports.get(&name.to_ascii_lowercase()).copied())
                })
        })
        .unwrap_or_else(|| {
            if name.eq_ignore_ascii_case("InitCommonControlsEx") {
                0xF000_0010
            } else {
                0
            }
        })
}

fn decompress_image_indirect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const E_INVALIDARG: u32 = 0x8007_0057;
    const IMGDECOMP_E_NOIMAGE: u32 = 0x800b_0100;
    let info = ctx.arg_u32(0)?;
    if info == 0 {
        return Ok(DispatchOutcome::ReturnedR0(E_INVALIDARG));
    }
    let header = ctx.cpu.read_mem(info, 60)?;
    let buffer = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let buffer_max = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let buffer_current = u32::from_le_bytes(header[12..16].try_into().unwrap());
    let bitmap_out = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let bit_depth = i32::from_le_bytes(header[24..28].try_into().unwrap());
    let max_width = u32::from_le_bytes(header[40..44].try_into().unwrap());
    let max_height = u32::from_le_bytes(header[44..48].try_into().unwrap());
    if buffer == 0 || bitmap_out == 0 {
        return Ok(DispatchOutcome::ReturnedR0(E_INVALIDARG));
    }
    let available = if buffer_current == 0 {
        buffer_max
    } else {
        buffer_current.min(buffer_max)
    }
    .min(16 * 1024 * 1024);
    if available == 0 {
        return Ok(DispatchOutcome::ReturnedR0(IMGDECOMP_E_NOIMAGE));
    }
    let bytes = ctx.cpu.read_mem(buffer, available)?;
    let decoded = match image::load_from_memory(&bytes) {
        Ok(image) => image,
        Err(error) => {
            log::debug!("DecompressImageIndirect: image decode failed: {error}");
            return Ok(DispatchOutcome::ReturnedR0(IMGDECOMP_E_NOIMAGE));
        }
    };
    let (source_width, source_height) = decoded.dimensions();
    let width = source_width.min(max_width.max(1));
    let height = source_height.min(max_height.max(1));
    let decoded = if width != source_width || height != source_height {
        decoded.resize_exact(width, height, image::imageops::FilterType::Nearest)
    } else {
        decoded
    };
    let rgba = decoded.to_rgba8();
    let handle = ctx.kernel.gdi.create_compatible_bitmap(width, height);
    if let Some(bitmap) = ctx.kernel.gdi.bitmap_mut(handle) {
        bitmap.bpp = if bit_depth > 0 { bit_depth as u16 } else { 16 };
        for y in 0..height {
            for x in 0..width {
                let pixel = rgba.get_pixel(x, y).0;
                let rgb565 = pocket_kernel::framebuffer::pack_rgb565(pixel[0], pixel[1], pixel[2]);
                let offset = ((y * width + x) * 2) as usize;
                bitmap.pixels[offset..offset + 2].copy_from_slice(&rgb565.to_le_bytes());
            }
        }
    }
    ctx.cpu.write_mem(bitmap_out, &handle.to_le_bytes())?;
    log::debug!("DecompressImageIndirect decoded {width}x{height} -> 0x{handle:08x}");
    Ok(DispatchOutcome::ReturnedR0(0))
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

/// Strip the `&` mnemonic markers from a control caption.
///
/// Win32 underlines the following character instead of drawing the
/// ampersand; our 6x8 font has no underline, so `E&xit` simply reads
/// `Exit`. A literal ampersand is escaped as `&&`.
fn strip_mnemonics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
        } else if let Some('&') = chars.clone().next() {
            chars.next();
            out.push('&');
        }
    }
    out
}

/// Create the panel and controls a `DLGTEMPLATE` describes.
///
/// On a device `USER` walks the template and creates every child before
/// `WM_INITDIALOG` is delivered, so the `DialogProc` can already reach
/// them with `GetDlgItem` — which is exactly what Solitaire does, calling
/// `GetDlgItem` ten times and `SetDlgItemTextW` four times for its Time
/// and Score readouts. Ignoring the template left the dialog an empty
/// rectangle and those calls addressing nothing.
fn build_dialog_from_template(ctx: &mut CallCtx<'_>, lp_template: u32) {
    build_dialog_from_template_for(ctx, lp_template, FAKE_DIALOG_HWND, false)
}

/// Materialise a `DLGTEMPLATE` as a panel owned by `owner_hwnd`.
///
/// `modal` marks a `DialogBox*` dialog rather than a `CreateDialog*`
/// one, which changes three things. It is centred on screen: a modal
/// dialog is authored against the screen it was designed for, and unlike
/// Solitaire's docked button strip it has no code of its own to
/// reposition itself afterwards. It is shown whether or not the template
/// says `WS_VISIBLE`. And it gets the Windows CE caption bar, with the
/// OK box that is the only way to dismiss a dialog on that shell.
fn build_dialog_from_template_for(
    ctx: &mut CallCtx<'_>,
    lp_template: u32,
    owner_hwnd: u32,
    modal: bool,
) {
    use crate::dlgtemplate::{class_ordinal, Dlu, ItemClass};
    use pocket_kernel::controls::{ControlClass, DialogPanel};

    /// How much of the blob to read. A template is variable-length and
    /// nothing tells us where it ends, so take a window comfortably
    /// larger than any real dialog and let the parser stop at `cdit`.
    const TEMPLATE_WINDOW: u32 = 4096;

    if lp_template == 0 {
        return;
    }
    // Templates live in the resource section, which may end before the
    // full window — back off until a read succeeds.
    let mut bytes = None;
    for len in [TEMPLATE_WINDOW, 1024, 256, 64] {
        if let Ok(b) = ctx.cpu.read_mem(lp_template, len) {
            bytes = Some(b);
            break;
        }
    }
    let Some(bytes) = bytes else {
        log::debug!("CreateDialogIndirectParamW: template at 0x{lp_template:08x} unreadable");
        return;
    };
    let Some(tmpl) = crate::dlgtemplate::parse(&bytes) else {
        log::debug!("CreateDialogIndirectParamW: template at 0x{lp_template:08x} did not parse");
        return;
    };

    let dlu = Dlu::default();
    let (px, py, pw, ph) = dlu.to_px(tmpl.x as i32, tmpl.y as i32, tmpl.cx as i32, tmpl.cy as i32);
    // A modal dialog always gets a caption, titled from the template or
    // left blank — the OK box is what closes it, so a dialog without one
    // could only ever be dismissed by our own spin limit.
    let caption = modal.then(|| tmpl.title.clone());
    let caption_h = i32::from(modal) * DialogPanel::CAPTION_H;
    // `DialogBox*` draws a frame around its dialog whether or not the
    // template asks for one; a `WS_CHILD` panel takes the style at its
    // word.
    let border = modal || tmpl.style & WS_BORDER != 0;
    let frame = 2 * i32::from(border);
    // The template's cx/cy describe the *client* area; the border and
    // caption sit outside it.
    let (win_w, win_h) = (pw + frame, ph + frame + caption_h);
    // A template is authored against the screen its dialog was designed
    // for. Solitaire's panel is positioned at 267 DLU, past the right
    // edge of a 480 px screen, and the app corrects it immediately by
    // reading the width back with `GetWindowRect` and `SetWindowPos`ing
    // the panel against the edge. Clamp so the panel is on screen even
    // if an app never does that.
    let (screen_w, screen_h) = screen_dims(ctx);
    let (x, y) = if modal {
        (
            ((screen_w as i32 - win_w) / 2).max(0),
            ((screen_h as i32 - win_h) / 2).max(0),
        )
    } else {
        (
            px.min(screen_w as i32 - win_w).max(0),
            py.min(screen_h as i32 - win_h).max(0),
        )
    };

    // Clear a previous incarnation first: this also drops the old panel,
    // so it has to happen before the new one is registered.
    ctx.kernel.controls.destroy_children_of(owner_hwnd);
    ctx.kernel.controls.add_panel(DialogPanel {
        hwnd: owner_hwnd,
        x,
        y,
        w: win_w,
        h: win_h,
        // `DialogBox*` shows its dialog whether or not the template says
        // `WS_VISIBLE`; only a `CreateDialog*` panel starts hidden.
        visible: modal || tmpl.style & WS_VISIBLE != 0,
        border,
        caption,
        ok_pressed: false,
    });

    for item in &tmpl.items {
        let class = match &item.class {
            ItemClass::Ordinal(class_ordinal::BUTTON) => ControlClass::Button,
            ItemClass::Ordinal(class_ordinal::EDIT) => ControlClass::Edit,
            ItemClass::Ordinal(class_ordinal::STATIC) => ControlClass::Static,
            other => {
                // Listboxes, combos, scrollbars and custom classes are
                // not modelled; skipping one loses a control but keeps
                // the rest of the dialog.
                log::debug!("dialog item id={} class {other:?} not modelled", item.id);
                continue;
            }
        };
        let (ix, iy, iw, ih) =
            dlu.to_px(item.x as i32, item.y as i32, item.cx as i32, item.cy as i32);
        let hwnd = ctx.kernel.controls.create(
            owner_hwnd,
            class,
            item.id as u32,
            strip_mnemonics(&item.title),
            item.style,
            ix,
            iy,
            iw,
            ih,
        );
        log::debug!(
            "dialog item id={} {:?} \"{}\" at ({ix},{iy},{iw},{ih}) -> hwnd=0x{hwnd:08x}",
            item.id,
            class,
            strip_mnemonics(&item.title),
        );
    }
    log::debug!(
        "CreateDialogIndirectParamW: panel at ({x},{y},{win_w},{win_h}) with {} controls",
        tmpl.items.len()
    );
    repaint_controls(ctx);
}

/// `CreateDialogIndirectParamW(hInstance, lpTemplate, hWndParent, lpDialogFunc, dwInitParam)`
///
/// Creates the dialog *and* delivers `WM_INITDIALOG` to `lpDialogFunc`
/// before returning, the way the real API does. The detour must not be
/// mistaken for the return value: a caller like Solitaire stores our
/// result in a global and then gates its whole message pump on it
/// (`if (g_hDlg) IsDialogMessageW(...)`), so handing back the
/// `DialogProc`'s `BOOL` — usually `FALSE` — leaves the pump fetching
/// and discarding every message, taps included, forever.
///
/// So use the same round-trip as [`create_window_ex_w`]: stash the
/// interrupted call, point `LR` at our own thunk so this handler
/// re-fires when the callback returns, then hand back the HWND.
fn create_dialog_indirect_param_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Must come first: on re-entry R0..R3 hold the callback's result,
    // not our arguments.
    if let Some(frame) = ctx.kernel.dialog_frame {
        // A nested `CreateDialogIndirectParamW` from inside the
        // `DialogProc` runs on a deeper stack; only SP back at its
        // saved value means our callback has actually returned.
        if ctx.cpu.read_reg(ArmReg::Sp)? >= frame.sp {
            ctx.kernel.dialog_frame = None;
            ctx.cpu.write_reg(ArmReg::Sp, frame.sp)?;
            ctx.cpu.write_reg(ArmReg::R1, frame.args[1])?;
            ctx.cpu.write_reg(ArmReg::R2, frame.args[2])?;
            ctx.cpu.write_reg(ArmReg::R3, frame.args[3])?;
            ctx.cpu.write_reg(ArmReg::Lr, frame.lr)?;
            log::debug!("CreateDialogIndirectParamW -> hwnd=0x{FAKE_DIALOG_HWND:08x}");
            return Ok(DispatchOutcome::ReturnedR0(FAKE_DIALOG_HWND));
        }
    }
    let dialog_proc = ctx.arg_u32(3)?;
    let init_param = ctx.arg_u32(4).unwrap_or(0);
    let lp_template = ctx.arg_u32(1)?;
    build_dialog_from_template(ctx, lp_template);
    if dialog_proc == 0 {
        return Ok(DispatchOutcome::ReturnedR0(FAKE_DIALOG_HWND));
    }
    // Route later SendMessageW / DispatchMessageW for this handle to the
    // DialogProc rather than the frame window's WndProc.
    ctx.kernel
        .window_procs
        .insert(FAKE_DIALOG_HWND, dialog_proc);
    let args = [
        ctx.cpu.read_reg(ArmReg::R0)?,
        ctx.cpu.read_reg(ArmReg::R1)?,
        ctx.cpu.read_reg(ArmReg::R2)?,
        ctx.cpu.read_reg(ArmReg::R3)?,
    ];
    let frame = GuestCallFrame {
        args,
        lr: ctx.cpu.read_reg(ArmReg::Lr)?,
        sp: ctx.cpu.read_reg(ArmReg::Sp)?,
    };
    // DialogProc's four arguments all travel in registers, so the guest
    // stack needs no adjustment here.
    ctx.cpu.write_reg(ArmReg::R0, FAKE_DIALOG_HWND)?;
    ctx.cpu.write_reg(ArmReg::R1, WM_INITDIALOG)?;
    ctx.cpu.write_reg(ArmReg::R2, 0)?;
    ctx.cpu.write_reg(ArmReg::R3, init_param)?;
    ctx.cpu.write_reg(ArmReg::Lr, ctx.thunk.thunk_va)?;
    ctx.kernel.dialog_frame = Some(frame);
    log::debug!("CreateDialogIndirectParamW -> WM_INITDIALOG trampoline at 0x{dialog_proc:08x}");
    Ok(DispatchOutcome::JumpTo(dialog_proc))
}

fn is_window(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let hwnd = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(is_live_hwnd(hwnd) as u32))
}

fn create_semaphore_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let initial_count = ctx.arg_u32(1)?;
    let max_count = ctx.arg_u32(2)?.max(initial_count);
    let handle = 0xDEAD_E301u32.wrapping_add(ctx.kernel.semaphores.len() as u32);
    ctx.kernel.semaphores.insert(
        handle,
        pocket_kernel::SemaphoreObject {
            count: initial_count.min(max_count),
            max_count,
        },
    );
    log::debug!("CreateSemaphore(initial={initial_count}, max={max_count}) -> 0x{handle:08x}");
    Ok(DispatchOutcome::ReturnedR0(handle))
}

/// `BOOL ReleaseSemaphore(HANDLE, LONG lReleaseCount, LPLONG lpPreviousCount)`.
///
/// The CyberSaurus mixer waits on a zero-count semaphore after submitting
/// each wave buffer. Returning success without changing the count makes
/// `WaitForSingleObject` observe an unsignalled object forever; the worker
/// then monopolizes the cooperative CPU and the main thread never reaches
/// its next GAPI frame. Keep the count bounded and report the previous
/// value when the caller asks for it.
fn release_semaphore(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let release_count = ctx.arg_u32(1)?;
    let previous_ptr = ctx.arg_u32(2)?;
    let Some(semaphore) = ctx.kernel.semaphores.get_mut(&handle) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let previous = semaphore.count;
    let next = semaphore.count.saturating_add(release_count);
    if next > semaphore.max_count {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    semaphore.count = next;
    if previous_ptr != 0 {
        ctx.cpu.write_mem(previous_ptr, &previous.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
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

/// `WAVEHDR.dwFlags` bits we care about.
const WHDR_DONE: u32 = 0x1;
const WHDR_INQUEUE: u32 = 0x4;
/// `MM_WOM_DONE` — "a wave-out buffer finished playing", posted to a
/// window or thread queue depending on how `waveOutOpen` was called.
const MM_WOM_DONE: u32 = 0x3BD;
/// `WOM_DONE` — the `uMsg` a `CALLBACK_FUNCTION` `waveOutProc` gets.
const WOM_DONE: u32 = 0x3BD;
/// Guest stack we reserve to call `waveOutProc`: four bytes for
/// `dwParam2`, which is the one argument AAPCS passes on the stack, and
/// four more to keep SP 8-byte aligned.
const WAVE_PROC_STACK_BYTES: u32 = 8;

/// If a `CALLBACK_FUNCTION` notification is due, redirect the CPU into
/// the guest's `waveOutProc` and arrange for it to land back in
/// `thunk_va` when it returns.
///
/// `waveOutProc(HWAVEOUT hwo, UINT uMsg, DWORD_PTR dwInstance,
/// DWORD_PTR dwParam1, DWORD_PTR dwParam2)` takes five arguments, so
/// the fifth goes on the guest stack per AAPCS. The interrupted call's
/// R0..R3 / LR / SP are stashed in [`GuestCallFrame`] and restored
/// on re-entry, which makes the detour invisible to the caller.
fn wave_out_enter_callback(
    ctx: &mut CallCtx<'_>,
    thunk_va: u32,
) -> Result<Option<DispatchOutcome>, KernelError> {
    if let Some(frame) = ctx.kernel.wave_out.function_frame {
        // Zuma's `waveOutProc` calls `Sleep(0)` before it acknowledges a
        // stop request, and `sleep` is one of the handlers that delivers
        // these callbacks — so a re-entry is not proof the callback
        // returned. Use the same test as `create_window_ex_w`: a nested
        // call runs on a deeper stack, and only SP back at its saved
        // value (less the stack argument we reserved) means
        // `waveOutProc` has actually popped its frame. Restoring on a
        // nested call would move SP out from under the running callback.
        if ctx.cpu.read_reg(ArmReg::Sp)? < frame.sp.wrapping_sub(WAVE_PROC_STACK_BYTES) {
            return Ok(None);
        }
        // `waveOutProc` just returned — undo the detour.
        ctx.kernel.wave_out.function_frame = None;
        ctx.cpu.write_reg(ArmReg::Sp, frame.sp)?;
        ctx.cpu.write_reg(ArmReg::R0, frame.args[0])?;
        ctx.cpu.write_reg(ArmReg::R1, frame.args[1])?;
        ctx.cpu.write_reg(ArmReg::R2, frame.args[2])?;
        ctx.cpu.write_reg(ArmReg::R3, frame.args[3])?;
        ctx.cpu.write_reg(ArmReg::Lr, frame.lr)?;
        return Ok(None);
    }
    if ctx.kernel.wave_out.callback_kind != WaveCallbackKind::Function {
        return Ok(None);
    }
    let Some(hdr) = ctx.kernel.wave_out.function_done.pop_front() else {
        return Ok(None);
    };
    let proc_va = ctx.kernel.wave_out.callback_target;
    if proc_va == 0 {
        return Ok(None);
    }
    let args = [
        ctx.cpu.read_reg(ArmReg::R0)?,
        ctx.cpu.read_reg(ArmReg::R1)?,
        ctx.cpu.read_reg(ArmReg::R2)?,
        ctx.cpu.read_reg(ArmReg::R3)?,
    ];
    let lr = ctx.cpu.read_reg(ArmReg::Lr)?;
    let sp = ctx.cpu.read_reg(ArmReg::Sp)?;
    let new_sp = sp.wrapping_sub(WAVE_PROC_STACK_BYTES);
    ctx.cpu.write_mem(new_sp, &0u32.to_le_bytes())?;
    ctx.cpu.write_reg(ArmReg::Sp, new_sp)?;
    ctx.cpu.write_reg(ArmReg::R0, ctx.kernel.wave_out.handle)?;
    ctx.cpu.write_reg(ArmReg::R1, WOM_DONE)?;
    ctx.cpu
        .write_reg(ArmReg::R2, ctx.kernel.wave_out.instance)?;
    ctx.cpu.write_reg(ArmReg::R3, hdr)?;
    ctx.cpu.write_reg(ArmReg::Lr, thunk_va)?;
    ctx.kernel.wave_out.function_frame = Some(pocket_kernel::GuestCallFrame { args, lr, sp });
    log::trace!("waveOutProc(0x{proc_va:08x}) for hdr=0x{hdr:08x}");
    Ok(Some(DispatchOutcome::JumpTo(proc_va)))
}

/// `MMRESULT waveOutGetNumDevs(void)` — number of host wave-out
/// devices. We always claim one so games that probe before opening
/// don't fall back to a "no audio" code path.
fn wave_out_get_num_devs(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn wave_out_get_dev_caps(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let caps = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)?;
    if caps != 0 && size != 0 {
        let mut buf = vec![0u8; (size as usize).min(84)];
        let name: Vec<u16> = "PocketHLE Wave Output".encode_utf16().collect();
        for (index, ch) in name.into_iter().take(31).enumerate() {
            let offset = 8 + index * 2;
            if offset + 2 <= buf.len() {
                buf[offset..offset + 2].copy_from_slice(&ch.to_le_bytes());
            }
        }
        if buf.len() >= 76 {
            buf[72..76].copy_from_slice(&1u32.to_le_bytes());
        }
        if buf.len() >= 78 {
            buf[76..78].copy_from_slice(&1u16.to_le_bytes());
        }
        ctx.cpu.write_mem(caps, &buf)?;
    }
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
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
    let callback = ctx.arg_u32(3)?;
    let instance = ctx.arg_u32(4)?;
    let flags = ctx.arg_u32(5)?;

    let requested_format = if pwfx != 0 {
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
        log::debug!(
            "waveOutOpen tag={format_tag} {sample_rate} Hz / {channels} ch / {bits}-bit, flags=0x{flags:08x}"
        );
        Some(fmt)
    } else {
        None
    };

    // WAVE_FORMAT_QUERY = 0x1: don't actually open, just verify.
    if flags & 0x1 == 0 {
        let already_open = ctx.kernel.wave_out.handle != 0;
        // The notification mode lives in the top half of `fdwOpen`.
        // Games double-buffer audio and only submit the next chunk
        // once they are told the previous one drained, so getting this
        // right is the difference between two buffers of music and a
        // continuous soundtrack.
        let kind = match flags & 0x0007_0000 {
            0x0001_0000 => WaveCallbackKind::Window,
            0x0002_0000 => WaveCallbackKind::Thread,
            0x0003_0000 => WaveCallbackKind::Function,
            // CALLBACK_EVENT. `WaitForSingleObject` already returns
            // WAIT_OBJECT_0 immediately here, so the guest never
            // blocks and needs no extra signalling from us.
            0x0005_0000 => WaveCallbackKind::Event,
            _ => WaveCallbackKind::None,
        };
        log::debug!("waveOutOpen notification: {kind:?} target=0x{callback:08x}");
        if !already_open {
            ctx.kernel.wave_out = pocket_kernel::WaveOutState {
                handle: FAKE_HWAVEOUT,
                callback_kind: kind,
                callback_target: callback,
                instance,
                owner_thread: ctx.kernel.current_thread,
                ..Default::default()
            };
            ctx.kernel.audio.flush();
        } else {
            ctx.kernel.wave_out.handle = FAKE_HWAVEOUT;
            ctx.kernel.wave_out.callback_kind = kind;
            ctx.kernel.wave_out.callback_target = callback;
            ctx.kernel.wave_out.instance = instance;
            ctx.kernel.wave_out.owner_thread = ctx.kernel.current_thread;
        }
        ctx.kernel.audio.start();
        if let Some(fmt) = requested_format {
            ctx.kernel.audio.set_guest_format(fmt);
        }
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
    retire_all_wave_buffers(ctx)?;
    ctx.kernel.wave_out = pocket_kernel::WaveOutState::default();
    ctx.kernel.audio.stop();
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutReset(HWAVEOUT)` — discard any queued samples.
fn wave_out_reset(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    // MSDN: `waveOutReset` marks every pending buffer as done and
    // notifies the caller for each, exactly as if they had played.
    retire_all_wave_buffers(ctx)?;
    ctx.kernel.audio.flush();
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutPause(HWAVEOUT)` — stop advancing the playback
/// cursor. Buffers stay queued; nothing is reported as finished until
/// `waveOutRestart`.
fn wave_out_pause(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    ctx.kernel.wave_out.paused = true;
    ctx.kernel.audio.set_paused(true);
    log::debug!("waveOutPause");
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutRestart(HWAVEOUT)` — resume after a pause.
fn wave_out_restart(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    ctx.kernel.wave_out.paused = false;
    ctx.kernel.audio.set_paused(false);
    log::debug!("waveOutRestart");
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// `MMRESULT waveOutGetPosition(HWAVEOUT, LPMMTIME pmmt, UINT cbmmt)`
///
/// `MMTIME` is `{ UINT wType; union { DWORD ms; DWORD sample;
/// DWORD cb; ... } u; }`. Games use it to pace streaming, so report
/// the real playback cursor instead of a constant zero. If we cannot
/// honour the requested unit we answer in bytes and say so in
/// `wType`, which is what the API expects.
fn wave_out_get_position(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _h = ctx.arg_u32(0)?;
    let pmmt = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)?;
    if pmmt == 0 || size < 8 {
        return Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR));
    }
    let want = u32::from_le_bytes(ctx.cpu.read_mem(pmmt, 4)?.try_into().unwrap_or([0; 4]));
    let fmt = ctx.kernel.wave_out_format;
    let channels = u64::from(fmt.channels.max(1));
    let rate = u64::from(fmt.sample_rate.max(1));
    let played = ctx.kernel.audio.playback_cursor();
    // TIME_MS = 1, TIME_SAMPLES = 2, TIME_BYTES = 4.
    let (ty, value) = match want {
        1 => (1u32, played.saturating_mul(1000) / (rate * channels)),
        2 => (2u32, played / channels),
        _ => (
            4u32,
            played.saturating_mul(u64::from(fmt.bits_per_sample.max(8)) / 8),
        ),
    };
    ctx.cpu.write_mem(pmmt, &ty.to_le_bytes())?;
    ctx.cpu.write_mem(pmmt + 4, &(value as u32).to_le_bytes())?;
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
    // The buffer is now queued, not finished. It is retired once the
    // playback cursor reaches the end of the samples we just pushed —
    // see `service_wave_out`.
    flags = (flags & !WHDR_DONE) | WHDR_INQUEUE;
    ctx.cpu.write_mem(p_hdr + 16, &flags.to_le_bytes())?;
    let end_cursor = ctx.kernel.audio.written_samples();
    log::debug!(
        "waveOutWrite hdr=0x{p_hdr:08x} bytes={n_bytes} end_cursor={end_cursor} paused={}",
        ctx.kernel.wave_out.paused
    );
    ctx.kernel
        .wave_out
        .pending
        .push_back(pocket_kernel::PendingWaveBuffer {
            hdr: p_hdr,
            end_cursor,
        });
    service_wave_out(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(MMSYSERR_NOERROR))
}

/// Mark `hdr` as played and tell the guest about it the way it asked
/// at `waveOutOpen` time.
fn retire_wave_buffer(ctx: &mut CallCtx<'_>, hdr: u32) -> Result<(), KernelError> {
    log::debug!(
        "retire hdr=0x{hdr:08x} kind={:?}",
        ctx.kernel.wave_out.callback_kind
    );
    if hdr != 0 {
        let cur = ctx.cpu.read_mem(hdr + 16, 4)?;
        let flags = u32::from_le_bytes([cur[0], cur[1], cur[2], cur[3]]);
        let flags = (flags & !WHDR_INQUEUE) | WHDR_DONE;
        ctx.cpu.write_mem(hdr + 16, &flags.to_le_bytes())?;
    }
    let target = ctx.kernel.wave_out.callback_target;
    match ctx.kernel.wave_out.callback_kind {
        WaveCallbackKind::Window => {
            ctx.kernel
                .posted_messages
                .push_back((target, MM_WOM_DONE, FAKE_HWAVEOUT, hdr));
        }
        WaveCallbackKind::Thread => {
            // CALLBACK_THREAD notifications belong to the thread ID
            // passed to waveOutOpen, not to the window queue. Routing
            // these to the worker queue is what lets a streaming mixer
            // submit the next buffer instead of stopping after its
            // initial pre-roll.
            if let Some(thread) = ctx
                .kernel
                .threads
                .iter_mut()
                .find(|thread| thread.id == target && !thread.finished)
            {
                if thread.messages.len() < 256 {
                    thread.messages.push_back((MM_WOM_DONE, FAKE_HWAVEOUT, hdr));
                }
            } else if ctx.kernel.posted_messages.len() < 256 {
                ctx.kernel
                    .posted_messages
                    .push_back((0, MM_WOM_DONE, FAKE_HWAVEOUT, hdr));
            }
        }
        WaveCallbackKind::Function => {
            ctx.kernel.wave_out.function_done.push_back(hdr);
        }
        WaveCallbackKind::Event => {
            // `CALLBACK_EVENT` means the driver sets this event every
            // time a buffer drains, and that is the only back-pressure
            // the guest's mixer thread has. Toy Golf's thread waits on
            // it and refills whatever is done; leaving the event alone
            // makes the wait fall through, so the thread refills as
            // fast as the CPU allows, never yields, and the main thread
            // never draws another frame.
            if let Some(event) = ctx.kernel.events.get_mut(&target) {
                event.signalled = true;
            }
        }
        WaveCallbackKind::None => {}
    }
    Ok(())
}

/// Retire every buffer whose samples the playback cursor has passed.
/// Called from `waveOutWrite`, from the message pump, and from `Sleep`
/// — the three places a guest gives us the CPU back, and so the three
/// places a game waiting on `MM_WOM_DONE` or a `waveOutProc` call can
/// notice one.
fn service_wave_out(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    if !ctx.kernel.wave_out.pending.is_empty() {
        log::trace!(
            "service_wave_out pending={} cursor={} paused={}",
            ctx.kernel.wave_out.pending.len(),
            ctx.kernel.audio.playback_cursor(),
            ctx.kernel.wave_out.paused
        );
    }
    if ctx.kernel.wave_out.pending.is_empty() || ctx.kernel.wave_out.paused {
        return Ok(());
    }
    let cursor = ctx.kernel.audio.playback_cursor();
    while let Some(front) = ctx.kernel.wave_out.pending.front().copied() {
        if front.end_cursor > cursor {
            break;
        }
        ctx.kernel.wave_out.pending.pop_front();
        retire_wave_buffer(ctx, front.hdr)?;
    }
    Ok(())
}

/// `waveOutReset` / `waveOutClose` semantics: everything still queued
/// is reported as finished right away.
fn retire_all_wave_buffers(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    while let Some(front) = ctx.kernel.wave_out.pending.pop_front() {
        retire_wave_buffer(ctx, front.hdr)?;
    }
    Ok(())
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
    // 16 bpp under BI_RGB is RGB555; only BI_BITFIELDS with a 0x07E0
    // green mask means our native RGB565. See `create_dib_section`.
    let rgb555 = bi_bpp == 16
        && if bi_compression == 3 {
            let masks = ctx.cpu.read_mem(p_bmi + bi_size, 12).unwrap_or_default();
            if masks.len() == 12 {
                let green = u32::from_le_bytes([masks[4], masks[5], masks[6], masks[7]]);
                // 0x07E0 is the 6-bit green of RGB565; 0x03E0 is the
                // 5-bit green of RGB555. Anything else we treat as
                // 565, our native layout.
                green == 0x0000_03E0
            } else {
                false
            }
        } else {
            true
        };
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
                16 => {
                    let p = u16::from_le_bytes([
                        raw[row_off + x as usize * 2],
                        raw[row_off + x as usize * 2 + 1],
                    ]);
                    if rgb555 {
                        pocket_kernel::gdi::rgb555_to_rgb565(p)
                    } else {
                        p
                    }
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
    Ok(DispatchOutcome::ReturnedR0(sys_color(idx)))
}

/// The system palette, as a `COLORREF`.
fn sys_color(idx: i32) -> u32 {
    match idx {
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
    }
}

/// Resolve a `WNDCLASS::hbrBackground` to the `COLORREF` the client
/// area should be erased with, or `None` for "the app paints it all".
///
/// The field is allowed to be a real `HBRUSH` or the integer
/// `COLOR_xxx + 1`, and the two are told apart by magnitude — every
/// handle we hand out is a `0xDEAD_xxxx` value, far above the couple
/// dozen system colour indices.
///
/// HelloWorld hardcodes a third form, `0x4000_0006`: the shorthand
/// with bit 30 set as a "this is not a pointer" tag. Its own
/// `RegisterClassW` never calls `GetStockObject`, so the constant is
/// baked in at compile time by whatever SDK header it was built
/// against. Masking the tag off gives `COLOR_WINDOW + 1` — white,
/// which is what the reference screenshot shows.
fn class_background_color(ctx: &CallCtx<'_>, hbr: u32) -> Option<u32> {
    /// The largest `COLOR_xxx` index any Pocket PC SDK defines, with
    /// room to spare; the `+ 1` shorthand can reach one past it.
    const MAX_SYS_COLOR: u32 = 32;
    match hbr {
        0 => None,
        1..=MAX_SYS_COLOR => Some(sys_color(hbr as i32 - 1)),
        _ => {
            if let Some(b) = ctx.kernel.gdi.brush(hbr) {
                return Some(b.color);
            }
            // Not a handle we issued — try the tagged shorthand.
            let untagged = hbr & !0x4000_0000;
            (untagged != hbr && (1..=MAX_SYS_COLOR).contains(&untagged))
                .then(|| sys_color(untagged as i32 - 1))
        }
    }
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

fn transparent_image(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let dx = ctx.arg_u32(1)? as i32;
    let dy = ctx.arg_u32(2)? as i32;
    let dw = ctx.arg_u32(3)? as i32;
    let dh = ctx.arg_u32(4)? as i32;
    let src = ctx.arg_u32(5)?;
    let sx = ctx.arg_u32(6)? as i32;
    let sy = ctx.arg_u32(7)? as i32;
    let sw = ctx.arg_u32(8)? as i32;
    let sh = ctx.arg_u32(9)? as i32;
    let _color = ctx.arg_u32(10).unwrap_or(0);
    let _flags = ctx.arg_u32(11).unwrap_or(0);
    bit_blt_inner(
        ctx,
        dst,
        dx,
        dy,
        dw.min(sw),
        dh.min(sh),
        src,
        sx,
        sy,
        pocket_kernel::gdi::rop3::SRCCOPY,
    )?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

// ---------- directory enumeration ----------

/// First handle handed out by [`find_first_file_w`].
const FIND_HANDLE_BASE: u32 = 0xDEAD_F000;

/// Offset of `cFileName` inside Windows CE's `WIN32_FIND_DATAW`.
///
/// CE's layout is *not* the desktop one: it stores a single `dwOID` at
/// offset 36 where desktop Win32 has `dwReserved0` + `dwReserved1`, so
/// the name starts at 40 rather than 44.
const FIND_DATA_NAME_OFF: u32 = 40;
/// `sizeof(WIN32_FIND_DATAW)` on CE: 40 + MAX_PATH (260) wide chars.
const FIND_DATA_BYTES: usize = 40 + 260 * 2;

/// `*` / `?` glob match, case-insensitive, as `FindFirstFile` does it.
///
/// `*.*` is special-cased to "everything" to match Win32 semantics
/// (it also matches names without a dot).
fn wildcard_match(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() || pattern == "*" || pattern == "*.*" {
        return true;
    }
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let n: Vec<char> = name.to_ascii_lowercase().chars().collect();
    // Iterative glob with backtracking — no recursion, no allocation
    // per candidate beyond the two char vectors above.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut star_ni) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            star_ni = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Split `\Path\To\*.pdb` into (`\Path\To`, `*.pdb`).
fn split_search_pattern(path: &str) -> (String, String) {
    match path.rfind(['\\', '/']) {
        Some(idx) => (path[..idx].to_string(), path[idx + 1..].to_string()),
        None => (".".to_string(), path.to_string()),
    }
}

fn write_find_data(
    ctx: &mut CallCtx<'_>,
    out: u32,
    entry: &(String, u64, bool),
) -> Result<(), KernelError> {
    if out == 0 {
        return Ok(());
    }
    let (name, size, is_dir) = entry;
    let mut buf = vec![0u8; FIND_DATA_BYTES];
    // FILE_ATTRIBUTE_DIRECTORY (0x10) or FILE_ATTRIBUTE_NORMAL (0x80).
    let attrs: u32 = if *is_dir { 0x10 } else { 0x80 };
    buf[0..4].copy_from_slice(&attrs.to_le_bytes());
    buf[28..32].copy_from_slice(&((*size >> 32) as u32).to_le_bytes());
    buf[32..36].copy_from_slice(&(*size as u32).to_le_bytes());
    let name_off = FIND_DATA_NAME_OFF as usize;
    for (index, unit) in name.encode_utf16().take(259).enumerate() {
        let at = name_off + index * 2;
        buf[at..at + 2].copy_from_slice(&unit.to_le_bytes());
    }
    ctx.cpu.write_mem(out, &buf)?;
    Ok(())
}

/// `HANDLE FindFirstFileW(LPCWSTR lpFileName, LPWIN32_FIND_DATAW lpFindFileData)`
///
/// Astraware's Bejeweled enumerates `*.pdb` next to its executable to
/// find its resource databases; with the old stub returning
/// `INVALID_HANDLE_VALUE` it wrote a settings file and exited before
/// ever drawing a frame.
fn find_first_file_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pattern_p = ctx.arg_u32(0)?;
    let out = ctx.arg_u32(1)?;
    let pattern = String::from_utf16_lossy(&read_wstr(ctx, pattern_p, 520)?);
    if !pattern.contains('*') && !pattern.contains('?') {
        if let Some(host) = ctx.kernel.vfs.resolve(&pattern) {
            if let Ok(meta) = std::fs::metadata(&host) {
                let name = pattern
                    .rsplit(['\\', '/'])
                    .next()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(".")
                    .to_string();
                let entry = (name, meta.len(), meta.is_dir());
                write_find_data(ctx, out, &entry)?;
                let handle = FIND_HANDLE_BASE.saturating_add(ctx.kernel.next_find_handle);
                ctx.kernel.next_find_handle = ctx.kernel.next_find_handle.wrapping_add(1);
                ctx.kernel
                    .find_handles
                    .insert(handle, std::collections::VecDeque::new());
                return Ok(DispatchOutcome::ReturnedR0(handle));
            }
        }
    }
    let (dir, mask) = split_search_pattern(&pattern);
    let entries = ctx.kernel.vfs.list_dir(&dir).unwrap_or_default();
    let mut matches: std::collections::VecDeque<(String, u64, bool)> = entries
        .into_iter()
        .filter(|(name, _, _)| wildcard_match(&mask, name))
        .collect();
    let Some(first) = matches.pop_front() else {
        log::debug!("FindFirstFileW({pattern:?}) -> INVALID_HANDLE_VALUE");
        return Ok(DispatchOutcome::ReturnedR0(INVALID_HANDLE_VALUE));
    };
    write_find_data(ctx, out, &first)?;
    let handle = FIND_HANDLE_BASE.saturating_add(ctx.kernel.next_find_handle);
    ctx.kernel.next_find_handle = ctx.kernel.next_find_handle.wrapping_add(1);
    log::debug!(
        "FindFirstFileW({pattern:?}) -> 0x{handle:08x} first={:?} ({} more)",
        first.0,
        matches.len()
    );
    ctx.kernel.find_handles.insert(handle, matches);
    Ok(DispatchOutcome::ReturnedR0(handle))
}

/// `BOOL FindNextFileW(HANDLE hFindFile, LPWIN32_FIND_DATAW lpFindFileData)`
fn find_next_file_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let out = ctx.arg_u32(1)?;
    let next = ctx
        .kernel
        .find_handles
        .get_mut(&handle)
        .and_then(|entries| entries.pop_front());
    match next {
        Some(entry) => {
            write_find_data(ctx, out, &entry)?;
            Ok(DispatchOutcome::ReturnedR0(1))
        }
        None => Ok(DispatchOutcome::ReturnedR0(0)),
    }
}

/// `BOOL FindClose(HANDLE hFindFile)`
fn find_close(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    ctx.kernel.find_handles.remove(&handle);
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `HANDLE FindFirstChangeNotificationW(LPCWSTR path, BOOL subtree,
///                                      DWORD filter)`
///
/// A directory watch, answered with a manual-reset event that exists but
/// never fires. Nothing in this HLE mutates a mounted host directory
/// behind the guest's back, so "no change has happened yet" is the honest
/// answer for the whole run — and it is also the answer that means *the
/// card is still in the slot*.
///
/// That distinction is the point. Ball Busters runs from the Gizmondo's
/// SD card, so during startup it watches the directory it was launched
/// from and then waits on that handle alongside its quit event. Returning
/// a failure here — which is what an unimplemented call did, by leaving
/// R0 at 0 — reads to the game as "the directory I live on cannot even be
/// watched", and it puts up its "SD card removed. Please reinsert" screen
/// instead of the main menu.
///
/// A watch on a directory that genuinely is not there still fails, so a
/// missing `--rom-dir` mount keeps meaning what it should.
fn find_first_change_notification_w(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const INVALID_HANDLE_VALUE: u32 = 0xFFFF_FFFF;
    let path_p = ctx.arg_u32(0)?;
    let subtree = ctx.arg_u32(1)? != 0;
    let filter = ctx.arg_u32(2)?;
    let path = if path_p == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&read_wstr(ctx, path_p, 260).unwrap_or_default())
    };
    if path.is_empty() || ctx.kernel.vfs.list_dir(&path).is_none() {
        log::debug!(
            "FindFirstChangeNotificationW({path:?}, subtree={subtree}, filter=0x{filter:08x}) \
             -> INVALID_HANDLE_VALUE (no such directory)"
        );
        return Ok(DispatchOutcome::ReturnedR0(INVALID_HANDLE_VALUE));
    }
    let handle = 0xDEAD_E001u32.wrapping_add(ctx.kernel.events.len() as u32);
    ctx.kernel.events.insert(
        handle,
        pocket_kernel::EventObject {
            manual_reset: true,
            signalled: false,
        },
    );
    log::debug!(
        "FindFirstChangeNotificationW({path:?}, subtree={subtree}, filter=0x{filter:08x}) \
         -> 0x{handle:08x} (never signalled)"
    );
    Ok(DispatchOutcome::ReturnedR0(handle))
}

/// `BOOL FindNextChangeNotification(HANDLE)` — re-arms the watch. The
/// event never fired, so there is nothing to clear; succeeding is what
/// keeps a guest that re-arms in a loop from treating it as an error.
fn find_next_change_notification(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let known = ctx.kernel.events.contains_key(&handle);
    if let Some(event) = ctx.kernel.events.get_mut(&handle) {
        event.signalled = false;
    }
    Ok(DispatchOutcome::ReturnedR0(u32::from(known)))
}

/// `BOOL FindCloseChangeNotification(HANDLE)`.
fn find_close_change_notification(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(u32::from(
        ctx.kernel.events.remove(&handle).is_some(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    use pocket_kernel::{
        vfs::{Access, Vfs},
        Heap, KernelState, Thunk,
    };
    use pocket_pe::ImportBinding;

    fn fresh_kernel() -> KernelState {
        use pocket_kernel::audio::{AudioEngine, GuestFormat};
        use pocket_kernel::{Framebuffer, GdiState};

        KernelState {
            heap: Heap::new(0x5000_0000, 0x10000),
            vfs: Vfs::new(),
            registry: pocket_kernel::registry::Registry::new(),
            find_handles: std::collections::HashMap::new(),
            next_find_handle: 0,
            module_path: "\\Program Files\\Game\\Game.exe".to_string(),
            pending_startup: std::collections::VecDeque::new(),
            framebuffer: Framebuffer::default(),
            gdi: GdiState::new(),
            resources: vec![],
            image_base: 0,
            image_size: 0,
            image_entry: 0,
            dynamic_exports: std::collections::HashMap::new(),
            next_module_handle: 0x1000_0001,
            modules: Vec::new(),
            next_module_base: pocket_kernel::MODULE_REGION_BASE,
            module_search_dirs: Vec::new(),
            fb_mapped: false,
            direct_fb_frames: 0,
            gx_readback_scratch: Vec::new(),
            mem_op_scratch: Vec::new(),
            mem_op_scratch_b: Vec::new(),
            bit_blt_src_scratch: Vec::new(),
            dib_sync_scratch: Vec::new(),
            dib_decode_scratch: Vec::new(),
            gx_last_pushed_counter: 0,
            gx_guest_signature: None,
            gles_strings: std::collections::HashMap::new(),
            synthetic_message_count: 0,
            synthetic_message_budget: 240,
            wnd_proc: 0,
            window_class_procs: std::collections::HashMap::new(),
            window_background: None,
            pending_create: None,
            window_procs: std::collections::HashMap::new(),
            window_userdata: std::collections::HashMap::new(),
            window_classes: std::collections::HashMap::new(),
            window_user_data: 0,
            synthetic_timer_id: 0,
            synthetic_timer_interval_ms: 16,
            synthetic_timer_next_ms: 0,
            synthetic_paint_next_ms: 0,
            synthetic_create_sent: false,
            synthetic_size_sent: false,
            create_frame: None,
            create_stage: pocket_kernel::CreateStage::Idle,
            dialog_frame: None,
            status_bar: None,
            message_frame: None,
            controls: Default::default(),
            pending_input: std::collections::VecDeque::new(),
            gapi_keys_queried: false,
            pending_message: None,
            threads: Vec::new(),
            events: Default::default(),
            semaphores: Default::default(),
            current_thread: 0,
            pressed_keys: [false; 256],
            should_stop: false,
            tls_slots_used: 0,
            vector_iter_stack: Vec::new(),
            qsort_frames: std::collections::HashMap::new(),
            strtok_pos: 0,
            security_cookie: 0,
            audio: AudioEngine::new(),
            wave_out_format: GuestFormat::default(),
            wave_out: Default::default(),
            hss: Default::default(),
            posted_messages: Default::default(),
            msg_queues: std::collections::HashMap::new(),
            next_msg_queue_handle: 0xDEAD_E500,
            menus: std::collections::HashMap::new(),
            next_menu_handle: 0xDEAD_2000,
            sub_menus: std::collections::HashMap::new(),
            modal: None,
            message_box_spins: 0,
            modal_dialog: None,
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

    /// Drive the `qsort` state machine to completion with a host-side
    /// stand-in for the guest comparator: every `JumpTo` is a request to
    /// compare the two `u32`s at R0 / R1, and the answer goes back in R0
    /// exactly the way a guest `compar` would leave it.
    #[test]
    fn qsort_sorts_through_guest_comparator_round_trips() {
        const BASE: u32 = 0x1000;
        const COMPAR: u32 = 0x4000;
        let input: [u32; 8] = [5, 1, 5, 9, 2, 6, 5, 3];

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        for (i, v) in input.iter().enumerate() {
            cpu.write_mem(BASE + (i as u32) * 4, &v.to_le_bytes())
                .unwrap();
        }
        cpu.write_reg(ArmReg::R0, BASE).unwrap();
        cpu.write_reg(ArmReg::R1, input.len() as u32).unwrap();
        cpu.write_reg(ArmReg::R2, 4).unwrap();
        cpu.write_reg(ArmReg::R3, COMPAR).unwrap();
        cpu.write_reg(ArmReg::Lr, 0xDEAD_BEEF).unwrap();

        let t = dummy_thunk();
        let mut comparisons = 0;
        loop {
            let outcome = {
                let mut c = CallCtx {
                    cpu: &mut cpu,
                    thunk: &t,
                    kernel: &mut kernel,
                };
                qsort(&mut c).unwrap()
            };
            match outcome {
                DispatchOutcome::JumpTo(target) => {
                    assert_eq!(target, COMPAR);
                    comparisons += 1;
                    assert!(comparisons < 1000, "state machine is not terminating");
                    let a = cpu.read_reg(ArmReg::R0).unwrap();
                    let b = cpu.read_reg(ArmReg::R1).unwrap();
                    let va = u32::from_le_bytes(cpu.read_mem(a, 4).unwrap().try_into().unwrap());
                    let vb = u32::from_le_bytes(cpu.read_mem(b, 4).unwrap().try_into().unwrap());
                    let r = match va.cmp(&vb) {
                        std::cmp::Ordering::Less => -1i32,
                        std::cmp::Ordering::Equal => 0,
                        std::cmp::Ordering::Greater => 1,
                    };
                    cpu.write_reg(ArmReg::R0, r as u32).unwrap();
                }
                DispatchOutcome::ReturnedR0(_) => break,
                other => panic!("unexpected outcome {other:?}"),
            }
        }

        let mut expected = input;
        expected.sort();
        let got: Vec<u32> = (0..input.len() as u32)
            .map(|i| u32::from_le_bytes(cpu.read_mem(BASE + i * 4, 4).unwrap().try_into().unwrap()))
            .collect();
        assert_eq!(got, expected.to_vec());
        // LR has to be restored so the guest resumes at qsort's caller.
        assert_eq!(cpu.read_reg(ArmReg::Lr).unwrap(), 0xDEAD_BEEF);
        // Binary insertion sort: at most ceil(log2(i+1)) comparisons per
        // element, so nowhere near the O(n^2) a linear scan would need.
        assert!(comparisons <= 8 * 3, "too many round-trips: {comparisons}");
        assert!(kernel.qsort_frames.is_empty());
    }

    /// Zuma's `Board` ctor runs `??_L` over two sub-objects, and each
    /// sub-object's ctor runs `??_L` again over its own 200-element
    /// array. The iterator state therefore has to nest: the inner run
    /// must not be mistaken for "the outer element's ctor returned",
    /// which used to corrupt the outer loop and hand the guest a bogus
    /// `this`.
    #[test]
    fn vector_ctor_iterator_nests() {
        const OUTER_BEGIN: u32 = 0x2000;
        const OUTER_STRIDE: u32 = 0x100;
        const INNER_STRIDE: u32 = 8;
        const OUTER_CTOR: u32 = 0x4000;
        const INNER_CTOR: u32 = 0x5000;
        const OUTER_SP: u32 = 0x7800;
        const INNER_SP: u32 = 0x7700;
        const OUTER_LR: u32 = 0xDEAD_BEEF;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x7000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();

        let begin_outer = |cpu: &mut StubCpu| {
            cpu.write_reg(ArmReg::R0, OUTER_BEGIN).unwrap();
            cpu.write_reg(ArmReg::R1, OUTER_STRIDE).unwrap();
            cpu.write_reg(ArmReg::R2, 2).unwrap();
            cpu.write_reg(ArmReg::R3, OUTER_CTOR).unwrap();
            cpu.write_reg(ArmReg::Sp, OUTER_SP).unwrap();
            cpu.write_reg(ArmReg::Lr, OUTER_LR).unwrap();
        };
        begin_outer(&mut cpu);

        let t = dummy_thunk();
        let mut outer_elems: Vec<u32> = Vec::new();
        let mut inner_elems: Vec<u32> = Vec::new();
        // `true` while the inner iterator owns the round-trips.
        let mut in_inner = false;
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps < 100, "state machine is not terminating");
            let outcome = {
                let mut c = CallCtx {
                    cpu: &mut cpu,
                    thunk: &t,
                    kernel: &mut kernel,
                };
                vector_ctor_iterator(&mut c).unwrap()
            };
            match outcome {
                DispatchOutcome::JumpTo(OUTER_CTOR) => {
                    let elem = cpu.read_reg(ArmReg::R0).unwrap();
                    outer_elems.push(elem);
                    // The element ctor immediately starts its own
                    // nested iteration, one frame deeper on the stack.
                    cpu.write_reg(ArmReg::R0, elem + 0x10).unwrap();
                    cpu.write_reg(ArmReg::R1, INNER_STRIDE).unwrap();
                    cpu.write_reg(ArmReg::R2, 3).unwrap();
                    cpu.write_reg(ArmReg::R3, INNER_CTOR).unwrap();
                    cpu.write_reg(ArmReg::Sp, INNER_SP).unwrap();
                    cpu.write_reg(ArmReg::Lr, 0x4100).unwrap();
                    in_inner = true;
                }
                DispatchOutcome::JumpTo(INNER_CTOR) => {
                    inner_elems.push(cpu.read_reg(ArmReg::R0).unwrap());
                    // Inner element ctor returns: same SP as its call.
                    cpu.write_reg(ArmReg::Sp, INNER_SP).unwrap();
                }
                DispatchOutcome::JumpTo(other) => panic!("unexpected target 0x{other:08x}"),
                DispatchOutcome::ReturnedR0(_) if in_inner => {
                    // Inner iteration done, so the outer element's ctor
                    // now returns to the outer iterator's thunk.
                    assert_eq!(cpu.read_reg(ArmReg::Lr).unwrap(), 0x4100);
                    in_inner = false;
                    cpu.write_reg(ArmReg::Sp, OUTER_SP).unwrap();
                }
                DispatchOutcome::ReturnedR0(_) => break,
                other => panic!("unexpected outcome {other:?}"),
            }
        }

        assert_eq!(outer_elems, vec![OUTER_BEGIN, OUTER_BEGIN + OUTER_STRIDE]);
        assert_eq!(
            inner_elems,
            vec![
                OUTER_BEGIN + 0x10,
                OUTER_BEGIN + 0x18,
                OUTER_BEGIN + 0x20,
                OUTER_BEGIN + OUTER_STRIDE + 0x10,
                OUTER_BEGIN + OUTER_STRIDE + 0x18,
                OUTER_BEGIN + OUTER_STRIDE + 0x20,
            ]
        );
        assert_eq!(cpu.read_reg(ArmReg::Lr).unwrap(), OUTER_LR);
        assert!(kernel.vector_iter_stack.is_empty());
    }

    #[test]
    fn qsort_with_fewer_than_two_elements_is_a_noop() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        cpu.write_reg(ArmReg::R1, 1).unwrap();
        cpu.write_reg(ArmReg::R2, 4).unwrap();
        cpu.write_reg(ArmReg::R3, 0x4000).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(qsort(&mut c).unwrap(), DispatchOutcome::ReturnedR0(0));
        assert!(kernel.qsort_frames.is_empty());
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

    /// CeGCC's `crt3.c` calls `_fcloseall` on its way into `ExitProcess`.
    /// It has to flush and drop every open stream and answer with how many
    /// it closed.
    #[test]
    fn fcloseall_closes_every_open_handle() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        kernel.vfs.mount("\\App\\", dir.path());
        let a = kernel
            .vfs
            .open("\\App\\a.txt", Access::Read, false)
            .unwrap();
        let b = kernel
            .vfs
            .open("\\App\\b.txt", Access::Read, false)
            .unwrap();
        assert!(kernel.vfs.is_open(a) && kernel.vfs.is_open(b));

        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(
            crt_fcloseall(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0(2)
        );
        assert!(!kernel.vfs.is_open(a));
        assert!(!kernel.vfs.is_open(b));
    }

    /// `_fpreset` is `void` and has nothing to reset in our FPU model, but
    /// it must be *registered*: it is the first call CeGCC startup makes,
    /// and an unimplemented stub there warns on every mingw32ce binary.
    #[test]
    fn fpreset_is_a_no_op_that_returns_cleanly() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(crt_fpreset(&mut c).unwrap(), DispatchOutcome::ReturnedR0(0));
    }

    #[test]
    fn remove_directory_w_succeeds_without_deleting_host_dir() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("scratch")).unwrap();
        kernel.vfs.mount("\\App\\", dir.path());
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let s: Vec<u8> = "\\App\\scratch\0"
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
        assert_eq!(
            remove_directory_w(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0(1)
        );
        assert!(dir.path().join("scratch").is_dir());
    }

    #[test]
    fn remove_directory_w_null_pointer_fails() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.write_reg(ArmReg::R0, 0).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(
            remove_directory_w(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0(0)
        );
    }

    #[test]
    fn file_mutation_handlers_write_through_vfs() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let dir = tempfile::tempdir().unwrap();
        kernel.vfs.mount("\\Save\\", dir.path());
        cpu.map_region(0x1000, 0x2000, Prot::READ | Prot::WRITE)
            .unwrap();
        let wide = |text: &str| -> Vec<u8> {
            text.encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect()
        };
        cpu.write_mem(0x1000, &wide("\\Save\\old.dat")).unwrap();
        cpu.write_mem(0x1400, &wide("\\Save\\new.dat")).unwrap();
        cpu.write_mem(0x1800, b"data").unwrap();
        cpu.write_mem(0x1c00, &wide("\\Save\\nested")).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1c00).unwrap();
        cpu.write_mem(0x1e00, &wide("\\Save\\nested\\old.dat"))
            .unwrap();
        cpu.write_mem(0x2200, &wide("\\Save\\nested\\new.dat"))
            .unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(
            create_directory_w(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0(1)
        );
        let handle = c
            .kernel
            .vfs
            .open("\\Save\\nested\\old.dat", Access::Write, true)
            .unwrap();
        c.cpu.write_reg(ArmReg::R0, handle).unwrap();
        c.cpu.write_reg(ArmReg::R1, 0x1800).unwrap();
        c.cpu.write_reg(ArmReg::R2, 4).unwrap();
        c.cpu.write_reg(ArmReg::R3, 0).unwrap();
        assert_eq!(write_file(&mut c).unwrap(), DispatchOutcome::ReturnedR0(1));
        c.cpu.write_reg(ArmReg::R0, 0x1e00).unwrap();
        c.cpu.write_reg(ArmReg::R1, 0x2200).unwrap();
        assert_eq!(move_file_w(&mut c).unwrap(), DispatchOutcome::ReturnedR0(1));
        c.cpu.write_reg(ArmReg::R0, 0x2200).unwrap();
        assert_eq!(
            delete_file_w(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0(1)
        );
        assert!(!dir.path().join("nested").join("new.dat").exists());
    }

    #[test]
    fn get_store_information_writes_store_size_and_free_size() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(
            get_store_information(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(cpu.read_u32_le(0x1000).unwrap(), 64 * 1024 * 1024);
        assert_eq!(cpu.read_u32_le(0x1004).unwrap(), 48 * 1024 * 1024);
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
    fn class_background_resolves_every_hbrbackground_form() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let green = kernel.gdi.create_solid_brush(0x0000_8000);
        let t = dummy_thunk();
        let c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };

        // NULL means "the app paints the whole client area".
        assert_eq!(class_background_color(&c, 0), None);
        // A real brush handle keeps its own colour (Solitaire's felt).
        assert_eq!(class_background_color(&c, green), Some(0x0000_8000));
        // COLOR_WINDOW + 1, the documented shorthand.
        assert_eq!(class_background_color(&c, 6), Some(0x00FF_FFFF));
        // The same shorthand with HelloWorld's bit-30 tag.
        assert_eq!(class_background_color(&c, 0x4000_0006), Some(0x00FF_FFFF));
        // A handle we never issued and cannot read as a shorthand.
        assert_eq!(class_background_color(&c, 0x1234_5678), None);
    }

    #[test]
    fn def_window_proc_erases_background_and_reports_it() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        kernel.window_background = Some(0x00FF_FFFF);
        cpu.write_reg(ArmReg::Sp, 0x4000).unwrap();
        // WM_ERASEBKGND must answer "handled" so DefWindowProc's caller
        // doesn't erase a second time.
        cpu.write_reg(ArmReg::R1, WM_ERASEBKGND).unwrap();
        let t = dummy_thunk();
        {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            assert_eq!(
                def_window_proc_w(&mut c).unwrap(),
                DispatchOutcome::ReturnedR0(1)
            );
        }
        // White, little-endian RGB565.
        assert_eq!(&kernel.framebuffer.pixels[0..2], &[0xff, 0xff]);

        // A guest holding the GAPI framebuffer owns every pixel, so the
        // erase has to stay out of its way.
        kernel.framebuffer.pixels[0..2].copy_from_slice(&[0x00, 0x00]);
        kernel.fb_mapped = true;
        cpu.write_reg(ArmReg::R1, WM_PAINT).unwrap();
        {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            assert_eq!(
                def_window_proc_w(&mut c).unwrap(),
                DispatchOutcome::ReturnedR0(0)
            );
        }
        assert_eq!(&kernel.framebuffer.pixels[0..2], &[0x00, 0x00]);
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

    #[test]
    fn set_bitmap_bits_copies_guest_rgb565_into_a_ddb() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x30000, Prot::READ | Prot::WRITE)
            .unwrap();
        let bits_va = 0x2000;
        let mut source = vec![0u8; 240 * 320 * 2];
        source[0..2].copy_from_slice(&0xF800u16.to_le_bytes());
        source[2..4].copy_from_slice(&0x07E0u16.to_le_bytes());
        cpu.write_mem(bits_va, &source).unwrap();

        let t = thunk_at(0x7000_0190);
        cpu.write_reg(ArmReg::R0, 240).unwrap();
        cpu.write_reg(ArmReg::R1, 320).unwrap();
        cpu.write_reg(ArmReg::R2, 1).unwrap();
        cpu.write_reg(ArmReg::R3, 16).unwrap();
        cpu.map_region(0x8000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_reg(ArmReg::Sp, 0x8800).unwrap();
        cpu.write_mem(0x8800, &bits_va.to_le_bytes()).unwrap();
        let bitmap = {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            match create_bitmap(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(handle) => handle,
                other => panic!("CreateBitmap returned {other:?}"),
            }
        };

        cpu.write_reg(ArmReg::R0, bitmap).unwrap();
        cpu.write_reg(ArmReg::R1, source.len() as u32).unwrap();
        cpu.write_reg(ArmReg::R2, bits_va).unwrap();
        let copied = {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            match set_bitmap_bits(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(count) => count,
                other => panic!("SetBitmapBits returned {other:?}"),
            }
        };

        assert_eq!(copied, source.len() as u32);
        let bitmap = kernel.gdi.bitmap(bitmap).expect("bitmap must exist");
        assert_eq!(&bitmap.pixels[..4], &source[..4]);
    }

    #[test]
    fn is_window_visible_follows_show_window_for_controls() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let t = dummy_thunk();

        // A hidden control reports invisible, and a visible one reports
        // visible — never a blanket "exists means shown".
        let hidden = kernel.controls.create(
            FAKE_MODAL_HWND,
            ControlClass::Static,
            1004,
            String::new(),
            0,
            0,
            0,
            77,
            17,
        );
        let shown = kernel.controls.create(
            FAKE_MODAL_HWND,
            ControlClass::Static,
            1006,
            String::new(),
            WS_VISIBLE,
            0,
            0,
            77,
            17,
        );

        let call = |cpu: &mut StubCpu, kernel: &mut KernelState, hwnd: u32| -> DispatchOutcome {
            cpu.write_reg(ArmReg::R0, hwnd).unwrap();
            is_window_visible(&mut CallCtx {
                cpu,
                thunk: &t,
                kernel,
            })
            .unwrap()
        };
        assert_eq!(
            call(&mut cpu, &mut kernel, hidden),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, shown),
            DispatchOutcome::ReturnedR0(1)
        );

        // Showing the hidden one flips the answer, as Solitaire's status
        // updater expects.
        cpu.write_reg(ArmReg::R0, hidden).unwrap();
        cpu.write_reg(ArmReg::R1, 5).unwrap(); // SW_SHOW
        show_window(&mut CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        })
        .unwrap();
        assert_eq!(
            call(&mut cpu, &mut kernel, hidden),
            DispatchOutcome::ReturnedR0(1)
        );
    }

    /// Drive `sscanf` with `input`/`fmt` and up to four out-pointers,
    /// all pointing into a scratch page. Returns the raw `int` result.
    fn run_scanf(
        cpu: &mut StubCpu,
        kernel: &mut KernelState,
        input: &str,
        fmt: &str,
        outs: &[u32],
    ) -> i32 {
        const SRC: u32 = 0x1000;
        const FMT: u32 = 0x1400;
        cpu.write_mem(SRC, format!("{input}\0").as_bytes()).unwrap();
        cpu.write_mem(FMT, format!("{fmt}\0").as_bytes()).unwrap();
        cpu.write_reg(ArmReg::R0, SRC).unwrap();
        cpu.write_reg(ArmReg::R1, FMT).unwrap();
        for (i, o) in outs.iter().enumerate() {
            match i {
                0 => cpu.write_reg(ArmReg::R2, *o).unwrap(),
                1 => cpu.write_reg(ArmReg::R3, *o).unwrap(),
                n => {
                    // Args 4+ live on the stack at the ABI offset.
                    let sp = cpu.read_reg(ArmReg::Sp).unwrap();
                    let off = sp + cpu.stack_arg_offset() + (n as u32 - 2) * 4;
                    cpu.write_mem(off, &o.to_le_bytes()).unwrap();
                }
            }
        }
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu,
            thunk: &t,
            kernel,
        };
        match sscanf(&mut c).unwrap() {
            DispatchOutcome::ReturnedR0(v) => v as i32,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    fn scanf_cpu() -> StubCpu {
        let mut cpu = StubCpu::new();
        cpu.map_region(0x1000, 0x4000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.map_region(0x8000, 0x2000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_reg(ArmReg::Sp, 0x9000).unwrap();
        cpu
    }

    #[test]
    fn sscanf_parses_several_integers_and_counts_them() {
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let (a, b, c) = (0x2000, 0x2004, 0x2008);
        let n = run_scanf(&mut cpu, &mut kernel, "12 -34 56", "%d %d %d", &[a, b, c]);
        assert_eq!(n, 3);
        assert_eq!(cpu.read_u32_le(a).unwrap(), 12);
        assert_eq!(cpu.read_u32_le(b).unwrap() as i32, -34);
        assert_eq!(cpu.read_u32_le(c).unwrap(), 56);
    }

    #[test]
    fn sscanf_stops_at_the_first_mismatch_and_reports_the_partial_count() {
        // The classic caller idiom is `if (sscanf(..) != 3) error;`, so
        // the count has to reflect only what was really assigned.
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let (a, b) = (0x2000, 0x2004);
        cpu.write_mem(b, &0xdead_beefu32.to_le_bytes()).unwrap();
        let n = run_scanf(&mut cpu, &mut kernel, "7 oops", "%d %d", &[a, b]);
        assert_eq!(n, 1);
        assert_eq!(cpu.read_u32_le(a).unwrap(), 7);
        assert_eq!(
            cpu.read_u32_le(b).unwrap(),
            0xdead_beef,
            "second out must be untouched"
        );
    }

    #[test]
    fn sscanf_reads_a_bare_float_as_four_bytes_not_eight() {
        // `%f` writing 8 bytes would clobber whatever field follows.
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let a = 0x2000;
        cpu.write_mem(a + 4, &0xa5a5_a5a5u32.to_le_bytes()).unwrap();
        let n = run_scanf(&mut cpu, &mut kernel, "1.5", "%f", &[a]);
        assert_eq!(n, 1);
        assert_eq!(f32::from_bits(cpu.read_u32_le(a).unwrap()), 1.5);
        assert_eq!(
            cpu.read_u32_le(a + 4).unwrap(),
            0xa5a5_a5a5,
            "%f must not write 8 bytes"
        );
    }

    #[test]
    fn sscanf_reads_a_long_float_as_a_double() {
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let a = 0x2000;
        let n = run_scanf(&mut cpu, &mut kernel, "-2.25e2", "%lf", &[a]);
        assert_eq!(n, 1);
        let lo = cpu.read_u32_le(a).unwrap() as u64;
        let hi = cpu.read_u32_le(a + 4).unwrap() as u64;
        assert_eq!(f64::from_bits((hi << 32) | lo), -225.0);
    }

    #[test]
    fn sscanf_reads_hex_with_and_without_the_prefix() {
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let (a, b) = (0x2000, 0x2004);
        let n = run_scanf(&mut cpu, &mut kernel, "0x1f ff", "%x %x", &[a, b]);
        assert_eq!(n, 2);
        assert_eq!(cpu.read_u32_le(a).unwrap(), 0x1f);
        assert_eq!(cpu.read_u32_le(b).unwrap(), 0xff);
    }

    #[test]
    fn sscanf_copies_a_string_with_a_terminator_and_honours_width() {
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let a = 0x2000;
        let n = run_scanf(&mut cpu, &mut kernel, "texture_wall diffuse", "%4s", &[a]);
        assert_eq!(n, 1);
        let raw = cpu.read_mem(a, 5).unwrap();
        assert_eq!(&raw, b"text\0");
    }

    #[test]
    fn sscanf_suppressed_fields_consume_input_without_consuming_an_argument() {
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let a = 0x2000;
        // Only one out-pointer is supplied even though there are two
        // conversions — the `%*d` must not eat it.
        let n = run_scanf(&mut cpu, &mut kernel, "10 20", "%*d %d", &[a]);
        assert_eq!(n, 1);
        assert_eq!(cpu.read_u32_le(a).unwrap(), 20);
    }

    #[test]
    fn sscanf_matches_literals_and_a_scanset() {
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let (a, b) = (0x2000, 0x2100);
        let n = run_scanf(
            &mut cpu,
            &mut kernel,
            "id=42;name=rifle",
            "id=%d;name=%[a-z]",
            &[a, b],
        );
        assert_eq!(n, 2);
        assert_eq!(cpu.read_u32_le(a).unwrap(), 42);
        let raw = cpu.read_mem(b, 6).unwrap();
        assert_eq!(&raw, b"rifle\0");
    }

    #[test]
    fn sscanf_returns_negative_one_when_the_input_ends_before_any_conversion() {
        // Callers distinguish this EOF case from "matched nothing".
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let a = 0x2000;
        let n = run_scanf(&mut cpu, &mut kernel, "   ", "%d", &[a]);
        assert_eq!(n, -1);
    }

    #[test]
    fn sscanf_percent_n_reports_offset_without_counting_as_a_conversion() {
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let (a, b) = (0x2000, 0x2004);
        let n = run_scanf(&mut cpu, &mut kernel, "123abc", "%d%n", &[a, b]);
        assert_eq!(n, 1, "%n must not be counted");
        assert_eq!(cpu.read_u32_le(a).unwrap(), 123);
        assert_eq!(cpu.read_u32_le(b).unwrap(), 3);
    }

    #[test]
    fn sscanf_short_modifier_writes_only_two_bytes() {
        let mut cpu = scanf_cpu();
        let mut kernel = fresh_kernel();
        let a = 0x2000;
        cpu.write_mem(a, &0u32.to_le_bytes()).unwrap();
        cpu.write_mem(a + 2, &0xbeefu16.to_le_bytes()).unwrap();
        let n = run_scanf(&mut cpu, &mut kernel, "300", "%hd", &[a]);
        assert_eq!(n, 1);
        let lo = u16::from_le_bytes(cpu.read_mem(a, 2).unwrap().try_into().unwrap());
        let hi = u16::from_le_bytes(cpu.read_mem(a + 2, 2).unwrap().try_into().unwrap());
        assert_eq!(lo, 300);
        assert_eq!(hi, 0xbeef, "%hd must not write past two bytes");
    }

    /// A `CallCtx` over a stub CPU with one mapped scratch page, a
    /// distinct thunk per call site, and the argument registers set.
    ///
    /// `resolve_handler` memoizes by `thunk_va` including negative
    /// results, so every thunk a test builds needs its own address.
    fn thunk_at(va: u32) -> Thunk {
        let mut t = dummy_thunk();
        t.thunk_va = va;
        t
    }

    /// `CreateThread` without `CREATE_SUSPENDED` must still return to
    /// the creator rather than entering the new thread's entry point.
    ///
    /// On the device the creator keeps its timeslice and finishes
    /// initialising the object the new thread is about to read. Toy
    /// Golf's audio thread dereferences the mixer its creator has not
    /// stored yet, so entering immediately faulted on a NULL
    /// `[r5,#0x38]`. Reverting the park-always path fails this.
    #[test]
    fn create_thread_returns_to_its_creator_instead_of_entering_the_thread() {
        const ENTRY: u32 = 0x0009_d000;
        const PARAM: u32 = 0xFEED_0001;
        const CALLER: u32 = 0x0006_1234;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.write_reg(ArmReg::R0, 0).unwrap();
        cpu.write_reg(ArmReg::R1, 0x4000).unwrap();
        cpu.write_reg(ArmReg::R2, ENTRY).unwrap();
        cpu.write_reg(ArmReg::R3, PARAM).unwrap();
        cpu.write_reg(ArmReg::Lr, CALLER).unwrap();
        // Argument 4 (the creation flags) is passed on the stack.
        cpu.map_region(0x6000_0000 - 0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let sp = 0x6000_0000u32 - 0x100;
        cpu.write_reg(ArmReg::Sp, sp).unwrap();
        cpu.write_mem(sp, &0u32.to_le_bytes()).unwrap();

        let t = thunk_at(0x7000_0100);
        let outcome = {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            create_thread(&mut c).unwrap()
        };

        // The creator gets a handle back and keeps running: no JumpTo
        // into the thread body.
        let handle = match outcome {
            DispatchOutcome::ReturnedR0(h) => h,
            other => panic!("CreateThread must return to its creator, got {other:?}"),
        };
        assert_ne!(handle, 0, "a valid entry point must produce a handle");

        let thread = kernel
            .threads
            .last()
            .expect("the thread object has to exist");
        assert_eq!(thread.handle, handle);
        // Parked at its entry point with the parameter in R0, eligible
        // to run at the creator's next blocking call.
        assert!(thread.worker_saved, "the thread must be parked, not live");
        assert!(thread.started, "a non-suspended thread is runnable");
        assert_eq!(thread.worker_regs[15], ENTRY);
        assert_eq!(thread.worker_regs[0], PARAM);
        assert_eq!(kernel.current_thread, 0, "the creator still owns the CPU");
    }

    /// `CREATE_SUSPENDED` differs only in whether the parked thread is
    /// eligible to run before `ResumeThread`.
    #[test]
    fn a_suspended_thread_is_parked_but_not_yet_runnable() {
        const ENTRY: u32 = 0x0009_e000;
        const CREATE_SUSPENDED: u32 = 0x4;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.write_reg(ArmReg::R2, ENTRY).unwrap();
        cpu.write_reg(ArmReg::R4, CREATE_SUSPENDED).unwrap();
        // Argument 4 is the creation flags; the stub reads it off the
        // stack, so place it there too.
        cpu.map_region(0x6000_0000 - 0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let sp = 0x6000_0000u32 - 0x100;
        cpu.write_reg(ArmReg::Sp, sp).unwrap();
        cpu.write_mem(sp, &CREATE_SUSPENDED.to_le_bytes()).unwrap();

        let t = thunk_at(0x7000_0120);
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        create_thread(&mut c).unwrap();

        let thread = kernel.threads.last().unwrap();
        assert!(thread.worker_saved);
        assert_eq!(thread.worker_regs[15], ENTRY);
        assert!(
            !thread.started,
            "CREATE_SUSPENDED must wait for ResumeThread"
        );
    }

    /// `CALLBACK_EVENT` means the driver sets an event every time a
    /// buffer drains, and that event is the only back-pressure the
    /// guest's mixer thread has.
    ///
    /// Toy Golf's thread waits on it and refills whatever is done.
    /// Treating the callback as a no-op made the wait fall through, so
    /// the thread refilled as fast as the CPU allowed, never yielded,
    /// and the main thread never drew another frame.
    #[test]
    fn a_drained_buffer_signals_its_callback_event() {
        const HDR: u32 = 0x1000;
        const EVENT: u32 = 0xDEAD_E001;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_mem(HDR + 16, &WHDR_INQUEUE.to_le_bytes())
            .unwrap();
        kernel.events.insert(
            EVENT,
            pocket_kernel::EventObject {
                manual_reset: false,
                signalled: false,
            },
        );
        kernel.wave_out.callback_kind = WaveCallbackKind::Event;
        kernel.wave_out.callback_target = EVENT;

        let t = thunk_at(0x7000_0140);
        {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            retire_wave_buffer(&mut c, HDR).unwrap();
        }

        assert!(
            kernel.events[&EVENT].signalled,
            "retiring a buffer must set the CALLBACK_EVENT event"
        );
        // The header still has to be marked done and dequeued.
        let flags = u32::from_le_bytes(cpu.read_mem(HDR + 16, 4).unwrap().try_into().unwrap());
        assert_eq!(flags & WHDR_DONE, WHDR_DONE);
        assert_eq!(flags & WHDR_INQUEUE, 0);
    }

    /// `WaitForSingleObject(handle, INFINITE)` from a worker is a
    /// scheduling point, and the retried call must see its arguments
    /// intact.
    ///
    /// This HLE runs one guest thread at a time, so whoever would
    /// signal the object needs the CPU first. Answering `WAIT_OBJECT_0`
    /// is what kept Toy Golf's mixer thread spinning; answering
    /// `WAIT_TIMEOUT` is a lie for `INFINITE`. Rewriting R0 on the way
    /// out would make the retried call wait on handle 0.
    #[test]
    fn an_infinite_wait_from_a_worker_yields_and_keeps_its_handle() {
        const EVENT: u32 = 0xDEAD_E001;
        const MAIN_RESUME: u32 = 0x0006_5000;
        const INFINITE: u32 = 0xFFFF_FFFF;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        kernel.events.insert(
            EVENT,
            pocket_kernel::EventObject {
                manual_reset: false,
                signalled: false,
            },
        );

        // One worker, currently on the CPU, with the main thread's
        // context saved in it.
        let mut saved = [0u32; 17];
        saved[15] = MAIN_RESUME;
        let mut thread = GuestThread::new(
            0x0009_d000,
            0,
            0x6200_0000,
            0x1000,
            THREAD_EXIT_TRAMPOLINE_BASE,
            MAIN_RESUME,
            0xDEAD_7C00,
            saved,
        );
        thread.started = true;
        kernel.threads.push(thread);
        kernel.current_thread = 1;

        cpu.write_reg(ArmReg::R0, EVENT).unwrap();
        cpu.write_reg(ArmReg::R1, INFINITE).unwrap();
        cpu.write_reg(ArmReg::Lr, 0x0009_d100).unwrap();

        let t = thunk_at(0x7000_0160);
        let outcome = {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            wait_for_single_object(&mut c).unwrap()
        };

        // Control goes back to the main thread, not on to the worker's LR.
        assert_eq!(outcome, DispatchOutcome::JumpTo(MAIN_RESUME));
        assert_eq!(kernel.current_thread, 0);
        let worker = &kernel.threads[0];
        assert!(worker.worker_saved, "the worker has to be parked");
        // Re-runs the same thunk, with the handle it was waiting on.
        assert_eq!(worker.worker_regs[15], t.thunk_va);
        assert_eq!(
            worker.worker_regs[0], EVENT,
            "an infinite wait must keep its handle argument"
        );
    }

    /// `InvalidateRect` asks for a *future* `WM_PAINT`; it changes no
    /// pixels, so it must not advance `frame_counter`.
    ///
    /// The hosts treat a counter change as "a new frame is ready".
    /// Toy Golf invalidates from its idle loop — ~100 000 calls in a
    /// couple of million slices — so the CLI's dump hook spent its
    /// whole `--max-frames` budget on identical black frames captured
    /// before the game had drawn anything, and stopped the run while
    /// the framebuffer went on to receive a full 320x240 image.
    #[test]
    fn invalidate_rect_does_not_count_as_a_rendered_frame() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let before = kernel.framebuffer.frame_counter;

        let t = thunk_at(0x7000_0180);
        for _ in 0..1000 {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            assert_eq!(
                invalidate_rect(&mut c).unwrap(),
                DispatchOutcome::ReturnedR0(1)
            );
        }

        assert_eq!(
            kernel.framebuffer.frame_counter, before,
            "invalidating a region must not fabricate frames"
        );
    }

    /// Call a handler with `args` already in r0.. and return r0.
    fn call_r0(
        handler: fn(&mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError>,
        args: &[u32],
    ) -> u32 {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        for (i, v) in args.iter().enumerate() {
            let reg = match i {
                0 => ArmReg::R0,
                1 => ArmReg::R1,
                2 => ArmReg::R2,
                3 => ArmReg::R3,
                _ => panic!("stack arguments are not modelled here"),
            };
            cpu.write_reg(reg, *v).unwrap();
        }
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        match handler(&mut c).unwrap() {
            DispatchOutcome::ReturnedR0(v) => v,
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    /// Write a NUL-terminated ASCII string into guest memory and hand
    /// back a handler-callable context.
    fn call_r0_with_strings(
        handler: fn(&mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError>,
        strings: &[&str],
    ) -> (u32, Vec<u32>) {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let mut addrs = Vec::new();
        let mut at = 0x1000u32;
        for s in strings {
            cpu.write_mem(at, s.as_bytes()).unwrap();
            cpu.write_mem(at + s.len() as u32, &[0]).unwrap();
            addrs.push(at);
            at += s.len() as u32 + 1;
        }
        for (i, v) in addrs.iter().enumerate() {
            let reg = match i {
                0 => ArmReg::R0,
                1 => ArmReg::R1,
                _ => panic!("only two string arguments are modelled"),
            };
            cpu.write_reg(reg, *v).unwrap();
        }
        let t = dummy_thunk();
        let r0 = {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            match handler(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(v) => v,
                other => panic!("unexpected outcome {other:?}"),
            }
        };
        (r0, addrs)
    }

    /// `sqrtf` takes a *float* in r0, not the low half of a double.
    ///
    /// X-Forge normalises with `sqrtf`, so before this existed the
    /// unimplemented-import stub answered ~160 calls a frame with zero
    /// and every direction the engine derived collapsed. Reading r0 as
    /// half a double would be just as wrong — 4.0f's bit pattern is a
    /// denormal when interpreted that way, so the result would be ~0
    /// rather than 2.
    #[test]
    fn single_precision_libm_reads_one_register_per_argument() {
        assert_eq!(
            f32::from_bits(call_r0(m_sqrtf, &[4.0f32.to_bits()])),
            2.0,
            "sqrtf(4.0) must be 2.0"
        );
        assert_eq!(
            f32::from_bits(call_r0(m_fabsf, &[(-3.5f32).to_bits()])),
            3.5
        );
        assert_eq!(f32::from_bits(call_r0(m_ceilf, &[1.25f32.to_bits()])), 2.0);
        // Binary operands sit in r0 and r1, one register each — a
        // double-precision reader would take r1 as the high word of the
        // first argument and find no second argument at all.
        assert_eq!(
            f32::from_bits(call_r0(m_powf, &[2.0f32.to_bits(), 10.0f32.to_bits()])),
            1024.0
        );
        assert_eq!(
            f32::from_bits(call_r0(m_fmodf, &[7.5f32.to_bits(), 2.0f32.to_bits()])),
            1.5
        );
        assert_eq!(
            f32::from_bits(call_r0(m_hypotf, &[3.0f32.to_bits(), 4.0f32.to_bits()])),
            5.0
        );
        let atan2 = f32::from_bits(call_r0(m_atan2f, &[1.0f32.to_bits(), 1.0f32.to_bits()]));
        assert!(
            (atan2 - std::f32::consts::FRAC_PI_4).abs() < 1e-6,
            "{atan2}"
        );
        // And the double family must keep its r0:r1 pair intact.
        let bits = 4.0f64.to_bits();
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.write_reg(ArmReg::R0, bits as u32).unwrap();
        cpu.write_reg(ArmReg::R1, (bits >> 32) as u32).unwrap();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(
            m_sqrt(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0R1(2.0f64.to_bits() as u32, (2.0f64.to_bits() >> 32) as u32)
        );
    }

    /// `frexpf` / `modff` split their result across the return value and
    /// an out-pointer, and the pointer is r1 for a float — not r2 as it
    /// is for the double versions.
    #[test]
    fn single_precision_splitters_write_through_r1() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let t = dummy_thunk();

        cpu.write_reg(ArmReg::R0, 12.0f32.to_bits()).unwrap();
        cpu.write_reg(ArmReg::R1, 0x1000).unwrap();
        let out = {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            m_frexpf(&mut c).unwrap()
        };
        // 12.0 == 0.75 * 2^4
        assert_eq!(out, DispatchOutcome::ReturnedR0(0.75f32.to_bits()));
        let e = i32::from_le_bytes(cpu.read_mem(0x1000, 4).unwrap().try_into().unwrap());
        assert_eq!(e, 4);

        cpu.write_reg(ArmReg::R0, (-2.25f32).to_bits()).unwrap();
        cpu.write_reg(ArmReg::R1, 0x1010).unwrap();
        let out = {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            m_modff(&mut c).unwrap()
        };
        assert_eq!(out, DispatchOutcome::ReturnedR0((-0.25f32).to_bits()));
        let ip = f32::from_bits(u32::from_le_bytes(
            cpu.read_mem(0x1010, 4).unwrap().try_into().unwrap(),
        ));
        assert_eq!(ip, -2.0);

        // ldexpf's exponent is an int in r1, so it is read as an integer
        // and not as a float's bit pattern.
        assert_eq!(
            f32::from_bits(call_r0(m_ldexpf, &[1.5f32.to_bits(), 3])),
            12.0
        );
    }

    /// `strcspn` / `strspn` / `strpbrk` scan against a byte set. X-Forge
    /// tokenises its config text with `strcspn`, which returned zero
    /// from the unimplemented stub — reading as "the delimiter is at
    /// offset 0" for every line.
    #[test]
    fn byte_set_scanners_measure_leading_runs() {
        assert_eq!(call_r0_with_strings(strcspn, &["hello=world", "=:"]).0, 5);
        assert_eq!(call_r0_with_strings(strcspn, &["nodelim", "=:"]).0, 7);
        assert_eq!(call_r0_with_strings(strcspn, &["=lead", "=:"]).0, 0);
        assert_eq!(call_r0_with_strings(strspn, &["  \tx", " \t"]).0, 3);
        assert_eq!(call_r0_with_strings(strspn, &["x  ", " \t"]).0, 0);

        let (r0, addrs) = call_r0_with_strings(strpbrk, &["ab=cd", "=:"]);
        assert_eq!(r0, addrs[0] + 2);
        assert_eq!(call_r0_with_strings(strpbrk, &["abcd", "=:"]).0, 0);
    }

    /// A `strstr` miss is NULL. Returning the haystack instead made
    /// every "is this substring present" test succeed, and an empty
    /// needle used to panic inside `windows(0)`.
    #[test]
    fn strstr_misses_return_null() {
        let (r0, addrs) = call_r0_with_strings(strstr, &["abcdef", "cde"]);
        assert_eq!(r0, addrs[0] + 2);
        assert_eq!(call_r0_with_strings(strstr, &["abcdef", "xyz"]).0, 0);
        assert_eq!(
            call_r0_with_strings(strstr, &["abc", "abcdef"]).0,
            0,
            "a needle longer than the haystack cannot match"
        );
        let (r0, addrs) = call_r0_with_strings(strstr, &["abc", ""]);
        assert_eq!(r0, addrs[0], "an empty needle matches at the start");
    }

    /// `strtok` carries its scan position between calls and terminates
    /// each token in place. Sticky Balls tokenises with the NULL-string
    /// form in a loop and dereferences the result unchecked.
    #[test]
    fn strtok_walks_a_string_across_null_continuation_calls() {
        const S: u32 = 0x1000;
        const DELIM: u32 = 0x1100;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        // Leading and doubled delimiters, and no trailing one, so every
        // edge of the scan is exercised.
        cpu.write_mem(S, b";;de;;en;it\0").unwrap();
        cpu.write_mem(DELIM, b";\0").unwrap();
        let t = dummy_thunk();

        let tok = |first: bool, cpu: &mut StubCpu, kernel: &mut KernelState| -> u32 {
            cpu.write_reg(ArmReg::R0, if first { S } else { 0 })
                .unwrap();
            cpu.write_reg(ArmReg::R1, DELIM).unwrap();
            let mut c = CallCtx {
                cpu,
                thunk: &t,
                kernel,
            };
            match strtok(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(v) => v,
                other => panic!("unexpected outcome {other:?}"),
            }
        };
        let text = |p: u32, cpu: &mut StubCpu| -> String {
            let mut out = Vec::new();
            let mut a = p;
            loop {
                let b = cpu.read_mem(a, 1).unwrap()[0];
                if b == 0 {
                    break;
                }
                out.push(b);
                a += 1;
            }
            String::from_utf8(out).unwrap()
        };

        let a = tok(true, &mut cpu, &mut kernel);
        assert_eq!(text(a, &mut cpu), "de", "leading delimiters are skipped");
        let b = tok(false, &mut cpu, &mut kernel);
        assert_eq!(text(b, &mut cpu), "en", "doubled delimiters collapse");
        let c = tok(false, &mut cpu, &mut kernel);
        assert_eq!(text(c, &mut cpu), "it", "the last token needs no delimiter");
        assert_eq!(
            tok(false, &mut cpu, &mut kernel),
            0,
            "a finished scan returns NULL"
        );
        assert_eq!(
            tok(false, &mut cpu, &mut kernel),
            0,
            "and keeps returning NULL rather than restarting"
        );
        assert!(a < b && b < c, "tokens point into the caller's own buffer");

        // A string that is nothing but delimiters has no first token.
        cpu.write_mem(S, b";;;\0").unwrap();
        assert_eq!(tok(true, &mut cpu, &mut kernel), 0);
    }

    /// A pump that ignores the `WM_QUIT` handed out when the synthetic
    /// message budget runs out must not be answered forever.
    ///
    /// X-Forge dispatches the `WM_QUIT` its `PeekMessage` loop receives
    /// like any other message and drops it, so it kept polling: Ball
    /// Busters froze on the publisher logo and spent the rest of its
    /// slice budget in the pump without rendering another frame.
    #[test]
    fn a_pump_that_ignores_wm_quit_is_halted_instead_of_spun() {
        const MSG: u32 = 0x1000;
        const SP: u32 = 0x1800;
        const BUDGET: u64 = 4;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        kernel.synthetic_message_budget = BUDGET;
        kernel.synthetic_message_count = BUDGET; // already spent
        let t = dummy_thunk();

        let poll = |cpu: &mut StubCpu, kernel: &mut KernelState| -> DispatchOutcome {
            cpu.write_reg(ArmReg::R0, MSG).unwrap();
            cpu.write_reg(ArmReg::R1, 0).unwrap();
            cpu.write_reg(ArmReg::R2, 0).unwrap();
            cpu.write_reg(ArmReg::R3, 0).unwrap();
            cpu.write_reg(ArmReg::Sp, SP).unwrap();
            // `wRemoveMsg` = PM_REMOVE, arg 4, so off the stack.
            let base = SP + cpu.stack_arg_offset();
            cpu.write_mem(base, &1u32.to_le_bytes()).unwrap();
            let mut c = CallCtx {
                cpu,
                thunk: &t,
                kernel,
            };
            peek_message_w(&mut c).unwrap()
        };

        // The quit is offered first — a guest that honours it exits here.
        assert_eq!(
            poll(&mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(1),
            "the pump has to be told to quit before it is cut off"
        );
        let msg = u32::from_le_bytes(cpu.read_mem(MSG + 4, 4).unwrap().try_into().unwrap());
        assert_eq!(msg, WM_QUIT, "and told with WM_QUIT, not an empty queue");

        // A guest that ignores it gets bounded grace, then the run ends.
        let mut polls = 1;
        loop {
            match poll(&mut cpu, &mut kernel) {
                DispatchOutcome::Halt => break,
                DispatchOutcome::ReturnedR0(1) => polls += 1,
                other => panic!("unexpected outcome {other:?} after {polls} polls"),
            }
            assert!(
                polls < 1000,
                "an ignored WM_QUIT must not be answered forever"
            );
        }
        assert_eq!(
            polls as u64, QUIT_POLL_GRACE,
            "the quit is offered exactly the grace number of times"
        );
    }

    /// With the budget disabled — how the desktop and Android frontends
    /// run — nothing above is allowed to fire, or a game would be halted
    /// mid-session.
    #[test]
    fn an_unlimited_message_budget_never_hands_out_wm_quit() {
        const MSG: u32 = 0x1000;
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        kernel.synthetic_message_budget = 0;
        kernel.synthetic_message_count = 100_000;
        let t = dummy_thunk();
        cpu.write_reg(ArmReg::R0, MSG).unwrap();
        cpu.write_reg(ArmReg::Sp, 0x1800).unwrap();
        cpu.write_mem(0x1800 + cpu.stack_arg_offset(), &1u32.to_le_bytes())
            .unwrap();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_ne!(peek_message_w(&mut c).unwrap(), DispatchOutcome::Halt);
    }

    /// `IOCTL_DISK_GET_STORAGEID` answers with a header whose two offsets
    /// point at strings that follow it, so the reply does not fit the
    /// bare header a caller has to guess with. The first call must fail
    /// with the required size in `dwSize` rather than truncate, and the
    /// retry must land a decimal serial at `dwSerialNumOffset`.
    ///
    /// Ball Busters parses that serial with `strtoul`. A zeroed reply
    /// leaves the offset at 0 and parses serial 0, which the game reads
    /// as an empty slot — the "storage card removed" screen.
    #[test]
    fn storage_id_ioctl_reports_its_size_then_the_card_serial() {
        const OUT: u32 = 0x1000;
        const RETURNED: u32 = 0x1900;
        const SP: u32 = 0x1800;

        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("GZGA200045");
        std::fs::create_dir(&game).unwrap();
        std::fs::write(game.join("GZGA200045"), 200_045u32.to_le_bytes()).unwrap();

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        kernel.vfs.mount("\\SD Card\\", dir.path());
        let handle = kernel
            .vfs
            .open("\\SD Card\\Vol:", Access::Read, false)
            .unwrap();
        let t = dummy_thunk();

        // args 0..3 in registers, 4..7 on the stack.
        let call = |h: u32, out_len: u32, cpu: &mut StubCpu, kernel: &mut KernelState| -> u32 {
            cpu.write_reg(ArmReg::R0, h).unwrap();
            cpu.write_reg(ArmReg::R1, 0x0007_1c24).unwrap(); // IOCTL_DISK_GET_STORAGEID
            cpu.write_reg(ArmReg::R2, 0).unwrap();
            cpu.write_reg(ArmReg::R3, 0).unwrap();
            cpu.write_reg(ArmReg::Sp, SP).unwrap();
            let base = SP + cpu.stack_arg_offset();
            for (i, v) in [OUT, out_len, RETURNED, 0].iter().enumerate() {
                cpu.write_mem(base + i as u32 * 4, &v.to_le_bytes())
                    .unwrap();
            }
            let mut c = CallCtx {
                cpu,
                thunk: &t,
                kernel,
            };
            match device_io_control(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(v) => v,
                other => panic!("unexpected outcome {other:?}"),
            }
        };

        // A bare four-DWORD header is too small: fail, but say how big.
        assert_eq!(call(handle, 16, &mut cpu, &mut kernel), 0);
        let needed = u32::from_le_bytes(cpu.read_mem(OUT, 4).unwrap().try_into().unwrap());
        assert!(needed > 16, "dwSize should carry the required size");
        assert_eq!(
            u32::from_le_bytes(cpu.read_mem(RETURNED, 4).unwrap().try_into().unwrap()),
            0,
            "nothing was returned"
        );

        // A handle that is neither a volume nor the MP3 decoder must fail,
        // so a genuinely missing IOCTL still surfaces as one instead of
        // being papered over with success.
        assert_eq!(call(0x1234_5678, needed, &mut cpu, &mut kernel), 0);

        // The retry the game makes with that size must succeed.
        assert_eq!(call(handle, needed, &mut cpu, &mut kernel), 1);
        let reply = cpu.read_mem(OUT, needed).unwrap();
        let size = u32::from_le_bytes(reply[0..4].try_into().unwrap());
        let flags = u32::from_le_bytes(reply[4..8].try_into().unwrap());
        let mfr_off = u32::from_le_bytes(reply[8..12].try_into().unwrap()) as usize;
        let serial_off = u32::from_le_bytes(reply[12..16].try_into().unwrap()) as usize;
        assert_eq!(size, needed);
        // Bit 1 is "serial number invalid"; Ball Busters gives up on the
        // card when it is set.
        assert_eq!(flags & 0b10, 0);
        assert!(mfr_off >= 16 && serial_off > mfr_off);

        let text = |off: usize| {
            let end = reply[off..].iter().position(|b| *b == 0).unwrap() + off;
            String::from_utf8(reply[off..end].to_vec()).unwrap()
        };
        assert!(!text(mfr_off).is_empty());
        assert_eq!(
            text(serial_off).parse::<u32>().unwrap(),
            200_045,
            "the serial has to be the decimal string the card declares"
        );
        assert_eq!(
            u32::from_le_bytes(cpu.read_mem(RETURNED, 4).unwrap().try_into().unwrap()),
            needed
        );
    }

    /// `EventModify` is the CE-native form of `SetEvent` / `ResetEvent` /
    /// `PulseEvent`, and it used to be a constant-1 stub. That made
    /// `WaitForSingleObject` wait on an event nothing could ever signal.
    #[test]
    fn event_modify_moves_real_event_state() {
        const EVENT_PULSE: u32 = 1;
        const EVENT_RESET: u32 = 2;
        const EVENT_SET: u32 = 3;
        const H: u32 = 0xE0E0_0001;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        kernel.events.insert(
            H,
            pocket_kernel::EventObject {
                manual_reset: true,
                signalled: false,
            },
        );
        let t = dummy_thunk();
        let modify = |action: u32, cpu: &mut StubCpu, kernel: &mut KernelState| -> u32 {
            cpu.write_reg(ArmReg::R0, H).unwrap();
            cpu.write_reg(ArmReg::R1, action).unwrap();
            let mut c = CallCtx {
                cpu,
                thunk: &t,
                kernel,
            };
            match event_modify(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(v) => v,
                other => panic!("unexpected outcome {other:?}"),
            }
        };

        assert_eq!(modify(EVENT_SET, &mut cpu, &mut kernel), 1);
        assert!(kernel.events[&H].signalled);
        assert_eq!(modify(EVENT_RESET, &mut cpu, &mut kernel), 1);
        assert!(!kernel.events[&H].signalled);
        assert_eq!(modify(EVENT_PULSE, &mut cpu, &mut kernel), 1);
        assert!(
            kernel.events[&H].signalled,
            "a pulse has to be observable: only one guest thread runs at a time"
        );
        assert_eq!(modify(99, &mut cpu, &mut kernel), 0, "unknown action");
    }

    /// `FindFirstChangeNotificationW` has to reject a directory that does
    /// not exist. Ball Busters watches `\` for the card being pulled and
    /// reads a valid handle as "the path is there".
    #[test]
    fn change_notification_needs_a_real_directory() {
        const PATH: u32 = 0x1000;
        let dir = tempfile::tempdir().unwrap();
        // `list_dir` reports a directory by its contents, so the card has
        // to hold something for the watch to have anything to resolve.
        std::fs::write(dir.path().join("hogfile_giz.hog"), b"x").unwrap();

        let watch = |guest: &str, mount: bool| -> u32 {
            let mut cpu = StubCpu::new();
            let mut kernel = fresh_kernel();
            cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
                .unwrap();
            if mount {
                kernel.vfs.mount("\\SD Card\\", dir.path());
            }
            let mut utf16: Vec<u8> = guest.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            utf16.extend_from_slice(&[0, 0]);
            cpu.write_mem(PATH, &utf16).unwrap();
            cpu.write_reg(ArmReg::R0, PATH).unwrap();
            cpu.write_reg(ArmReg::R1, 1).unwrap();
            cpu.write_reg(ArmReg::R2, 0x0000_0001).unwrap();
            let t = dummy_thunk();
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            match find_first_change_notification_w(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(v) => v,
                other => panic!("unexpected outcome {other:?}"),
            }
        };

        let h = watch("\\SD Card", true);
        assert_ne!(h, 0xFFFF_FFFF, "a mounted card is a real directory");
        assert_eq!(
            watch("\\SD Card", false),
            0xFFFF_FFFF,
            "with nothing mounted there is no such directory"
        );
        assert_eq!(watch("\\Nowhere", true), 0xFFFF_FFFF);
    }

    /// The watch handle has to survive being re-armed in a loop and then
    /// closed — and re-arming a handle that was never handed out has to
    /// fail, so a guest that leaks one does not get told it worked.
    #[test]
    fn change_notification_rearms_then_closes() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        const H: u32 = 0xDEAD_E001;
        kernel.events.insert(
            H,
            pocket_kernel::EventObject {
                manual_reset: true,
                signalled: true,
            },
        );
        let t = dummy_thunk();
        let call =
            |f: crate::Handler, h: u32, cpu: &mut StubCpu, kernel: &mut KernelState| -> u32 {
                cpu.write_reg(ArmReg::R0, h).unwrap();
                let mut c = CallCtx {
                    cpu,
                    thunk: &t,
                    kernel,
                };
                match f(&mut c).unwrap() {
                    DispatchOutcome::ReturnedR0(v) => v,
                    other => panic!("unexpected outcome {other:?}"),
                }
            };

        assert_eq!(
            call(find_next_change_notification, H, &mut cpu, &mut kernel),
            1
        );
        assert!(
            !kernel.events[&H].signalled,
            "re-arming has to clear the event or the guest sees the same change forever"
        );
        assert_eq!(
            call(find_next_change_notification, 0x99, &mut cpu, &mut kernel),
            0
        );
        assert_eq!(
            call(find_close_change_notification, H, &mut cpu, &mut kernel),
            1
        );
        assert!(!kernel.events.contains_key(&H), "the handle is gone");
        assert_eq!(
            call(find_close_change_notification, H, &mut cpu, &mut kernel),
            0,
            "closing twice fails"
        );
    }

    /// `MAS1:` is the Gizmondo's Micronas hardware MP3 decoder. Ball
    /// Busters configures it with an IOCTL before streaming, and a
    /// failure there makes it abandon its music player — leaving the
    /// player object zeroed for its next use to dereference.
    #[test]
    fn mp3_decoder_ioctl_succeeds_and_reports_the_buffer_filled() {
        const OUT: u32 = 0x1000;
        const RETURNED: u32 = 0x1900;
        const SP: u32 = 0x1800;
        const OUT_LEN: u32 = 8;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        // Stale contents so a handler that answers TRUE without writing
        // is not mistaken for one that filled the buffer.
        cpu.write_mem(OUT, &[0xAA; OUT_LEN as usize]).unwrap();

        let handle = kernel.vfs.open("MAS1:", Access::Write, false).unwrap();
        assert!(kernel.vfs.is_mp3_decoder(handle));

        cpu.write_reg(ArmReg::R0, handle).unwrap();
        cpu.write_reg(ArmReg::R1, 0x0001_0000).unwrap();
        cpu.write_reg(ArmReg::R2, 0).unwrap();
        cpu.write_reg(ArmReg::R3, 0).unwrap();
        cpu.write_reg(ArmReg::Sp, SP).unwrap();
        let base = SP + cpu.stack_arg_offset();
        for (i, v) in [OUT, OUT_LEN, RETURNED, 0].iter().enumerate() {
            cpu.write_mem(base + i as u32 * 4, &v.to_le_bytes())
                .unwrap();
        }
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(
            device_io_control(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(cpu.read_mem(OUT, OUT_LEN).unwrap(), [0u8; OUT_LEN as usize]);
        assert_eq!(
            u32::from_le_bytes(cpu.read_mem(RETURNED, 4).unwrap().try_into().unwrap()),
            0
        );
        // And the writes the game follows up with are counted, not refused.
        assert_eq!(kernel.vfs.feed_mp3_decoder(handle, 417), Some(417));
    }

    /// Zuma's sound engine stops a stream by setting a request byte and
    /// then spinning on `Sleep(0)` until its `waveOutProc` sets an
    /// acknowledgement byte (`ZumaPPC.exe` 1.50, `0x000e80e8` spinning,
    /// `0x000e896c` acknowledging). Nothing but a buffer-done callback
    /// ends that loop, so `Sleep` has to be able to deliver one — when
    /// only the message pump could, the game wedged at 100% CPU on
    /// shutdown and on any mid-game stream stop.
    ///
    /// The callback itself calls `Sleep(0)` before acknowledging, which
    /// is why the nested leg below matters: treating that as "the
    /// callback returned" would move SP out from under it.
    #[test]
    fn sleep_delivers_wave_out_callback_and_survives_a_nested_sleep() {
        const HDR: u32 = 0x1000;
        const PROC: u32 = 0x4000;
        const INSTANCE: u32 = 0x5001_0000;
        const CALL_SP: u32 = 0x7800;
        const CALL_LR: u32 = 0xDEAD_BEEF;
        /// What the guest's `waveOutProc` pushes before its own
        /// `Sleep(0)`: Zuma's is `push {r4, r5, lr}`.
        const CALLBACK_PUSH: u32 = 12;

        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.map_region(0x7000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        kernel.wave_out.handle = FAKE_HWAVEOUT;
        kernel.wave_out.callback_kind = pocket_kernel::WaveCallbackKind::Function;
        kernel.wave_out.callback_target = PROC;
        kernel.wave_out.instance = INSTANCE;
        // A buffer whose samples have already played: the host cursor
        // starts at zero, so this is due the moment anyone asks.
        kernel
            .wave_out
            .pending
            .push_back(pocket_kernel::PendingWaveBuffer {
                hdr: HDR,
                end_cursor: 0,
            });

        let t = dummy_thunk();
        let call_sleep = |cpu: &mut StubCpu, kernel: &mut KernelState| {
            let mut c = CallCtx {
                cpu,
                thunk: &t,
                kernel,
            };
            sleep(&mut c).unwrap()
        };

        // The guest sleeps inside its spin loop.
        cpu.write_reg(ArmReg::R0, 0).unwrap();
        cpu.write_reg(ArmReg::Sp, CALL_SP).unwrap();
        cpu.write_reg(ArmReg::Lr, CALL_LR).unwrap();
        let outcome = call_sleep(&mut cpu, &mut kernel);
        assert_eq!(outcome, DispatchOutcome::JumpTo(PROC));
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), FAKE_HWAVEOUT);
        assert_eq!(cpu.read_reg(ArmReg::R1).unwrap(), WOM_DONE);
        assert_eq!(cpu.read_reg(ArmReg::R2).unwrap(), INSTANCE);
        assert_eq!(cpu.read_reg(ArmReg::R3).unwrap(), HDR);
        // The callback must land back in our thunk, on a stack that has
        // room for the fifth argument.
        assert_eq!(cpu.read_reg(ArmReg::Lr).unwrap(), t.thunk_va);
        assert_eq!(
            cpu.read_reg(ArmReg::Sp).unwrap(),
            CALL_SP - WAVE_PROC_STACK_BYTES
        );
        let flags = u32::from_le_bytes(cpu.read_mem(HDR + 16, 4).unwrap().try_into().unwrap());
        assert_eq!(flags & WHDR_DONE, WHDR_DONE, "the guest polls this bit");
        assert!(kernel.wave_out.pending.is_empty());

        // `waveOutProc` now calls `Sleep(0)` itself, one frame deeper.
        cpu.write_reg(ArmReg::R0, 0).unwrap();
        cpu.write_reg(ArmReg::Sp, CALL_SP - WAVE_PROC_STACK_BYTES - CALLBACK_PUSH)
            .unwrap();
        cpu.write_reg(ArmReg::Lr, 0x4444).unwrap();
        let outcome = call_sleep(&mut cpu, &mut kernel);
        assert_eq!(outcome, DispatchOutcome::ReturnedR0(0));
        assert!(
            kernel.wave_out.function_frame.is_some(),
            "the interrupted call must still be on record"
        );
        assert_eq!(
            cpu.read_reg(ArmReg::Lr).unwrap(),
            0x4444,
            "restoring here would strand the running callback"
        );

        // `waveOutProc` returns: its `pop` leaves SP where we set it up
        // and PC at our thunk, and R0..R3 hold its result.
        cpu.write_reg(ArmReg::Sp, CALL_SP - WAVE_PROC_STACK_BYTES)
            .unwrap();
        for (i, reg) in [ArmReg::R0, ArmReg::R1, ArmReg::R2, ArmReg::R3]
            .into_iter()
            .enumerate()
        {
            cpu.write_reg(reg, 0xBADD_0000 + i as u32).unwrap();
        }
        let outcome = call_sleep(&mut cpu, &mut kernel);
        assert_eq!(outcome, DispatchOutcome::ReturnedR0(0));
        assert!(kernel.wave_out.function_frame.is_none());
        // The detour has to be invisible to the interrupted `Sleep`.
        assert_eq!(cpu.read_reg(ArmReg::Sp).unwrap(), CALL_SP);
        assert_eq!(cpu.read_reg(ArmReg::Lr).unwrap(), CALL_LR);
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), 0);
    }
}
