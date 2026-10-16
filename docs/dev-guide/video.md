# Video

A video plays as an ordinary texture: on a screen in the world (a material's emission or
base colour), in the UI (a sprite), anywhere a texture goes. Its sound plays with it and
keeps it in time (ADR 0100).

The format is **AV1 in WebM**, with **Opus** sound: royalty-free, decoded by the GPU's
video engine on both reference GPUs, and by rav1d on the CPU where the GPU cannot.

## Encoding your videos

Any AV1 encoder works. With ffmpeg and SVT-AV1:

```sh
ffmpeg -i intro.mov -c:v libsvtav1 -preset 6 -crf 30 -g 120 -pix_fmt yuv420p \
       -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv \
       -c:a libopus -b:a 128k intro.webm
```

- **Tag the colours** (`-colorspace` and its friends). A video that does not say is
  taken as BT.709 from 1280 × 720 up and BT.601 below, as players guess; ffmpeg's own
  conversion from RGB uses BT.601 unless told otherwise.
- **4:2:0, 8 or 10 bits** decode on the GPU (AV1's main profile). 4:4:4 and 4:2:2 play
  through rav1d.
- **A key frame now and then** (`-g`): a seek starts decoding at the key frame before
  its target. Two to four seconds apart is a good balance of size and seek time.
- **SDR only for now**: a stream tagged for HDR (PQ, HLG) or BT.2020's colours plays with
  its matrix but without its curve and gamut (ADR 0100).
- **No transparency yet** (an alpha channel): planned (`docs/status.md`, §6, Video).

## Playing one on the player

Ask the player for the GPU's decode queue (it costs a little device creation; without
it videos decode on the CPU):

```rust
let mut player = Player::new(&PlayerSettings {
    video_decode: true,
    ..PlayerSettings::default()
})?;
```

Open the video from its bytes (from a file, or your own assets), and show it:

```rust
use engine_app::{MaterialSlot, VideoDesc};
use engine_render::material::MaterialDesc;
use std::sync::Arc;

let setup = player.setup();
let bytes: Arc<[u8]> = std::fs::read("intro.webm")?.into();
let video = setup.videos.open(bytes, VideoDesc::default());

// A screen in the world: the video as the material's emission.
let screen = setup.materials.add(MaterialDesc {
    emissive: [1.0; 3], // how bright the screen glows
    ..MaterialDesc::solid([0.0; 3], 0.0, 1.0)
});
setup.videos.show_in(video, screen, MaterialSlot::Emissive);
```

- **`show_in`** replaces one texture of a material (its base colour or its emission);
  when the video closes, the material is as it was. Until the first picture arrives the
  material shades with its factors alone.
- **In the UI**, draw a sprite of the video's texture once it has a picture:

  ```rust
  if let Some(index) = videos.texture_index(video) {
      canvas.draw(&Quad::sprite(index, center, size));
  }
  ```

- **Your own materials** can name the video's texture like any other:
  `TextureRef::new(videos.texture(video).unwrap())`.

Everything on `Videos` takes effect at the next frame, so it can be called from any hook:
`player.setup().videos` before the first frame, `FrameParts::videos` in
`Game::animate` every frame.

## Controls and events

```rust
videos.play(video);
videos.pause(video);
videos.seek(video, 12.5);       // seconds
videos.set_looping(video, true);
videos.set_speed(video, 0.5);    // the sound's pitch follows
for event in videos.events(video) {
    match event {
        VideoEvent::Shown(time) => {} // a new picture is on screen
        VideoEvent::Ended => {}       // the last picture, not looping
        VideoEvent::Looped => {}      // started over
    }
}
```

- **`VideoDesc`**: start playing at once (`autoplay`, on by default), loop, decode on the
  GPU when it can, play the sound (and on which bus, how loud), how many pictures to
  decode ahead (3), rav1d's threads.
- **A seek** decodes from the key frame before the target, a few frames' work, and shows
  the picture at the target exactly; the clock waits for it.
- **`videos.error(video)`** says why a video could not open or play on.

## Time and sound

- **With sound**, the sound's position in the mix (less the output's latency) is the
  clock: the picture follows what you hear. The sound plays through an audio output the
  player starts with the first video that has sound.
- **Without sound**, the clock moves with the frame's time.
- **The picture shown** is the newest whose time the clock has reached. A late picture is
  skipped, an early one waits; a picture still decoding stays off screen a frame longer
  rather than stall the frame.

## What it costs

Measured on ref-1 (2026-10-16), a 1080p30 and a 2160p30 10-bit stream, each new picture:

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| Decoding on the GPU's video engine | its own engine, beside the frame | its own engine |
| Conversion into the texture, 1080p | 25 µs | 1.3 ms |
| Conversion, 2160p 10-bit | 0.17 ms | 4.3 ms |
| CPU, a frame (update), median / p99 | 1 µs / 0.2 ms | 1 µs / 0.1 ms |

- On the CPU path (rav1d), the decoding runs on worker threads (half the cores, at most
  eight) and each picture is copied to the GPU: 3 MB at 1080p, 0.2 ms of the frame's CPU
  time and 0.26 ms of the RTX's.
- A paused video costs nothing; a video with no new picture this frame costs nothing on
  the GPU.

## Limits

- **WebM files only**, and AV1 only (no H.264 or VP9); Opus only for the sound.
- **Packs**: a video is opened from bytes; cooking WebM files into packs (streamed through
  the pack's reads) is planned.
- **Small videos on the RTX**: its decoder takes nothing below 128 × 128; those play
  through rav1d. A stream whose frames change size below that hands over to rav1d at the
  key frame before the clock.
