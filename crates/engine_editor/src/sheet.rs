//! An image's own tab (ADR 0125's E13-2c, `docs/research/ux/2d.md`): the sheet at whole
//! zooms on a dark ground, its slices outlined over it, the selected ones lit. A double
//! click on an image in the FileSystem dock opens it; the wheel zooms about the pointer
//! (whole numbers up, halves down), a middle or right drag pans, a click picks a slice
//! (Shift a range, Ctrl one more), Delete takes the picked slices away, M makes a clip of
//! them, F frames the sheet. The slices are the image's `.meta`'s, written as undo steps
//! by the app.

use engine_core::math::{Vec2, Vec4};
use engine_render2d::{Canvas, ClipRect, Quad};
use engine_rhi::SamplerKind;
use engine_scene::{AssetId, AssetMeta, SliceImport};

/// The zooms the tab steps through (screen pixels an image pixel, at UI scale 1).
const ZOOMS: [f32; 12] = [
    0.125, 0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0, 32.0,
];

/// An image open in its tab.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SheetDoc {
    /// The image file.
    pub(crate) id: AssetId,
    /// Its path in the project, with `/`.
    pub(crate) path: String,
    /// Its tab's label (its file's name).
    pub(crate) tab: String,
    /// Its slices as its `.meta` holds them.
    pub(crate) slices: Vec<SliceImport>,
    /// Its clips' count, for the bar.
    pub(crate) clips: usize,
    /// Its clips as the `.meta` holds them (E13-2c-5).
    pub(crate) clip_list: Vec<engine_scene::SpriteClipImport>,
    /// Their chips' labels.
    pub(crate) clip_labels: Vec<String>,
    /// The chosen clip, and when its preview started.
    pub(crate) clip: Option<(usize, std::time::Instant)>,
    /// The picked slices, by index, in the order picked.
    pub(crate) picked: Vec<usize>,
    /// The slice a Shift+click ranges from.
    pub(crate) anchor: Option<usize>,
    /// Which of [`ZOOMS`] it shows at.
    pub(crate) zoom: usize,
    /// The image point at the canvas's centre, in image pixels from its top left.
    pub(crate) center: [f32; 2],
    /// Framed once its size is known.
    pub(crate) framed: bool,
    /// Its texture as the player has it: the bindless index and the size in pixels.
    pub(crate) image: Option<(u32, [f32; 2])>,
    /// A rectangle being drawn on the sheet by hand (E13-2c-3): where the press was and
    /// where the pointer is, in image pixels.
    pub(crate) drawing: Option<([f32; 2], [f32; 2])>,
    /// The last press on nothing: when and at which pixel (a second soon after on the
    /// same pixel fits a slice to the drawing there).
    pub(crate) last_press: Option<(std::time::Instant, [i64; 2])>,
    /// Its pixels, read from its file the first time a slice is fitted (RGBA8, rows from
    /// the top) with its size.
    pub(crate) pixels: Option<([u32; 2], Vec<u8>)>,
}

/// A clip's chip: its name, frames, frame time and mode (`run 4–9 · 80 ms · loop`).
pub(crate) fn clip_label(c: &engine_scene::SpriteClipImport) -> String {
    let mode = match c.mode {
        engine_scene::SpriteClipMode::Loop => "loop",
        engine_scene::SpriteClipMode::Once => "once",
        engine_scene::SpriteClipMode::PingPong => "ping-pong",
    };
    format!(
        "{} {}\u{2013}{} · {} ms · {mode}",
        c.name,
        c.first,
        c.last,
        (c.frame_time * 1000.0).round()
    )
}

/// The frame clip `c` shows `t` seconds in, as the player plays it: its run (backwards
/// when its last is below its first; ping-pong's way back without doubled ends), each
/// frame its own duration or the frame time, round and round (held at the end once).
pub(crate) fn clip_frame(c: &engine_scene::SpriteClipImport, t: f32) -> u32 {
    let run: Vec<u32> = if c.last >= c.first {
        (c.first..=c.last).collect()
    } else {
        (c.last..=c.first).rev().collect()
    };
    let mut places: Vec<usize> = (0..run.len()).collect();
    if c.mode == engine_scene::SpriteClipMode::PingPong && run.len() > 2 {
        places.extend((1..run.len() - 1).rev());
    }
    let time = |k: usize| {
        let d = c.durations.get(k).copied().unwrap_or(c.frame_time);
        if d.is_finite() && d > 0.0 { d } else { 0.1 }
    };
    let length: f32 = places.iter().map(|&k| time(k)).sum();
    let mut t = if c.mode == engine_scene::SpriteClipMode::Once {
        t.min(length - 1e-4)
    } else {
        t.rem_euclid(length.max(1e-4))
    };
    for &k in &places {
        let d = time(k);
        if t < d {
            return run[k];
        }
        t -= d;
    }
    places.last().map_or(0, |&k| run[k])
}

/// The nine pivot presets (E13-2c-3): a name and the pivot (shares of the width and
/// height from the bottom left).
pub(crate) const PIVOTS: [(&str, [f32; 2]); 9] = [
    ("Center", [0.5, 0.5]),
    ("Bottom Center (feet)", [0.5, 0.0]),
    ("Top Center", [0.5, 1.0]),
    ("Left Center", [0.0, 0.5]),
    ("Right Center", [1.0, 0.5]),
    ("Bottom Left", [0.0, 0.0]),
    ("Bottom Right", [1.0, 0.0]),
    ("Top Left", [0.0, 1.0]),
    ("Top Right", [1.0, 1.0]),
];

/// The rectangle of whole pixels between image points `a` and `b` (either corner first),
/// clamped to an image of `size`; none when it has no area.
pub(crate) fn pixel_rect(a: [f32; 2], b: [f32; 2], size: [f32; 2]) -> Option<[u32; 4]> {
    let lo = [
        a[0].min(b[0]).floor().max(0.0),
        a[1].min(b[1]).floor().max(0.0),
    ];
    let hi = [
        a[0].max(b[0]).ceil().min(size[0]),
        a[1].max(b[1]).ceil().min(size[1]),
    ];
    (hi[0] > lo[0] && hi[1] > lo[1]).then(|| {
        [
            lo[0] as u32,
            lo[1] as u32,
            (hi[0] - lo[0]) as u32,
            (hi[1] - lo[1]) as u32,
        ]
    })
}

/// A slice's name not among `taken`: `{prefix}_{n}` with the lowest free number.
pub(crate) fn free_name(prefix: &str, taken: &[SliceImport]) -> String {
    (0..)
        .map(|n| format!("{prefix}_{n}"))
        .find(|name| !taken.iter().any(|s| s.name == *name))
        .unwrap_or_default()
}

impl SheetDoc {
    /// Image `id` at `path` under `root`, its slices read from its `.meta`.
    pub(crate) fn open(root: &std::path::Path, id: AssetId, path: &str) -> Self {
        let mut doc = Self {
            id,
            path: path.to_owned(),
            tab: path.rsplit('/').next().unwrap_or(path).to_owned(),
            slices: Vec::new(),
            clips: 0,
            clip_list: Vec::new(),
            clip_labels: Vec::new(),
            clip: None,
            picked: Vec::new(),
            anchor: None,
            zoom: 6,
            center: [0.0; 2],
            framed: false,
            image: None,
            drawing: None,
            last_press: None,
            pixels: None,
        };
        doc.read_meta(root);
        doc
    }

    /// Its slices read again from its `.meta` (after an edit, an undo or a change on
    /// disk), the picks kept to those that remain.
    pub(crate) fn read_meta(&mut self, root: &std::path::Path) {
        let meta = AssetMeta::read(&root.join(&self.path));
        self.set_meta(&meta);
    }

    /// Its slices as `meta` holds them.
    pub(crate) fn set_meta(&mut self, meta: &AssetMeta) {
        self.slices = meta.sprite.slices.clone();
        self.clips = meta.sprite.clips.len();
        self.clip_list = meta.sprite.clips.clone();
        self.clip_labels = self.clip_list.iter().map(clip_label).collect();
        self.clip = self.clip.filter(|(k, _)| *k < self.clip_list.len());
        let n = self.slices.len();
        self.picked.retain(|&k| k < n);
        self.anchor = self.anchor.filter(|&k| k < n);
    }

    /// Screen pixels an image pixel at UI scale 1.
    pub(crate) fn zoom(&self) -> f32 {
        ZOOMS[self.zoom.min(ZOOMS.len() - 1)]
    }

    /// The zoom that shows all of it in a canvas of `size` panel pixels (the largest of
    /// [`ZOOMS`] that fits, with a margin), centred.
    pub(crate) fn frame(&mut self, size: [f32; 2]) {
        let Some((_, image)) = self.image else {
            return;
        };
        let fit = ((size[0] - 48.0) / image[0].max(1.0)).min((size[1] - 48.0) / image[1].max(1.0));
        self.zoom = ZOOMS.iter().rposition(|&z| z <= fit).unwrap_or(0);
        self.center = [image[0] * 0.5, image[1] * 0.5];
        self.framed = true;
    }

    /// The image point (pixels from its top left) under panel point `at` of a canvas
    /// whose top left is `origin` and size `size` (panel pixels).
    pub(crate) fn image_point(&self, at: [f32; 2], origin: [f32; 2], size: [f32; 2]) -> [f32; 2] {
        let z = self.zoom();
        [
            self.center[0] + (at[0] - origin[0] - size[0] * 0.5) / z,
            self.center[1] + (at[1] - origin[1] - size[1] * 0.5) / z,
        ]
    }

    /// Zooms by `notches` wheel steps keeping image point `about` where it is on screen.
    pub(crate) fn zoom_about(&mut self, notches: f32, about: [f32; 2]) {
        let before = self.zoom();
        let step = notches.round() as isize;
        self.zoom = (self.zoom as isize + step).clamp(0, ZOOMS.len() as isize - 1) as usize;
        let k = before / self.zoom();
        self.center = [
            about[0] - (about[0] - self.center[0]) * k,
            about[1] - (about[1] - self.center[1]) * k,
        ];
    }

    /// Pans by a drag of `delta` panel pixels.
    pub(crate) fn pan(&mut self, delta: [f32; 2]) {
        let z = self.zoom();
        self.center = [self.center[0] - delta[0] / z, self.center[1] - delta[1] / z];
    }

    /// The slice under image point `p`: of those holding it, the smallest (a frame inside a
    /// larger region wins).
    pub(crate) fn slice_at(&self, p: [f32; 2]) -> Option<usize> {
        self.slices
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let [x, y, w, h] = s.rect.map(|v| v as f32);
                p[0] >= x && p[1] >= y && p[0] < x + w && p[1] < y + h
            })
            .min_by_key(|(_, s)| u64::from(s.rect[2]) * u64::from(s.rect[3]))
            .map(|(k, _)| k)
    }

    /// A click on slice `k` (none: on nothing): picks it alone; with `range`, the slices
    /// from the anchor to it; with `toggle`, it is added or taken away.
    pub(crate) fn click(&mut self, k: Option<usize>, range: bool, toggle: bool) {
        let Some(k) = k else {
            if !range && !toggle {
                self.picked.clear();
                self.anchor = None;
            }
            return;
        };
        if range && let Some(a) = self.anchor {
            let (lo, hi) = (a.min(k), a.max(k));
            self.picked = (lo..=hi).collect();
            return;
        }
        if toggle {
            match self.picked.iter().position(|&p| p == k) {
                Some(i) => {
                    self.picked.remove(i);
                }
                None => self.picked.push(k),
            }
        } else {
            self.picked = vec![k];
        }
        self.anchor = Some(k);
    }

    /// The bar's line: its size, slices, clips, zoom and what is picked.
    pub(crate) fn status(&self, out: &mut String) {
        use core::fmt::Write as _;
        out.clear();
        if let Some((_, [w, h])) = self.image {
            let _ = write!(out, "{w} × {h} px · ");
        }
        let _ = write!(
            out,
            "{} slices · {} clips · {} %",
            self.slices.len(),
            self.clips,
            (self.zoom() * 100.0).round()
        );
        match self.picked.as_slice() {
            [] => {}
            [k] => {
                if let Some(s) = self.slices.get(*k) {
                    let [x, y, w, h] = s.rect;
                    let _ = write!(out, " · {} (frame {k}) {x}, {y} · {w} × {h}", s.name);
                }
            }
            many => {
                let _ = write!(out, " · {} picked", many.len());
            }
        }
    }
}

/// Draws `doc` in canvas `rect` (pixels) at UI scale `scale`: a dark ground, the image
/// sampled by nearest pixel, its slices' outlines (the picked ones in `accent`).
pub(crate) fn draw(
    doc: &SheetDoc,
    rect: [u32; 4],
    scale: f32,
    ground: [f32; 4],
    accent: [f32; 4],
    canvas: &mut Canvas,
) {
    let order = crate::graph::CANVAS_ORDER;
    let [rx, ry, rw, rh] = rect.map(|v| v as f32);
    canvas.set_clip(Some(ClipRect {
        x: rect[0] as i32,
        y: rect[1] as i32,
        width: rect[2],
        height: rect[3],
    }));
    canvas.draw(
        &Quad::solid(Vec2::new(rx + rw * 0.5, ry + rh * 0.5), Vec2::new(rw, rh))
            .color(Vec4::from_array(ground))
            .order(order),
    );
    let z = doc.zoom() * scale;
    // Image pixel `p` on screen (whole pixels, so edges stay sharp).
    let to = |p: [f32; 2]| {
        Vec2::new(
            (rx + rw * 0.5 + (p[0] - doc.center[0]) * z).round(),
            (ry + rh * 0.5 + (p[1] - doc.center[1]) * z).round(),
        )
    };
    if let Some((texture, size)) = doc.image {
        let (lo, hi) = (to([0.0, 0.0]), to(size));
        // A faint checker of the image's area under clear pixels: its edge.
        canvas.draw(
            &Quad::solid((lo + hi) * 0.5, hi - lo)
                .color(Vec4::new(1.0, 1.0, 1.0, 0.04))
                .order(order),
        );
        let mut quad = Quad::sprite(texture, (lo + hi) * 0.5, hi - lo).order(order);
        quad.sampler = SamplerKind::NearestClamp;
        canvas.draw(&quad);
    }
    let line = |canvas: &mut Canvas, a: Vec2, b: Vec2, color: Vec4| {
        canvas.draw(&Quad::line(a, b, scale.max(1.0)).color(color).order(order));
    };
    for (k, s) in doc.slices.iter().enumerate() {
        let picked = doc.picked.contains(&k);
        let color = if picked {
            Vec4::from_array(accent)
        } else {
            Vec4::new(1.0, 1.0, 1.0, 0.55)
        };
        let [x, y, w, h] = s.rect.map(|v| v as f32);
        let (a, b, c, d) = (
            to([x, y]),
            to([x + w, y]),
            to([x + w, y + h]),
            to([x, y + h]),
        );
        for (p, q) in [(a, b), (b, c), (c, d), (d, a)] {
            line(canvas, p, q, color);
        }
        if picked {
            // Its pivot: a small cross.
            let pivot = to([x + w * s.pivot[0], y + h * (1.0 - s.pivot[1])]);
            let r = 4.0 * scale;
            line(
                canvas,
                pivot - Vec2::new(r, 0.0),
                pivot + Vec2::new(r, 0.0),
                color,
            );
            line(
                canvas,
                pivot - Vec2::new(0.0, r),
                pivot + Vec2::new(0.0, r),
                color,
            );
        }
    }
    // The chosen clip (E13-2c-5): its frames outlined in green, its preview in the
    // canvas's top right corner at a whole zoom, its frame by the time since chosen.
    if let Some((k, since)) = doc.clip
        && let Some(c) = doc.clip_list.get(k)
    {
        let green = Vec4::new(0.45, 0.9, 0.5, 1.0);
        let (lo, hi) = (c.first.min(c.last) as usize, c.first.max(c.last) as usize);
        for s in doc.slices.iter().take(hi + 1).skip(lo) {
            let [x, y, w, h] = s.rect.map(|v| v as f32);
            let (a, b, c2, d) = (
                to([x, y]),
                to([x + w, y]),
                to([x + w, y + h]),
                to([x, y + h]),
            );
            for (p, q) in [(a, b), (b, c2), (c2, d), (d, a)] {
                canvas.draw(&Quad::line(p, q, scale.max(1.0)).color(green).order(order));
            }
        }
        let frame = clip_frame(c, since.elapsed().as_secs_f32()) as usize;
        if let (Some(s), Some((texture, size))) = (doc.slices.get(frame), doc.image) {
            let [x, y, w, h] = s.rect.map(|v| v as f32);
            let side = 128.0 * scale;
            let k = ((side / w.max(h).max(1.0)).floor()).max(1.0);
            let box_size = Vec2::new(w * k, h * k);
            let center = Vec2::new(
                rx + rw - 16.0 * scale - side * 0.5,
                ry + 16.0 * scale + side * 0.5,
            );
            canvas.draw(
                &Quad::solid(center, Vec2::splat(side + 8.0 * scale))
                    .color(Vec4::new(0.0, 0.0, 0.0, 0.5))
                    .order(order),
            );
            let mut quad = Quad::sprite(texture, center, box_size).order(order);
            quad.uv = [
                x / size[0],
                y / size[1],
                (x + w) / size[0],
                (y + h) / size[1],
            ];
            quad.sampler = SamplerKind::NearestClamp;
            canvas.draw(&quad);
        }
    }
    // A rectangle being drawn by hand, in the accent colour.
    if let Some((a, b)) = doc.drawing
        && let Some([x, y, w, h]) = doc.image.and_then(|(_, size)| pixel_rect(a, b, size))
    {
        let [x, y, w, h] = [x, y, w, h].map(|v| v as f32);
        let (p, q, r, s) = (
            to([x, y]),
            to([x + w, y]),
            to([x + w, y + h]),
            to([x, y + h]),
        );
        let accent = Vec4::from_array(accent);
        for (from, to) in [(p, q), (q, r), (r, s), (s, p)] {
            canvas.draw(
                &Quad::line(from, to, scale.max(1.0))
                    .color(accent)
                    .order(order),
            );
        }
    }
    canvas.set_clip(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet() -> SheetDoc {
        let slice = |name: &str, rect: [u32; 4]| SliceImport {
            name: String::from(name),
            rect,
            ..SliceImport::default()
        };
        SheetDoc {
            id: AssetId::new(),
            path: String::from("art/hero.png"),
            tab: String::from("hero.png"),
            slices: vec![
                slice("a", [0, 0, 16, 16]),
                slice("b", [16, 0, 16, 16]),
                slice("c", [32, 0, 16, 16]),
                slice("big", [0, 0, 48, 16]),
            ],
            clips: 0,
            clip_list: Vec::new(),
            clip_labels: Vec::new(),
            clip: None,
            picked: Vec::new(),
            anchor: None,
            zoom: 6,
            center: [0.0; 2],
            framed: false,
            image: Some((3, [48.0, 16.0])),
            drawing: None,
            last_press: None,
            pixels: None,
        }
    }

    /// A clip's preview plays its frames as the player does; its chip says what it is.
    #[test]
    fn a_clips_preview_follows_its_frames() {
        use engine_scene::{SpriteClipImport, SpriteClipMode};
        let swing = SpriteClipImport {
            name: String::from("swing"),
            first: 0,
            last: 3,
            durations: vec![0.5],
            mode: SpriteClipMode::PingPong,
            ..SpriteClipImport::default()
        };
        let at: Vec<u32> = [0.0, 0.45, 0.55, 0.65, 0.75, 0.85, 0.95, 1.05]
            .iter()
            .map(|&t| clip_frame(&swing, t))
            .collect();
        assert_eq!(at, [0, 0, 1, 2, 3, 2, 1, 0]);
        let once = SpriteClipImport {
            first: 5,
            last: 4,
            mode: SpriteClipMode::Once,
            ..SpriteClipImport::default()
        };
        assert_eq!(clip_frame(&once, 0.05), 5);
        assert_eq!(clip_frame(&once, 9.0), 4, "held at its end");
        assert_eq!(clip_label(&swing), "swing 0\u{2013}3 · 100 ms · ping-pong");
    }

    /// A rectangle drawn by hand covers whole pixels, either way round, inside the image;
    /// new names take the free numbers.
    #[test]
    fn hand_drawn_rectangles_cover_whole_pixels() {
        assert_eq!(
            pixel_rect([3.4, 2.6], [1.2, 7.1], [48.0, 16.0]),
            Some([1, 2, 3, 6])
        );
        assert_eq!(
            pixel_rect([40.0, 10.0], [60.0, 30.0], [48.0, 16.0]),
            Some([40, 10, 8, 6])
        );
        assert_eq!(pixel_rect([5.0, 5.0], [5.0, 9.0], [48.0, 16.0]), None);
        let doc = sheet();
        assert_eq!(free_name("hero", &doc.slices), "hero_0");
        let taken = [SliceImport {
            name: String::from("hero_0"),
            ..SliceImport::default()
        }];
        assert_eq!(free_name("hero", &taken), "hero_1");
    }

    /// Framed at the largest zoom that fits, centred; points map back to image pixels;
    /// the wheel keeps the pointed pixel in place.
    #[test]
    fn the_sheet_frames_maps_points_and_zooms_about_the_pointer() {
        let mut doc = sheet();
        doc.frame([500.0, 300.0]);
        assert_eq!(doc.zoom(), 8.0, "48 px at 8 is 384 of 452");
        assert_eq!(doc.center, [24.0, 8.0]);
        let (origin, size) = ([100.0, 50.0], [500.0, 300.0]);
        // The canvas's centre is the image's centre; 8 panel pixels right is one pixel.
        assert_eq!(doc.image_point([350.0, 200.0], origin, size), [24.0, 8.0]);
        assert_eq!(doc.image_point([358.0, 200.0], origin, size), [25.0, 8.0]);
        let at = doc.image_point([150.0, 120.0], origin, size);
        doc.zoom_about(1.0, at);
        assert_eq!(doc.zoom(), 12.0);
        let again = doc.image_point([150.0, 120.0], origin, size);
        assert!((again[0] - at[0]).abs() < 1e-4 && (again[1] - at[1]).abs() < 1e-4);
        doc.pan([12.0, 0.0]);
        assert!((doc.image_point([150.0, 120.0], origin, size)[0] - (at[0] - 1.0)).abs() < 1e-4);
    }

    /// The smallest slice under a point is picked; Shift ranges, Ctrl toggles, a click on
    /// nothing lets go.
    #[test]
    fn slices_are_picked_by_click_range_and_toggle() {
        let mut doc = sheet();
        assert_eq!(
            doc.slice_at([20.0, 5.0]),
            Some(1),
            "b, not the big one under it"
        );
        assert_eq!(doc.slice_at([60.0, 5.0]), None);
        doc.click(Some(0), false, false);
        doc.click(Some(2), true, false);
        assert_eq!(doc.picked, [0, 1, 2]);
        doc.click(Some(1), false, true);
        assert_eq!(doc.picked, [0, 2]);
        doc.click(None, false, false);
        assert!(doc.picked.is_empty());
        doc.click(Some(1), false, false);
        let mut line = String::new();
        doc.status(&mut line);
        assert_eq!(
            line,
            "48 × 16 px · 4 slices · 0 clips · 400 % · b (frame 1) 16, 0 · 16 × 16"
        );
    }
}
