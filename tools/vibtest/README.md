# PocketHLE ARM VIBTEST

Import `dist/PocketHLE-VIBTEST.zip` into the library and run it on Android.
Expect a 0.5-second continuous buzz, regular pulses, then paired pulses.
The test uses SDK-compatible VIB1 + DeviceIoControl and NLed APIs, closes VIB1
before waiting, tests invalid motor/null-pointer errors and 64 stop/reopen cycles.
It writes `VIBTEST.TXT` to its Flash Disk save directory.

Amplitude is fixed at 128/255 when the phone supports it. Otherwise the phone
uses its native fixed strength. API PASS is separate from confirming the motor
physically works: Windows forwards vibration to the selected SDL controller when supported.
A phone without a vibrator returns ERROR_NOT_SUPPORTED (50) on activation.

During another run, press Home while vibrating: output must stop. Return to
PocketHLE: the current guest setting may resume. Exit the game: output must stop
and stay stopped. Check that pressing an on-screen key during a pulse does not
replace the game waveform with button feedback.

Build without proprietary SDK libraries:
`python tools/vibtest/build.py --clang clang --lld ld.lld`

Version 2 increases motor ON/OFF lifecycle repetition to 64 cycles.
This validates returned contracts; physical stop on pause/exit still needs a device.
