# PocketHLE NETTEST (ARM WinCE)

Import `dist/PocketHLE-NETTEST.zip` into the game library and launch it.
It resolves all nine WinINet exports dynamically, then downloads example.com
once using HTTP (80) and once HTTPS (443). Each request checks the status,
UTF-16 Content-Length when present, streaming reads, EOF and parent-handle close.
It performs no login and sends no gameplay data. A HTTP error status may still
be a valid transport response; a nonempty response body is required for this test.

Read `NETTEST.TXT` in this game's Flash Disk save directory. PASS verifies both
transports on that host. FAIL records GetLastError (hex); check DNS/network,
firewall/proxy and device certificate/time configuration. Each native operation
has a 30-second timeout, so a failed connection can take some time.

The separate development-only loopback run adds a POST matching Colors' NULL/-1
header-length convention and 256-KiB fragmented bodies. Its simulated secure flag
checks the guest ABI only, not native TLS. That variant is not distributed.

Rebuild without any proprietary SDK using Python, Clang and LLD:
`python tools/nettest/build.py --clang clang --lld ld.lld`

Version 2 adds 40 nested session/connection/request create/cascading-close cycles.
These cycles do not send additional requests to example.com. Pending native
request cancellation and physical network operation remain separate checks.
