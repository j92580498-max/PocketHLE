# Reference resources

This document collects external resources that are useful when working on
PocketHLE — Windows Mobile / Pocket PC emulators, SDKs, and the relevant ARM
and PE/COFF specifications.

The links here are **for reference only**. Nothing in this repository
distributes any Microsoft binary, SDK or game asset. Users have to supply
their own legally obtained copies.

## Microsoft emulators and SDKs

These images / installers contain official Microsoft device emulators and
Pocket PC / Windows Mobile SDKs that are useful for cross-checking PocketHLE's
behaviour against the real platform (e.g. running the same `.cab` inside the
official Microsoft Device Emulator and comparing API behaviour).

- Microsoft ARM WinCE 6 Emulator (+ random apps):
  <https://archive.org/details/win-ce-6-emulator>
- Microsoft Windows Mobile 2003 SE SDK with emulator:
  <https://archive.org/details/PocketPC2003SDK>
- Microsoft Windows Mobile 5 SDK with emulator:
  <https://archive.org/details/WindowsMobile5.0PocketPCSDKAndEmulator>
- Microsoft Windows Mobile 6.1 emulator:
  <https://archive.org/details/WM614Emulator>
- Microsoft Windows Mobile 6.5 Developer Toolkit:
  <https://legacyupdate.net/download-center/download/17284/windows-mobile-6.5-developer-tool-kit>
- Microsoft x86 WinCE 5 Emulator:
  <https://archive.org/details/win-ce-5-emulator-fixed>
- Windows CE Platform SDK (H/PC) 2.0 02/98:
  <https://archive.org/details/MPLATSDK.20>

## ARM architecture

PocketHLE runs ARMv4T / ARMv5TE code (the variants supported by Pocket PC
2002, 2003 and Windows Mobile 5/6 devices). Pocket PC 2003 binaries make
heavy use of the Thumb instruction set, so both manuals matter.

- ARM Architecture Reference Manual (ARMv5TE):
  <https://developer.arm.com/documentation/ddi0100/latest>
- Thumb instruction set (PPC2003 code uses Thumb extensively):
  <https://developer.arm.com/documentation/ddi0210/latest>
- Calling convention / ABI overview (AAPCS for WinCE — note: differs slightly
  from Linux EABI):
  <https://learn.microsoft.com/en-us/cpp/build/overview-of-arm-abi-conventions>

## PE / COFF on ARM WinCE

The WinCE flavour of PE32 uses `IMAGE_FILE_MACHINE_ARM = 0x01C0`. The
`pocket-pe` crate implements the loader against this specification.

- PE / COFF specification (includes `IMAGE_FILE_MACHINE_ARM = 0x01C0`):
  <https://learn.microsoft.com/en-us/windows/win32/debug/pe-format>
