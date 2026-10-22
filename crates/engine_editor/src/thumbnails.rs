//! The FileSystem dock's thumbnails (ADR 0112, E4-6a): images made smaller on a thread of
//! their own and cached in `.ofelia/thumbnails/`, then kept in an atlas the UI draws from.
//!
//! - **A thumbnail** is the image fitted into a square of [`SIZE`] (never enlarged), its
//!   proportions kept, centred on transparency. Its pixels are averaged in sRGB.
//! - **The cache** keeps a PNG per asset, named by its id, and beside it the key it was made
//!   for (the file's size and modification time, the size, a version): a file written anew
//!   is made again.
//! - **The atlas** has [`CELLS`] cells. The thumbnails on screen take them; the one seen
//!   longest ago gives its cell up.

use engine_scene::{AssetId, AssetKind};
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
    /// Being made, for this key.
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

/// A thumbnail made on the thread: for which asset and key, and its pixels (none: the
/// file has none).
type Made = (AssetId, Key, Option<Vec<u8>>);

/// A thumbnail to make: asset `id`'s file at `path` under the project.
struct Job {
    id: AssetId,
    path: PathBuf,
    key: Key,
}

/// The dock's thumbnails.
pub(crate) struct Thumbnails {
    root: PathBuf,
    thumbs: HashMap<AssetId, Thumb>,
    jobs: Option<mpsc::Sender<Job>>,
    done: Option<mpsc::Receiver<Made>>,
    /// Each cell's thumbnail and the frame it was last on screen.
    cells: [Option<(AssetId, u64)>; CELLS],
    /// Cells whose pixels are still to be uploaded.
    uploads: Vec<usize>,
    /// The atlas's bindless index (none until the overlay made it).
    atlas: Option<u32>,
    frame: u64,
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
        }
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
        while let Ok((id, key, pixels)) = done.try_recv() {
            // Made for the file as it still is.
            if matches!(self.thumbs.get(&id), Some(Thumb::Making(k)) if *k == key) {
                let thumb = match pixels {
                    Some(pixels) => Thumb::Made {
                        key,
                        pixels,
                        cell: None,
                    },
                    None => Thumb::Without(key),
                };
                self.thumbs.insert(id, thumb);
            }
        }
    }

    /// Asset `id`'s thumbnail, on screen this frame: its picture in the atlas once made,
    /// none before (or for a kind that has none yet: E4-6b's rendered previews).
    pub(crate) fn image(
        &mut self,
        id: AssetId,
        path: &str,
        kind: AssetKind,
        key: Key,
    ) -> Option<UiImage> {
        if kind != AssetKind::Texture {
            return None;
        }
        let atlas = self.atlas?;
        let current = match self.thumbs.get(&id) {
            Some(Thumb::Making(k) | Thumb::Without(k)) => *k == key,
            Some(Thumb::Made { key: k, .. }) => *k == key,
            None => false,
        };
        if !current {
            self.make(id, path, key);
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
    fn take_cell(&mut self, c: usize, id: AssetId) {
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

    /// The thumbnail of `id` made on the thread (started the first time).
    fn make(&mut self, id: AssetId, path: &str, key: Key) {
        if self.jobs.is_none() {
            let (jobs, incoming) = mpsc::channel::<Job>();
            let (done, results) = mpsc::channel();
            let cache = self.root.join(".ofelia").join("thumbnails");
            let spawned = std::thread::Builder::new()
                .name(String::from("editor-thumbnails"))
                .spawn(move || {
                    while let Ok(job) = incoming.recv() {
                        let pixels = made(&cache, &job);
                        if done.send((job.id, job.key, pixels)).is_err() {
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
                id,
                path: self.root.join(path),
                key,
            };
            let thumb = if jobs.send(job).is_ok() {
                Thumb::Making(key)
            } else {
                Thumb::Without(key)
            };
            self.thumbs.insert(id, thumb);
        }
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

/// A job's thumbnail: from the cache when it was made for the same file, else made from
/// the image and cached. None when the file is not an image the editor reads.
fn made(cache: &Path, job: &Job) -> Option<Vec<u8>> {
    let png = cache.join(format!("{}.png", job.id));
    let key_file = cache.join(format!("{}.key", job.id));
    if std::fs::read_to_string(&key_file).is_ok_and(|k| k == job.key.text())
        && let Ok(bytes) = std::fs::read(&png)
        && let Ok(image) = engine_assets::decode_image(&bytes)
        && image.width == SIZE
        && image.height == SIZE
    {
        return Some(image.rgba);
    }
    let bytes = std::fs::read(&job.path).ok()?;
    let image = engine_assets::decode_image(&bytes).ok()?;
    let pixels = fit(&image.rgba, image.width, image.height, SIZE);
    if std::fs::create_dir_all(cache).is_ok()
        && let Some(encoded) =
            engine_render::png::encode_png(SIZE, SIZE, engine_render::png::PngFormat::Rgba, &pixels)
        && std::fs::write(&png, encoded).is_ok()
    {
        let _ = std::fs::write(&key_file, job.key.text());
    }
    Some(pixels)
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
            id: AssetId::new(),
            path: dir.join("rock.png"),
            key: Key {
                bytes: 1,
                modified: 2,
            },
        };
        let first = made(&cache, &job).unwrap();
        assert_eq!(first.len(), (SIZE * SIZE * 4) as usize);
        assert!(cache.join(format!("{}.png", job.id)).is_file());
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
