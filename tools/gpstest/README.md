# ARM GPS1 test

Import `dist/PocketHLE-GPSTEST.zip` into the Gizmondo library. Enable **GPS / host
location (GPS1)** under Emulator options before launching it. On Windows enable
OS location services and desktop app access; on Android allow precise location
and try outdoors if acquisition is slow. Approximate location remains supported,
but may be insufficient for Colors.

The diagnostic checks sharing, the exact 180-byte packed snapshot, malformed
buffers, access denial, closing and 20 reopen/close cycles. It waits up to 30
seconds for a native fix. `GPSTEST_RESULT PASS` confirms the API contract;
`GPS_POSITION AVAILABLE` separately confirms a physical location was received.
`NO_FIX` never means a fabricated coordinate was returned.

Reports are in Flash Disk: `GPSTEST.TXT` and the packed `GPSTEST.BIN` snapshot.
The report contains coordinates in signed integer degrees ×10^7, displayed as
hex, and horizontal error in centimetres. Keep that location information in mind
when sharing the diagnostic report. Colors requires a valid fix, age <300 seconds
and error <100 metres; `COLORS_POSITION_ELIGIBLE` checks the fix/error part.

GPS1 is a location snapshot device, not an NMEA serial port. The current bridge
implements position reading; geofence writes, SiRF resets, hardware APM and
undocumented version IOCTLs return `ERROR_NOT_SUPPORTED` (50). It does not change
the host clock, invent satellites or send data to Gizmondo Network Services.

Build without proprietary SDK code:

```text
python tools/gpstest/build.py --clang clang --lld ld.lld
```

Version 2 reads the packed snapshot on each of 32 open/read/close cycles.
This verifies API lifetimes; no-fix snapshots remain valid API results.
