# Viewports: split screen, minimaps, pixel art, render to texture

A **viewport** is a picture the engine renders: the game view, a minimap, a security
camera, the pixel-art screen of a retro game, a player's half of a split screen. Each
viewport has its own size, update rate and post-processing. Viewports can show each other:
a minimap is a viewport drawn inside the game viewport. 2D and 3D work the same way, and
the editor's views use the same system.

## 1. Creating viewports

```rust
use engine_render::viewport::*;

let mut viewports = Viewports::new(&device, frames_in_flight);

// The game view: the left half of the window, sized to it.
let game = viewports.create(ViewportDesc {
    name: "game".into(),
    placement: Some(Placement { rect: [0.0, 0.0, 0.5, 1.0], order: 0 }),
    ..ViewportDesc::default()
});

// A 160 × 160 minimap, shown inside the game view, updated every second frame.
let minimap = viewports.create(ViewportDesc {
    name: "minimap".into(),
    size: ViewportSize::Fixed { width: 160, height: 160 },
    parent: Some(game),
    placement: None,
    update: UpdateMode::Every { n: 2, offset: 0 },
    ..ViewportDesc::default()
});
```

### Settings

| Setting | What it does |
|---|---|
| `size: Fixed { width, height }` | Fixed resolution. |
| `size: Placement { scale }` | Follows the viewport's place in the window. `scale: 0.5` renders at half resolution and scales up. |
| `update: Always` | Renders every frame. |
| `update: Every { n, offset }` | Renders every `n`-th frame (a "render tick"). Give different offsets to spread work. |
| `update: OnRequest` | Renders when you call `viewports.request_update(id)` (portraits, photo modes). |
| `update: Disabled` | Never renders; keeps its picture. |
| `parent` | The viewport that shows this one. It renders first, so the parent sees this frame's picture. |
| `placement` | Where it appears in the window: `rect` is left, top, width, height in 0..1; higher `order` is on top. |
| `scaling: PixelPerfect` | Integer scaling with sharp pixels and borders (letterboxing). |
| `scaling: StretchNearest` | Fills the placement with nearest filtering: the cheapest upscale for weak hardware (blocky at non-integer ratios). |
| `exposure`, `tonemap` | Post-processing when shown in the window (`tonemap: false` for already-final colors). |
| `clear` | Background color, passed to your content. |

A viewport that skips a frame keeps showing its last picture.

## 2. Every frame

```rust
viewports.render(&mut fg, &device, &mut allocator, &bindless,
                 (window_width, window_height), frame.number,
                 |fg, vp| {
    // Draw into vp.color (vp.width × vp.height), starting with vp.load.
    if vp.id == game {
        forward.add_passes(fg, ring, bindless, &draws, &camera, &light, vp.color, depth, clear);
        renderer2d.add_pass(fg, ring, bindless, &hud_canvas, &View2d::pixels(vp.width, vp.height), vp.color, None)?;
    } else if vp.id == minimap {
        renderer2d.add_pass(fg, ring, bindless, &minimap_canvas, &View2d::pixels(vp.width, vp.height), vp.color, clear)?;
    }
})?;
viewports.composite(&mut fg, &tonemap, &bindless, backbuffer,
                    (window_width, window_height), frame.number, [0.0, 0.0, 0.0, 1.0]);
```

Only viewports that are due this frame call your closure, and children come first.

On an HDR display, call `viewports.set_output_levels(levels)` whenever the output
changes (`docs/dev-guide/hdr.md`): `composite` then rolls highlights off towards the
display's peak and writes the swapchain's encoding (scRGB or HDR10), including the clear
color. Changing a viewport's `format` (for example from sRGB to `HDR_FORMAT` when HDR
turns on) recreates its texture on the next `render`.

## 3. Showing a viewport inside another

`viewports.texture_index(minimap)` is the minimap's texture index. Use it like any
texture, for example as a sprite in the game view's HUD canvas:

```rust
if let Some(map) = viewports.texture_index(minimap) {
    hud.draw(&Quad::sprite(map, Vec2::new(100.0, 100.0), Vec2::splat(160.0)));
}
```

- It is `None` until the minimap has rendered once.
- The index changes if the minimap is resized, so read it every frame.

## 4. Split screen and picture-in-picture

- **Split screen:** give each player's viewport a placement: left and right halves, or
  four quarters.
- **Picture-in-picture:** a small placement with a higher `order`.
- All placed viewports are drawn into the window by `composite`, in `order`.

## 5. Pixel art

Render at the game's native resolution and let the engine scale by whole numbers:

```rust
ViewportDesc {
    size: ViewportSize::Fixed { width: 320, height: 180 },
    scaling: Scaling::PixelPerfect,
    ..
}
```

- In a 1920 × 1080 window this is exactly 6× with no borders.
- In other sizes it is the largest whole multiple that fits, centered, with the clear
  color around it.

Draw with `Quad::solid` and `Quad::sprite`, not the smooth shapes, for hard pixel edges.

## 6. Cost per viewport

`viewports.gpu_time_us(&graph, id)` is the GPU time of a viewport's passes in the last
completed frame. In RenderDoc or Nsight, a viewport's passes appear under a label with
its name.

## Lower resolution for weak hardware

Render the 3D viewport below the window's resolution and let the compositor scale it:

```rust
ViewportDesc {
    size: ViewportSize::Placement { scale: 0.5 }, // 960 × 540 in a 1080p window
    scaling: Scaling::Stretch,                    // bilinear (smooth)
    // scaling: Scaling::StretchNearest,          // nearest (sharp, blocky, cheapest)
    ..ViewportDesc::default()
}
```

- UI in its own viewport at scale 1 stays sharp.
- Better upscalers (own TAAU, FSR, DLSS, XeSS) plug in behind the same render scale
  later (CLAUDE.md §6).
