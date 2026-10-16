seeking.wmv is a synthetic PocketHLE regression fixture (FFmpeg testsrc plus a 440 Hz sine, two seconds). No game or SDK assets are included.

Generated with:

```sh
ffmpeg -f lavfi -i testsrc=size=96x64:rate=10 -f lavfi -i sine=frequency=440:sample_rate=44100 -t 2 -c:v wmv2 -c:a wmav2 -ac 2 seeking.wmv
```
