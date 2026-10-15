# Test clips

Small AV1 clips in WebM for `engine_video`'s tests (`tests/clips.rs`). Each comes from
ffmpeg's synthetic sources (`testsrc2` pictures, a `sine` tone), so nothing in them is
anyone else's work. ffmpeg (N-113236-g90bef6390f-20240108, a GPL build) was used as a
tool only: nothing of it is in the engine.

| Clip | Encoder | What it covers |
|---|---|---|
| `svt_8bit_320x180.webm` | SVT-AV1 1.8.0, preset 8, CRF 40 | 8 bits, an Opus track (codec delay), a coded height of 184 under a 180 picture, intra block copy |
| `aom_10bit_tiles_320x180.webm` | libaom, `cpu-used` 8, CRF 40 | 10 bits, 2 × 2 tiles, hidden frames shown later |
| `svt_grain_tiles_640x360.webm` | SVT-AV1 1.8.0, preset 8, CRF 45 | film grain parameters, 2 × 2 tiles |
| `rav1e_8bit_320x180.webm` | rav1e (speed 10, QP 120) | an `av1C` record without its configuration OBUs (the sequence header comes in the first block), chroma quantizer deltas, segmentation |
| `aom_tools_320x180.webm` | libaom, `cpu-used` 3, CRF 30, on a panning picture | quantizer matrices, delta q and delta loop filter, segmentation data, global motion, loop restoration of luma and chroma, film grain from libaom's denoiser |
| `aom_resilient_320x180.webm` | libaom realtime, `cpu-used` 8 | error resilient frames with frame ids |
| `aom_lossless_160x90.webm` | libaom, lossless | coded lossless frames |
| `svt_superres_320x180.webm` | SVT-AV1 1.8.0, super-resolution 8/12 | super-resolution, frame size overrides |
| `svt_resize_320x180.webm` | SVT-AV1 1.8.0, resize 8/12 | frames coded smaller than they show (render size), sizes taken from references |

`tests/clips.rs` checks that every feature it lists appears in some clip, so a regenerated
clip that loses one fails the test. None of these encoders makes intra-only or switch
frames, short reference signaling (`set_frame_refs` has a unit test instead), tile sizes
that are not uniform, loop filter delta updates, or film grain loaded from a reference or
scaled from luma.

Made with (`F=ffmpeg`):

```sh
$F -f lavfi -i "testsrc2=size=320x180:rate=30:duration=2" \
   -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
   -c:v libsvtav1 -preset 8 -crf 40 -g 30 -pix_fmt yuv420p \
   -c:a libopus -b:a 64k -ac 2 svt_8bit_320x180.webm
$F -f lavfi -i "testsrc2=size=320x180:rate=30:duration=2" \
   -c:v libaom-av1 -cpu-used 8 -crf 40 -g 30 -pix_fmt yuv420p10le \
   -tile-columns 1 -tile-rows 1 -row-mt 1 aom_10bit_tiles_320x180.webm
$F -f lavfi -i "testsrc2=size=640x360:rate=30:duration=1" \
   -c:v libsvtav1 -preset 8 -crf 45 -g 15 -pix_fmt yuv420p \
   -svtav1-params "film-grain=8:tile-columns=1:tile-rows=1" svt_grain_tiles_640x360.webm
$F -f lavfi -i "testsrc2=size=320x180:rate=30:duration=1" \
   -c:v librav1e -speed 10 -qp 120 -g 30 -pix_fmt yuv420p rav1e_8bit_320x180.webm
PAN="testsrc2=size=640x360:rate=30:duration=0.67,crop=320:180:x='t*90':y='t*45'"
$F -f lavfi -i "$PAN" -c:v libaom-av1 -cpu-used 3 -crf 30 -g 30 -pix_fmt yuv420p \
   -aq-mode 1 -enable-global-motion 1 -enable-restoration 1 -denoise-noise-level 20 \
   -aom-params "enable-qm=1:deltaq-mode=1:delta-lf-mode=1" aom_tools_320x180.webm
$F -f lavfi -i "$PAN" -c:v libaom-av1 -usage realtime -cpu-used 8 -crf 40 -g 30 \
   -pix_fmt yuv420p -error-resilience default aom_resilient_320x180.webm
$F -f lavfi -i "testsrc2=size=160x90:rate=30:duration=0.2" -c:v libaom-av1 -cpu-used 8 \
   -pix_fmt yuv420p -aom-params "lossless=1" aom_lossless_160x90.webm
$F -f lavfi -i "$PAN" -c:v libsvtav1 -preset 8 -crf 40 -g 30 -pix_fmt yuv420p \
   -svtav1-params "superres-mode=1:superres-denom=12" svt_superres_320x180.webm
$F -f lavfi -i "$PAN" -c:v libsvtav1 -preset 8 -crf 40 -g 30 -pix_fmt yuv420p \
   -svtav1-params "resize-mode=1:resize-denom=12" svt_resize_320x180.webm
```

Another ffmpeg or encoder version makes other bytes: the tests read what the files hold
(counts of blocks and frames, sizes, bit depths), so a regenerated clip may need its row in
the test's `CLIPS` updated.
