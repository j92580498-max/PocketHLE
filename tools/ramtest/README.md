# PocketHLE guest RAM diagnostic

`dist/PocketHLE-RAMTEST.zip` is a diagnostic title importable through the GUI.
It executes real ARM instructions and calls PocketHLE's WinCE APIs without
requiring a proprietary SDK or commercial assets.

1. Apply the source patch and rebuild the GUI.
2. Import `PocketHLE-RAMTEST.zip` as a Gizmondo ZIP game.
3. Launch it. The final dialog must display **PASS** .
   Press Enter to close it.
4. Read `RAMTEST.TXT`. Its final line must be
   `RAMTEST_RESULT PASS checks=0x00000088 failures=0x00000000` (136 checks).
5. After closing the dialog, check `flash/DLLTEST.TXT` for a final
   `DLLTEST_RESULT PASS`. Process exit callbacks write it after acknowledgment. The guest dialog is in English.
6. Check `flash/DEPTEST.TXT` for `DEPTEST_RESULT PASS` (dependency order and rollback).
7. Check `flash/PROCTEST.TXT` and `flash/ORPHANTEST.TXT` for
   `PROCTEST_RESULT PASS` and `ORPHANTEST_RESULT PASS`.
8. Run it a second time. If it fails, provide the report and `pockethle-gui.log`.

The report is created at `\Flash Disk\RAMTEST.TXT` on writable storage. Its host
copy is `flash/ramtest.txt` below the PocketHLE library root. The EXE's SD card
remains read-only. To find the report from Windows CMD:

```bat
powershell -NoProfile -Command "Get-ChildItem 'C:\Users\gtristant\Documents\PocketHLE' -Recurse -Filter RAMTEST.TXT | Select-Object -ExpandProperty FullName"
```

Version 8 retains the full v7 suite and adds 64 mixed allocation/event/mutex/
thread/DLL cycles with exact RAM/address-space refunds, plus eight consecutive
process launch/exit/handle-close cycles with RAM reclamation after each child.
This measures guest-accounted resources, not native GPU/driver allocations.

Version 7 adds a multiprocess suite: normal/suspended process creation, command
lines and identifiers, explicit duplication, private TLS, remote thread control,
waits and exit codes, a child outliving its parent, and rollback on insufficient
RAM or invalid PROCESS_INFORMATION output. Four RAMTEST checks surround the
suite and verify memory reclamation. Processes have their own CPUs and host
threads; SDCreateProcess retains the existing return-to-launcher behavior.
Automatic handle inheritance is rejected according to the CE contract; use
DuplicateHandle.

Version 6 adds 33 TLS/error checks: 64 slots, exhaustion, invalid parameters,
reset on reallocation, isolation between main and two workers, direct KData
writes, reuse while a worker waits, TLS preservation during DllMain and wait
errors. Successful TlsGetValue clears GetLastError, even for a null value.
Other successful TLS operations preserve the error. Get/Set implement WinCE's
minimal index validation for 0..63, including unallocated slots.

The diagnostic checks thread DllMain notifications, ordering and context, followed
by process detach events in DLLTEST.TXT. It checks native imports by name/ordinal,
shared dependencies, retention by explicit LoadLibrary, cycles, missing dependencies
and exports, and rollback after a rejected attach.

`pageprobe.dll` checks first-use loading of code, initialized data and zero pages,
unique page accounting, modified data, and unload/reload without leaks. The program
warms its own pages before measuring to isolate these operations. It also checks
reserve/commit/decommit/release, allocation reclamation and realloc failures,
thread-local GetLastError, 32 stack cycles, 24 DLL cycles with multiple references,
exports, rejected DllMain, load errors, RAM redistribution and refusal to shrink
the program partition below occupied pages. The initial partition is restored.
Measurements are hexadecimal bytes, except page counts returned by
GetSystemMemoryDivision.

`GZRT999999` selects the Gizmondo profile through normal title detection. The kernel
has no diagnostic-specific behavior. The program targets PocketHLE's exposed APIs;
compatibility with physical hardware has not been verified.

## Rebuild the binaries

Python 3, Clang and LLD with ARM support are required. Original local validation
used Clang/LLD 18.1.3:

```sh
python3 tools/ramtest/build.py --clang clang --lld ld.lld
```

The script compiles freestanding C and assembles PE32/WinCE headers, imports and
exports. `dist/GZRT999999` binaries are also integration fixtures; rebuild after
changing C sources.

```sh
cargo test -p pocket-core --no-default-features --features unicorn --test ram_guest -- --nocapture
```

The test mounts SD read-only and Flash Disk writable, executes the EXE through
Unicorn, acknowledges the final dialog and checks the report, exit code, restored
partition, unloaded modules and reclaimed worker stacks. It removes its files
afterward. Production games do not enable diagnostic logging.

The `dist/fixtures` tests also execute ExitProcess(77), and a case where main calls
ExitThread(11) before the last worker returns 33. They are not included in the
importable ZIP. All three variants are tested with Unicorn.
