# UNO (Windows Mobile) evidence

`title-screen.png` is a 480×800 screenshot of the supplied CAB reaching the UNO title screen.

The CAB identifies the title as Gameloft UNO and includes 480×800 DIB sections. New imports now default to WVGA in the shared library metadata, which is what both Android and desktop GUI launchers use. Existing entries retain their saved screen setting; select WVGA/480×800 in per-game settings or re-import the CAB to apply the new default.

The GUI test path is: import the CAB in the Android or desktop app, confirm the UNO entry uses WVGA/480×800, launch it, and use the title-screen tap/name-entry flow. The supplied screenshot is title-screen evidence, not a completed match or an Android-device run. The WinCE API suite previously passed all 133 tests.
