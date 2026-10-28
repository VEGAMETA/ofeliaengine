//! The FileSystem dock's thumbnails (ADR 0112): images made smaller and sounds' waveforms
//! drawn on a thread of their own (E4-6a, E11-3a), models and materials drawn by the player
//! (E4-6b), all cached in `.ofelia/thumbnails/`, then kept in an atlas the UI draws from.
//!
//! - **An image's thumbnail** is the image fitted into a square of [`SIZE`] (never
//!   enlarged), its proportions kept, centred on transparency, averaged in sRGB.
//! - **A sound's** is its waveform (`engine_app::waveform`): each pixel column's lowest and
//!   highest sample over the whole file, every channel together, about a faint middle line;
//!   a column that reaches full scale in a warning colour (it may clip).
//! - **A model's or a material's** is a preview the player draws (`Player::request_preview`)
//!   when the cache has none for the file as it is. So is a glTF file's mesh or material
//!   (an inspector's field, [`Subject`]) and a built-in mesh.
//! - **The cache** keeps a PNG per subject, named by its id (and part), and beside it the key
//!   it was made for (the file's size and modification time, the size, a version): a file
//!   written anew is made again.
//! - **The atlas** has [`CELLS`] cells. The thumbnails on screen take them; the one seen
//!   longest ago gives its cell up.

use engine_scene::{AssetId, AssetKind, AssetRef};
use engine_ui::UiImage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// A thumbnail's side, in pixels.
pub(crate) const SIZE: u32 = 128;
/// The atlas's side, in pixels: 8 × 8 cells.
pub(crate) const ATLAS: u32 = 1024;
const PER_ROW: u32 = ATLAS / SIZE;
/// Cells in the atlas.
pub(crate) const CELLS: usize = (PER_ROW * PER_ROW) as usize;
/// Thumbnails uploaded into the atlas a frame at most (64 KB each).
const UPLOADS_PER_FRAME: usize = 8;
/// The cache's format; a new one makes every thumbnail again.
const VERSION: u32 = 1;

/// What a thumbnail shows: a file of the project (the dock's tiles), or a glTF file's or the
/// engine's mesh or material (an inspector's asset field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Subject {
    File(AssetId),
    Mesh(AssetRef),
    Material(AssetRef),
}

impl Subject {
    /// Its name in the cache.
    fn cache_name(self) -> String {
        match self {
            Self::File(id) => format!("{id}"),
            Self::Mesh(a) => format!("{}-mesh-{:016x}", a.file, a.part),
            Self::Material(a) => format!("{}-material-{:016x}", a.file, a.part),
        }
    }
}

/// The file a thumbnail is made from, as the asset database saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) bytes: u64,
    pub(crate) modified: u64,
}

impl Key {
    fn text(self) -> String {
        format!(
            "version={VERSION} size={SIZE} bytes={} modified={}\n",
            self.bytes, self.modified
        )
    }
}

enum Thumb {
    /// Being made, for this key: on the thread, or (`rendering`) by the player.
    Making(Key),
    /// Made; in an atlas cell once on screen.
    Made {
        key: Key,
        pixels: Vec<u8>,
        cell: Option<usize>,
    },
    /// None for this key: not an image the editor reads, or one it could not.
    Without(Key),
}

/// What the thread did: for which asset and key, its pixels (none: not cached, and for a
/// file it reads, not one the editor can), and whether it read the file.
type Made = (Subject, Key, Option<Vec<u8>>, bool);

/// Where a thumbnail comes from when the cache has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum From {
    /// The player draws it (a model, a material).
    Player,
    /// The thread fits the image.
    Image,
    /// The thread draws the sound's waveform.
    Sound,
}

/// A thumbnail to make: `subject`'s, its file at `path` under the project, from the cache
/// or as `from` says; or pixels the player drew, to cache.
struct Job {
    subject: Subject,
    path: PathBuf,
    key: Key,
    from: From,
    save: Option<Vec<u8>>,
}

/// The dock's thumbnails.
pub(crate) struct Thumbnails {
    root: PathBuf,
    thumbs: HashMap<Subject, Thumb>,
    jobs: Option<mpsc::Sender<Job>>,
    done: Option<mpsc::Receiver<Made>>,
    /// Each cell's thumbnail and the frame it was last on screen.
    cells: [Option<(Subject, u64)>; CELLS],
    /// Cells whose pixels are still to be uploaded.
    uploads: Vec<usize>,
    /// The atlas's bindless index (none until the overlay made it).
    atlas: Option<u32>,
    frame: u64,
    /// Models and materials the cache had nothing for: to ask the player for.
    to_draw: Vec<(Subject, Key)>,
    /// Previews asked of the player, by their key.
    drawing: HashMap<u64, (Subject, Key)>,
}

impl Thumbnails {
    /// The thumbnails of the project in `root`.
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            thumbs: HashMap::new(),
            jobs: None,
            done: None,
            cells: [None; CELLS],
            uploads: Vec::new(),
            atlas: None,
            frame: 0,
            to_draw: Vec::new(),
            drawing: HashMap::new(),
        }
    }

    /// How many thumbnails are being made: on the thread, or drawn by the player (the
    /// Tasks panel).
    pub(crate) fn pending(&self) -> usize {
        self.thumbs
            .values()
            .filter(|t| matches!(t, Thumb::Making(_)))
            .count()
    }

    /// The atlas the overlay made: its bindless index.
    pub(crate) fn set_atlas(&mut self, index: u32) {
        self.atlas = Some(index);
    }

    /// A new frame: what the thread made is taken.
    pub(crate) fn begin_frame(&mut self) {
        self.frame += 1;
        let Some(done) = &self.done else {
            return;
        };
        while let Ok((id, key, pixels, image)) = done.try_recv() {
            // Made for the file as it still is.
            if !matches!(self.thumbs.get(&id), Some(Thumb::Making(k)) if *k == key) {
                continue;
            }
            match (pixels, image) {
                (Some(pixels), _) => {
                    self.thumbs.insert(
                        id,
                        Thumb::Made {
                            key,
                            pixels,
                            cell: None,
                        },
                    );
                }
                (None, true) => {
                    self.thumbs.insert(id, Thumb::Without(key));
                }
                // Not in the cache: the player draws it.
                (None, false) => self.to_draw.push((id, key)),
            }
        }
    }

    /// The models and materials to ask the player for, each with the key its preview is
    /// asked by ([`preview_key`]).
    pub(crate) fn take_to_draw(&mut self) -> Vec<(Subject, Key)> {
        let to_draw = core::mem::take(&mut self.to_draw);
        for &(id, key) in &to_draw {
            self.drawing.insert(preview_key(id), (id, key));
        }
        to_draw
    }

    /// A preview the player drew for `preview` (its key), or could not (`pixels` empty):
    /// the thumbnail made, and cached on the thread.
    pub(crate) fn drawn(&mut self, preview: u64, pixels: Vec<u8>) {
        let Some((id, key)) = self.drawing.remove(&preview) else {
            return;
        };
        if !matches!(self.thumbs.get(&id), Some(Thumb::Making(k)) if *k == key) {
            return;
        }
        if pixels.len() != (SIZE * SIZE * 4) as usize {
            self.thumbs.insert(id, Thumb::Without(key));
            return;
        }
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(Job {
                subject: id,
                path: PathBuf::new(),
                key,
                from: From::Player,
                save: Some(pixels.clone()),
            });
        }
        self.thumbs.insert(
            id,
            Thumb::Made {
                key,
                pixels,
                cell: None,
            },
        );
    }

    /// `id`'s thumbnail, on screen this frame: its picture in the atlas once made, none
    /// before (or for a kind that has none). `kind` is its file's.
    pub(crate) fn image(
        &mut self,
        id: Subject,
        path: &str,
        kind: AssetKind,
        key: Key,
    ) -> Option<UiImage> {
        let from = match (id, kind) {
            (Subject::File(_), AssetKind::Texture) => From::Image,
            (Subject::File(_), AssetKind::Sound) => From::Sound,
            (Subject::File(_), AssetKind::Model | AssetKind::Material)
            | (Subject::Mesh(_) | Subject::Material(_), _) => From::Player,
            (Subject::File(_), _) => return None,
        };
        let atlas = self.atlas?;
        let current = match self.thumbs.get(&id) {
            Some(Thumb::Making(k) | Thumb::Without(k)) => *k == key,
            Some(Thumb::Made { key: k, .. }) => *k == key,
            None => false,
        };
        if !current {
            self.make(id, path, key, from);
            return None;
        }
        let held = match self.thumbs.get(&id) {
            Some(Thumb::Made { cell, .. }) => *cell,
            _ => return None,
        };
        let c = match held {
            Some(c) => c,
            None => {
                let c = self.free_cell()?;
                self.take_cell(c, id);
                if let Some(Thumb::Made { cell, .. }) = self.thumbs.get_mut(&id) {
                    *cell = Some(c);
                }
                c
            }
        };
        self.cells[c] = Some((id, self.frame));
        // Shown once its pixels are in the atlas.
        (!self.uploads.contains(&c)).then(|| UiImage {
            texture: atlas,
            uv: uv_of(c),
        })
    }

    /// The cell a new thumbnail takes: a free one, or the one seen longest ago (none when
    /// every cell is on screen this frame).
    fn free_cell(&self) -> Option<usize> {
        if let Some(c) = self.cells.iter().position(Option::is_none) {
            return Some(c);
        }
        self.cells
            .iter()
            .enumerate()
            .filter_map(|(c, cell)| cell.map(|(_, seen)| (c, seen)))
            .filter(|&(_, seen)| seen < self.frame)
            .min_by_key(|&(_, seen)| seen)
            .map(|(c, _)| c)
    }

    /// Cell `c` given to `id`: its last thumbnail out of it, its pixels to upload.
    fn take_cell(&mut self, c: usize, id: Subject) {
        if let Some((old, _)) = self.cells[c]
            && let Some(Thumb::Made { cell, .. }) = self.thumbs.get_mut(&old)
        {
            *cell = None;
        }
        self.cells[c] = Some((id, self.frame));
        if !self.uploads.contains(&c) {
            self.uploads.push(c);
        }
    }

    /// The thumbnail of `id` made on the thread (started the first time): from the cache,
    /// else as `from` says.
    fn make(&mut self, id: Subject, path: &str, key: Key, from: From) {
        if self.jobs.is_none() {
            let (jobs, incoming) = mpsc::channel::<Job>();
            let (done, results) = mpsc::channel();
            let cache = self.root.join(".ofelia").join("thumbnails");
            let spawned = std::thread::Builder::new()
                .name(String::from("editor-thumbnails"))
                .spawn(move || {
                    while let Ok(job) = incoming.recv() {
                        if let Some(pixels) = &job.save {
                            save(&cache, job.subject, job.key, pixels);
                            continue;
                        }
                        let pixels = made(&cache, &job);
                        let read = job.from != From::Player;
                        if done.send((job.subject, job.key, pixels, read)).is_err() {
                            break;
                        }
                    }
                });
            if spawned.is_err() {
                self.thumbs.insert(id, Thumb::Without(key));
                return;
            }
            self.jobs = Some(jobs);
            self.done = Some(results);
        }
        if let Some(jobs) = &self.jobs {
            let job = Job {
                subject: id,
                path: self.root.join(path),
                key,
                from,
                save: None,
            };
            let thumb = if jobs.send(job).is_ok() {
                Thumb::Making(key)
            } else {
                Thumb::Without(key)
            };
            self.thumbs.insert(id, thumb);
        }
    }

    /// Whether `id` has its thumbnail made.
    pub(crate) fn made(&self, id: Subject) -> bool {
        matches!(self.thumbs.get(&id), Some(Thumb::Made { .. }))
    }

    /// The thumbnails to upload this frame: each one's place in the atlas and its pixels
    /// (`SIZE` × `SIZE`, RGBA8 sRGB).
    pub(crate) fn uploads(&mut self) -> Vec<((u32, u32), &[u8])> {
        let count = self.uploads.len().min(UPLOADS_PER_FRAME);
        let cells: Vec<usize> = self.uploads.drain(..count).collect();
        let mut out = Vec::with_capacity(cells.len());
        for c in cells {
            let Some((id, _)) = self.cells[c] else {
                continue;
            };
            if let Some(Thumb::Made { pixels, .. }) = self.thumbs.get(&id) {
                out.push((corner_of(c), pixels.as_slice()));
            }
        }
        out
    }
}

/// Cell `c`'s top left corner in the atlas.
fn corner_of(c: usize) -> (u32, u32) {
    let c = c as u32;
    ((c % PER_ROW) * SIZE, (c / PER_ROW) * SIZE)
}

/// Cell `c`'s region of the atlas.
fn uv_of(c: usize) -> [f32; 4] {
    let (x, y) = corner_of(c);
    let a = ATLAS as f32;
    [
        x as f32 / a,
        y as f32 / a,
        (x + SIZE) as f32 / a,
        (y + SIZE) as f32 / a,
    ]
}

/// A job's thumbnail: from the cache when it was made for the same file, else, for an
/// image or a sound, made from it and cached. None when the player has to draw it, or the
/// file is not one the editor reads.
fn made(cache: &Path, job: &Job) -> Option<Vec<u8>> {
    let name = job.subject.cache_name();
    let png = cache.join(format!("{name}.png"));
    let key_file = cache.join(format!("{name}.key"));
    if std::fs::read_to_string(&key_file).is_ok_and(|k| k == job.key.text())
        && let Ok(bytes) = std::fs::read(&png)
        && let Ok(image) = engine_assets::decode_image(&bytes)
        && image.width == SIZE
        && image.height == SIZE
    {
        return Some(image.rgba);
    }
    let pixels = match job.from {
        From::Player => return None,
        From::Image => {
            let bytes = std::fs::read(&job.path).ok()?;
            let image = engine_assets::decode_image(&bytes).ok()?;
            fit(&image.rgba, image.width, image.height, SIZE)
        }
        From::Sound => waveform_tile(&engine_app::waveform(&job.path, SIZE as usize).ok()?),
    };
    save(cache, job.subject, job.key, &pixels);
    Some(pixels)
}

/// A sound's waveform drawn into a [`SIZE`] square on transparency: a faint middle line,
/// each column from its lowest to its highest sample in the sounds' colour (the dock's), a
/// column at full scale (it may clip) in a warning colour.
pub(crate) fn waveform_tile(waveform: &engine_app::Waveform) -> Vec<u8> {
    let side = SIZE as usize;
    let mut out = vec![0u8; side * side * 4];
    // Room above and below, so the loudest column does not touch the tile's edge.
    crate::sound_view::band(&mut out, side, 0, side, 10, &waveform.columns);
    out
}

/// A thumbnail's pixels cached for the file as `key` says it is.
fn save(cache: &Path, id: Subject, key: Key, pixels: &[u8]) {
    let name = id.cache_name();
    let png = cache.join(format!("{name}.png"));
    if std::fs::create_dir_all(cache).is_ok()
        && let Some(encoded) =
            engine_render::png::encode_png(SIZE, SIZE, engine_render::png::PngFormat::Rgba, pixels)
        && std::fs::write(&png, encoded).is_ok()
    {
        let _ = std::fs::write(cache.join(format!("{name}.key")), key.text());
    }
}

/// The key a preview of `id` is asked of the player by.
pub(crate) fn preview_key(id: Subject) -> u64 {
    let (file, part, what) = match id {
        Subject::File(f) => (f, 0, 0u64),
        Subject::Mesh(a) => (a.file, a.part, 1),
        Subject::Material(a) => (a.file, a.part, 2),
    };
    let v = file.as_u128();
    let mixed = (v as u64) ^ ((v >> 64) as u64);
    mixed ^ part.rotate_left(17) ^ what.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// `rgba` (`width` × `height`) fitted into a square of `side`, never enlarged, its
/// proportions kept and centred on transparency: each target pixel the average of the
/// source pixels it covers.
pub(crate) fn fit(rgba: &[u8], width: u32, height: u32, side: u32) -> Vec<u8> {
    let mut out = vec![0u8; (side * side * 4) as usize];
    if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
        return out;
    }
    let scale = (side as f32 / width.max(height) as f32).min(1.0);
    let (w, h) = (
        ((width as f32 * scale).round() as u32).clamp(1, side),
        ((height as f32 * scale).round() as u32).clamp(1, side),
    );
    let (x0, y0) = ((side - w) / 2, (side - h) / 2);
    for ty in 0..h {
        let (sy0, sy1) = span(ty, h, height);
        for tx in 0..w {
            let (sx0, sx1) = span(tx, w, width);
            let mut sum = [0u32; 4];
            for sy in sy0..sy1 {
                let row = (sy * width) as usize * 4;
                for sx in sx0..sx1 {
                    let p = row + sx as usize * 4;
                    for (s, &v) in sum.iter_mut().zip(&rgba[p..p + 4]) {
                        *s += u32::from(v);
                    }
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)).max(1);
            let o = (((y0 + ty) * side + x0 + tx) * 4) as usize;
            for (k, s) in sum.iter().enumerate() {
                out[o + k] = ((s + n / 2) / n) as u8;
            }
        }
    }
    out
}

/// The source pixels target pixel `t` of `target` covers out of `source`: at least one.
fn span(t: u32, target: u32, source: u32) -> (u32, u32) {
    let lo = (u64::from(t) * u64::from(source) / u64::from(target)) as u32;
    let hi = ((u64::from(t) + 1) * u64::from(source)).div_ceil(u64::from(target)) as u32;
    (lo.min(source - 1), hi.clamp(lo + 1, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sounds_thumbnail_is_its_waveform() {
        // Silence, then half scale, then full scale (it may clip).
        let columns: Vec<[f32; 2]> = (0..SIZE as usize)
            .map(|x| match x {
                0..43 => [0.0, 0.0],
                43..86 => [-0.5, 0.5],
                _ => [-1.0, 1.0],
            })
            .collect();
        let waveform = engine_app::Waveform {
            channel_columns: vec![columns.clone()],
            columns,
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            peak: 1.0,
            silence: [0, 0],
        };
        let tile = waveform_tile(&waveform);
        assert_eq!(tile.len(), (SIZE * SIZE * 4) as usize);
        // A column's height in drawn pixels, and its colour where it is drawn.
        let column = |x: usize| {
            let drawn: Vec<[u8; 4]> = (0..SIZE as usize)
                .map(|y| {
                    let o = (y * SIZE as usize + x) * 4;
                    [tile[o], tile[o + 1], tile[o + 2], tile[o + 3]]
                })
                .filter(|p| p[3] == 255)
                .collect();
            (drawn.len(), drawn.first().copied())
        };
        let (silent, _) = column(10);
        let (half, half_color) = column(60);
        let (full, full_color) = column(100);
        assert!(silent <= 1, "{silent}");
        assert!(half > 40 && full > half + 40, "{half} {full}");
        assert_ne!(half_color, full_color, "full scale is marked");
        // The middle line runs under silence, faint.
        let o = ((SIZE as usize / 2) * SIZE as usize + 10) * 4;
        assert!(tile[o + 3] > 0);
    }

    #[test]
    fn an_image_is_fitted_centred_and_never_enlarged() {
        // 8 × 4, left half red, right half blue: into 4 × 4, two rows, centred.
        let rgba: Vec<u8> = (0..4)
            .flat_map(|_| {
                (0..8).flat_map(|x| {
                    if x < 4 {
                        [255, 0, 0, 255]
                    } else {
                        [0, 0, 255, 255]
                    }
                })
            })
            .collect();
        let out = fit(&rgba, 8, 4, 4);
        let px = |x: usize, y: usize| &out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(px(0, 0), [0, 0, 0, 0], "above: transparent");
        assert_eq!(px(0, 1), [255, 0, 0, 255]);
        assert_eq!(px(3, 2), [0, 0, 255, 255]);
        assert_eq!(px(3, 3), [0, 0, 0, 0], "below: transparent");
        // Smaller than the square: its own size, in the middle.
        let one = fit(&[10, 20, 30, 255], 1, 1, 4);
        let filled: Vec<usize> = (0..16).filter(|&i| one[i * 4 + 3] != 0).collect();
        assert_eq!(filled, [5]);
        assert_eq!(&one[20..24], [10, 20, 30, 255]);
    }

    #[test]
    fn a_square_is_averaged() {
        // 4 × 4 into 2 × 2: each target pixel the mean of its 2 × 2.
        let rgba: Vec<u8> = (0..16u8).flat_map(|v| [v * 10, 0, 0, 255]).collect();
        let out = fit(&rgba, 4, 4, 2);
        // The top left covers 0, 1, 4, 5 (× 10): 25.
        assert_eq!(&out[0..4], [25, 0, 0, 255]);
    }

    #[test]
    fn thumbnails_are_cached_for_the_file_they_were_made_from() {
        let dir = std::env::temp_dir().join(format!("ofelia-thumbs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rgba: Vec<u8> = (0..64 * 32).flat_map(|_| [200, 100, 50, 255]).collect();
        let png =
            engine_render::png::encode_png(64, 32, engine_render::png::PngFormat::Rgba, &rgba)
                .unwrap();
        std::fs::write(dir.join("rock.png"), png).unwrap();
        let cache = dir.join(".ofelia/thumbnails");
        let job = Job {
            subject: Subject::File(AssetId::new()),
            path: dir.join("rock.png"),
            key: Key {
                bytes: 1,
                modified: 2,
            },
            from: From::Image,
            save: None,
        };
        let first = made(&cache, &job).unwrap();
        assert_eq!(first.len(), (SIZE * SIZE * 4) as usize);
        assert!(
            cache
                .join(format!("{}.png", job.subject.cache_name()))
                .is_file()
        );
        // The source gone: still made from the cache for the same key, not for another.
        std::fs::remove_file(dir.join("rock.png")).unwrap();
        assert_eq!(made(&cache, &job), Some(first));
        let other = Job {
            key: Key {
                bytes: 1,
                modified: 3,
            },
            ..job
        };
        assert_eq!(made(&cache, &other), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
